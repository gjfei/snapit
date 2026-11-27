//! Fullscreen, borderless, topmost window showing one monitor's frozen frame.

use std::{
    cell::{Cell, RefCell},
    ops::Range,
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use gpui_kit::component::{
    ActiveTheme as _,
    color_picker::{ColorPickerEvent, ColorPickerState},
    slider::{SliderEvent, SliderState, SliderValue},
    toolbar::Toolbar,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, AsyncApp, Bounds, BoxShadow, ClipboardItem, Context,
    CursorStyle, ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Hsla,
    InteractiveElement as _, IntoElement, KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseExitEvent, MouseMoveEvent, MouseUpEvent, ObjectFit, ParentElement as _, Pixels, Point,
    Render, RenderImage, ScrollDelta, ScrollEvent, SharedString, Size, Styled as _,
    StyledImage as _, Subscription, UTF16Selection, WeakEntity, Window, actions, canvas, div, fill,
    img, point, prelude::FluentBuilder as _, px, rgb, size,
};

use crate::annotate::{self, Layer};
use crate::capture_toolbar;
use crate::controller::Controller;
use crate::session::{
    AnnotationPick, CaptureIntent, CaptureSession, CropKey, ResizeHandle, SessionCommand,
};
use crate::settings::AppSettings;
use gpui_kit::assets::IconName as LucideIcon;
use snapit_capture::RgbaImage;
use snapit_recorder::RecordFormat;
use snapit_store::{AnnotationTool, WatermarkPreset, WatermarkTarget};

const CONTEXT: &str = "SnapItOverlay";

/// Run after the window has presented a frame with its current content.
pub(crate) fn after_presented_frame(
    window: &mut Window,
    callback: impl FnOnce(&mut Window, &mut App) + 'static,
) {
    let pending = Rc::new(RefCell::new(Some(callback)));
    window.on_next_frame(move |window, _| {
        let pending = pending.clone();
        window.on_next_frame(move |window, cx| {
            if let Some(callback) = pending.borrow_mut().take() {
                callback(window, cx);
            }
        });
    });
}

/// Color magnifier: a 13 × 13 block of physical pixels at 9 px per cell.
const MAGNIFIER_CELLS: u32 = 13;
const MAGNIFIER_CELL: f32 = 9.0;
const MAGNIFIER_PAD: f32 = 6.0;
const MAGNIFIER_GAP: f32 = 14.0;
/// How long the "color copied" pill stays on screen.
const TOAST_DURATION: Duration = Duration::from_millis(1400);
/// Palette and slider drags emit a Change per frame; only the settled value
/// is written to config.toml.
const STYLE_PERSIST_DELAY: Duration = Duration::from_millis(250);

actions!(
    snapit_overlay,
    [
        Cancel,
        ConfirmSelection,
        CopySelection,
        SaveSelection,
        PinSelection,
        RecognizeSelection,
        ScrollCapture,
        RecordAnimation,
        RecordVideo,
        EditSelection,
        UndoAnnotations,
        ClearAnnotations,
        SelectFullScreen,
        CopyColor
    ]
);

pub fn init(cx: &mut App) {
    #[cfg(target_os = "macos")]
    macos_window::install();
    cx.bind_keys([
        KeyBinding::new("escape", Cancel, Some(CONTEXT)),
        KeyBinding::new("enter", ConfirmSelection, Some(CONTEXT)),
        KeyBinding::new("f", SelectFullScreen, Some(CONTEXT)),
        KeyBinding::new("c", CopyColor, Some(CONTEXT)),
        KeyBinding::new("e", EditSelection, Some(CONTEXT)),
        KeyBinding::new(crate::shortcut!("c"), CopySelection, Some(CONTEXT)),
        KeyBinding::new(crate::shortcut!("s"), SaveSelection, Some(CONTEXT)),
        KeyBinding::new(crate::shortcut!("z"), UndoAnnotations, Some(CONTEXT)),
    ]);
}

/// 选区浮层（含滚动标注状态）放在 Pin（101）之上；仅用作录制/滚动边框时降到
/// 控制条（101）之下。
#[cfg(target_os = "macos")]
const OVERLAY_WINDOW_LEVEL: isize = 102;
#[cfg(target_os = "macos")]
const RECORDING_OUTLINE_WINDOW_LEVEL: isize = 100;

/// 预建浮层在冻结帧到达前先以零 alpha 上屏（仅 macOS），让 GPUI 提前建好
/// surface，又不闪出窗口底色。Windows 的窗口在首帧之前就已显示，所以它在
/// 冻结完成后才创建浮层。
#[cfg(target_os = "macos")]
pub fn pre_place(window: &mut Window) {
    macos_window::pre_place(window);
}

/// 记录当前最前方的其它应用 PID（当 SnapIt 自身不是最前应用时）。
pub fn frontmost_other_app() -> Option<i32> {
    #[cfg(target_os = "macos")]
    {
        macos_window::frontmost_other_app()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// 仅激活 SnapIt 自身（macOS 不携带 NSApplicationActivateAllWindows），避免把
/// 后台已打开的设置/历史/编辑等窗口一并拉到屏幕最前。
pub fn activate_app_single_window() {
    #[cfg(target_os = "macos")]
    macos_window::activate_app_single_window();
}

/// 截图/录制结束后，将焦点交还原先的前台应用，避免 AppKit 自动把 SnapIt
/// 其它未关闭窗口提升为 key window 盖到用户工作窗口之上。
pub fn restore_frontmost_app(pid: i32) {
    #[cfg(target_os = "macos")]
    macos_window::restore_frontmost_app(pid);
    #[cfg(not(target_os = "macos"))]
    let _ = pid;
}

/// 立即把浮层从屏幕移除，但不销毁 GPUI 窗口。复制/保存时主线程销毁全屏
/// overlay 要花约 37 ms（Metal layer + renderer drop），排在剪贴板写入前面；
/// 先 orderOut 让浮层视觉上瞬间消失，并解除 delegate 与 tracking area 避免
/// 后续再向已摘除窗口投递失活或鼠标事件，等写入完成再真正 remove_window。
#[cfg(target_os = "macos")]
pub fn hide(window: &mut Window) {
    macos_window::hide(window);
}

/// 关闭并销毁窗口。在 macOS 上 `Window::remove_window()` 会把原生 `[NSWindow close]`
/// 延迟到下一轮 foreground 任务，期间 AppKit 仍可能向已从 GPUI 移除的 `WindowId`
/// 发送鼠标跟踪或失活回调（导致 `window not found` 错误日志）。先同步摘除
/// delegate/firstResponder/trackingAreas 并 `orderOut:`，再调用 `remove_window()`。
pub fn close_window(window: &mut Window) {
    #[cfg(target_os = "macos")]
    macos_window::hide(window);
    window.remove_window();
}

/// Physical state of the Control key, read straight from AppKit rather than
/// from an event. GPUI rewrites ctrl+left mouse down/up into right mouse
/// down/up (and clears `control` from the event's modifiers), so a selection
/// drag started while a Ctrl-based hotkey is still held arrives here as a
/// right click. `window.modifiers()` is fed from that rewritten event and is
/// therefore unusable for this check; the live NSEvent flags are not.
/// Only the macOS right-click paths call it, so it exists only on macOS.
#[cfg(target_os = "macos")]
fn control_key_held() -> bool {
    use objc2::msg_send;
    use objc2::runtime::AnyClass;

    // NSEventModifierFlagControl = 1 << 18 (NSEventModifierFlagDeviceIndependentFlagsMask).
    const NS_CONTROL_KEY_MASK: usize = 1 << 18;
    let Some(class) = AnyClass::get(c"NSEvent") else {
        return false;
    };
    let flags: usize = unsafe { msg_send![class, modifierFlags] };
    flags & NS_CONTROL_KEY_MASK != 0
}

pub struct OverlayView {
    session: Entity<CaptureSession>,
    frame_ix: usize,
    focus_handle: FocusHandle,
    color_picker: Entity<ColorPickerState>,
    /// One slider per drawable tool. Ranges stay fixed, so switching tools
    /// never rebuilds a `SliderState` (which would drop an in-flight Release).
    size_sliders: Vec<Entity<SliderState>>,
    /// Last values pushed into the controls, so render only writes on change.
    color_shown: u32,
    size_shown: [f32; 7],
    /// Invalidates queued debounced writes when a newer edit arrives.
    persist_generation: u64,
    /// Whether the inline annotation style bar below the toolbar is expanded.
    style_open: bool,
    /// Resize handle under the idle cursor, used for the window cursor style.
    hover_handle: Option<ResizeHandle>,
    /// An annotation is under the idle cursor: a press would pick it.
    hover_shape: bool,
    /// Last pointer position in this overlay, for the color magnifier.
    pointer: Option<Point<Pixels>>,
    /// Everything the mosaic needs from the frozen screenshot, kept between
    /// frames. Shared with the canvas paint closure, which only has `&App` and
    /// runs on every pointer move: cropping the region costs milliseconds per
    /// frame and averaging its blocks costs a few more.
    paint_cache: Arc<Mutex<PaintCache>>,
    /// Selection-sized SDR watermark layer, rebuilt only when the region or
    /// watermark configuration changes.
    watermark_preview: RefCell<Option<SelectionWatermarkCache>>,
    /// Transient "color copied" confirmation and when it was shown.
    toast: Option<(SharedString, Instant)>,
    /// The overlay has become the recording indicator: only this outline is
    /// drawn, everything else (frozen frame, mask, toolbar) stays off screen.
    recording: Option<Bounds<Pixels>>,
    scroll_annotation: Option<WeakEntity<Controller>>,
    scroll_pause_render: Option<(u64, Arc<RenderImage>)>,
    _scroll_subscription: Option<Subscription>,
    /// Colors the live recording/scroll outline in warning color when active.
    live_warning: bool,
    /// Toolbar top-left after the user dragged it; `None` keeps the automatic
    /// placement below/above/beside the selection.
    toolbar_position: Option<Point<Pixels>>,
    /// Exact rendered toolbar bounds, used for zero-jump drag start and clamping.
    toolbar_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// In-progress toolbar drag.
    toolbar_drag: Option<ToolbarDrag>,
    _subscription: Subscription,
    _color_subscription: Subscription,
    _size_subscriptions: Vec<Subscription>,
}

const TOOLBAR_HEIGHT: f32 = 36.;
const STYLE_ROW_HEIGHT: f32 = 38.;
/// Estimate of the one-row toolbar (seven annotation tools, scroll capture, two
/// record buttons, crop/edit popup and the capture actions), used to
/// right-align it and keep it inside the viewport.
const TOOLBAR_WIDTH: f32 = 541.;
const TOOLBAR_GAP: f32 = 8.;

/// The frozen region's pixels and the block colours sampled from them, kept
/// between painted frames.
#[derive(Default)]
struct PaintCache {
    crop: Option<(CropKey, RgbaImage)>,
    mosaics: annotate::MosaicCache,
}

struct SelectionWatermarkCache {
    key: (CropKey, WatermarkPreset),
    result: std::result::Result<Option<Arc<RenderImage>>, String>,
}

#[derive(Clone, Copy)]
struct ToolbarDrag {
    /// Pointer position minus the toolbar origin when the press began.
    grab: Point<Pixels>,
    /// Press position, for the movement threshold that separates a drag from a
    /// button click.
    press: Point<Pixels>,
    /// The pointer moved far enough to start moving the toolbar.
    active: bool,
}

/// Top-left corner of the annotation toolbar for a committed selection.
fn toolbar_origin(
    selection: Bounds<Pixels>,
    viewport: Bounds<Pixels>,
    toolbar_height: f32,
) -> Point<Pixels> {
    let bottom = selection.origin.y + selection.size.height;
    let right = selection.origin.x + selection.size.width;
    let top = if bottom + px(toolbar_height + TOOLBAR_GAP) <= viewport.size.height {
        bottom + px(TOOLBAR_GAP)
    } else if selection.origin.y >= px(toolbar_height + TOOLBAR_GAP) {
        selection.origin.y - px(toolbar_height + TOOLBAR_GAP)
    } else {
        (bottom - px(toolbar_height + TOOLBAR_GAP)).max(px(0.))
    };
    let left = if right >= px(TOOLBAR_WIDTH) {
        right - px(TOOLBAR_WIDTH)
    } else {
        selection.origin.x
    };
    point(left, top)
}

/// Keep a dragged toolbar inside the overlay using its measured size.
fn clamp_toolbar_origin_with_size(
    origin: Point<Pixels>,
    viewport: Size<Pixels>,
    toolbar_size: Size<Pixels>,
) -> Point<Pixels> {
    let max_x = (viewport.width - toolbar_size.width).max(px(0.));
    let max_y = (viewport.height - toolbar_size.height).max(px(0.));
    point(
        origin.x.max(px(0.)).min(max_x),
        origin.y.max(px(0.)).min(max_y),
    )
}

/// Keep a dragged toolbar inside the overlay.
#[cfg_attr(not(test), allow(dead_code))]
fn clamp_toolbar_origin(
    origin: Point<Pixels>,
    viewport: Size<Pixels>,
    toolbar_height: f32,
) -> Point<Pixels> {
    clamp_toolbar_origin_with_size(
        origin,
        viewport,
        size(px(TOOLBAR_WIDTH), px(toolbar_height)),
    )
}

impl OverlayView {
    pub fn new(
        session: Entity<CaptureSession>,
        frame_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        let subscription = cx.observe(&session, |_, _, cx| cx.notify());
        let (style_color, defaults) = {
            let annotations = session.read(cx).annotations();
            (annotations.style().color, annotations.config().clone())
        };
        let color_picker = cx.new(|cx| {
            ColorPickerState::new(window, cx).default_value(Hsla::from(rgb(style_color)))
        });
        let color_subscription =
            cx.subscribe(&color_picker, |this, _, event: &ColorPickerEvent, cx| {
                let ColorPickerEvent::Change(Some(color)) = event else {
                    return;
                };
                let color = capture_toolbar::color_to_rgb(*color);
                if let Some(controller) = this.scroll_controller() {
                    controller.update(cx, |controller, cx| {
                        controller.set_scroll_annotation_color(color, cx)
                    });
                } else {
                    this.session
                        .update(cx, |session, cx| session.set_annotation_color(color, cx));
                }
                this.schedule_style_persist(cx);
            });
        let mut size_sliders = Vec::new();
        let mut size_subscriptions = Vec::new();
        let mut size_shown = [0.0; 7];
        for (ix, key) in AnnotationTool::ALL.into_iter().enumerate() {
            let (min, max) = key.size_range();
            let value = defaults.get(key).size as f32;
            size_shown[ix] = value;
            let state = cx.new(|_| {
                SliderState::new()
                    .min(min as f32)
                    .max(max as f32)
                    .step(1.)
                    .default_value(value)
            });
            let size_subscription =
                cx.subscribe(&state, move |this, _, event: &SliderEvent, cx| {
                    let (value, release) = match event {
                        SliderEvent::Change(SliderValue::Single(value)) => (*value, false),
                        SliderEvent::Release(SliderValue::Single(value)) => (*value, true),
                        _ => return,
                    };
                    let active = this
                        .scroll_layer(cx)
                        .map(|layer| layer.active_tool().style_index())
                        .unwrap_or_else(|| {
                            this.session
                                .read(cx)
                                .annotations()
                                .active_tool()
                                .style_index()
                        });
                    if active != ix {
                        return;
                    }
                    if let Some(controller) = this.scroll_controller() {
                        controller.update(cx, |controller, cx| {
                            controller.set_scroll_annotation_size(value, cx)
                        });
                    } else {
                        this.session
                            .update(cx, |session, cx| session.set_annotation_size(value, cx));
                    }
                    if release {
                        this.flush_style_persist(cx);
                    } else {
                        // Keyboard/a11y changes never emit Release; the debounce
                        // still saves them a moment later.
                        this.schedule_style_persist(cx);
                    }
                });
            size_sliders.push(state);
            size_subscriptions.push(size_subscription);
        }
        Self {
            session,
            frame_ix,
            focus_handle,
            color_picker,
            size_sliders,
            color_shown: style_color,
            size_shown,
            persist_generation: 0,
            style_open: false,
            hover_handle: None,
            hover_shape: false,
            pointer: None,
            paint_cache: Arc::new(Mutex::new(PaintCache::default())),
            watermark_preview: RefCell::new(None),
            toast: None,
            recording: None,
            scroll_annotation: None,
            scroll_pause_render: None,
            _scroll_subscription: None,
            live_warning: false,
            toolbar_position: None,
            toolbar_bounds: Rc::new(Cell::new(None)),
            toolbar_drag: None,
            _subscription: subscription,
            _color_subscription: color_subscription,
            _size_subscriptions: size_subscriptions,
        }
    }

    /// Debounce palette/slider drags. Every queued write carries a generation
    /// so a newer edit (or the slider Release) cancels the earlier one.
    fn schedule_style_persist(&mut self, cx: &mut Context<Self>) {
        self.persist_generation = self.persist_generation.wrapping_add(1);
        // Restyling a picked shape changes no defaults: skip the disk write.
        if self
            .scroll_layer(cx)
            .is_some_and(|layer| layer.selected().is_some())
        {
            return;
        }
        let generation = self.persist_generation;
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor().timer(STYLE_PERSIST_DELAY).await;
            this.update(cx, |this, cx| {
                if this.persist_generation == generation {
                    this.flush_style_persist(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn flush_style_persist(&mut self, cx: &mut Context<Self>) {
        // Always flush on release/exit: a default edited just before a shape
        // was picked may still be waiting in its debounce window.
        self.persist_generation = self.persist_generation.wrapping_add(1);
        if let Some(controller) = self.scroll_controller() {
            controller.update(cx, |controller, cx| {
                controller.persist_scroll_annotation_defaults(cx)
            });
        } else {
            self.session
                .update(cx, |session, cx| session.persist_annotation_defaults(cx));
        }
    }

    /// Keep the palette chip and the active tool's slider in sync with the
    /// shape the toolbar is currently editing. `set_value` preserves the
    /// slider's drag state, so this is safe during a drag.
    fn sync_style_controls(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (style, active_tool) = if let Some(layer) = self.scroll_layer(cx) {
            (layer.style(), layer.active_tool())
        } else {
            let annotations = self.session.read(cx).annotations();
            (annotations.style(), annotations.active_tool())
        };
        let ix = active_tool.style_index();
        if self.color_shown != style.color {
            self.color_shown = style.color;
            let color = Hsla::from(rgb(style.color));
            self.color_picker
                .update(cx, |state, cx| state.set_value(color, window, cx));
        }
        if self.size_shown[ix] != style.size {
            self.size_shown[ix] = style.size;
            self.size_sliders[ix].update(cx, |state, cx| {
                state.set_value(style.size, window, cx);
            });
        }
    }

    /// Reveal the window once its frame is ready, with keyboard focus.
    pub fn activate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_handle.focus(window, cx);
        #[cfg(target_os = "macos")]
        {
            // 预建时窗口是零 alpha、还没有画过冻结帧；这时直接恢复 alpha 会先露出
            // 未渲染图层的黑色，等 GPUI 再画一帧才正常。等首帧合成完再置前。
            let asked = Instant::now();
            after_presented_frame(window, move |window, _| {
                let showing = Instant::now();
                macos_window::reveal(window);
                window.activate_window();
                log::info!(
                    target: "timing",
                    "overlay reveal: presented {:.1} ms after activate, reveal+activate {:.1} ms",
                    asked.elapsed().as_secs_f64() * 1e3,
                    showing.elapsed().as_secs_f64() * 1e3
                );
            });
        }
        #[cfg(not(target_os = "macos"))]
        window.activate_window();
    }

    /// Turn the overlay into the live recording indicator: drop the frozen
    /// frame, the dim mask and the toolbar so the screen shows through, keep
    /// only the outline of `selection`, and let the pointer reach the app
    /// being recorded.
    pub fn enter_recording(
        &mut self,
        selection: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.recording = Some(selection);
        self.live_warning = false;
        self.style_open = false;
        self.toast = None;
        self.hover_handle = None;
        self.hover_shape = false;
        self.pointer = None;
        self.toolbar_bounds.set(None);
        // The window root paints the theme background behind this view; with
        // the frozen frame gone it would cover the whole live screen.
        gpui_kit::base::Root::update(window, cx, |root, _, cx| {
            root.style().background = Some(gpui_kit::transparent_black().into());
            cx.notify();
        });
        #[cfg(target_os = "macos")]
        macos_window::set_window_level(window, RECORDING_OUTLINE_WINDOW_LEVEL);
        crate::recording_bar::make_recording_window(window);
        cx.notify();
    }

    pub fn enter_scroll_annotation(
        &mut self,
        controller: Entity<Controller>,
        selection: Bounds<Pixels>,
        toolbar_origin: Option<Point<Pixels>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.recording = Some(selection);
        self.live_warning = false;
        self.style_open = false;
        self.toast = None;
        self.hover_handle = None;
        self.hover_shape = false;
        self.pointer = None;
        self.toolbar_bounds.set(None);
        self.toolbar_position = toolbar_origin;
        self.toolbar_drag = None;
        self._scroll_subscription = Some(cx.observe(&controller, |_, _, cx| cx.notify()));
        self.scroll_annotation = Some(controller.downgrade());
        #[cfg(target_os = "macos")]
        {
            macos_window::set_window_level(window, OVERLAY_WINDOW_LEVEL);
            macos_window::activate_app_single_window();
        }
        crate::recording_bar::make_interactive_window(window);
        self.focus_handle.focus(window, cx);
        window.activate_window();
        cx.notify();
    }

    pub fn exit_scroll_annotation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._scroll_subscription = None;
        self.scroll_annotation = None;
        self.scroll_pause_render = None;
        self.style_open = false;
        self.color_picker
            .update(cx, |state, cx| state.set_open(false, cx));
        self.toolbar_position = None;
        self.toolbar_bounds.set(None);
        self.toolbar_drag = None;
        self.focus_handle.focus(window, cx);
        #[cfg(target_os = "macos")]
        macos_window::set_window_level(window, RECORDING_OUTLINE_WINDOW_LEVEL);
        crate::recording_bar::make_recording_window(window);
        cx.notify();
    }

    pub fn set_live_warning(&mut self, warning: bool, cx: &mut Context<Self>) {
        if self.live_warning != warning {
            self.live_warning = warning;
            cx.notify();
        }
    }

    fn request(&mut self, command: SessionCommand, cx: &mut Context<Self>) {
        if command != SessionCommand::Cancel
            && self.session.read(cx).committed_selection().is_none()
        {
            return;
        }
        // Exiting the capture must not lose a style edit still in its debounce.
        self.style_open = false;
        self.flush_style_persist(cx);
        self.session
            .update(cx, |session, cx| session.request(command, cx));
    }

    fn scroll_controller(&self) -> Option<Entity<Controller>> {
        self.scroll_annotation.as_ref()?.upgrade()
    }

    fn scroll_layer(&self, cx: &App) -> Option<Layer> {
        self.scroll_controller()?
            .read(cx)
            .scroll_annotations()
            .cloned()
    }

    fn scroll_local(&self, position: Point<Pixels>, cx: &App) -> Option<[f32; 2]> {
        let selection = self.recording?;
        if !selection.contains(&position) {
            return None;
        }
        let controller = self.scroll_controller()?;
        let (origin_y, scale, _, _) = controller.read(cx).scroll_annotation_state()?;
        Some([
            ((position.x - selection.origin.x).as_f32()).clamp(0.0, selection.size.width.as_f32()),
            ((position.y - selection.origin.y).as_f32() + origin_y as f32 / scale).clamp(
                0.0,
                selection.size.height.as_f32() + origin_y as f32 / scale,
            ),
        ])
    }

    fn typing(&self, cx: &App) -> bool {
        self.scroll_layer(cx)
            .map(|layer| layer.is_typing())
            .unwrap_or_else(|| self.session.read(cx).annotations().is_typing())
    }

    /// A committed region ends the pre-selection helpers: the color loupe and
    /// the `C` / `F` keys no longer respond. Every other shortcut (Enter,
    /// copy/save/undo, Delete, Esc) keeps working on the finished selection.
    fn selection_committed(&self, cx: &App) -> bool {
        self.session.read(cx).committed_selection().is_some()
    }

    /// Copy the pixel under the cursor as `#RRGGBB` (`C` before a selection).
    fn copy_color(&mut self, cx: &mut Context<Self>) {
        if self.typing(cx) || self.selection_committed(cx) {
            return;
        }
        let session = self.session.read(cx);
        let Some(frame) = session.frame(self.frame_ix) else {
            return;
        };
        let display = frame.display_bounds().size;
        let position = self
            .pointer
            .or_else(|| {
                session
                    .committed_selection()
                    .filter(|selection| selection.frame_ix() == self.frame_ix)
                    .map(|selection| {
                        let bounds = selection.bounds();
                        point(
                            bounds.origin.x + bounds.size.width / 2.0,
                            bounds.origin.y + bounds.size.height / 2.0,
                        )
                    })
            })
            .unwrap_or_else(|| point(display.width / 2.0, display.height / 2.0));
        let Some(sample) = frame.color_sample(position, 1) else {
            return;
        };
        let hex = format!("#{:06X}", sample.center);
        cx.write_to_clipboard(ClipboardItem::new_string(hex.clone()));
        self.show_toast(t!("overlay.color_copied", color = hex).to_string(), cx);
    }

    /// Show a short-lived message on this overlay. A newer toast resets the
    /// timer, so an older timer never clears a message the user just saw.
    pub(super) fn show_toast(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.toast = Some((text.into(), Instant::now()));
        cx.notify();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor().timer(TOAST_DURATION).await;
            this.update(cx, |this, cx| {
                let expired = this
                    .toast
                    .as_ref()
                    .is_some_and(|(_, shown)| shown.elapsed() >= TOAST_DURATION);
                if expired {
                    this.toast = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window, cx);
        self.pointer = Some(event.position);
        cx.notify();
        if self.scroll_annotation.is_some() {
            if let Some(local) = self.scroll_local(event.position, cx)
                && let Some(controller) = self.scroll_controller()
            {
                controller.update(cx, |controller, cx| {
                    controller.begin_scroll_annotation(
                        local[0],
                        local[1],
                        event.click_count >= 2,
                        cx,
                    )
                });
            }
            return;
        }
        let frame_ix = self.frame_ix;
        if self.session.read(cx).collecting() {
            // Shift-click toggles regions instead of switching to resize or
            // annotation; a drag still adds one more rectangle.
            self.hover_handle = None;
            self.style_open = false;
            if self.session.update(cx, |session, cx| {
                session.remove_selection_at(frame_ix, event.position, cx)
            }) {
                return;
            }
            self.session.update(cx, |session, cx| {
                session.begin_selection(frame_ix, event.position, true, cx)
            });
            return;
        }
        // Picking is tool-independent: any press on an annotation selects it
        // (and starts a move or a handle drag) instead of drawing over it.
        // Picking wins over the resize band and over double-click-to-copy,
        // both of which would otherwise swallow a click near the shapes.
        let picking = !self.session.read(cx).multi_selection()
            && self
                .session
                .read(cx)
                .committed_selection()
                .is_some_and(|selection| selection.frame_ix() == frame_ix);
        if picking {
            let picked = self.session.update(cx, |session, cx| {
                session.pick_annotation_at(event.position, event.click_count >= 2, cx)
            });
            match picked {
                AnnotationPick::Shape | AnnotationPick::Handle => {
                    self.hover_handle = None;
                    return;
                }
                // The text box and caret replace the style row while typing.
                AnnotationPick::Text => {
                    self.hover_handle = None;
                    self.style_open = false;
                    return;
                }
                AnnotationPick::None => {}
            }
        }
        let handle = self
            .session
            .read(cx)
            .committed_selection()
            .filter(|selection| selection.frame_ix() == frame_ix)
            .and_then(|selection| ResizeHandle::at(selection.bounds(), event.position));
        if let Some(handle) = handle {
            self.hover_handle = Some(handle);
            self.session.update(cx, |session, cx| {
                session.begin_resize(frame_ix, handle, cx);
            });
            return;
        }
        self.hover_handle = None;
        let inside_selection = self.session.update(cx, |session, cx| {
            session.activate_selection_at(frame_ix, event.position, cx)
        });
        if inside_selection {
            if event.click_count >= 2 {
                self.request(SessionCommand::Copy, cx);
                return;
            }
            // Annotations belong to one region; multi-region captures are
            // copied/saved/pinned as one combined image instead. Anything
            // pickable was already taken (and cleared) above, so this is a
            // press on empty canvas: draw with the armed tool.
            self.session.update(cx, |session, cx| {
                session.begin_annotation(event.position, cx)
            });
            return;
        }
        let (add, committed) = {
            let session = self.session.read(cx);
            (
                // Shift extends a collection only before anything is
                // committed; once the region is determined Shift adds nothing.
                event.modifiers.shift
                    && session.intent() == CaptureIntent::Choose
                    && session.selection().is_none(),
                session.committed_selection().is_some(),
            )
        };
        // Once a region is locked, a click outside no longer starts a new
        // selection — that used to wipe the annotations by accident.
        if committed {
            return;
        }
        // A new region gets the toolbar back at its automatic spot; a dragged
        // position only sticks for the region it was chosen for.
        self.toolbar_position = None;
        self.toolbar_bounds.set(None);
        self.style_open = false;
        self.session.update(cx, |session, cx| {
            session.begin_selection(frame_ix, event.position, add, cx)
        });
    }

    /// Track what the idle cursor is over: a resize handle on the region edge,
    /// or an annotation that a press would pick. Both change the cursor.
    fn update_hover_handle(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let frame_ix = self.frame_ix;
        let next = self
            .session
            .read(cx)
            .committed_selection()
            .filter(|_| {
                let session = self.session.read(cx);
                !session.collecting() && !session.is_composed()
            })
            .filter(|selection| selection.frame_ix() == frame_ix)
            .and_then(|selection| ResizeHandle::at(selection.bounds(), position));
        let over_shape = next.is_none()
            && !self.session.read(cx).multi_selection()
            && self.session.read(cx).annotation_under(position);
        if next != self.hover_handle || over_shape != self.hover_shape {
            self.hover_handle = next;
            self.hover_shape = over_shape;
            cx.notify();
        }
    }

    fn handle_toolbar_drag_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(mut drag) = self.toolbar_drag else {
            return false;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            self.toolbar_drag = None;
            cx.notify();
            return true;
        }
        if !drag.active {
            // A press without movement stays a button click.
            let dx = (event.position.x - drag.press.x).as_f32().abs();
            let dy = (event.position.y - drag.press.y).as_f32().abs();
            if dx < 2.0 && dy < 2.0 {
                return true;
            }
            drag.active = true;
            self.style_open = false;
        }
        let bar_size = if self.scroll_annotation.is_some() {
            size(
                px(crate::scroll_bar::SCROLL_BAR_SIZE.0),
                px(crate::scroll_bar::SCROLL_BAR_SIZE.1 + STYLE_ROW_HEIGHT),
            )
        } else {
            let height = if self.session.read(cx).annotations().selected().is_some()
                && !self.session.read(cx).multi_selection()
            {
                TOOLBAR_HEIGHT + STYLE_ROW_HEIGHT
            } else {
                TOOLBAR_HEIGHT
            };
            let width = self
                .toolbar_bounds
                .get()
                .map_or(px(TOOLBAR_WIDTH), |bounds| bounds.size.width);
            size(width, px(height))
        };
        let pos = clamp_toolbar_origin_with_size(
            event.position - drag.grab,
            window.viewport_size(),
            bar_size,
        );
        self.toolbar_position = Some(pos);
        if let Some(controller) = self.scroll_controller() {
            controller.update(cx, |controller, _| {
                controller.set_scroll_bar_local_origin(pos)
            });
        }
        self.toolbar_drag = Some(drag);
        cx.notify();
        true
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The magnifier follows every move, even while a range is being dragged.
        self.pointer = Some(event.position);
        if self.handle_toolbar_drag_move(event, window, cx) {
            return;
        }
        if self.scroll_annotation.is_some() {
            if event.pressed_button == Some(MouseButton::Left)
                && let Some(local) = self.scroll_local(event.position, cx)
                && let Some(controller) = self.scroll_controller()
            {
                controller.update(cx, |controller, cx| {
                    controller.move_scroll_annotation(local[0], local[1], cx)
                });
            }
            return;
        }
        // Only the pre-selection loupe and the Shift-collect preview put the
        // pointer on screen. Once a region is locked, a move changes nothing
        // unless it enters/leaves an annotation or a drag is running, and both
        // of those notify on their own.
        if !self.selection_committed(cx) || self.session.read(cx).collecting() {
            cx.notify();
        }
        let frame_ix = self.frame_ix;
        if event.pressed_button != Some(MouseButton::Left) {
            self.update_hover_handle(event.position, cx);
            self.session.update(cx, |session, cx| {
                session.set_hover(frame_ix, event.position, cx)
            });
            return;
        }
        if self.session.read(cx).is_resizing() {
            self.session.update(cx, |session, cx| {
                session.resize_to(frame_ix, event.position, cx)
            });
            return;
        }
        if self.session.read(cx).annotations().is_editing() {
            self.session.update(cx, |session, cx| {
                session.drag_annotation_edit(event.position, cx)
            });
            return;
        }
        if self.session.read(cx).annotations().is_dragging() {
            self.session.update(cx, |session, cx| {
                session.drag_annotation(event.position, cx)
            });
            return;
        }
        self.session.update(cx, |session, cx| {
            session.drag_to(frame_ix, event.position, cx)
        });
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.pointer = Some(event.position);
        cx.notify();
        if self.toolbar_drag.take().is_some() {
            return;
        }
        if self.scroll_annotation.is_some() {
            if let Some(controller) = self.scroll_controller() {
                controller.update(cx, |controller, cx| controller.end_scroll_annotation(cx));
            }
            return;
        }
        if self.session.read(cx).is_resizing() {
            self.session
                .update(cx, |session, cx| session.end_resize(cx));
            self.update_hover_handle(event.position, cx);
            return;
        }
        if self.session.read(cx).annotations().is_editing() {
            self.session
                .update(cx, |session, cx| session.end_annotation_edit(cx));
            return;
        }
        if self.session.read(cx).annotations().is_dragging() {
            self.session
                .update(cx, |session, cx| session.end_annotation(cx));
            return;
        }
        self.session
            .update(cx, |session, cx| session.end_selection(cx));
    }

    /// The ctrl+left rewrite is a macOS thing, so only macOS reads the event
    /// and the window.
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
    fn on_right_click(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.scroll_annotation.is_some() {
            cx.stop_propagation();
            return;
        }
        #[cfg(target_os = "macos")]
        if control_key_held() && !event.modifiers.control {
            // GPUI rewrites ctrl+left down into a right down and clears the
            // event's `control` flag. When a Ctrl-based capture hotkey (e.g.
            // Ctrl+Cmd+A) is still held, the press that should start a
            // selection drag arrives here instead of `on_mouse_down`, and the
            // rewrite also forces click_count to 1. Hand it back to the left
            // press path so dragging works without releasing the hotkey.
            // A genuine right-click keeps `event.modifiers.control` when Ctrl
            // is physically down, so it still takes the cancel path below.
            self.on_mouse_down(event, window, cx);
            return;
        }
        self.hover_handle = None;
        self.style_open = false;
        if self.session.read(cx).selection().is_some() {
            self.session
                .update(cx, |session, cx| session.clear_selection(cx));
        } else {
            self.request(SessionCommand::Cancel, cx);
        }
    }

    /// macOS GPUI also rewrites ctrl+left *up* into a right up, so the regular
    /// `on_mouse_up(Left)` never closes a drag whose hotkey is still held.
    /// Nothing to rewrite anywhere else, which leaves the parameters unread.
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
    fn on_right_mouse_up(
        &mut self,
        event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(target_os = "macos")]
        if control_key_held() && !event.modifiers.control {
            self.on_mouse_up(event, window, cx);
        }
    }

    /// The wheel widens the preview step by step, ending at the whole display.
    /// It works in either direction: the first scroll picks the direction and
    /// reversing it picks the smaller element again.
    fn on_scroll(&mut self, event: &ScrollEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.scroll_annotation.is_some() {
            cx.stop_propagation();
            let delta = event.delta.pixel_delta(window.line_height()).y.as_f32();
            if delta != 0.0
                && let Some(controller) = self.scroll_controller()
            {
                let scale = controller.read(cx).scroll_annotation_scale().unwrap_or(1.0);
                controller.update(cx, |controller, cx| {
                    controller.shift_scroll_annotation_origin(-(delta * scale) as i64, cx);
                });
            }
            return;
        }
        let delta = match event.delta {
            ScrollDelta::Pixels(delta) => delta.y.as_f32(),
            ScrollDelta::Lines(delta) => delta.y,
        };
        if delta == 0.0 {
            return;
        }
        let frame_ix = self.frame_ix;
        if self
            .session
            .update(cx, |session, cx| session.cycle_hover(frame_ix, delta, cx))
        {
            cx.stop_propagation();
        }
    }

    fn on_annotation_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.scroll_annotation.is_some() {
            let Some(controller) = self.scroll_controller() else {
                return;
            };
            let typing = controller
                .read(cx)
                .scroll_annotations()
                .is_some_and(Layer::is_typing);
            let stroke = &event.keystroke;
            if stroke.modifiers.control || stroke.modifiers.platform {
                if stroke.key.as_str() == "z" {
                    controller.update(cx, |controller, cx| controller.undo_scroll_annotation(cx));
                    cx.stop_propagation();
                }
                return;
            }
            if !typing {
                if !stroke.modifiers.function
                    && !stroke.modifiers.alt
                    && !stroke.modifiers.shift
                    && matches!(stroke.key.as_str(), "backspace" | "delete")
                {
                    controller.update(cx, |controller, cx| controller.delete_scroll_annotation(cx));
                    cx.stop_propagation();
                }
                return;
            }
            let handled = match stroke.key.as_str() {
                "backspace" | "delete" => {
                    controller.update(cx, |controller, cx| {
                        controller.backspace_scroll_annotation(cx)
                    });
                    true
                }
                "enter" => {
                    controller.update(cx, |controller, cx| controller.commit_scroll_annotation(cx));
                    true
                }
                "escape" => {
                    controller.update(cx, |controller, cx| {
                        controller.cancel_scroll_annotation_draft(cx)
                    });
                    true
                }
                _ => {
                    if let Some(text) = stroke.key_char.as_deref()
                        && !text.is_empty()
                        && text.chars().all(|ch| !ch.is_control())
                    {
                        controller.update(cx, |controller, cx| {
                            controller.insert_scroll_annotation_text(text, cx)
                        });
                        true
                    } else {
                        false
                    }
                }
            };
            if handled {
                cx.stop_propagation();
            }
            return;
        }
        let stroke = &event.keystroke;
        let plain = !stroke.modifiers.control
            && !stroke.modifiers.platform
            && !stroke.modifiers.function
            && !stroke.modifiers.alt
            && !stroke.modifiers.shift;
        if !self.typing(cx) {
            // Delete removes the picked annotation; typing keeps backspace.
            if plain
                && matches!(stroke.key.as_str(), "backspace" | "delete")
                && self.session.read(cx).annotations().selected().is_some()
            {
                self.style_open = false;
                self.session
                    .update(cx, |session, cx| session.delete_selected_annotation(cx));
                cx.stop_propagation();
            }
            return;
        }
        if stroke.modifiers.control || stroke.modifiers.platform || stroke.modifiers.function {
            return;
        }
        let handled = match stroke.key.as_str() {
            "backspace" | "delete" => {
                self.session
                    .update(cx, |session, cx| session.annotation_backspace(cx));
                true
            }
            "enter" | "escape" => false,
            _ => {
                if let Some(text) = stroke.key_char.as_deref()
                    && !text.is_empty()
                    && text.chars().all(|c| !c.is_control())
                {
                    self.session
                        .update(cx, |session, cx| session.annotation_insert(text, cx));
                    true
                } else {
                    false
                }
            }
        };
        if handled {
            cx.stop_propagation();
        }
    }

    /// Pixel loupe beside the cursor: the physical pixels under the pointer
    /// and the color of the center one.
    fn render_magnifier(&self, viewport: Bounds<Pixels>, cx: &App) -> Option<AnyElement> {
        let at = self.pointer?;
        // The loupe is a pre-selection tool: once a region is locked it only
        // gets in the way of the toolbar, annotations and resize handles.
        if self.typing(cx) || self.selection_committed(cx) {
            return None;
        }
        let sample = self
            .session
            .read(cx)
            .frame(self.frame_ix)?
            .color_sample(at, MAGNIFIER_CELLS)?;
        let theme = cx.theme();
        let side = MAGNIFIER_CELLS as usize;
        let grid = MAGNIFIER_CELLS as f32 * MAGNIFIER_CELL;
        let card_w = grid + MAGNIFIER_PAD * 2.0 + 2.0;
        let card_h = card_w + 26.0;
        let mut left = at.x.as_f32() + MAGNIFIER_GAP;
        if left + card_w > viewport.size.width.as_f32() {
            left = at.x.as_f32() - MAGNIFIER_GAP - card_w;
        }
        let mut top = at.y.as_f32() + MAGNIFIER_GAP;
        if top + card_h > viewport.size.height.as_f32() {
            top = at.y.as_f32() - MAGNIFIER_GAP - card_h;
        }
        let center = (side / 2) as f32 * MAGNIFIER_CELL;
        // 169 cells as 169 styled divs meant 169 layout nodes rebuilt on every
        // pointer move; one canvas paints the same quads with no layout at all.
        let cells = sample.cells.clone();
        let cell = MAGNIFIER_CELL;
        let grid_el = canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                for (ix, color) in cells.iter().enumerate() {
                    let at = point(
                        bounds.origin.x + px((ix % side) as f32 * cell),
                        bounds.origin.y + px((ix / side) as f32 * cell),
                    );
                    window.paint_quad(fill(Bounds::new(at, size(px(cell), px(cell))), rgb(*color)));
                }
            },
        )
        .w(px(grid))
        .h(px(grid));
        Some(
            div()
                .absolute()
                .left(px(left.max(0.0)))
                .top(px(top.max(0.0)))
                .p(px(MAGNIFIER_PAD))
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.border)
                .bg(theme.popover)
                .shadow_md()
                .overflow_hidden()
                .child(
                    div()
                        .relative()
                        .w(px(grid))
                        .h(px(grid))
                        .flex()
                        .flex_col()
                        .child(grid_el)
                        .child(
                            div()
                                .absolute()
                                .left(px(center - 1.0))
                                .top(px(center - 1.0))
                                .size(px(MAGNIFIER_CELL + 2.0))
                                .border_1()
                                .border_color(rgb(0x000000)),
                        )
                        .child(
                            div()
                                .absolute()
                                .left(px(center))
                                .top(px(center))
                                .size(px(MAGNIFIER_CELL))
                                .border_1()
                                .border_color(rgb(0xffffff)),
                        ),
                )
                .child(
                    div()
                        .pt(px(5.0))
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(
                            div()
                                .size(px(14.0))
                                .rounded(px(3.0))
                                .bg(rgb(sample.center))
                                .border_1()
                                .border_color(theme.border),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.popover_foreground)
                                .child(format!("#{:06X}", sample.center)),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_mask(
        &self,
        selections: &[Bounds<Pixels>],
        dim: bool,
        viewport: Bounds<Pixels>,
        cx: &App,
    ) -> Vec<gpui_kit::AnyElement> {
        if !dim {
            return Vec::new();
        }
        let color = mask_color(cx);
        shade_rects(selections, viewport)
            .into_iter()
            .map(|bounds| {
                div()
                    .absolute()
                    .left(bounds.origin.x)
                    .top(bounds.origin.y)
                    .w(bounds.size.width)
                    .h(bounds.size.height)
                    .bg(color)
                    .into_any_element()
            })
            .collect()
    }

    /// The toolbar keeps the position the user dragged it to; otherwise it
    /// follows the selection.
    fn toolbar_place(
        &self,
        selection: Bounds<Pixels>,
        viewport: Bounds<Pixels>,
        toolbar_size: Size<Pixels>,
    ) -> Point<Pixels> {
        self.toolbar_position
            .map(|pos| clamp_toolbar_origin_with_size(pos, viewport.size, toolbar_size))
            .unwrap_or_else(|| toolbar_origin(selection, viewport, toolbar_size.height.as_f32()))
    }

    fn render_toolbar(
        &self,
        selection: Bounds<Pixels>,
        viewport: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // Primary row: annotation tools, capture modes, then export actions.
        // Clicking an annotation tool toggles or switches the attached inline
        // style bar below; picking an existing annotation also reveals it.
        let session = self.session.read(cx);
        let annotations = session.annotations();
        let current = annotations.tool();
        let active_tool = annotations.active_tool();
        let style_size = annotations.style().size;
        let picked = annotations.selected().is_some();
        let can_undo = annotations.can_undo();
        let multi = session.multi_selection();
        let composed = session.is_composed();
        let count = session.selection_count();
        let show_style = !multi && (self.style_open || picked);
        let toolbar_height = if show_style {
            TOOLBAR_HEIGHT + STYLE_ROW_HEIGHT
        } else {
            TOOLBAR_HEIGHT
        };
        let toolbar_width = self
            .toolbar_bounds
            .get()
            .map_or(px(TOOLBAR_WIDTH), |bounds| bounds.size.width);
        let origin =
            self.toolbar_place(selection, viewport, size(toolbar_width, px(toolbar_height)));
        let right = selection.origin.x + selection.size.width;
        let anchored_right = right >= px(TOOLBAR_WIDTH);
        let moved = self.toolbar_position.is_some();
        let theme = cx.theme().clone();
        let overlay = cx.entity();
        let toolbar_bounds = self.toolbar_bounds.clone();

        div()
            .id("selection-toolbar")
            .flex()
            .flex_col()
            .items_start()
            .gap(px(4.))
            .absolute()
            .map(|this| {
                if moved {
                    this.left(origin.x).top(origin.y)
                } else if anchored_right {
                    this.top(origin.y).right(viewport.size.width - right)
                } else {
                    this.top(origin.y).left(selection.origin.x)
                }
            })
            .child(
                capture_toolbar::toolbar_card(&theme)
                    .relative()
                    .occlude()
                    .cursor_default()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            let actual_origin = this
                                .toolbar_bounds
                                .get()
                                .map_or(origin, |bounds| bounds.origin);
                            this.toolbar_drag = Some(ToolbarDrag {
                                grab: event.position - actual_origin,
                                press: event.position,
                                active: false,
                            });
                            cx.stop_propagation();
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                        if this.handle_toolbar_drag_move(event, window, cx) {
                            cx.stop_propagation();
                        }
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            if this.toolbar_drag.take().is_some() {
                                cx.notify();
                            }
                        }),
                    )
                    .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                    .child(
                        canvas(
                            move |bounds, _, _| toolbar_bounds.set(Some(bounds)),
                            |_, _, _, _| (),
                        )
                        .absolute()
                        .inset_0()
                        .size_full(),
                    )
                    .child(
                        Toolbar::new("selection-actions")
                            .when(multi, |this| {
                                this.content(
                                    div()
                                        .px_2()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(
                                            t!("overlay.multi_selected", count = count).to_string(),
                                        ),
                                )
                                .content(capture_toolbar::toolbar_divider(&theme))
                            })
                            .when(!multi, |this| {
                                let select_overlay = overlay.clone();
                                let undo_overlay = overlay.clone();
                                let clear_overlay = overlay.clone();
                                let this = capture_toolbar::append_annotation_tools(
                                    this,
                                    "sel",
                                    Some(current),
                                    can_undo,
                                    false,
                                    &theme,
                                    move |tool, _, _, cx| {
                                        select_overlay.update(cx, |this, cx| {
                                            let same = this.session.read(cx).annotations().tool()
                                                == tool
                                                && this
                                                    .session
                                                    .read(cx)
                                                    .annotations()
                                                    .selected()
                                                    .is_none();
                                            this.session.update(cx, |session, cx| {
                                                session.deselect_annotation(cx);
                                                session.set_annotation_tool(tool, cx);
                                            });
                                            this.style_open =
                                                if same { !this.style_open } else { true };
                                            cx.notify();
                                        });
                                    },
                                    move |_, _, cx| {
                                        undo_overlay.update(cx, |this, cx| {
                                            this.style_open = false;
                                            this.session.update(cx, |session, cx| {
                                                session.undo_annotations(cx)
                                            });
                                        });
                                    },
                                    move |_, _, cx| {
                                        clear_overlay.update(cx, |this, cx| {
                                            this.style_open = false;
                                            this.session.update(cx, |session, cx| {
                                                session.clear_annotations(cx)
                                            });
                                        });
                                    },
                                );
                                this.content(capture_toolbar::toolbar_divider(&theme))
                            })
                            .child(
                                capture_toolbar::toolbar_button(
                                    "scroll-capture",
                                    LucideIcon::ScrollText,
                                    t!("overlay.scroll").to_string(),
                                    false,
                                    multi || composed,
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| this.request(SessionCommand::Scroll, cx),
                                )),
                            )
                            .child({
                                let format = crate::controller::last_animation_format(cx);
                                let label = t!(
                                    "overlay.record.animation_hint",
                                    format = crate::controller::format_label(format)
                                )
                                .to_string();
                                capture_toolbar::toolbar_button(
                                    "record-animation",
                                    LucideIcon::Film,
                                    label,
                                    false,
                                    multi || composed,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.request(SessionCommand::Record(format), cx)
                                    },
                                ))
                            })
                            .child(
                                capture_toolbar::toolbar_button(
                                    "record-video",
                                    LucideIcon::Video,
                                    t!("overlay.record.video").to_string(),
                                    false,
                                    multi || composed,
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.request(SessionCommand::Record(RecordFormat::Mp4), cx)
                                    },
                                )),
                            )
                            .child(
                                capture_toolbar::toolbar_button(
                                    "edit-selection",
                                    LucideIcon::Crop,
                                    t!("overlay.edit").to_string(),
                                    false,
                                    false,
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| this.request(SessionCommand::Edit, cx),
                                )),
                            )
                            .content(capture_toolbar::toolbar_divider(&theme))
                            .map(|toolbar| {
                                let ocr_overlay = overlay.clone();
                                let pin_overlay = overlay.clone();
                                let save_overlay = overlay.clone();
                                let cancel_overlay = overlay.clone();
                                let copy_overlay = overlay.clone();
                                capture_toolbar::append_export_actions(
                                    toolbar,
                                    "sel",
                                    capture_toolbar::ExportActionLabels::overlay(),
                                    false,
                                    false,
                                    move |_, _, cx| {
                                        ocr_overlay.update(cx, |this, cx| {
                                            this.request(SessionCommand::Recognize, cx)
                                        });
                                    },
                                    move |_, _, cx| {
                                        pin_overlay.update(cx, |this, cx| {
                                            this.request(SessionCommand::Pin, cx)
                                        });
                                    },
                                    move |_, _, cx| {
                                        save_overlay.update(cx, |this, cx| {
                                            this.request(SessionCommand::Save, cx)
                                        });
                                    },
                                    move |_, _, cx| {
                                        cancel_overlay.update(cx, |this, cx| {
                                            this.request(SessionCommand::Cancel, cx)
                                        });
                                    },
                                    move |_, _, cx| {
                                        copy_overlay.update(cx, |this, cx| {
                                            this.request(SessionCommand::Copy, cx)
                                        });
                                    },
                                )
                            }),
                    ),
            )
            .when(show_style, |this| {
                let delete_overlay = overlay.clone();
                let on_delete = picked.then(|| -> capture_toolbar::DeleteHandler {
                    Box::new(move |_, _, cx| {
                        delete_overlay.update(cx, |this, cx| {
                            this.style_open = false;
                            this.session
                                .update(cx, |session, cx| session.delete_selected_annotation(cx));
                        });
                    })
                });
                this.child(
                    capture_toolbar::toolbar_card(&theme)
                        .occlude()
                        .cursor_default()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                        .child(capture_toolbar::render_annotation_style_bar(
                            "sel-style",
                            active_tool,
                            style_size,
                            &self.color_picker,
                            &self.size_sliders[active_tool.style_index()],
                            on_delete,
                            &theme,
                        )),
                )
            })
    }
}

