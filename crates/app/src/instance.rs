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
use std::time::Duration;

use gpui_kit::App;
use snapit_store::AppDirs;

use crate::controller::{Controller, Panel};
use crate::events::{AppEvent, Events};
use crate::session::CaptureIntent;

const LOCK_FILE: &str = "snapit.lock";
const REQUEST_FILE: &str = "snapit.request";
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
    dirs.data_dir().join(REQUEST_FILE)
}

/// Take the instance lock. The lock lives until the returned file is dropped,
/// i.e. for the rest of the process, and the OS releases it on a crash.
///
/// `Ok(None)` means another instance holds it; `Err` means locking is not
/// available (read-only or foreign filesystem) and the app should keep running.
pub(crate) fn acquire(dirs: &AppDirs) -> anyhow::Result<Option<File>> {
    let path = dirs.data_dir().join(LOCK_FILE);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let file = open(&path)?;
    match file.try_lock() {
        Ok(()) => {
            // A request left behind by an owner that died before polling it
            // belongs to that run; replaying it here would open a stray window.
            let _ = fs::remove_file(request_path(dirs));
            Ok(Some(file))
        }
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(err)) => Err(err.into()),
    }
}

/// Hand a launch request to the running instance. Write to a PID-scoped temp
/// file and rename, so the owner never reads a half-written command.
pub(crate) fn send(dirs: &AppDirs, request: LaunchRequest) -> anyhow::Result<()> {
    let path = request_path(dirs);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = dirs
        .data_dir()
        .join(format!("{REQUEST_FILE}.{}.tmp", std::process::id()));
    let mut file = File::create(&temp)?;
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

/// Consume a pending request. Removing it before dispatch means a failed
/// action, a restart or a retry cannot replay it. Unrecognised contents are
/// discarded so a stale file from another build stays harmless.
fn take(path: &Path) -> Option<LaunchRequest> {
    let contents = fs::read_to_string(path).ok()?;
    if fs::remove_file(path).is_err() {
        return None;
    }
    LaunchRequest::parse(contents.trim())
}

/// Poll requests from later launches and hand them to the event bus. The bus
/// consumer runs on the GPUI main thread, so this task never touches UI state.
/// Call after `events::listen`.
pub(crate) fn poll_requests(path: PathBuf, events: Events, cx: &mut App) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(POLL_INTERVAL).await;
            let Some(request) = take(&path) else {
                continue;
            };
            log::info!("handling forwarded launch request: {request:?}");
            events.send(AppEvent::LaunchRequest(request));
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

    fn temp_dirs() -> (AppDirs, PathBuf) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is after the epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("snapit-instance-{nanos}"));
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
    fn request_file_is_consumed_once() {
        let (dirs, root) = temp_dirs();
        let path = request_path(&dirs);
        for request in [
            LaunchRequest::Capture,
            LaunchRequest::Settings,
            LaunchRequest::History,
        ] {
            send(&dirs, request).expect("write request");
            assert_eq!(take(&path), Some(request));
            assert_eq!(take(&path), None, "a consumed request must not replay");
        }
        fs::write(&path, "unknown-command").expect("write stale request");
        assert_eq!(take(&path), None, "unknown contents are discarded");
        let _ = fs::remove_dir_all(root);
    }
}
