//! The background service: owns the PipeWire sink, the DSP, the default-output
//! takeover and volume sync, and answers the TUI over a Unix socket. It runs
//! headless, so the effect keeps working whether or not the TUI is open.

use crate::audio::{self, Cmd, Shared, Status};
use crate::ipc::{self, Served};
use crate::settings::{self, Settings};
use crate::sysaudio;
use crate::volsync::VolSync;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Args {
    pub name: String,
    pub output: Option<String>,
    pub no_default: bool,
    pub volume_sync: Option<bool>,
}

impl Args {
    /// The flags to hand to `lite-eq daemon` when the TUI starts the service.
    pub fn to_cli(&self) -> Vec<String> {
        let mut v = vec!["--name".to_string(), self.name.clone()];
        if let Some(o) = &self.output {
            v.extend(["--output".into(), o.clone()]);
        }
        if self.no_default {
            v.push("--no-default".into());
        }
        match self.volume_sync {
            Some(true) => v.push("--volume-sync".into()),
            Some(false) => v.push("--no-volume-sync".into()),
            None => {}
        }
        v
    }
}

/// Set by SIGTERM/SIGINT or the `QUIT` command.
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

/// Puts the previous default sink back when dropped.
struct DefaultSinkGuard(Option<String>);

impl Drop for DefaultSinkGuard {
    fn drop(&mut self) {
        if let Some(prev) = self.0.take() {
            sysaudio::set_default_sink(&prev);
        }
    }
}

fn serve_client(stream: UnixStream, shared: Arc<Shared>) {
    let Ok(mut writer) = stream.try_clone() else { return };
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        match ipc::handle_request(&shared, &line, &mut writer) {
            Ok(Served::Continue) => {}
            Ok(Served::Quit) => {
                STOP.store(true, Ordering::SeqCst);
                break;
            }
            Err(_) => break,
        }
    }
}

