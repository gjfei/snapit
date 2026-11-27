//! App-wide workflow: freeze → select → copy / save / recognize / record.
//!
//! The controller is a global entity so the tray, hotkeys, OCR panel, overlays
//! and the recording bar all drive the same state machine.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, AsyncApp, Bounds, ClipboardItem, Context, Entity,
    Global, Image, ImageFormat, Pixels, SharedString, Subscription, TitlebarOptions, WeakEntity,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, point, px, size,
};
use snapit_capture::{HdrImage, ImageSource, RgbaImage, WatermarkSpec};
use snapit_ocr::{OcrEngine, OcrOptions};
use snapit_recorder::{ClickEffect, RecordFormat, RecordOptions, Recorder, RecorderStart};
use snapit_scroll::{
    AutoScrollError, ScrollDirection, ScrollOptions, ScrollSession, ScrollSnapshot, ScrollStart,
    ScrollStatus,
};
use snapit_store::{EntryKind, ImageFormat as SaveFormat, WatermarkTarget};

use crate::annotate::{Layer, Shape};
use crate::appearance;
use crate::clipboard;
use crate::export::ExportComposition;
use crate::history_store::HistoryStore;
use crate::history_view::HistoryView;
use crate::hotkey::HotkeyAction;
use crate::ocr_view::OcrView;
use crate::overlay::{OverlayView, after_presented_frame};
use crate::permission_view::PermissionView;
use crate::permissions::{self, Permission};
use crate::pin;
use crate::recording_bar::{BAR_SIZE, RecordingBar};
use crate::scroll_bar::{ScrollBar, ScrollPreview, scroll_bar_bounds, scroll_preview_bounds};
use crate::session::{self, CaptureIntent, CaptureSession, SessionCommand, SessionSlot};
use crate::settings::AppSettings;
use crate::settings_view::{self, SettingsPage, SettingsView};
use crate::update_view::UpdateView;

const RECORDING_TICK: Duration = Duration::from_millis(250);
const SCROLL_TICK: Duration = Duration::from_millis(16);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollFinishAction {
    Copy,
    Save,
    Pin,
    Recognize,
    Edit,
}

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
    /// Distinguishes delayed callbacks from an older recording session.
    id: u64,
    /// `None` while the recorder is starting, or after stop while the encoder flushes.
    recorder: Option<Recorder>,
    start: Option<RecorderStart>,
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

struct ScrollAnnotationMode {
    origin_y: i64,
    resume_auto: bool,
    was_paused: bool,
    direction: ScrollDirection,
    requested_origin: Option<i64>,
}

struct ActiveScroll {
    id: u64,
    session: Option<ScrollSession>,
    start: Option<ScrollStart>,
    bar: Option<AnyWindowHandle>,
    saved_bar_bounds: Option<Bounds<Pixels>>,
    preview: Option<AnyWindowHandle>,
    frame: Option<OverlayWindow>,
    display_id: Option<gpui_kit::DisplayId>,
    display_bounds: Bounds<Pixels>,
    selection: Bounds<Pixels>,
    scale: f32,
    snapshot: Arc<ScrollSnapshot>,
    annotations: Layer,
    annotation_content_shift: u32,
    annotation_reset_pending: bool,
    annotation_auto_resume: bool,
    auto_scroll_intent: Option<ScrollDirection>,
    annotation_mode: Option<ScrollAnnotationMode>,
    preview_visible: bool,
    finishing: bool,
    pending_finish: Option<ScrollFinishAction>,
    last_warning: bool,
}

fn annotation_auto_direction(
    snapshot: &ScrollSnapshot,
    intent: Option<ScrollDirection>,
) -> Option<ScrollDirection> {
    if snapshot.paused || snapshot.status == ScrollStatus::ReachedEnd {
        None
    } else if snapshot.auto_scrolling {
        Some(snapshot.auto_direction)
    } else {
        intent
    }
}

/// A cropped selection with one frozen composition shared by every export.
#[derive(Clone)]
pub struct Captured {
    pub clean_sdr: Arc<ImageSource>,
    pub composition: Arc<ExportComposition>,
    pub scale: f32,
}

impl Captured {
    pub(crate) fn from_parts(
        image: impl Into<ImageSource>,
        hdr: Option<HdrImage>,
        layout_scale: f32,
        shapes: Option<Arc<Vec<Shape>>>,
        watermark: WatermarkSpec,
    ) -> Self {
        Self::new(image, hdr, layout_scale, shapes, watermark)
    }

    fn new(
        image: impl Into<ImageSource>,
        hdr: Option<HdrImage>,
        layout_scale: f32,
        shapes: Option<Arc<Vec<Shape>>>,
        watermark: WatermarkSpec,
    ) -> Self {
        let scale = if layout_scale.is_finite() {
            layout_scale.max(0.1)
        } else {
            1.0
        };
        let clean_sdr = Arc::new(image.into());
        let composition = Arc::new(ExportComposition::new(
            clean_sdr.clone(),
            hdr.map(Arc::new),
            shapes.unwrap_or_default(),
            Arc::new(watermark),
            scale,
        ));
        Self {
            clean_sdr,
            composition,
            scale,
        }
    }

    pub fn frozen(png: Arc<Vec<u8>>, preview: Arc<RgbaImage>, scale: f32, hdr: bool) -> Self {
        Self {
            clean_sdr: Arc::new(ImageSource::Memory(preview.clone())),
            composition: Arc::new(ExportComposition::frozen(png, preview, hdr)),
            scale: scale.max(0.1),
        }
    }
}

