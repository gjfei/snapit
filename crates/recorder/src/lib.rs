//! Recording of a screen region to GIF or MP4.
//!
//! Frames are captured on a dedicated thread and handed to a [`FrameSink`],
//! which owns the encoder. Both formats see the same capture loop, the same
//! duplicate-frame dropping and the same duration cap; only the encoder
//! differs, so a recording behaves identically whichever container is picked.
//!
//! The GIF sink streams frames to a writer thread that quantizes and encodes
//! them with the `gif` crate, so memory stays bounded regardless of recording
//! length. Consecutive identical frames are dropped; their time is folded into
//! the previous frame, which for video becomes the previous sample's duration.
//!
//! Three things keep the GIF writer ahead of the capture clock: one NeuQuant
//! palette trained on a sparse spatial sample (never per-frame
//! `Frame::from_rgba_speed`), a lazily filled 6-bit RGB lookup table that makes
//! mapping one array read per pixel, and partial frames — with `Keep` disposal,
//! LZW only encodes the rectangle that changed. A palette refresh forces one
//! full frame, since every index changes meaning.
//!
//! Retina frames arrive in physical pixels while the selection is in logical
//! points, so `video::prepare` halves both axes with an integer box mean. Both
//! sinks share it; `image::imageops::resize(Triangle)` cost ~10x as much.

use std::{
    fs::File,
    io::{BufWriter, Write as _},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow};
use color_quant::NeuQuant;
use gif::Repeat;
use snapit_capture::{
    DynamicRange, HdrImage, LiveFrameData, LiveFrameWait, LiveRecorder, OutputSize, Region,
    RegionCapturer, RgbaImage,
};

mod video;

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e3
}

/// Bits per channel in the GIF colour lookup table. 6 bits keeps the table at
/// 256 KiB and the quantisation error within two levels per channel.
const GIF_LUT_BITS: usize = 6;
const GIF_LUT_BINS: usize = 1 << GIF_LUT_BITS;
const GIF_LUT_SHIFT: usize = 8 - GIF_LUT_BITS;
const GIF_LUT_SIZE: usize = GIF_LUT_BINS * GIF_LUT_BINS * GIF_LUT_BINS;
/// Pixels sampled to decide whether the current palette still fits the scene.
const GIF_ERROR_SAMPLES: usize = 4_096;
/// Average squared RGB error that triggers a new palette.
const GIF_REFRESH_ERROR: u64 = 1_200;
/// Never retrain more often than this many frames, even on noisy content.
const GIF_REFRESH_COOLDOWN: usize = 12;
/// Only encode a sub-rectangle when the changed area is below this share of the
/// frame; a nearly full redraw is cheaper as one full frame.
const GIF_PARTIAL_MAX_PERCENT: u64 = 70;

/// Container the frames are written to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RecordFormat {
    #[default]
    Gif,
    Mp4,
}

impl RecordFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Gif => "gif",
            Self::Mp4 => "mp4",
        }
    }

    /// Config round-trip for the remembered format: `"gif"` or `"mp4"`.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "gif" => Some(Self::Gif),
            "mp4" => Some(Self::Mp4),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct RecordOptions {
    pub fps: u32,
    pub max_duration: Duration,
    /// GIF quantization quality, 1-100.
    pub quality: u8,
    /// Downscale frames to at most this width (e.g. the region's logical width on HiDPI).
    pub max_width: Option<u32>,
    pub format: RecordFormat,
    /// Draw the mouse cursor. macOS only; Windows duplication excludes it.
    pub captures_cursor: bool,
    /// Capture and encode extended-range frames. MP4 only, and best-effort: a
    /// display without HDR support keeps recording 8-bit SDR as before.
    pub hdr: bool,
}

impl Default for RecordOptions {
    fn default() -> Self {
        Self {
            fps: 12,
            max_duration: Duration::ZERO,
            quality: 90,
            max_width: None,
            format: RecordFormat::default(),
            captures_cursor: false,
            hdr: false,
        }
    }
}

impl RecordOptions {
    pub fn with_fps(mut self, fps: u32) -> Self {
        self.fps = fps.clamp(1, 30);
        self
    }

    /// A zero duration means "record until stopped".
    pub fn with_max_duration(mut self, max_duration: Duration) -> Self {
        self.max_duration = max_duration;
        self
    }

    pub fn with_quality(mut self, quality: u8) -> Self {
        self.quality = quality.clamp(1, 100);
        self
    }

    pub fn with_max_width(mut self, max_width: Option<u32>) -> Self {
        self.max_width = max_width;
        self
    }

    pub fn with_format(mut self, format: RecordFormat) -> Self {
        self.format = format;
        self
    }

    pub fn with_cursor(mut self, captures_cursor: bool) -> Self {
        self.captures_cursor = captures_cursor;
        self
    }

    pub fn with_hdr(mut self, hdr: bool) -> Self {
        self.hdr = hdr;
        self
    }
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct RecordingSummary {
    pub path: PathBuf,
    pub format: RecordFormat,
    pub frames: usize,
    pub duration: Duration,
    /// The first captured frame, at capture resolution.
    pub first_frame: Option<RgbaImage>,
}

/// Pause bookkeeping shared by the UI thread and the capture thread.
///
/// Wall-clock time keeps flowing while a recording is paused, but neither the
/// encoder's timeline nor the duration cap should. `total` holds completed
/// pauses; `since` is the start of the current one, so the capture thread can
/// read the accumulated pause without waiting for the UI to resume it.
#[derive(Default)]
struct PauseClock {
    total: AtomicU64,
    since: Mutex<Option<Instant>>,
}

impl PauseClock {
    fn since(&self) -> MutexGuard<'_, Option<Instant>> {
        self.since
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn is_paused(&self) -> bool {
        self.since().is_some()
    }

