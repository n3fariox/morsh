use std::fs;
use std::path::PathBuf;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Returns `~/.morsh/logs/`, creating it if it doesn't exist.
fn default_log_dir() -> PathBuf {
    let base = dirs::home_dir().unwrap_or_else(std::env::temp_dir);
    base.join(".morsh").join("logs")
}

/// Initialize file-based logging for a morsh binary.
///
/// - Logs append to `~/.morsh/logs/<binary_name>.log` (no rotation).
/// - Filter level defaults to `info`, override with `RUST_LOG` env var.
/// - Returns a guard that **must** be held for the program's lifetime; dropping it
///   stops the background writer and silently drops subsequent log records.
pub fn init(binary_name: &str) -> WorkerGuard {
    init_at(binary_name, &default_log_dir().join(format!("{binary_name}.log")))
}

/// Initialize file-based logging writing to the given path (no rotation).
///
/// Useful for `--log-file` overrides. The caller owns the path.
pub fn init_at(_binary_name: &str, path: &PathBuf) -> WorkerGuard {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }

    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap_or_else(|e| panic!("failed to open log file {}: {e}", path.display()));

    let (non_blocking, guard) = tracing_appender::non_blocking(file);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false)
                .with_target(true)
                .with_thread_ids(true),
        )
        .with(filter)
        .init();

    guard
}
