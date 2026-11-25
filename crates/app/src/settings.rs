//! The live settings: one `ConfigStore` in a global, with side effects applied
//! whenever it changes.

use gpui_kit::{App, Global};
use snapit_store::{AppDirs, Config, ConfigStore};

use crate::{
    appearance, autostart, controller::Controller, history_store::HistoryStore, hotkey, tray,
};

pub struct AppSettings {
    store: ConfigStore,
    watermark_dir: std::path::PathBuf,
}

impl Global for AppSettings {}

impl AppSettings {
    pub fn init(dirs: &AppDirs, cx: &mut App) {
        let path = dirs.config_file();
        let store = ConfigStore::open(&path).unwrap_or_else(|err| {
            log::error!("settings unavailable, using defaults: {err:#}");
            ConfigStore::detached(&path)
        });
        log::info!("settings: {}", store.path().display());
        cx.set_global(Self {
            store,
            watermark_dir: dirs.watermark_dir(),
        });

        let mut applied = Self::get(cx).clone();
        cx.observe_global::<Self>(move |cx| {
            let next = Self::get(cx).clone();
            apply_changes(&applied, &next, cx);
            applied = next;
        })
        .detach();
    }

    pub fn get(cx: &App) -> &Config {
        cx.global::<Self>().store.config()
    }

    pub fn config_path(cx: &App) -> &std::path::Path {
        cx.global::<Self>().store.path()
    }

    pub fn watermark_dir(cx: &App) -> &std::path::Path {
        &cx.global::<Self>().watermark_dir
    }

    /// Change and persist settings. Observers only fire when something changed.
    pub fn update(cx: &mut App, f: impl FnOnce(&mut Config)) {
        let mut next = Self::get(cx).clone();
        f(&mut next);
        next.sanitize();
        if &next == Self::get(cx) {
            return;
        }
        if let Err(err) = cx
            .global_mut::<Self>()
            .store
            .update(|config| *config = next)
        {
            log::error!("failed to save settings: {err:#}");
        }
    }
}

fn apply_changes(prev: &Config, next: &Config, cx: &mut App) {
    if prev.general.language != next.general.language {
        crate::i18n::apply(next);
        hotkey::sync(&next.hotkeys, cx);
        crate::install_menus(cx);
        tray::refresh(cx);
        Controller::global(cx).update(cx, |controller, cx| controller.refresh_titles(cx));
        cx.refresh_windows();
    }
    if prev.general.theme != next.general.theme {
        appearance::apply_theme(next.general.theme, None, cx);
    }
    if prev.general.launch_at_login != next.general.launch_at_login {
        autostart::apply(next.general.launch_at_login, cx);
    }
    if prev.hotkeys != next.hotkeys {
        hotkey::sync(&next.hotkeys, cx);
        tray::refresh(cx);
    }
    if prev.ocr != next.ocr {
        Controller::global(cx).update(cx, |controller, _| controller.reset_ocr_engine());
    }
    if prev.watermark != next.watermark {
        cx.refresh_windows();
    }
    if prev.history != next.history {
        // The limit is not applied here on purpose. Every edit — including each
        // click of the stepper — would prune immediately, and walking from 200
        // down to 0 would delete entries at the stops in between (`0` itself
        // means "keep everything"). Pruning happens when a capture is added and
        // when the app starts, both of which read the settled limit.
        HistoryStore::global(cx).update(cx, |_, cx| cx.notify());
    }
}
