//! Shared memory between the Equalizer APO plugin and the TUI (Windows).
//!
//! The plugin runs inside the Windows audio engine (`audiodg.exe`, as the
//! LocalService account), so it cannot open the user's socket or files. Both
//! sides instead map the same small file, `%ProgramData%\liteEQ\liteeq.shm`,
//! whose folder grants the audio service and logged-in users read/write.
//!
//! The file holds one [`Block`] made only of atomics:
//! - settings, written by the TUI under a sequence lock and read by the plugin
//!   once per audio block (a torn read is retried on the next block);
//! - meters (`fetch_max`, taken-and-reset by the TUI), a mono scope ring and a
//!   heartbeat, written by the plugin.
//!
//! The mapped file also persists the settings across reboots, since the plugin
//! cannot read the user's config directory.

use crate::dsp::BlockMeters;
use crate::settings::{Param, Settings};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::*, fence};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const SCOPE_LEN: usize = 8192;
/// "LEQ" + layout version; bump the last byte whenever `Block` changes.
const MAGIC: u32 = 0x4C45_5101;
const N_SETTINGS: usize = 8;

#[repr(C)]
pub struct Block {
    magic: AtomicU32,
    /// Odd while the TUI is writing `settings`.
    seq: AtomicU32,
    settings: [AtomicU32; N_SETTINGS],
    /// Plugin instances currently open in the audio engine.
    instances: AtomicU32,
    sample_rate: AtomicU32,
    /// Unix time (ms) of the plugin's last processed block.
    heartbeat_ms: AtomicU64,
    meters: [AtomicU32; 5],
    /// Held (1) by the plugin instance writing the scope; others skip.
    scope_lock: AtomicU32,
    scope_pos: AtomicU32,
    scope_written: AtomicU64,
    scope_in: [AtomicU32; SCOPE_LEN],
    scope_out: [AtomicU32; SCOPE_LEN],
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn encode(s: &Settings) -> [u32; N_SETTINGS] {
    let flags = s.bypass as u32 | (s.listen as u32) << 1 | (s.floor_active as u32) << 2;
    [flags, s.blend.to_bits(), s.amount_db.to_bits(), s.drive.to_bits(), s.scope.to_bits(), s.floor.to_bits(), s.input_db.to_bits(), s.output_db.to_bits()]
}

/// Clamps everything into range, so a zeroed or corrupt block is still safe.
fn decode(v: &[u32; N_SETTINGS]) -> Settings {
    let mut s = Settings { bypass: v[0] & 1 != 0, listen: v[0] & 2 != 0, floor_active: v[0] & 4 != 0, ..Settings::default() };
    let params = [Param::Blend, Param::Amount, Param::Harmonics, Param::Scope, Param::Floor, Param::Input, Param::Output];
    for (p, bits) in params.iter().zip(&v[1..]) {
        let x = f32::from_bits(*bits);
        if x.is_finite() {
            p.set(&mut s, x);
        }
    }
    s
}

impl Block {
    /// Fill in defaults on a fresh (all-zero) or outdated block.
    pub fn init(&self) {
        if self.magic.load(Acquire) != MAGIC {
            self.write_settings(&Settings::default());
            self.magic.store(MAGIC, Release);
        }
    }

    pub fn write_settings(&self, s: &Settings) {
        self.seq.fetch_add(1, AcqRel); // odd: writing
        for (slot, v) in self.settings.iter().zip(encode(s)) {
            slot.store(v, Relaxed);
        }
        self.seq.fetch_add(1, Release); // even: done
    }

    /// The current settings, or `None` if uninitialised or mid-write.
    pub fn read_settings(&self) -> Option<Settings> {
        if self.magic.load(Acquire) != MAGIC {
            return None;
        }
        let before = self.seq.load(Acquire);
        if before & 1 != 0 {
            return None;
        }
        let mut v = [0u32; N_SETTINGS];
        for (out, slot) in v.iter_mut().zip(&self.settings) {
            *out = slot.load(Relaxed);
        }
        fence(Acquire);
        (self.seq.load(Relaxed) == before).then(|| decode(&v))
    }

    // --- plugin side ---------------------------------------------------------

    pub fn plugin_opened(&self) {
        self.instances.fetch_add(1, Relaxed);
    }

    pub fn plugin_closed(&self) {
        let _ = self.instances.fetch_update(Relaxed, Relaxed, |n| n.checked_sub(1));
    }