    fn set_paused(&self, paused: bool) {
        let mut since = self.since();
        if paused {
            if since.is_none() {
                *since = Some(Instant::now());
            }
        } else if let Some(at) = since.take() {
            self.total
                .fetch_add(at.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
    }

    fn paused_duration(&self) -> Duration {
        let total = Duration::from_nanos(self.total.load(Ordering::Relaxed));
        let current = self
            .since()
            .as_ref()
            .map(|at| at.elapsed())
            .unwrap_or_default();
        total.saturating_add(current)
    }
}

/// A running recording. Call [`Recorder::stop`] to finish and flush the file,
/// or [`Recorder::discard`] to throw it away.
pub struct Recorder {
    stop: Arc<AtomicBool>,
    paused: Arc<PauseClock>,
    frames: Arc<AtomicUsize>,
    started: Instant,
    max_duration: Duration,
    output: PathBuf,
    format: RecordFormat,
    capture_thread: JoinHandle<Result<(Duration, Option<RgbaImage>)>>,
}

impl Recorder {
    /// Start recording `region` (monitor units) of `monitor_id` into `output`.
    pub fn start(
        monitor_id: u32,
        region: Region,
        options: RecordOptions,
        output: impl AsRef<Path>,
    ) -> Result<Self> {
        let output = output.as_ref().to_path_buf();
        let started = Instant::now();
        // Surface a missing permission before threads spawn. A full-screen
        // capture was used here only for this check; on macOS TCC answers the
        // same question instantly, and the fall-back capture is now deferred
        // to the rare path where the recorder never receives a frame.
        if !snapit_capture::screen_recording_granted() {
            return Err(anyhow!("screen recording permission is not granted"));
        }

        let stop = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(PauseClock::default());
        let frames = Arc::new(AtomicUsize::new(0));
        // AVFoundation takes about a second to start delivering frames. Wait
        // for the first one so the UI timer and the saved clip share an origin
        // and the first second is not silently lost.
        let (ready_tx, ready_rx) = sync_channel::<std::result::Result<(), String>>(1);
        let capture_thread = {
            let stop = stop.clone();
            let frames = frames.clone();
            let job = CaptureJob {
                monitor_id,
                region,
                output: output.clone(),
                options: options.clone(),
                stop,
                paused: paused.clone(),
                frames,
            };
            thread::Builder::new()
                .name("snapit-capture".into())
                .spawn(move || {
                    // Windows monitor handles are raw `HMONITOR` pointers and are
                    // not `Send`, so the recorder is built on the thread that uses it.
                    capture_loop(job, Some(ready_tx))
                })?
        };

        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => log::info!(
                target: "timing",
                "recorder start: ready after {:.1} ms (permission preflight only, no probe capture)",
                ms(started.elapsed())
            ),
            Ok(Err(message)) => {
                stop.store(true, Ordering::Relaxed);
                let _ = capture_thread.join();
                return Err(anyhow!("{message}"));
            }
            Err(RecvTimeoutError::Disconnected) => {
                stop.store(true, Ordering::Relaxed);
                return match capture_thread.join() {
                    Ok(Err(err)) => Err(err.context("recorder stopped before its first frame")),
                    _ => Err(anyhow!(
                        "the platform recorder stopped before its first frame"
                    )),
                };
            }
            Err(RecvTimeoutError::Timeout) => {
                stop.store(true, Ordering::Relaxed);
                let _ = capture_thread.join();
                return Err(anyhow!(
                    "the platform recorder delivered no frame within 5 s"
                ));
            }
        }

        Ok(Self {
            stop,
            paused,
            frames,
            started: Instant::now(),
            max_duration: options.max_duration,
            output,
            format: options.format,
            capture_thread,
        })
    }

    /// Time actually recorded, excluding paused stretches. This is what the UI
    /// shows and what the duration cap compares against.
    pub fn elapsed(&self) -> Duration {
        self.started
            .elapsed()
            .saturating_sub(self.paused.paused_duration())
    }

    pub fn is_paused(&self) -> bool {
        self.paused.is_paused()
    }

    /// Pause or resume frame delivery to the encoder. The capture thread keeps
    /// draining the platform recorder so its queue cannot grow while paused.
    pub fn set_paused(&self, paused: bool) {
        self.paused.set_paused(paused);
    }

    pub fn max_duration(&self) -> Duration {
        self.max_duration
    }

    /// Container being written, so the UI can label the recording.
    pub fn format(&self) -> RecordFormat {
        self.format
    }

    /// Number of distinct frames encoded so far.
    pub fn frame_count(&self) -> usize {
        self.frames.load(Ordering::Relaxed)
    }

    /// True once the capture loop ended on its own (max duration or error).
    pub fn is_finished(&self) -> bool {
        self.capture_thread.is_finished()
    }

    /// Stop capturing and wait for the file to be fully written. Blocking.
    pub fn stop(self) -> Result<RecordingSummary> {
        let requested = Instant::now();
        // Freeze the pause clock so stopping while paused reports the effective
        // duration instead of one that keeps shrinking while the sinks flush.
        self.paused.set_paused(false);
        self.stop.store(true, Ordering::Relaxed);
        let (duration, first_frame) = self
            .capture_thread
            .join()
            .map_err(|_| anyhow!("capture thread panicked"))??;
        log::info!(
            target: "timing",
            "recorder stop: capture thread joined after {:.1} ms",
            ms(requested.elapsed())
        );
        Ok(RecordingSummary {
            path: self.output,
            format: self.format,
            frames: self.frames.load(Ordering::Relaxed),
            duration,
            first_frame,
        })
    }

    /// Stop capturing and delete the container. No summary, no history, no
    /// clipboard: this is what "cancel" means to the UI.
    ///
    /// `stop` joins the sinks first, so their file handles are closed before
    /// the temporary file is removed (Windows refuses to delete an open file).
    /// Blocking, like [`Recorder::stop`]; the caller must run it off the UI
    /// thread.
    pub fn discard(self) -> Result<()> {
        let path = self.output.clone();
        let stopped = self.stop();
        let removed = match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).with_context(|| format!("failed to discard {}", path.display())),
        };
        stopped.map(|_| ())?;
        removed
    }
}

/// Where a captured frame goes. Both sinks are driven from the capture thread
/// and finish on it, so `stop` only has one thread left to join.
///
/// Frames are handed over as `Arc` because the capture loop keeps the previous
/// one for duplicate detection while the GIF sink holds it until the next
/// timestamp arrives; sharing the allocation avoids a full-frame copy per frame.
pub(crate) trait FrameSink: Send {
    /// Encode one frame, shown from `at` until the next one.
    fn push(&mut self, frame: FrameData, at: Duration) -> Result<()>;

    /// Flush the container. `end` is the recording's total duration, which is
    /// how long the last frame stays on screen.
    fn finish(self: Box<Self>, end: Duration) -> Result<()>;
}

/// The pixels of one frame: 8-bit SDR, or the extended-range samples an HDR
/// display delivered. A recorder stays in one variant for its whole session —
/// the platform either honours the HDR request or delivers SDR from the first
/// frame — so a sink picks its encoder from the frame it is opened with and
/// never has to handle the other kind.
#[derive(Clone)]
pub(crate) enum FrameData {
    Sdr(Arc<RgbaImage>),
    Hdr(Arc<HdrImage>),
}

impl From<LiveFrameData> for FrameData {
    fn from(data: LiveFrameData) -> Self {
        match data {
            LiveFrameData::Sdr(image) => Self::Sdr(Arc::new(image)),
            LiveFrameData::Hdr(hdr) => Self::Hdr(Arc::new(hdr)),
        }
    }
}

