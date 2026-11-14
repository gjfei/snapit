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

use std::{
    fs::File,
    io::{BufWriter, Write as _},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow};
use gif::Repeat;
use snapit_capture::{LiveFrameWait, LiveRecorder, Region, RegionCapturer, RgbaImage};

mod video;

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

/// A running recording. Call [`Recorder::stop`] to finish and flush the file.
pub struct Recorder {
    stop: Arc<AtomicBool>,
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
        // Probe once so permission errors surface before threads spawn.
        let probe = RegionCapturer::new(monitor_id, region)?
            .capture()
            .context("failed to capture region")?;

        let stop = Arc::new(AtomicBool::new(false));
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
                probe,
                output: output.clone(),
                options: options.clone(),
                stop,
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
            Ok(Ok(())) => {}
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
            frames,
            started: Instant::now(),
            max_duration: options.max_duration,
            output,
            format: options.format,
            capture_thread,
        })
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
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
        self.stop.store(true, Ordering::Relaxed);
        let (duration, first_frame) = self
            .capture_thread
            .join()
            .map_err(|_| anyhow!("capture thread panicked"))??;
        Ok(RecordingSummary {
            path: self.output,
            format: self.format,
            frames: self.frames.load(Ordering::Relaxed),
            duration,
            first_frame,
        })
    }
}

/// Where a captured frame goes. Both sinks are driven from the capture thread
/// and finish on it, so `stop` only has one thread left to join.
pub(crate) trait FrameSink: Send {
    /// Encode one frame, shown from `at` until the next one.
    fn push(&mut self, frame: &RgbaImage, at: Duration) -> Result<()>;

    /// Flush the container. `end` is the recording's total duration, which is
    /// how long the last frame stays on screen.
    fn finish(self: Box<Self>, end: Duration) -> Result<()>;
}

/// A frame handed to the writer thread: the image plus how long it stays on
/// screen, in GIF delay ticks of 10 ms.
struct PendingFrame {
    image: RgbaImage,
    ticks: u16,
}

/// Channel to the writer thread plus the frame shown last, which is queued only
/// once its duration is known (a frame ends when the next one starts).
struct GifSink {
    sender: Option<SyncSender<PendingFrame>>,
    writer: Option<JoinHandle<Result<()>>>,
    pending: Option<RgbaImage>,
    pending_at: Option<Duration>,
}

impl GifSink {
    fn new(output: &Path, options: &RecordOptions) -> Result<Self> {
        // Probe once so permission errors surface before threads spawn.
        let file = File::create(output)
            .with_context(|| format!("failed to create {}", output.display()))?;
        let speed = quality_to_speed(options.quality);
        let max_width = options.max_width;
        // Bounded so a slow encoder slows the capture down instead of piling
        // frames up in memory.
        let (sender, receiver) = sync_channel::<PendingFrame>(8);
        let writer = thread::Builder::new()
            .name("snapit-gif-writer".into())
            .spawn(move || encode_gif(receiver, file, speed, max_width))?;
        Ok(Self {
            sender: Some(sender),
            writer: Some(writer),
            pending: None,
            pending_at: None,
        })
    }
}

impl FrameSink for GifSink {
    fn push(&mut self, frame: &RgbaImage, at: Duration) -> Result<()> {
        let sender = self
            .sender
            .as_ref()
            .ok_or_else(|| anyhow!("GIF encoder already closed"))?;
        if let (Some(image), Some(started)) = (self.pending.take(), self.pending_at) {
            let ticks = duration_to_ticks(at - started);
            sender
                .send(PendingFrame { image, ticks })
                .map_err(|_| anyhow!("failed to queue frame: the GIF writer stopped"))?;
        }
        self.pending = Some(frame.clone());
        self.pending_at = Some(at);
        Ok(())
    }

    fn finish(mut self: Box<Self>, end: Duration) -> Result<()> {
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
        result?;
        if let Some(writer) = self.writer.take() {
            writer
                .join()
                .map_err(|_| anyhow!("GIF writer thread panicked"))??;
        }
        Ok(())
    }
}

/// Quantize and append frames to the file until the channel closes.
fn encode_gif(
    receiver: Receiver<PendingFrame>,
    file: File,
    speed: i32,
    max_width: Option<u32>,
) -> Result<()> {
    let mut writer = BufWriter::new(file);
    // The encoder's screen size is fixed at creation, so anchor it on the
    // first frame that is encodable. A stream that yields none leaves the
    // file empty rather than corrupt.
    let anchored = loop {
        match receiver.recv() {
            Ok(pending) => {
                // Owned up front so the frame and its pixels move out together.
                let scaled = video::prepare(&pending.image, max_width).into_owned();
                if let Some((width, height)) = fit_u16(&scaled) {
                    break Some((pending, scaled, width, height));
                }
            }
            Err(_) => break None,
        }
    };
    let Some((pending, scaled, width, height)) = anchored else {
        writer.flush().context("failed to flush the GIF file")?;
        return Ok(());
    };
    let mut encoder = gif::Encoder::new(&mut writer, width, height, &[])
        .context("failed to create GIF encoder")?;
    encoder
        .write_extension(gif::ExtensionData::Repetitions(Repeat::Infinite))
        .context("failed to set GIF looping")?;
    write_frame(&mut encoder, scaled, pending.ticks, width, height, speed)?;
    // Frames whose size drifted (a display change) do not fit the fixed screen
    // and are skipped rather than corrupting the container.
    while let Ok(pending) = receiver.recv() {
        let scaled = video::prepare(&pending.image, max_width).into_owned();
        let Some((width, height)) = fit_u16(&scaled) else {
            continue;
        };
        write_frame(&mut encoder, scaled, pending.ticks, width, height, speed)?;
    }
    drop(encoder);
    writer.flush().context("failed to flush the GIF file")?;
    Ok(())
}

