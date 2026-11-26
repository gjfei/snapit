//! Live scrolling screenshot capture session built on `snapit-capture`.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, Sender, channel, sync_channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use snapit_capture::{
    DiskImage, DynamicRange, LiveFrameData, LiveFrameWait, LiveRecorder, OutputSize, Region,
    RegionCapturer, RgbaImage,
};

pub mod paged_rows;
pub mod scroller;
pub mod stitch;

pub use scroller::{AutoScrollError, ScrollSpeed, auto_scroll_available};
pub use stitch::{
    FeedOutcome, PreviewTile, ScrollDirection, ScrollWarning, StitchConfig, Stitcher,
};

const CAPTURE_FPS: u32 = 0;
const POLL_TIMEOUT: Duration = Duration::from_millis(4);
const IDLE_FLUSH_DELAY: Duration = Duration::from_millis(120);
const PREVIEW_REFRESH_INTERVAL: Duration = Duration::from_millis(33);
const FRAME_BUFFER_BUDGET_BYTES: usize = 192 * 1024 * 1024;
const MIN_FRAME_BUFFER_CAPACITY: usize = 4;
const MAX_FRAME_BUFFER_CAPACITY: usize = 64;
const AUTO_SCROLL_STALL_LIMIT: u32 = 5;
const AUTO_SCROLL_QUEUE_BACKPRESSURE: usize = 6;
const AUTO_SCROLL_REWIND_LIMIT: u32 = 6;
const SETTLE_DRAIN_CAP: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScrollStatus {
    #[default]
    Starting,
    Capturing,
    AutoScrolling,
    Paused,
    ReachedEnd,
    StoppedByUser,
    Failed,
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ScrollOptions {
    pub auto_scroll_speed: ScrollSpeed,
    pub fps: u32,
    pub scale: f32,
}

impl Default for ScrollOptions {
    fn default() -> Self {
        Self {
            auto_scroll_speed: ScrollSpeed::Medium,
            fps: CAPTURE_FPS,
            scale: 1.0,
        }
    }
}

impl ScrollOptions {
    pub fn with_auto_scroll_speed(mut self, speed: ScrollSpeed) -> Self {
        self.auto_scroll_speed = speed;
        self
    }

    pub fn with_scale(mut self, scale: f32) -> Self {
        self.scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        self
    }
}

#[derive(Clone, Default)]
pub struct ScrollStart {
    cancelled: Arc<AtomicBool>,
}

impl ScrollStart {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

#[derive(Clone, Debug, Default)]
pub struct ScrollSnapshot {
    pub width: u32,
    pub height: u32,
    pub frame_height: u32,
    pub frames: usize,
    pub started: bool,
    pub auto_scrolling: bool,
    pub paused: bool,
    pub status: ScrollStatus,
    pub error: Option<String>,
    pub warning: Option<ScrollWarning>,
    pub direction: ScrollDirection,
    pub auto_direction: ScrollDirection,
    pub content_shift: u32,
    pub preview_revision: u64,
    pub preview: Option<PreviewTile>,
    pub preview_range: (u32, u32),
    pub viewport_origin_y: i64,
    pub pause_frame: Option<Arc<RgbaImage>>,
    pub pause_revision: u64,
}

enum ScrollCommand {
    SetAutoScroll {
        enabled: bool,
        direction: Option<ScrollDirection>,
    },
    SetPaused(bool),
    SetPreviewRange {
        start: u32,
        rows: u32,
        follow: bool,
    },
    SetAnnotationOrigin(i64),
    Reset,
    Finish,
    Discard,
}

type ScrollResult = Result<Option<(DiskImage, u32)>>;

/// Running scrolling capture session.
pub struct ScrollSession {
    cmd_tx: Sender<ScrollCommand>,
    snapshot: Arc<Mutex<Arc<ScrollSnapshot>>>,
    thread: Option<JoinHandle<ScrollResult>>,
}

impl ScrollSession {
    pub fn start_with_cancel(
        monitor_id: u32,
        region: Region,
        options: ScrollOptions,
        start: ScrollStart,
    ) -> Result<Self> {
        if start.cancelled.load(Ordering::Relaxed) {
            anyhow::bail!("scroll capture cancelled");
        }
        if !snapit_capture::screen_recording_granted() {
            return Err(anyhow!("screen recording permission is not granted"));
        }

        let center = snapit_capture::monitors()
            .ok()
            .and_then(|monitors| monitors.into_iter().find(|m| m.id == monitor_id))
            .map(|m| {
                (
                    m.x as f32 + region.x + region.width * 0.5,
                    m.y as f32 + region.y + region.height * 0.5,
                )
            });

        let snapshot = Arc::new(Mutex::new(Arc::new(ScrollSnapshot::default())));
        let (cmd_tx, cmd_rx) = channel();
        let (ready_tx, ready_rx) = sync_channel::<std::result::Result<(), String>>(1);

        let snap_clone = snapshot.clone();
        let thread = thread::Builder::new()
            .name("snapit-scroll".into())
            .spawn(move || {
                capture_loop(
                    monitor_id, region, center, options, cmd_rx, snap_clone, ready_tx,
                )
            })?;

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if start.cancelled.load(Ordering::Relaxed) {
                let _ = cmd_tx.send(ScrollCommand::Discard);
                let _ = thread.join();
                anyhow::bail!("scroll capture cancelled");
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = cmd_tx.send(ScrollCommand::Discard);
                let _ = thread.join();
                anyhow::bail!("the platform recorder delivered no frame within 5 s");
            }
            match ready_rx.recv_timeout(remaining.min(Duration::from_millis(10))) {
                Ok(Ok(())) => {
                    if start.cancelled.load(Ordering::Relaxed) {
                        let _ = cmd_tx.send(ScrollCommand::Discard);
                        let _ = thread.join();
                        anyhow::bail!("scroll capture cancelled");
                    }
                    break;
                }
                Ok(Err(err)) => {
                    let _ = thread.join();
                    return Err(anyhow!(err));
                }
                Err(_) => {}
            }
        }

        Ok(Self {
            cmd_tx,
            snapshot,
            thread: Some(thread),
        })
    }

    pub fn snapshot(&self) -> Arc<ScrollSnapshot> {
        self.snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub fn set_auto_scroll(&self, enabled: bool) -> std::result::Result<(), AutoScrollError> {
        self.set_auto_scroll_direction(enabled, self.snapshot().auto_direction)
    }

    pub fn set_auto_scroll_direction(
        &self,
        enabled: bool,
        direction: ScrollDirection,
    ) -> std::result::Result<(), AutoScrollError> {
        if enabled && !scroller::auto_scroll_available() {
            return Err(AutoScrollError::NeedsAccessibility);
        }
        let _ = self.cmd_tx.send(ScrollCommand::SetAutoScroll {
            enabled,
            direction: Some(direction),
        });
        Ok(())
    }

    pub fn set_paused(&self, paused: bool) {
        let _ = self.cmd_tx.send(ScrollCommand::SetPaused(paused));
    }

    pub fn set_preview_range(&self, start: u32, rows: u32) {
        self.set_preview_range_with_follow(start, rows, false);
    }

    pub fn set_preview_range_with_follow(&self, start: u32, rows: u32, follow: bool) {
        let _ = self.cmd_tx.send(ScrollCommand::SetPreviewRange {
            start,
            rows,
            follow,
        });
    }

    pub fn set_annotation_origin(&self, origin: i64) {
        let _ = self.cmd_tx.send(ScrollCommand::SetAnnotationOrigin(origin));
    }

    pub fn reset(&self) {
        let _ = self.cmd_tx.send(ScrollCommand::Reset);
    }

    pub fn finish(mut self) -> Result<Option<(DiskImage, u32)>> {
        let _ = self.cmd_tx.send(ScrollCommand::Finish);
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| anyhow!("scroll capture thread crashed"))?
        } else {
            Ok(None)
        }
    }

    pub fn discard(mut self) {
        let _ = self.cmd_tx.send(ScrollCommand::Discard);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ScrollSession {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = self.cmd_tx.send(ScrollCommand::Discard);
            // Detach rather than blocking a caller on a platform capture stop.
            drop(thread);
        }
    }
}

