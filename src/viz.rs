//! Dot-matrix visualizer: a small FFT, a spectrum analyser with smoothing and
//! peak-hold, and a braille-dot canvas (2x4 dots per terminal cell).
//!
//! The output spectrum is drawn in a gradient over a dim "ghost" of the input,
//! so the harmonics the enhancer adds show up as bright dots with no ghost under
//! them. No dependencies: the FFT is a plain iterative radix-2.

use crate::dsp::SAMPLE_RATE;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use std::f32::consts::PI;

pub const FFT_N: usize = 8192;
pub const F_MIN: f32 = 20.0;
pub const F_MAX: f32 = 2000.0;
const DB_FLOOR: f32 = -120.0;
const DB_TOP: f32 = -6.0;
const DB_BOTTOM: f32 = -78.0;
/// dB the bars fall per UI frame (~30 fps).
const FALL: f32 = 2.0;
const PEAK_FALL: f32 = 0.5;
const WAVE_LEN: usize = 1536;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VizStyle {
    Spectrum,
    Mirror,
    Scope,
}

impl VizStyle {
    pub const ALL: [VizStyle; 3] = [VizStyle::Spectrum, VizStyle::Mirror, VizStyle::Scope];

    pub fn next(self) -> VizStyle {
        match self {
            VizStyle::Spectrum => VizStyle::Mirror,
            VizStyle::Mirror => VizStyle::Scope,
            VizStyle::Scope => VizStyle::Spectrum,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            VizStyle::Spectrum => "Spectrum",
            VizStyle::Mirror => "Mirror",
            VizStyle::Scope => "Scope",
        }
    }
}

struct Fft {
    n: usize,
    tw_re: Vec<f32>,
    tw_im: Vec<f32>,
    rev: Vec<u32>,
}

impl Fft {
    fn new(n: usize) -> Self {
        assert!(n.is_power_of_two());
        let bits = n.trailing_zeros();
        let rev = (0..n as u32).map(|i| i.reverse_bits() >> (32 - bits)).collect();
        let (tw_re, tw_im) = (0..n / 2)
            .map(|k| {
                let a = -2.0 * PI * k as f32 / n as f32;
                (a.cos(), a.sin())
            })
            .unzip();
        Self { n, tw_re, tw_im, rev }
    }

