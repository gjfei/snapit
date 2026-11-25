//! Platform mouse-wheel injection and user-intervention detection for
//! automatic scrolling.

use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScrollSpeed {
    Slow,
    #[default]
    Medium,
    Fast,
}

impl ScrollSpeed {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Slow => "slow",
            Self::Medium => "medium",
            Self::Fast => "fast",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "slow" => Some(Self::Slow),
            "medium" => Some(Self::Medium),
            "fast" => Some(Self::Fast),
            _ => None,
        }
    }

    /// `(pulses_per_step, mac_pixels_per_pulse, win_notches_per_pulse, settle_duration)`.
    pub(crate) fn profile(self) -> (u32, i32, i32, Duration) {
        match self {
            Self::Slow => (3, 45, 3, Duration::from_millis(260)),
            Self::Medium => (3, 75, 5, Duration::from_millis(180)),
            Self::Fast => (3, 110, 8, Duration::from_millis(120)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoScrollError {
    /// macOS requires Accessibility permission before synthetic scroll events
    /// reach other applications.
    NeedsAccessibility,
    PlatformFailed,
}

impl std::fmt::Display for AutoScrollError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NeedsAccessibility => f.write_str("accessibility permission is required"),
            Self::PlatformFailed => f.write_str("failed to post synthetic scroll event"),
        }
    }
}

impl std::error::Error for AutoScrollError {}

pub fn auto_scroll_available() -> bool {
    #[cfg(target_os = "macos")]
    {
        snapit_capture::accessibility_available()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

pub fn cursor_position() -> Option<(f32, f32)> {
    imp::cursor_position()
}

pub fn move_cursor(x: f32, y: f32) {
    imp::move_cursor(x, y);
}

pub fn post_scroll_down(pixels: i32, notches: i32) -> bool {
    imp::post_scroll_down(pixels, notches)
}

/// True if the user moved the pointer significantly away from the auto-scroll
/// anchor or pressed any mouse button.
pub fn user_intervened(anchor: (f32, f32)) -> bool {
    if imp::any_button_down() {
        return true;
    }
    let Some((x, y)) = imp::cursor_position() else {
        return false;
    };
    (x - anchor.0).abs() > 20.0 || (y - anchor.1).abs() > 20.0
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    const K_CG_HID_EVENT_TAP: u32 = 0;
    const K_CG_EVENT_SOURCE_STATE_COMBINED_SESSION: i32 = 0;
    const K_CG_SCROLL_EVENT_UNIT_PIXEL: u32 = 0;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventCreate(source: *const c_void) -> *mut c_void;
        fn CGEventGetLocation(event: *const c_void) -> CGPoint;
        fn CGEventSourceButtonState(state_id: i32, button: u32) -> bool;
        fn CGWarpMouseCursorPosition(new_cursor_position: CGPoint) -> i32;
        fn CGEventCreateScrollWheelEvent2(
            source: *const c_void,
            units: u32,
            wheel_count: u32,
            wheel1: i32,
            wheel2: i32,
            wheel3: i32,
        ) -> *mut c_void;
        fn CGEventPost(tap: u32, event: *mut c_void);
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(cf: *const c_void);
    }

    pub fn cursor_position() -> Option<(f32, f32)> {
        unsafe {
            let event = CGEventCreate(std::ptr::null());
            if event.is_null() {
                return None;
            }
            let pt = CGEventGetLocation(event);
            CFRelease(event);
            Some((pt.x as f32, pt.y as f32))
        }
    }

    pub fn move_cursor(x: f32, y: f32) {
        unsafe {
            let _ = CGWarpMouseCursorPosition(CGPoint {
                x: f64::from(x),
                y: f64::from(y),
            });
        }
    }

    pub fn post_scroll_down(pixels: i32, _notches: i32) -> bool {
        unsafe {
            // Negative delta scrolls downward on macOS.
            let event = CGEventCreateScrollWheelEvent2(
                std::ptr::null(),
                K_CG_SCROLL_EVENT_UNIT_PIXEL,
                1,
                -pixels.abs(),
                0,
                0,
            );
            if event.is_null() {
                return false;
            }
            CGEventPost(K_CG_HID_EVENT_TAP, event);
            CFRelease(event);
            true
        }
    }

    pub fn any_button_down() -> bool {
        unsafe {
            CGEventSourceButtonState(K_CG_EVENT_SOURCE_STATE_COMBINED_SESSION, 0)
                || CGEventSourceButtonState(K_CG_EVENT_SOURCE_STATE_COMBINED_SESSION, 1)
                || CGEventSourceButtonState(K_CG_EVENT_SOURCE_STATE_COMBINED_SESSION, 2)
        }
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use windows::Win32::{
        Foundation::POINT,
        UI::{
            Input::KeyboardAndMouse::{
                GetAsyncKeyState, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_WHEEL, MOUSEINPUT,
                SendInput, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON,
            },
            WindowsAndMessaging::{GetCursorPos, SetCursorPos},
        },
    };

    pub fn cursor_position() -> Option<(f32, f32)> {
        let mut pt = POINT::default();
        if unsafe { GetCursorPos(&mut pt) }.is_ok() {
            Some((pt.x as f32, pt.y as f32))
        } else {
            None
        }
    }

    pub fn move_cursor(x: f32, y: f32) {
        unsafe {
            let _ = SetCursorPos(x.round() as i32, y.round() as i32);
        }
    }

    pub fn post_scroll_down(_pixels: i32, notches: i32) -> bool {
        let delta = -notches.abs() * 120;
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: delta as u32,
                    dwFlags: MOUSEEVENTF_WHEEL,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) == 1 }
    }

    pub fn any_button_down() -> bool {
        unsafe {
            (GetAsyncKeyState(i32::from(VK_LBUTTON.0)) as u16 & 0x8000) != 0
                || (GetAsyncKeyState(i32::from(VK_RBUTTON.0)) as u16 & 0x8000) != 0
                || (GetAsyncKeyState(i32::from(VK_MBUTTON.0)) as u16 & 0x8000) != 0
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod imp {
    pub fn cursor_position() -> Option<(f32, f32)> {
        None
    }
    pub fn move_cursor(_x: f32, _y: f32) {}
    pub fn post_scroll_down(_pixels: i32, _notches: i32) -> bool {
        false
    }
    pub fn any_button_down() -> bool {
        false
    }
}
