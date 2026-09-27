# Building TackleCast

TackleCast supports native builds on both **Windows 10/11** and **Linux** (Debian, Ubuntu, Fedora, Arch, etc.).

---

## 1. Windows Build Instructions

### Prerequisites
1. **Rust Toolchain**: Install via [rustup.rs](https://rustup.rs).
2. **C/C++ Build Tools**: Visual Studio C++ Build Tools (MSVC) or MinGW-w64 (`x86_64-pc-windows-gnu`).
3. **FFmpeg 7.x Development Libraries**: Shared + Dev build containing `include/`, `lib/`, and `bin/`.
   - Download from BtbN FFmpeg Builds (e.g. `ffmpeg-n7.1.1-latest-win64-gpl-shared-7.1.zip`).
4. **LLVM / libclang**: Required for `bindgen` during `ffmpeg-sys-next` compilation.
   - Install via Winget: `winget install LLVM.LLVM` or download from GitHub releases.
5. *(Optional)* **CUDA Toolkit 13+**: Only needed for developing NVIDIA nvJPEG hardware decode on Windows.

### Environment Setup (PowerShell)
```powershell
# Set paths to your FFmpeg and LLVM installations:
$env:PATH = "$env:USERPROFILE\.cargo\bin;C:\ffmpeg\bin;C:\Program Files\LLVM\bin;$env:PATH"
$env:FFMPEG_DIR = 'C:\ffmpeg'
$env:PKG_CONFIG_PATH = 'C:\ffmpeg\lib\pkgconfig'
$env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin'
```

### Compile & Test
```powershell
# Run unit tests (includes audio resampler, channel conversion, and ring buffer tests)
cargo test

# Compile release binary
cargo build --release
```
The output binary will be located at `target\release\tacklecast.exe`.

---

## 2. Linux Build Instructions

TackleCast natively supports modern Linux desktop environments with PipeWire, PulseAudio, ALSA, and V4L2 video capture.

### Prerequisites & System Packages

#### Debian / Ubuntu / Pop!_OS / Linux Mint
```bash
sudo apt update
sudo apt install -y \
  build-essential \
  clang \
  libclang-dev \
  pkg-config \
  libasound2-dev \
  libv4l-dev \
  libavcodec-dev \
  libavformat-dev \
  libavdevice-dev \
  libavutil-dev \
  libswresample-dev \
  libswscale-dev
```

#### Arch Linux / Manjaro
```bash
sudo pacman -S --needed \
  base-devel \
  clang \
  pkgconf \
  alsa-lib \
  v4l-utils \
  ffmpeg
```

#### Fedora / RHEL
```bash
sudo dnf install -y \
  gcc \
  clang \
  clang-devel \
  pkgconf-pkg-config \
  alsa-lib-devel \
  libv4l-devel \
  ffmpeg-free-devel \
  libswresample-free-devel \
  libswscale-free-devel
```

### Compile & Test
```bash
# Verify rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Run all test suites
cargo test

# Compile optimized release binary
cargo build --release
```
The output binary will be located at `target/release/tacklecast`.

---

## 3. Cargo Feature Flags

| Feature | Default | Description |
|---|---|---|
| `gpu-decode` | Enabled | Windows-only NVIDIA nvJPEG zero-copy GPU MJPEG decode via CUDA and DX12. Automatically disabled on non-Windows platforms. |

To build with strictly minimal dependencies (software decode only):
```bash
cargo build --release --no-default-features
```

---

## 4. Testing Without Capture Hardware

You can run TackleCast in synthetic test-pattern mode without plugging in any capture device:
```bash
# Alternate test patterns
cargo run -- --test

# Force NV12 format pattern
cargo run -- --test-nv12

# Force MJPEG / YUV 4:2:2 pattern
cargo run -- --test-mjpeg
```
