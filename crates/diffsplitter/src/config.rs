//! Environment-driven configuration.

use std::path::PathBuf;

use anyhow::{Context, Result};

#[derive(Clone, Debug)]
pub struct Config {
    pub backend_primary_url: String,
    pub backend_shadow_url: String,
    pub primary_admin_token: Option<String>,
    pub shadow_admin_token: Option<String>,
    pub shadow_sample_rate: f64,
    pub sqlite_path: PathBuf,
    pub port: u16,
    pub max_write_attempts: u32,
    pub version_poll_interval_secs: u64,
    pub diff_body_max_bytes: usize,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let backend_primary_url = std::env::var("BACKEND_PRIMARY_URL")
            .unwrap_or_else(|_| "https://twitter-formal-rust.fly.dev".to_string());
        let backend_shadow_url = std::env::var("BACKEND_SHADOW_URL")
            .unwrap_or_else(|_| "https://twitter-formal-go.fly.dev".to_string());

        let primary_admin_token = std::env::var("RUST_PRIMARY_ADMIN_TOKEN").ok();
        let shadow_admin_token = std::env::var("GO_SHADOW_ADMIN_TOKEN").ok();

        let shadow_sample_rate = std::env::var("SHADOW_SAMPLE_RATE")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.1)
            .clamp(0.0, 1.0);

        // Default path: /data is the Fly volume mount point.
        let sqlite_path = std::env::var("SQLITE_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/data/diffsplitter.db"));

        let port: u16 = std::env::var("PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8080);

        let max_write_attempts: u32 = std::env::var("MAX_WRITE_ATTEMPTS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5);

        let version_poll_interval_secs: u64 = std::env::var("VERSION_POLL_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5);

        // Truncate diff bodies stored in SQLite to keep the table small.
        let diff_body_max_bytes: usize = std::env::var("DIFF_BODY_MAX_BYTES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(64 * 1024);

        // Sanity: ensure URLs do not end with a trailing slash so we can do
        // simple `format!("{base}{path}")` concatenation.
        let backend_primary_url = backend_primary_url.trim_end_matches('/').to_string();
        let backend_shadow_url = backend_shadow_url.trim_end_matches('/').to_string();

        // Make sure parent directory of the sqlite path exists if relative.
        if let Some(parent) = sqlite_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating sqlite dir {}", parent.display()))?;
            }
        }

        Ok(Self {
            backend_primary_url,
            backend_shadow_url,
            primary_admin_token,
            shadow_admin_token,
            shadow_sample_rate,
            sqlite_path,
            port,
            max_write_attempts,
            version_poll_interval_secs,
            diff_body_max_bytes,
        })
    }
}
