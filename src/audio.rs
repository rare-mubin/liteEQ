//! PipeWire plumbing, the same shape EasyEffects uses:
//!
//!   apps -> [liteeq_sink: virtual sink stream] -> DSP -> ring -> [playback stream] -> real output
//!
//! Both streams live on one PipeWire loop; their `process` callbacks run on the
//! realtime data thread and only touch the ring through a short mutex.

use crate::dsp::{BassEnhancer, BlockMeters, SAMPLE_RATE};
use crate::settings::Settings;
use pipewire as pw;
use pw::{properties::properties, spa};
use spa::pod::Pod;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

pub const SINK_NAME: &str = "liteeq_sink";
const CHANNELS: usize = 2;
const BYTES_PER_FRAME: usize = CHANNELS * 4;

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Starting,
    Running,
    Error(String),
    /// The background service is not running (client side only).
    Stopped,
}

/// Peak meters. Values are non-negative f32 stored as bits, so `fetch_max` on
/// the raw bits is a correct float max; the UI takes-and-resets each frame.
#[derive(Default)]
pub struct Meters {
    in_l: AtomicU32,
    in_r: AtomicU32,
    out_l: AtomicU32,
    out_r: AtomicU32,
    harmonics: AtomicU32,
}

impl Meters {
    fn feed(&self, m: &BlockMeters) {
        self.in_l.fetch_max(m.in_l.to_bits(), Relaxed);
        self.in_r.fetch_max(m.in_r.to_bits(), Relaxed);
        self.out_l.fetch_max(m.out_l.to_bits(), Relaxed);
        self.out_r.fetch_max(m.out_r.to_bits(), Relaxed);
        self.harmonics.fetch_max(m.harmonics.to_bits(), Relaxed);
    }

    pub fn take(&self) -> BlockMeters {
        let t = |a: &AtomicU32| f32::from_bits(a.swap(0, Relaxed));
        BlockMeters {
            in_l: t(&self.in_l),
            in_r: t(&self.in_r),
            out_l: t(&self.out_l),
            out_r: t(&self.out_r),
            harmonics: t(&self.harmonics),
        }
    }
}

/// Recent mono audio (before/after the effect) for the visualizer.
pub const SCOPE_LEN: usize = 8192;

pub struct Scope {
    input: Vec<f32>,
    output: Vec<f32>,
    pos: usize,
    /// Total samples ever written; the UI uses it to tell whether audio is flowing.
    pub written: u64,
}

impl Scope {
    fn new() -> Self {
        Self { input: vec![0.0; SCOPE_LEN], output: vec![0.0; SCOPE_LEN], pos: 0, written: 0 }
    }

    fn push(&mut self, input: &[f32], output: &[f32]) {
        for (i, o) in input.iter().zip(output) {
            self.input[self.pos] = *i;
            self.output[self.pos] = *o;
            self.pos = (self.pos + 1) % SCOPE_LEN;
        }
        self.written += input.len() as u64;
    }

    /// The most recent `n` samples of (input, output), oldest first.
    pub fn snapshot(&self, n: usize) -> (Vec<f32>, Vec<f32>) {
        let n = n.min(SCOPE_LEN);
        let start = (self.pos + SCOPE_LEN - n) % SCOPE_LEN;
        let take = |v: &[f32]| (0..n).map(|k| v[(start + k) % SCOPE_LEN]).collect::<Vec<f32>>();
        (take(&self.input), take(&self.output))
    }
}

pub struct Shared {
    pub settings: Mutex<Settings>,
    pub meters: Meters,
    pub status: Mutex<Status>,
    pub scope: Mutex<Scope>,
    /// Human-readable routing, e.g. `liteeq_sink → device`; set once the service is up.
    pub route: Mutex<String>,
}

impl Shared {
    pub fn new(settings: Settings) -> Arc<Self> {
        Arc::new(Self {
            settings: Mutex::new(settings),
            meters: Meters::default(),
            status: Mutex::new(Status::Starting),
            scope: Mutex::new(Scope::new()),
            route: Mutex::new(String::new()),
        })
    }

    fn set_status(&self, s: Status) {
        if let Ok(mut g) = self.status.lock() {
            *g = s;
        }
    }
}

/// FIFO between the two streams (interleaved stereo f32). The sink and the
/// output device run on independent clocks, so the ring is kept shallow: it
/// primes to ~2 quanta before playing and trims itself if it ever runs long.
struct Bridge {
    q: VecDeque<f32>,
    primed: bool,
}