/// Dim-mask color from the configured opacity (0-100). A plain black with the
/// requested alpha keeps the outside uniformly dark regardless of theme;
/// higher opacity makes the un-dimmed selection stand out more.
fn mask_color(cx: &App) -> Hsla {
    let alpha = (AppSettings::get(cx).capture.mask_opacity as f32 / 100.).clamp(0., 1.);
    gpui_kit::hsla(0., 0., 0., alpha)
}

/// A real, gaussian box-shadow. GPUI's drop-shadow shader fills the whole
/// shadow rect (including the element's interior) because it assumes an opaque
/// element covers the middle; our selection frame is transparent, so we paint
/// the shadow layer and then restore the pristine screenshot inside the frame
/// on top of it. Only the blur that bleeds outside the frame remains visible.
fn selection_shadow(enabled: bool) -> Vec<BoxShadow> {
    if !enabled {
        return Vec::new();
    }
    vec![
        BoxShadow::new(px(0.), px(2.), gpui_kit::hsla(0., 0., 0., 0.5))
            .blur_radius(px(14.))
            .spread_radius(px(2.)),
    ]
}

/// A frame with a real gaussian shadow whose interior is restored to the
/// pristine screenshot. The shadow layer fills the whole rect; the image layer
/// clips the full-screen frame to the selection so only the blur bleeding past
/// the border is visible, never a dark tint over the pixels being captured.
fn shadowed_frame(
    bounds: Bounds<Pixels>,
    shadows: Vec<BoxShadow>,
    image: &Arc<RenderImage>,
    viewport: Bounds<Pixels>,
    scale: f32,
    border: Hsla,
) -> Vec<AnyElement> {
    if shadows.is_empty() {
        return selection_outline(bounds, scale, border);
    }
    let mut layers = vec![
        div()
            .absolute()
            .left(bounds.origin.x)
            .top(bounds.origin.y)
            .w(bounds.size.width)
            .h(bounds.size.height)
            .shadow(shadows)
            .into_any_element(),
        div()
            .absolute()
            .left(bounds.origin.x)
            .top(bounds.origin.y)
            .w(bounds.size.width)
            .h(bounds.size.height)
            .overflow_hidden()
            .child(
                img(image.clone())
                    .absolute()
                    .left(-bounds.origin.x)
                    .top(-bounds.origin.y)
                    .w(viewport.size.width)
                    .h(viewport.size.height)
                    .object_fit(ObjectFit::Fill),
            )
            .into_any_element(),
    ];
    layers.extend(selection_outline(bounds, scale, border));
    layers
}

