//! The settings window, built on gpui-kit's `Settings` component.

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    kbd::Kbd,
    setting::{
        NumberFieldOptions, RenderOptions, SettingField, SettingGroup, SettingItem, SettingPage,
        Settings,
    },
};
use gpui_kit::{
    App, Axis, Context, ElementId, Entity, FocusHandle, IntoElement, Keystroke, Modifiers,
    ParentElement as _, PathPromptOptions, Render, SharedString, Styled as _, Subscription, Window,
    div,
    prelude::FluentBuilder as _,
    prelude::{InteractiveElement as _, StatefulInteractiveElement as _},
    px,
};
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

        Self {
            hotkey_focus,
            recording: None,
            held_modifiers: Modifiers::none(),
            hotkey_errors: Default::default(),
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
                    ]),
            )
    }

    fn recording_page(&self) -> SettingPage {
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
                            t!("settings.fps").to_string(),
                            SettingField::number_input(
                                NumberFieldOptions {
                                    min: min_fps as f64,
                                    max: max_fps as f64,
                                    step: 1.,
                                },
                                |cx: &App| AppSettings::get(cx).recording.fps as f64,
                                |value: f64, cx: &mut App| {
                                    AppSettings::update(cx, |config| {
                                        config.recording.fps = value.round() as u32
                                    })
                                },
                            )
                            .default_value(defaults.fps as f64),
                        )
                        .description(t!("settings.fps.desc").to_string()),
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
                                    min: 1.,
                                    max: 10_000.,
                                    step: 10.,
                                },
                                |cx: &App| AppSettings::get(cx).history.max_items as f64,
                                |value: f64, cx: &mut App| {
                                    AppSettings::update(cx, |config| {
                                        config.history.max_items = value.round() as usize
                                    })
                                },
                            )
                            .default_value(defaults.max_items as f64),
                        )
                        .description(t!("settings.history.max_items.desc").to_string()),
                        SettingItem::new(
                            t!("settings.history.max_size").to_string(),
                            SettingField::number_input(
                                NumberFieldOptions {
                                    min: 1.,
                                    max: 100_000.,
                                    step: 64.,
                                },
                                |cx: &App| AppSettings::get(cx).history.max_total_mb as f64,
                                |value: f64, cx: &mut App| {
                                    AppSettings::update(cx, |config| {
                                        config.history.max_total_mb = value.round() as u64
                                    })
                                },
                            )
                            .default_value(defaults.max_total_mb as f64),
                        )
                        .description(t!("settings.history.max_size.desc").to_string()),
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
                        self.recording_page(),
                        self.ocr_page(),
                        self.history_page(),
                        self.about_page(),
                    ]),
            )
    }
}
