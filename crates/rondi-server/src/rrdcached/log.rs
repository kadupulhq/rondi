//! Daemon messages: plain text on standard error up to the `-V` level, and
//! structured events in the `-o` file.

use super::config::{LogFile, LogLevel};
use std::io::Write;
use std::sync::Arc;

pub(crate) struct Log {
    level: LogLevel,
}

impl Log {
    /// Starts logging. With a `-o` file, tracing events (the daemon's own
    /// and the request handlers') are written to it as JSON lines.
    // Compatibility: in the foreground the reference prints its messages up
    // to the `-V` level on standard error, LOG_ERR by default (RCD-50).
    // Provenance: black-box compatibility test.
    // Rondi difference (RD-007): `-o` holds Rondi's structured events, not
    // the reference's text.
    pub(crate) fn start(level: LogLevel, file: Option<LogFile>) -> Self {
        if let Some(file) = file {
            let verbosity = if level >= LogLevel::Debug {
                tracing::Level::DEBUG
            } else {
                tracing::Level::INFO
            };
            let _ = tracing_subscriber::fmt()
                .json()
                .with_max_level(verbosity)
                .with_writer(Arc::new(file.file))
                .try_init();
        }
        Self { level }
    }

    /// Records a lifecycle event: `text` on standard error when `level` is
    /// within `-V`, and `event` in the `-o` file.
    pub(crate) fn event(&self, level: LogLevel, event: &'static str, text: &str) {
        if level <= self.level {
            let _ = writeln!(std::io::stderr(), "{text}");
        }
        match level {
            LogLevel::Debug => tracing::debug!(detail = text, "{event}"),
            LogLevel::Info | LogLevel::Notice => tracing::info!(detail = text, "{event}"),
            LogLevel::Warning => tracing::warn!(detail = text, "{event}"),
            _ => tracing::error!(detail = text, "{event}"),
        }
    }
}
