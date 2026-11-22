//! Screen capture for SnapIt, built on `xcap`.
//!
//! Geometry is expressed in *monitor units*: the coordinate space `xcap` reports
//! for a monitor (points on macOS, physical pixels on Windows). Captured
//! images are always in physical pixels; [`Screenshot::pixels_per_unit`] maps
//! between the two.

use std::{
    path::Path,
    sync::{
        Arc,
        mpsc::{Receiver, RecvTimeoutError},
    },
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow};
pub use image::RgbaImage;
use image::{ImageEncoder as _, ImageFormat, imageops};
use xcap::Monitor;
pub use xcap::{HdrImage, OutputSize};

mod elements;
#[cfg(target_os = "macos")]
mod freeze;
mod windows;

pub use elements::{ElementHit, ElementRole, accessibility_available, request_accessibility};
pub use windows::WindowInfo;

/// A rectangle in monitor units, relative to the monitor's top-left corner.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct Region {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Region {
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.width < 1.0 || self.height < 1.0
    }

    pub fn scaled(&self, factor: f32) -> Self {
        Self::new(
            self.x * factor,
            self.y * factor,
            self.width * factor,
            self.height * factor,
        )
    }
}

/// Description of a physical display as reported by the OS.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct MonitorInfo {
    /// Platform display id (CGDirectDisplayID on macOS, HMONITOR on Windows).
    pub id: u32,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
    pub is_primary: bool,
}

impl MonitorInfo {
    fn from_monitor(monitor: &Monitor) -> Result<Self> {
        Ok(Self {
            id: monitor.id()?,
            name: monitor.name().unwrap_or_default(),
            x: monitor.x()?,
            y: monitor.y()?,
            width: monitor.width()?,
            height: monitor.height()?,
            scale_factor: monitor.scale_factor().unwrap_or(1.0),
            is_primary: monitor.is_primary().unwrap_or(false),
        })
    }
}

/// A frozen frame of one monitor.
pub struct Screenshot {
    monitor: MonitorInfo,
    image: Arc<RgbaImage>,
    hdr: Option<Arc<HdrImage>>,
    windows: Vec<WindowInfo>,
}

impl Screenshot {
    pub fn monitor(&self) -> &MonitorInfo {
        &self.monitor
    }

    /// On-screen windows at freeze time, monitor-relative and ordered
    /// front-to-back.
    pub fn windows(&self) -> &[WindowInfo] {
        &self.windows
    }

    pub fn image(&self) -> &RgbaImage {
        &self.image
    }

    /// The same frame as half-float scRGB, when the display is in HDR mode and
    /// the frame actually contains values above SDR white.
    pub fn hdr(&self) -> Option<&HdrImage> {
        self.hdr.as_deref()
    }

    pub fn into_image(self) -> RgbaImage {
        match Arc::try_unwrap(self.image) {
            Ok(image) => image,
            Err(image) => (*image).clone(),
        }
    }

    /// Physical pixels per monitor unit.
    pub fn pixels_per_unit(&self) -> f32 {
        if self.monitor.width == 0 {
            return 1.0;
        }
        self.image.width() as f32 / self.monitor.width as f32
    }

    /// Crop `region` (in monitor units) out of the frozen frame.
    pub fn crop(&self, region: Region) -> Option<RgbaImage> {
        crop_pixels(&self.image, region.scaled(self.pixels_per_unit()))
    }

    /// Crop `region` (in monitor units) out of the HDR frame.
    pub fn crop_hdr(&self, region: Region) -> Option<HdrImage> {
        let hdr = self.hdr.as_ref()?;
        let (x, y, width, height) = pixel_rect(
            region.scaled(self.pixels_per_unit()),
            hdr.width(),
            hdr.height(),
        )?;
        hdr.crop(x, y, width, height)
    }
}

/// Crop a rectangle given in physical pixels, clamped to the image bounds.
pub fn crop_pixels(image: &RgbaImage, region: Region) -> Option<RgbaImage> {
    let (x, y, width, height) = pixel_rect(region, image.width(), image.height())?;
    Some(imageops::crop_imm(image, x, y, width, height).to_image())
}

