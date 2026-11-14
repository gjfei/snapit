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
pub(crate) fn prepare(frame: &RgbaImage, max_width: Option<u32>) -> Cow<'_, RgbaImage> {
    let (width, height) = frame.dimensions();
    let scaled = match max_width.filter(|limit| *limit > 0 && *limit < width) {
        Some(limit) => {
            let limit = limit.max(2);
            let scale = limit as f32 / width as f32;
            let width = ((width as f32 * scale).round() as u32).max(2);
            let height = ((height as f32 * scale).round() as u32).max(2);
            Cow::Owned(image::imageops::resize(
                frame,
                width,
                height,
                image::imageops::FilterType::Triangle,
            ))
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
}
