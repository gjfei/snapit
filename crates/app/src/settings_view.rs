//! The settings window, built on gpui-kit's `Settings` component.

use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::Context as _;
use gpui_kit::AppContext as _;
use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, IndexPath, Selectable as _, Sizable as _,
    WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    kbd::Kbd,
    label::Label,
    menu::{DropdownMenu as _, PopupMenuItem},
    searchable_list::SearchableVec,
    select::{Select, SelectEvent, SelectState},
    setting::{
        NumberFieldOptions, RenderOptions, SettingField, SettingGroup, SettingItem, SettingPage,
        Settings,
    },
    switch::Switch,
    tab::{Tab, TabBar},
    v_flex,
};
use gpui_kit::{
    Anchor, Animation, AnimationExt as _, AnyElement, App, AsyncApp, Axis, Bounds, Context,
    CursorStyle, Div, ElementId, Entity, FocusHandle, Hsla, IntoElement, Keystroke, Modifiers,
    MouseButton, ObjectFit, ParentElement as _, PathPromptOptions, Pixels, Point, Render,
    RenderImage, SharedString, Styled as _, StyledImage as _, Subscription, Window, canvas,
    container_query, div, fill, img, point,
    prelude::FluentBuilder as _,
    prelude::{InteractiveElement as _, StatefulInteractiveElement as _},
    px, rgb, size,
};
use snapit_capture::{RgbaImage, load_watermark_image};
use snapit_recorder::{CLICK_DURATION, ClickEffect, ClickEffectGroup};
use snapit_store::{
    Config, HistoryConfig, Hotkeys, ImageFormat, RecordingConfig, ScrollConfig, ScrollSpeed,
    ThemePreference, WatermarkConfig, WatermarkPlacement, WatermarkPreset, WatermarkTarget,
};

use crate::{
    appearance,
    controller::{self, Controller, Panel},
    history_store::{HistoryStore, format_bytes},
    hotkey::{self, HotkeyAction},
    i18n,
    settings::AppSettings,
    updater,
};

const ANIMATION_FORMATS: &[(&str, &str)] = &[("gif", "format.gif"), ("apng", "format.apng")];

const CLICK_EFFECT_GROUPS: &[(ClickEffectGroup, &str)] = &[
    (ClickEffectGroup::Basic, "settings.click_effect.group.basic"),
    (
        ClickEffectGroup::Dynamic,
        "settings.click_effect.group.dynamic",
    ),
    (
        ClickEffectGroup::Particles,
        "settings.click_effect.group.particles",
    ),
    (ClickEffectGroup::Glow, "settings.click_effect.group.glow"),
];
const CLICK_PREVIEW_SIZE: f32 = 56.0;
const CLICK_PREVIEW_RESTORE_DELAY: Duration = Duration::from_millis(1200);
const WATERMARK_PREVIEW_FRAME_WIDTH: f32 = 512.0;
const WATERMARK_PREVIEW_FRAME_HEIGHT: f32 = 288.0;

const CLICK_EFFECT_LABELS: &[(ClickEffect, &str)] = &[
    (ClickEffect::Ring, "settings.click_effect.ring"),
    (ClickEffect::Dot, "settings.click_effect.dot"),
    (ClickEffect::Outline, "settings.click_effect.outline"),
    (ClickEffect::Cross, "settings.click_effect.cross"),
    (ClickEffect::Burst, "settings.click_effect.burst"),
    (ClickEffect::Ripple, "settings.click_effect.ripple"),
    (ClickEffect::Pulse, "settings.click_effect.pulse"),
    (ClickEffect::Sparkle, "settings.click_effect.sparkle"),
    (ClickEffect::Firework, "settings.click_effect.firework"),
    (ClickEffect::Comet, "settings.click_effect.comet"),
    (ClickEffect::Confetti, "settings.click_effect.confetti"),
    (ClickEffect::Bloom, "settings.click_effect.bloom"),
    (ClickEffect::Aurora, "settings.click_effect.aurora"),
    (ClickEffect::Prism, "settings.click_effect.prism"),
    (ClickEffect::Diamond, "settings.click_effect.diamond"),
    (ClickEffect::Corners, "settings.click_effect.corners"),
    (ClickEffect::Orbit, "settings.click_effect.orbit"),
    (ClickEffect::Radar, "settings.click_effect.radar"),
    (ClickEffect::Stardust, "settings.click_effect.stardust"),
    (ClickEffect::Rocket, "settings.click_effect.rocket"),
    (ClickEffect::Nebula, "settings.click_effect.nebula"),
    (ClickEffect::Rainbow, "settings.click_effect.rainbow"),
];

fn click_effect_label(effect: ClickEffect) -> String {
    let (_, key) = CLICK_EFFECT_LABELS
        .iter()
        .find(|(candidate, _)| *candidate == effect)
        .expect("every click effect has a label");
    t!(*key).to_string()
}

const OCR_LANGUAGES: &[(&str, &str)] = &[
    ("auto", "settings.ocr_language.auto"),
    ("zh-Hans", "settings.ocr_language.zh_hans"),
    ("zh-Hant", "settings.ocr_language.zh_hant"),
    ("en-US", "settings.ocr_language.en"),
    ("ja-JP", "settings.ocr_language.ja"),
    ("ko-KR", "settings.ocr_language.ko"),
    ("fr-FR", "settings.ocr_language.fr"),
    ("de-DE", "settings.ocr_language.de"),
];

fn language_label(value: &str) -> String {
    match value {
        "zh-CN" => t!("settings.language.zh").to_string(),
        "en" => t!("settings.language.en").to_string(),
        _ => t!("settings.language.auto").to_string(),
    }
}

/// OCR language codes in priority order for a setting value.
pub fn ocr_languages(value: &str) -> Vec<String> {
    if value == "auto" {
        Vec::new()
    } else {
        vec![value.to_string()]
    }
}

struct WatermarkPreview {
    target: WatermarkTarget,
    preset: WatermarkPreset,
    result: std::result::Result<Arc<RenderImage>, String>,
}

struct WatermarkTemplateSelect {
    state: Entity<SelectState<SearchableVec<crate::watermark_editor::WatermarkChoice>>>,
    _subscription: Subscription,
}

pub struct SettingsView {
    hotkey_focus: [FocusHandle; HotkeyAction::COUNT],
    /// The action whose field is waiting for keys.
    recording: Option<HotkeyAction>,
    /// Modifiers held right now, shown live while recording.
    held_modifiers: Modifiers,
    hotkey_errors: [Option<SharedString>; HotkeyAction::COUNT],
    click_effect_group: ClickEffectGroup,
    click_preview_trigger: Option<(ClickEffect, u64, Point<Pixels>)>,
    watermark_target_tab: WatermarkTarget,
    watermark_preview: [Option<WatermarkPreview>; 3],
    _subscriptions: Vec<Subscription>,
}

