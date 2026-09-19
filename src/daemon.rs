//! The background service: owns the PipeWire sink, the DSP, the default-output
//! takeover and volume sync, and answers the TUI over a Unix socket. It runs
//! headless, so the effect keeps working whether or not the TUI is open.

use crate::audio::{self, Shared, Status};
use crate::ipc::{self, Served};
use crate::settings::{self, Settings};
use crate::sysaudio;
use crate::volsync::VolSync;
use std::io::{BufRead, BufReader};
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
        if let Some(d) = sysaudio::default_sink().filter(|d| *d != args.name) {
            return Some(d);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    None
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

    let (quit_tx, quit_rx) = pipewire::channel::channel::<()>();
    let audio_thread = {
        let shared = shared.clone();
        let (name, target) = (args.name.clone(), Some(target.clone()));
        std::thread::spawn(move || {
            if let Err(e) = audio::run(shared.clone(), name, target, quit_rx) {
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
    let volsync = sync_volume.then(|| VolSync::start(args.name.clone(), target.clone()));

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
        }
    }
    let crashed = audio_thread.is_finished() && !STOP.load(Ordering::Relaxed);

    STOP.store(true, Ordering::SeqCst); // also ends the accept loop
    drop(volsync);
    drop(guard); // move apps back to the real output before the sink disappears
    let _ = quit_tx.send(());
    let _ = audio_thread.join();
    settings::save(&shared.settings.lock().map(|s| *s).unwrap_or_default());

    if crashed { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}