/// `(x, y, width, height)` of `region` in pixels, clamped to the frame.
fn pixel_rect(region: Region, width: u32, height: u32) -> Option<(u32, u32, u32, u32)> {
    let x = region.x.max(0.0).round() as u32;
    let y = region.y.max(0.0).round() as u32;
    let right = ((region.x + region.width).round() as u32).min(width);
    let bottom = ((region.y + region.height).round() as u32).min(height);
    (right > x && bottom > y).then_some((x, y, right - x, bottom - y))
}

pub fn monitors() -> Result<Vec<MonitorInfo>> {
    Monitor::all()
        .context("failed to enumerate monitors")?
        .iter()
        .map(MonitorInfo::from_monitor)
        .collect()
}

/// Monitors worth trying in HDR, by monitor id.
///
/// macOS reads the panel's current EDR headroom through AppKit, so call this
/// on the main thread. Displays left out keep the ordinary 8-bit freeze, and
/// the list is empty on Intel Macs and below macOS 15, where the system has
/// no public API for extended-range pixels.
#[cfg(target_os = "macos")]
pub fn hdr_candidates() -> Vec<u32> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSScreen;
    use objc2_foundation::{NSNumber, NSString};

    if !cfg!(target_arch = "aarch64") || !objc2::available!(macos = 15.0) {
        return Vec::new();
    }
    let Some(mtm) = MainThreadMarker::new() else {
        log::warn!("hdr_candidates must run on the main thread");
        return Vec::new();
    };
    NSScreen::screens(mtm)
        .iter()
        .filter(|screen| screen.maximumExtendedDynamicRangeColorComponentValue() > 1.0)
        .filter_map(|screen| {
            let number = screen
                .deviceDescription()
                .objectForKey(&NSString::from_str("NSScreenNumber"))?;
            number
                .downcast::<NSNumber>()
                .ok()
                .map(|number| number.unsignedIntValue())
        })
        .collect()
}

