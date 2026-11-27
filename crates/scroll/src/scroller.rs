//! Platform mouse-wheel injection and user-intervention detection for
//! automatic scrolling.

use std::time::Duration;

use crate::stitch::ScrollDirection;

#[cfg(target_os = "windows")]
use windows::Win32::Foundation::{LPARAM, LRESULT};

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
    #[allow(dead_code)]
    pub(crate) fn profile(self) -> (u32, i32, i32, Duration) {
        match self {
            Self::Slow => (3, 45, 3, Duration::from_millis(260)),
            Self::Medium => (3, 75, 5, Duration::from_millis(180)),
            Self::Fast => (3, 110, 8, Duration::from_millis(120)),
        }
    }

    pub(crate) fn target_viewports_per_sec(self) -> f32 {
        match self {
            Self::Slow => 1.0,
            Self::Medium => 1.8,
            Self::Fast => 3.0,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn target_fraction(self) -> f32 {
        match self {
            Self::Slow => 0.25,
            Self::Medium => 0.38,
            Self::Fast => 0.52,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn plan_step(
        self,
        active_h_px: u32,
        scale: f32,
        gain: f32,
        rewind: bool,
    ) -> StepPlan {
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        let active_pt = (active_h_px as f32 / scale).max(64.0);
        let gain = gain.clamp(0.10, 2.5);
        let target_frac = if rewind { 0.32 } else { self.target_fraction() };
        let settle = if rewind {
            Duration::from_millis(180)
        } else {
            match self {
                Self::Slow => Duration::from_millis(240),
                Self::Medium => Duration::from_millis(170),
                Self::Fast => Duration::from_millis(120),
            }
        };

        let desired_pt = (active_pt * target_frac * gain).clamp(24.0, active_pt * 0.58);
        let mac_pulses = if desired_pt >= 120.0 {
            4
        } else if desired_pt >= 60.0 {
            3
        } else {
            2
        };
        let mac_px_per_pulse = (desired_pt / mac_pulses as f32).round().max(12.0) as i32;

        let desired_notches = ((active_pt * target_frac / 48.0) * gain)
            .round()
            .clamp(1.0, 12.0) as u32;
        let win_notches_per_pulse = (desired_notches / 4).max(1) as i32;
        let win_pulses = (desired_notches / win_notches_per_pulse as u32).clamp(1, 4);

        StepPlan {
            mac_pulses,
            mac_px_per_pulse,
            win_pulses,
            win_notches_per_pulse,
            settle,
        }
    }

    pub(crate) fn plan_continuous(
        self,
        active_h_px: u32,
        scale: f32,
        gain: f32,
        rewind: bool,
    ) -> ContinuousStep {
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        let active_pt = (active_h_px as f32 / scale).max(64.0);
        let gain = gain.clamp(0.10, 2.5);
        let vps = if rewind {
            1.2
        } else {
            self.target_viewports_per_sec()
        };
        let eval_window = if rewind {
            Duration::from_millis(140)
        } else {
            match self {
                Self::Slow => Duration::from_millis(220),
                Self::Medium => Duration::from_millis(160),
                Self::Fast => Duration::from_millis(120),
            }
        };

        let mac_base_ms = if rewind {
            20.0_f32
        } else {
            match self {
                Self::Slow => 16.0,
                Self::Medium => 16.0,
                Self::Fast => 12.0,
            }
        };
        let raw_px = active_pt * vps * gain * (mac_base_ms / 1000.0);
        let max_px = (active_pt * 0.25).max(8.0);
        let (mac_px, mac_interval) = if raw_px < 8.0 {
            let ms = (mac_base_ms * (8.0 / raw_px.max(1.0)))
                .round()
                .clamp(mac_base_ms, 160.0) as u64;
            (8, Duration::from_millis(ms))
        } else {
            (
                raw_px.round().clamp(8.0, max_px) as i32,
                Duration::from_millis(mac_base_ms as u64),
            )
        };

        let win_base_ms = if rewind {
            36.0_f32
        } else {
            match self {
                Self::Slow => 64.0,
                Self::Medium => 36.0,
                Self::Fast => 24.0,
            }
        };
        let raw_notches = (active_pt * vps / 60.0) * gain * (win_base_ms / 1000.0);
        let max_notches = ((active_pt * 0.25) / 60.0).ceil().clamp(1.0, 6.0) as i32;
        let (win_notches, win_interval) = if raw_notches < 1.0 {
            let ms = (win_base_ms / raw_notches.max(0.15))
                .round()
                .clamp(win_base_ms, 240.0) as u64;
            (1, Duration::from_millis(ms))
        } else {
            (
                (raw_notches.round() as i32).clamp(1, max_notches),
                Duration::from_millis(win_base_ms as u64),
            )
        };

        ContinuousStep {
            mac_px,
            mac_interval,
            win_notches,
            win_interval,
            eval_window,
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StepPlan {
    pub mac_pulses: u32,
    pub mac_px_per_pulse: i32,
    pub win_pulses: u32,
    pub win_notches_per_pulse: i32,
    pub settle: Duration,
}

impl StepPlan {
    #[allow(dead_code)]
    pub(crate) fn platform_pulses(self) -> u32 {
        #[cfg(target_os = "windows")]
        {
            self.win_pulses
        }
        #[cfg(not(target_os = "windows"))]
        {
            self.mac_pulses
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ContinuousStep {
    pub mac_px: i32,
    pub mac_interval: Duration,
    pub win_notches: i32,
    pub win_interval: Duration,
    pub eval_window: Duration,
}

impl ContinuousStep {
    pub(crate) fn platform_interval(self) -> Duration {
        #[cfg(target_os = "windows")]
        {
            self.win_interval
        }
        #[cfg(not(target_os = "windows"))]
        {
            self.mac_interval
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
        assert_eq!(scroll_delta(ScrollDirection::Up, i32::MIN), 32_760);
        assert_eq!(scroll_delta(ScrollDirection::Down, i32::MIN), -32_760);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn pixel_scroll_uses_native_wheel_sign() {
        assert_eq!(signed_scroll_pixels(ScrollDirection::Up, 24), 24);
        assert_eq!(signed_scroll_pixels(ScrollDirection::Down, 24), -24);
    }

    #[test]
    fn continuous_short_viewport_caps_mac_px_and_stretches_windows_interval() {
        let step = ScrollSpeed::Fast.plan_continuous(200, 1.0, 1.0, false);
        assert!(step.mac_px <= (200.0_f32 * 0.45).round() as i32);
        assert_eq!(step.win_notches, 1);
        assert!(step.win_interval > Duration::from_millis(16));
    }

    #[test]
    fn continuous_tall_viewport_scales_with_speed_and_gain() {
        let slow = ScrollSpeed::Slow.plan_continuous(1600, 2.0, 1.0, false);
        let medium = ScrollSpeed::Medium.plan_continuous(1600, 2.0, 1.0, false);
        let fast = ScrollSpeed::Fast.plan_continuous(1600, 2.0, 1.0, false);
        let mac_rate = |s: ContinuousStep| s.mac_px as f32 / s.mac_interval.as_secs_f32();
        let win_rate = |s: ContinuousStep| s.win_notches as f32 / s.win_interval.as_secs_f32();
        assert!(mac_rate(slow) < mac_rate(medium));
        assert!(mac_rate(medium) < mac_rate(fast));
        assert!(win_rate(slow) < win_rate(medium));
        assert!(win_rate(medium) < win_rate(fast));

        let damped = ScrollSpeed::Medium.plan_continuous(1600, 2.0, 0.25, false);
        assert!(mac_rate(damped) < mac_rate(medium));
        assert!(win_rate(damped) < win_rate(medium));
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
fn wheel_wparam(direction: ScrollDirection, notches: i32) -> usize {
    // WM_MOUSEWHEEL reads the signed delta from the high 16 bits.
    (scroll_delta(direction, notches) as u16 as usize) << 16
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
    fn wheel_wparam_places_delta_in_the_high_word() {
        let up = wheel_wparam(ScrollDirection::Up, 2) as u32;
        let down = wheel_wparam(ScrollDirection::Down, 2) as u32;
        assert_eq!(up, 240 << 16);
        assert_eq!(down, ((-240_i32) as u16 as u32) << 16);
        assert_eq!(up & 0xffff, 0);
        assert_eq!(down & 0xffff, 0);
    }

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

    #[test]
    fn normal_zero_wheel_result_is_not_a_send_failure() {
        assert!(wheel_result_succeeded(LRESULT(0), 0));
        assert!(!wheel_result_succeeded(LRESULT(0), 5));
        assert!(!wheel_result_succeeded(LRESULT(1), 0));
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
        log::debug!("scroll auto-scroll intervention: mouse button or key active");
        return true;
    }
    let Some((x, y)) = imp::cursor_position() else {
        return false;
    };
    if (x - anchor.0).abs() > 20.0 || (y - anchor.1).abs() > 20.0 {
        log::debug!(
            "scroll auto-scroll intervention: cursor={x},{y} anchor={:?}",
            anchor
        );
        return true;
    }
    false
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
    use super::{ScrollDirection, wheel_lparam, wheel_result_succeeded, wheel_wparam};
    use windows::Win32::{
        Foundation::{GetLastError, HWND, POINT, SetLastError, WIN32_ERROR, WPARAM},
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
            SetLastError(WIN32_ERROR(0));
            let result = SendMessageTimeoutW(
                hwnd,
                WM_MOUSEWHEEL,
                WPARAM(wheel_wparam(direction, notches)),
                wheel_lparam(x, y),
                SMTO_ABORTIFHUNG,
                120,
                None,
            );
            wheel_result_succeeded(result, GetLastError().0)
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

#[cfg(target_os = "windows")]
fn wheel_result_succeeded(result: LRESULT, last_error: u32) -> bool {
    result.0 == 0 && last_error == 0
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
