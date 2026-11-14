//! Accessibility hit-testing for element-level auto selection.
//!
//! [`at`] returns the element rectangles under a global point, smallest first,
//! in global monitor units. macOS uses the Accessibility API on the owning
//! process; Windows uses MSAA.

use crate::{Region, WindowInfo};

/// How useful an accessibility hit is as a snap target.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ElementRole {
    /// Buttons, fields, links, images, cells — what users usually mean to grab.
    Control,
    /// Static text runs and labels.
    Text,
    /// Groups, scroll areas, toolbars, window chrome.
    Container,
    #[default]
    Unknown,
}

/// One rectangle in the hit-test chain: the element under the pointer first,
/// then its ancestors.
#[derive(Clone, Copy, Debug)]
pub struct ElementHit {
    pub region: Region,
    pub role: ElementRole,
}

#[cfg(target_os = "macos")]
pub use imp::{accessibility_available, request_accessibility};

#[cfg(not(target_os = "macos"))]
pub fn accessibility_available() -> bool {
    cfg!(target_os = "windows")
}

#[cfg(not(target_os = "macos"))]
pub fn request_accessibility() {}

pub(crate) fn at(window: WindowInfo, x: f32, y: f32) -> Vec<ElementHit> {
    imp::at(window, x, y)
}

/// Shared cleanup: keep only sane rectangles that still touch the window.
#[cfg(any(target_os = "macos", target_os = "windows", test))]
fn keep(region: Region, window: Region) -> bool {
    if region.is_empty() {
        return false;
    }
    let window_area = window.width * window.height;
    let area = region.width * region.height;
    if window_area > 0.0 && area > window_area * 1.5 {
        return false;
    }
    region.x < window.x + window.width
        && region.x + region.width > window.x
        && region.y < window.y + window.height
        && region.y + region.height > window.y
}

#[cfg(any(target_os = "macos", target_os = "windows", test))]
fn same(a: Region, b: Region) -> bool {
    (a.x - b.x).abs() < 0.5
        && (a.y - b.y).abs() < 0.5
        && (a.width - b.width).abs() < 0.5
        && (a.height - b.height).abs() < 0.5
}

#[cfg(target_os = "macos")]
mod imp {
    use std::{ffi::c_void, ptr};

    use objc2_core_foundation::{CFBoolean, CFDictionary, CFRetained, CFString, kCFBooleanTrue};

    use super::{ElementHit, ElementRole, Region, WindowInfo, keep, same};

    type AXUIElementRef = *const c_void;
    type CFTypeRef = *const c_void;

    #[repr(C)]
    #[derive(Default)]
    struct CGPoint {
        x: f64,
        y: f64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct CGSize {
        width: f64,
        height: f64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct CGRect {
        origin: CGPoint,
        size: CGSize,
    }

    const AX_VALUE_CG_POINT: u32 = 1;
    const AX_VALUE_CG_SIZE: u32 = 2;
    const AX_VALUE_CG_RECT: u32 = 3;
    const AX_ERROR_SUCCESS: i32 = 0;

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
        fn AXUIElementCopyAttributeValue(
            element: AXUIElementRef,
            attribute: *const c_void,
            value: *mut CFTypeRef,
        ) -> i32;
        fn AXUIElementCopyElementAtPosition(
            application: AXUIElementRef,
            x: f32,
            y: f32,
            element: *mut AXUIElementRef,
        ) -> i32;
        fn AXValueGetValue(value: CFTypeRef, kind: u32, out: *mut c_void) -> u8;
        fn AXIsProcessTrusted() -> u8;
        fn AXIsProcessTrustedWithOptions(options: *const c_void) -> u8;
        fn CFRelease(value: CFTypeRef);
    }

    /// Core Foundation values returned by the AX functions are +1.
    struct Owned(CFTypeRef);

    impl Owned {
        fn new(value: CFTypeRef) -> Option<Self> {
            (!value.is_null()).then_some(Self(value))
        }

        fn get(&self) -> CFTypeRef {
            self.0
        }
    }

