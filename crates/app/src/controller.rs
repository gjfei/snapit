//! App-wide workflow: freeze → select → copy / save / recognize / record.
//!
//! The controller is a global entity so the tray, hotkeys, OCR panel, overlays
//! and the recording bar all drive the same state machine.

use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, AsyncApp, Bounds, ClipboardItem, Context, Entity,
    Global, Image, ImageFormat, Pixels, SharedString, Subscription, TitlebarOptions, WeakEntity,
    Window, WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, point, px, size,
};
use snapit_capture::{HdrImage, RgbaImage};
use snapit_ocr::{OcrEngine, OcrOptions};
use snapit_recorder::{RecordFormat, RecordOptions, Recorder};
use snapit_store::{EntryKind, ImageFormat as SaveFormat};

use crate::appearance;
use crate::clipboard;
use crate::history_store::HistoryStore;
use crate::history_view::HistoryView;
use crate::hotkey::HotkeyAction;
use crate::ocr_view::OcrView;
use crate::overlay::OverlayView;
use crate::permission_view::PermissionView;
use crate::permissions::{self, Permission};
use crate::pin;
use crate::recording_bar::{BAR_SIZE, RecordingBar};
use crate::session::{self, CaptureIntent, CaptureSession, SessionCommand, SessionSlot};
use crate::settings::AppSettings;
use crate::settings_view::{self, SettingsView};
use crate::update_view::UpdateView;

const RECORDING_TICK: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, Default)]
pub enum OcrState {
    #[default]
    Idle,
    Running,
    Done {
        text: SharedString,
        engine: &'static str,
    },
    Failed(SharedString),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Busy {
    Capturing,
    Encoding,
}

struct ActiveSelection {
    session: Entity<CaptureSession>,
    /// `slot` keys each overlay to the session frame that fills it.
    overlays: Vec<OverlayWindow>,
    _subscription: Subscription,
}

#[derive(Clone)]
struct OverlayWindow {
    slot: usize,
    window: AnyWindowHandle,
    view: Entity<OverlayView>,
}

struct ActiveRecording {
    /// `None` while the recorder is starting, or after stop while the encoder flushes.
    recorder: Option<Recorder>,
    bar: Option<AnyWindowHandle>,
    /// The recorded display's overlay, kept on screen as the region highlight.
    frame: Option<AnyWindowHandle>,
    /// The recorder has produced its first live frame and is capturing.
    started: bool,
    /// Kept here so the bar can name the container while the encoder owns the
    /// `Recorder`.
    format: RecordFormat,
    /// Where the `.part` file is renamed once encoding finishes.
    dest: PathBuf,
    /// The file the recorder is writing; removed if encoding fails.
    part: PathBuf,
}

/// A cropped selection and its physical-to-logical pixel ratio.
///
/// `image` is the 8-bit SDR tone map used for the overlay, pins, OCR and the
/// history thumbnail. `hdr` is the untouched 16-bit source behind it; when
/// present, every durable export (Save, Copy, history) writes that HDR data so
/// the three stay consistent. Both are `Arc` so cloning a capture is cheap.
#[derive(Clone)]
pub struct Captured {
    pub image: Arc<RgbaImage>,
    pub hdr: Option<Arc<HdrImage>>,
    pub scale: f32,
}

impl Captured {
    fn new(image: RgbaImage, hdr: Option<HdrImage>, selection: Bounds<Pixels>) -> Self {
        let logical_width = selection.size.width.as_f32().max(1.0);
        Self {
            scale: (image.width() as f32 / logical_width).max(0.1),
            image: Arc::new(image),
            hdr: hdr.map(Arc::new),
        }
    }
}

/// The app's singleton windows (settings / history / OCR results / permissions / update).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Panel {
    Ocr,
    Settings,
    History,
    Permissions,
    Update,
}

impl Panel {
    const COUNT: usize = 5;
    const ALL: [Self; Self::COUNT] = [
        Self::Ocr,
        Self::Settings,
        Self::History,
        Self::Permissions,
        Self::Update,
    ];
}

fn panel_title(panel: Panel) -> String {
    let key = match panel {
        Panel::Ocr => "controller.panel.ocr",
        Panel::Settings => "controller.panel.settings",
        Panel::History => "controller.panel.history",
        Panel::Permissions => "controller.panel.permissions",
        Panel::Update => "controller.panel.update",
    };
    t!(key).to_string()
}

#[derive(Default)]
struct PanelSlot {
    handle: Option<AnyWindowHandle>,
    pending: bool,
}

pub struct Controller {
    busy: Option<Busy>,
    panels: [PanelSlot; Panel::COUNT],
    selection: Option<ActiveSelection>,
    /// macOS: overlays already hidden with `orderOut` but whose GPUI teardown
    /// (~37 ms on the main thread) is deferred until the command's clipboard or
    /// file write has landed. Empty on Windows, where overlays are removed
    /// synchronously.
    pending_close: Vec<AnyWindowHandle>,
    recording: Option<ActiveRecording>,
    ocr: OcrState,
    ocr_revision: usize,
    ocr_engine: Option<Arc<dyn OcrEngine>>,
    last_capture: Option<Captured>,
    /// Screen the user last captured/recorded on. Panel windows (and their
    /// notifications) open there instead of always landing on the primary
    /// display in a multi-monitor setup.
    last_display_id: Option<gpui_kit::DisplayId>,
    /// Capture requested from the permission panel once access is granted.
    pending_capture: Option<CaptureIntent>,
    /// Action pressed while a recording was stopping. Encoding keeps `busy`
    /// set for seconds, so it is started once the container is finalised.
    queued_capture: Option<CaptureIntent>,
    /// The user chose to capture without element snapping for this run.
    element_permission_acknowledged: bool,
}

struct GlobalController(Entity<Controller>);

impl Global for GlobalController {}

/// Run `callback` once the window has presented a frame with its current
/// content. `Window::on_next_frame` runs callbacks before the frame is drawn,
/// so the callback has to wait for one more frame.
fn after_presented_frame(window: &mut Window, callback: impl FnOnce(&mut App) + 'static) {
    let pending = Rc::new(RefCell::new(Some(callback)));
    window.on_next_frame(move |window, _| {
        let pending = pending.clone();
        window.on_next_frame(move |_, cx| {
            if let Some(callback) = pending.borrow_mut().take() {
                callback(cx);
            }
        });
    });
}

impl Controller {
    pub fn init(cx: &mut App) -> Entity<Self> {
        let controller = cx.new(|_| Self {
            busy: None,
            panels: Default::default(),
            selection: None,
            pending_close: Vec::new(),
            recording: None,
            ocr: OcrState::Idle,
            ocr_revision: 0,
            ocr_engine: None,
            last_capture: None,
            last_display_id: None,
            pending_capture: None,
            queued_capture: None,
            element_permission_acknowledged: false,
        });
        cx.set_global(GlobalController(controller.clone()));
        cx.on_window_closed(|cx, window_id| {
            // A window can close while this controller is mid-update, so
            // recompute after that update returns.
            cx.defer(move |cx| {
                Controller::global(cx).update(cx, |controller, cx| {
                    // Forget the slot: a closed panel must be recreated rather
                    // than "reactivated" through a handle that no longer has a
                    // window behind it.
                    for slot in &mut controller.panels {
                        if slot
                            .handle
                            .is_some_and(|handle| handle.window_id() == window_id)
                        {
                            slot.handle = None;
                        }
                    }
                    controller.sync_dock_visible(cx);
                });
            });
        })
        .detach();
        #[cfg(target_os = "macos")]
        controller.update(cx, |controller, cx| controller.prewarm_ocr(cx));
        controller
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalController>().0.clone()
    }

