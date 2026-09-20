//! liteEQ as a VST 2.4 plugin for Equalizer APO (Windows).
//!
//! Equalizer APO runs VST plugins inside the Windows audio engine, on the real
//! output device, so the effect adds no latency and needs no virtual cable.
//! The plugin has no parameters or editor of its own: the `lite-eq` TUI
//! controls it through the shared block in `shm.rs`.
//!
//! Only the small part of the VST 2.4 ABI a stereo effect needs is declared
//! here (the layout is the de-facto standard one, as in VeSTige), so no SDK is
//! required.
#![cfg(windows)]

#[allow(dead_code)]
#[path = "../dsp.rs"]
mod dsp;
#[allow(dead_code)]
#[path = "../settings.rs"]
mod settings;
#[allow(dead_code)]
#[path = "../shm.rs"]
mod shm;

use dsp::BassEnhancer;
use settings::Settings;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};

const MAGIC: i32 = i32::from_be_bytes(*b"VstP");
const UNIQUE_ID: i32 = i32::from_be_bytes(*b"lEQb");
const EFF_FLAGS_CAN_REPLACING: i32 = 1 << 4;
const CHANNELS: usize = 2;

type Dispatcher = unsafe extern "C" fn(*mut AEffect, i32, i32, isize, *mut c_void, f32) -> isize;
type Process = unsafe extern "C" fn(*mut AEffect, *const *const f32, *mut *mut f32, i32);
type ProcessDouble = unsafe extern "C" fn(*mut AEffect, *const *const f64, *mut *mut f64, i32);
type SetParameter = unsafe extern "C" fn(*mut AEffect, i32, f32);
type GetParameter = unsafe extern "C" fn(*mut AEffect, i32) -> f32;

#[repr(C)]
struct AEffect {
    magic: i32,
    dispatcher: Dispatcher,
    process: Process,
    set_parameter: SetParameter,
    get_parameter: GetParameter,
    num_programs: i32,
    num_params: i32,
    num_inputs: i32,
    num_outputs: i32,
    flags: i32,
    reserved1: isize,
    reserved2: isize,
    initial_delay: i32,
    real_qualities: i32,
    off_qualities: i32,
    io_ratio: f32,
    object: *mut c_void,
    /// Owned by the host (Equalizer APO stores its own pointer here).
    user: *mut c_void,
    unique_id: i32,
    version: i32,
    process_replacing: Process,
    process_double_replacing: ProcessDouble,
    future: [u8; 56],
}

// Dispatcher opcodes we answer.
const EFF_OPEN: i32 = 0;
const EFF_CLOSE: i32 = 1;
const EFF_SET_SAMPLE_RATE: i32 = 10;
const EFF_SET_BLOCK_SIZE: i32 = 11;
const EFF_MAINS_CHANGED: i32 = 12;
const EFF_GET_PLUG_CATEGORY: i32 = 35;
const EFF_GET_EFFECT_NAME: i32 = 45;
const EFF_GET_VENDOR_STRING: i32 = 47;
const EFF_GET_PRODUCT_STRING: i32 = 48;
const EFF_GET_VENDOR_VERSION: i32 = 49;
const EFF_GET_VST_VERSION: i32 = 58;
const PLUG_CATEG_EFFECT: isize = 1;

/// Per-instance state; `effect` must stay first so `*mut AEffect` is `*mut Plugin`.
#[repr(C)]
struct Plugin {
    effect: AEffect,
    dsp: BassEnhancer,
    sample_rate: u32,
    settings: Settings,
    /// The shared block; only mapped inside the audio engine (see `is_audio_engine`).
    shm: Option<shm::Mapping>,
    registered: bool,
    block: Vec<f32>,
    mono_in: Vec<f32>,
    mono_out: Vec<f32>,
}

impl Plugin {
    fn reserve(&mut self, frames: usize) {
        self.block.reserve(frames * CHANNELS);
        self.mono_in.reserve(frames);
        self.mono_out.reserve(frames);
    }

    fn open(&mut self) {
        if is_audio_engine() {
            self.shm = shm::Mapping::open(&shm::default_path()).ok();
        }
        if let Some(m) = &self.shm {
            m.block().plugin_opened();
            self.registered = true;
            if let Some(s) = m.block().read_settings() {
                self.settings = s;
            }
        }
    }

    fn close(&mut self) {
        if let (Some(m), true) = (&self.shm, self.registered) {
            m.block().plugin_closed();
            self.registered = false;
        }
    }

