// SPDX-License-Identifier: GPL-3.0-only

use crate::config::Config;
use crate::fl;
use ashpd::desktop::file_chooser::{FileFilter, SelectedFiles};
use cosmic::cosmic_config::{self, CosmicConfigEntry};
use cosmic::iced::{window::Id, Limits, Subscription};
use cosmic::iced_winit::commands::popup::{destroy_popup, get_popup};
use cosmic::prelude::*;
use cosmic::widget;
use futures_util::SinkExt;

const APP_ID: &str = "com.system76.CosmicAppletFlux";

#[derive(Debug, Clone)]
pub enum Message {
    TogglePopup,
    PopupClosed(Id),
    OpenFilePicker,
    FileSelected(Option<String>),
    Play,
    Pause,
    Stop,
    SetFitMode(String),
    SetSpanMode(bool),
    SetFpsCap(u32),
    UpdateConfig(Config),
    DaemonState { playing: bool, error: Option<String>, cpu: f64, memory: f64, fps: f64, source_fps: f64 },
    CommandSent,
    DaemonUnavailable,
}

pub struct AppModel {
    core: cosmic::Core,
    popup: Option<Id>,
    config: Config,
    daemon_playing: bool,
    daemon_available: bool,
    daemon_error: Option<String>,
    daemon_cpu: f64,
    daemon_memory: f64,
    daemon_fps: f64,
    source_fps: f64,
    fit_options: Vec<String>,
}

impl Default for AppModel {
    fn default() -> Self {
        Self {
            core: cosmic::Core::default(),
            popup: None,
            config: Config::default(),
            daemon_playing: false,
            daemon_available: true,
            daemon_error: None,
            daemon_cpu: 0.0,
            daemon_memory: 0.0,
            daemon_fps: 0.0,
            source_fps: 0.0,
            fit_options: Vec::new(),
        }
    }
}

impl cosmic::Application for AppModel {
    type Executor = cosmic::executor::Default;
    type Flags = ();
    type Message = Message;
    const APP_ID: &'static str = APP_ID;

    fn core(&self) -> &cosmic::Core {
        &self.core
    }
    fn core_mut(&mut self) -> &mut cosmic::Core {
        &mut self.core
    }

    fn init(core: cosmic::Core, _flags: ()) -> (Self, Task<cosmic::Action<Message>>) {
        let mut config = cosmic_config::Config::new(APP_ID, Config::VERSION)
            .map(|ctx| match Config::get_entry(&ctx) {
                Ok(config) => config,
                Err((_errors, config)) => config,
            })
            .unwrap_or_default();
        config.fps_cap = config.fps_cap.clamp(5, 60);

        let app = AppModel {
            core,
            config,
            daemon_available: true,
            fit_options: vec![
                String::from("Zoom (crop to fill)"),
                String::from("Fit (letterbox)"),
                String::from("Stretch"),
            ],
            ..Default::default()
        };

        (app, Task::none())
    }

    fn on_close_requested(&self, id: Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    // Panel icon — changes based on playback state
    fn view(&self) -> Element<'_, Message> {
        let icon_name = if self.daemon_playing {
            "com.system76.CosmicAppletFlux"
        } else {
            "com.system76.CosmicAppletFlux-stopped"
        };
        self.core
            .applet
            .icon_button(icon_name)
            .on_press(Message::TogglePopup)
            .into()
    }

    // Popup window contents
    fn view_window(&self, _id: Id) -> Element<'_, Message> {
        // File picker row
        let source_label = if self.config.source_path.is_empty() {
            fl!("no-file-selected")
        } else {
            std::path::Path::new(&self.config.source_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&self.config.source_path)
                .to_owned()
        };

        let file_row = widget::settings::item(
            fl!("source-file"),
            widget::button::text(source_label).on_press(Message::OpenFilePicker),
        );

        // Playback controls: Play/Pause + Stop in one row
        let playback_row = widget::settings::item(
            fl!("playback"),
            widget::row::with_capacity(2)
                .spacing(8)
                .push(if self.daemon_playing {
                    widget::button::text(fl!("pause")).on_press(Message::Pause)
                } else {
                    widget::button::text(fl!("play")).on_press(Message::Play)
                })
                .push(widget::button::text(fl!("stop")).on_press(Message::Stop)),
        );

        // Fit mode dropdown
        let selected_fit = match self.config.fit_mode.as_str() {
            "fit" => Some(1usize),
            "stretch" => Some(2usize),
            _ => Some(0usize),
        };
        let fit_row = widget::settings::item(
            fl!("fit-mode"),
            widget::dropdown(&self.fit_options, selected_fit, |idx| {
                let mode = match idx {
                    1 => "fit",
                    2 => "stretch",
                    _ => "zoom",
                };
                Message::SetFitMode(mode.to_string())
            }),
        );