    pub fn ocr(&self) -> &OcrState {
        &self.ocr
    }

    pub fn ocr_revision(&self) -> usize {
        self.ocr_revision
    }

    pub fn is_busy(&self) -> bool {
        self.busy.is_some() || self.selection.is_some() || self.recording.is_some()
    }

    /// True while an encoder is flushing after stop / duration cap.
    pub fn is_encoding_recording(&self) -> bool {
        matches!(self.busy, Some(Busy::Encoding))
    }

    /// True while a recording session owns the control bar. The bar uses this
    /// to stop painting once its session is gone but its window removal is
    /// still pending (see `finish_recording_ui`).
    pub fn has_recording(&self) -> bool {
        self.recording.is_some()
    }

    /// Container the running (or finishing) recording is being written to.
    pub fn recording_format(&self) -> Option<RecordFormat> {
        self.recording.as_ref().map(|active| active.format)
    }

    /// `(elapsed, max)` while frames are still being captured. `None` while encoding.
    pub fn recording_progress(&self) -> Option<(Duration, Duration)> {
        self.recording
            .as_ref()?
            .recorder
            .as_ref()
            .map(|recorder| (recorder.elapsed(), recorder.max_duration()))
    }

    /// True between committing the region and the recorder's first live frame.
    pub fn is_starting_recording(&self) -> bool {
        self.recording
            .as_ref()
            .is_some_and(|active| !active.started)
    }

    /// Pausing is only possible once the platform recorder is producing frames.
    pub fn is_recording_paused(&self) -> bool {
        self.recording
            .as_ref()
            .and_then(|active| active.recorder.as_ref())
            .is_some_and(|recorder| recorder.is_paused())
    }

    pub fn can_pause_recording(&self) -> bool {
        self.recording
            .as_ref()
            .is_some_and(|active| active.started && active.recorder.is_some())
    }

    /// Hotkey entry point: start a capture, or stop an ongoing recording.
    ///
    /// Any shortcut still stops a recording (it is the only global way out of
    /// fullscreen capture), but the pressed action is no longer swallowed: it
    /// runs once the encoder has flushed. Pressing the GIF shortcut while a GIF
    /// is recording therefore starts the next one instead of just stopping.
    pub fn on_hotkey(&mut self, action: HotkeyAction, cx: &mut Context<Self>) {
        let intent = match action {
            HotkeyAction::Screenshot => CaptureIntent::Choose,
            HotkeyAction::Ocr => CaptureIntent::Recognize,
            HotkeyAction::Gif => CaptureIntent::Record(RecordFormat::Gif),
            HotkeyAction::Video => CaptureIntent::Record(RecordFormat::Mp4),
        };
        if self.recording.is_some() {
            self.queued_capture = Some(intent);
            self.stop_recording(cx);
            return;
        }
        self.start_capture(intent, cx);
    }

    pub fn reset_ocr_engine(&mut self) {
        self.ocr_engine = None;
    }

    /// 后台预热 OCR 后端，支付 Vision 首次调用的模型编译开销。
    #[cfg(target_os = "macos")]
    pub fn prewarm_ocr(&mut self, cx: &mut Context<Self>) {
        if self.ocr_engine.is_some() {
            return;
        }
        let options = OcrOptions::default().with_languages(settings_view::ocr_languages(
            &AppSettings::get(cx).ocr.language,
        ));
        cx.spawn(async move |this, cx| {
            let engine = cx
                .background_spawn(async move {
                    let engine = snapit_ocr::engine_with(&options)?;
                    engine.prewarm()?;
                    Ok::<_, anyhow::Error>(engine)
                })
                .await;
            match engine {
                Ok(engine) => {
                    this.update(cx, |this, _| {
                        this.ocr_engine.get_or_insert(Arc::from(engine));
                    })
                    .ok();
                }
                Err(err) => log::warn!("OCR prewarm failed: {err:#}"),
            }
        })
        .detach();
    }

    pub fn start_capture(&mut self, intent: CaptureIntent, cx: &mut Context<Self>) {
        if self.is_busy() {
            self.hint_busy(cx);
            return;
        }
        let element_snap = AppSettings::get(cx).capture.element_snap_enabled();
        let missing = permissions::missing(element_snap);
        if missing.is_empty() {
            self.start_capture_now(intent, cx);
            return;
        }
        for permission in &missing {
            // Accessibility is non-blocking: avoid re-prompting once the user
            // chose to capture without element snapping.
            if *permission == Permission::Accessibility && self.element_permission_acknowledged {
                continue;
            }
            permissions::request(*permission);
        }
        let blocking = missing.contains(&Permission::ScreenRecording)
            || (missing.contains(&Permission::Accessibility)
                && !self.element_permission_acknowledged);
        if !blocking {
            self.start_capture_now(intent, cx);
            return;
        }
        self.pending_capture = Some(intent);
        self.open_panel(Panel::Permissions, cx);
    }

    /// A second capture request while one is running used to disappear without
    /// a trace. The selection overlay already has a toast, so use it; encoding
    /// and recording have no suitable surface and fall back to the log.
    fn hint_busy(&mut self, cx: &mut Context<Self>) {
        let text = t!("controller.busy").to_string();
        let Some(overlay) = self
            .selection
            .as_ref()
            .and_then(|active| active.overlays.first())
        else {
            log::info!("{text}");
            return;
        };
        overlay
            .view
            .update(cx, |view, cx| view.show_toast(text, cx));
    }

    /// Run the action a hotkey asked for while a recording was encoding. Only
    /// called on the success path; `fail` clears the queue on any error.
    fn resume_queued_capture(&mut self, cx: &mut Context<Self>) {
        if let Some(intent) = self.queued_capture.take() {
            self.start_capture(intent, cx);
        }
    }

    /// Prompt for missing permissions on launch. `--background` launches stay
    /// silent in the tray and only get guided when a capture starts.
    pub fn check_permissions_on_launch(&mut self, show_panel: bool, cx: &mut Context<Self>) {
        if !show_panel {
            return;
        }
        let missing = permissions::missing(AppSettings::get(cx).capture.element_snap_enabled());
        if missing.is_empty() {
            return;
        }
        for permission in &missing {
            permissions::request(*permission);
        }
        self.open_panel(Panel::Permissions, cx);
    }

    pub fn pending_capture(&self) -> Option<CaptureIntent> {
        self.pending_capture
    }

    pub fn clear_pending_capture(&mut self) {
        self.pending_capture = None;
    }

    /// Continue a capture paused by the permission panel. Screen Recording is
    /// required; Accessibility can be skipped for this run.
    pub fn continue_pending_capture(&mut self, cx: &mut Context<Self>) {
        if !permissions::screen_recording_granted() {
            permissions::request(Permission::ScreenRecording);
            cx.notify();
            return;
        }
        if !permissions::accessibility_granted() {
            self.element_permission_acknowledged = true;
        }
        let Some(intent) = self.pending_capture.take() else {
            return;
        };
        self.start_capture_now(intent, cx);
    }