    fn process(&mut self, inputs: [&[f32]; CHANNELS], outputs: [&mut [f32]; CHANNELS]) {
        let shared = self.shm.as_ref().map(|m| m.block());
        if let Some(s) = shared.and_then(|b| b.read_settings()) {
            self.settings = s;
        }
        // Interleave first: the host may pass the same buffers for input and output.
        self.block.clear();
        for (l, r) in inputs[0].iter().zip(inputs[1]) {
            self.block.push(*l);
            self.block.push(*r);
        }
        self.mono_in.clear();
        self.mono_in.extend(self.block.chunks_exact(CHANNELS).map(|f| (f[0] + f[1]) * 0.5));
        let meters = self.dsp.process(&self.settings, &mut self.block);
        let [out_l, out_r] = outputs;
        for ((frame, l), r) in self.block.chunks_exact(CHANNELS).zip(out_l.iter_mut()).zip(out_r.iter_mut()) {
            *l = frame[0];
            *r = frame[1];
        }
        if let Some(b) = shared {
            self.mono_out.clear();
            self.mono_out.extend(self.block.chunks_exact(CHANNELS).map(|f| (f[0] + f[1]) * 0.5));
            b.push_scope(&self.mono_in, &self.mono_out);
            b.feed_meters(&meters);
            b.beat(self.sample_rate);
        }
    }
}

/// True inside `audiodg.exe`. Equalizer APO's Editor and Benchmark also load
/// the plugin; those instances must not show up as the live effect.
fn is_audio_engine() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().eq_ignore_ascii_case("audiodg.exe")))
        .unwrap_or(false)
}

fn write_str(ptr: *mut c_void, s: &str, max: usize) {
    if ptr.is_null() {
        return;
    }
    let bytes = &s.as_bytes()[..s.len().min(max - 1)];
    // SAFETY: the host passes a buffer of at least `max` bytes for these opcodes.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
        *(ptr as *mut u8).add(bytes.len()) = 0;
    }
}

unsafe extern "C" fn dispatcher(effect: *mut AEffect, opcode: i32, _index: i32, value: isize, ptr: *mut c_void, opt: f32) -> isize {
    let plugin = effect as *mut Plugin;
    if opcode == EFF_CLOSE {
        // SAFETY: `effect` came from `VSTPluginMain`'s Box; the host never uses it again.
        let mut p = unsafe { Box::from_raw(plugin) };
        let _ = catch_unwind(AssertUnwindSafe(|| p.close()));
        return 1;
    }
    // SAFETY: the host only calls us with the pointer we handed out, from one thread at a time.
    let p = unsafe { &mut *plugin };
    catch_unwind(AssertUnwindSafe(|| match opcode {
        EFF_OPEN => {
            p.open();
            0
        }
        EFF_SET_SAMPLE_RATE => {
            let rate = opt.round() as u32;
            if rate > 0 && rate != p.sample_rate {
                p.sample_rate = rate;
                p.dsp = BassEnhancer::with_sample_rate(rate);
            }
            0
        }
        EFF_SET_BLOCK_SIZE => {
            p.reserve(value.max(0) as usize);
            0
        }
        EFF_MAINS_CHANGED => {
            if value != 0 {
                p.dsp = BassEnhancer::with_sample_rate(p.sample_rate); // fresh filter state
            }
            0
        }
        EFF_GET_PLUG_CATEGORY => PLUG_CATEG_EFFECT,
        EFF_GET_EFFECT_NAME => {
            write_str(ptr, "liteEQ Bass Enhancer", 32);
            1
        }
        EFF_GET_VENDOR_STRING => {
            write_str(ptr, "liteEQ", 64);
            1
        }
        EFF_GET_PRODUCT_STRING => {
            write_str(ptr, "liteEQ", 64);
            1
        }
        EFF_GET_VENDOR_VERSION => 100,
        EFF_GET_VST_VERSION => 2400,
        _ => 0,
    }))
    .unwrap_or(0)
}

unsafe extern "C" fn process_replacing(effect: *mut AEffect, inputs: *const *const f32, outputs: *mut *mut f32, frames: i32) {
    if inputs.is_null() || outputs.is_null() || frames <= 0 {
        return;
    }
    let n = frames as usize;
    // SAFETY: the host passes `num_inputs`/`num_outputs` (2) channel pointers of `frames` samples each.
    let (ins, outs) = unsafe {
        let i = std::slice::from_raw_parts(inputs, CHANNELS);
        let o = std::slice::from_raw_parts(outputs, CHANNELS);
        (
            [std::slice::from_raw_parts(i[0], n), std::slice::from_raw_parts(i[1], n)],
            [o[0], o[1]],
        )
    };
    let p = unsafe { &mut *(effect as *mut Plugin) };
    let done = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: output buffers are valid for `n` samples; they may alias the
        // inputs, which `Plugin::process` has finished reading before it writes.
        let outputs = unsafe { [std::slice::from_raw_parts_mut(outs[0], n), std::slice::from_raw_parts_mut(outs[1], n)] };
        p.process(ins, outputs);
    }));
    if done.is_err() {
        // Never let a panic reach the audio engine: pass the audio through.
        for c in 0..CHANNELS {
            // SAFETY: as above; `copy` allows overlap.
            unsafe { std::ptr::copy(ins[c].as_ptr(), outs[c], n) };
        }
    }
}

