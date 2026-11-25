//! Pinned captures: borderless, always-on-top image windows.

use std::{
    cell::Cell,
    collections::{HashMap, hash_map::Entry},
    fs::File,
    io::{Read as _, Seek as _, SeekFrom},
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use anyhow::{Context as _, Result, bail, ensure};
use gpui_kit::component::{
    ActiveTheme as _, native_menu::NativeMenu, scroll::ScrollableElement as _,
};
use gpui_kit::{
    App, AppContext as _, AsyncApp, Bounds, Context, DisplayId, FocusHandle,
    InteractiveElement as _, IntoElement, KeyBinding, MouseButton, MouseDownEvent, ObjectFit,
    ParentElement as _, Pixels, Render, RenderImage, ScrollEvent, ScrollHandle, SharedString, Size,
    StatefulInteractiveElement as _, Styled as _, StyledImage as _, Task, WeakEntity, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions, actions, canvas, div, img,
    prelude::FluentBuilder as _, px, size,
};

use snapit_capture::{DiskImage, ImageSource};

use crate::controller::{Captured, Controller};
use crate::export::{ExportComposition, ExportResult};
use crate::session;

const CONTEXT: &str = "SnapItPin";
const ZOOM_RANGE: (f32, f32) = (0.1, 5.0);
const MIN_OPACITY: f32 = 0.2;
/// Pins opened away from their selection start at most this share of the display.
const MAX_INITIAL_SHARE: f32 = 0.6;
const MIN_SIDE: f32 = 24.;
const TILE_OVERSCAN: usize = 1;
const TILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_TILE_ROWS: u32 = 2048;
const MAX_TILE_ATTEMPTS: u8 = 3;

#[derive(Clone, Copy)]
struct PinTile {
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

fn tile_rows(width: u32) -> Result<u32> {
    ensure!(width > 0, "pin tile width is zero");
    let row_bytes = (width as usize)
        .checked_mul(4)
        .context("pin tile row is too large")?;
    ensure!(
        row_bytes <= TILE_BYTES,
        "pin tile row is {:.2} MiB, exceeding the {:.0} MiB tile budget",
        row_bytes as f64 / (1024.0 * 1024.0),
        TILE_BYTES as f64 / (1024.0 * 1024.0)
    );
    Ok((TILE_BYTES / row_bytes.max(1)).clamp(1, MAX_TILE_ROWS as usize) as u32)
}

fn describe_pin_tiles(width: u32, height: u32) -> Result<Vec<PinTile>> {
    if width == 0 || height == 0 {
        return Ok(Vec::new());
    }
    let rows = tile_rows(width)?;
    let count = height.div_ceil(rows) as usize;
    Ok((0..count)
        .map(|id| {
            let y = id as u32 * rows;
            PinTile {
                id,
                y,
                height: rows.min(height - y),
            }
        })
        .collect())
}

fn build_pin_tile(image: &ImageSource, tile: PinTile) -> Result<Arc<RenderImage>> {
    let rows = image.read_rows(tile.y, tile.height)?;
    Ok(session::render_image(&rows))
}

fn visible_tile_range(
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
    first.saturating_sub(overscan)..end.saturating_add(overscan).min(count)
}

fn retain_visible_tiles(cache: &mut HashMap<usize, Arc<RenderImage>>, visible: &Range<usize>) {
    cache.retain(|tile_id, _| visible.contains(tile_id));
}

actions!(
    snapit_pin,
    [
        ClosePin, CopyPin, SavePin, ZoomIn, ZoomOut, ResetZoom, Opacity100, Opacity80, Opacity60,
        Opacity40
    ]
);

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", ClosePin, Some(CONTEXT)),
        KeyBinding::new(crate::shortcut!("w"), ClosePin, Some(CONTEXT)),
        KeyBinding::new(crate::shortcut!("c"), CopyPin, Some(CONTEXT)),
        KeyBinding::new(crate::shortcut!("s"), SavePin, Some(CONTEXT)),
        KeyBinding::new(crate::shortcut!("="), ZoomIn, Some(CONTEXT)),
        KeyBinding::new(crate::shortcut!("-"), ZoomOut, Some(CONTEXT)),
        KeyBinding::new(crate::shortcut!("0"), ResetZoom, Some(CONTEXT)),
    ]);
}

/// Where a pin appears: over the selection it was made from, or centered.
pub struct Placement {
    pub display_id: Option<DisplayId>,
    pub bounds: Bounds<Pixels>,
}