impl SettingsView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let hotkey_focus = HotkeyAction::ALL.map(|_| cx.focus_handle());
        let mut subscriptions = vec![
            cx.observe_global::<AppSettings>(|_, cx| cx.notify()),
            cx.observe(&HistoryStore::global(cx), |_, _, cx| cx.notify()),
            appearance::follow_system_theme(window),
        ];

        // Keys must be caught before the keymap so ⌘Q, ⌘, and friends do not fire
        // while a combination is being recorded.
        let view = cx.entity();
        subscriptions.push(cx.intercept_keystrokes({
            let view = view.clone();
            move |event, window, cx| {
                let (action, focused) = {
                    let this = view.read(cx);
                    let Some(action) = this.recording else {
                        return;
                    };
                    (
                        action,
                        this.hotkey_focus[action as usize].is_focused(window),
                    )
                };
                if !focused || event.keystroke.is_modifier_key() {
                    return;
                }
                let stroke = event.keystroke.clone();
                view.update(cx, |view, cx| view.record(action, &stroke, cx));
                cx.stop_propagation();
            }
        }));
        subscriptions.push(cx.observe_release(&view, |view, _, cx| {
            // Closing the window mid-recording must not leave hotkeys suspended.
            if view.recording.take().is_some() {
                hotkey::resume(cx);
            }
        }));
        for (ix, focus) in hotkey_focus.iter().enumerate() {
            subscriptions.push(window.on_focus_out(focus, cx, {
                let view = view.clone();
                move |_, _, cx| {
                    view.update(cx, |view, cx| {
                        view.stop_recording(HotkeyAction::ALL[ix], cx)
                    })
                }
            }));
        }

        let click_effect =
            ClickEffect::parse(&AppSettings::get(cx).recording.click_effect).unwrap_or_default();
        Self {
            hotkey_focus,
            recording: None,
            held_modifiers: Modifiers::none(),
            hotkey_errors: Default::default(),
            click_effect_group: click_effect.group(),
            click_preview_trigger: None,
            watermark_target_tab: WatermarkTarget::Screenshot,
            watermark_preview: std::array::from_fn(|_| None),
            _subscriptions: subscriptions,
        }
    }

    fn toggle_recording(
        &mut self,
        action: HotkeyAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.recording == Some(action) {
            self.stop_recording(action, cx);
            return;
        }
        self.recording = Some(action);
        self.held_modifiers = Modifiers::none();
        self.hotkey_errors[action as usize] = None;
        hotkey::suspend(cx);
        self.hotkey_focus[action as usize].focus(window, cx);
        cx.notify();
    }

    fn stop_recording(&mut self, action: HotkeyAction, cx: &mut Context<Self>) {
        if self.recording != Some(action) {
            return;
        }
        self.recording = None;
        self.held_modifiers = Modifiers::none();
        hotkey::resume(cx);
        cx.notify();
    }

    fn held_modifiers_changed(
        &mut self,
        action: HotkeyAction,
        modifiers: Modifiers,
        cx: &mut Context<Self>,
    ) {
        if self.recording != Some(action) {
            return;
        }
        self.held_modifiers = modifiers;
        cx.notify();
    }

    fn record(&mut self, action: HotkeyAction, stroke: &Keystroke, cx: &mut Context<Self>) {
        let plain = !stroke.modifiers.modified();
        if plain && stroke.key == "escape" {
            self.stop_recording(action, cx);
            return;
        }
        if plain && matches!(stroke.key.as_str(), "backspace" | "delete") {
            self.commit(action, String::new(), cx);
            return;
        }
        match hotkey::from_keystroke(stroke) {
            Ok(binding) => self.commit(action, binding, cx),
            Err(err) => {
                self.hotkey_errors[action as usize] = Some(err.into());
                self.held_modifiers = stroke.modifiers;
                cx.notify();
            }
        }
    }

    fn commit(&mut self, action: HotkeyAction, binding: String, cx: &mut Context<Self>) {
        let ix = action as usize;
        let mut candidate = AppSettings::get(cx).hotkeys.clone();
        action.set_binding(&mut candidate, binding);
        match hotkey::check(action, &candidate) {
            Ok(_) => {
                self.hotkey_errors[ix] = None;
                self.recording = None;
                self.held_modifiers = Modifiers::none();
                AppSettings::update(cx, |config| config.hotkeys = candidate);
                hotkey::resume(cx);
                cx.notify();
            }
            Err(err) => {
                self.hotkey_errors[ix] = Some(err.into());
                cx.notify();
            }
        }
    }

    fn hotkey_description(&self, action: HotkeyAction, cx: &App) -> String {
        let ix = action as usize;
        if let Some(err) = &self.hotkey_errors[ix] {
            return format!("⚠ {err}");
        }
        if self.recording == Some(action) {
            return if cfg!(target_os = "macos") {
                t!("settings.hotkey.record_hint.macos").to_string()
            } else {
                t!("settings.hotkey.record_hint.other").to_string()
            };
        }
        if let Some(reason) = hotkey::unavailable_reason(cx) {
            return format!("⚠ {reason}");
        }
        if let Some(err) = hotkey::error(action, cx) {
            return format!("⚠ {err}");
        }
        if action
            .binding(&AppSettings::get(cx).hotkeys)
            .trim()
            .is_empty()
        {
            return t!("settings.hotkey.disabled").to_string();
        }
        t!("settings.hotkey.idle").to_string()
    }

    fn general_page(&self) -> SettingPage {
        let defaults = Config::default();
        SettingPage::new(t!("settings.page.general").to_string())
            .icon(Icon::new(IconName::Settings2))
            .default_open(true)
            .resettable(true)
            .group(
                SettingGroup::new()
                    .title(t!("settings.group.appearance").to_string())
                    .items([
                        SettingItem::new(
                            t!("settings.language").to_string(),
                            SettingField::dropdown(
                                i18n::LANGUAGE_VALUES
                                    .into_iter()
                                    .map(|value| (value.into(), language_label(value).into()))
                                    .collect(),
                                |cx: &App| AppSettings::get(cx).general.language.clone().into(),
                                |value: SharedString, cx: &mut App| {
                                    AppSettings::update(cx, |config| {
                                        config.general.language = value.to_string()
                                    });
                                },
                            )
                            .default_value(defaults.general.language.clone()),
                        ),
                        SettingItem::new(
                            t!("settings.theme").to_string(),
                            SettingField::dropdown(
                                vec![
                                    (
                                        "system".into(),
                                        t!("settings.theme.system").to_string().into(),
                                    ),
                                    (
                                        "light".into(),
                                        t!("settings.theme.light").to_string().into(),
                                    ),
                                    ("dark".into(), t!("settings.theme.dark").to_string().into()),
                                ],
                                |cx: &App| AppSettings::get(cx).general.theme.as_str().into(),
                                |value: SharedString, cx: &mut App| {
                                    if let Some(theme) = ThemePreference::parse(&value) {
                                        AppSettings::update(cx, |config| {
                                            config.general.theme = theme
                                        });
                                    }
                                },
                            )
                            .default_value(defaults.general.theme.as_str()),
                        ),
                    ]),
            )
            .group(
                SettingGroup::new()
                    .title(t!("settings.group.launch").to_string())
                    .items([SettingItem::new(
                        t!("settings.launch_at_login").to_string(),
                        SettingField::switch(
                            |cx: &App| AppSettings::get(cx).general.launch_at_login,
                            |value: bool, cx: &mut App| {
                                AppSettings::update(cx, |config| {
                                    config.general.launch_at_login = value
                                })
                            },
                        )
                        .default_value(defaults.general.launch_at_login),
                    )
                    .description(t!("settings.launch_at_login.desc").to_string())]),
            )
            .group(
                SettingGroup::new()
                    .title(t!("settings.group.updates").to_string())
                    .items([SettingItem::new(
                        t!("settings.check_updates").to_string(),
                        SettingField::switch(
                            |cx: &App| AppSettings::get(cx).update.check,
                            |value: bool, cx: &mut App| {
                                AppSettings::update(cx, |config| config.update.check = value)
                            },
                        )
                        .default_value(defaults.update.check),
                    )
                    .description(t!("settings.check_updates.desc").to_string())]),
            )
    }

    fn hotkey_page(&self, view: &Entity<Self>, cx: &App) -> SettingPage {
        let items = HotkeyAction::ALL.map(|action| {
            let field_view = view.clone();
            SettingItem::new(
                action.label(),
                SettingField::render(move |options, _, cx| {
                    hotkey_recorder(&field_view, action, options, cx)
                })
                .on_reset(
                    move |cx: &App| {
                        let default = Hotkeys::default();
                        action.binding(&AppSettings::get(cx).hotkeys) != action.binding(&default)
                    },
                    move |_, cx: &mut App| {
                        let default = Hotkeys::default();
                        let mut next = AppSettings::get(cx).hotkeys.clone();
                        action.set_binding(&mut next, action.binding(&default).to_string());
                        AppSettings::update(cx, |config| config.hotkeys = next);
                    },
                ),
            )
            .description(self.hotkey_description(action, cx))
        });
        SettingPage::new(t!("settings.page.hotkeys").to_string())
            .icon(Icon::new(LucideIcon::Keyboard))
            .group(
                SettingGroup::new()
                    .title(t!("settings.group.global_hotkeys").to_string())
                    .description(t!("settings.group.global_hotkeys.desc").to_string())
                    .items(items),
            )
    }

    fn capture_page(&self) -> SettingPage {
        let defaults = Config::default();
        SettingPage::new(t!("settings.page.capture").to_string())
            .icon(Icon::new(IconName::Frame))
            .group(
                SettingGroup::new()
                    .title(t!("settings.group.save").to_string())
                    .items([
                        SettingItem::new(
                            t!("settings.save_dir").to_string(),
                            SettingField::render(|options, _, cx| {
                                let custom = AppSettings::get(cx).capture.save_dir.is_some();
                                h_flex()
                                    .gap_2()
                                    .child(
                                        div()
                                            .max_w(px(260.))
                                            .truncate()
                                            .text_sm()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(controller::save_dir(cx).display().to_string()),
                                    )
                                    .child(
                                        Button::new("choose-save-dir")
                                            .outline()
                                            .with_size(options.size())
                                            .label(t!("settings.save_dir.choose").to_string())
                                            .on_click(|_, _, cx| choose_save_dir(cx)),
                                    )
                                    .when(custom, |this| {
                                        this.child(
                                            Button::new("reset-save-dir")
                                                .ghost()
                                                .with_size(options.size())
                                                .label(t!("settings.save_dir.reset").to_string())
                                                .on_click(|_, _, cx| {
                                                    AppSettings::update(cx, |config| {
                                                        config.capture.save_dir = None
                                                    })
                                                }),
                                        )
                                    })
                            })
                            .on_reset(
                                |cx: &App| AppSettings::get(cx).capture.save_dir.is_some(),
                                |_, cx: &mut App| {
                                    AppSettings::update(cx, |config| config.capture.save_dir = None)
                                },
                            ),
                        )
                        .layout(Axis::Vertical),
                        SettingItem::new(
                            t!("settings.image_format").to_string(),
                            SettingField::dropdown(
                                vec![("png".into(), "PNG".into()), ("jpg".into(), "JPG".into())],
                                |cx: &App| {
                                    AppSettings::get(cx).capture.image_format.extension().into()
                                },
                                |value: SharedString, cx: &mut App| {
                                    if let Some(format) = ImageFormat::parse(&value) {
                                        AppSettings::update(cx, |config| {
                                            config.capture.image_format = format
                                        });
                                    }
                                },
                            )
                            .default_value(defaults.capture.image_format.extension()),
                        )
                        .description(t!("settings.image_format.desc").to_string()),
                        SettingItem::new(
                            t!("settings.jpeg_quality").to_string(),
                            SettingField::number_input(
                                NumberFieldOptions {
                                    min: 1.,
                                    max: 100.,
                                    step: 5.,
                                },
                                |cx: &App| AppSettings::get(cx).capture.jpeg_quality as f64,
                                |value: f64, cx: &mut App| {
                                    AppSettings::update(cx, |config| {
                                        config.capture.jpeg_quality = value.round() as u8
                                    })
                                },
                            )
                            .w_48()
                            .default_value(defaults.capture.jpeg_quality as f64),
                        ),
                    ]),
            )
            .group(
                SettingGroup::new()
                    .title(t!("settings.group.selection").to_string())
                    .items([
                        SettingItem::new(
                            t!("settings.window_snap").to_string(),
                            SettingField::switch(
                                |cx: &App| AppSettings::get(cx).capture.window_snap,
                                |value: bool, cx: &mut App| {
                                    AppSettings::update(cx, |config| {
                                        config.capture.window_snap = value
                                    })
                                },
                            )
                            .default_value(defaults.capture.window_snap),
                        )
                        .description(t!("settings.window_snap.desc").to_string()),
                        SettingItem::new(
                            t!("settings.element_snap").to_string(),
                            SettingField::switch(
                                |cx: &App| AppSettings::get(cx).capture.element_snap,
                                |value: bool, cx: &mut App| {
                                    AppSettings::update(cx, |config| {
                                        config.capture.element_snap = value
                                    })
                                },
                            )
                            .default_value(defaults.capture.element_snap),
                        )
                        .description(t!("settings.element_snap.desc").to_string()),
                        SettingItem::new(
                            t!("settings.mask_opacity").to_string(),
                            SettingField::number_input(
                                NumberFieldOptions {
                                    min: 0.,
                                    max: 100.,
                                    step: 5.,
                                },
                                |cx: &App| AppSettings::get(cx).capture.mask_opacity as f64,
                                |value: f64, cx: &mut App| {
                                    AppSettings::update(cx, |config| {
                                        config.capture.mask_opacity = value.round() as u8
                                    })
                                },
                            )
                            .w_48()
                            .default_value(defaults.capture.mask_opacity as f64),
                        )
                        .description(t!("settings.mask_opacity.desc").to_string()),
                        SettingItem::new(
                            t!("settings.selection_shadow").to_string(),
                            SettingField::switch(
                                |cx: &App| AppSettings::get(cx).capture.selection_shadow,
                                |value: bool, cx: &mut App| {
                                    AppSettings::update(cx, |config| {
                                        config.capture.selection_shadow = value
                                    })
                                },
                            )
                            .default_value(defaults.capture.selection_shadow),
                        )
                        .description(t!("settings.selection_shadow.desc").to_string()),
                    ]),
            )
            .group(
                SettingGroup::new()
                    .title(t!("settings.group.scroll").to_string())
                    .items([
                        SettingItem::new(
                            t!("settings.scroll.max_height").to_string(),
                            SettingField::number_input(
                                NumberFieldOptions {
                                    min: ScrollConfig::HEIGHT_RANGE.0 as f64,
                                    max: ScrollConfig::HEIGHT_RANGE.1 as f64,
                                    step: 2000.,
                                },
                                |cx: &App| AppSettings::get(cx).scroll.max_height as f64,
                                |value: f64, cx: &mut App| {
                                    AppSettings::update(cx, |config| {
                                        config.scroll.max_height = value.round() as u32
                                    })
                                },
                            )
                            .w_48()
                            .default_value(defaults.scroll.max_height as f64),
                        )
                        .description(t!("settings.scroll.max_height.desc").to_string()),
                        SettingItem::new(
                            t!("settings.scroll.auto_speed").to_string(),
                            SettingField::dropdown(
                                vec![
                                    (
                                        "slow".into(),
                                        t!("settings.scroll.speed.slow").to_string().into(),
                                    ),
                                    (
                                        "medium".into(),
                                        t!("settings.scroll.speed.medium").to_string().into(),
                                    ),
                                    (
                                        "fast".into(),
                                        t!("settings.scroll.speed.fast").to_string().into(),
                                    ),
                                ],
                                |cx: &App| {
                                    AppSettings::get(cx)
                                        .scroll
                                        .auto_scroll_speed
                                        .as_str()
                                        .into()
                                },
                                |value: SharedString, cx: &mut App| {
                                    if let Some(speed) = ScrollSpeed::parse(&value) {
                                        AppSettings::update(cx, |config| {
                                            config.scroll.auto_scroll_speed = speed
                                        });
                                    }
                                },
                            )
                            .default_value(defaults.scroll.auto_scroll_speed.as_str()),
                        )
                        .description(t!("settings.scroll.auto_speed.desc").to_string()),
                        SettingItem::new(
                            t!("settings.scroll.show_preview").to_string(),
                            SettingField::switch(
                                |cx: &App| AppSettings::get(cx).scroll.show_preview,
                                |value: bool, cx: &mut App| {
                                    AppSettings::update(cx, |config| {
                                        config.scroll.show_preview = value
                                    })
                                },
                            )
                            .default_value(defaults.scroll.show_preview),
                        )
                        .description(t!("settings.scroll.show_preview.desc").to_string()),
                    ]),
            )
    }

    fn watermark_page(&mut self, view: &Entity<Self>, cx: &mut Context<Self>) -> SettingPage {
        let watermark_defaults = WatermarkConfig::default();
        let config = AppSettings::get(cx).watermark.clone();
        let page = SettingPage::new(t!("settings.page.watermark").to_string())
            .icon(Icon::new(LucideIcon::Stamp))
            .group(
                SettingGroup::new()
                    .title(t!("settings.watermark.group.general").to_string())
                    .items([
                        SettingItem::new(
                            t!("settings.watermark.enabled").to_string(),
                            SettingField::switch(
                                |cx: &App| AppSettings::get(cx).watermark.enabled,
                                |value: bool, cx: &mut App| {
                                    AppSettings::update(cx, |config| {
                                        config.watermark.enabled = value
                                    })
                                },
                            )
                            .default_value(watermark_defaults.enabled),
                        )
                        .description(t!("settings.watermark.enabled.desc").to_string()),
                        SettingItem::new(
                            t!("settings.watermark.shared").to_string(),
                            SettingField::switch(
                                |cx: &App| AppSettings::get(cx).watermark.shared,
                                |value: bool, cx: &mut App| {
                                    AppSettings::update(cx, |config| {
                                        config.watermark.set_shared(value)
                                    })
                                },
                            )
                            .default_value(watermark_defaults.shared),
                        )
                        .description(t!("settings.watermark.shared.desc").to_string()),
                    ]),
            )
            .group(
                SettingGroup::new()
                    .title(t!("settings.watermark.group.scope").to_string())
                    .description(t!("settings.watermark.group.scope.desc").to_string())
                    .items([
                        SettingItem::new(
                            t!("settings.watermark.screenshots").to_string(),
                            SettingField::switch(
                                |cx: &App| {
                                    AppSettings::get(cx)
                                        .watermark
                                        .target_enabled(WatermarkTarget::Screenshot)
                                },
                                |value: bool, cx: &mut App| {
                                    AppSettings::update(cx, |config| {
                                        config
                                            .watermark
                                            .set_target_enabled(WatermarkTarget::Screenshot, value)
                                    })
                                },
                            )
                            .default_value(watermark_defaults.apply_to_screenshots),
                        )
                        .description(t!("settings.watermark.screenshots.desc").to_string())
                        .disabled(!config.enabled),
                        SettingItem::new(
                            t!("settings.watermark.animations").to_string(),
                            SettingField::switch(
                                |cx: &App| {
                                    AppSettings::get(cx)
                                        .watermark
                                        .target_enabled(WatermarkTarget::Animation)
                                },
                                |value: bool, cx: &mut App| {
                                    AppSettings::update(cx, |config| {
                                        config
                                            .watermark
                                            .set_target_enabled(WatermarkTarget::Animation, value)
                                    })
                                },
                            )
                            .default_value(watermark_defaults.apply_to_animations),
                        )
                        .description(t!("settings.watermark.animations.desc").to_string())
                        .disabled(!config.enabled),
                        SettingItem::new(
                            t!("settings.watermark.videos").to_string(),
                            SettingField::switch(
                                |cx: &App| {
                                    AppSettings::get(cx)
                                        .watermark
                                        .target_enabled(WatermarkTarget::Video)
                                },
                                |value: bool, cx: &mut App| {
                                    AppSettings::update(cx, |config| {
                                        config
                                            .watermark
                                            .set_target_enabled(WatermarkTarget::Video, value)
                                    })
                                },
                            )
                            .default_value(watermark_defaults.apply_to_videos),
                        )
                        .description(t!("settings.watermark.videos.desc").to_string())
                        .disabled(!config.enabled),
                    ]),
            );
        if !config.enabled || !config.has_active_target() {
            return page.group(
                SettingGroup::new()
                    .title(t!("settings.watermark.group.preview").to_string())
                    .item(SettingItem::render(watermark_preview_inactive_setting)),
            );
        }
        let previews: [std::result::Result<Arc<RenderImage>, String>; 3] =
            std::array::from_fn(|index| {
                self.watermark_preview_image(WatermarkTarget::ALL[index], cx)
            });
        let template_names: Vec<SharedString> = config
            .templates
            .values()
            .map(|template| template.name.clone().into())
            .collect();
        let mut keywords = watermark_editor_keywords();
        keywords.extend(template_names);
        if config.shared {
            let group = SettingGroup::new()
                .title(t!("settings.watermark.group.presets").to_string())
                .description(t!("settings.watermark.group.presets.desc").to_string());
            let group = add_watermark_target_items(
                group,
                WatermarkTarget::Screenshot,
                &config,
                previews[watermark_target_index(WatermarkTarget::Screenshot)].clone(),
                keywords,
            );
            page.group(group)
        } else {
            let active_target = self.watermark_target_tab;
            let tab_view = view.clone();
            let group = SettingGroup::new()
                .title(t!("settings.watermark.group.presets").to_string())
                .description(t!("settings.watermark.group.presets.desc").to_string())
                .item(SettingItem::render(move |_, _, cx| {
                    let watermark = &AppSettings::get(cx).watermark;
                    let selected_ix = watermark_target_index(active_target);
                    let tab_view = tab_view.clone();
                    TabBar::new("watermark-target-tabs")
                        .segmented()
                        .w_full()
                        .selected_index(selected_ix)
                        .children(WatermarkTarget::ALL.into_iter().map(|target| {
                            let enabled = watermark.target_enabled(target);
                            Tab::new().label(watermark_target_label(target)).suffix(
                                div().size(px(6.)).rounded_full().bg(if enabled {
                                    cx.theme().primary
                                } else {
                                    cx.theme().muted_foreground.opacity(0.35)
                                }),
                            )
                        }))
                        .on_click(move |ix, _, cx| {
                            if let Some(&target) = WatermarkTarget::ALL.get(*ix) {
                                tab_view.update(cx, |this, cx| {
                                    this.watermark_target_tab = target;
                                    cx.notify();
                                });
                            }
                        })
                }));
            let group = add_watermark_target_items(
                group,
                active_target,
                &config,
                previews[watermark_target_index(active_target)].clone(),
                keywords,
            );
            page.group(group)
        }
    }
}