    fn transform(&self, re: &mut [f32], im: &mut [f32]) {
        let n = self.n;
        for i in 0..n {
            let j = self.rev[i] as usize;
            if j > i {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= n {
            let half = len / 2;
            let step = n / len;
            for start in (0..n).step_by(len) {
                for k in 0..half {
                    let (wr, wi) = (self.tw_re[k * step], self.tw_im[k * step]);
                    let (u, v) = (start + k, start + k + half);
                    let tr = re[v] * wr - im[v] * wi;
                    let ti = re[v] * wi + im[v] * wr;
                    re[v] = re[u] - tr;
                    im[v] = im[u] - ti;
                    re[u] += tr;
                    im[u] += ti;
                }
            }
            len *= 2;
        }
    }
}

pub struct Analyzer {
    fft: Fft,
    window: Vec<f32>,
    re: Vec<f32>,
    im: Vec<f32>,
    bin_hz: f32,
    max_bin: usize,
    /// Smoothed level per FFT bin in dB (index = bin), input and output.
    in_db: Vec<f32>,
    out_db: Vec<f32>,
    peak_db: Vec<f32>,
    wave_in: Vec<f32>,
    wave_out: Vec<f32>,
    /// Slow auto-gain for the scope so quiet material stays visible.
    scope_peak: f32,
}

impl Analyzer {
    pub fn new() -> Self {
        let bin_hz = SAMPLE_RATE as f32 / FFT_N as f32;
        let max_bin = (F_MAX / bin_hz).ceil() as usize + 2;
        // 4-term Blackman-Harris: ~-92 dB sidelobes keep a pure tone from smearing.
        let window = (0..FFT_N)
            .map(|i| {
                let t = 2.0 * PI * i as f32 / FFT_N as f32;
                0.35875 - 0.48829 * t.cos() + 0.14128 * (2.0 * t).cos() - 0.01168 * (3.0 * t).cos()
            })
            .collect();
        Self {
            fft: Fft::new(FFT_N),
            window,
            re: vec![0.0; FFT_N],
            im: vec![0.0; FFT_N],
            bin_hz,
            max_bin,
            in_db: vec![DB_FLOOR; max_bin + 1],
            out_db: vec![DB_FLOOR; max_bin + 1],
            peak_db: vec![DB_FLOOR; max_bin + 1],
            wave_in: vec![0.0; WAVE_LEN],
            wave_out: vec![0.0; WAVE_LEN],
            scope_peak: 0.05,
        }
    }

    fn spectrum(&mut self, samples: &[f32], into: &mut [f32]) {
        let off = samples.len().saturating_sub(FFT_N);
        let tail = &samples[off..];
        let pad = FFT_N - tail.len();
        for i in 0..FFT_N {
            self.re[i] = if i < pad { 0.0 } else { tail[i - pad] * self.window[i] };
            self.im[i] = 0.0;
        }
        self.fft.transform(&mut self.re, &mut self.im);
        // A full-scale sine reads 0 dB (window coherent gain 0.35875, single-sided).
        let norm = 2.0 / (FFT_N as f32 * 0.35875);
        for b in 0..=self.max_bin {
            let mag = (self.re[b] * self.re[b] + self.im[b] * self.im[b]).sqrt() * norm;
            let db = if mag > 1e-7 { 20.0 * mag.log10() } else { DB_FLOOR };
            into[b] = db.max(DB_FLOOR);
        }
    }

    /// Feed the latest audio, or `None` when nothing is playing (bars fall away).
    pub fn update(&mut self, audio: Option<(&[f32], &[f32])>) {
        match audio {
            Some((input, output)) => {
                let mut fresh = vec![DB_FLOOR; self.max_bin + 1];
                self.spectrum(input, &mut fresh);
                for (cur, new) in self.in_db.iter_mut().zip(&fresh) {
                    *cur = new.max(*cur - FALL);
                }
                self.spectrum(output, &mut fresh);
                for b in 0..=self.max_bin {
                    self.out_db[b] = fresh[b].max(self.out_db[b] - FALL);
                    self.peak_db[b] = self.out_db[b].max(self.peak_db[b] - PEAK_FALL);
                }
                let take = |src: &[f32], dst: &mut Vec<f32>| {
                    let n = dst.len().min(src.len());
                    dst.iter_mut().for_each(|d| *d = 0.0);
                    let at = dst.len() - n;
                    dst[at..].copy_from_slice(&src[src.len() - n..]);
                };
                take(input, &mut self.wave_in);
                take(output, &mut self.wave_out);
                let peak = self.wave_out.iter().chain(&self.wave_in).fold(0.0f32, |m, v| m.max(v.abs()));
                self.scope_peak = if peak > self.scope_peak { peak } else { self.scope_peak * 0.98 + peak * 0.02 }.max(0.02);
            }
            None => {
                for v in self.in_db.iter_mut().chain(self.out_db.iter_mut()) {
                    *v = (*v - FALL).max(DB_FLOOR);
                }
                for v in self.peak_db.iter_mut() {
                    *v = (*v - PEAK_FALL * 2.0).max(DB_FLOOR);
                }
                for v in self.wave_in.iter_mut().chain(self.wave_out.iter_mut()) {
                    *v *= 0.8;
                }
            }
        }
    }

    /// True when there is anything visible to draw.
    pub fn is_silent(&self) -> bool {
        self.out_db.iter().all(|d| *d < DB_BOTTOM) && self.in_db.iter().all(|d| *d < DB_BOTTOM)
    }

    fn band(&self, arr: &[f32], f_lo: f32, f_hi: f32) -> f32 {
        let (lo, hi) = (f_lo / self.bin_hz, f_hi / self.bin_hz);
        let last = self.max_bin;
        if hi - lo < 1.0 {
            let mid = ((lo + hi) * 0.5).min(last as f32 - 1.0);
            let i = mid.floor() as usize;
            let t = mid - i as f32;
            return arr[i] * (1.0 - t) + arr[(i + 1).min(last)] * t;
        }
        let (a, b) = (lo.ceil() as usize, (hi.floor() as usize).min(last));
        arr[a.min(last)..=b.max(a.min(last))].iter().cloned().fold(DB_FLOOR, f32::max)
    }
}

/// Frequency at dot column `x` of `dots` columns, log-spaced.
fn col_freq(x: f32, dots: usize) -> f32 {
    F_MIN * (F_MAX / F_MIN).powf(x / dots as f32)
}

pub fn freq_to_col(f: f32, dots: usize) -> usize {
    let t = (f / F_MIN).ln() / (F_MAX / F_MIN).ln();
    ((t.clamp(0.0, 1.0)) * (dots as f32 - 1.0)).round() as usize
}

fn lerp(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t).round() as u8
}

/// Bottom-to-top colour ramp: violet -> orchid -> pink.
fn gradient(t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    let stops: [(u8, u8, u8); 3] = [(84, 70, 214), (176, 88, 226), (255, 118, 170)];
    let (a, b, u) = if t < 0.5 { (stops[0], stops[1], t * 2.0) } else { (stops[1], stops[2], (t - 0.5) * 2.0) };
    Color::Rgb(lerp(a.0, b.0, u), lerp(a.1, b.1, u), lerp(a.2, b.2, u))
}

const GHOST: Color = Color::Rgb(66, 70, 98);
const GRID: Color = Color::Rgb(48, 50, 70);
const MARK: Color = Color::Rgb(120, 200, 190);
const PEAK: Color = Color::Rgb(240, 240, 255);

struct Braille {
    w: usize,
    h: usize,
    bits: Vec<u8>,
    colors: Vec<Color>,
}

impl Braille {
    fn new(w: usize, h: usize) -> Self {
        Self { w, h, bits: vec![0; w * h], colors: vec![Color::Reset; w * h] }
    }

