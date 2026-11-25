//! Small topmost control window shown next to the region while a GIF / APNG / MP4 is
//! recording: timer, pause / resume, cancel (discard) and stop (save).

use std::time::Duration;

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
};
use gpui_kit::{
    Context, Entity, FocusHandle, InteractiveElement as _, IntoElement, KeyBinding, MouseButton,
    ParentElement as _, Render, Styled as _, Subscription, Window, actions, div,
};

use crate::controller::Controller;

actions!(
    recording_bar,
    [StopRecording, CancelRecording, TogglePauseRecording]
);

/// Window size in logical px; a platform window boundary, not UI layout.
/// Just wide enough for the timer and three icon-only buttons side by side.
pub const BAR_SIZE: (f32, f32) = (312., 44.);

const CONTEXT: &str = "RecordingBar";

pub struct RecordingBar {
    controller: Entity<Controller>,
    focus_handle: FocusHandle,
    _subscription: Subscription,
}

/// Keep a control window out of the recording.
///
/// Windows honours `WDA_EXCLUDEFROMCAPTURE` for Desktop Duplication. The macOS
/// recorder drops the process's own windows through ScreenCaptureKit, so it
/// needs no per-window call.
pub(crate) fn exclude_from_capture(window: &mut Window) {
    #[cfg(target_os = "windows")]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE,
        };

        let Ok(handle) = HasWindowHandle::window_handle(&*window) else {
            return;
        };
        let RawWindowHandle::Win32(handle) = handle.as_raw() else {
            return;
        };
        let hwnd = handle.hwnd.get() as *mut std::ffi::c_void;
        if unsafe { SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) } == 0 {
            log::debug!("display affinity is not available on this Windows version");
        }
    }
    #[cfg(not(target_os = "windows"))]
    let _ = window;
}

impl RecordingBar {
    pub fn new(
        controller: Entity<Controller>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        exclude_from_capture(window);
        cx.bind_keys([
            // Esc throws the recording away; stopping is an explicit action
            // (the button, Enter, or a global hotkey) and always saves.
            KeyBinding::new("escape", CancelRecording, Some(CONTEXT)),
            KeyBinding::new("enter", StopRecording, Some(CONTEXT)),
            KeyBinding::new("space", TogglePauseRecording, Some(CONTEXT)),
        ]);
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        let subscription = cx.observe(&controller, |_, _, cx| cx.notify());
        Self {
            controller,
            focus_handle,
            _subscription: subscription,
        }
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        self.controller
            .update(cx, |controller, cx| controller.stop_recording(cx));
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        self.controller
            .update(cx, |controller, cx| controller.cancel_recording(cx));
    }

    fn toggle_pause(&mut self, cx: &mut Context<Self>) {
        self.controller
            .update(cx, |controller, cx| controller.toggle_recording_pause(cx));
    }

    /// Move the window with the system drag loop.
    fn drag(&mut self, window: &mut Window, _cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        window.start_window_move();
        #[cfg(target_os = "windows")]
        native_drag(window);
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let _ = window;
    }
}

/// The window has no titlebar, so the drag has to start as a caption drag.
#[cfg(target_os = "windows")]
pub(crate) fn native_drag(window: &mut Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
    use windows_sys::Win32::UI::WindowsAndMessaging::{HTCAPTION, SendMessageW, WM_NCLBUTTONDOWN};

    let Ok(handle) = HasWindowHandle::window_handle(&*window) else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };
    let hwnd = handle.hwnd.get() as *mut std::ffi::c_void;
    unsafe {
        ReleaseCapture();
        SendMessageW(hwnd, WM_NCLBUTTONDOWN, HTCAPTION as usize, 0);
    }
}

fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

/// Let pointer events fall through to whatever is underneath. macOS has a window
/// flag for this; on Windows the extended style does the hit-testing.
fn make_click_through(window: &mut Window) {
    #[cfg(target_os = "macos")]
    {
        use objc2::msg_send;
        use objc2::runtime::{AnyObject, Bool};
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};

        let Ok(handle) = HasWindowHandle::window_handle(&*window) else {
            return;
        };
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            return;
        };
        unsafe {
            let view = handle.ns_view.as_ptr().cast::<AnyObject>();
            let native_window: *mut AnyObject = msg_send![view, window];
            if !native_window.is_null() {
                let _: () = msg_send![native_window, setIgnoresMouseEvents: Bool::YES];
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GWL_EXSTYLE, GetWindowLongPtrW, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE,
            SWP_NOSIZE, SWP_NOZORDER, SetWindowLongPtrW, SetWindowPos, WS_EX_NOACTIVATE,
            WS_EX_TRANSPARENT,
        };

        let Ok(handle) = HasWindowHandle::window_handle(&*window) else {
            return;
        };
        let RawWindowHandle::Win32(handle) = handle.as_raw() else {
            return;
        };
        let hwnd = handle.hwnd.get() as *mut std::ffi::c_void;
        unsafe {
            let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(
                hwnd,
                GWL_EXSTYLE,
                style | WS_EX_TRANSPARENT as isize | WS_EX_NOACTIVATE as isize,
            );
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER,
            );
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let _ = window;
}