impl SettingsView {
    fn watermark_preview_image(
        &mut self,
        target: WatermarkTarget,
        cx: &App,
    ) -> std::result::Result<Arc<RenderImage>, String> {
        let target_index = watermark_target_index(target);
        let mut preset = AppSettings::get(cx)
            .watermark
            .preset(target)
            .cloned()
            .ok_or_else(|| "watermark template is unavailable".to_string())?;
        preset.sanitize();
        if let Some(cached) = &self.watermark_preview[target_index]
            && cached.target == target
            && cached.preset == preset
        {
            return cached.result.clone();
        }
        let result = build_watermark_preview(target, &preset, AppSettings::watermark_dir(cx));
        self.watermark_preview[target_index] = Some(WatermarkPreview {
            target,
            preset,
            result: result.clone(),
        });
        result
    }

    fn recording_page(&self, view: &Entity<SettingsView>) -> SettingPage {
        let defaults = RecordingConfig::default();
        let (min_fps, max_fps) = RecordingConfig::FPS_RANGE;
        let (min_secs, max_secs) = RecordingConfig::DURATION_RANGE;
        SettingPage::new(t!("settings.page.recording").to_string())
            .icon(Icon::new(IconName::Play))
            .group(
                SettingGroup::new()
                    .title(t!("settings.group.recording").to_string())
                    .items([
                        SettingItem::new(
                            t!("settings.animation_format").to_string(),
                            SettingField::dropdown(
                                ANIMATION_FORMATS
                                    .iter()
                                    .map(|(value, key)| {
                                        ((*value).into(), t!(*key).to_string().into())
                                    })
                                    .collect(),
                                |cx: &App| {
                                    AppSettings::get(cx)
                                        .recording
                                        .animation_format
                                        .clone()
                                        .into()
                                },
                                |value: SharedString, cx: &mut App| {
                                    AppSettings::update(cx, |config| {
                                        config.recording.animation_format = value.to_string()
                                    })
                                },
                            )
                            .default_value("gif"),
                        )
                        .description(t!("settings.animation_format.desc").to_string()),
                        SettingItem::new(
                            t!("settings.fps.gif").to_string(),
                            SettingField::number_input(
                                NumberFieldOptions {
                                    min: min_fps as f64,
                                    max: max_fps as f64,
                                    step: 1.,
                                },
                                |cx: &App| AppSettings::get(cx).recording.gif_fps as f64,
                                |value: f64, cx: &mut App| {
                                    AppSettings::update(cx, |config| {
                                        config.recording.gif_fps = value.round() as u32
                                    })
                                },
                            )
                            .w_48()
                            .default_value(defaults.gif_fps as f64),
                        )
                        .description(t!("settings.fps.gif.desc").to_string()),
                        SettingItem::new(
                            t!("settings.fps.mp4").to_string(),
                            SettingField::number_input(
                                NumberFieldOptions {
                                    min: min_fps as f64,
                                    max: max_fps as f64,
                                    step: 1.,
                                },
                                |cx: &App| AppSettings::get(cx).recording.mp4_fps as f64,
                                |value: f64, cx: &mut App| {
                                    AppSettings::update(cx, |config| {
                                        config.recording.mp4_fps = value.round() as u32
                                    })
                                },
                            )
                            .w_48()
                            .default_value(defaults.mp4_fps as f64),
                        )
                        .description(t!("settings.fps.mp4.desc").to_string()),
                        SettingItem::new(
                            t!("settings.mp4_bitrate").to_string(),
                            SettingField::number_input(
                                NumberFieldOptions {
                                    min: 0.,
                                    max: u32::MAX as f64,
                                    step: 1.,
                                },
                                |cx: &App| {
                                    AppSettings::get(cx).recording.mp4_bitrate_kbps.unwrap_or(0)
                                        as f64
                                },
                                |value: f64, cx: &mut App| {
                                    AppSettings::update(cx, |config| {
                                        config.recording.mp4_bitrate_kbps =
                                            Some(value.round() as u32);
                                    });
                                },
                            )
                            .w_48()
                            .default_value(0.),
                        )
                        .description(t!("settings.mp4_bitrate.desc").to_string()),
                        SettingItem::new(
                            t!("settings.max_duration").to_string(),
                            SettingField::number_input(
                                NumberFieldOptions {
                                    min: min_secs as f64,
                                    max: max_secs as f64,
                                    step: 5.,
                                },
                                |cx: &App| AppSettings::get(cx).recording.max_duration_secs as f64,
                                |value: f64, cx: &mut App| {
                                    AppSettings::update(cx, |config| {
                                        config.recording.max_duration_secs = value.round() as u32
                                    })
                                },
                            )
                            .w_48()
                            .default_value(defaults.max_duration_secs as f64),
                        )
                        .description(t!("settings.max_duration.desc").to_string()),
                        SettingItem::new(
                            t!("settings.capture_cursor").to_string(),
                            SettingField::switch(
                                |cx: &App| AppSettings::get(cx).recording.captures_cursor,
                                |value: bool, cx: &mut App| {
                                    AppSettings::update(cx, |config| {
                                        config.recording.captures_cursor = value
                                    })
                                },
                            )
                            .default_value(defaults.captures_cursor),
                        )
                        .description(t!("settings.capture_cursor.desc").to_string()),
                        SettingItem::render({
                            let view = view.clone();
                            move |options, _, cx| click_effect_setting(&view, options, cx)
                        })
                        .on_reset(
                            |cx: &App| {
                                let recording = &AppSettings::get(cx).recording;
                                !recording.click_effects || recording.click_effect != "ring"
                            },
                            |_, cx: &mut App| {
                                AppSettings::update(cx, |config| {
                                    config.recording.click_effects = true;
                                    config.recording.click_effect = "ring".to_string();
                                })
                            },
                        )
                        .keywords([
                            t!("settings.click_effects").to_string(),
                            t!("settings.click_effect").to_string(),
                            t!("settings.click_effects.desc").to_string(),
                        ]),
                        SettingItem::new(
                            t!("settings.recording_hdr").to_string(),
                            SettingField::switch(
                                |cx: &App| AppSettings::get(cx).recording.hdr,
                                |value: bool, cx: &mut App| {
                                    AppSettings::update(cx, |config| config.recording.hdr = value)
                                },
                            )
                            .default_value(defaults.hdr),
                        )
                        .description(t!("settings.recording_hdr.desc").to_string()),
                    ]),
            )
    }

