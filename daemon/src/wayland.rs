// SPDX-License-Identifier: GPL-3.0-only

//! Wayland layer-shell surface setup and frame rendering loop.
//! Supports multiple outputs with mirror mode (default) and span mode.

use anyhow::Result;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_seat,
    delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{Capability, SeatHandler, SeatState},
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_seat, wl_shm, wl_surface},
    Connection, QueueHandle,
};

/// Video scaling mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FitMode {
    Zoom,
    Fit,
    Stretch,
}

impl FitMode {
    pub fn from_str(s: &str) -> Self {
        match s {
            "fit" => Self::Fit,
            "stretch" => Self::Stretch,
            _ => Self::Zoom,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Zoom => "zoom",
            Self::Fit => "fit",
            Self::Stretch => "stretch",
        }
    }
}

/// Commands sent from the D-Bus handler to the Wayland loop.
#[derive(Debug)]
pub enum Command {
    SetSource(String),
    Play,
    Pause,
    Stop,
    SetFitMode(FitMode),
    SetSpanMode(bool),
    SetFpsCap(u32),
}

/// Shared readable state published by the daemon (read by D-Bus properties).
pub struct DaemonState {
    pub source_path: String,
    pub playing: bool,
    pub fit_mode: FitMode,
    pub span_mode: bool,
    pub error: Option<String>,
    pub cpu_percent: f32,
    pub memory_mb: f32,
    pub fps: f32,
    pub fps_cap: u32,
    pub source_fps: f32,
}

/// Shared flag set by GStreamer when a new frame is available.
pub type DirtyFlag = Arc<AtomicBool>;

/// Per-output state: each connected monitor gets its own layer surface and SHM pool.
struct OutputSurface {
    output: wl_output::WlOutput,
    layer: LayerSurface,
    pool: SlotPool,
    width: u32,
    height: u32,
    logical_position: (i32, i32),
    logical_size: (i32, i32),
    first_configure: bool,
    configured: bool,
}

/// Duration of the cross-fade transition in milliseconds.
const FADE_DURATION_MS: u128 = 500;

struct WallpaperRenderer {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    compositor: CompositorState,
    layer_shell: LayerShell,

    outputs: Vec<OutputSurface>,
    span_mode: bool,
    decode_width: u32,
    decode_height: u32,

    frame_buffer: Arc<Mutex<Option<Vec<u8>>>>,
    frame_dirty: DirtyFlag,
    daemon_state: Arc<Mutex<DaemonState>>,
    command_rx: std::sync::mpsc::Receiver<Command>,

    pipeline: Option<crate::decoder::DecoderPipeline>,
    is_playing: bool,
    exit: bool,

    // Cross-fade state
    prev_frame: Option<Vec<u8>>,
    prev_decode_w: u32,
    prev_decode_h: u32,
    fade_start: Option<std::time::Instant>,

    // Frame cache for static fallback
    last_cache_save: Option<std::time::Instant>,

    // Performance stats
    frames_drawn: u32,
    fps_last_update: std::time::Instant,
    last_proc_stat: Option<(std::time::Instant, u64)>, // (time, total_cpu_ticks)

    // Tuneable settings
    fps_cap: u32,
    fit_mode: FitMode,

    // Reusable buffers for cross-fade blending (avoid per-frame allocation)
    blend_buffer: Vec<u8>,
    scaled_prev_buffer: Vec<u8>,

    // Cached bounding box (invalidated on output add/remove/configure)
    cached_bb: ((i32, i32), (i32, i32)),
    bb_dirty: bool,
}

