//! User-facing parameters (ranges taken from the Calf bass enhancer metadata)
//! and their on-disk persistence.

use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    pub bypass: bool,
    pub listen: bool,
    pub floor_active: bool,
    /// -10 (3rd harmonic) .. +10 (2nd harmonic)
    pub blend: f32,
    /// Gain of the generated harmonics in dB; at `AMOUNT_MIN_DB` it is fully off.
    pub amount_db: f32,
    /// "Harmonics" knob, 0.1..10
    pub drive: f32,
    /// Low-pass cut-off around the saturator, Hz
    pub scope: f32,
    /// High-pass on the generated harmonics (when `floor_active`), Hz
    pub floor: f32,
    pub input_db: f32,
    pub output_db: f32,
}

impl Settings {
    pub const AMOUNT_MIN_DB: f32 = -60.0;
}

impl Default for Settings {
    /// Calf defaults, with amount at 0 dB (linear 1.0).
    fn default() -> Self {
        Self {
            bypass: false,
            listen: false,
            floor_active: false,
            blend: 0.0,
            amount_db: 0.0,
            drive: 8.5,
            scope: 100.0,
            floor: 20.0,
            input_db: 0.0,
            output_db: 0.0,
        }
    }
}

/// The adjustable rows, in on-screen order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Param {
    Blend,
    Amount,
    Harmonics,
    Scope,
    Floor,
    Input,
    Output,
}

impl Param {
    pub const ALL: [Param; 7] =
        [Param::Blend, Param::Amount, Param::Harmonics, Param::Scope, Param::Floor, Param::Input, Param::Output];

    pub fn range(self) -> (f32, f32) {
        match self {
            Param::Blend => (-10.0, 10.0),
            Param::Amount => (Settings::AMOUNT_MIN_DB, 36.0),
            Param::Harmonics => (0.1, 10.0),
            Param::Scope => (10.0, 250.0),
            Param::Floor => (10.0, 120.0),
            Param::Input | Param::Output => (-36.0, 36.0),
        }
    }

    /// (fine, coarse) step sizes.
    pub fn steps(self) -> (f32, f32) {
        match self {
            Param::Blend => (0.1, 1.0),
            Param::Amount => (0.5, 3.0),
            Param::Harmonics => (0.1, 1.0),
            Param::Scope | Param::Floor => (1.0, 10.0),
            Param::Input | Param::Output => (0.1, 1.0),
        }
    }

    /// Position of `v` along the slider, 0..1 (frequencies are log-scaled, as in Calf).
    pub fn to_frac(self, v: f32) -> f32 {
        let (lo, hi) = self.range();
        match self {
            Param::Scope | Param::Floor => (v / lo).ln() / (hi / lo).ln(),
            _ => (v - lo) / (hi - lo),
        }
        .clamp(0.0, 1.0)
    }

    /// Value at slider position `f` (0..1), snapped to the fine step.
    pub fn from_frac(self, f: f32) -> f32 {
        let (lo, hi) = self.range();
        let f = f.clamp(0.0, 1.0);
        let v = match self {
            Param::Scope | Param::Floor => lo * (hi / lo).powf(f),
            _ => lo + f * (hi - lo),
        };
        let (step, _) = self.steps();
        let v = lo + ((v - lo) / step).round() * step;
        let v = ((v.clamp(lo, hi)) * 100.0).round() / 100.0;
        // Sticky centre for the bipolar controls.
        match self {
            Param::Blend if v.abs() < 0.25 => 0.0,
            Param::Input | Param::Output if v.abs() < 0.5 => 0.0,
            _ => v,
        }
    }

    /// Where the filled part of the slider starts: the centre for bipolar controls.
    pub fn origin_frac(self) -> f32 {
        match self {
            Param::Blend | Param::Input | Param::Output => self.to_frac(0.0),
            _ => 0.0,
        }
    }

    pub fn get(self, s: &Settings) -> f32 {
        match self {
            Param::Blend => s.blend,
            Param::Amount => s.amount_db,
            Param::Harmonics => s.drive,
            Param::Scope => s.scope,
            Param::Floor => s.floor,
            Param::Input => s.input_db,
            Param::Output => s.output_db,
        }
    }

    pub fn set(self, s: &mut Settings, v: f32) {
        let (lo, hi) = self.range();
        // Round to 0.01 so repeated 0.1 steps do not accumulate float noise.
        let v = ((v.clamp(lo, hi)) * 100.0).round() / 100.0;
        match self {
            Param::Blend => s.blend = v,
            Param::Amount => s.amount_db = v,
            Param::Harmonics => s.drive = v,
            Param::Scope => s.scope = v,
            Param::Floor => s.floor = v,
            Param::Input => s.input_db = v,
            Param::Output => s.output_db = v,
        }
    }

    pub fn nudge(self, s: &mut Settings, dir: f32, coarse: bool) {
        let (fine, big) = self.steps();
        self.set(s, self.get(s) + dir * if coarse { big } else { fine });
    }

