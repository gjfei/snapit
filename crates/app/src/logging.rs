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
//! would not follow the work across threads. We keep the active operation kind
//! and id in one atomic encoded value: [`begin_capture`]/[`begin_recording`]
//! publish a fresh id and [`retire_operation`] retires only that id. The logger
//! takes one consistent snapshot, so one capture's freeze → HDR decision → crop
//! → encode → save lines can be grepped with a single tag, e.g.
//! `grep '\[cap#12\]' snapit.log`.

use std::{
    fs::{File, OpenOptions},
    io::{BufWriter, Write as _},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use chrono::Local;

/// Max size before the current log is rotated to `snapit.log.old` (8 MiB).
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

const KIND_BITS: u32 = 2;
const KIND_MASK: u64 = (1 << KIND_BITS) - 1;
const MAX_OPERATION_ID: u64 = u64::MAX >> KIND_BITS;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
/// 0 means idle; otherwise `(id << 2) | kind`. Kind and id are always read in
/// one load, rather than from two independently racing atomics.
static ACTIVE_OPERATION: AtomicU64 = AtomicU64::new(0);

fn encode_operation(id: u64, kind: u64) -> Option<u64> {
    (id != 0 && id <= MAX_OPERATION_ID && (1..=3).contains(&kind))
        .then_some((id << KIND_BITS) | kind)
}

fn decode_operation(encoded: u64) -> Option<(u64, u64)> {
    let kind = encoded & KIND_MASK;
    let id = encoded >> KIND_BITS;
    encode_operation(id, kind).map(|_| (id, kind))
}

/// The CAS result for retiring `id`: `None` means a newer operation owns the
/// slot, while `Some(0)` means this operation was retired.
#[allow(dead_code)] // Used by the retirement API and its lifecycle tests.
fn retire_matching(encoded: u64, id: u64) -> Option<u64> {
    decode_operation(encoded)
        .is_some_and(|(current, _)| current == id)
        .then_some(0)
}

/// Publish a new capture id and return it for callers that want to log it too.
pub fn begin_capture() -> u64 {
    begin(1)
}

/// Publish a new recording id and return it.
pub fn begin_recording() -> u64 {
    begin(2)
}

/// Publish a new scrolling capture id and return it.
pub fn begin_scroll() -> u64 {
    begin(3)
}

fn begin(kind: u64) -> u64 {
    let id = NEXT_ID
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            (current != 0 && current < MAX_OPERATION_ID).then_some(current + 1)
        })
        .expect("operation id space exhausted");
    let encoded = encode_operation(id, kind).expect("valid operation kind");
    ACTIVE_OPERATION.store(encoded, Ordering::Relaxed);
    id
}

/// Retire the active operation only if `id` still owns the correlation slot.
/// Returns `false` when a later capture/record/scroll has replaced it.
#[allow(dead_code)] // Controller/scroll lifecycle integration calls this next.
pub fn retire_operation(id: u64) -> bool {
    let mut current = ACTIVE_OPERATION.load(Ordering::Acquire);
    loop {
        let Some(next) = retire_matching(current, id) else {
            return false;
        };
        match ACTIVE_OPERATION.compare_exchange(current, next, Ordering::AcqRel, Ordering::Relaxed)
        {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

fn tag() -> Option<String> {
    let (id, kind) = decode_operation(ACTIVE_OPERATION.load(Ordering::Acquire))?;
    Some(match kind {
        1 => format!("cap#{id}"),
        2 => format!("rec#{id}"),
        3 => format!("scr#{id}"),
        _ => unreachable!("decode_operation validates the kind"),
    })
}

struct FileSink {
    writer: BufWriter<File>,
    len: u64,
}

struct WriteCount<'a> {
    writer: &'a mut BufWriter<File>,
    written: usize,
}

impl std::io::Write for WriteCount<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = self.writer.write(bytes)?;
        self.written += written;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

impl FileSink {
    fn open(path: &Path) -> std::io::Result<Self> {
        let file = open_append(path)?;
        let len = file.metadata()?.len();
        Ok(Self {
            writer: BufWriter::with_capacity(16 * 1024, file),
            len,
        })
    }

    fn rotate(&mut self, path: &Path) -> std::io::Result<()> {
        self.writer.flush()?;
        rotate(path);
        *self = Self::open(path)?;
        Ok(())
    }
}

struct FileLogger {
    file: std::sync::Mutex<Option<FileSink>>,
    path: std::path::PathBuf,
}

impl log::Log for FileLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::LevelFilter::Debug
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let ts = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%:z");
        let stamp = tag().unwrap_or_else(|| "-".to_string());
        let Ok(mut slot) = self.file.lock() else {
            return;
        };
        if slot.is_none() {
            *slot = FileSink::open(&self.path).ok();
        }
        let Some(current) = slot.as_mut() else {
            return;
        };
        if current.len >= MAX_LOG_BYTES && current.rotate(&self.path).is_err() {
            *slot = None;
        }
        let Some(current) = slot.as_mut() else {
            return;
        };
        let mut counted = WriteCount {
            writer: &mut current.writer,
            written: 0,
        };
        let _ = writeln!(
            counted,
            "[{ts}] [{stamp}] {:5} {}: {}",
            record.level(),
            record.target(),
            record.args()
        );
        current.len = current.len.saturating_add(counted.written as u64);
        // Capture/format decisions are the post-mortem record, so persist Info
        // immediately. High-frequency timing diagnostics stay buffered.
        if record.level() <= log::LevelFilter::Warn
            || (record.level() == log::Level::Info && record.target() != "timing")
            || current.len >= MAX_LOG_BYTES
        {
            let _ = current.writer.flush();
        }
        if record.level() <= log::LevelFilter::Warn && current.len >= MAX_LOG_BYTES {
            let _ = current.rotate(&self.path);
        }
    }

    fn flush(&self) {
        if let Ok(mut file) = self.file.lock()
            && let Some(file) = file.as_mut()
        {
            let _ = file.writer.flush();
        }
    }
}

