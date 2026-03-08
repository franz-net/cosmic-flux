// SPDX-License-Identifier: GPL-3.0-only

//! D-Bus client proxy for communicating with the daemon.
//! Uses a cached connection to avoid repeated handshakes.

use std::sync::OnceLock;
use tokio::sync::Mutex;
use zbus::proxy;

#[proxy(
    interface = "com.system76.CosmicFlux1",
    default_service = "com.system76.CosmicFlux1",
    default_path = "/com/system76/CosmicFlux"
)]
pub trait WallpaperDaemon {
    async fn set_source(&self, path: &str) -> zbus::Result<()>;
    async fn play(&self) -> zbus::Result<()>;
    async fn pause(&self) -> zbus::Result<()>;
    async fn stop(&self) -> zbus::Result<()>;
    async fn set_fit_mode(&self, mode: &str) -> zbus::Result<()>;
    async fn set_span_mode(&self, enabled: bool) -> zbus::Result<()>;
    async fn set_fps_cap(&self, fps: u32) -> zbus::Result<()>;
    /// Returns (playing, error, cpu, memory, fps, source_fps) in a single D-Bus call.
    async fn get_state(&self) -> zbus::Result<(bool, String, f64, f64, f64, f64)>;
    // Properties are defined here for proxy generation but polling code uses
    // get_state() for efficient batched reads.
    #[zbus(property)]
    fn playing(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn source(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn fit_mode(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn span_mode(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn error(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn cpu_percent(&self) -> zbus::Result<f64>;
    #[zbus(property)]
    fn memory_mb(&self) -> zbus::Result<f64>;
    #[zbus(property)]
    fn fps(&self) -> zbus::Result<f64>;
    #[zbus(property)]
    fn fps_cap(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn source_fps(&self) -> zbus::Result<f64>;
}

/// Cached proxy — created once, reused for all calls.
static PROXY: OnceLock<Mutex<Option<WallpaperDaemonProxy<'static>>>> = OnceLock::new();

pub async fn connect() -> anyhow::Result<WallpaperDaemonProxy<'static>> {
    let mtx = PROXY.get_or_init(|| Mutex::new(None));
    let mut guard = mtx.lock().await;

    if let Some(proxy) = guard.as_ref() {
        return Ok(proxy.clone());
    }

    let conn = zbus::Connection::session().await?;
    let proxy = WallpaperDaemonProxy::new(&conn).await?;
    *guard = Some(proxy.clone());
    Ok(proxy)
}

/// Clear the cached proxy so the next `connect()` call creates a fresh connection.
pub async fn clear_cache() {
    let mtx = PROXY.get_or_init(|| Mutex::new(None));
    let mut guard = mtx.lock().await;
    *guard = None;
}
