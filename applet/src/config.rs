// SPDX-License-Identifier: GPL-3.0-only

use cosmic::cosmic_config;
use cosmic::cosmic_config::{cosmic_config_derive::CosmicConfigEntry, CosmicConfigEntry};

#[derive(Debug, Clone, PartialEq, CosmicConfigEntry)]
#[version = 3]
pub struct Config {
    pub source_path: String,
    pub fit_mode: String,
    pub autostart: bool,
    pub span_mode: bool,
    pub fps_cap: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            source_path: String::new(),
            fit_mode: String::new(),
            autostart: false,
            span_mode: false,
            fps_cap: 15,
        }
    }
}
