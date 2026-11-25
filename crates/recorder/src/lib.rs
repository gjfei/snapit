//! Recording of a screen region to GIF, APNG or MP4.
//!
//! Frames are captured on a dedicated thread and handed to a [`FrameSink`],
//! All three formats see the same capture loop, the same duplicate-frame
//! dropping and the same duration cap; only the encoder differs, so a recording
//! behaves identically whichever container is picked.
//!
//! GIF and APNG stream frames to a writer thread through a bounded queue, so
//! memory stays bounded regardless of recording length. GIF quantizes with the
//! `gif` crate; APNG writes lossless RGBA8 with the `png` crate. Consecutive
//! identical frames are dropped; their time is folded into the previous frame,
//! which for video becomes the previous sample's duration.
//!
//! Three things keep the GIF writer ahead of the capture clock: one NeuQuant
//! palette trained on a sparse spatial sample (never per-frame
//! `Frame::from_rgba_speed`), a lazily filled 7-bit RGB lookup table that makes
//! mapping one array read per pixel, and partial frames — with `Keep` disposal,
//! LZW only encodes the rectangle that changed. A palette refresh forces one
//! full frame, since every index changes meaning. Ordered dithering softens the
//! 256-colour banding without a per-pixel nearest-colour search.
//!
//! Frames are captured at the display's physical pixel resolution, matching
//! still screenshots. `video::prepare` only downscales when a caller explicitly
//! sets `max_width`; `image::imageops::resize(Triangle)` is never used.

use std::{
    borrow::Cow,
    fs::{File, OpenOptions},
    io::{BufWriter, Write as _},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow};
use color_quant::NeuQuant;
use gif::Repeat;
#[cfg(target_os = "windows")]
use snapit_capture::RegionCapturer;
use snapit_capture::{
    DynamicRange, HdrImage, LiveFrameData, LiveFrameWait, LiveRecorder, OutputSize, PixelRect,
    RasterLayer, Region, RgbaImage, WatermarkSpec,
};

mod apng;
mod pointer;
mod video;

pub use pointer::{CLICK_COLOR, CLICK_DURATION, CLICK_PALETTE};

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e3
}

/// Bits per channel in the GIF colour lookup table. 7 bits keeps the table at
/// 2 MiB and limits the lookup approximation to one level per channel.
const GIF_LUT_BITS: usize = 7;
const GIF_LUT_BINS: usize = 1 << GIF_LUT_BITS;
const GIF_LUT_SHIFT: usize = 8 - GIF_LUT_BITS;
const GIF_LUT_SIZE: usize = GIF_LUT_BINS * GIF_LUT_BINS * GIF_LUT_BINS;
/// A 4×4 ordered matrix adds ±4 levels before lookup, breaking up gradients
/// without the full-frame error diffusion and its extra buffers.
const GIF_DITHER: [[i32; 4]; 4] = [
    [-4, 4, -2, 2],
    [2, -2, 3, -3],
    [-3, 3, -4, 4],
    [4, -4, 1, -1],
];
/// Pixels sampled to decide whether the current palette still fits the scene.
const GIF_ERROR_SAMPLES: usize = 4_096;
/// Average squared RGB error that triggers a new palette.
const GIF_REFRESH_ERROR: u64 = 900;
/// A small but colourful object must not be hidden by a large neutral background.
const GIF_REFRESH_BAD_PIXEL_PERCENT: usize = 1;
const GIF_REFRESH_BAD_PIXEL_ERROR: u64 = 2_500;
/// Never retrain more often than this many frames, even on noisy content.
const GIF_REFRESH_COOLDOWN: usize = 12;
/// Covers the LUT bin center and the 4×4 dither offsets around a reserved color.
const GIF_RESERVED_COLOR_ERROR: u64 = 832;
/// Only encode a sub-rectangle when the changed area is below this share of the
/// frame; a nearly full redraw is cheaper as one full frame.
const GIF_PARTIAL_MAX_PERCENT: u64 = 70;
/// Queued animation frames share this budget. The writer still needs its own
/// scratch buffers, and capture keeps the previous/current frames separately.
const ANIMATION_QUEUE_BUDGET: usize = 128 * 1024 * 1024;
/// DXGI only reports desktop changes. A static desktop therefore gets one
/// bounded grace period before a still seeds the encoder and the recording.
#[cfg(target_os = "windows")]
const WINDOWS_FIRST_FRAME_GRACE: Duration = Duration::from_millis(250);
const FRAME_SAMPLE_GRID: u32 = 4;

/// Container the frames are written to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RecordFormat {
    #[default]
    Gif,
    Apng,
    Mp4,
}

impl RecordFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Gif => "gif",
            Self::Apng => "png",
            Self::Mp4 => "mp4",
        }
    }

    pub fn config_name(self) -> &'static str {
        match self {
            Self::Gif => "gif",
            Self::Apng => "apng",
            Self::Mp4 => "mp4",
        }
    }

    /// Config round-trip for the remembered format: `"gif"`, `"apng"` or `"mp4"`.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "gif" => Some(Self::Gif),
            "apng" | "png" => Some(Self::Apng),
            "mp4" => Some(Self::Mp4),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClickEffectGroup {
    Basic,
    Dynamic,
    Particles,
    Glow,
}

impl ClickEffectGroup {
    pub const ALL: [Self; 4] = [Self::Basic, Self::Dynamic, Self::Particles, Self::Glow];

