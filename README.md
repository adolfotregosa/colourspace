## About this project

A pattern generator for [ColourSpace](https://www.lightillusion.com/colourspace.html) on Linux. It connects to ColourSpace over the network (by IP address) and shows the colour patches ColourSpace asks for, in SDR (8-bit, or 10-bit when your system supports it) or in HDR10 / HLG.

### Who made this

I'm not a developer. This project was built mostly with the help of a friend and Claude AI. Please keep that in mind: I can use it and test it, but I may not be able to fix or explain everything in the code myself.

### Status

- Tested mainly on Linux with KDE Plasma (Wayland) and an AMD GPU, and briefly on Intel graphics.
- HDR needs a Wayland session with HDR enabled, and a GPU driver with Vulkan.
- Not tested on Windows or macOS.
- It works for me, but it comes as is, with no warranty. Please check your results with your own meter.
- Feedback and bug reports are welcome.

## Download (no compiling needed)

A ready-to-run Linux build is available on the **[Releases page](https://github.com/adolfotregosa/colourspace/releases)**. Download the latest `calibrationclient-linux-x86_64.tar.gz`, then:

```bash
tar xzf calibrationclient-linux-x86_64.tar.gz
cd CalibrationClient
./calibrationclient
```
## How to compile

> **Important:** the current version is on the **`hdr-support`** branch. The default branch (`main`) does not have it yet, so make sure you are on `hdr-support` before building.

### 1. Install what you need (Linux)

You need a Rust toolchain (**1.85 or newer**), a C compiler, `pkg-config` and the SDL2 development files. Install them with your distribution's package manager:

- **Fedora** (where this was built and tested):
```bash
  sudo dnf install rust cargo gcc pkgconf-pkg-config SDL2-devel
```
- **Debian / Ubuntu** (not tested by me):
```bash
  sudo apt install build-essential pkg-config libsdl2-dev rustc cargo
```
- **Arch** (not tested by me):
```bash
  sudo pacman -S base-devel sdl2 rust
```

### 2. Get the code on the right branch

```bash
git clone -b hdr-support https://github.com/adolfotregosa/colourspace.git
cd colourspace
```

If you already have a copy, switch to the branch and update it:

```bash
git checkout hdr-support
git pull
```

Check with `git branch` that `hdr-support` is the one marked with `*`.

### 3. Build

```bash
cargo build --release
```

The program ends up in `target/release/calibrationclient`.

### 4. Run

```bash
./target/release/calibrationclient
```

This opens a startup window where you enter the ColourSpace address (and choose HDR options if your system supports them). You can also skip the window:

```bash
./target/release/calibrationclient 192.168.1.50            # SDR
./target/release/calibrationclient 192.168.1.50 --hdr hdr10
./target/release/calibrationclient --help                  # all options
```

Your last address and HDR choices are remembered in `calibrationclient.settings`, in the folder you started the program from.

**HDR needs:** a Wayland session with HDR switched on in your desktop's display settings, and an HDR-capable display.