fn selection_outline_rects(bounds: Bounds<Pixels>, scale: f32) -> [Bounds<Pixels>; 4] {
    let line = px(1.0 / scale);
    let left = bounds.origin.x;
    let top = bounds.origin.y;
    let right = bounds.right();
    let bottom = bounds.bottom();
    [
        Bounds::new(
            point(left - line, top - line),
            size(bounds.size.width + line + line, line),
        ),
        Bounds::new(
            point(left - line, bottom),
            size(bounds.size.width + line + line, line),
        ),
        Bounds::new(point(left - line, top), size(line, bounds.size.height)),
        Bounds::new(point(right, top), size(line, bounds.size.height)),
    ]
}

fn selection_outline(bounds: Bounds<Pixels>, scale: f32, color: Hsla) -> Vec<AnyElement> {
    selection_outline_rects(bounds, scale)
        .into_iter()
        .map(|bounds| {
            div()
                .absolute()
                .left(bounds.origin.x)
                .top(bounds.origin.y)
                .w(bounds.size.width)
                .h(bounds.size.height)
                .bg(color)
                .into_any_element()
        })
        .collect()
}

/// A shaded band: top/bottom edges plus the free x-intervals inside it.
type ShadeBand = (f32, f32, Vec<(f32, f32)>);

/// The dimmed part of the viewport: everything outside the selected regions.
/// Splitting into horizontal bands with their free x-intervals keeps the four
/// strips of a single selection and still cuts several holes for Shift-drag.
fn shade_rects(selections: &[Bounds<Pixels>], viewport: Bounds<Pixels>) -> Vec<Bounds<Pixels>> {
    const EPS: f32 = 0.01;
    let width = viewport.size.width.as_f32();
    let height = viewport.size.height.as_f32();
    if width <= 0.0 || height <= 0.0 {
        return Vec::new();
    }
    let rects: Vec<(f32, f32, f32, f32)> = selections
        .iter()
        .filter_map(|selection| {
            let left = selection.origin.x.as_f32().clamp(0.0, width);
            let top = selection.origin.y.as_f32().clamp(0.0, height);
            let right =
                (selection.origin.x.as_f32() + selection.size.width.as_f32()).clamp(0.0, width);
            let bottom =
                (selection.origin.y.as_f32() + selection.size.height.as_f32()).clamp(0.0, height);
            (right - left >= EPS && bottom - top >= EPS).then_some((left, top, right, bottom))
        })
        .collect();
    if rects.is_empty() {
        return vec![viewport];
    }
    let mut edges = vec![0.0, height];
    for (_, top, _, bottom) in &rects {
        edges.push(*top);
        edges.push(*bottom);
    }
    edges.sort_by(f32::total_cmp);
    edges.dedup_by(|a, b| (*a - *b).abs() < EPS);
    let mut bands: Vec<ShadeBand> = Vec::new();
    for pair in edges.windows(2) {
        let (top, bottom) = (pair[0], pair[1]);
        if bottom - top < EPS {
            continue;
        }
        let mut holes: Vec<(f32, f32)> = rects
            .iter()
            .filter(|(_, rect_top, _, rect_bottom)| {
                *rect_top <= top + EPS && bottom <= *rect_bottom + EPS
            })
            .map(|(left, _, right, _)| (*left, *right))
            .collect();
        holes.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut covered: Vec<(f32, f32)> = Vec::new();
        for (left, right) in holes {
            match covered.last_mut() {
                Some(last) if left <= last.1 + EPS => last.1 = last.1.max(right),
                _ => covered.push((left, right)),
            }
        }
        let mut free = Vec::new();
        let mut cursor = 0.0;
        for (left, right) in covered {
            if left - cursor > EPS {
                free.push((cursor, left));
            }
            cursor = cursor.max(right);
        }
        if width - cursor > EPS {
            free.push((cursor, width));
        }
        if free.is_empty() {
            continue;
        }
        // Only merge bands that actually touch. A hole spanning the full
        // viewport width leaves a band with no free space in between: merging
        // the strips above and below it would stretch one strip across the
        // hole and dim the selection it was supposed to leave clear.
        let same_as_last = bands.last().is_some_and(|(_, last_bottom, previous)| {
            (top - *last_bottom).abs() < EPS
                && previous.len() == free.len()
                && previous
                    .iter()
                    .zip(&free)
                    .all(|(a, b)| (a.0 - b.0).abs() < EPS && (a.1 - b.1).abs() < EPS)
        });
        if same_as_last {
            if let Some(last) = bands.last_mut() {
                last.1 = bottom;
            }
        } else {
            bands.push((top, bottom, free));
        }
    }
    bands
        .into_iter()
        .flat_map(|(top, bottom, free)| {
            free.into_iter().map(move |(left, right)| {
                Bounds::new(
                    viewport.origin + point(px(left), px(top)),
                    size(px(right - left), px(bottom - top)),
                )
            })
        })
        .collect()
}

