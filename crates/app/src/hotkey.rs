//! System-wide shortcuts: parsing, conflict checks and live re-registration.

use global_hotkey::{GlobalHotKeyManager, hotkey::HotKey};
use gpui_kit::{App, Global, Keystroke, Modifiers, component::kbd::Kbd};
use snapit_store::Hotkeys;

use crate::{
    controller::{Controller, Panel},
    settings::AppSettings,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HotkeyAction {
    Screenshot,
    Ocr,
    Gif,
    Video,
    Scroll,
}

impl HotkeyAction {
    pub const COUNT: usize = 5;
    pub const ALL: [Self; 5] = [
        Self::Screenshot,
        Self::Ocr,
        Self::Gif,
        Self::Video,
        Self::Scroll,
    ];

    pub fn label(self) -> String {
        match self {
            Self::Screenshot => t!("action.screenshot").to_string(),
            Self::Ocr => t!("action.ocr").to_string(),
            Self::Gif => t!("action.gif").to_string(),
            Self::Video => t!("action.video").to_string(),
            Self::Scroll => t!("action.scroll").to_string(),
        }
    }

    fn ix(self) -> usize {
        self as usize
    }

    pub fn binding(self, hotkeys: &Hotkeys) -> &str {
        match self {
            Self::Screenshot => &hotkeys.screenshot,
            Self::Ocr => &hotkeys.ocr,
            Self::Gif => &hotkeys.gif,
            Self::Video => &hotkeys.video,
            Self::Scroll => &hotkeys.scroll,
        }
    }

    pub fn set_binding(self, hotkeys: &mut Hotkeys, binding: String) {
        match self {
            Self::Screenshot => hotkeys.screenshot = binding,
            Self::Ocr => hotkeys.ocr = binding,
            Self::Gif => hotkeys.gif = binding,
            Self::Video => hotkeys.video = binding,
            Self::Scroll => hotkeys.scroll = binding,
        }
    }
}

/// `Ok(None)` means the shortcut is disabled.
pub fn parse(binding: &str) -> Result<Option<HotKey>, String> {
    let binding = binding.trim();
    if binding.is_empty() {
        return Ok(None);
    }
    let hotkey: HotKey = binding
        .parse()
        .map_err(|err| t!("hotkey.error.parse", binding = binding, err = err))?;
    if hotkey.mods.is_empty() {
        return Err(t!("hotkey.error.modifier").into());
    }
    Ok(Some(hotkey))
}

/// Parse every binding and flag ones that repeat an earlier action's shortcut.
pub fn validate(hotkeys: &Hotkeys) -> [Result<Option<HotKey>, String>; HotkeyAction::COUNT] {
    let parsed = HotkeyAction::ALL.map(|action| parse(action.binding(hotkeys)));
    let mut checked = parsed.clone();
    for (ix, result) in parsed.iter().enumerate() {
        let Ok(Some(hotkey)) = result else { continue };
        let earlier = parsed[..ix]
            .iter()
            .position(|other| matches!(other, Ok(Some(other)) if other.id() == hotkey.id()));
        if let Some(earlier) = earlier {
            checked[ix] = Err(t!(
                "hotkey.error.conflict",
                action = HotkeyAction::ALL[earlier].label()
            )
            .into());
        }
    }
    checked
}

/// Validate one action's binding against all the others.
pub fn check(action: HotkeyAction, hotkeys: &Hotkeys) -> Result<Option<HotKey>, String> {
    let Some(hotkey) = parse(action.binding(hotkeys))? else {
        return Ok(None);
    };
    let clash = HotkeyAction::ALL
        .into_iter()
        .filter(|other| *other != action)
        .find(|other| {
            matches!(parse(other.binding(hotkeys)), Ok(Some(other)) if other.id() == hotkey.id())
        });
    match clash {
        Some(other) => Err(t!("hotkey.error.conflict", action = other.label()).into()),
        None => Ok(Some(hotkey)),
    }
}

const PLATFORM_MODIFIER: &str = if cfg!(target_os = "macos") {
    "Cmd"
} else {
    "Super"
};

/// macOS folds Shift into the character itself (⌘⇧2 arrives as ⌘@); undo that
/// with the US layout, which is also what GPUI reports while Cmd is held.
const SHIFTED_SYMBOLS: &str = "~!@#$%^&*()_+{}|:\"<>?";
const PLAIN_SYMBOLS: &str = "`1234567890-=[]\\;',./";

fn cap(key: impl Into<String>) -> Keystroke {
    Keystroke {
        modifiers: Modifiers::none(),
        key: key.into(),
        key_char: None,
    }
}

/// Held modifiers as individual caps, in the order both platforms print them.
pub fn modifier_caps(modifiers: Modifiers) -> Vec<Keystroke> {
    let mut caps = Vec::new();
    if modifiers.control {
        caps.push(cap("ctrl"));
    }
    if modifiers.alt {
        caps.push(cap("alt"));
    }
    if modifiers.shift {
        caps.push(cap("shift"));
    }
    if modifiers.platform {
        caps.push(cap("cmd"));
    }
    caps
}

/// One cap per modifier plus one for the main key; `Kbd` prints this platform's
/// labels. Empty when the shortcut is disabled.
pub fn caps(binding: &str) -> Vec<Keystroke> {
    let binding = binding.trim();
    if binding.is_empty() {
        return Vec::new();
    }
    let (modifiers, key) = binding.rsplit_once('+').unwrap_or(("", binding));
    let mut held = Modifiers::none();
    for token in modifiers.split('+') {
        match token.to_ascii_uppercase().as_str() {
            "CONTROL" | "CTRL" => held.control = true,
            "OPTION" | "ALT" => held.alt = true,
            "SHIFT" => held.shift = true,
            "COMMAND" | "CMD" | "SUPER" | "META" => held.platform = true,
            _ => {}
        }
    }
    let mut caps = modifier_caps(held);
    caps.push(cap(key.to_ascii_lowercase()));
    caps
}

/// Plain-text shortcut for places that cannot render `Kbd`, like the tray menu.
pub fn label(binding: &str) -> String {
    let separator = if cfg!(target_os = "macos") { "" } else { "+" };
    caps(binding)
        .iter()
        .map(Kbd::format)
        .collect::<Vec<_>>()
        .join(separator)
}

/// Write a pressed combination back into config syntax. `global-hotkey` accepts
/// GPUI's key names, so `parse` doubles as the validator.
pub fn from_keystroke(stroke: &Keystroke) -> Result<String, String> {
    let mut held = stroke.modifiers;
    if held.function {
        return Err(t!("hotkey.error.fn").into());
    }
    let mut key = stroke.key.clone();
    if key.len() == 1
        && let Some(ix) = SHIFTED_SYMBOLS.find(&key)
    {
        key = PLAIN_SYMBOLS[ix..ix + 1].to_string();
        held.shift = true;
    }
    if !(held.control || held.alt || held.shift || held.platform) {
        return Err(t!("hotkey.error.need_modifier").into());
    }
    key.make_ascii_uppercase();
    let mut tokens = Vec::new();
    if held.control {
        tokens.push("Ctrl");
    }
    if held.alt {
        tokens.push("Alt");
    }
    if held.shift {
        tokens.push("Shift");
    }
    if held.platform {
        tokens.push(PLATFORM_MODIFIER);
    }
    tokens.push(&key);
    let binding = tokens.join("+");
    parse(&binding)?;
    Ok(binding)
}

struct HotkeyService {
    manager: GlobalHotKeyManager,
    registered: [Option<HotKey>; HotkeyAction::COUNT],
    errors: [Option<String>; HotkeyAction::COUNT],
    /// Set while the settings window records a new combination.
    suspended: bool,
}

impl Global for HotkeyService {}

/// Kept when the OS refuses to create the manager, so the settings page can
/// explain why shortcuts are unavailable instead of showing a generic line.
struct HotkeyInitError(String);

impl Global for HotkeyInitError {}

/// `notify_on_failure` is false for `--background` (login) launches: they stay
/// tray-only even when the shortcuts could not be registered.
pub fn init(cx: &mut App, notify_on_failure: bool) {
    let manager = match GlobalHotKeyManager::new() {
        Ok(manager) => manager,
        Err(err) => {
            let reason = t!("hotkey.error.init", err = err).to_string();
            log::error!("{reason}");
            cx.set_global(HotkeyInitError(reason));
            if notify_on_failure {
                // The app is tray-only and has no window yet, so opening the
                // shortcuts page is the only visible way to surface the error.
                // Defer: `open_panel` renders its window immediately.
                cx.defer(|cx| {
                    Controller::global(cx).update(cx, |controller, cx| {
                        controller.open_panel(Panel::Settings, cx)
                    });
                });
            }
            return;
        }
    };
    cx.set_global(HotkeyService {
        manager,
        registered: [None; HotkeyAction::COUNT],
        errors: Default::default(),
        suspended: false,
    });
    let hotkeys = AppSettings::get(cx).hotkeys.clone();
    sync(&hotkeys, cx);
    // Pressed events arrive through `events::install`'s callback; there is no
    // polling loop to start here.
}

pub(crate) fn action_for(id: u32, cx: &App) -> Option<HotkeyAction> {
    let service = cx.try_global::<HotkeyService>()?;
    HotkeyAction::ALL
        .into_iter()
        .find(|action| service.registered[action.ix()].is_some_and(|hotkey| hotkey.id() == id))
}

/// Make the OS registrations match `hotkeys`, recording per-action errors.
pub fn sync(hotkeys: &Hotkeys, cx: &mut App) {
    if !cx.has_global::<HotkeyService>() {
        return;
    }
    let wanted = validate(hotkeys);
    let service = cx.global_mut::<HotkeyService>();
    // Release changed bindings first so two actions can swap shortcuts.
    for (ix, wanted) in wanted.iter().enumerate() {
        let keep = matches!((wanted, service.registered[ix]), (Ok(Some(new)), Some(old)) if new.id() == old.id());
        if !keep && let Some(old) = service.registered[ix].take() {
            service.manager.unregister(old).ok();
        }
    }
    for (ix, wanted) in wanted.into_iter().enumerate() {
        service.errors[ix] = match wanted {
            Ok(Some(_)) if service.registered[ix].is_some() => None,
            Ok(Some(hotkey)) if !service.suspended => match service.manager.register(hotkey) {
                Ok(()) => {
                    service.registered[ix] = Some(hotkey);
                    None
                }
                Err(err) => Some(t!("hotkey.error.registration", err = err).into()),
            },
            Ok(Some(_)) | Ok(None) => None,
            Err(err) => Some(err),
        };
        if let Some(err) = &service.errors[ix] {
            log::warn!("hotkey {:?}: {err}", HotkeyAction::ALL[ix]);
        }
    }
}

/// Release every OS registration while a new combination is being recorded:
/// otherwise pressing the live shortcut starts a capture instead of reaching the
/// settings window.
pub fn suspend(cx: &mut App) {
    if !cx.has_global::<HotkeyService>() {
        return;
    }
    let service = cx.global_mut::<HotkeyService>();
    service.suspended = true;
    for slot in &mut service.registered {
        if let Some(old) = slot.take() {
            service.manager.unregister(old).ok();
        }
    }
}

pub fn resume(cx: &mut App) {
    if !cx.has_global::<HotkeyService>() {
        return;
    }
    cx.global_mut::<HotkeyService>().suspended = false;
    let hotkeys = AppSettings::get(cx).hotkeys.clone();
    sync(&hotkeys, cx);
}

pub fn error(action: HotkeyAction, cx: &App) -> Option<&str> {
    cx.try_global::<HotkeyService>()?.errors[action.ix()].as_deref()
}

pub fn is_available(cx: &App) -> bool {
    cx.has_global::<HotkeyService>()
}

/// Why global shortcuts cannot be used, for the settings page. `None` means the
/// manager started successfully.
pub fn unavailable_reason(cx: &App) -> Option<String> {
    if is_available(cx) {
        return None;
    }
    Some(
        cx.try_global::<HotkeyInitError>()
            .map(|error| error.0.clone())
            .unwrap_or_else(|| t!("settings.hotkey.unavailable").to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hotkeys(screenshot: &str, ocr: &str, gif: &str) -> Hotkeys {
        let mut hotkeys = Hotkeys::default();
        hotkeys.screenshot = screenshot.into();
        hotkeys.ocr = ocr.into();
        hotkeys.gif = gif.into();
        hotkeys
    }

    #[test]
    fn defaults_are_valid() {
        assert!(
            validate(&Hotkeys::default())
                .iter()
                .all(|r| matches!(r, Ok(Some(_))))
        );
    }

    #[test]
    fn detects_conflicts_case_insensitively() {
        let result = validate(&hotkeys("Alt+Shift+A", "shift+alt+KeyA", ""));
        assert!(matches!(result[0], Ok(Some(_))));
        assert!(
            result[1]
                .as_ref()
                .unwrap_err()
                .contains(&HotkeyAction::Screenshot.label())
        );
        assert!(matches!(result[2], Ok(None)));
        assert!(
            matches!(result[3], Ok(Some(_))),
            "the video action keeps its own default while GIF is cleared"
        );

        let both = hotkeys("Alt+Shift+A", "Alt+Shift+A", "Alt+Shift+G");
        assert!(check(HotkeyAction::Screenshot, &both).is_err());
        assert!(check(HotkeyAction::Ocr, &both).is_err());
        assert!(check(HotkeyAction::Gif, &both).is_ok());
    }

    #[test]
    fn a_shared_shortcut_is_reported_on_the_later_action() {
        let mut hotkeys = Hotkeys::default();
        hotkeys.video = hotkeys.gif.clone();
        let result = validate(&hotkeys);
        assert!(
            result[3]
                .as_ref()
                .unwrap_err()
                .contains(&HotkeyAction::Gif.label())
        );
        assert!(matches!(result[2], Ok(Some(_))), "GIF keeps the shortcut");
    }

    #[test]
    fn rejects_garbage_and_bare_keys() {
        assert!(parse("Alt+Shift+").is_err());
        assert!(parse("Ctrl+Nope").is_err());
        assert!(parse("A").is_err());
        assert!(parse("  ").unwrap().is_none());
    }

    /// Each cap as `Kbd` prints it, so the assertions follow the platform.
    fn shown(binding: &str) -> Vec<String> {
        caps(binding).iter().map(Kbd::format).collect()
    }

    #[test]
    fn caps_follow_platform_labels() {
        if cfg!(target_os = "macos") {
            assert_eq!(shown("Alt+Shift+A"), ["⌥", "⇧", "A"]);
            assert_eq!(shown("Cmd+Shift+Space"), ["⇧", "⌘", "Space"]);
            assert_eq!(shown("Ctrl+Alt+Left"), ["⌃", "⌥", "←"]);
        } else {
            assert_eq!(shown("Alt+Shift+A"), ["Alt", "Shift", "A"]);
            assert_eq!(shown("Cmd+Shift+Space"), ["Shift", "Win", "Space"]);
            assert_eq!(shown("Ctrl+Alt+Left"), ["Ctrl", "Alt", "Left"]);
        }
        assert_eq!(shown("Alt+PageUp").last().unwrap(), "Page Up");
        assert!(shown("   ").is_empty());
    }

    #[test]
    fn keystrokes_round_trip() {
        let stroke = |key: &str, modifiers: Modifiers| Keystroke {
            modifiers,
            key: key.into(),
            key_char: None,
        };
        let alt_shift = Modifiers {
            alt: true,
            shift: true,
            ..Modifiers::none()
        };
        assert_eq!(
            from_keystroke(&stroke("a", alt_shift)).unwrap(),
            "Alt+Shift+A"
        );
        assert_eq!(
            from_keystroke(&stroke("left", alt_shift)).unwrap(),
            "Alt+Shift+LEFT"
        );
        assert_eq!(
            from_keystroke(&stroke(
                "f5",
                Modifiers {
                    control: true,
                    ..Modifiers::none()
                }
            ))
            .unwrap(),
            "Ctrl+F5"
        );
        // macOS reports ⌘⇧2 as ⌘@, so the symbol has to go back to 2 + Shift.
        assert_eq!(
            from_keystroke(&stroke(
                "@",
                Modifiers {
                    platform: true,
                    ..Modifiers::none()
                }
            ))
            .unwrap(),
            format!("Shift+{PLATFORM_MODIFIER}+2")
        );
        assert!(from_keystroke(&stroke("a", Modifiers::none())).is_err());
        assert!(
            from_keystroke(&stroke(
                "a",
                Modifiers {
                    function: true,
                    alt: true,
                    ..Modifiers::none()
                }
            ))
            .is_err()
        );
    }

    #[test]
    fn labels_are_platform_specific() {
        if cfg!(target_os = "macos") {
            assert_eq!(label("Alt+Shift+A"), "⌥⇧A");
        } else {
            assert_eq!(label("Alt+Shift+A"), "Alt+Shift+A");
        }
        assert_eq!(label(""), "");
    }

    /// Settings keeps parallel arrays indexed by `action as usize`, so every
    /// new action must be added to ALL and COUNT together.
    #[test]
    fn action_discriminants_stay_dense_and_match_all() {
        assert_eq!(HotkeyAction::ALL.len(), HotkeyAction::COUNT);
        for (ix, action) in HotkeyAction::ALL.iter().enumerate() {
            assert_eq!(*action as usize, ix);
        }
    }
}
