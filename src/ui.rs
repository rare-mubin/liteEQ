//! Full-screen terminal UI in a btop-like style: rounded panels whose titles,
//! buttons and hints live in the border itself. Every clickable element registers
//! a hitbox while it is drawn, so mouse handling in `main` never repeats layout maths.

use crate::remote::Status;
use crate::dsp::lin_to_db;
use crate::settings::{Param, Settings};
use crate::viz::{self, Analyzer, VizStyle};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, BorderType, Clear};

const ACCENT: Color = Color::Rgb(124, 108, 220);
const ACCENT_HI: Color = Color::Rgb(164, 150, 250);
const PINK: Color = Color::Rgb(236, 110, 190);
const GREEN: Color = Color::Rgb(110, 220, 150);
const RED: Color = Color::Rgb(240, 90, 90);
const TEXT: Color = Color::Rgb(214, 214, 230);
const DIM: Color = Color::Rgb(96, 98, 124);
const TRACK: Color = Color::Rgb(58, 60, 84);
const SEL_BG: Color = Color::Rgb(34, 32, 54);
const BTN_BG: Color = Color::Rgb(46, 46, 66);
const BTN_HOVER: Color = Color::Rgb(72, 70, 104);
/// Border colours of the three panels.
const B_VIZ: Color = Color::Rgb(112, 98, 200);
const B_CTRL: Color = Color::Rgb(190, 98, 168);
const B_SET: Color = Color::Rgb(78, 160, 150);
const B_POPUP: Color = Color::Rgb(164, 150, 250);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Toggle {
    Listen,
    Floor,
    Bypass,
}

/// App-level switches shown in the settings panel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SettingBtn {
    Autostart,
    KeepRunning,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    /// The draggable track of a parameter.
    Slider(Param),
    /// Its name or value: selects the row.
    Label(Param),
    /// The `-` / `+` buttons (direction -1 / +1).
    Step(Param, i8),
    Toggle(Toggle),
    Style(VizStyle),
    /// Start / stop the background service.
    Service,
    /// An ON/OFF switch; the bool is the state that half of the switch selects.
    Switch(SettingBtn, bool),
    /// The `Settings` button and the popup's close chip / backdrop.
    OpenSettings,
    CloseSettings,
    Backdrop,
    Viz,
    /// Windows: install liteEQ into Equalizer APO / take it out again.
    EapoInstall,
    EapoRemove,
    /// Equalizer APO device choice (index into `EapoView::labels`; 0 = all).
    EapoDevice(u8),
}

#[derive(Default)]
pub struct Hits(Vec<(Rect, Target)>);

impl Hits {
    fn add(&mut self, rect: Rect, target: Target) {
        self.0.push((rect, target));
    }

    /// Topmost element under the cell (later registrations win).
    pub fn at(&self, x: u16, y: u16) -> Option<Target> {
        self.0
            .iter()
            .rev()
            .find(|(r, _)| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height)
            .map(|(_, t)| *t)
    }

    pub fn slider_rect(&self, p: Param) -> Option<Rect> {
        self.0.iter().find(|(_, t)| *t == Target::Slider(p)).map(|(r, _)| *r)
    }
}

/// Meter values in dB, with peak-fall ballistics.
#[derive(Clone, Copy)]
pub struct MeterView {
    pub in_l: f32,
    pub in_r: f32,
    pub out_l: f32,
    pub out_r: f32,
    pub harmonics: f32,
}

impl Default for MeterView {
    fn default() -> Self {
        Self { in_l: -120.0, in_r: -120.0, out_l: -120.0, out_r: -120.0, harmonics: -120.0 }
    }
}

impl MeterView {
    /// True once every meter has fallen to nothing (nothing left to animate).
    pub fn is_idle(&self) -> bool {
        [self.in_l, self.in_r, self.out_l, self.out_r, self.harmonics].iter().all(|d| *d < -100.0)
    }

    /// Feed new linear peaks; readings fall at `fall_db` per call.
    pub fn update(&mut self, peaks: [f32; 5], fall_db: f32) {
        let fields = [&mut self.in_l, &mut self.in_r, &mut self.out_l, &mut self.out_r, &mut self.harmonics];
        for (f, p) in fields.into_iter().zip(peaks) {
            *f = lin_to_db(p).max(*f - fall_db);
        }
    }
}

pub struct View<'a> {
    pub settings: &'a Settings,
    pub selected: usize,
    pub meters: MeterView,
    pub status: &'a Status,
    pub route: &'a str,
    pub note: &'a str,
    pub hover: Option<Target>,
    pub viz_style: VizStyle,
    pub analyzer: &'a Analyzer,
    /// `None` when start-at-login cannot be managed (e.g. no systemd user session).
    pub autostart: Option<bool>,
    pub keep_running: bool,
    pub settings_open: bool,
    /// Controlling the Equalizer APO plugin rather than the service.
    pub plugin: bool,
    /// Equalizer APO is installed (Windows): offers Install, or the device choice.
    pub eapo: Option<EapoView<'a>>,
}