/// Quantize one frame and append it to the container.
fn write_frame(
    encoder: &mut gif::Encoder<&mut BufWriter<File>>,
    scaled: RgbaImage,
    ticks: u16,
    width: u16,
    height: u16,
    speed: i32,
) -> Result<()> {
    let mut rgba = scaled.into_raw();
    let mut frame = gif::Frame::from_rgba_speed(width, height, &mut rgba, speed);
    frame.delay = ticks;
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

/// Map the 1-100 quality setting onto the quantizer's 1-30 speed range, where
/// lower is slower but better: quality 100 becomes speed 1, quality 1 becomes 30.
fn quality_to_speed(quality: u8) -> i32 {
    ((101 - quality as i32) * 30 / 100).clamp(0, 29) + 1
}

/// A zero cap disables the automatic stop; otherwise the loop ends once the
/// elapsed time reaches it.
fn past_duration_limit(elapsed: Duration, max: Duration) -> bool {
    !max.is_zero() && elapsed >= max
}

fn open_sink(
    output: &Path,
    options: &RecordOptions,
    first_frame: &RgbaImage,
) -> Result<Box<dyn FrameSink>> {
    Ok(match options.format {
        RecordFormat::Gif => Box::new(GifSink::new(output, options)?),
        RecordFormat::Mp4 => Box::new(video::Mp4Sink::new(output, options, first_frame)?),
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
    probe: RgbaImage,
    output: PathBuf,
    options: RecordOptions,
    stop: Arc<AtomicBool>,
    frames: Arc<AtomicUsize>,
}

fn capture_loop(
    job: CaptureJob,
    mut ready: Option<SyncSender<std::result::Result<(), String>>>,
) -> Result<(Duration, Option<RgbaImage>)> {
    let CaptureJob {
        monitor_id,
        region,
        probe,
        output,
        options,
        stop,
        frames,
    } = job;
    let started = Instant::now();
    let live = match LiveRecorder::start_with(
        monitor_id,
        region,
        options.fps,
        options.captures_cursor,
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
    let mut previous: Option<RgbaImage> = None;
    let mut pending_duplicate_at: Option<Duration> = None;
    let mut pushed = 0usize;

    let mut push = |sink: &mut Box<dyn FrameSink>,
                    frames: &Arc<AtomicUsize>,
                    image: &RgbaImage,
                    at: Duration|
     -> Result<()> {
        sink.push(image, at)?;
        pushed += 1;
        frames.store(pushed, Ordering::Relaxed);
        Ok(())
    };

    while !stop.load(Ordering::Relaxed)
        && !past_duration_limit(started.elapsed(), options.max_duration)
    {
        match live.recv_timeout(Duration::from_millis(50)) {
            LiveFrameWait::Frame(frame) => {
                let frame_origin = *origin.get_or_insert(frame.timestamp);
                let at = frame.timestamp.saturating_sub(frame_origin);
                if sink.is_none() {
                    sink = Some(open_sink(&output, &options, &frame.image)?);
                }
                let sink = sink.as_mut().expect("sink was just created");
                let is_duplicate = previous.as_ref().is_some_and(|prev| {
                    prev.dimensions() == frame.image.dimensions()
                        && prev.as_raw() == frame.image.as_raw()
                });
                if is_duplicate {
                    pending_duplicate_at = Some(at);
                } else {
                    push(sink, &frames, &frame.image, at)?;
                    pending_duplicate_at = None;
                    if first.is_none() {
                        first = Some(frame.image.clone());
                    }
                    signal_ready(&mut ready, Ok(()));
                    previous = Some(frame.image);
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

    if let Err(err) = live.stop() {
        log::warn!("failed to stop the platform recorder: {err:#}");
    }
    if sink.is_none() {
        // Windows duplication only reports changes, so a static screen can
        // yield no frame at all; the probe still makes a valid file.
        let mut created = open_sink(&output, &options, &probe)?;
        push(&mut created, &frames, &probe, Duration::ZERO)?;
        first = Some(probe);
        sink = Some(created);
    } else if let (Some(image), Some(timestamp), Some(created)) =
        (&previous, pending_duplicate_at, sink.as_mut())
    {
        // Keep the final still stretch visible for its real duration.
        push(created, &frames, image, timestamp)?;
    }

    let duration = started
        .elapsed()
        .saturating_sub(origin.unwrap_or(Duration::ZERO));
    if let Some(sink) = sink {
        sink.finish(duration)?;
    }
    Ok((duration, first))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

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
    fn quality_maps_onto_the_quantizer_speed_range() {
        assert_eq!(quality_to_speed(100), 1);
        assert_eq!(quality_to_speed(90), 4);
        assert_eq!(quality_to_speed(1), 30);
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
            sink.push(&frame, Duration::from_secs_f64(i as f64 * 0.1))
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
        let _ = std::fs::remove_dir_all(&dir);
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
