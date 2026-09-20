//! Equalizer APO integration (Windows): installing the liteEQ VST plugin into
//! Equalizer APO's config, and the TUI's link to the running plugin.
//!
//! `lite-eq setup` copies `liteeq_vst.dll` into Equalizer APO's `VSTPlugins`
//! folder, writes `config\liteeq.txt` and adds `Include: liteeq.txt` to the end of
//! `config.txt`. Equalizer APO notices the change and loads the plugin at once.
//! Neither folder needs admin rights (the Equalizer APO installer makes them
//! writable for users).

use crate::remote::{Frame, Status};
use crate::settings::{self, Settings};
use crate::shm;
use crate::viz::FFT_N;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;
use windows_registry::LOCAL_MACHINE;

pub const DLL: &str = "liteeq_vst.dll";
const INCLUDE_FILE: &str = "liteeq.txt";
const INCLUDE_LINE: &str = "Include: liteeq.txt";

/// An output device Equalizer APO is installed on.
#[derive(Clone, Debug, PartialEq)]
pub struct EapoDevice {
    /// Friendly name, e.g. `Speakers (MOONDROP Dawn Pro)`.
    pub name: String,
    /// Endpoint GUID with braces, lower case: a `Device:` pattern for just this device.
    pub guid: String,
}

impl EapoDevice {
    /// A short label for buttons: the part in parentheses (the hardware name).
    pub fn short_name(&self) -> String {
        let inner = self.name.split_once('(').and_then(|(_, r)| r.rsplit_once(')')).map(|(i, _)| i.trim());
        inner.filter(|i| !i.is_empty()).unwrap_or(&self.name).to_string()
    }
}

/// Where Equalizer APO is installed.
pub struct Eapo {
    pub install: PathBuf,
    pub config: PathBuf,
}

impl Eapo {
    pub fn find() -> Option<Eapo> {
        let key = LOCAL_MACHINE.open(r"SOFTWARE\EqualizerAPO").ok()?;
        let install = PathBuf::from(key.get_string("InstallPath").ok()?);
        let config = key.get_string("ConfigPath").map(PathBuf::from).unwrap_or_else(|_| install.join("config"));
        Some(Eapo { install, config })
    }

    fn include_file(&self) -> PathBuf {
        self.config.join(INCLUDE_FILE)
    }

    /// True when `lite-eq setup` has been run (the plugin is in the config).
    pub fn is_set_up(&self) -> bool {
        self.include_file().is_file() && read_config(&self.config.join("config.txt")).is_ok_and(|t| has_include(&t))
    }

    /// Output devices Equalizer APO is installed on (its `Child APOs` are keyed
    /// by endpoint GUID); capture devices are left out.
    pub fn devices(&self) -> Vec<EapoDevice> {
        let Ok(key) = LOCAL_MACHINE.open(r"SOFTWARE\EqualizerAPO\Child APOs") else { return Vec::new() };
        let guids: Vec<String> = key.keys().map(|k| k.map(|g| g.to_lowercase()).collect()).unwrap_or_default();
        crate::windevices::outputs()
            .into_iter()
            .filter_map(|d| {
                let id = d.id.to_lowercase();
                let guid = guids.iter().find(|g| id.ends_with(g.as_str()))?;
                Some(EapoDevice { name: d.name, guid: guid.clone() })
            })
            .collect()
    }

    /// The pattern a fresh setup uses: the only device, or all of them.
    pub fn default_device(&self) -> String {
        match self.devices().as_slice() {
            [one] => one.guid.clone(),
            _ => "all".into(),
        }
    }

    /// Point the plugin at other devices (rewrites `liteeq.txt`; Equalizer APO reloads it).
    pub fn set_device(&self, pattern: &str) -> Result<(), String> {
        std::fs::write(self.include_file(), include_text(pattern)).map_err(|e| format!("cannot write {}: {e}", self.include_file().display()))
    }

    /// The `Device:` pattern liteEQ is applied to.
    pub fn device(&self) -> Option<String> {
        let text = std::fs::read_to_string(self.include_file()).ok()?;
        text.lines().find_map(|l| l.trim().strip_prefix("Device:").map(|d| d.trim().to_string()))
    }
}

fn has_include(config: &str) -> bool {
    config.lines().any(|l| l.trim().eq_ignore_ascii_case(INCLUDE_LINE))
}