pub(crate) fn image_source(
    composition: &ExportComposition,
) -> std::result::Result<Arc<ImageSource>, String> {
    let result = composition.result()?;
    match result.as_ref() {
        ExportResult::Sdr(image) => Ok(image.clone()),
        // HDR pins still use the existing full SDR preview until HDR pin
        // migration is implemented.
        ExportResult::Hdr(_) => Ok(Arc::new(ImageSource::Memory(composition.sdr_preview()?))),
    }
}

pub fn open(
    captured: Captured,
    image: Arc<ImageSource>,
    placement: Option<Placement>,
    cx: &mut App,
) {
    log::info!(
        "pin image: {}x{} source={} scale={:.2}",
        image.width(),
        image.height(),
        if image.is_disk() { "disk" } else { "memory" },
        captured.scale
    );
    let natural = size(
        px(image.width() as f32 / captured.scale),
        px(image.height() as f32 / captured.scale),
    );
    let display_size = display_id_size(placement.as_ref().and_then(|p| p.display_id), cx);
    let is_tall =
        natural.height > display_size.height * 0.8 && natural.height > natural.width * 1.5;
    let (display_id, bounds, zoom, scrollable) = match placement {
        Some(placement) => {
            if is_tall {
                let win_h = placement.bounds.size.height.min(display_size.height * 0.75);
                let bounds = Bounds::new(
                    placement.bounds.origin,
                    size(
                        placement.bounds.size.width.max(px(MIN_SIDE)),
                        win_h.max(px(MIN_SIDE)),
                    ),
                );
                (placement.display_id, bounds, 1.0, true)
            } else {
                (placement.display_id, placement.bounds, 1.0, false)
            }
        }
        None => {
            if is_tall {
                let zoom = ((display_size.width * 0.55) / natural.width).clamp(0.25, 1.0);
                let win_w = (natural.width * zoom).max(px(MIN_SIDE));
                let win_h = (natural.height * zoom)
                    .min(display_size.height * 0.75)
                    .max(px(MIN_SIDE));
                (
                    None,
                    Bounds::centered(None, size(win_w, win_h), cx),
                    zoom,
                    true,
                )
            } else {
                let zoom = ((display_size.width / natural.width)
                    .min(display_size.height / natural.height)
                    * MAX_INITIAL_SHARE)
                    .min(1.0);
                let size = scaled(natural, zoom);
                (None, Bounds::centered(None, size, cx), zoom, false)
            }
        }
    };
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
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
    if let Err(err) = gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| PinView::new(captured, image, natural, zoom, scrollable, window, cx))
    }) {
        log::error!("failed to open pin window: {err:#}");
    }
}

fn display_id_size(display_id: Option<DisplayId>, cx: &App) -> Size<Pixels> {
    display_id
        .and_then(|id| cx.displays().into_iter().find(|d| d.id() == id))
        .or_else(|| cx.primary_display())
        .map(|d| d.bounds().size)
        .unwrap_or_else(|| size(px(1440.), px(900.)))
}

/// Pin an image file, e.g. from history. The history index carries the HDR bit,
/// while callers that only have a path retain the old metadata-based detection.
pub fn open_file(path: PathBuf, scale: f32, cx: &mut App) {
    open_file_inner(path, scale, None, cx);
}

pub(crate) fn open_file_with_hdr(path: PathBuf, scale: f32, hdr: bool, cx: &mut App) {
    open_file_inner(path, scale, Some(hdr), cx);
}

fn open_file_inner(path: PathBuf, scale: f32, hdr: Option<bool>, cx: &mut App) {
    let load = cx.background_spawn(async move {
        let context = || t!("pin.read_failed", path = path.display()).to_string();
        let hdr = match hdr {
            Some(hdr) => hdr,
            None => png_has_hdr_cicp(&path).with_context(context)?,
        };
        if hdr {
            let png = Arc::new(std::fs::read(&path).with_context(context)?);
            match snapit_capture::decode_hdr_png_preview(&png) {
                Ok(Some(preview)) => {
                    log::info!(
                        "pin history: HDR PNG -> SDR preview {}x{}",
                        preview.width(),
                        preview.height()
                    );
                    let preview = Arc::new(preview);
                    let captured = Captured::frozen(png, preview.clone(), scale, true);
                    let image = Arc::new(ImageSource::Memory(preview));
                    Ok((captured, image))
                }
                Ok(None) => load_disk_pin(&path, scale).with_context(context),
                Err(err) => Err(err).with_context(context),
            }
        } else {
            load_disk_pin(&path, scale).with_context(context)
        }
    });
    cx.spawn(async move |cx| {
        let loaded = load.await;
        cx.update(|cx| match loaded {
            Ok((captured, image)) => open(captured, image, None, cx),
            Err(err) => Controller::global(cx)
                .update(cx, |controller, cx| controller.fail(format!("{err:#}"), cx)),
        })
    })
    .detach();
}