#[derive(Clone, Copy)]
pub struct EapoView<'a> {
    /// "All" first, then each output device Equalizer APO is on.
    pub labels: &'a [String],
    /// Which of `labels` the plugin applies to (`None`: a custom pattern).
    pub selected: Option<usize>,
    /// liteEQ is in Equalizer APO's config.
    pub set_up: bool,
}

pub const MIN_W: u16 = 40;
/// From `Cargo.toml`; bump it there.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const PANEL_H: u16 = 9; // 7 parameter rows + borders

fn text(f: &mut Frame, x: u16, y: u16, s: &str, style: Style) -> u16 {
    let area = f.area();
    if y >= area.y + area.height || x >= area.x + area.width {
        return 0;
    }
    let max = (area.x + area.width - x) as usize;
    let shown: String = s.chars().take(max).collect();
    let w = shown.chars().count() as u16;
    f.buffer_mut().set_string(x, y, &shown, style);
    w
}

fn fill(f: &mut Frame, r: Rect, style: Style) {
    f.buffer_mut().set_style(r, style);
}

/// A rounded panel; returns the area inside the border.
fn panel(f: &mut Frame, r: Rect, color: Color) -> Rect {
    let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(color));
    let inner = block.inner(r);
    f.render_widget(block, r);
    inner
}

/// A label sitting in a panel border: `┤ label ├`. The first `hot.0` characters
/// can use a highlight style (like btop's red hotkey letters).
struct Chip {
    label: String,
    style: Style,
    hot: Option<(usize, Style)>,
    target: Option<Target>,
}

impl Chip {
    fn new(label: impl Into<String>, style: Style) -> Chip {
        Chip { label: label.into(), style, hot: None, target: None }
    }

    fn hot(mut self, n: usize, style: Style) -> Chip {
        self.hot = Some((n, style));
        self
    }

    fn click(mut self, t: Target) -> Chip {
        self.target = Some(t);
        self
    }

    fn width(&self) -> u16 {
        self.label.chars().count() as u16 + 4
    }
}

fn chips_width(chips: &[Chip]) -> u16 {
    chips.iter().map(Chip::width).sum()
}

fn draw_chips(f: &mut Frame, hits: &mut Hits, mut x: u16, y: u16, border: Color, chips: &[Chip]) -> u16 {
    let edge = Style::new().fg(border);
    for c in chips {
        text(f, x, y, "┤", edge);
        let lx = x + 1;
        let n = c.label.chars().count() as u16;
        text(f, lx, y, " ", c.style);
        let (hot_n, hot_style) = c.hot.unwrap_or((0, c.style));
        let head: String = c.label.chars().take(hot_n).collect();
        let tail: String = c.label.chars().skip(hot_n).collect();
        let w = text(f, lx + 1, y, &head, hot_style);
        text(f, lx + 1 + w, y, &tail, c.style);
        text(f, lx + 1 + n, y, " ", c.style);
        text(f, lx + 2 + n, y, "├", edge);
        if let Some(t) = c.target {
            hits.add(Rect::new(lx, y, n + 2, 1), t);
        }
        x += c.width();
    }
    x
}

fn chip_style(on: bool, hover: bool) -> Style {
    match (on, hover) {
        (true, true) => Style::new().fg(Color::Black).bg(ACCENT_HI).bold(),
        (true, false) => Style::new().fg(Color::Black).bg(ACCENT).bold(),
        (false, true) => Style::new().fg(Color::White).bg(BTN_HOVER),
        (false, false) => Style::new().fg(TEXT),
    }
}

pub fn draw(f: &mut Frame, v: &View) -> Hits {
    let mut hits = Hits::default();
    let area = f.area();
    let wide = area.width >= 100;
    let need_h = if wide { 4 + PANEL_H } else { 3 + PANEL_H + 7 };
    if area.width < MIN_W || area.height < need_h {
        text(f, area.x, area.y, "terminal too small", Style::new().fg(PINK));
        return hits;
    }

    let (viz_r, ctrl_r, lvl_r) = if wide {
        let rows = Layout::vertical([Constraint::Min(4), Constraint::Length(PANEL_H)]).split(area);
        let cols = Layout::horizontal([Constraint::Percentage(60), Constraint::Min(30)]).split(rows[1]);
        (rows[0], cols[0], cols[1])
    } else {
        let rows = Layout::vertical([Constraint::Min(3), Constraint::Length(PANEL_H), Constraint::Length(7)]).split(area);
        (rows[0], rows[1], rows[2])
    };

    // While the settings popup is open the panels behind it are drawn dimmed and
    // do not register hitboxes, so clicks cannot reach them.
    let mut behind = Hits::default();
    {
        let h = if v.settings_open { &mut behind } else { &mut hits };
        draw_viz(f, viz_r, v, h);
        draw_controls(f, ctrl_r, v, h);
        draw_levels(f, lvl_r, v);
    }
    if v.settings_open {
        f.buffer_mut().set_style(area, Style::new().add_modifier(Modifier::DIM));
        hits.add(area, Target::Backdrop);
        draw_settings_popup(f, area, v, &mut hits);
    }
    hits
}