/// Prepare a window that covers the recorded display: keep it out of the
/// capture and let pointer events reach the app being recorded.
pub(crate) fn make_recording_window(window: &mut Window) {
    exclude_from_capture(window);
    make_click_through(window);
}

impl Render for RecordingBar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let controller = self.controller.read(cx);
        // The session is gone; the controller defers this window's removal to
        // the end of the effect cycle. Paint nothing in the meantime so a
        // stale "Finishing…" card can never be left on screen.
        if !controller.has_recording() {
            return div().id("recording-bar").size_full();
        }
        let encoding = controller.is_encoding_recording();
        let starting = controller.is_starting_recording();
        let paused = controller.is_recording_paused();
        let progress = controller.recording_progress();
        let label = if encoding {
            let format = controller
                .recording_format()
                .map(crate::controller::format_label)
                .unwrap_or_else(|| t!("format.gif").to_string());
            t!("recording.encoding", format = format).to_string()
        } else if starting {
            t!("recording.starting").to_string()
        } else if paused {
            let elapsed = format_duration(progress.map(|(elapsed, _)| elapsed).unwrap_or_default());
            t!("recording.paused", elapsed = elapsed).to_string()
        } else {
            progress
                .map(|(elapsed, max)| {
                    let elapsed = format_duration(elapsed);
                    if max.is_zero() {
                        t!("recording.elapsed", elapsed = elapsed).to_string()
                    } else {
                        t!(
                            "recording.progress",
                            elapsed = elapsed,
                            total = format_duration(max)
                        )
                        .to_string()
                    }
                })
                .unwrap_or_else(|| t!("recording.finishing").to_string())
        };
        let theme = cx.theme();
        // Stopping/cancelling while the recorder warms up cancels the start;
        // once encoding owns the file the bar only reports progress.
        let can_act = !encoding;
        let can_pause = controller.can_pause_recording();
        let pause_hint = if paused {
            t!("recording.resume").to_string()
        } else {
            t!("recording.pause").to_string()
        };
        let cancel_hint = t!("recording.cancel").to_string();
        let stop_hint = if encoding {
            t!("recording.busy").to_string()
        } else {
            t!("recording.stop_hint").to_string()
        };
        let stop_label = t!("recording.stop").to_string();

        div()
            .id("recording-bar")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .on_action(cx.listener(|this, _: &StopRecording, _, cx| {
                if !this.controller.read(cx).is_encoding_recording() {
                    this.stop(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CancelRecording, _, cx| {
                if !this.controller.read(cx).is_encoding_recording() {
                    this.cancel(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &TogglePauseRecording, _, cx| {
                if this.controller.read(cx).can_pause_recording() {
                    this.toggle_pause(cx);
                }
            }))
            .child(
                h_flex()
                    .size_full()
                    .px_2()
                    .gap_2()
                    .justify_between()
                    .rounded(theme.radius)
                    .bg(theme.popover)
                    .text_color(theme.popover_foreground)
                    .text_sm()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| this.drag(window, cx)),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().size_2().rounded_full().bg(if encoding || paused {
                                theme.warning
                            } else {
                                theme.danger
                            }))
                            .child(label),
                    )
                    // The buttons keep their clicks: the press must not start
                    // a window drag.
                    .child(
                        h_flex()
                            .gap_1()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(
                                Button::new("pause-recording")
                                    .small()
                                    .ghost()
                                    .icon(if paused {
                                        LucideIcon::Play
                                    } else {
                                        LucideIcon::Pause
                                    })
                                    .tooltip(pause_hint.clone())
                                    .accessibility_label(pause_hint)
                                    .disabled(!can_pause)
                                    .on_click(cx.listener(|this, _, _, cx| this.toggle_pause(cx))),
                            )
                            .child(
                                Button::new("cancel-recording")
                                    .small()
                                    .ghost()
                                    .icon(LucideIcon::CircleX)
                                    .tooltip(cancel_hint.clone())
                                    .accessibility_label(cancel_hint)
                                    .disabled(!can_act)
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                            )
                            .child(
                                Button::new("stop-recording")
                                    .small()
                                    .icon(LucideIcon::CircleStop)
                                    .tooltip(stop_hint)
                                    .accessibility_label(stop_label)
                                    .disabled(!can_act)
                                    .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                            ),
                    ),
            )
    }
}
