//! HDR frames for the HEVC sinks: linear scRGB in, PQ BT.2020 10-bit 4:2:0 out.
//!
//! The chain is the one HDR10 expects: linear extended sRGB -> linear BT.2020
//! -> absolute nits -> ST 2084 PQ -> BT.2020 non-constant-luminance Y'CbCr,
//! quantized to 10-bit video range and stored MSB-aligned in little-endian
//! 16-bit words. That is P010 on Windows and 'x420' on macOS — the same layout,
//! so both sinks share this converter.
//!
//! PQ is absolute: only `reference_white_nits` scales the samples (the SDR
//! white level is a tone-mapping concern, not an HDR10 one). Two lookup tables
//! keep the per-pixel work to array reads: f16 bits -> linear f32, and
//! `sqrt(nits / 10000)` -> PQ, the same indexing the HDR PNG export uses.

use std::borrow::Cow;
use std::sync::OnceLock;

use anyhow::{Result, bail};
use snapit_capture::{HdrImage, RgbaImage};

/// A converted frame: the Y plane followed by interleaved CbCr, both
/// little-endian u16 with the 10-bit code in the high bits (`code << 6`).
/// Only the tests read a whole frame at once; a sink uses [`convert_into`].
#[cfg(test)]
pub(crate) struct P010 {
    /// `width * height * 2` bytes, row-major.
    pub y: Vec<u8>,
    /// `(width / 2) * (height / 2) * 4` bytes, CbCr pairs in row order.
    pub uv: Vec<u8>,
}

/// linear sRGB (scRGB primaries) -> linear BT.2020, the same matrix the HDR
/// PNG export uses.
const BT2020_FROM_SRGB: [[f32; 3]; 3] = [
    [0.627_404, 0.329_282, 0.043_313_6],
    [0.069_097, 0.919_54, 0.011_361_2],
    [0.016_391_6, 0.088_013_2, 0.895_595],
];

const PQ_STEPS: usize = 65536;

/// Below this many pixels one thread beats the spawn overhead, the same
/// threshold class as the overlay's RGBA->BGRA swap.
const PARALLEL_PIXELS: usize = 256 * 1024;

/// Shrink to `max_width` and crop to even dimensions, mirroring `video::prepare`
/// for SDR frames: 4:2:0 subsamples chroma by two, so every encoder rejects odd
/// sizes. The box mean runs in linear light, before the transfer function.
pub(crate) fn prepare(hdr: &HdrImage, max_width: Option<u32>) -> Cow<'_, HdrImage> {
    let width = hdr.width();
    let scaled = match max_width.filter(|limit| *limit > 0 && *limit < width) {
        Some(limit) => {
            let limit = limit.max(2);
            let scale = limit as f32 / width as f32;
            let width = ((width as f32 * scale).round() as u32).max(2);
            let height = ((hdr.height() as f32 * scale).round() as u32).max(2);
            Cow::Owned(box_downscale(hdr, width, height))
        }
        None => Cow::Borrowed(hdr),
    };
    let (width, height) = (scaled.width(), scaled.height());
    let (even_width, even_height) = (width & !1, height & !1);
    if (even_width, even_height) == (width, height) {
        return scaled;
    }
    Cow::Owned(
        scaled
            .crop(0, 0, even_width.max(2), even_height.max(2))
            .expect("a non-empty frame keeps a non-empty crop"),
    )
}

