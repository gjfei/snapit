//! Clipboard helpers shared by the history panel and finished recordings.

use std::path::Path;

use gpui_kit::{App, ClipboardItem, Image, ImageFormat};

/// Put an on-disk PNG / GIF on the clipboard as image data.
pub(crate) fn copy_image_file(path: &Path, format: ImageFormat, cx: &App) -> anyhow::Result<()> {
    let bytes = std::fs::read(path)?;
    cx.write_to_clipboard(ClipboardItem::new_image(&Image::from_bytes(format, bytes)));
    Ok(())
}

/// What actually ended up on the clipboard, so callers can describe it honestly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileCopy {
    /// A real file reference (`fileURL` / `CF_HDROP`): paste inserts the file.
    File,
    /// Only the path as text, because the native file reference failed.
    PathText,
}

/// Put a file on the clipboard so Finder / Explorer / chat apps paste the file
/// itself. Both platform backends ignore `ClipboardEntry::ExternalPaths` on
/// write, so the pasteboard (`fileURL`) / clipboard (`CF_HDROP`) is filled
/// directly. Returns `PathText` when only the path text could be written.
pub(crate) fn copy_file_reference(path: &Path, cx: &App) -> anyhow::Result<FileCopy> {
    #[cfg(target_os = "macos")]
    let native = copy_file_reference_macos(path);
    #[cfg(target_os = "windows")]
    let native = copy_file_reference_windows(path);
    // Unsupported targets are rejected by `main.rs`, but keep the function total
    // so adding a platform cannot turn into a missing-return type error.
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let native: anyhow::Result<()> = Err(anyhow::anyhow!(
        "no native file-reference clipboard backend"
    ));

    match native {
        Ok(()) => Ok(FileCopy::File),
        Err(err) => {
            // Windows may already have emptied the clipboard before it failed,
            // so always put something back and report what was actually written.
            log::warn!("clipboard file reference failed, copying the path as text: {err:#}");
            cx.write_to_clipboard(ClipboardItem::new_string(path.display().to_string()));
            Ok(FileCopy::PathText)
        }
    }
}

/// Explorer and chat apps read dropped files from `CF_HDROP`; the registered
/// "PNG"/"GIF" formats GPUI writes are images, not file references.
#[cfg(target_os = "windows")]
fn copy_file_reference_windows(path: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;

    use windows_sys::Win32::Foundation::{GlobalFree, POINT};
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalUnlock,
    };
    use windows_sys::Win32::System::Ole::CF_HDROP;
    use windows_sys::Win32::UI::Shell::DROPFILES;

    // The file list is double-null-terminated UTF-16.
    let mut names: Vec<u16> = path.as_os_str().encode_wide().collect();
    names.push(0);
    names.push(0);
    let header = std::mem::size_of::<DROPFILES>();
    let size = header + names.len() * std::mem::size_of::<u16>();

    let mut opened = false;
    for _ in 0..5 {
        // The clipboard is a global resource other apps hold briefly.
        if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
            opened = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    anyhow::ensure!(opened, "OpenClipboard failed");

    let result = (|| -> anyhow::Result<()> {
        anyhow::ensure!(unsafe { EmptyClipboard() } != 0, "EmptyClipboard failed");
        let mem = unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, size) };
        anyhow::ensure!(!mem.is_null(), "GlobalAlloc failed");
        let ptr = unsafe { GlobalLock(mem) };
        if ptr.is_null() {
            unsafe { GlobalFree(mem) };
            anyhow::bail!("GlobalLock failed");
        }
        let dropfiles = DROPFILES {
            pFiles: header as u32,
            pt: POINT { x: 0, y: 0 },
            fNC: 0,
            fWide: 1,
        };
        unsafe {
            std::ptr::write(ptr as *mut DROPFILES, dropfiles);
            std::ptr::copy_nonoverlapping(
                names.as_ptr(),
                (ptr as *mut u8).add(header) as *mut u16,
                names.len(),
            );
            GlobalUnlock(mem);
            if SetClipboardData(CF_HDROP as u32, mem).is_null() {
                GlobalFree(mem);
                anyhow::bail!("SetClipboardData(CF_HDROP) failed");
            }
        }
        // SetClipboardData succeeded — the clipboard owns the memory now.
        Ok(())
    })();
    unsafe { CloseClipboard() };
    result
}

#[cfg(target_os = "macos")]
fn copy_file_reference_macos(path: &Path) -> anyhow::Result<()> {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    use objc2_foundation::{NSArray, NSString, NSURL};

    // Spawning `osascript` for this took ~230 ms per copy and ran on the main
    // thread, which delayed every MP4 completion. The pasteboard API is a
    // sub-millisecond call and needs no extra dependency.
    let class = AnyClass::get(c"NSPasteboard")
        .ok_or_else(|| anyhow::anyhow!("NSPasteboard unavailable"))?;
    let pasteboard: *mut AnyObject = unsafe { msg_send![class, generalPasteboard] };
    anyhow::ensure!(!pasteboard.is_null(), "no general pasteboard");
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    let objects = NSArray::from_retained_slice(&[url]);
    unsafe {
        let _: isize = msg_send![pasteboard, clearContents];
        let written: Bool = msg_send![pasteboard, writeObjects: &*objects];
        anyhow::ensure!(
            written == Bool::YES,
            "NSPasteboard refused {}",
            path.display()
        );
    }
    Ok(())
}
