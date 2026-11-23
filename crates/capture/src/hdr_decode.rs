//! Decode the 16-bit BT.2020/PQ PNGs this app writes back into an 8-bit SDR
//! preview. Needed when an HDR history original is re-opened (pin from
//! history, pin-last after restart): the generic `image` crate would read the
//! 16-bit PQ samples as if they were linear sRGB and show badly wrong colours.
//!
//! This is the exact inverse of `super::{encode_png_hdr, write_hdr_png}`:
//! PQ code -> absolute nits -> linear BT.2020 -> linear scRGB -> sRGB 8-bit.
//! Display white is 203 nits (HDR10's reference), matching how any HDR viewer
//! renders the file and how `HdrImage::to_sdr()` maps macOS captures.

use anyhow::Result;
use image::RgbaImage;

use crate::HDR_CICP;

/// Decode a PNG that may be one of our HDR files into an 8-bit SDR preview.
///
/// Returns `Ok(Some(image))` for a 16-bit RGB PNG carrying cICP 9/16/0/1, and
/// `Ok(None)` for any ordinary PNG (callers fall back to the generic decoder).
pub fn decode_hdr_png_preview(bytes: &[u8]) -> Result<Option<RgbaImage>> {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = match decoder.read_info() {
        Ok(reader) => reader,
        Err(_) => return Ok(None),
    };
    let (width, height, is_hdr) = {
        let info = reader.info();
        let is_hdr = info.bit_depth == png::BitDepth::Sixteen
            && info.color_type == png::ColorType::Rgb
            && info
                .coding_independent_code_points
                .as_ref()
                .is_some_and(|cicp| {
                    [
                        cicp.color_primaries,
                        cicp.transfer_function,
                        cicp.matrix_coefficients,
                        u8::from(cicp.is_video_full_range_image),
                    ] == HDR_CICP
                });
        (info.width, info.height, is_hdr)
    };
    if !is_hdr {
        return Ok(None);
    }

    let buffer_size = reader
        .output_buffer_size()
        .ok_or_else(|| anyhow::anyhow!("HDR PNG output buffer too large"))?;
    let mut raw = vec![0u8; buffer_size];
    reader
        .next_frame(&mut raw)
        .map_err(|err| anyhow::anyhow!("failed to decode HDR PNG frame: {err}"))?;
    let stride = reader
        .output_line_size(width)
        .ok_or_else(|| anyhow::anyhow!("HDR PNG line size too large"))?; // 16-bit RGB, big-endian
    let preview = pq_bt2020_to_sdr(&raw, stride, width, height);
    Ok(Some(preview))
}

/// Inverse of `pq_bt2020_samples`: 16-bit big-endian PQ BT.2020 -> 8-bit sRGB.
fn pq_bt2020_to_sdr(raw: &[u8], stride: usize, width: u32, height: u32) -> RgbaImage {
    const REFERENCE_WHITE_NITS: f32 = 203.0;
    // Inverse of BT2020_FROM_SRGB (linear BT.2020 -> linear scRGB).
    const SRGB_FROM_BT2020: [[f32; 3]; 3] = [
        [1.716_651, -0.355_670, -0.253_367],
        [-0.666_684, 1.616_481, 0.015_769],
        [0.017_640, -0.042_771, 0.942_103],
    ];

    let (w, h) = (width as usize, height as usize);
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        let line = &raw[y * stride..(y + 1) * stride];
        for x in 0..w {
            let at = x * 6;
            if at + 6 > line.len() {
                break;
            }
            let bt2020 = [
                pq_to_nits(u16::from_be_bytes([line[at], line[at + 1]])) / REFERENCE_WHITE_NITS,
                pq_to_nits(u16::from_be_bytes([line[at + 2], line[at + 3]])) / REFERENCE_WHITE_NITS,
                pq_to_nits(u16::from_be_bytes([line[at + 4], line[at + 5]])) / REFERENCE_WHITE_NITS,
            ];
            let m = &SRGB_FROM_BT2020;
            let srgb = [
                m[0][0] * bt2020[0] + m[0][1] * bt2020[1] + m[0][2] * bt2020[2],
                m[1][0] * bt2020[0] + m[1][1] * bt2020[1] + m[1][2] * bt2020[2],
                m[2][0] * bt2020[0] + m[2][1] * bt2020[1] + m[2][2] * bt2020[2],
            ];
            let dst = (y * w + x) * 4;
            for (channel, linear) in srgb.iter().enumerate() {
                out[dst + channel] = linear_to_srgb_u8(linear.clamp(0.0, 1.0));
            }
            out[dst + 3] = 255;
        }
    }
    RgbaImage::from_raw(width, height, out).expect("RGBA buffer matches dimensions")
}

/// SMPTE ST 2084 PQ EOTF: 16-bit code -> absolute luminance in nits.
fn pq_to_nits(code: u16) -> f32 {
    const M1: f64 = 2610.0 / 16384.0;
    const M2: f64 = 2523.0 / 32.0;
    const C1: f64 = 3424.0 / 4096.0;
    const C2: f64 = 2413.0 / 128.0;
    const C3: f64 = 2392.0 / 128.0;
    let v = f64::from(code) / f64::from(u16::MAX);
    let vp = v.powf(1.0 / M2);
    let num = (vp - C1).max(0.0);
    let den = C2 - C3 * vp;
    if den <= 0.0 {
        return 0.0;
    }
    (num / den).powf(1.0 / M1).clamp(0.0, 1.0) as f32 * 10_000.0
}

fn linear_to_srgb_u8(linear: f32) -> u8 {
    let srgb = if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    };
    (srgb.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}
