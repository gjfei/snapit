//! The settings window, built on gpui-kit's `Settings` component.

use std::{cell::Cell, rc::Rc, time::Duration};

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    kbd::Kbd,
    label::Label,
    setting::{
        NumberFieldOptions, RenderOptions, SettingField, SettingGroup, SettingItem, SettingPage,
        Settings,
    },
    switch::Switch,
    v_flex,
};
use gpui_kit::{
    Animation, AnimationExt as _, AnyElement, App, AsyncApp, Axis, Bounds, Context, Div, ElementId,
    Entity, FocusHandle, Hsla, IntoElement, Keystroke, Modifiers, ParentElement as _,
    PathPromptOptions, Pixels, Point, Render, SharedString, Styled as _, Subscription, Window,
    canvas, div, fill, point,
    prelude::FluentBuilder as _,
    prelude::{InteractiveElement as _, StatefulInteractiveElement as _},
    px, rgb, size,
};
use snapit_recorder::{CLICK_DURATION, ClickEffect, ClickEffectGroup};
use snapit_store::{Config, HistoryConfig, Hotkeys, ImageFormat, RecordingConfig, ThemePreference};

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

pub struct SettingsView {
    hotkey_focus: [FocusHandle; HotkeyAction::COUNT],
    /// The action whose field is waiting for keys.
    recording: Option<HotkeyAction>,
    /// Modifiers held right now, shown live while recording.
    held_modifiers: Modifiers,
    hotkey_errors: [Option<SharedString>; HotkeyAction::COUNT],
    click_effect_group: ClickEffectGroup,
    click_preview_trigger: Option<(ClickEffect, u64, Point<Pixels>)>,
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
