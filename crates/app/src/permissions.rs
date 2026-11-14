//! System permissions needed for capturing and element selection.
//!
//! macOS gates screen capture behind Screen Recording and element hit-testing
//! behind Accessibility. Windows needs neither (UIA/MSAA is always available).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Permission {
    ScreenRecording,
    Accessibility,
}

impl Permission {
    pub fn label(self) -> String {
        match self {
            Self::ScreenRecording => t!("permission.screen_recording").to_string(),
            Self::Accessibility => t!("permission.accessibility").to_string(),
        }
    }

    pub fn hint(self) -> String {
        match self {
            Self::ScreenRecording => t!("permission.screen_recording.desc").to_string(),
            Self::Accessibility => t!("permission.accessibility.desc").to_string(),
        }
    }
}

/// Permissions that block or degrade a capture.
pub fn missing(element_snap: bool) -> Vec<Permission> {
    let mut missing = Vec::new();
    if !screen_recording_granted() {
        missing.push(Permission::ScreenRecording);
    }
    if element_snap && element_snap_supported() && !accessibility_granted() {
        missing.push(Permission::Accessibility);
    }
    missing
}

/// Element hit-testing needs a platform accessibility API.
pub fn element_snap_supported() -> bool {
    cfg!(any(target_os = "macos", target_os = "windows"))
}

pub fn screen_recording_granted() -> bool {
    imp::screen_recording_granted()
}

pub fn accessibility_granted() -> bool {
    snapit_capture::accessibility_available()
}

/// Trigger the OS permission prompt. Safe to call when already granted.
pub fn request(permission: Permission) {
    match permission {
        Permission::ScreenRecording => imp::request_screen_recording(),
        Permission::Accessibility => snapit_capture::request_accessibility(),
    }
}

/// Open the system pane where the user can toggle this permission.
pub fn open_settings(permission: Permission) {
    imp::open_settings(permission);
}

#[cfg(target_os = "macos")]
mod imp {
    use std::process::Command;

    use super::Permission;

    const SCREEN_RECORDING_PANE: &str =
        "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";
    const ACCESSIBILITY_PANE: &str =
        "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

    pub fn screen_recording_granted() -> bool {
        objc2_core_graphics::CGPreflightScreenCaptureAccess()
    }

    pub fn request_screen_recording() {
        objc2_core_graphics::CGRequestScreenCaptureAccess();
    }

    pub fn open_settings(permission: Permission) {
        let pane = match permission {
            Permission::ScreenRecording => SCREEN_RECORDING_PANE,
            Permission::Accessibility => ACCESSIBILITY_PANE,
        };
        if let Err(err) = Command::new("open").arg(pane).spawn() {
            log::warn!("failed to open System Settings: {err}");
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::Permission;

    pub fn screen_recording_granted() -> bool {
        true
    }

    pub fn request_screen_recording() {}

    pub fn open_settings(_: Permission) {}
}