/// The live recording frame: the capture overlay's look without the frozen
/// image — everything outside the region dimmed, and the one-pixel selection
/// border kept just outside the captured pixels so it stays out of the file.
fn recording_frame(
    selection: Bounds<Pixels>,
    viewport: Bounds<Pixels>,
    scale: f32,
    warning: bool,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let outline = if warning {
        theme.warning
    } else {
        theme.primary
    };
    div()
        .size_full()
        .children(
            shade_rects(&[selection], viewport)
                .into_iter()
                .map(|bounds| {
                    div()
                        .absolute()
                        .left(bounds.origin.x)
                        .top(bounds.origin.y)
                        .w(bounds.size.width)
                        .h(bounds.size.height)
                        .bg(mask_color(cx))
                        .into_any_element()
                }),
        )
        .children(selection_outline(selection, scale, outline))
        .into_any_element()
}

fn resize_cursor(handle: Option<ResizeHandle>) -> CursorStyle {
    match handle {
        None => CursorStyle::Crosshair,
        Some(ResizeHandle::NorthWest | ResizeHandle::SouthEast) => {
            CursorStyle::ResizeUpLeftDownRight
        }
        Some(ResizeHandle::NorthEast | ResizeHandle::SouthWest) => {
            CursorStyle::ResizeUpRightDownLeft
        }
        Some(ResizeHandle::North | ResizeHandle::South) => CursorStyle::ResizeUpDown,
        Some(ResizeHandle::East | ResizeHandle::West) => CursorStyle::ResizeLeftRight,
    }
}

