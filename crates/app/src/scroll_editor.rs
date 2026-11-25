//! Dedicated scrollable editor window for stitched long screenshots:
//! tile-rendered canvas (supports > 16384 px GPU height), top/bottom trim,
//! full 7-tool annotations + IME text, zoom, and direct Copy/Save/Pin/OCR.

use std::{
    cell::Cell,
    ops::Range,
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Selectable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState},
    h_flex,
    scroll::ScrollableElement as _,
    slider::{Slider, SliderEvent, SliderState, SliderValue},
    toolbar::Toolbar,
    v_flex,
};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, AsyncApp, Bounds, Context, CursorStyle, DisplayId,
    ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Hsla, InteractiveElement as _,
    IntoElement, KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ObjectFit, ParentElement as _, Pixels, Point, Render, RenderImage, Rgba,
    ScrollHandle, SharedString, StatefulInteractiveElement as _, Styled as _, StyledImage as _,
    Subscription, TitlebarOptions, UTF16Selection, WeakEntity, Window, WindowBounds, WindowOptions,
    actions, canvas, div, img, point, prelude::FluentBuilder as _, px, rgb, size,
};
use snapit_capture::{RgbaImage, WatermarkSpec};
use snapit_store::{AnnotationTool, WatermarkTarget};

use crate::annotate::{self, Layer, Shape, Tool};
use crate::appearance;
use crate::controller::{Captured, Controller};
use crate::session;
use crate::settings::AppSettings;

const CONTEXT: &str = "ScrollEditor";
const TILE_MAX_HEIGHT: u32 = 2048;
const MIN_TRIM_HEIGHT_PX: u32 = 16;
const TOAST_DURATION: Duration = Duration::from_millis(1600);

actions!(
    scroll_editor,
    [
        CopyResult,
        SaveResult,
        UndoEdit,
        CancelEditor,
        ZoomIn,
        ZoomOut,
        FitWidth,
        ActualSize
    ]
);

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new(crate::shortcut!("c"), CopyResult, Some(CONTEXT)),
        KeyBinding::new(crate::shortcut!("s"), SaveResult, Some(CONTEXT)),
        KeyBinding::new(crate::shortcut!("z"), UndoEdit, Some(CONTEXT)),
        KeyBinding::new(crate::shortcut!("="), ZoomIn, Some(CONTEXT)),
        KeyBinding::new(crate::shortcut!("-"), ZoomOut, Some(CONTEXT)),
        KeyBinding::new(crate::shortcut!("0"), ActualSize, Some(CONTEXT)),
        KeyBinding::new("escape", CancelEditor, Some(CONTEXT)),
    ]);
}

pub fn open(
    image: RgbaImage,
    scale: f32,
    display_id: Option<DisplayId>,
    cx: &mut App,
) -> anyhow::Result<AnyWindowHandle> {
    let window_size = size(px(920.), px(720.));
    let min_size = size(px(640.), px(520.));
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            display_id,
            window_size,
            cx,
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some(t!("scroll_editor.title").to_string().into()),
            ..Default::default()
        }),
        window_min_size: Some(min_size),
        display_id,
        ..Default::default()
    };
    let handle = gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| ScrollEditorView::new(image, scale, window, cx))
    })?
    .0;
    Ok(handle)
}

