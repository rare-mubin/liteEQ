#[cfg(not(any(target_os = "linux", windows)))]
compile_error!("liteEQ supports Linux (PipeWire) and Windows (Equalizer APO)");

#[cfg(target_os = "linux")]
mod audio;
#[cfg(target_os = "linux")]
mod daemon;
#[cfg_attr(windows, allow(dead_code))] // the DSP itself lives in the plugin on Windows
mod dsp;
#[cfg(windows)]
mod eapo;
#[cfg(target_os = "linux")]
mod ipc;
mod remote;
#[cfg(target_os = "linux")]
mod service;
#[cfg_attr(windows, allow(dead_code))] // the wire format and app.conf are the service's
mod settings;
#[cfg(windows)]
#[allow(dead_code)] // the plugin-side half is used by the VST library
mod shm;
#[cfg(target_os = "linux")]
mod sysaudio;
mod ui;
mod viz;
#[cfg(target_os = "linux")]
mod volsync;
#[cfg(windows)]
mod windevices;

#[cfg(target_os = "linux")]
use audio::SINK_NAME;
#[cfg(target_os = "linux")]
use ipc::Client;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::crossterm::execute;
use remote::Status;
use settings::{Param, Settings};
use std::io::stdout;
use std::ops::ControlFlow;
use std::process::ExitCode;
use std::time::{Duration, Instant};
use ui::{Hits, MeterView, Target, Toggle, View};
use viz::{Analyzer, VizStyle};

/// The panel's connection to the effect.
#[cfg(target_os = "linux")]
type Link = Client;
#[cfg(windows)]
type Link = eapo::PluginLink;

#[cfg(target_os = "linux")]
const HELP: &str = "\
lite-eq - lightweight terminal bass enhancer (Calf Bass Enhancer DSP on PipeWire)

USAGE:
  lite-eq [OPTIONS]          open the control panel (starts the service if needed)
  lite-eq daemon [OPTIONS]   run the effect service in the foreground (headless)
  lite-eq stop               stop the background service
  lite-eq --list             list available output sinks

The effect runs as a background service: a virtual sink named `liteeq_sink` that
becomes the default output (restored when the service stops). The panel is only a
remote control, so closing it does not interrupt your audio. Both can be set in
the panel's settings: start at login (a systemd user unit) and whether closing the
panel also stops the service.

OPTIONS:
  -o, --output <NAME>   output sink node name (default: current default sink)
      --name <NAME>     node name of the virtual sink (default: liteeq_sink)
      --no-default      do not make liteEQ the default sink
      --volume-sync     mirror sink volume <-> device volume even with --no-default
      --no-volume-sync  leave volumes alone (the sink slider then stacks on the device volume)
  -V, --version         show the version
  -h, --help            show this help

While it runs, the system volume keys/slider/alsamixer act on `liteeq_sink`. That
sink is transparent, and its volume and mute are mirrored to the real device (and
back), so the volume controls keep driving the device's own mixer.
Settings are saved to ~/.config/liteeq/.";

#[cfg(windows)]
const HELP: &str = "\
lite-eq - lightweight terminal bass enhancer (Calf Bass Enhancer DSP in Equalizer APO)

USAGE:
  lite-eq                    open the control panel
  lite-eq setup [--device <PATTERN>] [--plugin <DLL>]
                             install the liteEQ plugin into Equalizer APO
  lite-eq setup --remove     remove it again

