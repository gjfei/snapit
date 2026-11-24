//! One process per data directory. A second launch would add a duplicate tray
//! icon and fail to register the same global hotkeys, so it exits after handing
//! any requested action (`--capture` / `--settings` / `--history`) to the owner.
//!
//! The hand-off uses a request file next to the lock rather than a socket: it
//! needs no port, no listener thread, and works the same on macOS and Windows.
//! A file has no notification mechanism, so the owner polls it every 50 ms and
//! then posts the request onto the shared event bus for main-thread dispatch.

use std::fs::{self, File, TryLockError};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui_kit::App;
use snapit_store::AppDirs;

use crate::controller::{Controller, Panel};
use crate::events::{AppEvent, Events};
use crate::session::CaptureIntent;

const LOCK_FILE: &str = "snapit.lock";
const SPOOL_LOCK_FILE: &str = "snapit.requests.lock";
const REQUEST_DIR: &str = "snapit.requests";
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// An action a second launch can ask the running instance to perform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LaunchRequest {
    Capture,
    Settings,
    History,
}

impl LaunchRequest {
    /// `--background` only changes how the *first* launch starts, so it is not
    /// a request. The first recognised flag wins, matching the old if/else.
    pub(crate) fn from_args(args: &[String]) -> Option<Self> {
        args.iter().find_map(|arg| match arg.as_str() {
            "--capture" => Some(Self::Capture),
            "--settings" => Some(Self::Settings),
            "--history" => Some(Self::History),
            _ => None,
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Capture => "capture",
            Self::Settings => "settings",
            Self::History => "history",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "capture" => Some(Self::Capture),
            "settings" => Some(Self::Settings),
            "history" => Some(Self::History),
            _ => None,
        }
    }
}

pub(crate) fn request_path(dirs: &AppDirs) -> PathBuf {
    dirs.data_dir().join(REQUEST_DIR)
}

/// Take the instance lock. The lock lives until the returned file is dropped,
/// i.e. for the rest of the process, and the OS releases it on a crash.
///
/// `Ok(None)` means another instance holds it; `Err` means locking is not
/// available (read-only or foreign filesystem) and the caller must not risk a
/// duplicate tray and hotkey registration.
pub(crate) fn acquire(dirs: &AppDirs) -> anyhow::Result<Option<File>> {
    let data = dirs.data_dir();
    fs::create_dir_all(data)?;
    // Lock the spool before the instance: a sender holding the spool can then
    // reliably distinguish a live owner from one that is still initialising.
    let spool_lock = open(&data.join(SPOOL_LOCK_FILE))?;
    spool_lock.lock()?;
    let file = open(&data.join(LOCK_FILE))?;
    match file.try_lock() {
        Ok(()) => {
            // Requests left behind by an owner that died before polling them
            // belong to that run; replaying them here would open stray windows.
            let _ = fs::remove_dir_all(request_path(dirs));
            Ok(Some(file))
        }
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(err)) => Err(err.into()),
    }
}

/// Hand a launch request to the running instance through a unique spool file.
/// The owner never reads a half-written command, and concurrent senders cannot
/// overwrite or delete each other's requests.
pub(crate) fn send(dirs: &AppDirs, request: LaunchRequest) -> anyhow::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let data = dirs.data_dir();
    fs::create_dir_all(data)?;
    let spool_lock = open(&data.join(SPOOL_LOCK_FILE))?;
    spool_lock.lock()?;
    let owner = open(&data.join(LOCK_FILE))?;
    match owner.try_lock() {
        Ok(()) => anyhow::bail!("no running SnapIt instance"),
        Err(TryLockError::WouldBlock) => {}
        Err(TryLockError::Error(err)) => return Err(err.into()),
    }
    let dir = request_path(dirs);
    fs::create_dir_all(&dir)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let serial = NEXT.fetch_add(1, Ordering::Relaxed);
    let stem = format!("{nanos}-{}-{serial}", std::process::id());
    let temp = dir.join(format!(".{stem}.tmp"));
    let path = dir.join(format!("{stem}.request"));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    file.write_all(request.as_str().as_bytes())?;
    drop(file);
    match fs::rename(&temp, &path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = fs::remove_file(&temp);
            Err(err.into())
        }
    }
}

/// Atomically claim one unique request before reading it. A failed action,
/// restart or retry therefore cannot replay an already consumed command.
fn take(path: &Path) -> Option<LaunchRequest> {
    let claimed = path.with_extension("claimed");
    fs::rename(path, &claimed).ok()?;
    let contents = fs::read_to_string(&claimed).ok()?;
    let _ = fs::remove_file(&claimed);
    LaunchRequest::parse(contents.trim())
}

fn request_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "request"))
        .collect();
    paths.sort();
    paths
}

/// Poll requests from later launches and hand them to the event bus. The bus
/// consumer runs on the GPUI main thread, so this task never touches UI state.
/// Call after `events::listen`.
pub(crate) fn poll_requests(dir: PathBuf, events: Events, cx: &mut App) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(POLL_INTERVAL).await;
            for path in request_files(&dir) {
                let Some(request) = take(&path) else {
                    continue;
                };
                log::info!("handling forwarded launch request: {request:?}");
                events.send(AppEvent::LaunchRequest(request));
            }
        }
    })
    .detach();
}