pub fn run(
    frame_buffer: Arc<Mutex<Option<Vec<u8>>>>,
    frame_dirty: DirtyFlag,
    daemon_state: Arc<Mutex<DaemonState>>,
    command_rx: std::sync::mpsc::Receiver<Command>,
) -> Result<()> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;

    let mut renderer = WallpaperRenderer {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        compositor,
        layer_shell,
        outputs: Vec::new(),
        span_mode: false,
        decode_width: 0,
        decode_height: 0,
        frame_buffer,
        frame_dirty,
        daemon_state,
        command_rx,
        pipeline: None,
        is_playing: false,
        exit: false,
        prev_frame: None,
        prev_decode_w: 0,
        prev_decode_h: 0,
        fade_start: None,
        last_cache_save: None,
        frames_drawn: 0,
        fps_last_update: std::time::Instant::now(),
        last_proc_stat: None,
        fps_cap: 15,
        fit_mode: FitMode::Zoom,
        blend_buffer: Vec::new(),
        scaled_prev_buffer: Vec::new(),
        cached_bb: ((0, 0), (0, 0)),
        bb_dirty: true,
    };

    tracing::info!("Entering Wayland event loop");

    // Use poll-based loop so D-Bus commands are processed even when idle.
    // blocking_dispatch would block forever when no Wayland events are pending,
    // preventing D-Bus commands from being picked up.
    loop {
        // Flush any pending outgoing messages
        event_queue.flush()?;

        // Poll the Wayland fd with a 100ms timeout so we wake up to check commands
        if let Some(guard) = event_queue.prepare_read() {
            let fd = guard.connection_fd();
            let raw_fd = std::os::unix::io::AsRawFd::as_raw_fd(&fd);
            let mut pollfd = libc::pollfd {
                fd: raw_fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // Safety: calling poll on a valid fd with proper pollfd struct
            let poll_ret = unsafe { libc::poll(&mut pollfd, 1, 100) };
            if poll_ret > 0 && pollfd.revents & libc::POLLIN != 0 {
                match guard.read() {
                    Ok(_) => {}
                    Err(wayland_client::backend::WaylandError::Io(e))
                        if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }

        // Dispatch any events that were read
        event_queue.dispatch_pending(&mut renderer)?;

        while let Ok(cmd) = renderer.command_rx.try_recv() {
            renderer.handle_command(cmd, &qh);
        }

        // If commands arrived after output configure (race between Wayland events
        // and D-Bus commands), try starting the deferred pipeline now.
        renderer.try_deferred_start(&qh);

        if renderer.exit {
            break;
        }
    }

    Ok(())
}

// --- Blit helpers (free functions) ---

/// Nearest-neighbor scale BGRA frame from src to dst dimensions.
fn blit_scaled(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    dst: &mut [u8],
    dst_w: u32,
    dst_h: u32,
    fit_mode: FitMode,
) {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        dst.fill(0);
        return;
    }
    let Some(expected_src) = (src_w as usize).checked_mul(src_h as usize).and_then(|n| n.checked_mul(4)) else {
        dst.fill(0);
        return;
    };
    let Some(expected_dst) = (dst_w as usize).checked_mul(dst_h as usize).and_then(|n| n.checked_mul(4)) else {
        dst.fill(0);
        return;
    };
    if src.len() < expected_src || dst.len() < expected_dst {
        let len = dst.len();
        dst[..expected_dst.min(len)].fill(0);
        return;
    }

    // Fast path: dimensions match exactly — direct copy, no per-pixel scaling
    if src_w == dst_w && src_h == dst_h {
        dst[..expected_dst].copy_from_slice(&src[..expected_dst]);
        return;
    }

    match fit_mode {
        FitMode::Fit => blit_fit(src, src_w, src_h, dst, dst_w, dst_h),
        FitMode::Zoom => blit_zoom(src, src_w, src_h, dst, dst_w, dst_h),
        FitMode::Stretch => blit_stretch(src, src_w, src_h, dst, dst_w, dst_h),
    }
}

/// Build a pre-computed x-index lookup table mapping dst_x → src byte offset.
/// Avoids per-pixel division in the inner blit loop.
fn build_x_lut(dst_w: u32, src_w: u32) -> Vec<usize> {
    (0..dst_w as usize)
        .map(|dx| {
            let sx = (dx as u64 * src_w as u64 / dst_w as u64) as usize;
            sx.min(src_w as usize - 1) * 4
        })
        .collect()
}

/// Copy a row of pixels from src to dst using a pre-computed x-index LUT.
/// Uses u32 pixel copies for better throughput.
#[inline(always)]
fn copy_row_lut(src: &[u8], src_row: usize, dst: &mut [u8], dst_row: usize, x_lut: &[usize]) {
    for (dx, &sx_off) in x_lut.iter().enumerate() {
        let si = src_row + sx_off;
        let di = dst_row + dx * 4;
        // Copy 4 bytes as a single u32 — avoids per-byte copy_from_slice overhead
        let pixel = u32::from_ne_bytes([src[si], src[si + 1], src[si + 2], src[si + 3]]);
        dst[di..di + 4].copy_from_slice(&pixel.to_ne_bytes());
    }
}

/// Stretch: scale ignoring aspect ratio.
fn blit_stretch(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    dst: &mut [u8],
    dst_w: u32,
    dst_h: u32,
) {
    if src_w == dst_w && src_h == dst_h {
        let n = (dst_w as usize) * (dst_h as usize) * 4;
        dst[..n].copy_from_slice(&src[..n]);
        return;
    }
    let x_lut = build_x_lut(dst_w, src_w);
    let dst_stride = dst_w as usize * 4;
    let src_stride = src_w as usize * 4;
    for dy in 0..dst_h as usize {
        let sy = (dy as u64 * src_h as u64 / dst_h as u64) as usize;
        copy_row_lut(src, sy * src_stride, dst, dy * dst_stride, &x_lut);
    }
}

/// Fit (letterbox): scale preserving aspect ratio, black bars on sides.
fn blit_fit(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    dst: &mut [u8],
    dst_w: u32,
    dst_h: u32,
) {
    let expected_dst = (dst_w as usize) * (dst_h as usize) * 4;
    dst[..expected_dst].fill(0); // black background

    let src_aspect = src_w as f64 / src_h as f64;
    let dst_aspect = dst_w as f64 / dst_h as f64;

    let (render_w, render_h) = if src_aspect > dst_aspect {
        (dst_w, ((dst_w as f64 / src_aspect) as u32).max(1).min(dst_h))
    } else {
        (((dst_h as f64 * src_aspect) as u32).max(1).min(dst_w), dst_h)
    };

    let offset_x = (dst_w - render_w) / 2;
    let offset_y = (dst_h - render_h) / 2;
    let x_lut = build_x_lut(render_w, src_w);
    let src_stride = src_w as usize * 4;
    let dst_stride = dst_w as usize * 4;

    for dy in 0..render_h as usize {
        let sy = ((dy as u64 * src_h as u64 / render_h as u64) as usize).min(src_h as usize - 1);
        let src_row = sy * src_stride;
        let dst_row = (offset_y as usize + dy) * dst_stride + offset_x as usize * 4;
        copy_row_lut(src, src_row, dst, dst_row, &x_lut);
    }
}

/// Zoom (crop to fill): scale preserving aspect ratio, crop overflow.
fn blit_zoom(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    dst: &mut [u8],
    dst_w: u32,
    dst_h: u32,
) {
    let src_aspect = src_w as f64 / src_h as f64;
    let dst_aspect = dst_w as f64 / dst_h as f64;

    let (crop_w, crop_h) = if src_aspect > dst_aspect {
        (((src_h as f64 * dst_aspect) as u32).max(1), src_h)
    } else {
        (src_w, ((src_w as f64 / dst_aspect) as u32).max(1))
    };

    let crop_x = (src_w - crop_w) / 2;
    let crop_y = (src_h - crop_h) / 2;

    // Build x-LUT with crop offset baked in
    let x_lut: Vec<usize> = (0..dst_w as usize)
        .map(|dx| {
            let sx = (crop_x + (dx as u64 * crop_w as u64 / dst_w as u64) as u32).min(src_w - 1);
            sx as usize * 4
        })
        .collect();
    let src_stride = src_w as usize * 4;
    let dst_stride = dst_w as usize * 4;

    for dy in 0..dst_h as usize {
        let sy = (crop_y + (dy as u64 * crop_h as u64 / dst_h as u64) as u32).min(src_h - 1);
        copy_row_lut(src, sy as usize * src_stride, dst, dy * dst_stride, &x_lut);
    }
}

/// Span mode: extract viewport for one output from the full bounding-box frame.
fn blit_viewport(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    dst: &mut [u8],
    dst_w: u32,
    dst_h: u32,
    logical_pos: (i32, i32),
    logical_size: (i32, i32),
    bb_origin: (i32, i32),
    bb_size: (i32, i32),
) {
    let expected_dst = (dst_w as usize) * (dst_h as usize) * 4;
    if bb_size.0 <= 0 || bb_size.1 <= 0 || src.len() < (src_w as usize * src_h as usize * 4) {
        let len = dst.len();
        dst[..expected_dst.min(len)].fill(0);
        return;
    }

    let rel_x = (logical_pos.0 - bb_origin.0) as f64;
    let rel_y = (logical_pos.1 - bb_origin.1) as f64;
    let lw = logical_size.0 as f64;
    let lh = logical_size.1 as f64;
    let bb_w = bb_size.0 as f64;
    let bb_h = bb_size.1 as f64;

    // Pre-compute x-LUT with viewport mapping baked in
    let x_lut: Vec<usize> = (0..dst_w as usize)
        .map(|dx| {
            let frac_x = ((rel_x + lw * dx as f64 / dst_w as f64) / bb_w).clamp(0.0, 1.0);
            let sx = ((frac_x * src_w as f64) as usize).min(src_w as usize - 1);
            sx * 4
        })
        .collect();
    let src_stride = src_w as usize * 4;
    let dst_stride = dst_w as usize * 4;

    for dy in 0..dst_h as usize {
        let frac_y = ((rel_y + lh * dy as f64 / dst_h as f64) / bb_h).clamp(0.0, 1.0);
        let sy = ((frac_y * src_h as f64) as usize).min(src_h as usize - 1);
        copy_row_lut(src, sy * src_stride, dst, dy * dst_stride, &x_lut);
    }
}

/// Alpha-blend two BGRA frames. alpha: 0.0 = fully old, 1.0 = fully new.
/// Processes pixels as u32 with channel extraction for better auto-vectorization.
fn blend_frames(old: &[u8], new: &[u8], dst: &mut Vec<u8>, alpha: f32) {
    let len = (old.len().min(new.len())) & !3;
    dst.resize(len, 0);
    let a = (alpha * 256.0) as u32;
    let inv = ((1.0 - alpha) * 256.0) as u32;
    // Process as u32 pixels for better throughput
    let pixel_count = len / 4;
    for i in 0..pixel_count {
        let off = i * 4;
        let b = (old[off] as u32 * inv + new[off] as u32 * a) >> 8;
        let g = (old[off + 1] as u32 * inv + new[off + 1] as u32 * a) >> 8;
        let r = (old[off + 2] as u32 * inv + new[off + 2] as u32 * a) >> 8;
        dst[off] = b as u8;
        dst[off + 1] = g as u8;
        dst[off + 2] = r as u8;
        dst[off + 3] = 0xFF;
    }
}

/// Draw a single output surface. Free function to avoid borrow conflicts with WallpaperRenderer.
fn draw_single_output(
    os: &mut OutputSurface,
    frame: Option<&[u8]>,
    src_w: u32,
    src_h: u32,
    span_mode: bool,
    is_playing: bool,
    fit_mode: FitMode,
    bb_origin: (i32, i32),
    bb_size: (i32, i32),
    qh: &QueueHandle<WallpaperRenderer>,
) {
    let dst_w = os.width;
    let dst_h = os.height;
    if dst_w == 0 || dst_h == 0 || !os.configured {
        return;
    }
    let stride = dst_w as i32 * 4;

    let (buffer, canvas) = match os.pool.create_buffer(
        dst_w as i32,
        dst_h as i32,
        stride,
        wl_shm::Format::Argb8888,
    ) {
        Ok(pair) => pair,
        Err(e) => {
            tracing::error!("Failed to create SHM buffer: {e}");
            return;
        }
    };

    match frame {
        Some(frame) if src_w > 0 && src_h > 0 => {
            if span_mode {
                blit_viewport(
                    frame,
                    src_w,
                    src_h,
                    canvas,
                    dst_w,
                    dst_h,
                    os.logical_position,
                    os.logical_size,
                    bb_origin,
                    bb_size,
                );
            } else {
                blit_scaled(frame, src_w, src_h, canvas, dst_w, dst_h, fit_mode);
            }
        }
        _ => {
            let n = (dst_w as usize) * (dst_h as usize) * 4;
            canvas[..n].fill(0);
        }
    }

    os.layer
        .wl_surface()
        .damage_buffer(0, 0, dst_w as i32, dst_h as i32);

    if is_playing {
        os.layer
            .wl_surface()
            .frame(qh, os.layer.wl_surface().clone());
    }

    if let Err(e) = buffer.attach_to(os.layer.wl_surface()) {
        tracing::error!("Failed to attach buffer: {e}");
        return;
    }
    os.layer.commit();
}

impl WallpaperRenderer {
    /// Update FPS counter and process stats. Called once per draw cycle.
    fn update_stats(&mut self) {
        self.frames_drawn += 1;
        let fps_elapsed = self.fps_last_update.elapsed();
        if fps_elapsed.as_secs() >= 1 {
            let fps = self.frames_drawn as f32 / fps_elapsed.as_secs_f32();
            self.frames_drawn = 0;
            self.fps_last_update = std::time::Instant::now();
            if let Ok(mut state) = self.daemon_state.lock() {
                state.fps = fps;
            }
        }
        self.update_process_stats();
    }

    /// Save the current frame to disk cache periodically.
    fn maybe_save_cache(&mut self, frame: &[u8], src_w: u32, src_h: u32) {
        let should_save = self
            .last_cache_save
            .map(|t| t.elapsed().as_secs() >= 10)
            .unwrap_or(true);
        if should_save && src_w > 0 && src_h > 0 {
            self.last_cache_save = Some(std::time::Instant::now());
            let frame_clone = frame.to_vec();
            let w = src_w;
            let h = src_h;
            std::thread::spawn(move || {
                save_frame_cache(&frame_clone, w, h);
            });
        }
    }

    fn draw_all_outputs(&mut self, qh: &QueueHandle<Self>) {
        self.update_stats();
        let src_w = self.decode_width;
        let src_h = self.decode_height;
        let span = self.span_mode;
        let playing = self.is_playing || self.fade_start.is_some();
        let fit_mode = self.fit_mode;
        let (bb_origin, bb_size) = self.bounding_box();

        // Cross-fade path: reuses self.blend_buffer to avoid per-frame allocation
        if let (Some(prev), Some(start)) = (&self.prev_frame, self.fade_start) {
            let elapsed = start.elapsed().as_millis();
            if elapsed >= FADE_DURATION_MS {
                self.prev_frame = None;
                self.fade_start = None;
                // Fall through to normal path below
            } else {
                let frame_data = self.frame_buffer.lock().ok().and_then(|mut f| f.take());
                if let Some(ref new_frame) = frame_data {
                    let alpha = elapsed as f32 / FADE_DURATION_MS as f32;
                    if self.prev_decode_w == src_w && self.prev_decode_h == src_h {
                        blend_frames(prev, new_frame, &mut self.blend_buffer, alpha);
                    } else {
                        let expected = (src_w as usize) * (src_h as usize) * 4;
                        self.scaled_prev_buffer.resize(expected, 0);
                        blit_stretch(prev, self.prev_decode_w, self.prev_decode_h, &mut self.scaled_prev_buffer, src_w, src_h);
                        blend_frames(&self.scaled_prev_buffer, new_frame, &mut self.blend_buffer, alpha);
                    }
                } else {
                    self.blend_buffer.clear();
                    self.blend_buffer.extend_from_slice(prev);
                }
                for os in &mut self.outputs {
                    draw_single_output(os, Some(&self.blend_buffer), src_w, src_h, span, playing, fit_mode, bb_origin, bb_size, qh);
                }
                // Put the frame back so GStreamer can reuse the allocation
                if let Some(frame) = frame_data {
                    if let Ok(mut fb) = self.frame_buffer.lock() {
                        *fb = Some(frame);
                    }
                }
                return;
            }
        }

        // Swap frame out of the mutex quickly to minimize lock contention with GStreamer.
        // The frame is put back after blitting so the next callback can reuse the allocation.
        let frame_data = self.frame_buffer.lock().ok().and_then(|mut f| f.take());

        if self.is_playing {
            if let Some(ref frame) = frame_data {
                self.maybe_save_cache(frame, src_w, src_h);
            }
        }

        for os in &mut self.outputs {
            draw_single_output(os, frame_data.as_deref(), src_w, src_h, span, playing, fit_mode, bb_origin, bb_size, qh);
        }

        // Put the buffer back so GStreamer can reuse the allocation
        if let Some(frame) = frame_data {
            if let Ok(mut fb) = self.frame_buffer.lock() {
                *fb = Some(frame);
            }
        }
    }

    fn draw_output_by_surface(
        &mut self,
        surface: &wl_surface::WlSurface,
        qh: &QueueHandle<Self>,
    ) {
        let src_w = self.decode_width;
        let src_h = self.decode_height;
        let span = self.span_mode;
        let playing = self.is_playing || self.fade_start.is_some();
        let fit_mode = self.fit_mode;
        let (bb_origin, bb_size) = self.bounding_box();

        // Cross-fade path: reuses self.blend_buffer to avoid per-frame allocation
        if let (Some(prev), Some(start)) = (&self.prev_frame, self.fade_start) {
            let elapsed = start.elapsed().as_millis();
            if elapsed >= FADE_DURATION_MS {
                self.prev_frame = None;
                self.fade_start = None;
            } else {
                let frame_data = self.frame_buffer.lock().ok().and_then(|mut f| f.take());
                if let Some(ref new_frame) = frame_data {
                    let alpha = elapsed as f32 / FADE_DURATION_MS as f32;
                    if self.prev_decode_w == src_w && self.prev_decode_h == src_h {
                        blend_frames(prev, new_frame, &mut self.blend_buffer, alpha);
                    } else {
                        let expected = (src_w as usize) * (src_h as usize) * 4;
                        self.scaled_prev_buffer.resize(expected, 0);
                        blit_stretch(prev, self.prev_decode_w, self.prev_decode_h, &mut self.scaled_prev_buffer, src_w, src_h);
                        blend_frames(&self.scaled_prev_buffer, new_frame, &mut self.blend_buffer, alpha);
                    }
                } else {
                    self.blend_buffer.clear();
                    self.blend_buffer.extend_from_slice(prev);
                }
                if let Some(os) = self.outputs.iter_mut().find(|o| o.layer.wl_surface() == surface) {
                    draw_single_output(os, Some(&self.blend_buffer), src_w, src_h, span, playing, fit_mode, bb_origin, bb_size, qh);
                }
                if let Some(frame) = frame_data {
                    if let Ok(mut fb) = self.frame_buffer.lock() {
                        *fb = Some(frame);
                    }
                }
                return;
            }
        }

        // Swap frame out quickly to minimize lock contention
        let frame_data = self.frame_buffer.lock().ok().and_then(|mut f| f.take());

        if let Some(os) = self.outputs.iter_mut().find(|o| o.layer.wl_surface() == surface) {
            draw_single_output(os, frame_data.as_deref(), src_w, src_h, span, playing, fit_mode, bb_origin, bb_size, qh);
        }

        if let Some(frame) = frame_data {
            if let Ok(mut fb) = self.frame_buffer.lock() {
                *fb = Some(frame);
            }
        }
    }

    /// Start the frame callback loop on all outputs.
    fn request_frame(&self, qh: &QueueHandle<Self>) {
        for os in &self.outputs {
            os.layer
                .wl_surface()
                .frame(qh, os.layer.wl_surface().clone());
            os.layer.commit();
        }
    }

    fn handle_command(&mut self, cmd: Command, qh: &QueueHandle<Self>) {
        match cmd {
            Command::SetSource(path) => {
                // Snapshot current frame for cross-fade — take() then clone
                // outside the lock to minimize mutex hold time
                if self.decode_width > 0 && self.decode_height > 0 {
                    let taken = self.frame_buffer.lock().ok().and_then(|mut fb| fb.take());
                    if let Some(frame) = taken {
                        self.prev_frame = Some(frame.clone());
                        self.prev_decode_w = self.decode_width;
                        self.prev_decode_h = self.decode_height;
                        self.fade_start = Some(std::time::Instant::now());
                        // Put the frame back for GStreamer
                        if let Ok(mut fb) = self.frame_buffer.lock() {
                            *fb = Some(frame);
                        }
                    }
                }
                if let Some(p) = self.pipeline.take() {
                    p.stop();
                }
                if self.decode_width == 0 || self.decode_height == 0 {
                    tracing::warn!("Cannot set source before any output is configured");
                    // Store the path so it starts when outputs configure
                    if let Ok(mut state) = self.daemon_state.lock() {
                        state.source_path = path;
                    }
                    return;
                }
                let fb = Arc::clone(&self.frame_buffer);
                let dirty = Arc::clone(&self.frame_dirty);
                match crate::decoder::DecoderPipeline::new(
                    &path,
                    fb,
                    dirty,
                    self.decode_width,
                    self.decode_height,
                    self.fps_cap,
                ) {
                    Ok(pipeline) => {
                        pipeline.play();
                        self.is_playing = true;
                        if let Ok(mut state) = self.daemon_state.lock() {
                            state.source_path = path;
                            state.playing = true;
                            state.error = None;
                        }
                        self.pipeline = Some(pipeline);
                        self.request_frame(qh);
                    }
                    Err(e) => {
                        tracing::error!("Failed to create decoder pipeline: {e}");
                        if let Ok(mut state) = self.daemon_state.lock() {
                            state.error = Some(format!("{e}"));
                        }
                    }
                }
            }
            Command::Play => {
                if let Some(p) = &self.pipeline {
                    // Resume paused pipeline
                    p.play();
                    self.is_playing = true;
                    if let Ok(mut state) = self.daemon_state.lock() {
                        state.playing = true;
                    }
                    self.request_frame(qh);
                } else {
                    // Pipeline was destroyed (Stop) — recreate from saved source
                    let source = self
                        .daemon_state
                        .lock()
                        .ok()
                        .map(|s| s.source_path.clone())
                        .unwrap_or_default();
                    if !source.is_empty() {
                        self.handle_command(Command::SetSource(source), qh);
                    }
                }
            }
            Command::Pause => {
                if let Some(p) = &self.pipeline {
                    p.pause();
                    self.is_playing = false;
                    if let Ok(mut state) = self.daemon_state.lock() {
                        state.playing = false;
                    }
                }
            }
            Command::Stop => {
                if let Some(p) = self.pipeline.take() {
                    p.stop();
                }
                self.is_playing = false;
                if let Ok(mut fb) = self.frame_buffer.lock() {
                    *fb = None;
                }
                if let Ok(mut state) = self.daemon_state.lock() {
                    state.playing = false;
                }
                self.draw_all_outputs(qh);
            }
            Command::SetFitMode(mode) => {
                self.fit_mode = mode;
                if let Ok(mut state) = self.daemon_state.lock() {
                    state.fit_mode = mode;
                }
                // Redraw with new fit mode
                self.draw_all_outputs(qh);
            }
            Command::SetSpanMode(span) => {
                self.span_mode = span;
                if let Ok(mut state) = self.daemon_state.lock() {
                    state.span_mode = span;
                }
                self.recalculate_decode_resolution();
                self.draw_all_outputs(qh);
            }
            Command::SetFpsCap(fps) => {
                let fps = fps.clamp(5, 60);
                self.fps_cap = fps;
                if let Ok(mut state) = self.daemon_state.lock() {
                    state.fps_cap = fps;
                }
                self.recreate_pipeline_if_active();
            }
        }
    }

    // --- Decode resolution management ---

    fn recalculate_decode_resolution(&mut self) {
        let (new_w, new_h) = if self.span_mode {
            self.compute_bounding_box_resolution()
        } else {
            self.compute_max_output_resolution()
        };

        if new_w == 0 || new_h == 0 {
            return;
        }
        if new_w != self.decode_width || new_h != self.decode_height {
            tracing::info!("Decode resolution: {}x{}", new_w, new_h);
            self.decode_width = new_w;
            self.decode_height = new_h;
            self.recreate_pipeline_if_active();
        }
    }

    fn compute_max_output_resolution(&self) -> (u32, u32) {
        self.outputs
            .iter()
            .filter(|o| o.configured)
            .max_by_key(|o| (o.width as u64) * (o.height as u64))
            .map(|o| (o.width, o.height))
            .unwrap_or((0, 0))
    }

    fn compute_bounding_box_resolution(&mut self) -> (u32, u32) {
        let (_, bb_size) = self.bounding_box();
        if bb_size.0 <= 0 || bb_size.1 <= 0 {
            return (0, 0);
        }
        (bb_size.0 as u32, bb_size.1 as u32)
    }

    /// Returns the cached bounding box, recomputing only when dirty.
    fn bounding_box(&mut self) -> ((i32, i32), (i32, i32)) {
        if self.bb_dirty {
            self.cached_bb = Self::compute_bounding_box_inner(&self.outputs);
            self.bb_dirty = false;
        }
        self.cached_bb
    }

    fn compute_bounding_box_inner(outputs: &[OutputSurface]) -> ((i32, i32), (i32, i32)) {
        let mut configured = outputs.iter().filter(|o| o.configured).peekable();
        if configured.peek().is_none() {
            return ((0, 0), (0, 0));
        }
        let mut min_x = i32::MAX;
        let mut min_y = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_y = i32::MIN;
        for o in configured {
            min_x = min_x.min(o.logical_position.0);
            min_y = min_y.min(o.logical_position.1);
            max_x = max_x.max(o.logical_position.0 + o.logical_size.0);
            max_y = max_y.max(o.logical_position.1 + o.logical_size.1);
        }
        ((min_x, min_y), (max_x - min_x, max_y - min_y))
    }

    fn invalidate_bounding_box(&mut self) {
        self.bb_dirty = true;
    }

    fn recreate_pipeline_if_active(&mut self) {
        // Only recreate if a pipeline already exists (i.e. we're actively playing/paused).
        // If there's no pipeline, try_deferred_start will handle first-time creation.
        let old = match self.pipeline.take() {
            Some(p) => p,
            None => return,
        };
        old.stop();

        let source = self
            .daemon_state
            .lock()
            .ok()
            .map(|s| s.source_path.clone())
            .unwrap_or_default();

        if source.is_empty() || self.decode_width == 0 || self.decode_height == 0 {
            return;
        }

        let fb = Arc::clone(&self.frame_buffer);
        let dirty = Arc::clone(&self.frame_dirty);
        match crate::decoder::DecoderPipeline::new(
            &source,
            fb,
            dirty,
            self.decode_width,
            self.decode_height,
            self.fps_cap,
        ) {
            Ok(pipeline) => {
                if self.is_playing {
                    pipeline.play();
                }
                self.pipeline = Some(pipeline);
            }
            Err(e) => tracing::error!("Failed to recreate pipeline: {e}"),
        }
    }

    /// Try to start a deferred pipeline after first output configures.
    fn try_deferred_start(&mut self, qh: &QueueHandle<Self>) {
        if self.pipeline.is_some() {
            return;
        }
        if self.decode_width == 0 || self.decode_height == 0 {
            return;
        }
        let source = self
            .daemon_state
            .lock()
            .ok()
            .map(|s| s.source_path.clone())
            .unwrap_or_default();
        if source.is_empty() {
            return;
        }
        tracing::info!("Starting deferred pipeline for: {source}");
        let fb = Arc::clone(&self.frame_buffer);
        let dirty = Arc::clone(&self.frame_dirty);
        match crate::decoder::DecoderPipeline::new(
            &source,
            fb,
            dirty,
            self.decode_width,
            self.decode_height,
            self.fps_cap,
        ) {
            Ok(pipeline) => {
                pipeline.play();
                self.is_playing = true;
                if let Ok(mut state) = self.daemon_state.lock() {
                    state.playing = true;
                }
                self.pipeline = Some(pipeline);
                self.request_frame(qh);
            }
            Err(e) => tracing::error!("Failed to start deferred pipeline: {e}"),
        }
    }

    fn update_process_stats(&mut self) {
        let should_update = self
            .last_proc_stat
            .as_ref()
            .map(|(t, _)| t.elapsed().as_secs() >= 2)
            .unwrap_or(true);
        if !should_update {
            return;
        }

        // Read CPU ticks from /proc/self/stat
        // The comm field (field 2) can contain spaces and parens, so find the
        // last ')' to reliably locate subsequent numeric fields.
        let cpu_ticks = std::fs::read_to_string("/proc/self/stat")
            .ok()
            .and_then(|s| {
                let after_comm = &s[s.rfind(')')? + 1..];
                // After ')': field 0=state, ..., 11=utime, 12=stime
                let mut fields = after_comm.split_whitespace();
                let utime: u64 = fields.nth(11)?.parse().ok()?;
                let stime: u64 = fields.next()?.parse().ok()?;
                Some(utime + stime)
            });

        let now = std::time::Instant::now();
        let cpu_percent = if let (Some(ticks), Some((prev_time, prev_ticks))) =
            (cpu_ticks, &self.last_proc_stat)
        {
            let dt = prev_time.elapsed().as_secs_f32();
            if dt > 0.0 {
                let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
                let tick_hz = if clk_tck > 0 { clk_tck as f32 } else { 100.0 };
                let delta_ticks = ticks.saturating_sub(*prev_ticks) as f32;
                (delta_ticks / tick_hz / dt * 100.0).min(100.0 * num_cpus())
            } else {
                0.0
            }
        } else {
            0.0
        };

        if let Some(ticks) = cpu_ticks {
            self.last_proc_stat = Some((now, ticks));
        }

        // Read RSS from /proc/self/statm
        let memory_mb = std::fs::read_to_string("/proc/self/statm")
            .ok()
            .and_then(|s| {
                let rss_pages: u64 = s.split_whitespace().nth(1)?.parse().ok()?;
                let ps = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
                let page_size = if ps > 0 { ps as u64 } else { 4096 };
                Some(rss_pages * page_size)
            })
            .map(|bytes| bytes as f32 / (1024.0 * 1024.0))
            .unwrap_or(0.0);

        if let Ok(mut state) = self.daemon_state.lock() {
            state.cpu_percent = cpu_percent;
            state.memory_mb = memory_mb;
        }
    }
}

fn num_cpus() -> f32 {
    let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    if n > 0 { n as f32 } else { 1.0 }
}

/// Max dimension for cached frames (8K).
const MAX_CACHE_DIM: u32 = 7680;

/// Save current frame to disk cache for static fallback on restart.
/// Uses atomic write (write to temp, rename) to avoid partial writes and symlink attacks.
fn save_frame_cache(frame: &[u8], width: u32, height: u32) {
    let cache_dir = match dirs_cache_path() {
        Some(d) => d,
        None => return,
    };
    if std::fs::create_dir_all(&cache_dir).is_err() {
        return;
    }
    // Verify the cache directory is not a symlink
    if cache_dir.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(true) {
        tracing::warn!("Cache directory is a symlink, refusing to write");
        return;
    }
    let path = cache_dir.join("last-frame.bin");
    let tmp_path = cache_dir.join("last-frame.tmp");
    let mut data = Vec::with_capacity(8 + frame.len());
    data.extend_from_slice(&width.to_le_bytes());
    data.extend_from_slice(&height.to_le_bytes());
    data.extend_from_slice(frame);
    // Write to temp file, then atomically rename
    if std::fs::write(&tmp_path, data).is_ok() {
        let _ = std::fs::rename(&tmp_path, &path);
    }
}

/// Load cached frame from disk.
pub fn load_frame_cache() -> Option<(Vec<u8>, u32, u32)> {
    let cache_dir = dirs_cache_path()?;
    let path = cache_dir.join("last-frame.bin");
    // Don't follow symlinks
    let meta = path.symlink_metadata().ok()?;
    if meta.file_type().is_symlink() {
        tracing::warn!("Cache file is a symlink, refusing to load");
        return None;
    }
    let data = std::fs::read(path).ok()?;
    if data.len() < 8 {
        return None;
    }
    let width = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let height = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    if width == 0 || height == 0 || width > MAX_CACHE_DIM || height > MAX_CACHE_DIM {
        return None;
    }
    let expected = (width as usize).checked_mul(height as usize)?.checked_mul(4)?;
    if data.len() < 8 + expected {
        return None;
    }
    Some((data[8..8 + expected].to_vec(), width, height))
}

fn dirs_cache_path() -> Option<std::path::PathBuf> {
    let cache_home = std::env::var("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            std::path::PathBuf::from(home).join(".cache")
        });
    Some(cache_home.join("cosmic-flux"))
}