impl FrameData {
    fn dimensions(&self) -> (u32, u32) {
        match self {
            Self::Sdr(image) => image.dimensions(),
            Self::Hdr(image) => (image.width(), image.height()),
        }
    }

    /// Exact byte equality, the same rule the SDR path uses: a frame that only
    /// differs in the low bits of a half-float sample is not a duplicate.
    fn is_same_as(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Sdr(a), Self::Sdr(b)) => {
                a.dimensions() == b.dimensions() && a.as_raw() == b.as_raw()
            }
            (Self::Hdr(a), Self::Hdr(b)) => {
                a.width() == b.width() && a.height() == b.height() && a.as_bytes() == b.as_bytes()
            }
            _ => false,
        }
    }

    /// 8-bit pixels, for the history thumbnail and for a container that has no
    /// HDR path.
    fn to_sdr(&self) -> RgbaImage {
        match self {
            Self::Sdr(image) => (**image).clone(),
            Self::Hdr(image) => image.to_sdr(),
        }
    }
}

/// A frame handed to the writer thread: the image plus how long it stays on
/// screen, in GIF delay ticks of 10 ms.
struct PendingFrame {
    image: Arc<RgbaImage>,
    ticks: u16,
}

/// Channel to the writer thread plus the frame shown last, which is queued only
/// once its duration is known (a frame ends when the next one starts).
struct GifSink {
    sender: Option<SyncSender<PendingFrame>>,
    writer: Option<JoinHandle<Result<()>>>,
    pending: Option<Arc<RgbaImage>>,
    pending_at: Option<Duration>,
    /// Time the capture thread spent blocked on the bounded channel.
    blocked: Duration,
    max_block: Duration,
}

impl GifSink {
    fn new(output: &Path, options: &RecordOptions) -> Result<Self> {
        // Probe once so permission errors surface before threads spawn.
        let file = File::create(output)
            .with_context(|| format!("failed to create {}", output.display()))?;
        let quality = options.quality;
        let max_width = options.max_width;
        // Bounded so a slow encoder slows the capture down instead of piling
        // frames up in memory.
        let (sender, receiver) = sync_channel::<PendingFrame>(8);
        let writer = thread::Builder::new()
            .name("snapit-gif-writer".into())
            .spawn(move || encode_gif(receiver, file, quality, max_width))?;
        Ok(Self {
            sender: Some(sender),
            writer: Some(writer),
            pending: None,
            pending_at: None,
            blocked: Duration::ZERO,
            max_block: Duration::ZERO,
        })
    }
}

impl FrameSink for GifSink {
    fn push(&mut self, frame: FrameData, at: Duration) -> Result<()> {
        let frame = match frame {
            FrameData::Sdr(image) => image,
            // The GIF container has no extended range, so an HDR session
            // quantizes the tonemapped frames and loses only the highlights.
            FrameData::Hdr(hdr) => Arc::new(hdr.to_sdr()),
        };
        let sender = self
            .sender
            .as_ref()
            .ok_or_else(|| anyhow!("GIF encoder already closed"))?;
        if let (Some(image), Some(started)) = (self.pending.take(), self.pending_at) {
            let ticks = duration_to_ticks(at - started);
            let queued = Instant::now();
            let sent = sender
                .send(PendingFrame { image, ticks })
                .map_err(|_| anyhow!("failed to queue frame: the GIF writer stopped"));
            let wait = queued.elapsed();
            self.blocked += wait;
            self.max_block = self.max_block.max(wait);
            sent?;
        }
        self.pending = Some(frame);
        self.pending_at = Some(at);
        Ok(())
    }

    fn finish(mut self: Box<Self>, end: Duration) -> Result<()> {
        let queued = Instant::now();
        let result = match self.sender.take() {
            Some(sender) => match (self.pending.take(), self.pending_at) {
                (Some(image), Some(started)) => {
                    let ticks = duration_to_ticks(end.saturating_sub(started));
                    sender
                        .send(PendingFrame { image, ticks })
                        .map_err(|_| anyhow!("failed to flush the GIF: the writer stopped"))
                }
                _ => Ok(()),
            },
            None => Ok(()),
        };
        let final_send = queued.elapsed();
        result?;
        let joined = Instant::now();
        if let Some(writer) = self.writer.take() {
            writer
                .join()
                .map_err(|_| anyhow!("GIF writer thread panicked"))??;
        }
        log::info!(
            target: "timing",
            "gif sink: capture blocked {:.1} ms total (max {:.1} ms), final send {:.1} ms, writer join {:.1} ms",
            ms(self.blocked),
            ms(self.max_block),
            ms(final_send),
            ms(joined.elapsed())
        );
        Ok(())
    }
}

/// A NeuQuant palette plus a lazily filled 6-bit RGB lookup table. Building all
/// 262,144 entries up front costs ~50 ms because each one runs a nearest-colour
/// search, while a screen recording visits only a small fraction of the RGB
/// cube: resolve a bin the first time a frame actually contains it, then every
/// later pixel is one array read.
struct PaletteMap {
    quant: NeuQuant,
    palette: Vec<u8>,
    lut: Vec<u8>,
    resolved: Vec<bool>,
}

impl PaletteMap {
    /// Train on a sparse spatial sample of one frame. Screen captures are fully
    /// opaque, so alpha is ignored: GIF only has a 1-bit transparency mask.
    fn train(frame: &RgbaImage, quality: u8) -> Self {
        let samples = quality_to_samples(quality);
        let rgba = spatial_sample(frame, samples);
        let quant = NeuQuant::new(1, 256, &rgba);
        let palette = quant.color_map_rgb();
        Self {
            quant,
            palette,
            lut: vec![0u8; GIF_LUT_SIZE],
            resolved: vec![false; GIF_LUT_SIZE],
        }
    }

    fn index(&mut self, pixel: &[u8]) -> u8 {
        let r = (pixel[0] as usize) >> GIF_LUT_SHIFT;
        let g = (pixel[1] as usize) >> GIF_LUT_SHIFT;
        let b = (pixel[2] as usize) >> GIF_LUT_SHIFT;
        let bin = (r << (2 * GIF_LUT_BITS)) | (g << GIF_LUT_BITS) | b;
        if !self.resolved[bin] {
            let step = 1 << GIF_LUT_SHIFT;
            let center = [
                (r * step + step / 2) as u8,
                (g * step + step / 2) as u8,
                (b * step + step / 2) as u8,
                255,
            ];
            self.lut[bin] = self.quant.index_of(&center) as u8;
            self.resolved[bin] = true;
        }
        self.lut[bin]
    }

