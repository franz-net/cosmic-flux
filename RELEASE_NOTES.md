<!-- Update this file before each tagged release. -->
<!-- The workflow appends auto-generated commit notes below this body. -->

## Highlights

<!-- Replace with the key changes for this release -->
- First release of Cosmic Flux — a live wallpaper system for the COSMIC desktop
- Play video (MP4, WebM, MKV) and GIF files as your desktop wallpaper
- Hardware-accelerated decoding via VA-API (Intel/AMD)
- Display modes: zoom, fit, stretch
- Multi-monitor support with mirror and span modes
- Cross-fade transitions when switching wallpapers
- Tuneable FPS cap (5–60 fps) via panel applet
- Performance stats (CPU%, RAM, FPS) in applet popup
- Auto-restore wallpaper on daemon restart
- Systemd user service with auto-enable on install
- Start daemon button in applet when service is not running

## Install

```sh
sudo apt-get install -y gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-vaapi
sudo dpkg -i cosmic-flux_<version>_amd64.deb
```

Add the **Cosmic Flux** applet to your panel via Settings > Desktop > Panel > Applets.

## Uninstall

```sh
systemctl --user disable --now cosmic-flux-daemon
sudo dpkg -r cosmic-flux
```

## Requirements

- COSMIC desktop environment
- GStreamer 1.x with video decode plugins
- VA-API drivers recommended for hardware decode