    fn ocr_page(&self) -> SettingPage {
        SettingPage::new(t!("settings.page.ocr").to_string())
            .icon(Icon::new(LucideIcon::Languages))
            .group(
                SettingGroup::new()
                    .title(t!("settings.group.recognize").to_string())
                    .item(
                        SettingItem::new(
                            t!("settings.ocr_language").to_string(),
                            SettingField::dropdown(
                                OCR_LANGUAGES
                                    .iter()
                                    .map(|(value, key)| {
                                        ((*value).into(), t!(*key).to_string().into())
                                    })
                                    .collect(),
                                |cx: &App| AppSettings::get(cx).ocr.language.clone().into(),
                                |value: SharedString, cx: &mut App| {
                                    AppSettings::update(cx, |config| {
                                        config.ocr.language = value.to_string()
                                    })
                                },
                            )
                            .default_value("auto"),
                        )
                        .description(if cfg!(target_os = "macos") {
                            t!("settings.ocr.engine.macos").to_string()
                        } else {
                            t!("settings.ocr.engine.ocrs").to_string()
                        }),
                    ),
            )
    }

    fn history_page(&self) -> SettingPage {
        let defaults = HistoryConfig::default();
        SettingPage::new(t!("settings.page.history").to_string())
            .icon(Icon::new(LucideIcon::Images))
            .group(
                SettingGroup::new()
                    .title(t!("settings.group.autosave").to_string())
                    .description(t!("settings.group.autosave.desc").to_string())
                    .items([
                        SettingItem::new(
                            t!("settings.history.enabled").to_string(),
                            SettingField::switch(
                                |cx: &App| AppSettings::get(cx).history.enabled,
                                |value: bool, cx: &mut App| {
                                    AppSettings::update(cx, |config| config.history.enabled = value)
                                },
                            )
                            .default_value(defaults.enabled),
                        ),
                        SettingItem::new(
                            t!("settings.history.max_items").to_string(),
                            SettingField::number_input(
                                NumberFieldOptions {
                                    min: 0.,
                                    max: 100_000.,
                                    step: 10.,
                                },
                                |cx: &App| AppSettings::get(cx).history.max_items as f64,
                                |value: f64, cx: &mut App| {
                                    AppSettings::update(cx, |config| {
                                        config.history.max_items = value.round() as usize
                                    })
                                },
                            )
                            .w_48()
                            .default_value(defaults.max_items as f64),
                        )
                        .description(t!("settings.history.max_items.desc").to_string()),
                        SettingItem::new(
                            t!("settings.history.view").to_string(),
                            SettingField::render(|options, _, cx| {
                                let store = HistoryStore::global(cx).read(cx);
                                let summary = t!(
                                    "settings.history.summary",
                                    count = store.len(),
                                    size = format_bytes(store.total_bytes())
                                )
                                .to_string();
                                h_flex()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(summary),
                                    )
                                    .child(
                                        Button::new("open-history")
                                            .outline()
                                            .with_size(options.size())
                                            .label(t!("settings.history.open").to_string())
                                            .on_click(|_, _, cx| {
                                                Controller::global(cx).update(
                                                    cx,
                                                    |controller, cx| {
                                                        controller.open_panel(Panel::History, cx)
                                                    },
                                                )
                                            }),
                                    )
                            }),
                        ),
                    ]),
            )
    }

    fn about_page(&self) -> SettingPage {
        let mut application = SettingGroup::new()
            .title(t!("settings.group.application").to_string())
            .item(SettingItem::new(
                t!("settings.version").to_string(),
                SettingField::render(|_, _, cx| {
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(updater::CURRENT_VERSION.to_string())
                }),
            ));
        if let Some(url) = updater::repo_url() {
            application = application.item(
                SettingItem::new(
                    t!("settings.repository").to_string(),
                    SettingField::render(move |options, _, cx| {
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .max_w(px(320.))
                                    .truncate()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(url.clone()),
                            )
                            .child(
                                Button::new("open-repository")
                                    .outline()
                                    .with_size(options.size())
                                    .label(t!("settings.open").to_string())
                                    .on_click({
                                        let url = url.clone();
                                        move |_, _, cx| cx.open_url(&url)
                                    }),
                            )
                    }),
                )
                .layout(Axis::Vertical),
            );
        }

        SettingPage::new(t!("settings.page.about").to_string())
            .icon(Icon::new(IconName::Info))
            .group(application)
            .group(
                SettingGroup::new()
                    .title(t!("settings.group.files").to_string())
                    .item(
                        SettingItem::new(
                            t!("settings.config_file").to_string(),
                            SettingField::render(|options, _, cx| {
                                let path = AppSettings::config_path(cx).to_path_buf();
                                h_flex()
                                    .gap_2()
                                    .child(
                                        div()
                                            .max_w(px(320.))
                                            .truncate()
                                            .text_sm()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(path.display().to_string()),
                                    )
                                    .child(
                                        Button::new("reveal-config")
                                            .outline()
                                            .with_size(options.size())
                                            .label(t!("settings.reveal").to_string())
                                            .on_click(move |_, _, cx| cx.reveal_path(&path)),
                                    )
                            }),
                        )
                        .layout(Axis::Vertical),
                    ),
            )
    }
}

struct WatermarkTemplateNameState {
    input: Entity<InputState>,
    synced_id: Option<String>,
    synced_name: String,
    syncing: bool,
    error: Option<String>,
    _subscription: Subscription,
}

fn active_editable_template_id(
    watermark: &WatermarkConfig,
    target: WatermarkTarget,
) -> Option<String> {
    if watermark.shared {
        watermark.effective_template_id(target).map(str::to_string)
    } else {
        watermark.selected_template_id(target).map(str::to_string)
    }
}

fn add_watermark_target_items(
    mut group: SettingGroup,
    target: WatermarkTarget,
    config: &WatermarkConfig,
    preview: std::result::Result<Arc<RenderImage>, String>,
    keywords: Vec<SharedString>,
) -> SettingGroup {
    let target_label = watermark_target_label(target);
    let editable_id = active_editable_template_id(config, target);
    let inherited = editable_id.is_none();
    let effective_preset = config.preset(target).cloned().unwrap_or_default();
    let defaults = WatermarkPreset::default();

    group = group
        .item(
            SettingItem::render(move |_, window, cx| {
                watermark_preset_row(target, target_label.clone(), window, cx)
            })
            .keywords(keywords),
        )
        .item(
            SettingItem::new(
                t!("settings.watermark.template.name").to_string(),
                SettingField::render(move |options, window, cx| {
                    watermark_template_name_field(target, options, window, cx)
                }),
            )
            .disabled(inherited),
        )
        .item(
            SettingItem::new(
                t!("settings.watermark.placement").to_string(),
                SettingField::render(move |options, _, cx| {
                    watermark_placement_grid(target, inherited, options, cx)
                })
                .on_reset(
                    move |cx: &App| {
                        AppSettings::get(cx)
                            .watermark
                            .preset(target)
                            .is_some_and(|preset| preset.placement != defaults.placement)
                    },
                    move |_, cx: &mut App| {
                        AppSettings::update(cx, |config| {
                            if let Some(id) = active_editable_template_id(&config.watermark, target)
                                && let Some(template) = config.watermark.template_mut(&id)
                            {
                                template.preset.placement = defaults.placement;
                            }
                        });
                    },
                ),
            )
            .disabled(inherited),
        )
        .item(
            SettingItem::new(
                t!("settings.watermark.margin_x").to_string(),
                SettingField::number_input(
                    NumberFieldOptions {
                        min: 0.,
                        max: 2048.,
                        step: 4.,
                    },
                    move |cx: &App| {
                        AppSettings::get(cx)
                            .watermark
                            .preset(target)
                            .map_or(defaults.margin_x as f64, |preset| preset.margin_x as f64)
                    },
                    move |value: f64, cx: &mut App| {
                        AppSettings::update(cx, |config| {
                            if let Some(id) = active_editable_template_id(&config.watermark, target)
                                && let Some(template) = config.watermark.template_mut(&id)
                            {
                                template.preset.margin_x = value as f32;
                            }
                        });
                    },
                )
                .w_48()
                .default_value(defaults.margin_x as f64),
            )
            .disabled(inherited),
        )
        .item(
            SettingItem::new(
                t!("settings.watermark.margin_y").to_string(),
                SettingField::number_input(
                    NumberFieldOptions {
                        min: 0.,
                        max: 2048.,
                        step: 4.,
                    },
                    move |cx: &App| {
                        AppSettings::get(cx)
                            .watermark
                            .preset(target)
                            .map_or(defaults.margin_y as f64, |preset| preset.margin_y as f64)
                    },
                    move |value: f64, cx: &mut App| {
                        AppSettings::update(cx, |config| {
                            if let Some(id) = active_editable_template_id(&config.watermark, target)
                                && let Some(template) = config.watermark.template_mut(&id)
                            {
                                template.preset.margin_y = value as f32;
                            }
                        });
                    },
                )
                .w_48()
                .default_value(defaults.margin_y as f64),
            )
            .disabled(inherited),
        )
        .item(
            SettingItem::new(
                t!("settings.watermark.tiled").to_string(),
                SettingField::switch(
                    move |cx: &App| {
                        AppSettings::get(cx)
                            .watermark
                            .preset(target)
                            .is_some_and(|preset| preset.tiled)
                    },
                    move |value: bool, cx: &mut App| {
                        AppSettings::update(cx, |config| {
                            if let Some(id) = active_editable_template_id(&config.watermark, target)
                                && let Some(template) = config.watermark.template_mut(&id)
                            {
                                template.preset.tiled = value;
                            }
                        });
                    },
                )
                .default_value(defaults.tiled),
            )
            .disabled(inherited),
        );

    if effective_preset.tiled {
        group = group
            .item(
                SettingItem::new(
                    t!("settings.watermark.spacing_x").to_string(),
                    SettingField::number_input(
                        NumberFieldOptions {
                            min: 8.,
                            max: 4096.,
                            step: 8.,
                        },
                        move |cx: &App| {
                            AppSettings::get(cx)
                                .watermark
                                .preset(target)
                                .map_or(defaults.tile_spacing_x as f64, |preset| {
                                    preset.tile_spacing_x as f64
                                })
                        },
                        move |value: f64, cx: &mut App| {
                            AppSettings::update(cx, |config| {
                                if let Some(id) =
                                    active_editable_template_id(&config.watermark, target)
                                    && let Some(template) = config.watermark.template_mut(&id)
                                {
                                    template.preset.tile_spacing_x = value as f32;
                                }
                            });
                        },
                    )
                    .w_48()
                    .default_value(defaults.tile_spacing_x as f64),
                )
                .disabled(inherited),
            )
            .item(
                SettingItem::new(
                    t!("settings.watermark.spacing_y").to_string(),
                    SettingField::number_input(
                        NumberFieldOptions {
                            min: 8.,
                            max: 4096.,
                            step: 8.,
                        },
                        move |cx: &App| {
                            AppSettings::get(cx)
                                .watermark
                                .preset(target)
                                .map_or(defaults.tile_spacing_y as f64, |preset| {
                                    preset.tile_spacing_y as f64
                                })
                        },
                        move |value: f64, cx: &mut App| {
                            AppSettings::update(cx, |config| {
                                if let Some(id) =
                                    active_editable_template_id(&config.watermark, target)
                                    && let Some(template) = config.watermark.template_mut(&id)
                                {
                                    template.preset.tile_spacing_y = value as f32;
                                }
                            });
                        },
                    )
                    .w_48()
                    .default_value(defaults.tile_spacing_y as f64),
                )
                .disabled(inherited),
            );
    }

    group.item(SettingItem::render(move |options, window, cx| {
        watermark_preview_setting(target, preview.clone(), options, window, cx)
    }))
}

