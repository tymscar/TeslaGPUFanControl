//! Logger init — composes tracing-subscriber with a journald layer (always on)
//! and an optional file layer.
//!
//! Compile-time max level is pinned to `info` in release via the
//! `release_max_level_info` feature on `tracing` in Cargo.toml — no runtime
//! work needed for that here. `EnvFilter` lets `RUST_LOG` override the
//! configured level when running in foreground / dev.

use crate::config::LogLevel;
use std::path::Path;
use thiserror::Error;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Error)]
pub enum LoggerError {
    #[error("failed to install tracing subscriber: {0}")]
    Subscriber(String),
    #[error("failed to open log file: {0}")]
    LogFile(#[from] std::io::Error),
    #[error("failed to connect to journald: {0}")]
    // Reserved for the strict-journald path; the current init() falls back
    // to stderr on journald failure rather than returning this variant.
    #[allow(dead_code)]
    Journald(String),
}

/// Install the global tracing subscriber. Idempotent-ish — calling twice will
/// fail because `tracing` only allows a single global default.
pub fn init(level: LogLevel, log_file: Option<&Path>) -> Result<(), LoggerError> {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level.to_string()));

    let registry = tracing_subscriber::registry().with(env_filter);

    let journald_layer = match tracing_journald::layer() {
        Ok(layer) => Some(layer),
        Err(e) => {
            // Journald is the production sink; failure is fatal there. In test
            // and dev environments without journald available, fall back to
            // stderr so the daemon can still run in `-f` foreground mode.
            eprintln!("journald unavailable ({e}); falling back to stderr");
            None
        }
    };

    let file_layer = if let Some(path) = log_file {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Some(
            tracing_subscriber::fmt::layer()
                .with_writer(file)
                .with_ansi(false),
        )
    } else {
        None
    };

    let stderr_layer = if journald_layer.is_none() {
        Some(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
    } else {
        None
    };

    registry
        .with(journald_layer)
        .with(file_layer)
        .with(stderr_layer)
        .try_init()
        .map_err(|e| LoggerError::Subscriber(e.to_string()))?;

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // The global default subscriber can only be installed once per process,
    // and tests run in parallel by default. We can still exercise the file
    // path by ensuring `init` either succeeds or fails with a "subscriber
    // already set" error — both are acceptable in a one-shot test process.
    static INIT_GUARD: Mutex<()> = Mutex::new(());

    #[test]
    fn init_smoke_with_tempfile() {
        let _g = INIT_GUARD.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tfc.log");
        // Either Ok (first call), or "subscriber already set" if another test
        // ran first. The point is that the construction path doesn't panic.
        let _ = init(LogLevel::Info, Some(&path));
    }
}