/// Windows decides for itself once it asks the output for its color space, so
/// every monitor is a candidate and an SDR output costs only that query.
#[cfg(target_os = "windows")]
pub fn hdr_candidates() -> Vec<u32> {
    Monitor::all()
        .map(|monitors| {
            monitors
                .iter()
                .filter_map(|monitor| monitor.id().ok())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn hdr_candidates() -> Vec<u32> {
    Vec::new()
}

/// Freeze every monitor. Monitors that fail to capture are skipped; an error is
/// returned only when nothing could be captured (usually a permission problem).
///
/// `hdr_monitors` is the [`hdr_candidates`] list: only those monitors are
/// tried with an extended-range capture before the ordinary SDR freeze.
pub fn capture_all(hdr_monitors: &[u32]) -> Result<Vec<Screenshot>> {
    let monitors = Monitor::all().context("failed to enumerate monitors")?;
    let (images, mut windows) = capture_monitors(&monitors, hdr_monitors);
    // Overlay windows can already exist during the freeze, so never let this
    // process's own windows become snap targets.
    let own_pid = std::process::id();
    windows.retain(|window| window.pid != own_pid);
    let mut shots = Vec::with_capacity(monitors.len());
    let mut last_error = None;
    for (monitor, image) in monitors.iter().zip(images) {
        let shot = MonitorInfo::from_monitor(monitor).and_then(|info| {
            let captured = image?;
            Ok(Screenshot {
                monitor: info,
                image: Arc::new(captured.image),
                hdr: captured.hdr.map(Arc::new),
                windows: Vec::new(),
            })
        });
        match shot {
            Ok(shot) => shots.push(shot),
            Err(err) => {
                log::warn!("failed to capture monitor: {err:#}");
                last_error = Some(err);
            }
        }
    }
    if shots.is_empty() {
        return Err(last_error.unwrap_or_else(|| anyhow!("no monitors found")));
    }
    for shot in &mut shots {
        shot.windows = windows::clip_to_monitor(&windows, &shot.monitor);
    }
    Ok(shots)
}

/// Capture each monitor's pixels, returning results in monitor order.
#[cfg(target_os = "macos")]
fn capture_monitors(
    monitors: &[Monitor],
    hdr_monitors: &[u32],
) -> (Vec<Result<MonitorCapture>>, Vec<WindowInfo>) {
    // A 5K frame takes long enough that multi-monitor freezes should overlap;
    // the window list is independent and runs alongside them.
    std::thread::scope(|scope| {
        let windows = scope.spawn(windows::all);
        let captures: Vec<_> = monitors
            .iter()
            .map(|monitor| {
                let try_hdr = monitor.id().is_ok_and(|id| hdr_monitors.contains(&id));
                scope.spawn(move || capture_one(monitor, try_hdr))
            })
            .collect();
        let images = captures
            .into_iter()
            .map(|capture| {
                capture
                    .join()
                    .unwrap_or_else(|_| Err(anyhow!("monitor capture panicked")))
            })
            .collect();
        (images, windows.join().unwrap_or_default())
    })
}

/// Non-macOS platforms keep the serial freeze: their monitor handles are not
/// safe to move across threads.
#[cfg(not(target_os = "macos"))]
fn capture_monitors(
    monitors: &[Monitor],
    hdr_monitors: &[u32],
) -> (Vec<Result<MonitorCapture>>, Vec<WindowInfo>) {
    let images = monitors
        .iter()
        .map(|monitor| {
            let try_hdr = monitor.id().is_ok_and(|id| hdr_monitors.contains(&id));
            capture_one(monitor, try_hdr)
        })
        .collect();
    (images, windows::all())
}

/// One monitor's pixels, with the HDR frame when the display delivered one.
struct MonitorCapture {
    image: RgbaImage,
    hdr: Option<HdrImage>,
}

impl MonitorCapture {
    /// Keep the HDR frame only when it actually carries highlights; otherwise
    /// the SDR view is the whole capture and a 16-bit file would only be larger.
    fn from_hdr(hdr: HdrImage) -> Self {
        let image = hdr.to_sdr();
        let hdr = hdr.has_highlights().then_some(hdr);
        Self { image, hdr }
    }
}

fn capture_one(monitor: &Monitor, try_hdr: bool) -> Result<MonitorCapture> {
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let _ = try_hdr;
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    if try_hdr {
        match monitor.capture_image_hdr() {
            Ok(hdr) => {
                log::debug!("captured HDR frame {}x{}", hdr.width(), hdr.height());
                return Ok(MonitorCapture::from_hdr(hdr));
            }
            Err(xcap::XCapError::NotSupported) => {}
            Err(err) => log::warn!("HDR capture failed ({err:#}); falling back to SDR"),
        }
    }
    capture_sdr(monitor)
}

#[cfg(target_os = "macos")]
fn capture_sdr(monitor: &Monitor) -> Result<MonitorCapture> {
    freeze::display(monitor)
        .or_else(|err| {
            log::warn!("fast freeze failed ({err:#}); falling back to xcap");
            monitor.capture_image().map_err(anyhow::Error::from)
        })
        .map(|image| MonitorCapture { image, hdr: None })
}

#[cfg(not(target_os = "macos"))]
fn capture_sdr(monitor: &Monitor) -> Result<MonitorCapture> {
    monitor
        .capture_image()
        .map(|image| MonitorCapture { image, hdr: None })
        .map_err(anyhow::Error::from)
}

/// Accessibility hit-test chain under a global point, the element under the
/// pointer first, in global monitor units. `window` is the window under the
/// cursor (see [`Screenshot::windows`]); platforms without an element API
/// return nothing.
pub fn elements_at(window: WindowInfo, x: f32, y: f32) -> Vec<ElementHit> {
    elements::at(window, x, y)
}

/// Cheap, non-prompting check for screen-recording permission. macOS answers
/// from TCC state without capturing a frame; other platforms have no separate
/// permission gate and always grant. Callers use this instead of a throwaway
/// full-screen capture, which costs ~10 ms and is only needed when a recorder
/// never receives a frame at all.
pub fn screen_recording_granted() -> bool {
    #[cfg(target_os = "macos")]
    {
        objc2_core_graphics::CGPreflightScreenCaptureAccess()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// Live capture of a region (in monitor units) of one monitor.
pub struct RegionCapturer {
    monitor: Monitor,
    region: Region,
}

impl RegionCapturer {
    pub fn new(monitor_id: u32, region: Region) -> Result<Self> {
        let (monitor, region) = resolve_region(monitor_id, region)?;
        Ok(Self { monitor, region })
    }

    pub fn capture(&self) -> Result<RgbaImage> {
        let r = self.region;
        Ok(self
            .monitor
            .capture_region(r.x as u32, r.y as u32, r.width as u32, r.height as u32)?)
    }
}

/// A frame from [`LiveRecorder`], cropped to the recorded region.
pub struct LiveFrame {
    pub image: RgbaImage,
    /// Time since the recorder was created, on the recorder's own clock.
    pub timestamp: Duration,
}

/// Outcome of waiting for the next frame.
pub enum LiveFrameWait {
    Frame(LiveFrame),
    Timeout,
    Disconnected,
}

/// Hardware-paced capture of a region, backed by xcap's platform recorder.
///
/// Unlike repeated [`RegionCapturer`] screenshots, the platform captures and
/// converts frames itself. The receiver blocks, so use [`Self::recv_timeout`]
/// when the caller also has to notice a stop request.
pub struct LiveRecorder {
    recorder: xcap::VideoRecorder,
    frames: Receiver<xcap::Frame>,
}

impl LiveRecorder {
    pub fn start(monitor_id: u32, region: Region, fps: u32) -> Result<Self> {
        Self::start_with(monitor_id, region, fps, false, OutputSize::Native)
    }

    /// `output_size` states the intent, not a pixel number: `Logical` asks for
    /// frames at the selection's monitor-unit size (macOS compositor scaling),
    /// `Native` for physical pixels. It is best-effort — DXGI ignores it and
    /// the platform may still deliver native frames — so callers must keep
    /// their own downscale as a fallback.
    pub fn start_with(
        monitor_id: u32,
        region: Region,
        fps: u32,
        captures_cursor: bool,
        output_size: OutputSize,
    ) -> Result<Self> {
        let (monitor, region) = resolve_region(monitor_id, region)?;
        let options = xcap::VideoRecorderOptions::default()
            .fps(fps)
            .region(xcap::CaptureRegion {
                x: region.x as u32,
                y: region.y as u32,
                width: region.width as u32,
                height: region.height as u32,
            })
            .captures_cursor(captures_cursor)
            // The overlay and the recording bar belong to this process; the
            // platform recorder must leave them out of the frames.
            .excludes_own_windows(true)
            .output_size(output_size);
        let (recorder, frames) = monitor.video_recorder_with_options(options)?;
        recorder.start()?;
        Ok(Self { recorder, frames })
    }

    pub fn recv_timeout(&self, timeout: Duration) -> LiveFrameWait {
        match self.frames.recv_timeout(timeout) {
            Ok(frame) => match RgbaImage::from_raw(frame.width, frame.height, frame.raw) {
                Some(image) => LiveFrameWait::Frame(LiveFrame {
                    image,
                    timestamp: frame.timestamp,
                }),
                None => {
                    log::warn!("recorder delivered a frame with mismatched dimensions");
                    LiveFrameWait::Timeout
                }
            },
            Err(RecvTimeoutError::Timeout) => LiveFrameWait::Timeout,
            Err(RecvTimeoutError::Disconnected) => LiveFrameWait::Disconnected,
        }
    }

    /// Stop the platform capture. Dropping the receiver also releases a
    /// producer blocked on the next frame.
    pub fn stop(self) -> Result<()> {
        self.recorder.stop()?;
        Ok(())
    }
}

fn resolve_region(monitor_id: u32, region: Region) -> Result<(Monitor, Region)> {
    let monitor = Monitor::all()?
        .into_iter()
        .find(|monitor| monitor.id().ok() == Some(monitor_id))
        .ok_or_else(|| anyhow!("monitor {monitor_id} not found"))?;
    let width = monitor.width()? as f32;
    let height = monitor.height()? as f32;
    let x = region.x.clamp(0.0, width - 1.0).floor();
    let y = region.y.clamp(0.0, height - 1.0).floor();
    let region = Region::new(
        x,
        y,
        region.width.min(width - x).floor().max(1.0),
        region.height.min(height - y).floor().max(1.0),
    );
    Ok((monitor, region))
}

/// PNG for the clipboard. The default encoder spends most of its time choosing
/// an adaptive filter per scanline; `Sub` skips that (20 ms -> 3 ms for a
/// 1560x1040 capture) for ~27% more bytes, which is the right trade for a
/// transient pasteboard payload. [`save_png`] keeps the small-file default.
pub fn encode_png(image: &RgbaImage) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new_with_quality(
        &mut bytes,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::Sub,
    )
    .write_image(
        image.as_raw(),
        image.width(),
        image.height(),
        image::ExtendedColorType::Rgba8,
    )
    .context("failed to encode PNG")?;
    Ok(bytes)
}

pub fn save_png(image: &RgbaImage, path: &Path) -> Result<()> {
    image
        .save_with_format(path, ImageFormat::Png)
        .with_context(|| format!("failed to save {}", path.display()))
}

/// JPEG has no alpha channel, so pixels are flattened to RGB first.
pub fn save_jpeg(image: &RgbaImage, path: &Path, quality: u8) -> Result<()> {
    let rgb = image::DynamicImage::ImageRgba8(image.clone()).into_rgb8();
    let file = std::fs::File::create(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(
        std::io::BufWriter::new(file),
        quality.clamp(1, 100),
    );
    rgb.write_with_encoder(encoder)
        .with_context(|| format!("failed to save {}", path.display()))
}

/// HDR stills are BT.2020 primaries with the PQ transfer function, written as
/// 16-bit RGB. The cICP chunk carries `(primaries, transfer, matrix, full
/// range)`; PNG has no matrix stage, so it is 0 (identity / RGB).
const HDR_CICP: [u8; 4] = [9, 16, 0, 1];

/// linear sRGB (scRGB primaries) -> linear BT.2020, so PQ-encoded values land
/// in the same wide-gamut container as HDR video.
const BT2020_FROM_SRGB: [[f32; 3]; 3] = [
    [0.627_404, 0.329_282, 0.043_313_6],
    [0.069_097, 0.919_54, 0.011_361_2],
    [0.016_391_6, 0.088_013_2, 0.895_595],
];

const PQ_STEPS: usize = 65536;

/// Encode an HDR frame as a 16-bit RGB PNG labelled with cICP.
pub fn encode_png_hdr(hdr: &HdrImage) -> Result<Vec<u8>> {
    let data = pq_bt2020_samples(hdr);
    let mut bytes = Vec::new();
    write_hdr_png(&mut bytes, hdr, &data).context("failed to encode HDR PNG")?;
    Ok(bytes)
}

pub fn save_png_hdr(hdr: &HdrImage, path: &Path) -> Result<()> {
    let data = pq_bt2020_samples(hdr);
    let file = std::fs::File::create(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    write_hdr_png(std::io::BufWriter::new(file), hdr, &data)
        .with_context(|| format!("failed to save {}", path.display()))
}

fn write_hdr_png<W: std::io::Write>(
    writer: W,
    hdr: &HdrImage,
    data: &[u8],
) -> std::result::Result<(), png::EncodingError> {
    let mut encoder = png::Encoder::new(writer, hdr.width(), hdr.height());
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Sixteen);
    let mut writer = encoder.write_header()?;
    writer.write_chunk(png::chunk::cICP, &HDR_CICP)?;
    writer.write_image_data(data)?;
    writer.finish()
}

/// PQ-encode every pixel as big-endian 16-bit BT.2020 RGB.
fn pq_bt2020_samples(hdr: &HdrImage) -> Vec<u8> {
    let width = hdr.width() as usize;
    let height = hdr.height() as usize;
    let mut out = vec![0u8; width * height * 6];
    let white_nits = hdr.reference_white_nits();
    let pq = pq_lut();
    // Same threshold as the overlay's RGBA->BGRA swap: below it, spawning
    // threads costs more than the conversion.
    if out.len() < 1024 * 1024 {
        write_pq_band(hdr, &mut out, 0, white_nits, &pq);
        return out;
    }
    let threads = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(4)
        .clamp(2, 8);
    let row_bytes = width * 6;
    let rows_per_band = height.div_ceil(threads).max(1);
    let band_bytes = rows_per_band * row_bytes;
    let pq = &*pq;
    std::thread::scope(|scope| {
        for (band_index, band) in out.chunks_mut(band_bytes).enumerate() {
            scope.spawn(move || {
                write_pq_band(hdr, band, band_index * rows_per_band, white_nits, pq);
            });
        }
    });
    out
}

fn write_pq_band(
    hdr: &HdrImage,
    band: &mut [u8],
    first_row: usize,
    white_nits: f32,
    pq: &[u16; PQ_STEPS],
) {
    let width = hdr.width() as usize;
    for (row, line) in band.chunks_mut(width * 6).enumerate() {
        let y = (first_row + row) as u32;
        for (x, pixel) in line.chunks_mut(6).enumerate() {
            let [r, g, b] = hdr.pixel_f32(x as u32, y);
            let m = &BT2020_FROM_SRGB;
            let rgb = [
                m[0][0] * r + m[0][1] * g + m[0][2] * b,
                m[1][0] * r + m[1][1] * g + m[1][2] * b,
                m[2][0] * r + m[2][1] * g + m[2][2] * b,
            ];
            for (bytes, linear) in pixel.chunks_mut(2).zip(rgb) {
                bytes.copy_from_slice(&pq_sample(linear, white_nits, pq).to_be_bytes());
            }
        }
    }
}

/// Map one linear scRGB component to a 16-bit PQ code.
fn pq_sample(linear: f32, white_nits: f32, pq: &[u16; PQ_STEPS]) -> u16 {
    let nits = (linear * white_nits).clamp(0.0, 10_000.0);
    // Indexing by sqrt keeps sub-nit precision where PQ needs it most; the
    // table itself is linear in nits.
    let index = ((nits / 10_000.0).sqrt() * (PQ_STEPS - 1) as f32).round() as usize;
    pq[index.min(PQ_STEPS - 1)]
}

/// 16-bit PQ codes over 0..=10000 nits, indexed by `sqrt(nits / 10000)`.
fn pq_lut() -> Box<[u16; PQ_STEPS]> {
    // SMPTE ST 2084 constants.
    const M1: f64 = 2610.0 / 16384.0;
    const M2: f64 = 2523.0 / 32.0;
    const C1: f64 = 3424.0 / 4096.0;
    const C2: f64 = 2413.0 / 128.0;
    const C3: f64 = 2392.0 / 128.0;
    let mut lut = Box::new([0u16; PQ_STEPS]);
    for (index, code) in lut.iter_mut().enumerate() {
        let normalized = index as f64 / (PQ_STEPS - 1) as f64;
        let luminance = normalized * normalized;
        let powered = luminance.powf(M1);
        let encoded = ((C1 + C2 * powered) / (1.0 + C3 * powered)).powf(M2);
        *code = (encoded.clamp(0.0, 1.0) * f64::from(u16::MAX)).round() as u16;
    }
    lut
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn hdr_pixels(linear: &[[f32; 3]], white_nits: f32) -> HdrImage {
        let mut raw = Vec::new();
        for [r, g, b] in linear {
            for value in [r, g, b] {
                raw.extend_from_slice(&half_bits(*value).to_le_bytes());
            }
        }
        HdrImage::new(linear.len() as u32, 1, raw, white_nits)
    }

    /// Minimal f32 -> f16 conversion for the handful of test values.
    fn half_bits(value: f32) -> u16 {
        let bits = value.to_bits();
        let sign = ((bits >> 16) & 0x8000) as u16;
        let exponent = ((bits >> 23) & 0xff) as i32 - 127 + 15;
        let mantissa = (bits >> 13) & 0x3ff;
        sign | ((exponent as u16) << 10) | mantissa as u16
    }

    #[test]
    fn hdr_png_is_16_bit_pq_bt2020() {
        let hdr = hdr_pixels(&[[1.0, 1.0, 1.0], [4.0, 4.0, 4.0]], 203.0);
        let encoded = encode_png_hdr(&hdr).unwrap();

        let mut reader = png::Decoder::new(Cursor::new(encoded)).read_info().unwrap();
        assert_eq!(reader.info().bit_depth, png::BitDepth::Sixteen);
        assert_eq!(reader.info().color_type, png::ColorType::Rgb);
        let cicp = reader.info().coding_independent_code_points.unwrap();
        assert_eq!(cicp.color_primaries, 9);
        assert_eq!(cicp.transfer_function, 16);
        assert_eq!(cicp.matrix_coefficients, 0);
        assert!(cicp.is_video_full_range_image);

        let mut buf = vec![0; reader.output_buffer_size().expect("known image size")];
        let frame = reader.next_frame(&mut buf).unwrap();
        let samples = &buf[..frame.buffer_size()];
        let channel = |index: usize| {
            f32::from(u16::from_be_bytes([
                samples[index * 2],
                samples[index * 2 + 1],
            ])) / 65535.0
        };
        // PQ(203 nits) and PQ(812 nits).
        assert!((channel(0) - 0.58069).abs() < 0.002, "{}", channel(0));
        assert!((channel(3) - 0.72914).abs() < 0.002, "{}", channel(3));
    }

    #[test]
    fn crop_hdr_clamps_to_the_frame() {
        let mut raw = Vec::new();
        for value in 0..12u16 {
            let bits = half_bits(value as f32);
            raw.extend_from_slice(&bits.to_le_bytes());
        }
        let hdr = HdrImage::new(2, 2, raw, 80.0);
        let cropped = hdr.crop(1, 1, 5, 5).unwrap();
        assert_eq!((cropped.width(), cropped.height()), (1, 1));
    }

    #[test]
    fn bt2020_matrix_preserves_neutral_and_keeps_red_inside() {
        for row in &BT2020_FROM_SRGB {
            assert!((row.iter().sum::<f32>() - 1.0).abs() < 1e-4);
        }
        // sRGB primaries are inside BT.2020, so pure red gains a little
        // green and blue instead of pushing another channel negative.
        let [r, g, b] = [
            BT2020_FROM_SRGB[0][0],
            BT2020_FROM_SRGB[1][0],
            BT2020_FROM_SRGB[2][0],
        ];
        assert!(r > 0.6 && (0.05..0.1).contains(&g) && b < 0.03);
    }

    #[test]
    fn pq_encoding_matches_reference_levels() {
        let pq = pq_lut();
        assert_eq!(pq_sample(0.0, 80.0, &pq), 0);
        assert_eq!(pq_sample(125.0, 80.0, &pq), u16::MAX);
        // ST 2084 reference codes: 100 nits -> 0.5081, 1000 nits -> 0.7518.
        for (nits, expected) in [(100.0, 0.5081), (1000.0, 0.7518)] {
            let code = f32::from(pq_sample(nits / 80.0, 80.0, &pq)) / 65535.0;
            assert!((code - expected).abs() < 0.001, "{nits} nits -> {code}");
        }
    }
}