fn resize_pool_for(os: &mut OutputSurface) {
    let frame_size = (os.width as usize) * (os.height as usize) * 4;
    let needed = frame_size * 3; // triple-buffering headroom
    if let Err(e) = os.pool.resize(needed) {
        tracing::error!("Failed to resize SHM pool: {e}");
    }
}

// --- sctk Handler implementations ---

impl CompositorHandler for WallpaperRenderer {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        // Sync error and source_fps from pipeline to daemon state (cheap reads)
        if let Some(p) = &self.pipeline {
            let errored = p.has_error();
            let sfps = p.source_fps();
            if let Ok(mut state) = self.daemon_state.lock() {
                if sfps > 0.0 {
                    state.source_fps = sfps;
                }
                if errored {
                    state.error = p.error_message();
                    state.playing = false;
                }
            }
            if errored {
                self.is_playing = false;
            }
        }

        let new_frame = self.frame_dirty.swap(false, Ordering::Relaxed);

        if new_frame {
            // New GStreamer frame — draw ALL outputs to keep them in sync.
            // swap(false) ensures only the first callback per vsync does this.
            self.draw_all_outputs(qh);
        } else if self.fade_start.is_some() {
            // Cross-fade in progress — only draw the requesting output to avoid N² work
            self.draw_output_by_surface(surface, qh);
        } else if self.is_playing {
            // No new frame yet — keep the callback chain alive for this output only
            if let Some(os) = self
                .outputs
                .iter()
                .find(|o| o.layer.wl_surface() == surface)
            {
                os.layer
                    .wl_surface()
                    .frame(qh, os.layer.wl_surface().clone());
                os.layer.commit();
            }
        }
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for WallpaperRenderer {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        let info = self.output_state.info(&output);
        let name = info
            .as_ref()
            .and_then(|i| i.name.as_deref())
            .unwrap_or("unknown");
        tracing::info!("New output: {name}");

        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Background,
            Some("cosmic-flux"),
            Some(&output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(0, 0);
        layer.commit();

        let pool = match SlotPool::new(256, &self.shm) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("Failed to create SHM pool for output: {e}");
                return;
            }
        };

