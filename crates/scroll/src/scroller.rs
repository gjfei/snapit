//! Platform mouse-wheel injection and user-intervention detection for
//! automatic scrolling.

use std::time::Duration;

use crate::stitch::ScrollDirection;

#[cfg(target_os = "windows")]
use windows::Win32::Foundation::LPARAM;

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
    imp::cursor_position().is_some_and(|(x, y)| post_scroll_down_at(x, y, pixels, notches))
}

pub fn post_scroll_at(
    direction: ScrollDirection,
    x: f32,
    y: f32,
    pixels: i32,
    notches: i32,
) -> bool {
    imp::post_scroll_at(direction, x, y, pixels, notches)
}

pub fn post_scroll_down_at(x: f32, y: f32, pixels: i32, notches: i32) -> bool {
    post_scroll_at(ScrollDirection::Down, x, y, pixels, notches)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "windows")]
    #[test]
    fn scroll_delta_follows_native_wheel_sign() {
        assert_eq!(scroll_delta(ScrollDirection::Up, 2), 240);
        assert_eq!(scroll_delta(ScrollDirection::Down, 2), -240);
        assert_eq!(scroll_delta(ScrollDirection::Up, 0), 0);
        assert_eq!(scroll_delta(ScrollDirection::Down, i32::MIN), -2_654_000);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn pixel_scroll_uses_native_wheel_sign() {
        assert_eq!(signed_scroll_pixels(ScrollDirection::Up, 24), 24);
        assert_eq!(signed_scroll_pixels(ScrollDirection::Down, 24), -24);
    }
}

#[cfg(target_os = "windows")]
fn scroll_delta(direction: ScrollDirection, notches: i32) -> i32 {
    let magnitude = notches.unsigned_abs().min(i16::MAX as u32 / 120) * 120;
    if direction == ScrollDirection::Up {
        magnitude as i32
    } else {
        -(magnitude as i32)
    }
}

#[cfg(not(target_os = "windows"))]
fn signed_scroll_pixels(direction: ScrollDirection, pixels: i32) -> i32 {
    let magnitude = pixels.unsigned_abs().min(i32::MAX as u32) as i32;
    if direction == ScrollDirection::Up {
        magnitude
    } else {
        -magnitude
    }
}

#[cfg(target_os = "windows")]
fn wheel_lparam(x: f32, y: f32) -> LPARAM {
    fn coordinate(value: f32) -> u16 {
        value.round().clamp(i32::MIN as f32, i32::MAX as f32) as i32 as u16
    }
    LPARAM(((coordinate(y) as usize) << 16 | coordinate(x) as usize) as isize)
}

#[cfg(test)]
#[cfg(target_os = "windows")]
mod windows_tests {
    use super::*;

    #[test]
    fn wheel_coordinates_preserve_signed_virtual_desktop_positions() {
        assert_eq!(
            wheel_lparam(-10.0, 40_000.0).0 as u32,
            (40_000 << 16) | 65_526
        );
        assert_eq!(
            wheel_lparam(40_000.0, -10.0).0 as u32,
            (65_526 << 16) | 40_000
        );
    }
}

pub(crate) struct ScrollInputGuard {
    original_cursor: Option<(f32, f32)>,
    moved: bool,
}

impl ScrollInputGuard {
    pub(crate) fn new() -> Self {
        Self {
            original_cursor: imp::cursor_position(),
            moved: false,
        }
    }

    pub(crate) fn move_to(&mut self, x: f32, y: f32) {
        if !self.moved {
            self.original_cursor = imp::cursor_position();
        }
        imp::move_cursor(x, y);
        self.moved = true;
    }

    pub(crate) fn restore(&mut self) {
        if self.moved
            && let Some((x, y)) = self.original_cursor
        {
            imp::move_cursor(x, y);
        }
        self.moved = false;
    }
}