const AUTO_SCROLL_WARNING_RETRIES: u32 = 3;
const AUTO_SCROLL_REVERSE_DISTANCE: u32 = 48;
const AUTO_SCROLL_REVERSE_FRAMES: u32 = 2;

struct QueuedFrame {
    image: Arc<RgbaImage>,
    grid: [u32; 64],
    captured_at: Instant,
}

struct FrameQueue {
    frames: VecDeque<QueuedFrame>,
    capacity: usize,
    disconnected: bool,
    stop: bool,
    dropped: u64,
    max_depth: usize,
}

impl FrameQueue {
    fn new() -> Self {
        Self {
            frames: VecDeque::with_capacity(MIN_FRAME_BUFFER_CAPACITY),
            capacity: MIN_FRAME_BUFFER_CAPACITY,
            disconnected: false,
            stop: false,
            dropped: 0,
            max_depth: 0,
        }
    }

    fn push(&mut self, image: Arc<RgbaImage>, grid: [u32; 64], captured_at: Instant) {
        let frame_bytes = image.as_raw().len().max(1);
        self.capacity = (FRAME_BUFFER_BUDGET_BYTES / frame_bytes)
            .clamp(MIN_FRAME_BUFFER_CAPACITY, MAX_FRAME_BUFFER_CAPACITY);
        self.frames.push_back(QueuedFrame {
            image,
            grid,
            captured_at,
        });
        while self.frames.len() > self.capacity {
            let len = self.frames.len();
            let mut drop_idx = 1usize;
            let mut min_span = Duration::MAX;
            for i in 1..len - 1 {
                let span = self.frames[i + 1]
                    .captured_at
                    .saturating_duration_since(self.frames[i - 1].captured_at);
                if span < min_span {
                    min_span = span;
                    drop_idx = i;
                }
            }
            self.frames.remove(drop_idx);
            self.dropped += 1;
        }
        self.max_depth = self.max_depth.max(self.frames.len());
    }
}

#[allow(clippy::large_enum_variant)]
enum QueuePop {
    Frame {
        image: Arc<RgbaImage>,
        grid: [u32; 64],
        queue_empty: bool,
        dropped: u64,
        max_depth: usize,
    },
    Timeout,
    Disconnected,
}

fn pop_frame(queue: &(Mutex<FrameQueue>, Condvar), timeout: Duration) -> QueuePop {
    let mut guard = queue.0.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(item) = guard.frames.pop_front() {
        return QueuePop::Frame {
            image: item.image,
            grid: item.grid,
            queue_empty: guard.frames.is_empty(),
            dropped: guard.dropped,
            max_depth: guard.max_depth,
        };
    }
    if guard.disconnected {
        return QueuePop::Disconnected;
    }
    let (mut guard, _) = queue
        .1
        .wait_timeout(guard, timeout)
        .unwrap_or_else(|p| p.into_inner());
    if let Some(item) = guard.frames.pop_front() {
        QueuePop::Frame {
            image: item.image,
            grid: item.grid,
            queue_empty: guard.frames.is_empty(),
            dropped: guard.dropped,
            max_depth: guard.max_depth,
        }
    } else if guard.disconnected {
        QueuePop::Disconnected
    } else {
        QueuePop::Timeout
    }
}

fn queue_stats(queue: &(Mutex<FrameQueue>, Condvar)) -> (usize, u64, usize) {
    let guard = queue.0.lock().unwrap_or_else(|p| p.into_inner());
    (guard.frames.len(), guard.dropped, guard.max_depth)
}