fn watermark_template_name_field(
    target: WatermarkTarget,
    options: &RenderOptions,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let watermark = &AppSettings::get(cx).watermark;
    let effective_id = watermark.effective_template_id(target).map(str::to_string);
    let current_name = effective_id
        .as_deref()
        .and_then(|id| watermark.templates.get(id))
        .map(|template| template.name.clone())
        .unwrap_or_default();

    let state = window.use_keyed_state(
        ElementId::from(SharedString::from(format!(
            "watermark-template-name-{}",
            target.as_str()
        ))),
        cx,
        {
            let current_name = current_name.clone();
            let effective_id = effective_id.clone();
            move |window, cx| {
                let input = cx.new(|cx| {
                    InputState::new(window, cx)
                        .default_value(current_name.clone())
                        .placeholder(t!("settings.watermark.template.name").to_string())
                });
                let subscription = cx.subscribe_in(&input, window, {
                    move |state: &mut WatermarkTemplateNameState,
                          input,
                          event: &InputEvent,
                          _,
                          cx| {
                        if state.syncing || !matches!(event, InputEvent::Change) {
                            return;
                        }
                        let raw = input.read(cx).value().to_string();
                        let trimmed = raw.trim();
                        let watermark = &AppSettings::get(cx).watermark;
                        let Some(editable_id) = active_editable_template_id(watermark, target)
                        else {
                            return;
                        };
                        if trimmed.is_empty() {
                            state.error =
                                Some(t!("settings.watermark.template.name_required").to_string());
                            cx.notify();
                            return;
                        }
                        if watermark.template_name_exists(trimmed, Some(&editable_id)) {
                            state.error = Some(
                                t!("settings.watermark.template.name_exists", name = trimmed)
                                    .to_string(),
                            );
                            cx.notify();
                            return;
                        }
                        state.error = None;
                        state.synced_name = trimmed.chars().take(64).collect();
                        let next_name = trimmed.to_string();
                        AppSettings::update(cx, |config| {
                            config.watermark.rename_template(&editable_id, next_name);
                        });
                    }
                });
                WatermarkTemplateNameState {
                    input,
                    synced_id: effective_id,
                    synced_name: current_name,
                    syncing: false,
                    error: None,
                    _subscription: subscription,
                }
            }
        },
    );

    state.update(cx, |state, cx| {
        if state.synced_id != effective_id
            || (state.error.is_none() && state.synced_name != current_name)
        {
            state.syncing = true;
            state.synced_id = effective_id.clone();
            state.synced_name = current_name.clone();
            state.error = None;
            state.input.update(cx, |input, cx| {
                input.set_value(current_name, window, cx);
            });
            state.syncing = false;
        }
    });

    let read_state = state.read(cx);
    let input = read_state.input.clone();
    let error = read_state.error.clone();
    v_flex()
        .w_56()
        .gap_1()
        .child(
            Input::new(&input)
                .w_full()
                .with_size(options.size())
                .disabled(options.is_disabled())
                .aria_label(t!("settings.watermark.template.name").to_string()),
        )
        .when_some(error, |this, err| {
            this.child(div().text_xs().text_color(cx.theme().danger).child(err))
        })
        .into_any_element()
}

fn watermark_preset_row(
    target: WatermarkTarget,
    target_label: SharedString,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let watermark = AppSettings::get(cx).watermark.clone();
    let choices = crate::watermark_editor::template_choices(&watermark, target);
    let selected = crate::watermark_editor::selection_value(&watermark, target);
    let selected_index = choices
        .iter()
        .position(|choice| choice.value == selected)
        .map(|index| IndexPath::default().row(index));
    let select = window.use_keyed_state(
        ElementId::from(SharedString::from(format!(
            "watermark-template-{}",
            target.as_str()
        ))),
        cx,
        {
            let choices = choices.clone();
            move |window, cx| {
                let state = cx.new(|cx| {
                    SelectState::new(SearchableVec::from(choices), selected_index, window, cx)
                        .searchable(true)
                });
                let subscription = cx.subscribe_in(
                    &state,
                    window,
                    move |_: &mut WatermarkTemplateSelect,
                          _,
                          event: &SelectEvent<
                        SearchableVec<crate::watermark_editor::WatermarkChoice>,
                    >,
                          _,
                          cx| {
                        let SelectEvent::Confirm(Some(value)) = event else {
                            return;
                        };
                        let id = (value != crate::watermark_editor::INHERIT_VALUE)
                            .then(|| value.clone());
                        AppSettings::update(cx, |config| {
                            config.watermark.select_template(target, id);
                        });
                    },
                );
                WatermarkTemplateSelect {
                    state,
                    _subscription: subscription,
                }
            }
        },
    );
    select.update(cx, |select, cx| {
        select.state.update(cx, |state, cx| {
            state.set_items(choices.into(), window, cx);
            if state.selected_value().map(String::as_str) != Some(selected.as_str()) {
                state.set_selected_value(&selected, window, cx);
            }
        });
    });
    let explicit_id = active_editable_template_id(&watermark, target);
    let editable_id = explicit_id.clone();
    let effective_id = watermark.effective_template_id(target).map(str::to_string);
    let effective_preset = watermark.preset(target).cloned().unwrap_or_default();
    let inherited_from_name = (!watermark.shared && explicit_id.is_none()).then(|| {
        effective_id
            .as_deref()
            .and_then(|id| watermark.templates.get(id))
            .map(|template| template.name.clone())
            .unwrap_or_else(|| "SnapIt".to_string())
    });
    let delete_id = explicit_id
        .as_ref()
        .filter(|id| id.as_str() != WatermarkConfig::DEFAULT_TEMPLATE_ID)
        .cloned();
    let delete_name = delete_id
        .as_ref()
        .and_then(|id| watermark.templates.get(id))
        .map(|template| template.name.clone())
        .unwrap_or_default();
    let duplicate_id = effective_id.clone();
    let shared_targets = (!watermark.shared)
        .then(|| {
            effective_id
                .as_deref()
                .map(|id| watermark.template_usages(id))
                .filter(|usages| usages.len() > 1)
                .map(|usages| {
                    usages
                        .into_iter()
                        .map(|used| watermark_target_label(used).to_string())
                        .collect::<Vec<_>>()
                        .join(" · ")
                })
        })
        .flatten();
    v_flex()
        .w_full()
        .gap_2()
        .child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .gap_3()
                .child(Label::new(target_label).text_sm())
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new(format!("watermark-new-{}", target.as_str()))
                                .small()
                                .label(t!("settings.watermark.template.new").to_string())
                                .dropdown_menu_with_anchor(
                                    Anchor::TopRight,
                                    move |mut menu, _, _| {
                                        for starter in
                                            crate::watermark_editor::WatermarkStarter::ALL
                                        {
                                            menu = menu.item(
                                                PopupMenuItem::new(starter.label()).on_click(
                                                    move |_, window, cx| {
                                                        crate::watermark_editor::open(
                                                            window,
                                                            cx,
                                                            target,
                                                            None,
                                                            starter.preset(),
                                                        )
                                                    },
                                                ),
                                            );
                                        }
                                        menu
                                    },
                                ),
                        )
                        .child(
                            Button::new(format!("watermark-edit-{}", target.as_str()))
                                .small()
                                .outline()
                                .label(t!("settings.watermark.template.edit").to_string())
                                .disabled(effective_id.is_none())
                                .on_click(move |_, window, cx| {
                                    crate::watermark_editor::open(
                                        window,
                                        cx,
                                        target,
                                        editable_id.clone(),
                                        effective_preset.clone(),
                                    )
                                }),
                        )
                        .child(
                            Button::new(format!("watermark-duplicate-{}", target.as_str()))
                                .small()
                                .outline()
                                .label(t!("settings.watermark.template.duplicate").to_string())
                                .disabled(effective_id.is_none())
                                .on_click(move |_, _, cx| {
                                    if let Some(id) = duplicate_id.as_deref() {
                                        AppSettings::update(cx, |config| {
                                            config.watermark.duplicate_template(id, target);
                                        });
                                    }
                                }),
                        )
                        .when_some(delete_id, |this, id| {
                            this.child(
                                Button::new(format!("watermark-delete-{}", target.as_str()))
                                    .small()
                                    .ghost()
                                    .label(t!("settings.watermark.template.delete").to_string())
                                    .on_click(move |_, window, cx| {
                                        let name = delete_name.clone();
                                        let id_to_delete = id.clone();
                                        window.open_alert_dialog(cx, move |alert, _, _| {
                                            let id_to_delete = id_to_delete.clone();
                                            alert
                                                .title(
                                                    t!(
                                                        "settings.watermark.template.delete_title",
                                                        name = name.clone()
                                                    )
                                                    .to_string(),
                                                )
                                                .description(
                                                    t!("settings.watermark.template.delete_desc")
                                                        .to_string(),
                                                )
                                                .confirm()
                                                .ok_text(
                                                    t!("settings.watermark.template.delete")
                                                        .to_string(),
                                                )
                                                .on_ok(move |_, _, cx| {
                                                    AppSettings::update(cx, |config| {
                                                        config
                                                            .watermark
                                                            .delete_template(&id_to_delete);
                                                    });
                                                    true
                                                })
                                        })
                                    }),
                            )
                        }),
                ),
        )
        .child(Select::new(&select.read(cx).state).w_full())
        .when_some(inherited_from_name, |this, name| {
            this.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(t!("settings.watermark.template.inherit_hint", name = name).to_string()),
            )
        })
        .when_some(shared_targets, |this, targets| {
            this.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        t!(
                            "settings.watermark.template.shared_usage",
                            targets = targets
                        )
                        .to_string(),
                    ),
            )
        })
        .into_any_element()
}

fn watermark_target_index(target: WatermarkTarget) -> usize {
    match target {
        WatermarkTarget::Screenshot => 0,
        WatermarkTarget::Animation => 1,
        WatermarkTarget::Video => 2,
    }
}

