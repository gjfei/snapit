//! On-screen windows for window- and element-level auto selection.
//!
//! [`all`] returns windows in *global* monitor units (the same space as
//! `MonitorInfo::{x,y}`), ordered front-to-back.

use crate::{MonitorInfo, Region};

/// An on-screen window snapshot.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct WindowInfo {
    /// Global monitor units, matching `MonitorInfo::{x,y}`.
    pub region: Region,
    /// Platform window id (CGWindowNumber or HWND).
    pub id: u64,
    /// Owning process id; 0 when the platform does not report one.
    pub pid: u32,
}

impl WindowInfo {
    pub fn new(region: Region, id: u64, pid: u32) -> Self {
        Self { region, id, pid }
    }
}

/// Clip global window rectangles to one monitor and make them monitor-relative.
pub(crate) fn clip_to_monitor(windows: &[WindowInfo], monitor: &MonitorInfo) -> Vec<WindowInfo> {
    let mx = monitor.x as f32;
    let my = monitor.y as f32;
    let mw = monitor.width as f32;
    let mh = monitor.height as f32;
    windows
        .iter()
        .filter_map(|w| {
            let r = w.region;
            let x = r.x.max(mx);
            let y = r.y.max(my);
            let right = (r.x + r.width).min(mx + mw);
            let bottom = (r.y + r.height).min(my + mh);
            (right - x >= 1.0 && bottom - y >= 1.0).then(|| WindowInfo {
                region: Region::new(x - mx, y - my, right - x, bottom - y),
                ..*w
            })
        })
        .collect()
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::c_void;

    use objc2_core_foundation::{CFDictionary, CFNumber, CFNumberType, CFString, CGRect};
    use objc2_core_graphics::{
        CGRectMakeWithDictionaryRepresentation, CGWindowListCopyWindowInfo, CGWindowListOption,
    };

    use crate::{Region, windows::WindowInfo};

    unsafe fn dict_value(dict: &CFDictionary, key: &str) -> Option<*const c_void> {
        let key = CFString::from_str(key);
        let value = unsafe { dict.value((key.as_ref() as *const CFString).cast()) };
        (!value.is_null()).then_some(value)
    }

    unsafe fn dict_i32(dict: &CFDictionary, key: &str) -> Option<i32> {
        let value = unsafe { dict_value(dict, key)? };
        let number = unsafe { &*(value as *const CFNumber) };
        let mut out = 0i32;
        unsafe { number.value(CFNumberType::IntType, (&mut out as *mut i32).cast()) }.then_some(out)
    }

    unsafe fn dict_f64(dict: &CFDictionary, key: &str) -> Option<f64> {
        let value = unsafe { dict_value(dict, key)? };
        let number = unsafe { &*(value as *const CFNumber) };
        let mut out = 0f64;
        unsafe { number.value(CFNumberType::Float64Type, (&mut out as *mut f64).cast()) }
            .then_some(out)
    }

    pub fn all() -> Vec<WindowInfo> {
        // The list is ordered front-to-back; desktop elements are excluded.
        let Some(list) = CGWindowListCopyWindowInfo(
            CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements,
            0,
        ) else {
            return Vec::new();
        };
        let mut windows = Vec::new();
        for index in 0..list.count() {
            let entry = unsafe { list.value_at_index(index) };
            if entry.is_null() {
                continue;
            }
            let dict = unsafe { &*(entry as *const CFDictionary) };
            unsafe {
                // Invisible helpers and capture-excluded windows are not in the
                // frozen frame, so selecting them would capture the wrong pixels.
                if dict_f64(dict, "kCGWindowAlpha").unwrap_or(0.0) <= 0.0 {
                    continue;
                }
                // Only normal application windows. The menu bar, status items,
                // the Dock, and Notification Center report their own layers; the
                // last is a full-screen transparent window that would otherwise
                // sit in front of every app and win the hit test with no
                // selectable elements inside.
                if dict_i32(dict, "kCGWindowLayer").unwrap_or(0) != 0 {
                    continue;
                }
                if dict_i32(dict, "kCGWindowSharingState").unwrap_or(0) == 0 {
                    continue;
                }
                let Some(bounds) = dict_value(dict, "kCGWindowBounds") else {
                    continue;
                };
                let bounds = &*(bounds as *const CFDictionary);
                let mut rect = CGRect::default();
                if !CGRectMakeWithDictionaryRepresentation(Some(bounds), &mut rect) {
                    continue;
                }
                if rect.size.width < 1.0 || rect.size.height < 1.0 {
                    continue;
                }
                windows.push(WindowInfo::new(
                    Region::new(
                        rect.origin.x as f32,
                        rect.origin.y as f32,
                        rect.size.width as f32,
                        rect.size.height as f32,
                    ),
                    dict_f64(dict, "kCGWindowNumber").unwrap_or(0.0) as u64,
                    dict_f64(dict, "kCGWindowOwnerPID").unwrap_or(0.0) as u32,
                ));
            }
        }
        windows
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use windows::{
        Win32::{
            Foundation::{HWND, LPARAM, RECT, TRUE},
            Graphics::Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
            UI::WindowsAndMessaging::{
                EnumWindows, GetClassNameW, GetWindowRect, GetWindowThreadProcessId, IsIconic,
                IsWindowVisible,
            },
        },
        core::BOOL,
    };

    use crate::{Region, windows::WindowInfo};

    unsafe fn window_rect(hwnd: HWND) -> RECT {
        let mut rect = RECT::default();
        // The DWM frame excludes the invisible resize/shadow border.
        let dwm = unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                (&mut rect as *mut RECT).cast(),
                std::mem::size_of::<RECT>() as u32,
            )
        };
        if dwm.is_err() {
            unsafe {
                let _ = GetWindowRect(hwnd, &mut rect);
            }
        }
        rect
    }

    unsafe extern "system" fn enum_window(hwnd: HWND, state: LPARAM) -> BOOL {
        let windows = unsafe { &mut *(state.0 as *mut Vec<WindowInfo>) };
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
                return TRUE;
            }
            let mut class = [0u16; 64];
            let len = GetClassNameW(hwnd, &mut class) as usize;
            let class = String::from_utf16_lossy(&class[..len]);
            if matches!(class.as_str(), "Progman" | "WorkerW") {
                return TRUE;
            }
            let mut cloaked = 0u32;
            let _ = DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                (&mut cloaked as *mut u32).cast(),
                std::mem::size_of::<u32>() as u32,
            );
            if cloaked != 0 {
                return TRUE;
            }
            let rect = window_rect(hwnd);
            if rect.right > rect.left && rect.bottom > rect.top {
                let mut pid = 0u32;
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
                windows.push(WindowInfo::new(
                    Region::new(
                        rect.left as f32,
                        rect.top as f32,
                        (rect.right - rect.left) as f32,
                        (rect.bottom - rect.top) as f32,
                    ),
                    hwnd.0 as usize as u64,
                    pid,
                ));
            }
        }
        TRUE
    }

    pub fn all() -> Vec<WindowInfo> {
        // EnumWindows walks top-level windows front-to-back.
        let mut windows = Vec::new();
        unsafe {
            let _ = EnumWindows(
                Some(enum_window),
                LPARAM(&mut windows as *mut Vec<WindowInfo> as isize),
            );
        }
        windows
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod imp {
    pub fn all() -> Vec<super::WindowInfo> {
        Vec::new()
    }
}

pub(crate) fn all() -> Vec<WindowInfo> {
    imp::all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(x: i32, y: i32, width: u32, height: u32) -> MonitorInfo {
        MonitorInfo {
            id: 1,
            name: String::new(),
            x,
            y,
            width,
            height,
            scale_factor: 1.0,
            is_primary: true,
        }
    }

    fn window(x: f32, y: f32, width: f32, height: f32) -> WindowInfo {
        WindowInfo::new(Region::new(x, y, width, height), 7, 42)
    }

    #[test]
    fn clips_and_relativizes() {
        let monitor = monitor(100, 50, 800, 600);
        let windows = [
            window(150.0, 100.0, 200.0, 150.0), // fully inside
            window(50.0, 0.0, 200.0, 200.0),    // overlaps top-left
            window(0.0, 0.0, 50.0, 50.0),       // fully outside
        ];
        let clipped = clip_to_monitor(&windows, &monitor);
        assert_eq!(
            clipped,
            vec![
                WindowInfo::new(Region::new(50.0, 50.0, 200.0, 150.0), 7, 42),
                WindowInfo::new(Region::new(0.0, 0.0, 150.0, 150.0), 7, 42),
            ]
        );
    }

    #[test]
    fn keeps_front_to_back_order() {
        let monitor = monitor(0, 0, 1000, 1000);
        let windows = [
            window(10.0, 10.0, 100.0, 100.0),
            window(20.0, 20.0, 100.0, 100.0),
        ];
        let clipped = clip_to_monitor(&windows, &monitor);
        assert_eq!(clipped[0].region.x, 10.0);
        assert_eq!(clipped[1].region.x, 20.0);
    }
}
