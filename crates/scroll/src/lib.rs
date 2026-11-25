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
    DynamicRange, LiveFrameData, LiveFrameWait, LiveRecorder, OutputSize, Region, RegionCapturer,
    RgbaImage,
};

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
    pub preview_revision: u64,
    pub preview_tiles: Vec<PreviewTile>,
}

enum ScrollCommand {
    SetAutoScroll(bool),
    SetPaused(bool),
    Reset,
    Finish,
    Discard,
}

/// Running scrolling capture session.
pub struct ScrollSession {
    cmd_tx: Sender<ScrollCommand>,
    snapshot: Arc<Mutex<Arc<ScrollSnapshot>>>,
    thread: Option<JoinHandle<Result<Option<RgbaImage>>>>,
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
        if enabled && !scroller::auto_scroll_available() {
            return Err(AutoScrollError::NeedsAccessibility);
        }
        let _ = self.cmd_tx.send(ScrollCommand::SetAutoScroll(enabled));
        Ok(())
    }

    pub fn set_paused(&self, paused: bool) {
        let _ = self.cmd_tx.send(ScrollCommand::SetPaused(paused));
    }

    pub fn reset(&self) {
        let _ = self.cmd_tx.send(ScrollCommand::Reset);
    }

    pub fn finish(mut self) -> Result<Option<RgbaImage>> {
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

struct AutoScrollState {
    enabled: bool,
    anchor: (f32, f32),
    pulses_left: u32,
    next_pulse_at: Instant,
    settle_until: Instant,
    grace_until: Instant,
    waiting_settle: bool,
    step_scale: f32,
    stalls: u32,
    height_at_step: u32,
}

impl AutoScrollState {
    fn new(center: (f32, f32)) -> Self {
        let now = Instant::now();
        Self {
            enabled: false,
            anchor: center,
            pulses_left: 0,
            next_pulse_at: now,
            settle_until: now,
            grace_until: now,
            waiting_settle: false,
            step_scale: 1.0,
            stalls: 0,
            height_at_step: 0,
        }
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
) -> Result<Option<RgbaImage>> {
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

    // Wait briefly for the first live frame; on Windows DXGI an idle screen
    // may not deliver a frame immediately, so fall back once to RegionCapturer.
    let initial_wait_deadline = Instant::now() + Duration::from_millis(250);
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
                    stitcher.feed(img);
                    first_frame_ready = true;
                }
            }
            LiveFrameWait::Timeout => {
                if Instant::now() >= initial_wait_deadline
                    && let Ok(capturer) = RegionCapturer::new(monitor_id, region)
                    && let Ok(img) = capturer.capture()
                {
                    stitcher.feed(img);
                    first_frame_ready = true;
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
    publish_snapshot(&snapshot_slot, &stitcher, status, warning, false, paused);
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
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                ScrollCommand::SetAutoScroll(enable) => {
                    if enable && !auto.enabled {
                        if let Some((cx, cy)) = center {
                            scroller::move_cursor(cx, cy);
                            auto.anchor = scroller::cursor_position().unwrap_or((cx, cy));
                        } else if let Some(pos) = scroller::cursor_position() {
                            auto.anchor = pos;
                        }
                        auto.enabled = true;
                        auto.pulses_left = 0;
                        auto.waiting_settle = false;
                        auto.stalls = 0;
                        auto.step_scale = 1.0;
                        auto.height_at_step = stitcher.total_height();
                        let now = Instant::now();
                        auto.next_pulse_at = now + Duration::from_millis(80);
                        auto.grace_until = now + Duration::from_millis(150);
                        paused = false;
                        status = ScrollStatus::AutoScrolling;
                        log::info!("scroll auto-scroll enabled at {:?}", auto.anchor);
                    } else if !enable && auto.enabled {
                        auto.enabled = false;
                        if status == ScrollStatus::AutoScrolling {
                            status = ScrollStatus::Capturing;
                        }
                        log::info!("scroll auto-scroll disabled");
                    }
                    publish_snapshot(
                        &snapshot_slot,
                        &stitcher,
                        status,
                        warning,
                        auto.enabled,
                        paused,
                    );
                }
                ScrollCommand::SetPaused(p) => {
                    paused = p;
                    if paused {
                        auto.enabled = false;
                        status = ScrollStatus::Paused;
                    } else {
                        status = ScrollStatus::Capturing;
                    }
                    publish_snapshot(
                        &snapshot_slot,
                        &stitcher,
                        status,
                        warning,
                        auto.enabled,
                        paused,
                    );
                }
                ScrollCommand::Reset => {
                    auto.enabled = false;
                    warning = None;
                    paused = false;
                    status = ScrollStatus::Capturing;
                    stitcher = Stitcher::new(stitch_cfg);
                    if let Some(frame) = last_frame.clone() {
                        stitcher.feed(frame);
                    }
                    publish_snapshot(&snapshot_slot, &stitcher, status, warning, false, false);
                    log::info!("scroll stitcher reset");
                }
                ScrollCommand::Finish => finish_requested = true,
                ScrollCommand::Discard => discard_requested = true,
            }
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
                auto.enabled = false;
                status = ScrollStatus::StoppedByUser;
                log::info!("scroll auto-scroll stopped by user intervention");
                publish_snapshot(&snapshot_slot, &stitcher, status, warning, false, paused);
            } else {
                let (pulses, px_per_pulse, notches_per_pulse, settle) =
                    options.auto_scroll_speed.profile();
                if auto.pulses_left > 0 && now >= auto.next_pulse_at {
                    let px = ((px_per_pulse as f32 * auto.step_scale).round() as i32).max(16);
                    let notches =
                        ((notches_per_pulse as f32 * auto.step_scale).round() as i32).max(1);
                    if !scroller::post_scroll_down(px, notches) {
                        auto.enabled = false;
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
                    if stitcher.flush_pending() {
                        publish_snapshot(
                            &snapshot_slot,
                            &stitcher,
                            status,
                            warning,
                            auto.enabled,
                            paused,
                        );
                    }
                    let cur_h = stitcher.total_height();
                    if cur_h == auto.height_at_step {
                        auto.stalls += 1;
                        if auto.stalls >= AUTO_SCROLL_STALL_LIMIT {
                            auto.enabled = false;
                            status = ScrollStatus::ReachedEnd;
                            log::info!("scroll auto-scroll reached end (height={cur_h})");
                            publish_snapshot(
                                &snapshot_slot,
                                &stitcher,
                                status,
                                warning,
                                false,
                                paused,
                            );
                        }
                    } else {
                        auto.stalls = 0;
                        auto.height_at_step = cur_h;
                    }
                    if auto.enabled {
                        auto.pulses_left = pulses;
                        auto.next_pulse_at = now;
                    }
                } else if !auto.waiting_settle && auto.pulses_left == 0 && now >= auto.next_pulse_at
                {
                    auto.height_at_step = stitcher.total_height();
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
                    FeedOutcome::Moved => {
                        last_activity = Instant::now();
                        if warning.take().is_some() {
                            publish_snapshot(
                                &snapshot_slot,
                                &stitcher,
                                status,
                                warning,
                                auto.enabled,
                                paused,
                            );
                        }
                    }
                    FeedOutcome::Stitched { dy, total_height } => {
                        last_activity = Instant::now();
                        warning = None;
                        log::debug!("scroll stitched dy={dy} height={total_height}");
                        publish_snapshot(
                            &snapshot_slot,
                            &stitcher,
                            status,
                            warning,
                            auto.enabled,
                            paused,
                        );
                    }
                    FeedOutcome::Warning(warn) => {
                        if auto.enabled && warn == ScrollWarning::TooFast {
                            auto.step_scale = (auto.step_scale * 0.5).max(0.25);
                        }
                        if warning != Some(warn) {
                            warning = Some(warn);
                            publish_snapshot(
                                &snapshot_slot,
                                &stitcher,
                                status,
                                warning,
                                auto.enabled,
                                paused,
                            );
                        }
                    }
                    FeedOutcome::MaxHeightReached => {
                        auto.enabled = false;
                        warning = None;
                        status = ScrollStatus::MaxHeightReached;
                        log::info!("scroll max height reached: {}", stitcher.total_height());
                        publish_snapshot(&snapshot_slot, &stitcher, status, warning, false, paused);
                    }
                }
            }
            LiveFrameWait::Timeout => {
                if !paused
                    && last_activity.elapsed() >= IDLE_FLUSH_DELAY
                    && stitcher.flush_pending()
                {
                    publish_snapshot(
                        &snapshot_slot,
                        &stitcher,
                        status,
                        warning,
                        auto.enabled,
                        paused,
                    );
                }
            }
            LiveFrameWait::Disconnected => break,
        }
    }

    let _ = live.stop();
    let image = stitcher.assemble();
    if let Some(img) = &image {
        log::info!(
            "scroll finished: {}x{} frames={}",
            img.width(),
            img.height(),
            stitcher.frames()
        );
    }
    Ok(image)
}

fn publish_snapshot(
    slot: &Arc<Mutex<Arc<ScrollSnapshot>>>,
    stitcher: &Stitcher,
    status: ScrollStatus,
    warning: Option<ScrollWarning>,
    auto_scrolling: bool,
    paused: bool,
) {
    let snap = Arc::new(ScrollSnapshot {
        width: stitcher.width(),
        height: stitcher.total_height(),
        frames: stitcher.frames(),
        started: true,
        auto_scrolling,
        paused,
        status,
        warning,
        direction: stitcher.direction(),
        preview_revision: stitcher.preview_revision(),
        preview_tiles: stitcher.preview_tiles(),
    });
    if let Ok(mut guard) = slot.lock() {
        *guard = snap;
    }
}