/// Why the plugin is not running (Windows): Equalizer APO missing, not set up, or just idle.
fn plugin_stopped(v: &View) -> (&'static str, &'static str) {
    match v.eapo {
        None => ("no Equalizer APO", "Equalizer APO is not installed · see Settings (,)"),
        Some(e) if !e.set_up => ("not set up", "not set up · open Settings (,) and press Install"),
        Some(_) => ("plugin idle", "plugin idle · play something"),
    }
}

fn status_parts(status: &Status, v: Option<&View>) -> (&'static str, String, Color) {
    match status {
        Status::Starting => ("◌", "starting".into(), Color::Yellow),
        Status::Running => ("●", "running".into(), GREEN),
        Status::Stopped if v.is_some_and(|v| v.plugin) => ("○", plugin_stopped(v.unwrap()).0.into(), Color::Yellow),
        Status::Stopped => ("○", "service stopped".into(), RED),
        Status::Error(e) => ("●", format!("error: {e}"), RED),
    }
}

fn draw_viz(f: &mut Frame, r: Rect, v: &View, hits: &mut Hits) {
    let inner = panel(f, r, B_VIZ);
    let s = v.settings;

    // Visualizer fills the panel; a click cycles the view.
    hits.add(inner, Target::Viz);
    let empty = match v.status {
        Status::Stopped if v.plugin => plugin_stopped(v).1,
        Status::Stopped => "service stopped · press s to start",
        _ => "waiting for audio",
    };
    viz::draw(f.buffer_mut(), inner, v.analyzer, v.viz_style, s.scope, s.floor_active.then_some(s.floor), empty);

    // --- top border: brand, view buttons and Settings on the left; route + status
    // on the right. On narrow terminals the view buttons drop out (a click on the
    // graph or `v` still cycles the view) and the status shrinks to a dot.
    let (dot, label, color) = status_parts(v.status, Some(v));
    let status_full = Chip::new(format!("{dot} {label}"), Style::new().fg(color));
    let status_dot = Chip::new(dot, Style::new().fg(color));
    let brand = || {
        let label = if r.width >= 100 { format!("liteEQ v{VERSION}") } else { "liteEQ".to_string() };
        Chip::new(label, Style::new().fg(Color::Black).bg(ACCENT).bold())
    };
    let settings_chip = || Chip::new("Settings", chip_style(v.settings_open, v.hover == Some(Target::OpenSettings))).click(Target::OpenSettings);
    let mut left = vec![brand()];
    for st in VizStyle::ALL {
        let on = v.viz_style == st;
        let hover = v.hover == Some(Target::Style(st));
        left.push(Chip::new(st.label(), chip_style(on, hover)).click(Target::Style(st)));
    }
    left.push(settings_chip());
    if chips_width(&left) + status_dot.width() + 6 > r.width {
        left = vec![brand(), settings_chip()];
    }
    let x = draw_chips(f, hits, r.x + 2, r.y, B_VIZ, &left);

    let status_chip = if x + status_full.width() + 6 <= r.x + r.width { status_full } else { status_dot };
    let room = (r.x + r.width).saturating_sub(x + status_chip.width() + 6) as usize;
    let mut right = Vec::new();
    if room > 12 && !v.route.is_empty() {
        let route: String = if v.route.chars().count() > room {
            v.route.chars().take(room - 1).chain(std::iter::once('…')).collect()
        } else {
            v.route.to_string()
        };
        right.push(Chip::new(route, Style::new().fg(DIM)));
    }
    right.push(status_chip);
    let rx = (r.x + r.width).saturating_sub(chips_width(&right) + 2);
    draw_chips(f, hits, rx, r.y, B_VIZ, &right);

    // --- bottom border: frequency axis, legend
    let by = r.y + r.height - 1;
    let key = |label: &str, color: Color| Chip::new(label, Style::new().fg(DIM)).hot(1, Style::new().fg(color));
    let legend = [key("▪ in", Color::Rgb(90, 96, 130)), key("▪ out", PINK), key("▪ peak", Color::White)];
    let legend_x = (r.x + r.width).saturating_sub(chips_width(&legend) + 2);
    let mut last_end = r.x + 1;
    for (cx, label) in viz::axis_labels(inner.width) {
        let len = label.len() as u16;
        let x = (inner.x + cx.saturating_sub(len / 2)).max(r.x + 2);
        if x > last_end && x + len < legend_x.saturating_sub(1) && x + len < r.x + r.width - 2 {
            text(f, x, by, label, Style::new().fg(DIM));
            last_end = x + len + 1;
        }
    }
    if legend_x > last_end + 2 {
        draw_chips(f, hits, legend_x, by, B_VIZ, &legend);
    }
}

