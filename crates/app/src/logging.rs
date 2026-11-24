//! Persistent, correlation-tagged logging for post-mortem diagnosis.
//!
//! The app is a tray resident with no console on macOS/Windows, so stderr logs
//! vanish for real users. This module registers the single global `log::Log`
//! implementation and fans every record out to **both** stderr and a rotating
//! file under [`AppDirs::log_file`]. Because it hooks the `log` facade, logs
//! from every crate — including the vendored `xcap` — land in the file without
//! those crates needing to depend on anything new.
//!
//! # Correlation IDs
//!
//! A screenshot or recording spawns background work, so a plain thread-local
//! would not follow the work across threads. We keep the active operation id in
//! an atomic slot: [`begin_capture`]/[`begin_recording`] publish a fresh id and
//! [`clear_operation`] retires it. The logger stamps every line with the active
//! id, so one capture's freeze → HDR decision → crop → encode → save lines can
//! be grepped with a single tag, e.g. `grep '\[cap#12\]' snapit.log`.

use std::{
    fs::{File, OpenOptions},
    io::Write as _,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use chrono::Local;

/// Max size before the current log is rotated to `snapit.log.old` (8 MiB).
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

/// 0 means "no operation active". Format is `cap` / `rec` plus a monotonic id.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static ACTIVE_KIND: AtomicU64 = AtomicU64::new(0); // 0 none, 1 capture, 2 recording
static ACTIVE_ID: AtomicU64 = AtomicU64::new(0);

/// Publish a new capture id and return it for callers that want to log it too.
pub fn begin_capture() -> u64 {
    begin(1)
}

/// Publish a new recording id and return it.
pub fn begin_recording() -> u64 {
    begin(2)
}

fn begin(kind: u64) -> u64 {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    ACTIVE_ID.store(id, Ordering::Relaxed);
    ACTIVE_KIND.store(kind, Ordering::Relaxed);
    id
}

fn tag() -> Option<String> {
    let kind = ACTIVE_KIND.load(Ordering::Relaxed);
    let id = ACTIVE_ID.load(Ordering::Relaxed);
    match (kind, id) {
        (1, n) if n != 0 => Some(format!("cap#{n}")),
        (2, n) if n != 0 => Some(format!("rec#{n}")),
        _ => None,
    }
}

struct FileLogger {
    file: std::sync::Mutex<File>,
}

impl log::Log for FileLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Debug
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let ts = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%:z");
        let stamp = tag().unwrap_or_else(|| "-".to_string());
        let mut file = self.file.lock().unwrap();
        let _ = writeln!(
            file,
            "[{ts}] [{stamp}] {:5} {}: {}",
            record.level(),
            record.target(),
            record.args()
        );
        let _ = file.flush();
    }

    fn flush(&self) {
        if let Ok(mut file) = self.file.lock() {
            let _ = file.flush();
        }
    }
}

impl FileLogger {
    fn open(path: &std::path::Path) -> std::io::Result<Self> {
        if let Ok(meta) = std::fs::metadata(path)
            && meta.len() >= MAX_LOG_BYTES
        {
            let old = path.with_extension("log.old");
            let _ = std::fs::remove_file(&old);
            let _ = std::fs::rename(path, &old);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file: std::sync::Mutex::new(file),
        })
    }
}

/// A second logger that mirrors records to stderr with the same tag format, so
/// terminal-driven debugging (`cargo run`) and the file stay consistent.
struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let stamp = tag().unwrap_or_else(|| "-".to_string());
        eprintln!(
            "[{stamp}] {:5} {}: {}",
            record.level(),
            record.target(),
            record.args()
        );
    }

    fn flush(&self) {}
}

/// Logger that fans out to both sinks.
struct Fanout {
    file: FileLogger,
    stderr: StderrLogger,
}

impl log::Log for Fanout {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        self.file.enabled(metadata) || self.stderr.enabled(metadata)
    }

    fn log(&self, record: &log::Record<'_>) {
        self.file.log(record);
        self.stderr.log(record);
    }

    fn flush(&self) {
        self.file.flush();
        self.stderr.flush();
    }
}

/// Initialise the global logger. Call once, before any `log::*` use.
///
/// The file sink records `Debug` and above; stderr keeps `Info` and above to
/// match the previous env_logger behaviour. `RUST_LOG` is honoured for the file
/// level too when set (falls back to `debug`).
pub fn init(log_path: &Path) {
    let file = FileLogger::open(log_path).expect("cannot open SnapIt log file");
    let logger = Fanout {
        file,
        stderr: StderrLogger,
    };
    let level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|s| match s.trim().to_ascii_lowercase().as_str() {
            "trace" => Some(log::LevelFilter::Trace),
            "debug" => Some(log::LevelFilter::Debug),
            "info" => Some(log::LevelFilter::Info),
            "warn" => Some(log::LevelFilter::Warn),
            "error" => Some(log::LevelFilter::Error),
            "off" => Some(log::LevelFilter::Off),
            _ => None,
        })
        .unwrap_or(log::LevelFilter::Debug);
    log::set_boxed_logger(Box::new(logger)).expect("logger already initialised");
    log::set_max_level(level);
    log::info!("logging to {}", log_path.display());
}
