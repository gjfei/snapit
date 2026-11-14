//! Screen capture for SnapIt, built on `xcap`.
//!
//! Geometry is expressed in *monitor units*: the coordinate space `xcap` reports
//! for a monitor (points on macOS, physical pixels on Windows). Captured
//! images are always in physical pixels; [`Screenshot::pixels_per_unit`] maps
//! between the two.

use std::{
    io::Cursor,
    path::Path,
    sync::{
        Arc,
        mpsc::{Receiver, RecvTimeoutError},
    },
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow};
pub use image::RgbaImage;
use image::{ImageFormat, imageops};
use xcap::Monitor;

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
}

/// Crop a rectangle given in physical pixels, clamped to the image bounds.
pub fn crop_pixels(image: &RgbaImage, region: Region) -> Option<RgbaImage> {
    let x = region.x.max(0.0).round() as u32;
    let y = region.y.max(0.0).round() as u32;
    let right = ((region.x + region.width).round() as u32).min(image.width());
    let bottom = ((region.y + region.height).round() as u32).min(image.height());
    if right <= x || bottom <= y {
        return None;
    }
    Some(imageops::crop_imm(image, x, y, right - x, bottom - y).to_image())
}

pub fn monitors() -> Result<Vec<MonitorInfo>> {
    Monitor::all()
        .context("failed to enumerate monitors")?
        .iter()
        .map(MonitorInfo::from_monitor)
        .collect()
}

/// Freeze every monitor. Monitors that fail to capture are skipped; an error is
/// returned only when nothing could be captured (usually a permission problem).
pub fn capture_all() -> Result<Vec<Screenshot>> {
    let monitors = Monitor::all().context("failed to enumerate monitors")?;
    let (images, mut windows) = capture_monitors(&monitors);
    // Overlay windows can already exist during the freeze, so never let this
    // process's own windows become snap targets.
    let own_pid = std::process::id();
    windows.retain(|window| window.pid != own_pid);
    let mut shots = Vec::with_capacity(monitors.len());
    let mut last_error = None;
    for (monitor, image) in monitors.iter().zip(images) {
        let shot = MonitorInfo::from_monitor(monitor).and_then(|info| {
            Ok(Screenshot {
                monitor: info,
                image: Arc::new(image?),
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
fn capture_monitors(monitors: &[Monitor]) -> (Vec<Result<RgbaImage>>, Vec<WindowInfo>) {
    // A 5K frame takes long enough that multi-monitor freezes should overlap;
    // the window list is independent and runs alongside them.
    std::thread::scope(|scope| {
        let windows = scope.spawn(windows::all);
        let captures: Vec<_> = monitors
            .iter()
            .map(|monitor| {
                scope.spawn(move || {
                    freeze::display(monitor).or_else(|err| {
                        log::warn!("fast freeze failed ({err:#}); falling back to xcap");
                        monitor.capture_image().map_err(anyhow::Error::from)
                    })
                })
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
fn capture_monitors(monitors: &[Monitor]) -> (Vec<Result<RgbaImage>>, Vec<WindowInfo>) {
    let images = monitors
        .iter()
        .map(|monitor| monitor.capture_image().map_err(anyhow::Error::from))
        .collect();
    (images, windows::all())
}

/// Accessibility hit-test chain under a global point, the element under the
/// pointer first, in global monitor units. `window` is the window under the
/// cursor (see [`Screenshot::windows`]); platforms without an element API
/// return nothing.
pub fn elements_at(window: WindowInfo, x: f32, y: f32) -> Vec<ElementHit> {
    elements::at(window, x, y)
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
        Self::start_with(monitor_id, region, fps, false)
    }

    pub fn start_with(
        monitor_id: u32,
        region: Region,
        fps: u32,
        captures_cursor: bool,
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
            .excludes_own_windows(true);
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

pub fn encode_png(image: &RgbaImage) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
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