fn draw_levels(f: &mut Frame, r: Rect, v: &View) {
    let inner = panel(f, r, B_SET);
    let mut none = Hits::default();
    draw_chips(f, &mut none, r.x + 2, r.y, B_SET, &[Chip::new("levels", Style::new().fg(TEXT).bold())]);
    let by = r.y + r.height - 1;
    draw_chips(f, &mut none, r.x + 2, by, B_SET, &[Chip::new("peak · dB", Style::new().fg(DIM))]);

    let m = v.meters;
    const LABEL_W: u16 = 10;
    let bar_w = inner.width.saturating_sub(2 + LABEL_W + 8).max(4);
    // Grouped as inputs / outputs / harmonics when there is room for the gaps.
    let rows: &[Option<(&str, f32)>] = if inner.height >= 7 {
        &[Some(("Input L", m.in_l)), Some(("Input R", m.in_r)), None, Some(("Output L", m.out_l)), Some(("Output R", m.out_r)), None, Some(("Harmonics", m.harmonics))]
    } else {
        &[Some(("Input L", m.in_l)), Some(("Input R", m.in_r)), Some(("Output L", m.out_l)), Some(("Output R", m.out_r)), Some(("Harmonics", m.harmonics))]
    };
    for (i, row) in rows.iter().enumerate() {
        let y = inner.y + i as u16;
        let Some((name, db)) = row else { continue };
        if y >= inner.y + inner.height {
            break;
        }
        let x0 = inner.x + 1;
        text(f, x0, y, &format!("{name:<w$}", w = LABEL_W as usize), Style::new().fg(TEXT));
        // -60..0 dB, eighth-block resolution.
        let t = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
        let eighths = (t * bar_w as f32 * 8.0).round() as u32;
        const PART: [&str; 9] = [" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];
        for c in 0..bar_w {
            let cell = eighths.saturating_sub(c as u32 * 8).min(8) as usize;
            let style = Style::new().fg(meter_color((c as f32 + 0.5) / bar_w as f32)).bg(Color::Rgb(30, 30, 46));
            text(f, x0 + LABEL_W + c, y, PART[cell], style);
        }
        let style = Style::new().fg(if *db > -3.0 { RED } else { DIM });
        text(f, x0 + LABEL_W + bar_w + 1, y, &format!("{:>4.0} dB", db.max(-99.0)), style);
    }
}

/// Colour along a meter: violet -> pink, red in the last few percent.
fn meter_color(t: f32) -> Color {
    let lerp = |a: u8, b: u8, u: f32| (a as f32 + (b as f32 - a as f32) * u).round() as u8;
    if t > 0.95 {
        return RED;
    }
    let u = (t / 0.95).clamp(0.0, 1.0);
    Color::Rgb(lerp(110, 236, u), lerp(96, 110, u), lerp(230, 190, u))
}

fn value_text(p: Param, s: &Settings) -> String {
    let v = p.get(s);
    match p {
        Param::Blend => format!("{v:+.1}"),
        Param::Amount if v <= Settings::AMOUNT_MIN_DB => "off".into(),
        Param::Amount | Param::Input | Param::Output => format!("{v:.1} dB"),
        Param::Harmonics => format!("{v:.1}"),
        Param::Scope | Param::Floor => format!("{v:.0} Hz"),
    }
}

fn draw_controls(f: &mut Frame, r: Rect, v: &View, hits: &mut Hits) {
    let inner = panel(f, r, B_CTRL);
    let s = v.settings;

    // --- top border: title + the three toggles
    let toggle = |name: &str, on: bool, t: Toggle| {
        let hover = v.hover == Some(Target::Toggle(t));
        let base = chip_style(on, hover);
        Chip::new(name, base).hot(1, if on { base } else { Style::new().fg(PINK).bold() }).click(Target::Toggle(t))
    };
    let chips = [
        Chip::new("controls", Style::new().fg(TEXT).bold()),
        toggle("Listen", s.listen, Toggle::Listen),
        toggle("Floor", s.floor_active, Toggle::Floor),
        toggle("Bypass", s.bypass, Toggle::Bypass),
    ];
    draw_chips(f, hits, r.x + 2, r.y, B_CTRL, &chips);

    // --- bottom border: key hints, as many as fit
    let hint = |label: &str, keys: usize| Chip::new(label, Style::new().fg(DIM)).hot(keys, Style::new().fg(ACCENT_HI).bold());
    let mut x = r.x + 2;
    let by = r.y + r.height - 1;
    for chip in [hint("↑↓ select", 2), hint("←→ adjust", 2), hint("r reset", 1), hint("R all", 1), hint("v view", 1), hint(", settings", 1), hint("q quit", 1)] {
        if x + chip.width() + 2 > r.x + r.width {
            break;
        }
        x = draw_chips(f, hits, x, by, B_CTRL, &[chip]);
    }

    // --- parameter rows
    const LABEL_W: u16 = 14;
    let bar_w = inner.width.saturating_sub(2 + LABEL_W + 1 + 3 + 1 + 8 + 1 + 3 + 1).max(6);
    for (i, p) in Param::ALL.iter().copied().enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.y + inner.height {
            break;
        }
        let sel = i == v.selected;
        let dimmed = (p == Param::Floor && !s.floor_active) || (s.bypass && !matches!(p, Param::Input | Param::Output));
        if sel {
            fill(f, Rect::new(inner.x, y, inner.width, 1), Style::new().bg(SEL_BG));
        }
        let bg = if sel { SEL_BG } else { Color::Reset };
        let mut x = inner.x;
        x += text(f, x, y, if sel { "▸ " } else { "  " }, Style::new().fg(ACCENT_HI).bg(bg));

        let name = match p {
            Param::Blend => "Blend 3rd↔2nd",
            Param::Amount => "Amount",
            Param::Harmonics => "Harmonics",
            Param::Scope => "Scope",
            Param::Floor => "Floor",
            Param::Input => "Input",
            Param::Output => "Output",
        };
        let name_style = Style::new().fg(if dimmed { DIM } else { TEXT }).bg(bg);
        text(f, x, y, &format!("{name:<w$}", w = LABEL_W as usize), if sel { name_style.bold() } else { name_style });
        hits.add(Rect::new(x, y, LABEL_W, 1), Target::Label(p));
        x += LABEL_W;

        // Slider track.
        let hover_slider = v.hover == Some(Target::Slider(p));
        let fill_col = if dimmed { DIM } else if hover_slider { ACCENT_HI } else { ACCENT };
        let knob = (p.to_frac(p.get(s)) * (bar_w - 1) as f32).round() as u16;
        let origin = (p.origin_frac() * (bar_w - 1) as f32).round() as u16;
        let (lo, hi) = (knob.min(origin), knob.max(origin));
        for c in 0..bar_w {
            let (ch, style) = if c == knob {
                ("●", Style::new().fg(if hover_slider || sel { Color::White } else { fill_col }).bg(bg).bold())
            } else if c >= lo && c <= hi {
                ("━", Style::new().fg(fill_col).bg(bg))
            } else if c == origin && origin > 0 {
                ("┃", Style::new().fg(DIM).bg(bg))
            } else {
                ("─", Style::new().fg(TRACK).bg(bg))
            };
            text(f, x + c, y, ch, style);
        }
        hits.add(Rect::new(x, y, bar_w, 1), Target::Slider(p));
        x += bar_w + 1;

        // [-]  value  [+]
        let step_style = |dir: i8| {
            let hot = v.hover == Some(Target::Step(p, dir));
            Style::new().fg(if hot { Color::White } else { TEXT }).bg(if hot { BTN_HOVER } else { BTN_BG })
        };
        text(f, x, y, "[−]", step_style(-1));
        hits.add(Rect::new(x, y, 3, 1), Target::Step(p, -1));
        x += 4;
        let vstyle = Style::new().fg(if dimmed { DIM } else { Color::White }).bg(bg);
        text(f, x, y, &format!("{:>8}", value_text(p, s)), if sel { vstyle.bold() } else { vstyle });
        hits.add(Rect::new(x, y, 8, 1), Target::Label(p));
        x += 9;
        text(f, x, y, "[+]", step_style(1));
        hits.add(Rect::new(x, y, 3, 1), Target::Step(p, 1));
    }
}