/// Eight small squares on a committed selection, one per resize handle.
fn resize_handles(bounds: Bounds<Pixels>, fill: Hsla, border: Hsla) -> Vec<AnyElement> {
    const SIZE: f32 = 8.0;
    let left = bounds.origin.x.as_f32();
    let top = bounds.origin.y.as_f32();
    let right = left + bounds.size.width.as_f32();
    let bottom = top + bounds.size.height.as_f32();
    let (mid_x, mid_y) = ((left + right) / 2.0, (top + bottom) / 2.0);
    [
        (left, top),
        (mid_x, top),
        (right, top),
        (right, mid_y),
        (right, bottom),
        (mid_x, bottom),
        (left, bottom),
        (left, mid_y),
    ]
    .into_iter()
    .map(|(x, y)| {
        div()
            .absolute()
            .left(px(x - SIZE / 2.0))
            .top(px(y - SIZE / 2.0))
            .size(px(SIZE))
            .rounded(px(2.0))
            .bg(fill)
            .border_1()
            .border_color(border)
            .into_any_element()
    })
    .collect()
}

/// 选区对齐到物理像素网格：半透明遮罩由四块矩形拼成，小数坐标的公共边会因抗锯齿显出横线。
fn snap_bounds(bounds: Bounds<Pixels>, scale: f32) -> Bounds<Pixels> {
    let step = 1.0 / scale;
    let snap = |value: Pixels| px((value.as_f32() / step).round() * step);
    Bounds::new(
        point(snap(bounds.origin.x), snap(bounds.origin.y)),
        size(snap(bounds.size.width), snap(bounds.size.height)),
    )
}