    pub fn beat(&self, sample_rate: u32) {
        self.sample_rate.store(sample_rate, Relaxed);
        self.heartbeat_ms.store(now_ms(), Relaxed);
    }

    pub fn feed_meters(&self, m: &BlockMeters) {
        for (slot, v) in self.meters.iter().zip([m.in_l, m.in_r, m.out_l, m.out_r, m.harmonics]) {
            slot.fetch_max(v.max(0.0).to_bits(), Relaxed);
        }
    }

    /// Append mono (before, after) samples; skipped if another instance is writing.
    pub fn push_scope(&self, input: &[f32], output: &[f32]) {
        if self.scope_lock.compare_exchange(0, 1, Acquire, Relaxed).is_err() {
            return;
        }
        let mut pos = self.scope_pos.load(Relaxed) as usize % SCOPE_LEN;
        for (i, o) in input.iter().zip(output) {
            self.scope_in[pos].store(i.to_bits(), Relaxed);
            self.scope_out[pos].store(o.to_bits(), Relaxed);
            pos = (pos + 1) % SCOPE_LEN;
        }
        self.scope_pos.store(pos as u32, Relaxed);
        self.scope_written.fetch_add(input.len() as u64, Release);
        self.scope_lock.store(0, Release);
    }

    // --- TUI side ------------------------------------------------------------

    pub fn take_meters(&self) -> BlockMeters {
        let t = |a: &AtomicU32| f32::from_bits(a.swap(0, Relaxed));
        let m = &self.meters;
        BlockMeters { in_l: t(&m[0]), in_r: t(&m[1]), out_l: t(&m[2]), out_r: t(&m[3]), harmonics: t(&m[4]) }
    }

    pub fn scope_written(&self) -> u64 {
        self.scope_written.load(Acquire)
    }

    /// The most recent `n` samples of (input, output), oldest first.
    pub fn snapshot(&self, n: usize) -> (Vec<f32>, Vec<f32>) {
        let n = n.min(SCOPE_LEN);
        let end = self.scope_pos.load(Relaxed) as usize % SCOPE_LEN;
        let start = (end + SCOPE_LEN - n) % SCOPE_LEN;
        let take = |v: &[AtomicU32]| (0..n).map(|k| f32::from_bits(v[(start + k) % SCOPE_LEN].load(Relaxed))).collect();
        (take(&self.scope_in), take(&self.scope_out))
    }

    pub fn instances(&self) -> u32 {
        self.instances.load(Relaxed)
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate.load(Relaxed)
    }

    /// How long ago the plugin last processed audio (`None`: never).
    pub fn since_heartbeat(&self) -> Option<Duration> {
        let t = self.heartbeat_ms.load(Relaxed);
        (t != 0).then(|| Duration::from_millis(now_ms().saturating_sub(t)))
    }
}

#[cfg(windows)]
pub use mapping::{Mapping, default_path};

#[cfg(windows)]
mod mapping {
    use super::Block;
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::path::{Path, PathBuf};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows::Win32::Storage::FileSystem::CreateDirectoryW;
    use windows::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READWRITE,
        UnmapViewOfFile,
    };
    use windows::core::{HSTRING, w};