struct EditorTile {
    id: usize,
    height: u32,
    render: Arc<RenderImage>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TrimEdge {
    Top,
    Bottom,
}

fn build_tiles(image: &RgbaImage) -> Vec<EditorTile> {
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 {
        return Vec::new();
    }
    let count = height.div_ceil(TILE_MAX_HEIGHT) as usize;
    let mut tiles = Vec::with_capacity(count);
    for ix in 0..count {
        let y0 = ix as u32 * TILE_MAX_HEIGHT;
        let h = TILE_MAX_HEIGHT.min(height - y0);
        let slice = image::imageops::crop_imm(image, 0, y0, width, h).to_image();
        tiles.push(EditorTile {
            id: ix,
            height: h,
            render: session::render_image(&slice),
        });
    }
    tiles
}

/// Crop `image` vertically to `[trim_top .. trim_bottom]` and shift annotation
/// shapes into the cropped coordinate space.
fn crop_and_shift_shapes(
    image: &RgbaImage,
    shapes: &[Shape],
    scale: f32,
    trim_top: u32,
    trim_bottom: u32,
) -> (RgbaImage, Option<Arc<Vec<Shape>>>) {
    let (width, height) = image.dimensions();
    let top = trim_top.min(height.saturating_sub(1));
    let bottom = trim_bottom.clamp(top + 1, height);
    let cropped = if top == 0 && bottom == height {
        image.clone()
    } else {
        image::imageops::crop_imm(image, 0, top, width, bottom - top).to_image()
    };
    let shifted = if shapes.is_empty() {
        None
    } else {
        let dy = -(top as f32 / scale.max(0.1));
        Some(Arc::new(annotate::shift_shapes_vertical(shapes, dy)))
    };
    (cropped, shifted)
}

pub struct ScrollEditorView {
    image: Arc<RgbaImage>,
    scale: f32,
    tiles: Vec<EditorTile>,
    annotations: Layer,
    color_picker: Entity<ColorPickerState>,
    size_sliders: Vec<Entity<SliderState>>,
    color_shown: u32,
    size_shown: [f32; 7],
    style_open: bool,
    mosaics: Arc<Mutex<annotate::MosaicCache>>,
    trim_top: u32,
    trim_bottom: u32,
    trim_mode: bool,
    trim_drag: Option<TrimEdge>,
    zoom: f32,
    scroll_handle: ScrollHandle,
    canvas_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    focus_handle: FocusHandle,
    toast: Option<(SharedString, Instant)>,
    _subscriptions: Vec<Subscription>,
}

impl ScrollEditorView {
    fn new(image: RgbaImage, scale: f32, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let scale = if scale.is_finite() && scale > 0.1 {
            scale
        } else {
            1.0
        };
        let (width, height) = image.dimensions();
        let tiles = build_tiles(&image);
        let logical_w = (width as f32 / scale).max(1.0);
        let fit_zoom = (700.0 / logical_w).clamp(0.2, 1.0);
        let defaults = AppSettings::get(cx).annotations.clone();
        let annotations = Layer::new(defaults.clone());
        let style_color = annotations.style().color;

        let color_picker = cx.new(|cx| {
            ColorPickerState::new(window, cx).default_value(Hsla::from(rgb(style_color)))
        });
        let mut subscriptions = vec![
            appearance::follow_system_theme(window),
            cx.subscribe(&color_picker, |this, _, event: &ColorPickerEvent, cx| {
                let ColorPickerEvent::Change(Some(color)) = event else {
                    return;
                };
                let rgb_color = color_to_rgb(*color);
                this.annotations.set_color(rgb_color);
                this.persist_defaults(cx);
                cx.notify();
            }),
        ];

        let mut size_sliders = Vec::new();
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
            subscriptions.push(
                cx.subscribe(&state, move |this, _, event: &SliderEvent, cx| {
                    let (value, release) = match event {
                        SliderEvent::Change(SliderValue::Single(v)) => (*v, false),
                        SliderEvent::Release(SliderValue::Single(v)) => (*v, true),
                        _ => return,
                    };
                    if this.annotations.active_tool().style_index() != ix {
                        return;
                    }
                    this.annotations.set_size(value);
                    if release {
                        this.persist_defaults(cx);
                    }
                    cx.notify();
                }),
            );
            size_sliders.push(state);
        }

        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);