        let logical_pos = info
            .as_ref()
            .and_then(|i| i.logical_position)
            .unwrap_or((0, 0));
        let logical_sz = info
            .as_ref()
            .and_then(|i| i.logical_size)
            .unwrap_or((0, 0));

        self.outputs.push(OutputSurface {
            output,
            layer,
            pool,
            width: 0,
            height: 0,
            logical_position: logical_pos,
            logical_size: logical_sz,
            first_configure: true,
            configured: false,
        });
        self.invalidate_bounding_box();
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if let Some(info) = self.output_state.info(&output) {
            if let Some(os) = self.outputs.iter_mut().find(|o| o.output == output) {
                if let Some(pos) = info.logical_position {
                    os.logical_position = pos;
                }
                if let Some(sz) = info.logical_size {
                    os.logical_size = sz;
                }
            }
        }
        self.invalidate_bounding_box();
        if self.span_mode {
            self.recalculate_decode_resolution();
        }
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if let Some(idx) = self.outputs.iter().position(|o| o.output == output) {
            self.outputs.remove(idx);
            tracing::info!("Output removed, {} remaining", self.outputs.len());
        }
        self.invalidate_bounding_box();
        self.recalculate_decode_resolution();
    }
}

impl LayerShellHandler for WallpaperRenderer {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        const MAX_DIM: u32 = 7680;
        let new_w = configure.new_size.0.max(1).min(MAX_DIM);
        let new_h = configure.new_size.1.max(1).min(MAX_DIM);

