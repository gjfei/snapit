//! Theme and Dock presence.

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{ActivationPolicy, App, Subscription, Window, WindowAppearance};
use snapit_store::ThemePreference;

use crate::settings::AppSettings;

pub fn apply_theme(preference: ThemePreference, window: Option<&mut Window>, cx: &mut App) {
    // Theme 只换内容配色；标题栏等原生部分跟随 NSApplication 外观，需单独同步。
    cx.set_window_appearance(match preference {
        ThemePreference::System => None,
        ThemePreference::Light => Some(WindowAppearance::Light),
        ThemePreference::Dark => Some(WindowAppearance::Dark),
    });
    match preference {
        ThemePreference::System => Theme::sync_system_appearance(window, cx),
        ThemePreference::Light => Theme::change(ThemeMode::Light, window, cx),
        ThemePreference::Dark => Theme::change(ThemeMode::Dark, window, cx),
    }
}

/// Re-sync the `System` theme when the OS appearance of `window` changes.
pub fn follow_system_theme(window: &Window) -> Subscription {
    window.observe_window_appearance(|window, cx| {
        if AppSettings::get(cx).general.theme == ThemePreference::System {
            Theme::sync_system_appearance(Some(window), cx);
        }
    })
}

/// macOS only: `Accessory` drops the Dock icon and app menu but keeps windows working.
/// The controller shows it while a panel window is open.
pub fn set_dock_visible(show: bool, cx: &mut App) {
    if !cfg!(target_os = "macos") {
        return;
    }
    cx.set_activation_policy(if show {
        ActivationPolicy::Regular
    } else {
        ActivationPolicy::Accessory
    });
}
