//! Thin wrappers around `pactl` (works on PipeWire via pipewire-pulse) for
//! discovering and switching the default output. Kept out of the PipeWire code
//! so the audio path has no dependency on it.

use std::process::{Command, Stdio};
use std::time::Duration;

fn pactl(args: &[&str]) -> Option<String> {
    let out = Command::new("pactl").args(args).stderr(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn default_sink() -> Option<String> {
    pactl(&["get-default-sink"]).filter(|s| !s.is_empty())
}

pub fn set_default_sink(name: &str) -> bool {
    pactl(&["set-default-sink", name]).is_some()
}

pub fn list_sinks() -> Vec<String> {
    pactl(&["list", "short", "sinks"])
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split('\t').nth(1).map(str::to_string))
        .collect()
}

/// Retry until our freshly created sink is visible to the pulse layer, then
/// make it the default. Returns true on success.
pub fn set_default_sink_retry(name: &str) -> bool {
    for _ in 0..30 {
        if set_default_sink(name) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}