        Self {
            image: Arc::new(image),
            scale,
            tiles,
            annotations,
            color_picker,
            size_sliders,
            color_shown: style_color,
            size_shown,
            style_open: false,
            mosaics: Arc::new(Mutex::new(annotate::MosaicCache::default())),
            trim_top: 0,
            trim_bottom: height,
            trim_mode: false,
            trim_drag: None,
            zoom: fit_zoom,
            scroll_handle: ScrollHandle::new(),
            canvas_bounds: Rc::new(Cell::new(None)),
            focus_handle,
            toast: None,
            _subscriptions: subscriptions,
        }
    }

    fn persist_defaults(&self, cx: &mut App) {
        if self.annotations.selected().is_some() {
            return;
        }
        let defaults = self.annotations.config().clone();
        AppSettings::update(cx, |config| {
            config.annotations = defaults;
        });
    }

    fn sync_style_controls(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let style = self.annotations.style();
        let ix = self.annotations.active_tool().style_index();
        if self.color_shown != style.color {
            self.color_shown = style.color;
            let color = Hsla::from(rgb(style.color));
            self.color_picker
                .update(cx, |state, cx| state.set_value(color, window, cx));
        }
        if self.size_shown[ix] != style.size {
            self.size_shown[ix] = style.size;
            self.size_sliders[ix].update(cx, |state, cx| state.set_value(style.size, window, cx));
        }
    }

    fn show_toast(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.toast = Some((text.into(), Instant::now()));
        cx.notify();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor().timer(TOAST_DURATION).await;
            this.update(cx, |this, cx| {
                if this
                    .toast
                    .as_ref()
                    .is_some_and(|(_, at)| at.elapsed() >= TOAST_DURATION)
                {
                    this.toast = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn to_logical(&self, window_pos: Point<Pixels>) -> Option<[f32; 2]> {
        let bounds = self.canvas_bounds.get()?;
        let x = ((window_pos.x - bounds.origin.x).as_f32() / self.zoom).max(0.0);
        let y = ((window_pos.y - bounds.origin.y).as_f32() / self.zoom).max(0.0);
        let max_x = self.image.width() as f32 / self.scale;
        let max_y = self.image.height() as f32 / self.scale;
        Some([x.min(max_x), y.min(max_y)])
    }

    fn is_trimmed(&self) -> bool {
        self.trim_top > 0 || self.trim_bottom < self.image.height()
    }

    fn export_captured(&mut self, cx: &mut Context<Self>) -> anyhow::Result<Captured> {
        self.annotations.commit();
        let watermark_cfg = AppSettings::get(cx).watermark.clone();
        let watermark =
            if watermark_cfg.enabled && watermark_cfg.target_enabled(WatermarkTarget::Screenshot) {
                watermark_cfg
                    .preset(WatermarkTarget::Screenshot)
                    .ok_or_else(|| anyhow::anyhow!("watermark template is unavailable"))
                    .and_then(|preset| {
                        crate::export::watermark_spec(preset, AppSettings::watermark_dir(cx))
                    })?
            } else {
                WatermarkSpec::default()
            };
        let (cropped, shapes) = crop_and_shift_shapes(
            &self.image,
            self.annotations.committed(),
            self.scale,
            self.trim_top,
            self.trim_bottom,
        );
        Ok(Captured::from_parts(
            cropped, None, self.scale, shapes, watermark,
        ))
    }

    fn copy_result(&mut self, cx: &mut Context<Self>) {
        match self.export_captured(cx) {
            Ok(captured) => {
                let (w, h) = captured.clean_sdr.dimensions();
                Controller::global(cx).update(cx, |controller, cx| {
                    controller.remember_and_copy(captured, cx);
                });
                self.show_toast(
                    t!("scroll_editor.copied", width = w, height = h).to_string(),
                    cx,
                );
            }
            Err(err) => self.show_toast(format!("{err:#}"), cx),
        }
    }

    fn save_result(&mut self, cx: &mut Context<Self>) {
        match self.export_captured(cx) {
            Ok(captured) => {
                Controller::global(cx).update(cx, |controller, cx| {
                    controller.remember_and_save(captured, cx);
                });
            }
            Err(err) => self.show_toast(format!("{err:#}"), cx),
        }
    }

    fn pin_result(&mut self, cx: &mut Context<Self>) {
        match self.export_captured(cx) {
            Ok(captured) => {
                Controller::global(cx).update(cx, |controller, cx| {
                    controller.remember_and_pin(captured, cx);
                });
            }
            Err(err) => self.show_toast(format!("{err:#}"), cx),
        }
    }

    fn ocr_result(&mut self, cx: &mut Context<Self>) {
        match self.export_captured(cx) {
            Ok(captured) => {
                Controller::global(cx).update(cx, |controller, cx| {
                    controller.recognize_captured(captured, cx);
                });
            }
            Err(err) => self.show_toast(format!("{err:#}"), cx),
        }
    }

    fn on_canvas_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window, cx);
        let Some(local) = self.to_logical(event.position) else {
            return;
        };
        if self.annotations.select_at(local[0], local[1]) {
            if event.click_count >= 2 && self.annotations.begin_text_edit(local[0], local[1]) {
                self.style_open = false;
                cx.notify();
                return;
            }
            let _ = self.annotations.begin_edit(local[0], local[1]);
            self.style_open = true;
            cx.notify();
            return;
        }
        self.annotations.pointer_down(local[0], local[1]);
        cx.notify();
    }

    fn on_canvas_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(edge) = self.trim_drag {
            if event.pressed_button != Some(MouseButton::Left) {
                self.trim_drag = None;
                return;
            }
            let Some(bounds) = self.canvas_bounds.get() else {
                return;
            };
            let py = (((event.position.y - bounds.origin.y).as_f32() / self.zoom) * self.scale)
                .round()
                .max(0.0) as u32;
            let max_h = self.image.height();
            match edge {
                TrimEdge::Top => {
                    self.trim_top = py.min(self.trim_bottom.saturating_sub(MIN_TRIM_HEIGHT_PX));
                }
                TrimEdge::Bottom => {
                    self.trim_bottom = py.clamp(self.trim_top + MIN_TRIM_HEIGHT_PX, max_h);
                }
            }
            cx.notify();
            return;
        }
        if event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        let Some(local) = self.to_logical(event.position) else {
            return;
        };
        let viewport = [
            self.image.width() as f32 / self.scale,
            self.image.height() as f32 / self.scale,
        ];
        if self.annotations.is_editing() {
            if self.annotations.drag_edit(local[0], local[1], viewport) {
                cx.notify();
            }
            return;
        }
        if self.annotations.is_dragging() {
            self.annotations.pointer_move(local[0], local[1]);
            cx.notify();
        }
    }

    fn on_canvas_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.trim_drag.take().is_some() {
            cx.notify();
            return;
        }
        if self.annotations.is_editing() {
            let _ = self.annotations.end_edit();
            cx.notify();
            return;
        }
        if self.annotations.is_dragging() {
            self.annotations.pointer_up();
            cx.notify();
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let stroke = &event.keystroke;
        let plain = !stroke.modifiers.control
            && !stroke.modifiers.platform
            && !stroke.modifiers.function
            && !stroke.modifiers.alt
            && !stroke.modifiers.shift;
        if !self.annotations.is_typing() {
            if plain
                && matches!(stroke.key.as_str(), "backspace" | "delete")
                && self.annotations.delete_selected()
            {
                self.style_open = false;
                cx.notify();
                cx.stop_propagation();
            }
            return;
        }
        if stroke.modifiers.control || stroke.modifiers.platform || stroke.modifiers.function {
            return;
        }
        let handled = match stroke.key.as_str() {
            "backspace" | "delete" => {
                self.annotations.backspace();
                true
            }
            "enter" => {
                self.annotations.commit();
                true
            }
            "escape" => {
                self.annotations.cancel_draft();
                true
            }
            _ => {
                if let Some(text) = stroke.key_char.as_deref()
                    && !text.is_empty()
                    && text.chars().all(|c| !c.is_control())
                {
                    self.annotations.insert_text(text);
                    true
                } else {
                    false
                }
            }
        };
        if handled {
            cx.notify();
            cx.stop_propagation();
        }
    }

    fn tool_button(
        &self,
        id: &'static str,
        icon: LucideIcon,
        label: String,
        tool: Tool,
        current: Tool,
        cx: &mut Context<Self>,
    ) -> Button {
        Button::new(id)
            .ghost()
            .small()
            .icon(icon)
            .tooltip(label.clone())
            .accessibility_label(label)
            .selected(current == tool)
            .on_click(cx.listener(move |this, _, _, cx| {
                let same = this.annotations.tool() == tool && this.annotations.selected().is_none();
                this.annotations.deselect();
                this.annotations.set_tool(tool);
                this.style_open = if same { !this.style_open } else { true };
                cx.notify();
            }))
    }

    fn render_style_strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let active = self.annotations.active_tool();
        let style = self.annotations.style();
        let picked = self.annotations.selected().is_some();
        let slider_ix = active.style_index();
        let uses_color = active.uses_color();
        let size_label = t!("overlay.size_value", value = style.size.round() as u32).to_string();

        let swatches = annotate::COLORS.into_iter().map(|color| {
            let selected = (style.color & 0x00FF_FFFF) == color;
            div()
                .id(SharedString::from(format!("editor-color-{color}")))
                .size(px(16.))
                .rounded(px(4.))
                .bg(rgb(color))
                .when(selected, |this| this.border_2().border_color(theme.primary))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.annotations.set_color(color);
                    this.persist_defaults(cx);
                    cx.notify();
                }))
        });

        let picker = ColorPicker::new(&self.color_picker)
            .with_size(px(16.))
            .accessibility_label(t!("overlay.style").to_string())
            .featured_colors(
                annotate::COLORS
                    .into_iter()
                    .map(|color| Hsla::from(rgb(color)))
                    .collect(),
            );

        h_flex()
            .flex_none()
            .px_3()
            .py_1p5()
            .gap_3()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.group_box)
            .when(uses_color, |this| {
                this.child(
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .children(swatches)
                        .child(picker),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(t!("overlay.size").to_string()),
                    )
                    .child(Slider::new(&self.size_sliders[slider_ix]).w(px(140.)))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(size_label),
                    ),
            )
            .when(picked, |this| {
                this.child(
                    Button::new("editor-delete-annotation")
                        .ghost()
                        .small()
                        .icon(LucideIcon::Trash)
                        .tooltip(t!("overlay.delete").to_string())
                        .accessibility_label(t!("overlay.delete").to_string())
                        .on_click(cx.listener(|this, _, _, cx| {
                            if this.annotations.delete_selected() {
                                this.style_open = false;
                                cx.notify();
                            }
                        })),
                )
            })
    }
}

