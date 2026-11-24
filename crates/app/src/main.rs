// A tray app launched from Explorer or at login must not open a console even in
// debug builds. Std handles inherited from a terminal still carry the logs, so
// `cargo run -p snapit` keeps printing.
#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

// Platforms without a capture / tray / updater backend would fail in a
// scattering of modules; say it once.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
compile_error!("SnapIt supports macOS and Windows only");

#[macro_use]
extern crate rust_i18n;

rust_i18n::i18n!("locales", fallback = "en");

mod annotate;
mod appearance;
mod assets;
mod autostart;
mod clipboard;
mod controller;
mod events;
mod history_store;
mod history_view;
mod hotkey;
mod i18n;
mod instance;
mod logging;
mod ocr_view;
mod overlay;
mod permission_view;
mod permissions;
mod pin;
mod recording_bar;
mod session;
mod settings;
mod settings_view;
mod tray;
mod update_view;
mod updater;

use gpui_kit::{KeyBinding, Menu, MenuItem, QuitMode, actions};
use snapit_store::AppDirs;

use crate::controller::{Controller, Panel};
use crate::history_store::HistoryStore;
use crate::session::CaptureIntent;
use crate::settings::AppSettings;

actions!(
    snapit,
    [
        Quit,
        Capture,
        OpenSettings,
        OpenHistory,
        PinLast,
        CheckUpdates
    ]
);

/// `KeyBinding::new` does not resolve `secondary-`, so spell the platform modifier.
macro_rules! shortcut {
    ($key:literal) => {
        if cfg!(target_os = "macos") {
            concat!("cmd-", $key)
        } else {
            concat!("ctrl-", $key)
        }
    };
}
pub(crate) use shortcut;

/// App menus follow the UI language; call again after it changes.
pub(crate) fn install_menus(cx: &mut gpui_kit::App) {
    cx.set_menus([Menu::new("SnapIt").items([
        MenuItem::action(t!("action.screenshot"), Capture),
        MenuItem::action(t!("action.pin"), PinLast),
        MenuItem::action(t!("action.history"), OpenHistory),
        MenuItem::action(t!("menu.settings"), OpenSettings),
        MenuItem::action(t!("action.check_updates"), CheckUpdates),
        MenuItem::separator(),
        MenuItem::action(t!("action.quit"), Quit),
    ])]);
}

fn main() {
    #[cfg(target_os = "windows")]
    updater::run_update_installer_helper();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let launch_request = instance::LaunchRequest::from_args(&args);
    // `--background` is still accepted for launch-at-login; tray-only either way.
    let background = args.iter().any(|arg| arg == autostart::BACKGROUND_ARG);
    let dirs = AppDirs::discover().unwrap_or_else(|err| {
        // Logger may not be up yet; this line is best-effort.
        eprintln!("{err:#}; keeping data in the temp directory");
        AppDirs::at(std::env::temp_dir().join("snapit"))
    });
    // Persistent, correlation-tagged logging: stderr + rotating file. Must come
    // after `dirs` so the log lands in the same root as config/history.
    logging::init(&dirs.log_file());
    let _instance = match instance::acquire(&dirs) {
        Ok(Some(lock)) => Some(lock),
        Ok(None) => {
            // A second launch must not duplicate the tray icon or fight over the
            // global hotkeys, but it still has to honour the requested action.
            // Forward it to the running instance instead of dropping it.
            match launch_request {
                Some(request) => match instance::send(&dirs, request) {
                    Ok(()) => log::info!("another instance is running; forwarded {request:?}"),
                    Err(err) => log::error!("cannot forward {request:?}: {err:#}"),
                },
                None => log::info!("another SnapIt instance is running; exiting"),
            }
            return;
        }
        Err(err) => {
            log::error!("cannot acquire the single-instance lock: {err:#}");
            return;
        }
    };

    // Register the OS callbacks before the event loop can produce any hotkey or
    // menu event; see `events::install` for why the ordering is required.
    let (events, incoming) = events::install();

    let app = gpui_kit::application()
        .with_assets(assets::AppAssets)
        .with_quit_mode(QuitMode::Explicit);
    // Tray-only: never open a home/launcher window on launch.
    app.run(move |cx| {
        gpui_kit::init(cx);
        AppSettings::init(&dirs, cx);
        i18n::apply(AppSettings::get(cx));
        HistoryStore::init(dirs.history_dir(), cx);
        let theme = AppSettings::get(cx).general.theme;
        appearance::apply_theme(theme, None, cx);
        // Tray-only until a panel window opens.
        appearance::set_dock_visible(false, cx);
        overlay::init(cx);
        pin::init(cx);

        cx.bind_keys([
            KeyBinding::new(shortcut!("q"), Quit, None),
            KeyBinding::new(shortcut!(","), OpenSettings, None),
            KeyBinding::new(shortcut!("y"), OpenHistory, None),
        ]);
        cx.on_action(|_: &Quit, cx| {
            log::logger().flush();
            cx.quit();
        });
        cx.on_action(|_: &Capture, cx| {
            Controller::global(cx).update(cx, |controller, cx| {
                controller.start_capture(CaptureIntent::Choose, cx)
            })
        });
        cx.on_action(|_: &OpenSettings, cx| {
            Controller::global(cx).update(cx, |controller, cx| {
                controller.open_panel(Panel::Settings, cx)
            })
        });
        cx.on_action(|_: &OpenHistory, cx| {
            Controller::global(cx).update(cx, |controller, cx| {
                controller.open_panel(Panel::History, cx)
            })
        });
        cx.on_action(|_: &PinLast, cx| {
            Controller::global(cx).update(cx, |controller, cx| controller.pin_last(cx))
        });
        cx.on_action(|_: &CheckUpdates, cx| updater::check(true, cx));
        install_menus(cx);

        let controller = Controller::init(cx);
        events::listen(incoming, cx);
        updater::init(cx);
        hotkey::init(cx, !background);
        tray::init(cx);
        autostart::sync_on_launch(cx);
        instance::poll_requests(instance::request_path(&dirs), events, cx);
        controller.update(cx, |controller, cx| match launch_request {
            Some(instance::LaunchRequest::Capture) => {
                controller.start_capture(CaptureIntent::Choose, cx)
            }
            Some(instance::LaunchRequest::Settings) => controller.open_panel(Panel::Settings, cx),
            Some(instance::LaunchRequest::History) => controller.open_panel(Panel::History, cx),
            None => controller.check_permissions_on_launch(!background, cx),
        });
    });
}