fn watermark_editor_keywords() -> Vec<SharedString> {
    let mut keywords = vec![
        SharedString::from("watermark"),
        SharedString::from("水印"),
        t!("settings.watermark.group.editor").to_string().into(),
        t!("settings.watermark.kind").to_string().into(),
        t!("settings.watermark.kind.text").to_string().into(),
        t!("settings.watermark.kind.image").to_string().into(),
        t!("settings.watermark.placement").to_string().into(),
        t!("settings.watermark.placement.top_left")
            .to_string()
            .into(),
        t!("settings.watermark.placement.top_center")
            .to_string()
            .into(),
        t!("settings.watermark.placement.top_right")
            .to_string()
            .into(),
        t!("settings.watermark.placement.middle_left")
            .to_string()
            .into(),
        t!("settings.watermark.placement.center").to_string().into(),
        t!("settings.watermark.placement.middle_right")
            .to_string()
            .into(),
        t!("settings.watermark.placement.bottom_left")
            .to_string()
            .into(),
        t!("settings.watermark.placement.bottom_center")
            .to_string()
            .into(),
        t!("settings.watermark.placement.bottom_right")
            .to_string()
            .into(),
        t!("settings.watermark.margin_x").to_string().into(),
        t!("settings.watermark.margin_y").to_string().into(),
        t!("settings.watermark.rotation").to_string().into(),
        t!("settings.watermark.tiled").to_string().into(),
        t!("settings.watermark.spacing_x").to_string().into(),
        t!("settings.watermark.spacing_y").to_string().into(),
        t!("settings.watermark.text.content").to_string().into(),
        t!("settings.watermark.text.font_size").to_string().into(),
        t!("settings.watermark.text.color").to_string().into(),
        t!("settings.watermark.text.bold").to_string().into(),
        t!("settings.watermark.text.shadow").to_string().into(),
        t!("settings.watermark.text.stroke").to_string().into(),
        t!("settings.watermark.text.background").to_string().into(),
        t!("settings.watermark.text.opacity").to_string().into(),
        t!("settings.watermark.image.source").to_string().into(),
        t!("settings.watermark.image.choose").to_string().into(),
        t!("settings.watermark.image.width").to_string().into(),
        t!("settings.watermark.image.corner_radius")
            .to_string()
            .into(),
        t!("settings.watermark.image.opacity").to_string().into(),
        t!("settings.watermark.preview").to_string().into(),
    ];
    keywords.extend(WatermarkTarget::ALL.into_iter().map(watermark_target_label));
    keywords
}

fn watermark_target_label(target: WatermarkTarget) -> SharedString {
    match target {
        WatermarkTarget::Screenshot => t!("settings.watermark.screenshots"),
        WatermarkTarget::Animation => t!("settings.watermark.animations"),
        WatermarkTarget::Video => t!("settings.watermark.videos"),
    }
    .to_string()
    .into()
}

pub(crate) fn build_watermark_preview(
    target: WatermarkTarget,
    preset: &WatermarkPreset,
    image_dir: &std::path::Path,
) -> std::result::Result<Arc<RenderImage>, String> {
    let (width, height) = match target {
        WatermarkTarget::Screenshot => (480, 240),
        WatermarkTarget::Animation => (240, 240),
        WatermarkTarget::Video => (480, 270),
    };
    let sample = RgbaImage::from_fn(width, height, |x, y| {
        let shade = ((y * 255) / height) as u8;
        match target {
            WatermarkTarget::Screenshot if x < 104 => image::Rgba([20, 23, 32, 255]),
            WatermarkTarget::Screenshot if (120..420).contains(&x) && (28..72).contains(&y) => {
                image::Rgba([53, 61, 79, 255])
            }
            WatermarkTarget::Screenshot
                if (120..270).contains(&x) && (86..136).contains(&y)
                    || (284..420).contains(&x) && (86..136).contains(&y) =>
            {
                image::Rgba([39, 46, 62, 255])
            }
            WatermarkTarget::Animation => {
                let distance = ((x as f32 - 120.).powi(2) + (y as f32 - 120.).powi(2)).sqrt();
                if distance < 70. {
                    image::Rgba([57, 67, 88, 255])
                } else if distance < 94. {
                    image::Rgba([40, 48, 65, 255])
                } else {
                    image::Rgba([22, 26, 36, 255])
                }
            }
            WatermarkTarget::Video => {
                if (64..416).contains(&x) && (42..230).contains(&y) {
                    let band = (y / 24) % 2;
                    image::Rgba(if band == 0 {
                        [52, 60, 78, 255]
                    } else {
                        [42, 49, 66, 255]
                    })
                } else {
                    image::Rgba([20, 23, 32, 255])
                }
            }
            WatermarkTarget::Screenshot => {
                image::Rgba([28 + shade / 5, 32 + shade / 4, 43 + shade / 3, 255])
            }
        }
    });
    let spec = crate::export::watermark_spec(preset, image_dir).map_err(|err| err.to_string())?;
    let layer = spec
        .rasterize(width, height, 1.0)
        .map_err(|err| err.to_string())?;
    let composed = Arc::new(layer).apply_sdr(Arc::new(sample));
    Ok(crate::session::render_image(&composed))
}

fn watermark_preview_size(
    target: WatermarkTarget,
    frame_width: f32,
    frame_height: f32,
) -> (f32, f32) {
    let (width, height) = match target {
        WatermarkTarget::Screenshot => (480., 240.),
        WatermarkTarget::Animation => (240., 240.),
        WatermarkTarget::Video => (480., 270.),
    };
    let scale = (frame_width / width).min(frame_height / height);
    (width * scale, height * scale)
}

fn watermark_preview_inactive_setting(
    _options: &RenderOptions,
    _window: &mut Window,
    cx: &mut App,
) -> impl IntoElement + use<> {
    let config = AppSettings::get(cx).watermark.clone();
    let (title, description) = if !config.enabled {
        (
            t!("settings.watermark.inactive").to_string(),
            t!("settings.watermark.inactive.desc").to_string(),
        )
    } else {
        (
            t!("settings.watermark.no_target").to_string(),
            t!("settings.watermark.no_target.desc").to_string(),
        )
    };
    v_flex()
        .w_full()
        .min_h(px(96.))
        .justify_center()
        .items_center()
        .gap_1()
        .rounded_md()
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().muted)
        .child(Label::new(title).text_sm())
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(description),
        )
}

fn watermark_placement_grid(
    target: WatermarkTarget,
    inherited: bool,
    _options: &RenderOptions,
    cx: &App,
) -> AnyElement {
    const ROWS: [[WatermarkPlacement; 3]; 3] = [
        [
            WatermarkPlacement::TopLeft,
            WatermarkPlacement::TopCenter,
            WatermarkPlacement::TopRight,
        ],
        [
            WatermarkPlacement::MiddleLeft,
            WatermarkPlacement::Center,
            WatermarkPlacement::MiddleRight,
        ],
        [
            WatermarkPlacement::BottomLeft,
            WatermarkPlacement::BottomCenter,
            WatermarkPlacement::BottomRight,
        ],
    ];
    let current = AppSettings::get(cx)
        .watermark
        .preset(target)
        .map_or(WatermarkPlacement::default(), |preset| preset.placement);
    let theme = cx.theme();

    h_flex()
        .items_center()
        .gap_3()
        .child(
            v_flex()
                .gap(px(3.))
                .p(px(4.))
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.background)
                .when(inherited, |this| this.opacity(0.6))
                .children(ROWS.into_iter().map(|row| {
                    h_flex().gap(px(3.)).children(row.into_iter().map(|cell| {
                        let active = cell == current;
                        let label = crate::watermark_editor::placement_label(cell);
                        Button::new(SharedString::from(format!(
                            "wm-place-{}-{}",
                            target.as_str(),
                            cell.as_str()
                        )))
                        .ghost()
                        .p_0()
                        .w(px(34.))
                        .h(px(26.))
                        .selected(active)
                        .disabled(inherited)
                        .tooltip(label.clone())
                        .accessibility_label(label)
                        .on_click(move |_, _, cx| {
                            AppSettings::update(cx, |config| {
                                if let Some(id) =
                                    active_editable_template_id(&config.watermark, target)
                                    && let Some(template) = config.watermark.template_mut(&id)
                                {
                                    template.preset.placement = cell;
                                }
                            });
                        })
                        .child(
                            div()
                                .size_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(div().size(px(7.)).rounded_full().bg(if active {
                                    theme.primary
                                } else {
                                    theme.muted_foreground.opacity(0.45)
                                })),
                        )
                    }))
                })),
        )
        .child(
            div()
                .text_sm()
                .text_color(theme.foreground)
                .child(crate::watermark_editor::placement_label(current)),
        )
        .into_any_element()
}

#[derive(Default)]
struct SettingsPreviewDrag {
    active: bool,
    press: Point<Pixels>,
    start_margin_x: f32,
    start_margin_y: f32,
    ratio: f32,
    placement: WatermarkPlacement,
}

fn preview_margin_axes(placement: WatermarkPlacement) -> (f32, f32) {
    let x = match placement {
        WatermarkPlacement::TopLeft
        | WatermarkPlacement::MiddleLeft
        | WatermarkPlacement::BottomLeft => 1.0,
        WatermarkPlacement::TopRight
        | WatermarkPlacement::MiddleRight
        | WatermarkPlacement::BottomRight => -1.0,
        WatermarkPlacement::TopCenter
        | WatermarkPlacement::Center
        | WatermarkPlacement::BottomCenter => 0.0,
    };
    let y = match placement {
        WatermarkPlacement::TopLeft
        | WatermarkPlacement::TopCenter
        | WatermarkPlacement::TopRight => 1.0,
        WatermarkPlacement::BottomLeft
        | WatermarkPlacement::BottomCenter
        | WatermarkPlacement::BottomRight => -1.0,
        WatermarkPlacement::MiddleLeft
        | WatermarkPlacement::Center
        | WatermarkPlacement::MiddleRight => 0.0,
    };
    (x, y)
}

