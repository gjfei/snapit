//! MP4 encoding, one backend per platform.
//!
//! Each backend exposes a `Mp4Sink` with the same construction signature and
//! implements the recorder's `FrameSink`, so the capture loop never learns which
//! encoder it is feeding.

use std::borrow::Cow;

use snapit_capture::RgbaImage;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(crate) use macos::Mp4Sink;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub(crate) use windows::Mp4Sink;

/// Shrink a frame to `max_width`, keeping the aspect ratio, and crop it to even
/// dimensions: H.264 subsamples chroma by two, so every encoder rejects odd
/// sizes. Cropping a pixel is invisible, resizing is not.
///
/// Retina displays deliver frames in physical pixels while the selection is in
/// logical points, so this halves each axis. That is why both sinks share it.
pub(crate) fn prepare(frame: &RgbaImage, max_width: Option<u32>) -> Cow<'_, RgbaImage> {
    let (width, height) = frame.dimensions();
    let scaled = match max_width.filter(|limit| *limit > 0 && *limit < width) {
        Some(limit) => {
            let limit = limit.max(2);
            let scale = limit as f32 / width as f32;
            let width = ((width as f32 * scale).round() as u32).max(2);
            let height = ((height as f32 * scale).round() as u32).max(2);
            Cow::Owned(box_downscale(frame, width, height))
        }
        None => Cow::Borrowed(frame),
    };
    let (width, height) = scaled.dimensions();
    let (even_width, even_height) = (width & !1, height & !1);
    if (even_width, even_height) == (width, height) {
        return scaled;
    }
    let mut cropped = scaled.into_owned();
    cropped =
        image::imageops::crop_imm(&cropped, 0, 0, even_width.max(2), even_height.max(2)).to_image();
    Cow::Owned(cropped)
}

/// Area-average downscale in one sequential pass over the source.
///
/// `image::imageops::resize(Triangle)` costs ~46 ms on a 2000x1200 frame: it
/// runs a separable convolution with per-pixel floating point weights. A screen
/// capture only needs a box mean, and integer bounds let us touch every source
/// pixel exactly once with plain additions — no weights, no temporary buffers.
fn box_downscale(frame: &RgbaImage, target_width: u32, target_height: u32) -> RgbaImage {
    let (source_width, source_height) = frame.dimensions();
    let (sw, sh) = (source_width as usize, source_height as usize);
    let (tw, th) = (target_width as usize, target_height as usize);
    let source = frame.as_raw();
    let mut output = vec![0u8; tw * th * 4];

    // Integer bounds of the source rectangle behind each output pixel. Both
    // axes shrink, so consecutive bounds strictly increase and every source
    // pixel belongs to exactly one output pixel.
    let mut x_bounds = Vec::with_capacity(tw + 1);
    for dx in 0..=tw {
        x_bounds.push((dx * sw / tw).min(sw));
    }
    let mut y_bounds = Vec::with_capacity(th + 1);
    for dy in 0..=th {
        y_bounds.push((dy * sh / th).min(sh));
    }

    for dy in 0..th {
        let y0 = y_bounds[dy];
        let y1 = y_bounds[dy + 1].max(y0 + 1).min(sh);
        let row_out = &mut output[dy * tw * 4..(dy + 1) * tw * 4];
        for dx in 0..tw {
            let x0 = x_bounds[dx];
            let x1 = x_bounds[dx + 1].max(x0 + 1).min(sw);
            let (mut r, mut g, mut b, mut a) = (0u32, 0u32, 0u32, 0u32);
            for y in y0..y1 {
                let start = (y * sw + x0) * 4;
                let end = (y * sw + x1) * 4;
                let (pixels, _) = source[start..end].as_chunks::<4>();
                for p in pixels {
                    r += p[0] as u32;
                    g += p[1] as u32;
                    b += p[2] as u32;
                    a += p[3] as u32;
                }
            }
            let count = ((x1 - x0) * (y1 - y0)) as u32;
            let half = count / 2;
            let dst = &mut row_out[dx * 4..dx * 4 + 4];
            dst[0] = ((r + half) / count) as u8;
            dst[1] = ((g + half) / count) as u8;
            dst[2] = ((b + half) / count) as u8;
            dst[3] = ((a + half) / count) as u8;
        }
    }
    RgbaImage::from_raw(target_width, target_height, output)
        .expect("the output buffer has exactly target_width * target_height * 4 bytes")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u32, height: u32) -> RgbaImage {
        RgbaImage::new(width, height)
    }

    #[test]
    fn frames_that_already_fit_are_borrowed() {
        let image = frame(800, 600);
        assert!(matches!(prepare(&image, Some(1600)), Cow::Borrowed(_)));
        assert!(matches!(prepare(&image, None), Cow::Borrowed(_)));
    }

    #[test]
    fn downscaling_keeps_the_aspect_ratio() {
        let wide = prepare(&frame(1800, 1000), Some(901)).into_owned();
        assert_eq!(wide.dimensions(), (900, 500));
    }

    #[test]
    fn odd_sizes_are_cropped_to_even() {
        let cropped = prepare(&frame(1001, 501), None).into_owned();
        assert_eq!(cropped.dimensions(), (1000, 500));
        let after_scale = prepare(&frame(1001, 501), Some(501)).into_owned();
        assert_eq!(after_scale.dimensions(), (500, 250));
    }

    #[test]
    fn box_downscale_averages_each_source_rectangle() {
        let mut image = frame(4, 2);
        image.put_pixel(1, 0, image::Rgba([100, 0, 0, 255]));
        for pixel in image.as_mut().as_chunks_mut::<4>().0 {
            if pixel[3] == 0 {
                pixel.copy_from_slice(&[0, 0, 0, 255]);
            }
        }
        let small = box_downscale(&image, 2, 1);
        // The first output pixel covers the 2x2 block containing the red dot.
        assert_eq!(*small.get_pixel(0, 0), image::Rgba([25, 0, 0, 255]));
        assert_eq!(*small.get_pixel(1, 0), image::Rgba([0, 0, 0, 255]));
    }

    #[test]
    fn box_downscale_keeps_a_solid_colour_at_any_ratio() {
        let mut image = frame(1001, 677);
        for pixel in image.as_mut().as_chunks_mut::<4>().0 {
            pixel.copy_from_slice(&[17, 42, 200, 255]);
        }
        for (width, height) in [(500, 338), (333, 225), (2, 2)] {
            let small = box_downscale(&image, width, height);
            assert_eq!(small.dimensions(), (width, height));
            for pixel in small.as_raw().as_chunks::<4>().0 {
                assert_eq!(pixel, &[17, 42, 200, 255]);
            }
        }
    }

    #[test]
    fn every_source_pixel_contributes_once() {
        // Each source pixel is white in one 4x4 frame; a downscale that skips
        // or double-counts rows cannot produce the exact mean.
        let mut image = frame(8, 4);
        for pixel in image.as_mut().as_chunks_mut::<4>().0 {
            pixel.copy_from_slice(&[0, 0, 0, 255]);
        }
        image.put_pixel(3, 2, image::Rgba([255, 0, 0, 255]));
        let small = box_downscale(&image, 4, 2);
        let lit = small
            .as_raw()
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[0] > 0)
            .count();
        assert_eq!(lit, 1, "one source pixel must affect exactly one output");
        assert_eq!(
            small
                .as_raw()
                .as_chunks::<4>()
                .0
                .iter()
                .map(|p| p[0] as u32)
                .sum::<u32>(),
            64,
            "255 spread over the four source pixels of one 2x2 mean"
        );
    }
}