impl Bridge {
    fn push(&mut self, samples: &[f32]) {
        self.q.extend(samples.iter().copied());
        let frames = samples.len() / CHANNELS;
        let max = (frames * 6).max(2048) * CHANNELS;
        if self.q.len() > max {
            let keep = (frames * 3).max(1024) * CHANNELS;
            let drop = self.q.len() - keep;
            self.q.drain(..drop);
        }
    }
}

/// Volume/mute the sink's adapter applies to incoming audio before we see it.
/// The system volume controls drive these; liteEQ forwards the level to the
/// real device instead, so the DSP divides the sink's gain back out and the
/// sink stays transparent (no double attenuation).
struct SinkGain {
    master: AtomicU32,
    channels: [AtomicU32; CHANNELS],
    muted: AtomicBool,
}

impl SinkGain {
    fn new() -> Self {
        let one = || AtomicU32::new(1.0f32.to_bits());
        Self { master: one(), channels: [one(), one()], muted: AtomicBool::new(false) }
    }

    /// Multiplier that restores the original level for `channel`, 0 when muted.
    fn compensation(&self, channel: usize) -> f32 {
        if self.muted.load(Relaxed) {
            return 0.0;
        }
        let gain = f32::from_bits(self.master.load(Relaxed)) * f32::from_bits(self.channels[channel].load(Relaxed));
        if gain < 1e-6 { 0.0 } else { 1.0 / gain }
    }

    fn update(&self, id: u32, values: &[f32]) {
        let store = |a: &AtomicU32, v: f32| a.store(v.to_bits(), Relaxed);
        if id == spa::sys::SPA_PROP_volume {
            if let Some(v) = values.first() {
                store(&self.master, *v);
            }
        } else if id == spa::sys::SPA_PROP_channelVolumes {
            for (slot, v) in self.channels.iter().zip(values.iter().cycle()) {
                store(slot, *v);
            }
        } else if id == spa::sys::SPA_PROP_mute {
            self.muted.store(values.first().is_some_and(|v| *v >= 0.5), Relaxed);
        }
    }
}

struct SinkData {
    dsp: BassEnhancer,
    shared: Arc<Shared>,
    bridge: Arc<Mutex<Bridge>>,
    settings: Settings,
    scratch: Vec<f32>,
    gain: Arc<SinkGain>,
    mono_in: Vec<f32>,
    mono_out: Vec<f32>,
}

fn format_pod() -> Vec<u8> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(CHANNELS as u32);
    let mut position = [0; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &spa::pod::Value::Object(obj))
        .expect("serialize audio format")
        .0
        .into_inner()
}