/// 文字标注的输入法支持：草稿即文本缓冲，接收键位置于 `draft_origin`。
impl EntityInputHandler for OverlayView {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        if let Some(controller) = self.scroll_controller() {
            let (text, range) = controller
                .read(cx)
                .scroll_annotations()?
                .text_for_range(range_utf16)?;
            *actual_range = Some(range);
            return Some(text);
        }
        let (text, range) = self
            .session
            .read(cx)
            .annotations()
            .text_for_range(range_utf16)?;
        *actual_range = Some(range);
        Some(text)
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        if let Some(controller) = self.scroll_controller() {
            let end = controller
                .read(cx)
                .scroll_annotations()?
                .text_length_utf16()?;
            return Some(UTF16Selection {
                range: end..end,
                reversed: false,
            });
        }
        let end = self.session.read(cx).annotations().text_length_utf16()?;
        Some(UTF16Selection {
            range: end..end,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, cx: &mut Context<Self>) -> Option<Range<usize>> {
        if let Some(controller) = self.scroll_controller() {
            return controller
                .read(cx)
                .scroll_annotations()?
                .marked_text_range();
        }
        self.session.read(cx).annotations().marked_text_range()
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(controller) = self.scroll_controller() {
            controller.update(cx, |controller, cx| {
                controller.unmark_scroll_annotation_text(cx)
            });
        } else {
            self.session
                .update(cx, |session, cx| session.annotation_unmark_text(cx));
        }
        window.notify_text_input_changed(&self.focus_handle);
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(controller) = self.scroll_controller() {
            controller.update(cx, |controller, cx| {
                controller.replace_scroll_annotation_text(range_utf16, text, cx)
            });
        } else {
            self.session.update(cx, |session, cx| {
                session.annotation_replace_text(range_utf16, text, cx)
            });
        }
        window.notify_text_input_changed(&self.focus_handle);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(controller) = self.scroll_controller() {
            controller.update(cx, |controller, cx| {
                controller.replace_and_mark_scroll_annotation_text(range_utf16, new_text, cx)
            });
        } else {
            self.session.update(cx, |session, cx| {
                session.annotation_replace_and_mark_text(range_utf16, new_text, cx)
            });
        }
        window.notify_text_input_changed(&self.focus_handle);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        if let Some(controller) = self.scroll_controller() {
            let (origin_y, scale, _, _) = controller.read(cx).scroll_annotation_state()?;
            let annotations = controller.read(cx).scroll_annotations()?;
            let at = annotations.draft_origin()?;
            let font_size = annotations.draft_style().map_or(16.0, |style| style.size);
            let origin = point(
                element_bounds.origin.x + px(at[0]),
                element_bounds.origin.y + px(at[1] - origin_y as f32 / scale),
            );
            return Some(Bounds::new(origin, size(px(1.), px(font_size))));
        }
        let annotations = self.session.read(cx).annotations();
        let at = annotations.draft_origin()?;
        let font_size = annotations.draft_style().map_or(16.0, |style| style.size);
        let origin = point(
            element_bounds.origin.x + px(at[0]),
            element_bounds.origin.y + px(at[1]),
        );
        Some(Bounds::new(origin, size(px(1.), px(font_size))))
    }

    fn character_index_for_point(
        &mut self,
        _: gpui_kit::Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

impl OverlayView {
    fn selection_watermark_preview(
        &self,
        multi: bool,
        composed: bool,
        cx: &App,
    ) -> Option<Arc<RenderImage>> {
        let config = &AppSettings::get(cx).watermark;
        if !config.target_enabled(WatermarkTarget::Screenshot) || (multi && !composed) {
            self.watermark_preview.borrow_mut().take();
            return None;
        }
        let Some(preset) = config.preset(WatermarkTarget::Screenshot) else {
            self.watermark_preview.borrow_mut().take();
            return None;
        };
        let session = self.session.read(cx);
        let key = session.crop_key()?;
        if let Some(cached) = self.watermark_preview.borrow().as_ref()
            && cached.key.0 == key
            && cached.key.1 == *preset
        {
            return cached.result.clone().ok().flatten();
        }
        let preset = preset.clone();
        let result = (|| -> std::result::Result<Option<Arc<RenderImage>>, String> {
            let (width, height, scale) = session
                .selection_pixel_dimensions()
                .ok_or_else(|| "selection pixels are unavailable".to_string())?;
            let spec = crate::export::watermark_spec(&preset, AppSettings::watermark_dir(cx))
                .map_err(|err| err.to_string())?;
            let layer = spec
                .rasterize(width, height, scale)
                .map_err(|err| err.to_string())?;
            if layer.is_empty() {
                return Ok(None);
            }
            let mut pixels = RgbaImage::new(width, height);
            layer.apply_sdr_mut(&mut pixels);
            Ok(Some(crate::session::render_image(&pixels)))
        })();
        if let Err(error) = &result {
            log::warn!("selection watermark preview failed: {error}");
        }
        self.watermark_preview
            .replace(Some(SelectionWatermarkCache {
                key: (key, preset),
                result: result.clone(),
            }));
        result.ok().flatten()
    }
}

impl OverlayView {
    fn render_scroll_annotation(
        &mut self,
        selection: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(controller) = self.scroll_controller() else {
            return recording_frame(
                selection,
                Bounds::new(Default::default(), window.viewport_size()),
                window.scale_factor(),
                false,
                cx,
            );
        };
        self.sync_style_controls(window, cx);
        let Some((origin_y, scale, paused, revision)) =
            controller.read(cx).scroll_annotation_state()
        else {
            return recording_frame(
                selection,
                Bounds::new(Default::default(), window.viewport_size()),
                window.scale_factor(),
                false,
                cx,
            );
        };
        let Some(layer) = controller.read(cx).scroll_annotations().cloned() else {
            return div().into_any_element();
        };
        let Some(snap) = controller.read(cx).scroll_snapshot() else {
            return div().into_any_element();
        };
        let preview_visible = controller.read(cx).is_scroll_preview_visible();
        let finishing = controller.read(cx).is_finishing_scroll();
        let can_act = !finishing;
        let can_export = snap.started && !finishing;
        let viewport = Bounds::new(Default::default(), window.viewport_size());
        let theme = cx.theme().clone();
        let current = layer.tool();
        let can_undo = layer.can_undo();
        let style = layer.style();
        let active = layer.active_tool();
        let picked = layer.selected().is_some();
        let bar_size = size(
            px(crate::scroll_bar::SCROLL_BAR_SIZE.0),
            px(crate::scroll_bar::SCROLL_BAR_SIZE.1 + STYLE_ROW_HEIGHT),
        );
        let fallback_origin = controller
            .read(cx)
            .scroll_bar_local_origin()
            .unwrap_or_else(|| toolbar_origin(selection, viewport, bar_size.height.as_f32()));
        let bar_origin = clamp_toolbar_origin_with_size(
            self.toolbar_position.unwrap_or(fallback_origin),
            viewport.size,
            bar_size,
        );
        let render_image = match &self.scroll_pause_render {
            Some((cached_rev, cached_img)) if *cached_rev == revision => cached_img.clone(),
            _ => {
                let image = crate::session::render_image(&paused);
                self.scroll_pause_render = Some((revision, image.clone()));
                image
            }
        };
        let paint_cache = self.paint_cache.clone();
        let overlay = cx.entity();
        let focus_handle = self.focus_handle.clone();
        let canvas_layer = layer.clone();
        let canvas_origin_y = origin_y;
        let canvas_scale = scale;
        let canvas_paused = paused.clone();
        let canvas_revision = revision;
        let width = paused.width() as f32 / scale;
        let height = paused.height() as f32 / scale;

        let select_controller = controller.clone();
        let drag_overlay = overlay.clone();
        let on_delete = picked.then(|| -> capture_toolbar::DeleteHandler {
            let controller = controller.clone();
            Box::new(move |_, _, cx| {
                controller.update(cx, |controller, cx| controller.delete_scroll_annotation(cx));
            })
        });
        let style_card = capture_toolbar::toolbar_card(&theme)
            .occlude()
            .cursor_default()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .child(capture_toolbar::render_annotation_style_bar(
                "scroll-ann-style",
                active,
                style.size,
                &self.color_picker,
                &self.size_sliders[active.style_index()],
                on_delete,
                &theme,
            ));
        let bar_card = crate::scroll_bar::render_scroll_bar_card(
            controller.clone(),
            &snap,
            Some(current),
            preview_visible,
            can_act,
            can_export,
            can_undo,
            "scroll-ann-bar",
            move |tool, _, _, cx| {
                let should_finish = {
                    let layer = select_controller.read(cx).scroll_annotations();
                    layer.is_some_and(|l| l.tool() == tool && l.selected().is_none())
                };
                select_controller.update(cx, |controller, cx| {
                    if should_finish {
                        controller.finish_scroll_annotation(cx);
                    } else {
                        controller.set_scroll_annotation_tool(tool, cx);
                    }
                });
            },
            move |event, _, cx| {
                drag_overlay.update(cx, |this, _| {
                    this.toolbar_drag = Some(ToolbarDrag {
                        grab: event.position - bar_origin,
                        press: event.position,
                        active: false,
                    });
                });
                cx.stop_propagation();
            },
            None,
            &theme,
        )
        .shadow_md()
        .occlude()
        .cursor_default()
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
            if this.handle_toolbar_drag_move(event, window, cx) {
                cx.stop_propagation();
            }
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, _, _, cx| {
                if this.toolbar_drag.take().is_some() {
                    cx.notify();
                }
            }),
        );
        let annotation_bar = div()
            .id("scroll-annotation-bar")
            .absolute()
            .left(bar_origin.x)
            .top(bar_origin.y)
            .flex()
            .flex_col()
            .items_start()
            .gap(px(4.))
            .child(bar_card)
            .child(style_card);

        div()
            .id("scroll-annotation-overlay")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .overflow_hidden()
            .cursor_crosshair()
            .on_action(cx.listener(|this, _: &Cancel, _, cx| {
                let Some(controller) = this.scroll_controller() else {
                    return;
                };
                if controller
                    .read(cx)
                    .scroll_annotations()
                    .is_some_and(Layer::is_typing)
                {
                    controller.update(cx, |controller, cx| {
                        controller.cancel_scroll_annotation_draft(cx)
                    });
                } else {
                    controller.update(cx, |controller, cx| controller.finish_scroll_annotation(cx));
                }
            }))
            .on_action(cx.listener(|this, _: &ConfirmSelection, _, cx| {
                let Some(controller) = this.scroll_controller() else {
                    return;
                };
                if controller
                    .read(cx)
                    .scroll_annotations()
                    .is_some_and(Layer::is_typing)
                {
                    controller.update(cx, |controller, cx| controller.commit_scroll_annotation(cx));
                } else {
                    controller.update(cx, |controller, cx| controller.finish_scroll_annotation(cx));
                }
            }))
            .on_action(cx.listener(|this, _: &UndoAnnotations, _, cx| {
                if let Some(controller) = this.scroll_controller() {
                    controller.update(cx, |controller, cx| controller.undo_scroll_annotation(cx));
                }
            }))
            .on_key_down(cx.listener(Self::on_annotation_key))
            .on_scroll(cx.listener(Self::on_scroll))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_right_click))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_right_mouse_up))
            .child(recording_frame(selection, viewport, scale, false, cx))
            .child(
                div()
                    .absolute()
                    .left(selection.origin.x)
                    .top(selection.origin.y)
                    .w(selection.size.width)
                    .h(selection.size.height)
                    .overflow_hidden()
                    .child(
                        img(render_image)
                            .absolute()
                            .inset_0()
                            .size_full()
                            .object_fit(ObjectFit::Fill),
                    )
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, _, window, cx| {
                                let clip = [
                                    0.0,
                                    canvas_origin_y as f32 / canvas_scale,
                                    width,
                                    canvas_origin_y as f32 / canvas_scale + height,
                                ];
                                let source = canvas_layer.needs_source().then(|| {
                                    annotate::MosaicSource::source_at_with_ratio(
                                        &canvas_paused,
                                        canvas_revision,
                                        [0.0, canvas_origin_y as f32 / canvas_scale],
                                        canvas_scale,
                                    )
                                });
                                let mut cache = paint_cache.lock().ok();
                                let mut fallback = annotate::MosaicCache::default();
                                let mosaics = cache
                                    .as_deref_mut()
                                    .map_or(&mut fallback, |cache| &mut cache.mosaics);
                                annotate::paint_layer_zoomed_clipped(
                                    &canvas_layer,
                                    bounds.origin
                                        - point(px(0.), px(canvas_origin_y as f32 / canvas_scale)),
                                    canvas_scale,
                                    1.0,
                                    clip,
                                    source,
                                    mosaics,
                                    window,
                                    cx,
                                );
                                if canvas_layer.is_typing() {
                                    window.handle_input(
                                        &focus_handle,
                                        ElementInputHandler::new(bounds, overlay.clone()),
                                        cx,
                                    );
                                }
                            },
                        )
                        .absolute()
                        .inset_0()
                        .size_full(),
                    ),
            )
            .child(annotation_bar)
            .when(self.toolbar_drag.is_some_and(|drag| drag.active), |this| {
                this.child(
                    div()
                        .id("toolbar-drag-shield")
                        .absolute()
                        .inset_0()
                        .size_full()
                        .cursor_grabbing()
                        .on_mouse_move(cx.listener(|this, event, window, cx| {
                            this.handle_toolbar_drag_move(event, window, cx);
                            cx.stop_propagation();
                        }))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.toolbar_drag = None;
                                cx.notify();
                                cx.stop_propagation();
                            }),
                        ),
                )
            })
            .into_any_element()
    }
}