impl Drop for ScrollInputGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// True if the user moved the pointer significantly away from the auto-scroll
/// anchor, pressed any mouse button, or pressed a non-modifier key.
pub fn user_intervened(anchor: (f32, f32)) -> bool {
    if imp::any_button_down() || imp::keyboard_active() {
        return true;
    }
    let Some((x, y)) = imp::cursor_position() else {
        return false;
    };
    (x - anchor.0).abs() > 20.0 || (y - anchor.1).abs() > 20.0
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{ScrollDirection, signed_scroll_pixels};
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
        fn CGEventSourceKeyState(state_id: i32, key: u32) -> bool;
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

    pub fn post_scroll_at(
        direction: ScrollDirection,
        _x: f32,
        _y: f32,
        pixels: i32,
        _notches: i32,
    ) -> bool {
        unsafe {
            let event = CGEventCreateScrollWheelEvent2(
                std::ptr::null(),
                K_CG_SCROLL_EVENT_UNIT_PIXEL,
                1,
                signed_scroll_pixels(direction, pixels),
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

    pub fn keyboard_active() -> bool {
        unsafe {
            (0..128).any(|key| {
                !matches!(key, 54..=61 | 63)
                    && CGEventSourceKeyState(K_CG_EVENT_SOURCE_STATE_COMBINED_SESSION, key)
            })
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
    use super::{ScrollDirection, scroll_delta, wheel_lparam};
    use windows::Win32::{
        Foundation::{HWND, LRESULT, POINT, WPARAM},
        UI::{
            Input::KeyboardAndMouse::{
                GetAsyncKeyState, VK_CONTROL, VK_LBUTTON, VK_LMENU, VK_MBUTTON, VK_RBUTTON,
                VK_RMENU, VK_SHIFT,
            },
            WindowsAndMessaging::{
                GW_HWNDPREV, GetCursorPos, GetTopWindow, GetWindow, GetWindowRect,
                GetWindowThreadProcessId, IsWindowVisible, SMTO_ABORTIFHUNG, SendMessageTimeoutW,
                SetCursorPos, WM_MOUSEWHEEL,
            },
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

    fn window_at_point(x: f32, y: f32) -> Option<HWND> {
        let point = POINT {
            x: x.round() as i32,
            y: y.round() as i32,
        };
        let own_pid = std::process::id();
        unsafe {
            let Ok(mut hwnd) = GetTopWindow(None) else {
                return None;
            };
            loop {
                let mut pid = 0;
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
                let mut rect = windows::Win32::Foundation::RECT::default();
                let contains_point = GetWindowRect(hwnd, &mut rect).is_ok()
                    && point.x >= rect.left
                    && point.x < rect.right
                    && point.y >= rect.top
                    && point.y < rect.bottom;
                if pid != own_pid && IsWindowVisible(hwnd).as_bool() && contains_point {
                    return Some(hwnd);
                }
                hwnd = GetWindow(hwnd, GW_HWNDPREV).ok()?;
            }
        }
    }

    pub fn post_scroll_at(
        direction: ScrollDirection,
        x: f32,
        y: f32,
        _pixels: i32,
        notches: i32,
    ) -> bool {
        let Some(hwnd) = window_at_point(x, y) else {
            return false;
        };
        unsafe {
            SendMessageTimeoutW(
                hwnd,
                WM_MOUSEWHEEL,
                WPARAM(scroll_delta(direction, notches) as u16 as usize),
                wheel_lparam(x, y),
                SMTO_ABORTIFHUNG,
                120,
                None,
            ) != LRESULT(0)
        }
    }

    pub fn keyboard_active() -> bool {
        unsafe {
            let modifiers = [
                i32::from(VK_SHIFT.0),
                i32::from(VK_CONTROL.0),
                i32::from(VK_LMENU.0),
                i32::from(VK_RMENU.0),
            ];
            (8..=254).any(|key| {
                !modifiers.contains(&key) && (GetAsyncKeyState(key) as u16 & 0x8000) != 0
            })
        }
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
    use super::ScrollDirection;

    pub fn cursor_position() -> Option<(f32, f32)> {
        None
    }
    pub fn move_cursor(_x: f32, _y: f32) {}
    pub fn post_scroll_at(
        _direction: ScrollDirection,
        _x: f32,
        _y: f32,
        _pixels: i32,
        _notches: i32,
    ) -> bool {
        false
    }
    pub fn keyboard_active() -> bool {
        false
    }
    pub fn any_button_down() -> bool {
        false
    }
}