    fn indices(&mut self, frame: &RgbaImage) -> Vec<u8> {
        frame
            .as_raw()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|pixel| self.index(pixel))
            .collect()
    }

    fn indices_rect(&mut self, frame: &RgbaImage, rect: DirtyRect) -> Vec<u8> {
        let width = frame.width() as usize;
        let raw = frame.as_raw();
        let mut out = Vec::with_capacity(rect.width as usize * rect.height as usize);
        for y in rect.y..rect.y + rect.height {
            let start = (y as usize * width + rect.x as usize) * 4;
            let (pixels, _) = raw[start..start + rect.width as usize * 4].as_chunks::<4>();
            for pixel in pixels {
                out.push(self.index(pixel));
            }
        }
        out
    }

    /// Average squared RGB error over a small sample. A scene change makes the
    /// old palette a poor fit long before it becomes visible as banding.
    fn error(&mut self, frame: &RgbaImage) -> u64 {
        let rgba = spatial_sample(frame, GIF_ERROR_SAMPLES);
        let mut total = 0u64;
        for pixel in rgba.as_chunks::<4>().0 {
            let at = self.index(pixel) as usize * 3;
            let color = &self.palette[at..at + 3];
            for (value, mapped) in pixel.iter().take(3).zip(color) {
                let delta = *value as i32 - *mapped as i32;
                total += (delta * delta) as u64;
            }
        }
        total / (rgba.len() / 4).max(1) as u64
    }
}

/// Rectangle of a frame that must be redrawn. GIF's `Keep` disposal keeps every
/// pixel outside it from the previous frame, so LZW only has to encode this.
#[derive(Clone, Copy)]
struct DirtyRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl DirtyRect {
    fn area(&self) -> u64 {
        self.width as u64 * self.height as u64
    }
}

/// Bounding box of the pixels that changed since the last written frame. One
/// linear pass with no allocation; a static screen costs a comparison, not an
/// encode.
fn dirty_rect(previous: &RgbaImage, current: &RgbaImage) -> Option<DirtyRect> {
    let (width, height) = current.dimensions();
    let (w, h) = (width as usize, height as usize);
    let (before, _) = previous.as_raw().as_chunks::<4>();
    let (after, _) = current.as_raw().as_chunks::<4>();
    if before.len() != after.len() {
        return Some(DirtyRect {
            x: 0,
            y: 0,
            width,
            height,
        });
    }
    let (mut min_x, mut min_y) = (w, h);
    let (mut max_x, mut max_y) = (0usize, 0usize);
    let mut changed = false;
    for y in 0..h {
        let (row_before, row_after) = (&before[y * w..(y + 1) * w], &after[y * w..(y + 1) * w]);
        for (x, (a, b)) in row_before.iter().zip(row_after).enumerate() {
            if a != b {
                changed = true;
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
            }
        }
    }
    if !changed {
        return None;
    }
    Some(DirtyRect {
        x: min_x as u32,
        y: min_y as u32,
        width: (max_x - min_x + 1) as u32,
        height: (max_y - min_y + 1) as u32,
    })
}

/// Pick at most `target` RGBA pixels on a square grid so the sample covers the
/// whole frame instead of just its first rows.
fn spatial_sample(frame: &RgbaImage, target: usize) -> Vec<u8> {
    let (width, height) = frame.dimensions();
    let pixels = width as usize * height as usize;
    let step = ((pixels as f64 / target.max(1) as f64).sqrt().ceil() as usize).max(1);
    let mut sample = Vec::with_capacity(target.min(pixels).max(1) * 4);
    let raw = frame.as_raw();
    for y in (0..height as usize).step_by(step) {
        for x in (0..width as usize).step_by(step) {
            let at = (y * width as usize + x) * 4;
            sample.extend_from_slice(&raw[at..at + 4]);
        }
    }
    sample
}

/// Quantize and append frames to the file until the channel closes.
fn encode_gif(
    receiver: Receiver<PendingFrame>,
    file: File,
    quality: u8,
    max_width: Option<u32>,
) -> Result<()> {
    let mut writer = BufWriter::new(file);
    let (mut t_prepare, mut t_diff, mut t_error, mut t_train, mut t_write) = (
        Duration::ZERO,
        Duration::ZERO,
        Duration::ZERO,
        Duration::ZERO,
        Duration::ZERO,
    );
    let mut retrains = 0usize;
    let mut encoded = 0usize;
    // The encoder's screen size is fixed at creation, so anchor it on the
    // first frame that is encodable. A stream that yields none leaves the
    // file empty rather than corrupt.
    let anchored = loop {
        match receiver.recv() {
            Ok(pending) => {
                let scratch = Instant::now();
                let scaled = video::prepare(&pending.image, max_width).into_owned();
                t_prepare += scratch.elapsed();
                if let Some((width, height)) = fit_u16(&scaled) {
                    break Some((pending, scaled, width, height));
                }
            }
            Err(_) => break None,
        }
    };
    let Some((first, first_scaled, width, height)) = anchored else {
        writer.flush().context("failed to flush the GIF file")?;
        return Ok(());
    };
    let mut encoder = gif::Encoder::new(&mut writer, width, height, &[])
        .context("failed to create GIF encoder")?;
    encoder
        .write_extension(gif::ExtensionData::Repetitions(Repeat::Infinite))
        .context("failed to set GIF looping")?;
    let mut palette = {
        let scratch = Instant::now();
        let palette = PaletteMap::train(&first_scaled, quality);
        t_train += scratch.elapsed();
        palette
    };
    let mut since_train = 0usize;
    {
        let scratch = Instant::now();
        write_indexed_frame(
            &mut encoder,
            &first,
            &first_scaled,
            width,
            height,
            &mut palette,
            None,
        )?;
        t_write += scratch.elapsed();
        encoded += 1;
    }
    let mut previous = first_scaled;
    let full_area = width as u64 * height as u64;
    // Frames whose size drifted (a display change) do not fit the fixed screen
    // and are skipped rather than corrupting the container.
    for pending in receiver.iter() {
        let scratch = Instant::now();
        let scaled = video::prepare(&pending.image, max_width).into_owned();
        t_prepare += scratch.elapsed();
        let Some((frame_width, frame_height)) = fit_u16(&scaled) else {
            continue;
        };
        if (frame_width, frame_height) != (width, height) {
            continue;
        }
        let scratch = Instant::now();
        let retrain =
            since_train >= GIF_REFRESH_COOLDOWN && palette.error(&scaled) > GIF_REFRESH_ERROR;
        t_error += scratch.elapsed();
        if retrain {
            let scratch = Instant::now();
            palette = PaletteMap::train(&scaled, quality);
            t_train += scratch.elapsed();
            retrains += 1;
            since_train = 0;
        } else {
            since_train += 1;
        }
        let region = if retrain {
            // A new palette changes what every index means, so pixels outside
            // the changed rectangle can no longer be left as they were.
            None
        } else {
            // Even a completely static frame needs a frame of its own to keep
            // its delay and the frame count; one pixel is enough with Keep.
            let scratch = Instant::now();
            let changed = dirty_rect(&previous, &scaled).unwrap_or(DirtyRect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            });
            t_diff += scratch.elapsed();
            (changed.area() * 100 < full_area * GIF_PARTIAL_MAX_PERCENT).then_some(changed)
        };
        let scratch = Instant::now();
        write_indexed_frame(
            &mut encoder,
            &pending,
            &scaled,
            width,
            height,
            &mut palette,
            region,
        )?;
        t_write += scratch.elapsed();
        encoded += 1;
        previous = scaled;
    }
    drop(encoder);
    writer.flush().context("failed to flush the GIF file")?;
    log::info!(
        target: "timing",
        "gif writer: {encoded} frames, prepare {:.1} ms, diff {:.1} ms, palette error {:.1} ms, train {:.1} ms ({retrains} refreshes), encode {:.1} ms",
        ms(t_prepare),
        ms(t_diff),
        ms(t_error),
        ms(t_train),
        ms(t_write)
    );
    Ok(())
}