    fn start_capture_now(&mut self, intent: CaptureIntent, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        crate::logging::begin_capture();
        log::info!("capture start: intent={intent:?}");
        self.busy = Some(Busy::Capturing);
        cx.notify();

        // Start the freeze on a worker first: the BGRA conversion runs on the
        // same thread, so the overlay only waits for one background round trip
        // while window creation overlaps the capture.
        let started = Instant::now();
        let hdr_started = Instant::now();
        let hdr = snapit_capture::hdr_candidates();
        let hdr_ms = ms(hdr_started.elapsed());
        log::info!("HDR candidates: {:?} ({} ms)", hdr, hdr_ms);
        let capture = cx.background_spawn(async move {
            let capture_started = Instant::now();
            let shots = session::capture_shots(&hdr)?;
            let captured = capture_started.elapsed();
            let convert_started = Instant::now();
            let frames = shots
                .into_iter()
                .map(|shot| {
                    let image = session::render_image(shot.image());
                    session::CapturedFrame::new(shot, image)
                })
                .collect::<Vec<_>>();
            log::info!(
                target: "timing",
                "freeze: {} displays, capture {:.1} ms, BGRA {:.1} ms",
                frames.len(),
                ms(captured),
                ms(convert_started.elapsed())
            );
            Ok::<_, anyhow::Error>(frames)
        });
        // Only macOS can pre-place overlays without them reaching the screen:
        // the window stays hidden and is revealed after the first frozen frame.
        // A Windows window with `show: true` is visible before its first
        // (transparent) frame is presented, which flashes the desktop, so
        // Windows opens overlays once the freeze is ready instead.
        let placed = cfg!(target_os = "macos")
            .then(|| self.open_overlays(intent, None, cx))
            .flatten();
        let pre_placed = started.elapsed();

        cx.spawn(async move |this, cx| {
            let frames = capture.await;
            let frozen = started.elapsed();
            this.update(cx, |this, cx| {
                this.busy = None;
                let frames = match frames {
                    Ok(frames) => frames,
                    Err(err) => {
                        this.abort_capture(placed.as_ref(), cx);
                        this.fail(
                            t!("controller.capture_failed", error = format!("{err:#}")).to_string(),
                            cx,
                        );
                        return;
                    }
                };
                match placed {
                    Some(session) => {
                        log::info!(
                            target: "timing",
                            "capture: {:.1} ms to a frozen frame (hdr {hdr_ms:.1} ms on main, overlays pre-placed at {:.1} ms)",
                            ms(frozen),
                            ms(pre_placed)
                        );
                        this.reveal_overlays(&session, frames, cx)
                    }
                    None => {
                        let opening = Instant::now();
                        if let Some(session) = this.open_overlays(intent, Some(frames), cx) {
                            let opened = opening.elapsed();
                            let activating = Instant::now();
                            this.activate_overlays(&session, cx);
                            log::info!(
                                target: "timing",
                                "capture: {:.1} ms to a frozen frame (hdr {hdr_ms:.1} ms), overlays opened in {:.1} ms, activated in {:.1} ms",
                                ms(frozen),
                                ms(opened),
                                ms(activating.elapsed())
                            );
                        }
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Drop a pre-placed capture session whose freeze failed.
    fn abort_capture(&mut self, session: Option<&Entity<CaptureSession>>, cx: &mut Context<Self>) {
        if self
            .selection
            .as_ref()
            .is_some_and(|active| Some(&active.session) == session)
        {
            self.close_selection(cx);
            self.finish_pending_close(cx);
        }
    }

    /// Place one overlay window per display. `frames` is `None` when the
    /// freeze is still running; the windows stay empty and transparent until
    /// it lands.
    fn open_overlays(
        &mut self,
        intent: CaptureIntent,
        frames: Option<Vec<session::CapturedFrame>>,
        cx: &mut Context<Self>,
    ) -> Option<Entity<CaptureSession>> {
        let displays = cx.displays();
        if displays.is_empty() {
            self.busy = None;
            self.fail(t!("controller.overlay_failed").to_string(), cx);
            return None;
        }
        let slots: Vec<_> = displays
            .iter()
            .map(|display| SessionSlot::new(Some(display.id()), display.bounds()))
            .collect();
        let window_snap = AppSettings::get(cx).capture.window_snap;
        let element_snap = AppSettings::get(cx).capture.element_snap_enabled();
        let annotation_defaults = AppSettings::get(cx).annotations.clone();
        let session = cx.new(|_| {
            CaptureSession::new(
                slots,
                intent,
                window_snap,
                element_snap,
                annotation_defaults,
            )
        });
        // macOS keeps a window created with `show: false` hidden, so the
        // pre-placed overlays are ordered front at zero alpha once the window
        // exists: GPUI warms the surface during the freeze without exposing
        // the opaque default background AppKit paints before GPUI applies
        // `window_background`. Windows passes the frames up front and shows an
        // already-drawn window.
        #[cfg(target_os = "macos")]
        let warmed = frames.is_none();
        #[cfg(not(target_os = "macos"))]
        let warmed = false;
        if let Some(frames) = frames {
            session.update(cx, |session, cx| session.set_frames(frames, cx));
        }
        let subscription = cx.subscribe(&session, |this, _, command: &SessionCommand, cx| {
            this.handle_command(*command, cx)
        });

        let mut overlays = Vec::with_capacity(displays.len());
        for (slot, placed) in session.read(cx).slots().to_vec().into_iter().enumerate() {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(placed.bounds())),
                titlebar: None,
                focus: false,
                show: !warmed,
                kind: WindowKind::PopUp,
                is_movable: false,
                is_resizable: false,
                is_minimizable: false,
                display_id: placed.display_id(),
                // Until the frozen frame is drawn, the live screen shows
                // through instead of a black or stale window.
                window_background: WindowBackgroundAppearance::Transparent,
                ..Default::default()
            };
            let session = session.clone();
            match gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| OverlayView::new(session, slot, window, cx))
            }) {
                Ok((window, view)) => {
                    #[cfg(target_os = "macos")]
                    if warmed {
                        window
                            .update(cx, |_, window, _| crate::overlay::pre_place(window))
                            .ok();
                    }
                    overlays.push(OverlayWindow { slot, window, view });
                }
                Err(err) => log::error!("failed to open overlay window: {err:#}"),
            }
        }

        if overlays.is_empty() {
            self.busy = None;
            self.fail(t!("controller.overlay_failed").to_string(), cx);
            return None;
        }
        self.selection = Some(ActiveSelection {
            session: session.clone(),
            overlays,
            _subscription: subscription,
        });
        cx.notify();
        Some(session)
    }

    /// Fill a pre-placed session with the freeze, then show it.
    fn reveal_overlays(
        &mut self,
        session: &Entity<CaptureSession>,
        frames: Vec<session::CapturedFrame>,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self
            .selection
            .as_ref()
            .filter(|active| &active.session == session)
            .map(|active| active.session.clone())
        else {
            return;
        };
        let filling = Instant::now();
        session.update(cx, |session, cx| session.set_frames(frames, cx));
        let set_frames = filling.elapsed();
        let activating = Instant::now();
        self.activate_overlays(&session, cx);
        log::info!(
            target: "timing",
            "overlays: set_frames {:.1} ms, activate call {:.1} ms",
            ms(set_frames),
            ms(activating.elapsed())
        );
    }

    /// Show and focus the overlays of the current session. A display that
    /// failed to freeze has no frame, so its window is closed instead.
    fn activate_overlays(&mut self, session: &Entity<CaptureSession>, cx: &mut Context<Self>) {
        let Some(active) = self
            .selection
            .as_ref()
            .filter(|active| &active.session == session)
        else {
            return;
        };
        let overlays = active.overlays.clone();
        let mut kept = Vec::with_capacity(overlays.len());
        for overlay in overlays {
            if session.read(cx).frame(overlay.slot).is_none() {
                overlay
                    .window
                    .update(cx, |_, window, _| window.remove_window())
                    .ok();
                continue;
            }
            overlay
                .window
                .update(cx, |_, window, cx| {
                    overlay
                        .view
                        .update(cx, |view, cx| view.activate(window, cx));
                })
                .ok();
            kept.push(overlay);
        }
        cx.activate(true);
        if kept.is_empty() {
            self.close_selection(cx);
            self.finish_pending_close(cx);
            self.fail(t!("controller.overlay_failed").to_string(), cx);
            return;
        }
        if let Some(active) = self.selection.as_mut() {
            active.overlays = kept;
        }
        cx.notify();
    }

    fn handle_command(&mut self, command: SessionCommand, cx: &mut Context<Self>) {
        let started = Instant::now();
        let Some(active) = self.selection.as_ref() else {
            return;
        };
        let multi = active.session.read(cx).multi_selection();
        let composed = active.session.read(cx).is_composed();
        let bake = matches!(
            command,
            SessionCommand::Copy | SessionCommand::Save | SessionCommand::Pin
        );
        // Every durable export (Save, Copy, history via remember, and Pin) plus
        // OCR (which also indexes into history) must see the same HDR frame so
        // all outputs stay consistent. Cancel/Record need no still HDR crop.
        let want_hdr = matches!(
            command,
            SessionCommand::Copy
                | SessionCommand::Save
                | SessionCommand::Pin
                | SessionCommand::Recognize
        );
        let (image, record_target) = active.session.update(cx, |session, _| {
            if bake {
                session.finish_annotations();
            }
            let hdr = if want_hdr { session.export_hdr() } else { None };
            let image = if bake {
                session.export_selections()
            } else {
                session.crop_selections()
            }
            .map(|(image, bounds)| Captured::new(image, hdr, bounds));
            let record_target = session.selected_frame().map(|(frame, bounds)| {
                (
                    frame.monitor_id(),
                    frame.to_region(bounds),
                    frame.display_id(),
                    frame.display_bounds(),
                    bounds,
                )
            });
            (image, record_target)
        });
        if let Some((_, _, display_id, _, _)) = record_target.as_ref() {
            self.last_display_id = *display_id;
        }
        let exported = started.elapsed();
        // A record command keeps the active overlay: it becomes the live
        // recording outline instead of closing and opening another window.
        let record_now = matches!(command, SessionCommand::Record(_))
            && !composed
            && image.is_some()
            && record_target.is_some();
        if !record_now {
            self.close_selection(cx);
        }
        log::info!(
            target: "timing",
            "command {command:?}: bake+crop {:.1} ms, overlays closed at {:.1} ms",
            ms(exported),
            ms(started.elapsed())
        );

        match (command, image) {
            (SessionCommand::Cancel, _) => {}
            (SessionCommand::Copy, Some(captured)) => {
                self.remember(&captured, cx);
                self.copy_image(captured, cx)
            }
            (SessionCommand::Save, Some(captured)) => {
                self.remember(&captured, cx);
                self.save_image(captured, cx)
            }
            (SessionCommand::Pin, Some(captured)) => {
                self.remember(&captured, cx);
                // A cross-display collection is larger than the active region,
                // so center it instead of reusing that region's box.
                let placement = record_target.filter(|_| !multi).map(
                    |(_, _, display_id, display_bounds, bounds)| pin::Placement {
                        display_id,
                        bounds: Bounds::new(display_bounds.origin + bounds.origin, bounds.size),
                    },
                );
                // Open after the overlays are gone so the pin is not stacked beneath them.
                cx.defer(move |cx| pin::open(captured, placement, cx));
            }
            (SessionCommand::Recognize, Some(captured)) => self.recognize(captured, cx),
            (SessionCommand::Record(format), Some(_)) => {
                if record_now && let Some(target) = record_target {
                    self.start_recording(target, format, cx);
                }
            }
            (_, None) => self.fail(t!("controller.selection_empty").to_string(), cx),
        }
        // Copy and Save flush the hidden overlays from their async completion
        // (after the clipboard/file write). Every other command has no such
        // late work, so release the overlays now rather than leaking them.
        if !matches!(command, SessionCommand::Copy | SessionCommand::Save) {
            self.finish_pending_close(cx);
        }
        cx.notify();
    }

    fn close_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(active) = self.selection.take() {
            for overlay in active.overlays {
                #[cfg(target_os = "macos")]
                {
                    // Hide now so the selection disappears instantly, but keep
                    // the handle: tearing down a fullscreen GPUI layer on the
                    // main thread costs ~37 ms and would block the clipboard
                    // write. `finish_pending_close` runs once the command lands.
                    overlay
                        .window
                        .update(cx, |_, window, _| crate::overlay::hide(window))
                        .ok();
                    self.pending_close.push(overlay.window);
                }
                #[cfg(not(target_os = "macos"))]
                overlay
                    .window
                    .update(cx, |_, window, _| window.remove_window())
                    .ok();
            }
        }
    }

    /// Destroy overlays hidden by `close_selection`. Called after the
    /// clipboard/file write so the ~37 ms teardown no longer delays the user's
    /// command. Idempotent: the queue is drained on the first call.
    fn finish_pending_close(&mut self, cx: &mut Context<Self>) {
        for handle in self.pending_close.drain(..) {
            handle
                .update(cx, |_, window, _| window.remove_window())
                .ok();
        }
    }

    /// Keep a capture for "pin last capture" and auto-save it to history.
    fn remember(&mut self, captured: &Captured, cx: &mut Context<Self>) {
        self.last_capture = Some(captured.clone());
        let (image, hdr, scale) = (captured.image.clone(), captured.hdr.clone(), captured.scale);
        HistoryStore::global(cx).update(cx, |history, cx| {
            history.add_image(image, hdr, scale, None, cx)
        });
    }

    /// Pin the most recent capture of this session, else the newest image in history.
    pub fn pin_last(&mut self, cx: &mut Context<Self>) {
        if let Some(captured) = self.last_capture.clone() {
            cx.defer(move |cx| pin::open(captured, None, cx));
            return;
        }
        match HistoryStore::global(cx).read(cx).latest_image() {
            Some((path, scale)) => pin::open_file(path, scale, cx),
            None => self.fail(t!("controller.pin_nothing").to_string(), cx),
        }
    }

    pub(crate) fn copy_image(&mut self, captured: Captured, cx: &mut Context<Self>) {
        let (width, height) = captured.image.dimensions();
        let started = Instant::now();
        cx.spawn(async move |this, cx| {
            let encoded = Instant::now();
            let png = cx
                .background_spawn(async move {
                    let started = Instant::now();
                    // Put the same HDR data on the clipboard that Save and the
                    // history original use. GPUI writes PNG bytes verbatim on
                    // both platforms, so the 16-bit + cICP payload survives.
                    let png = match captured.hdr.as_deref() {
                        Some(hdr) => match snapit_capture::encode_png_hdr(hdr) {
                            Ok(bytes) => {
                                log::info!("copy: clipboard HDR PNG (16-bit BT.2020/PQ)");
                                Ok(bytes)
                            }
                            Err(err) => {
                                log::warn!("copy: HDR encode failed, falling back to SDR: {err:#}");
                                snapit_capture::encode_png(&captured.image)
                            }
                        },
                        None => snapit_capture::encode_png(&captured.image),
                    };
                    log::info!(
                        target: "timing",
                        "copy: {width}x{height} PNG encode {:.1} ms",
                        ms(started.elapsed())
                    );
                    png
                })
                .await;
            let encode = encoded.elapsed();
            this.update(cx, |this, cx| {
                match png {
                    Ok(png) => {
                        let writing = Instant::now();
                        cx.write_to_clipboard(ClipboardItem::new_image(&Image::from_bytes(
                            ImageFormat::Png,
                            png,
                        )));
                        log::info!(
                            target: "timing",
                            "copy: clipboard filled {:.1} ms after the command (worker result at {:.1} ms), write {:.1} ms",
                            ms(started.elapsed()),
                            ms(encode),
                            ms(writing.elapsed())
                        );
                        this.inform(
                            t!("controller.copy_done", width = width, height = height).to_string(),
                            cx,
                        );
                    }
                    Err(err) => this.fail(
                        t!("controller.copy_failed", error = format!("{err:#}")).to_string(),
                        cx,
                    ),
                }
                // The overlay was hidden at command time; destroy it only now
                // that the clipboard holds the image, so the ~37 ms teardown is
                // no longer on the critical path to a paste.
                this.finish_pending_close(cx);
            })
            .ok();
            log::debug!(target: "timing", "copy command handled in {:.1} ms", ms(started.elapsed()));
        })
        .detach();
    }

    pub(crate) fn save_image(&mut self, captured: Captured, cx: &mut Context<Self>) {
        let Captured { image, hdr, .. } = captured;
        let capture = &AppSettings::get(cx).capture;
        let (format, quality) = (capture.image_format, capture.jpeg_quality);
        let name = format!("SnapIt {}.{}", timestamp(), format.extension());
        cx.activate(true);
        let path = cx.prompt_for_new_path(&save_dir(cx), Some(&name));
        cx.spawn(async move |this, cx| {
            let Some(mut path) = await_path(path, &this, cx).await else {
                // Save dialog dismissed: nothing will follow to flush the
                // hidden overlays, so release them here.
                this.update(cx, |this, cx| this.finish_pending_close(cx))
                    .ok();
                return;
            };
        let format = SaveFormat::from_path(&path).unwrap_or_else(|| {
            path.set_extension(format.extension());
            format
        });
        let hdr_available = hdr.is_some();
        log::info!(
            "save image: format={format:?} hdr_available={hdr_available} path={}",
            path.display()
        );
        let saved = cx
            .background_spawn(async move {
                let writing = Instant::now();
                let branch = match (format, &hdr) {
                    (SaveFormat::Png, Some(_)) => "HDR PNG (16-bit + cICP)",
                    (SaveFormat::Png, None) => "SDR PNG (8-bit)",
                    (SaveFormat::Jpg, _) => "JPEG (8-bit)",
                };
                log::info!("save branch: {branch}");
                let result = match (format, hdr) {
                    (SaveFormat::Png, Some(hdr)) => snapit_capture::save_png_hdr(&hdr, &path),
                    (SaveFormat::Png, None) => snapit_capture::save_png(&image, &path),
                    (SaveFormat::Jpg, _) => snapit_capture::save_jpeg(&image, &path, quality),
                };
                log::info!(
                    "save result: {} ({:.1} ms) {}",
                    if result.is_ok() { "ok" } else { "FAILED" },
                    ms(writing.elapsed()),
                    if let Err(err) = &result {
                        format!("error={err:#}")
                    } else {
                        String::new()
                    }
                );
                result.map(|_| path)
            })
            .await;
            this.update(cx, |this, cx| {
                match saved {
                    Ok(path) => this.inform(
                        t!("controller.save_done", path = path.display()).to_string(),
                        cx,
                    ),
                    Err(err) => this.fail(
                        t!("controller.save_failed", error = format!("{err:#}")).to_string(),
                        cx,
                    ),
                }
                this.finish_pending_close(cx);
            })
            .ok();
        })
        .detach();
    }

    fn recognize(&mut self, captured: Captured, cx: &mut Context<Self>) {
        self.last_capture = Some(captured.clone());
        let Captured {
            image,
            hdr,
            scale,
        } = captured;
        let history_image = image.clone();
        self.ocr = OcrState::Running;
        self.ocr_revision += 1;
        self.open_panel(Panel::Ocr, cx);
        let engine = self.ocr_engine.clone();
        let options = OcrOptions::default().with_languages(settings_view::ocr_languages(
            &AppSettings::get(cx).ocr.language,
        ));
        let revision = self.ocr_revision;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let engine: Arc<dyn OcrEngine> = match engine {
                        Some(engine) => engine,
                        None => Arc::from(snapit_ocr::engine_with(&options)?),
                    };
                    let blocks = engine.recognize(&image)?;
                    Ok::<_, anyhow::Error>((engine, blocks))
                })
                .await;
            this.update(cx, |this, cx| {
                let text = result
                    .as_ref()
                    .ok()
                    .map(|(_, blocks)| snapit_ocr::blocks_to_text(blocks));
                HistoryStore::global(cx).update(cx, |history, cx| {
                    history.add_image(history_image, hdr, scale, text.clone(), cx)
                });
                if this.ocr_revision != revision {
                    return;
                }
                this.ocr = match result {
                    Ok((engine, _)) => {
                        let name = engine.name();
                        this.ocr_engine = Some(engine);
                        let text: SharedString = text.unwrap_or_default().into();
                        if !text.is_empty() {
                            cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
                        }
                        OcrState::Done { text, engine: name }
                    }
                    Err(err) => OcrState::Failed(format!("{err:#}").into()),
                };
                this.ocr_revision += 1;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn start_recording(
        &mut self,
        (monitor_id, region, display_id, display_bounds, selection): (
            u32,
            snapit_capture::Region,
            Option<gpui_kit::DisplayId>,
            Bounds<Pixels>,
            Bounds<Pixels>,
        ),
        format: RecordFormat,
        cx: &mut Context<Self>,
    ) {
        crate::logging::begin_recording();
        log::info!("recording start: format={format:?} display={monitor_id} region={region:?}");
        self.busy = Some(Busy::Capturing);
        self.last_display_id = display_id;
        // Remember the choice so the overlay's record button repeats it.
        if last_record_format(cx) != format {
            AppSettings::update(cx, |config| {
                config.recording.last_format = format.extension().to_string()
            });
        }
        let recording = &AppSettings::get(cx).recording;
        // GIF and MP4 are tuned separately: a palette container stays small at
        // a low rate, while a video codec can afford a smoother one.
        let fps = match format {
            RecordFormat::Gif => recording.gif_fps,
            RecordFormat::Mp4 => recording.mp4_fps,
        };
        let (output, dest) = recording_paths(cx, format);
        let options = RecordOptions::default()
            .with_fps(fps)
            .with_max_duration(Duration::from_secs(recording.max_duration_secs.into()))
            .with_max_width(Some(selection.size.width.as_f32().round().max(1.0) as u32))
            .with_format(format)
            .with_cursor(recording.captures_cursor)
            // The recorder ignores this for GIF, which has no extended range.
            .with_hdr(recording.hdr);
        log::info!("recording options: fps={fps} hdr_requested={}", recording.hdr);
        // Keep the overlay on screen as the region highlight: drop the frozen
        // frame and the toolbar, keep the dim mask and the selection border.
        // The bar opens right away in its "starting" state; the recorder joins
        // the same state once the live screen has replaced the frozen frame.
        let Some(overlay) = self.take_recording_overlay(display_id, cx) else {
            self.busy = None;
            self.fail(
                t!(
                    "controller.record_start_failed",
                    error = "overlay window is gone"
                )
                .to_string(),
                cx,
            );
            return;
        };
        self.recording = Some(ActiveRecording {
            recorder: None,
            bar: None,
            frame: Some(overlay.window),
            started: false,
            format,
            dest,
            part: output.clone(),
        });
        self.open_recording_bar(display_id, display_bounds, selection, cx);
        let controller = cx.entity();
        let entered = overlay.window.update(cx, |_, window, cx| {
            overlay
                .view
                .update(cx, |view, cx| view.enter_recording(selection, window, cx));
            after_presented_frame(window, move |cx| {
                controller.update(cx, |this, cx| {
                    this.spawn_recorder((monitor_id, region), options, output, cx)
                });
            });
        });
        if entered.is_err() {
            self.busy = None;
            self.finish_recording_ui(cx);
            self.fail(
                t!("controller.record_start_failed", error = "overlay").to_string(),
                cx,
            );
        }
    }

    /// The recorded display's overlay becomes the live recording outline; every
    /// other overlay closes.
    fn take_recording_overlay(
        &mut self,
        display_id: Option<gpui_kit::DisplayId>,
        cx: &mut Context<Self>,
    ) -> Option<OverlayWindow> {
        let active = self.selection.take()?;
        let slot = display_id.and_then(|id| {
            active
                .session
                .read(cx)
                .slots()
                .iter()
                .position(|slot| slot.display_id() == Some(id))
        });
        let keep = slot.unwrap_or(0);
        let mut kept = None;
        for overlay in active.overlays {
            if overlay.slot == keep {
                kept = Some(overlay);
            } else {
                overlay
                    .window
                    .update(cx, |_, window, _| window.remove_window())
                    .ok();
            }
        }
        kept
    }

    fn spawn_recorder(
        &mut self,
        (monitor_id, region): (u32, snapit_capture::Region),
        options: RecordOptions,
        output: PathBuf,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let path = output.clone();
            let recorder = cx
                .background_spawn(
                    async move { Recorder::start(monitor_id, region, options, output) },
                )
                .await;
            this.update(cx, |this, cx| {
                this.busy = None;
                match recorder {
                    Ok(recorder) => {
                        let Some(active) = this.recording.as_mut() else {
                            // Cancelled while the platform recorder warmed up:
                            // stop it off the main thread and drop the file.
                            std::thread::spawn(move || {
                                let _ = recorder.stop();
                                let _ = std::fs::remove_file(path);
                            });
                            return;
                        };
                        active.recorder = Some(recorder);
                        active.started = true;
                        cx.notify();
                        Self::tick_recording(cx);
                    }
                    Err(err) => {
                        if this.recording.is_none() {
                            return;
                        }
                        // A failed start can leave an empty `.part` behind in
                        // the save directory.
                        std::fs::remove_file(&path).ok();
                        this.finish_recording_ui(cx);
                        this.fail(
                            t!("controller.record_start_failed", error = format!("{err:#}"))
                                .to_string(),
                            cx,
                        );
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn open_recording_bar(
        &mut self,
        display_id: Option<gpui_kit::DisplayId>,
        display_bounds: Bounds<Pixels>,
        selection: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) {
        // Window and visible bar are the same size: a transparent margin around
        // it exposes the system window's own border and shadow edge.
        let bar = size(px(BAR_SIZE.0), px(BAR_SIZE.1));
        let gap = px(8.);
        let display_size = display_bounds.size;
        let bottom = selection.origin.y + selection.size.height;
        let right = selection.origin.x + selection.size.width;
        let aligned_x = if right >= bar.width {
            right - bar.width
        } else {
            selection.origin.x
        }
        .max(px(0.))
        .min(display_size.width - bar.width);
        // Prefer placing the bar outside the captured area: below or above
        // first, then in free space beside a tall selection. It is safe to
        // overlap when a selection covers the whole display, since the bar is
        // excluded from the frames either way.
        let (x, y) = if bottom + gap + bar.height <= display_size.height {
            (aligned_x, bottom + gap)
        } else if selection.origin.y >= bar.height + gap {
            (aligned_x, selection.origin.y - gap - bar.height)
        } else if right + gap + bar.width <= display_size.width {
            (right + gap, (bottom - gap - bar.height).max(px(0.)))
        } else if selection.origin.x >= bar.width + gap {
            (
                selection.origin.x - gap - bar.width,
                (bottom - gap - bar.height).max(px(0.)),
            )
        } else {
            (aligned_x, bottom - gap - bar.height)
        };
        let origin = display_bounds.origin + point(x, y);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(origin, bar))),
            titlebar: None,
            focus: true,
            show: true,
            kind: WindowKind::PopUp,
            is_movable: true,
            is_resizable: false,
            is_minimizable: false,
            display_id,
            window_background: WindowBackgroundAppearance::Transparent,
            ..Default::default()
        };
        let controller = cx.entity();
        // Deferred for the same reason as `open_panel`.
        cx.defer(move |cx| {
            let view_controller = controller.clone();
            // Plain window: `gpui_kit::open_window` would paint the theme
            // background behind the bar's rounded corners.
            let opened = cx
                .open_window(options, |window, cx| {
                    cx.new(|cx| RecordingBar::new(view_controller, window, cx))
                })
                .map(AnyWindowHandle::from)
                .inspect_err(|err| log::error!("failed to open recording bar: {err:#}"));
            let Ok(handle) = opened else {
                return;
            };
            let orphaned = controller.update(cx, |this, _| match this.recording.as_mut() {
                Some(active) => {
                    active.bar = Some(handle);
                    false
                }
                None => true,
            });
            if orphaned {
                handle
                    .update(cx, |_, window, _| window.remove_window())
                    .ok();
            }
        });
    }

    fn tick_recording(cx: &mut Context<Self>) {
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
                cx.background_executor().timer(RECORDING_TICK).await;
                let keep_ticking = this.update(cx, |this, cx| {
                    let Some(active) = this.recording.as_ref() else {
                        return false;
                    };
                    let Some(recorder) = active.recorder.as_ref() else {
                        // Stop already requested; encoding task owns the session.
                        return false;
                    };
                    if recorder.is_finished() {
                        this.stop_recording(cx);
                        return false;
                    }
                    cx.notify();
                    true
                });
                if !matches!(keep_ticking, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    /// Throw the current recording away. Nothing is saved, added to history or
    /// copied to the clipboard — the opposite of `stop_recording`.
    ///
    /// Esc and the bar's cancel button land here. A recorder that has not
    /// produced its first frame yet is discarded by `spawn_recorder` once it
    /// notices the session is gone; a running one is stopped on a worker
    /// thread, since joining the sinks of a long GIF can take seconds.
    pub fn cancel_recording(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.recording.as_mut() else {
            return;
        };
        // The encoder already owns the file and is about to publish it; there
        // is nothing left to cancel at that point.
        if matches!(self.busy, Some(Busy::Encoding)) {
            return;
        }
        if let Some(recorder) = active.recorder.take() {
            let discard = std::thread::Builder::new()
                .name("snapit-record-discard".into())
                .spawn(move || {
                    if let Err(err) = recorder.discard() {
                        log::warn!("failed to discard recording: {err:#}");
                    }
                });
            if let Err(err) = discard {
                log::warn!("failed to start the discard thread: {err:#}");
            }
        }
        self.busy = None;
        self.finish_recording_ui(cx);
        log::info!("recording discarded");
        self.resume_queued_capture(cx);
    }

    /// Pause/resume the running recorder. Frames captured while paused are
    /// drained but never encoded, and the paused time is left out of the file.
    pub fn toggle_recording_pause(&mut self, cx: &mut Context<Self>) {
        let Some(recorder) = self
            .recording
            .as_mut()
            .and_then(|active| active.recorder.as_ref())
        else {
            return;
        };
        recorder.set_paused(!recorder.is_paused());
        cx.notify();
    }

    pub fn stop_recording(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.recording.as_mut() else {
            return;
        };
        if active.recorder.is_none() {
            if matches!(self.busy, Some(Busy::Encoding)) {
                return;
            }
            // Still waiting for the platform recorder: cancel the start. The
            // task that owns the recorder discards it when it finds no session.
            self.busy = None;
            self.finish_recording_ui(cx);
            self.resume_queued_capture(cx);
            return;
        }
        let recorder = active.recorder.take().expect("checked above");
        let dest = active.dest.clone();
        let part = active.part.clone();
        let clicked = Instant::now();
        self.busy = Some(Busy::Encoding);
        cx.notify();

        // Keep the recording bar open with an "encoding…" label. The GIF flush
        // can take seconds; join it on a dedicated OS thread and poll so neither
        // the GPUI main thread nor the async background pool is blocked. Do not
        // open a modal save dialog afterwards (tray-only / no parent window
        // hangs on macOS).
        cx.spawn(async move |this, cx| {
            let handle = match std::thread::Builder::new()
                .name("snapit-gif-encode".into())
                .spawn(move || recorder.stop())
            {
                Ok(handle) => handle,
                Err(err) => {
                    std::fs::remove_file(&part).ok();
                    this.update(cx, |this, cx| {
                        this.finish_recording_ui(cx);
                        this.fail(
                            t!("controller.record_stop_failed", error = err).to_string(),
                            cx,
                        );
                    })
                    .ok();
                    return;
                }
            };
            // 4 ms, not 16: this timer is the only delay between the encoder
            // finishing and the clipboard being filled.
            while !handle.is_finished() {
                cx.background_executor()
                    .timer(Duration::from_millis(4))
                    .await;
            }
            let summary = match handle.join() {
                Ok(Ok(summary)) => summary,
                Ok(Err(err)) => {
                    std::fs::remove_file(&part).ok();
                    this.update(cx, |this, cx| {
                        this.finish_recording_ui(cx);
                        this.fail(
                            t!("controller.record_failed", error = format!("{err:#}")).to_string(),
                            cx,
                        );
                    })
                    .ok();
                    return;
                }
                Err(_) => {
                    std::fs::remove_file(&part).ok();
                    this.update(cx, |this, cx| {
                        this.finish_recording_ui(cx);
                        this.fail(t!("controller.record_thread_crashed").to_string(), cx);
                    })
                    .ok();
                    return;
                }
            };

            let encoded = clicked.elapsed();
            let source = summary.path.clone();
            let format = summary.format;
            let publishing = Instant::now();
            let saved = cx
                .background_spawn(async move {
                    // The recorder wrote a `.part` file next to its final name,
                    // so publishing it is a same-directory rename.
                    let path = match move_file(&source, &dest) {
                        Ok(()) => dest,
                        Err(err) => {
                            std::fs::remove_file(&source).ok();
                            return Err(err);
                        }
                    };
                    // Read a finished GIF off the main thread; large recordings
                    // would otherwise stall a frame while copying.
                    let gif = matches!(format, RecordFormat::Gif)
                        .then(|| std::fs::read(&path).ok())
                        .flatten();
                    Ok::<_, anyhow::Error>((path, gif))
                })
                .await;
            let published = publishing.elapsed();

            let preview = summary.first_frame.clone();
            this.update(cx, |this, cx| {
                match saved {
                    Ok((path, gif)) => {
                        // Fill the clipboard before the bar closes: a paste
                        // right after completion must find this recording, not
                        // whatever was copied last.
                        let filling = Instant::now();
                        let copied = match (summary.format, gif) {
                            (RecordFormat::Gif, Some(bytes)) => {
                                cx.write_to_clipboard(ClipboardItem::new_image(
                                    &Image::from_bytes(ImageFormat::Gif, bytes),
                                ));
                                Ok(clipboard::FileCopy::File)
                            }
                            (RecordFormat::Gif, None) => {
                                Err(anyhow::anyhow!("cannot read {}", path.display()))
                            }
                            (RecordFormat::Mp4, _) => clipboard::copy_file_reference(&path, cx),
                        };
                        let clipboard_time = filling.elapsed();
                        this.finish_recording_ui(cx);
                        log::info!(
                            target: "timing",
                            "stop: encode {:.1} ms, publish+read {:.1} ms, clipboard {:.1} ms, UI closed {:.1} ms after the stop click",
                            ms(encoded),
                            ms(published),
                            ms(clipboard_time),
                            ms(clicked.elapsed())
                        );
                        this.inform(
                            t!(
                                "controller.record_saved",
                                format = format_label(summary.format),
                                path = path.display(),
                                frames = summary.frames,
                                seconds = format!("{:.1}", summary.duration.as_secs_f32())
                            )
                            .to_string(),
                            cx,
                        );
                        match copied {
                            Ok(clipboard::FileCopy::File) => this.inform(
                                t!(
                                    "controller.record_copied",
                                    format = format_label(summary.format)
                                )
                                .to_string(),
                                cx,
                            ),
                            Ok(clipboard::FileCopy::PathText) => this.inform(
                                t!(
                                    "controller.record_copied_path",
                                    format = format_label(summary.format)
                                )
                                .to_string(),
                                cx,
                            ),
                            Err(err) => this.fail(
                                t!("controller.record_copy_failed", error = format!("{err:#}"))
                                    .to_string(),
                                cx,
                            ),
                        }
                        // History copies the file and renders a thumbnail;
                        // neither has to finish before the recording is
                        // usable, so let it run after the bar is gone.
                        if let Some(preview) = preview {
                            let kind = entry_kind(summary.format);
                            HistoryStore::global(cx)
                                .update(cx, |history, cx| {
                                    history.add_recording(path, kind, preview, 1.0, cx)
                                })
                                .detach();
                        }
                    }
                    Err(err) => {
                        this.finish_recording_ui(cx);
                        this.fail(
                            t!(
                                "controller.record_save_failed",
                                format = format_label(summary.format),
                                error = format!("{err:#}")
                            )
                            .to_string(),
                            cx,
                        );
                    }
                }
                this.resume_queued_capture(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Close the recording bar and clear encoding busy after stop finishes.
    fn finish_recording_ui(&mut self, cx: &mut Context<Self>) {
        // Esc / Enter / button clicks are dispatched *inside* the bar window's
        // update stack. GPUI takes a window out of `App::windows` while it is
        // being updated, so `handle.update(...)` on that same window returns
        // "window not found" and `remove_window()` is silently lost — the bar
        // then sits on screen forever showing its "Finishing…" fallback.
        // Defer the removals until the stack has unwound. The handles belong to
        // this closure, so a recording started in the meantime keeps its own
        // windows.
        let windows = self
            .recording
            .take()
            .map(|active| (active.bar, active.frame));
        if let Some((bar, frame)) = windows {
            let deferred_at = Instant::now();
            cx.defer(move |cx| {
                for handle in [bar, frame].into_iter().flatten() {
                    let removing = Instant::now();
                    handle
                        .update(cx, |_, window, _| window.remove_window())
                        .ok();
                    log::info!(
                        target: "timing",
                        "teardown: remove_window {:.1} ms, ran {:.1} ms after the deferred close",
                        ms(removing.elapsed()),
                        ms(deferred_at.elapsed())
                    );
                }
            });
        }
        if matches!(self.busy, Some(Busy::Encoding)) {
            self.busy = None;
        }
        cx.notify();
    }

    /// macOS: the Dock icon is visible only while a panel window is open.
    fn sync_dock_visible(&self, cx: &mut App) {
        let open = cx.windows();
        let any_panel = self.panels.iter().any(|slot| {
            slot.handle.is_some_and(|handle| {
                open.iter()
                    .any(|window| window.window_id() == handle.window_id())
            })
        });
        appearance::set_dock_visible(any_panel, cx);
    }

    /// Retitle open panels after the UI language changes.
    pub fn refresh_titles(&self, cx: &mut App) {
        for (panel, slot) in Panel::ALL.into_iter().zip(&self.panels) {
            let Some(handle) = slot.handle else {
                continue;
            };
            let title = panel_title(panel);
            handle
                .update(cx, |_, window, _| window.set_window_title(&title))
                .ok();
        }
    }

    /// Focus `panel`'s window, opening it if needed.
    pub fn open_panel(&mut self, panel: Panel, cx: &mut Context<Self>) {
        // Trust the live window list, not the slot: a window that was closed a
        // moment ago can still leave its handle behind, and "reactivating" it
        // would silently drop the open request (no panel, no refresh).
        let live: Vec<_> = cx
            .windows()
            .iter()
            .map(|window| window.window_id())
            .collect();
        let slot = &mut self.panels[panel as usize];
        if slot
            .handle
            .is_some_and(|handle| !live.contains(&handle.window_id()))
        {
            slot.handle = None;
        }
        if let Some(handle) = slot.handle.take()
            && handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
        {
            slot.handle = Some(handle);
            cx.activate(true);
            // The panel keeps the previous run's content; tell its view the
            // controller changed so a repeated capture refreshes in place
            // instead of waiting for the next result to land.
            cx.notify();
            return;
        }
        if slot.pending {
            return;
        }
        slot.pending = true;
        let controller = cx.entity();
        let display_id = self.last_display_id;
        // `open_window` renders immediately and the views read the controller,
        // so it must run after this update has released the controller.
        cx.defer(move |cx| {
            let handle = open_panel_window(panel, controller.clone(), display_id, cx)
                .inspect_err(|err| log::error!("failed to open {panel:?} window: {err:#}"))
                .ok();
            controller.update(cx, |this, cx| {
                let slot = &mut this.panels[panel as usize];
                slot.pending = false;
                slot.handle = handle;
                this.sync_dock_visible(cx);
            });
            cx.activate(true);
        });
    }

    pub(crate) fn inform(&mut self, text: String, cx: &mut Context<Self>) {
        log::info!("{text}");
        cx.notify();
    }

    pub(crate) fn fail(&mut self, text: String, cx: &mut Context<Self>) {
        log::error!("{text}");
        // Never chain a queued follow-up action onto a failed capture.
        self.queued_capture = None;
        cx.notify();
    }
}

/// Await a `prompt_for_new_path` answer; `None` when cancelled or failed.
async fn await_path<E>(
    prompt: impl Future<Output = Result<Result<Option<PathBuf>>, E>>,
    this: &WeakEntity<Controller>,
    cx: &mut AsyncApp,
) -> Option<PathBuf> {
    match prompt.await {
        Ok(Ok(path)) => path,
        Ok(Err(err)) => {
            this.update(cx, |this, cx| {
                this.fail(
                    t!("controller.save_dialog_failed", error = format!("{err:#}")).to_string(),
                    cx,
                )
            })
            .ok();
            None
        }
        Err(_) => None,
    }
}

fn open_panel_window(
    panel: Panel,
    controller: Entity<Controller>,
    display_id: Option<gpui_kit::DisplayId>,
    cx: &mut App,
) -> Result<AnyWindowHandle> {
    let (window_size, min_size) = match panel {
        Panel::Ocr => (size(px(480.), px(420.)), size(px(360.), px(280.))),
        Panel::Settings => (size(px(780.), px(580.)), size(px(560.), px(400.))),
        Panel::History => (size(px(640.), px(640.)), size(px(480.), px(360.))),
        Panel::Permissions => (size(px(520.), px(360.)), size(px(420.), px(280.))),
        Panel::Update => (size(px(520.), px(460.)), size(px(420.), px(360.))),
    };
    // `WindowBounds::centered` hardcodes `None` for the display, which resolves
    // to the primary screen. Centre on the display we were told about and pass
    // the same id to the window, otherwise panels and their notifications show
    // up on the wrong monitor.
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            display_id,
            window_size,
            cx,
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some(panel_title(panel).into()),
            ..Default::default()
        }),
        window_min_size: Some(min_size),
        display_id,
        ..Default::default()
    };
    let handle = match panel {
        Panel::Ocr => {
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| OcrView::new(controller, window, cx))
            })?
            .0
        }
        Panel::Settings => {
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| SettingsView::new(window, cx))
            })?
            .0
        }
        Panel::History => {
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| HistoryView::new(window, cx))
            })?
            .0
        }
        Panel::Permissions => {
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| PermissionView::new(controller, window, cx))
            })?
            .0
        }
        Panel::Update => {
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| UpdateView::new(window, cx))
            })?
            .0
        }
    };
    Ok(handle)
}