/// Read a config file, which may be UTF-8 or UTF-16 (with BOM).
fn read_config(path: &Path) -> io::Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(match bytes.as_slice() {
        [0xFF, 0xFE, rest @ ..] => String::from_utf16_lossy(&rest.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>()),
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        _ => String::from_utf8_lossy(&bytes).into_owned(),
    })
}

/// Write `text` back in the encoding `path` had.
fn write_config(path: &Path, text: &str) -> io::Result<()> {
    let old = std::fs::read(path).unwrap_or_default();
    let bytes = if old.starts_with(&[0xFF, 0xFE]) {
        let mut b = vec![0xFF, 0xFE];
        b.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        b
    } else if old.starts_with(&[0xEF, 0xBB, 0xBF]) {
        [&[0xEF, 0xBB, 0xBF][..], text.as_bytes()].concat()
    } else {
        text.as_bytes().to_vec()
    };
    std::fs::write(path, bytes)
}

fn with_include(config: &str) -> String {
    if has_include(config) {
        return config.to_string();
    }
    let nl = if config.contains("\r\n") || config.is_empty() { "\r\n" } else { "\n" };
    let mut out = config.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push_str(nl);
    }
    out.push_str(INCLUDE_LINE);
    out.push_str(nl);
    out
}

fn without_include(config: &str) -> String {
    let mut out = String::with_capacity(config.len());
    for line in config.split_inclusive('\n') {
        if !line.trim().eq_ignore_ascii_case(INCLUDE_LINE) {
            out.push_str(line);
        }
    }
    out
}

fn include_text(device: &str) -> String {
    format!(
        "# liteEQ bass enhancer: written by `lite-eq setup` and controlled from the lite-eq app.\r\n\
         # Remove it with `lite-eq setup --remove`.\r\n\
         Device: {device}\r\n\
         # Once, on the final mix (post-mix also keeps it off microphones).\r\n\
         Stage: post-mix\r\n\
         VSTPlugin: Library {DLL}\r\n"
    )
}

/// Copy the plugin into `VSTPlugins`. A loaded DLL cannot be overwritten but can
/// be renamed, so an in-use copy is moved aside first.
fn install_dll(src: &Path, dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let dest = dir.join(DLL);
    if std::fs::read(src).ok() == std::fs::read(&dest).ok().filter(|_| dest.exists()) {
        return Ok(());
    }
    if dest.exists() && std::fs::remove_file(&dest).is_err() {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let aside = dir.join(format!("liteeq_vst.old-{stamp}.dll"));
        std::fs::rename(&dest, &aside).map_err(|e| format!("cannot replace {}: {e}", dest.display()))?;
    }
    std::fs::copy(src, &dest).map_err(|e| format!("cannot copy the plugin to {}: {e}", dest.display()))?;
    // Old copies can go once the audio engine has let go of them.
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with("liteeq_vst.old-") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    Ok(())
}

/// The plugin next to this executable (where `cargo build` puts it).
pub fn bundled_dll() -> Option<PathBuf> {
    let p = std::env::current_exe().ok()?.parent()?.join(DLL);
    p.is_file().then_some(p)
}

/// `lite-eq setup`: install the plugin. Returns what was done, for the user.
pub fn setup(plugin: Option<PathBuf>, device: Option<String>) -> Result<Vec<String>, String> {
    let eapo = Eapo::find().ok_or("Equalizer APO is not installed (https://sourceforge.net/projects/equalizerapo/)")?;
    let src = plugin.or_else(bundled_dll).ok_or_else(|| format!("{DLL} not found next to lite-eq; build it with `cargo build --release` or pass --plugin <PATH>"))?;
    let mut done = Vec::new();

    let plugins = eapo.install.join("VSTPlugins");
    install_dll(&src, &plugins)?;
    done.push(format!("plugin     {}", plugins.join(DLL).display()));

    // Seed the shared settings from the app's saved ones the first time.
    let shm_path = shm::default_path();
    let fresh = !shm_path.exists();
    let map = shm::Mapping::open(&shm_path).map_err(|e| format!("cannot create {}: {e}", shm_path.display()))?;
    if fresh {
        map.block().write_settings(&settings::load());
    }
    done.push(format!("settings   {}", shm_path.display()));

    let device = device.or_else(|| eapo.device()).unwrap_or_else(|| eapo.default_device());
    let include = eapo.include_file();
    std::fs::write(&include, include_text(&device)).map_err(|e| format!("cannot write {}: {e}", include.display()))?;
    done.push(format!("config     {} (Device: {device})", include.display()));

    let config = eapo.config.join("config.txt");
    let text = read_config(&config).unwrap_or_default();
    if !has_include(&text) {
        let backup = eapo.config.join("config.txt.liteeq-backup");
        if !backup.exists() {
            let _ = std::fs::copy(&config, &backup);
        }
        write_config(&config, &with_include(&text)).map_err(|e| format!("cannot update {}: {e}", config.display()))?;
        done.push(format!("added      `{INCLUDE_LINE}` to {}", config.display()));
    }
    Ok(done)
}

