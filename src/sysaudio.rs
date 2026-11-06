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

/// Placeholder sink PipeWire shows while no real device is ready (e.g. early at login).
pub const DUMMY_SINK: &str = "auto_null";

/// Every sink with its session priority (higher = preferred by the session manager).
pub fn sinks() -> Vec<(String, i64)> {
    parse_sinks(&pactl(&["list", "sinks"]).unwrap_or_default())
}

fn parse_sinks(text: &str) -> Vec<(String, i64)> {
    let mut out: Vec<(String, i64)> = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix("Name:") {
            out.push((name.trim().to_string(), 0));
        } else if let Some(v) = line.strip_prefix("priority.session = ") {
            if let (Some(last), Ok(p)) = (out.last_mut(), v.trim_matches('"').parse()) {
                last.1 = p;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_sink_names_and_priorities() {
        let text = "Sink #51\n\tState: RUNNING\n\tName: alsa_output.x.HiFi__Headphones__sink\n\tProperties:\n\t\tpriority.session = \"1009\"\n\nSink #326\n\tName: liteeq_sink\n\tProperties:\n\t\tnode.name = \"liteeq_sink\"\n";
        assert_eq!(
            super::parse_sinks(text),
            vec![("alsa_output.x.HiFi__Headphones__sink".to_string(), 1009), ("liteeq_sink".to_string(), 0)]
        );
    }
}
