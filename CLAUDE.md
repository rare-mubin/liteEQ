# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

liteEQ: a terminal bass enhancer for Linux/PipeWire and Windows/Equalizer APO. The DSP is a port of the Calf Studio Gear
"Bass Enhancer" (as used by EasyEffects). The Cargo package is `liteeq`, but the binary is
**`lite-eq`** (`[[bin]]` in `Cargo.toml`), so test output and process names say `lite-eq`.
README.md covers user install/usage; the Linux build needs `clang`/libclang (bindgen) and `libpipewire-0.3-dev`,
the Windows build only the `windows` crates (no system deps).

**Platform split.** The two platforms share the TUI (`ui.rs`, `viz.rs`, `main.rs`), the DSP (`dsp.rs`),
`settings.rs` and `remote.rs` (`Status`/`Frame`: what the panel learns from whatever runs the effect); everything
else is `cfg`-gated in `main.rs`. Linux-only: `audio.rs`, `daemon.rs`, `ipc.rs`, `service.rs`, `sysaudio.rs`,
`volsync.rs` (the background service). Windows-only: `eapo.rs`, `shm.rs`, `windevices.rs` and the plugin in
`src/plugin/`. There is no service, socket, daemon, autostart or volume sync on Windows, and no virtual device:
Equalizer APO runs the effect. Only Linux and Windows are supported (`compile_error!` in `main.rs` otherwise).
A Windows machine cannot compile the Linux side (pipewire-sys needs the system library), so review Linux-only
edits carefully — and keep them minimal, since those files are meant to stay as they are.

## Licensing

The project is MIT (`LICENSE`) **except `src/dsp.rs`**, which is derived from Calf/TAP and stays LGPL-2.0-or-later
(`COPYING.LGPL`; notice in the file header). Don't copy Calf-derived code into other files or relicense `dsp.rs`.

## Commands

```bash
cargo build --release
cargo test                          # all unit tests
cargo test <name>                   # one test, e.g. cargo test wire_round_trips
cargo test renders_at_many -- --nocapture   # prints the rendered TUI layout as text
cargo install --path . --locked --force     # installs ~/.cargo/bin/lite-eq
```

No rustfmt/clippy config exists and the code is not rustfmt-formatted (long lines); don't reformat files wholesale.

## Architecture (needs several files to see)

**Two processes, one binary (Linux).** `lite-eq daemon` (`daemon.rs`) is a headless service that owns all audio
work. `lite-eq` (the TUI, `main.rs`) is only a remote control: it connects to the daemon's Unix socket
(`ipc.rs`, `$XDG_RUNTIME_DIR/liteeq.sock`), starts the daemon detached (`service.rs`, `setsid`) if none is
running, polls `GET` at ~30 Hz for meters + visualizer samples, and pushes edited settings with `SET`.
The daemon is the source of truth; the TUI adopts its settings on (re)connect. Closing the TUI leaves the daemon
running unless `keep_running` is off (`app.conf`). Protocol is line-based; both sides live in `ipc.rs` — change them
together. On Windows the panel talks to the plugin instead (`App::svc` vs `App::eapo`; `Link` is `ipc::Client`
there and `eapo::PluginLink` here), and the Settings popup shows only Equalizer APO.

**Audio path (`audio.rs`, Linux).** A PipeWire virtual sink (`liteeq_sink`, a `pw_stream` with `media.class=Audio/Sink`)
feeds `dsp.rs`, which pushes into a mutex-guarded ring (`Bridge`) that a second playback stream drains to the real
device. Both streams run on one loop with `RT_PROCESS`; the process callbacks touch only atomics, `try_lock`ed
mutexes and the short ring lock. Settings reach the audio thread via `Mutex<Settings>` (`try_lock` per block);
meters via `AtomicU32` holding f32 bits with `fetch_max` (valid because values are non-negative); the visualizer via
`Scope` (`snapshot()` of the last 8192 mono samples, `written` counter tells the UI whether audio is flowing).

**Equalizer APO plugin (Windows).** The package also builds a `cdylib`, `liteeq_vst.dll` (`src/plugin/lib.rs`,
empty off Windows): a hand-written VST 2.4 ABI (no SDK) around `dsp.rs`, which it pulls in with `#[path]` together
with `settings.rs` and `shm.rs`. Equalizer APO loads it inside `audiodg.exe` (as LocalService) on the real device,
so there is no added latency. It can't reach the user's socket or files, so the TUI talks to it through a mapped
file, `%ProgramData%\liteEQ\liteeq.shm` (`shm.rs`: atomics only; settings under a seqlock, meters/scope/heartbeat
from the plugin; the folder ACL grants LocalService, WRITE RESTRICTED and Authenticated Users R/W — that file also
persists the settings, since the plugin cannot read `%APPDATA%`). Only instances inside `audiodg.exe` touch the
block (Equalizer APO's Editor loads plugins too). Never let a panic cross the ABI (`catch_unwind`, pass-through on
failure); the DSP must support the device's rate (`BassEnhancer::with_sample_rate`; `Frame::sample_rate` retunes
the analyzer). `eapo.rs` has `lite-eq setup [--remove]` and the same actions from the Settings popup
(`Target::EapoInstall/EapoRemove/EapoDevice`, keys `e`/`u`/`d`): it copies the DLL to `<InstallPath>\VSTPlugins`
(renaming a loaded copy aside), writes `config\liteeq.txt` (`Device:` + `Stage: post-mix`, which keeps it off
capture devices and runs it once on the mix) and appends `Include: liteeq.txt` to `config.txt` in its original
encoding. Paths come from `HKLM\SOFTWARE\EqualizerAPO`; device buttons are its `Child APOs` GUIDs matched against
render endpoint ids (`windevices.rs`), and a chosen device is written as its GUID.

