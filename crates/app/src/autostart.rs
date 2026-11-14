//! Launch at login via `auto-launch` (LaunchAgent on macOS, registry Run key on
//! Windows).

use anyhow::{Context as _, Result, anyhow};
use auto_launch::{AutoLaunch, AutoLaunchBuilder, MacOSLaunchMode, WindowsEnableMode};
use gpui_kit::{App, AppContext as _};

use crate::{controller::Controller, settings::AppSettings};

/// Started by the OS at login: stay in the tray.
pub const BACKGROUND_ARG: &str = "--background";

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

fn set_enabled(enabled: bool) -> Result<bool> {
    let launcher = launcher()?;
    if launcher.is_enabled().unwrap_or(!enabled) == enabled {
        return Ok(enabled);
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
    Ok(enabled)
}

fn is_enabled() -> Result<bool> {
    Ok(launcher()?.is_enabled()?)
}

/// Make the OS registration match `enabled`; on failure the setting is reverted.
pub fn apply(enabled: bool, cx: &mut App) {
    let task = cx.background_spawn(async move {
        set_enabled(enabled).map_err(|err| (err, is_enabled().unwrap_or(!enabled)))
    });
    cx.spawn(async move |cx| {
        if let Err((err, actual)) = task.await {
            cx.update(|cx| {
                AppSettings::update(cx, |config| config.general.launch_at_login = actual);
                Controller::global(cx).update(cx, |controller, cx| {
                    controller.fail(
                        t!("autostart.failed", error = format!("{err:#}")).to_string(),
                        cx,
                    )
                });
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