    impl Drop for Owned {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CFRelease(self.0) };
            }
        }
    }

    fn copy_attr(element: AXUIElementRef, attribute: &str) -> Option<Owned> {
        let key = CFString::from_str(attribute);
        let mut value: CFTypeRef = ptr::null();
        let error = unsafe {
            AXUIElementCopyAttributeValue(
                element,
                CFRetained::as_ptr(&key).cast().as_ptr(),
                &mut value,
            )
        };
        (error == AX_ERROR_SUCCESS)
            .then(|| Owned::new(value))
            .flatten()
    }

    fn string_attr(element: AXUIElementRef, attribute: &str) -> Option<String> {
        let value = copy_attr(element, attribute)?;
        let string = unsafe { &*(value.get() as *const CFString) };
        Some(string.to_string())
    }

    fn point_attr(element: AXUIElementRef, attribute: &str) -> Option<CGPoint> {
        let value = copy_attr(element, attribute)?;
        let mut point = CGPoint::default();
        let ok = unsafe {
            AXValueGetValue(
                value.get(),
                AX_VALUE_CG_POINT,
                (&mut point as *mut CGPoint).cast(),
            )
        };
        (ok != 0).then_some(point)
    }

    fn size_attr(element: AXUIElementRef, attribute: &str) -> Option<CGSize> {
        let value = copy_attr(element, attribute)?;
        let mut size = CGSize::default();
        let ok = unsafe {
            AXValueGetValue(
                value.get(),
                AX_VALUE_CG_SIZE,
                (&mut size as *mut CGSize).cast(),
            )
        };
        (ok != 0).then_some(size)
    }

    fn rect_attr(element: AXUIElementRef, attribute: &str) -> Option<CGRect> {
        let value = copy_attr(element, attribute)?;
        let mut rect = CGRect::default();
        let ok = unsafe {
            AXValueGetValue(
                value.get(),
                AX_VALUE_CG_RECT,
                (&mut rect as *mut CGRect).cast(),
            )
        };
        (ok != 0).then_some(rect)
    }

    fn element_region(element: AXUIElementRef) -> Option<Region> {
        // AXFrame packs position and size into one accessibility call; only
        // fall back to the separate attributes when it is missing or empty.
        if let Some(frame) = rect_attr(element, "AXFrame") {
            let region = Region::new(
                frame.origin.x as f32,
                frame.origin.y as f32,
                frame.size.width as f32,
                frame.size.height as f32,
            );
            if !region.is_empty() {
                return Some(region);
            }
        }
        let (origin, size) = (
            point_attr(element, "AXPosition")?,
            size_attr(element, "AXSize")?,
        );
        let region = Region::new(
            origin.x as f32,
            origin.y as f32,
            size.width as f32,
            size.height as f32,
        );
        (!region.is_empty()).then_some(region)
    }

    fn role_of(ax_role: Option<&str>) -> ElementRole {
        match ax_role {
            Some(
                "AXButton"
                | "AXLink"
                | "AXMenuItem"
                | "AXMenuBarItem"
                | "AXTextField"
                | "AXTextArea"
                | "AXSearchField"
                | "AXImage"
                | "AXCell"
                | "AXListItem"
                | "AXRow"
                | "AXCheckBox"
                | "AXRadioButton"
                | "AXPopUpButton"
                | "AXComboBox"
                | "AXSlider"
                | "AXTab"
                | "AXDisclosureTriangle"
                | "AXMenuButton"
                | "AXSegmentedControl"
                | "AXColorWell"
                | "AXDateField"
                | "AXIncrementor"
                | "AXSwitch"
                | "AXToggle",
            ) => ElementRole::Control,
            Some("AXStaticText") => ElementRole::Text,
            Some(
                "AXGroup" | "AXScrollArea" | "AXToolbar" | "AXSplitter" | "AXSplitGroup"
                | "AXTable" | "AXOutline" | "AXList" | "AXMenu" | "AXMenuBar" | "AXWindow"
                | "AXSheet" | "AXDrawer" | "AXPopover" | "AXLayoutArea" | "AXLayoutItem"
                | "AXWebArea" | "AXApplication",
            ) => ElementRole::Container,
            _ => ElementRole::Unknown,
        }
    }

    pub fn accessibility_available() -> bool {
        unsafe { AXIsProcessTrusted() != 0 }
    }

    /// Ask macOS to show the Accessibility permission prompt. The system only
    /// prompts while the app is untrusted; otherwise this is a no-op.
    pub fn request_accessibility() {
        let key = CFString::from_str("AXTrustedCheckOptionPrompt");
        let Some(value) = (unsafe { kCFBooleanTrue }) else {
            return;
        };
        let options = CFDictionary::<CFString, CFBoolean>::from_slices(&[&*key], &[value]);
        unsafe {
            AXIsProcessTrustedWithOptions(CFRetained::as_ptr(&options).cast().as_ptr());
        }
    }

    pub fn at(window: WindowInfo, x: f32, y: f32) -> Vec<ElementHit> {
        if window.pid == 0 || !accessibility_available() {
            return Vec::new();
        }
        unsafe {
            let Some(application) = Owned::new(AXUIElementCreateApplication(window.pid as i32))
            else {
                return Vec::new();
            };
            let mut hit: AXUIElementRef = ptr::null();
            if AXUIElementCopyElementAtPosition(application.get(), x, y, &mut hit)
                != AX_ERROR_SUCCESS
                || hit.is_null()
            {
                return Vec::new();
            }
            let mut elements: Vec<ElementHit> = Vec::new();
            let mut current = Owned(hit);
            // The hit element first, then its parents up to the window.
            for _ in 0..64 {
                let element = current.get();
                let ax_role = string_attr(element, "AXRole");
                if ax_role.as_deref() == Some("AXApplication") {
                    break;
                }
                if let Some(region) = element_region(element)
                    && keep(region, window.region)
                    && !elements.iter().any(|kept| same(kept.region, region))
                {
                    elements.push(ElementHit {
                        region,
                        role: role_of(ax_role.as_deref()),
                    });
                }
                if ax_role.as_deref() == Some("AXWindow") {
                    break;
                }
                match copy_attr(element, "AXParent") {
                    Some(parent) => current = parent,
                    None => break,
                }
            }
            elements
        }
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use std::{ffi::c_void, ptr, sync::Once};

    use windows::{
        Win32::{
            Foundation::HWND,
            System::{
                Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, IDispatch},
                Variant::{VARIANT, VT_I4, VariantClear},
            },
            UI::Accessibility::{AccessibleObjectFromWindow, IAccessible},
        },
        core::Interface,
    };

    use super::{ElementHit, ElementRole, Region, WindowInfo, keep, same};

    /// `OBJID_CLIENT`, the accessible object for a window's client area.
    const OBJID_CLIENT: u32 = 0xFFFF_FFFC;

    /// MSAA role ids (`oleacc.h`) grouped by snap usefulness.
    fn role_of(id: i32) -> ElementRole {
        match id as u32 {
            0x2B | 0x1E | 0x0C | 0x2A | 0x28 | 0x1D | 0x22 | 0x1C | 0x2C | 0x2D | 0x2E | 0x2F
            | 0x33 | 0x25 | 0x24 | 0x34 | 0x38 | 0x39 | 0x3A | 0x32 => ElementRole::Control,
            0x29 => ElementRole::Text,
            0x02 | 0x03 | 0x09 | 0x0A | 0x0B | 0x0E | 0x0F | 0x10 | 0x12 | 0x14 | 0x16 | 0x17
            | 0x18 | 0x19 | 0x1A | 0x1B | 0x21 | 0x23 | 0x3B | 0x3C => ElementRole::Container,
            _ => ElementRole::Unknown,
        }
    }

    fn element_role(accessible: &IAccessible, child: &VARIANT) -> ElementRole {
        let Ok(value) = (unsafe { accessible.get_accRole(child) }) else {
            return ElementRole::Unknown;
        };
        let value = OwnedVariant(value);
        // Most providers return a `VT_I4` role id; string roles are ignored.
        match i32::try_from(&value.0) {
            Ok(id) => role_of(id),
            Err(_) => ElementRole::Unknown,
        }
    }

    fn ensure_com() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        });
    }

    fn child_self() -> VARIANT {
        let mut variant = VARIANT::default();
        unsafe {
            let inner = &mut *variant.Anonymous.Anonymous;
            inner.vt = VT_I4;
            inner.Anonymous.lVal = 0;
        }
        variant
    }

    /// A `VARIANT` returned by MSAA owns strings/objects until cleared.
    struct OwnedVariant(VARIANT);

    impl Drop for OwnedVariant {
        fn drop(&mut self) {
            unsafe {
                let _ = VariantClear(&mut self.0);
            }
        }
    }

    fn location(accessible: &IAccessible, child: &VARIANT) -> Option<Region> {
        let (mut x, mut y, mut width, mut height) = (0, 0, 0, 0);
        unsafe {
            accessible
                .accLocation(&mut x, &mut y, &mut width, &mut height, child)
                .ok()?;
        }
        (width > 0 && height > 0)
            .then(|| Region::new(x as f32, y as f32, width as f32, height as f32))
    }

    fn parent(accessible: &IAccessible) -> Option<IAccessible> {
        let parent: IDispatch = unsafe { accessible.accParent().ok()? };
        parent.cast().ok()
    }

    pub fn at(window: WindowInfo, x: f32, y: f32) -> Vec<ElementHit> {
        if window.id == 0 {
            return Vec::new();
        }
        ensure_com();
        let hwnd = HWND(window.id as usize as *mut c_void);
        unsafe {
            let mut raw: *mut c_void = ptr::null_mut();
            if AccessibleObjectFromWindow(hwnd, OBJID_CLIENT, &IAccessible::IID, &mut raw).is_err()
                || raw.is_null()
            {
                return Vec::new();
            }
            let root: IAccessible = Interface::from_raw(raw);
            let Ok(hit) = root.accHitTest(x.round() as i32, y.round() as i32) else {
                return Vec::new();
            };
            let hit = OwnedVariant(hit);
            let mut elements = Vec::new();
            // The hit result may be a child id or an object; `accLocation`
            // resolves either against `root`.
            if let Some(region) = location(&root, &hit.0)
                && keep(region, window.region)
            {
                elements.push(ElementHit {
                    region,
                    role: element_role(&root, &hit.0),
                });
            }
            let mut current = parent(&root);
            for _ in 0..64 {
                let Some(accessible) = current else {
                    break;
                };
                let Some(region) = location(&accessible, &child_self()) else {
                    break;
                };
                if !keep(region, window.region) {
                    break;
                }
                if !elements.iter().any(|kept| same(kept.region, region)) {
                    elements.push(ElementHit {
                        region,
                        role: element_role(&accessible, &child_self()),
                    });
                }
                current = parent(&accessible);
            }
            elements
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod imp {
    use super::ElementHit;
    use crate::WindowInfo;

    pub fn at(_: WindowInfo, _: f32, _: f32) -> Vec<ElementHit> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{Region, WindowInfo, keep, same};

    #[test]
    fn keeps_only_rectangles_inside_the_window() {
        let window = Region::new(100.0, 100.0, 200.0, 200.0);
        assert!(keep(Region::new(120.0, 120.0, 40.0, 20.0), window));
        assert!(!keep(Region::new(0.0, 0.0, 10.0, 10.0), window));
        assert!(!keep(Region::new(100.0, 100.0, 2000.0, 2000.0), window));
        assert!(!keep(Region::new(0.0, 0.0, 0.0, 0.0), window));
    }

    #[test]
    fn dedupes_near_identical_rectangles() {
        let window = WindowInfo {
            region: Region::new(0.0, 0.0, 100.0, 100.0),
            id: 1,
            pid: 2,
        };
        assert_eq!(window.region, Region::new(0.0, 0.0, 100.0, 100.0));
        assert!(same(
            Region::new(1.0, 2.0, 3.0, 4.0),
            Region::new(1.2, 2.1, 3.2, 4.2),
        ));
        assert!(!same(
            Region::new(1.0, 2.0, 3.0, 4.0),
            Region::new(1.0, 2.0, 5.0, 4.0),
        ));
    }
}