/// Map a frame (or just its changed rectangle) through the shared palette and
/// append it to the container. Every frame carries its own local palette so a
/// decoder can reconstruct it independently.
fn write_indexed_frame(
    encoder: &mut gif::Encoder<&mut BufWriter<File>>,
    pending: &PendingFrame,
    scaled: &RgbaImage,
    width: u16,
    height: u16,
    map: &mut PaletteMap,
    region: Option<DirtyRect>,
) -> Result<()> {
    let (left, top, frame_width, frame_height, indices) = match region {
        Some(rect) => (
            rect.x as u16,
            rect.y as u16,
            rect.width as u16,
            rect.height as u16,
            map.indices_rect(scaled, rect),
        ),
        None => (0, 0, width, height, map.indices(scaled)),
    };
    let mut frame = gif::Frame::from_palette_pixels(
        frame_width,
        frame_height,
        indices,
        map.palette.clone(),
        None,
    );
    frame.left = left;
    frame.top = top;
    frame.dispose = gif::DisposalMethod::Keep;
    frame.delay = pending.ticks;
    encoder
        .write_frame(&frame)
        .context("failed to encode GIF frame")
}

/// GIF dimensions are `u16`, so frames larger than 65535 pixels are skipped.
fn fit_u16(image: &RgbaImage) -> Option<(u16, u16)> {
    let (width, height) = image.dimensions();
    (width <= u16::MAX as u32 && height <= u16::MAX as u32).then_some((width as u16, height as u16))
}

/// GIF delays are `u16` ticks of 10 ms. Sub-tick durations round up so a frame
/// is never dropped, and never zero: browsers render a zero delay as fast as
/// the file can be parsed.
fn duration_to_ticks(duration: Duration) -> u16 {
    let ticks = (duration.as_secs_f64() * 100.).round();
    ticks.clamp(1., u16::MAX as f64) as u16
}

/// Map the 1-100 quality setting onto the number of pixels used to train the
/// palette. More samples give a better palette but cost a little more on the
/// (rare) retraining frames.
fn quality_to_samples(quality: u8) -> usize {
    1_024 + quality as usize * 160
}

/// A zero cap disables the automatic stop; otherwise the loop ends once the
/// elapsed time reaches it.
fn past_duration_limit(elapsed: Duration, max: Duration) -> bool {
    !max.is_zero() && elapsed >= max
}

fn open_sink(
    output: &Path,
    options: &RecordOptions,
    first_frame: &FrameData,
) -> Result<Box<dyn FrameSink>> {
    Ok(match options.format {
        RecordFormat::Gif => Box::new(GifSink::new(output, options)?),
        // The MP4 backend picks the container profile from the frames it is
        // handed: HEVC Main10 for extended range, H.264 otherwise.
        RecordFormat::Mp4 => video::open(output, options, first_frame)?,
    })
}

fn signal_ready(
    ready: &mut Option<SyncSender<std::result::Result<(), String>>>,
    result: std::result::Result<(), String>,
) {
    if let Some(ready) = ready.take() {
        let _ = ready.send(result);
    }
}

struct CaptureJob {
    monitor_id: u32,
    region: Region,
    output: PathBuf,
    options: RecordOptions,
    stop: Arc<AtomicBool>,
    paused: Arc<PauseClock>,
    frames: Arc<AtomicUsize>,
}

