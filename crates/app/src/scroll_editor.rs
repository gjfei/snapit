//! Dedicated scrollable editor window for stitched long screenshots:
//! tile-rendered canvas (supports > 16384 px GPU height), top/bottom trim,
//! full 7-tool annotations + IME text, zoom, and direct Copy/Save/Pin/OCR.

use std::{
    cell::Cell,
    collections::{HashMap, hash_map::Entry},
    ops::Range,
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::Context as _;
use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::{
    ActiveTheme as _, IconName, Selectable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    color_picker::{ColorPickerEvent, ColorPickerState},
    h_flex,
    scroll::ScrollableElement as _,
    slider::{SliderEvent, SliderState, SliderValue},
    toolbar::Toolbar,
    v_flex,
};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, AsyncApp, Bounds, Context, CursorStyle, DisplayId,
    ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Hsla, InteractiveElement as _,
    IntoElement, KeyBinding, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ObjectFit, ParentElement as _, Pixels, Point, Render, RenderImage, ScrollHandle,
    SharedString, StatefulInteractiveElement as _, Styled as _, StyledImage as _, Subscription,
    Task, TitlebarOptions, UTF16Selection, WeakEntity, Window, WindowBounds, WindowOptions,
    actions, canvas, div, img, point, prelude::FluentBuilder as _, px, rgb, size,
};
use snapit_capture::{ImageSource, WatermarkSpec};
use snapit_store::{AnnotationTool, WatermarkTarget};

use crate::annotate::{self, Layer, Shape};
use crate::appearance;
use crate::capture_toolbar;
use crate::controller::{Captured, Controller};
use crate::session;
use crate::settings::AppSettings;

const CONTEXT: &str = "ScrollEditor";
const TILE_OVERSCAN: usize = 1;
const TILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_TILE_ROWS: u32 = 2048;
const MAX_TILE_ATTEMPTS: u8 = 3;
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
    image: ImageSource,
    scale: f32,
    display_id: Option<DisplayId>,
    shapes: Arc<Vec<Shape>>,
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
        cx.new(|cx| ScrollEditorView::new(image, scale, shapes, window, cx))
    })?
    .0;
    Ok(handle)
}

#[derive(Clone, Copy)]
struct EditorTile {
    id: usize,
    y: u32,
    height: u32,
}

#[derive(Debug, PartialEq, Eq)]
struct TileFailure {
    revision: u64,
    error: String,
    attempts: u8,
}

#[derive(Default)]
struct TileLoadState {
    failures: HashMap<usize, TileFailure>,
}

impl TileLoadState {
    fn can_attempt(&self, tile_id: usize, revision: u64) -> bool {
        self.failures.get(&tile_id).is_none_or(|failure| {
            failure.revision != revision || failure.attempts < MAX_TILE_ATTEMPTS
        })
    }

    fn fail(&mut self, tile_id: usize, revision: u64, error: String) {
        let failure = self.failures.entry(tile_id).or_insert_with(|| TileFailure {
            revision,
            error: String::new(),
            attempts: 0,
        });
        if failure.revision == revision {
            failure.attempts = failure.attempts.saturating_add(1);
        } else {
            failure.revision = revision;
            failure.attempts = 1;
        }
        failure.error = error;
    }

    fn loaded(&mut self, tile_id: usize) {
        self.failures.remove(&tile_id);
    }

    fn retry(&mut self, tile_id: usize) -> bool {
        self.failures.remove(&tile_id).is_some()
    }

    fn retain(&mut self, visible: &Range<usize>) {
        self.failures.retain(|tile_id, _| visible.contains(tile_id));
    }