/// Area-average downscale in linear light, one sequential pass over the
/// source. Integer bounds give every source pixel exactly one output pixel,
/// the same scheme as the SDR box mean.
fn box_downscale(hdr: &HdrImage, target_width: u32, target_height: u32) -> HdrImage {
    let (sw, sh) = (hdr.width() as usize, hdr.height() as usize);
    let (tw, th) = (target_width as usize, target_height as usize);
    let source = hdr.as_bytes();
    let f16 = f16_lut();
    let mut output = vec![0u8; tw * th * 6];

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
        let row_out = &mut output[dy * tw * 6..(dy + 1) * tw * 6];
        for dx in 0..tw {
            let x0 = x_bounds[dx];
            let x1 = x_bounds[dx + 1].max(x0 + 1).min(sw);
            let (mut r, mut g, mut b) = (0f32, 0f32, 0f32);
            for y in y0..y1 {
                let start = (y * sw + x0) * 6;
                let end = (y * sw + x1) * 6;
                for p in source[start..end].as_chunks::<6>().0 {
                    r += f16[u16::from_le_bytes([p[0], p[1]]) as usize];
                    g += f16[u16::from_le_bytes([p[2], p[3]]) as usize];
                    b += f16[u16::from_le_bytes([p[4], p[5]]) as usize];
                }
            }
            let count = ((x1 - x0) * (y1 - y0)) as f32;
            let dst = &mut row_out[dx * 6..dx * 6 + 6];
            for (bytes, sum) in dst.chunks_mut(2).zip([r, g, b]) {
                bytes.copy_from_slice(&f32_to_f16_bits(sum / count).to_le_bytes());
            }
        }
    }
    HdrImage::new(
        target_width,
        target_height,
        output,
        hdr.reference_white_nits(),
    )
    .with_sdr_white(hdr.sdr_white_nits())
}

/// Lift an 8-bit sRGB frame into the HDR10 sink without changing the apparent
/// brightness of SDR content. sRGB white is linear `1.0`; in scRGB it therefore
/// sits at `sdr_white / reference_white`, which keeps Windows' boosted SDR
/// white (and macOS' 203-nit reference white) at the same absolute luminance.
pub(crate) fn promote_sdr(
    frame: &RgbaImage,
    reference_white_nits: f32,
    sdr_white_nits: f32,
) -> Result<HdrImage> {
    let (width, height) = frame.dimensions();
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| anyhow::anyhow!("SDR frame pixel count overflow"))?;
    let raw_len = pixels
        .checked_mul(6)
        .ok_or_else(|| anyhow::anyhow!("SDR frame HDR sample size overflow"))?;
    let scale = sdr_white_nits / reference_white_nits;
    if !scale.is_finite() || scale <= 0.0 {
        bail!("invalid SDR/reference white ratio: {sdr_white_nits}/{reference_white_nits}");
    }
    let mut raw = Vec::with_capacity(raw_len);
    for pixel in frame.as_raw().as_chunks::<4>().0 {
        for channel in &pixel[..3] {
            let linear = srgb_to_linear(f32::from(*channel) / 255.0) * scale;
            raw.extend_from_slice(&f32_to_f16_bits(linear).to_le_bytes());
        }
    }
    HdrImage::try_new(width, height, raw, reference_white_nits)?
        .try_with_sdr_white(sdr_white_nits)
        .map_err(Into::into)
}

fn srgb_to_linear(value: f32) -> f32 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

/// Byte lengths of the two planes of a frame of this size: 10-bit samples are
/// stored one per little-endian u16, and chroma is half resolution in both
/// directions with Cb and Cr interleaved — half the luma plane, but with the
/// same row length, since a chroma row has `width / 2` CbCr pairs.
pub(crate) fn plane_lens(width: u32, height: u32) -> (usize, usize) {
    let luma = width as usize * height as usize * 2;
    (luma, width as usize * height as usize)
}

/// Convert an even-sized frame to 10-bit PQ BT.2020 4:2:0. Chroma is the mean
/// of each 2x2 block's Cb/Cr, taken after the transfer function.
#[cfg(test)]
pub(crate) fn convert(hdr: &HdrImage) -> P010 {
    let (y_len, uv_len) = plane_lens(hdr.width(), hdr.height());
    let mut y = vec![0u8; y_len];
    let mut uv = vec![0u8; uv_len];
    convert_into(hdr, &mut y, &mut uv);
    P010 { y, uv }
}