impl FileLogger {
    fn open(path: &std::path::Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        rotate_if_needed(path);
        let file = FileSink::open(path)?;
        Ok(Self {
            file: std::sync::Mutex::new(Some(file)),
            path: path.to_path_buf(),
        })
    }
}

fn open_append(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn rotate_if_needed(path: &Path) {
    if std::fs::metadata(path).is_ok_and(|meta| meta.len() >= MAX_LOG_BYTES) {
        rotate(path);
    }
}

fn rotate(path: &Path) {
    let old = path.with_extension("log.old");
    let _ = std::fs::remove_file(&old);
    if let Err(err) = std::fs::rename(path, &old) {
        eprintln!("cannot rotate log {}: {err}", path.display());
    }
}

/// A second logger that mirrors records to stderr with the same tag format, so
/// terminal-driven debugging (`cargo run`) and the file stay consistent.
struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::LevelFilter::Info
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
    file: Option<FileLogger>,
    stderr: StderrLogger,
}

impl log::Log for Fanout {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        self.file.as_ref().is_none_or(|file| file.enabled(metadata))
            || self.stderr.enabled(metadata)
    }

    fn log(&self, record: &log::Record<'_>) {
        if let Some(file) = &self.file {
            file.log(record);
        }
        self.stderr.log(record);
    }

    fn flush(&self) {
        if let Some(file) = &self.file {
            file.flush();
        }
        self.stderr.flush();
    }
}