/// `[ON][off]` / `[on][OFF]` switch; returns its width.
fn switch(f: &mut Frame, hits: &mut Hits, x: u16, y: u16, on: bool, which: SettingBtn, v: &View) -> u16 {
    let seg = |label: &str, active: bool, colour: Color, value: bool, x: u16, f: &mut Frame, hits: &mut Hits| {
        let hover = v.hover == Some(Target::Switch(which, value));
        let style = match (active, hover) {
            (true, _) => Style::new().fg(Color::Black).bg(colour).bold(),
            (false, true) => Style::new().fg(Color::White).bg(BTN_HOVER),
            (false, false) => Style::new().fg(DIM).bg(BTN_BG),
        };
        let w = text(f, x, y, label, style);
        hits.add(Rect::new(x, y, w, 1), Target::Switch(which, value));
        w
    };
    let w1 = seg(" ON ", on, GREEN, true, x, f, hits);
    let w2 = seg(" OFF ", !on, RED, false, x + w1, f, hits);
    w1 + w2
}

/// A solid button; returns its width.
fn button(f: &mut Frame, hits: &mut Hits, x: u16, y: u16, label: &str, colour: Color, target: Target, v: &View) -> u16 {
    let hot = v.hover == Some(target);
    let style = if hot { Style::new().fg(Color::Black).bg(Color::White).bold() } else { Style::new().fg(Color::Black).bg(colour).bold() };
    let w = text(f, x, y, label, style);
    hits.add(Rect::new(x, y, w, 1), target);
    w
}

