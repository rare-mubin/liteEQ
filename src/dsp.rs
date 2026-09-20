//! Bass enhancer DSP, a port of the Calf Studio Gear "Bass Enhancer"
//! (`bassenhancer_audio_module`) and its `tap_distortion` waveshaper
//! (Tom Szilagyi's TAP tube-warmth code), following the Calf sources.
//!
//! Signal flow per channel (identical to Calf):
//!   in * level_in -> LP(scope) x2 -> saturate -> LP(scope) x2 -> [HP(floor) x2]
//!   out = (harmonics * amount + in) * level_out        (listen: harmonics only)
//!
//! Deviation from Calf: no 2x oversampling around the saturator. The signal is
//! low-passed at <=250 Hz on both sides of it, so aliasing is inaudible.
//!
//! LICENCE: unlike the rest of this project (MIT, see `LICENSE`), this file is derived
//! from Calf Studio Gear (Copyright (C) 2001-2010 Krzysztof Foltman, Markus Schmidt,
//! Thor Harald Johansen and others; the saturator is Tom Szilagyi's TAP tube-warmth
//! code, included in Calf with his permission). It is therefore distributed under the
//! GNU Lesser General Public License, version 2 or (at your option) any later version;
//! see `COPYING.LGPL`. Do not copy Calf-derived code into other files.

use crate::settings::Settings;

pub const SAMPLE_RATE: u32 = 48_000;

#[inline]
fn d(x: f32) -> f32 {
    let x = x.abs();
    if x > 1e-8 { x.sqrt() } else { 0.0 }
}

#[inline]
fn m(x: f32) -> f32 {
    if x.abs() > 1e-8 { x } else { 0.0 }
}

/// Direct-form-II biquad with RBJ cookbook coefficients (Calf `biquad_d2`).
#[derive(Clone, Copy, Default)]
struct Biquad {
    a0: f64,
    a1: f64,
    a2: f64,
    b1: f64,
    b2: f64,
    w1: f64,
    w2: f64,
}

impl Biquad {
    fn lowpass(fc: f32, q: f32, sr: f32) -> Self {
        let omega = 2.0 * std::f64::consts::PI * fc as f64 / sr as f64;
        let (sn, cs) = omega.sin_cos();
        let alpha = sn / (2.0 * q as f64);
        let inv = 1.0 / (1.0 + alpha);
        let a0 = inv * (1.0 - cs) * 0.5;
        Self { a0, a1: a0 + a0, a2: a0, b1: -2.0 * cs * inv, b2: (1.0 - alpha) * inv, ..Self::default() }
    }

    fn highpass(fc: f32, q: f32, sr: f32) -> Self {
        let omega = 2.0 * std::f64::consts::PI * fc as f64 / sr as f64;
        let (sn, cs) = omega.sin_cos();
        let alpha = sn / (2.0 * q as f64);
        let inv = 1.0 / (1.0 + alpha);
        let a0 = inv * (1.0 + cs) * 0.5;
        Self { a0, a1: -2.0 * a0, a2: a0, b1: -2.0 * cs * inv, b2: (1.0 - alpha) * inv, ..Self::default() }
    }

    /// Replace coefficients but keep the filter state, so retuning does not click.
    fn retune(&mut self, other: &Biquad) {
        self.a0 = other.a0;
        self.a1 = other.a1;
        self.a2 = other.a2;
        self.b1 = other.b1;
        self.b2 = other.b2;
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let w = x as f64 - self.b1 * self.w1 - self.b2 * self.w2;
        let out = self.a0 * w + self.a1 * self.w1 + self.a2 * self.w2;
        // Flush denormals: they are very slow on ARM once the input goes quiet.
        self.w2 = self.w1;
        self.w1 = if w.abs() < 1e-20 { 0.0 } else { w };
        out as f32
    }
}

/// TAP tube-warmth style saturator. `blend` -10..10 crossfades 3rd (symmetric)
/// to 2nd (asymmetric) harmonics, `drive` 0.1..10 sets the amount of curvature.
struct TapDistortion {
    srate: f32,
    blend_old: f32,
    drive_old: f32,
    meter: f32,
    kpa: f32,
    kpb: f32,
    kna: f32,
    knb: f32,
    ap: f32,
    an: f32,
    srct: f32,
    pwrq: f32,
    prev_med: f32,
    prev_out: f32,
}