fn load_disk_pin(path: &Path, scale: f32) -> Result<(Captured, Arc<ImageSource>)> {
    let disk = Arc::new(DiskImage::from_png(path)?);
    log::info!(
        "pin history: SDR PNG -> disk image {}x{}",
        disk.width(),
        disk.height()
    );
    let image = Arc::new(ImageSource::Disk(disk));
    let captured = Captured::from_parts(image.clone(), None, scale, None, Default::default());
    Ok((captured, image))
}

/// Inspect only PNG metadata so ordinary history files never need to be read
/// into memory merely to discover whether they use SnapIt's HDR cICP profile.
fn png_has_hdr_cicp(path: &Path) -> Result<bool> {
    let mut file = File::open(path)?;
    let mut signature = [0u8; 8];
    file.read_exact(&mut signature)?;
    if signature != [137, 80, 78, 71, 13, 10, 26, 10] {
        bail!("invalid PNG signature");
    }
    loop {
        let mut header = [0u8; 8];
        file.read_exact(&mut header)
            .context("PNG ended before its chunk header")?;
        let len = u32::from_be_bytes(header[..4].try_into().expect("four bytes"));
        let kind = &header[4..];
        if kind == b"cICP" {
            if len != 4 {
                return Ok(false);
            }
            let mut cicp = [0u8; 4];
            file.read_exact(&mut cicp)?;
            let mut crc = [0u8; 4];
            file.read_exact(&mut crc)?;
            return Ok(cicp == [9, 16, 0, 1]);
        }
        if kind == b"IDAT" || kind == b"IEND" {
            return Ok(false);
        }
        file.seek(SeekFrom::Current(i64::from(len) + 4))?;
    }
}

fn scaled(natural: Size<Pixels>, zoom: f32) -> Size<Pixels> {
    size(
        (natural.width * zoom).max(px(MIN_SIDE)),
        (natural.height * zoom).max(px(MIN_SIDE)),
    )
}

pub struct PinView {
    captured: Captured,
    image: Arc<ImageSource>,
    tiles: Vec<PinTile>,
    tile_range: Range<usize>,
    tile_cache: HashMap<usize, Arc<RenderImage>>,
    tile_loads: TileLoadState,
    tile_layout_error: Option<String>,
    tile_revision: u64,
    tile_task: Option<Task<()>>,
    tile_worker_active: bool,
    requested_tile_range: Rc<Cell<Option<(usize, usize)>>>,
    viewport_geometry: Rc<Cell<Option<(f32, Pixels)>>>,
    natural: Size<Pixels>,
    zoom: f32,
    opacity: f32,
    scrollable: bool,
    scroll_handle: ScrollHandle,
    focus_handle: FocusHandle,
}