fn watermark_preview_setting(
    target: WatermarkTarget,
    preview: std::result::Result<Arc<RenderImage>, String>,
    _options: &RenderOptions,
    window: &mut Window,
    cx: &mut App,
) -> impl IntoElement + use<> {
    let image = preview.as_ref().ok().cloned();
    let error = match &preview {
        Err(error) => Some(error.clone()),
        Ok(_) => None,
    };
    let watermark = AppSettings::get(cx).watermark.clone();
    let editable_id = active_editable_template_id(&watermark, target);
    let preset = watermark.preset(target).cloned().unwrap_or_default();
    let drag_enabled =
        editable_id.is_some() && !preset.tiled && preset.placement != WatermarkPlacement::Center;
    let drag = window.use_keyed_state(
        SharedString::from(format!("watermark-preview-drag-{}", target.as_str())),
        cx,
        |_, _| SettingsPreviewDrag::default(),
    );
    let start_margin_x = preset.margin_x;
    let start_margin_y = preset.margin_y;
    let placement = preset.placement;

    v_flex()
        .w_full()
        .gap_2()
        .child(Label::new(t!("settings.watermark.preview").to_string()).text_sm())
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(if drag_enabled {
                    format!(
                        "{} {}",
                        t!("settings.watermark.preview.desc"),
                        t!("settings.watermark.preview.drag_hint")
                    )
                } else {
                    t!("settings.watermark.preview.desc").to_string()
                }),
        )
        .child(
            div()
                .w_full()
                .justify_center()
                .child(container_query(move |size, _, cx| {
                    let frame_width = size.width.min(px(WATERMARK_PREVIEW_FRAME_WIDTH));
                    let frame_height = frame_width
                        * (WATERMARK_PREVIEW_FRAME_HEIGHT / WATERMARK_PREVIEW_FRAME_WIDTH);
                    let (width, height) = watermark_preview_size(
                        target,
                        f32::from(frame_width),
                        f32::from(frame_height),
                    );
                    let logical_width = match target {
                        WatermarkTarget::Animation => 240.0,
                        WatermarkTarget::Screenshot | WatermarkTarget::Video => 480.0,
                    };
                    let ratio = if width > 0.0 {
                        logical_width / width
                    } else {
                        1.0
                    };
                    let down_drag = drag.clone();
                    let move_drag = drag.clone();
                    let up_drag = drag.clone();
                    let out_drag = drag.clone();

                    div()
                        .w(frame_width)
                        .h(frame_height)
                        .flex_none()
                        .justify_center()
                        .items_center()
                        .rounded_md()
                        .overflow_hidden()
                        .border_1()
                        .border_color(cx.theme().border)
                        .bg(cx.theme().muted)
                        .children(image.clone().map(|image| {
                            div()
                                .id(SharedString::from(format!(
                                    "watermark-preview-canvas-{}",
                                    target.as_str()
                                )))
                                .w(px(width))
                                .h(px(height))
                                .flex_none()
                                .overflow_hidden()
                                .when(drag_enabled, |this| this.cursor(CursorStyle::OpenHand))
                                .on_mouse_down(MouseButton::Left, move |event, _, cx| {
                                    if !drag_enabled {
                                        return;
                                    }
                                    down_drag.update(cx, |state, _| {
                                        *state = SettingsPreviewDrag {
                                            active: true,
                                            press: event.position,
                                            start_margin_x,
                                            start_margin_y,
                                            ratio,
                                            placement,
                                        };
                                    });
                                })
                                .on_mouse_move(move |event, _, cx| {
                                    let snapshot = {
                                        let state = move_drag.read(cx);
                                        state.active.then_some((
                                            state.press,
                                            state.start_margin_x,
                                            state.start_margin_y,
                                            state.ratio,
                                            state.placement,
                                        ))
                                    };
                                    let Some((press, sx, sy, ratio, placement)) = snapshot else {
                                        return;
                                    };
                                    let (x_dir, y_dir) = preview_margin_axes(placement);
                                    let dx = f32::from(event.position.x - press.x) * ratio;
                                    let dy = f32::from(event.position.y - press.y) * ratio;
                                    AppSettings::update(cx, |config| {
                                        if let Some(id) =
                                            active_editable_template_id(&config.watermark, target)
                                            && let Some(template) =
                                                config.watermark.template_mut(&id)
                                        {
                                            if x_dir != 0.0 {
                                                template.preset.margin_x =
                                                    (sx + dx * x_dir).round().clamp(0.0, 2048.0);
                                            }
                                            if y_dir != 0.0 {
                                                template.preset.margin_y =
                                                    (sy + dy * y_dir).round().clamp(0.0, 2048.0);
                                            }
                                        }
                                    });
                                })
                                .on_mouse_up(MouseButton::Left, move |_, _, cx| {
                                    up_drag.update(cx, |state, _| state.active = false);
                                })
                                .on_mouse_up_out(MouseButton::Left, move |_, _, cx| {
                                    out_drag.update(cx, |state, _| state.active = false);
                                })
                                .child(img(image).size_full().object_fit(ObjectFit::Fill))
                        }))
                        .children(error.clone().map(|error| {
                            div()
                                .size_full()
                                .p_4()
                                .text_sm()
                                .text_color(cx.theme().danger)
                                .child(error)
                        }))
                })),
        )
}

static WATERMARK_PASTE_SEQ: AtomicU64 = AtomicU64::new(1);

pub(crate) fn import_watermark_image_bytes(
    bytes: &[u8],
    directory: &std::path::Path,
) -> anyhow::Result<(String, u32, u32)> {
    const MAX_BYTES: usize = 16 * 1024 * 1024;
    anyhow::ensure!(
        bytes.len() <= MAX_BYTES,
        "watermark image exceeds the 16 MiB limit"
    );
    let rgba = image::load_from_memory(bytes)
        .context("invalid watermark image")?
        .to_rgba8();
    let (width, height) = rgba.dimensions();
    anyhow::ensure!(
        (1..=8192).contains(&width)
            && (1..=8192).contains(&height)
            && u64::from(width) * u64::from(height) <= 64_000_000,
        "watermark image dimensions are invalid or too large"
    );
    std::fs::create_dir_all(directory).context("failed to create the watermark folder")?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seq = WATERMARK_PASTE_SEQ.fetch_add(1, Ordering::Relaxed);
    let name = format!("watermark-{}-{nonce}-{seq}.png", std::process::id());
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join(&name))
        .context("failed to create the managed watermark image")?;
    image::DynamicImage::ImageRgba8(rgba)
        .write_to(&mut output, image::ImageFormat::Png)
        .context("failed to write pasted watermark image")?;
    Ok((name, width, height))
}

pub(crate) fn import_watermark_image(
    source: &std::path::Path,
    directory: &std::path::Path,
) -> anyhow::Result<(String, u32, u32)> {
    let extension = source
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|value| matches!(value.as_str(), "png" | "jpg" | "jpeg"))
        .ok_or_else(|| anyhow::anyhow!("watermark images must be PNG or JPEG"))?;
    let image = load_watermark_image(source).context("invalid watermark image")?;
    let (width, height) = image.dimensions();
    std::fs::create_dir_all(directory).context("failed to create the watermark folder")?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name = format!("watermark-{}-{nonce}.{extension}", std::process::id());
    let mut input = std::fs::File::open(source).context("failed to open the watermark image")?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join(&name))
        .context("failed to create the managed watermark image")?;
    std::io::copy(&mut input, &mut output).context("failed to copy the watermark image")?;
    Ok((name, width, height))
}

fn click_effect_setting(
    view: &Entity<SettingsView>,
    options: &RenderOptions,
    cx: &App,
) -> impl IntoElement + use<> {
    let recording = &AppSettings::get(cx).recording;
    let enabled = recording.click_effects;
    let theme = cx.theme();
    let title = t!("settings.click_effects").to_string();

    v_flex()
        .w_full()
        .gap_3()
        .child(
            h_flex()
                .w_full()
                .justify_between()
                .items_start()
                .gap_3()
                .child(
                    v_flex()
                        .flex_1()
                        .max_w_3_5()
                        .gap_1()
                        .child(Label::new(title.clone()).text_sm())
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(t!("settings.click_effects.desc").to_string()),
                        ),
                )
                .child(
                    Switch::new("click-effects")
                        .accessibility_label(title)
                        .checked(enabled)
                        .disabled(options.is_disabled())
                        .with_size(options.size())
                        .on_click(move |value: &bool, _, cx: &mut App| {
                            AppSettings::update(cx, |config| {
                                config.recording.click_effects = *value;
                            });
                        }),
                ),
        )
        .when(enabled && recording.captures_cursor, |this| {
            this.child(click_effect_picker(view, options, cx))
        })
}

fn click_effect_picker(
    view: &Entity<SettingsView>,
    options: &RenderOptions,
    cx: &App,
) -> impl IntoElement + use<> {
    let recording = &AppSettings::get(cx).recording;
    let selected = ClickEffect::parse(&recording.click_effect).unwrap_or_default();
    let selected_group = view.read(cx).click_effect_group;
    let interactive = recording.captures_cursor && !options.is_disabled();
    let preview_trigger = view.read(cx).click_preview_trigger;
    let theme = cx.theme();

    v_flex()
        .id("click-effect-picker")
        .w_full()
        .gap_2()
        .child(
            h_flex()
                .id("click-effect-groups")
                .w_full()
                .gap_1()
                .flex_nowrap()
                .overflow_x_scroll()
                .children(CLICK_EFFECT_GROUPS.iter().map(|(group, group_key)| {
                    let group = *group;
                    let label = t!(*group_key).to_string();
                    let view = view.clone();
                    Button::new(ElementId::named_usize(
                        "click-effect-group-tab",
                        group as usize,
                    ))
                    .small()
                    .ghost()
                    .selected(group == selected_group)
                    .toggled(group == selected_group)
                    .label(label.clone())
                    .accessibility_label(label)
                    .on_click(move |_, _, cx| {
                        view.update(cx, |view, cx| {
                            if view.click_effect_group != group {
                                view.click_effect_group = group;
                                cx.notify();
                            }
                        });
                    })
                })),
        )
        .child(
            h_flex()
                .id(ElementId::named_usize(
                    "click-effect-group",
                    selected_group as usize,
                ))
                .w_full()
                .gap_1()
                .flex_nowrap()
                .overflow_x_scroll()
                .children(selected_group.effects().iter().map(|effect| {
                    let effect = *effect;
                    let label = click_effect_label(effect);
                    let trigger = preview_trigger
                        .filter(|(triggered, _, _)| *triggered == effect)
                        .map(|(_, trigger, position)| (trigger, position));
                    let preview_bounds = Rc::new(Cell::new(None));
                    let click_bounds = preview_bounds.clone();
                    let view = view.clone();
                    Button::new(ElementId::named_usize("click-effect", effect as usize))
                        .ghost()
                        .selected(effect == selected)
                        .disabled(!interactive)
                        .flex_shrink_0()
                        .w(px(72.))
                        .h(px(84.))
                        .p_1()
                        .relative()
                        .border_1()
                        .border_color(theme.border)
                        .bg(if effect == selected {
                            theme.selection
                        } else {
                            theme.background
                        })
                        .cursor_pointer()
                        .accessibility_label(label.clone())
                        .on_click(move |event, _, cx| {
                            let position = event
                                .mouse_position()
                                .and_then(|position| {
                                    preview_local_position(position, click_bounds.get())
                                })
                                .unwrap_or_else(click_preview_center);
                            let trigger = view
                                .read(cx)
                                .click_preview_trigger
                                .map_or(1, |(_, trigger, _)| trigger.wrapping_add(1));
                            let preview_view = view.downgrade();
                            view.update(cx, |view, cx| {
                                view.click_effect_group = effect.group();
                                view.click_preview_trigger = Some((effect, trigger, position));
                                cx.notify();
                            });
                            cx.spawn(async move |cx: &mut AsyncApp| {
                                cx.background_executor()
                                    .timer(CLICK_PREVIEW_RESTORE_DELAY)
                                    .await;
                                preview_view
                                    .update(cx, |this, cx| {
                                        if click_preview_is_current(
                                            this.click_preview_trigger,
                                            effect,
                                            trigger,
                                        ) {
                                            this.click_preview_trigger = None;
                                            cx.notify();
                                        }
                                    })
                                    .ok();
                            })
                            .detach();
                            AppSettings::update(cx, |config| {
                                config.recording.click_effect = effect.config_name().to_string();
                            })
                        })
                        .when(effect == selected, |this| {
                            this.child(
                                div()
                                    .absolute()
                                    .top_1()
                                    .right_1()
                                    .size(px(6.))
                                    .rounded_full()
                                    .bg(theme.primary),
                            )
                        })
                        .child(
                            v_flex()
                                .size_full()
                                .items_center()
                                .justify_center()
                                .gap_1()
                                .child(click_effect_preview(
                                    effect,
                                    interactive,
                                    trigger,
                                    preview_bounds,
                                    theme.muted,
                                ))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_center()
                                        .max_w_full()
                                        .whitespace_normal()
                                        .text_color(if effect == selected {
                                            theme.primary
                                        } else {
                                            theme.muted_foreground
                                        })
                                        .child(label),
                                ),
                        )
                })),
        )
}

fn click_effect_preview(
    effect: ClickEffect,
    interactive: bool,
    trigger: Option<(u64, Point<Pixels>)>,
    preview_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    background: Hsla,
) -> AnyElement {
    let animation_bounds = preview_bounds.clone();
    let animation = trigger.filter(|_| interactive).map(|(nonce, center)| {
        click_effect_canvas(effect, 0.0, center, animation_bounds.clone()).with_animation(
            ElementId::named_usize("click-effect-animation", nonce as usize),
            Animation::new(CLICK_DURATION).with_max_fps(30.0),
            move |_canvas, phase| {
                click_effect_canvas(
                    effect,
                    click_preview_progress(phase),
                    center,
                    animation_bounds.clone(),
                )
            },
        )
    });
    let playing = animation.is_some();

    div()
        .relative()
        .w(px(CLICK_PREVIEW_SIZE))
        .h(px(CLICK_PREVIEW_SIZE))
        .rounded_md()
        .overflow_hidden()
        .bg(background)
        .when(!playing, |this| {
            this.child(click_effect_canvas(
                effect,
                0.0,
                click_preview_center(),
                preview_bounds,
            ))
        })
        .when_some(animation, |this, animation| {
            this.child(div().absolute().inset_0().child(animation))
        })
        .into_any_element()
}

