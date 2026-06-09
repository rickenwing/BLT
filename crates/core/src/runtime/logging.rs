//! Structured logging to rotating files (F17 / HARD CONSTRAINT #16).
//!
//! Each component logs to its own `logs/` folder under the data root, plus
//! console output in dev. Rotation is daily with a bounded file count so logs
//! don't grow unbounded across many parties. **Never log secrets** (admin
//! password, session tokens) — that is enforced at call sites, not here.

use std::path::Path;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Initialise logging for a component. Returns a [`WorkerGuard`] that **must be
/// kept alive** for the lifetime of the process (dropping it flushes + stops the
/// background writer).
///
/// - `logs_dir`: the component's `logs/` directory (created by the caller).
/// - `file_prefix`: log filename prefix, e.g. `blt-server` / `blt-client`.
/// - `default_directive`: env-filter fallback (e.g. `"info"`), overridable via
///   the `BLT_LOG` environment variable.
pub fn init(logs_dir: &Path, file_prefix: &str, default_directive: &str) -> WorkerGuard {
    let file_appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(file_prefix)
        .filename_suffix("log")
        .max_log_files(14)
        .build(logs_dir)
        .unwrap_or_else(|_| RollingFileAppender::new(Rotation::DAILY, logs_dir, file_prefix));

    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_env("BLT_LOG")
        .unwrap_or_else(|_| EnvFilter::new(default_directive));

    let file_layer = fmt::layer()
        .with_ansi(false)
        .with_target(true)
        .with_writer(non_blocking);

    let console_layer = fmt::layer().with_target(false).with_writer(std::io::stderr);

    // Ignore the error if a global subscriber was already set (e.g. in tests).
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(console_layer)
        .try_init();

    guard
}