fn accept_loop(listener: UnixListener, shared: Arc<Shared>) {
    while !STOP.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let shared = shared.clone();
                std::thread::spawn(move || serve_client(stream, shared));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// The output device to play to: `--output`, or the current default sink,
/// waiting for the audio server to come up (this runs at login too).
fn wait_for_output(args: &Args) -> Option<String> {
    if args.output.is_some() {
        return args.output.clone();
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && !STOP.load(Ordering::Relaxed) {
        if let Some(d) = sysaudio::default_sink().filter(|d| *d != args.name && d != sysaudio::DUMMY_SINK) {
            return Some(d);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    None
}

/// Keeps the output on a live device: follows the user's choice of output and
/// moves off a device that went away (unplugged headphones, a profile switch,
/// a Bluetooth disconnect). The session manager destroys our playback stream
/// when its device disappears (`node.dont-reconnect`), so it is rebuilt here.
struct Follow {
    name: String,
    target: String,
    /// `--output` was given: stay on it, only rebuild the stream when needed.
    pinned: bool,
    /// We took over the default output and take it back when the user picks a device.
    own_default: bool,
    /// Set by `pactl subscribe` when sinks come and go or the default changes.
    events: Arc<AtomicBool>,
    monitor: Option<Child>,
    last_poll: Instant,
    last_retarget: Instant,
}

impl Follow {
    fn new(args: &Args, target: String, own_default: bool) -> Self {
        let events = Arc::new(AtomicBool::new(false));
        let monitor = Command::new("pactl").arg("subscribe").stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok().map(|mut child| {
            let stdout = child.stdout.take().expect("piped stdout");
            let events = events.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    let sink_added_or_removed = line.contains(" on sink #") && !line.contains("'change'");
                    if sink_added_or_removed || line.contains(" on server") {
                        events.store(true, Ordering::Relaxed);
                    }
                }
            });
            child
        });
        Self {
            name: args.name.clone(),
            target,
            pinned: args.output.is_some(),
            own_default,
            events,
            monitor,
            last_poll: Instant::now(),
            last_retarget: Instant::now(),
        }
    }

    /// The device to move the output to, if it should move (or be rebuilt).
    fn check(&mut self, shared: &Shared) -> Option<String> {
        let dead = !shared.output_ok.load(Ordering::Relaxed) && self.last_retarget.elapsed() >= Duration::from_secs(3);
        let poll = self.last_poll.elapsed() >= Duration::from_secs(10);
        if !(self.events.swap(false, Ordering::Relaxed) || poll || dead) {
            return None;
        }
        self.last_poll = Instant::now();
        let sinks: Vec<(String, i64)> =
            sysaudio::sinks().into_iter().filter(|(s, _)| *s != self.name && s != sysaudio::DUMMY_SINK).collect();
        let has = |n: &str| sinks.iter().any(|(s, _)| s == n);
        let mut want = self.target.clone();
        if !self.pinned {
            if let Some(d) = sysaudio::default_sink().filter(|d| has(d)) {
                // The user picked another output: play there, and stay in front of it.
                want = d;
                if self.own_default {
                    sysaudio::set_default_sink(&self.name);
                }
            }
            if !has(&want) {
                // Our device is gone: take the one the session manager would prefer.
                if let Some((best, _)) = sinks.iter().max_by_key(|(_, p)| *p) {
                    want = best.clone();
                }
            }
        }
        if !has(&want) || (want == self.target && !dead) {
            return None; // nothing to move to yet, or nothing to do
        }
        self.target = want.clone();
        self.last_retarget = Instant::now();
        Some(want)
    }
}

impl Drop for Follow {
    fn drop(&mut self) {
        if let Some(mut c) = self.monitor.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

pub fn run(args: Args) -> ExitCode {
    // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
    unsafe {
        let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }

    let path = ipc::socket_path(&args.name);
    if UnixStream::connect(&path).is_ok() {
        eprintln!("lite-eq: the service is already running");
        return ExitCode::SUCCESS;
    }
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("lite-eq: cannot create {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let _ = listener.set_nonblocking(true);

    let shared = Shared::new(settings::load());
    {
        let shared = shared.clone();
        std::thread::spawn(move || accept_loop(listener, shared));
    }

    let code = serve(&args, &shared);
    let _ = std::fs::remove_file(&path);
    code
}

fn serve(args: &Args, shared: &Arc<Shared>) -> ExitCode {
    let prev_default = sysaudio::default_sink();
    let Some(target) = wait_for_output(args) else {
        eprintln!("lite-eq: no output device found; pass --output <NAME> (see `lite-eq --list`)");
        *shared.status.lock().unwrap() = Status::Error("no output device".into());
        return ExitCode::FAILURE;
    };
    *shared.route.lock().unwrap() = format!("{} → {}", args.name, target);

    let (cmd_tx, cmd_rx) = pipewire::channel::channel::<Cmd>();
    let audio_thread = {
        let shared = shared.clone();
        let (name, target) = (args.name.clone(), Some(target.clone()));
        std::thread::spawn(move || {
            if let Err(e) = audio::run(shared.clone(), name, target, cmd_rx) {
                *shared.status.lock().unwrap() = Status::Error(e.to_string());
            }
        })
    };

    let mut guard = DefaultSinkGuard(None);
    let mut takeover_ok = false;
    if !args.no_default {
        if sysaudio::set_default_sink_retry(&args.name) {
            guard.0 = prev_default.filter(|d| *d != args.name).or(Some(target.clone()));
            takeover_ok = true;
        } else {
            eprintln!("lite-eq: could not make {} the default output; route audio to it manually", args.name);
        }
    }
    let sync_volume = args.volume_sync.unwrap_or(!args.no_default && takeover_ok);
    let mut volsync = sync_volume.then(|| VolSync::start(args.name.clone(), target.clone()));
    let mut follow = Follow::new(args, target.clone(), takeover_ok);

    // Idle until asked to stop; persist settings shortly after they change.
    let mut saved: Settings = *shared.settings.lock().unwrap();
    let mut last_check = Instant::now();
    while !STOP.load(Ordering::Relaxed) && !audio_thread.is_finished() {
        std::thread::sleep(Duration::from_millis(100));
        if last_check.elapsed() >= Duration::from_secs(1) {
            last_check = Instant::now();
            let now: Settings = *shared.settings.lock().unwrap();
            if now != saved {
                settings::save(&now);
                saved = now;
            }
            if let Some(dev) = follow.check(shared) {
                eprintln!("lite-eq: output → {dev}");
                let _ = cmd_tx.send(Cmd::Retarget(dev.clone()));
                *shared.route.lock().unwrap() = format!("{} → {}", args.name, dev);
                if guard.0.is_some() {
                    guard.0 = Some(dev.clone()); // on exit, hand the default to the current device
                }
                if volsync.take().is_some() {
                    volsync = Some(VolSync::start(args.name.clone(), dev));
                }
            }
        }
    }
    let crashed = audio_thread.is_finished() && !STOP.load(Ordering::Relaxed);

    STOP.store(true, Ordering::SeqCst); // also ends the accept loop
    drop(follow);
    drop(volsync);
    drop(guard); // move apps back to the real output before the sink disappears
    let _ = cmd_tx.send(Cmd::Quit);
    let _ = audio_thread.join();
    settings::save(&shared.settings.lock().map(|s| *s).unwrap_or_default());

    if crashed { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}