    pub fn reset(self, s: &mut Settings) {
        self.set(s, self.get(&Settings::default()));
    }
}

fn config_path() -> Option<PathBuf> {
    // `LITEEQ_CONFIG_DIR` relocates just liteEQ's own files (used by tests).
    if let Some(dir) = std::env::var_os("LITEEQ_CONFIG_DIR") {
        return Some(PathBuf::from(dir).join("settings.conf"));
    }
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    Some(base?.join("liteeq").join("settings.conf"))
}

fn parse_flag(v: &str) -> bool {
    matches!(v, "true" | "1")
}

/// Apply one `key=value` pair; unknown keys and bad numbers are ignored.
fn apply_kv(s: &mut Settings, k: &str, v: &str) {
    match k {
        "bypass" => s.bypass = parse_flag(v),
        "listen" => s.listen = parse_flag(v),
        "floor_active" => s.floor_active = parse_flag(v),
        _ => {
            let Ok(x) = v.parse::<f32>() else { return };
            if !x.is_finite() {
                return;
            }
            match k {
                "blend" => Param::Blend.set(s, x),
                "amount_db" => Param::Amount.set(s, x),
                "harmonics" => Param::Harmonics.set(s, x),
                "scope" => Param::Scope.set(s, x),
                "floor" => Param::Floor.set(s, x),
                "input_db" => Param::Input.set(s, x),
                "output_db" => Param::Output.set(s, x),
                _ => {}
            }
        }
    }
}

impl Settings {
    /// Compact single-line form used on the service socket: `k=v;k=v;...`
    pub fn to_wire(&self) -> String {
        format!(
            "bypass={};listen={};floor_active={};blend={};amount_db={};harmonics={};scope={};floor={};input_db={};output_db={}",
            self.bypass as u8,
            self.listen as u8,
            self.floor_active as u8,
            self.blend,
            self.amount_db,
            self.drive,
            self.scope,
            self.floor,
            self.input_db,
            self.output_db
        )
    }

    pub fn from_wire(text: &str) -> Settings {
        let mut s = Settings::default();
        for pair in text.split(';') {
            if let Some((k, v)) = pair.split_once('=') {
                apply_kv(&mut s, k.trim(), v.trim());
            }
        }
        s
    }
}

pub fn load() -> Settings {
    let mut s = Settings::default();
    let Some(text) = config_path().and_then(|p| std::fs::read_to_string(p).ok()) else {
        return s;
    };
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            // Bypass is deliberately never restored from disk.
            if k.trim() != "bypass" {
                apply_kv(&mut s, k.trim(), v.trim());
            }
        }
    }
    s
}

pub fn save(s: &Settings) {
    let Some(path) = config_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = format!(
        "listen={}\nfloor_active={}\nblend={}\namount_db={}\nharmonics={}\nscope={}\nfloor={}\ninput_db={}\noutput_db={}\n",
        s.listen, s.floor_active, s.blend, s.amount_db, s.drive, s.scope, s.floor, s.input_db, s.output_db
    );
    let _ = std::fs::write(path, text);
}

/// Options of the app itself (not of the effect).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AppConfig {
    /// Leave the background service running when the TUI closes.
    pub keep_running: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self { keep_running: true }
    }
}

fn app_config_path() -> Option<PathBuf> {
    config_path().map(|p| p.with_file_name("app.conf"))
}

pub fn load_app() -> AppConfig {
    let mut c = AppConfig::default();
    if let Some(text) = app_config_path().and_then(|p| std::fs::read_to_string(p).ok()) {
        for line in text.lines() {
            if let Some(("keep_running", v)) = line.split_once('=') {
                c.keep_running = parse_flag(v.trim());
            }
        }
    }
    c
}

pub fn save_app(c: &AppConfig) {
    let Some(path) = app_config_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, format!("keep_running={}\n", c.keep_running));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_round_trips() {
        let s = Settings { bypass: true, listen: true, floor_active: true, blend: -3.5, amount_db: 8.5, drive: 4.2, scope: 180.0, floor: 33.0, input_db: -4.1, output_db: 2.0 };
        assert_eq!(Settings::from_wire(&s.to_wire()), s);
    }

    #[test]
    fn wire_ignores_junk_and_clamps() {
        let s = Settings::from_wire("scope=99999;nonsense;amount_db=abc;harmonics=nan;blend=1.5");
        assert_eq!(s.scope, 250.0);
        assert_eq!(s.blend, 1.5);
        assert_eq!(s.drive, Settings::default().drive);
        assert_eq!(s.amount_db, Settings::default().amount_db);
    }

    #[test]
    fn slider_mapping_round_trips() {
        for p in Param::ALL {
            let (lo, hi) = p.range();
            assert_eq!(p.from_frac(0.0), lo);
            assert_eq!(p.from_frac(1.0), hi);
            let mid = p.from_frac(0.5);
            assert!((p.to_frac(mid) - 0.5).abs() < 0.03, "{p:?}");
        }
    }
}