/// [`convert`] into caller-owned planes, so a sink reuses one pair of buffers
/// instead of allocating and zeroing a new frame for every push.
pub(crate) fn convert_into(hdr: &HdrImage, y: &mut [u8], uv: &mut [u8]) {
    let width = hdr.width() as usize;
    let height = hdr.height() as usize;
    debug_assert!(
        width.is_multiple_of(2) && height.is_multiple_of(2),
        "prepare crops to even"
    );
    let (y_len, uv_len) = plane_lens(width as u32, height as u32);
    debug_assert!(y.len() == y_len && uv.len() == uv_len, "plane_lens sizes");
    let white = hdr.reference_white_nits();
    let pq = pq_lut();
    let f16 = f16_lut();
    let chroma_rows = height / 2;
    if width * height < PARALLEL_PIXELS || chroma_rows < 2 {
        write_band(hdr, y, uv, 0, white, pq, f16);
    } else {
        let threads = std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(4)
            .clamp(2, 8)
            .min(chroma_rows);
        let rows_per_band = chroma_rows.div_ceil(threads);
        std::thread::scope(|scope| {
            let y_bands = y.chunks_mut(rows_per_band * 2 * width * 2);
            let uv_bands = uv.chunks_mut(rows_per_band * width * 2);
            for (band, (y_band, uv_band)) in y_bands.zip(uv_bands).enumerate() {
                scope.spawn(move || {
                    write_band(hdr, y_band, uv_band, band * rows_per_band, white, pq, f16);
                });
            }
        });
    }
}

/// One band of chroma rows: two luma rows and one CbCr row per step.
fn write_band(
    hdr: &HdrImage,
    y_band: &mut [u8],
    uv_band: &mut [u8],
    first_chroma_row: usize,
    white: f32,
    pq: &[f32; PQ_STEPS],
    f16: &[f32; 65536],
) {
    let width = hdr.width() as usize;
    let raw = hdr.as_bytes();
    let luma_pairs = y_band.chunks_mut(width * 4);
    let chroma_lines = uv_band.chunks_mut(width * 2);
    for (step, (pair, uv_row)) in luma_pairs.zip(chroma_lines).enumerate() {
        let row = (first_chroma_row + step) * 2;
        let (y0, y1) = pair.split_at_mut(width * 2);
        for c in 0..width / 2 {
            let x = c * 2;
            let i00 = (row * width + x) * 6;
            let i10 = ((row + 1) * width + x) * 6;
            let (l00, cb00, cr00) = pixel(&raw[i00..i00 + 6], white, pq, f16);
            let (l01, cb01, cr01) = pixel(&raw[i00 + 6..i00 + 12], white, pq, f16);
            let (l10, cb10, cr10) = pixel(&raw[i10..i10 + 6], white, pq, f16);
            let (l11, cb11, cr11) = pixel(&raw[i10 + 6..i10 + 12], white, pq, f16);
            y0[c * 4..c * 4 + 2].copy_from_slice(&l00.to_le_bytes());
            y0[c * 4 + 2..c * 4 + 4].copy_from_slice(&l01.to_le_bytes());
            y1[c * 4..c * 4 + 2].copy_from_slice(&l10.to_le_bytes());
            y1[c * 4 + 2..c * 4 + 4].copy_from_slice(&l11.to_le_bytes());
            let cb = chroma_code((cb00 + cb01 + cb10 + cb11) * 0.25);
            let cr = chroma_code((cr00 + cr01 + cr10 + cr11) * 0.25);
            uv_row[c * 4..c * 4 + 2].copy_from_slice(&cb.to_le_bytes());
            uv_row[c * 4 + 2..c * 4 + 4].copy_from_slice(&cr.to_le_bytes());
        }
    }
}

/// One pixel: MSB-aligned 10-bit luma plus unquantized Cb/Cr in [-0.5, 0.5].
///
/// The chroma divisors are the BT.2020 non-constant-luminance pair,
/// `2(1 - Kb) = 1.8814` and `2(1 - Kr) = 1.4746`. BT.709's 1.5748 must not be
/// mixed in: it would squash red-green chroma under the BT.2020 luma weights.
#[inline]
fn pixel(raw: &[u8], white: f32, pq: &[f32; PQ_STEPS], f16: &[f32; 65536]) -> (u16, f32, f32) {
    let r = f16[u16::from_le_bytes([raw[0], raw[1]]) as usize];
    let g = f16[u16::from_le_bytes([raw[2], raw[3]]) as usize];
    let b = f16[u16::from_le_bytes([raw[4], raw[5]]) as usize];
    let m = &BT2020_FROM_SRGB;
    let (sr, sg, sb) = (r, g, b);
    let r = m[0][0] * sr + m[0][1] * sg + m[0][2] * sb;
    let g = m[1][0] * sr + m[1][1] * sg + m[1][2] * sb;
    let b = m[2][0] * sr + m[2][1] * sg + m[2][2] * sb;
    let r = pq_code(r, white, pq);
    let g = pq_code(g, white, pq);
    let b = pq_code(b, white, pq);
    let luma = 0.2627 * r + 0.6780 * g + 0.0593 * b;
    let cb = (b - luma) / 1.8814;
    let cr = (r - luma) / 1.4746;
    let code = (64.0 + 876.0 * luma).round().clamp(64.0, 940.0) as u16;
    (code << 6, cb, cr)
}