struct GrabGuard {
    queue: Arc<(Mutex<FrameQueue>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl GrabGuard {
    fn stop_and_join(&mut self) {
        {
            let mut guard = self.queue.0.lock().unwrap_or_else(|p| p.into_inner());
            guard.stop = true;
            self.queue.1.notify_all();
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for GrabGuard {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AutoScrollSettlement {
    Continue,
    ReachedBoundary,
    StopWithoutBoundary,
}

struct AutoScrollState {
    enabled: bool,
    direction: ScrollDirection,
    anchor: (f32, f32),
    next_pulse_at: Instant,
    next_eval_at: Instant,
    pause_pulses_until: Instant,
    grace_until: Instant,
    gain: f32,
    rewinding: bool,
    rewind_steps: u32,
    recovered_from_rewind: bool,
    stalls: u32,
    warning_retries: u32,
    window_displacement: i32,
    window_max_frame_dy: u32,
    reverse_distance: u32,
    reverse_frames: u32,
    boundary_grace_steps: u32,
}

impl AutoScrollState {
    fn new(center: (f32, f32)) -> Self {
        let now = Instant::now();
        Self {
            enabled: false,
            direction: ScrollDirection::Down,
            anchor: center,
            next_pulse_at: now,
            next_eval_at: now,
            pause_pulses_until: now,
            grace_until: now,
            gain: 1.0,
            rewinding: false,
            rewind_steps: 0,
            recovered_from_rewind: false,
            stalls: 0,
            warning_retries: 0,
            window_displacement: 0,
            window_max_frame_dy: 0,
            reverse_distance: 0,
            reverse_frames: 0,
            boundary_grace_steps: 0,
        }
    }

    fn reset_step(&mut self) {
        self.stalls = 0;
        self.warning_retries = 0;
        self.gain = 1.0;
        self.rewinding = false;
        self.rewind_steps = 0;
        self.recovered_from_rewind = false;
        self.boundary_grace_steps = 0;
        self.begin_step();
    }

    fn begin_step(&mut self) {
        self.window_displacement = 0;
        self.window_max_frame_dy = 0;
        self.reverse_distance = 0;
        self.reverse_frames = 0;
    }

    fn observe_motion(&mut self, dy: i32, observed: ScrollDirection) -> bool {
        self.window_displacement = self.window_displacement.saturating_add(dy);
        self.window_max_frame_dy = self.window_max_frame_dy.max(dy.unsigned_abs());
        if !self.enabled {
            self.direction = observed;
            return false;
        }
        if self.rewinding {
            // Same-direction momentum can still arrive right after entering
            // rewind; wait for actual reverse motion before recovering.
            if observed == self.direction {
                return false;
            }
            self.rewinding = false;
            self.rewind_steps = 0;
            self.warning_retries = 0;
            self.recovered_from_rewind = true;
            self.reverse_distance = 0;
            self.reverse_frames = 0;
            return false;
        }
        if observed == self.direction {
            self.reverse_distance = 0;
            self.reverse_frames = 0;
            return false;
        }
        self.reverse_distance = self.reverse_distance.saturating_add(dy.unsigned_abs());
        self.reverse_frames = self.reverse_frames.saturating_add(1);
        self.reverse_distance >= AUTO_SCROLL_REVERSE_DISTANCE
            && self.reverse_frames >= AUTO_SCROLL_REVERSE_FRAMES
    }

    fn settle(
        &mut self,
        warning: Option<ScrollWarning>,
        active_h: u32,
        target_px: f32,
    ) -> AutoScrollSettlement {
        if self.recovered_from_rewind {
            self.recovered_from_rewind = false;
            self.stalls = 0;
            return AutoScrollSettlement::Continue;
        }
        if warning.is_none() && self.boundary_grace_steps > 0 {
            self.boundary_grace_steps -= 1;
            self.stalls = 0;
            return AutoScrollSettlement::Continue;
        }
        if !self.rewinding && self.window_displacement.unsigned_abs() >= 2 {
            self.stalls = 0;
            self.warning_retries = 0;
            if active_h > 0 {
                if self.window_max_frame_dy as f32 > active_h as f32 * 0.50 {
                    self.gain = (self.gain * 0.65).clamp(0.10, 4.5);
                } else {
                    let actual = self.window_displacement.unsigned_abs() as f32;
                    let ratio = target_px.max(32.0) / actual;
                    self.gain =
                        (self.gain * (0.55 + 0.45 * ratio.clamp(0.4, 2.0))).clamp(0.10, 4.5);
                }
            }
            return AutoScrollSettlement::Continue;
        }
        self.stalls = self.stalls.saturating_add(1);
        match warning {
            Some(ScrollWarning::TooFast) => {
                if !self.rewinding {
                    self.gain = (self.gain * 0.5).max(0.10);
                    self.rewinding = true;
                    self.rewind_steps = 1;
                    AutoScrollSettlement::Continue
                } else if self.rewind_steps < AUTO_SCROLL_REWIND_LIMIT {
                    self.rewind_steps += 1;
                    AutoScrollSettlement::Continue
                } else {
                    self.rewinding = false;
                    AutoScrollSettlement::StopWithoutBoundary
                }
            }
            Some(ScrollWarning::LowTexture) => {
                if self.warning_retries < AUTO_SCROLL_WARNING_RETRIES {
                    self.warning_retries += 1;
                    self.gain = (self.gain * 0.6).max(0.10);
                    AutoScrollSettlement::Continue
                } else {
                    AutoScrollSettlement::StopWithoutBoundary
                }
            }
            None if self.stalls >= AUTO_SCROLL_STALL_LIMIT => AutoScrollSettlement::ReachedBoundary,
            None => AutoScrollSettlement::Continue,
        }
    }

    fn brake_for_too_fast(&mut self, now: Instant, already_too_fast: bool) {
        if self.enabled && !self.rewinding && !already_too_fast {
            let brake = now + Duration::from_millis(90);
            self.pause_pulses_until = brake;
            self.next_eval_at = brake;
        }
    }

    fn stop(&mut self) {
        self.enabled = false;
        self.rewinding = false;
        self.rewind_steps = 0;
        self.recovered_from_rewind = false;
    }
}

fn capture_loop(
    monitor_id: u32,
    region: Region,
    center: Option<(f32, f32)>,
    options: ScrollOptions,
    cmd_rx: Receiver<ScrollCommand>,
    snapshot_slot: Arc<Mutex<Arc<ScrollSnapshot>>>,
    ready_tx: std::sync::mpsc::SyncSender<std::result::Result<(), String>>,
) -> Result<Option<(DiskImage, u32)>> {
    let queue = Arc::new((Mutex::new(FrameQueue::new()), Condvar::new()));
    let (grab_ready_tx, grab_ready_rx) = sync_channel::<std::result::Result<(), String>>(1);
    let grab_queue = queue.clone();
    let grab_fps = options.fps;
    let grab_thread = thread::Builder::new()
        .name("snapit-scroll-grab".into())
        .spawn(move || {
            let live = match LiveRecorder::start_with(
                monitor_id,
                region,
                grab_fps,
                false,
                OutputSize::Native,
                DynamicRange::Sdr,
            ) {
                Ok(live) => {
                    let _ = grab_ready_tx.send(Ok(()));
                    live
                }
                Err(err) => {
                    let _ = grab_ready_tx.send(Err(format!("{err:#}")));
                    let mut guard = grab_queue.0.lock().unwrap_or_else(|p| p.into_inner());
                    guard.disconnected = true;
                    grab_queue.1.notify_all();
                    return;
                }
            };
            let mut last_grid: Option<[u32; 64]> = None;
            let mut last_image: Option<Arc<RgbaImage>> = None;
            loop {
                if grab_queue.0.lock().unwrap_or_else(|p| p.into_inner()).stop {
                    break;
                }
                match live.recv_timeout(Duration::from_millis(8)) {
                    LiveFrameWait::Frame(frame) => {
                        let LiveFrameData::Sdr(img) = frame.data else {
                            continue;
                        };
                        let (w, h) = img.dimensions();
                        if w == 0 || h == 0 {
                            continue;
                        }
                        let grid = stitch::sample_grid(img.as_raw(), w as usize, h as usize);
                        if last_grid == Some(grid)
                            && last_image
                                .as_ref()
                                .is_some_and(|prev| prev.as_raw() == img.as_raw())
                        {
                            continue;
                        }
                        let shared = Arc::new(img);
                        last_grid = Some(grid);
                        last_image = Some(shared.clone());
                        let mut guard = grab_queue.0.lock().unwrap_or_else(|p| p.into_inner());
                        if guard.stop {
                            break;
                        }
                        guard.push(shared, grid, Instant::now());
                        grab_queue.1.notify_one();
                    }
                    LiveFrameWait::Timeout => {}
                    LiveFrameWait::Disconnected => {
                        let mut guard = grab_queue.0.lock().unwrap_or_else(|p| p.into_inner());
                        guard.disconnected = true;
                        grab_queue.1.notify_all();
                        break;
                    }
                }
            }
            let _ = live.stop();
        })?;

    let mut grab = GrabGuard {
        queue: queue.clone(),
        thread: Some(grab_thread),
    };

    match grab_ready_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(msg)) => {
            grab.stop_and_join();
            let _ = ready_tx.send(Err(msg.clone()));
            return Err(anyhow!(msg));
        }
        Err(_) => {
            grab.stop_and_join();
            let msg = "scroll grab thread stopped before initialization".to_string();
            let _ = ready_tx.send(Err(msg.clone()));
            return Err(anyhow!(msg));
        }
    }

    let stitch_cfg = StitchConfig::default().with_scale(options.scale);
    let mut stitcher = Stitcher::new(stitch_cfg);
    let mut status;
    let mut warning: Option<ScrollWarning> = None;
    let mut paused = false;
    let mut auto = AutoScrollState::new(center.unwrap_or((0.0, 0.0)));
    let mut last_activity = Instant::now();
    let mut first_frame_ready = false;
    let mut input_guard: Option<scroller::ScrollInputGuard> = None;

    // Wait briefly for the first live frame; on Windows DXGI an idle screen
    // may not deliver a frame immediately, so fall back once to RegionCapturer.
    let initial_wait_deadline = Instant::now() + Duration::from_millis(250);
    let mut next_fallback_at = initial_wait_deadline;
    let mut fallback_delay = Duration::from_millis(50);
    while !first_frame_ready {
        if let Ok(cmd) = cmd_rx.try_recv()
            && matches!(cmd, ScrollCommand::Discard | ScrollCommand::Finish)
        {
            grab.stop_and_join();
            let _ = ready_tx.send(Ok(()));
            return Ok(None);
        }
        match pop_frame(&queue, Duration::from_millis(16)) {
            QueuePop::Frame { image, .. } => {
                if let Err(err) = accept_initial_frame(&mut stitcher, image) {
                    grab.stop_and_join();
                    let _ = ready_tx.send(Err(format!("{err:#}")));
                    return Err(err);
                }
                first_frame_ready = true;
            }
            QueuePop::Timeout => {
                let now = Instant::now();
                if now >= initial_wait_deadline && now >= next_fallback_at {
                    match RegionCapturer::new(monitor_id, region)
                        .and_then(|capturer| capturer.capture())
                    {
                        Ok(img) => {
                            if let Err(err) = accept_initial_frame(&mut stitcher, Arc::new(img)) {
                                grab.stop_and_join();
                                let _ = ready_tx.send(Err(format!("{err:#}")));
                                return Err(err);
                            }
                            first_frame_ready = true;
                        }
                        Err(err) => {
                            log::warn!("scroll initial fallback capture failed: {err:#}");
                            fallback_delay = (fallback_delay * 2).min(Duration::from_secs(1));
                            next_fallback_at = now + fallback_delay;
                        }
                    }
                }
            }
            QueuePop::Disconnected => break,
        }
    }

    if !first_frame_ready {
        grab.stop_and_join();
        let msg = "scroll capture received no initial frame".to_string();
        let _ = ready_tx.send(Err(msg.clone()));
        return Err(anyhow!(msg));
    }

    status = ScrollStatus::Capturing;
    let mut pause_frame: Option<(Arc<RgbaImage>, i64)> = None;
    let mut pause_revision = 0u64;
    stitcher.set_preview_range_with_follow(0, stitcher.total_height().saturating_mul(3), true);
    if let Err(err) = stitcher.refresh_preview() {
        grab.stop_and_join();
        let _ = ready_tx.send(Err(format!("{err:#}")));
        return Err(err);
    }
    publish_snapshot(
        &snapshot_slot,
        &stitcher,
        status,
        warning,
        &auto,
        paused,
        (pause_frame.as_ref(), pause_revision),
    );
    let _ = ready_tx.send(Ok(()));
    let display_hz = snapit_capture::monitor_refresh_rate(monitor_id);
    log::info!(
        "scroll capture started: {}x{} fps={} display_hz={:?} speed={:?}",
        stitcher.width(),
        stitcher.total_height(),
        options.fps,
        display_hz,
        options.auto_scroll_speed
    );

    macro_rules! capture_step {
        ($result:expr) => {
            match $result {
                Ok(value) => value,
                Err(error) => {
                    auto.stop();
                    if let Some(guard) = input_guard.as_mut() {
                        guard.restore();
                    }
                    grab.stop_and_join();
                    publish_failure(
                        &snapshot_slot,
                        &stitcher,
                        &auto,
                        paused,
                        (pause_frame.as_ref(), pause_revision),
                        &error,
                    );
                    return Err(error);
                }
            }
        };
    }

    let mut finish_requested = false;
    let mut discard_requested = false;
    let mut pending_preview_range: Option<(u32, u32, bool)> = None;
    let mut pending_annotation_origin: Option<i64> = None;
    let mut last_frame: Option<Arc<RgbaImage>> = None;
    let mut last_preview_at = Instant::now();

    loop {
        let mut commands_open = true;
        while commands_open {
            match cmd_rx.try_recv() {
                Ok(cmd) => match cmd {
                    ScrollCommand::SetAutoScroll { enabled, direction } => {
                        if let Some(direction) = direction {
                            auto.direction = direction;
                        }
                        if enabled {
                            let guard =
                                input_guard.get_or_insert_with(scroller::ScrollInputGuard::new);
                            if let Some((cx, cy)) = center {
                                guard.move_to(cx, cy);
                                auto.anchor = scroller::cursor_position().unwrap_or((cx, cy));
                            } else if let Some(pos) = scroller::cursor_position() {
                                auto.anchor = pos;
                            }
                            auto.enabled = true;
                            auto.reset_step();
                            auto.boundary_grace_steps = 2;
                            let now = Instant::now();
                            let eval_window = options
                                .auto_scroll_speed
                                .plan_continuous(
                                    stitcher.active_height(),
                                    options.scale,
                                    auto.gain,
                                    false,
                                )
                                .eval_window;
                            auto.next_pulse_at = now + Duration::from_millis(60);
                            auto.pause_pulses_until = now;
                            auto.next_eval_at = now + Duration::from_millis(60) + eval_window;
                            auto.grace_until = now + Duration::from_millis(150);
                            warning = None;
                            paused = false;
                            status = ScrollStatus::AutoScrolling;
                            log::info!(
                                "scroll auto-scroll enabled direction={:?} at {:?}",
                                auto.direction,
                                auto.anchor
                            );
                        } else {
                            auto.stop();
                            if let Some(guard) = input_guard.as_mut() {
                                guard.restore();
                            }
                            if matches!(
                                status,
                                ScrollStatus::AutoScrolling | ScrollStatus::ReachedEnd
                            ) {
                                status = ScrollStatus::Capturing;
                            }
                            log::info!("scroll auto-scroll disabled");
                        }
                        publish_snapshot(
                            &snapshot_slot,
                            &stitcher,
                            status,
                            warning,
                            &auto,
                            paused,
                            (pause_frame.as_ref(), pause_revision),
                        );
                    }
                    ScrollCommand::SetPaused(p) => {
                        paused = p;
                        if paused {
                            auto.stop();
                            if let Some(guard) = input_guard.as_mut() {
                                guard.restore();
                            }
                            capture_step!(stitcher.flush_pending());
                            capture_step!(stitcher.refresh_preview());
                            last_preview_at = Instant::now();
                            pause_frame = capture_step!(stitcher.annotation_frame())
                                .map(|(image, origin)| (Arc::new(image), origin));
                            pause_revision = pause_revision.wrapping_add(1);
                            status = ScrollStatus::Paused;
                        } else {
                            pause_frame = None;
                            pending_annotation_origin = None;
                            status = ScrollStatus::Capturing;
                        }
                        publish_snapshot(
                            &snapshot_slot,
                            &stitcher,
                            status,
                            warning,
                            &auto,
                            paused,
                            (pause_frame.as_ref(), pause_revision),
                        );
                    }
                    ScrollCommand::SetPreviewRange {
                        start,
                        rows,
                        follow,
                    } => {
                        pending_preview_range = Some((start, rows, follow));
                    }
                    ScrollCommand::SetAnnotationOrigin(origin) => {
                        pending_annotation_origin = Some(origin);
                    }
                    ScrollCommand::Reset => {
                        pending_preview_range = None;
                        pending_annotation_origin = None;
                        auto.stop();
                        auto.direction = ScrollDirection::Down;
                        auto.reset_step();
                        if let Some(guard) = input_guard.as_mut() {
                            guard.restore();
                        }
                        warning = None;
                        paused = false;
                        pause_frame = None;
                        status = ScrollStatus::Capturing;
                        stitcher = Stitcher::new(stitch_cfg);
                        if let Some(frame) = last_frame.clone() {
                            stitcher.feed_shared(frame, true);
                        }
                        stitcher.set_preview_range_with_follow(
                            0,
                            stitcher.total_height().saturating_mul(3),
                            true,
                        );
                        let _ = stitcher.refresh_preview();
                        last_preview_at = Instant::now();
                        publish_snapshot(
                            &snapshot_slot,
                            &stitcher,
                            status,
                            warning,
                            &auto,
                            false,
                            (None, pause_revision),
                        );
                        log::info!("scroll stitcher reset");
                    }
                    ScrollCommand::Finish => finish_requested = true,
                    ScrollCommand::Discard => discard_requested = true,
                },
                Err(std::sync::mpsc::TryRecvError::Disconnected) => commands_open = false,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
            }
        }

        if let Some((start, rows, follow)) = pending_preview_range.take()
            && !finish_requested
            && !discard_requested
        {
            stitcher.set_preview_range_with_follow(start, rows, follow);
            capture_step!(stitcher.refresh_preview());
            last_preview_at = Instant::now();
            publish_snapshot(
                &snapshot_slot,
                &stitcher,
                status,
                warning,
                &auto,
                paused,
                (pause_frame.as_ref(), pause_revision),
            );
        }

        if let Some(origin) = pending_annotation_origin.take()
            && paused
            && pause_frame.is_some()
            && !finish_requested
            && !discard_requested
        {
            pause_frame = capture_step!(stitcher.annotation_frame_at(origin))
                .map(|(image, origin)| (Arc::new(image), origin));
            pause_revision = pause_revision.wrapping_add(1);
            publish_snapshot(
                &snapshot_slot,
                &stitcher,
                status,
                warning,
                &auto,
                paused,
                (pause_frame.as_ref(), pause_revision),
            );
        }

        if !commands_open {
            auto.enabled = false;
            if let Some(guard) = input_guard.as_mut() {
                guard.restore();
            }
            finish_requested = true;
        }

        if discard_requested {
            grab.stop_and_join();
            return Ok(None);
        }
        if finish_requested {
            break;
        }

        if auto.enabled {
            let now = Instant::now();
            if now >= auto.grace_until && scroller::user_intervened(auto.anchor) {
                auto.stop();
                if let Some(guard) = input_guard.as_mut() {
                    guard.restore();
                }
                status = ScrollStatus::StoppedByUser;
                log::info!("scroll auto-scroll stopped by user intervention");
                publish_snapshot(
                    &snapshot_slot,
                    &stitcher,
                    status,
                    warning,
                    &auto,
                    paused,
                    (pause_frame.as_ref(), pause_revision),
                );
            } else {
                let (queue_len, _, _) = queue_stats(&queue);
                let queue_empty = queue_len == 0;
                let step = options.auto_scroll_speed.plan_continuous(
                    stitcher.active_height(),
                    options.scale,
                    auto.gain,
                    auto.rewinding,
                );
                if now >= auto.pause_pulses_until && now >= auto.next_pulse_at {
                    if queue_len >= AUTO_SCROLL_QUEUE_BACKPRESSURE {
                        auto.next_pulse_at = now + Duration::from_millis(16);
                    } else {
                        let pulse_dir = if auto.rewinding {
                            auto.direction.opposite()
                        } else {
                            auto.direction
                        };
                        if !scroller::post_scroll_at(
                            pulse_dir,
                            auto.anchor.0,
                            auto.anchor.1,
                            step.mac_px,
                            step.win_notches,
                        ) {
                            auto.stop();
                            if let Some(guard) = input_guard.as_mut() {
                                guard.restore();
                            }
                            status = ScrollStatus::Capturing;
                            publish_snapshot(
                                &snapshot_slot,
                                &stitcher,
                                status,
                                warning,
                                &auto,
                                paused,
                                (pause_frame.as_ref(), pause_revision),
                            );
                        } else {
                            auto.next_pulse_at = now + step.platform_interval();
                        }
                    }
                }
                if auto.enabled
                    && now >= auto.next_eval_at
                    && (queue_empty || now >= auto.next_eval_at + SETTLE_DRAIN_CAP)
                {
                    if (warning.is_some() || auto.window_displacement.unsigned_abs() < 2)
                        && capture_step!(stitcher.flush_pending())
                    {
                        if last_preview_at.elapsed() >= PREVIEW_REFRESH_INTERVAL {
                            capture_step!(stitcher.refresh_preview());
                            last_preview_at = Instant::now();
                        }
                        publish_snapshot(
                            &snapshot_slot,
                            &stitcher,
                            status,
                            warning,
                            &auto,
                            paused,
                            (pause_frame.as_ref(), pause_revision),
                        );
                    }
                    let was_rewinding = auto.rewinding;
                    let target_px = stitcher.active_height() as f32
                        * options.auto_scroll_speed.target_viewports_per_sec()
                        * step.eval_window.as_secs_f32();
                    let settlement = auto.settle(warning, stitcher.active_height(), target_px);
                    match settlement {
                        AutoScrollSettlement::Continue => {
                            if !was_rewinding && auto.rewinding {
                                log::info!(
                                    "scroll auto-scroll entering rewind step {}/{} (gain={:.2})",
                                    auto.rewind_steps,
                                    AUTO_SCROLL_REWIND_LIMIT,
                                    auto.gain
                                );
                            }
                        }
                        AutoScrollSettlement::ReachedBoundary => {
                            auto.stop();
                            if let Some(guard) = input_guard.as_mut() {
                                guard.restore();
                            }
                            status = ScrollStatus::ReachedEnd;
                            log::info!(
                                "scroll auto-scroll reached {:?} boundary (height={})",
                                auto.direction,
                                stitcher.total_height()
                            );
                        }
                        AutoScrollSettlement::StopWithoutBoundary => {
                            auto.stop();
                            if let Some(guard) = input_guard.as_mut() {
                                guard.restore();
                            }
                            status = ScrollStatus::Capturing;
                            log::warn!(
                                "scroll auto-scroll stopped without a confirmed boundary: {:?} (rewinding={was_rewinding}, gain={:.2})",
                                warning,
                                auto.gain
                            );
                        }
                    }
                    publish_snapshot(
                        &snapshot_slot,
                        &stitcher,
                        status,
                        warning,
                        &auto,
                        paused,
                        (pause_frame.as_ref(), pause_revision),
                    );
                    if auto.enabled {
                        auto.begin_step();
                        let next_step = options.auto_scroll_speed.plan_continuous(
                            stitcher.active_height(),
                            options.scale,
                            auto.gain,
                            auto.rewinding,
                        );
                        auto.next_eval_at = now + next_step.eval_window;
                    }
                }
            }
        }

        match pop_frame(&queue, POLL_TIMEOUT) {
            QueuePop::Frame {
                image,
                grid,
                queue_empty,
                dropped,
                max_depth,
            } => {
                last_frame = Some(image.clone());
                if paused {
                    continue;
                }
                let refresh_now =
                    queue_empty && last_preview_at.elapsed() >= PREVIEW_REFRESH_INTERVAL;
                match stitcher.feed_shared_with_grid(image, grid, refresh_now) {
                    FeedOutcome::Initial | FeedOutcome::Unchanged => {}
                    FeedOutcome::Moved { dy } => {
                        if refresh_now {
                            last_preview_at = Instant::now();
                        }
                        last_activity = Instant::now();
                        let was_rewinding = auto.rewinding;
                        let took_over = auto.observe_motion(dy, stitcher.direction());
                        if took_over {
                            auto.stop();
                            if let Some(guard) = input_guard.as_mut() {
                                guard.restore();
                            }
                            status = ScrollStatus::StoppedByUser;
                            log::info!("scroll auto-scroll taken over by manual reverse scrolling");
                        } else if was_rewinding && !auto.rewinding {
                            let resume = Instant::now() + Duration::from_millis(120);
                            auto.pause_pulses_until = resume;
                            auto.next_pulse_at = resume;
                            let eval_window = options
                                .auto_scroll_speed
                                .plan_continuous(
                                    stitcher.active_height(),
                                    options.scale,
                                    auto.gain,
                                    false,
                                )
                                .eval_window;
                            auto.next_eval_at = resume + eval_window;
                            log::info!(
                                "scroll auto-scroll recovered from rewind (dy={dy}, gain={:.2})",
                                auto.gain
                            );
                        } else if matches!(
                            status,
                            ScrollStatus::ReachedEnd | ScrollStatus::StoppedByUser
                        ) {
                            status = ScrollStatus::Capturing;
                        }
                        warning = None;
                        publish_snapshot(
                            &snapshot_slot,
                            &stitcher,
                            status,
                            warning,
                            &auto,
                            paused,
                            (pause_frame.as_ref(), pause_revision),
                        );
                    }
                    FeedOutcome::Stitched { dy, total_height } => {
                        if refresh_now {
                            last_preview_at = Instant::now();
                        }
                        last_activity = Instant::now();
                        let was_rewinding = auto.rewinding;
                        let took_over = auto.observe_motion(dy, stitcher.direction());
                        if took_over {
                            auto.stop();
                            if let Some(guard) = input_guard.as_mut() {
                                guard.restore();
                            }
                            status = ScrollStatus::StoppedByUser;
                            log::info!("scroll auto-scroll taken over by manual reverse scrolling");
                        } else if was_rewinding && !auto.rewinding {
                            let resume = Instant::now() + Duration::from_millis(120);
                            auto.pause_pulses_until = resume;
                            auto.next_pulse_at = resume;
                            let eval_window = options
                                .auto_scroll_speed
                                .plan_continuous(
                                    stitcher.active_height(),
                                    options.scale,
                                    auto.gain,
                                    false,
                                )
                                .eval_window;
                            auto.next_eval_at = resume + eval_window;
                            log::info!(
                                "scroll auto-scroll recovered from rewind (dy={dy}, gain={:.2})",
                                auto.gain
                            );
                        } else if matches!(
                            status,
                            ScrollStatus::ReachedEnd | ScrollStatus::StoppedByUser
                        ) {
                            status = ScrollStatus::Capturing;
                        }
                        warning = None;
                        log::debug!("scroll stitched dy={dy} height={total_height}");
                        publish_snapshot(
                            &snapshot_slot,
                            &stitcher,
                            status,
                            warning,
                            &auto,
                            paused,
                            (pause_frame.as_ref(), pause_revision),
                        );
                    }
                    FeedOutcome::Warning(warn) => {
                        if warn == ScrollWarning::TooFast {
                            auto.brake_for_too_fast(
                                Instant::now(),
                                warning == Some(ScrollWarning::TooFast),
                            );
                        }
                        if warning != Some(warn) {
                            log::info!(
                                "scroll warning={warn:?} active_h={} queue_dropped={dropped} max_queue_depth={max_depth}",
                                stitcher.active_height()
                            );
                            warning = Some(warn);
                            publish_snapshot(
                                &snapshot_slot,
                                &stitcher,
                                status,
                                warning,
                                &auto,
                                paused,
                                (pause_frame.as_ref(), pause_revision),
                            );
                        }
                    }
                    FeedOutcome::DimensionsChanged { expected, actual } => {
                        let error = anyhow!(
                            "scroll frame dimensions changed from {}x{} to {}x{}",
                            expected.0,
                            expected.1,
                            actual.0,
                            actual.1
                        );
                        auto.stop();
                        if let Some(guard) = input_guard.as_mut() {
                            guard.restore();
                        }
                        grab.stop_and_join();
                        publish_failure(
                            &snapshot_slot,
                            &stitcher,
                            &auto,
                            paused,
                            (pause_frame.as_ref(), pause_revision),
                            &error,
                        );
                        return Err(error);
                    }
                    FeedOutcome::Failed(error) => {
                        let error = anyhow!("scroll canvas I/O failed: {error}");
                        auto.stop();
                        if let Some(guard) = input_guard.as_mut() {
                            guard.restore();
                        }
                        grab.stop_and_join();
                        publish_failure(
                            &snapshot_slot,
                            &stitcher,
                            &auto,
                            paused,
                            (pause_frame.as_ref(), pause_revision),
                            &error,
                        );
                        return Err(error);
                    }
                }
                if queue_empty
                    && stitcher.preview_dirty()
                    && last_preview_at.elapsed() >= PREVIEW_REFRESH_INTERVAL
                {
                    capture_step!(stitcher.refresh_preview());
                    last_preview_at = Instant::now();
                    publish_snapshot(
                        &snapshot_slot,
                        &stitcher,
                        status,
                        warning,
                        &auto,
                        paused,
                        (pause_frame.as_ref(), pause_revision),
                    );
                }
            }
            QueuePop::Timeout => {
                let mut changed = false;
                if !paused
                    && last_activity.elapsed() >= IDLE_FLUSH_DELAY
                    && capture_step!(stitcher.flush_pending())
                {
                    changed = true;
                }
                if !paused
                    && stitcher.preview_dirty()
                    && last_preview_at.elapsed() >= PREVIEW_REFRESH_INTERVAL
                {
                    capture_step!(stitcher.refresh_preview());
                    last_preview_at = Instant::now();
                    changed = true;
                }
                if changed {
                    publish_snapshot(
                        &snapshot_slot,
                        &stitcher,
                        status,
                        warning,
                        &auto,
                        paused,
                        (pause_frame.as_ref(), pause_revision),
                    );
                }
            }
            QueuePop::Disconnected => break,
        }
    }

    let (_, dropped, max_depth) = queue_stats(&queue);
    grab.stop_and_join();
    capture_step!(stitcher.flush_pending());
    let content_shift = stitcher.content_shift();
    let image = capture_step!(stitcher.assemble_disk());
    if let Some(img) = &image {
        log::info!(
            "scroll finished: {}x{} frames={} queue_dropped={dropped} max_queue_depth={max_depth}",
            img.width(),
            img.height(),
            stitcher.frames()
        );
    }
    Ok(image.map(|image| (image, content_shift)))
}

fn accept_initial_frame(stitcher: &mut Stitcher, image: Arc<RgbaImage>) -> Result<()> {
    match stitcher.feed_shared(image, true) {
        FeedOutcome::Initial => Ok(()),
        FeedOutcome::Failed(error) => Err(anyhow!("scroll canvas I/O failed: {error}")),
        outcome => Err(anyhow!("unexpected initial frame outcome: {outcome:?}")),
    }
}

fn publish_snapshot(
    slot: &Arc<Mutex<Arc<ScrollSnapshot>>>,
    stitcher: &Stitcher,
    status: ScrollStatus,
    warning: Option<ScrollWarning>,
    auto: &AutoScrollState,
    paused: bool,
    pause: (Option<&(Arc<RgbaImage>, i64)>, u64),
) {
    let (pause_frame, pause_revision) = pause;
    let snap = Arc::new(ScrollSnapshot {
        width: stitcher.width(),
        height: stitcher.total_height(),
        frame_height: stitcher.frame_height(),
        frames: stitcher.frames(),
        started: true,
        auto_scrolling: auto.enabled,
        paused,
        status,
        error: None,
        warning,
        direction: stitcher.direction(),
        auto_direction: auto.direction,
        content_shift: stitcher.content_shift(),
        preview_revision: stitcher.preview_revision(),
        preview: stitcher.preview_tile(),
        preview_range: stitcher.preview_range(),
        viewport_origin_y: pause_frame.map_or(stitcher.viewport_origin_y(), |frame| frame.1),
        pause_frame: pause_frame.map(|frame| frame.0.clone()),
        pause_revision,
    });
    if let Ok(mut guard) = slot.lock() {
        *guard = snap;
    }
}

fn publish_failure(
    slot: &Arc<Mutex<Arc<ScrollSnapshot>>>,
    stitcher: &Stitcher,
    auto: &AutoScrollState,
    paused: bool,
    pause: (Option<&(Arc<RgbaImage>, i64)>, u64),
    error: &anyhow::Error,
) {
    publish_snapshot(
        slot,
        stitcher,
        ScrollStatus::Failed,
        None,
        auto,
        paused,
        pause,
    );
    if let Ok(mut guard) = slot.lock() {
        Arc::make_mut(&mut guard).error = Some(format!("{error:#}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auto() -> AutoScrollState {
        AutoScrollState::new((0.0, 0.0))
    }

    #[test]
    fn background_failure_is_published_to_the_ui() {
        let slot = Arc::new(Mutex::new(Arc::new(ScrollSnapshot::default())));
        let stitcher = Stitcher::new(StitchConfig::default());
        let auto = auto();
        let error = anyhow!("disk full");

        publish_failure(&slot, &stitcher, &auto, false, (None, 0), &error);

        let snapshot = slot.lock().unwrap();
        assert_eq!(snapshot.status, ScrollStatus::Failed);
        assert_eq!(snapshot.error.as_deref(), Some("disk full"));
    }

    #[test]
    fn manual_upward_motion_becomes_the_next_automatic_direction() {
        let mut state = auto();
        assert!(!state.observe_motion(-20, ScrollDirection::Up));
        assert_eq!(state.direction, ScrollDirection::Up);
    }

    #[test]
    fn automatic_rollback_needs_sustained_reverse_motion() {
        let mut state = auto();
        state.enabled = true;
        assert!(!state.observe_motion(-8, ScrollDirection::Up));
        assert!(!state.observe_motion(6, ScrollDirection::Down));
        assert!(!state.observe_motion(-20, ScrollDirection::Up));
        assert!(state.observe_motion(-30, ScrollDirection::Up));
    }

    #[test]
    fn motion_inside_the_existing_canvas_is_not_a_boundary_stall() {
        let mut state = auto();
        state.enabled = true;
        state.observe_motion(-20, ScrollDirection::Up);
        assert_eq!(
            state.settle(None, 500, 190.0),
            AutoScrollSettlement::Continue
        );
        state.observe_motion(-20, ScrollDirection::Up);
        assert_eq!(
            state.settle(None, 500, 190.0),
            AutoScrollSettlement::Continue
        );
        assert_eq!(state.stalls, 0);
    }

    #[test]
    fn cancelling_pixel_jitter_does_not_hide_a_scroll_boundary() {
        let mut state = auto();
        state.enabled = true;
        for expected in 1..=AUTO_SCROLL_STALL_LIMIT {
            state.observe_motion(1, ScrollDirection::Down);
            state.observe_motion(-1, ScrollDirection::Up);
            let outcome = state.settle(None, 500, 190.0);
            assert_eq!(state.stalls, expected);
            if expected == AUTO_SCROLL_STALL_LIMIT {
                assert_eq!(outcome, AutoScrollSettlement::ReachedBoundary);
            } else {
                assert_eq!(outcome, AutoScrollSettlement::Continue);
            }
        }
    }

    #[test]
    fn one_pixel_motion_is_below_the_boundary_noise_floor() {
        let mut state = auto();
        state.enabled = true;
        for expected in 1..=AUTO_SCROLL_STALL_LIMIT {
            state.begin_step();
            state.observe_motion(1, ScrollDirection::Down);
            let outcome = state.settle(None, 500, 190.0);
            assert_eq!(state.stalls, expected);
            if expected == AUTO_SCROLL_STALL_LIMIT {
                assert_eq!(outcome, AutoScrollSettlement::ReachedBoundary);
            } else {
                assert_eq!(outcome, AutoScrollSettlement::Continue);
            }
        }
    }

    #[test]
    fn motion_is_measured_per_pulse_group() {
        let mut state = auto();
        state.enabled = true;
        state.observe_motion(100, ScrollDirection::Down);
        assert_eq!(
            state.settle(None, 500, 190.0),
            AutoScrollSettlement::Continue
        );
        state.begin_step();
        for expected in 1..=AUTO_SCROLL_STALL_LIMIT {
            let outcome = state.settle(None, 500, 190.0);
            if expected == AUTO_SCROLL_STALL_LIMIT {
                assert_eq!(outcome, AutoScrollSettlement::ReachedBoundary);
            } else {
                assert_eq!(outcome, AutoScrollSettlement::Continue);
            }
        }
    }

    #[test]
    fn resumed_auto_scroll_gets_two_settle_grace_steps() {
        let mut state = auto();
        state.enabled = true;
        state.boundary_grace_steps = 2;
        assert_eq!(
            state.settle(None, 500, 190.0),
            AutoScrollSettlement::Continue
        );
        assert_eq!(
            state.settle(None, 500, 190.0),
            AutoScrollSettlement::Continue
        );
        for expected in 1..=AUTO_SCROLL_STALL_LIMIT {
            let outcome = state.settle(None, 500, 190.0);
            if expected == AUTO_SCROLL_STALL_LIMIT {
                assert_eq!(outcome, AutoScrollSettlement::ReachedBoundary);
            } else {
                assert_eq!(outcome, AutoScrollSettlement::Continue);
            }
        }
    }

    #[test]
    fn recovered_motion_resets_warning_backoff() {
        let mut state = auto();
        state.enabled = true;
        state.settle(Some(ScrollWarning::LowTexture), 500, 190.0);
        assert_eq!(state.warning_retries, 1);
        state.observe_motion(190, ScrollDirection::Down);
        assert_eq!(
            state.settle(None, 500, 190.0),
            AutoScrollSettlement::Continue
        );
        assert_eq!(state.warning_retries, 0);
        assert!((state.gain - 0.6).abs() < 1e-4);
    }

    #[test]
    fn warning_recovery_is_bounded_and_never_claims_a_boundary() {
        let mut state = auto();
        state.enabled = true;
        for expected in 1..=AUTO_SCROLL_WARNING_RETRIES {
            assert_eq!(
                state.settle(Some(ScrollWarning::LowTexture), 500, 190.0),
                AutoScrollSettlement::Continue
            );
            assert_eq!(state.warning_retries, expected);
        }
        assert_eq!(
            state.settle(Some(ScrollWarning::LowTexture), 500, 190.0),
            AutoScrollSettlement::StopWithoutBoundary
        );
    }

    #[test]
    fn frame_queue_thins_interior_frames_with_minimum_gap() {
        let mut q = FrameQueue::new();
        // ~400 KiB per frame -> capacity clamps to MAX_FRAME_BUFFER_CAPACITY (64)
        let mk_img = |v: u8| Arc::new(RgbaImage::from_pixel(320, 320, image::Rgba([v, 0, 0, 255])));
        let t0 = Instant::now();
        for i in 0..80u8 {
            q.push(
                mk_img(i),
                [0; 64],
                t0 + Duration::from_millis(u64::from(i) * 16),
            );
        }
        assert_eq!(q.frames.len(), MAX_FRAME_BUFFER_CAPACITY);
        assert_eq!(q.dropped, 16);
        assert_eq!(q.frames.front().unwrap().image.get_pixel(0, 0)[0], 0);
        assert_eq!(q.frames.back().unwrap().image.get_pixel(0, 0)[0], 79);
        // Check maximum gap between remaining frames stayed tightly bounded.
        let mut max_gap = Duration::ZERO;
        for pair in q.frames.make_contiguous().windows(2) {
            max_gap = max_gap.max(pair[1].captured_at - pair[0].captured_at);
        }
        assert!(max_gap <= Duration::from_millis(32), "max_gap={max_gap:?}");
    }

    #[test]
    fn too_fast_triggers_bounded_rewind_and_reduces_gain() {
        let mut state = auto();
        state.enabled = true;
        assert_eq!(
            state.settle(Some(ScrollWarning::TooFast), 500, 190.0),
            AutoScrollSettlement::Continue
        );
        assert!(state.rewinding);
        assert_eq!(state.rewind_steps, 1);
        assert!(state.gain <= 0.5);
        for step in 2..=AUTO_SCROLL_REWIND_LIMIT {
            assert_eq!(
                state.settle(Some(ScrollWarning::TooFast), 500, 190.0),
                AutoScrollSettlement::Continue
            );
            assert!(state.rewinding);
            assert_eq!(state.rewind_steps, step);
        }
        assert_eq!(
            state.settle(Some(ScrollWarning::TooFast), 500, 190.0),
            AutoScrollSettlement::StopWithoutBoundary
        );
        assert!(!state.rewinding);
    }

    #[test]
    fn rewind_recovery_does_not_trigger_manual_takeover_and_resumes_forward() {
        let mut state = auto();
        state.enabled = true;
        state.direction = ScrollDirection::Down;
        state.settle(Some(ScrollWarning::TooFast), 500, 190.0);
        assert!(state.rewinding);
        let gain_after_rewind = state.gain;

        // Recovering via upward motion while rewinding must NOT trigger manual takeover,
        // and same-direction inertia must not end rewind early.
        assert!(!state.observe_motion(60, ScrollDirection::Down));
        assert!(state.rewinding);
        assert!(!state.recovered_from_rewind);

        assert!(!state.observe_motion(-120, ScrollDirection::Up));
        assert!(!state.rewinding);
        assert!(state.recovered_from_rewind);
        assert_eq!(state.direction, ScrollDirection::Down);

        // Settle after rewind keeps the damped gain and continues forward.
        assert_eq!(
            state.settle(None, 500, 190.0),
            AutoScrollSettlement::Continue
        );
        assert!(!state.recovered_from_rewind);
        assert_eq!(state.gain, gain_after_rewind);
    }

    #[test]
    fn repeated_too_fast_warnings_do_not_extend_eval_deadline() {
        let mut state = auto();
        let t0 = Instant::now();
        state.enabled = true;
        state.brake_for_too_fast(t0, false);
        assert_eq!(state.pause_pulses_until, t0 + Duration::from_millis(90));
        assert_eq!(state.next_eval_at, t0 + Duration::from_millis(90));

        state.brake_for_too_fast(t0 + Duration::from_millis(50), true);
        assert_eq!(state.pause_pulses_until, t0 + Duration::from_millis(90));
        assert_eq!(state.next_eval_at, t0 + Duration::from_millis(90));
    }

    #[test]
    fn adaptive_gain_damps_on_overshoot_and_ramps_on_undershoot() {
        let mut state = auto();
        state.enabled = true;
        // Single-frame jump > 50% of active_h -> damps proactively before TooFast.
        state.observe_motion(280, ScrollDirection::Down);
        assert_eq!(
            state.settle(None, 500, 260.0),
            AutoScrollSettlement::Continue
        );
        assert!(state.gain < 0.7, "gain={}", state.gain);

        // Reset and test undershoot (60 px vs 260 px target) -> ramps up.
        state.reset_step();
        state.observe_motion(60, ScrollDirection::Down);
        assert_eq!(
            state.settle(None, 500, 260.0),
            AutoScrollSettlement::Continue
        );
        assert!(state.gain > 1.0, "gain={}", state.gain);
    }
}