        let os = match self
            .outputs
            .iter_mut()
            .find(|o| o.layer.wl_surface() == layer.wl_surface())
        {
            Some(os) => os,
            None => {
                tracing::warn!("Configure for unknown layer surface");
                return;
            }
        };

        let resized = new_w != os.width || new_h != os.height;
        os.width = new_w;
        os.height = new_h;

        if resized || os.first_configure {
            tracing::info!("Output configured: {}x{}", new_w, new_h);
            resize_pool_for(os);
        }

        let was_first = os.first_configure;
        if os.first_configure {
            os.first_configure = false;
            os.configured = true;
        }

        // Recalculate decode resolution (may change if this is the largest output)
        self.invalidate_bounding_box();
        self.recalculate_decode_resolution();

        if was_first {
            // Try starting a deferred pipeline
            self.try_deferred_start(qh);
            // Draw this output (black frame or first decoded frame)
            let surface = layer.wl_surface().clone();
            self.draw_output_by_surface(&surface, qh);
        }
    }
}

impl SeatHandler for WallpaperRenderer {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        _: Capability,
    ) {
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        _: Capability,
    ) {
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl ShmHandler for WallpaperRenderer {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_compositor!(WallpaperRenderer);
delegate_output!(WallpaperRenderer);
delegate_shm!(WallpaperRenderer);
delegate_seat!(WallpaperRenderer);
delegate_layer!(WallpaperRenderer);
delegate_registry!(WallpaperRenderer);

impl ProvidesRegistryState for WallpaperRenderer {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}