impl PinView {
    fn new(
        captured: Captured,
        image: Arc<ImageSource>,
        natural: Size<Pixels>,
        zoom: f32,
        scrollable: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        let (width, height) = image.dimensions();
        let (tiles, tile_layout_error) = match describe_pin_tiles(width, height) {
            Ok(tiles) => (tiles, None),
            Err(err) => {
                let error = format!("{err:#}");
                log::error!("failed to describe pin tiles: {error}");
                (Vec::new(), Some(error))
            }
        };
        let mut view = Self {
            captured,
            image,
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
            natural,
            zoom,
            opacity: 1.0,
            scrollable,
            scroll_handle: ScrollHandle::new(),
            focus_handle,
        };
        let initial_range = visible_tile_range(
            width,
            height,
            view.captured.scale,
            zoom,
            0.0..window.viewport_size().height.as_f32(),
            TILE_OVERSCAN,
        );
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

    fn render_tile_range(&self, window: &Window) -> Range<usize> {
        let visible = if self.scrollable {
            let (visible_top, last_offset) = self
                .viewport_geometry
                .get()
                .unwrap_or((0.0, self.scroll_handle.offset().y));
            let offset_delta = self.scroll_handle.offset().y - last_offset;
            let visible_top = visible_top - offset_delta.as_f32();
            let height = self.scroll_handle.bounds().size.height.as_f32();
            let height = if height > 0.0 {
                height
            } else {
                window.viewport_size().height.as_f32()
            };
            visible_top..visible_top + height
        } else {
            0.0..window.viewport_size().height.as_f32()
        };
        visible_tile_range(
            self.image.width(),
            self.image.height(),
            self.captured.scale,
            self.zoom,
            visible,
            TILE_OVERSCAN,
        )
    }

    fn request_tile_range(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        let key = (range.start, range.end);
        if self.requested_tile_range.get() == Some(key) {
            return;
        }
        self.requested_tile_range.set(Some(key));
        let view = cx.entity().downgrade();
        cx.defer(move |cx| {
            view.update(cx, |this, cx| this.set_tile_range(range, cx))
                .ok();
        });
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
                        this.update(cx, |this, _| this.tile_worker_active = false)
                            .ok();
                        break;
                    };

                    let image = image.clone();
                    let render = cx
                        .background_executor()
                        .spawn(async move { build_pin_tile(&image, tile) })
                        .await;
                    let render = match render {
                        Ok(render) => render,
                        Err(err) => {
                            let error = format!("{err:#}");
                            log::warn!("failed to load pin tile {}: {error}", tile.id);
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
                        this.update(cx, |this, _| this.tile_worker_active = false)
                            .ok();
                        break;
                    }
                }
            }),
        );
    }

    fn set_zoom(&mut self, zoom: f32, window: &mut Window, cx: &mut Context<Self>) {
        let zoom = zoom.clamp(ZOOM_RANGE.0, ZOOM_RANGE.1);
        if (zoom - self.zoom).abs() < f32::EPSILON {
            return;
        }
        self.zoom = zoom;
        let mut next = scaled(self.natural, zoom);
        if self.scrollable {
            let max_h = window
                .display(cx)
                .map(|d| d.bounds().size.height)
                .unwrap_or_else(|| display_id_size(None, cx).height)
                * 0.85;
            next.height = next.height.min(max_h).max(px(MIN_SIDE));
        }
        window.resize(next);
        cx.notify();
    }

    fn set_opacity(&mut self, opacity: f32, cx: &mut Context<Self>) {
        self.opacity = opacity.clamp(MIN_OPACITY, 1.0);
        cx.notify();
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window, cx);
        if event.click_count >= 2 {
            window.remove_window();
        } else {
            window.start_window_move();
        }
    }

    fn on_right_click(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window, cx);
        let opacity = |percent: u8| (self.opacity * 100.).round() as u8 == percent;
        NativeMenu::new()
            .menu(t!("pin.copy").to_string(), Box::new(CopyPin))
            .menu(t!("pin.save").to_string(), Box::new(SavePin))
            .separator()
            .submenu(
                t!("pin.opacity").to_string(),
                NativeMenu::new()
                    .menu_with_check("100%", opacity(100), Box::new(Opacity100))
                    .menu_with_check("80%", opacity(80), Box::new(Opacity80))
                    .menu_with_check("60%", opacity(60), Box::new(Opacity60))
                    .menu_with_check("40%", opacity(40), Box::new(Opacity40)),
            )
            .submenu(
                t!("pin.zoom").to_string(),
                NativeMenu::new()
                    .menu(t!("pin.zoom_in").to_string(), Box::new(ZoomIn))
                    .menu(t!("pin.zoom_out").to_string(), Box::new(ZoomOut))
                    .menu_with_check(
                        t!("pin.reset_zoom").to_string(),
                        (self.zoom - 1.0).abs() < 0.01,
                        Box::new(ResetZoom),
                    ),
            )
            .separator()
            .menu(t!("pin.close").to_string(), Box::new(ClosePin))
            .show(event.position, window, cx);
    }

    /// Scroll zooms; for tall scrollable pins, plain scroll moves through the
    /// image while Cmd/Ctrl+scroll zooms and Alt+scroll changes opacity.
    fn on_scroll(&mut self, event: &ScrollEvent, window: &mut Window, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(window.line_height()).y.as_f32();
        if delta == 0. {
            return;
        }
        if event.modifiers.alt {
            self.set_opacity(self.opacity + delta * 0.002, cx);
            cx.stop_propagation();
        } else if !self.scrollable || event.modifiers.platform || event.modifiers.control {
            self.set_zoom(self.zoom * (delta * 0.005).exp(), window, cx);
            cx.stop_propagation();
        }
    }

    fn copy(&mut self, _: &CopyPin, _: &mut Window, cx: &mut Context<Self>) {
        let captured = self.captured.clone();
        Controller::global(cx).update(cx, |controller, cx| controller.copy_image(captured, cx));
    }

    fn save(&mut self, _: &SavePin, _: &mut Window, cx: &mut Context<Self>) {
        let captured = self.captured.clone();
        Controller::global(cx).update(cx, |controller, cx| controller.save_image(captured, cx));
    }
}

