//! macOS freeze-frame capture.
//!
//! `xcap` copies the whole display image twice and swaps channels on one
//! thread. Asking CoreGraphics for the display directly lets the BGRA buffer it
//! returns be converted into RGBA in the same pass, which keeps the overlay
//! mapping sooner.

use anyhow::{Result, anyhow};
use image::RgbaImage;
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGDataProvider, CGImage, CGWindowImageOption, CGWindowListOption};
use xcap::Monitor;

pub(crate) fn display(monitor: &Monitor) -> Result<RgbaImage> {
    let bounds = CGRect {
        origin: CGPoint {
            x: monitor.x()? as f64,
            y: monitor.y()? as f64,
        },
        size: CGSize {
            width: monitor.width()? as f64,
            height: monitor.height()? as f64,
        },
    };
    let width = bounds.size.width;
    let height = bounds.size.height;
    if width <= 0.0 || height <= 0.0 {
        return Err(anyhow!("display has an empty bounds"));
    }
    // A rotated display's bitmap can come back in its unrotated orientation;
    // the on-screen window list composite is always oriented like the screen.
    let aspect = width / height;
    let image = display_bitmap(monitor.id()?)
        .filter(|image| aspect_of(image).is_some_and(|it| (it - aspect).abs() < 0.01))
        .or_else(|| screen_bitmap(bounds))
        .ok_or_else(|| anyhow!("CoreGraphics returned no display image"))?;

    let width = CGImage::width(Some(&image));
    let height = CGImage::height(Some(&image));
    let stride = CGImage::bytes_per_row(Some(&image));
    if width == 0 || height == 0 || stride < width * 4 {
        return Err(anyhow!(
            "unexpected image layout {width}x{height} stride {stride}"
        ));
    }
    let provider = CGImage::data_provider(Some(&image))
        .ok_or_else(|| anyhow!("image has no data provider"))?;
    let data = CGDataProvider::data(Some(&provider)).ok_or_else(|| anyhow!("image has no data"))?;
    let len = data.length() as usize;
    if len < (height - 1) * stride + width * 4 {
        return Err(anyhow!(
            "image data is {len} bytes, too short for {width}x{height}"
        ));
    }
    // SAFETY: the provider owns `len` bytes for as long as `data` is alive.
    let raw = unsafe { std::slice::from_raw_parts(data.byte_ptr(), len) };

    let mut pixels = vec![0u8; width * height * 4];
    bgra_to_rgba(raw, &mut pixels, width, height, stride);
    RgbaImage::from_raw(width as u32, height as u32, pixels)
        .ok_or_else(|| anyhow!("RGBA buffer is the wrong size"))
}

// ScreenCaptureKit is the supported replacement, but its first frame needs an
// async stream setup that costs more than this deprecated synchronous grab.
#[allow(deprecated)]
fn display_bitmap(id: u32) -> Option<CFRetained<CGImage>> {
    use objc2_core_graphics::CGDisplayCreateImage;
    CGDisplayCreateImage(id)
}

#[allow(deprecated)]
fn screen_bitmap(bounds: CGRect) -> Option<CFRetained<CGImage>> {
    use objc2_core_graphics::CGWindowListCreateImage;
    CGWindowListCreateImage(
        bounds,
        CGWindowListOption::OptionOnScreenOnly,
        0,
        CGWindowImageOption::Default,
    )
}

fn aspect_of(image: &CGImage) -> Option<f64> {
    let height = CGImage::height(Some(image));
    (height > 0).then(|| CGImage::width(Some(image)) as f64 / height as f64)
}

/// CoreGraphics hands back premultiplied BGRA rows; GPUI and the rest of the
/// app expect tightly packed RGBA. Bands of rows are converted in parallel so a
/// 5K frame stays cheap.
fn bgra_to_rgba(src: &[u8], dst: &mut [u8], width: usize, height: usize, stride: usize) {
    let row_bytes = width * 4;
    let threads = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(4)
        .clamp(2, 8)
        .min(height);
    if threads <= 1 || row_bytes * height < 1024 * 1024 {
        for (row, out) in dst.chunks_mut(row_bytes).enumerate() {
            swap_row(&src[row * stride..][..row_bytes], out);
        }
        return;
    }
    let band_rows = height.div_ceil(threads);
    std::thread::scope(|scope| {
        for (band, out) in dst.chunks_mut(band_rows * row_bytes).enumerate() {
            let first_row = band * band_rows;
            scope.spawn(move || {
                for (row, out) in out.chunks_mut(row_bytes).enumerate() {
                    let at = (first_row + row) * stride;
                    swap_row(&src[at..][..row_bytes], out);
                }
            });
        }
    });
}

fn swap_row(src: &[u8], dst: &mut [u8]) {
    let (from, _) = src.as_chunks::<4>();
    let (to, _) = dst.as_chunks_mut::<4>();
    for (from, to) in from.iter().zip(to) {
        to[0] = from[2];
        to[1] = from[1];
        to[2] = from[0];
        to[3] = from[3];
    }
}

#[cfg(test)]
mod tests {
    use super::bgra_to_rgba;

    #[test]
    fn converts_strided_rows_and_swaps_channels() {
        // Two 2-pixel rows with 4 bytes of padding per row.
        let src = [
            1, 2, 3, 255, 4, 5, 6, 255, 0, 0, 0, 0, 0, 0, 0, 0, //
            7, 8, 9, 255, 10, 11, 12, 255, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        let mut dst = vec![0u8; 16];
        bgra_to_rgba(&src, &mut dst, 2, 2, 16);
        assert_eq!(
            dst,
            [3, 2, 1, 255, 6, 5, 4, 255, 9, 8, 7, 255, 12, 11, 10, 255]
        );
    }
}