impl TapDistortion {
    fn new(srate: u32) -> Self {
        let mut s = Self {
            srate: srate as f32,
            blend_old: f32::NAN,
            drive_old: f32::NAN,
            meter: 0.0,
            kpa: 0.0,
            kpb: 0.0,
            kna: 0.0,
            knb: 0.0,
            ap: 0.0,
            an: 0.0,
            srct: 0.0,
            pwrq: 0.0,
            prev_med: 0.0,
            prev_out: 0.0,
        };
        s.set_params(0.0, 8.5);
        s
    }

    fn set_params(&mut self, blend: f32, drive: f32) {
        if self.drive_old == drive && self.blend_old == blend {
            return;
        }
        let rdrive = 12.0 / drive;
        let rbdr = rdrive / (10.5 - blend) * 780.0 / 33.0;
        self.kpa = d(2.0 * (rdrive * rdrive) - 1.0) + 1.0;
        self.kpb = (2.0 - self.kpa) / 2.0;
        self.ap = ((rdrive * rdrive) - self.kpa + 1.0) / 2.0;
        let kc = self.kpa / d(2.0 * d(2.0 * (rdrive * rdrive) - 1.0) - 2.0 * rdrive * rdrive);

        self.srct = (0.1 * self.srate) / (0.1 * self.srate + 1.0);
        let sq = kc * kc + 1.0;
        self.knb = -1.0 * rbdr / d(sq);
        self.kna = 2.0 * kc * rbdr / d(sq);
        self.an = rbdr * rbdr / sq;
        let imr = 2.0 * self.knb + d(2.0 * self.kna + 4.0 * self.an - 1.0);
        self.pwrq = 2.0 / (imr + 1.0);
        self.drive_old = drive;
        self.blend_old = blend;
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let med = if x >= 0.0 {
            (d(self.ap + x * (self.kpa - x)) + self.kpb) * self.pwrq
        } else {
            (d(self.an - x * (self.kna + x)) + self.knb) * self.pwrq * -1.0
        };
        let out = self.srct * (med - self.prev_med + self.prev_out);
        self.prev_med = m(med);
        self.prev_out = m(out);
        self.meter = self.meter.max(out);
        out
    }

    /// Positive peak since the last call.
    fn take_level(&mut self) -> f32 {
        std::mem::replace(&mut self.meter, 0.0)
    }
}

/// Peaks seen in one processed block (linear, >= 0).
#[derive(Clone, Copy, Default)]
pub struct BlockMeters {
    pub in_l: f32,
    pub in_r: f32,
    pub out_l: f32,
    pub out_r: f32,
    pub harmonics: f32,
}

pub struct BassEnhancer {
    lp: [[Biquad; 4]; 2],
    hp: [[Biquad; 2]; 2],
    dist: [TapDistortion; 2],
    freq_old: f32,
    floor_old: f32,
    sample_rate: u32,
}

impl BassEnhancer {
    pub fn new() -> Self {
        Self::with_sample_rate(SAMPLE_RATE)
    }

    /// For hosts that run at the device's own rate (the Equalizer APO plugin).
    pub fn with_sample_rate(sample_rate: u32) -> Self {
        let mut s = Self {
            lp: [[Biquad::default(); 4]; 2],
            hp: [[Biquad::default(); 2]; 2],
            dist: [TapDistortion::new(sample_rate), TapDistortion::new(sample_rate)],
            freq_old: f32::NAN,
            floor_old: f32::NAN,
            sample_rate,
        };
        s.update_filters(&Settings::default());
        s
    }

    fn update_filters(&mut self, s: &Settings) {
        let sr = self.sample_rate as f32;
        if s.scope != self.freq_old {
            let lp = Biquad::lowpass(s.scope, 0.707, sr);
            for ch in self.lp.iter_mut() {
                for f in ch.iter_mut() {
                    f.retune(&lp);
                }
            }
            self.freq_old = s.scope;
        }
        if s.floor != self.floor_old {
            let hp = Biquad::highpass(s.floor, 0.707, sr);
            for ch in self.hp.iter_mut() {
                for f in ch.iter_mut() {
                    f.retune(&hp);
                }
            }
            self.floor_old = s.floor;
        }
        for dist in &mut self.dist {
            dist.set_params(s.blend, s.drive);
        }
    }