    fn dots_w(&self) -> usize {
        self.w * 2
    }

    fn dots_h(&self) -> usize {
        self.h * 4
    }

    fn set(&mut self, x: usize, y: usize, color: Color) {
        if x >= self.dots_w() || y >= self.dots_h() {
            return;
        }
        // Braille dot numbering: left column 1,2,3,7 and right column 4,5,6,8.
        const BIT: [[u8; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];
        let idx = (y / 4) * self.w + x / 2;
        self.bits[idx] |= BIT[x % 2][y % 4];
        self.colors[idx] = color;
    }

    fn render(&self, buf: &mut Buffer, area: Rect) {
        for cy in 0..self.h {
            for cx in 0..self.w {
                let i = cy * self.w + cx;
                if self.bits[i] != 0 {
                    let c = char::from_u32(0x2800 + self.bits[i] as u32).unwrap_or(' ');
                    let cell = &mut buf[(area.x + cx as u16, area.y + cy as u16)];
                    cell.set_char(c).set_fg(self.colors[i]);
                }
            }
        }
    }
}

/// x positions (in terminal cells) and labels for the frequency axis.
pub fn axis_labels(width: u16) -> Vec<(u16, &'static str)> {
    let dots = width as usize * 2;
    [(20.0, "20"), (50.0, "50"), (100.0, "100"), (200.0, "200"), (500.0, "500"), (1000.0, "1k"), (2000.0, "2k")]
        .iter()
        .map(|(f, l)| ((freq_to_col(*f, dots) / 2) as u16, *l))
        .collect()
}

pub fn draw(buf: &mut Buffer, area: Rect, an: &Analyzer, style: VizStyle, scope_hz: f32, floor_hz: Option<f32>, empty_msg: &str) {
    if area.width < 4 || area.height < 2 {
        return;
    }
    let mut cv = Braille::new(area.width as usize, area.height as usize);
    let (dw, dh) = (cv.dots_w(), cv.dots_h());

    // Faint vertical grid at the axis frequencies.
    for (f, _) in [(50.0, ""), (100.0, ""), (200.0, ""), (500.0, ""), (1000.0, "")] {
        let x = freq_to_col(f, dw);
        for y in (0..dh).step_by(4) {
            cv.set(x, y + 3, GRID);
        }
    }

    match style {
        VizStyle::Spectrum | VizStyle::Mirror => {
            let mirror = style == VizStyle::Mirror;
            let half = if mirror { dh as f32 / 2.0 } else { dh as f32 };
            let level = |db: f32| ((db - DB_BOTTOM) / (DB_TOP - DB_BOTTOM)).clamp(0.0, 1.0) * half;
            for x in 0..dw {
                let (f0, f1) = (col_freq(x as f32, dw), col_freq(x as f32 + 1.0, dw));
                let h_in = level(an.band(&an.in_db, f0, f1)) as usize;
                let h_out = level(an.band(&an.out_db, f0, f1)) as usize;
                let h_peak = level(an.band(&an.peak_db, f0, f1)) as usize;
                let put = |cv: &mut Braille, y: usize, c: Color| {
                    if mirror {
                        let mid = dh / 2;
                        cv.set(x, mid.saturating_sub(1 + y), c);
                        cv.set(x, mid + y, c);
                    } else {
                        cv.set(x, dh - 1 - y, c);
                    }
                };
                for y in (0..h_in).step_by(2) {
                    put(&mut cv, y, GHOST);
                }
                for y in (0..h_out).step_by(2) {
                    put(&mut cv, y, gradient(y as f32 / half));
                }
                if h_peak > 1 {
                    put(&mut cv, h_peak - 1, PEAK);
                }
            }
        }
        VizStyle::Scope => {
            let mid = dh as f32 / 2.0;
            let scale = mid * 0.9 / an.scope_peak;
            let trace = |cv: &mut Braille, wave: &[f32], colour: &dyn Fn(f32) -> Color| {
                let mut prev: Option<i32> = None;
                for x in 0..dw {
                    let i = (x * wave.len() / dw).min(wave.len() - 1);
                    let v = wave[i];
                    let y = (mid - v * scale).round().clamp(0.0, dh as f32 - 1.0) as i32;
                    let c = colour(v.abs() / an.scope_peak);
                    let (a, b) = match prev {
                        Some(p) => (p.min(y), p.max(y)),
                        None => (y, y),
                    };
                    for yy in a..=b {
                        cv.set(x, yy as usize, c);
                    }
                    prev = Some(y);
                }
            };
            trace(&mut cv, &an.wave_in, &|_| GHOST);
            trace(&mut cv, &an.wave_out, &|t| gradient(t));
        }
    }

    // Scope / Floor markers: dashed vertical lines across the plot.
    let mut mark = |f: f32| {
        let x = freq_to_col(f, dw);
        for y in (0..dh).step_by(3) {
            cv.set(x, y, MARK);
        }
        x / 2
    };
    let scope_cell = mark(scope_hz);
    let floor_cell = floor_hz.map(|f| mark(f));
    cv.render(buf, area);

    // Marker captions on the top row.
    let mut caption = |cell: usize, text: &str| {
        let x = (cell as u16 + 1).min(area.width.saturating_sub(text.len() as u16));
        for (i, ch) in text.chars().enumerate() {
            if x + (i as u16) < area.width {
                let c = &mut buf[(area.x + x + i as u16, area.y)];
                c.set_char(ch).set_fg(MARK);
            }
        }
    };
    caption(scope_cell, &format!("scope {scope_hz:.0}Hz"));
    if let Some(c) = floor_cell {
        // Keep the floor caption clear of the scope one (floor is always left of it).
        caption(c.saturating_sub(12), &format!("floor {:.0}Hz", floor_hz.unwrap_or(0.0)));
    }

    if an.is_silent() {
        let msg = format!("▁ ▂ ▃ ▄ ▅  {empty_msg}  ▅ ▄ ▃ ▂ ▁");
        let len = msg.chars().count() as u16;
        if area.width > len + 2 {
            let x0 = area.x + (area.width - len) / 2;
            let y = area.y + area.height / 2;
            for (i, ch) in msg.chars().enumerate() {
                buf[(x0 + i as u16, y)].set_char(ch).set_fg(Color::Rgb(90, 92, 120));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fft_finds_a_sine() {
        let mut an = Analyzer::new();
        let f = 100.0f32;
        let tone: Vec<f32> = (0..FFT_N).map(|i| 0.5 * (2.0 * PI * f * i as f32 / SAMPLE_RATE as f32).sin()).collect();
        an.update(Some((&tone, &tone)));
        let bin = (f / an.bin_hz).round() as usize;
        // 0.5 amplitude = -6 dB.
        assert!((an.out_db[bin] - -6.0).abs() < 1.5, "got {}", an.out_db[bin]);
        // Far bins are much lower.
        assert!(an.out_db[bin + 40] < -60.0);
    }

    #[test]
    fn bars_fall_when_silent() {
        let mut an = Analyzer::new();
        let tone: Vec<f32> = (0..FFT_N).map(|i| (2.0 * PI * 60.0 * i as f32 / SAMPLE_RATE as f32).sin()).collect();
        an.update(Some((&tone, &tone)));
        assert!(!an.is_silent());
        for _ in 0..80 {
            an.update(None);
        }
        assert!(an.is_silent());
    }

    #[test]
    fn draws_at_any_size_without_panicking() {
        let mut an = Analyzer::new();
        let tone: Vec<f32> = (0..FFT_N).map(|i| (2.0 * PI * 60.0 * i as f32 / SAMPLE_RATE as f32).sin()).collect();
        an.update(Some((&tone, &tone)));
        for style in VizStyle::ALL {
            for (w, h) in [(4, 2), (5, 3), (40, 8), (200, 60)] {
                let area = Rect::new(0, 0, w, h);
                let mut buf = Buffer::empty(area);
                draw(&mut buf, area, &an, style, 250.0, Some(20.0), "waiting for audio");
            }
        }
    }
}