pub(crate) fn dispatch(request: LaunchRequest, cx: &mut App) {
    match request {
        LaunchRequest::Capture => Controller::global(cx).update(cx, |controller, cx| {
            controller.start_capture(CaptureIntent::Choose, cx)
        }),
        LaunchRequest::Settings => Controller::global(cx).update(cx, |controller, cx| {
            controller.open_panel(Panel::Settings, cx)
        }),
        LaunchRequest::History => Controller::global(cx).update(cx, |controller, cx| {
            controller.open_panel(Panel::History, cx)
        }),
    }
}

fn open(path: &Path) -> std::io::Result<File> {
    fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running_owner(dirs: &AppDirs) -> File {
        fs::create_dir_all(dirs.data_dir()).unwrap();
        let owner = open(&dirs.data_dir().join(LOCK_FILE)).unwrap();
        owner.try_lock().unwrap();
        owner
    }

    fn temp_dirs() -> (AppDirs, PathBuf) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is after the epoch")
            .as_nanos();
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("snapit-instance-{nanos}-{serial}"));
        (AppDirs::at(&root), root)
    }

    #[test]
    fn flags_map_to_a_single_request() {
        let args = |flags: &[&str]| flags.iter().map(|f| (*f).to_string()).collect::<Vec<_>>();
        assert_eq!(LaunchRequest::from_args(&args(&[])), None);
        assert_eq!(
            LaunchRequest::from_args(&args(&["--background"])),
            None,
            "a login launch must not open a panel"
        );
        assert_eq!(
            LaunchRequest::from_args(&args(&["--background", "--history"])),
            Some(LaunchRequest::History)
        );
        assert_eq!(
            LaunchRequest::from_args(&args(&["--capture", "--settings"])),
            Some(LaunchRequest::Capture),
            "the first recognised flag wins"
        );
    }

    #[test]
    fn concurrent_senders_cannot_overwrite_or_lose_requests() {
        let (dirs, root) = temp_dirs();
        let _owner = running_owner(&dirs);
        let data_dir = dirs.data_dir().to_path_buf();
        let senders: Vec<_> = (0..24)
            .map(|index| {
                let data_dir = data_dir.clone();
                std::thread::spawn(move || {
                    let request = match index % 3 {
                        0 => LaunchRequest::Capture,
                        1 => LaunchRequest::Settings,
                        _ => LaunchRequest::History,
                    };
                    send(&AppDirs::at(&data_dir), request).expect("write concurrent request");
                })
            })
            .collect();
        for sender in senders {
            sender.join().unwrap();
        }

        let files = request_files(&request_path(&dirs));
        assert_eq!(files.len(), 24, "concurrent senders overwrote a request");
        let mut consumed = Vec::new();
        for path in files {
            consumed.push(take(&path).expect("every unique request is consumable"));
        }
        assert_eq!(
            consumed
                .iter()
                .filter(|request| **request == LaunchRequest::Capture)
                .count(),
            8
        );
        assert_eq!(
            consumed
                .iter()
                .filter(|request| **request == LaunchRequest::Settings)
                .count(),
            8
        );
        assert_eq!(
            consumed
                .iter()
                .filter(|request| **request == LaunchRequest::History)
                .count(),
            8
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn sending_without_an_owner_does_not_leave_a_stale_request() {
        let (dirs, root) = temp_dirs();
        assert!(send(&dirs, LaunchRequest::Capture).is_err());
        assert!(request_files(&request_path(&dirs)).is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_new_owner_clears_only_before_senders_can_enter() {
        let (dirs, root) = temp_dirs();
        fs::create_dir_all(request_path(&dirs)).unwrap();
        fs::write(request_path(&dirs).join("stale.request"), "capture").unwrap();
        let owner = acquire(&dirs).unwrap().expect("first owner");
        assert!(request_files(&request_path(&dirs)).is_empty());
        send(&dirs, LaunchRequest::History).expect("new request");
        assert_eq!(request_files(&request_path(&dirs)).len(), 1);
        drop(owner);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn request_file_is_consumed_once() {
        let (dirs, root) = temp_dirs();
        let _owner = running_owner(&dirs);
        let dir = request_path(&dirs);
        for request in [
            LaunchRequest::Capture,
            LaunchRequest::Settings,
            LaunchRequest::History,
        ] {
            send(&dirs, request).expect("write request");
            let files = request_files(&dir);
            assert_eq!(files.len(), 1);
            assert_eq!(take(&files[0]), Some(request));
            assert_eq!(take(&files[0]), None, "a consumed request must not replay");
        }
        send(&dirs, LaunchRequest::Capture).expect("write stale request");
        let files = request_files(&dir);
        fs::write(&files[0], "unknown-command").expect("write stale request");
        assert_eq!(take(&files[0]), None, "unknown contents are discarded");
        let _ = fs::remove_dir_all(root);
    }
}