/// Initialise the global logger. Call once, before any `log::*` use.
///
/// The file sink records `Debug` and above; stderr keeps `Info` and above to
/// match the previous env_logger behaviour. `RUST_LOG` is honoured for the file
/// level too when set (falls back to `debug`).
pub fn init(log_path: &Path) {
    let file = match FileLogger::open(log_path) {
        Ok(file) => Some(file),
        Err(err) => {
            eprintln!(
                "cannot open log file {}: {err}; using stderr only",
                log_path.display()
            );
            None
        }
    };
    let file_enabled = file.is_some();
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
    if file_enabled {
        log::info!("logging to {}", log_path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Log as _;
    use std::path::PathBuf;

    fn temp_log(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("snapit-log-{tag}-{nanos}.log"))
    }

    #[test]
    fn severity_filters_use_log_level_filter_ordering() {
        let metadata = |level| log::Metadata::builder().level(level).target("test").build();
        for level in [
            log::Level::Error,
            log::Level::Warn,
            log::Level::Info,
            log::Level::Debug,
        ] {
            assert!(FileLogger::enabled(
                &FileLogger {
                    file: std::sync::Mutex::new(None),
                    path: PathBuf::new(),
                },
                &metadata(level)
            ));
        }
        assert!(!FileLogger::enabled(
            &FileLogger {
                file: std::sync::Mutex::new(None),
                path: PathBuf::new(),
            },
            &metadata(log::Level::Trace)
        ));
        assert!(StderrLogger.enabled(&metadata(log::Level::Info)));
        assert!(!StderrLogger.enabled(&metadata(log::Level::Debug)));
    }

    #[test]
    fn file_sink_buffers_until_flush() {
        let path = temp_log("buffer");
        let mut sink = FileSink::open(&path).unwrap();
        sink.writer.write_all(b"diagnostic\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        sink.writer.flush().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"diagnostic\n");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn capture_decisions_are_durable_without_flushing_timing_spam() {
        let path = temp_log("decision");
        let logger = FileLogger::open(&path).unwrap();
        logger.log(
            &log::Record::builder()
                .args(format_args!("HDR capture selected"))
                .level(log::Level::Info)
                .target("capture")
                .build(),
        );
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("HDR capture selected")
        );
        let timing = log::Record::builder()
            .args(format_args!("encode 1.2 ms"))
            .level(log::Level::Info)
            .target("timing")
            .build();
        assert_eq!(timing.target(), "timing");
        assert_eq!(timing.level(), log::Level::Info);
        assert!(timing.level() <= log::LevelFilter::Info);
        logger.log(&timing);
        let slot = logger.file.lock().unwrap();
        let current = slot.as_ref().unwrap();
        assert!(current.len < MAX_LOG_BYTES, "unexpected rotation");
        let buffered = current.writer.buffer().len();
        drop(slot);
        assert!(buffered > 0, "timing record should remain buffered");
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains("encode 1.2 ms")
        );
        logger.flush();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("encode 1.2 ms")
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn runtime_rotation_moves_the_current_log_aside() {
        let path = temp_log("rotate");
        let mut sink = FileSink::open(&path).unwrap();
        sink.writer.write_all(b"before rotation\n").unwrap();
        sink.len = MAX_LOG_BYTES;
        sink.rotate(&path).unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"");
        let old = path.with_extension("log.old");
        assert_eq!(std::fs::read(&old).unwrap(), b"before rotation\n");
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(old);
    }

    #[test]
    fn operation_encoding_round_trips_without_mixed_kind_and_id() {
        for (id, kind, expected) in [
            (1, 1, "cap#1"),
            (2, 2, "rec#2"),
            (3, 3, "scr#3"),
            (MAX_OPERATION_ID, 3, "scr#4611686018427387903"),
        ] {
            let encoded = encode_operation(id, kind).unwrap();
            assert_eq!(decode_operation(encoded), Some((id, kind)));
            let (decoded_id, decoded_kind) = decode_operation(encoded).unwrap();
            assert_eq!(
                Some(match decoded_kind {
                    1 => format!("cap#{decoded_id}"),
                    2 => format!("rec#{decoded_id}"),
                    3 => format!("scr#{decoded_id}"),
                    _ => unreachable!(),
                }),
                Some(expected.to_string())
            );
        }
        assert_eq!(encode_operation(0, 1), None);
        assert_eq!(encode_operation(MAX_OPERATION_ID + 1, 1), None);
        assert_eq!(encode_operation(1, 0), None);
        assert_eq!(encode_operation(1, 4), None);
        assert_eq!(decode_operation(0), None);
        assert_eq!(decode_operation(3), None);
    }

    #[test]
    fn retiring_an_old_operation_cannot_clear_a_newer_snapshot() {
        let capture = encode_operation(7, 1).unwrap();
        let recording = encode_operation(8, 2).unwrap();
        assert_eq!(retire_matching(capture, 7), Some(0));
        assert_eq!(retire_matching(capture, 8), None);
        assert_eq!(retire_matching(recording, 7), None);
        assert_eq!(retire_matching(recording, 8), Some(0));
        assert_eq!(retire_matching(0, 7), None);
    }

    #[test]
    fn concurrent_retirement_uses_a_compare_exchange_loop() {
        let initial = encode_operation(7, 3).unwrap();
        let newer = encode_operation(8, 1).unwrap();
        let mut current = initial;
        let mut attempts = 0;
        let retired = loop {
            let Some(next) = retire_matching(current, 7) else {
                break false;
            };
            attempts += 1;
            match current {
                // Simulate a new operation publishing between the load and CAS.
                value if value == initial && attempts == 1 => current = newer,
                value if value == initial => {
                    current = next;
                    break true;
                }
                _ => unreachable!(),
            }
        };
        assert!(!retired);
        assert_eq!(current, newer);
    }
}