**Volume handling is the subtle part (Linux).** While liteEQ is the default sink, system volume keys act on `liteeq_sink`.
`SinkGain` reads the sink's applied volume/mute from the stream's `control_info` callback and the DSP divides it
back out, keeping the sink transparent; `volsync.rs` (via `pactl`) mirrors that volume/mute to the real device and
back. `channelmix.disable` does NOT stop the sink volume being applied — don't try it again.

**PipeWire/WirePlumber gotchas encoded in the code:**
- The output stream sets `node.dont-reconnect` + `StreamFlags::DONT_RECONNECT`; without it WirePlumber re-links
  it to the new default sink (`liteeq_sink` itself) → feedback loop, no sound.
  The flip side: when its device disappears (unplug, UCM profile switch — e.g. HiFi Headphones/Speaker are
  separate sinks) WirePlumber *destroys* the stream. `daemon.rs` `Follow` rebuilds it (`audio::Cmd::Retarget`),
  follows the user's choice of default (then takes the default back) and skips the `auto_null` placeholder.
- WirePlumber persists sink volume/mute per `media.name` (derived from `--name`, `_`→`-`) and restores it on the
  next start. Anything that mutes/lowers a sink named `liteeq_sink` poisons the next real launch.
- Default sink is taken over at start and restored on exit (`DefaultSinkGuard`); the service also handles SIGTERM.

**DSP (`dsp.rs`)** is meant to stay faithful to Calf: `D()` is `sqrt(|x|)`, four LP biquads at Scope around the
saturator, optional two HP at Floor, `out = (harmonics*amount + in)`, Listen = harmonics only. It deliberately omits
Calf's 2× oversampling. At low Scope with Amount ≈ 0 dB the harmonic path is roughly anti-phase and *reduces* the
fundamental (e.g. −3 dB at 60 Hz, Scope 100) — that is inherent to the algorithm, not a bug.

**UI (`ui.rs`, `viz.rs`).** btop-style rounded panels with buttons ("chips") drawn inside the border lines. Every
clickable element registers a hitbox (`Hits`) *while drawing*; `main.rs` never recomputes layout. Adding a `Target`
variant means handling it in `handle_mouse` (the match is exhaustive). The Settings popup is modal: panels behind it
draw dimmed into a throwaway `Hits`, so only the popup and a backdrop are clickable. The event loop redraws only
when dirty (input, live audio, falling meters, status change). The mouse wheel is intentionally ignored. The
visualizer is a hand-written FFT (Blackman-Harris) drawn on a braille dot canvas.

## Testing against the real system — read before running anything

This machine runs the user's *live* audio (PipeWire + their real `lite-eq` service). Past mistakes here: a test
tone played into the user's headphones, `lite-eq stop` (default name) killing their running service, and `pkill -f`
matching the calling shell. So:

- Never run `lite-eq stop`, `lite-eq daemon`, or the TUI with the default name; never `pkill -f`. Check
  `ps -C lite-eq` first and only touch processes you started.
- Isolate every test: `--name liteeq_t --output liteeq_test --no-default --no-volume-sync`, plus
  `LITEEQ_CONFIG_DIR=<tmp>` and `XDG_STATE_HOME=<tmp>` (not `XDG_CONFIG_HOME`, which also relocates the systemd
  unit path). Both variables also work on Windows.
- Windows: `lite-eq setup` (and the Install button) changes the user's live Equalizer APO config, i.e. their real
  audio: ask first, and `lite-eq setup --remove` afterwards if it was only a test. `LITEEQ_SHM=<file>` relocates
  the TUI's shared block. A running `lite-eq.exe` locks `target\release\lite-eq.exe` against rebuilds.
  Stand-in output: `pactl load-module module-null-sink sink_name=liteeq_test` — unload it afterwards.
- Set the null sink to 100% first (WirePlumber restores saved volumes per node name and will skew level checks).
- Measure audio with `pw-cat -r --target <sink> -P stream.capture.sink=true ...`; play with `pw-cat -p --target liteeq_t file.wav`.
- Drive the TUI through a pty with raw SGR mouse sequences; hitbox coordinates can be dumped from a `TestBackend`
  render in a throwaway `#[test]`. Poll with timeouts — the daemon takes ~0.15 s to exit and a closed pty makes
  `read` return immediately.
- The login-start toggle writes the real `~/.config/systemd/user/liteeq.service`; if you test it, disable it again
  and confirm the unit file is gone.
