//! Live scrolling screenshot capture session built on `snapit-capture`.

use std::{
    sync::{
        Arc, Mutex,
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

const CAPTURE_FPS: u32 = 24;
const POLL_TIMEOUT: Duration = Duration::from_millis(4);
const IDLE_FLUSH_DELAY: Duration = Duration::from_millis(120);
const AUTO_SCROLL_STALL_LIMIT: u32 = 3;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScrollStatus {
    #[default]
    Starting,
    Capturing,
    AutoScrolling,
    Paused,
    ReachedEnd,
    MaxHeightReached,
    StoppedByUser,
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ScrollOptions {
    pub max_height: u32,
    pub auto_scroll_speed: ScrollSpeed,
    pub fps: u32,
    pub scale: f32,
}

impl Default for ScrollOptions {
    fn default() -> Self {
        Self {
            max_height: 30_000,
            auto_scroll_speed: ScrollSpeed::Medium,
            fps: CAPTURE_FPS,
            scale: 1.0,
        }
    }
}

impl ScrollOptions {
    pub fn with_max_height(mut self, max_height: u32) -> Self {
        self.max_height = max_height.clamp(4_000, 60_000);
        self
    }

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
    pub frames: usize,
    pub started: bool,
    pub auto_scrolling: bool,
    pub paused: bool,
    pub status: ScrollStatus,
    pub warning: Option<ScrollWarning>,
    pub direction: ScrollDirection,
    pub auto_direction: ScrollDirection,
    pub content_shift: u32,
    pub preview_revision: u64,
    pub preview: Option<PreviewTile>,
    pub preview_range: (u32, u32),
}

enum ScrollCommand {
    SetAutoScroll {
        enabled: bool,
        direction: Option<ScrollDirection>,
    },
    SetPaused(bool),
    SetPreviewRange(u32, u32),
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
        let _ = self
            .cmd_tx
            .send(ScrollCommand::SetPreviewRange(start, rows));
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
    pulses_left: u32,
    next_pulse_at: Instant,
    settle_until: Instant,
    grace_until: Instant,
    waiting_settle: bool,
    step_scale: f32,
    stalls: u32,
    warning_retries: u32,
    step_moved: bool,
    reverse_distance: u32,
    reverse_frames: u32,
}

impl AutoScrollState {
    fn new(center: (f32, f32)) -> Self {
        let now = Instant::now();
        Self {
            enabled: false,
            direction: ScrollDirection::Down,
            anchor: center,
            pulses_left: 0,
            next_pulse_at: now,
            settle_until: now,
            grace_until: now,
            waiting_settle: false,
            step_scale: 1.0,
            stalls: 0,
            warning_retries: 0,
            step_moved: false,
            reverse_distance: 0,
            reverse_frames: 0,
        }
    }

    fn reset_step(&mut self) {
        self.pulses_left = 0;
        self.waiting_settle = false;
        self.stalls = 0;
        self.warning_retries = 0;
        self.step_scale = 1.0;
        self.step_moved = false;
        self.reverse_distance = 0;
        self.reverse_frames = 0;
    }

    fn observe_motion(&mut self, dy: i32, observed: ScrollDirection) -> bool {
        self.step_moved = true;
        if !self.enabled {
            self.direction = observed;
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

    fn settle(&mut self, warning: Option<ScrollWarning>) -> AutoScrollSettlement {
        if self.step_moved {
            self.stalls = 0;
            return AutoScrollSettlement::Continue;
        }
        self.stalls = self.stalls.saturating_add(1);
        match warning {
            Some(_) => {
                if self.warning_retries < AUTO_SCROLL_WARNING_RETRIES {
                    self.warning_retries += 1;
                    self.step_scale *= 0.5;
                    AutoScrollSettlement::Continue
                } else {
                    AutoScrollSettlement::StopWithoutBoundary
                }
            }
            None if self.stalls >= AUTO_SCROLL_STALL_LIMIT => AutoScrollSettlement::ReachedBoundary,
            None => AutoScrollSettlement::Continue,
        }
    }

    fn stop(&mut self) {
        self.enabled = false;
        self.pulses_left = 0;
        self.waiting_settle = false;
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
    let live = match LiveRecorder::start_with(
        monitor_id,
        region,
        options.fps,
        false,
        OutputSize::Native,
        DynamicRange::Sdr,
    ) {
        Ok(live) => live,
        Err(err) => {
            let _ = ready_tx.send(Err(format!("{err:#}")));
            return Err(err);
        }
    };

    let stitch_cfg = StitchConfig::default()
        .with_max_height(options.max_height)
        .with_scale(options.scale);
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
            let _ = live.stop();
            let _ = ready_tx.send(Ok(()));
            return Ok(None);
        }
        match live.recv_timeout(Duration::from_millis(16)) {
            LiveFrameWait::Frame(frame) => {
                if let LiveFrameData::Sdr(img) = frame.data {
                    if let Err(err) = accept_initial_frame(&mut stitcher, img) {
                        let _ = live.stop();
                        let _ = ready_tx.send(Err(format!("{err:#}")));
                        return Err(err);
                    }
                    first_frame_ready = true;
                }
            }
            LiveFrameWait::Timeout => {
                let now = Instant::now();
                if now >= initial_wait_deadline && now >= next_fallback_at {
                    match RegionCapturer::new(monitor_id, region)
                        .and_then(|capturer| capturer.capture())
                    {
                        Ok(img) => {
                            if let Err(err) = accept_initial_frame(&mut stitcher, img) {
                                let _ = live.stop();
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
            LiveFrameWait::Disconnected => break,
        }
    }

    if !first_frame_ready {
        let _ = live.stop();
        let msg = "scroll capture received no initial frame".to_string();
        let _ = ready_tx.send(Err(msg.clone()));
        return Err(anyhow!(msg));
    }

    status = ScrollStatus::Capturing;
    stitcher.set_preview_range(0, stitcher.total_height());
    if let Err(err) = stitcher.refresh_preview() {
        let _ = live.stop();
        let _ = ready_tx.send(Err(format!("{err:#}")));
        return Err(err);
    }
    publish_snapshot(&snapshot_slot, &stitcher, status, warning, &auto, paused);
    let _ = ready_tx.send(Ok(()));
    log::info!(
        "scroll capture started: {}x{} max_height={} speed={:?}",
        stitcher.width(),
        stitcher.total_height(),
        options.max_height,
        options.auto_scroll_speed
    );

    let mut finish_requested = false;
    let mut discard_requested = false;
    let mut last_frame: Option<RgbaImage> = None;

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
                            let now = Instant::now();
                            auto.next_pulse_at = now + Duration::from_millis(80);
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
                        publish_snapshot(&snapshot_slot, &stitcher, status, warning, &auto, paused);
                    }
                    ScrollCommand::SetPaused(p) => {
                        paused = p;
                        if paused {
                            auto.stop();
                            if let Some(guard) = input_guard.as_mut() {
                                guard.restore();
                            }
                            status = ScrollStatus::Paused;
                        } else {
                            status = ScrollStatus::Capturing;
                        }
                        publish_snapshot(&snapshot_slot, &stitcher, status, warning, &auto, paused);
                    }
                    ScrollCommand::SetPreviewRange(start, rows) => {
                        stitcher.set_preview_range(start, rows);
                        stitcher.refresh_preview()?;
                        publish_snapshot(&snapshot_slot, &stitcher, status, warning, &auto, paused);
                    }
                    ScrollCommand::Reset => {
                        auto.stop();
                        auto.direction = ScrollDirection::Down;
                        auto.reset_step();
                        if let Some(guard) = input_guard.as_mut() {
                            guard.restore();
                        }
                        warning = None;
                        paused = false;
                        status = ScrollStatus::Capturing;
                        stitcher = Stitcher::new(stitch_cfg);
                        if let Some(frame) = last_frame.clone() {
                            stitcher.feed(frame);
                        }
                        publish_snapshot(&snapshot_slot, &stitcher, status, warning, &auto, false);
                        log::info!("scroll stitcher reset");
                    }
                    ScrollCommand::Finish => finish_requested = true,
                    ScrollCommand::Discard => discard_requested = true,
                },
                Err(std::sync::mpsc::TryRecvError::Disconnected) => commands_open = false,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
            }
        }

        if !commands_open {
            auto.enabled = false;
            if let Some(guard) = input_guard.as_mut() {
                guard.restore();
            }
            finish_requested = true;
        }

        if discard_requested {
            let _ = live.stop();
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
                publish_snapshot(&snapshot_slot, &stitcher, status, warning, &auto, paused);
            } else {
                let (pulses, px_per_pulse, notches_per_pulse, settle) =
                    options.auto_scroll_speed.profile();
                if auto.pulses_left > 0 && now >= auto.next_pulse_at {
                    let px = ((px_per_pulse as f32 * auto.step_scale).round() as i32).max(16);
                    let notches =
                        ((notches_per_pulse as f32 * auto.step_scale).round() as i32).max(1);
                    if !scroller::post_scroll_at(
                        auto.direction,
                        auto.anchor.0,
                        auto.anchor.1,
                        px,
                        notches,
                    ) {
                        auto.stop();
                        if let Some(guard) = input_guard.as_mut() {
                            guard.restore();
                        }
                        status = ScrollStatus::Capturing;
                    } else {
                        auto.pulses_left -= 1;
                        auto.next_pulse_at = now + Duration::from_millis(24);
                        if auto.pulses_left == 0 {
                            auto.waiting_settle = true;
                            auto.settle_until = now + settle;
                        }
                    }
                } else if auto.waiting_settle && now >= auto.settle_until {
                    auto.waiting_settle = false;
                    if stitcher.flush_pending()? {
                        publish_snapshot(&snapshot_slot, &stitcher, status, warning, &auto, paused);
                    }
                    match auto.settle(warning) {
                        AutoScrollSettlement::Continue => {}
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
                                "scroll auto-scroll stopped without a confirmed boundary: {:?}",
                                warning
                            );
                        }
                    }
                    publish_snapshot(&snapshot_slot, &stitcher, status, warning, &auto, paused);
                    if auto.enabled {
                        auto.pulses_left = pulses;
                        auto.next_pulse_at = now;
                    }
                } else if !auto.waiting_settle && auto.pulses_left == 0 && now >= auto.next_pulse_at
                {
                    auto.step_moved = false;
                    auto.reverse_distance = 0;
                    auto.reverse_frames = 0;
                    auto.pulses_left = pulses;
                    auto.next_pulse_at = now;
                }
            }
        }

        match live.recv_timeout(POLL_TIMEOUT) {
            LiveFrameWait::Frame(frame) => {
                let LiveFrameData::Sdr(img) = frame.data else {
                    continue;
                };
                last_frame = Some(img.clone());
                if paused {
                    continue;
                }
                match stitcher.feed(img) {
                    FeedOutcome::Initial | FeedOutcome::Unchanged => {}
                    FeedOutcome::Moved { dy } => {
                        last_activity = Instant::now();
                        let took_over = auto.observe_motion(dy, stitcher.direction());
                        if took_over {
                            auto.stop();
                            if let Some(guard) = input_guard.as_mut() {
                                guard.restore();
                            }
                            status = ScrollStatus::StoppedByUser;
                            log::info!("scroll auto-scroll taken over by manual reverse scrolling");
                        } else if matches!(
                            status,
                            ScrollStatus::ReachedEnd | ScrollStatus::StoppedByUser
                        ) {
                            status = ScrollStatus::Capturing;
                        }
                        warning = None;
                        publish_snapshot(&snapshot_slot, &stitcher, status, warning, &auto, paused);
                    }
                    FeedOutcome::Stitched { dy, total_height } => {
                        last_activity = Instant::now();
                        let took_over = auto.observe_motion(dy, stitcher.direction());
                        if took_over {
                            auto.stop();
                            if let Some(guard) = input_guard.as_mut() {
                                guard.restore();
                            }
                            status = ScrollStatus::StoppedByUser;
                            log::info!("scroll auto-scroll taken over by manual reverse scrolling");
                        } else if matches!(
                            status,
                            ScrollStatus::ReachedEnd | ScrollStatus::StoppedByUser
                        ) {
                            status = ScrollStatus::Capturing;
                        }
                        warning = None;
                        log::debug!("scroll stitched dy={dy} height={total_height}");
                        publish_snapshot(&snapshot_slot, &stitcher, status, warning, &auto, paused);
                    }
                    FeedOutcome::Warning(warn) => {
                        if warning != Some(warn) {
                            warning = Some(warn);
                            publish_snapshot(
                                &snapshot_slot,
                                &stitcher,
                                status,
                                warning,
                                &auto,
                                paused,
                            );
                        }
                    }
                    FeedOutcome::DimensionsChanged { expected, actual } => {
                        let _ = live.stop();
                        return Err(anyhow!(
                            "scroll frame dimensions changed from {}x{} to {}x{}",
                            expected.0,
                            expected.1,
                            actual.0,
                            actual.1
                        ));
                    }
                    FeedOutcome::Failed(error) => {
                        let _ = live.stop();
                        return Err(anyhow!("scroll canvas I/O failed: {error}"));
                    }
                    FeedOutcome::MaxHeightReached => {
                        auto.stop();
                        if let Some(guard) = input_guard.as_mut() {
                            guard.restore();
                        }
                        warning = None;
                        status = ScrollStatus::MaxHeightReached;
                        log::info!("scroll max height reached: {}", stitcher.total_height());
                        publish_snapshot(&snapshot_slot, &stitcher, status, warning, &auto, paused);
                    }
                }
            }
            LiveFrameWait::Timeout => {
                if !paused
                    && last_activity.elapsed() >= IDLE_FLUSH_DELAY
                    && stitcher.flush_pending()?
                {
                    publish_snapshot(&snapshot_slot, &stitcher, status, warning, &auto, paused);
                }
            }
            LiveFrameWait::Disconnected => break,
        }
    }

    let _ = live.stop();
    stitcher.flush_pending()?;
    let content_shift = stitcher.content_shift();
    let image = stitcher.assemble_disk()?;
    if let Some(img) = &image {
        log::info!(
            "scroll finished: {}x{} frames={}",
            img.width(),
            img.height(),
            stitcher.frames()
        );
    }
    Ok(image.map(|image| (image, content_shift)))
}

fn accept_initial_frame(stitcher: &mut Stitcher, image: RgbaImage) -> Result<()> {
    match stitcher.feed(image) {
        FeedOutcome::Initial => Ok(()),
        FeedOutcome::Failed(error) => Err(anyhow!("scroll canvas I/O failed: {error}")),
        FeedOutcome::MaxHeightReached => Err(anyhow!(
            "first frame exceeds the configured scroll height limit"
        )),
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
) {
    let snap = Arc::new(ScrollSnapshot {
        width: stitcher.width(),
        height: stitcher.total_height(),
        frames: stitcher.frames(),
        started: true,
        auto_scrolling: auto.enabled,
        paused,
        status,
        warning,
        direction: stitcher.direction(),
        auto_direction: auto.direction,
        content_shift: stitcher.content_shift(),
        preview_revision: stitcher.preview_revision(),
        preview: stitcher.preview_tile(),
        preview_range: stitcher.preview_range(),
    });
    if let Ok(mut guard) = slot.lock() {
        *guard = snap;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auto() -> AutoScrollState {
        AutoScrollState::new((0.0, 0.0))
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
        assert_eq!(state.settle(None), AutoScrollSettlement::Continue);
        state.observe_motion(-20, ScrollDirection::Up);
        assert_eq!(state.settle(None), AutoScrollSettlement::Continue);
        assert_eq!(state.stalls, 0);
    }

    #[test]
    fn warning_recovery_is_bounded_and_never_claims_a_boundary() {
        let mut state = auto();
        state.enabled = true;
        for expected in 1..=AUTO_SCROLL_WARNING_RETRIES {
            assert_eq!(
                state.settle(Some(ScrollWarning::LowTexture)),
                AutoScrollSettlement::Continue
            );
            assert_eq!(state.warning_retries, expected);
        }
        assert_eq!(
            state.settle(Some(ScrollWarning::LowTexture)),
            AutoScrollSettlement::StopWithoutBoundary
        );
    }
}
