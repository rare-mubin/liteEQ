//! Keeps the volume/mute of the virtual sink and the real output device in step.
//!
//! While liteEQ is the default output, the desktop volume keys, the shell slider
//! and `alsamixer` (PipeWire card) all act on `liteeq_sink`. The sink itself is
//! kept transparent (its own volume is not applied to the audio), so the value
//! is forwarded to the real device instead, e.g. the MOONDROP `PCM` control.
//! Changes made on the device (say in `alsamixer -c N`) are mirrored back so the
//! slider always shows the real level.
//!
//! Uses `pactl` so the device's hardware mixer is driven the same way the
//! desktop does it.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// ~1% of full scale (raw 65536 = 100%). Devices quantise volume to hardware
/// steps, so tiny differences must not read as a change.
const TOLERANCE: u32 = 700;

#[derive(Clone, Debug, PartialEq)]
struct Vol {
    chans: Vec<u32>,
    mute: bool,
}

impl Vol {
    fn differs(&self, other: &Vol) -> bool {
        self.mute != other.mute
            || self.chans.len() != other.chans.len()
            || self.chans.iter().zip(&other.chans).any(|(a, b)| a.abs_diff(*b) > TOLERANCE)
    }
}

fn pactl(args: &[&str]) -> Option<String> {
    let out = Command::new("pactl").args(args).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

fn read(sink: &str) -> Option<Vol> {
    let v = pactl(&["get-sink-volume", sink])?;
    let chans: Vec<u32> = v
        .trim()
        .strip_prefix("Volume:")?
        .split(',')
        .filter_map(|c| c.split(':').nth(1)?.split('/').next()?.trim().parse().ok())
        .collect();
    let mute = pactl(&["get-sink-mute", sink])?.contains("yes");
    (!chans.is_empty()).then_some(Vol { chans, mute })
}

fn write(sink: &str, v: &Vol) {
    let mut args = vec!["set-sink-volume".to_string(), sink.to_string()];
    args.extend(v.chans.iter().map(|c| c.to_string()));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    pactl(&args);
    pactl(&["set-sink-mute", sink, if v.mute { "1" } else { "0" }]);
}

pub struct VolSync {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl VolSync {
    /// Start mirroring. The sink is first set to the device's current level, so
    /// the slider does not jump and the device volume is not touched.
    pub fn start(sink: String, device: String) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || run(&sink, &device, &flag));
        Self { stop, thread: Some(thread) }
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for VolSync {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run(sink: &str, device: &str, stop: &AtomicBool) {
    // Wait for the sink to be visible to the pulse layer.
    let mut tries = 0;
    while read(sink).is_none() {
        tries += 1;
        if tries > 50 || stop.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let Some(mut last) = read(device) else { return };
    write(sink, &last);

    let Ok(mut child) = Command::new("pactl").arg("subscribe").stdout(Stdio::piped()).stderr(Stdio::null()).spawn() else {
        return;
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.contains(" on sink #") && tx.send(()).is_err() {
                break;
            }
        }
    });

    while !stop.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(()) => {
                // Coalesce the burst of events a slider drag produces.
                std::thread::sleep(Duration::from_millis(30));
                while rx.try_recv().is_ok() {}
                let (Some(s), Some(d)) = (read(sink), read(device)) else { continue };
                if s.differs(&last) {
                    write(device, &s); // the slider/keys moved: drive the real device
                    last = s;
                } else if d.differs(&last) {
                    write(sink, &d); // the device moved elsewhere: mirror it
                    last = d;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(l: u32, r: u32, mute: bool) -> Vol {
        Vol { chans: vec![l, r], mute }
    }

    #[test]
    fn tolerance_absorbs_hardware_steps() {
        assert!(!v(30000, 30000, false).differs(&v(30500, 29700, false)));
        assert!(v(30000, 30000, false).differs(&v(31000, 30000, false)));
        assert!(v(30000, 30000, false).differs(&v(30000, 30000, true)));
    }
}