/// `[ All ][ MOONDROP Dawn Pro ]…`: the Equalizer APO device choice, clipped to `max_x`.
fn device_choice(f: &mut Frame, hits: &mut Hits, mut x: u16, y: u16, max_x: u16, e: &EapoView, v: &View) {
    for (i, label) in e.labels.iter().enumerate() {
        let target = Target::EapoDevice(i as u8);
        let seg = format!(" {label} ");
        let w = seg.chars().count() as u16;
        if x + w > max_x {
            text(f, x, y, "…", Style::new().fg(DIM));
            break;
        }
        let style = match (e.selected == Some(i), v.hover == Some(target)) {
            (true, _) => Style::new().fg(Color::Black).bg(GREEN).bold(),
            (false, true) => Style::new().fg(Color::White).bg(BTN_HOVER),
            (false, false) => Style::new().fg(DIM).bg(BTN_BG),
        };
        text(f, x, y, &seg, style);
        hits.add(Rect::new(x, y, w, 1), target);
        x += w + 1;
    }
}

/// The settings window: a centred modal over everything else.
fn draw_settings_popup(f: &mut Frame, area: Rect, v: &View, hits: &mut Hits) {
    let w = 64.min(area.width.saturating_sub(4));
    let h = 12.min(area.height.saturating_sub(2));
    let r = Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h);
    f.render_widget(Clear, r);
    let inner = panel(f, r, B_POPUP);
    hits.add(r, Target::Viz); // swallow clicks on the window itself (not the backdrop)

    // Top border: title left, close chip right. Bottom border: note or key hints.
    draw_chips(
        f,
        hits,
        r.x + 2,
        r.y,
        B_POPUP,
        &[Chip::new("Settings", Style::new().fg(TEXT).bold()), Chip::new(format!("v{VERSION}"), Style::new().fg(DIM))],
    );
    let close = Chip::new("close", chip_style(false, v.hover == Some(Target::CloseSettings))).hot(0, Style::new()).click(Target::CloseSettings);
    let cx = (r.x + r.width).saturating_sub(close.width() + 2);
    draw_chips(f, hits, cx, r.y, B_POPUP, &[close]);

    let by = r.y + r.height - 1;
    if !v.note.is_empty() {
        let max = r.width.saturating_sub(8) as usize;
        let note: String = v.note.chars().take(max).collect();
        draw_chips(f, hits, r.x + 2, by, B_POPUP, &[Chip::new(note, Style::new().fg(Color::Yellow))]);
    } else {
        let hint = |label: &str, keys: usize| Chip::new(label, Style::new().fg(DIM)).hot(keys, Style::new().fg(ACCENT_HI).bold());
        let mut x = r.x + 2;
        let hints = if v.plugin {
            match v.eapo {
                Some(e) if e.set_up => vec![hint("d device", 1), hint("u remove", 1), hint("esc close", 3)],
                Some(_) => vec![hint("e install", 1), hint("esc close", 3)],
                None => vec![hint("esc close", 3)],
            }
        } else {
            vec![hint("s service", 1), hint("a login", 1), hint("p keep", 1), hint("esc close", 3)]
        };
        for chip in hints {
            if x + chip.width() + 2 > r.x + r.width {
                break;
            }
            x = draw_chips(f, hits, x, by, B_POPUP, &[chip]);
        }
    }

    let lx = inner.x + 2;
    const LABEL_W: u16 = 16;
    let label = |f: &mut Frame, y: u16, s: &str| {
        text(f, lx, y, &format!("{s:<w$}", w = LABEL_W as usize), Style::new().fg(TEXT));
    };
    let hint = |f: &mut Frame, y: u16, s: &str| {
        let max = inner.width.saturating_sub(5) as usize;
        let shown: String = s.chars().take(max).collect();
        text(f, lx + 1, y, &shown, Style::new().fg(DIM));
    };
    let vx = lx + LABEL_W;

    if v.plugin && !v.eapo.is_some_and(|e| e.set_up) {
        // Windows, not set up yet: only Equalizer APO matters here.
        let y = inner.y + 1;
        label(f, y, "Equalizer APO");
        if v.eapo.is_some() {
            text(f, vx, y, "found", Style::new().fg(TEXT));
            button(f, hits, vx + 7, y, " Install ", GREEN, Target::EapoInstall, v);
            hint(f, y + 1, "adds liteEQ to it: no added latency, no cable");
        } else {
            text(f, vx, y, "not installed", Style::new().fg(RED));
            hint(f, y + 1, "get it at sourceforge.net/projects/equalizerapo,");
            hint(f, y + 2, "enable it for your output in its Configurator,");
            hint(f, y + 3, "then open these settings again");
        }
        return;
    }

    if v.plugin {
        // Equalizer APO runs the effect: nothing to start, stop or autostart.
        let y = inner.y + 1;
        label(f, y, "Effect");
        let (dot, word, color) = match v.status {
            Status::Running => ("●", "active in Equalizer APO", GREEN),
            _ => ("○", "idle (no audio, or not loaded)", Color::Yellow),
        };
        text(f, vx, y, &format!("{dot} {word}"), Style::new().fg(color));
        hint(f, y + 1, "always on inside Windows audio; the app is a remote");
        let y = inner.y + 4;
        label(f, y, "Device");
        match &v.eapo {
            Some(e) if !e.labels.is_empty() => device_choice(f, hits, vx, y, inner.x + inner.width - 1, e, v),
            _ => {
                text(f, vx, y, "custom (see config\\liteeq.txt)", Style::new().fg(DIM));
            }
        }
        hint(f, y + 1, "the outputs that get the effect");
        let y = inner.y + 7;
        label(f, y, "Equalizer APO");
        text(f, vx, y, "installed", Style::new().fg(GREEN));
        button(f, hits, vx + 11, y, " Remove ", PINK, Target::EapoRemove, v);
        hint(f, y + 1, "takes liteEQ out of Equalizer APO's config");
        return;
    }

    // Service: state + Start/Stop button on the right.
    let y = inner.y + 1;
    label(f, y, "Service");
    let (dot, word, color) = status_parts(v.status, None);
    let word = match v.status {
        Status::Error(_) => "failed".to_string(),
        Status::Stopped => "stopped".to_string(),
        _ => word,
    };
    text(f, vx, y, &format!("{dot} {word}"), Style::new().fg(color));
    let (btn_label, active) = match v.status {
        Status::Running => (" Stop ", true),
        Status::Starting => (" ... ", false),
        _ => (" Start ", true),
    };
    let bw = btn_label.chars().count() as u16;
    let bx = (inner.x + inner.width).saturating_sub(bw + 2);
    if bx > vx + 12 {
        let hover = active && v.hover == Some(Target::Service);
        let style = match (matches!(v.status, Status::Running), active, hover) {
            (_, false, _) => Style::new().fg(DIM).bg(BTN_BG),
            (true, _, false) => Style::new().fg(Color::Black).bg(PINK).bold(),
            (true, _, true) => Style::new().fg(Color::Black).bg(Color::Rgb(255, 150, 215)).bold(),
            (false, _, false) => Style::new().fg(Color::Black).bg(GREEN).bold(),
            (false, _, true) => Style::new().fg(Color::Black).bg(Color::Rgb(160, 245, 190)).bold(),
        };
        text(f, bx, y, btn_label, style);
        if active {
            hits.add(Rect::new(bx, y, bw, 1), Target::Service);
        }
    }
    hint(f, y + 1, "runs the effect in the background; the app is a remote");

    // Start at login.
    let y = inner.y + 4;
    label(f, y, "Start at login");
    match v.autostart {
        Some(on) => {
            switch(f, hits, vx, y, on, SettingBtn::Autostart, v);
            hint(f, y + 1, "systemd user unit · liteeq.service");
        }
        None => {
            text(f, vx, y, "unavailable", Style::new().fg(DIM));
            hint(f, y + 1, "no systemd user session found");
        }
    }

    // Keep running after the app closes.
    let y = inner.y + 7;
    label(f, y, "Keep running");
    switch(f, hits, vx, y, v.keep_running, SettingBtn::KeepRunning, v);
    hint(f, y + 1, if v.keep_running { "closing the app leaves the effect playing" } else { "closing the app also stops the effect" });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn render(w: u16, h: u16, status: Status, open: bool) -> (String, usize) {
        render_with(w, h, status, open, false, false)
    }

    fn render_with(w: u16, h: u16, status: Status, open: bool, plugin: bool, eapo: bool) -> (String, usize) {
        let labels = ["All".to_string(), "MOONDROP Dawn Pro".to_string()];
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        let s = Settings { amount_db: 8.0, scope: 250.0, input_db: -8.1, floor_active: true, ..Settings::default() };
        let meters = MeterView { in_l: -21.0, in_r: -21.0, out_l: -7.0, out_r: -7.0, harmonics: -10.0 };
        let mut an = Analyzer::new();
        let tone: Vec<f32> = (0..viz::FFT_N).map(|i| 0.4 * (2.0 * std::f32::consts::PI * 60.0 * i as f32 / 48000.0).sin()).collect();
        let harm: Vec<f32> = tone.iter().enumerate().map(|(i, x)| x + 0.15 * (2.0 * std::f32::consts::PI * 120.0 * i as f32 / 48000.0).sin()).collect();
        an.update(Some((&tone, &harm)));
        let mut n_hits = 0;
        t.draw(|f| {
            let v = View {
                settings: &s,
                selected: 1,
                meters,
                status: &status,
                route: "liteeq_sink → alsa_output.usb-MOONDROP_MOONDROP_Dawn_Pro-00.analog-stereo",
                note: "",
                hover: None,
                viz_style: VizStyle::Spectrum,
                analyzer: &an,
                autostart: Some(true),
                keep_running: true,
                settings_open: open,
                plugin,
                eapo: eapo.then_some(EapoView { labels: &labels, selected: Some(1), set_up: plugin }),
            };
            n_hits = draw(f, &v).0.len();
        })
        .unwrap();
        let buf = t.backend().buffer().clone();
        let text = (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string()).collect::<Vec<_>>().join("\n");
        (text, n_hits)
    }

    #[test]
    fn plugin_mode_has_no_service_controls() {
        let (text, _) = render_with(120, 34, Status::Running, true, true, true);
        assert!(text.contains("active in Equalizer APO"), "{text}");
        assert!(text.contains(" MOONDROP Dawn Pro ") && text.contains(" Remove "), "{text}");
        assert!(!text.contains(" Stop "), "no service button");
        let (idle, _) = render_with(120, 34, Status::Stopped, false, true, true);
        assert!(idle.contains("plugin idle"), "{idle}");
    }

    #[test]
    fn windows_popup_shows_only_equalizer_apo() {
        // Windows, Equalizer APO missing: no service/login/keep rows.
        let (text, _) = render_with(120, 34, Status::Stopped, true, true, false);
        assert!(text.contains("not installed"), "{text}");
        for row in ["Service", "Start at login", "Keep running"] {
            assert!(!text.contains(row), "{row} shown:\n{text}");
        }
        assert!(text.contains("no Equalizer APO"), "status chip");
    }

    #[test]
    fn popups_fit_in_a_cramped_terminal() {
        for (w, h) in [(40, 20), (70, 24), (120, 34)] {
            render_with(w, h, Status::Stopped, true, true, false); // Windows, no Equalizer APO
            render_with(w, h, Status::Stopped, true, true, true); // Windows, Equalizer APO found
            render_with(w, h, Status::Running, true, false, false); // Linux service
        }
    }

    #[test]
    fn renders_at_many_sizes() {
        let (wide, hits) = render(120, 34, Status::Running, false);
        println!("{wide}");
        assert!(wide.contains(&format!("liteEQ v{VERSION}")), "wide layout shows the version");
        assert!(render(120, 34, Status::Running, true).0.contains(&format!("v{VERSION}")), "popup shows the version");
        assert!(hits > 25, "panels should register hitboxes, got {hits}");
        for (w, h) in [(1, 1), (10, 5), (39, 20), (40, 19), (60, 24), (95, 30), (100, 13), (100, 20), (250, 80)] {
            for st in [Status::Running, Status::Stopped, Status::Starting, Status::Error("boom".into())] {
                for open in [false, true] {
                    render(w, h, st.clone(), open);
                }
            }
        }
        println!("{}", render(70, 24, Status::Stopped, false).0);
        println!("{}", render(120, 34, Status::Running, true).0);
    }

    #[test]
    fn popup_blocks_the_panels_behind_it() {
        let (_, open_hits) = render(120, 34, Status::Running, true);
        let (_, closed_hits) = render(120, 34, Status::Running, false);
        assert!(open_hits < closed_hits, "controls must not be clickable while the popup is open");
    }

    #[test]
    fn hit_testing_finds_topmost() {
        let mut h = Hits::default();
        h.add(Rect::new(0, 0, 10, 10), Target::Viz);
        h.add(Rect::new(2, 2, 3, 1), Target::Toggle(Toggle::Listen));
        assert_eq!(h.at(3, 2), Some(Target::Toggle(Toggle::Listen)));
        assert_eq!(h.at(8, 8), Some(Target::Viz));
        assert_eq!(h.at(20, 20), None);
    }
}
