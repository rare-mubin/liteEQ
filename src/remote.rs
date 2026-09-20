//! What the panel knows about whatever is running the effect: the background
//! service on Linux (`ipc.rs`), the Equalizer APO plugin on Windows (`eapo.rs`).

use crate::dsp::BlockMeters;

#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(windows, allow(dead_code))] // Windows has no service to fail to start
pub enum Status {
    Starting,
    Running,
    Error(String),
    /// Nothing is running the effect (client side only).
    Stopped,
}

/// One poll's worth of live data.
pub struct Frame {
    pub status: Status,
    pub meters: BlockMeters,
    /// New audio for the visualizer, present only if it changed.
    pub samples: Option<(Vec<f32>, Vec<f32>)>,
    /// Rate of `samples`: the service always runs at 48 kHz, the plugin at the device's rate.
    pub sample_rate: u32,
}