/// Runs the PipeWire loop until `quit` fires. Blocks; call from its own thread.
pub fn run(
    shared: Arc<Shared>,
    sink_name: String,
    output_target: Option<String>,
    quit: pw::channel::Receiver<()>,
) -> Result<(), pw::Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    let _quit = quit.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });

    let bridge = Arc::new(Mutex::new(Bridge { q: VecDeque::new(), primed: false }));
    let latency = format!("256/{SAMPLE_RATE}");
    let out_name = format!("{sink_name}_output");

    // --- virtual sink: apps play into this ---------------------------------
    let sink_props = properties! {
        "media.type" => "Audio",
        "media.category" => "Capture",
        "media.class" => "Audio/Sink",
        "node.name" => sink_name.as_str(),
        "node.description" => "liteEQ Bass Enhancer",
        "node.latency" => latency.as_str(),
        "audio.channels" => "2",
        "audio.position" => "FL,FR",
        "node.virtual" => "true",
        // The sink's own volume/mute must not attenuate the audio: the system
        // volume is forwarded to the real device instead (see volsync).
    };
    let gain = Arc::new(SinkGain::new());
    let sink = pw::stream::StreamBox::new(&core, &sink_name.replace('_', "-"), sink_props)?;
    let sink_data = SinkData {
        dsp: BassEnhancer::new(),
        shared: shared.clone(),
        bridge: bridge.clone(),
        settings: Settings::default(),
        scratch: Vec::with_capacity(8192),
        gain: gain.clone(),
        mono_in: Vec::with_capacity(4096),
        mono_out: Vec::with_capacity(4096),
    };
    let _sink_listener = sink
        .add_local_listener_with_user_data(sink_data)
        .state_changed({
            let shared = shared.clone();
            move |_, _, _, new| match new {
                pw::stream::StreamState::Error(e) => shared.set_status(Status::Error(format!("sink: {e}"))),
                pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming => {
                    let mut st = shared.status.lock().unwrap();
                    if !matches!(*st, Status::Error(_)) {
                        *st = Status::Running;
                    }
                }
                _ => {}
            }
        })
        .control_info({
            let gain = gain.clone();
            move |_, _, id, ctrl| {
                // SAFETY: PipeWire passes a valid control for the duration of the callback.
                let Some(c) = (unsafe { ctrl.as_ref() }) else { return };
                if c.values.is_null() {
                    return;
                }
                let values = unsafe { std::slice::from_raw_parts(c.values, c.n_values as usize) };
                gain.update(id, values);
            }
        })
        .process(|stream, ud| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let Some(data) = buffer.datas_mut().first_mut() else { return };
            let off = data.chunk().offset() as usize;
            let size = data.chunk().size() as usize;
            let Some(bytes) = data.data() else { return };
            let end = (off + size).min(bytes.len());
            let bytes = &bytes[off.min(end)..end];

            ud.scratch.clear();
            ud.scratch
                .extend(bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
            ud.scratch.truncate(ud.scratch.len() / CHANNELS * CHANNELS);
            let comp = [ud.gain.compensation(0), ud.gain.compensation(1)];
            if comp != [1.0, 1.0] {
                for frame in ud.scratch.chunks_exact_mut(CHANNELS) {
                    frame[0] *= comp[0];
                    frame[1] *= comp[1];
                }
            }

            if let Ok(s) = ud.shared.settings.try_lock() {
                ud.settings = *s;
            }
            ud.mono_in.clear();
            ud.mono_in.extend(ud.scratch.chunks_exact(CHANNELS).map(|f| (f[0] + f[1]) * 0.5));
            let meters = ud.dsp.process(&ud.settings, &mut ud.scratch);
            ud.mono_out.clear();
            ud.mono_out.extend(ud.scratch.chunks_exact(CHANNELS).map(|f| (f[0] + f[1]) * 0.5));
            if let Ok(mut sc) = ud.shared.scope.try_lock() {
                sc.push(&ud.mono_in, &ud.mono_out);
            }
            ud.shared.meters.feed(&meters);
            if let Ok(mut b) = ud.bridge.lock() {
                b.push(&ud.scratch);
            }
        })
        .register()?;

    // --- playback: the enhanced signal goes to the real device -------------
    let mut out_props = properties! {
        "media.type" => "Audio",
        "media.category" => "Playback",
        "media.role" => "Music",
        "node.name" => out_name.as_str(),
        "node.description" => "liteEQ Output",
        "node.latency" => latency.as_str(),
        // Stay on the chosen device: without this the session manager moves the
        // stream to the new default sink (liteeq_sink itself) => feedback loop.
        "node.dont-reconnect" => "true",
    };
    if let Some(t) = &output_target {
        out_props.insert("target.object", t.as_str());
    }
    let out = pw::stream::StreamBox::new(&core, &format!("{}-output", sink_name.replace('_', "-")), out_props)?;
    let _out_listener = out
        .add_local_listener_with_user_data(bridge.clone())
        .state_changed({
            let shared = shared.clone();
            move |_, _, _, new| {
                if let pw::stream::StreamState::Error(e) = new {
                    shared.set_status(Status::Error(format!("output: {e}")));
                }
            }
        })
        .process(|stream, bridge| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let requested = buffer.requested() as usize;
            let Some(data) = buffer.datas_mut().first_mut() else { return };
            let frames = {
                let Some(bytes) = data.data() else { return };
                let cap = bytes.len() / BYTES_PER_FRAME;
                let frames = if requested > 0 { requested.min(cap) } else { cap };

                let mut b = bridge.lock().unwrap();
                if !b.primed && b.q.len() >= frames * CHANNELS * 2 {
                    b.primed = true;
                }
                if b.primed && b.q.len() < frames * CHANNELS {
                    b.primed = false; // underrun: emit silence until the ring refills
                }
                for out in bytes[..frames * BYTES_PER_FRAME].chunks_exact_mut(4) {
                    let v = if b.primed { b.q.pop_front().unwrap_or(0.0) } else { 0.0 };
                    out.copy_from_slice(&v.to_le_bytes());
                }
                frames
            };
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = BYTES_PER_FRAME as i32;
            *chunk.size_mut() = (frames * BYTES_PER_FRAME) as u32;
        })
        .register()?;

    let flags = pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS;
    let out_flags = flags | pw::stream::StreamFlags::DONT_RECONNECT;
    let pod_bytes = format_pod();
    let mut params = [Pod::from_bytes(&pod_bytes).expect("format pod")];
    sink.connect(spa::utils::Direction::Input, None, flags, &mut params)?;
    let mut params = [Pod::from_bytes(&pod_bytes).expect("format pod")];
    out.connect(spa::utils::Direction::Output, None, out_flags, &mut params)?;

    mainloop.run();
    Ok(())
}
