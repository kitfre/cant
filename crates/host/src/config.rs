//! Optional user configuration (`~/.config/cant/config.json` or `$CANT_CONFIG`).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CantConfig {
    /// Capability namespaces disabled by default (e.g., `["web"]`).
    #[serde(default)]
    pub disabled_namespaces: Vec<String>,
    /// Default worker driver for `delegate/*` and `agents/*` (default: `"agy"`).
    #[serde(default)]
    pub worker_driver: Option<String>,
    /// Optional default model passed to the worker driver.
    #[serde(default)]
    pub worker_model: Option<String>,
    /// Enable strict mode in PreToolUse hooks (blocks direct file/shell tools in favor of cant repl).
    #[serde(default)]
    pub strict: Option<bool>,
}

impl CantConfig {
    pub fn load() -> Self {
        let path = std::env::var("CANT_CONFIG")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var("XDG_CONFIG_HOME")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
                    .map(|p| PathBuf::from(p).join("cant").join("config.json"))
            })
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| PathBuf::from(h).join(".config").join("cant").join("config.json"))
            });
        let Some(p) = path else {
            return Self::default();
        };
        std::fs::read_to_string(p)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }
}
