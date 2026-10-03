![Feathercut](assets/feather.png)

# Feathercut

Lightweight video editor with target file size compression.

_Subsecond launch time; Targetted compression; Simple and quick_

---

### Download

For the official prebuilt application with all playback engines, codecs, and media dependencies bundled, get Feathercut on the **Microsoft Store** (€1). You can also build it yourself following the instructions below.

---

### Key Features

- **Trimming & Sequencing**: Fast single-clip In/Out cutting, multi-layer clips workspace, splitting, track reordering, and gap preservation.
- **Export Engines**:
  - *Fast Copy*: Keyframe-aligned stream copy without re-encoding.
  - *Target Size*: Bitrate-calculated compression guaranteeing strict file size caps.
  - *Exact Lossless*: Frame-accurate FFV1/MKV rendering.
- **Framing & Audio**: Interactive cropping with aspect-ratio snapping, non-blocking background audio waveforms, per-clip gain/mute, and multi-track audio mixdown.
- **Native Glass UI**: DirectComposition Gaussian backdrop blur with customizable opacity and themes.

---

### Building from Source

#### Prerequisites

- **Rust**: 2024 edition / recent stable (`x86_64-pc-windows-msvc`).
- **Windows SDK / C++ Build Tools**: Required for the Windows resource compiler (`rc.exe`) to embed the executable icon.
- **FFmpeg & FFprobe**: Must be available on system `PATH` for timeline export and media probing.
- **Playback Runtime (`libmpv-2.dll`)**: Feathercut dynamically loads `libmpv-2.dll` at runtime for video rendering. A compatible Windows build (e.g. from [mpv.io](https://mpv.io/installation/)) must be placed:
  - In `runtime/mpv/libmpv-2.dll` (Cargo will automatically copy it to the target directory during build), or
  - Directly beside the compiled binary in `target/release/`.

#### Build

```powershell
cargo build --release
```

#### Run

```powershell
.\target\release\feathercut.exe [path\to\video.mp4]
```

#### Test Suite

```powershell
cargo test --bin feathercut
```

---

### License

Feathercut is licensed under the [GNU General Public License v3.0](LICENSE) (`GPL-3.0-only`). Third-party dependencies retain their respective upstream licenses.