/// `lite-eq setup --remove`.
pub fn remove() -> Result<Vec<String>, String> {
    let eapo = Eapo::find().ok_or("Equalizer APO is not installed")?;
    let mut done = Vec::new();
    let config = eapo.config.join("config.txt");
    if let Ok(text) = read_config(&config) {
        if has_include(&text) {
            write_config(&config, &without_include(&text)).map_err(|e| format!("cannot update {}: {e}", config.display()))?;
            done.push(format!("removed    `{INCLUDE_LINE}` from {}", config.display()));
        }
    }
    if std::fs::remove_file(eapo.include_file()).is_ok() {
        done.push(format!("deleted    {}", eapo.include_file().display()));
    }
    let dll = eapo.install.join("VSTPlugins").join(DLL);
    match std::fs::remove_file(&dll) {
        Ok(()) => done.push(format!("deleted    {}", dll.display())),
        Err(_) if dll.exists() => done.push(format!("kept       {} (still loaded; delete it after a restart)", dll.display())),
        Err(_) => {}
    }
    Ok(done)
}

/// `Equalizer APO → <device>`, for the panel's top border.
pub fn route() -> String {
    let Some(eapo) = Eapo::find() else { return "Equalizer APO".into() };
    let pattern = eapo.device().unwrap_or_else(|| "all".into());
    let target = if pattern.eq_ignore_ascii_case("all") {
        "all outputs".to_string()
    } else {
        eapo.devices().into_iter().find(|d| d.guid.eq_ignore_ascii_case(&pattern)).map_or(pattern, |d| d.short_name())
    };
    format!("Equalizer APO → {target}")
}

/// The TUI's connection to the plugin, through the shared block.
pub struct PluginLink {
    map: shm::Mapping,
    last_written: u64,
    pub route: String,
}

impl PluginLink {
    pub fn connect() -> io::Result<PluginLink> {
        let map = shm::Mapping::open(&shm::default_path())?;
        Ok(PluginLink { map, last_written: 0, route: route() })
    }

    pub fn settings(&mut self) -> io::Result<Settings> {
        for _ in 0..100 {
            if let Some(s) = self.map.block().read_settings() {
                return Ok(s);
            }
            std::thread::yield_now();
        }
        Err(io::Error::other("shared settings are busy"))
    }

    pub fn set(&mut self, s: &Settings) -> io::Result<()> {
        self.map.block().write_settings(s);
        Ok(())
    }

