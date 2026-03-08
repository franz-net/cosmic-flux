default: build

build:
    cargo build

build-release:
    cargo build --release

run-daemon:
    RUST_LOG=debug cargo run -p cosmic-flux-daemon

run-applet:
    RUST_LOG=debug cargo run -p cosmic-applet-flux

install:
    install -Dm755 target/release/cosmic-flux-daemon \
        ~/.local/bin/cosmic-flux-daemon
    install -Dm755 target/release/cosmic-applet-flux \
        ~/.local/bin/cosmic-applet-flux
    install -Dm644 applet/resources/app.desktop \
        ~/.local/share/applications/com.system76.CosmicAppletFlux.desktop
    install -Dm644 applet/resources/icon.svg \
        ~/.local/share/icons/hicolor/scalable/apps/com.system76.CosmicAppletFlux.svg
    install -Dm644 applet/resources/icon-stopped.svg \
        ~/.local/share/icons/hicolor/scalable/apps/com.system76.CosmicAppletFlux-stopped.svg
    install -Dm644 data/cosmic-flux-daemon.service \
        ~/.config/systemd/user/cosmic-flux-daemon.service
    systemctl --user daemon-reload

check:
    cargo clippy --all-targets --all-features

clean:
    cargo clean