fn click_preview_is_current(
    current: Option<(ClickEffect, u64, Point<Pixels>)>,
    effect: ClickEffect,
    trigger: u64,
) -> bool {
    current.is_some_and(|(current_effect, current_trigger, _)| {
        current_effect == effect && current_trigger == trigger
    })
}

fn click_preview_center() -> Point<Pixels> {
    point(px(CLICK_PREVIEW_SIZE / 2.0), px(CLICK_PREVIEW_SIZE / 2.0))
}

fn preview_local_position(
    position: Point<Pixels>,
    bounds: Option<Bounds<Pixels>>,
) -> Option<Point<Pixels>> {
    let bounds = bounds?;
    bounds
        .contains(&position)
        .then(|| point(position.x - bounds.origin.x, position.y - bounds.origin.y))
}

fn click_preview_progress(phase: f32) -> f64 {
    phase.clamp(0.0, 1.0) as f64
}

fn click_effect_canvas(
    effect: ClickEffect,
    progress: f64,
    center: Point<Pixels>,
    preview_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
) -> Div {
    div()
        .w(px(CLICK_PREVIEW_SIZE))
        .h(px(CLICK_PREVIEW_SIZE))
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    preview_bounds.set(Some(bounds));
                    let cell = bounds.size.width.as_f32() / 64.0;
                    let offset_x = center.x.as_f32() - 32.0 * cell;
                    let offset_y = center.y.as_f32() - 32.0 * cell;
                    effect.visit_preview(progress, 1.0, |x, y, color, opacity| {
                        let paint = Hsla {
                            a: opacity as f32,
                            ..Hsla::from(rgb((u32::from(color[0]) << 16)
                                | (u32::from(color[1]) << 8)
                                | u32::from(color[2])))
                        };
                        let origin = point(
                            bounds.origin.x + px(x as f32 * cell + offset_x),
                            bounds.origin.y + px(y as f32 * cell + offset_y),
                        );
                        window.paint_quad(fill(
                            Bounds::new(origin, size(px(cell + 0.15), px(cell + 0.15))),
                            paint,
                        ));
                    });
                },
            )
            .size_full(),
        )
}

/// Records a shortcut by pressing it; `Kbd` renders the caps for this platform.
fn hotkey_recorder(
    view: &Entity<SettingsView>,
    action: HotkeyAction,
    options: &RenderOptions,
    cx: &mut App,
) -> impl IntoElement + use<> {
    let ix = action as usize;
    let (recording, held, error, focus, binding) = {
        let this = view.read(cx);
        (
            this.recording == Some(action),
            this.held_modifiers,
            this.hotkey_errors[ix].is_some(),
            this.hotkey_focus[ix].clone(),
            action.binding(&AppSettings::get(cx).hotkeys).to_string(),
        )
    };
    let caps = if recording {
        hotkey::modifier_caps(held)
    } else {
        hotkey::caps(&binding)
    };
    let theme = cx.theme();
    let click_view = view.clone();
    let keys_view = view.clone();
    let clear_view = view.clone();

    h_flex()
        .gap_1()
        .child(
            div()
                .id(ElementId::named_usize("hotkey-record", ix))
                .track_focus(&focus)
                .flex()
                .items_center()
                .gap_1()
                .h(px(32.))
                .min_w(px(150.))
                .px_2()
                .rounded_md()
                .border_1()
                .border_color(match (error, recording) {
                    (true, _) => theme.danger,
                    (false, true) => theme.ring,
                    (false, false) => theme.border,
                })
                .bg(if recording {
                    theme.selection
                } else {
                    theme.background
                })
                .cursor_pointer()
                .on_click(move |_, window, cx| {
                    click_view.update(cx, |view, cx| view.toggle_recording(action, window, cx))
                })
                .on_modifiers_changed(move |event, _, cx| {
                    let modifiers = event.modifiers;
                    keys_view.update(cx, |view, cx| {
                        view.held_modifiers_changed(action, modifiers, cx)
                    })
                })
                .when(caps.is_empty(), |this| {
                    this.child(div().text_sm().text_color(theme.muted_foreground).child(
                        if recording {
                            t!("settings.hotkey.recording").to_string()
                        } else {
                            t!("settings.hotkey.unset").to_string()
                        },
                    ))
                })
                .children(caps.into_iter().map(|stroke| Kbd::new(stroke).outline())),
        )
        .when(!recording && !binding.trim().is_empty(), |this| {
            this.child(
                Button::new(ElementId::named_usize("hotkey-clear", ix))
                    .ghost()
                    .with_size(options.size())
                    .icon(Icon::new(IconName::CircleX))
                    .tooltip(t!("settings.hotkey.clear").to_string())
                    .on_click(move |_, _, cx| {
                        clear_view.update(cx, |view, cx| view.commit(action, String::new(), cx))
                    }),
            )
        })
}

fn choose_save_dir(cx: &mut App) {
    let picked = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some(t!("settings.pick_dir").to_string().into()),
    });
    cx.spawn(async move |cx| {
        if let Ok(Ok(Some(paths))) = picked.await
            && let Some(dir) = paths.into_iter().next()
        {
            cx.update(|cx| AppSettings::update(cx, |config| config.capture.save_dir = Some(dir)));
        }
    })
    .detach();
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        div()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                Settings::new("snapit-settings")
                    .sidebar_width(px(180.))
                    .pages([
                        self.general_page(),
                        self.hotkey_page(&view, cx),
                        self.capture_page(),
                        self.watermark_page(&view, cx),
                        self.recording_page(&view),
                        self.ocr_page(),
                        self.history_page(),
                        self.about_page(),
                    ]),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_grouped_click_effect_has_one_card_label() {
        let labels = CLICK_EFFECT_GROUPS
            .iter()
            .flat_map(|(group, _)| group.effects().iter())
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(labels.len(), ClickEffect::ALL.len());
        for effect in ClickEffect::ALL {
            assert_eq!(
                CLICK_EFFECT_LABELS
                    .iter()
                    .filter(|(candidate, _)| *candidate == effect)
                    .count(),
                1
            );
            assert!(labels.contains(&effect));
        }
    }

    #[test]
    fn click_preview_starts_immediately_and_plays_one_full_effect() {
        assert_eq!(click_preview_progress(0.0), 0.0);
        assert!((click_preview_progress(0.18) - 0.18).abs() < 1e-6);
        assert_eq!(click_preview_progress(0.5), 0.5);
        assert_eq!(click_preview_progress(1.0), 1.0);
    }

    #[test]
    fn click_preview_thumbnail_returns_to_a_centered_rest_state() {
        assert_eq!(click_preview_center(), point(px(28.), px(28.)));
        assert!(CLICK_PREVIEW_RESTORE_DELAY > CLICK_DURATION);
    }

    #[test]
    fn only_the_latest_click_can_restore_the_idle_thumbnail() {
        let center = click_preview_center();
        assert!(!click_preview_is_current(None, ClickEffect::Ring, 1));
        assert!(click_preview_is_current(
            Some((ClickEffect::Ring, 1, center)),
            ClickEffect::Ring,
            1,
        ));
        assert!(!click_preview_is_current(
            Some((ClickEffect::Ring, 2, center)),
            ClickEffect::Ring,
            1,
        ));
        assert!(!click_preview_is_current(
            Some((ClickEffect::Dot, 1, center)),
            ClickEffect::Ring,
            1,
        ));
    }

    #[test]
    fn watermark_preview_keeps_a_wide_frame_and_undistorted_canvases() {
        assert!(
            (WATERMARK_PREVIEW_FRAME_WIDTH / WATERMARK_PREVIEW_FRAME_HEIGHT - 16. / 9.).abs()
                < 0.0001
        );
        for (target, ratio, expected) in [
            (WatermarkTarget::Screenshot, 2.0, (512., 256.)),
            (WatermarkTarget::Animation, 1.0, (288., 288.)),
            (WatermarkTarget::Video, 16. / 9., (512., 288.)),
        ] {
            let (width, height) = watermark_preview_size(
                target,
                WATERMARK_PREVIEW_FRAME_WIDTH,
                WATERMARK_PREVIEW_FRAME_HEIGHT,
            );
            assert!(width <= WATERMARK_PREVIEW_FRAME_WIDTH);
            assert!(height <= WATERMARK_PREVIEW_FRAME_HEIGHT);
            assert!((width / height - ratio).abs() < 0.0001);
            assert!((width - expected.0).abs() < 0.0001);
            assert!((height - expected.1).abs() < 0.0001);
        }
    }

    #[test]
    fn watermark_search_keywords_keep_editor_fields_discoverable() {
        let keywords = watermark_editor_keywords();
        assert!(keywords.iter().any(|keyword| keyword == "watermark"));
        assert!(keywords.iter().any(|keyword| keyword == "水印"));

        for label in [
            t!("settings.watermark.kind"),
            t!("settings.watermark.placement"),
            t!("settings.watermark.margin_x"),
            t!("settings.watermark.rotation"),
            t!("settings.watermark.tiled"),
            t!("settings.watermark.text.content"),
            t!("settings.watermark.image.source"),
            t!("settings.watermark.preview"),
        ] {
            let label: SharedString = label.to_string().into();
            assert!(keywords.contains(&label));
        }
        for target in WatermarkTarget::ALL {
            assert!(keywords.contains(&watermark_target_label(target)));
        }
    }

    #[test]
    fn watermark_preview_uses_the_shared_text_rasterizer() {
        let mut preset = WatermarkPreset::default();
        preset.elements[0].text.color = 0xFA_CC_15;
        preset.elements[0].opacity = 1.0;
        let preview = build_watermark_preview(
            WatermarkTarget::Screenshot,
            &preset,
            std::path::Path::new("."),
        )
        .unwrap();
        let bytes = preview.as_bytes(0).unwrap();
        assert!(
            bytes.as_chunks::<4>().0.iter().any(|pixel| {
                pixel[0] < 80 && pixel[1] > 180 && pixel[2] > 220 && pixel[3] == 255
            })
        );
    }

    #[test]
    fn watermark_preview_reports_an_unmanaged_image_source() {
        let mut preset = WatermarkPreset::default();
        preset.elements = vec![snapit_store::WatermarkElement::new_image(
            "el-1",
            "../outside.png",
            120.0,
        )];
        let error = build_watermark_preview(
            WatermarkTarget::Screenshot,
            &preset,
            std::path::Path::new("."),
        )
        .unwrap_err();
        assert!(error.contains("managed file name"));
    }

    #[test]
    fn click_preview_maps_each_click_to_its_position_in_the_canvas() {
        let bounds = Bounds::new(point(px(100.), px(200.)), size(px(64.), px(64.)));
        assert_eq!(
            preview_local_position(point(px(112.), px(233.)), Some(bounds)),
            Some(point(px(12.), px(33.)))
        );
        assert_eq!(
            preview_local_position(point(px(150.), px(210.)), Some(bounds)),
            Some(point(px(50.), px(10.)))
        );
        assert_eq!(
            preview_local_position(point(px(99.), px(210.)), Some(bounds)),
            None
        );
    }
}