unsafe extern "C" fn process_accumulating(effect: *mut AEffect, inputs: *const *const f32, outputs: *mut *mut f32, frames: i32) {
    // Deprecated in VST 2.4 and unused by Equalizer APO (we set CanReplacing).
    unsafe { process_replacing(effect, inputs, outputs, frames) }
}

unsafe extern "C" fn process_double(_: *mut AEffect, _: *const *const f64, _: *mut *mut f64, _: i32) {}
unsafe extern "C" fn set_parameter(_: *mut AEffect, _: i32, _: f32) {}
unsafe extern "C" fn get_parameter(_: *mut AEffect, _: i32) -> f32 {
    0.0
}

/// The VST 2.4 entry point Equalizer APO looks for.
///
/// # Safety
/// Called by the host with its callback (unused: we never call back); the
/// returned `AEffect` is freed on `effClose`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn VSTPluginMain(_host: *const c_void) -> *mut c_void {
    let plugin = Box::new(Plugin {
        effect: AEffect {
            magic: MAGIC,
            dispatcher,
            process: process_accumulating,
            set_parameter,
            get_parameter,
            num_programs: 0,
            num_params: 0,
            num_inputs: CHANNELS as i32,
            num_outputs: CHANNELS as i32,
            flags: EFF_FLAGS_CAN_REPLACING,
            reserved1: 0,
            reserved2: 0,
            initial_delay: 0,
            real_qualities: 0,
            off_qualities: 0,
            io_ratio: 1.0,
            object: std::ptr::null_mut(),
            user: std::ptr::null_mut(),
            unique_id: UNIQUE_ID,
            version: 100,
            process_replacing,
            process_double_replacing: process_double,
            future: [0; 56],
        },
        dsp: BassEnhancer::new(),
        sample_rate: dsp::SAMPLE_RATE,
        settings: Settings::default(),
        shm: None,
        registered: false,
        block: Vec::with_capacity(8192),
        mono_in: Vec::with_capacity(4096),
        mono_out: Vec::with_capacity(4096),
    });
    let raw = Box::into_raw(plugin);
    // SAFETY: `raw` is a fresh, valid allocation.
    unsafe { (*raw).effect.object = raw as *mut c_void };
    raw as *mut c_void
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn behaves_like_a_stereo_vst_effect() {
        unsafe {
            let e = VSTPluginMain(std::ptr::null()) as *mut AEffect;
            assert_eq!((*e).magic, MAGIC);
            assert_eq!(((*e).num_inputs, (*e).num_outputs), (2, 2));
            let d = (*e).dispatcher;
            d(e, EFF_OPEN, 0, 0, std::ptr::null_mut(), 0.0);
            d(e, EFF_SET_SAMPLE_RATE, 0, 0, std::ptr::null_mut(), 44100.0);
            d(e, EFF_SET_BLOCK_SIZE, 0, 512, std::ptr::null_mut(), 0.0);
            d(e, EFF_MAINS_CHANGED, 0, 1, std::ptr::null_mut(), 0.0);
            let mut name = [0u8; 64];
            d(e, EFF_GET_EFFECT_NAME, 0, 0, name.as_mut_ptr() as *mut c_void, 0.0);
            assert!(name.starts_with(b"liteEQ"));

            // In-place processing (inputs == outputs), as some hosts do.
            let mut l: Vec<f32> = (0..512).map(|i| (i as f32 * 0.01).sin() * 0.3).collect();
            let mut r = l.clone();
            let orig = l.clone();
            let mut chans = [l.as_mut_ptr(), r.as_mut_ptr()];
            ((*e).process_replacing)(e, chans.as_ptr() as *const *const f32, chans.as_mut_ptr(), 512);
            assert!(l.iter().all(|v| v.is_finite()));
            assert_ne!(l, orig, "the effect changed the signal");
            assert_eq!(l, r);
            d(e, EFF_CLOSE, 0, 0, std::ptr::null_mut(), 0.0);
        }
    }
}
