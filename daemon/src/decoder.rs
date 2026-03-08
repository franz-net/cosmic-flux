// SPDX-License-Identifier: GPL-3.0-only

//! GStreamer pipeline for decoding GIF/video files into BGRA frame data.

use anyhow::{anyhow, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

pub struct DecoderPipeline {
    pipeline: gst::Pipeline,
    has_error: Arc<AtomicBool>,
    error_message: Arc<Mutex<Option<String>>>,
    source_fps: Arc<Mutex<f32>>,
    shutdown: Arc<AtomicBool>,
}

impl DecoderPipeline {
    /// Build a GStreamer pipeline that decodes `path` into BGRA frames
    /// scaled to `width x height`, writing each frame into `frame_buffer`.
    /// Audio tracks are ignored — a live wallpaper should not produce sound.
    pub fn new(
        path: &str,
        frame_buffer: Arc<Mutex<Option<Vec<u8>>>>,
        frame_dirty: crate::wayland::DirtyFlag,
        width: u32,
        height: u32,
        fps_cap: u32,
    ) -> Result<Self> {
        // Validate that path points to a regular file (not a FIFO, device, symlink to /proc, etc.)
        let metadata = std::fs::metadata(path)
            .map_err(|e| anyhow!("Cannot access source file '{path}': {e}"))?;
        if !metadata.is_file() {
            return Err(anyhow!("Source path '{path}' is not a regular file"));
        }

        let pipeline = gst::Pipeline::default();

        // Source + decoder
        let filesrc = gst::ElementFactory::make("filesrc")
            .property("location", path)
            .build()
            .map_err(|e| anyhow!("Failed to create filesrc: {e}"))?;

        let decodebin = gst::ElementFactory::make("decodebin")
            .build()
            .map_err(|e| anyhow!("Failed to create decodebin: {e}"))?;

        // Video branch: videorate → rate_caps → videoscale → scale_caps → videoconvert → format_caps → appsink
        // Order matters for CPU: drop frames first, scale in native format, then color-convert the smaller frame.
        let videorate = gst::ElementFactory::make("videorate")
            .property("drop-only", true)
            .build()
            .map_err(|e| anyhow!("Failed to create videorate: {e}"))?;

        // Cap fps in native format before color conversion
        let rate_caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                &gst::Caps::builder("video/x-raw")
                    .field("framerate", gst::Fraction::new(fps_cap.max(1) as i32, 1))
                    .build(),
            )
            .build()
            .map_err(|e| anyhow!("Failed to create rate capsfilter: {e}"))?;

        // Scale in native pixel format (e.g. NV12) BEFORE color conversion —
        // converting a smaller frame is much cheaper than converting then scaling.
        let videoscale = gst::ElementFactory::make("videoscale")
            .build()
            .map_err(|e| anyhow!("Failed to create videoscale: {e}"))?;

        let scale_caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                &gst::Caps::builder("video/x-raw")
                    .field("width", width as i32)
                    .field("height", height as i32)
                    .build(),
            )
            .build()
            .map_err(|e| anyhow!("Failed to create scale capsfilter: {e}"))?;

        let videoconvert = gst::ElementFactory::make("videoconvert")
            .build()
            .map_err(|e| anyhow!("Failed to create videoconvert: {e}"))?;

        let format_caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                &gst::Caps::builder("video/x-raw")
                    .field("format", "BGRA")
                    .build(),
            )
            .build()
            .map_err(|e| anyhow!("Failed to create format capsfilter: {e}"))?;

        let appsink = gst_app::AppSink::builder()
            .sync(true)
            .max_buffers(2)
            .drop(true)
            .name("sink")
            .build();

        // Add video elements to pipeline
        pipeline
            .add_many([
                &filesrc,
                &decodebin,
                &videorate,
                &rate_caps,
                &videoscale,
                &scale_caps,
                &videoconvert,
                &format_caps,
                appsink.upcast_ref(),
            ])
            .map_err(|e| anyhow!("Failed to add elements: {e}"))?;

        // Link static chains
        gst::Element::link_many([&filesrc, &decodebin])
            .map_err(|e| anyhow!("Failed to link filesrc→decodebin: {e}"))?;
        gst::Element::link_many([&videorate, &rate_caps, &videoscale, &scale_caps, &videoconvert, &format_caps, appsink.upcast_ref()])
            .map_err(|e| anyhow!("Failed to link video chain: {e}"))?;

        // Handle decodebin's dynamic pads — link video pads only, ignore audio
        let video_sink_pad = videorate
            .static_pad("sink")
            .ok_or_else(|| anyhow!("videorate has no sink pad"))?;
        let source_fps: Arc<Mutex<f32>> = Arc::new(Mutex::new(0.0));
        let source_fps_flag = Arc::clone(&source_fps);

        decodebin.connect_pad_added(move |_dbin, src_pad| {
            let caps = match src_pad.current_caps() {
                Some(c) => c,
                None => return,
            };
            let structure = match caps.structure(0) {
                Some(s) => s,
                None => return,
            };

            if structure.name().as_str().starts_with("video/") {
                if video_sink_pad.is_linked() {
                    return;
                }
                // Extract source framerate from caps
                if let Ok(fps) = structure.get::<gst::Fraction>("framerate") {
                    let fps_val = fps.numer() as f32 / fps.denom().max(1) as f32;
                    if let Ok(mut f) = source_fps_flag.lock() {
                        *f = fps_val;
                    }
                    tracing::info!("Source video framerate: {fps_val:.1} fps");
                }
                if src_pad.link(&video_sink_pad).is_ok() {
                    tracing::info!("Linked video pad");
                }
            }
            // Audio pads are intentionally ignored — wallpapers should not produce sound
        });

        // AppSink callback: copies decoded frames into shared buffer
        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = match sink.pull_sample() {
                        Ok(s) => s,
                        Err(_) => return Err(gst::FlowError::Eos),
                    };

                    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;

                    let mut fb = frame_buffer.lock().map_err(|_| gst::FlowError::Error)?;
                    match fb.as_mut() {
                        Some(existing) if existing.len() == map.len() => {
                            existing.copy_from_slice(&map);
                        }
                        _ => {
                            *fb = Some(map.to_vec());
                        }
                    }
                    drop(fb);
                    frame_dirty.store(true, Ordering::Relaxed);

                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        // Loop playback: listen for EOS on the bus and seek back to start.
        // Short timed_pop (100ms) + shutdown flag so thread exits quickly on drop.
        let has_error = Arc::new(AtomicBool::new(false));
        let error_message: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let shutdown = Arc::new(AtomicBool::new(false));

        let bus = pipeline.bus().expect("pipeline has no bus");
        let pipeline_weak = pipeline.downgrade();
        let err_flag = Arc::clone(&has_error);
        let err_msg = Arc::clone(&error_message);
        let shutdown_flag = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            loop {
                if shutdown_flag.load(Ordering::Relaxed) {
                    break;
                }
                let msg = match bus.timed_pop(gst::ClockTime::from_mseconds(100)) {
                    Some(msg) => msg,
                    None => continue,
                };
                match msg.view() {
                    gst::MessageView::Eos(_) => {
                        if let Some(pipeline) = pipeline_weak.upgrade() {
                            let _ = pipeline.seek_simple(
                                gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                                gst::ClockTime::ZERO,
                            );
                        } else {
                            break;
                        }
                    }
                    gst::MessageView::Error(err) => {
                        let msg = format!("{}", err.error());
                        tracing::error!("GStreamer error: {msg} ({:?})", err.debug());
                        err_flag.store(true, Ordering::Relaxed);
                        if let Ok(mut m) = err_msg.lock() {
                            *m = Some(msg);
                        }
                        break;
                    }
                    _ => {}
                }
            }
        });

        Ok(Self {
            pipeline,
            has_error,
            error_message,
            source_fps,
            shutdown,
        })
    }

    /// Whether the pipeline has encountered an error.
    pub fn has_error(&self) -> bool {
        self.has_error.load(Ordering::Relaxed)
    }

    /// The source video's native framerate (populated after pad linking).
    pub fn source_fps(&self) -> f32 {
        self.source_fps.lock().map(|f| *f).unwrap_or(0.0)
    }

    /// Get the error message, if any.
    pub fn error_message(&self) -> Option<String> {
        self.error_message.lock().ok().and_then(|m| m.clone())
    }

    pub fn play(&self) {
        if let Err(e) = self.pipeline.set_state(gst::State::Playing) {
            tracing::error!("Failed to set pipeline to Playing: {e}");
        }
    }

    pub fn pause(&self) {
        if let Err(e) = self.pipeline.set_state(gst::State::Paused) {
            tracing::error!("Failed to set pipeline to Paused: {e}");
        }
    }

    pub fn stop(&self) {
        if let Err(e) = self.pipeline.set_state(gst::State::Null) {
            tracing::error!("Failed to set pipeline to Null: {e}");
        }
    }
}

impl Drop for DecoderPipeline {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