fn capture_loop(
    job: CaptureJob,
    mut ready: Option<SyncSender<std::result::Result<(), String>>>,
) -> Result<(Duration, Option<RgbaImage>)> {
    let CaptureJob {
        monitor_id,
        region,
        output,
        options,
        stop,
        paused,
        frames,
    } = job;
    let started = Instant::now();
    // The compositor hint targets the selection's logical size, i.e. exactly
    // the width the app already passes as `max_width`. It is disabled by
    // `SNAPIT_CPU_DOWNSCALE=1` so one build can A/B the CPU box mean, and
    // skipped when a caller asks for an even narrower output (then
    // `video::prepare` should do the whole downscale itself). Nothing relies
    // on the hint: `video::prepare` still downscales native frames, and DXGI
    // ignores it entirely.
    let logical_width = region.width.round().max(1.0) as u32;
    let logical_height = region.height.round().max(1.0) as u32;
    let output_size = if std::env::var_os("SNAPIT_CPU_DOWNSCALE").is_some()
        || options.max_width.is_some_and(|limit| limit < logical_width)
    {
        OutputSize::Native
    } else {
        OutputSize::Logical
    };
    // Extended range only pays for itself where the container can store it:
    // a GIF has no HDR, so asking the compositor for half-float frames would
    // only add tonemapping work on the capture thread.
    let dynamic_range = if options.hdr && matches!(options.format, RecordFormat::Mp4) {
        DynamicRange::Hdr
    } else {
        DynamicRange::Sdr
    };
    log::info!(
        "recorder dynamic_range={dynamic_range:?} (hdr_option={} format={:?})",
        options.hdr,
        options.format
    );
    let live = match LiveRecorder::start_with(
        monitor_id,
        region,
        options.fps,
        options.captures_cursor,
        output_size,
        dynamic_range,
    ) {
        Ok(live) => live,
        Err(err) => {
            signal_ready(&mut ready, Err(format!("{err:#}")));
            return Err(err);
        }
    };
    // The encoder's size is fixed when it is created, so it is opened on the
    // first frame the platform recorder actually delivers.
    let mut sink: Option<Box<dyn FrameSink>> = None;
    let mut origin: Option<Duration> = None;
    let mut first: Option<RgbaImage> = None;
    let mut previous: Option<FrameData> = None;
    let mut pending_duplicate_at: Option<Duration> = None;
    // Presentation times must strictly increase: AVFoundation rejects two
    // samples sharing a time, and a resume can land a millisecond before the
    // pause clock has accumulated the whole stretch.
    let mut last_at = Duration::ZERO;
    let mut pushed = 0usize;

    let mut push = |sink: &mut Box<dyn FrameSink>,
                    frames: &Arc<AtomicUsize>,
                    image: FrameData,
                    at: Duration|
     -> Result<()> {
        sink.push(image, at)?;
        pushed += 1;
        frames.store(pushed, Ordering::Relaxed);
        Ok(())
    };

    while !stop.load(Ordering::Relaxed)
        && !past_duration_limit(
            started.elapsed().saturating_sub(paused.paused_duration()),
            options.max_duration,
        )
    {
        // Bounds how long a stop request waits for this thread to notice it
        // without busy-waiting: 4 ms costs ~0.1% of a core, 0 ms pegs one.
        match live.recv_timeout(Duration::from_millis(4)) {
            LiveFrameWait::Frame(frame) => {
                if paused.is_paused() {
                    // Keep draining the platform recorder, but do not encode
                    // these frames or let them advance the timeline.
                    continue;
                }
                let frame_origin = *origin.get_or_insert(frame.timestamp);
                let at = frame
                    .timestamp
                    .saturating_sub(frame_origin)
                    .saturating_sub(paused.paused_duration());
                let at = if at > last_at {
                    at
                } else {
                    last_at + Duration::from_millis(1)
                };
                last_at = at;
                let image = FrameData::from(frame.data);
                if sink.is_none() {
                    // A/B evidence: the delivered size next to the selection's
                    // logical size shows whether the compositor honoured the
                    // hint. A mismatch only costs performance — `prepare`
                    // still scales native frames before they reach the sink.
                    let (width, height) = image.dimensions();
                    let range = match &image {
                        FrameData::Hdr(_) => "hdr",
                        FrameData::Sdr(_) => "sdr",
                    };
                    log::info!(
                        "first capture frame {width}x{height} ({range}, output size hint \
                         {output_size:?}, logical selection {logical_width}x{logical_height})"
                    );
                    sink = Some(open_sink(&output, &options, &image)?);
                }
                let sink = sink.as_mut().expect("sink was just created");
                let is_duplicate = previous
                    .as_ref()
                    .is_some_and(|prev| prev.is_same_as(&image));
                if is_duplicate {
                    pending_duplicate_at = Some(at);
                } else {
                    push(sink, &frames, image.clone(), at)?;
                    pending_duplicate_at = None;
                    if first.is_none() {
                        first = Some(image.to_sdr());
                    }
                    signal_ready(&mut ready, Ok(()));
                    previous = Some(image);
                }
            }
            LiveFrameWait::Timeout => {}
            LiveFrameWait::Disconnected => {
                signal_ready(
                    &mut ready,
                    Err("the platform recorder stopped before delivering a frame".into()),
                );
                break;
            }
        }
    }

    let closing = Instant::now();
    if sink.is_none() {
        // Windows duplication only reports changes, so a static screen can
        // yield no frame at all. Capture one now, only on this rare path: doing
        // it at `start` cost ~10 ms on every recording for no benefit. The
        // still screenshot is SDR, so this also decides an HDR session that
        // never delivered a frame as an ordinary H.264 file.
        let probe = FrameData::Sdr(Arc::new(
            RegionCapturer::new(monitor_id, region)?
                .capture()
                .context("failed to capture region")?,
        ));
        let mut created = open_sink(&output, &options, &probe)?;
        push(&mut created, &frames, probe.clone(), Duration::ZERO)?;
        first = Some(probe.to_sdr());
        sink = Some(created);
    } else if let (Some(image), Some(timestamp), Some(created)) =
        (&previous, pending_duplicate_at, sink.as_mut())
    {
        // Keep the final still stretch visible for its real duration.
        push(created, &frames, image.clone(), timestamp)?;
    }
    let last_frame = closing.elapsed();

    let duration = started
        .elapsed()
        .saturating_sub(origin.unwrap_or(Duration::ZERO))
        .saturating_sub(paused.paused_duration());
    // Platform stop and container finalisation are independent: the capture
    // loop has already pushed its last frame, so the sink only has to flush
    // what it holds, and stopping the stream talks to SCK/DXGI. Overlap them.
    // Only the sink crosses threads (`FrameSink: Send`); `LiveRecorder` holds
    // Objective-C objects and stays on this thread.
    let (stop_error, platform, finish, finish_ms) = thread::scope(move |scope| {
        let finisher = sink.map(|sink| {
            scope.spawn(move || {
                let begun = Instant::now();
                let result = sink.finish(duration);
                (result, begun.elapsed())
            })
        });
        let flushing = Instant::now();
        let stop_error = live.stop().err();
        let platform = flushing.elapsed();
        let mut finish = Ok(());
        let mut finish_ms = Duration::ZERO;
        if let Some(finisher) = finisher {
            match finisher.join() {
                Ok((result, elapsed)) => {
                    finish = result;
                    finish_ms = elapsed;
                }
                Err(_) => finish = Err(anyhow!("sink finish thread panicked")),
            }
        }
        (stop_error, platform, finish, finish_ms)
    });
    if let Some(err) = stop_error {
        log::warn!("failed to stop the platform recorder: {err:#}");
    }
    finish?;
    log::info!(
        target: "timing",
        "recording flush: platform stop {:.1} ms (parallel with sink finish), final frame {:.1} ms, sink finish {:.1} ms (duration {:.1} s)",
        ms(platform),
        ms(last_frame),
        ms(finish_ms),
        duration.as_secs_f64()
    );
    Ok((duration, first))
}

#[cfg(test)]
mod tests {

    use super::*;
    use std::sync::mpsc;

    /// A frame as the capture loop hands it to a sink.
    fn sdr(image: RgbaImage) -> FrameData {
        FrameData::Sdr(Arc::new(image))
    }

    #[test]
    fn zero_duration_never_expires() {
        assert_eq!(RecordOptions::default().max_duration, Duration::ZERO);
        assert!(!past_duration_limit(Duration::ZERO, Duration::ZERO));
        assert!(!past_duration_limit(
            Duration::from_secs(3600),
            Duration::ZERO
        ));
        assert!(!past_duration_limit(
            Duration::from_secs(29),
            Duration::from_secs(30)
        ));
        assert!(past_duration_limit(
            Duration::from_secs(30),
            Duration::from_secs(30)
        ));
    }