    /// The folder ACL: full control for SYSTEM, Administrators and the owner;
    /// read/write for the audio service (LocalService, write-restricted
    /// tokens) and logged-in users. Inherited by the file inside.
    const FOLDER_SDDL: windows::core::PCWSTR =
        w!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;OW)(A;OICI;FRFW;;;LS)(A;OICI;FRFW;;;WR)(A;OICI;FRFW;;;AU)");

    pub fn default_path() -> PathBuf {
        if let Some(p) = std::env::var_os("LITEEQ_SHM") {
            return PathBuf::from(p);
        }
        let base = std::env::var_os("ProgramData").map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from);
        base.join("liteEQ").join("liteeq.shm")
    }

    fn ensure_dir(dir: &Path) -> io::Result<()> {
        if dir.is_dir() {
            return Ok(());
        }
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // SAFETY: the descriptor is allocated by Windows, used for one call and freed.
        unsafe {
            let mut sd = PSECURITY_DESCRIPTOR::default();
            ConvertStringSecurityDescriptorToSecurityDescriptorW(FOLDER_SDDL, SDDL_REVISION_1, &mut sd, None).map_err(io::Error::from)?;
            let sa = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd.0,
                bInheritHandle: false.into(),
            };
            let res = CreateDirectoryW(&HSTRING::from(dir.as_os_str()), Some(&sa));
            LocalFree(Some(HLOCAL(sd.0)));
            match res {
                Ok(()) => Ok(()),
                Err(_) if dir.is_dir() => Ok(()), // created concurrently
                Err(e) => Err(e.into()),
            }
        }
    }

    /// The shared block, mapped into this process.
    pub struct Mapping {
        view: MEMORY_MAPPED_VIEW_ADDRESS,
        map: HANDLE,
        _file: std::fs::File,
    }

    // SAFETY: the view is only accessed through `Block`'s atomics.
    unsafe impl Send for Mapping {}
    unsafe impl Sync for Mapping {}

    impl Mapping {
        /// Map `path`, creating its folder (with the shared ACL) and the file if needed.
        pub fn open(path: &Path) -> io::Result<Mapping> {
            if let Some(dir) = path.parent() {
                ensure_dir(dir)?;
            }
            let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
            let size = size_of::<Block>() as u64;
            // SAFETY: mapping a file we hold open; a new mapping extends it with zeros.
            unsafe {
                let map = CreateFileMappingW(HANDLE(file.as_raw_handle()), None, PAGE_READWRITE, (size >> 32) as u32, size as u32, None)?;
                let view = MapViewOfFile(map, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, size as usize);
                if view.Value.is_null() {
                    let e = io::Error::last_os_error();
                    let _ = CloseHandle(map);
                    return Err(e);
                }
                let m = Mapping { view, map, _file: file };
                m.block().init();
                Ok(m)
            }
        }

        pub fn block(&self) -> &Block {
            // SAFETY: the view is page-aligned, at least `size_of::<Block>()` long,
            // lives as long as `self`, and every field is an atomic (valid when zeroed).
            unsafe { &*(self.view.Value as *const Block) }
        }
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            unsafe {
                let _ = UnmapViewOfFile(self.view);
                let _ = CloseHandle(self.map);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block() -> Box<Block> {
        // SAFETY: every field of Block is an atomic integer, for which zero is valid.
        unsafe { Box::<Block>::new_zeroed().assume_init() }
    }

    #[test]
    fn settings_round_trip_through_the_block() {
        let b = block();
        assert!(b.read_settings().is_none(), "uninitialised");
        b.init();
        assert_eq!(b.read_settings(), Some(Settings::default()));
        let s = Settings { listen: true, floor_active: true, blend: -3.5, amount_db: 8.5, drive: 4.2, scope: 180.0, floor: 33.0, input_db: -4.1, output_db: 2.0, bypass: true };
        b.write_settings(&s);
        assert_eq!(b.read_settings(), Some(s));
    }

    #[test]
    fn a_write_in_progress_is_not_read() {
        let b = block();
        b.init();
        b.seq.fetch_add(1, AcqRel);
        assert!(b.read_settings().is_none());
    }

    #[test]
    fn scope_keeps_the_latest_samples() {
        let b = block();
        let input: Vec<f32> = (0..SCOPE_LEN + 10).map(|i| i as f32).collect();
        b.push_scope(&input, &input);
        let (i, o) = b.snapshot(4);
        assert_eq!(i, [(SCOPE_LEN + 6) as f32, (SCOPE_LEN + 7) as f32, (SCOPE_LEN + 8) as f32, (SCOPE_LEN + 9) as f32]);
        assert_eq!(i, o);
        assert_eq!(b.scope_written(), (SCOPE_LEN + 10) as u64);
    }

    #[cfg(windows)]
    #[test]
    fn two_mappings_share_one_block() {
        let dir = std::env::temp_dir().join(format!("liteeq-shm-{}", std::process::id())).join("liteEQ");
        let path = dir.join("liteeq.shm");
        let a = Mapping::open(&path).unwrap();
        let b = Mapping::open(&path).unwrap();
        let s = Settings { amount_db: 12.0, ..Settings::default() };
        a.block().write_settings(&s);
        assert_eq!(b.block().read_settings(), Some(s));
        b.block().beat(44100);
        assert_eq!(a.block().sample_rate(), 44100);
        drop((a, b));
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn meters_are_taken_and_reset() {
        let b = block();
        b.feed_meters(&BlockMeters { in_l: 0.5, in_r: 0.25, out_l: 0.75, out_r: 0.1, harmonics: 0.2 });
        b.feed_meters(&BlockMeters { in_l: 0.1, ..BlockMeters::default() });
        assert_eq!(b.take_meters().in_l, 0.5);
        assert_eq!(b.take_meters().in_l, 0.0);
    }
}