    fn error(&self, tile_id: usize, revision: u64) -> Option<&str> {
        self.failures
            .get(&tile_id)
            .filter(|failure| failure.revision == revision && failure.attempts >= MAX_TILE_ATTEMPTS)
            .map(|failure| failure.error.as_str())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TrimEdge {
    Top,
    Bottom,
}

fn tile_rows(width: u32) -> anyhow::Result<u32> {
    anyhow::ensure!(width > 0, "editor tile width is zero");
    let row_bytes = (width as usize)
        .checked_mul(4)
        .context("editor tile row is too large")?;
    anyhow::ensure!(
        row_bytes <= TILE_BYTES,
        "editor tile row is {:.2} MiB, exceeding the {:.0} MiB tile budget",
        row_bytes as f64 / (1024.0 * 1024.0),
        TILE_BYTES as f64 / (1024.0 * 1024.0)
    );
    Ok((TILE_BYTES / row_bytes).clamp(1, MAX_TILE_ROWS as usize) as u32)
}

fn describe_tiles(width: u32, height: u32) -> anyhow::Result<Vec<EditorTile>> {
    if width == 0 || height == 0 {
        return Ok(Vec::new());
    }
    let tile_rows = tile_rows(width)?;
    let count = height.div_ceil(tile_rows) as usize;
    Ok((0..count)
        .map(|ix| {
            let y = ix as u32 * tile_rows;
            EditorTile {
                id: ix,
                y,
                height: tile_rows.min(height - y),
            }
        })
        .collect())
}

fn build_tile(image: &ImageSource, tile: EditorTile) -> anyhow::Result<Arc<RenderImage>> {
    let slice = image.read_rows(tile.y, tile.height)?;
    Ok(session::render_image(&slice))
}

fn visible_tile_range(
    // `visible` is a half-open range in canvas display coordinates.
    image_width: u32,
    image_height: u32,
    scale: f32,
    zoom: f32,
    visible: Range<f32>,
    overscan: usize,
) -> Range<usize> {
    if image_width == 0
        || image_height == 0
        || !scale.is_finite()
        || scale <= 0.0
        || !zoom.is_finite()
        || zoom <= 0.0
    {
        return 0..0;
    }
    let display_height = image_height as f32 / scale * zoom;
    let start = visible.start.clamp(0.0, display_height) * scale / zoom;
    let end = visible.end.clamp(0.0, display_height) * scale / zoom;
    if !start.is_finite() || !end.is_finite() || end <= start {
        return 0..0;
    }

    let rows = match tile_rows(image_width) {
        Ok(rows) => rows as usize,
        Err(_) => return 0..0,
    };
    let count = image_height.div_ceil(rows as u32) as usize;
    let first = (start.floor() as usize / rows).min(count);
    let end = (end.ceil() as usize).div_ceil(rows).min(count);
    first.saturating_sub(overscan)..(end.saturating_add(overscan)).min(count)
}

fn retain_visible_tiles<T>(cache: &mut HashMap<usize, T>, visible: &Range<usize>) {
    cache.retain(|tile_id, _| visible.contains(tile_id));
}

/// Crop `image` vertically to `[trim_top .. trim_bottom]` and shift annotation
/// shapes into the cropped coordinate space.
fn crop_and_shift_shapes(
    image: &ImageSource,
    shapes: &[Shape],
    scale: f32,
    trim_top: u32,
    trim_bottom: u32,
) -> anyhow::Result<(ImageSource, Option<Arc<Vec<Shape>>>)> {
    let (width, height) = image.dimensions();
    let top = trim_top.min(height.saturating_sub(1));
    let bottom = trim_bottom.clamp(top + 1, height);
    let cropped = match image {
        ImageSource::Memory(_) if top == 0 && bottom == height => image.clone(),
        ImageSource::Memory(image) => ImageSource::Memory(Arc::new(
            image::imageops::crop_imm(image.as_ref(), 0, top, width, bottom - top).to_image(),
        )),
        ImageSource::Disk(image) => ImageSource::Disk(Arc::new(image.crop_vertical(top, bottom)?)),
    };
    let shifted = if shapes.is_empty() {
        None
    } else {
        let dy = -(top as f32 / scale.max(0.1));
        Some(Arc::new(annotate::shift_shapes_vertical(shapes, dy)))
    };
    Ok((cropped, shifted))
}

pub struct ScrollEditorView {
    image: Arc<ImageSource>,
    scale: f32,
    tiles: Vec<EditorTile>,
    tile_range: Range<usize>,
    tile_cache: HashMap<usize, Arc<RenderImage>>,
    tile_loads: TileLoadState,
    tile_layout_error: Option<String>,
    tile_revision: u64,
    tile_task: Option<Task<()>>,
    tile_worker_active: bool,
    requested_tile_range: Rc<Cell<Option<(usize, usize)>>>,
    viewport_geometry: Rc<Cell<Option<(f32, Pixels)>>>,
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
    fn new(
        image: ImageSource,
        scale: f32,
        shapes: Arc<Vec<Shape>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let scale = if scale.is_finite() && scale > 0.1 {
            scale
        } else {
            1.0
        };
        let (width, height) = image.dimensions();
        let (tiles, tile_layout_error) = match describe_tiles(width, height) {
            Ok(tiles) => (tiles, None),
            Err(err) => {
                let error = format!("{err:#}");
                log::error!("failed to describe scroll editor tiles: {error}");
                (Vec::new(), Some(error))
            }
        };
        let logical_w = (width as f32 / scale).max(1.0);
        let fit_zoom = (700.0 / logical_w).clamp(0.2, 1.0);
        let defaults = AppSettings::get(cx).annotations.clone();
        let annotations = Layer::from_shapes(defaults.clone(), shapes);
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
                let rgb_color = capture_toolbar::color_to_rgb(*color);
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

        let mut view = Self {
            image: Arc::new(image),
            scale,
            tiles,
            tile_range: 0..0,
            tile_cache: HashMap::new(),
            tile_loads: TileLoadState::default(),
            tile_layout_error,
            tile_revision: 0,
            tile_task: None,
            tile_worker_active: false,
            requested_tile_range: Rc::new(Cell::new(None)),
            viewport_geometry: Rc::new(Cell::new(None)),
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
        };
        let initial_range = if view.tiles.is_empty() {
            0..0
        } else {
            visible_tile_range(
                width,
                height,
                scale,
                fit_zoom,
                0.0..window.viewport_size().height.as_f32(),
                TILE_OVERSCAN,
            )
        };
        view.set_tile_range(initial_range, cx);
        view
    }

    fn set_tile_range(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        let start = range.start.min(self.tiles.len());
        let end = range.end.clamp(start, self.tiles.len());
        let range = start..end;
        if range == self.tile_range {
            return;
        }
        self.tile_range = range;
        self.tile_revision = self.tile_revision.wrapping_add(1);
        retain_visible_tiles(&mut self.tile_cache, &self.tile_range);
        self.tile_loads.retain(&self.tile_range);
        cx.notify();
        self.start_tile_worker(cx);
    }

    fn retry_tile(&mut self, tile_id: usize, cx: &mut Context<Self>) {
        if !self.tile_range.contains(&tile_id) || !self.tile_loads.retry(tile_id) {
            return;
        }
        cx.notify();
        self.start_tile_worker(cx);
    }

    fn render_tile_range(&self, image_height: u32) -> Range<usize> {
        let Some((visible_top, last_offset)) = self.viewport_geometry.get() else {
            return self.tile_range.clone();
        };
        let offset_delta = self.scroll_handle.offset().y - last_offset;
        // Prepaint bounds already include the scroll offset. Predict its next
        // position so a fast scroll gets placeholders before the deferred update.
        let visible_top = visible_top - offset_delta.as_f32();
        visible_tile_range(
            self.image.width(),
            image_height,
            self.scale,
            self.zoom,
            visible_top..visible_top + self.scroll_handle.bounds().size.height.as_f32(),
            TILE_OVERSCAN,
        )
    }

    fn start_tile_worker(&mut self, cx: &mut Context<Self>) {
        if self.tile_worker_active
            || !self.tiles[self.tile_range.clone()].iter().any(|tile| {
                self.tile_loads.can_attempt(tile.id, self.tile_revision)
                    && !self.tile_cache.contains_key(&tile.id)
            })
        {
            return;
        }

        self.tile_worker_active = true;
        let image = self.image.clone();
        self.tile_task = Some(
            cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
                loop {
                    let next = this
                        .update(cx, |this, _| {
                            let tile = this.tiles[this.tile_range.clone()].iter().copied().find(
                                |tile| {
                                    this.tile_loads.can_attempt(tile.id, this.tile_revision)
                                        && !this.tile_cache.contains_key(&tile.id)
                                },
                            )?;
                            Some((this.tile_revision, tile))
                        })
                        .ok()
                        .flatten();
                    let Some((revision, tile)) = next else {
                        this.update(cx, |this, _| {
                            this.tile_worker_active = false;
                        })
                        .ok();
                        break;
                    };

                    let image = image.clone();
                    let render = cx
                        .background_executor()
                        .spawn(async move { build_tile(&image, tile) })
                        .await;
                    let render = match render {
                        Ok(render) => render,
                        Err(err) => {
                            let error = format!("{err:#}");
                            log::warn!("failed to load editor tile {}: {error}", tile.id);
                            this.update(cx, |this, cx| {
                                let visible = this.tile_range.contains(&tile.id);
                                this.tile_loads.fail(tile.id, revision, error);
                                if !visible {
                                    this.tile_loads.retain(&this.tile_range);
                                }
                                if visible && this.tile_revision == revision {
                                    cx.notify();
                                }
                            })
                            .ok();
                            continue;
                        }
                    };
                    let keep_going = this
                        .update(cx, |this, cx| {
                            if this.tile_revision != revision || !this.tile_range.contains(&tile.id)
                            {
                                // Keep one sequential worker and let it pick up
                                // the latest range instead of converting this stale tile.
                                return true;
                            }
                            if let Entry::Vacant(entry) = this.tile_cache.entry(tile.id) {
                                entry.insert(render);
                                this.tile_loads.loaded(tile.id);
                                cx.notify();
                            }
                            true
                        })
                        .unwrap_or(false);
                    if !keep_going {
                        this.update(cx, |this, _| {
                            this.tile_worker_active = false;
                        })
                        .ok();
                        break;
                    }
                }
            }),
        );
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
        let watermark = if watermark_cfg.target_enabled(WatermarkTarget::Screenshot) {
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
        )?;
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
        let render_tile_range = self.render_tile_range(img_h);
        self.start_tile_worker(cx);

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
        let requested_tile_range = self.requested_tile_range.clone();
        let viewport_geometry = self.viewport_geometry.clone();
        let scroll_handle = self.scroll_handle.clone();
        let annotation_scroll_handle = scroll_handle.clone();
        let ime_focus = self.focus_handle.clone();
        let view_entity = cx.entity();
        let tile_view = view_entity.downgrade();

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
                    .child({
                        let select_view = view_entity.clone();
                        let undo_view = view_entity.clone();
                        let clear_view = view_entity.clone();
                        capture_toolbar::append_annotation_tools(
                            Toolbar::new("scroll-editor-tools"),
                            "ed",
                            Some(current),
                            can_undo,
                            false,
                            &theme,
                            move |tool, _, _, cx| {
                                select_view.update(cx, |this, cx| {
                                    let same = this.annotations.tool() == tool
                                        && this.annotations.selected().is_none();
                                    this.annotations.deselect();
                                    this.annotations.set_tool(tool);
                                    this.style_open = if same { !this.style_open } else { true };
                                    cx.notify();
                                });
                            },
                            move |_, _, cx| {
                                undo_view.update(cx, |this, cx| {
                                    this.annotations.undo();
                                    cx.notify();
                                });
                            },
                            move |_, _, cx| {
                                clear_view.update(cx, |this, cx| {
                                    this.annotations.clear();
                                    cx.notify();
                                });
                            },
                        )
                        .content(capture_toolbar::toolbar_divider(&theme))
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
                                    .accessibility_label(t!("scroll_editor.trim_reset").to_string())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.trim_top = 0;
                                        this.trim_bottom = this.image.height();
                                        cx.notify();
                                    })),
                            )
                        })
                    })
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
                |this| {
                    let active = self.annotations.active_tool();
                    let style = self.annotations.style();
                    let picked = self.annotations.selected().is_some();
                    let delete_view = view_entity.clone();
                    let on_delete = picked.then(|| -> capture_toolbar::DeleteHandler {
                        Box::new(move |_, _, cx| {
                            delete_view.update(cx, |this, cx| {
                                if this.annotations.delete_selected() {
                                    this.style_open = false;
                                    cx.notify();
                                }
                            });
                        })
                    });
                    this.child(
                        capture_toolbar::render_annotation_style_bar(
                            "ed-style",
                            active,
                            style.size,
                            &self.color_picker,
                            &self.size_sliders[active.style_index()],
                            on_delete,
                            &theme,
                        )
                        .flex_none()
                        .px_3()
                        .border_b_1()
                        .border_color(theme.border)
                        .bg(theme.group_box),
                    )
                },
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
                                        .child(div().absolute().inset_0().children(
                                            render_tile_range.clone().filter_map(|ix| {
                                                self.tiles.get(ix).map(|tile| {
                                                    let tile_h =
                                                        (tile.height as f32 / scale) * zoom;
                                                    div()
                                                        .id(("ed-tile", tile.id))
                                                        .absolute()
                                                        .left_0()
                                                        .top(px((tile.y as f32 / scale) * zoom))
                                                        .w(px(disp_w))
                                                        .h(px(tile_h))
                                                        .bg(theme.background)
                                                        .when_some(
                                                            self.tile_cache.get(&tile.id),
                                                            |tile, render| {
                                                                tile.child(
                                                                    img(render.clone())
                                                                        .w_full()
                                                                        .h_full()
                                                                        .object_fit(
                                                                            ObjectFit::Fill,
                                                                        ),
                                                                )
                                                            },
                                                        )
                                                })
                                            }),
                                        ))
                                        .child(
                                            div().absolute().inset_0().overflow_hidden().child(
                                                canvas(
                                                    move |bounds, _, cx| {
                                                        canvas_bounds_slot.set(Some(bounds));
                                                        let viewport = scroll_handle.bounds();
                                                        let visible_top = (viewport.top()
                                                            - bounds.top())
                                                        .as_f32();
                                                        viewport_geometry.set(Some((
                                                            visible_top,
                                                            scroll_handle.offset().y,
                                                        )));
                                                        let range = visible_tile_range(
                                                            img_w,
                                                            img_h,
                                                            scale,
                                                            zoom,
                                                            visible_top
                                                                ..visible_top
                                                                    + viewport.size.height.as_f32(),
                                                            TILE_OVERSCAN,
                                                        );
                                                        let range_key = (range.start, range.end);
                                                        if requested_tile_range.get()
                                                            != Some(range_key)
                                                        {
                                                            requested_tile_range
                                                                .set(Some(range_key));
                                                            cx.defer(move |cx| {
                                                                tile_view
                                                                    .update(cx, |this, cx| {
                                                                        this.set_tile_range(
                                                                            range, cx,
                                                                        );
                                                                    })
                                                                    .ok();
                                                            });
                                                        }
                                                    },
                                                    move |bounds, _, window, cx| {
                                                        let viewport =
                                                            annotation_scroll_handle.bounds();
                                                        let visible_top = (viewport.top()
                                                            - bounds.top())
                                                        .as_f32();
                                                        let clip = [
                                                            0.0,
                                                            visible_top / zoom,
                                                            logical_w,
                                                            (visible_top
                                                                + viewport.size.height.as_f32())
                                                                / zoom,
                                                        ];
                                                        let mut guard = mosaics.lock().ok();
                                                        let mut fallback =
                                                            annotate::MosaicCache::default();
                                                        let cache = guard
                                                            .as_deref_mut()
                                                            .unwrap_or(&mut fallback);
                                                        let source =
                                                            layer.needs_source().then_some(
                                                                annotate::MosaicSource::source(
                                                                    &image, 1,
                                                                ),
                                                            );
                                                        if source.is_none() {
                                                            cache.clear();
                                                        }
                                                        annotate::paint_layer_zoomed_clipped(
                                                            &layer,
                                                            bounds.origin,
                                                            scale,
                                                            zoom,
                                                            clip,
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
                                        .children(render_tile_range.filter_map(|ix| {
                                            let tile = self.tiles.get(ix)?;
                                            let error =
                                                self.tile_loads.error(ix, self.tile_revision)?;
                                            let message =
                                                format!("{error}\n{}", t!("permission.recheck"));
                                            Some(
                                                div()
                                                    .id(("ed-tile-error", tile.id))
                                                    .absolute()
                                                    .left_0()
                                                    .top(px((tile.y as f32 / scale) * zoom))
                                                    .w(px(disp_w))
                                                    .h(px((tile.height as f32 / scale) * zoom))
                                                    .bg(theme.background)
                                                    .border_1()
                                                    .border_color(theme.danger)
                                                    .overflow_hidden()
                                                    .cursor_pointer()
                                                    .on_mouse_down(
                                                        MouseButton::Left,
                                                        cx.listener(move |this, _, _, cx| {
                                                            this.retry_tile(ix, cx);
                                                            cx.stop_propagation();
                                                        }),
                                                    )
                                                    .child(
                                                        div()
                                                            .size_full()
                                                            .flex()
                                                            .flex_col()
                                                            .items_center()
                                                            .justify_center()
                                                            .gap_1()
                                                            .p_2()
                                                            .text_xs()
                                                            .text_color(theme.danger)
                                                            .child(message),
                                                    ),
                                            )
                                        }))
                                        .when_some(
                                            self.tile_layout_error.as_ref(),
                                            |this, error| {
                                                this.child(
                                                    div()
                                                        .absolute()
                                                        .inset_0()
                                                        .bg(theme.background)
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .p_4()
                                                        .text_sm()
                                                        .text_color(theme.danger)
                                                        .child(error.clone()),
                                                )
                                            },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotate::Style;

    #[test]
    fn tile_descriptors_keep_source_offsets_without_pixels() {
        let tiles = describe_tiles(1024, 4113).unwrap();
        assert_eq!(tiles.len(), 3);
        assert_eq!((tiles[0].id, tiles[0].y, tiles[0].height), (0, 0, 2048));
        assert_eq!((tiles[1].id, tiles[1].y, tiles[1].height), (1, 2048, 2048));
        assert_eq!((tiles[2].id, tiles[2].y, tiles[2].height), (2, 4096, 17));
        assert!(describe_tiles(0, 4096).unwrap().is_empty());
        assert!(describe_tiles(1024, 0).unwrap().is_empty());
    }

    #[test]
    fn ultrawide_editor_tiles_stay_within_read_and_tile_budgets() {
        for width in [65_536, 65_537, 100_000, 1_000_000, TILE_BYTES as u32 / 4] {
            let rows = tile_rows(width).unwrap();
            if width > 65_536 {
                assert!(rows < 256);
            }
            let tiles = describe_tiles(width, 1025).unwrap();
            let mut next_y = 0;
            for tile in tiles {
                assert_eq!(tile.y, next_y);
                assert!(tile.height > 0);
                let bytes = tile.height as usize * width as usize * 4;
                assert!(bytes <= TILE_BYTES);
                assert!(bytes <= 64 * 1024 * 1024);
                next_y += tile.height;
            }
            assert_eq!(next_y, 1025);
        }

        let too_wide = TILE_BYTES as u32 / 4 + 1;
        let error = tile_rows(too_wide).unwrap_err();
        assert!(error.to_string().contains("tile budget"));
        assert!(describe_tiles(too_wide, 1).is_err());
        assert!(visible_tile_range(too_wide, 1, 1.0, 1.0, 0.0..1.0, 0).is_empty());
    }

    #[test]
    fn editor_tile_failures_are_bounded_and_revision_scoped() {
        let mut loads = TileLoadState::default();
        for attempt in 1..=MAX_TILE_ATTEMPTS {
            assert!(loads.can_attempt(2, 11));
            loads.fail(2, 11, format!("failure {attempt}"));
        }
        assert!(!loads.can_attempt(2, 11));
        assert_eq!(loads.error(2, 11), Some("failure 3"));
        assert!(loads.error(2, 12).is_none());
        assert!(loads.can_attempt(2, 12));

        loads.fail(2, 12, "new revision".to_string());
        let failure = loads.failures.get(&2).unwrap();
        assert_eq!(failure.revision, 12);
        assert_eq!(failure.attempts, 1);
        assert!(loads.retry(2));
        assert!(!loads.retry(2));
        assert!(loads.can_attempt(2, 12));

        loads.fail(2, 12, "transient".to_string());
        loads.loaded(2);
        assert!(loads.error(2, 12).is_none());
        loads.fail(7, 12, "offscreen".to_string());
        loads.retain(&(1..3));
        assert!(!loads.failures.contains_key(&7));
    }

    #[test]
    fn tile_range_uses_half_open_visible_boundaries() {
        assert_eq!(
            visible_tile_range(1024, 6144, 1.0, 1.0, 0.0..4096.0, 0),
            0..2
        );
        assert_eq!(
            visible_tile_range(1024, 6144, 1.0, 1.0, 2048.0..2049.0, 0),
            1..2
        );
        assert_eq!(
            visible_tile_range(1024, 6144, 1.0, 1.0, 4096.0..5000.0, 0),
            2..3
        );
    }

    #[test]
    fn tile_range_adds_bounded_overscan_and_clamps_to_image_edges() {
        assert_eq!(
            visible_tile_range(1024, 10_000, 1.0, 1.0, 4200.0..4300.0, TILE_OVERSCAN),
            1..4
        );
        assert_eq!(
            visible_tile_range(1024, 5000, 1.0, 1.0, 4900.0..6000.0, TILE_OVERSCAN),
            1..3
        );
        assert_eq!(
            visible_tile_range(1024, 10_000, 1.0, 1.0, 0.0..100.0, TILE_OVERSCAN),
            0..2
        );
    }

    #[test]
    fn tile_range_maps_zoom_and_ignores_invalid_geometry() {
        assert_eq!(
            visible_tile_range(1024, 6000, 2.0, 0.5, 0.0..1000.0, 0),
            0..2
        );
        assert_eq!(
            visible_tile_range(1024, 6000, 2.0, 0.5, 1400.0..1600.0, 0),
            2..3
        );
        for range in [
            visible_tile_range(1024, 0, 1.0, 1.0, 0.0..100.0, 1),
            visible_tile_range(1024, 6000, 0.0, 1.0, 0.0..100.0, 1),
            visible_tile_range(1024, 6000, 1.0, f32::NAN, 0.0..100.0, 1),
            visible_tile_range(1024, 6000, 1.0, 1.0, f32::NAN..100.0, 1),
            visible_tile_range(1024, 6000, 1.0, 1.0, 100.0..100.0, 1),
            visible_tile_range(1024, 6000, 1.0, 1.0, -100.0..-10.0, 1),
            visible_tile_range(1024, 6000, 1.0, 1.0, 7000.0..8000.0, 1),
        ] {
            assert!(range.is_empty());
        }
    }

    #[test]
    fn eviction_releases_tiles_outside_the_visible_range() {
        let retained = Arc::new(());
        let evicted = Arc::new(());
        let retained_weak = Arc::downgrade(&retained);
        let evicted_weak = Arc::downgrade(&evicted);
        let mut cache = HashMap::from([(1, evicted), (3, retained), (5, Arc::new(()))]);

        retain_visible_tiles(&mut cache, &(2..4));

        assert_eq!(cache.len(), 1);
        assert!(cache.contains_key(&3));
        assert!(retained_weak.upgrade().is_some());
        assert!(evicted_weak.upgrade().is_none());
    }

    #[test]
    fn cropping_trim_shifts_annotations_vertically() {
        let image = ImageSource::from(snapit_capture::RgbaImage::new(200, 1000));
        let shapes = vec![Shape::Rect {
            from: [10.0, 120.0],
            to: [50.0, 200.0],
            style: Style::default(),
        }];
        let (cropped, shifted) = crop_and_shift_shapes(&image, &shapes, 2.0, 100, 800).unwrap();
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

    #[test]
    fn cropping_disk_trim_uses_exclusive_bottom_row() {
        let mut raw = vec![0u8; 4 * 100 * 4];
        for y in 0..100u32 {
            for x in 0..4u32 {
                let at = ((y * 4 + x) * 4) as usize;
                raw[at..at + 4].copy_from_slice(&[y as u8, 0, 0, 255]);
            }
        }
        let img = snapit_capture::RgbaImage::from_raw(4, 100, raw).unwrap();
        let disk = ImageSource::Disk(Arc::new(
            snapit_capture::DiskImage::from_image(&img).unwrap(),
        ));
        let (cropped, _) = crop_and_shift_shapes(&disk, &[], 1.0, 30, 80).unwrap();
        assert_eq!(cropped.dimensions(), (4, 50));
        if let ImageSource::Disk(cropped_disk) = cropped {
            let rows = cropped_disk.read_rows(0, 50).unwrap();
            assert_eq!(rows.get_pixel(0, 0).0[0], 30);
            assert_eq!(rows.get_pixel(0, 49).0[0], 79);
        } else {
            panic!("expected disk image");
        }
    }
}