impl EntityInputHandler for ScrollEditorView {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let (text, range) = self.annotations.text_for_range(range_utf16)?;
        *actual_range = Some(range);
        Some(text)
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let end = self.annotations.text_length_utf16()?;
        Some(UTF16Selection {
            range: end..end,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.annotations.marked_text_range()
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.annotations.unmark_text();
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
        self.annotations.replace_text(range_utf16, text);
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
        self.annotations
            .replace_and_mark_text(range_utf16, new_text);
        window.notify_text_input_changed(&self.focus_handle);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let bounds = self.canvas_bounds.get()?;
        let at = self.annotations.draft_origin()?;
        let font_size = self
            .annotations
            .draft_style()
            .map_or(16.0, |style| style.size)
            * self.zoom;
        Some(Bounds::new(
            point(
                bounds.origin.x + px(at[0] * self.zoom),
                bounds.origin.y + px(at[1] * self.zoom),
            ),
            size(px(1.), px(font_size)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

impl Render for ScrollEditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_style_controls(window, cx);
        let theme = cx.theme().clone();
        let current = self.annotations.tool();
        let can_undo = self.annotations.can_undo();
        let trimmed = self.is_trimmed();
        let show_trim = self.trim_mode || trimmed;

        let (img_w, img_h) = self.image.dimensions();
        let logical_w = img_w as f32 / self.scale;
        let logical_h = img_h as f32 / self.scale;
        let disp_w = (logical_w * self.zoom).max(1.0);
        let disp_h = (logical_h * self.zoom).max(1.0);

        let trim_top_disp = (self.trim_top as f32 / self.scale) * self.zoom;
        let trim_bottom_disp = (self.trim_bottom as f32 / self.scale) * self.zoom;
        let cropped_h = self.trim_bottom.saturating_sub(self.trim_top);
        let size_badge = t!("scroll_editor.size", width = img_w, height = cropped_h).to_string();
        let zoom_pct = format!("{}%", (self.zoom * 100.0).round() as u32);

        let image = self.image.clone();
        let layer = self.annotations.clone();
        let scale = self.scale;
        let zoom = self.zoom;
        let mosaics = self.mosaics.clone();
        let canvas_bounds_slot = self.canvas_bounds.clone();
        let ime_focus = self.focus_handle.clone();
        let view_entity = cx.entity();

        let fit_w_zoom = {
            let vw = (window.viewport_size().width.as_f32() - 64.0).max(200.0);
            (vw / logical_w).clamp(0.15, 1.0)
        };

        let mask_shade = gpui_kit::hsla(0., 0., 0., 0.55);

        v_flex()
            .id("scroll-editor")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_action(cx.listener(|this, _: &CopyResult, _, cx| this.copy_result(cx)))
            .on_action(cx.listener(|this, _: &SaveResult, _, cx| this.save_result(cx)))
            .on_action(cx.listener(|this, _: &UndoEdit, _, cx| {
                this.annotations.undo();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CancelEditor, window, cx| {
                if this.annotations.is_typing() {
                    this.annotations.cancel_draft();
                    cx.notify();
                } else {
                    window.remove_window();
                }
            }))
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| {
                this.zoom = (this.zoom * 1.25).min(3.0);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| {
                this.zoom = (this.zoom / 1.25).max(0.15);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ActualSize, _, cx| {
                this.zoom = 1.0;
                cx.notify();
            }))
            .on_action(cx.listener(move |this, _: &FitWidth, _, cx| {
                this.zoom = fit_w_zoom;
                cx.notify();
            }))
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_move(cx.listener(Self::on_canvas_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_canvas_mouse_up))
            .child(
                h_flex()
                    .flex_none()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .justify_between()
                    .border_b_1()
                    .border_color(theme.border)
                    .bg(theme.popover)
                    .child(
                        Toolbar::new("scroll-editor-tools")
                            .child(self.tool_button(
                                "ed-rect",
                                LucideIcon::Square,
                                t!("overlay.tool.rect").to_string(),
                                Tool::Rect,
                                current,
                                cx,
                            ))
                            .child(self.tool_button(
                                "ed-arrow",
                                LucideIcon::ArrowUpRight,
                                t!("overlay.tool.arrow").to_string(),
                                Tool::Arrow,
                                current,
                                cx,
                            ))
                            .child(self.tool_button(
                                "ed-line",
                                LucideIcon::Minus,
                                t!("overlay.tool.line").to_string(),
                                Tool::Line,
                                current,
                                cx,
                            ))
                            .child(self.tool_button(
                                "ed-pen",
                                LucideIcon::Pencil,
                                t!("overlay.tool.pen").to_string(),
                                Tool::Pen,
                                current,
                                cx,
                            ))
                            .child(self.tool_button(
                                "ed-mosaic",
                                LucideIcon::Grid3x3,
                                t!("overlay.tool.mosaic").to_string(),
                                Tool::Mosaic,
                                current,
                                cx,
                            ))
                            .child(self.tool_button(
                                "ed-text",
                                LucideIcon::Type,
                                t!("overlay.tool.text").to_string(),
                                Tool::Text,
                                current,
                                cx,
                            ))
                            .child(self.tool_button(
                                "ed-step",
                                LucideIcon::ListOrdered,
                                t!("overlay.tool.step").to_string(),
                                Tool::Step,
                                current,
                                cx,
                            ))
                            .content(div().w(px(1.)).h(px(16.)).bg(theme.border).mx_1())
                            .child(
                                Button::new("ed-undo")
                                    .ghost()
                                    .small()
                                    .icon(LucideIcon::Undo2)
                                    .tooltip(t!("overlay.undo").to_string())
                                    .accessibility_label(t!("overlay.undo").to_string())
                                    .disabled(!can_undo)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.annotations.undo();
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("ed-clear")
                                    .ghost()
                                    .small()
                                    .icon(LucideIcon::Eraser)
                                    .tooltip(t!("overlay.clear").to_string())
                                    .accessibility_label(t!("overlay.clear").to_string())
                                    .disabled(!can_undo)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.annotations.clear();
                                        cx.notify();
                                    })),
                            )
                            .content(div().w(px(1.)).h(px(16.)).bg(theme.border).mx_1())
                            .child(
                                Button::new("ed-trim")
                                    .ghost()
                                    .small()
                                    .selected(self.trim_mode)
                                    .icon(LucideIcon::Scissors)
                                    .tooltip(t!("scroll_editor.trim_hint").to_string())
                                    .accessibility_label(t!("scroll_editor.trim").to_string())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.trim_mode = !this.trim_mode;
                                        cx.notify();
                                    })),
                            )
                            .when(trimmed, |this| {
                                this.child(
                                    Button::new("ed-trim-reset")
                                        .ghost()
                                        .small()
                                        .icon(LucideIcon::RotateCcw)
                                        .tooltip(t!("scroll_editor.trim_reset").to_string())
                                        .accessibility_label(
                                            t!("scroll_editor.trim_reset").to_string(),
                                        )
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.trim_top = 0;
                                            this.trim_bottom = this.image.height();
                                            cx.notify();
                                        })),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .mr_1()
                                    .child(size_badge),
                            )
                            .child(
                                Button::new("ed-zoom-out")
                                    .ghost()
                                    .small()
                                    .icon(LucideIcon::ZoomOut)
                                    .tooltip(t!("pin.zoom_out").to_string())
                                    .accessibility_label(t!("pin.zoom_out").to_string())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.zoom = (this.zoom / 1.25).max(0.15);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("ed-zoom-fit")
                                    .ghost()
                                    .small()
                                    .label(zoom_pct)
                                    .tooltip(t!("scroll_editor.fit_width").to_string())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.zoom = if (this.zoom - fit_w_zoom).abs() < 0.02 {
                                            1.0
                                        } else {
                                            fit_w_zoom
                                        };
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("ed-zoom-in")
                                    .ghost()
                                    .small()
                                    .icon(LucideIcon::ZoomIn)
                                    .tooltip(t!("pin.zoom_in").to_string())
                                    .accessibility_label(t!("pin.zoom_in").to_string())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.zoom = (this.zoom * 1.25).min(3.0);
                                        cx.notify();
                                    })),
                            )
                            .child(div().w(px(1.)).h(px(16.)).bg(theme.border).mx_1())
                            .child(
                                Button::new("ed-ocr")
                                    .ghost()
                                    .small()
                                    .icon(LucideIcon::ScanText)
                                    .tooltip(t!("overlay.ocr").to_string())
                                    .accessibility_label(t!("overlay.ocr").to_string())
                                    .on_click(cx.listener(|this, _, _, cx| this.ocr_result(cx))),
                            )
                            .child(
                                Button::new("ed-pin")
                                    .ghost()
                                    .small()
                                    .icon(LucideIcon::Pin)
                                    .tooltip(t!("overlay.pin").to_string())
                                    .accessibility_label(t!("overlay.pin").to_string())
                                    .on_click(cx.listener(|this, _, _, cx| this.pin_result(cx))),
                            )
                            .child(
                                Button::new("ed-save")
                                    .ghost()
                                    .small()
                                    .icon(LucideIcon::Save)
                                    .label(t!("overlay.save").to_string())
                                    .on_click(cx.listener(|this, _, _, cx| this.save_result(cx))),
                            )
                            .child(
                                Button::new("ed-copy")
                                    .primary()
                                    .small()
                                    .icon(IconName::Copy)
                                    .label(t!("overlay.copy").to_string())
                                    .on_click(cx.listener(|this, _, _, cx| this.copy_result(cx))),
                            ),
                    ),
            )
            .when(
                self.style_open || self.annotations.selected().is_some(),
                |this| this.child(self.render_style_strip(cx)),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(
                        div()
                            .id("scroll-editor-viewport")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll_handle)
                            .p_6()
                            .child(
                                div().w_full().flex().justify_center().child(
                                    div()
                                        .id("scroll-editor-canvas")
                                        .relative()
                                        .w(px(disp_w))
                                        .h(px(disp_h))
                                        .shadow_md()
                                        .border_1()
                                        .border_color(theme.border)
                                        .cursor(if self.trim_drag.is_some() {
                                            CursorStyle::ResizeUpDown
                                        } else {
                                            CursorStyle::Crosshair
                                        })
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(Self::on_canvas_mouse_down),
                                        )
                                        .child(v_flex().size_full().children(
                                            self.tiles.iter().map(|tile| {
                                                let tile_h = (tile.height as f32 / scale) * zoom;
                                                img(tile.render.clone())
                                                    .id(SharedString::from(format!(
                                                        "ed-tile-{}",
                                                        tile.id
                                                    )))
                                                    .w(px(disp_w))
                                                    .h(px(tile_h))
                                                    .object_fit(ObjectFit::Fill)
                                            }),
                                        ))
                                        .child(
                                            div().absolute().inset_0().overflow_hidden().child(
                                                canvas(
                                                    move |bounds, _, _| {
                                                        canvas_bounds_slot.set(Some(bounds));
                                                    },
                                                    move |bounds, _, window, cx| {
                                                        let mut guard = mosaics.lock().ok();
                                                        let mut fallback =
                                                            annotate::MosaicCache::default();
                                                        let cache = guard
                                                            .as_deref_mut()
                                                            .unwrap_or(&mut fallback);
                                                        let source = layer
                                                            .needs_source()
                                                            .then_some(annotate::MosaicSource {
                                                                image: &image,
                                                                stamp: 1,
                                                            });
                                                        annotate::paint_layer_zoomed(
                                                            &layer,
                                                            bounds.origin,
                                                            scale,
                                                            zoom,
                                                            source,
                                                            cache,
                                                            window,
                                                            cx,
                                                        );
                                                        if layer.is_typing() {
                                                            window.handle_input(
                                                                &ime_focus,
                                                                ElementInputHandler::new(
                                                                    bounds,
                                                                    view_entity.clone(),
                                                                ),
                                                                cx,
                                                            );
                                                        }
                                                    },
                                                )
                                                .size_full(),
                                            ),
                                        )
                                        .when(show_trim, |this| {
                                            this.when(trim_top_disp > 0.5, |this| {
                                                this.child(
                                                    div()
                                                        .absolute()
                                                        .top_0()
                                                        .left_0()
                                                        .right_0()
                                                        .h(px(trim_top_disp))
                                                        .bg(mask_shade),
                                                )
                                            })
                                            .when(disp_h - trim_bottom_disp > 0.5, |this| {
                                                this.child(
                                                    div()
                                                        .absolute()
                                                        .top(px(trim_bottom_disp))
                                                        .left_0()
                                                        .right_0()
                                                        .h(px((disp_h - trim_bottom_disp).max(0.0)))
                                                        .bg(mask_shade),
                                                )
                                            })
                                            .child(
                                                div()
                                                    .id("trim-handle-top")
                                                    .absolute()
                                                    .left_0()
                                                    .right_0()
                                                    .top(px((trim_top_disp - 8.0).max(0.0)))
                                                    .h(px(16.))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .cursor(CursorStyle::ResizeUpDown)
                                                    .on_mouse_down(
                                                        MouseButton::Left,
                                                        cx.listener(|this, _, _, cx| {
                                                            this.trim_drag = Some(TrimEdge::Top);
                                                            cx.stop_propagation();
                                                        }),
                                                    )
                                                    .child(
                                                        div().w_full().h(px(2.)).bg(theme.primary),
                                                    )
                                                    .child(
                                                        div()
                                                            .absolute()
                                                            .px_2()
                                                            .py_0p5()
                                                            .rounded_full()
                                                            .bg(theme.primary)
                                                            .text_color(theme.primary_foreground)
                                                            .text_xs()
                                                            .child(format!("{} px", self.trim_top)),
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .id("trim-handle-bottom")
                                                    .absolute()
                                                    .left_0()
                                                    .right_0()
                                                    .top(px((trim_bottom_disp - 8.0)
                                                        .clamp(0.0, (disp_h - 16.0).max(0.0))))
                                                    .h(px(16.))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .cursor(CursorStyle::ResizeUpDown)
                                                    .on_mouse_down(
                                                        MouseButton::Left,
                                                        cx.listener(|this, _, _, cx| {
                                                            this.trim_drag = Some(TrimEdge::Bottom);
                                                            cx.stop_propagation();
                                                        }),
                                                    )
                                                    .child(
                                                        div().w_full().h(px(2.)).bg(theme.primary),
                                                    )
                                                    .child(
                                                        div()
                                                            .absolute()
                                                            .px_2()
                                                            .py_0p5()
                                                            .rounded_full()
                                                            .bg(theme.primary)
                                                            .text_color(theme.primary_foreground)
                                                            .text_xs()
                                                            .child(format!(
                                                                "{} px",
                                                                self.trim_bottom
                                                            )),
                                                    ),
                                            )
                                        }),
                                ),
                            ),
                    )
                    .vertical_scrollbar(&self.scroll_handle),
            )
            .when_some(self.toast.as_ref(), |this, (msg, _)| {
                this.child(
                    div()
                        .absolute()
                        .bottom_4()
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .px_3()
                                .py_1()
                                .rounded(theme.radius)
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.popover)
                                .text_color(theme.popover_foreground)
                                .text_sm()
                                .shadow_md()
                                .child(msg.clone()),
                        ),
                )
            })
    }
}

fn color_to_rgb(color: Hsla) -> u32 {
    let rgba = Rgba::from(color);
    let channel = |value: f32| (value.clamp(0., 1.) * 255.).round() as u32;
    (channel(rgba.r) << 16) | (channel(rgba.g) << 8) | channel(rgba.b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotate::Style;

    #[test]
    fn cropping_trim_shifts_annotations_vertically() {
        let image = RgbaImage::new(200, 1000);
        let shapes = vec![Shape::Rect {
            from: [10.0, 120.0],
            to: [50.0, 200.0],
            style: Style::default(),
        }];
        let (cropped, shifted) = crop_and_shift_shapes(&image, &shapes, 2.0, 100, 800);
        assert_eq!(cropped.dimensions(), (200, 700));
        let shifted = shifted.unwrap();
        match &shifted[0] {
            Shape::Rect { from, to, .. } => {
                assert_eq!(*from, [10.0, 70.0]);
                assert_eq!(*to, [50.0, 150.0]);
            }
            _ => panic!("expected Rect"),
        }
    }
}