#[inline]
fn chroma_code(value: f32) -> u16 {
    let code = (512.0 + 896.0 * value).round().clamp(64.0, 960.0) as u16;
    code << 6
}

#[inline]
fn pq_code(linear: f32, white: f32, pq: &[f32; PQ_STEPS]) -> f32 {
    let nits = (linear * white).clamp(0.0, 10_000.0);
    // Indexing by sqrt keeps sub-nit precision where PQ needs it most.
    let index = ((nits / 10_000.0).sqrt() * (PQ_STEPS - 1) as f32).round() as usize;
    pq[index.min(PQ_STEPS - 1)]
}

/// PQ values over 0..=10000 nits, indexed by `sqrt(nits / 10000)`.
fn pq_lut() -> &'static [f32; PQ_STEPS] {
    static LUT: OnceLock<Box<[f32; PQ_STEPS]>> = OnceLock::new();
    LUT.get_or_init(|| {
        // SMPTE ST 2084 constants.
        const M1: f64 = 2610.0 / 16384.0;
        const M2: f64 = 2523.0 / 32.0;
        const C1: f64 = 3424.0 / 4096.0;
        const C2: f64 = 2413.0 / 128.0;
        const C3: f64 = 2392.0 / 128.0;
        let mut lut = Box::new([0f32; PQ_STEPS]);
        for (index, slot) in lut.iter_mut().enumerate() {
            let normalized = index as f64 / (PQ_STEPS - 1) as f64;
            let luminance = normalized * normalized;
            let powered = luminance.powf(M1);
            let encoded = ((C1 + C2 * powered) / (1.0 + C3 * powered)).powf(M2);
            *slot = encoded.clamp(0.0, 1.0) as f32;
        }
        lut
    })
}

/// Every half-float bit pattern decoded to f32, so a sample is one array read.
fn f16_lut() -> &'static [f32; 65536] {
    static LUT: OnceLock<Box<[f32; 65536]>> = OnceLock::new();
    LUT.get_or_init(|| {
        let mut lut = Box::new([0f32; 65536]);
        for (bits, slot) in lut.iter_mut().enumerate() {
            *slot = f16_to_f32(bits as u16);
        }
        lut
    })
}

fn f16_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits >> 15) << 31;
    let exponent = (bits >> 10) & 0x1f;
    let mantissa = u32::from(bits & 0x3ff);
    let f32_bits = if exponent == 0 {
        if mantissa == 0 {
            sign
        } else {
            let mut exponent = 127 - 15 + 1;
            let mut mantissa = mantissa;
            while mantissa & 0x400 == 0 {
                mantissa <<= 1;
                exponent -= 1;
            }
            sign | (exponent << 23) | ((mantissa & 0x3ff) << 13)
        }
    } else if exponent == 31 {
        sign | 0x7f80_0000 | (mantissa << 13)
    } else {
        sign | ((u32::from(exponent) + 127 - 15) << 23) | (mantissa << 13)
    };
    f32::from_bits(f32_bits)
}

/// f32 -> half-float bits, round toward zero (the same choice xcap makes).
fn f32_to_f16_bits(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    let mantissa = bits & 0x007f_ffff;
    if exponent >= 31 {
        sign | 0x7c00
    } else if exponent <= 0 {
        sign
    } else {
        sign | ((exponent as u16) << 10) | ((mantissa >> 13) as u16)
    }
}

/// A frame from f32 scRGB samples, so a test can supply HDR pixels without a
/// capture device. The platform sink tests need this; `white_nits` is the
/// SDR reference level the display was in.
#[cfg(test)]
pub(crate) fn synth(width: u32, height: u32, pixels: &[[f32; 3]], white_nits: f32) -> HdrImage {
    assert_eq!(pixels.len(), (width * height) as usize);
    let mut raw = Vec::with_capacity(pixels.len() * 6);
    for [r, g, b] in pixels {
        for value in [r, g, b] {
            raw.extend_from_slice(&f32_to_f16_bits(*value).to_le_bytes());
        }
    }
    HdrImage::new(width, height, raw, white_nits)
}