    pub fn poll(&mut self) -> io::Result<Frame> {
        let b = self.map.block();
        let fresh = b.since_heartbeat().is_some_and(|d| d < Duration::from_millis(1500));
        let status = if fresh || b.instances() > 0 { Status::Running } else { Status::Stopped };
        let written = b.scope_written();
        let samples = (fresh && written != self.last_written).then(|| {
            self.last_written = written;
            b.snapshot(FFT_N)
        });
        let rate = b.sample_rate();
        Ok(Frame { status, meters: b.take_meters(), samples, sample_rate: if rate > 0 { rate } else { crate::dsp::SAMPLE_RATE } })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn include_is_added_once_and_removed_cleanly() {
        let orig = "# Preamp: -9.5 dB\r\nInclude: peace.txt\r\nInclude: E:\\mic.txt";
        let added = with_include(orig);
        assert_eq!(added, "# Preamp: -9.5 dB\r\nInclude: peace.txt\r\nInclude: E:\\mic.txt\r\nInclude: liteeq.txt\r\n");
        assert_eq!(with_include(&added), added);
        assert_eq!(without_include(&added), "# Preamp: -9.5 dB\r\nInclude: peace.txt\r\nInclude: E:\\mic.txt\r\n");
    }

    #[test]
    fn utf16_config_keeps_its_encoding() {
        let dir = std::env::temp_dir().join(format!("liteeq-eapo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.txt");
        let mut b = vec![0xFF, 0xFE];
        b.extend("Include: peace.txt\r\n".encode_utf16().flat_map(u16::to_le_bytes));
        std::fs::write(&p, b).unwrap();
        let text = read_config(&p).unwrap();
        write_config(&p, &with_include(&text)).unwrap();
        assert!(std::fs::read(&p).unwrap().starts_with(&[0xFF, 0xFE]));
        assert!(has_include(&read_config(&p).unwrap()));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Load the built plugin DLL and drive it through the VST 2.4 ABI the way
    /// Equalizer APO's VSTPluginInstance does.
    #[test]
    fn built_dll_loads_like_equalizer_apo_loads_it() {
        use std::ffi::c_void;
        use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
        use windows::core::{HSTRING, s};
        // Tests run from target/<profile>/deps; cargo puts the cdylib in target/<profile>.
        let exe = std::env::current_exe().unwrap();
        let dll = exe.parent().unwrap().parent().unwrap().join(DLL);
        assert!(dll.is_file(), "{} not built", dll.display());

        type Dispatcher = unsafe extern "C" fn(*mut c_void, i32, i32, isize, *mut c_void, f32) -> isize;
        type Process = unsafe extern "C" fn(*mut c_void, *const *const f32, *mut *mut f32, i32);
        #[repr(C)]
        struct Head {
            magic: i32,
            dispatcher: Dispatcher,
            _process: usize,
            _set: usize,
            _get: usize,
            _programs: i32,
            _params: i32,
            inputs: i32,
            outputs: i32,
            flags: i32,
            _r: [isize; 2],
            _q: [i32; 3],
            _ratio: f32,
            _object: *mut c_void,
            _user: *mut c_void,
            _id: i32,
            _version: i32,
            process_replacing: Process,
        }
        unsafe {
            let lib = LoadLibraryW(&HSTRING::from(dll.as_os_str())).unwrap();
            let main = GetProcAddress(lib, s!("VSTPluginMain")).expect("VSTPluginMain exported");
            let main: unsafe extern "C" fn(*const c_void) -> *mut Head = std::mem::transmute(main);
            let e = main(std::ptr::null());
            assert_eq!((*e).magic, i32::from_be_bytes(*b"VstP"));
            assert_eq!(((*e).inputs, (*e).outputs), (2, 2));
            assert_ne!((*e).flags & (1 << 4), 0, "processReplacing");
            let d = (*e).dispatcher;
            let null = std::ptr::null_mut();
            d(e as _, 0, 0, 0, null, 0.0); // effOpen
            d(e as _, 10, 0, 0, null, 96000.0); // effSetSampleRate
            d(e as _, 11, 0, 480, null, 0.0); // effSetBlockSize
            d(e as _, 12, 0, 1, null, 0.0); // effMainsChanged(on)
            d(e as _, 71, 0, 0, null, 0.0); // effStartProcess (not handled: must be harmless)
            let input: Vec<f32> = (0..480).map(|i| (i as f32 * 0.004).sin() * 0.5).collect();
            let (mut l, mut r) = (vec![0.0f32; 480], vec![0.0f32; 480]);
            let ins = [input.as_ptr(), input.as_ptr()];
            let mut outs = [l.as_mut_ptr(), r.as_mut_ptr()];
            ((*e).process_replacing)(e as _, ins.as_ptr(), outs.as_mut_ptr(), 480);
            assert!(l.iter().chain(&r).all(|v| v.is_finite()));
            assert!(l.iter().any(|v| *v != 0.0));
            d(e as _, 1, 0, 0, null, 0.0); // effClose
        }
    }

    #[test]
    fn short_names_are_the_hardware_name() {
        let d = |n: &str| EapoDevice { name: n.into(), guid: String::new() };
        assert_eq!(d("Speakers (MOONDROP Dawn Pro)").short_name(), "MOONDROP Dawn Pro");
        assert_eq!(d("Speakers (Realtek(R) Audio)").short_name(), "Realtek(R) Audio");
        assert_eq!(d("Headphones").short_name(), "Headphones");
    }

    #[test]
    #[ignore = "reads this machine's Equalizer APO install"]
    fn print_eapo_devices() {
        let e = Eapo::find().expect("Equalizer APO installed");
        for d in e.devices() {
            println!("{} | {} | {}", d.name, d.short_name(), d.guid);
        }
        println!("default: {}", e.default_device());
        println!("route: {}", route());
    }

    #[test]
    fn include_file_scopes_the_plugin() {
        let t = include_text("MOONDROP");
        assert!(t.contains("Device: MOONDROP\r\n"));
        assert!(t.contains("Stage: post-mix\r\n"));
        assert!(t.contains("VSTPlugin: Library liteeq_vst.dll\r\n"));
    }
}