        let mut content = widget::list_column()
            .padding(5)
            .spacing(0)
            .add(file_row)
            .add(playback_row);

        content = content.add(fit_row);

        // FPS cap slider with source info
        let fps_label = if self.source_fps > 0.0 {
            let recommended = ((self.source_fps / 3.0).round() as u32).clamp(5, 15);
            format!("{} ({}) · src: {:.0}fps, rec: {}", fl!("fps-cap"), self.config.fps_cap, self.source_fps, recommended)
        } else {
            format!("{} ({})", fl!("fps-cap"), self.config.fps_cap)
        };
        let fps_row = widget::settings::item(
            fps_label,
            widget::slider(5.0..=60.0, self.config.fps_cap as f64, |v| {
                Message::SetFpsCap(v as u32)
            })
            .step(5.0),
        );
        content = content.add(fps_row);

        // Span mode toggle
        let span_row = widget::settings::item(
            fl!("span-mode"),
            widget::toggler(self.config.span_mode)
                .on_toggle(Message::SetSpanMode),
        );
        content = content.add(span_row);

        // Show performance stats when playing
        if self.daemon_playing {
            let stats_text = format!(
                "CPU: {:.0}%  |  RAM: {:.0} MB  |  FPS: {:.0}",
                self.daemon_cpu, self.daemon_memory, self.daemon_fps
            );
            content = content.add(widget::text::body(stats_text));
        }

        if let Some(err) = &self.daemon_error {
            content = content.add(widget::text(format!("{}: {err}", fl!("error"))));
        }

        if !self.daemon_available {
            content = content.add(widget::text(fl!("daemon-unavailable")));
        }

        self.core.applet.popup_container(content).into()
    }

    fn subscription(&self) -> Subscription<Message> {
        struct DaemonPoll;

        Subscription::batch(vec![
            self.core()
                .watch_config::<Config>(APP_ID)
                .map(|update| Message::UpdateConfig(update.config)),
            Subscription::run_with_id(
                std::any::TypeId::of::<DaemonPoll>(),
                cosmic::iced::stream::channel(4, move |mut sender| async move {
                    loop {
                        match poll_daemon_state().await {
                            Ok((playing, error, cpu, memory, fps, source_fps)) => {
                                let _ = sender
                                    .send(Message::DaemonState { playing, error, cpu, memory, fps, source_fps })
                                    .await;
                            }
                            Err(_) => {
                                let _ = sender.send(Message::DaemonUnavailable).await;
                            }
                        }
                        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                    }
                }),
            ),
        ])
    }

    fn update(&mut self, message: Message) -> Task<cosmic::Action<Message>> {
        match message {
            Message::TogglePopup => {
                return if let Some(p) = self.popup.take() {
                    destroy_popup(p)
                } else {
                    let Some(main_id) = self.core.main_window_id() else {
                        return Task::none();
                    };
                    let new_id = Id::unique();
                    self.popup.replace(new_id);
                    let mut popup_settings = self.core.applet.get_popup_settings(
                        main_id,
                        new_id,
                        None,
                        None,
                        None,
                    );
                    popup_settings.positioner.size_limits = Limits::NONE
                        .max_width(372.0)
                        .min_width(300.0)
                        .min_height(200.0)
                        .max_height(600.0);
                    get_popup(popup_settings)
                };
            }
            Message::PopupClosed(id) => {
                if self.popup.as_ref() == Some(&id) {
                    self.popup = None;
                }
            }
            Message::UpdateConfig(mut config) => {
                config.fps_cap = config.fps_cap.clamp(5, 60);
                self.config = config;
            }
            Message::DaemonState { playing, error, cpu, memory, fps, source_fps } => {
                self.daemon_playing = playing;
                self.daemon_error = error.map(|e| {
                    if e.len() > 256 {
                        e.char_indices().take_while(|&(i, _)| i < 256).map(|(_, c)| c).collect()
                    } else {
                        e
                    }
                });
                self.daemon_cpu = cpu.clamp(0.0, 10000.0);
                self.daemon_memory = memory.clamp(0.0, 1_000_000.0);
                self.daemon_fps = fps.clamp(0.0, 10000.0);
                self.source_fps = source_fps.clamp(0.0, 10000.0);
                self.daemon_available = true;
            }
            Message::CommandSent => {
                // Command completed — state will be picked up by the daemon poll
            }
            Message::DaemonUnavailable => {
                self.daemon_available = false;
            }
            Message::OpenFilePicker => {
                return Task::perform(pick_media_file(), |path| {
                    cosmic::Action::App(Message::FileSelected(path))
                });
            }
            Message::FileSelected(Some(path)) => {
                // Validate the path from the file portal is an absolute file path
                let p = std::path::Path::new(&path);
                if !p.is_absolute() || !p.is_file() {
                    tracing::warn!("Rejected non-absolute or non-file path from portal: {path}");
                    return Task::none();
                }
                self.config.source_path = path.clone();
                self.config.autostart = true;
                self.save_config();
                self.daemon_playing = true;
                return Task::perform(send_command(DaemonCommand::SetSource(path)), |_| {
                    cosmic::Action::App(Message::CommandSent)
                });
            }
            Message::FileSelected(None) => {
                // User cancelled file picker — do nothing
            }
            Message::Play => {
                self.daemon_playing = true;
                return Task::perform(send_command(DaemonCommand::Play), |_| {
                    cosmic::Action::App(Message::CommandSent)
                });
            }
            Message::Pause => {
                self.daemon_playing = false;
                return Task::perform(send_command(DaemonCommand::Pause), |_| {
                    cosmic::Action::App(Message::CommandSent)
                });
            }
            Message::Stop => {
                self.daemon_playing = false;
                self.config.autostart = false;
                self.save_config();
                return Task::perform(send_command(DaemonCommand::Stop), |_| {
                    cosmic::Action::App(Message::CommandSent)
                });
            }
            Message::SetFitMode(mode) => {
                self.config.fit_mode = mode.clone();
                self.save_config();
                return Task::perform(
                    send_command(DaemonCommand::SetFitMode(mode)),
                    |_| cosmic::Action::App(Message::CommandSent),
                );
            }
            Message::SetSpanMode(enabled) => {
                self.config.span_mode = enabled;
                self.save_config();
                return Task::perform(
                    send_command(DaemonCommand::SetSpanMode(enabled)),
                    |_| cosmic::Action::App(Message::CommandSent),
                );
            }
            Message::SetFpsCap(fps) => {
                let fps = fps.clamp(5, 60);
                self.config.fps_cap = fps;
                self.save_config();
                return Task::perform(
                    send_command(DaemonCommand::SetFpsCap(fps)),
                    |_| cosmic::Action::App(Message::CommandSent),
                );
            }
        }
        Task::none()
    }

    fn style(&self) -> Option<cosmic::iced_runtime::Appearance> {
        Some(cosmic::applet::style())
    }
}