fn timestamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H.%M.%S").to_string()
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e3
}

/// Format the overlay's single record button uses: whatever was recorded last,
/// wherever that happened (shortcut, tray or the button's dropdown).
pub fn last_record_format(cx: &App) -> RecordFormat {
    RecordFormat::parse(&AppSettings::get(cx).recording.last_format).unwrap_or_default()
}

/// History kind for a finished recording, so GIFs and videos can sit side by
/// side in the list.
fn entry_kind(format: RecordFormat) -> EntryKind {
    match format {
        RecordFormat::Gif => EntryKind::Gif,
        RecordFormat::Mp4 => EntryKind::Mp4,
    }
}

/// Localized name of a container, for the bar and for notifications.
pub fn format_label(format: RecordFormat) -> String {
    match format {
        RecordFormat::Gif => t!("format.gif").to_string(),
        RecordFormat::Mp4 => t!("format.mp4").to_string(),
    }
}

/// The configured save directory if it still exists, else the platform default.
pub fn save_dir(cx: &App) -> PathBuf {
    AppSettings::get(cx)
        .capture
        .save_dir
        .clone()
        .filter(|dir| dir.is_dir())
        .unwrap_or_else(default_save_dir)
}

/// Where a recording is written while it runs, and the name it takes once the
/// encoder finishes. The `.part` file already sits in the final directory, so
/// publishing it is a rename instead of a copy out of the temp directory. A
/// save directory that cannot be written falls back to the temp directory.
fn recording_paths(cx: &App, format: RecordFormat) -> (PathBuf, PathBuf) {
    let extension = format.extension();
    let name = format!("SnapIt {}.{extension}", timestamp());
    let paths_in = |dir: PathBuf| {
        let dest = dir.join(&name);
        (dest.with_extension(format!("{extension}.part")), dest)
    };
    let (part, dest) = paths_in(save_dir(cx));
    // AVAssetWriter refuses to open an existing file, so probe writability
    // with a throwaway sibling instead of the recording path itself.
    let probe = part.with_extension("probe");
    let writable = std::fs::File::create(&probe).is_ok();
    std::fs::remove_file(&probe).ok();
    if writable {
        (part, dest)
    } else {
        log::warn!(
            "cannot record into {}; using the temporary directory",
            part.display()
        );
        paths_in(std::env::temp_dir())
    }
}

fn default_save_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    ["Pictures", "Desktop"]
        .iter()
        .map(|dir| home.join(dir))
        .find(|dir| dir.is_dir())
        .unwrap_or(home)
}

fn move_file(from: &Path, to: &Path) -> Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to).with_context(|| format!("failed to write {}", to.display()))?;
    std::fs::remove_file(from).ok();
    Ok(())
}
