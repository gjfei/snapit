//! Launch at login via `auto-launch` (LaunchAgent on macOS, registry Run key on
//! Windows).

use anyhow::{Context as _, Result, anyhow};
use auto_launch::{AutoLaunch, AutoLaunchBuilder, MacOSLaunchMode, WindowsEnableMode};
use gpui_kit::{App, Global};

use crate::{controller::Controller, settings::AppSettings};

/// Started by the OS at login: stay in the tray.
pub const BACKGROUND_ARG: &str = "--background";

#[derive(Default)]
struct AutostartQueue {
    generation: u64,
    running: bool,
    /// Only the newest requested state needs to run after the current one.
    pending: Option<bool>,
}

impl Global for AutostartQueue {}

#[derive(Debug, PartialEq, Eq)]
enum QueueAdvance {
    Idle,
    Next { generation: u64, enabled: bool },
}

impl AutostartQueue {
    fn enqueue(&mut self, enabled: bool) -> u64 {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("autostart request generation exhausted");
        self.pending = Some(enabled);
        self.running = true;
        self.generation
    }

    fn take_next(&mut self) -> Option<(u64, bool)> {
        if !self.running {
            return None;
        }
        self.pending
            .take()
            .map(|enabled| (self.generation, enabled))
    }

    fn after_completed(&mut self, generation: u64) -> QueueAdvance {
        debug_assert!(generation <= self.generation);
        // A stale result may only advance the queue to the newest request. It
        // must never publish its own config change or notification.
        if let Some(enabled) = self.pending.take() {
            QueueAdvance::Next {
                generation: self.generation,
                enabled,
            }
        } else {
            self.running = false;
            QueueAdvance::Idle
        }
    }

    fn is_current(&self, generation: u64) -> bool {
        generation == self.generation
    }
}

fn launcher() -> Result<AutoLaunch> {
    let exe = std::env::current_exe().context("cannot locate the SnapIt executable")?;
    let exe = exe
        .to_str()
        .ok_or_else(|| anyhow!("executable path is not valid UTF-8"))?;
    AutoLaunchBuilder::new()
        .set_app_name("SnapIt")
        .set_app_path(exe)
        .set_macos_launch_mode(MacOSLaunchMode::LaunchAgent)
        .set_windows_enable_mode(WindowsEnableMode::CurrentUser)
        .set_args(&[BACKGROUND_ARG])
        .build()
        .context("failed to configure launch at login")
}

fn set_enabled(enabled: bool) -> Result<()> {
    let launcher = launcher()?;
    // A failed query says nothing about the registration. In particular, do
    // not treat it as `!enabled` and issue the opposite mutation.
    if launcher
        .is_enabled()
        .context("failed to query launch at login")?
        == enabled
    {
        return Ok(());
    }
    if enabled {
        launcher
            .enable()
            .context("failed to enable launch at login")?;
    } else {
        launcher
            .disable()
            .context("failed to disable launch at login")?;
    }
    Ok(())
}

fn is_enabled() -> Result<bool> {
    Ok(launcher()?.is_enabled()?)
}

struct SyncOutcome {
    result: Result<()>,
    /// `None` means the post-failure query also failed: the OS state is unknown.
    actual: Option<bool>,
}

fn synchronize(enabled: bool) -> SyncOutcome {
    match set_enabled(enabled) {
        Ok(()) => SyncOutcome {
            result: Ok(()),
            actual: Some(enabled),
        },
        Err(err) => SyncOutcome {
            result: Err(err),
            actual: is_enabled().ok(),
        },
    }
}

fn failure_detail(err: &anyhow::Error, actual: Option<bool>) -> String {
    match actual {
        Some(enabled) => format!(
            "{err:#}; current registration: {}",
            if enabled { "enabled" } else { "disabled" }
        ),
        None => format!("{err:#}; registration status unknown"),
    }
}

/// Preserve the user's setting when the OS state cannot be queried. A known
/// mismatch is reflected in Settings; an unknown one is not guessed.
fn config_after_failure(requested: bool, actual: Option<bool>) -> bool {
    actual.unwrap_or(requested)
}

fn apply_outcome(outcome: SyncOutcome, generation: u64, cx: &mut App) {
    let Err(err) = outcome.result else {
        return;
    };
    if let Some(actual) = outcome.actual {
        let requested = AppSettings::get(cx).general.launch_at_login;
        AppSettings::update(cx, |config| {
            config.general.launch_at_login = config_after_failure(requested, Some(actual));
        });
    }
    if !cx.global::<AutostartQueue>().is_current(generation) {
        return;
    }
    Controller::global(cx).update(cx, |controller, cx| {
        controller.fail(
            t!(
                "autostart.failed",
                error = failure_detail(&err, outcome.actual)
            )
            .to_string(),
            cx,
        )
    });
}