impl Render for OverlayView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(selection) = self.recording {
            if self.scroll_annotation.is_some() {
                return self.render_scroll_annotation(selection, window, cx);
            }
            let viewport = Bounds::new(Default::default(), window.viewport_size());
            return recording_frame(
                selection,
                viewport,
                window.scale_factor(),
                self.live_warning,
                cx,
            );
        }
        self.sync_style_controls(window, cx);
        let session = self.session.read(cx);
        let Some(frame) = session.frame(self.frame_ix) else {
            return div().into_any_element();
        };
        let image = frame.image().clone();
        let scale = frame.physical_scale();
        let physical_size = |bounds| frame.physical_size(bounds);
        let selection = session
            .selection()
            .filter(|selection| selection.frame_ix() == self.frame_ix);
        let other_monitor_selected = session.selection().is_some() && selection.is_none();
        let selections_on_frame: Vec<Bounds<Pixels>> = session
            .selections_on_frame(self.frame_ix)
            .iter()
            .map(|selection| snap_bounds(selection.bounds(), scale))
            .collect();
        let intent = session.intent();
        let hint = intent.hint();
        let committed = selection.filter(|selection| !selection.is_dragging());
        let selection_bounds = selection.map(|selection| snap_bounds(selection.bounds(), scale));
        let committed_bounds = committed.map(|s| snap_bounds(s.bounds(), scale));
        let hover_bounds = session
            .hover_bounds(self.frame_ix)
            .map(|bounds| snap_bounds(bounds, scale));
        let collecting = session.collecting();
        let composed = session.is_composed();
        let multi = session.multi_selection();
        let watermark_preview = committed_bounds.and_then(|_| {
            (intent == CaptureIntent::Choose)
                .then(|| self.selection_watermark_preview(multi, composed, cx))
                .flatten()
        });
        // Crosshair only while a region can still be drawn. Once the region is
        // locked the arrow comes back; over an annotation a hand says the
        // press will pick it instead of drawing.
        let cursor = if let Some(handle) = self.hover_handle {
            resize_cursor(Some(handle))
        } else if self.hover_shape {
            CursorStyle::OpenHand
        } else if collecting || committed.is_none() {
            // A Shift collection keeps the crosshair: the next drag adds
            // another region rather than annotating the finished one.
            CursorStyle::Crosshair
        } else {
            CursorStyle::Arrow
        };
        // The region a Shift-click would remove, drawn in the danger color.
        let remove_bounds = if collecting {
            self.pointer
                .and_then(|position| session.selection_at(self.frame_ix, position))
                .map(|selection| snap_bounds(selection.bounds(), scale))
        } else {
            None
        };
        // Before a selection is committed (or while collecting), the hovered
        // target is lit like a selection so it reads as the region a click
        // would capture. A finished selection stops those previews.
        let mut mask_holes = selections_on_frame.clone();
        mask_holes.extend(hover_bounds);
        // Before a selection exists, the hovered window previews like one.
        let preview_bounds = hover_bounds.or(selection_bounds);
        let toast = self.toast.clone();
        let size_label = preview_bounds.map(|bounds| {
            let (w, h) = physical_size(bounds);
            format!("{w} × {h}")
        });
        let viewport = Bounds::new(Default::default(), window.viewport_size());
        let shadows = selection_shadow(AppSettings::get(cx).capture.selection_shadow);
        let theme = cx.theme();
        let (ring, label_bg, label_fg, muted, radius, toast_border, danger) = (
            theme.primary,
            theme.popover,
            theme.popover_foreground,
            theme.muted_foreground,
            theme.radius,
            theme.border,
            theme.danger,
        );
        let collect_hint = if remove_bounds.is_some() {
            t!("session.hint.collect_remove").to_string()
        } else {
            t!("session.hint.collect").to_string()
        };
        let session_entity = self.session.clone();
        let paint_cache = self.paint_cache.clone();
        let frame_ix = self.frame_ix;
        let overlay = cx.entity();
        let ime_focus = self.focus_handle.clone();
        let magnifier = self.render_magnifier(viewport, cx);

        // Shift is the multi-select modifier: holding it collects regions and
        // releasing it finishes, like Xnip's multiple-window capture.
        let collect_session = self.session.clone();
        div()
            .id("capture-overlay")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_modifiers_changed(move |event, _, cx| {
                collect_session.update(cx, |session, cx| {
                    session.set_collecting(event.modifiers.shift, cx)
                });
            })
            .relative()
            .size_full()
            .overflow_hidden()
            .cursor(cursor)
            .on_action(cx.listener(|this, _: &Cancel, _, cx| {
                if this.typing(cx) {
                    this.style_open = false;
                    this.session
                        .update(cx, |session, cx| session.cancel_annotation_draft(cx));
                    return;
                }
                // Esc exits the capture in one press; right-click still clears
                // the selection first.
                this.request(SessionCommand::Cancel, cx);
            }))
            .on_action(cx.listener(|this, _: &ConfirmSelection, _, cx| {
                if this.typing(cx) {
                    this.style_open = false;
                    this.session
                        .update(cx, |session, _| session.finish_annotations());
                    cx.notify();
                    return;
                }
                this.request(SessionCommand::Copy, cx)
            }))
            .on_action(
                cx.listener(|this, _: &CopySelection, _, cx| {
                    this.request(SessionCommand::Copy, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &SaveSelection, _, cx| {
                    this.request(SessionCommand::Save, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &PinSelection, _, cx| this.request(SessionCommand::Pin, cx)),
            )
            .on_action(cx.listener(|this, _: &RecognizeSelection, _, cx| {
                this.request(SessionCommand::Recognize, cx)
            }))
            .on_action(cx.listener(|this, _: &ScrollCapture, _, cx| {
                if this.session.read(cx).is_composed() {
                    return;
                }
                this.request(SessionCommand::Scroll, cx)
            }))
            .on_action(cx.listener(|this, _: &RecordAnimation, _, cx| {
                if this.session.read(cx).is_composed() {
                    return;
                }
                this.request(
                    SessionCommand::Record(crate::controller::last_animation_format(cx)),
                    cx,
                )
            }))
            .on_action(cx.listener(|this, _: &RecordVideo, _, cx| {
                if this.session.read(cx).is_composed() {
                    return;
                }
                this.request(SessionCommand::Record(RecordFormat::Mp4), cx)
            }))
            .on_action(cx.listener(|this, _: &EditSelection, _, cx| {
                if this.typing(cx) || !this.selection_committed(cx) {
                    return;
                }
                this.request(SessionCommand::Edit, cx)
            }))
            .on_action(cx.listener(|this, _: &UndoAnnotations, _, cx| {
                this.style_open = false;
                this.session
                    .update(cx, |session, cx| session.undo_annotations(cx));
            }))
            .on_action(cx.listener(|this, _: &ClearAnnotations, _, cx| {
                this.style_open = false;
                this.session
                    .update(cx, |session, cx| session.clear_annotations(cx));
            }))
            .on_action(cx.listener(|this, _: &SelectFullScreen, _, cx| {
                // `F` picks a display while choosing a region; after that it
                // would silently replace the annotated selection.
                if this.typing(cx) || this.selection_committed(cx) {
                    return;
                }
                let frame_ix = this.frame_ix;
                this.style_open = false;
                this.session
                    .update(cx, |session, cx| session.select_full(frame_ix, cx));
            }))
            .on_action(cx.listener(|this, _: &CopyColor, _, cx| this.copy_color(cx)))
            .on_key_down(cx.listener(Self::on_annotation_key))
            .on_scroll(cx.listener(Self::on_scroll))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_right_click))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_right_mouse_up))
            .on_mouse_exit(cx.listener(|this, _: &MouseExitEvent, _, cx| {
                if this.pointer.take().is_some() {
                    cx.notify();
                }
            }))
            .child(
                img(image.clone())
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .object_fit(ObjectFit::Fill),
            )
            .children(self.render_mask(
                &mask_holes,
                session.selection().is_some() || hover_bounds.is_some(),
                viewport,
                cx,
            ))
            .when(
                selection_bounds.is_none() && !other_monitor_selected && !collecting,
                |this| {
                    this.child(
                        div()
                            .absolute()
                            .top_6()
                            .left_0()
                            .w_full()
                            .flex()
                            .justify_center()
                            .child(
                                div()
                                    .px_3()
                                    .py_1()
                                    .rounded(radius)
                                    .bg(label_bg)
                                    .text_color(muted)
                                    .text_sm()
                                    .child(hint),
                            ),
                    )
                },
            )
            .children(selections_on_frame.clone().into_iter().flat_map({
                let image = image.clone();
                let shadows = shadows.clone();
                move |bounds| {
                    let removing = remove_bounds == Some(bounds);
                    shadowed_frame(
                        bounds,
                        shadows.clone(),
                        &image,
                        viewport,
                        scale,
                        if removing { danger } else { ring },
                    )
                }
            }))
            .when_some(hover_bounds, |this, bounds| {
                this.children(shadowed_frame(
                    bounds,
                    shadows.clone(),
                    &image,
                    viewport,
                    scale,
                    ring,
                ))
            })
            .when_some(preview_bounds, |this, bounds| {
                let label_top = if bounds.origin.y >= px(28.) {
                    bounds.origin.y - px(28.)
                } else {
                    bounds.origin.y + px(4.)
                };
                this.when_some(size_label, |this, label| {
                    this.child(
                        div()
                            .absolute()
                            .left(bounds.origin.x)
                            .top(label_top)
                            .px_2()
                            .py_0p5()
                            .rounded(radius)
                            .bg(label_bg)
                            .text_color(label_fg)
                            .text_xs()
                            .child(label),
                    )
                })
            })
            .when_some(committed_bounds, |this, bounds| {
                this.child(
                    div()
                        .absolute()
                        .left(bounds.origin.x)
                        .top(bounds.origin.y)
                        .w(bounds.size.width)
                        .h(bounds.size.height)
                        .overflow_hidden()
                        .child(
                            canvas(
                                |_, _, _| (),
                                move |bounds, _, window, cx| {
                                    let (layer, scale, crop_key) = {
                                        let session = session_entity.read(cx);
                                        let Some(frame) = session.frame(frame_ix) else {
                                            return;
                                        };
                                        let layer = session.annotations().clone();
                                        let scale = frame.physical_scale();
                                        let crop_key = layer
                                            .needs_source()
                                            .then(|| session.crop_key())
                                            .flatten();
                                        (layer, scale, crop_key)
                                    };
                                    // The mosaic needs the region pixels, and this
                                    // closure runs on every frame: cut them once per
                                    // region instead of once per frame.
                                    let mut cache = paint_cache.lock().ok();
                                    if let Some(cache) = cache.as_deref_mut() {
                                        match crop_key {
                                            None => {
                                                cache.crop = None;
                                                cache.mosaics.clear();
                                            }
                                            Some(key)
                                                if cache
                                                    .crop
                                                    .as_ref()
                                                    .is_none_or(|(hit, _)| *hit != key) =>
                                            {
                                                cache.crop = session_entity
                                                    .read(cx)
                                                    .crop_selection()
                                                    .map(|image| (key, image));
                                            }
                                            Some(_) => {}
                                        }
                                    }
                                    let mut fallback = annotate::MosaicCache::default();
                                    let (source, mosaics) = match cache.as_deref_mut() {
                                        Some(cache) => (
                                            cache.crop.as_ref().map(|(key, image)| {
                                                annotate::MosaicSource::memory(image, key.stamp())
                                            }),
                                            &mut cache.mosaics,
                                        ),
                                        None => (None, &mut fallback),
                                    };
                                    annotate::paint_layer(
                                        &layer,
                                        bounds.origin,
                                        scale,
                                        source,
                                        mosaics,
                                        window,
                                        cx,
                                    );
                                    if layer.is_typing() {
                                        window.handle_input(
                                            &ime_focus,
                                            ElementInputHandler::new(bounds, overlay.clone()),
                                            cx,
                                        );
                                    }
                                },
                            )
                            .size_full(),
                        ),
                )
                .when_some(watermark_preview, |this, preview| {
                    this.child(
                        img(preview)
                            .absolute()
                            .left(bounds.origin.x)
                            .top(bounds.origin.y)
                            .w(bounds.size.width)
                            .h(bounds.size.height)
                            .object_fit(ObjectFit::Fill),
                    )
                })
                .when(!collecting && !composed, |this| {
                    this.children(resize_handles(bounds, ring, cx.theme().background))
                })
            })
            .when_some(
                committed_bounds.filter(|_| intent == CaptureIntent::Choose && !collecting),
                |this, bounds| this.child(self.render_toolbar(bounds, viewport, cx)),
            )
            .when_some(magnifier, |this, loupe| this.child(loupe))
            .when(collecting, |this| {
                this.child(
                    div()
                        .absolute()
                        .bottom(px(20.))
                        .left(px(20.))
                        .px_3()
                        .py_1()
                        .rounded(radius)
                        .border_1()
                        .border_color(toast_border)
                        .bg(label_bg)
                        .text_color(if remove_bounds.is_some() {
                            danger
                        } else {
                            label_fg
                        })
                        .text_sm()
                        .shadow_md()
                        .child(collect_hint),
                )
            })
            .when_some(toast, |this, (text, _)| {
                this.child(
                    div()
                        .absolute()
                        .bottom(px(20.))
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .px_3()
                                .py_1()
                                .rounded(radius)
                                .border_1()
                                .border_color(toast_border)
                                .bg(label_bg)
                                .text_color(label_fg)
                                .text_sm()
                                .shadow_md()
                                .child(text),
                        ),
                )
            })
            .when(self.toolbar_drag.is_some_and(|drag| drag.active), |this| {
                this.child(
                    div()
                        .id("toolbar-drag-shield")
                        .absolute()
                        .inset_0()
                        .size_full()
                        .cursor_grabbing()
                        .on_mouse_move(cx.listener(|this, event, window, cx| {
                            this.handle_toolbar_drag_move(event, window, cx);
                            cx.stop_propagation();
                        }))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.toolbar_drag = None;
                                cx.notify();
                                cx.stop_propagation();
                            }),
                        ),
                )
            })
            .into_any_element()
    }
}

#[cfg(target_os = "macos")]
mod macos_window {
    use std::sync::{Once, OnceLock};