liteEQ runs as a plugin of Equalizer APO (https://sourceforge.net/projects/equalizerapo/),
inside the Windows audio engine on your real output device: no added latency, no
virtual devices, and it is active whenever Windows plays audio, including at login.

Install Equalizer APO and enable it for your output in its Configurator, then run
`lite-eq setup` (or press Install in the panel's settings). `--device` limits the
effect to the Equalizer APO devices whose name or GUID contains PATTERN (default:
all of them); the panel's settings can change this later.

OPTIONS:
  -V, --version         show the version
  -h, --help            show this help

Settings are saved to %APPDATA%\\liteeq\\ and shared with the plugin through
%ProgramData%\\liteEQ\\liteeq.shm.";

enum Mode {
    Tui,
    #[cfg(target_os = "linux")]
    Daemon,
    #[cfg(target_os = "linux")]
    Stop,
    #[cfg(windows)]
    Setup { remove: bool, device: Option<String>, plugin: Option<std::path::PathBuf> },
}

/// Command-line options for the service (Linux only).
#[cfg(target_os = "linux")]
type Opts = daemon::Args;
#[cfg(windows)]
type Opts = ();

fn parse_args() -> Result<Option<(Mode, Opts)>, String> {
    #[cfg(target_os = "linux")]
    let mut args = daemon::Args { name: SINK_NAME.to_string(), output: None, no_default: false, volume_sync: None };
    #[cfg(windows)]
    let args = ();
    let mut mode = Mode::Tui;
    let mut it = std::env::args().skip(1).peekable();
    if let Some(first) = it.peek() {
        match first.as_str() {
            #[cfg(target_os = "linux")]
            "daemon" => {
                mode = Mode::Daemon;
                it.next();
            }
            #[cfg(target_os = "linux")]
            "stop" => {
                mode = Mode::Stop;
                it.next();
            }
            #[cfg(windows)]
            "setup" => {
                mode = Mode::Setup { remove: false, device: None, plugin: None };
                it.next();
            }
            _ => {}
        }
    }
    while let Some(a) = it.next() {
        #[cfg(windows)]
        if let Mode::Setup { remove, device, plugin } = &mut mode {
            match a.as_str() {
                "--remove" => *remove = true,
                "--device" => *device = Some(it.next().ok_or("--device needs a pattern")?),
                "--plugin" => *plugin = Some(std::path::PathBuf::from(it.next().ok_or("--plugin needs a path")?)),
                "-h" | "--help" => {
                    println!("{HELP}");
                    return Ok(None);
                }
                other => return Err(format!("unknown setup option `{other}` (try --help)")),
            }
            continue;
        }
        match a.as_str() {
            "-h" | "--help" => {
                println!("{HELP}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("lite-eq {}", ui::VERSION);
                return Ok(None);
            }
            #[cfg(target_os = "linux")]
            "--list" => {
                for s in sysaudio::list_sinks().iter().filter(|s| *s != SINK_NAME) {
                    println!("{s}");
                }
                return Ok(None);
            }
            #[cfg(target_os = "linux")]
            "--no-default" => args.no_default = true,
            #[cfg(target_os = "linux")]
            "--volume-sync" => args.volume_sync = Some(true),
            #[cfg(target_os = "linux")]
            "--no-volume-sync" => args.volume_sync = Some(false),
            #[cfg(target_os = "linux")]
            "--name" => args.name = it.next().ok_or("--name needs a value")?,
            #[cfg(target_os = "linux")]
            "-o" | "--output" => args.output = Some(it.next().ok_or("--output needs a sink name")?),
            other => return Err(format!("unknown option `{other}` (try --help)")),
        }
    }
    Ok(Some((mode, args)))
}

fn main() -> ExitCode {
    let (mode, args) = match parse_args() {
        Ok(Some(v)) => v,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lite-eq: {e}");
            return ExitCode::from(2);
        }
    };
    match mode {
        #[cfg(target_os = "linux")]
        Mode::Daemon => daemon::run(args),
        #[cfg(target_os = "linux")]
        Mode::Stop => match Client::connect(&ipc::socket_path(&args.name)) {
            Ok(mut c) => {
                c.quit();
                println!("lite-eq: service stopped");
                ExitCode::SUCCESS
            }
            Err(_) => {
                println!("lite-eq: service is not running");
                ExitCode::SUCCESS
            }
        },
        #[cfg(windows)]
        Mode::Setup { remove, device, plugin } => {
            let res = if remove { eapo::remove() } else { eapo::setup(plugin, device) };
            match res {
                Ok(done) => {
                    for line in &done {
                        println!("  {line}");
                    }
                    if remove {
                        println!("lite-eq: removed from Equalizer APO");
                    } else {
                        println!("lite-eq: installed; Equalizer APO loads it now. Run `lite-eq` to control it.");
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("lite-eq: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Mode::Tui => match run_tui(args) {
            Ok(msg) => {
                if let Some(m) = msg {
                    println!("{m}");
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("lite-eq: {e}");
                ExitCode::FAILURE
            }
        },
    }
}

// ---------------------------------------------------------------------------
// Control panel
// ---------------------------------------------------------------------------

const DOUBLE_CLICK: Duration = Duration::from_millis(350);
const TICK_ACTIVE: Duration = Duration::from_millis(33);
const TICK_IDLE: Duration = Duration::from_millis(150);

/// Everything the panel needs for the background service (Linux).
#[cfg(target_os = "linux")]
struct ServiceState {
    daemon_args: daemon::Args,
    socket: std::path::PathBuf,
    cfg: settings::AppConfig,
    autostart: Option<bool>,
    /// While set, we are waiting for a freshly started service to appear.
    starting_until: Option<Instant>,
    /// Do not reconnect before this (avoids latching onto a service that is shutting down).
    quiet_until: Instant,
}

/// Everything the panel needs for Equalizer APO (Windows).
#[cfg(windows)]
#[derive(Default)]
struct EapoState {
    /// Equalizer APO itself is installed.
    available: bool,
    /// liteEQ is in its config (`lite-eq setup`, or the Install button).
    set_up: bool,
    /// Device buttons: "All" first, then one per output Equalizer APO is on.
    labels: Vec<String>,
    patterns: Vec<String>,
    selected: Option<usize>,
}

struct App {
    client: Option<Link>,
    /// What the panel shows and edits; pushed to the effect when it changes.
    settings: Settings,
    sent: Settings,
    status: Status,
    route: String,
    last_connect_try: Instant,
    #[cfg(target_os = "linux")]
    svc: ServiceState,
    #[cfg(windows)]
    eapo: EapoState,

    selected: usize,
    hover: Option<Target>,
    drag: Option<Param>,
    last_click: Option<(Instant, Param)>,
    style: VizStyle,
    note: String,
    settings_open: bool,
}

impl App {
    fn select(&mut self, p: Param) {
        if let Some(i) = Param::ALL.iter().position(|q| *q == p) {
            self.selected = i;
        }
    }

    fn param(&self) -> Param {
        Param::ALL[self.selected]
    }

    fn is_double_click(&mut self, p: Param) -> bool {
        let double = matches!(self.last_click, Some((t, q)) if q == p && t.elapsed() < DOUBLE_CLICK);
        self.last_click = if double { None } else { Some((Instant::now(), p)) };
        double
    }

    fn disconnect(&mut self) {
        self.client = None;
        self.status = Status::Stopped;
    }

    /// Start-at-login state; `None` when it cannot be managed (always on Windows,
    /// where Equalizer APO loads the plugin itself).
    fn autostart(&self) -> Option<bool> {
        #[cfg(target_os = "linux")]
        return self.svc.autostart;
        #[cfg(windows)]
        return None;
    }

    fn keep_running(&self) -> bool {
        #[cfg(target_os = "linux")]
        return self.svc.cfg.keep_running;
        #[cfg(windows)]
        return true;
    }

    /// Connect to whatever is running the effect and adopt its settings.
    fn try_connect(&mut self) -> bool {
        self.last_connect_try = Instant::now();
        #[cfg(target_os = "linux")]
        let link = Client::connect(&self.svc.socket);
        #[cfg(windows)]
        let link = {
            // Nothing to talk to (and no shared file to create) until it is set up.
            if !self.eapo.set_up {
                return false;
            }
            eapo::PluginLink::connect()
        };
        let Ok(mut c) = link else { return false };
        match c.settings() {
            Ok(s) => {
                self.settings = s;
                self.sent = s;
            }
            Err(_) => return false,
        }
        self.route = c.route.clone();
        self.client = Some(c);
        self.status = Status::Starting;
        #[cfg(target_os = "linux")]
        {
            self.svc.starting_until = None;
        }
        true
    }

    fn open_settings(&mut self) {
        self.settings_open = true;
        self.hover = None;
        #[cfg(windows)]
        self.refresh_eapo(); // Equalizer APO may have been installed meanwhile
    }

    /// Push local edits to the effect.
    fn sync_settings(&mut self) {
        if self.settings == self.sent {
            return;
        }
        self.sent = self.settings;
        if let Some(c) = &mut self.client {
            if c.set(&self.settings).is_err() {
                self.disconnect();
            }
        }
    }
}

// --- the background service (Linux) ----------------------------------------

#[cfg(target_os = "linux")]
impl App {
    fn connected(&self) -> bool {
        self.client.is_some()
    }

    fn start_service(&mut self) {
        settings::save(&self.settings); // the service loads these when it starts
        match service::spawn_daemon(&self.svc.daemon_args.to_cli()) {
            Ok(()) => {
                self.svc.starting_until = Some(Instant::now() + Duration::from_secs(8));
                self.status = Status::Starting;
                self.note.clear();
            }
            Err(e) => self.note = format!("could not start the service: {e}"),
        }
    }

    fn stop_service(&mut self) {
        if let Some(mut c) = self.client.take() {
            c.quit();
        }
        self.status = Status::Stopped;
        self.svc.starting_until = None;
        self.svc.quiet_until = Instant::now() + Duration::from_secs(2);
    }

    fn toggle_service(&mut self) {
        if self.connected() || matches!(self.status, Status::Running) {
            self.stop_service();
        } else if self.svc.starting_until.is_none() {
            self.start_service();
        }
    }

    fn set_autostart(&mut self, on: bool) {
        if self.svc.autostart.is_none() {
            self.note = "no systemd user session available".into();
            return;
        }
        match service::set_autostart(on) {
            Ok(()) => self.note.clear(),
            Err(e) => self.note = e,
        }
        self.svc.autostart = service::unit_enabled();
    }

    fn set_keep_running(&mut self, on: bool) {
        self.svc.cfg.keep_running = on;
        settings::save_app(&self.svc.cfg);
    }

    fn apply_switch(&mut self, which: ui::SettingBtn, on: bool) {
        match which {
            ui::SettingBtn::Autostart => self.set_autostart(on),
            ui::SettingBtn::KeepRunning => self.set_keep_running(on),
        }
    }

    /// Why a service we started did not come up: the last line of its log.
    fn service_failure_note() -> String {
        let Some(path) = service::log_path() else { return "service did not start".into() };
        let last = std::fs::read_to_string(&path).ok().and_then(|t| t.lines().rev().find(|l| !l.trim().is_empty()).map(str::to_string));
        match last {
            Some(l) => format!("service did not start · {}", l.trim_start_matches("lite-eq: ")),
            None => format!("service did not start · see {}", path.display()),
        }
    }
}

// --- Equalizer APO (Windows) ------------------------------------------------

#[cfg(windows)]
impl App {
    /// Re-read Equalizer APO's devices and liteEQ's current `Device:` choice.
    fn refresh_eapo(&mut self) {
        let Some(e) = eapo::Eapo::find() else {
            self.eapo = EapoState::default();
            return;
        };
        let devices = e.devices();
        self.eapo.available = true;
        self.eapo.set_up = e.is_set_up();
        self.eapo.labels = std::iter::once("All".to_string()).chain(devices.iter().map(|d| d.short_name())).collect();
        self.eapo.patterns = std::iter::once("all".to_string()).chain(devices.into_iter().map(|d| d.guid)).collect();
        let current = e.device().unwrap_or_default();
        self.eapo.selected = self.eapo.patterns.iter().position(|p| p.eq_ignore_ascii_case(&current));
    }

    /// Settings popup: add liteEQ to Equalizer APO's config.
    fn install_plugin(&mut self) {
        if !self.eapo.available || self.eapo.set_up {
            return;
        }
        match eapo::setup(None, None) {
            Ok(_) => {
                self.refresh_eapo();
                self.try_connect();
                self.note = "installed into Equalizer APO".into();
            }
            Err(e) => self.note = e,
        }
    }

    /// Settings popup: take liteEQ out again. A still-loaded DLL is left in
    /// `VSTPlugins` (harmless once out of the config; the next install replaces it).
    fn remove_plugin(&mut self) {
        if !self.eapo.set_up {
            return;
        }
        match eapo::remove() {
            Ok(_) => {
                self.client = None;
                self.status = Status::Stopped;
                self.route.clear();
                self.refresh_eapo();
                self.note = "removed from Equalizer APO".into();
            }
            Err(e) => self.note = e,
        }
    }

    /// Settings popup: apply the plugin to device button `i` (0 = all outputs).
    fn select_eapo_device(&mut self, i: usize) {
        let (Some(pattern), Some(e)) = (self.eapo.patterns.get(i), eapo::Eapo::find()) else { return };
        match e.set_device(pattern) {
            Ok(()) => {
                self.eapo.selected = Some(i);
                self.route = eapo::route();
                self.note.clear();
            }
            Err(err) => self.note = err,
        }
    }

    fn cycle_eapo_device(&mut self) {
        if self.eapo.set_up && !self.eapo.labels.is_empty() {
            let next = self.eapo.selected.map_or(0, |i| (i + 1) % self.eapo.labels.len());
            self.select_eapo_device(next);
        }
    }
}

fn toggle(s: &mut Settings, t: Toggle) {
    match t {
        Toggle::Listen => s.listen = !s.listen,
        Toggle::Floor => s.floor_active = !s.floor_active,
        Toggle::Bypass => s.bypass = !s.bypass,
    }
}

/// Parameter a target belongs to, if any.
fn param_of(t: Option<Target>) -> Option<Param> {
    match t? {
        Target::Slider(p) | Target::Label(p) | Target::Step(p, _) => Some(p),
        _ => None,
    }
}

fn set_from_x(app: &mut App, hits: &Hits, p: Param, x: u16) {
    let Some(r) = hits.slider_rect(p) else { return };
    let span = (r.width.max(2) - 1) as f32;
    let frac = (x as f32 - r.x as f32) / span;
    p.set(&mut app.settings, p.from_frac(frac));
}

fn handle_mouse(app: &mut App, hits: &Hits, m: MouseEvent) {
    let target = hits.at(m.column, m.row);
    let coarse = m.modifiers.contains(KeyModifiers::SHIFT);

    // The settings popup is modal: only its own controls (and the backdrop) react.
    if app.settings_open {
        match m.kind {
            MouseEventKind::Moved => app.hover = target,
            MouseEventKind::Down(MouseButton::Left) => match target {
                #[cfg(target_os = "linux")]
                Some(Target::Service) => app.toggle_service(),
                #[cfg(target_os = "linux")]
                Some(Target::Switch(which, on)) => app.apply_switch(which, on),
                #[cfg(windows)]
                Some(Target::EapoInstall) => app.install_plugin(),
                #[cfg(windows)]
                Some(Target::EapoRemove) => app.remove_plugin(),
                #[cfg(windows)]
                Some(Target::EapoDevice(i)) => app.select_eapo_device(i as usize),
                Some(Target::CloseSettings) | Some(Target::Backdrop) => {
                    app.settings_open = false;
                    app.hover = None;
                }
                _ => {}
            },
            _ => {}
        }
        return;
    }

    match m.kind {
        MouseEventKind::Moved => app.hover = target,
        MouseEventKind::Down(MouseButton::Left) => {
            app.note.clear();
            match target {
                Some(Target::Slider(p)) => {
                    app.select(p);
                    if app.is_double_click(p) {
                        p.reset(&mut app.settings);
                    } else {
                        app.drag = Some(p);
                        set_from_x(app, hits, p, m.column);
                    }
                }
                Some(Target::Label(p)) => {
                    app.select(p);
                    if app.is_double_click(p) {
                        p.reset(&mut app.settings);
                    }
                }
                Some(Target::Step(p, dir)) => {
                    app.select(p);
                    p.nudge(&mut app.settings, dir as f32, coarse);
                }
                Some(Target::Toggle(t)) => toggle(&mut app.settings, t),
                Some(Target::Style(st)) => app.style = st,
                Some(Target::Viz) => app.style = app.style.next(),
                #[cfg(target_os = "linux")]
                Some(Target::Service) => app.toggle_service(),
                #[cfg(target_os = "linux")]
                Some(Target::Switch(which, on)) => app.apply_switch(which, on),
                #[cfg(windows)]
                Some(Target::Service | Target::Switch(..)) => {} // not drawn on Windows
                Some(Target::OpenSettings) => app.open_settings(),
                // Only reachable inside the popup.
                Some(Target::EapoInstall | Target::EapoRemove | Target::EapoDevice(_)) => {}
                Some(Target::CloseSettings) | Some(Target::Backdrop) | None => {}
            }
        }
        MouseEventKind::Down(MouseButton::Right) => {
            if let Some(p) = param_of(target) {
                app.select(p);
                p.reset(&mut app.settings);
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if let Some(p) = app.drag {
                set_from_x(app, hits, p, m.column);
            }
        }
        MouseEventKind::Up(MouseButton::Left) => app.drag = None,
        // The wheel is deliberately ignored.
        _ => {}
    }
}

fn handle_key(app: &mut App, key: event::KeyEvent) -> ControlFlow<()> {
    if key.kind == KeyEventKind::Release {
        return ControlFlow::Continue(());
    }
    let coarse = key.modifiers.contains(KeyModifiers::SHIFT);
    let param = app.param();
    let n = Param::ALL.len();

    if app.settings_open {
        // Popup keys; the effect controls behind it stay untouched.
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char(',') => app.settings_open = false,
            KeyCode::Char('q') => return ControlFlow::Break(()),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return ControlFlow::Break(()),
            #[cfg(target_os = "linux")]
            KeyCode::Char('s') => app.toggle_service(),
            #[cfg(target_os = "linux")]
            KeyCode::Char('a') => {
                let on = !app.autostart().unwrap_or(false);
                app.set_autostart(on);
            }
            #[cfg(target_os = "linux")]
            KeyCode::Char('p') => {
                let on = !app.keep_running();
                app.set_keep_running(on);
            }
            #[cfg(windows)]
            KeyCode::Char('e') | KeyCode::Char('i') => app.install_plugin(),
            #[cfg(windows)]
            KeyCode::Char('u') => app.remove_plugin(),
            #[cfg(windows)]
            KeyCode::Char('d') => app.cycle_eapo_device(),
            _ => {}
        }
        return ControlFlow::Continue(());
    }

    app.note.clear();
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return ControlFlow::Break(()),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return ControlFlow::Break(()),
        KeyCode::Up | KeyCode::Char('k') => app.selected = (app.selected + n - 1) % n,
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => app.selected = (app.selected + 1) % n,
        KeyCode::Left => param.nudge(&mut app.settings, -1.0, coarse),
        KeyCode::Right => param.nudge(&mut app.settings, 1.0, coarse),
        KeyCode::PageDown => param.nudge(&mut app.settings, -1.0, true),
        KeyCode::PageUp => param.nudge(&mut app.settings, 1.0, true),
        KeyCode::Char('r') => param.reset(&mut app.settings),
        KeyCode::Char('R') => app.settings = Settings::default(),
        KeyCode::Char('l') | KeyCode::Char(' ') => toggle(&mut app.settings, Toggle::Listen),
        KeyCode::Char('f') => toggle(&mut app.settings, Toggle::Floor),
        KeyCode::Char('b') => toggle(&mut app.settings, Toggle::Bypass),
        KeyCode::Char('v') => app.style = app.style.next(),
        KeyCode::Char(',') => app.open_settings(),
        #[cfg(target_os = "linux")]
        KeyCode::Char('s') => app.toggle_service(),
        #[cfg(target_os = "linux")]
        KeyCode::Char('a') => {
            let on = !app.autostart().unwrap_or(false);
            app.set_autostart(on);
        }
        #[cfg(target_os = "linux")]
        KeyCode::Char('p') => {
            let on = !app.keep_running();
            app.set_keep_running(on);
        }
        _ => {}
    }
    ControlFlow::Continue(())
}

fn run_tui(opts: Opts) -> std::io::Result<Option<String>> {
    let _ = &opts;
    let mut app = App {
        client: None,
        settings: settings::load(),
        sent: settings::load(),
        status: Status::Stopped,
        route: String::new(),
        last_connect_try: Instant::now(),
        #[cfg(target_os = "linux")]
        svc: ServiceState {
            socket: ipc::socket_path(&opts.name),
            daemon_args: opts,
            cfg: settings::load_app(),
            autostart: service::systemd_available().then(|| service::unit_enabled().unwrap_or(false)),
            starting_until: None,
            quiet_until: Instant::now(),
        },
        #[cfg(windows)]
        eapo: EapoState::default(),
        selected: 1,
        hover: None,
        drag: None,
        last_click: None,
        style: VizStyle::Spectrum,
        note: String::new(),
        settings_open: false,
    };
    #[cfg(windows)]
    app.refresh_eapo();

    // Attach to the effect; on Linux start the service if it is not running.
    #[cfg(target_os = "linux")]
    if !app.try_connect() {
        app.start_service();
    }
    #[cfg(windows)]
    app.try_connect();

    let mut terminal = ratatui::init();
    let _ = execute!(stdout(), EnableMouseCapture);
    // Turn mouse reporting off again even if we panic.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableMouseCapture);
        prev_hook(info);
    }));
    let res = event_loop(&mut terminal, &mut app);
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    res?;

    // Closing the panel: on Windows the plugin keeps running, and its settings
    // live in the shared block; on Linux, stop the service if asked to.
    #[cfg(windows)]
    {
        settings::save(&app.settings);
        return Ok(None);
    }
    #[cfg(target_os = "linux")]
    {
        let message = if app.connected() {
            if app.keep_running() {
                Some("liteEQ service is still running in the background (`lite-eq stop` to stop it)".to_string())
            } else {
                if let Some(c) = &mut app.client {
                    c.quit();
                }
                Some("liteEQ service stopped".to_string())
            }
        } else {
            settings::save(&app.settings);
            None
        };
        Ok(message)
    }
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App) -> std::io::Result<()> {
    let mut meters = MeterView::default();
    let mut analyzer = Analyzer::new();
    let mut hits = Hits::default();
    // Only redraw when something changed: input, live audio, meters still falling,
    // or the status. An idle panel then costs next to no CPU.
    let mut dirty = true;
    let mut last_drawn: Option<(Status, Option<bool>, String)> = None;
    let mut next_tick = Instant::now();

    loop {
        if Instant::now() >= next_tick {
            let busy = !analyzer.is_silent() || !meters.is_idle();
            next_tick = Instant::now() + if busy { TICK_ACTIVE } else { TICK_IDLE };

            match app.client.as_mut().map(Link::poll) {
                Some(Ok(frame)) => {
                    app.status = frame.status;
                    if frame.sample_rate != analyzer.sample_rate() {
                        analyzer = Analyzer::with_sample_rate(frame.sample_rate);
                    }
                    let m = frame.meters;
                    meters.update([m.in_l, m.in_r, m.out_l, m.out_r, m.harmonics], 1.5);
                    match frame.samples {
                        Some((input, output)) => analyzer.update(Some((&input, &output))),
                        None if !analyzer.is_silent() => analyzer.update(None),
                        None => {}
                    }
                }
                other => {
                    if let Some(Err(_)) = other {
                        // The effect went away (stopped elsewhere, crashed, restarted).
                        app.disconnect();
                    }
                    meters.update([0.0; 5], 1.5);
                    if !analyzer.is_silent() {
                        analyzer.update(None);
                    }
                    #[cfg(target_os = "linux")]
                    let (waiting, quiet) = (app.svc.starting_until.is_some_and(|t| Instant::now() < t), Instant::now() < app.svc.quiet_until);
                    #[cfg(windows)]
                    let (waiting, quiet) = (false, false);
                    let due = app.last_connect_try.elapsed() >= if waiting { Duration::from_millis(250) } else { Duration::from_secs(1) };
                    if due && !quiet && app.try_connect() {
                        dirty = true;
                    }
                    #[cfg(target_os = "linux")]
                    if app.client.is_none() {
                        if let Some(deadline) = app.svc.starting_until {
                            if Instant::now() >= deadline {
                                app.svc.starting_until = None;
                                app.status = Status::Stopped;
                                app.note = App::service_failure_note();
                            }
                        }
                    }
                }
            }
            dirty |= !analyzer.is_silent() || !meters.is_idle();
        }

        app.sync_settings();

        let key = (app.status.clone(), app.autostart(), app.note.clone());
        if last_drawn.as_ref() != Some(&key) {
            dirty = true;
            last_drawn = Some(key);
        }
        if dirty {
            dirty = false;
            terminal.draw(|f| {
                #[cfg(windows)]
                let eapo = app.eapo.available.then_some(ui::EapoView {
                    labels: &app.eapo.labels,
                    selected: app.eapo.selected,
                    set_up: app.eapo.set_up,
                });
                #[cfg(target_os = "linux")]
                let eapo = None;
                let view = View {
                    settings: &app.settings,
                    selected: app.selected,
                    meters,
                    status: &app.status,
                    route: &app.route,
                    note: &app.note,
                    hover: app.hover,
                    viz_style: app.style,
                    analyzer: &analyzer,
                    autostart: app.autostart(),
                    keep_running: app.keep_running(),
                    plugin: cfg!(windows),
                    eapo,
                    settings_open: app.settings_open,
                };
                hits = ui::draw(f, &view);
            })?;
        }

        if event::poll(next_tick.saturating_duration_since(Instant::now()))? {
            dirty = true;
            loop {
                match event::read()? {
                    Event::Key(k) => {
                        if handle_key(app, k).is_break() {
                            return Ok(());
                        }
                    }
                    Event::Mouse(m) => handle_mouse(app, &hits, m),
                    _ => {}
                }
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
            app.sync_settings();
        }
    }
}