enum ClipboardPayload {
    Image(Vec<u8>),
    File(PathBuf),
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

#[derive(Default)]
pub struct Controller {
    busy: Option<Busy>,
    /// Identifies the async operation allowed to release `busy`. Recording and
    /// scrolling own it directly and therefore leave this empty.
    busy_serial: u64,
    busy_owner: Option<u64>,
    panels: [PanelSlot; Panel::COUNT],
    settings_view: Option<WeakEntity<SettingsView>>,
    pending_settings_page: Option<SettingsPage>,
    selection: Option<ActiveSelection>,
    /// macOS: overlays already hidden with `orderOut` but whose GPUI teardown
    /// (~37 ms on the main thread) is deferred until the command's clipboard or
    /// file write has landed. Empty on Windows, where overlays are removed
    /// synchronously.
    pending_close: Vec<AnyWindowHandle>,
    recording: Option<ActiveRecording>,
    recording_serial: u64,
    scroll: Option<ActiveScroll>,
    scroll_serial: u64,
    scroll_editors: Vec<AnyWindowHandle>,
    watermark_editor: Option<AnyWindowHandle>,
    workflow_serial: u64,
    log_operation: Option<u64>,
    ocr: OcrState,
    ocr_revision: usize,
    ocr_engine_revision: u64,
    ocr_engine: Option<Arc<dyn OcrEngine>>,
    last_capture: Option<Captured>,
    clipboard_file: Option<PathBuf>,
    /// Only the newest disk-backed copy may publish, replace the managed file,
    /// or announce completion. Older workers clean up only their own output.
    copy_serial: u64,
    /// Screen the user last captured/recorded on. Panel windows (and their
    /// notifications) open there instead of always landing on the primary
    /// display in a multi-monitor setup.
    last_display_id: Option<gpui_kit::DisplayId>,
    /// Capture requested from the permission panel once access is granted.
    pending_capture: Option<CaptureIntent>,
    /// Action pressed while a recording was stopping. Encoding keeps `busy`
    /// set for seconds, so it is started once the container is finalised.
    queued_capture: Option<CaptureIntent>,
    /// macOS: PID of the other application that held focus before capture
    /// began so closing the overlay returns focus instead of raising open
    /// SnapIt windows in the background.
    pending_restore_app: Option<i32>,
    /// The user chose to capture without element snapping for this run.
    element_permission_acknowledged: bool,
}

struct GlobalController(Entity<Controller>);

impl Global for GlobalController {}

impl Controller {
    pub fn init(cx: &mut App) -> Entity<Self> {
        let controller = cx.new(|_| Self {
            busy: None,
            busy_serial: 0,
            busy_owner: None,
            panels: Default::default(),
            settings_view: None,
            pending_settings_page: None,
            selection: None,
            pending_close: Vec::new(),
            recording: None,
            recording_serial: 0,
            scroll: None,
            scroll_serial: 0,
            scroll_editors: Vec::new(),
            watermark_editor: None,
            workflow_serial: 0,
            log_operation: None,
            ocr: OcrState::Idle,
            ocr_revision: 0,
            ocr_engine_revision: 0,
            ocr_engine: None,
            last_capture: None,
            clipboard_file: None,
            copy_serial: 0,
            last_display_id: None,
            pending_capture: None,
            queued_capture: None,
            pending_restore_app: None,
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
                    controller
                        .scroll_editors
                        .retain(|handle| handle.window_id() != window_id);
                    if controller
                        .watermark_editor
                        .is_some_and(|handle| handle.window_id() == window_id)
                    {
                        controller.watermark_editor = None;
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
        self.busy.is_some()
            || self.selection.is_some()
            || self.recording.is_some()
            || self.scroll.is_some()
    }

    /// Start an owned async phase and return the token its completion must use.
    /// A late callback can never clear a newer operation's busy state.
    fn begin_busy(&mut self, state: Busy) -> u64 {
        self.busy_serial = self.busy_serial.wrapping_add(1);
        let owner = self.busy_serial;
        self.busy = Some(state);
        self.busy_owner = Some(owner);
        owner
    }

    fn finish_busy(&mut self, owner: u64) -> bool {
        if self.busy_owner != Some(owner) {
            return false;
        }
        self.busy_owner = None;
        self.busy = None;
        true
    }

    /// Hand the capture/selection phase over to recording or scrolling, which
    /// manage `busy` directly. Any older callback keeps its stale token but can
    /// no longer release the new workflow.
    fn handoff_busy(&mut self) {
        self.busy_owner = None;
    }

    fn retire_log_operation(&mut self) {
        if let Some(id) = self.log_operation.take() {
            crate::logging::retire_operation(id);
        }
    }

    fn retire_log_operation_after_update(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.log_operation.take() {
            cx.defer(move |_| {
                crate::logging::retire_operation(id);
            });
        }
    }

    /// Release a selection/capture only if no export replaced its owner.
    fn finish_selection_busy(&mut self, capture_busy: Option<u64>) {
        if capture_busy.is_some_and(|owner| self.busy_owner == Some(owner))
            && let Some(owner) = capture_busy
        {
            self.finish_busy(owner);
        }
    }

    fn next_copy_serial(&mut self) -> u64 {
        self.copy_serial = self.copy_serial.wrapping_add(1);
        self.copy_serial
    }

    fn copy_is_current(&self, serial: u64) -> bool {
        self.copy_serial == serial
    }

    fn clear_clipboard_file(&mut self, serial: u64) -> Option<PathBuf> {
        self.copy_is_current(serial)
            .then(|| self.clipboard_file.take())
            .flatten()
    }

    fn replace_clipboard_file(&mut self, serial: u64, path: PathBuf) -> Option<PathBuf> {
        if !self.copy_is_current(serial) {
            return None;
        }
        self.clipboard_file.replace(path)
    }

    fn begin_ocr_attempt(&mut self) -> usize {
        self.ocr_revision += 1;
        self.ocr_revision
    }

    /// Claim an OCR result before any history or clipboard side effect. The
    /// revision advances immediately so duplicate/late completions are rejected.
    fn accept_ocr_result(&mut self, revision: usize) -> bool {
        if self.ocr_revision != revision {
            return false;
        }
        self.ocr_revision += 1;
        true
    }

    pub fn has_scroll(&self) -> bool {
        self.scroll.is_some()
    }

    pub fn is_finishing_scroll(&self) -> bool {
        self.scroll.as_ref().is_some_and(|active| active.finishing)
    }

    pub fn can_finish_scroll(&self) -> bool {
        self.scroll.as_ref().is_some_and(|active| {
            !active.finishing && active.snapshot.status != ScrollStatus::Failed
        })
    }

    pub fn is_scroll_preview_visible(&self) -> bool {
        self.scroll
            .as_ref()
            .is_some_and(|active| active.preview_visible)
    }

    pub fn scroll_snapshot(&self) -> Option<Arc<ScrollSnapshot>> {
        self.scroll.as_ref().map(|active| active.snapshot.clone())
    }

    pub fn set_scroll_preview_range_with_follow(&mut self, start: u32, rows: u32, follow: bool) {
        if let Some(session) = self
            .scroll
            .as_ref()
            .and_then(|active| active.session.as_ref())
        {
            session.set_preview_range_with_follow(start, rows, follow);
        }
    }

    pub fn scroll_annotations(&self) -> Option<&Layer> {
        self.scroll.as_ref().map(|active| &active.annotations)
    }

    pub fn is_scroll_annotating(&self) -> bool {
        self.scroll
            .as_ref()
            .is_some_and(|active| active.annotation_mode.is_some())
    }

    pub fn scroll_annotation_state(&self) -> Option<(i64, f32, Arc<RgbaImage>, u64)> {
        let active = self.scroll.as_ref()?;
        let mode = active.annotation_mode.as_ref()?;
        Some((
            mode.origin_y,
            active.scale,
            active.snapshot.pause_frame.clone()?,
            active.snapshot.pause_revision,
        ))
    }

    pub fn start_scroll_annotation_with_tool(
        &mut self,
        tool: crate::annotate::Tool,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.scroll.as_mut() else {
            return;
        };
        if active.finishing || !active.snapshot.started {
            return;
        }
        active.annotations.deselect();
        active.annotations.set_tool(tool);
        self.start_scroll_annotation(cx);
    }

    pub fn start_scroll_annotation(&mut self, cx: &mut Context<Self>) {
        let Some((scroll_id, frame, selection)) = self
            .scroll
            .as_ref()
            .map(|active| (active.id, active.frame.clone(), active.selection))
        else {
            return;
        };
        let Some(active) = self.scroll.as_mut() else {
            return;
        };
        if active.annotation_mode.is_some() || active.finishing || !active.snapshot.started {
            return;
        }
        let direction = annotation_auto_direction(&active.snapshot, active.auto_scroll_intent);
        if direction.is_none() {
            active.auto_scroll_intent = None;
        }
        active.annotation_auto_resume = false;
        active.annotation_mode = Some(ScrollAnnotationMode {
            origin_y: active.snapshot.viewport_origin_y,
            resume_auto: direction.is_some(),
            was_paused: active.snapshot.paused,
            direction: direction.unwrap_or(active.snapshot.auto_direction),
            requested_origin: None,
        });
        active.annotations.commit();
        if let Some(session) = active.session.as_ref() {
            session.set_paused(true);
        }
        Arc::make_mut(&mut active.snapshot).paused = true;
        Arc::make_mut(&mut active.snapshot).auto_scrolling = false;
        let bar_handle = active.bar.take();
        let controller = cx.entity();
        cx.defer(move |cx| {
            if let Some(bar_handle) = bar_handle
                && let Ok(bounds) = bar_handle.update(cx, |_, window, _| {
                    let bounds = window.bounds();
                    crate::overlay::close_window(window);
                    bounds
                })
            {
                controller.update(cx, |this, _| {
                    if let Some(active) = this.scroll.as_mut()
                        && active.id == scroll_id
                    {
                        active.saved_bar_bounds = Some(bounds);
                    }
                });
            }
            let bar_origin = {
                let this = controller.read(cx);
                let still_annotating = this.scroll.as_ref().is_some_and(|active| {
                    active.id == scroll_id && active.annotation_mode.is_some()
                });
                if !still_annotating {
                    return;
                }
                this.scroll_bar_local_origin()
            };
            if let Some(frame) = frame {
                frame
                    .window
                    .update(cx, |_, window, cx| {
                        frame.view.update(cx, |view, cx| {
                            view.enter_scroll_annotation(
                                controller.clone(),
                                selection,
                                bar_origin,
                                window,
                                cx,
                            )
                        });
                    })
                    .ok();
            }
        });
        cx.notify();
    }

    pub fn finish_scroll_annotation(&mut self, cx: &mut Context<Self>) {
        self.leave_scroll_annotation(true, true, cx);
    }

    fn leave_scroll_annotation(&mut self, resume: bool, reopen_bar: bool, cx: &mut Context<Self>) {
        let Some((id, frame, bar, mode)) = self.scroll.as_mut().and_then(|active| {
            let mode = active.annotation_mode.take()?;
            active.annotations.commit();
            Some((active.id, active.frame.clone(), active.bar, mode))
        }) else {
            return;
        };
        let keep_paused = resume && !mode.resume_auto && mode.was_paused;
        if !keep_paused && let Some(active) = self.scroll.as_mut() {
            Arc::make_mut(&mut active.snapshot).paused = false;
        }
        let controller = cx.entity();
        cx.defer(move |cx| {
            controller.update(cx, |this, cx| {
                let Some(active) = this.scroll.as_mut() else {
                    return;
                };
                if active.id != id || active.annotation_mode.is_some() {
                    return;
                }
                if let Some(frame) = frame {
                    frame
                        .window
                        .update(cx, |_, window, cx| {
                            frame
                                .view
                                .update(cx, |view, cx| view.exit_scroll_annotation(window, cx));
                        })
                        .ok();
                }
                let reopen_params = (reopen_bar && active.bar.is_none() && !active.finishing)
                    .then_some((
                        active.id,
                        active.display_id,
                        active.display_bounds,
                        active.selection,
                        active.saved_bar_bounds,
                    ));
                if let Some(bar) = bar {
                    bar.update(cx, |_, window, _| window.activate_window()).ok();
                }
                let mut resume_auto_error = None;
                if !keep_paused && let Some(session) = active.session.as_ref() {
                    session.set_paused(false);
                    if resume && mode.resume_auto {
                        match session.set_auto_scroll_direction(true, mode.direction) {
                            Ok(()) => {
                                active.auto_scroll_intent = Some(mode.direction);
                                let snap = Arc::make_mut(&mut active.snapshot);
                                snap.auto_direction = mode.direction;
                                snap.auto_scrolling = true;
                            }
                            Err(err) => {
                                active.auto_scroll_intent = None;
                                Arc::make_mut(&mut active.snapshot).auto_scrolling = false;
                                resume_auto_error = Some(err);
                            }
                        }
                    }
                }
                if let Some((scroll_id, display_id, display_bounds, selection, saved_bar_bounds)) =
                    reopen_params
                {
                    this.open_scroll_bar(
                        scroll_id,
                        display_id,
                        display_bounds,
                        selection,
                        saved_bar_bounds,
                        cx,
                    );
                }
                match resume_auto_error {
                    Some(AutoScrollError::NeedsAccessibility) => {
                        log::warn!("auto-scroll resume requested without accessibility permission");
                        permissions::request(Permission::Accessibility);
                        this.open_panel(Panel::Permissions, cx);
                        this.inform(t!("scroll.accessibility_needed").to_string(), cx);
                    }
                    Some(err) => {
                        this.fail(format!("{err}"), cx);
                    }
                    None => {}
                }
                log::info!(
                    "scroll annotation finished: resume_auto={} direction={:?} keep_paused={}",
                    resume && mode.resume_auto,
                    mode.direction,
                    keep_paused
                );
                cx.notify();
            });
        });
        cx.notify();
    }

    pub fn begin_scroll_annotation(
        &mut self,
        x: f32,
        y: f32,
        double_click: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.scroll.as_mut() else {
            return;
        };
        if active.finishing
            || (active.annotation_mode.is_some() && active.snapshot.pause_frame.is_none())
        {
            return;
        }
        if active.annotation_mode.is_none()
            && active.snapshot.auto_scrolling
            && !active.annotation_auto_resume
        {
            active.annotation_auto_resume = true;
            if let Some(session) = active.session.as_ref() {
                let _ = session.set_auto_scroll(false);
            }
            Arc::make_mut(&mut active.snapshot).auto_scrolling = false;
        }
        if active.annotations.select_at(x, y) {
            if double_click && active.annotations.begin_text_edit(x, y) {
                cx.notify();
                return;
            }
            active.annotations.begin_edit(x, y);
        } else {
            active.annotations.pointer_down(x, y);
        }
        cx.notify();
    }

    pub fn move_scroll_annotation(&mut self, x: f32, y: f32, cx: &mut Context<Self>) {
        let Some(active) = self.scroll.as_mut() else {
            return;
        };
        let viewport = if let Some(mode) = active.annotation_mode.as_ref() {
            [
                active.snapshot.width as f32 / active.scale,
                mode.origin_y as f32 / active.scale + active.selection.size.height.as_f32(),
            ]
        } else {
            [
                active.snapshot.width as f32 / active.scale,
                active.snapshot.height as f32 / active.scale,
            ]
        };
        let changed = if active.annotations.is_editing() {
            active.annotations.drag_edit(x, y, viewport)
        } else if active.annotations.is_dragging() {
            active.annotations.pointer_move(x, y);
            true
        } else {
            false
        };
        if changed {
            cx.notify();
        }
    }

    pub fn end_scroll_annotation(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.scroll.as_mut() else {
            return;
        };
        if active.annotations.is_editing() {
            active.annotations.end_edit();
        } else if active.annotations.is_dragging() {
            active.annotations.pointer_up();
        }
        if !active.annotations.is_typing() {
            Self::resume_scroll_annotation_auto(active);
        }
        cx.notify();
    }

    pub fn shift_scroll_annotation_origin(&mut self, delta: i64, cx: &mut Context<Self>) {
        let Some(active) = self.scroll.as_mut() else {
            return;
        };
        if delta == 0 {
            return;
        }
        let Some(mode) = active.annotation_mode.as_mut() else {
            return;
        };
        let Some(frame) = active.snapshot.pause_frame.as_ref() else {
            return;
        };
        let max_origin = (active.snapshot.height as i64 - frame.height() as i64).max(0);
        let base = mode
            .requested_origin
            .unwrap_or(active.snapshot.viewport_origin_y);
        let origin = (base + delta).clamp(0, max_origin);
        if origin == base {
            return;
        }
        mode.requested_origin = Some(origin);
        if let Some(session) = active.session.as_ref() {
            session.set_annotation_origin(origin);
        }
        cx.notify();
    }

    pub fn set_scroll_annotation_tool(
        &mut self,
        tool: crate::annotate::Tool,
        cx: &mut Context<Self>,
    ) {
        let Some(active) = self.scroll.as_mut() else {
            return;
        };
        active.annotations.deselect();
        active.annotations.set_tool(tool);
        Self::resume_scroll_annotation_auto(active);
        cx.notify();
    }

    pub fn undo_scroll_annotation(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.scroll.as_mut() else {
            return;
        };
        active.annotations.undo();
        Self::resume_scroll_annotation_auto(active);
        cx.notify();
    }

    pub fn clear_scroll_annotations(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.scroll.as_mut() else {
            return;
        };
        active.annotations.clear();
        Self::resume_scroll_annotation_auto(active);
        cx.notify();
    }

    pub fn delete_scroll_annotation(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.scroll.as_mut() else {
            return;
        };
        if active.annotations.delete_selected() {
            cx.notify();
        }
    }

    pub fn set_scroll_annotation_color(&mut self, color: u32, cx: &mut Context<Self>) {
        let Some(active) = self.scroll.as_mut() else {
            return;
        };
        active.annotations.set_color(color);
        cx.notify();
    }

    pub fn set_scroll_annotation_size(&mut self, size: f32, cx: &mut Context<Self>) {
        let Some(active) = self.scroll.as_mut() else {
            return;
        };
        active.annotations.set_size(size);
        cx.notify();
    }

    pub fn persist_scroll_annotation_defaults(&self, cx: &mut Context<Self>) {
        if let Some(defaults) = self.scroll_annotations().map(Layer::config) {
            AppSettings::update(cx, |config| config.annotations = defaults.clone());
        }
    }

    pub fn commit_scroll_annotation(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.scroll.as_mut() else {
            return;
        };
        active.annotations.commit();
        Self::resume_scroll_annotation_auto(active);
        cx.notify();
    }

    pub fn cancel_scroll_annotation_draft(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.scroll.as_mut() else {
            return;
        };
        active.annotations.cancel_draft();
        Self::resume_scroll_annotation_auto(active);
        cx.notify();
    }

    pub fn insert_scroll_annotation_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(active) = self.scroll.as_mut() {
            active.annotations.insert_text(text);
            cx.notify();
        }
    }

    pub fn backspace_scroll_annotation(&mut self, cx: &mut Context<Self>) {
        if let Some(active) = self.scroll.as_mut() {
            active.annotations.backspace();
            cx.notify();
        }
    }

    pub fn replace_scroll_annotation_text(
        &mut self,
        range: Option<std::ops::Range<usize>>,
        text: &str,
        cx: &mut Context<Self>,
    ) {
        if let Some(active) = self.scroll.as_mut() {
            active.annotations.replace_text(range, text);
            cx.notify();
        }
    }

    pub fn replace_and_mark_scroll_annotation_text(
        &mut self,
        range: Option<std::ops::Range<usize>>,
        text: &str,
        cx: &mut Context<Self>,
    ) {
        if let Some(active) = self.scroll.as_mut() {
            active.annotations.replace_and_mark_text(range, text);
            cx.notify();
        }
    }

    pub fn unmark_scroll_annotation_text(&mut self, cx: &mut Context<Self>) {
        if let Some(active) = self.scroll.as_mut() {
            active.annotations.unmark_text();
            cx.notify();
        }
    }

    pub fn scroll_annotation_scale(&self) -> Option<f32> {
        self.scroll.as_ref().map(|active| active.scale)
    }

    fn resume_scroll_annotation_auto(active: &mut ActiveScroll) {
        if active.annotation_mode.is_some() || !active.annotation_auto_resume {
            return;
        }
        active.annotation_auto_resume = false;
        if let Some(session) = active.session.as_ref()
            && session.set_auto_scroll(true).is_ok()
        {
            active.auto_scroll_intent = Some(active.snapshot.auto_direction);
            Arc::make_mut(&mut active.snapshot).auto_scrolling = true;
        }
    }

    fn sync_scroll_annotation_content(&mut self, snap: &ScrollSnapshot) {
        let Some(active) = self.scroll.as_mut() else {
            return;
        };
        Self::sync_scroll_annotation_shapes(
            &mut active.annotations,
            &mut active.annotation_content_shift,
            &mut active.annotation_reset_pending,
            active.scale,
            snap.content_shift,
        );
    }

    fn sync_scroll_annotation_shapes(
        annotations: &mut Layer,
        content_shift: &mut u32,
        reset_pending: &mut bool,
        scale: f32,
        shift: u32,
    ) {
        if *reset_pending {
            if shift == 0 {
                *reset_pending = false;
                *content_shift = 0;
            }
            return;
        }
        let delta = i64::from(shift) - i64::from(*content_shift);
        if delta != 0 {
            let scale = if scale.is_finite() && scale > 0.1 {
                scale
            } else {
                1.0
            };
            annotations.translate(0.0, delta as f32 / scale);
            *content_shift = shift;
        }
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
            HotkeyAction::Scroll => CaptureIntent::Scroll,
            HotkeyAction::Ocr => CaptureIntent::Recognize,
            HotkeyAction::Gif => CaptureIntent::Record(last_animation_format(cx)),
            HotkeyAction::Video => CaptureIntent::Record(RecordFormat::Mp4),
        };
        if self.recording.is_some() {
            self.queued_capture = Some(intent);
            self.stop_recording(cx);
            return;
        }
        if self.scroll.is_some() {
            if action != HotkeyAction::Scroll {
                self.queued_capture = Some(intent);
            }
            self.finish_scroll(ScrollFinishAction::Copy, cx);
            return;
        }
        self.start_capture(intent, cx);
    }

    pub fn reset_ocr_engine(&mut self) {
        self.ocr_engine_revision = self.ocr_engine_revision.wrapping_add(1);
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
        let revision = self.ocr_engine_revision;
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
                        if this.ocr_engine_revision == revision {
                            this.ocr_engine.get_or_insert(Arc::from(engine));
                        }
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
        // Copy/Save may still be encoding. Keep the request queued until that
        // operation releases its owned busy phase instead of consuming it with
        // a rejected start_capture call.
        if self.is_busy() || self.queued_capture.is_none() {
            return;
        }
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
        self.pending_restore_app = crate::overlay::frontmost_other_app();
        self.workflow_serial = self.workflow_serial.wrapping_add(1);
        self.log_operation = Some(crate::logging::begin_capture());
        log::info!("capture start: intent={intent:?}");
        let capture_busy = self.begin_busy(Busy::Capturing);
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
                if !this.finish_busy(capture_busy) {
                    // A newer workflow owns the app now. Tear down only this
                    // capture's pre-placed overlays, never the new selection.
                    this.abort_capture(placed.as_ref(), cx);
                    this.finish_pending_close(cx);
                    return;
                }
                let frames = match frames {
                    Ok(frames) => frames,
                    Err(err) => {
                        this.abort_capture(placed.as_ref(), cx);
                        this.fail(
                            t!("controller.capture_failed", error = format!("{err:#}")).to_string(),
                            cx,
                        );
                        this.retire_log_operation();
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

    fn restore_previous_app(&mut self) {
        if let Some(pid) = self.pending_restore_app.take() {
            crate::overlay::restore_frontmost_app(pid);
        }
    }

    /// Drop a pre-placed capture session whose freeze failed.
    fn abort_capture(&mut self, session: Option<&Entity<CaptureSession>>, cx: &mut Context<Self>) {
        if self
            .selection
            .as_ref()
            .is_some_and(|active| Some(&active.session) == session)
        {
            self.restore_previous_app();
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
            self.retire_log_operation();
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
            self.retire_log_operation();
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
                    .update(cx, |_, window, _| crate::overlay::close_window(window))
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
        #[cfg(not(target_os = "macos"))]
        cx.activate(true);
        if kept.is_empty() {
            self.close_selection(cx);
            self.finish_pending_close(cx);
            self.fail(t!("controller.overlay_failed").to_string(), cx);
            self.retire_log_operation();
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
        let capture_busy = self.busy_owner;
        let wants_still = matches!(
            command,
            SessionCommand::Copy
                | SessionCommand::Save
                | SessionCommand::Pin
                | SessionCommand::Recognize
        );
        let watermark_config = AppSettings::get(cx).watermark.clone();
        let screenshot_target_enabled =
            watermark_config.target_enabled(WatermarkTarget::Screenshot);
        let watermark = if wants_still && screenshot_target_enabled {
            match watermark_config
                .preset(WatermarkTarget::Screenshot)
                .context("watermark template is unavailable")
                .and_then(|preset| {
                    crate::export::watermark_spec(preset, AppSettings::watermark_dir(cx))
                }) {
                Ok(spec) => {
                    log::info!("watermark: enabled for screenshot export");
                    spec
                }
                Err(err) => {
                    self.fail(
                        t!("controller.watermark_failed", error = format!("{err:#}")).to_string(),
                        cx,
                    );
                    return;
                }
            }
        } else {
            log::info!(
                "watermark: skipped for screenshot export (template={:?})",
                watermark_config.selected_template_id(WatermarkTarget::Screenshot)
            );
            WatermarkSpec::default()
        };
        let multi = active.session.read(cx).multi_selection();
        let composed = active.session.read(cx).is_composed();
        let bake = matches!(
            command,
            SessionCommand::Copy | SessionCommand::Save | SessionCommand::Pin
        );
        let edit_mode = matches!(command, SessionCommand::Edit);
        // Every durable export (Save, Copy, history via remember, and Pin) plus
        // OCR (which also indexes into history) and Crop & Edit must see the
        // same HDR frame so all outputs stay consistent. Cancel/Record need no
        // still HDR crop.
        let want_hdr = matches!(
            command,
            SessionCommand::Copy
                | SessionCommand::Save
                | SessionCommand::Pin
                | SessionCommand::Recognize
                | SessionCommand::Edit
        );
        let (image, record_target) = active.session.update(cx, |session, _| {
            if bake || edit_mode {
                session.finish_annotations();
            }
            let hdr = if want_hdr { session.export_hdr() } else { None };
            let image = session.crop_selections().map(|(image, _bounds, scale)| {
                let shapes = ((bake || edit_mode)
                    && !multi
                    && !session.annotations().committed().is_empty())
                .then(|| Arc::new(session.annotations().committed().to_vec()));
                Captured::new(image, hdr, scale, shapes, watermark.clone())
            });
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
        // A record or scroll command keeps the active overlay: it becomes the
        // live indicator outline instead of closing and opening another window.
        let record_now = matches!(command, SessionCommand::Record(_))
            && !composed
            && image.is_some()
            && record_target.is_some();
        let scroll_now = matches!(command, SessionCommand::Scroll)
            && !composed
            && !multi
            && image.is_some()
            && record_target.is_some();
        if !record_now && !scroll_now {
            if matches!(
                command,
                SessionCommand::Cancel | SessionCommand::Copy | SessionCommand::Pin
            ) || image.is_none()
            {
                self.restore_previous_app();
            } else if matches!(command, SessionCommand::Recognize | SessionCommand::Edit) {
                self.pending_restore_app = None;
            }
            self.close_selection(cx);
        } else {
            // Recording/scrolling set their own busy phase synchronously.
            self.handoff_busy();
        }
        log::info!(
            target: "timing",
            "command {command:?}: bake+crop {:.1} ms, overlays closed at {:.1} ms",
            ms(exported),
            ms(started.elapsed())
        );

        let async_still =
            matches!(command, SessionCommand::Copy | SessionCommand::Save) && image.is_some();
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
                self.open_pin_async(captured, placement, cx);
            }
            (SessionCommand::Recognize, Some(captured)) => self.recognize(captured, cx),
            (SessionCommand::Edit, Some(captured)) => {
                let display_id = record_target
                    .filter(|_| !multi)
                    .and_then(|(_, _, display_id, _, _)| display_id);
                let image = (*captured.clean_sdr).clone();
                let hdr = captured.composition.hdr_source().cloned();
                let shapes = captured.composition.shapes().clone();
                self.open_image_editor(image, hdr, captured.scale, display_id, shapes, true, cx);
            }
            (SessionCommand::Scroll, Some(captured)) => {
                if scroll_now && let Some(target) = record_target {
                    self.start_scroll(target, captured.scale, cx);
                }
            }
            (SessionCommand::Record(format), Some(_)) => {
                if record_now && let Some(target) = record_target {
                    self.start_recording(target, format, cx);
                }
            }
            (_, None) => self.fail(t!("controller.selection_empty").to_string(), cx),
        }
        // Copy/Save replace the capture owner with an Encoding owner. Cancel,
        // Pin, Recognize, and rejected commands explicitly release the capture
        // phase; closing the selection itself is not a state transition.
        if !record_now && !scroll_now {
            self.finish_selection_busy(capture_busy);
            if !async_still {
                self.retire_log_operation();
            }
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
                    .update(cx, |_, window, _| crate::overlay::close_window(window))
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
                .update(cx, |_, window, _| crate::overlay::close_window(window))
                .ok();
        }
    }

    /// Keep a capture for "pin last capture" and auto-save it to history.
    pub(crate) fn remember(&mut self, captured: &Captured, cx: &mut Context<Self>) {
        self.last_capture = Some(captured.clone());
        HistoryStore::global(cx).update(cx, |history, cx| {
            history.add_image(captured.composition.clone(), captured.scale, None, cx)
        });
    }

    pub(crate) fn remember_and_copy(&mut self, captured: Captured, cx: &mut Context<Self>) {
        self.remember(&captured, cx);
        self.copy_image(captured, cx);
    }

    pub(crate) fn remember_and_save(&mut self, captured: Captured, cx: &mut Context<Self>) {
        self.remember(&captured, cx);
        self.save_image(captured, cx);
    }

    pub(crate) fn remember_and_pin(&mut self, captured: Captured, cx: &mut Context<Self>) {
        self.remember(&captured, cx);
        self.open_pin_async(captured, None, cx);
    }

    pub(crate) fn recognize_captured(&mut self, captured: Captured, cx: &mut Context<Self>) {
        self.recognize(captured, cx);
    }

    fn open_pin_async(
        &mut self,
        captured: Captured,
        placement: Option<pin::Placement>,
        cx: &mut Context<Self>,
    ) {
        let controller = cx.entity();
        let workflow = self.workflow_serial;
        cx.spawn(async move |this, cx| {
            let composition = captured.composition.clone();
            let image = cx
                .background_spawn(async move { pin::image_source(&composition) })
                .await;
            this.update(cx, |this, cx| {
                // `controller` and `this` are the same entity, which is leased
                // for this update. Reading it here panics with
                // "cannot read Controller while it is already being updated".
                if this.workflow_serial != workflow {
                    return;
                }
                match image {
                    Ok(image) => {
                        let controller = controller.clone();
                        cx.defer(move |cx| {
                            if controller.read(cx).workflow_serial == workflow {
                                pin::open(captured, image, placement, cx);
                            }
                        });
                    }
                    Err(err) => {
                        this.finish_pending_close(cx);
                        this.fail(t!("controller.pin_failed", error = err).to_string(), cx);
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Pin the most recent capture of this session, else the newest image in history.
    pub fn pin_last(&mut self, cx: &mut Context<Self>) {
        if let Some(captured) = self.last_capture.clone() {
            self.open_pin_async(captured, None, cx);
            return;
        }
        let latest = {
            let store = HistoryStore::global(cx).read(cx);
            store.entries().next().map(|entry| {
                let (path, scale) = store.pin_source(entry);
                (path, scale, entry.hdr)
            })
        };
        match latest {
            Some((path, scale, hdr)) => pin::open_file_with_hdr(path, scale, hdr, cx),
            None => self.fail(t!("controller.pin_nothing").to_string(), cx),
        }
    }

    pub(crate) fn copy_image(&mut self, captured: Captured, cx: &mut Context<Self>) {
        let busy = self.begin_busy(Busy::Encoding);
        let copy_serial = self.next_copy_serial();
        let (width, height) = captured.clean_sdr.dimensions();
        let started = Instant::now();
        let clipboard_dir = snapit_store::AppDirs::discover()
            .map(|dirs| dirs.clipboard_dir())
            .unwrap_or_else(|_| std::env::temp_dir().join("snapit").join("clipboard"));
        cx.spawn(async move |this, cx| {
            let encoded = Instant::now();
            let payload = cx
                .background_spawn(async move {
                    let started = Instant::now();
                    let hdr = captured.composition.has_hdr_original();
                    let payload = if captured.composition.clipboard_uses_file() {
                        log::info!("copy: clipboard disk-backed SDR PNG file reference");
                        captured
                            .composition
                            .write_clipboard_png(&clipboard_dir)
                            .map(ClipboardPayload::File)
                    } else {
                        if hdr {
                            log::info!("copy: clipboard HDR PNG (16-bit BT.2020/PQ)");
                        } else {
                            log::info!("copy: clipboard composed SDR PNG");
                        }
                        captured.composition.png().map(|png| {
                            ClipboardPayload::Image(
                                Arc::try_unwrap(png).unwrap_or_else(|png| png.as_ref().clone()),
                            )
                        })
                    };
                    log::info!(
                        target: "timing",
                        "copy: {width}x{height} PNG encode {:.1} ms",
                        ms(started.elapsed())
                    );
                    payload
                })
                .await;
            let encode = encoded.elapsed();
            this.update(cx, |this, cx| {
                if !this.copy_is_current(copy_serial) {
                    // A newer copy owns the pasteboard and managed backing file.
                    // This worker may clean only the path it generated itself.
                    if let Ok(ClipboardPayload::File(path)) = payload {
                        clipboard::remove_clipboard_file(&path);
                    }
                    if this.finish_busy(busy) {
                        this.retire_log_operation();
                    }
                    return;
                }
                let writing = Instant::now();
                let result = match payload {
                    Ok(ClipboardPayload::Image(png)) => {
                        if let Some(previous) = this.clear_clipboard_file(copy_serial) {
                            clipboard::remove_clipboard_file(&previous);
                        }
                        cx.write_to_clipboard(ClipboardItem::new_image(&Image::from_bytes(
                            ImageFormat::Png,
                            png,
                        )));
                        Ok("image data")
                    }
                    Ok(ClipboardPayload::File(path)) => {
                        let copied = clipboard::copy_file_reference(&path, cx)
                            .map_err(|error| format!("{error:#}"));
                        if copied.is_ok() {
                            if let Some(previous) =
                                this.replace_clipboard_file(copy_serial, path.clone())
                            {
                                clipboard::remove_clipboard_file(&previous);
                            }
                        } else {
                            clipboard::remove_clipboard_file(&path);
                        }
                        copied.map(|kind| match kind {
                            clipboard::FileCopy::File => "file reference",
                            clipboard::FileCopy::PathText => "path text",
                        })
                    }
                    Err(err) => Err(err),
                };
                match result {
                    Ok(mode) => {
                        log::info!(
                            target: "timing",
                            "copy: clipboard filled as {mode} {:.1} ms after the command (worker result at {:.1} ms), write {:.1} ms",
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
                if this.finish_busy(busy) {
                    this.retire_log_operation();
                }
                this.finish_pending_close(cx);
                if this.busy.is_none() {
                    this.resume_queued_capture(cx);
                }
            })
            .ok();
            log::debug!(target: "timing", "copy command handled in {:.1} ms", ms(started.elapsed()));
        })
        .detach();
    }

    pub(crate) fn save_image(&mut self, captured: Captured, cx: &mut Context<Self>) {
        let busy = self.begin_busy(Busy::Encoding);
        let Captured { composition, .. } = captured;
        let capture = &AppSettings::get(cx).capture;
        let (format, quality) = (capture.image_format, capture.jpeg_quality);
        let name = format!("SnapIt {}.{}", timestamp(), format.extension());
        #[cfg(target_os = "macos")]
        crate::overlay::activate_app_single_window();
        #[cfg(not(target_os = "macos"))]
        cx.activate(true);
        let path = cx.prompt_for_new_path(&save_dir(cx), Some(&name));
        cx.spawn(async move |this, cx| {
            let Some(mut path) = await_path(path, &this, cx).await else {
                // Save dialog dismissed: nothing will follow to flush the
                // hidden overlays, so release them and the busy phase here.
                this.update(cx, |this, cx| {
                    this.restore_previous_app();
                    if this.finish_busy(busy) {
                        this.retire_log_operation();
                    }
                    this.finish_pending_close(cx);
                    if this.busy.is_none() {
                        this.resume_queued_capture(cx);
                    }
                })
                .ok();
                return;
            };
            let format = SaveFormat::from_path(&path).unwrap_or_else(|| {
                path.set_extension(format.extension());
                format
            });
            log::info!("save image: format={format:?} path={}", path.display());
            let saved = cx
                .background_spawn(async move {
                    let writing = Instant::now();
                    let hdr_available = composition.has_hdr_original();
                    let branch = match (format, hdr_available) {
                        (SaveFormat::Png, true) => "HDR PNG (16-bit + cICP)",
                        (SaveFormat::Png, false) => "SDR PNG (8-bit)",
                        (SaveFormat::Jpg, _) => "JPEG (8-bit)",
                    };
                    log::info!("save branch: {branch} hdr_available={hdr_available}");
                    let result = match (format, hdr_available) {
                        (SaveFormat::Png, true) | (SaveFormat::Png, false) => {
                            composition.write_png(&path).map_err(anyhow::Error::msg)
                        }
                        (SaveFormat::Jpg, _) => composition
                            .write_jpeg(&path, quality)
                            .map_err(anyhow::Error::msg),
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
                this.restore_previous_app();
                if this.finish_busy(busy) {
                    this.retire_log_operation();
                }
                this.finish_pending_close(cx);
                if this.busy.is_none() {
                    this.resume_queued_capture(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn recognize(&mut self, captured: Captured, cx: &mut Context<Self>) {
        self.last_capture = Some(captured.clone());
        let Captured {
            clean_sdr,
            composition,
            scale,
            ..
        } = captured;
        self.ocr = OcrState::Running;
        let revision = self.begin_ocr_attempt();
        self.open_panel(Panel::Ocr, cx);
        let engine = self.ocr_engine.clone();
        let options = OcrOptions::default().with_languages(settings_view::ocr_languages(
            &AppSettings::get(cx).ocr.language,
        ));
        let engine_revision = self.ocr_engine_revision;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let engine: Arc<dyn OcrEngine> = match engine {
                        Some(engine) => engine,
                        None => Arc::from(snapit_ocr::engine_with(&options)?),
                    };
                    let blocks = snapit_ocr::recognize_rows(
                        engine.as_ref(),
                        clean_sdr.width(),
                        clean_sdr.height(),
                        |start, rows| clean_sdr.read_rows(start, rows),
                    )?;
                    Ok::<_, anyhow::Error>((engine, blocks))
                })
                .await;
            this.update(cx, |this, cx| {
                if !this.accept_ocr_result(revision) {
                    // Superseded (or already completed): no history, clipboard,
                    // or OCR panel update for the stale attempt.
                    return;
                }
                let text = result
                    .as_ref()
                    .ok()
                    .map(|(_, blocks)| snapit_ocr::blocks_to_text(blocks));
                HistoryStore::global(cx).update(cx, |history, cx| {
                    history.add_image(composition, scale, text.clone(), cx)
                });
                this.ocr = match result {
                    Ok((engine, _)) => {
                        let name = engine.name();
                        if this.ocr_engine_revision == engine_revision {
                            this.ocr_engine = Some(engine);
                        }
                        let text: SharedString = text.unwrap_or_default().into();
                        if !text.is_empty() {
                            cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
                        }
                        OcrState::Done { text, engine: name }
                    }
                    Err(err) => OcrState::Failed(format!("{err:#}").into()),
                };
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
        self.log_operation = Some(crate::logging::begin_recording());
        log::info!("recording start: format={format:?} display={monitor_id} region={region:?}");
        self.busy = Some(Busy::Capturing);
        self.last_display_id = display_id;
        let recording = AppSettings::get(cx).recording.clone();
        let watermark_config = AppSettings::get(cx).watermark.clone();
        let (watermark_target, watermark_target_name) = match format {
            RecordFormat::Gif | RecordFormat::Apng => (WatermarkTarget::Animation, "animation"),
            RecordFormat::Mp4 => (WatermarkTarget::Video, "video"),
        };
        let watermark_target_enabled = watermark_config.target_enabled(watermark_target);
        let watermark = if watermark_target_enabled {
            match watermark_config
                .preset(watermark_target)
                .context("watermark template is unavailable")
                .and_then(|preset| {
                    crate::export::watermark_spec(preset, AppSettings::watermark_dir(cx))
                }) {
                Ok(spec) => {
                    log::info!("watermark: enabled for {watermark_target_name} recording");
                    spec
                }
                Err(err) => {
                    self.busy = None;
                    self.fail(
                        t!("controller.watermark_failed", error = err).to_string(),
                        cx,
                    );
                    return;
                }
            }
        } else {
            log::info!(
                "watermark: skipped for {watermark_target_name} recording (template={:?})",
                watermark_config.selected_template_id(watermark_target)
            );
            WatermarkSpec::default()
        };
        // GIF and APNG share the animation rate; MP4 can afford a smoother one.
        let fps = match format {
            RecordFormat::Gif | RecordFormat::Apng => recording.gif_fps,
            RecordFormat::Mp4 => recording.mp4_fps,
        };
        let (output, dest) = recording_paths(cx, format);
        // Keep the per-display gate used by still capture; this runs on the main
        // thread because macOS HDR capability comes from NSScreen.
        let hdr_requested = recording.hdr && snapit_capture::hdr_candidates().contains(&monitor_id);
        let options = RecordOptions::default()
            .with_fps(fps)
            .with_max_duration(Duration::from_secs(recording.max_duration_secs.into()))
            .with_format(format)
            .with_cursor(recording.captures_cursor)
            .with_click_effects(recording.click_effects)
            .with_click_effect(ClickEffect::parse(&recording.click_effect).unwrap_or_default())
            // The recorder ignores this for GIF/APNG, which have no extended range.
            .with_hdr(hdr_requested)
            .with_mp4_bitrate_kbps(recording.mp4_bitrate_kbps)
            .with_watermark(watermark);
        log::info!(
            "recording options: fps={fps} hdr_requested={hdr_requested} captures_cursor={} \
             click_effects={} click_effect={:?} mp4_bitrate_kbps={:?} watermark={}",
            recording.captures_cursor,
            recording.click_effects,
            options.click_effect,
            options.mp4_bitrate_kbps,
            options.watermark.is_enabled()
        );
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
        self.workflow_serial = self.workflow_serial.wrapping_add(1);
        self.recording_serial = self.recording_serial.wrapping_add(1);
        let recording_id = self.recording_serial;
        self.recording = Some(ActiveRecording {
            id: recording_id,
            recorder: None,
            start: Some(RecorderStart::default()),
            bar: None,
            frame: Some(overlay.window),
            started: false,
            format,
            dest,
            part: output.clone(),
        });
        self.open_recording_bar(recording_id, display_id, display_bounds, selection, cx);
        let controller = cx.entity();
        let entered = overlay.window.update(cx, |_, window, cx| {
            overlay
                .view
                .update(cx, |view, cx| view.enter_recording(selection, window, cx));
            after_presented_frame(window, move |_, cx| {
                controller.update(cx, |this, cx| {
                    if this
                        .recording
                        .as_ref()
                        .is_some_and(|active| active.id == recording_id)
                    {
                        let start = this
                            .recording
                            .as_ref()
                            .and_then(|active| active.start.clone())
                            .expect("current recording has a start token");
                        this.spawn_recorder(
                            recording_id,
                            (monitor_id, region),
                            options,
                            output,
                            start,
                            cx,
                        );
                    }
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
                    .update(cx, |_, window, _| crate::overlay::close_window(window))
                    .ok();
            }
        }
        kept
    }

    fn spawn_recorder(
        &mut self,
        recording_id: u64,
        (monitor_id, region): (u32, snapit_capture::Region),
        options: RecordOptions,
        output: PathBuf,
        start: RecorderStart,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let path = output.clone();
            let recorder = cx
                .background_spawn(async move {
                    Recorder::start_with_cancel(monitor_id, region, options, output, start)
                })
                .await;
            this.update(cx, |this, cx| {
                let Some(active) = this.recording.as_mut() else {
                    // Cancelled or superseded while the platform recorder warmed up.
                    discard_stale_recorder(recorder, path);
                    return;
                };
                if active.id != recording_id {
                    discard_stale_recorder(recorder, path);
                    return;
                }
                this.busy = None;
                match recorder {
                    Ok(recorder) => {
                        let Some(active) = this.recording.as_mut() else {
                            return;
                        };
                        active.start = None;
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
        recording_id: u64,
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
                controller.update(cx, |this, cx| {
                    if this
                        .recording
                        .as_ref()
                        .is_some_and(|active| active.id == recording_id)
                    {
                        this.cancel_recording(cx);
                        this.fail(
                            t!("controller.record_start_failed", error = "recording bar")
                                .to_string(),
                            cx,
                        );
                    }
                });
                return;
            };
            let orphaned = controller.update(cx, |this, _| match this.recording.as_mut() {
                Some(active) if active.id == recording_id => {
                    active.bar = Some(handle);
                    false
                }
                _ => true,
            });
            if orphaned {
                handle
                    .update(cx, |_, window, _| crate::overlay::close_window(window))
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
        if let Some(start) = active.start.take() {
            start.cancel();
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
            if let Some(start) = active.start.take() {
                start.cancel();
            }
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
                    // Read finished animation bytes off the main thread; large
                    // recordings would otherwise stall a frame while copying.
                    let image = matches!(format, RecordFormat::Gif | RecordFormat::Apng)
                        .then(|| std::fs::read(&path).ok())
                        .flatten();
                    Ok::<_, anyhow::Error>((path, image))
                })
                .await;
            let published = publishing.elapsed();

            let preview = summary.first_frame.clone();
            this.update(cx, |this, cx| {
                match saved {
                    Ok((path, image)) => {
                        // Fill the clipboard before the bar closes: a paste
                        // right after completion must find this recording, not
                        // whatever was copied last.
                        let filling = Instant::now();
                        let copied = match (summary.format, image) {
                            (RecordFormat::Gif, Some(bytes)) => {
                                cx.write_to_clipboard(ClipboardItem::new_image(
                                    &Image::from_bytes(ImageFormat::Gif, bytes),
                                ));
                                Ok(clipboard::FileCopy::File)
                            }
                            (RecordFormat::Apng, Some(bytes)) => {
                                cx.write_to_clipboard(ClipboardItem::new_image(
                                    &Image::from_bytes(ImageFormat::Png, bytes),
                                ));
                                Ok(clipboard::FileCopy::File)
                            }
                            (RecordFormat::Gif | RecordFormat::Apng, None) => {
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
        self.restore_previous_app();
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
                        .update(cx, |_, window, _| crate::overlay::close_window(window))
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
        self.retire_log_operation_after_update(cx);
        cx.notify();
    }

    fn start_scroll(
        &mut self,
        (monitor_id, region, display_id, display_bounds, selection): (
            u32,
            snapit_capture::Region,
            Option<gpui_kit::DisplayId>,
            Bounds<Pixels>,
            Bounds<Pixels>,
        ),
        scale: f32,
        cx: &mut Context<Self>,
    ) {
        self.log_operation = Some(crate::logging::begin_scroll());
        log::info!("scroll start: display={monitor_id} region={region:?} scale={scale}");
        self.busy = Some(Busy::Capturing);
        self.last_display_id = display_id;
        let options = ScrollOptions::default().with_scale(scale);
        let Some(overlay) = self.take_recording_overlay(display_id, cx) else {
            self.busy = None;
            self.fail(
                t!(
                    "controller.scroll_start_failed",
                    error = "overlay window is gone"
                )
                .to_string(),
                cx,
            );
            self.retire_log_operation();
            return;
        };
        self.workflow_serial = self.workflow_serial.wrapping_add(1);
        self.scroll_serial = self.scroll_serial.wrapping_add(1);
        let scroll_id = self.scroll_serial;
        let overlay_handle = overlay.window;
        let overlay_view = overlay.view.clone();
        self.scroll = Some(ActiveScroll {
            id: scroll_id,
            session: None,
            start: Some(ScrollStart::default()),
            bar: None,
            saved_bar_bounds: None,
            preview: None,
            frame: Some(overlay),
            display_id,
            display_bounds,
            selection,
            scale,
            snapshot: Arc::new(ScrollSnapshot::default()),
            annotations: Layer::new(AppSettings::get(cx).annotations.clone()),
            annotation_content_shift: 0,
            annotation_reset_pending: false,
            annotation_auto_resume: false,
            auto_scroll_intent: None,
            annotation_mode: None,
            preview_visible: true,
            finishing: false,
            pending_finish: None,
            last_warning: false,
        });
        self.open_scroll_bar(scroll_id, display_id, display_bounds, selection, None, cx);
        self.open_scroll_preview(scroll_id, display_id, display_bounds, selection, cx);
        let controller = cx.entity();
        let entered = overlay_handle.update(cx, |_, window, cx| {
            overlay_view.update(cx, |view, cx| view.enter_recording(selection, window, cx));
            after_presented_frame(window, move |_, cx| {
                controller.update(cx, |this, cx| {
                    if this
                        .scroll
                        .as_ref()
                        .is_some_and(|active| active.id == scroll_id)
                    {
                        let start = this
                            .scroll
                            .as_ref()
                            .and_then(|active| active.start.clone())
                            .expect("current scroll capture has a start token");
                        this.spawn_scroll(scroll_id, (monitor_id, region), options, start, cx);
                    }
                });
            });
        });
        if entered.is_err() {
            self.busy = None;
            self.finish_scroll_ui(true, cx);
            self.fail(
                t!("controller.scroll_start_failed", error = "overlay").to_string(),
                cx,
            );
            self.retire_log_operation();
        }
    }

    fn spawn_scroll(
        &mut self,
        scroll_id: u64,
        (monitor_id, region): (u32, snapit_capture::Region),
        options: ScrollOptions,
        start: ScrollStart,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let session = cx
                .background_spawn(async move {
                    ScrollSession::start_with_cancel(monitor_id, region, options, start)
                })
                .await;
            this.update(cx, |this, cx| {
                let Some(active) = this.scroll.as_mut() else {
                    if let Ok(session) = session {
                        discard_stale_scroll(session);
                    }
                    return;
                };
                if active.id != scroll_id {
                    if let Ok(session) = session {
                        discard_stale_scroll(session);
                    }
                    return;
                }
                this.busy = None;
                match session {
                    Ok(session) => {
                        let snap = session.snapshot();
                        active.start = None;
                        active.snapshot = snap;
                        active.session = Some(session);
                        let pending_finish = active.pending_finish.take();
                        cx.notify();
                        Self::tick_scroll(scroll_id, cx);
                        if let Some(action) = pending_finish {
                            this.finish_scroll(action, cx);
                        }
                    }
                    Err(err) => {
                        this.finish_scroll_ui(true, cx);
                        this.fail(
                            t!("controller.scroll_start_failed", error = format!("{err:#}"))
                                .to_string(),
                            cx,
                        );
                        this.retire_log_operation();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn tick_scroll(scroll_id: u64, cx: &mut Context<Self>) {
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
                cx.background_executor().timer(SCROLL_TICK).await;
                let keep_ticking = this.update(cx, |this, cx| {
                    let (changed, warn_changed, warn_now, frame, snap) = {
                        let Some(active) = this.scroll.as_mut() else {
                            return false;
                        };
                        if active.id != scroll_id || active.finishing {
                            return false;
                        }
                        let Some(session) = active.session.as_ref() else {
                            return false;
                        };
                        let snap = session.snapshot();
                        let warn_now = snap.warning.is_some();
                        let changed = snap.preview_revision != active.snapshot.preview_revision
                            || snap.status != active.snapshot.status
                            || snap.warning != active.snapshot.warning
                            || snap.auto_scrolling != active.snapshot.auto_scrolling
                            || snap.direction != active.snapshot.direction
                            || snap.auto_direction != active.snapshot.auto_direction
                            || snap.paused != active.snapshot.paused
                            || snap.height != active.snapshot.height
                            || snap.frame_height != active.snapshot.frame_height
                            || snap.preview_range != active.snapshot.preview_range
                            || snap.content_shift != active.snapshot.content_shift
                            || snap.viewport_origin_y != active.snapshot.viewport_origin_y
                            || snap.pause_revision != active.snapshot.pause_revision
                            || snap.pause_frame.is_some() != active.snapshot.pause_frame.is_some()
                            || snap.started != active.snapshot.started;
                        let warn_changed = warn_now != active.last_warning;
                        active.last_warning = warn_now;
                        if snap.auto_scrolling {
                            active.auto_scroll_intent = Some(snap.auto_direction);
                        } else if snap.status == ScrollStatus::ReachedEnd {
                            active.auto_scroll_intent = None;
                        }
                        active.snapshot = snap.clone();
                        if let Some(mode) = active.annotation_mode.as_mut()
                            && snap.paused
                            && snap.pause_frame.is_some()
                        {
                            mode.origin_y = snap.viewport_origin_y;
                        }
                        let frame = active.frame.clone();
                        (changed, warn_changed, warn_now, frame, snap)
                    };
                    this.sync_scroll_annotation_content(&snap);
                    if warn_changed && let Some(frame) = frame {
                        let view = frame.view.clone();
                        frame
                            .window
                            .update(cx, |_, _, cx| {
                                view.update(cx, |view, cx| view.set_live_warning(warn_now, cx));
                            })
                            .ok();
                    }
                    if changed {
                        cx.notify();
                    }
                    if snap.status == ScrollStatus::Failed {
                        let error = snap
                            .error
                            .clone()
                            .unwrap_or_else(|| "scroll capture failed".to_string());
                        this.cancel_scroll(cx);
                        this.fail(
                            t!("controller.scroll_failed", error = error).to_string(),
                            cx,
                        );
                        this.retire_log_operation();
                        return false;
                    }
                    true
                });
                if !matches!(keep_ticking, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    fn open_scroll_bar(
        &mut self,
        scroll_id: u64,
        display_id: Option<gpui_kit::DisplayId>,
        display_bounds: Bounds<Pixels>,
        selection: Bounds<Pixels>,
        override_bounds: Option<Bounds<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        let mut bar = scroll_bar_bounds(display_bounds, selection);
        if let Some(saved) = override_bounds {
            let min_x = display_bounds.origin.x;
            let max_x =
                (display_bounds.origin.x + display_bounds.size.width - bar.size.width).max(min_x);
            let min_y = display_bounds.origin.y;
            let max_y = (display_bounds.origin.y + display_bounds.size.height
                - px(crate::scroll_bar::SCROLL_BAR_EXPANDED_HEIGHT))
            .max(min_y);
            bar.origin = gpui_kit::point(
                saved.origin.x.max(min_x).min(max_x),
                saved.origin.y.max(min_y).min(max_y),
            );
        }
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bar)),
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
        cx.defer(move |cx| {
            let view_controller = controller.clone();
            let opened = cx
                .open_window(options, |window, cx| {
                    cx.new(|cx| ScrollBar::new(view_controller, window, cx))
                })
                .map(AnyWindowHandle::from)
                .inspect_err(|err| log::error!("failed to open scroll bar: {err:#}"));
            let Ok(handle) = opened else {
                controller.update(cx, |this, cx| {
                    if this.scroll.as_ref().is_some_and(|a| a.id == scroll_id) {
                        this.cancel_scroll(cx);
                        this.fail(
                            t!("controller.scroll_start_failed", error = "scroll bar").to_string(),
                            cx,
                        );
                    }
                });
                return;
            };
            let orphaned = controller.update(cx, |this, _| match this.scroll.as_mut() {
                Some(active) if active.id == scroll_id && active.annotation_mode.is_none() => {
                    active.bar = Some(handle);
                    false
                }
                _ => true,
            });
            if orphaned {
                handle
                    .update(cx, |_, window, _| crate::overlay::close_window(window))
                    .ok();
            }
        });
    }

    fn open_scroll_preview(
        &mut self,
        scroll_id: u64,
        display_id: Option<gpui_kit::DisplayId>,
        display_bounds: Bounds<Pixels>,
        selection: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let controller = cx.entity();
        cx.defer(move |cx| {
            let (bar_handle, saved_bar_bounds) = {
                let this = controller.read(cx);
                let Some(active) = this
                    .scroll
                    .as_ref()
                    .filter(|active| active.id == scroll_id && active.preview_visible)
                else {
                    return;
                };
                (active.bar, active.saved_bar_bounds)
            };
            let live_bar_bounds = bar_handle
                .and_then(|handle| handle.update(cx, |_, window, _| window.bounds()).ok());
            let bar_bounds = live_bar_bounds
                .or(saved_bar_bounds)
                .unwrap_or_else(|| scroll_bar_bounds(display_bounds, selection));
            let bounds = scroll_preview_bounds(display_bounds, selection, bar_bounds);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: None,
                focus: false,
                show: true,
                kind: WindowKind::PopUp,
                is_movable: true,
                is_resizable: false,
                is_minimizable: false,
                display_id,
                window_background: WindowBackgroundAppearance::Transparent,
                ..Default::default()
            };
            let view_controller = controller.clone();
            let opened = cx
                .open_window(options, |window, cx| {
                    cx.new(|cx| ScrollPreview::new(view_controller, window, cx))
                })
                .map(AnyWindowHandle::from)
                .inspect_err(|err| log::error!("failed to open scroll preview: {err:#}"));
            let Ok(handle) = opened else {
                return;
            };
            let orphaned = controller.update(cx, |this, _| match this.scroll.as_mut() {
                Some(active) if active.id == scroll_id && active.preview_visible => {
                    active.preview = Some(handle);
                    false
                }
                _ => true,
            });
            if orphaned {
                handle
                    .update(cx, |_, window, _| crate::overlay::close_window(window))
                    .ok();
            }
        });
    }

    pub fn toggle_scroll_auto(&mut self, cx: &mut Context<Self>) {
        if self.is_scroll_annotating() {
            return;
        }
        let Some(active) = self.scroll.as_mut() else {
            return;
        };
        let Some(session) = active.session.as_ref() else {
            return;
        };
        let next = !active.snapshot.auto_scrolling;
        match session.set_auto_scroll(next) {
            Ok(()) => {
                active.auto_scroll_intent = next.then_some(active.snapshot.auto_direction);
                active.snapshot = session.snapshot();
                cx.notify();
            }
            Err(AutoScrollError::NeedsAccessibility) => {
                log::warn!("auto-scroll requested without accessibility permission");
                permissions::request(Permission::Accessibility);
                self.open_panel(Panel::Permissions, cx);
                self.inform(t!("scroll.accessibility_needed").to_string(), cx);
            }
            Err(err) => {
                self.fail(format!("{err}"), cx);
            }
        }
    }

    pub fn set_scroll_auto_direction(
        &mut self,
        direction: ScrollDirection,
        cx: &mut Context<Self>,
    ) {
        if let Some(active) = self.scroll.as_mut()
            && let Some(mode) = active.annotation_mode.as_mut()
        {
            mode.resume_auto = true;
            mode.direction = direction;
            self.leave_scroll_annotation(true, true, cx);
            return;
        }
        let Some(active) = self.scroll.as_mut() else {
            return;
        };
        let Some(session) = active.session.as_ref() else {
            return;
        };
        let enabled =
            !active.snapshot.auto_scrolling || active.snapshot.auto_direction != direction;
        match session.set_auto_scroll_direction(enabled, direction) {
            Ok(()) => {
                active.auto_scroll_intent = enabled.then_some(direction);
                active.snapshot = session.snapshot();
                cx.notify();
            }
            Err(AutoScrollError::NeedsAccessibility) => {
                log::warn!("auto-scroll requested without accessibility permission");
                permissions::request(Permission::Accessibility);
                self.open_panel(Panel::Permissions, cx);
                self.inform(t!("scroll.accessibility_needed").to_string(), cx);
            }
            Err(err) => {
                self.fail(format!("{err}"), cx);
            }
        }
    }

    pub fn scroll_bar_local_origin(&self) -> Option<gpui_kit::Point<Pixels>> {
        let active = self.scroll.as_ref()?;
        let bounds = active
            .saved_bar_bounds
            .unwrap_or_else(|| scroll_bar_bounds(active.display_bounds, active.selection));
        Some(bounds.origin - active.display_bounds.origin)
    }

    pub fn set_scroll_bar_local_origin(&mut self, local: gpui_kit::Point<Pixels>) {
        let Some(active) = self.scroll.as_mut() else {
            return;
        };
        active.saved_bar_bounds = Some(Bounds::new(
            active.display_bounds.origin + local,
            gpui_kit::size(
                px(crate::scroll_bar::SCROLL_BAR_SIZE.0),
                px(crate::scroll_bar::SCROLL_BAR_SIZE.1),
            ),
        ));
    }

    pub fn toggle_scroll_preview(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.scroll.as_mut() else {
            return;
        };
        active.preview_visible = !active.preview_visible;
        if active.preview_visible {
            let id = active.id;
            let display_id = active.display_id;
            let display_bounds = active.display_bounds;
            let selection = active.selection;
            if active.preview.is_none() {
                self.open_scroll_preview(id, display_id, display_bounds, selection, cx);
            }
        } else if let Some(handle) = active.preview.take() {
            cx.defer(move |cx| {
                handle
                    .update(cx, |_, window, _| crate::overlay::close_window(window))
                    .ok();
            });
        }
        cx.notify();
    }

    pub fn reset_scroll(&mut self, cx: &mut Context<Self>) {
        self.leave_scroll_annotation(false, true, cx);
        let Some(active) = self.scroll.as_mut() else {
            return;
        };
        if let Some(session) = active.session.as_ref() {
            session.reset();
            active.annotations.clear();
            active.annotation_content_shift = 0;
            active.annotation_reset_pending = true;
            active.annotation_auto_resume = false;
            active.auto_scroll_intent = None;
            active.snapshot = session.snapshot();
            cx.notify();
        }
    }

    pub fn cancel_scroll(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.scroll.as_mut() else {
            return;
        };
        if active.finishing {
            return;
        }
        if let Some(start) = active.start.take() {
            start.cancel();
        }
        if let Some(session) = active.session.take() {
            discard_stale_scroll(session);
        }
        self.busy = None;
        self.finish_scroll_ui(true, cx);
        log::info!("scroll capture discarded");
        self.retire_log_operation();
        self.resume_queued_capture(cx);
    }

    pub fn finish_scroll(&mut self, action: ScrollFinishAction, cx: &mut Context<Self>) {
        let Some(active) = self.scroll.as_mut() else {
            return;
        };
        if active.finishing {
            return;
        }
        self.leave_scroll_annotation(false, false, cx);
        let Some(active) = self.scroll.as_mut() else {
            return;
        };
        if active.session.is_none() {
            if active.start.is_some() {
                active.pending_finish = Some(action);
                cx.notify();
            } else {
                self.cancel_scroll(cx);
            }
            return;
        }
        let needs_watermark = matches!(
            action,
            ScrollFinishAction::Copy | ScrollFinishAction::Save | ScrollFinishAction::Pin
        );
        let watermark = if needs_watermark {
            let watermark_config = AppSettings::get(cx).watermark.clone();
            if watermark_config.target_enabled(WatermarkTarget::Screenshot) {
                let resolved = watermark_config
                    .preset(WatermarkTarget::Screenshot)
                    .ok_or_else(|| anyhow::anyhow!("watermark template is unavailable"))
                    .and_then(|preset| {
                        crate::export::watermark_spec(preset, AppSettings::watermark_dir(cx))
                    });
                match resolved {
                    Ok(spec) => spec,
                    Err(err) => {
                        if let Some(session) = active.session.take() {
                            discard_stale_scroll(session);
                        }
                        self.finish_scroll_ui(true, cx);
                        self.fail(
                            t!("controller.watermark_failed", error = format!("{err:#}"))
                                .to_string(),
                            cx,
                        );
                        self.retire_log_operation();
                        self.resume_queued_capture(cx);
                        return;
                    }
                }
            } else {
                WatermarkSpec::default()
            }
        } else {
            WatermarkSpec::default()
        };
        let session = active.session.take().expect("checked above");
        let mut annotations = active.annotations.clone();
        let mut annotation_content_shift = active.annotation_content_shift;
        let mut annotation_reset_pending = active.annotation_reset_pending;
        let scale = active.scale;
        let display_id = active.display_id;
        active.finishing = true;
        self.busy = Some(Busy::Encoding);
        cx.notify();

        cx.spawn(async move |this, cx| {
            let handle = std::thread::Builder::new()
                .name("snapit-scroll-finish".into())
                .spawn(move || {
                    session.finish().map(|result| {
                        result.map(|(image, content_shift)| {
                            Self::sync_scroll_annotation_shapes(
                                &mut annotations,
                                &mut annotation_content_shift,
                                &mut annotation_reset_pending,
                                scale,
                                content_shift,
                            );
                            annotations.commit();
                            (image, Arc::new(annotations.committed().to_vec()))
                        })
                    })
                });
            let image = match handle {
                Ok(handle) => {
                    while !handle.is_finished() {
                        cx.background_executor()
                            .timer(Duration::from_millis(4))
                            .await;
                    }
                    match handle.join() {
                        Ok(res) => res,
                        Err(_) => Err(anyhow::anyhow!("scroll finish thread crashed")),
                    }
                }
                Err(err) => Err(err.into()),
            };
            this.update(cx, |this, cx| {
                let restore_app = !matches!(&image, Ok(Some(_)))
                    || matches!(action, ScrollFinishAction::Copy | ScrollFinishAction::Pin);
                this.finish_scroll_ui(restore_app, cx);
                let hands_off_export = matches!(&image, Ok(Some(_)))
                    && matches!(action, ScrollFinishAction::Copy | ScrollFinishAction::Save);
                match image {
                    Ok(Some((image, shapes))) => {
                        if action == ScrollFinishAction::Edit {
                            this.open_image_editor(
                                ImageSource::from(image),
                                None,
                                scale,
                                display_id,
                                shapes,
                                false,
                                cx,
                            );
                        } else {
                            let captured =
                                Captured::new(image, None, scale, Some(shapes), watermark);
                            match action {
                                ScrollFinishAction::Copy => {
                                    this.remember(&captured, cx);
                                    this.copy_image(captured, cx);
                                }
                                ScrollFinishAction::Save => {
                                    this.remember(&captured, cx);
                                    this.save_image(captured, cx);
                                }
                                ScrollFinishAction::Pin => {
                                    this.remember(&captured, cx);
                                    this.open_pin_async(captured, None, cx);
                                }
                                ScrollFinishAction::Recognize => this.recognize(captured, cx),
                                ScrollFinishAction::Edit => unreachable!(),
                            }
                        }
                    }
                    Ok(None) => {
                        this.fail(t!("controller.scroll_empty").to_string(), cx);
                    }
                    Err(err) => {
                        this.fail(
                            t!("controller.scroll_failed", error = format!("{err:#}")).to_string(),
                            cx,
                        );
                    }
                }
                if !hands_off_export {
                    this.retire_log_operation();
                }
                this.resume_queued_capture(cx);
            })
            .ok();
        })
        .detach();
    }

    fn finish_scroll_ui(&mut self, restore_app: bool, cx: &mut Context<Self>) {
        self.leave_scroll_annotation(false, false, cx);
        if restore_app {
            self.restore_previous_app();
        } else {
            self.pending_restore_app = None;
        }
        let windows = self
            .scroll
            .take()
            .map(|active| (active.bar, active.preview, active.frame.map(|f| f.window)));
        if let Some((bar, preview, frame)) = windows {
            cx.defer(move |cx| {
                for handle in [bar, preview, frame].into_iter().flatten() {
                    handle
                        .update(cx, |_, window, _| crate::overlay::close_window(window))
                        .ok();
                }
            });
        }
        if matches!(self.busy, Some(Busy::Encoding)) && self.busy_owner.is_none() {
            self.busy = None;
        }
        cx.notify();
    }

    #[allow(clippy::too_many_arguments)]
    fn open_image_editor(
        &mut self,
        image: ImageSource,
        hdr: Option<Arc<HdrImage>>,
        scale: f32,
        display_id: Option<gpui_kit::DisplayId>,
        shapes: Arc<Vec<Shape>>,
        initial_crop_mode: bool,
        cx: &mut Context<Self>,
    ) {
        // The editor does not read the controller while it is constructed, so
        // open it synchronously and handle failure before this update returns.
        // Deferring used to silently lose the image after teardown.
        self.pending_restore_app = None;
        match crate::scroll_editor::open(
            image,
            hdr,
            scale,
            display_id,
            shapes,
            initial_crop_mode,
            cx,
        ) {
            Ok(handle) => {
                self.scroll_editors.push(handle);
                self.sync_dock_visible(cx);
                #[cfg(target_os = "macos")]
                crate::overlay::activate_app_single_window();
                #[cfg(not(target_os = "macos"))]
                cx.activate(true);
            }
            Err(err) => {
                log::error!("failed to open scroll editor: {err:#}");
                self.fail(
                    t!(
                        "controller.scroll_failed",
                        error = format!("editor: {err:#}")
                    )
                    .to_string(),
                    cx,
                );
            }
        }
    }

    pub fn open_watermark_editor(
        &mut self,
        target: snapit_store::WatermarkTarget,
        template_id: Option<String>,
        initial_preset: snapit_store::WatermarkPreset,
        display_id: Option<gpui_kit::DisplayId>,
        cx: &mut Context<Self>,
    ) {
        let previous = self.watermark_editor.take();
        let controller = cx.entity();
        cx.defer(move |cx| {
            if let Some(prev) = previous {
                prev.update(cx, |_, window, _| crate::overlay::close_window(window))
                    .ok();
            }
            match crate::watermark_editor::open_window(
                target,
                template_id,
                initial_preset,
                display_id,
                cx,
            ) {
                Ok(handle) => {
                    controller.update(cx, |this, cx| {
                        this.pending_restore_app = None;
                        this.watermark_editor = Some(handle);
                        this.sync_dock_visible(cx);
                    });
                    #[cfg(target_os = "macos")]
                    crate::overlay::activate_app_single_window();
                    #[cfg(not(target_os = "macos"))]
                    cx.activate(true);
                }
                Err(err) => {
                    log::error!("failed to open watermark editor: {err:#}");
                }
            }
        });
    }

    /// macOS: the Dock icon is visible only while a panel or editor window is open.
    fn sync_dock_visible(&self, cx: &mut App) {
        let open = cx.windows();
        let any_panel = self.panels.iter().any(|slot| {
            slot.handle.is_some_and(|handle| {
                open.iter()
                    .any(|window| window.window_id() == handle.window_id())
            })
        });
        let any_editor = self.scroll_editors.iter().any(|handle| {
            open.iter()
                .any(|window| window.window_id() == handle.window_id())
        });
        let has_watermark_editor = self.watermark_editor.is_some_and(|handle| {
            open.iter()
                .any(|window| window.window_id() == handle.window_id())
        });
        appearance::set_dock_visible(any_panel || any_editor || has_watermark_editor, cx);
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
        let editor_title = t!("scroll_editor.title").to_string();
        for handle in &self.scroll_editors {
            handle
                .update(cx, |_, window, _| window.set_window_title(&editor_title))
                .ok();
        }
        if let Some(handle) = self.watermark_editor {
            let watermark_title = t!("settings.watermark.editor.title").to_string();
            handle
                .update(cx, |_, window, _| window.set_window_title(&watermark_title))
                .ok();
        }
    }

    /// Focus Settings directly on `page`, opening it if needed.
    pub fn open_settings_page(&mut self, page: SettingsPage, cx: &mut Context<Self>) {
        let live: Vec<_> = cx
            .windows()
            .iter()
            .map(|window| window.window_id())
            .collect();
        let slot = &mut self.panels[Panel::Settings as usize];
        if slot
            .handle
            .is_some_and(|handle| !live.contains(&handle.window_id()))
        {
            slot.handle = None;
            self.settings_view = None;
        }
        if let Some(handle) = slot.handle.take()
            && handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
        {
            slot.handle = Some(handle);
            self.pending_restore_app = None;
            if let Some(view) = self.settings_view.as_ref().and_then(WeakEntity::upgrade) {
                view.update(cx, |view, cx| view.select_page(page, cx));
            }
            #[cfg(target_os = "macos")]
            crate::overlay::activate_app_single_window();
            #[cfg(not(target_os = "macos"))]
            cx.activate(true);
            cx.notify();
            return;
        }
        self.pending_settings_page = Some(page);
        self.open_panel(Panel::Settings, cx);
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
            if panel == Panel::Settings {
                self.settings_view = None;
            }
        }
        if let Some(handle) = slot.handle.take()
            && handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
        {
            slot.handle = Some(handle);
            self.pending_restore_app = None;
            #[cfg(target_os = "macos")]
            crate::overlay::activate_app_single_window();
            #[cfg(not(target_os = "macos"))]
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
        let initial_settings_page = (panel == Panel::Settings)
            .then(|| self.pending_settings_page.take())
            .flatten()
            .unwrap_or_default();
        // `open_window` renders immediately and the views read the controller,
        // so it must run after this update has released the controller.
        cx.defer(move |cx| {
            let opened =
                open_panel_window(panel, controller.clone(), display_id, initial_settings_page, cx)
                    .inspect_err(|err| log::error!("failed to open {panel:?} window: {err:#}"))
                    .ok();
            controller.update(cx, |this, cx| {
                this.pending_restore_app = None;
                let slot = &mut this.panels[panel as usize];
                slot.pending = false;
                if let Some((handle, settings_view)) = opened {
                    slot.handle = Some(handle);
                    if panel == Panel::Settings {
                        this.settings_view = settings_view;
                    }
                } else {
                    slot.handle = None;
                }
                this.sync_dock_visible(cx);
            });
            #[cfg(target_os = "macos")]
            crate::overlay::activate_app_single_window();
            #[cfg(not(target_os = "macos"))]
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
    initial_settings_page: SettingsPage,
    cx: &mut App,
) -> Result<(AnyWindowHandle, Option<WeakEntity<SettingsView>>)> {
    let (window_size, min_size) = match panel {
        Panel::Ocr => (size(px(480.), px(420.)), size(px(360.), px(280.))),
        Panel::Settings => (size(px(860.), px(640.)), size(px(620.), px(440.))),
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
    let opened = match panel {
        Panel::Ocr => {
            let handle = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| OcrView::new(controller, window, cx))
            })?
            .0;
            (handle, None)
        }
        Panel::Settings => {
            let (handle, view) = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| SettingsView::new_with_page(initial_settings_page, window, cx))
            })?;
            (handle, Some(view.downgrade()))
        }
        Panel::History => {
            let handle = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| HistoryView::new(window, cx))
            })?
            .0;
            (handle, None)
        }
        Panel::Permissions => {
            let handle = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| PermissionView::new(controller, window, cx))
            })?
            .0;
            (handle, None)
        }
        Panel::Update => {
            let handle = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| UpdateView::new(window, cx))
            })?
            .0;
            (handle, None)
        }
    };
    Ok(opened)
}

fn discard_stale_recorder(recorder: Result<Recorder>, path: PathBuf) {
    match recorder {
        Ok(recorder) => {
            if let Err(err) = std::thread::Builder::new()
                .name("snapit-stale-recorder".into())
                .spawn(move || {
                    if let Err(err) = recorder.discard() {
                        log::warn!("failed to discard stale recorder: {err:#}");
                    }
                })
            {
                log::warn!("failed to start stale-recorder cleanup: {err:#}");
            }
        }
        Err(_) => {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn discard_stale_scroll(session: ScrollSession) {
    if let Err(err) = std::thread::Builder::new()
        .name("snapit-scroll-discard".into())
        .spawn(move || session.discard())
    {
        log::warn!("failed to start scroll-discard thread: {err:#}");
    }
}

fn timestamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H.%M.%S").to_string()
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e3
}

pub fn last_animation_format(cx: &App) -> RecordFormat {
    match AppSettings::get(cx).recording.animation_format.as_str() {
        "apng" => RecordFormat::Apng,
        _ => RecordFormat::Gif,
    }
}

/// History kind for a finished recording, so GIFs and videos can sit side by
/// side in the list.
fn entry_kind(format: RecordFormat) -> EntryKind {
    match format {
        RecordFormat::Gif => EntryKind::Gif,
        RecordFormat::Apng => EntryKind::Apng,
        RecordFormat::Mp4 => EntryKind::Mp4,
    }
}

/// Localized name of a container, for the bar and for notifications.
pub fn format_label(format: RecordFormat) -> String {
    match format {
        RecordFormat::Gif => t!("format.gif").to_string(),
        RecordFormat::Apng => t!("format.apng").to_string(),
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
static NEXT_RECORDING_FILE: AtomicU64 = AtomicU64::new(0);

fn recording_paths(cx: &App, format: RecordFormat) -> (PathBuf, PathBuf) {
    let extension = format.extension();
    let serial = NEXT_RECORDING_FILE.fetch_add(1, Ordering::Relaxed) + 1;
    let millis = chrono::Local::now().format("%Y-%m-%d %H.%M.%S%.3f");
    let name = format!(
        "SnapIt {millis}-{}-{serial}.{extension}",
        std::process::id()
    );
    let paths_in = |dir: PathBuf| {
        let dest = dir.join(&name);
        (dest.with_extension(format!("{extension}.part")), dest)
    };
    let (part, dest) = paths_in(save_dir(cx));
    // AVAssetWriter refuses to open an existing file, so probe writability
    // with a throwaway sibling instead of the recording path itself.
    let probe = part.with_extension("probe");
    let writable = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .is_ok();
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
    match std::fs::hard_link(from, to) {
        Ok(()) => {
            std::fs::remove_file(from)
                .with_context(|| format!("failed to remove {}", from.display()))?;
            return Ok(());
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!(
                "refusing to overwrite existing recording {}: {err}",
                to.display()
            );
        }
        Err(_) => {}
    }
    let mut source =
        std::fs::File::open(from).with_context(|| format!("failed to open {}", from.display()))?;
    let mut destination = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)
        .with_context(|| format!("failed to create {}", to.display()))?;
    let copied = (|| {
        std::io::copy(&mut source, &mut destination)
            .with_context(|| format!("failed to write {}", to.display()))?;
        destination
            .sync_all()
            .with_context(|| format!("failed to flush {}", to.display()))?;
        Ok::<_, anyhow::Error>(())
    })();
    drop(destination);
    if let Err(err) = copied {
        let _ = std::fs::remove_file(to);
        return Err(err);
    }
    std::fs::remove_file(from).with_context(|| format!("failed to remove {}", from.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("snapit-controller-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn annotation_resumes_auto_scroll_stopped_by_control_interaction() {
        for direction in [ScrollDirection::Up, ScrollDirection::Down] {
            let snapshot = ScrollSnapshot {
                status: ScrollStatus::StoppedByUser,
                auto_direction: direction,
                ..ScrollSnapshot::default()
            };
            assert_eq!(
                annotation_auto_direction(&snapshot, Some(direction)),
                Some(direction)
            );
        }
    }

    #[test]
    fn annotation_does_not_revive_cleared_paused_or_finished_auto_scroll() {
        let snapshot = ScrollSnapshot::default();
        assert_eq!(annotation_auto_direction(&snapshot, None), None);
        assert_eq!(
            annotation_auto_direction(
                &ScrollSnapshot {
                    paused: true,
                    ..ScrollSnapshot::default()
                },
                Some(ScrollDirection::Down),
            ),
            None
        );
        assert_eq!(
            annotation_auto_direction(
                &ScrollSnapshot {
                    status: ScrollStatus::ReachedEnd,
                    ..ScrollSnapshot::default()
                },
                Some(ScrollDirection::Down),
            ),
            None
        );
    }

    #[test]
    fn annotation_uses_the_live_auto_scroll_direction() {
        let snapshot = ScrollSnapshot {
            auto_scrolling: true,
            auto_direction: ScrollDirection::Up,
            ..ScrollSnapshot::default()
        };
        assert_eq!(
            annotation_auto_direction(&snapshot, Some(ScrollDirection::Down)),
            Some(ScrollDirection::Up)
        );
    }

    #[test]
    fn upward_stitching_moves_annotations_in_logical_coordinates_once() {
        let mut annotations = Layer::default();
        annotations.set_tool(crate::annotate::Tool::Line);
        annotations.pointer_down(10.0, 10.0);
        annotations.pointer_move(30.0, 10.0);
        annotations.pointer_up();
        let mut content_shift = 0;
        let mut reset_pending = false;

        Controller::sync_scroll_annotation_shapes(
            &mut annotations,
            &mut content_shift,
            &mut reset_pending,
            2.0,
            200,
        );
        Controller::sync_scroll_annotation_shapes(
            &mut annotations,
            &mut content_shift,
            &mut reset_pending,
            2.0,
            200,
        );
        assert_eq!(content_shift, 200);
        assert_eq!(
            annotations.committed()[0].extents(),
            (10.0, 110.0, 20.0, 0.0)
        );

        Controller::sync_scroll_annotation_shapes(
            &mut annotations,
            &mut content_shift,
            &mut reset_pending,
            2.0,
            260,
        );
        assert_eq!(
            annotations.committed()[0].extents(),
            (10.0, 140.0, 20.0, 0.0)
        );

        Controller::sync_scroll_annotation_shapes(
            &mut annotations,
            &mut content_shift,
            &mut reset_pending,
            2.0,
            200,
        );
        assert_eq!(
            annotations.committed()[0].extents(),
            (10.0, 110.0, 20.0, 0.0)
        );
    }

    #[test]
    fn reset_ignores_stale_content_shifts_before_the_new_origin() {
        let mut annotations = Layer::default();
        annotations.set_tool(crate::annotate::Tool::Line);
        annotations.pointer_down(10.0, 10.0);
        annotations.pointer_move(30.0, 10.0);
        annotations.pointer_up();
        let mut content_shift = 400;
        let mut reset_pending = true;

        Controller::sync_scroll_annotation_shapes(
            &mut annotations,
            &mut content_shift,
            &mut reset_pending,
            1.0,
            900,
        );
        assert!(reset_pending);
        assert_eq!(content_shift, 400);
        assert_eq!(
            annotations.committed()[0].extents(),
            (10.0, 10.0, 20.0, 0.0)
        );

        Controller::sync_scroll_annotation_shapes(
            &mut annotations,
            &mut content_shift,
            &mut reset_pending,
            1.0,
            0,
        );
        assert!(!reset_pending);
        assert_eq!(content_shift, 0);
        assert_eq!(
            annotations.committed()[0].extents(),
            (10.0, 10.0, 20.0, 0.0)
        );
    }

    #[test]
    fn cancel_releases_capture_busy_for_the_next_capture() {
        let mut controller = Controller::default();
        let first = controller.begin_busy(Busy::Capturing);
        assert!(controller.is_busy());

        controller.finish_selection_busy(Some(first));
        assert!(!controller.is_busy());

        let second = controller.begin_busy(Busy::Capturing);
        assert_ne!(first, second);
        assert!(controller.finish_busy(second));
        assert!(!controller.is_busy());
    }

    #[test]
    fn pin_and_recognize_release_capture_busy_synchronously() {
        let mut controller = Controller::default();
        for _ in 0..2 {
            let capture = controller.begin_busy(Busy::Capturing);
            controller.finish_selection_busy(Some(capture));
            assert!(!controller.is_busy());
        }
    }

    #[test]
    fn copy_export_holds_encoding_until_its_own_completion() {
        let mut controller = Controller::default();
        let capture = controller.begin_busy(Busy::Capturing);
        let copy = controller.begin_busy(Busy::Encoding);

        // Closing the selection must not release the copy that replaced it.
        controller.finish_selection_busy(Some(capture));
        assert_eq!(controller.busy, Some(Busy::Encoding));
        assert!(controller.is_busy());
        assert!(!controller.finish_busy(capture));
        assert!(controller.finish_busy(copy));
        assert!(!controller.is_busy());

        let next = controller.begin_busy(Busy::Capturing);
        assert!(controller.finish_busy(next));
    }

    #[test]
    fn save_export_holds_encoding_until_its_own_completion() {
        let mut controller = Controller::default();
        let capture = controller.begin_busy(Busy::Capturing);
        let save = controller.begin_busy(Busy::Encoding);

        controller.finish_selection_busy(Some(capture));
        assert_eq!(controller.busy, Some(Busy::Encoding));
        assert!(controller.finish_busy(save));
        assert!(!controller.is_busy());

        let next = controller.begin_busy(Busy::Capturing);
        assert!(controller.finish_busy(next));
    }

    #[test]
    fn stale_completion_cannot_release_a_new_capture() {
        let mut controller = Controller::default();
        let old_capture = controller.begin_busy(Busy::Capturing);
        let new_capture = controller.begin_busy(Busy::Capturing);

        // A delayed Cancel/failure from the old selection cannot clear the new
        // capture's busy state.
        controller.finish_selection_busy(Some(old_capture));
        assert!(!controller.finish_busy(old_capture));
        assert_eq!(controller.busy, Some(Busy::Capturing));
        assert_eq!(controller.busy_owner, Some(new_capture));
        assert!(controller.finish_busy(new_capture));
    }

    #[test]
    fn stale_export_completion_cannot_release_a_newer_export() {
        let mut controller = Controller::default();
        let old_save = controller.begin_busy(Busy::Encoding);
        let new_copy = controller.begin_busy(Busy::Encoding);

        assert!(!controller.finish_busy(old_save));
        assert_eq!(controller.busy, Some(Busy::Encoding));
        assert_eq!(controller.busy_owner, Some(new_copy));
        assert!(controller.finish_busy(new_copy));
        assert!(!controller.is_busy());
    }

    #[test]
    fn recording_handoff_detaches_the_selection_busy_owner() {
        let mut controller = Controller::default();
        let capture = controller.begin_busy(Busy::Capturing);
        controller.handoff_busy();
        controller.busy = Some(Busy::Capturing); // recording owns the phase now

        assert!(!controller.finish_busy(capture));
        assert_eq!(controller.busy, Some(Busy::Capturing));
    }

    #[test]
    fn stale_copy_cannot_replace_or_delete_the_current_backing_file() {
        let dir = temp_dir("copy-owner");
        let old_path = dir.join("snapit-copy-old.png");
        let new_path = dir.join("snapit-copy-new.png");
        std::fs::write(&old_path, b"old").unwrap();
        std::fs::write(&new_path, b"new").unwrap();
        let mut controller = Controller::default();

        let previous = controller.next_copy_serial();
        assert!(
            controller
                .replace_clipboard_file(previous, old_path.clone())
                .is_none()
        );
        let old = controller.next_copy_serial();
        let new = controller.next_copy_serial();
        assert!(controller.copy_is_current(new));
        assert!(!controller.copy_is_current(old));
        assert!(controller.clear_clipboard_file(old).is_none());
        assert!(
            controller
                .replace_clipboard_file(old, dir.join("snapit-copy-stale.png"))
                .is_none()
        );
        assert_eq!(controller.clipboard_file, Some(old_path.clone()));
        assert!(old_path.exists());

        assert_eq!(
            controller.replace_clipboard_file(new, new_path.clone()),
            Some(old_path.clone())
        );
        clipboard::remove_clipboard_file(&old_path);
        assert!(!old_path.exists());
        assert_eq!(std::fs::read(&new_path).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stale_copy_cleans_only_its_own_generated_file() {
        let dir = temp_dir("copy-cleanup");
        let stale_path = dir.join("snapit-copy-stale.png");
        let current_path = dir.join("snapit-copy-current.png");
        std::fs::write(&stale_path, b"stale").unwrap();
        std::fs::write(&current_path, b"current").unwrap();

        clipboard::remove_clipboard_file(&stale_path);
        clipboard::remove_clipboard_file(&stale_path); // missing is already clean
        assert!(!stale_path.exists());
        assert_eq!(std::fs::read(&current_path).unwrap(), b"current");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn out_of_order_ocr_completion_records_only_the_latest_attempt() {
        let mut controller = Controller::default();
        let old = controller.begin_ocr_attempt();
        let new = controller.begin_ocr_attempt();
        let mut history_entries = 0;

        // The newer worker finishes first; the older result then arrives late.
        for revision in [new, old, new] {
            if controller.accept_ocr_result(revision) {
                history_entries += 1;
            }
        }
        assert_eq!(history_entries, 1);
    }

    #[test]
    fn publishing_a_recording_never_replaces_an_existing_file() {
        let dir = temp_dir("publish");
        let part = dir.join("recording.gif.part");
        let dest = dir.join("recording.gif");
        std::fs::write(&part, b"new recording").unwrap();
        std::fs::write(&dest, b"user file").unwrap();

        assert!(move_file(&part, &dest).is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), b"user file");
        assert_eq!(std::fs::read(&part).unwrap(), b"new recording");

        std::fs::remove_file(&dest).unwrap();
        move_file(&part, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new recording");
        assert!(!part.exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