/// A frame of one colour, large enough to exercise a real encoder.
#[cfg(test)]
pub(crate) fn synth_solid(width: u32, height: u32, value: [f32; 3], white_nits: f32) -> HdrImage {
    synth(
        width,
        height,
        &vec![value; (width * height) as usize],
        white_nits,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u32, height: u32, values: &[[f32; 3]], white_nits: f32) -> HdrImage {
        synth(width, height, values, white_nits)
    }

    fn solid(width: u32, height: u32, value: [f32; 3], white_nits: f32) -> HdrImage {
        synth_solid(width, height, value, white_nits)
    }

    fn code(bytes: &[u8]) -> u16 {
        u16::from_le_bytes([bytes[0], bytes[1]]) >> 6
    }

    /// Independent ST 2084 evaluation in f64 for the reference levels.
    fn reference_pq(nits: f64) -> f64 {
        let m1 = 2610.0 / 16384.0;
        let m2 = 2523.0 / 32.0;
        let c1 = 3424.0 / 4096.0;
        let c2 = 2413.0 / 128.0;
        let c3 = 2392.0 / 128.0;
        let y = (nits / 10_000.0).powf(m1);
        ((c1 + c2 * y) / (1.0 + c3 * y)).powf(m2)
    }

    #[test]
    fn neutral_levels_match_the_pq_reference() {
        for (nits, expected) in [(100.0, 0.5081), (1000.0, 0.7518)] {
            let hdr = solid(2, 2, [1.0, 1.0, 1.0], nits as f32);
            let p010 = convert(&hdr);
            let pq = reference_pq(nits);
            assert!((pq - expected).abs() < 0.001, "reference drifted: {pq}");
            let want = (64.0 + 876.0 * pq).round() as u16;
            for sample in p010.y.chunks(2) {
                let got = code(sample);
                assert!(got.abs_diff(want) <= 1, "{nits} nits: {got} vs {want}");
            }
            for sample in p010.uv.chunks(2) {
                assert_eq!(code(sample), 512, "neutral chroma sits at 512");
            }
        }
    }

    #[test]
    fn black_and_peak_land_on_the_video_range_ends() {
        let black = convert(&solid(2, 2, [0.0, 0.0, 0.0], 203.0));
        assert!(black.y.chunks(2).all(|s| code(s) == 64));
        assert!(black.uv.chunks(2).all(|s| code(s) == 512));
        let peak = convert(&solid(2, 2, [1.0, 1.0, 1.0], 10_000.0));
        assert!(peak.y.chunks(2).all(|s| code(s) == 940));
        // Above 10000 nits clips instead of wrapping.
        let hot = convert(&solid(2, 2, [4.0, 4.0, 4.0], 10_000.0));
        assert!(hot.y.chunks(2).all(|s| code(s) == 940));
    }

    #[test]
    fn primaries_take_every_matrix_row() {
        // Pure red spreads over all three BT.2020 rows, so an in-place matrix
        // that feeds a partially converted channel into the next row lands on
        // a different luma code than the independent reference below.
        let p010 = convert(&solid(2, 2, [1.0, 0.0, 0.0], 203.0));
        let luma = 0.2627 * reference_pq(0.627_404 * 203.0)
            + 0.6780 * reference_pq(0.069_097 * 203.0)
            + 0.0593 * reference_pq(0.016_391_6 * 203.0);
        let want = (64.0 + 876.0 * luma).round() as u16;
        for sample in p010.y.chunks(2) {
            let got = code(sample);
            assert!(got.abs_diff(want) <= 1, "{got} vs {want}");
        }
    }

    #[test]
    fn matches_an_independent_bt2020_ncl_reference() {
        // f64 evaluations made outside this code: BT.2020 primaries at a
        // 203-nit SDR white through ST 2084, the non-constant-luminance luma
        // weights and chroma divisors (1.8814 / 1.4746), then 10-bit video
        // range. Red pins the Cr divisor — BT.709's 1.5748 lands six codes low.
        for (rgb, want) in [
            ([1.0, 0.0, 0.0], (392, 438, 608)),
            ([0.0, 1.0, 0.0], (529, 424, 474)),
            ([0.0, 0.0, 1.0], (277, 667, 540)),
            ([1.0, 1.0, 1.0], (573, 512, 512)),
        ] {
            let frame = convert(&solid(2, 2, rgb, 203.0));
            let got = (
                code(&frame.y[0..2]),
                code(&frame.uv[0..2]),
                code(&frame.uv[2..4]),
            );
            assert!(
                got.0.abs_diff(want.0) <= 1
                    && got.1.abs_diff(want.1) <= 1
                    && got.2.abs_diff(want.2) <= 1,
                "{rgb:?}: {got:?} vs {want:?}"
            );
        }
    }

    /// The luma and chroma codes an independent ST 2084 / BT.2020 evaluation
    /// gives one scRGB sample, for the corner colours the tests feed in.
    fn reference_codes(rgb: [f32; 3], white: f32) -> (u16, u16, u16) {
        let m = &BT2020_FROM_SRGB;
        let pq = |value: f32| reference_pq(f64::from(value * white)) as f32;
        let (r, g, b) = (
            pq(m[0][0] * rgb[0] + m[0][1] * rgb[1] + m[0][2] * rgb[2]),
            pq(m[1][0] * rgb[0] + m[1][1] * rgb[1] + m[1][2] * rgb[2]),
            pq(m[2][0] * rgb[0] + m[2][1] * rgb[1] + m[2][2] * rgb[2]),
        );
        let luma = 0.2627 * r + 0.6780 * g + 0.0593 * b;
        let y = (64.0 + 876.0 * luma).round() as u16;
        let cb = (512.0 + 896.0 * (b - luma) / 1.8814).round() as u16;
        let cr = (512.0 + 896.0 * (r - luma) / 1.4746).round() as u16;
        (y, cb, cr)
    }

    #[test]
    fn chroma_is_the_mean_of_each_2x2_block() {
        // 4x2: columns 0-1 red, columns 2-3 blue. Red's Cb and Cr differ by
        // hundreds of codes, so this pins which of the two the matrix's red
        // row feeds into — a swap would sail through the neutral frames that
        // the other tests use, because their chroma is 512 either way.
        let mut values = vec![[0.0, 0.0, 0.0]; 8];
        for row in 0..2 {
            for x in 0..2 {
                values[row * 4 + x] = [1.0, 0.0, 0.0];
                values[row * 4 + x + 2] = [0.0, 0.0, 1.0];
            }
        }
        let p010 = convert(&frame(4, 2, &values, 203.0));
        assert_eq!(p010.y.len(), 4 * 2 * 2);
        assert_eq!(p010.uv.len(), 2 * 4);
        for (block, rgb) in [
            (&p010.uv[0..4], [1.0, 0.0, 0.0]),
            (&p010.uv[4..8], [0.0, 0.0, 1.0]),
        ] {
            let (_, cb, cr) = reference_codes(rgb, 203.0);
            for (got, want) in [(code(&block[0..2]), cb), (code(&block[2..4]), cr)] {
                assert!(got.abs_diff(want) <= 1, "{rgb:?}: {got} vs {want}");
            }
        }
    }

    #[test]
    fn chroma_averages_mixed_blocks() {
        // One red pixel in an otherwise neutral 2x2 block: the chroma codes
        // move a quarter of the way toward the red extreme.
        let mut values = vec![[1.0, 1.0, 1.0]; 4];
        values[0] = [1.0, 0.0, 0.0];
        let mixed = convert(&frame(2, 2, &values, 203.0));
        let red = convert(&solid(2, 2, [1.0, 0.0, 0.0], 203.0));
        let cb_mix = code(&mixed.uv[0..2]) as i32;
        let cb_red = code(&red.uv[0..2]) as i32;
        let expected = 512 + (cb_red - 512) / 4;
        assert!(cb_mix.abs_diff(expected) <= 1, "{cb_mix} vs {expected}");
    }

    #[test]
    fn threaded_bands_agree_with_the_reference() {
        // Large enough to take the threaded path; every row is a neutral
        // level, so each band can be checked against an independent PQ.
        let width = 600u32;
        let height = 520u32;
        let mut values = Vec::with_capacity((width * height) as usize);
        for y in 0..height {
            let level = 0.05 + (y as f32 / height as f32) * 3.0;
            for _ in 0..width {
                values.push([level, level, level]);
            }
        }
        let p010 = convert(&frame(width, height, &values, 203.0));
        for y in [0usize, 130, 259, 260, 400, 519] {
            // f16 quantizes the level; decode it back for the reference.
            let level = f16_to_f32(f32_to_f16_bits(0.05 + (y as f32 / height as f32) * 3.0));
            let want = (64.0 + 876.0 * reference_pq(level as f64 * 203.0)).round() as u16;
            let row = &p010.y[y * width as usize * 2..(y + 1) * width as usize * 2];
            for sample in row.chunks(2) {
                let got = code(sample);
                assert!(got.abs_diff(want) <= 1, "row {y}: {got} vs {want}");
            }
        }
    }

    #[test]
    fn promoted_sdr_keeps_absolute_sdr_white_in_a_mixed_hdr_session() {
        let white = RgbaImage::from_pixel(2, 2, image::Rgba([255, 255, 255, 255]));
        let promoted = promote_sdr(&white, 80.0, 200.0).unwrap();
        assert_eq!(promoted.pixel_f32(0, 0), [2.5; 3]);
        assert_eq!(promoted.reference_white_nits(), 80.0);
        assert_eq!(promoted.sdr_white_nits(), 200.0);
        assert!(!promoted.has_highlights());

        // A real HDR frame carrying the same 200-nit neutral must produce the
        // same PQ codes as the promoted SDR frame in this HDR10 session.
        let hdr = solid(2, 2, [2.5; 3], 80.0).with_sdr_white(200.0);
        let sdr_codes = convert(&promoted);
        let hdr_codes = convert(&hdr);
        assert_eq!(sdr_codes.y, hdr_codes.y);
        assert_eq!(sdr_codes.uv, hdr_codes.uv);

        let mac_white = promote_sdr(&white, 203.0, 203.0).unwrap();
        assert_eq!(mac_white.pixel_f32(0, 0), [1.0; 3]);
    }

    #[test]
    fn promoted_sdr_decodes_channels_in_linear_light_and_keeps_black() {
        let frame = RgbaImage::from_pixel(2, 1, image::Rgba([0, 128, 255, 255]));
        let promoted = promote_sdr(&frame, 203.0, 203.0).unwrap();
        let [r, g, b] = promoted.pixel_f32(0, 0);
        assert_eq!(r, 0.0);
        assert!((g - 0.215_86).abs() < 0.001, "mid sRGB green: {g}");
        assert!((b - 1.0).abs() < 0.001, "sRGB white blue: {b}");
    }

    #[test]
    fn prepare_borrows_frames_that_fit_and_crops_odd_sizes() {
        let hdr = solid(800, 600, [0.5, 0.5, 0.5], 203.0);
        assert!(matches!(prepare(&hdr, Some(1600)), Cow::Borrowed(_)));
        assert!(matches!(prepare(&hdr, None), Cow::Borrowed(_)));
        let odd = solid(1001, 501, [0.5, 0.5, 0.5], 203.0);
        let cropped = prepare(&odd, None).into_owned();
        assert_eq!((cropped.width(), cropped.height()), (1000, 500));
    }

    #[test]
    fn downscaling_keeps_aspect_and_solid_colour() {
        let hdr = solid(1800, 1000, [0.25, 0.5, 1.5], 80.0).with_sdr_white(200.0);
        let small = prepare(&hdr, Some(901)).into_owned();
        assert_eq!((small.width(), small.height()), (900, 500));
        assert_eq!(small.sdr_white_nits(), 200.0);
        assert_eq!(small.reference_white_nits(), 80.0);
        let [r, g, b] = small.pixel_f32(450, 250);
        assert!((r - 0.25).abs() < 0.001 && (g - 0.5).abs() < 0.001 && (b - 1.5).abs() < 0.002);
        // The converted solid frame stays uniform.
        let p010 = convert(&small);
        let first = code(&p010.y[0..2]);
        assert!(p010.y.chunks(2).all(|s| code(s) == first));
    }
}