    #[test]
    fn pause_clock_freezes_time_until_resumed() {
        let clock = PauseClock::default();
        assert!(!clock.is_paused());
        assert!(clock.paused_duration().is_zero());
        clock.set_paused(true);
        clock.set_paused(true); // idempotent
        std::thread::sleep(Duration::from_millis(20));
        let during = clock.paused_duration();
        assert!(during >= Duration::from_millis(10));
        clock.set_paused(false);
        clock.set_paused(false); // idempotent
        assert!(!clock.is_paused());
        let frozen = clock.paused_duration();
        assert!(frozen >= during);
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(clock.paused_duration(), frozen);
    }

    #[test]
    fn the_remembered_format_reads_back_from_config_text() {
        for format in [RecordFormat::Gif, RecordFormat::Mp4] {
            assert_eq!(RecordFormat::parse(format.extension()), Some(format));
        }
        assert_eq!(RecordFormat::parse(" MP4 "), Some(RecordFormat::Mp4));
        assert_eq!(RecordFormat::parse("webm"), None);
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "snapit-recorder-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn quality_maps_onto_palette_training_samples() {
        assert!(quality_to_samples(1) < quality_to_samples(50));
        assert!(quality_to_samples(50) < quality_to_samples(100));
        assert_eq!(quality_to_samples(90), 15_424);
    }

    #[test]
    fn the_palette_lut_maps_every_rgb_bin_to_a_valid_colour() {
        let mut frame = RgbaImage::from_pixel(64, 48, image::Rgba([10, 20, 30, 255]));
        for x in 0..frame.width() {
            for y in 0..frame.height() {
                frame.put_pixel(x, y, image::Rgba([x as u8 * 3, y as u8 * 4, 30, 255]));
            }
        }
        let mut map = PaletteMap::train(&frame, 90);
        assert_eq!(map.lut.len(), GIF_LUT_SIZE);
        assert_eq!(map.palette.len(), 256 * 3);
        assert!(
            map.lut
                .iter()
                .all(|index| (*index as usize) * 3 < map.palette.len())
        );
        assert_eq!(map.indices(&frame).len(), (64 * 48) as usize);
    }

    #[test]
    fn a_scene_change_raises_the_palette_error() {
        let dark = RgbaImage::from_pixel(96, 64, image::Rgba([12, 18, 24, 255]));
        let bright = RgbaImage::from_pixel(96, 64, image::Rgba([240, 200, 120, 255]));
        let mut map = PaletteMap::train(&dark, 90);
        assert!(map.error(&dark) < GIF_REFRESH_ERROR);
        assert!(map.error(&bright) > GIF_REFRESH_ERROR);
    }

    #[test]
    fn durations_become_whole_delay_ticks() {
        assert_eq!(duration_to_ticks(Duration::from_millis(0)), 1);
        assert_eq!(duration_to_ticks(Duration::from_millis(4)), 1);
        assert_eq!(duration_to_ticks(Duration::from_millis(83)), 8);
        // The 600 s duration cap still fits in u16 ticks.
        assert_eq!(duration_to_ticks(Duration::from_secs(600)), 60000);
        assert_eq!(duration_to_ticks(Duration::from_secs(700)), u16::MAX);
    }

