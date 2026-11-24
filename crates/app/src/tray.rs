//! Resident tray icon, created on the GPUI main thread after launch.

use anyhow::{Context as _, Result};
use gpui_kit::{App, Global};
use snapit_recorder::RecordFormat;
use snapit_store::Hotkeys;
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuItem, PredefinedMenuItem},
};

use crate::{
    controller::{Controller, Panel},
    hotkey::{self, HotkeyAction},
    session::CaptureIntent,
    settings::AppSettings,
};

const ICON_SIZE: u32 = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrayCommand {
    Screenshot,
    Animation,
    Video,
    Ocr,
    Pin,
    History,
    Settings,
    Update,
    Quit,
}

impl TrayCommand {
    const ITEMS: [(Self, &'static str); 9] = [
        (Self::Screenshot, "screenshot"),
        (Self::Animation, "animation"),
        (Self::Video, "video"),
        (Self::Ocr, "ocr"),
        (Self::Pin, "pin"),
        (Self::History, "history"),
        (Self::Settings, "settings"),
        (Self::Update, "update"),
        (Self::Quit, "quit"),
    ];

    const TOP_LEVEL: [Self; 9] = [
        Self::Screenshot,
        Self::Animation,
        Self::Video,
        Self::Ocr,
        Self::Pin,
        Self::History,
        Self::Settings,
        Self::Update,
        Self::Quit,
    ];

    fn from_id(id: &str) -> Option<Self> {
        Self::ITEMS
            .iter()
            .find(|(_, key)| *key == id)
            .map(|(command, _)| *command)
    }

    fn label(self) -> String {
        match self {
            Self::Screenshot => t!("action.screenshot").to_string(),
            Self::Animation => t!("action.gif").to_string(),
            Self::Video => t!("action.video").to_string(),
            Self::Ocr => t!("action.ocr").to_string(),
            Self::Pin => t!("action.pin").to_string(),
            Self::History => t!("action.history").to_string(),
            Self::Settings => t!("action.settings").to_string(),
            Self::Update => t!("action.check_updates").to_string(),
            Self::Quit => t!("action.quit").to_string(),
        }
    }

    fn hotkey_action(self) -> Option<HotkeyAction> {
        match self {
            Self::Screenshot => Some(HotkeyAction::Screenshot),
            Self::Animation => Some(HotkeyAction::Gif),
            Self::Video => Some(HotkeyAction::Video),
            Self::Ocr => Some(HotkeyAction::Ocr),
            _ => None,
        }
    }
}

type BuiltTray = (TrayIcon, Vec<(TrayCommand, MenuItem)>);

struct TrayService {
    /// Dropping this removes the status item, so it must outlive the app.
    _icon: TrayIcon,
    /// Every entry, so the menu can be re-labelled after a settings change.
    items: Vec<(TrayCommand, MenuItem)>,
}

impl Global for TrayService {}

/// Re-label the menu after shortcuts or the language change.
pub fn refresh(cx: &App) {
    let Some(service) = cx.try_global::<TrayService>() else {
        return;
    };
    let hotkeys = &AppSettings::get(cx).hotkeys;
    for (command, item) in &service.items {
        item.set_text(item_text(*command, hotkeys));
    }
}

fn item_text(command: TrayCommand, hotkeys: &Hotkeys) -> String {
    match command.hotkey_action() {
        Some(action) => menu_label(action, action.binding(hotkeys)),
        None => command.label(),
    }
}

fn menu_label(action: HotkeyAction, binding: &str) -> String {
    let keys = hotkey::label(binding);
    if keys.is_empty() {
        action.label()
    } else {
        format!("{}   {keys}", action.label())
    }
}

/// Create the tray after launch, on the GPUI main thread.
pub fn init(cx: &mut App) {
    // `Application::run` already finished launching; defer one tick so macOS is
    // pumping events before NSStatusItem is created (fullscreen apps otherwise
    // glitch — see tray-icon's macOS notes).
    cx.defer(|cx| {
        let hotkeys = AppSettings::get(cx).hotkeys.clone();
        match attach(&hotkeys) {
            Ok((icon, items)) => {
                cx.set_global(TrayService { _icon: icon, items });
                log::info!("tray icon ready");
            }
            Err(err) => log::error!("tray icon unavailable: {err:#}"),
        }
    });
}

/// Menu ids arrive on the event bus (see `events::install`); resolve and run
/// them here, on the GPUI main thread.
pub(crate) fn dispatch_menu(id: &str, cx: &mut App) {
    let Some(command) = TrayCommand::from_id(id) else {
        log::debug!("ignoring unknown tray menu id: {id}");
        return;
    };
    handle(command, cx);
}

fn handle(command: TrayCommand, cx: &mut App) {
    match command {
        TrayCommand::Screenshot => Controller::global(cx).update(cx, |controller, cx| {
            controller.start_capture(CaptureIntent::Choose, cx)
        }),
        TrayCommand::Animation => Controller::global(cx).update(cx, |controller, cx| {
            controller.start_capture(
                CaptureIntent::Record(crate::controller::last_animation_format(cx)),
                cx,
            )
        }),
        TrayCommand::Video => Controller::global(cx).update(cx, |controller, cx| {
            controller.start_capture(CaptureIntent::Record(RecordFormat::Mp4), cx)
        }),
        TrayCommand::Ocr => Controller::global(cx).update(cx, |controller, cx| {
            controller.start_capture(CaptureIntent::Recognize, cx)
        }),
        TrayCommand::Pin => {
            Controller::global(cx).update(cx, |controller, cx| controller.pin_last(cx))
        }
        TrayCommand::History => Controller::global(cx).update(cx, |controller, cx| {
            controller.open_panel(Panel::History, cx)
        }),
        TrayCommand::Settings => Controller::global(cx).update(cx, |controller, cx| {
            controller.open_panel(Panel::Settings, cx)
        }),
        TrayCommand::Update => crate::updater::check(true, cx),
        TrayCommand::Quit => cx.quit(),
    }
}

fn attach(hotkeys: &Hotkeys) -> Result<BuiltTray> {
    let menu = Menu::new();
    let mut items = Vec::new();
    for command in TrayCommand::TOP_LEVEL {
        // Separators before the "act on a capture" group and before Quit.
        if matches!(command, TrayCommand::Pin | TrayCommand::Quit) {
            menu.append(&PredefinedMenuItem::separator())
                .context("failed to build tray menu")?;
        }
        let id = TrayCommand::ITEMS
            .iter()
            .find_map(|(candidate, id)| (*candidate == command).then_some(*id))
            .expect("top-level tray command has an id");
        let item = MenuItem::with_id(id, item_text(command, hotkeys), true, None);
        items.push((command, item.clone()));
        menu.append(&item).context("failed to build tray menu")?;
    }

    let (rgba, width, height) = tray_icon_rgba();
    let icon = Icon::from_rgba(rgba, width, height).context("invalid tray icon")?;
    let builder = TrayIconBuilder::new()
        .with_tooltip("SnapIt")
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(true)
        .with_autosave_name("dev.snapit.tray");
    let icon = with_platform_icon(builder, icon)
        .build()
        .context("failed to create tray icon")?;
    Ok((icon, items))
}

fn with_platform_icon(builder: TrayIconBuilder, icon: Icon) -> TrayIconBuilder {
    #[cfg(target_os = "macos")]
    let builder = builder.with_icon_templated(icon);
    #[cfg(not(target_os = "macos"))]
    let builder = builder.with_icon(icon);
    builder
}

fn tray_icon_rgba() -> (Vec<u8>, u32, u32) {
    // macOS only reads the alpha channel of a template icon; Windows draws the
    // pixels as-is, where white disappears on a light taskbar.
    let ink: [u8; 4] = if cfg!(target_os = "macos") {
        [255, 255, 255, 255]
    } else {
        [238, 155, 44, 255]
    };
    let mut rgba = vec![0u8; (ICON_SIZE * ICON_SIZE * 4) as usize];
    let paint = |rgba: &mut [u8], x: u32, y: u32, w: u32, h: u32| {
        for yy in y..(y + h).min(ICON_SIZE) {
            for xx in x..(x + w).min(ICON_SIZE) {
                let i = ((yy * ICON_SIZE + xx) * 4) as usize;
                rgba[i..i + 4].copy_from_slice(&ink);
            }
        }
    };
    const T: u32 = 4;
    const ARM: u32 = 10;
    const INSET: u32 = 3;
    let far = ICON_SIZE - INSET - T;
    let far_arm = ICON_SIZE - INSET - ARM;
    paint(&mut rgba, INSET, INSET, ARM, T);
    paint(&mut rgba, INSET, INSET, T, ARM);
    paint(&mut rgba, far_arm, INSET, ARM, T);
    paint(&mut rgba, far, INSET, T, ARM);
    paint(&mut rgba, INSET, far, ARM, T);
    paint(&mut rgba, INSET, far_arm, T, ARM);
    paint(&mut rgba, far_arm, far, ARM, T);
    paint(&mut rgba, far, far_arm, T, ARM);
    (rgba, ICON_SIZE, ICON_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_icon_is_valid_rgba() {
        let (rgba, width, height) = tray_icon_rgba();
        assert_eq!(width, height);
        assert_eq!(rgba.len(), (width * height * 4) as usize);
        assert!(rgba.chunks(4).any(|px| px[3] > 0));
        assert!(Icon::from_rgba(rgba, width, height).is_ok());
    }

    #[test]
    fn animation_is_one_command_without_format_choices() {
        assert!(TrayCommand::TOP_LEVEL.contains(&TrayCommand::Animation));
        assert_eq!(
            TrayCommand::from_id("animation"),
            Some(TrayCommand::Animation)
        );
        assert_eq!(TrayCommand::from_id("gif"), None);
        assert_eq!(TrayCommand::from_id("apng"), None);
    }

    #[test]
    fn menu_ids_round_trip() {
        for (command, id) in TrayCommand::ITEMS {
            assert_eq!(TrayCommand::from_id(id), Some(command));
        }
        assert_eq!(TrayCommand::from_id("nope"), None);
    }
}