    pub fn effects(self) -> &'static [ClickEffect] {
        match self {
            Self::Basic => &[
                ClickEffect::Ring,
                ClickEffect::Dot,
                ClickEffect::Outline,
                ClickEffect::Cross,
                ClickEffect::Diamond,
                ClickEffect::Corners,
            ],
            Self::Dynamic => &[
                ClickEffect::Burst,
                ClickEffect::Ripple,
                ClickEffect::Pulse,
                ClickEffect::Sparkle,
                ClickEffect::Orbit,
                ClickEffect::Radar,
            ],
            Self::Particles => &[
                ClickEffect::Firework,
                ClickEffect::Comet,
                ClickEffect::Confetti,
                ClickEffect::Stardust,
                ClickEffect::Rocket,
            ],
            Self::Glow => &[
                ClickEffect::Bloom,
                ClickEffect::Aurora,
                ClickEffect::Prism,
                ClickEffect::Nebula,
                ClickEffect::Rainbow,
            ],
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClickEffect {
    #[default]
    Ring,
    Dot,
    Outline,
    Cross,
    Burst,
    Ripple,
    Pulse,
    Sparkle,
    Firework,
    Comet,
    Confetti,
    Bloom,
    Aurora,
    Prism,
    Diamond,
    Corners,
    Orbit,
    Radar,
    Stardust,
    Rocket,
    Nebula,
    Rainbow,
}

impl ClickEffect {
    pub const ALL: [Self; 22] = [
        Self::Ring,
        Self::Dot,
        Self::Outline,
        Self::Cross,
        Self::Diamond,
        Self::Corners,
        Self::Burst,
        Self::Ripple,
        Self::Pulse,
        Self::Sparkle,
        Self::Orbit,
        Self::Radar,
        Self::Firework,
        Self::Comet,
        Self::Confetti,
        Self::Stardust,
        Self::Rocket,
        Self::Bloom,
        Self::Aurora,
        Self::Prism,
        Self::Nebula,
        Self::Rainbow,
    ];

    pub fn group(self) -> ClickEffectGroup {
        match self {
            Self::Ring
            | Self::Dot
            | Self::Outline
            | Self::Cross
            | Self::Diamond
            | Self::Corners => ClickEffectGroup::Basic,
            Self::Burst
            | Self::Ripple
            | Self::Pulse
            | Self::Sparkle
            | Self::Orbit
            | Self::Radar => ClickEffectGroup::Dynamic,
            Self::Firework | Self::Comet | Self::Confetti | Self::Stardust | Self::Rocket => {
                ClickEffectGroup::Particles
            }
            Self::Bloom | Self::Aurora | Self::Prism | Self::Nebula | Self::Rainbow => {
                ClickEffectGroup::Glow
            }
        }
    }

    pub fn config_name(self) -> &'static str {
        match self {
            Self::Ring => "ring",
            Self::Dot => "dot",
            Self::Outline => "outline",
            Self::Cross => "cross",
            Self::Burst => "burst",
            Self::Ripple => "ripple",
            Self::Pulse => "pulse",
            Self::Sparkle => "sparkle",
            Self::Firework => "firework",
            Self::Comet => "comet",
            Self::Confetti => "confetti",
            Self::Bloom => "bloom",
            Self::Aurora => "aurora",
            Self::Prism => "prism",
            Self::Diamond => "diamond",
            Self::Corners => "corners",
            Self::Orbit => "orbit",
            Self::Radar => "radar",
            Self::Stardust => "stardust",
            Self::Rocket => "rocket",
            Self::Nebula => "nebula",
            Self::Rainbow => "rainbow",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|effect| effect.config_name() == value.trim().to_ascii_lowercase())
    }
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct RecordOptions {
    pub fps: u32,
    pub max_duration: Duration,
    /// GIF quantization quality, 1-100.
    pub quality: u8,
    /// Downscale frames to at most this width. `None` preserves physical pixels.
    pub max_width: Option<u32>,
    pub format: RecordFormat,
    /// Include the pointer. ScreenCaptureKit supplies it on macOS; Windows
    /// composites the current system cursor into DXGI frames.
    pub captures_cursor: bool,
    /// Draw a click effect when a mouse button is pressed.
    pub click_effects: bool,
    pub click_effect: ClickEffect,
    /// Capture and encode extended-range frames. MP4 only, and best-effort: a
    /// display without HDR support keeps recording 8-bit SDR as before.
    pub hdr: bool,
    /// MP4 target bitrate in Kbps. None or zero uses the automatic formula.
    pub mp4_bitrate_kbps: Option<u32>,
    /// Frozen for the whole recording and rasterized after the first frame's
    /// physical dimensions are known.
    pub watermark: WatermarkSpec,
}

impl Default for RecordOptions {
    fn default() -> Self {
        Self {
            fps: 12,
            max_duration: Duration::ZERO,
            quality: 90,
            max_width: None,
            format: RecordFormat::default(),
            captures_cursor: true,
            click_effects: true,
            click_effect: ClickEffect::default(),
            hdr: false,
            mp4_bitrate_kbps: None,
            watermark: WatermarkSpec::default(),
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

    pub fn with_click_effects(mut self, click_effects: bool) -> Self {
        self.click_effects = click_effects;
        self
    }

    pub fn with_click_effect(mut self, click_effect: ClickEffect) -> Self {
        self.click_effect = click_effect;
        self
    }

    pub fn click_effects_enabled(&self) -> bool {
        self.click_effects && self.captures_cursor
    }

    pub fn with_hdr(mut self, hdr: bool) -> Self {
        self.hdr = hdr;
        self
    }

    pub fn with_mp4_bitrate_kbps(mut self, kbps: Option<u32>) -> Self {
        self.mp4_bitrate_kbps = kbps.filter(|value| *value > 0);
        self
    }

    pub fn with_watermark(mut self, watermark: WatermarkSpec) -> Self {
        self.watermark = watermark;
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

/// Recording time starts when the first sample is accepted (or the Windows
/// static-desktop still is encoded), never at backend or encoder creation.
/// Warm-up and sink setup therefore cannot consume the cap or make the UI clock
/// run ahead of the encoded timeline.
fn recording_elapsed(origin: Option<Instant>, now: Instant, paused: Duration) -> Duration {
    origin
        .map(|origin| now.saturating_duration_since(origin))
        .unwrap_or_default()
        .saturating_sub(paused)
}

type CaptureResult = Result<(Duration, Option<RgbaImage>)>;
type StartReady = std::result::Result<Instant, String>;

/// Cancels a recorder that is still waiting for its platform's first frame.
#[derive(Clone, Default)]
pub struct RecorderStart {
    cancelled: Arc<AtomicBool>,
}

impl RecorderStart {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

fn remove_cancelled_start(output: &Path) {
    match std::fs::remove_file(output) {
        Ok(()) => log::info!("removed output from cancelled recording start"),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => log::warn!("failed to remove cancelled recording output: {err}"),
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
    capture_thread: Option<JoinHandle<CaptureResult>>,
}

impl Recorder {
    /// Start recording `region` (monitor units) of `monitor_id` into `output`.
    pub fn start(
        monitor_id: u32,
        region: Region,
        options: RecordOptions,
        output: impl AsRef<Path>,
    ) -> Result<Self> {
        Self::start_with_cancel(
            monitor_id,
            region,
            options,
            output,
            RecorderStart::default(),
        )
    }

    pub fn start_with_cancel(
        monitor_id: u32,
        region: Region,
        options: RecordOptions,
        output: impl AsRef<Path>,
        start: RecorderStart,
    ) -> Result<Self> {
        if start.cancelled.load(Ordering::Relaxed) {
            anyhow::bail!("recording start cancelled");
        }
        let output = output.as_ref().to_path_buf();
        if output.exists() {
            anyhow::bail!("recording output already exists: {}", output.display());
        }
        let requested_at = Instant::now();
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
        let (ready_tx, ready_rx) = sync_channel::<StartReady>(1);
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

        let deadline = Instant::now() + Duration::from_secs(5);
        let media_origin = loop {
            if start.cancelled.load(Ordering::Relaxed) {
                stop.store(true, Ordering::Relaxed);
                let _ = capture_thread.join();
                remove_cancelled_start(&output);
                anyhow::bail!("recording start cancelled");
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                stop.store(true, Ordering::Relaxed);
                let _ = capture_thread.join();
                remove_cancelled_start(&output);
                anyhow::bail!("the platform recorder delivered no frame within 5 s");
            }
            match ready_rx.recv_timeout(remaining.min(Duration::from_millis(10))) {
                Ok(Ok(origin)) => {
                    if start.cancelled.load(Ordering::Relaxed) {
                        stop.store(true, Ordering::Relaxed);
                        let _ = capture_thread.join();
                        remove_cancelled_start(&output);
                        anyhow::bail!("recording start cancelled");
                    }
                    log::info!(
                        target: "timing",
                        "recorder start: ready after {:.1} ms (permission preflight only, no probe capture)",
                        ms(requested_at.elapsed())
                    );
                    break origin;
                }
                Ok(Err(message)) => {
                    stop.store(true, Ordering::Relaxed);
                    let _ = capture_thread.join();
                    anyhow::bail!("{message}");
                }
                Err(RecvTimeoutError::Disconnected) => {
                    stop.store(true, Ordering::Relaxed);
                    let result = capture_thread.join();
                    remove_cancelled_start(&output);
                    return match result {
                        Ok(Err(err)) => Err(err.context("recorder stopped before its first frame")),
                        _ => Err(anyhow!(
                            "the platform recorder stopped before its first frame"
                        )),
                    };
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        };

        Ok(Self {
            stop,
            paused,
            frames,
            started: media_origin,
            max_duration: options.max_duration,
            output,
            format: options.format,
            capture_thread: Some(capture_thread),
        })
    }

    /// Time actually recorded, excluding paused stretches. This is what the UI
    /// shows and what the duration cap compares against.
    pub fn elapsed(&self) -> Duration {
        recording_elapsed(
            Some(self.started),
            Instant::now(),
            self.paused.paused_duration(),
        )
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
        self.capture_thread
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
    }

    /// Stop capturing and wait for the file to be fully written. Blocking.
    pub fn stop(mut self) -> Result<RecordingSummary> {
        let requested = Instant::now();
        // Freeze the pause clock so stopping while paused reports the effective
        // duration instead of one that keeps shrinking while the sinks flush.
        self.paused.set_paused(false);
        self.stop.store(true, Ordering::Relaxed);
        let capture_thread = self
            .capture_thread
            .take()
            .expect("recorder capture thread is joined only once");
        let (duration, first_frame) = capture_thread
            .join()
            .map_err(|_| anyhow!("capture thread panicked"))??;
        log::info!(
            target: "timing",
            "recorder stop: capture thread joined after {:.1} ms",
            ms(requested.elapsed())
        );
        Ok(RecordingSummary {
            path: self.output.clone(),
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

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(capture_thread) = self.capture_thread.take()
            && let Err(err) = thread::Builder::new()
                .name("snapit-capture-cleanup".into())
                .spawn(move || {
                    let _ = capture_thread.join();
                })
        {
            log::warn!("failed to start capture cleanup thread: {err:#}");
        }
    }
}

/// Where a captured frame goes. Both sinks are driven from the capture thread
/// and finish on it, so `stop` only has one thread left to join.
///
/// Frames are handed over as `Arc` because the capture loop keeps the previous
/// one for duplicate detection while the GIF sink holds it until the next
/// timestamp arrives; sharing the allocation avoids a full-frame copy per frame.
pub(crate) trait FrameSink: Send {
    /// Encode one frame, shown from `at` until the next one. Returns whether the
    /// new visual state was accepted; a full animation queue may skip it.
    fn push(&mut self, frame: FrameData, at: Duration) -> Result<bool>;

    /// Flush the container. `end` is the recording's total duration, which is
    /// how long the last frame stays on screen.
    fn finish(self: Box<Self>, end: Duration) -> Result<()>;
}

/// The pixels of one frame: 8-bit SDR, or the extended-range samples an HDR
/// display delivered. The first frame chooses the MP4 encoder, but the capture
/// platform may change dynamic range during the session. HDR sinks promote SDR
/// frames through linear scRGB without changing absolute SDR white; SDR sinks
/// tone-map any later HDR frames.
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

    fn byte_len(&self) -> usize {
        match self {
            Self::Sdr(image) => image.as_raw().len(),
            Self::Hdr(image) => image.as_bytes().len(),
        }
    }

    fn apply_watermark(self, watermark: Option<&Arc<RasterLayer>>) -> Self {
        let Some(watermark) = watermark else {
            return self;
        };
        match self {
            Self::Sdr(image) => Self::Sdr(watermark.apply_sdr(image)),
            Self::Hdr(image) => Self::Hdr(watermark.apply_hdr(image)),
        }
    }

    /// Cheap rejection before the exact comparison. Equal frames necessarily
    /// have equal samples; a collision only costs the full byte comparison.
    fn likely_same_as(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Sdr(a), Self::Sdr(b)) => {
                Arc::ptr_eq(a, b)
                    || sampled_pixels_equal(
                        a.dimensions(),
                        a.as_raw(),
                        b.dimensions(),
                        b.as_raw(),
                        4,
                    )
            }
            (Self::Hdr(a), Self::Hdr(b)) => {
                Arc::ptr_eq(a, b)
                    || sampled_pixels_equal(
                        (a.width(), a.height()),
                        a.as_bytes(),
                        (b.width(), b.height()),
                        b.as_bytes(),
                        6,
                    )
            }
            _ => false,
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

fn sampled_pixels_equal(
    a_dimensions: (u32, u32),
    a: &[u8],
    b_dimensions: (u32, u32),
    b: &[u8],
    pixel_bytes: usize,
) -> bool {
    if a_dimensions != b_dimensions || a.len() != b.len() || a.is_empty() {
        return a == b;
    }
    let (width, height) = a_dimensions;
    (0..FRAME_SAMPLE_GRID).all(|row| {
        (0..FRAME_SAMPLE_GRID).all(|column| {
            let x = width * column / FRAME_SAMPLE_GRID;
            let y = height * row / FRAME_SAMPLE_GRID;
            let offset = ((y * width + x) as usize) * pixel_bytes;
            a[offset..offset + pixel_bytes] == b[offset..offset + pixel_bytes]
        })
    })
}

fn animation_queue_capacity(frame_bytes: usize) -> usize {
    (ANIMATION_QUEUE_BUDGET / frame_bytes.max(1)).clamp(1, 8)
}

fn validate_animation_frame_size(frame_bytes: usize) -> Result<()> {
    if frame_bytes > ANIMATION_QUEUE_BUDGET {
        anyhow::bail!(
            "first animation frame is {:.2} MiB, exceeding the {:.0} MiB queue budget; \
             recording was not downscaled",
            frame_bytes as f64 / (1024.0 * 1024.0),
            ANIMATION_QUEUE_BUDGET as f64 / (1024.0 * 1024.0)
        );
    }
    Ok(())
}

fn validate_animation_queue_frame(frame_bytes: usize, capacity: usize) -> Result<()> {
    let max_frame_bytes = ANIMATION_QUEUE_BUDGET / capacity.max(1);
    if frame_bytes > max_frame_bytes {
        anyhow::bail!(
            "animation frame is {:.2} MiB, exceeding the {:.2} MiB per-frame limit for a {capacity}-frame queue",
            frame_bytes as f64 / (1024.0 * 1024.0),
            max_frame_bytes as f64 / (1024.0 * 1024.0)
        );
    }
    Ok(())
}

/// A frame handed to the writer thread: the image plus how long it stays on
/// screen. GIF and APNG convert this to their respective delay units.
struct PendingFrame {
    image: Arc<RgbaImage>,
    duration: Duration,
}

/// Channel to the writer thread plus the frame shown last, which is queued only
/// once its duration is known (a frame ends when the next one starts).
struct AnimationSink {
    format: RecordFormat,
    sender: Option<SyncSender<PendingFrame>>,
    queue_capacity: usize,
    writer: Option<JoinHandle<Result<()>>>,
    pending: Option<Arc<RgbaImage>>,
    pending_at: Option<Duration>,
    dropped: usize,
}

impl AnimationSink {
    fn new(
        output: &Path,
        options: &RecordOptions,
        format: RecordFormat,
        first_frame_bytes: usize,
        watermark: Option<Arc<RasterLayer>>,
        source_dimensions: (u32, u32),
    ) -> Result<Self> {
        // Reject before creating the output so an impossible queue never leaves
        // an empty container behind. Native dimensions are never changed here.
        validate_animation_frame_size(first_frame_bytes)?;
        let capacity = animation_queue_capacity(first_frame_bytes);
        log::info!(
            "animation queue: capacity={capacity}, frame={:.2} MiB, queued budget={} MiB",
            first_frame_bytes as f64 / (1024.0 * 1024.0),
            ANIMATION_QUEUE_BUDGET / (1024 * 1024)
        );
        // Never truncate a previous recording if a stale path is reused.
        let (file, name) = match format {
            RecordFormat::Gif => (
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(output)
                    .with_context(|| format!("failed to create {}", output.display()))?,
                "snapit-gif-writer",
            ),
            RecordFormat::Apng => (apng::create(output)?, "snapit-apng-writer"),
            RecordFormat::Mp4 => unreachable!("MP4 uses the video sink"),
        };
        let max_width = options.max_width;
        let quality = options.quality;
        let click_effects = options.click_effects;
        let (sender, receiver) = sync_channel::<PendingFrame>(capacity);
        let writer = thread::Builder::new()
            .name(name.into())
            .spawn(move || match format {
                RecordFormat::Gif => encode_gif(
                    receiver,
                    file,
                    quality,
                    max_width,
                    click_effects,
                    watermark,
                    source_dimensions,
                ),
                RecordFormat::Apng => apng::encode(receiver, file, max_width),
                RecordFormat::Mp4 => unreachable!(),
            })?;
        Ok(Self {
            format,
            sender: Some(sender),
            queue_capacity: capacity,
            writer: Some(writer),
            pending: None,
            pending_at: None,
            dropped: 0,
        })
    }
}

impl FrameSink for AnimationSink {
    fn push(&mut self, frame: FrameData, at: Duration) -> Result<bool> {
        let frame = match frame {
            FrameData::Sdr(image) => image,
            // GIF and APNG have no extended range, so an HDR session
            // quantizes the tonemapped frames and loses only the highlights.
            FrameData::Hdr(hdr) => Arc::new(hdr.to_sdr()),
        };
        let sender = self
            .sender
            .as_ref()
            .ok_or_else(|| anyhow!("{:?} encoder already closed", self.format))?;
        validate_animation_queue_frame(frame.as_raw().len(), self.queue_capacity)?;
        if let (Some(image), Some(started)) = (self.pending.take(), self.pending_at) {
            let duration = at.saturating_sub(started).max(Duration::from_millis(1));
            match sender.try_send(PendingFrame { image, duration }) {
                Ok(()) => {}
                Err(TrySendError::Full(pending)) => {
                    self.dropped += 1;
                    self.pending = Some(pending.image);
                    self.pending_at = Some(started);
                    return Ok(false);
                }
                Err(TrySendError::Disconnected(_)) => {
                    anyhow::bail!(
                        "failed to queue frame: the {:?} writer stopped",
                        self.format
                    );
                }
            }
        }
        self.pending = Some(frame);
        self.pending_at = Some(at);
        Ok(true)
    }

    fn finish(mut self: Box<Self>, end: Duration) -> Result<()> {
        if let Some(sender) = self.sender.take()
            && let (Some(image), Some(started)) = (self.pending.take(), self.pending_at)
        {
            let duration = end.saturating_sub(started).max(Duration::from_millis(1));
            // Capture has already stopped, so flushing the final frame can wait
            // for queue space without delaying the stop signal.
            match sender.send(PendingFrame { image, duration }) {
                Ok(()) => {}
                Err(_) => {
                    anyhow::bail!("failed to flush the {:?}: the writer stopped", self.format)
                }
            }
        }
        let joined = Instant::now();
        if let Some(writer) = self.writer.take() {
            writer
                .join()
                .map_err(|_| anyhow!("{:?} writer thread panicked", self.format))??;
        }
        if self.dropped > 0 {
            log::warn!(
                "{:?} encoder fell behind; dropped {} frames",
                self.format,
                self.dropped
            );
        }
        log::info!(
            target: "timing",
            "{:?} sink: dropped {} queued frames, writer join {:.1} ms",
            self.format,
            self.dropped,
            ms(joined.elapsed())
        );
        Ok(())
    }
}

/// A NeuQuant palette plus a lazily filled 7-bit RGB lookup table. Building all
/// 2,097,152 entries up front is expensive because each one runs a nearest-colour
/// search, while a screen recording visits only a small fraction of the RGB
/// cube: resolve a bin the first time a frame actually contains it, then every
/// later pixel is one array read.
struct PaletteMap {
    quant: NeuQuant,
    palette: Vec<u8>,
    lut: Vec<u8>,
    resolved: Vec<bool>,
    reserved: Vec<u8>,
}

fn squared_rgb_distance(a: &[u8], b: &[u8]) -> u64 {
    a.iter()
        .zip(b)
        .take(3)
        .map(|(left, right)| {
            let delta = i32::from(*left) - i32::from(*right);
            (delta * delta) as u64
        })
        .sum()
}

impl PaletteMap {
    /// Train on a sparse spatial sample of one frame. Screen captures are fully
    /// opaque, so alpha is ignored: GIF only has a 1-bit transparency mask.
    #[cfg(test)]
    fn train(frame: &RgbaImage, quality: u8) -> Self {
        Self::train_with_regions(frame, quality, &[])
    }

    fn train_with_regions(frame: &RgbaImage, quality: u8, regions: &[PixelRect]) -> Self {
        let samples = quality_to_samples(quality);
        let mut rgba = spatial_sample(frame, samples);
        rgba.extend(spatial_sample_regions(frame, regions, samples));
        let quant = NeuQuant::new(1, 256, &rgba);
        let palette = quant.color_map_rgb();
        Self {
            quant,
            palette,
            lut: vec![0u8; GIF_LUT_SIZE],
            resolved: vec![false; GIF_LUT_SIZE],
            reserved: Vec::new(),
        }
    }

    fn reserve_color(&mut self, color: [u8; 3]) {
        if self.reserved.iter().any(|index| {
            let at = *index as usize * 3;
            self.palette[at..at + 3] == color
        }) {
            return;
        }
        let (index, _) = self
            .palette
            .as_chunks::<3>()
            .0
            .iter()
            .enumerate()
            .filter(|(index, _)| !self.reserved.contains(&(*index as u8)))
            .min_by_key(|(_, entry)| squared_rgb_distance(entry.as_slice(), &color))
            .expect("NeuQuant always returns 256 colors");
        self.palette[index * 3..index * 3 + 3].copy_from_slice(&color);
        self.reserved.push(index as u8);
        self.resolved.fill(false);
    }

    fn index(&mut self, pixel: &[u8]) -> u8 {
        self.index_rgb(pixel[0], pixel[1], pixel[2])
    }

    fn index_at(&mut self, pixel: &[u8], x: u32, y: u32) -> u8 {
        let offset = GIF_DITHER[y as usize % 4][x as usize % 4];
        self.index_rgb(
            (i32::from(pixel[0]) + offset).clamp(0, 255) as u8,
            (i32::from(pixel[1]) + offset).clamp(0, 255) as u8,
            (i32::from(pixel[2]) + offset).clamp(0, 255) as u8,
        )
    }

    fn index_rgb(&mut self, red: u8, green: u8, blue: u8) -> u8 {
        let r = (red as usize) >> GIF_LUT_SHIFT;
        let g = (green as usize) >> GIF_LUT_SHIFT;
        let b = (blue as usize) >> GIF_LUT_SHIFT;
        let bin = (r << (2 * GIF_LUT_BITS)) | (g << GIF_LUT_BITS) | b;
        if !self.resolved[bin] {
            let step = 1 << GIF_LUT_SHIFT;
            let center = [
                (r * step + step / 2) as u8,
                (g * step + step / 2) as u8,
                (b * step + step / 2) as u8,
                255,
            ];
            let reserved = self
                .reserved
                .iter()
                .copied()
                .map(|index| {
                    let at = index as usize * 3;
                    (
                        index,
                        squared_rgb_distance(&center, &self.palette[at..at + 3]),
                    )
                })
                .filter(|(_, error)| *error <= GIF_RESERVED_COLOR_ERROR)
                .min_by_key(|(_, error)| *error)
                .map(|(index, _)| index);
            self.lut[bin] = reserved.unwrap_or_else(|| self.quant.index_of(&center) as u8);
            self.resolved[bin] = true;
        }
        self.lut[bin]
    }

    fn indices(&mut self, frame: &RgbaImage) -> Vec<u8> {
        let width = frame.width();
        frame
            .as_raw()
            .as_chunks::<4>()
            .0
            .iter()
            .enumerate()
            .map(|(at, pixel)| self.index_at(pixel, at as u32 % width, at as u32 / width))
            .collect()
    }

    fn indices_rect(&mut self, frame: &RgbaImage, rect: DirtyRect) -> Vec<u8> {
        let width = frame.width() as usize;
        let raw = frame.as_raw();
        let mut out = Vec::with_capacity(rect.width as usize * rect.height as usize);
        for y in rect.y..rect.y + rect.height {
            let start = (y as usize * width + rect.x as usize) * 4;
            let (pixels, _) = raw[start..start + rect.width as usize * 4].as_chunks::<4>();
            for (column, pixel) in pixels.iter().enumerate() {
                out.push(self.index_at(pixel, rect.x + column as u32, y));
            }
        }
        out
    }

    /// Average squared RGB error over a small sample. A scene change makes the
    /// old palette a poor fit long before it becomes visible as banding. A small
    /// share of very bad pixels also triggers a refresh so a colourful object is
    /// not hidden by a large neutral background.
    fn needs_refresh(&mut self, frame: &RgbaImage) -> bool {
        let rgba = spatial_sample(frame, GIF_ERROR_SAMPLES);
        let mut total = 0u64;
        let mut bad = 0usize;
        for pixel in rgba.as_chunks::<4>().0 {
            let at = self.index(pixel) as usize * 3;
            let color = &self.palette[at..at + 3];
            let mut squared = 0u64;
            for (value, mapped) in pixel.iter().take(3).zip(color) {
                let delta = *value as i32 - *mapped as i32;
                squared += (delta * delta) as u64;
            }
            total += squared;
            if squared > GIF_REFRESH_BAD_PIXEL_ERROR {
                bad += 1;
            }
        }
        let pixels = (rgba.len() / 4).max(1);
        total / pixels as u64 > GIF_REFRESH_ERROR
            || bad * 100 >= pixels * GIF_REFRESH_BAD_PIXEL_PERCENT
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
    if previous.dimensions() != (width, height) {
        return Some(DirtyRect {
            x: 0,
            y: 0,
            width,
            height,
        });
    }
    let row_bytes = width as usize * 4;
    let before = previous.as_raw();
    let after = current.as_raw();
    let row_changed = |y: usize| {
        let start = y * row_bytes;
        before[start..start + row_bytes] != after[start..start + row_bytes]
    };
    let first_y = (0..height as usize).find(|&y| row_changed(y))?;
    let last_y = (first_y + 1..height as usize)
        .rev()
        .find(|&y| row_changed(y))
        .unwrap_or(first_y);
    let mut min_x = usize::MAX;
    let mut max_x = 0;
    for y in first_y..=last_y {
        let start = y * row_bytes;
        let Some((min, max)) = changed_span(
            &before[start..start + row_bytes],
            &after[start..start + row_bytes],
        ) else {
            continue;
        };
        min_x = min_x.min(min);
        max_x = max_x.max(max);
    }
    Some(DirtyRect {
        x: min_x as u32,
        y: first_y as u32,
        width: (max_x - min_x + 1) as u32,
        height: (last_y - first_y + 1) as u32,
    })
}

fn changed_span(before: &[u8], after: &[u8]) -> Option<(usize, usize)> {
    let pixels = before.len() / 4;
    let different = |x: usize| before[x * 4..x * 4 + 4] != after[x * 4..x * 4 + 4];
    let min = (0..pixels).find(|&x| different(x))?;
    let max = (min + 1..pixels)
        .rev()
        .find(|&x| different(x))
        .unwrap_or(min);
    Some((min, max))
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

fn spatial_sample_regions(frame: &RgbaImage, regions: &[PixelRect], target: usize) -> Vec<u8> {
    if regions.is_empty() {
        return Vec::new();
    }
    let (width, height) = frame.dimensions();
    let raw = frame.as_raw();
    let per_tile = target.max(1).div_ceil(regions.len());
    let mut sample = Vec::with_capacity(target.clamp(1, 65_536) * 4);
    for region in regions {
        let right = region.right().min(width);
        let bottom = region.bottom().min(height);
        if region.x >= right || region.y >= bottom {
            continue;
        }
        let area = (right - region.x) as usize * (bottom - region.y) as usize;
        let step = (area as f64 / per_tile as f64).sqrt().ceil() as usize;
        let step = step.max(1);
        let mut taken = 0;
        for y in (region.y..bottom).step_by(step) {
            for x in (region.x..right).step_by(step) {
                let at = (y as usize * width as usize + x as usize) * 4;
                sample.extend_from_slice(&raw[at..at + 4]);
                taken += 1;
                if taken == per_tile {
                    break;
                }
            }
            if taken == per_tile {
                break;
            }
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
    click_effects: bool,
    watermark: Option<Arc<RasterLayer>>,
    source_dimensions: (u32, u32),
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
                let scaled = video::prepare(&pending.image, max_width);
                t_prepare += scratch.elapsed();
                if let Some((width, height)) = fit_u16(&scaled) {
                    let first_scaled = match scaled {
                        Cow::Borrowed(_) => pending.image.clone(),
                        Cow::Owned(image) => Arc::new(image),
                    };
                    break Some((pending, first_scaled, width, height));
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
        let regions = watermark
            .as_ref()
            .map(|layer| {
                layer.scaled_sample_regions(
                    source_dimensions.0,
                    source_dimensions.1,
                    first_scaled.width(),
                    first_scaled.height(),
                )
            })
            .unwrap_or_default();
        let palette = PaletteMap::train_with_regions(&first_scaled, quality, &regions);
        t_train += scratch.elapsed();
        palette
    };
    if click_effects {
        for color in pointer::CLICK_PALETTE {
            palette.reserve_color(color);
        }
    }
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
        let scaled = video::prepare(&pending.image, max_width);
        t_prepare += scratch.elapsed();
        let Some((frame_width, frame_height)) = fit_u16(&scaled) else {
            continue;
        };
        if (frame_width, frame_height) != (width, height) {
            continue;
        }
        let scratch = Instant::now();
        let retrain = since_train >= GIF_REFRESH_COOLDOWN && palette.needs_refresh(&scaled);
        t_error += scratch.elapsed();
        if retrain {
            let scratch = Instant::now();
            let regions = watermark
                .as_ref()
                .map(|layer| {
                    layer.scaled_sample_regions(
                        source_dimensions.0,
                        source_dimensions.1,
                        scaled.width(),
                        scaled.height(),
                    )
                })
                .unwrap_or_default();
            palette = PaletteMap::train_with_regions(&scaled, quality, &regions);
            t_train += scratch.elapsed();
            if click_effects {
                for color in pointer::CLICK_PALETTE {
                    palette.reserve_color(color);
                }
            }
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
        previous = match scaled {
            Cow::Borrowed(_) => pending.image,
            Cow::Owned(image) => Arc::new(image),
        };
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
    frame.delay = duration_to_ticks(pending.duration);
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
    4_096 + quality as usize * 512
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
    watermark: Option<&Arc<RasterLayer>>,
) -> Result<Box<dyn FrameSink>> {
    Ok(match options.format {
        RecordFormat::Gif | RecordFormat::Apng => Box::new(AnimationSink::new(
            output,
            options,
            options.format,
            first_frame.byte_len(),
            watermark.cloned(),
            first_frame.dimensions(),
        )?),
        // The MP4 backend picks the container profile from the frames it is
        // handed: HEVC Main10 for extended range, H.264 otherwise.
        RecordFormat::Mp4 => video::open(output, options, first_frame)?,
    })
}

fn open_first_sink(
    output: &Path,
    options: &RecordOptions,
    first_frame: &FrameData,
    at: Duration,
    watermark: Option<&Arc<RasterLayer>>,
) -> Result<(Box<dyn FrameSink>, RgbaImage)> {
    let first_frame = first_frame.clone().apply_watermark(watermark);
    let thumbnail = first_frame.to_sdr();
    let mut sink = open_sink(output, options, &first_frame, watermark)?;
    if let Err(err) = sink.push(first_frame.clone(), at) {
        if matches!(options.format, RecordFormat::Mp4) && matches!(first_frame, FrameData::Hdr(_)) {
            log::warn!("HEVC Main10 rejected the first frame, recording SDR: {err:#}");
            drop(sink);
            if let Err(remove_err) = std::fs::remove_file(output)
                && remove_err.kind() != std::io::ErrorKind::NotFound
            {
                log::warn!("failed to remove the rejected HDR container: {remove_err}");
            }
            let sdr = FrameData::Sdr(Arc::new(thumbnail.clone()));
            let mut sink = open_sink(output, options, &sdr, watermark)?;
            sink.push(sdr, at)?;
            return Ok((sink, thumbnail));
        }
        return Err(err);
    }
    Ok((sink, thumbnail))
}

fn signal_ready(ready: &mut Option<SyncSender<StartReady>>, result: StartReady) {
    if let Some(ready) = ready.take() {
        let _ = ready.send(result);
    }
}

struct StopOnDrop(Option<LiveRecorder>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if let Some(live) = self.0.take()
            && let Err(err) = live.stop()
        {
            log::warn!("failed to stop the platform recorder during cleanup: {err:#}");
        }
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

#[derive(Default)]
struct CaptureStageStats {
    frames: usize,
    duplicates: usize,
    pointer: Duration,
    pointer_max: Duration,
    duplicate: Duration,
    duplicate_max: Duration,
    sink: Duration,
    sink_max: Duration,
}

impl CaptureStageStats {
    fn record(
        &mut self,
        pointer: Duration,
        duplicate: Duration,
        sink: Duration,
        is_duplicate: bool,
    ) {
        self.frames += 1;
        self.duplicates += usize::from(is_duplicate);
        self.pointer += pointer;
        self.pointer_max = self.pointer_max.max(pointer);
        self.duplicate += duplicate;
        self.duplicate_max = self.duplicate_max.max(duplicate);
        self.sink += sink;
        self.sink_max = self.sink_max.max(sink);
    }

    fn log(&mut self) {
        if self.frames == 0 {
            return;
        }
        let frames = self.frames as f64;
        log::info!(
            target: "timing",
            "capture stages ({} frames, {} duplicates): pointer avg {:.2}/max {:.2} ms, \
             dedupe avg {:.2}/max {:.2} ms, sink push avg {:.2}/max {:.2} ms",
            self.frames,
            self.duplicates,
            ms(self.pointer) / frames,
            ms(self.pointer_max),
            ms(self.duplicate) / frames,
            ms(self.duplicate_max),
            ms(self.sink) / frames,
            ms(self.sink_max),
        );
        *self = Self::default();
    }
}

fn rasterize_recording_watermark(
    options: &RecordOptions,
    frame: &FrameData,
    logical_width: u32,
    logical_height: u32,
) -> Result<Option<Arc<RasterLayer>>> {
    if !options.watermark.is_enabled() {
        return Ok(None);
    }
    let (width, height) = frame.dimensions();
    let scale = (width as f32 / logical_width.max(1) as f32)
        .max(height as f32 / logical_height.max(1) as f32);
    let layer = Arc::new(options.watermark.rasterize(width, height, scale)?);
    log::info!(
        "recording watermark: enabled, canvas={width}x{height}, layout_scale={scale:.3}, tiles={}",
        layer.tiles.len()
    );
    Ok(Some(layer))
}

fn capture_loop(
    job: CaptureJob,
    mut ready: Option<SyncSender<StartReady>>,
) -> Result<(Duration, Option<RgbaImage>)> {
    let CaptureJob {
        monitor_id,
        region,
        output,
        mut options,
        stop,
        paused,
        frames,
    } = job;
    let configured_click_effects = options.click_effects;
    options.click_effects = options.click_effects_enabled();
    log::info!(
        "pointer effects: cursor={}, configured_clicks={configured_click_effects}, effective_clicks={}",
        options.captures_cursor,
        options.click_effects
    );
    #[cfg(target_os = "windows")]
    let started = Instant::now();
    let logical_width = region.width.round().max(1.0) as u32;
    let logical_height = region.height.round().max(1.0) as u32;
    // Screenshots and recordings share the display's physical pixels. A caller
    // that explicitly sets `max_width` still gets the CPU box-mean fallback,
    // but the app never silently treats logical points as output pixels.
    let output_size = OutputSize::Native;
    // A GIF or APNG has no extended range, so asking the compositor for
    // half-float frames would only add tonemapping work on the capture thread.
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
    let mut live = StopOnDrop(Some(live));
    let mut pointer = pointer::PointerState::new(
        monitor_id,
        region,
        options.captures_cursor,
        options.click_effects,
        options.click_effect,
    );
    // The encoder's size is fixed when it is created, so it is opened on the
    // first frame the platform recorder actually delivers.
    let mut sink: Option<Box<dyn FrameSink>> = None;
    let mut origin: Option<Duration> = None;
    let mut first_received: Option<Instant> = None;
    // A Windows fallback still seeds timestamp zero. DXGI timestamps remain
    // relative to backend creation, so keep the offset that aligns the first
    // live frame with wall time and apply it to every later frame as well.
    let mut rebase_next_frame = false;
    let mut live_timestamp_offset = Duration::ZERO;
    let mut first: Option<RgbaImage> = None;
    let mut previous: Option<FrameData> = None;
    let mut pending_duplicate_at: Option<Duration> = None;
    let mut watermark_layer: Option<Arc<RasterLayer>> = None;
    // Presentation times must strictly increase: AVFoundation rejects two
    // samples sharing a time, and a resume can land a millisecond before the
    // pause clock has accumulated the whole stretch.
    let mut last_at = Duration::ZERO;
    let mut last_animation_at = Duration::ZERO;
    let animation_interval = Duration::from_secs_f64(1.0 / options.fps.max(1) as f64);
    let mut pushed = 0usize;
    let mut stage_stats = CaptureStageStats::default();

    let initialize_sink = |image: FrameData, at: Duration| -> Result<_> {
        // A/B evidence: the delivered size next to the selection's logical
        // size shows whether the compositor honoured the native-size hint.
        let (width, height) = image.dimensions();
        let range = match &image {
            FrameData::Hdr(_) => "hdr",
            FrameData::Sdr(_) => "sdr",
        };
        log::info!(
            "first capture frame {width}x{height} ({range}, output size hint \
             {output_size:?}, logical selection {logical_width}x{logical_height})"
        );
        let watermark =
            rasterize_recording_watermark(&options, &image, logical_width, logical_height)?;
        let (created, thumbnail) =
            open_first_sink(&output, &options, &image, at, watermark.as_ref())?;
        Ok((created, thumbnail, watermark))
    };

    let push = |sink: &mut Box<dyn FrameSink>,
                frames: &Arc<AtomicUsize>,
                pushed: &mut usize,
                image: FrameData,
                at: Duration,
                watermark: Option<&Arc<RasterLayer>>|
     -> Result<()> {
        let image = image.apply_watermark(watermark);
        if sink.push(image, at)? {
            *pushed += 1;
            frames.store(*pushed, Ordering::Relaxed);
        }
        Ok(())
    };

    while !stop.load(Ordering::Relaxed)
        && !past_duration_limit(
            recording_elapsed(first_received, Instant::now(), paused.paused_duration()),
            options.max_duration,
        )
    {
        // Bounds how long a stop request waits for this thread to notice it
        // without busy-waiting: 4 ms costs ~0.1% of a core, 0 ms pegs one.
        match live
            .0
            .as_ref()
            .expect("recorder is active")
            .recv_timeout(Duration::from_millis(4))
        {
            LiveFrameWait::Frame(frame) => {
                if stop.load(Ordering::Relaxed) {
                    // Leave the closing path intact: it writes the final held
                    // frame and finalizes the container before publishing it.
                    continue;
                }
                if paused.is_paused() {
                    // Keep draining the platform recorder and the button baseline,
                    // but do not encode these frames or advance the timeline.
                    pointer.sample_paused();
                    continue;
                }
                let first_frame = origin.is_none();
                let frame_origin = *origin.get_or_insert(frame.timestamp);
                let at = if rebase_next_frame {
                    rebase_next_frame = false;
                    let wall_time = first_received
                        .expect("fallback frame established the media origin")
                        .elapsed()
                        .saturating_sub(paused.paused_duration());
                    live_timestamp_offset = frame.timestamp.saturating_sub(wall_time);
                    wall_time
                } else if first_frame {
                    Duration::ZERO
                } else {
                    frame
                        .timestamp
                        .saturating_sub(frame_origin)
                        .saturating_sub(live_timestamp_offset)
                        .saturating_sub(paused.paused_duration())
                };
                let at = if at > last_at {
                    at
                } else {
                    last_at + Duration::from_millis(1)
                };
                last_at = at;
                pointer.sample(at);
                let pointer_started = Instant::now();
                let image = pointer.apply(FrameData::from(frame.data), at);
                let pointer_elapsed = pointer_started.elapsed();
                if sink.is_none() {
                    let sink_started = Instant::now();
                    let (created, thumbnail, watermark) = initialize_sink(image.clone(), at)?;
                    let sink_elapsed = sink_started.elapsed();
                    sink = Some(created);
                    watermark_layer = watermark;
                    pushed = 1;
                    frames.store(pushed, Ordering::Relaxed);
                    first = Some(thumbnail);
                    // Encoder setup is outside the media timeline. Start the
                    // UI clock only once the first sample has been accepted.
                    let received = Instant::now();
                    first_received = Some(received);
                    signal_ready(&mut ready, Ok(received));
                    previous = Some(image);
                    stage_stats.record(pointer_elapsed, Duration::ZERO, sink_elapsed, false);
                    continue;
                }
                let sink = sink.as_mut().expect("sink was created on the first frame");
                let duplicate_started = Instant::now();
                let duplicate = previous
                    .as_ref()
                    .is_some_and(|prev| prev.likely_same_as(&image) && prev.is_same_as(&image));
                let duplicate_elapsed = duplicate_started.elapsed();
                let sink_started = Instant::now();
                if duplicate {
                    pending_duplicate_at = Some(at);
                } else {
                    push(
                        sink,
                        &frames,
                        &mut pushed,
                        image.clone(),
                        at,
                        watermark_layer.as_ref(),
                    )?;
                    pending_duplicate_at = None;
                    previous = Some(image);
                }
                let sink_elapsed = sink_started.elapsed();
                stage_stats.record(pointer_elapsed, duplicate_elapsed, sink_elapsed, duplicate);
                if stage_stats.frames >= 120 {
                    stage_stats.log();
                }
            }
            LiveFrameWait::Timeout => {
                if paused.is_paused() {
                    pointer.sample_paused();
                    continue;
                }
                #[cfg(target_os = "windows")]
                if sink.is_none()
                    && first_received.is_none()
                    && !stop.load(Ordering::Relaxed)
                    && started.elapsed() >= WINDOWS_FIRST_FRAME_GRACE
                {
                    // DXGI only reports changes. Give the stream a short chance
                    // to deliver, then seed a static desktop from one still. A
                    // cancellation that lands during this capture still takes
                    // the discard-only path and never opens an output file.
                    log::info!(
                        "no live frame after {:.1} ms; capturing Windows fallback still",
                        ms(started.elapsed())
                    );
                    let capture_started = Instant::now();
                    let probe = match RegionCapturer::new(monitor_id, region).and_then(|capturer| {
                        capturer
                            .capture()
                            .context("failed to capture Windows fallback still")
                    }) {
                        Ok(image) => FrameData::Sdr(Arc::new(image)),
                        Err(err) => {
                            signal_ready(&mut ready, Err(format!("{err:#}")));
                            return Err(err);
                        }
                    };
                    if stop.load(Ordering::Relaxed) {
                        return Ok((Duration::ZERO, None));
                    }
                    let still_elapsed = capture_started.elapsed();
                    origin = Some(Duration::ZERO);
                    pointer.sample(Duration::ZERO);
                    let pointer_started = Instant::now();
                    let image = pointer.apply(probe, Duration::ZERO);
                    let pointer_elapsed = pointer_started.elapsed();
                    let sink_started = Instant::now();
                    let (created, thumbnail, watermark) =
                        initialize_sink(image.clone(), Duration::ZERO)?;
                    let sink_elapsed = sink_started.elapsed();
                    sink = Some(created);
                    watermark_layer = watermark;
                    rebase_next_frame = true;
                    pushed = 1;
                    frames.store(pushed, Ordering::Relaxed);
                    first = Some(thumbnail);
                    previous = Some(image);
                    stage_stats.record(pointer_elapsed, Duration::ZERO, sink_elapsed, false);
                    let received = Instant::now();
                    first_received = Some(received);
                    signal_ready(&mut ready, Ok(received));
                    log::info!(
                        "Windows fallback first frame ready after {:.1} ms (still {:.1} ms, sink {:.1} ms); HDR request falls back to SDR when no HDR frame arrived",
                        ms(started.elapsed()),
                        ms(still_elapsed),
                        ms(sink_elapsed)
                    );
                    continue;
                }
                let at = first_received
                    .map(|received| received.elapsed())
                    .unwrap_or_default()
                    .saturating_sub(paused.paused_duration());
                pointer.sample(at);
                let pointer_started = Instant::now();
                if sink.is_some()
                    && at.saturating_sub(last_animation_at) >= animation_interval
                    && let Some(image) = pointer.apply_timeout(at)
                {
                    let pointer_elapsed = pointer_started.elapsed();
                    let at = at.max(last_at + Duration::from_millis(1));
                    last_at = at;
                    last_animation_at = at;
                    let sink_started = Instant::now();
                    push(
                        sink.as_mut().expect("sink was created on the first frame"),
                        &frames,
                        &mut pushed,
                        image.clone(),
                        at,
                        watermark_layer.as_ref(),
                    )?;
                    let sink_elapsed = sink_started.elapsed();
                    stage_stats.record(pointer_elapsed, Duration::ZERO, sink_elapsed, false);
                    if stage_stats.frames >= 120 {
                        stage_stats.log();
                    }
                    pending_duplicate_at = None;
                    previous = Some(image);
                }
            }
            LiveFrameWait::Disconnected => {
                signal_ready(
                    &mut ready,
                    Err("the platform recorder stopped before delivering a frame".into()),
                );
                break;
            }
        }
    }

    stage_stats.log();
    let closing = Instant::now();
    if sink.is_none() {
        // The Windows static-desktop fallback runs inside the timeout branch.
        // Reaching here means cancellation or a disconnected backend: neither
        // may create a file after start has failed or been cancelled.
        return Ok((Duration::ZERO, None));
    }
    if let (Some(image), Some(timestamp), Some(created)) =
        (&previous, pending_duplicate_at, sink.as_mut())
    {
        // Keep the final still stretch visible for its real duration.
        push(
            created,
            &frames,
            &mut pushed,
            image.clone(),
            timestamp,
            watermark_layer.as_ref(),
        )?;
    }
    let last_frame = closing.elapsed();

    let duration = recording_elapsed(first_received, closing, paused.paused_duration());
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
        let stop_error = live
            .0
            .take()
            .expect("recorder is still active")
            .stop()
            .err();
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
    use snapit_capture::RasterTile;
    use std::sync::mpsc;

    fn red_watermark() -> Arc<RasterLayer> {
        Arc::new(RasterLayer {
            tiles: vec![RasterTile {
                bounds: PixelRect {
                    x: 4,
                    y: 4,
                    width: 8,
                    height: 8,
                },
                pixels: vec![[255, 0, 0, 255]; 8 * 8],
            }],
        })
    }

    #[test]
    fn watermark_composition_is_opt_in_and_does_not_mutate_the_shared_source() {
        let source = Arc::new(RgbaImage::from_pixel(16, 16, image::Rgba([0, 0, 0, 255])));
        let unchanged = FrameData::Sdr(source.clone()).apply_watermark(None);
        assert!(matches!(unchanged, FrameData::Sdr(ref image) if Arc::ptr_eq(&source, image)));

        let watermark = Arc::new(RasterLayer {
            tiles: vec![RasterTile {
                bounds: PixelRect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                pixels: vec![[255, 0, 0, 128]],
            }],
        });
        let composed = FrameData::Sdr(source.clone()).apply_watermark(Some(&watermark));
        assert_eq!(source.get_pixel(0, 0).0, [0, 0, 0, 255]);
        assert!(
            matches!(composed, FrameData::Sdr(ref image) if image.get_pixel(0, 0).0 == [128, 0, 0, 255])
        );
    }

    #[test]
    fn animation_first_frames_include_the_watermark() {
        for format in [RecordFormat::Gif, RecordFormat::Apng] {
            let dir = temp_dir("watermark-first-frame");
            let output = dir.join(format!("out.{}", format.extension()));
            let options = RecordOptions::default()
                .with_format(format)
                .with_quality(90);
            let watermark = red_watermark();
            let first = sdr(RgbaImage::from_pixel(32, 24, image::Rgba([0, 0, 0, 255])));
            let (sink, thumbnail) =
                open_first_sink(&output, &options, &first, Duration::ZERO, Some(&watermark))
                    .unwrap();
            assert_eq!(thumbnail.get_pixel(5, 5).0, [255, 0, 0, 255]);
            Box::new(sink).finish(Duration::from_millis(100)).unwrap();

            let decoded = match format {
                RecordFormat::Gif => {
                    let mut options = gif::DecodeOptions::new();
                    options.set_color_output(gif::ColorOutput::RGBA);
                    let mut decoder = options.read_info(File::open(&output).unwrap()).unwrap();
                    let frame = decoder.read_next_frame().unwrap().unwrap();
                    RgbaImage::from_raw(
                        u32::from(frame.width),
                        u32::from(frame.height),
                        frame.buffer.to_vec(),
                    )
                    .unwrap()
                }
                RecordFormat::Apng => image::ImageReader::open(&output)
                    .unwrap()
                    .decode()
                    .unwrap()
                    .to_rgba8(),
                RecordFormat::Mp4 => unreachable!(),
            };
            assert_eq!(decoded.get_pixel(5, 5).0, [255, 0, 0, 255]);
            assert!(
                decoded.get_pixel(0, 0).0[..3]
                    .iter()
                    .all(|value| *value <= 1)
            );
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn gif_palette_training_samples_the_composed_watermark_region() {
        let mut frame = RgbaImage::from_pixel(64, 64, image::Rgba([0, 0, 0, 255]));
        let color = [250, 0, 250];
        let region = PixelRect {
            x: 24,
            y: 24,
            width: 16,
            height: 16,
        };
        for y in region.y..region.bottom() {
            for x in region.x..region.right() {
                frame.put_pixel(x, y, image::Rgba([color[0], color[1], color[2], 255]));
            }
        }
        let samples = spatial_sample_regions(&frame, &[region], 64);
        assert_eq!(samples.len(), 64 * 4);
        assert!(
            samples
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| { pixel[..3] == color && pixel[3] == 255 })
        );
        let palette = PaletteMap::train_with_regions(&frame, 90, &[region]);
        assert!(
            palette
                .palette
                .as_chunks::<3>()
                .0
                .iter()
                .any(|entry| entry.as_slice() == color)
        );
    }

    /// A frame as the capture loop hands it to a sink.
    fn sdr(image: RgbaImage) -> FrameData {
        FrameData::Sdr(Arc::new(image))
    }

    #[test]
    fn click_effects_round_trip_through_their_config_names() {
        for effect in ClickEffect::ALL {
            assert_eq!(ClickEffect::parse(effect.config_name()), Some(effect));
        }
        assert_eq!(ClickEffect::parse(" BURST "), Some(ClickEffect::Burst));
        assert_eq!(ClickEffect::parse("sparkle"), Some(ClickEffect::Sparkle));
        assert_eq!(ClickEffect::parse("unknown"), None);
        assert_eq!(RecordOptions::default().click_effect, ClickEffect::Ring);
    }

    #[test]
    fn click_effect_groups_cover_every_style_once() {
        let grouped = ClickEffectGroup::ALL
            .into_iter()
            .flat_map(ClickEffectGroup::effects)
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(grouped.len(), ClickEffect::ALL.len());
        for effect in ClickEffect::ALL {
            assert_eq!(grouped.iter().filter(|item| **item == effect).count(), 1);
            assert!(effect.group().effects().contains(&effect));
        }
    }

    #[test]
    fn click_effects_require_cursor_capture_without_erasing_the_preference() {
        let options = RecordOptions::default().with_cursor(false);
        assert!(options.click_effects);
        assert!(!options.click_effects_enabled());
        assert!(options.clone().with_cursor(true).click_effects_enabled());
        assert!(!options.with_click_effects(false).click_effects_enabled());
    }

    #[test]
    fn formats_round_trip_through_their_config_names() {
        for format in [RecordFormat::Gif, RecordFormat::Apng, RecordFormat::Mp4] {
            assert_eq!(RecordFormat::parse(format.config_name()), Some(format));
        }
        assert_eq!(RecordFormat::parse("png"), Some(RecordFormat::Apng));
        assert_eq!(RecordFormat::Apng.extension(), "png");
        assert_eq!(RecordFormat::Apng.config_name(), "apng");
    }

    #[test]
    fn duplicate_filter_keeps_exact_comparison_as_the_final_authority() {
        let first_image = RgbaImage::from_pixel(128, 128, image::Rgba([10, 20, 30, 255]));
        let first = sdr(first_image.clone());
        let mut changed = first_image.clone();
        changed.put_pixel(7, 9, image::Rgba([200, 30, 10, 255]));
        let changed = sdr(changed);
        assert!(first.likely_same_as(&changed));
        assert!(!first.is_same_as(&changed));

        let mut moved = first_image;
        moved.put_pixel(32, 0, image::Rgba([1, 2, 3, 255]));
        assert!(!first.likely_same_as(&sdr(moved)));
    }

    #[test]
    fn animation_queue_scales_down_at_physical_resolution() {
        assert_eq!(animation_queue_capacity(1), 8);
        assert_eq!(animation_queue_capacity(1920 * 1080 * 4), 8);
        assert_eq!(animation_queue_capacity(5120 * 2880 * 4), 2);
        assert_eq!(animation_queue_capacity(7680 * 4320 * 4), 1);
        assert_eq!(animation_queue_capacity(ANIMATION_QUEUE_BUDGET), 1);
        assert_eq!(animation_queue_capacity(ANIMATION_QUEUE_BUDGET + 1), 1);
    }

    #[test]
    fn animation_queue_rejects_a_frame_larger_than_its_budget() {
        assert!(validate_animation_frame_size(ANIMATION_QUEUE_BUDGET).is_ok());
        let error = validate_animation_frame_size(ANIMATION_QUEUE_BUDGET + 1).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("first animation frame"));
        assert!(message.contains("128 MiB queue budget"));
        assert!(message.contains("not downscaled"));

        let dir = temp_dir("oversized-animation");
        let output = dir.join("out.gif");
        let error = AnimationSink::new(
            &output,
            &RecordOptions::default(),
            RecordFormat::Gif,
            ANIMATION_QUEUE_BUDGET + 1,
            None,
            (1, 1),
        )
        .err()
        .expect("oversized animation frame must be rejected");
        assert!(error.to_string().contains("128 MiB queue budget"));
        assert!(!output.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn animation_queue_rejects_later_frames_that_exceed_its_byte_budget() {
        assert!(validate_animation_queue_frame(ANIMATION_QUEUE_BUDGET / 8, 8).is_ok());
        let error = validate_animation_queue_frame(ANIMATION_QUEUE_BUDGET / 8 + 1, 8).unwrap_err();
        assert!(error.to_string().contains("per-frame limit"));
    }

    #[test]
    fn recording_time_starts_at_the_media_origin_and_subtracts_pauses() {
        let backend_started = Instant::now();
        let warmup = Duration::from_secs(2);
        let media_origin = backend_started + warmup;
        let paused = Duration::from_millis(750);

        assert_eq!(
            recording_elapsed(None, backend_started + warmup, paused),
            Duration::ZERO
        );
        assert_eq!(
            recording_elapsed(Some(media_origin), media_origin, paused),
            Duration::ZERO
        );
        assert_eq!(
            recording_elapsed(
                Some(media_origin),
                media_origin + Duration::from_secs(3),
                paused
            ),
            Duration::from_millis(2250)
        );
        assert_eq!(
            recording_elapsed(Some(media_origin), media_origin - warmup, paused),
            Duration::ZERO
        );
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
        for format in [RecordFormat::Gif, RecordFormat::Apng, RecordFormat::Mp4] {
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
    fn a_sink_never_truncates_an_existing_recording() {
        let dir = temp_dir("existing");
        let output = dir.join("out.gif");
        std::fs::write(&output, b"previous recording").unwrap();

        assert!(
            AnimationSink::new(
                &output,
                &RecordOptions::default(),
                RecordFormat::Gif,
                4,
                None,
                (0, 0)
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&output).unwrap(), b"previous recording");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_full_gif_queue_drops_frames_instead_of_blocking_capture() {
        let (sender, receiver) = sync_channel(1);
        let mut sink = AnimationSink {
            format: RecordFormat::Gif,
            sender: Some(sender),
            queue_capacity: 1,
            writer: None,
            pending: None,
            pending_at: None,
            dropped: 0,
        };
        for value in 1..=3 {
            sink.push(
                sdr(RgbaImage::from_pixel(2, 2, image::Rgba([value, 0, 0, 255]))),
                Duration::from_millis(value as u64 * 100),
            )
            .unwrap();
        }

        assert_eq!(sink.dropped, 1);
        assert_eq!(receiver.try_recv().unwrap().image.as_raw()[0], 1);
        assert_eq!(sink.pending_at, Some(Duration::from_millis(200)));
        assert_eq!(sink.pending.unwrap().as_raw()[0], 2);
    }

    #[test]
    fn dropping_a_recorder_signals_capture_to_exit() {
        let dir = temp_dir("drop");
        let stop = Arc::new(AtomicBool::new(false));
        let capture_stop = stop.clone();
        let (exited_tx, exited_rx) = mpsc::channel();
        let capture_thread = thread::Builder::new()
            .name("snapit-test-capture".into())
            .spawn(move || {
                while !capture_stop.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(1));
                }
                let _ = exited_tx.send(());
                Ok::<_, anyhow::Error>((Duration::ZERO, None))
            })
            .unwrap();
        let recorder = Recorder {
            stop,
            paused: Arc::new(PauseClock::default()),
            frames: Arc::new(AtomicUsize::new(0)),
            started: Instant::now(),
            max_duration: Duration::ZERO,
            output: dir.join("unused.gif"),
            format: RecordFormat::Gif,
            capture_thread: Some(capture_thread),
        };

        drop(recorder);
        exited_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("dropping a recorder left its capture thread running");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn default_options_preserve_physical_frame_pixels() {
        let options = RecordOptions::default();
        assert_eq!(options.max_width, None);
        let image = RgbaImage::new(2560, 1600);
        let prepared = video::prepare(&image, options.max_width);
        assert!(matches!(prepared, Cow::Borrowed(_)));
        assert_eq!(prepared.dimensions(), (2560, 1600));
    }

    #[test]
    fn quality_maps_onto_palette_training_samples() {
        assert!(quality_to_samples(1) < quality_to_samples(50));
        assert!(quality_to_samples(50) < quality_to_samples(100));
        assert_eq!(quality_to_samples(90), 50_176);
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
    fn the_gif_palette_reserves_every_click_color() {
        let frame = RgbaImage::from_pixel(64, 48, image::Rgba([250, 225, 224, 255]));
        let mut map = PaletteMap::train(&frame, 90);
        for color in pointer::CLICK_PALETTE {
            map.reserve_color(color);
        }
        assert_eq!(map.reserved.len(), pointer::CLICK_PALETTE.len());

        for color in pointer::CLICK_PALETTE {
            for y in 0..4 {
                for x in 0..4 {
                    let index = map.index_at(&[color[0], color[1], color[2], 255], x, y) as usize;
                    assert_eq!(&map.palette[index * 3..index * 3 + 3], &color);
                }
            }
        }
    }

    #[test]
    fn a_scene_change_requires_a_palette_refresh() {
        let dark = RgbaImage::from_pixel(96, 64, image::Rgba([12, 18, 24, 255]));
        let bright = RgbaImage::from_pixel(96, 64, image::Rgba([240, 200, 120, 255]));
        let mut map = PaletteMap::train(&dark, 90);
        assert!(!map.needs_refresh(&dark));
        assert!(map.needs_refresh(&bright));
    }

    #[test]
    fn a_small_colourful_object_requires_a_palette_refresh() {
        let neutral = RgbaImage::from_pixel(96, 64, image::Rgba([128, 128, 128, 255]));
        let mut map = PaletteMap::train(&neutral, 90);
        let mut scene = neutral.clone();
        for y in 10..19 {
            for x in 10..19 {
                scene.put_pixel(x, y, image::Rgba([250, 0, 250, 255]));
            }
        }

        let sampled = spatial_sample(&scene, GIF_ERROR_SAMPLES);
        let mut total = 0u64;
        let mut bad = 0usize;
        for pixel in sampled.as_chunks::<4>().0 {
            let at = map.index(pixel) as usize * 3;
            let color = &map.palette[at..at + 3];
            let mut squared = 0u64;
            for (value, mapped) in pixel.iter().take(3).zip(color) {
                let delta = *value as i32 - *mapped as i32;
                squared += (delta * delta) as u64;
            }
            total += squared;
            if squared > GIF_REFRESH_BAD_PIXEL_ERROR {
                bad += 1;
            }
        }
        let pixels = sampled.len() / 4;
        assert!(total / pixels as u64 <= GIF_REFRESH_ERROR);
        assert!(bad * 100 >= pixels * GIF_REFRESH_BAD_PIXEL_PERCENT);
        assert!(map.needs_refresh(&scene));
    }

    #[test]
    fn ordered_dithering_spreads_a_midtone_across_neighbouring_colours() {
        let mut ramp = RgbaImage::new(256, 1);
        for value in 0..256u16 {
            ramp.put_pixel(
                value as u32,
                0,
                image::Rgba([value as u8, value as u8, value as u8, 255]),
            );
        }
        let mut map = PaletteMap::train(&ramp, 100);
        let midtone = RgbaImage::from_pixel(4, 4, image::Rgba([65, 65, 65, 255]));
        let indices = map.indices(&midtone);
        assert!(indices.iter().any(|index| *index != indices[0]));
    }

    #[test]
    fn partial_frames_use_the_same_dither_coordinates_as_full_frames() {
        let mut frame = RgbaImage::new(32, 32);
        for y in 0..frame.height() {
            for x in 0..frame.width() {
                frame.put_pixel(x, y, image::Rgba([x as u8 * 8, y as u8 * 8, 128, 255]));
            }
        }
        let mut map = PaletteMap::train(&frame, 90);
        let full = map.indices(&frame);
        let rect = DirtyRect {
            x: 5,
            y: 7,
            width: 9,
            height: 11,
        };
        let partial = map.indices_rect(&frame, rect);
        let expected: Vec<u8> = (rect.y..rect.y + rect.height)
            .flat_map(|y| {
                full[y as usize * frame.width() as usize + rect.x as usize
                    ..y as usize * frame.width() as usize + (rect.x + rect.width) as usize]
                    .iter()
                    .copied()
            })
            .collect();
        assert_eq!(partial, expected);
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

    #[test]
    fn a_full_animation_queue_keeps_the_oldest_pending_duration() {
        let (sender, receiver) = sync_channel(1);
        let mut sink = AnimationSink {
            format: RecordFormat::Apng,
            sender: Some(sender),
            queue_capacity: 1,
            writer: None,
            pending: None,
            pending_at: None,
            dropped: 0,
        };
        let first = Arc::new(RgbaImage::from_pixel(2, 2, image::Rgba([10, 0, 0, 255])));
        let second = Arc::new(RgbaImage::from_pixel(2, 2, image::Rgba([20, 0, 0, 255])));
        let third = Arc::new(RgbaImage::from_pixel(2, 2, image::Rgba([30, 0, 0, 255])));

        assert!(sink.push(FrameData::Sdr(first), Duration::ZERO).unwrap());
        assert!(
            sink.push(FrameData::Sdr(second.clone()), Duration::from_millis(100))
                .unwrap()
        );
        assert!(
            !sink
                .push(FrameData::Sdr(third), Duration::from_millis(200))
                .unwrap()
        );
        assert_eq!(sink.dropped, 1);
        assert_eq!(sink.pending_at, Some(Duration::from_millis(100)));
        assert!(Arc::ptr_eq(sink.pending.as_ref().unwrap(), &second));
        let queued = receiver.try_recv().unwrap();
        assert_eq!(queued.duration, Duration::from_millis(100));
        let finish = thread::spawn(move || Box::new(sink).finish(Duration::from_millis(500)));
        let held = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(held.duration, Duration::from_millis(400));
        finish.join().unwrap().unwrap();
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
        let mut sink =
            AnimationSink::new(&output, &options, RecordFormat::Gif, 4, None, (0, 0)).unwrap();

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
        let quality = 80;
        let max_width = Some(32);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output)
            .unwrap();
        let (sender, receiver) = sync_channel(24);
        let writer = thread::Builder::new()
            .name("snapit-test-gif-writer".into())
            .spawn(move || encode_gif(receiver, file, quality, max_width, false, None, (0, 0)))
            .unwrap();
        for i in 0..24u64 {
            let color = if i < 10 { 12 } else { 240 };
            sender
                .send(PendingFrame {
                    image: Arc::new(RgbaImage::from_pixel(
                        64,
                        48,
                        image::Rgba([color, color, color, 255]),
                    )),
                    duration: Duration::from_millis(100),
                })
                .unwrap();
        }
        drop(sender);
        writer.join().unwrap().unwrap();
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
        let mut sink =
            AnimationSink::new(&output, &options, RecordFormat::Gif, 4, None, (0, 0)).unwrap();
        let base = RgbaImage::from_pixel(64, 48, image::Rgba([250, 225, 224, 255]));
        let mut lit = base.clone();
        for y in 10..20 {
            for x in 20..30 {
                lit.put_pixel(x, y, image::Rgba([238, 155, 44, 255]));
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
        assert_eq!(frames.len(), 3, "a static frame must still be written");
        assert_eq!(frames[1].get_pixel(25, 15).0, [238, 155, 44, 255]);
        assert_eq!(
            frames[1].get_pixel(5, 5),
            frames[0].get_pixel(5, 5),
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
    fn a_cancelled_start_never_opens_the_capture_backend() {
        let start = RecorderStart::default();
        start.cancel();
        let result = Recorder::start_with_cancel(
            u32::MAX,
            Region::new(0., 0., 1., 1.),
            RecordOptions::default(),
            std::env::temp_dir().join("snapit-cancelled-start.gif"),
            start,
        );
        assert!(result.is_err());
    }

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
