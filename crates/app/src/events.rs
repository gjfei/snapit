//! One event bus for everything that originates outside the GPUI main thread.
//!
//! `global-hotkey` and `tray-icon` deliver events on platform threads, while
//! `Controller` and every window must be touched on the GPUI main thread. Both
//! libraries expose a `set_event_handler` callback, so events are pushed into
//! an unbounded channel here and drained by one task spawned with `cx.spawn`.
//! The channel's waker reschedules that task through the platform dispatcher
//! (GCD main queue on macOS, `PostMessageW` on Windows), which also wakes an
//! otherwise idle event loop — no timers and no polling for OS events.
//!
//! Requests forwarded by a second launch are the one exception: a file on disk
//! has no notification mechanism, so `instance.rs` still polls and then posts
//! onto this same bus.

use futures::StreamExt as _;
use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use global_hotkey::{GlobalHotKeyEvent, HotKeyState};
use gpui_kit::App;
use tray_icon::menu::MenuEvent;

use crate::controller::Controller;
use crate::hotkey;
use crate::instance::{self, LaunchRequest};
use crate::tray;

/// An event whose payload is safe to build on a platform thread. Hotkey and
/// menu ids are resolved on the main thread, where the live registrations and
/// GPUI globals live.
#[derive(Debug)]
pub(crate) enum AppEvent {
    HotkeyPressed(u32),
    TrayMenu(String),
    LaunchRequest(LaunchRequest),
}

/// Cloneable, thread-safe producer half of the bus.
#[derive(Clone)]
pub(crate) struct Events(UnboundedSender<AppEvent>);

impl Events {
    pub(crate) fn send(&self, event: AppEvent) {
        if let Err(err) = self.0.unbounded_send(event) {
            log::error!("event bus is closed: {err}");
        }
    }
}

/// Install the OS callbacks and return the queue the main thread will drain.
///
/// This MUST run before `GlobalHotKeyManager::new()` and `TrayIconBuilder::build()`:
/// both libraries keep their handler in a `OnceLock` that `send()` initialises
/// to `None` on the first event, after which `set_event_handler` silently fails.
/// Calling it from `main()` before `Application::run` guarantees that ordering.
/// The callbacks only push into the channel; they never touch GPUI state.
pub(crate) fn install() -> (Events, UnboundedReceiver<AppEvent>) {
    let (sender, receiver) = mpsc::unbounded();
    let events = Events(sender);

    let hotkeys = events.clone();
    GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
        // Releases are only used by the library on Windows to detect key-up;
        // dropping them here keeps the queue to one event per press.
        if event.state() == HotKeyState::Pressed {
            hotkeys.send(AppEvent::HotkeyPressed(event.id()));
        }
    }));

    let menus = events.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        menus.send(AppEvent::TrayMenu(event.id.as_ref().to_owned()));
    }));

    (events, receiver)
}

/// Drain the bus on the GPUI main thread. Call after `Controller::init`, since
/// every event is dispatched through the controller.
pub(crate) fn listen(mut receiver: UnboundedReceiver<AppEvent>, cx: &mut App) {
    cx.spawn(async move |cx| {
        while let Some(event) = receiver.next().await {
            cx.update(|cx| dispatch(event, cx));
        }
    })
    .detach();
}

fn dispatch(event: AppEvent, cx: &mut App) {
    match event {
        AppEvent::HotkeyPressed(id) => {
            // An id from an old registration can arrive just after a settings
            // change re-registered the shortcuts; `action_for` ignores it.
            let Some(action) = hotkey::action_for(id, cx) else {
                return;
            };
            Controller::global(cx).update(cx, |controller, cx| controller.on_hotkey(action, cx));
        }
        AppEvent::TrayMenu(id) => tray::dispatch_menu(&id, cx),
        AppEvent::LaunchRequest(request) => instance::dispatch(request, cx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bus_delivers_every_kind_of_event() {
        let (sender, mut receiver) = mpsc::unbounded();
        let events = Events(sender);
        events.send(AppEvent::HotkeyPressed(7));
        events.send(AppEvent::TrayMenu("history".into()));
        events.send(AppEvent::LaunchRequest(LaunchRequest::Capture));

        assert!(matches!(
            receiver.try_recv(),
            Ok(AppEvent::HotkeyPressed(7))
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(AppEvent::TrayMenu(id)) if id == "history"
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(AppEvent::LaunchRequest(LaunchRequest::Capture))
        ));
        drop(events);
        assert!(
            receiver.try_recv().is_err(),
            "the bus closes once its last sender is dropped"
        );
    }
}
