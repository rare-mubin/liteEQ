# liteEQ

**Version 0.1.0**

A lightweight terminal bass enhancer for Linux, written in Rust. It uses the same
DSP as the Calf Bass Enhancer in EasyEffects, with a dot
visualizer. The effect runs as a background service, so closing the app doesn't
stop your audio.

![liteEQ v0.1.0](screenshot/liteEQ_v0.1.0_screenshot.png)

Needs **PipeWire** (with `pipewire-pulse` and `pactl`).

## Install

1. Install the dependencies for your distro:

   **Debian, Ubuntu, Mint, Pop!_OS**

   ```bash
   sudo apt install build-essential pkg-config clang libpipewire-0.3-dev libspa-0.2-dev pipewire-pulse wireplumber pulseaudio-utils
   ```

   **Fedora**

   ```bash
   sudo dnf install gcc pkgconf-pkg-config clang clang-devel pipewire-devel pipewire-pulseaudio wireplumber pulseaudio-utils
   ```

   **Arch, Manjaro, EndeavourOS**

   ```bash
   sudo pacman -S base-devel clang pipewire pipewire-pulse wireplumber libpulse
   ```

   **openSUSE**

   ```bash
   sudo zypper install gcc pkgconf-pkg-config clang-devel pipewire-devel pipewire-pulseaudio wireplumber pulseaudio-utils
   ```

   Only the Debian/Ubuntu commands have been tested. The others are based on each
   distro's package names, so a package may be named slightly differently.

   Most desktop distros already run PipeWire. Check with `pactl info`: the line
   `Server Name` should mention PipeWire.

   You also need [Rust](https://rustup.rs) 1.85 or newer:

   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```

2. Build and install:

   ```bash
   cargo install --path . --locked
   ```

3. Run it from anywhere:

   ```bash
   lite-eq
   ```

`cargo install` puts `lite-eq` in `~/.cargo/bin`. Make sure that folder is in your `PATH`.

## Build only

```bash
cargo build --release      # binary: target/release/lite-eq
cargo test
```

## Use

```bash
lite-eq          # open the app (starts the service if needed)
lite-eq stop     # stop the service
lite-eq --help      # all options
lite-eq --version   # show the version
```

While it runs, all audio goes through the effect, and your volume keys control the
real output device as usual. Stopping the service restores your previous output.

- **Mouse:** click or drag sliders, right-click or double-click to reset.
- **Keyboard:** `↑↓` select, `←→` adjust, `r` reset, `l` listen, `b` bypass, `v` change view, `q` quit.
- **Settings** (button at the top, or `,`): start or stop the service, start at login, keep running after closing the app.

## Uninstall

```bash
lite-eq stop
cargo uninstall liteeq
```

If you turned on start at login, switch it off in Settings first.

## Licence

MIT, see [LICENSE](LICENSE), with one exception: `src/dsp.rs` is a port of the
Calf Studio Gear Bass Enhancer and stays under the LGPL (version 2 or later, see
[COPYING.LGPL](COPYING.LGPL)). If you fork the project, keep that file under its
own licence.
