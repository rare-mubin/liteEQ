mod audio;
mod daemon;
mod dsp;
mod ipc;
mod service;
mod settings;
mod sysaudio;
mod ui;
mod viz;
mod volsync;

use audio::{SINK_NAME, Status};
use ipc::Client;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::crossterm::execute;
use settings::{AppConfig, Param, Settings};
use std::io::stdout;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};
use ui::{Hits, MeterView, SettingBtn, Target, Toggle, View};
use viz::{Analyzer, VizStyle};

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

enum Mode {
    Tui,
    Daemon,
    Stop,
}

fn parse_args() -> Result<Option<(Mode, daemon::Args)>, String> {
    let mut args = daemon::Args { name: SINK_NAME.to_string(), output: None, no_default: false, volume_sync: None };
    let mut mode = Mode::Tui;
    let mut it = std::env::args().skip(1).peekable();
    if let Some(first) = it.peek() {
        match first.as_str() {
            "daemon" => {
                mode = Mode::Daemon;
                it.next();
            }
            "stop" => {
                mode = Mode::Stop;
                it.next();
            }
            _ => {}
        }
    }
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                println!("{HELP}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("lite-eq {}", ui::VERSION);
                return Ok(None);
            }
            "--list" => {
                for s in sysaudio::list_sinks().iter().filter(|s| *s != SINK_NAME) {
                    println!("{s}");
                }
                return Ok(None);
            }
            "--no-default" => args.no_default = true,
            "--volume-sync" => args.volume_sync = Some(true),
            "--no-volume-sync" => args.volume_sync = Some(false),
            "--name" => args.name = it.next().ok_or("--name needs a value")?,
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
        Mode::Daemon => daemon::run(args),
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
// Control panel (client of the service)
// ---------------------------------------------------------------------------

const DOUBLE_CLICK: Duration = Duration::from_millis(350);
const TICK_ACTIVE: Duration = Duration::from_millis(33);
const TICK_IDLE: Duration = Duration::from_millis(150);

struct App {
    daemon_args: daemon::Args,
    socket: PathBuf,
    client: Option<Client>,
    /// What the panel shows and edits; pushed to the service when it changes.
    settings: Settings,
    sent: Settings,
    status: Status,
    route: String,
    cfg: AppConfig,
    autostart: Option<bool>,
    /// While set, we are waiting for a freshly started service to appear.
    starting_until: Option<Instant>,
    /// Do not reconnect before this (avoids latching onto a service that is shutting down).
    quiet_until: Instant,
    last_connect_try: Instant,

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

    fn connected(&self) -> bool {
        self.client.is_some()
    }

    fn disconnect(&mut self) {
        self.client = None;
        self.status = Status::Stopped;
    }

    /// Connect to a running service and adopt its state.
    fn try_connect(&mut self) -> bool {
        self.last_connect_try = Instant::now();
        let Ok(mut c) = Client::connect(&self.socket) else { return false };
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
        self.starting_until = None;
        true
    }

    fn start_service(&mut self) {
        settings::save(&self.settings); // the service loads these when it starts
        match service::spawn_daemon(&self.daemon_args.to_cli()) {
            Ok(()) => {
                self.starting_until = Some(Instant::now() + Duration::from_secs(8));
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
        self.starting_until = None;
        self.quiet_until = Instant::now() + Duration::from_secs(2);
    }

    fn toggle_service(&mut self) {
        if self.connected() || matches!(self.status, Status::Running) {
            self.stop_service();
        } else if self.starting_until.is_none() {
            self.start_service();
        }
    }

    fn set_autostart(&mut self, on: bool) {
        if self.autostart.is_none() {
            self.note = "no systemd user session available".into();
            return;
        }
        match service::set_autostart(on) {
            Ok(()) => self.note.clear(),
            Err(e) => self.note = e,
        }
        self.autostart = service::unit_enabled();
    }

    fn set_keep_running(&mut self, on: bool) {
        self.cfg.keep_running = on;
        settings::save_app(&self.cfg);
    }

    fn apply_switch(&mut self, which: SettingBtn, on: bool) {
        match which {
            SettingBtn::Autostart => self.set_autostart(on),
            SettingBtn::KeepRunning => self.set_keep_running(on),
        }
    }

    /// Push local edits to the service.
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
                Some(Target::Service) => app.toggle_service(),
                Some(Target::Switch(which, on)) => app.apply_switch(which, on),
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
                Some(Target::Service) => app.toggle_service(),
                Some(Target::Switch(which, on)) => app.apply_switch(which, on),
                Some(Target::OpenSettings) => {
                    app.settings_open = true;
                    app.hover = None;
                }
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
            KeyCode::Char('s') => app.toggle_service(),
            KeyCode::Char('a') => {
                let on = !app.autostart.unwrap_or(false);
                app.set_autostart(on);
            }
            KeyCode::Char('p') => {
                let on = !app.cfg.keep_running;
                app.set_keep_running(on);
            }
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
        KeyCode::Char(',') => app.settings_open = true,
        KeyCode::Char('s') => app.toggle_service(),
        KeyCode::Char('a') => {
            let on = !app.autostart.unwrap_or(false);
            app.set_autostart(on);
        }
        KeyCode::Char('p') => {
            let on = !app.cfg.keep_running;
            app.set_keep_running(on);
        }
        _ => {}
    }
    ControlFlow::Continue(())
}

fn run_tui(args: daemon::Args) -> std::io::Result<Option<String>> {
    let socket = ipc::socket_path(&args.name);
    let mut app = App {
        daemon_args: args,
        socket,
        client: None,
        settings: settings::load(),
        sent: settings::load(),
        status: Status::Stopped,
        route: String::new(),
        cfg: settings::load_app(),
        autostart: service::systemd_available().then(|| service::unit_enabled().unwrap_or(false)),
        starting_until: None,
        quiet_until: Instant::now(),
        last_connect_try: Instant::now(),
        selected: 1,
        hover: None,
        drag: None,
        last_click: None,
        style: VizStyle::Spectrum,
        note: String::new(),
        settings_open: false,
    };

    // Attach to the running service, or start one.
    if !app.try_connect() {
        app.start_service();
    }

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

    // Closing the panel: stop the service only if the user asked for that.
    let message = if app.connected() {
        if app.cfg.keep_running {
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

            match app.client.as_mut().map(Client::poll) {
                Some(Ok(frame)) => {
                    app.status = frame.status;
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
                        // The service went away (stopped elsewhere, crashed, restarted).
                        app.disconnect();
                    }
                    meters.update([0.0; 5], 1.5);
                    if !analyzer.is_silent() {
                        analyzer.update(None);
                    }
                    let waiting = app.starting_until.is_some_and(|t| Instant::now() < t);
                    let due = app.last_connect_try.elapsed() >= if waiting { Duration::from_millis(250) } else { Duration::from_secs(1) };
                    if due && Instant::now() >= app.quiet_until && app.try_connect() {
                        dirty = true;
                    }
                    if app.client.is_none() {
                        if let Some(deadline) = app.starting_until {
                            if Instant::now() >= deadline {
                                app.starting_until = None;
                                app.status = Status::Stopped;
                                app.note = "service did not start · see ~/.local/state/liteeq/daemon.log".into();
                            }
                        }
                    }
                }
            }
            dirty |= !analyzer.is_silent() || !meters.is_idle();
        }

        app.sync_settings();

        let key = (app.status.clone(), app.autostart, app.note.clone());
        if last_drawn.as_ref() != Some(&key) {
            dirty = true;
            last_drawn = Some(key);
        }
        if dirty {
            dirty = false;
            terminal.draw(|f| {
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
                    autostart: app.autostart,
                    keep_running: app.cfg.keep_running,
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