    /// Process interleaved stereo f32 in place.
    pub fn process(&mut self, s: &Settings, data: &mut [f32]) -> BlockMeters {
        let mut mt = BlockMeters::default();
        if s.bypass {
            for f in data.chunks_exact(2) {
                mt.in_l = mt.in_l.max(f[0].abs());
                mt.in_r = mt.in_r.max(f[1].abs());
            }
            mt.out_l = mt.in_l;
            mt.out_r = mt.in_r;
            return mt;
        }

        self.update_filters(s);
        let level_in = db_to_lin(s.input_db);
        let level_out = db_to_lin(s.output_db);
        let amount = if s.amount_db <= Settings::AMOUNT_MIN_DB { 0.0 } else { db_to_lin(s.amount_db) };

        for frame in data.chunks_exact_mut(2) {
            let input = [frame[0] * level_in, frame[1] * level_in];
            mt.in_l = mt.in_l.max(input[0].abs());
            mt.in_r = mt.in_r.max(input[1].abs());

            let mut out = [0.0f32; 2];
            for c in 0..2 {
                let lp = &mut self.lp[c];
                let mut p = lp[0].process(input[c]);
                p = lp[1].process(p);
                p = self.dist[c].process(p);
                p = lp[3].process(p);
                p = lp[2].process(p);
                if s.floor_active {
                    let hp = &mut self.hp[c];
                    p = hp[1].process(p);
                    p = hp[0].process(p);
                }
                out[c] = if s.listen { p * amount * level_out } else { (p * amount + input[c]) * level_out };
            }
            frame[0] = out[0];
            frame[1] = out[1];
            mt.out_l = mt.out_l.max(out[0].abs());
            mt.out_r = mt.out_r.max(out[1].abs());
            mt.harmonics = mt.harmonics.max(self.dist[0].take_level().max(self.dist[1].take_level()) * amount);
        }
        mt
    }
}

#[inline]
pub fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[inline]
pub fn lin_to_db(x: f32) -> f32 {
    if x <= 1e-6 { -120.0 } else { 20.0 * x.log10() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    /// Magnitude of `freq` in interleaved-stereo left channel via Goertzel.
    fn tone_mag(buf: &[f32], freq: f32) -> f32 {
        let n = buf.len() / 2;
        let w = 2.0 * PI * freq / SAMPLE_RATE as f32;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for i in 0..n {
            let ph = (w * i as f32) as f64;
            re += buf[i * 2] as f64 * ph.cos();
            im += buf[i * 2] as f64 * ph.sin();
        }
        (2.0 * (re * re + im * im).sqrt() / n as f64) as f32
    }

    fn run(s: &Settings, freq: f32, amp: f32) -> Vec<f32> {
        let mut e = BassEnhancer::new();
        let n = SAMPLE_RATE as usize; // 1 second
        let mut buf = Vec::with_capacity(n * 2);
        for i in 0..n {
            let x = amp * (2.0 * PI * freq * i as f32 / SAMPLE_RATE as f32).sin();
            buf.push(x);
            buf.push(x);
        }
        for chunk in buf.chunks_mut(1024) {
            e.process(s, chunk);
        }
        buf[n / 2 * 2..].to_vec() // drop the settling half
    }

    #[test]
    fn adds_harmonics() {
        let s = Settings { amount_db: 6.0, scope: 250.0, ..Settings::default() };
        let out = run(&s, 60.0, 0.5);
        assert!(out.iter().all(|x| x.is_finite() && x.abs() < 8.0));
        let h2 = tone_mag(&out, 120.0);
        let h3 = tone_mag(&out, 180.0);
        assert!(h2 + h3 > 0.01, "expected harmonics, got 2nd={h2} 3rd={h3}");
    }

    #[test]
    fn blend_moves_between_2nd_and_3rd() {
        let mk = |blend| Settings { amount_db: 6.0, scope: 250.0, blend, listen: true, ..Settings::default() };
        let third = run(&mk(-10.0), 60.0, 0.5);
        let second = run(&mk(10.0), 60.0, 0.5);
        let r3 = tone_mag(&third, 180.0) / tone_mag(&third, 120.0).max(1e-9);
        let r2 = tone_mag(&second, 180.0) / tone_mag(&second, 120.0).max(1e-9);
        assert!(r3 > r2, "3rd/2nd ratio should fall as blend goes to 2nd: {r3} vs {r2}");
    }

    #[test]
    fn zero_amount_is_transparent() {
        let s = Settings { amount_db: Settings::AMOUNT_MIN_DB, ..Settings::default() };
        let out = run(&s, 60.0, 0.5);
        assert!((tone_mag(&out, 60.0) - 0.5).abs() < 0.01);
    }

    #[test]
    fn bypass_is_bit_exact() {
        let s = Settings { bypass: true, ..Settings::default() };
        let mut e = BassEnhancer::new();
        let mut buf = vec![0.25f32, -0.5, 0.1, 0.9];
        let orig = buf.clone();
        e.process(&s, &mut buf);
        assert_eq!(buf, orig);
    }
}