    /// The sink must flush through its writer thread; hanging here is the same
    /// class of freeze users hit when SnapIt awaits `Recorder::stop`.
    #[test]
    fn encode_finishes_without_blocking_forever() {
        let dir = temp_dir("gif");
        let output = dir.join("out.gif");

        let options = RecordOptions::default()
            .with_quality(80)
            .with_max_width(Some(32));
        let mut sink = GifSink::new(&output, &options).unwrap();

        let mut frame = RgbaImage::from_pixel(64, 48, image::Rgba([10, 20, 30, 255]));
        for x in 0..frame.width() {
            for y in 0..frame.height() {
                frame.put_pixel(x, y, image::Rgba([(x % 32) as u8, (y % 48) as u8, 30, 255]));
            }
        }
        for i in 0..8u64 {
            sink.push(sdr(frame.clone()), Duration::from_secs_f64(i as f64 * 0.1))
                .unwrap();
        }

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = Box::new(sink).finish(Duration::from_secs_f64(0.8));
            let _ = tx.send(result);
        });

        let result = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("GIF encode blocked for more than 10s");
        result.expect("GIF encode failed");
        assert!(
            output.metadata().map(|m| m.len() > 0).unwrap_or(false),
            "expected a non-empty GIF at {}",
            output.display()
        );
        assert_eq!(decode_frame_count(&output), 8, "the GIF lost frames");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A scene change retrains the shared palette; every frame must still carry
    /// a readable local palette so decoders can reconstruct it.
    #[test]
    fn a_palette_refresh_keeps_the_gif_decodable() {
        let dir = temp_dir("gif-refresh");
        let output = dir.join("out.gif");
        let options = RecordOptions::default()
            .with_quality(80)
            .with_max_width(Some(32));
        let mut sink = GifSink::new(&output, &options).unwrap();
        for i in 0..24u64 {
            let color = if i < 10 { 12 } else { 240 };
            sink.push(
                sdr(RgbaImage::from_pixel(
                    64,
                    48,
                    image::Rgba([color, color, color, 255]),
                )),
                Duration::from_millis(i * 100),
            )
            .unwrap();
        }
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = Box::new(sink).finish(Duration::from_millis(2_400));
            let _ = tx.send(result);
        });
        rx.recv_timeout(Duration::from_secs(10))
            .expect("GIF encode blocked for more than 10s")
            .expect("GIF encode failed");
        assert_eq!(decode_frame_count(&output), 24, "the GIF lost frames");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dirty_rect_covers_only_the_changed_pixels() {
        let before = RgbaImage::from_pixel(32, 20, image::Rgba([5, 5, 5, 255]));
        assert!(dirty_rect(&before, &before).is_none());
        let mut after = before.clone();
        after.put_pixel(3, 4, image::Rgba([200, 0, 0, 255]));
        after.put_pixel(9, 12, image::Rgba([200, 0, 0, 255]));
        let rect = dirty_rect(&before, &after).expect("two pixels changed");
        assert_eq!((rect.x, rect.y, rect.width, rect.height), (3, 4, 7, 9));
    }

    /// Partial frames must compose back into the full screen: a decoder keeps
    /// untouched pixels from the previous frame, so the GIF has to stay correct
    /// after both a small change and a completely static frame.
    #[test]
    fn partial_frames_decode_to_the_full_screen() {
        let dir = temp_dir("gif-partial");
        let output = dir.join("out.gif");
        let options = RecordOptions::default().with_quality(80);
        let mut sink = GifSink::new(&output, &options).unwrap();
        let base = RgbaImage::from_pixel(64, 48, image::Rgba([10, 20, 30, 255]));
        let mut lit = base.clone();
        for y in 10..20 {
            for x in 20..30 {
                lit.put_pixel(x, y, image::Rgba([240, 100, 5, 255]));
            }
        }
        sink.push(sdr(base), Duration::ZERO).unwrap();
        sink.push(sdr(lit.clone()), Duration::from_millis(100))
            .unwrap();
        sink.push(sdr(lit), Duration::from_millis(200)).unwrap();
        Box::new(sink).finish(Duration::from_millis(300)).unwrap();

        let file = File::open(&output).unwrap();
        let mut decode = gif::DecodeOptions::new();
        decode.set_color_output(gif::ColorOutput::RGBA);
        let mut decoder = decode.read_info(file).unwrap();
        let mut canvas = RgbaImage::new(decoder.width() as u32, decoder.height() as u32);
        let mut frames = Vec::new();
        while let Some(frame) = decoder.read_next_frame().unwrap() {
            for y in 0..frame.height as usize {
                for x in 0..frame.width as usize {
                    let at = (y * frame.width as usize + x) * 4;
                    canvas.put_pixel(
                        frame.left as u32 + x as u32,
                        frame.top as u32 + y as u32,
                        image::Rgba([
                            frame.buffer[at],
                            frame.buffer[at + 1],
                            frame.buffer[at + 2],
                            frame.buffer[at + 3],
                        ]),
                    );
                }
            }
            frames.push(canvas.clone());
        }
        let brightness = |image: &RgbaImage, x: u32, y: u32| {
            let pixel = image.get_pixel(x, y);
            pixel[0] as u32 + pixel[1] as u32 + pixel[2] as u32
        };
        assert_eq!(frames.len(), 3, "a static frame must still be written");
        assert!(brightness(&frames[0], 25, 15) < 150);
        assert!(brightness(&frames[1], 25, 15) > 250, "the lit block");
        assert!(
            brightness(&frames[1], 5, 5) < 150,
            "pixels outside the rectangle stay from the previous frame"
        );
        assert_eq!(
            frames[1].as_raw(),
            frames[2].as_raw(),
            "a static frame must not change the picture"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn decode_frame_count(path: &Path) -> usize {
        let file = File::open(path).expect("cannot reopen the GIF");
        let mut options = gif::DecodeOptions::new();
        options.set_color_output(gif::ColorOutput::RGBA);
        let mut decoder = options.read_info(file).expect("the GIF cannot be decoded");
        let mut count = 0;
        while let Some(frame) = decoder.read_next_frame().expect("bad GIF frame") {
            assert_eq!(
                frame.buffer.len(),
                frame.width as usize * frame.height as usize * 4,
                "a frame decoded to the wrong pixel count"
            );
            count += 1;
        }
        count
    }

    /// `Recorder::stop` must return after the duration cap without needing a UI
    /// thread. Skipped when Screen Recording permission is missing.
    #[test]
    fn recorder_stop_after_cap_completes() {
        let dir = temp_dir("live");
        let output = dir.join("live.gif");
        let options = RecordOptions::default()
            .with_fps(8)
            .with_max_duration(Duration::from_millis(400))
            .with_max_width(Some(160));
        let summary = match record_to_end(&output, options) {
            Some(summary) => summary,
            None => {
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
        };
        assert!(summary.frames >= 1, "expected at least one frame");
        assert_eq!(summary.format, RecordFormat::Gif);
        assert!(
            summary.path.is_file(),
            "missing GIF at {}",
            summary.path.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A paused recording must not advance its clock, and cancel must not
    /// leave a container behind. Skipped when Screen Recording is missing.
    #[test]
    fn pause_freezes_the_clock_and_discard_leaves_no_file() {
        let dir = temp_dir("discard");
        let output = dir.join("cancelled.gif");
        let options = RecordOptions::default()
            .with_fps(8)
            .with_max_duration(Duration::from_millis(400))
            .with_max_width(Some(160));
        let Some(recorder) = (|| {
            let monitor = snapit_capture::monitors()
                .ok()?
                .into_iter()
                .find(|m| m.is_primary)?;
            Recorder::start(
                monitor.id,
                Region::new(0., 0., 160., 120.),
                options,
                &output,
            )
            .ok()
        })() else {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };

        recorder.set_paused(true);
        assert!(recorder.is_paused());
        let paused_at = recorder.elapsed();
        thread::sleep(Duration::from_millis(120));
        assert!(
            recorder.elapsed() <= paused_at + Duration::from_millis(30),
            "the effective recording clock kept running while paused"
        );
        recorder.discard().expect("discard failed");
        assert!(
            !output.exists(),
            "a cancelled recording left its file behind"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The MP4 path shares the capture loop with GIF, so this exercises the
    /// platform encoder and the container end to end.
    #[test]
    fn mp4_recording_writes_a_container() {
        let dir = temp_dir("mp4");
        let output = dir.join("live.mp4");
        let options = RecordOptions::default()
            .with_format(RecordFormat::Mp4)
            .with_fps(8)
            .with_max_duration(Duration::from_millis(400))
            .with_max_width(Some(160));
        let Some(summary) = record_to_end(&output, options) else {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        assert_eq!(summary.format, RecordFormat::Mp4);
        let bytes = std::fs::read(&summary.path).expect("the MP4 was not written");
        assert!(
            bytes.len() > 1_000,
            "suspiciously small: {} bytes",
            bytes.len()
        );
        assert!(
            bytes.get(4..8) == Some(b"ftyp"),
            "an MP4 starts with its file type box"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Start a capped recording and stop it once frames are flowing. Returns
    /// `None` when the machine cannot capture (no permission, no monitor).
    fn record_to_end(output: &Path, options: RecordOptions) -> Option<RecordingSummary> {
        let monitors = snapit_capture::monitors().ok()?;
        let monitor = monitors.into_iter().find(|m| m.is_primary)?;
        let recorder =
            Recorder::start(monitor.id, Region::new(0., 0., 160., 120.), options, output).ok()?;

        // The capture thread builds its monitor handle after spawning, so wait
        // for the first frame before stopping; otherwise it exits with none.
        let deadline = Instant::now() + Duration::from_secs(5);
        while recorder.frame_count() == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(recorder.stop());
        });
        let summary = rx
            .recv_timeout(Duration::from_secs(20))
            .expect("Recorder::stop blocked for more than 20s")
            .expect("Recorder::stop failed");
        Some(summary)
    }
}