impl Render for PinView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let border = theme.border;
        let background = theme.background;
        let render_range = self.render_tile_range(window);
        self.request_tile_range(render_range.clone(), cx);
        self.start_tile_worker(cx);

        let content_w = if self.scrollable {
            self.natural.width * self.zoom
        } else {
            window.viewport_size().width
        };
        let content_h = self.natural.height * self.zoom;
        let scale = self.captured.scale;
        let zoom = self.zoom;
        let tile_elements = |range: Range<usize>| {
            range
                .filter_map(|id| {
                    self.tiles.get(id).map(|tile| {
                        let tile_h = px((tile.height as f32 / scale) * zoom);
                        div()
                            .id(SharedString::from(format!("pin-tile-{id}")))
                            .absolute()
                            .left_0()
                            .top(px((tile.y as f32 / scale) * zoom))
                            .w(content_w)
                            .h(tile_h)
                            .bg(background)
                            .when_some(self.tile_cache.get(&id), |tile, render| {
                                tile.child(
                                    img(render.clone()).size_full().object_fit(ObjectFit::Fill),
                                )
                            })
                    })
                })
                .collect::<Vec<_>>()
        };

        let viewport_geometry = self.viewport_geometry.clone();
        let scroll_handle = self.scroll_handle.clone();
        let scrollable = self.scrollable;
        let retry_label = t!("permission.recheck").to_string();
        let error_elements = |range: Range<usize>| {
            range
                .filter_map(|id| {
                    let tile = self.tiles.get(id)?;
                    let error = self.tile_loads.error(id, self.tile_revision)?;
                    let message = format!("{error}\n{retry_label}");
                    Some(
                        div()
                            .id(SharedString::from(format!("pin-tile-error-{id}")))
                            .absolute()
                            .left_0()
                            .top(px((tile.y as f32 / scale) * zoom))
                            .w(content_w)
                            .h(px((tile.height as f32 / scale) * zoom))
                            .bg(background)
                            .border_1()
                            .border_color(theme.danger)
                            .overflow_hidden()
                            .cursor_pointer()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    this.retry_tile(id, cx);
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
                })
                .collect::<Vec<_>>()
        };
        let geometry = || {
            let viewport_geometry = viewport_geometry.clone();
            let scroll_handle = scroll_handle.clone();
            div().absolute().inset_0().child(canvas(
                move |bounds, _, _cx| {
                    let viewport = scroll_handle.bounds();
                    let visible_top = if scrollable {
                        (viewport.top() - bounds.top()).as_f32()
                    } else {
                        0.0
                    };
                    viewport_geometry.set(Some((visible_top, scroll_handle.offset().y)));
                },
                |_, _, _, _| {},
            ))
        };

        div()
            .id("pin")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .opacity(self.opacity)
            .cursor_move()
            .on_action(cx.listener(|_, _: &ClosePin, window, _| window.remove_window()))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(|this, _: &ZoomIn, window, cx| {
                this.set_zoom(this.zoom * 1.25, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ZoomOut, window, cx| {
                this.set_zoom(this.zoom / 1.25, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &ResetZoom, window, cx| this.set_zoom(1.0, window, cx)),
            )
            .on_action(cx.listener(|this, _: &Opacity100, _, cx| this.set_opacity(1.0, cx)))
            .on_action(cx.listener(|this, _: &Opacity80, _, cx| this.set_opacity(0.8, cx)))
            .on_action(cx.listener(|this, _: &Opacity60, _, cx| this.set_opacity(0.6, cx)))
            .on_action(cx.listener(|this, _: &Opacity40, _, cx| this.set_opacity(0.4, cx)))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_right_click))
            .on_scroll(cx.listener(Self::on_scroll))
            .when(self.scrollable, |this| {
                let content = div()
                    .id("pin-content")
                    .relative()
                    .w(content_w)
                    .h(content_h)
                    .children(tile_elements(render_range.clone()))
                    .child(geometry())
                    .children(error_elements(render_range.clone()));
                this.child(
                    div()
                        .id("pin-scroll-wrap")
                        .size_full()
                        .relative()
                        .child(
                            div()
                                .id("pin-scroll")
                                .size_full()
                                .overflow_y_scroll()
                                .track_scroll(&self.scroll_handle)
                                .child(content),
                        )
                        .vertical_scrollbar(&self.scroll_handle),
                )
            })
            .when(!self.scrollable, |this| {
                this.child(
                    div()
                        .relative()
                        .size_full()
                        .overflow_hidden()
                        .children(tile_elements(render_range.clone()))
                        .child(geometry())
                        .children(error_elements(render_range.clone())),
                )
            })
            .when_some(self.tile_layout_error.as_ref(), |this, error| {
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .bg(background)
                        .flex()
                        .items_center()
                        .justify_center()
                        .p_4()
                        .text_sm()
                        .text_color(theme.danger)
                        .child(error.clone()),
                )
            })
            .child(div().absolute().inset_0().border_1().border_color(border))
            // Windows has no programmatic window drag; give it a caption strip.
            .when(cfg!(target_os = "windows"), |this| {
                this.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(px(12.))
                        .window_control_area(gpui_kit::WindowControlArea::Drag),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_tiles_split_long_images_at_the_byte_budget() {
        for width in [1, 512, 1024, 2048, 4096] {
            let rows = tile_rows(width).unwrap();
            assert!((1..=MAX_TILE_ROWS).contains(&rows));
            assert!(rows as usize * width as usize * 4 <= TILE_BYTES);
        }

        let tiles = describe_pin_tiles(4096, 2300).unwrap();
        let heights: Vec<u32> = tiles.iter().map(|tile| tile.height).collect();
        assert_eq!(heights, vec![1024, 1024, 252]);
        assert_eq!(tiles[1].y, 1024);
        assert!(describe_pin_tiles(0, 2300).unwrap().is_empty());
        assert!(describe_pin_tiles(4096, 0).unwrap().is_empty());
    }

    #[test]
    fn ultrawide_pin_tiles_stay_within_read_and_tile_budgets() {
        for width in [65_536, 65_537, 100_000, 1_000_000, TILE_BYTES as u32 / 4] {
            let rows = tile_rows(width).unwrap();
            if width > 65_536 {
                assert!(rows < 256);
            }
            let tiles = describe_pin_tiles(width, 1025).unwrap();
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
        assert!(describe_pin_tiles(too_wide, 1).is_err());
        assert!(visible_tile_range(too_wide, 1, 1.0, 1.0, 0.0..1.0, 0).is_empty());
    }

    #[test]
    fn pin_tile_failures_are_bounded_and_revision_scoped() {
        let mut loads = TileLoadState::default();
        for attempt in 1..=MAX_TILE_ATTEMPTS {
            assert!(loads.can_attempt(4, 7));
            loads.fail(4, 7, format!("failure {attempt}"));
        }
        assert!(!loads.can_attempt(4, 7));
        assert_eq!(loads.error(4, 7), Some("failure 3"));
        assert!(loads.error(4, 8).is_none());
        assert!(loads.can_attempt(4, 8));

        loads.fail(4, 8, "new revision".to_string());
        let failure = loads.failures.get(&4).unwrap();
        assert_eq!(failure.revision, 8);
        assert_eq!(failure.attempts, 1);
        assert!(loads.retry(4));
        assert!(!loads.retry(4));
        assert!(loads.can_attempt(4, 8));

        loads.fail(4, 8, "transient".to_string());
        loads.loaded(4);
        assert!(loads.error(4, 8).is_none());
        loads.fail(9, 8, "offscreen".to_string());
        loads.retain(&(1..3));
        assert!(!loads.failures.contains_key(&9));
    }

    #[test]
    fn visible_pin_tiles_include_overscan_and_clamp_to_content() {
        assert_eq!(
            visible_tile_range(4096, 5000, 1.0, 1.0, 1024.0..2049.0, 1),
            0..4
        );
        assert_eq!(
            visible_tile_range(4096, 5000, 2.0, 0.5, -100.0..10000.0, 0),
            0..5
        );
        assert_eq!(
            visible_tile_range(4096, 5000, 1.0, 1.0, 100.0..100.0, 1),
            0..0
        );
    }

    #[test]
    fn pin_cache_drops_tiles_outside_the_visible_range() {
        let mut cache = HashMap::new();
        for id in 0..5 {
            cache.insert(
                id,
                Arc::new(RenderImage::new(vec![image::Frame::new(
                    image::RgbaImage::new(1, 1),
                )])),
            );
        }
        retain_visible_tiles(&mut cache, &(1..3));
        let mut ids: Vec<_> = cache.keys().copied().collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2]);
    }
}
