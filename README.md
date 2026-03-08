<p align="center">
  <img src="assets/logo.svg" width="128" alt="Cosmic Flux logo">
</p>

# Cosmic Flux

Cosmic Flux turns your [COSMIC](https://github.com/pop-os/cosmic-epoch) desktop background into a living canvas. Drop in any video (MP4, WebM, MKV) or GIF and watch it play seamlessly behind your windows. Hardware-accelerated decoding keeps CPU usage low, while the panel applet gives you instant access to playback controls, display modes, FPS tuning, and multi-monitor settings — all without leaving your workflow.

## Components

| Component | Description |
|-----------|------------|
| **Daemon** (`cosmic-flux-daemon`) | Wayland layer-shell service that decodes video via GStreamer and renders frames to the desktop background |
| **Applet** (`cosmic-applet-flux`) | COSMIC panel applet for selecting files, controlling playback, and adjusting settings |

The daemon and applet communicate over D-Bus (`com.system76.CosmicFlux1`).

## Features

- Play video (MP4, WebM, MKV) and GIF files as desktop wallpaper
- Native file picker via xdg-desktop-portal
- Playback controls: play, pause, stop
- Display modes: zoom (crop to fill), fit (letterbox), stretch
- Multi-monitor support with mirror and span modes
- Cross-fade transitions when switching wallpapers
- Auto-restore wallpaper on daemon restart
- Cached last frame for instant display on startup
- Hardware video decode via VA-API (Intel/AMD)
- Tuneable FPS cap (5–60 fps) for CPU/quality balance
- Performance stats (CPU%, RAM, FPS) visible in the applet

## Install (Pre-built Package)

Download the latest `.deb` from [Releases](https://github.com/franz-net/cosmic-flux/releases).

```sh
# Download the latest release
wget https://github.com/franz-net/cosmic-flux/releases/download/v0.1.0/cosmic-flux_0.1.0_amd64.deb

# Install (automatically pulls dependencies)
sudo apt install ./cosmic-flux_0.1.0_amd64.deb
```

The daemon is automatically enabled and started on install. Add the **Cosmic Flux** applet to your COSMIC panel via Settings > Desktop > Panel > Applets.

### Uninstall

```sh
# Stop and disable the daemon
systemctl --user disable --now cosmic-flux-daemon

# Remove the package
sudo dpkg -r cosmic-flux
```

User configuration in `~/.config/cosmic/` is preserved after uninstall. To remove it as well:

```sh
rm -rf ~/.config/cosmic/com.system76.CosmicAppletFlux/
```

## Build from Source

### Requirements

**Rust 1.85+** (edition 2024)

```sh
# Ubuntu/Debian
sudo apt-get install -y \
    libgstreamer1.0-dev \
    libgstreamer-plugins-base1.0-dev \
    gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad \
    gstreamer1.0-vaapi

# Fedora
sudo dnf install -y \
    gstreamer1-devel \
    gstreamer1-plugins-base-devel \
    gstreamer1-plugins-good \
    gstreamer1-plugins-bad-free \
    gstreamer1-vaapi
```

### Runtime

- COSMIC desktop environment
- Wayland compositor with `wlr-layer-shell` support
- GStreamer 1.x with video decode plugins
- VA-API drivers for hardware decode (recommended)

## Build & Install

```sh
# Build
just build-release

# Install to ~/.local (binaries, desktop entry, icons, systemd service)
just install

# Enable the daemon to start with your session
systemctl --user enable --now cosmic-flux-daemon
```

## Usage

1. Add the **Cosmic Flux** applet to your COSMIC panel
2. Click the applet icon to open the popup
3. Select a video or GIF file
4. Adjust display mode, FPS cap, and span settings as desired

The daemon automatically restores your last wallpaper on restart.

## Video Recommendations

For the best balance of visual quality and CPU usage:

| Setting | Recommendation |
|---------|---------------|
| **Resolution** | Match your display (1920x1080). 720p is also fine — barely noticeable as a background |
| **Framerate** | 15-24 fps. Lower source fps = lower CPU |
| **Codec** | H.264 (best hardware decode support) |
| **Bitrate** | 2-5 Mbps |
| **Content** | Slow-moving scenes (rotating planets, clouds, water) compress better and use less CPU |

Convert any video to an efficient wallpaper source:

```sh
ffmpeg -i input.mp4 -vf "scale=1920:1080,fps=24" -c:v libx264 -crf 23 -preset slow -an wallpaper.mp4
```

### Multi-Monitor Impact

- **Mirror mode** (default): Decodes at largest monitor resolution. Extra monitors add minimal blit cost.
- **Span mode**: Decodes at the combined bounding box of all monitors (e.g., 2x 1080p = 3840x1080). Use lower-fps sources to compensate.

## Architecture

```
                    D-Bus (session bus)
Applet ◄──────────────────────────────────► Daemon
(panel UI)                                  │
                                            ├─ GStreamer pipeline
                                            │   filesrc → decodebin → videoconvert
                                            │   → videoscale → videorate → capsfilter
                                            │   → appsink (BGRA frames)
                                            │
                                            ├─ Wayland layer-shell surfaces
                                            │   (one per output, wlr-layer-shell background layer)
                                            │
                                            └─ Frame flow
                                                GStreamer BGRA → shared buffer → SHM blit → commit
```

## D-Bus Interface

Service: `com.system76.CosmicFlux1`
Path: `/com/system76/CosmicFlux`

### Methods

| Method | Arguments | Description |
|--------|-----------|-------------|
| `SetSource` | `path: String` | Set video/GIF file path and start playback |
| `Play` | — | Resume playback |
| `Pause` | — | Pause playback |
| `Stop` | — | Stop playback and destroy pipeline |
| `SetFitMode` | `mode: String` | `"zoom"`, `"fit"`, or `"stretch"` |
| `SetSpanMode` | `enabled: bool` | Span wallpaper across monitors |
| `SetFpsCap` | `fps: u32` | 5–60, target frames per second |
| `GetState` | — | Returns `(playing, error, cpu, memory, fps, source_fps)` |

### Properties

`Playing`, `Source`, `FitMode`, `SpanMode`, `FpsCap`, `Error`, `CpuPercent`, `MemoryMb`, `Fps`, `SourceFps`

## Development

```sh
# Run daemon with debug logging
just run-daemon

# Run applet with debug logging
just run-applet

# Lint
just check
```

## License

GPL-3.0-only