/// Make the OS registration match `enabled`. Mutations run one at a time; a
/// newer user choice supersedes pending work and only the newest result may
/// update Settings or notify the user.
pub fn apply(enabled: bool, cx: &mut App) {
    if !cx.has_global::<AutostartQueue>() {
        cx.set_global(AutostartQueue::default());
    }
    let was_running = cx.global::<AutostartQueue>().running;
    cx.global_mut::<AutostartQueue>().enqueue(enabled);
    if was_running {
        return;
    }
    // The first request starts the sole pump. Later requests only replace
    // `pending`; `take_next` at the top of the loop observes that replacement.
    cx.spawn(async move |cx| {
        let mut request = cx.update(|cx| cx.global_mut::<AutostartQueue>().take_next());
        while let Some((generation, enabled)) = request {
            let outcome = cx
                .background_executor()
                .spawn(async move { synchronize(enabled) })
                .await;
            request = cx.update(|cx| {
                if cx.global::<AutostartQueue>().is_current(generation) {
                    apply_outcome(outcome, generation, cx);
                }
                match cx
                    .global_mut::<AutostartQueue>()
                    .after_completed(generation)
                {
                    QueueAdvance::Idle => None,
                    QueueAdvance::Next {
                        generation,
                        enabled,
                    } => Some((generation, enabled)),
                }
            });
        }
    })
    .detach();
}

/// Restore a missing registration when the setting is on.
pub fn sync_on_launch(cx: &mut App) {
    if AppSettings::get(cx).general.launch_at_login {
        apply(true, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn newest_request_supersedes_pending_work() {
        let mut queue = AutostartQueue::default();
        assert_eq!(queue.enqueue(false), 1);
        assert_eq!(queue.take_next(), Some((1, false)));
        assert_eq!(queue.enqueue(true), 2);
        assert_eq!(queue.enqueue(false), 3);
        assert!(!queue.is_current(1));
        assert!(queue.is_current(3));
        assert_eq!(
            queue.after_completed(1),
            QueueAdvance::Next {
                generation: 3,
                enabled: false
            }
        );
        assert_eq!(queue.after_completed(3), QueueAdvance::Idle);
        assert!(!queue.running);
    }

    #[test]
    fn query_failure_preserves_the_requested_configuration() {
        assert!(config_after_failure(true, None));
        assert!(!config_after_failure(false, None));
        assert!(config_after_failure(false, Some(true)));
        assert!(!config_after_failure(true, Some(false)));
        let err = anyhow!("query failed");
        assert!(failure_detail(&err, None).contains("status unknown"));
        assert!(failure_detail(&err, Some(true)).contains("enabled"));
    }

    #[test]
    fn mock_worker_is_serial_and_drops_stale_results() {
        let queue = Arc::new(std::sync::Mutex::new(AutostartQueue::default()));
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let started = Arc::new(std::sync::Mutex::new(Vec::new()));
        let applied = Arc::new(std::sync::Mutex::new(Vec::new()));
        let worker_queue = queue.clone();
        let worker_entered = entered.clone();
        let worker_release = release.clone();
        let worker_started = started.clone();
        let worker_applied = applied.clone();
        assert_eq!(queue.lock().unwrap().enqueue(false), 1);
        let worker = std::thread::spawn(move || {
            let mut request = worker_queue.lock().unwrap().take_next();
            let mut first = true;
            while let Some((generation, enabled)) = request {
                worker_started.lock().unwrap().push(enabled);
                if first {
                    first = false;
                    worker_entered.wait();
                    worker_release.wait();
                }
                // This is the same publication guard as the GPUI pump: stale
                // mock outcomes are deliberately not recorded as applied.
                if worker_queue.lock().unwrap().is_current(generation) {
                    // A mock notification/config publication would happen here.
                    worker_applied.lock().unwrap().push(enabled);
                }
                request = match worker_queue.lock().unwrap().after_completed(generation) {
                    QueueAdvance::Idle => None,
                    QueueAdvance::Next {
                        generation,
                        enabled,
                    } => Some((generation, enabled)),
                };
            }
        });

        entered.wait();
        // Both changes land while the mocked OS query is blocked.
        assert_eq!(queue.lock().unwrap().enqueue(true), 2);
        assert_eq!(queue.lock().unwrap().enqueue(false), 3);
        release.wait();
        worker.join().unwrap();
        assert_eq!(*started.lock().unwrap(), vec![false, false]);
        assert_eq!(*applied.lock().unwrap(), vec![false]);
        assert!(!queue.lock().unwrap().running);
    }
}