    use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, Sel};
    use objc2::{ffi, msg_send, sel};
    use objc2_foundation::NSRect;

    use gpui_kit::Window;

    type ConstrainFn =
        unsafe extern "C-unwind" fn(&AnyObject, Sel, NSRect, *mut AnyObject) -> NSRect;

    static ORIGINAL: OnceLock<Imp> = OnceLock::new();

    /// 全屏尺寸的帧原样返回，不被 AppKit 压到菜单栏下方；小窗（pin/录制条）仍走原约束。
    /// 创建阶段窗口尚未上屏，screen 为 nil，也要按所有屏幕的完整尺寸匹配，
    /// 否则原约束在这一步就把全屏帧下移，浮层整体靠下。
    unsafe extern "C-unwind" fn constrain_frame_rect(
        this: &AnyObject,
        selector: Sel,
        frame: NSRect,
        screen: *mut AnyObject,
    ) -> NSRect {
        let full = unsafe { is_full_screen_frame(frame, screen) };
        if full {
            return frame;
        }
        match ORIGINAL.get() {
            Some(&imp) => unsafe {
                std::mem::transmute::<Imp, ConstrainFn>(imp)(this, selector, frame, screen)
            },
            None => frame,
        }
    }

    unsafe fn is_full_screen_frame(frame: NSRect, screen: *mut AnyObject) -> bool {
        let full = |screen_frame: NSRect| {
            (frame.size.width - screen_frame.size.width).abs() < 1.0
                && (frame.size.height - screen_frame.size.height).abs() < 1.0
        };
        if !screen.is_null() {
            return full(unsafe { msg_send![screen, frame] });
        }
        let Some(class) = AnyClass::get(c"NSScreen") else {
            return false;
        };
        let screens: *mut AnyObject = unsafe { msg_send![class, screens] };
        let count: usize = unsafe { msg_send![screens, count] };
        for index in 0..count {
            let screen: *mut AnyObject = unsafe { msg_send![screens, objectAtIndex: index] };
            if full(unsafe { msg_send![screen, frame] }) {
                return true;
            }
        }
        false
    }

    fn native_view_and_window(window: &Window) -> Option<(*mut AnyObject, *mut AnyObject)> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};

        let handle = HasWindowHandle::window_handle(window).ok()?;
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            return None;
        };
        unsafe {
            let view = handle.ns_view.as_ptr().cast::<AnyObject>();
            let native_window: *mut AnyObject = msg_send![view, window];
            (!native_window.is_null()).then_some((view, native_window))
        }
    }

    fn native_window(window: &Window) -> Option<*mut AnyObject> {
        native_view_and_window(window).map(|(_, native_window)| native_window)
    }

    unsafe fn remove_tracking_areas(view: *mut AnyObject) {
        if view.is_null() {
            return;
        }
        let areas: *mut AnyObject = unsafe { msg_send![view, trackingAreas] };
        if areas.is_null() {
            return;
        }
        let count: usize = unsafe { msg_send![areas, count] };
        let mut list = Vec::with_capacity(count);
        for index in 0..count {
            let area: *mut AnyObject = unsafe { msg_send![areas, objectAtIndex: index] };
            if !area.is_null() {
                list.push(area);
            }
        }
        for area in list {
            unsafe {
                let _: () = msg_send![view, removeTrackingArea: area];
            }
        }
    }

    unsafe fn detach_native_window(native_window: *mut AnyObject, ns_view: *mut AnyObject) {
        unsafe {
            // 必须在 orderOut 之前清掉 delegate，避免窗口隐藏触发 windowDidResignKey
            // 并把 activate_callback 投递给即将或已经移除的 GPUI WindowId。
            let _: () = msg_send![native_window, setDelegate: std::ptr::null_mut::<AnyObject>()];
            let _: Bool =
                msg_send![native_window, makeFirstResponder: std::ptr::null_mut::<AnyObject>()];
            let _: () = msg_send![native_window, setIgnoresMouseEvents: Bool::YES];
            remove_tracking_areas(ns_view);
            let content_view: *mut AnyObject = msg_send![native_window, contentView];
            if !content_view.is_null() {
                remove_tracking_areas(content_view);
                let subviews: *mut AnyObject = msg_send![content_view, subviews];
                if !subviews.is_null() {
                    let count: usize = msg_send![subviews, count];
                    let mut list = Vec::with_capacity(count);
                    for index in 0..count {
                        let subview: *mut AnyObject = msg_send![subviews, objectAtIndex: index];
                        if !subview.is_null() {
                            list.push(subview);
                        }
                    }
                    for subview in list {
                        remove_tracking_areas(subview);
                    }
                }
            }
            let _: () = msg_send![native_window, orderOut: std::ptr::null_mut::<AnyObject>()];
        }
    }

    /// 预建浮层以零 alpha 置前：GPUI 在冻结期间就能建好 surface、画完透明首帧，而
    /// AppKit 的默认不透明底色不会露出来。GPUI 创建窗口时先 `orderFront` 再应用
    /// `window_background`，直接 `show: true` 会把整屏白底闪一下。零 alpha 的窗口也
    /// 不参与截图合成，冻结帧仍然只包含真实桌面，同时挡住冻结期间的误点击。
    ///
    /// 未出过帧的 `CAMetalLayer` 默认不透明黑，`reveal` 一恢复 alpha 就会黑一帧。
    /// 这里把图层底色清成透明，置前时最后一层画布就是桌面本身。
    pub fn pre_place(window: &mut Window) {
        let Some(native_window) = native_window(window) else {
            return;
        };
        unsafe {
            let _: () = msg_send![native_window, setLevel: super::OVERLAY_WINDOW_LEVEL];
            let _: () = msg_send![native_window, setAlphaValue: 0.0f64];
            let _: () = msg_send![native_window, setIgnoresMouseEvents: Bool::YES];
            clear_layer_background(native_window);
            let _: () = msg_send![
                native_window,
                orderFront: std::ptr::null_mut::<AnyObject>()
            ];
        }
    }

    pub fn set_window_level(window: &mut Window, level: isize) {
        let Some(native_window) = native_window(window) else {
            return;
        };
        unsafe {
            let _: () = msg_send![native_window, setLevel: level];
        }
    }

    pub fn frontmost_other_app() -> Option<i32> {
        let workspace_class = AnyClass::get(c"NSWorkspace")?;
        unsafe {
            let workspace: *mut AnyObject = msg_send![workspace_class, sharedWorkspace];
            if workspace.is_null() {
                return None;
            }
            let app: *mut AnyObject = msg_send![workspace, frontmostApplication];
            if app.is_null() {
                return None;
            }
            let pid: i32 = msg_send![app, processIdentifier];
            (pid > 0 && pid != std::process::id() as i32).then_some(pid)
        }
    }

    pub fn activate_app_single_window() {
        // NSApplicationActivateIgnoringOtherApps = 1 << 1，刻意不传
        // NSApplicationActivateAllWindows (1 << 0)，避免把后台其它面板拉到最前。
        const ACTIVATE_IGNORING_OTHER_APPS: usize = 1 << 1;
        let Some(app_class) = AnyClass::get(c"NSRunningApplication") else {
            return;
        };
        unsafe {
            let app: *mut AnyObject = msg_send![app_class, currentApplication];
            if !app.is_null() {
                let _: Bool = msg_send![app, activateWithOptions: ACTIVATE_IGNORING_OTHER_APPS];
            }
        }
    }

    pub fn restore_frontmost_app(pid: i32) {
        const ACTIVATE_IGNORING_OTHER_APPS: usize = 1 << 1;
        let Some(app_class) = AnyClass::get(c"NSRunningApplication") else {
            return;
        };
        unsafe {
            let app: *mut AnyObject =
                msg_send![app_class, runningApplicationWithProcessIdentifier: pid];
            if !app.is_null() {
                let _: Bool = msg_send![app, activateWithOptions: ACTIVATE_IGNORING_OTHER_APPS];
            }
        }
    }

    unsafe fn clear_layer_background(native_window: *mut AnyObject) {
        let content_view: *mut AnyObject = unsafe { msg_send![native_window, contentView] };
        if content_view.is_null() {
            return;
        }
        let layer: *mut AnyObject = unsafe { msg_send![content_view, layer] };
        if layer.is_null() {
            return;
        }
        let Some(color_class) = AnyClass::get(c"NSColor") else {
            return;
        };
        let clear: *mut AnyObject = unsafe { msg_send![color_class, clearColor] };
        let cg_color: *mut AnyObject = unsafe { msg_send![clear, CGColor] };
        unsafe {
            let _: () = msg_send![layer, setBackgroundColor: cg_color];
            let _: () = msg_send![layer, setOpaque: Bool::NO];
        }
    }

    /// 从屏幕移除窗口并摘除事件回调，保留 GPUI 状态供稍后的 `remove_window` 销毁。
    /// `orderOut` 是同步且近乎零成本的；真正昂贵的是 GPUI 释放全屏 layer 的那 37 ms。
    pub fn hide(window: &mut Window) {
        let Some((ns_view, native_window)) = native_view_and_window(window) else {
            return;
        };
        unsafe {
            detach_native_window(native_window, ns_view);
        }
    }

    /// 把窗口即刻置前并设为 key。GPUI 的 `activate_window` 把激活交给异步执行器，
    /// 预建的透明浮层在冻结帧写入后才需要接收键盘；同步置前保证首帧就带键焦点，
    /// 不会在 App 激活与渲染之间丢按键。alpha 恢复为 1 时才真正可见。
    pub fn reveal(window: &mut Window) {
        let Some(native_window) = native_window(window) else {
            return;
        };
        activate_app_single_window();
        unsafe {
            let _: () = msg_send![native_window, setLevel: super::OVERLAY_WINDOW_LEVEL];
            let _: () = msg_send![native_window, setAlphaValue: 1.0f64];
            let _: () = msg_send![native_window, setIgnoresMouseEvents: Bool::NO];
            let _: () = msg_send![
                native_window,
                makeKeyAndOrderFront: std::ptr::null_mut::<AnyObject>()
            ];
        }
    }

    /// GPUI 把浮层建成 titled 面板，AppKit 默认把它压到菜单栏下，再异步改 borderless 移回
    /// 全屏就会闪一下。这里替换 GPUIPanel 的 constrainFrameRect:toScreen:，全屏帧从创建起就
    /// 不被下压（titled 已无 chrome、level 101 盖住菜单栏），无需任何异步改样式。
    pub fn install() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| unsafe {
            let Some(class) = AnyClass::get(c"GPUIPanel") else {
                log::warn!("constrain override: GPUIPanel class not found");
                return;
            };
            let selector = sel!(constrainFrameRect:toScreen:);
            let Some(method) = class.instance_method(selector) else {
                log::warn!("constrain override: constrainFrameRect method not found");
                return;
            };
            let _ = ORIGINAL.set(method.implementation());
            let types = ffi::method_getTypeEncoding(method);
            let imp = std::mem::transmute::<ConstrainFn, Imp>(constrain_frame_rect as ConstrainFn);
            ffi::class_replaceMethod(
                class as *const AnyClass as *mut AnyClass,
                selector,
                imp,
                types,
            );
            log::info!("constrain override installed on GPUIPanel");
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{
        STYLE_ROW_HEIGHT, TOOLBAR_GAP, TOOLBAR_HEIGHT, TOOLBAR_WIDTH, clamp_toolbar_origin,
        selection_outline_rects, shade_rects, toolbar_origin,
    };
    use gpui_kit::{Bounds, Pixels, point, px, size};

    fn bounds(x: f32, y: f32, width: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(width), px(height)))
    }

    fn covered(rects: &[Bounds<Pixels>], x: f32, y: f32) -> bool {
        rects.iter().any(|rect| {
            let left = rect.origin.x.as_f32();
            let top = rect.origin.y.as_f32();
            x >= left
                && x < left + rect.size.width.as_f32()
                && y >= top
                && y < top + rect.size.height.as_f32()
        })
    }

    #[test]
    fn selection_outline_stays_one_device_pixel_outside_the_selection() {
        let selection = bounds(10.5, 20.5, 100.5, 50.5);
        let [top, bottom, left, right] = selection_outline_rects(selection, 2.0);
        assert_eq!(top, bounds(10., 20., 101.5, 0.5));
        assert_eq!(bottom, bounds(10., 71., 101.5, 0.5));
        assert_eq!(left, bounds(10., 20.5, 0.5, 50.5));
        assert_eq!(right, bounds(111., 20.5, 0.5, 50.5));
    }

    #[test]
    fn shade_leaves_every_selection_uncovered() {
        let viewport = bounds(0., 0., 100., 100.);
        let selections = [bounds(10., 10., 20., 20.), bounds(50., 50., 30., 40.)];
        let shade = shade_rects(&selections, viewport);
        assert!(!covered(&shade, 20., 20.));
        assert!(!covered(&shade, 60., 60.));
        assert!(covered(&shade, 5., 5.));
        assert!(covered(&shade, 40., 40.));
        assert!(covered(&shade, 90., 20.));
        let area: f32 = shade
            .iter()
            .map(|rect| rect.size.width.as_f32() * rect.size.height.as_f32())
            .sum();
        assert!((area - (100.0 * 100.0 - 20.0 * 20.0 - 30.0 * 40.0)).abs() < 0.1);
    }

    #[test]
    fn shade_covers_everything_without_selections() {
        let viewport = bounds(0., 0., 100., 100.);
        let shade = shade_rects(&[], viewport);
        assert_eq!(shade.len(), 1);
        assert_eq!(shade[0], viewport);
    }

    #[test]
    fn shade_keeps_a_full_width_selection_clear() {
        // A window as wide as the display leaves two strips with identical
        // free intervals and an empty band between them. Merging those strips
        // would dim the very window the user snapped to.
        let viewport = bounds(0., 0., 1728., 1117.);
        let selection = bounds(0., 115., 1728., 757.);
        let shade = shade_rects(&[selection], viewport);
        assert_eq!(shade.len(), 2);
        assert_eq!(shade[0], bounds(0., 0., 1728., 115.));
        assert_eq!(shade[1], bounds(0., 872., 1728., 245.));
        assert!(!covered(&shade, 100., 200.));
        assert!(!covered(&shade, 100., 800.));
        assert!(covered(&shade, 100., 50.));
        assert!(covered(&shade, 100., 1000.));
    }

    #[test]
    fn shade_keeps_several_full_width_selections_clear() {
        let viewport = bounds(0., 0., 100., 100.);
        let selections = [bounds(0., 10., 100., 20.), bounds(0., 50., 100., 20.)];
        let shade = shade_rects(&selections, viewport);
        assert_eq!(shade.len(), 3);
        let area: f32 = shade
            .iter()
            .map(|rect| rect.size.width.as_f32() * rect.size.height.as_f32())
            .sum();
        assert!((area - 6000.0).abs() < 0.1);
        assert!(!covered(&shade, 50., 20.));
        assert!(!covered(&shade, 50., 60.));
        assert!(covered(&shade, 50., 40.));
        assert!(covered(&shade, 50., 90.));
    }

    #[test]
    fn shade_is_empty_for_a_full_viewport_selection() {
        let viewport = bounds(0., 0., 100., 100.);
        assert!(shade_rects(&[viewport], viewport).is_empty());
    }

    #[test]
    fn toolbar_origin_places_below_selection_when_room_allows() {
        let viewport = bounds(0., 0., 1200., 800.);
        let selection = bounds(200., 100., 600., 300.);
        let collapsed = toolbar_origin(selection, viewport, TOOLBAR_HEIGHT);
        assert_eq!(
            collapsed,
            point(px(800. - TOOLBAR_WIDTH), px(400. + TOOLBAR_GAP))
        );

        let expanded = toolbar_origin(selection, viewport, TOOLBAR_HEIGHT + STYLE_ROW_HEIGHT);
        assert_eq!(expanded, collapsed);
    }

    #[test]
    fn toolbar_origin_flips_above_when_expanded_style_bar_hits_bottom_edge() {
        let viewport = bounds(0., 0., 1200., 800.);
        // Collapsed height (36) fits below at y = 740 + 8 = 748 (748 + 36 = 784 <= 800),
        // while expanded height (74) overflows (748 + 74 = 822 > 800) and moves above.
        let selection = bounds(200., 440., 600., 300.);
        let collapsed = toolbar_origin(selection, viewport, TOOLBAR_HEIGHT);
        assert_eq!(collapsed.y, px(740. + TOOLBAR_GAP));

        let expanded = toolbar_origin(selection, viewport, TOOLBAR_HEIGHT + STYLE_ROW_HEIGHT);
        assert_eq!(
            expanded.y,
            px(440. - TOOLBAR_HEIGHT - STYLE_ROW_HEIGHT - TOOLBAR_GAP)
        );
    }

    #[test]
    fn clamp_toolbar_origin_keeps_expanded_toolbar_inside_viewport() {
        let viewport = size(px(800.), px(600.));
        let expanded_h = TOOLBAR_HEIGHT + STYLE_ROW_HEIGHT;
        let clamped = clamp_toolbar_origin(point(px(700.), px(590.)), viewport, expanded_h);
        assert_eq!(clamped.x, px(800. - TOOLBAR_WIDTH));
        assert_eq!(clamped.y, px(600. - expanded_h));

        let negative = clamp_toolbar_origin(point(px(-20.), px(-10.)), viewport, TOOLBAR_HEIGHT);
        assert_eq!(negative, point(px(0.), px(0.)));
    }
}