impl AppModel {
    fn save_config(&self) {
        if let Ok(ctx) = cosmic_config::Config::new(APP_ID, Config::VERSION) {
            if let Err(e) = self.config.write_entry(&ctx) {
                tracing::error!("Failed to save config: {e:?}");
            }
        }
    }
}

// --- Async helpers ---

/// Open the native file picker via xdg-desktop-portal (works on COSMIC/Wayland).
async fn pick_media_file() -> Option<String> {
    let response = SelectedFiles::open_file()
        .title("Select Live Wallpaper")
        .accept_label("Open")
        .modal(true)
        .multiple(false)
        .filter(
            FileFilter::new("Video & GIF files")
                .mimetype("video/mp4")
                .mimetype("video/webm")
                .mimetype("video/x-matroska")
                .mimetype("image/gif"),
        )
        .filter(FileFilter::new("All files").glob("*"))
        .send()
        .await
        .ok()?
        .response()
        .ok()?;

    let uri = response.uris().first()?;
    uri.to_file_path().ok().map(|p| p.to_string_lossy().into_owned())
}

#[derive(Debug, Clone)]
enum DaemonCommand {
    Play,
    Pause,
    Stop,
    SetSource(String),
    SetFitMode(String),
    SetSpanMode(bool),
    SetFpsCap(u32),
}

async fn send_command(cmd: DaemonCommand) -> Result<(), anyhow::Error> {
    let proxy = crate::dbus_client::connect().await?;
    match cmd {
        DaemonCommand::Play => proxy.play().await?,
        DaemonCommand::Pause => proxy.pause().await?,
        DaemonCommand::Stop => proxy.stop().await?,
        DaemonCommand::SetSource(p) => proxy.set_source(&p).await?,
        DaemonCommand::SetFitMode(m) => proxy.set_fit_mode(&m).await?,
        DaemonCommand::SetSpanMode(e) => proxy.set_span_mode(e).await?,
        DaemonCommand::SetFpsCap(f) => proxy.set_fps_cap(f).await?,
    }
    Ok(())
}

async fn poll_daemon_state() -> Result<(bool, Option<String>, f64, f64, f64, f64), anyhow::Error> {
    let proxy = crate::dbus_client::connect().await?;
    // Single D-Bus method call returns all state at once (replaces 6+ property reads)
    match proxy.get_state().await {
        Ok((playing, error, cpu, memory, fps, source_fps)) => {
            let error = if error.is_empty() { None } else { Some(error) };
            Ok((playing, error, cpu, memory, fps, source_fps))
        }
        Err(e) => {
            // Connection may be stale — clear cached proxy so next call reconnects
            crate::dbus_client::clear_cache().await;
            Err(e.into())
        }
    }
}
