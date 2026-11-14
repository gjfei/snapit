//! A frozen-screen selection session shared by every overlay window.

use std::{ops::Range, sync::Arc, time::Instant};

use anyhow::Result;
use gpui_kit::{
    Bounds, Context, DisplayId, EventEmitter, Pixels, Point, RenderImage, Size, point, px, size,
};
use snapit_capture::{ElementRole, Region, RgbaImage, Screenshot, WindowInfo};
use snapit_recorder::RecordFormat;
use snapit_store::AnnotationConfig;

use crate::annotate::{self, Layer, Tool};

/// Selections smaller than this (in logical px) count as a click.
const MIN_SELECTION: f32 = 4.0;
/// Accessibility elements with a side below this (logical px) are not snapped
/// to; their parent in the chain is used instead.
const MIN_ELEMENT_SIZE: f32 = 16.0;
/// Accessibility queries are throttled while the pointer sweeps across
/// elements, so a fast drag does not issue an IPC round-trip per pixel.
const ELEMENT_QUERY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// One monitor's frozen frame, with a BGRA image ready for the overlay.
pub struct CapturedFrame {
    shot: Screenshot,
    image: Arc<RenderImage>,
}

impl CapturedFrame {
    pub fn new(shot: Screenshot, image: Arc<RenderImage>) -> Self {
        Self { shot, image }
    }

    pub fn image(&self) -> &Arc<RenderImage> {
        &self.image
    }
}

/// Freeze every monitor. BGRA conversion happens before overlay windows map.
pub fn capture_shots() -> Result<Vec<Screenshot>> {
    snapit_capture::capture_all()
}

/// GPUI renders BGRA, captures are RGBA.
pub fn render_image(rgba: &RgbaImage) -> Arc<RenderImage> {
    let (width, height) = rgba.dimensions();
    let src = rgba.as_raw();
    let mut dst = vec![0u8; src.len()];
    rgba_to_bgra(src, &mut dst);
    let bgra = RgbaImage::from_raw(width, height, dst)
        .expect("BGRA buffer is the same length as the RGBA source");
    Arc::new(RenderImage::new(vec![image::Frame::new(bgra)]))
}

fn rgba_to_bgra(src: &[u8], dst: &mut [u8]) {
    debug_assert_eq!(src.len(), dst.len());
    debug_assert_eq!(src.len() % 4, 0);
    // A 5K frame is ~60 MiB; walk it on a few threads so the overlay can map
    // as soon as the freeze is ready.
    const PARALLEL_AFTER: usize = 1024 * 1024;
    if src.len() < PARALLEL_AFTER {
        swap_rgba_bgra_chunk(src, dst);
        return;
    }
    let threads = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(4)
        .clamp(2, 8);
    let chunk_bytes = src.len().div_ceil(threads * 4) * 4;
    std::thread::scope(|scope| {
        let mut src_rest = src;
        let mut dst_rest = dst;
        while !src_rest.is_empty() {
            let take = chunk_bytes.min(src_rest.len());
            let (src_chunk, src_tail) = src_rest.split_at(take);
            let (dst_chunk, dst_tail) = dst_rest.split_at_mut(take);
            src_rest = src_tail;
            dst_rest = dst_tail;
            scope.spawn(move || swap_rgba_bgra_chunk(src_chunk, dst_chunk));
        }
    });
}

fn swap_rgba_bgra_chunk(src: &[u8], dst: &mut [u8]) {
    let (src_pixels, _) = src.as_chunks::<4>();
    let (dst_pixels, _) = dst.as_chunks_mut::<4>();
    for (from, to) in src_pixels.iter().zip(dst_pixels) {
        to[0] = from[2];
        to[1] = from[1];
        to[2] = from[0];
        to[3] = from[3];
    }
}

/// One overlay window, placed before the freeze so window creation overlaps
/// the capture. Its frame is filled in when the freeze lands.
#[derive(Clone, Copy)]
pub struct SessionSlot {
    display_id: Option<DisplayId>,
    /// Display bounds in the coordinate space GPUI uses for window bounds.
    bounds: Bounds<Pixels>,
}

impl SessionSlot {
    pub fn new(display_id: Option<DisplayId>, bounds: Bounds<Pixels>) -> Self {
        Self { display_id, bounds }
    }

    pub fn display_id(&self) -> Option<DisplayId> {
        self.display_id
    }

    pub fn bounds(&self) -> Bounds<Pixels> {
        self.bounds
    }

    fn matches(&self, monitor_id: u32) -> bool {
        self.display_id
            .is_some_and(|id| u64::from(id) as u32 == monitor_id)
    }
}

/// A frame placed on a GPUI display.
pub struct SessionFrame {
    frame: CapturedFrame,
    display_id: Option<DisplayId>,
    /// Display bounds in the coordinate space GPUI uses for window bounds.
    display_bounds: Bounds<Pixels>,
    /// On-screen windows at freeze time, monitor-relative, front-to-back.
    windows: Vec<WindowInfo>,
}

impl SessionFrame {
    fn new(frame: CapturedFrame, slot: &SessionSlot) -> Self {
        Self {
            windows: frame.shot.windows().to_vec(),
            frame,
            display_id: slot.display_id,
            display_bounds: slot.bounds,
        }
    }

    pub fn image(&self) -> &Arc<RenderImage> {
        self.frame.image()
    }

    pub fn display_id(&self) -> Option<DisplayId> {
        self.display_id
    }

    pub fn display_bounds(&self) -> Bounds<Pixels> {
        self.display_bounds
    }

    pub fn monitor_id(&self) -> u32 {
        self.frame.shot.monitor().id
    }

    /// Monitor units (see `snapit_capture`) per logical window pixel.
    fn units_per_px(&self) -> f32 {
        let width = self.display_bounds.size.width.as_f32();
        if width <= 0.0 {
            return 1.0;
        }
        self.frame.shot.monitor().width as f32 / width
    }

    pub fn to_region(&self, bounds: Bounds<Pixels>) -> Region {
        let factor = self.units_per_px();
        Region::new(
            bounds.origin.x.as_f32(),
            bounds.origin.y.as_f32(),
            bounds.size.width.as_f32(),
            bounds.size.height.as_f32(),
        )
        .scaled(factor)
    }

    pub fn physical_size(&self, bounds: Bounds<Pixels>) -> (u32, u32) {
        let scale = self.physical_scale();
        (
            (bounds.size.width.as_f32() * scale).round() as u32,
            (bounds.size.height.as_f32() * scale).round() as u32,
        )
    }

    /// Physical pixels per logical overlay pixel.
    pub fn physical_scale(&self) -> f32 {
        self.units_per_px() * self.frame.shot.pixels_per_unit()
    }

    /// The frontmost window under a window-local point, in logical px.
    pub fn window_at(&self, position: Point<Pixels>) -> Option<(WindowInfo, Bounds<Pixels>)> {
        let index = window_index_at(&self.windows, self.units_per_px(), position)?;
        let mut window = self.windows[index];
        let units = self.units_per_px();
        let display = size(
            px(self.display_bounds.size.width.as_f32() * units),
            px(self.display_bounds.size.height.as_f32() * units),
        );
        window.region = snap_window_region(&self.windows, display, index);
        let bounds = region_bounds(
            window.region,
            0.0,
            0.0,
            self.units_per_px(),
            self.display_bounds.size,
        )?;
        Some((window, bounds))
    }

    /// Everything an accessibility lookup needs, detached from the frozen
    /// frame so it can run on the background executor.
    fn element_query(&self, window: WindowInfo, position: Point<Pixels>) -> Option<ElementQuery> {
        let factor = self.units_per_px();
        if factor <= 0.0 {
            return None;
        }
        let monitor = self.frame.shot.monitor();
        let x = monitor.x as f32 + position.x.as_f32() * factor;
        let y = monitor.y as f32 + position.y.as_f32() * factor;
        // Accessibility rectangles are global, but the snapshot's window list
        // is monitor-relative; hand the platform a global copy so its
        // "still inside the window" check matches on every display.
        let global = WindowInfo::new(
            Region::new(
                window.region.x + monitor.x as f32,
                window.region.y + monitor.y as f32,
                window.region.width,
                window.region.height,
            ),
            window.id,
            window.pid,
        );
        Some(ElementQuery {
            window: global,
            offset: (monitor.x as f32, monitor.y as f32),
            factor,
            display: (
                self.display_bounds.size.width.as_f32() * factor,
                self.display_bounds.size.height.as_f32() * factor,
            ),
            position: (x, y),
        })
    }
}

/// Physical-pixel block around the cursor, for the color magnifier.
#[derive(Clone, Debug)]
pub struct ColorSample {
    /// `side × side` cells, row-major, `0xRRGGBB`.
    pub cells: Vec<u32>,
    /// The pixel directly under the cursor.
    pub center: u32,
}

impl SessionFrame {
    /// Sample a `side × side` block of physical pixels around a window-local
    /// point, clamped to the frame edges.
    pub fn color_sample(&self, position: Point<Pixels>, side: u32) -> Option<ColorSample> {
        let image = self.frame.shot.image();
        let scale = self.physical_scale();
        if side == 0
            || image.width() == 0
            || image.height() == 0
            || !scale.is_finite()
            || scale <= 0.0
        {
            return None;
        }
        let x = (position.x.as_f32() * scale).round() as i64;
        let y = (position.y.as_f32() * scale).round() as i64;
        Some(sample_block(image, x, y, side))
    }
}

/// RGBA pixels are read straight from the frame buffer; alpha is ignored
/// because frozen screenshots are opaque.
fn sample_block(image: &RgbaImage, x: i64, y: i64, side: u32) -> ColorSample {
    let (width, height) = image.dimensions();
    let raw = image.as_raw();
    let half = (side / 2) as i64;
    let mut cells = Vec::with_capacity((side * side) as usize);
    for row in 0..side as i64 {
        for col in 0..side as i64 {
            let px = (x - half + col).clamp(0, width as i64 - 1) as usize;
            let py = (y - half + row).clamp(0, height as i64 - 1) as usize;
            let at = (py * width as usize + px) * 4;
            cells.push(((raw[at] as u32) << 16) | ((raw[at + 1] as u32) << 8) | raw[at + 2] as u32);
        }
    }
    let center = cells[(half * side as i64 + half) as usize];
    ColorSample { cells, center }
}

/// One accessibility hit converted to logical px, with its role.
#[derive(Clone, Copy, Debug)]
struct HoverElement {
    bounds: Bounds<Pixels>,
    role: ElementRole,
}

/// Elements thinner than this on either side are skipped below; glyphs, icons
/// and separators would otherwise swallow the pointer.
fn is_snappable(bounds: &Bounds<Pixels>) -> bool {
    bounds.size.width.as_f32() >= MIN_ELEMENT_SIZE
        && bounds.size.height.as_f32() >= MIN_ELEMENT_SIZE
}

/// Start the preview on the first real control in the chain instead of a text
/// run or container; the caller can still scroll through every level.
fn preferred_level<'a>(mut elements: impl Iterator<Item = &'a HoverElement>) -> usize {
    elements
        .position(|element| element.role == ElementRole::Control)
        .unwrap_or(0)
}

/// The level that keeps showing the rectangle the user scrolled to, if the
/// rebuilt chain still contains it; otherwise the preferred (smallest control)
/// level.
fn preserved_level(
    levels: &[Bounds<Pixels>],
    previous: Option<Bounds<Pixels>>,
    preferred: usize,
) -> usize {
    let last = levels.len().saturating_sub(1);
    previous
        .and_then(|bounds| levels.iter().position(|level| bounds_close(level, &bounds)))
        .unwrap_or(preferred)
        .min(last)
}

/// Accessibility frames can drift by a fraction of a pixel between queries for
/// the same element.
fn bounds_close(a: &Bounds<Pixels>, b: &Bounds<Pixels>) -> bool {
    let close = |a: f32, b: f32| (a - b).abs() <= 1.0;
    close(a.origin.x.as_f32(), b.origin.x.as_f32())
        && close(a.origin.y.as_f32(), b.origin.y.as_f32())
        && close(a.size.width.as_f32(), b.size.width.as_f32())
        && close(a.size.height.as_f32(), b.size.height.as_f32())
}

/// Index of the frontmost window containing `position`, in monitor units.
fn window_index_at(
    windows: &[WindowInfo],
    units_per_px: f32,
    position: Point<Pixels>,
) -> Option<usize> {
    if units_per_px <= 0.0 {
        return None;
    }
    let ux = position.x.as_f32() * units_per_px;
    let uy = position.y.as_f32() * units_per_px;
    windows.iter().position(|window| {
        let region = window.region;
        ux >= region.x
            && ux < region.x + region.width
            && uy >= region.y
            && uy < region.y + region.height
    })
}

/// Snap region for the window at `index`, with `display` in monitor units. A
/// fullscreen app can split one visual window into several platform windows:
/// Chrome puts the tab strip, toolbar and page content in separate windows, so
/// hitting only the content would never reach the whole browser. When the
/// touching windows of one process cover the display, their union is the
/// window the user sees. Ordinary windows of the same app never reach that
/// coverage, so overlapping or tiled windows stay separate.
fn snap_window_region(windows: &[WindowInfo], display: Size<Pixels>, index: usize) -> Region {
    let base = windows[index];
    let mut union = base.region;
    loop {
        let mut grown = false;
        for other in windows {
            let region = other.region;
            if other.pid != base.pid
                || !same_span(base.region, region)
                || region_within(union, region)
                || !regions_touch(union, region)
            {
                continue;
            }
            union = union_region(union, region);
            grown = true;
        }
        if !grown {
            break;
        }
    }
    let full_width = union.x <= 1.0 && union.width >= display.width.as_f32() - 2.0;
    let tall = union.height >= display.height.as_f32() * 0.9;
    if full_width && tall && union != base.region {
        union
    } else {
        base.region
    }
}

/// Split parts of one visual window share the display width (Chrome's
/// fullscreen tab strip, toolbar and content all span it). Windows the user
/// placed side by side start at different x positions, so they are never
/// merged.
fn same_span(a: Region, b: Region) -> bool {
    (a.x - b.x).abs() <= 1.0 && (a.width - b.width).abs() <= 1.0
}

fn regions_touch(a: Region, b: Region) -> bool {
    a.x <= b.x + b.width + 1.0
        && b.x <= a.x + a.width + 1.0
        && a.y <= b.y + b.height + 1.0
        && b.y <= a.y + a.height + 1.0
}

fn region_within(outer: Region, inner: Region) -> bool {
    inner.x >= outer.x - 0.5
        && inner.y >= outer.y - 0.5
        && inner.x + inner.width <= outer.x + outer.width + 0.5
        && inner.y + inner.height <= outer.y + outer.height + 0.5
}

fn union_region(a: Region, b: Region) -> Region {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    let right = (a.x + a.width).max(b.x + b.width);
    let bottom = (a.y + a.height).max(b.y + b.height);
    Region::new(x, y, right - x, bottom - y)
}

/// Convert a global monitor-unit rectangle to viewport-local logical px,
/// clamping it to the visible area.
fn region_bounds(
    region: Region,
    origin_x: f32,
    origin_y: f32,
    units_per_px: f32,
    viewport: Size<Pixels>,
) -> Option<Bounds<Pixels>> {
    if units_per_px <= 0.0 {
        return None;
    }
    let x = ((region.x - origin_x) / units_per_px).max(0.0);
    let y = ((region.y - origin_y) / units_per_px).max(0.0);
    let right = ((region.x + region.width - origin_x) / units_per_px).min(viewport.width.as_f32());
    let bottom =
        ((region.y + region.height - origin_y) / units_per_px).min(viewport.height.as_f32());
    (right - x >= 1.0 && bottom - y >= 1.0)
        .then(|| Bounds::new(point(px(x), px(y)), size(px(right - x), px(bottom - y))))
}

/// Edges and corners of a committed selection that can be dragged to resize.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResizeHandle {
    NorthWest,
    North,
    NorthEast,
    East,
    SouthEast,
    South,
    SouthWest,
    West,
}

impl ResizeHandle {
    /// Grabbable band around the border, logical px.
    const HIT_BAND: f32 = 8.0;

    /// The handle under a window-local position, if any. Corners win over edges.
    pub fn at(bounds: Bounds<Pixels>, position: Point<Pixels>) -> Option<Self> {
        let left = bounds.origin.x.as_f32();
        let top = bounds.origin.y.as_f32();
        let right = left + bounds.size.width.as_f32();
        let bottom = top + bounds.size.height.as_f32();
        let x = position.x.as_f32();
        let y = position.y.as_f32();
        if x < left - Self::HIT_BAND
            || x > right + Self::HIT_BAND
            || y < top - Self::HIT_BAND
            || y > bottom + Self::HIT_BAND
        {
            return None;
        }
        let near_left = (x - left).abs() <= Self::HIT_BAND;
        let near_right = (x - right).abs() <= Self::HIT_BAND;
        let near_top = (y - top).abs() <= Self::HIT_BAND;
        let near_bottom = (y - bottom).abs() <= Self::HIT_BAND;
        match (near_left, near_right, near_top, near_bottom) {
            (true, _, true, _) => Some(Self::NorthWest),
            (_, true, true, _) => Some(Self::NorthEast),
            (true, _, _, true) => Some(Self::SouthWest),
            (_, true, _, true) => Some(Self::SouthEast),
            (_, _, true, _) => Some(Self::North),
            (_, _, _, true) => Some(Self::South),
            (true, _, _, _) => Some(Self::West),
            (_, true, _, _) => Some(Self::East),
            _ => None,
        }
    }

    fn moves_left(self) -> bool {
        matches!(self, Self::NorthWest | Self::West | Self::SouthWest)
    }

    fn moves_right(self) -> bool {
        matches!(self, Self::NorthEast | Self::East | Self::SouthEast)
    }

    fn moves_top(self) -> bool {
        matches!(self, Self::NorthWest | Self::North | Self::NorthEast)
    }

    fn moves_bottom(self) -> bool {
        matches!(self, Self::SouthWest | Self::South | Self::SouthEast)
    }
}

/// Apply a handle drag to the selection, keeping it inside the display and no
/// smaller than `MIN_SELECTION`.
fn resized_bounds(
    bounds: Bounds<Pixels>,
    handle: ResizeHandle,
    position: Point<Pixels>,
    limit: Size<Pixels>,
) -> Bounds<Pixels> {
    let min = px(MIN_SELECTION);
    let mut left = bounds.origin.x;
    let mut top = bounds.origin.y;
    let mut right = bounds.origin.x + bounds.size.width;
    let mut bottom = bounds.origin.y + bounds.size.height;
    let x = position.x.max(px(0.)).min(limit.width);
    let y = position.y.max(px(0.)).min(limit.height);
    if handle.moves_left() {
        left = x.min(right - min);
    }
    if handle.moves_right() {
        right = x.max(left + min);
    }
    if handle.moves_top() {
        top = y.min(bottom - min);
    }
    if handle.moves_bottom() {
        bottom = y.max(top + min);
    }
    Bounds::new(point(left, top), size(right - left, bottom - top))
}

#[derive(Clone, Copy, Debug)]
pub struct Selection {
    frame_ix: usize,
    anchor: Point<Pixels>,
    head: Point<Pixels>,
    dragging: bool,
    /// Element or window bounds under the cursor when the drag started; a
    /// click (no real drag) commits this rectangle instead of clearing.
    click_bounds: Option<Bounds<Pixels>>,
    /// Edge or corner currently being dragged to resize the selection.
    resize: Option<ResizeHandle>,
}

impl Selection {
    pub fn frame_ix(&self) -> usize {
        self.frame_ix
    }

    pub fn is_dragging(&self) -> bool {
        self.dragging
    }

    pub fn is_resizing(&self) -> bool {
        self.resize.is_some()
    }

    /// Window-local bounds in logical px.
    pub fn bounds(&self) -> Bounds<Pixels> {
        let origin = point(
            self.anchor.x.min(self.head.x),
            self.anchor.y.min(self.head.y),
        );
        let size = Size::new(
            (self.anchor.x - self.head.x).abs(),
            (self.anchor.y - self.head.y).abs(),
        );
        Bounds::new(origin, size)
    }

    fn is_meaningful(&self) -> bool {
        let size = self.bounds().size;
        size.width.as_f32() >= MIN_SELECTION && size.height.as_f32() >= MIN_SELECTION
    }
}

/// What a pointer press inside the captured region landed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnnotationPick {
    /// Empty space: the pick was cleared.
    None,
    /// A shape was picked and is now being dragged.
    Shape,
    /// A resize handle of the picked shape is being dragged.
    Handle,
    /// A text annotation was reopened for editing.
    Text,
}

/// What the user asked to do with the current selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionCommand {
    Copy,
    Save,
    Pin,
    Recognize,
    Record(RecordFormat),
    Cancel,
}

/// What a capture was started for. Anything but `Choose` acts as soon as a
/// region is selected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CaptureIntent {
    #[default]
    Choose,
    Recognize,
    Record(RecordFormat),
}

impl CaptureIntent {
    fn command(self) -> Option<SessionCommand> {
        match self {
            Self::Choose => None,
            Self::Recognize => Some(SessionCommand::Recognize),
            Self::Record(format) => Some(SessionCommand::Record(format)),
        }
    }

    pub fn hint(self) -> String {
        match self {
            Self::Choose => t!("session.hint.choose").to_string(),
            Self::Recognize => t!("session.hint.recognize").to_string(),
            Self::Record(_) => t!("session.hint.record").to_string(),
        }
    }
}

pub struct CaptureSession {
    /// One entry per overlay window; `frames` is filled after the freeze.
    slots: Vec<SessionSlot>,
    frames: Vec<Option<SessionFrame>>,
    /// The selection being dragged or resized, or the last committed region.
    selection: Option<Selection>,
    /// Earlier committed regions while Shift-dragging several areas. Combined
    /// with `selection` in the order they were added.
    extra_selections: Vec<Selection>,
    /// Shift is held: clicking adds or removes regions instead of replacing
    /// the current one, and releasing Shift finishes the collection.
    collecting: bool,
    /// The finished collection composited into one image, so the toolbar and
    /// annotations work on the combined result.
    composed: Option<Composed>,
    intent: CaptureIntent,
    annotations: Layer,
    window_snap: bool,
    element_snap: bool,
    /// Element/window rectangle highlighted under the cursor.
    hover: Option<Hover>,
    /// Sign of the last wheel scroll, so the first scroll in either direction
    /// widens the preview and reversing it narrows again.
    hover_scroll: Option<f32>,
    /// Accessibility result for the window currently under the cursor.
    element_cache: Option<ElementCache>,
    /// Accessibility lookup in flight; stale results are dropped by generation.
    pending_hover: Option<PendingHover>,
    hover_generation: u64,
    /// The highlight is only the window, waiting for the element chain.
    hover_provisional: bool,
}

/// A Shift collection combined into one selection when Shift is released.
/// Regions keep their on-screen positions on a transparent canvas, so the
/// image is what Copy / Save / Pin export; the gaps stay transparent.
struct Composed {
    /// The collected regions, so pressing Shift again re-opens the collection.
    regions: Vec<Selection>,
    image: RgbaImage,
    /// Union of the regions on screen, in overlay-local logical px.
    bounds: Bounds<Pixels>,
}

/// Identifies the pixels [`CaptureSession::crop_selection`] cuts. A mosaic needs
/// those pixels on every painted frame, so the overlay caches one crop per key
/// instead of cutting a full-screen screenshot again (milliseconds) each time.
/// Bounds are quantised to a thousandth of a pixel: any resize lands on a new
/// key, so a stale crop can never be shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CropKey {
    /// `None` for a combined (composited) selection, which has its own image.
    frame_ix: Option<usize>,
    bounds: [i64; 4],
}

impl CropKey {
    fn new(frame_ix: Option<usize>, bounds: Bounds<Pixels>) -> Self {
        let quantise = |value: Pixels| (f64::from(value.as_f32()) * 1000.0).round() as i64;
        Self {
            frame_ix,
            bounds: [
                quantise(bounds.origin.x),
                quantise(bounds.origin.y),
                quantise(bounds.size.width),
                quantise(bounds.size.height),
            ],
        }
    }

    /// A cheap value that changes with the pixels behind this key. The mosaic
    /// block cache compares it so a new region never reuses the colours
    /// sampled from the old one.
    pub fn stamp(self) -> u64 {
        let mut hash = match self.frame_ix {
            Some(ix) => ix as u64,
            None => u64::MAX,
        };
        for value in self.bounds {
            hash = (hash ^ value as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            hash ^= hash >> 29;
        }
        hash
    }
}

/// Hovered element chain: smallest element first, then the whole window, then
/// the whole display. The display is the last wheel step, so scrolling past a
/// window reaches full screen without having to remember a separate key.
#[derive(Clone, Debug, PartialEq)]
struct Hover {
    frame_ix: usize,
    window_id: u64,
    levels: Vec<Bounds<Pixels>>,
    level: usize,
}

impl Hover {
    fn bounds(&self) -> Bounds<Pixels> {
        self.levels[self.level.min(self.levels.len().saturating_sub(1))]
    }
}

struct ElementCache {
    frame_ix: usize,
    window_id: u64,
    window: Bounds<Pixels>,
    queried_at: Instant,
    elements: Vec<HoverElement>,
}

/// An accessibility lookup running off the UI thread. Its chain replaces the
/// provisional window highlight when it lands, unless the pointer moved on.
#[derive(Clone, Copy, PartialEq)]
struct PendingHover {
    generation: u64,
    frame_ix: usize,
    window_id: u64,
    window: Bounds<Pixels>,
}

/// A detached accessibility lookup: everything it needs is owned plain data.
struct ElementQuery {
    window: WindowInfo,
    /// Monitor origin in global units.
    offset: (f32, f32),
    /// Monitor units per logical overlay pixel.
    factor: f32,
    /// Display size in monitor units.
    display: (f32, f32),
    /// Global point to hit-test.
    position: (f32, f32),
}

impl ElementQuery {
    fn run(self) -> Vec<HoverElement> {
        let (offset_x, offset_y) = self.offset;
        let display = size(px(self.display.0), px(self.display.1));
        snapit_capture::elements_at(self.window, self.position.0, self.position.1)
            .into_iter()
            .filter_map(|hit| {
                let bounds = region_bounds(hit.region, offset_x, offset_y, self.factor, display)?;
                Some(HoverElement {
                    bounds,
                    role: hit.role,
                })
            })
            .collect()
    }
}

/// Turn an accessibility chain into hover levels (smallest first, window
/// last) and the level to show without scrolling. `apply_hover` appends the
/// whole display as the outermost wheel level. Borrowed: the cached chain is
/// reused across pointer moves, so it must not be consumed.
fn element_levels(
    elements: &[HoverElement],
    window_bounds: Bounds<Pixels>,
) -> (Vec<Bounds<Pixels>>, usize) {
    let snappable = || {
        elements
            .iter()
            .filter(|element| is_snappable(&element.bounds))
    };
    let preferred = preferred_level(snappable());
    let mut levels: Vec<Bounds<Pixels>> = snappable().map(|element| element.bounds).collect();
    if levels.last() != Some(&window_bounds) {
        levels.push(window_bounds);
    }
    (levels, preferred)
}

/// Append the whole display as the outermost wheel level. A window that
/// already spans the display is not duplicated, so the dial never stalls on
/// two identical rectangles.
fn with_fullscreen_level(
    mut levels: Vec<Bounds<Pixels>>,
    display: Size<Pixels>,
) -> Vec<Bounds<Pixels>> {
    let full = Bounds::new(point(px(0.), px(0.)), display);
    if levels.last() != Some(&full) {
        levels.push(full);
    }
    levels
}

impl EventEmitter<SessionCommand> for CaptureSession {}

impl CaptureSession {
    pub fn new(
        slots: Vec<SessionSlot>,
        intent: CaptureIntent,
        window_snap: bool,
        element_snap: bool,
        annotation_defaults: AnnotationConfig,
    ) -> Self {
        let frames = slots.iter().map(|_| None).collect();
        Self {
            slots,
            frames,
            selection: None,
            extra_selections: Vec::new(),
            collecting: false,
            composed: None,
            intent,
            annotations: Layer::new(annotation_defaults),
            window_snap,
            element_snap,
            hover: None,
            hover_scroll: None,
            element_cache: None,
            pending_hover: None,
            hover_generation: 0,
            hover_provisional: false,
        }
    }

    pub fn intent(&self) -> CaptureIntent {
        self.intent
    }

    pub fn slots(&self) -> &[SessionSlot] {
        &self.slots
    }

    pub fn frame(&self, ix: usize) -> Option<&SessionFrame> {
        self.frames.get(ix)?.as_ref()
    }

    /// Attach the freeze to the pre-placed slots. Monitors are matched to
    /// slots by platform display id, then by capture order; a slot left empty
    /// (its display failed to freeze) stays frameless and is closed by the
    /// caller.
    pub fn set_frames(&mut self, frames: Vec<CapturedFrame>, cx: &mut Context<Self>) {
        let mut frames: Vec<Option<CapturedFrame>> = frames.into_iter().map(Some).collect();
        for slot_ix in 0..self.slots.len() {
            let slot = self.slots[slot_ix];
            let matched = frames.iter().position(|frame| {
                frame
                    .as_ref()
                    .is_some_and(|frame| slot.matches(frame.shot.monitor().id))
            });
            if let Some(frame) = matched.and_then(|frame_ix| frames[frame_ix].take()) {
                self.place(slot_ix, frame);
            }
        }
        let mut remaining = frames.into_iter().flatten();
        for slot_ix in 0..self.slots.len() {
            if self.frames[slot_ix].is_some() {
                continue;
            }
            let Some(frame) = remaining.next() else {
                break;
            };
            self.place(slot_ix, frame);
        }
        cx.notify();
    }

    fn place(&mut self, slot_ix: usize, frame: CapturedFrame) {
        let frame = SessionFrame::new(frame, &self.slots[slot_ix]);
        self.frames[slot_ix] = Some(frame);
    }

    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }

    /// The finished (non-dragging) selection, if any.
    pub fn committed_selection(&self) -> Option<Selection> {
        self.selection.filter(|selection| !selection.dragging)
    }

    /// Whether several committed regions are selected (Shift-drag). Actions
    /// then combine them into one image.
    pub fn multi_selection(&self) -> bool {
        !self.extra_selections.is_empty() && self.committed_selection().is_some()
    }

    pub fn selection_count(&self) -> usize {
        self.extra_selections.len() + usize::from(self.committed_selection().is_some())
    }

    /// Auto window/element snap only helps find a region. Once a selection is
    /// committed, previewing or click-snapping another window would fight the
    /// resize handles, annotations and toolbar. Shift collecting is the one
    /// exception: more regions are being added on purpose.
    fn auto_snap(&self, adding: bool) -> bool {
        self.window_snap && (adding || self.collecting || self.selection_count() == 0)
    }

    /// Whether the collected regions were combined into one selection. The
    /// combined selection behaves like a normal one: annotations, copy, save
    /// and pin, but no resizing or GIF recording.
    pub fn is_composed(&self) -> bool {
        self.composed.is_some()
    }

    /// Committed regions in the order they were added; the active one is last.
    pub fn committed_selections(&self) -> Vec<Selection> {
        let mut all = self.extra_selections.clone();
        if let Some(active) = self.committed_selection() {
            all.push(active);
        }
        all
    }

    /// The committed region under `position`. Hover and every painted frame ask
    /// for this, so it walks the collection instead of building one.
    pub fn selection_at(&self, frame_ix: usize, position: Point<Pixels>) -> Option<Selection> {
        let active = self.committed_selection();
        self.extra_selections
            .iter()
            .chain(active.iter())
            .find(|selection| {
                selection.frame_ix == frame_ix && selection.bounds().contains(&position)
            })
            .copied()
    }

    /// Shift multi-select mode: clicking toggles regions instead of replacing
    /// them, and releasing Shift finishes the collection. Annotation work
    /// owns Shift for its own modifiers, so it never enters this mode.
    pub fn collecting(&self) -> bool {
        self.collecting
    }

    /// Shift collection opens whenever the region is still empty. Once the
    /// selection has been drawn on (or an annotation is picked) it is locked,
    /// so Shift neither opens a collection nor appends another region: the
    /// annotations belong to that one region.
    pub fn set_collecting(&mut self, collecting: bool, cx: &mut Context<Self>) {
        if collecting
            && (self.intent != CaptureIntent::Choose
                || self.annotations.is_typing()
                || self.annotations.can_undo()
                || self.annotations.selected().is_some())
        {
            return;
        }
        if self.collecting == collecting {
            return;
        }
        self.collecting = collecting;
        if collecting {
            // Re-open the combined selection's regions so Shift-click can add
            // or remove them again.
            if let Some(composed) = self.composed.take() {
                self.annotations.clear();
                let mut regions = composed.regions;
                self.selection = regions.pop();
                self.extra_selections = regions;
            }
        } else {
            self.clear_hover(cx);
            if self.selection.is_some_and(|selection| selection.dragging) {
                self.end_selection(cx);
            }
            self.compose();
        }
        cx.notify();
    }

    /// Combine the collected regions into one selection. Only regions on the
    /// same display share a screen coordinate space; a collection spanning
    /// displays keeps the separate-region behavior.
    fn compose(&mut self) {
        let regions = self.committed_selections();
        let Some(frame_ix) = regions.first().map(|region| region.frame_ix) else {
            return;
        };
        if regions.len() < 2 || regions.iter().any(|region| region.frame_ix != frame_ix) {
            return;
        }
        let Some(frame) = self.frames.get(frame_ix).and_then(Option::as_ref) else {
            return;
        };
        let mut origin = regions[0].bounds().origin;
        let mut crops = Vec::with_capacity(regions.len());
        for region in &regions {
            let bounds = region.bounds();
            if bounds.origin.x < origin.x {
                origin.x = bounds.origin.x;
            }
            if bounds.origin.y < origin.y {
                origin.y = bounds.origin.y;
            }
            let Some(image) = frame.frame.shot.crop(frame.to_region(bounds)) else {
                return;
            };
            crops.push((bounds, image));
        }
        let (image, composed) = compose_at_positions(crops, frame.physical_scale());
        let bounds = Bounds::new(origin, composed.size);
        self.selection = Some(Selection {
            frame_ix,
            anchor: bounds.origin,
            head: point(
                bounds.origin.x + bounds.size.width,
                bounds.origin.y + bounds.size.height,
            ),
            dragging: false,
            click_bounds: None,
            resize: None,
        });
        self.extra_selections.clear();
        self.annotations.clear();
        self.composed = Some(Composed {
            regions,
            image,
            bounds,
        });
    }

    /// Remove the committed region under `position`. Shift-clicking a region
    /// while collecting toggles it out of the collection.
    pub fn remove_selection_at(
        &mut self,
        frame_ix: usize,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.committed_selection().is_some_and(|selection| {
            selection.frame_ix == frame_ix && selection.bounds().contains(&position)
        }) {
            // Annotations belong to the active region.
            self.annotations.clear();
            self.selection = self.extra_selections.pop();
        } else if let Some(ix) = self.extra_selections.iter().position(|selection| {
            selection.frame_ix == frame_ix && selection.bounds().contains(&position)
        }) {
            self.extra_selections.remove(ix);
        } else {
            return false;
        }
        self.clear_hover(cx);
        cx.notify();
        true
    }

    /// Selections whose frame is `frame_ix`, including a region being dragged.
    pub fn selections_on_frame(&self, frame_ix: usize) -> Vec<Selection> {
        self.extra_selections
            .iter()
            .chain(self.selection.iter())
            .filter(|selection| selection.frame_ix == frame_ix)
            .copied()
            .collect()
    }

    /// Make the region under `position` active (so it can be resized and
    /// annotated). Returns whether a committed region was hit.
    pub fn activate_selection_at(
        &mut self,
        frame_ix: usize,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.committed_selection().is_some_and(|selection| {
            selection.frame_ix == frame_ix && selection.bounds().contains(&position)
        }) {
            return true;
        }
        let Some(ix) = self.extra_selections.iter().position(|selection| {
            selection.frame_ix == frame_ix && selection.bounds().contains(&position)
        }) else {
            return false;
        };
        let clicked = self.extra_selections.remove(ix);
        // Annotations are local to the old active region.
        self.annotations.clear();
        if let Some(active) = self.selection.replace(clicked) {
            self.extra_selections.push(active);
        }
        cx.notify();
        true
    }

    /// Select the whole display. `F` during a capture; replaces any regions.
    pub fn select_full(&mut self, frame_ix: usize, cx: &mut Context<Self>) {
        let Some(frame) = self.frames.get(frame_ix).and_then(Option::as_ref) else {
            return;
        };
        let size = frame.display_bounds().size;
        self.annotations.clear();
        self.extra_selections.clear();
        self.composed = None;
        self.clear_hover(cx);
        self.selection = Some(Selection {
            frame_ix,
            anchor: point(px(0.), px(0.)),
            head: point(size.width, size.height),
            dragging: false,
            click_bounds: None,
            resize: None,
        });
        // Whole-display capture is still annotation territory.
        if let Some(command) = self.intent.command() {
            cx.emit(command);
        }
        cx.notify();
    }

    pub fn is_resizing(&self) -> bool {
        self.selection
            .is_some_and(|selection| selection.is_resizing())
    }

    pub fn begin_selection(
        &mut self,
        frame_ix: usize,
        position: Point<Pixels>,
        add: bool,
        cx: &mut Context<Self>,
    ) {
        // Do not snap a click to a window once a selection is finished; the
        // click only clears the selection or starts a new manual drag.
        // `add` reaches here set by the caller: Shift-clicking during a
        // collection keeps the finished regions, a plain press replaces them.
        let auto_snap = self.auto_snap(add);
        self.annotations.clear();
        self.composed = None;
        if add {
            // Shift keeps the finished regions and starts one more.
            if let Some(active) = self.committed_selection() {
                self.extra_selections.push(active);
            }
        } else {
            self.extra_selections.clear();
        }
        // A click (rather than a drag) commits whatever the hover preview shows.
        let click_bounds = self
            .hover
            .as_ref()
            .filter(|_| auto_snap)
            .filter(|hover| hover.frame_ix == frame_ix && hover.bounds().contains(&position))
            .map(Hover::bounds)
            .or_else(|| {
                if !auto_snap {
                    return None;
                }
                self.frames
                    .get(frame_ix)?
                    .as_ref()?
                    .window_at(position)
                    .map(|(_, bounds)| bounds)
            });
        self.hover = None;
        self.hover_scroll = None;
        self.element_cache = None;
        self.pending_hover = None;
        self.hover_provisional = false;
        self.selection = Some(Selection {
            frame_ix,
            anchor: position,
            head: position,
            dragging: true,
            click_bounds,
            resize: None,
        });
        cx.notify();
    }

    pub fn drag_to(&mut self, frame_ix: usize, position: Point<Pixels>, cx: &mut Context<Self>) {
        if let Some(selection) = self.selection.as_mut()
            && selection.dragging
            && selection.frame_ix == frame_ix
        {
            selection.head = position;
            cx.notify();
        }
    }

    pub fn end_selection(&mut self, cx: &mut Context<Self>) {
        let Some(selection) = self.selection.as_mut() else {
            return;
        };
        if !selection.dragging {
            return;
        }
        selection.dragging = false;
        if !selection.is_meaningful() {
            match selection.click_bounds {
                Some(bounds) => {
                    selection.anchor = bounds.origin;
                    selection.head = point(
                        bounds.origin.x + bounds.size.width,
                        bounds.origin.y + bounds.size.height,
                    );
                }
                None => self.selection = self.extra_selections.pop(),
            }
        }
        // A finished single selection owns the pointer: drop the preview and
        // any accessibility lookup still in flight, otherwise a late result
        // can light up a window over the committed region. A Shift collection
        // stays open, so the next drag adds another region and the hover
        // preview is still what tells the user where that one would land.
        if !self.collecting {
            self.clear_hover(cx);
        }
        if self.selection.is_some()
            && let Some(command) = self.intent.command()
        {
            cx.emit(command);
        }
        cx.notify();
    }

    /// Start dragging one edge or corner of the committed selection.
    pub fn begin_resize(
        &mut self,
        frame_ix: usize,
        handle: ResizeHandle,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.composed.is_some() {
            return false;
        }
        let Some(selection) = self
            .selection
            .filter(|selection| !selection.dragging && selection.frame_ix == frame_ix)
        else {
            return false;
        };
        // Finish a text draft so it translates with the rest of the annotations.
        self.annotations.commit();
        let bounds = selection.bounds();
        self.selection = Some(Selection {
            frame_ix,
            anchor: bounds.origin,
            head: point(
                bounds.origin.x + bounds.size.width,
                bounds.origin.y + bounds.size.height,
            ),
            dragging: false,
            click_bounds: None,
            resize: Some(handle),
        });
        cx.notify();
        true
    }

    /// Move the dragged edge or corner, clamped to the display. Annotations
    /// shift with the top-left corner so they stay on the same screen content.
    pub fn resize_to(
        &mut self,
        frame_ix: usize,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(selection) = self.selection.as_mut() else {
            return false;
        };
        let Some(handle) = selection.resize else {
            return false;
        };
        if selection.frame_ix != frame_ix {
            return false;
        }
        let Some(frame) = self.frames.get(frame_ix).and_then(Option::as_ref) else {
            return false;
        };
        let previous = selection.bounds();
        let bounds = resized_bounds(previous, handle, position, frame.display_bounds().size);
        selection.anchor = bounds.origin;
        selection.head = point(
            bounds.origin.x + bounds.size.width,
            bounds.origin.y + bounds.size.height,
        );
        self.annotations.translate(
            (bounds.origin.x - previous.origin.x).as_f32(),
            (bounds.origin.y - previous.origin.y).as_f32(),
        );
        cx.notify();
        true
    }

    pub fn end_resize(&mut self, cx: &mut Context<Self>) {
        let Some(selection) = self.selection.as_mut() else {
            return;
        };
        if selection.resize.take().is_some() {
            cx.notify();
        }
    }

    /// Highlight the element or window under the cursor until a region is
    /// committed. A finished selection owns the pointer: no other target is
    /// previewed, inside or outside it, so resizing and annotation stay calm.
    /// Shift collecting re-enables the preview outside existing regions so
    /// the next one can be added. The window lights up right away;
    /// accessibility chains are cached, throttled and queried off-thread so a
    /// sweep across windows never stalls the UI.
    pub fn set_hover(&mut self, frame_ix: usize, position: Point<Pixels>, cx: &mut Context<Self>) {
        let over_region = self.selection_at(frame_ix, position).is_some();
        if !self.auto_snap(false) || over_region || self.selection.is_some_and(|s| s.is_dragging())
        {
            self.clear_hover(cx);
            return;
        }
        let target = self
            .frames
            .get(frame_ix)
            .and_then(Option::as_ref)
            .and_then(|frame| {
                let (window, window_bounds) = frame.window_at(position)?;
                let query = frame.element_query(window, position);
                Some((window, window_bounds, query))
            });
        let Some((window, window_bounds, query)) = target else {
            // Over empty desktop there is no window to snap to. Keep a full
            // display preview the wheel already selected instead of dropping
            // it on the first pointer move.
            if !self.hover_is_full_frame(frame_ix) {
                self.clear_hover(cx);
            }
            return;
        };
        let mut provisional = false;
        if self.element_snap {
            match self.hover_levels(frame_ix, window, window_bounds, query, position, cx) {
                Some((levels, preferred)) => {
                    let preserve = !self.hover_provisional;
                    self.apply_hover(frame_ix, window.id, levels, preferred, preserve, cx);
                    self.hover_provisional = false;
                    return;
                }
                None => provisional = true,
            }
        }
        self.apply_hover(
            frame_ix,
            window.id,
            vec![window_bounds],
            0,
            !provisional,
            cx,
        );
        self.hover_provisional = provisional;
    }

    /// Hover levels for the current pointer, reusing the cached accessibility
    /// chain while the cursor stays inside its smallest element (or the
    /// throttle window has not passed). Returns `None` only when a lookup was
    /// started off-thread and no chain has been cached yet.
    ///
    /// The cache is read, never cloned: this runs on every idle pointer move.
    fn hover_levels(
        &mut self,
        frame_ix: usize,
        window: WindowInfo,
        window_bounds: Bounds<Pixels>,
        query: Option<ElementQuery>,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<(Vec<Bounds<Pixels>>, usize)> {
        let now = Instant::now();
        let cached = self
            .element_cache
            .as_ref()
            .filter(|cache| {
                cache.frame_ix == frame_ix
                    && cache.window_id == window.id
                    && cache.window == window_bounds
            })
            .map(|cache| {
                let fresh = cache
                    .elements
                    .first()
                    .is_some_and(|smallest| smallest.bounds.contains(&position))
                    || now.duration_since(cache.queried_at) < ELEMENT_QUERY_INTERVAL;
                (fresh, element_levels(&cache.elements, window_bounds))
            });
        if let Some((true, levels)) = cached {
            return Some(levels);
        }
        if let Some(elements) =
            self.start_element_query(frame_ix, window.id, window_bounds, query, cx)
        {
            return Some(element_levels(&elements, window_bounds));
        }
        // The lookup runs off-thread: keep the stale chain on screen meanwhile.
        cached.map(|(_, levels)| levels)
    }

    /// Whether the wheel has dialled all the way out to the whole display.
    fn hover_is_full_frame(&self, frame_ix: usize) -> bool {
        let Some(hover) = self.hover.as_ref() else {
            return false;
        };
        if hover.frame_ix != frame_ix || hover.level + 1 != hover.levels.len() {
            return false;
        }
        self.frames
            .get(frame_ix)
            .and_then(Option::as_ref)
            .is_some_and(|frame| {
                hover.bounds() == Bounds::new(point(px(0.), px(0.)), frame.display_bounds().size)
            })
    }

    fn clear_hover(&mut self, cx: &mut Context<Self>) {
        self.pending_hover = None;
        self.hover_provisional = false;
        if self.hover.take().is_some() {
            self.element_cache = None;
            cx.notify();
        }
        self.hover_scroll = None;
    }

    /// Start an accessibility lookup. On macOS the AX round-trip is slow and
    /// runs on the background executor, so only the latest lookup for a window
    /// is kept and older results are discarded; elsewhere the lookup is cheap
    /// and its chain is returned to apply right away.
    fn start_element_query(
        &mut self,
        frame_ix: usize,
        window_id: u64,
        window_bounds: Bounds<Pixels>,
        query: Option<ElementQuery>,
        cx: &mut Context<Self>,
    ) -> Option<Vec<HoverElement>> {
        let query = query?;
        if self
            .pending_hover
            .is_some_and(|pending| pending.frame_ix == frame_ix && pending.window_id == window_id)
        {
            return None;
        }
        self.hover_generation += 1;
        let pending = PendingHover {
            generation: self.hover_generation,
            frame_ix,
            window_id,
            window: window_bounds,
        };
        self.pending_hover = Some(pending);
        #[cfg(target_os = "macos")]
        {
            cx.spawn(async move |session, cx| {
                let elements = cx
                    .background_executor()
                    .spawn(async move { query.run() })
                    .await;
                session
                    .update(cx, |session, cx| {
                        session.finish_element_query(pending, elements, cx)
                    })
                    .ok();
            })
            .detach();
            None
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = cx;
            let elements = query.run();
            self.pending_hover = None;
            self.element_cache = Some(ElementCache {
                frame_ix,
                window_id,
                window: window_bounds,
                queried_at: Instant::now(),
                elements: elements.clone(),
            });
            Some(elements)
        }
    }

    /// Only macOS queries elements off the main thread; everywhere else the
    /// result is already in hand when this would be called.
    #[cfg(target_os = "macos")]
    fn finish_element_query(
        &mut self,
        pending: PendingHover,
        elements: Vec<HoverElement>,
        cx: &mut Context<Self>,
    ) {
        if self.pending_hover != Some(pending) {
            return;
        }
        self.pending_hover = None;
        self.hover_provisional = false;
        // A selection may have been committed while the lookup was running.
        if !self.auto_snap(false) {
            self.clear_hover(cx);
            return;
        }
        let still_here = self.hover.as_ref().is_some_and(|hover| {
            hover.frame_ix == pending.frame_ix && hover.window_id == pending.window_id
        });
        if !self.element_snap || !still_here {
            return;
        }
        self.element_cache = Some(ElementCache {
            frame_ix: pending.frame_ix,
            window_id: pending.window_id,
            window: pending.window,
            queried_at: Instant::now(),
            elements: elements.clone(),
        });
        let (levels, preferred) = element_levels(&elements, pending.window);
        self.apply_hover(
            pending.frame_ix,
            pending.window_id,
            levels,
            preferred,
            false,
            cx,
        );
    }

    /// Show a hover chain, keeping the level the wheel scrolled to when the
    /// chain still contains that rectangle. `preserve` is off when the previous
    /// highlight was only a provisional window waiting for accessibility.
    fn apply_hover(
        &mut self,
        frame_ix: usize,
        window_id: u64,
        levels: Vec<Bounds<Pixels>>,
        preferred: usize,
        preserve: bool,
        cx: &mut Context<Self>,
    ) {
        // Every chain ends at the whole display, including the provisional
        // window-only one shown while accessibility is still being queried.
        let levels = match self.frames.get(frame_ix).and_then(Option::as_ref) {
            Some(frame) => with_fullscreen_level(levels, frame.display_bounds().size),
            None => levels,
        };
        let same_chain = self.hover.as_ref().is_some_and(|hover| {
            hover.frame_ix == frame_ix && hover.window_id == window_id && hover.levels == levels
        });
        // A new window starts a fresh dial, so the next scroll widens the
        // preview whichever way it turns.
        if !self
            .hover
            .as_ref()
            .is_some_and(|hover| hover.window_id == window_id)
        {
            self.hover_scroll = None;
        }
        let level = if same_chain {
            self.hover
                .as_ref()
                .map_or(0, |hover| hover.level.min(levels.len() - 1))
        } else {
            // The accessibility chain is live, so a slightly moved pointer can
            // return a different chain in the same window. Keep the level the
            // user scrolled to by matching the highlighted rectangle instead
            // of dropping back to the smallest element.
            let previous = preserve.then(|| {
                self.hover
                    .as_ref()
                    .filter(|hover| hover.frame_ix == frame_ix && hover.window_id == window_id)
                    .map(Hover::bounds)
            });
            preserved_level(&levels, previous.flatten(), preferred)
        };
        let next = Hover {
            frame_ix,
            window_id,
            levels,
            level,
        };
        if self.hover.as_ref() != Some(&next) {
            self.hover = Some(next);
            cx.notify();
        }
    }

    /// Step the hover preview with the wheel. The wheel acts like a dial: the
    /// first scroll in either direction widens the preview to its parent, and
    /// reversing it narrows back to the smaller element.
    /// Returns true when the hover handled the scroll.
    pub fn cycle_hover(&mut self, frame_ix: usize, delta: f32, cx: &mut Context<Self>) -> bool {
        if !self
            .hover
            .as_ref()
            .is_some_and(|hover| hover.frame_ix == frame_ix)
        {
            return false;
        }
        let outward = self
            .hover_scroll
            .is_none_or(|previous| previous.signum() == delta.signum());
        self.hover_scroll = Some(delta);
        let Some(hover) = self.hover.as_mut() else {
            return false;
        };
        let last = hover.levels.len().saturating_sub(1);
        let next = if outward {
            (hover.level + 1).min(last)
        } else {
            hover.level.saturating_sub(1)
        };
        if next != hover.level {
            hover.level = next;
            cx.notify();
        }
        true
    }

    pub fn hover_bounds(&self, frame_ix: usize) -> Option<Bounds<Pixels>> {
        self.hover
            .as_ref()
            .filter(|hover| hover.frame_ix == frame_ix)
            .map(Hover::bounds)
    }

    pub fn clear_selection(&mut self, cx: &mut Context<Self>) {
        self.annotations.clear();
        self.clear_hover(cx);
        self.extra_selections.clear();
        self.composed = None;
        if self.selection.take().is_some() {
            cx.notify();
        }
    }

    pub fn annotations(&self) -> &Layer {
        &self.annotations
    }

    pub fn set_annotation_tool(&mut self, tool: Tool, cx: &mut Context<Self>) {
        self.annotations.set_tool(tool);
        cx.notify();
    }

    /// Clear the picked annotation, so a tool button's style panel edits that
    /// tool's defaults rather than the shape that was drawn or picked last.
    pub fn deselect_annotation(&mut self, cx: &mut Context<Self>) {
        if self.annotations.deselect() {
            cx.notify();
        }
    }

    pub fn set_annotation_color(&mut self, color: u32, cx: &mut Context<Self>) {
        self.annotations.set_color(color);
        cx.notify();
    }

    pub fn set_annotation_size(&mut self, size: f32, cx: &mut Context<Self>) {
        self.annotations.set_size(size);
        cx.notify();
    }

    /// Only the remembered defaults are durable. A picked shape's style lives
    /// with this capture and is baked into the exported image: restyling a
    /// selection never touches `config()`, so this is safe even while a shape
    /// is selected. The overlay debounces this while a control is dragged.
    pub fn persist_annotation_defaults(&self, cx: &mut Context<Self>) {
        let defaults = self.annotations.config().clone();
        crate::settings::AppSettings::update(cx, |config| config.annotations = defaults);
    }

    /// Pick an annotation, grab one of its handles, or reopen a text
    /// annotation for editing. Picking is independent of the armed tool: any
    /// press inside the region tries this before drawing. Finishes any
    /// in-progress text draft first, and a picked shape starts moving right
    /// away, so press-and-drag works in one gesture.
    pub fn pick_annotation_at(
        &mut self,
        position: Point<Pixels>,
        double_click: bool,
        cx: &mut Context<Self>,
    ) -> AnnotationPick {
        let Some(bounds) = self
            .committed_selection()
            .map(|selection| selection.bounds())
        else {
            return AnnotationPick::None;
        };
        let local = local_point(bounds, position);
        let pick = if double_click && self.annotations.begin_text_edit(local[0], local[1]) {
            AnnotationPick::Text
        } else {
            match self.annotations.begin_edit(local[0], local[1]) {
                annotate::EditStart::Handle => AnnotationPick::Handle,
                annotate::EditStart::Shape => AnnotationPick::Shape,
                annotate::EditStart::None => AnnotationPick::None,
            }
        };
        cx.notify();
        pick
    }

    /// Whether an annotation sits under `position`. Read-only: used for the
    /// idle cursor, which turns into a hand over anything that can be grabbed.
    pub fn annotation_under(&self, position: Point<Pixels>) -> bool {
        let Some(bounds) = self
            .committed_selection()
            .map(|selection| selection.bounds())
        else {
            return false;
        };
        let local = local_point(bounds, position);
        self.annotations.hits_at(local[0], local[1])
    }

    /// Follow the pointer while a picked shape is being moved or resized.
    pub fn drag_annotation_edit(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(bounds) = self
            .committed_selection()
            .map(|selection| selection.bounds())
        else {
            return;
        };
        let local = local_point(bounds, position);
        let viewport = [bounds.size.width.as_f32(), bounds.size.height.as_f32()];
        // A pointer pinned against the edge of the region moves nothing, and
        // the canvas is expensive to repaint, so only ask when it changed.
        if self.annotations.drag_edit(local[0], local[1], viewport) {
            cx.notify();
        }
    }

    pub fn end_annotation_edit(&mut self, cx: &mut Context<Self>) {
        if self.annotations.end_edit() {
            cx.notify();
        }
    }

    pub fn delete_selected_annotation(&mut self, cx: &mut Context<Self>) {
        if self.annotations.delete_selected() {
            cx.notify();
        }
    }

    /// Press inside a committed region with a drawing tool: start a new shape.
    /// Picking an existing annotation happens first (see `pick_annotation_at`),
    /// so this only ever sees empty canvas.
    pub fn begin_annotation(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if self.multi_selection() {
            return;
        }
        let Some(selection) = self.committed_selection() else {
            return;
        };
        let local = local_point(selection.bounds(), position);
        self.annotations.pointer_down(local[0], local[1]);
        cx.notify();
    }

    pub fn drag_annotation(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(selection) = self.committed_selection() else {
            return;
        };
        if !self.annotations.is_dragging() {
            return;
        }
        let local = local_point(selection.bounds(), position);
        self.annotations.pointer_move(local[0], local[1]);
        cx.notify();
    }

    pub fn end_annotation(&mut self, cx: &mut Context<Self>) {
        if !self.annotations.is_dragging() && !self.annotations.is_typing() {
            return;
        }
        self.annotations.pointer_up();
        cx.notify();
    }

    pub fn annotation_insert(&mut self, text: &str, cx: &mut Context<Self>) {
        self.annotations.insert_text(text);
        cx.notify();
    }

    pub fn annotation_backspace(&mut self, cx: &mut Context<Self>) {
        self.annotations.backspace();
        cx.notify();
    }

    pub fn annotation_replace_text(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        cx: &mut Context<Self>,
    ) {
        self.annotations.replace_text(range, text);
        cx.notify();
    }

    pub fn annotation_replace_and_mark_text(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        cx: &mut Context<Self>,
    ) {
        self.annotations.replace_and_mark_text(range, text);
        cx.notify();
    }

    pub fn annotation_unmark_text(&mut self, cx: &mut Context<Self>) {
        self.annotations.unmark_text();
        cx.notify();
    }

    pub fn undo_annotations(&mut self, cx: &mut Context<Self>) {
        self.annotations.undo();
        cx.notify();
    }

    pub fn clear_annotations(&mut self, cx: &mut Context<Self>) {
        self.annotations.clear();
        cx.notify();
    }

    pub fn finish_annotations(&mut self) {
        self.annotations.commit();
    }

    pub fn cancel_annotation_draft(&mut self, cx: &mut Context<Self>) {
        self.annotations.cancel_draft();
        cx.notify();
    }

    pub fn request(&mut self, command: SessionCommand, cx: &mut Context<Self>) {
        cx.emit(command);
    }

    pub fn selected_frame(&self) -> Option<(&SessionFrame, Bounds<Pixels>)> {
        let selection = self.committed_selection()?;
        Some((
            self.frames.get(selection.frame_ix)?.as_ref()?,
            selection.bounds(),
        ))
    }

    fn frames_of<'a>(
        &'a self,
        selections: &[Selection],
    ) -> Vec<(&'a SessionFrame, Bounds<Pixels>)> {
        selections
            .iter()
            .filter_map(|selection| {
                Some((
                    self.frames.get(selection.frame_ix)?.as_ref()?,
                    selection.bounds(),
                ))
            })
            .collect()
    }

    /// The active region only, for the annotation canvas. A combined
    /// selection uses its already-composited image.
    pub fn crop_selection(&self) -> Option<RgbaImage> {
        if let Some(composed) = &self.composed {
            return Some(composed.image.clone());
        }
        let (frame, bounds) = self.selected_frame()?;
        frame.frame.shot.crop(frame.to_region(bounds))
    }

    /// What [`Self::crop_selection`] would return right now, without the
    /// pixels: a mosaic needs the source image on every painted frame, so the
    /// overlay keeps one crop per key instead of cutting a full-screen
    /// screenshot again (milliseconds) on each of them.
    pub fn crop_key(&self) -> Option<CropKey> {
        if let Some(composed) = &self.composed {
            return Some(CropKey::new(None, composed.bounds));
        }
        let selection = self.committed_selection()?;
        Some(CropKey::new(Some(selection.frame_ix), selection.bounds()))
    }

    /// Crop every committed region onto one image. Regions on the same display
    /// keep their on-screen positions over a transparent canvas; regions on
    /// different displays cannot share a coordinate space and are stitched
    /// vertically instead. The second value is the resulting logical size for
    /// scale and pin placement.
    pub fn crop_selections(&self) -> Option<(RgbaImage, Bounds<Pixels>)> {
        if let Some(composed) = &self.composed {
            return Some((composed.image.clone(), composed.bounds));
        }
        let regions = self.frames_of(&self.committed_selections());
        let (first_frame, first_bounds) = *regions.first()?;
        if regions.len() == 1 {
            return Some((
                first_frame
                    .frame
                    .shot
                    .crop(first_frame.to_region(first_bounds))?,
                first_bounds,
            ));
        }
        if regions
            .iter()
            .all(|(frame, _)| frame.monitor_id() == first_frame.monitor_id())
        {
            let scale = first_frame.physical_scale();
            let mut crops = Vec::with_capacity(regions.len());
            for (frame, bounds) in regions {
                crops.push((bounds, frame.frame.shot.crop(frame.to_region(bounds))?));
            }
            return Some(compose_at_positions(crops, scale));
        }
        let mut images = Vec::with_capacity(regions.len());
        let (mut width, mut height) = (0.0_f32, 0.0_f32);
        for (frame, bounds) in regions {
            images.push(frame.frame.shot.crop(frame.to_region(bounds))?);
            width = width.max(bounds.size.width.as_f32());
            height += bounds.size.height.as_f32();
        }
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(width), px(height)));
        Some((stitch_vertical(images), bounds))
    }

    /// Crop plus baked annotations (Copy / Save / Pin). A combined selection is
    /// annotated as a whole; a cross-display collection is copied unannotated.
    pub fn export_selections(&self) -> Option<(RgbaImage, Bounds<Pixels>)> {
        let (mut image, bounds) = self.crop_selections()?;
        if self.extra_selections.is_empty() {
            let scale = image.width() as f32 / bounds.size.width.as_f32().max(1.0);
            annotate::bake(&mut image, self.annotations.committed(), scale);
        }
        Some((image, bounds))
    }
}

/// Stack crops top to bottom on a transparent canvas wide enough for the widest.
fn stitch_vertical(images: Vec<RgbaImage>) -> RgbaImage {
    let width = images
        .iter()
        .map(RgbaImage::width)
        .max()
        .unwrap_or(1)
        .max(1);
    let height = images.iter().map(RgbaImage::height).sum::<u32>().max(1);
    let mut out = RgbaImage::from_pixel(width, height, image::Rgba([0, 0, 0, 0]));
    let mut y = 0;
    for image in &images {
        image::imageops::replace(&mut out, image, 0, i64::from(y));
        y += image.height();
    }
    out
}

/// Composite crops onto one transparent canvas at their original on-screen
/// positions, cropping the result to the regions' bounding box.
fn compose_at_positions(
    crops: Vec<(Bounds<Pixels>, RgbaImage)>,
    scale: f32,
) -> (RgbaImage, Bounds<Pixels>) {
    let scale = if scale > 0.0 { scale } else { 1.0 };
    let origin = crops
        .iter()
        .fold(None::<(f32, f32)>, |acc, (bounds, _)| {
            let x = bounds.origin.x.as_f32();
            let y = bounds.origin.y.as_f32();
            Some(acc.map_or((x, y), |(ox, oy)| (ox.min(x), oy.min(y))))
        })
        .unwrap_or((0.0, 0.0));
    let mut placed = Vec::with_capacity(crops.len());
    let (mut width, mut height) = (0_u32, 0_u32);
    for (bounds, image) in crops {
        let x = ((bounds.origin.x.as_f32() - origin.0) * scale)
            .round()
            .max(0.0) as u32;
        let y = ((bounds.origin.y.as_f32() - origin.1) * scale)
            .round()
            .max(0.0) as u32;
        width = width.max(x.saturating_add(image.width()));
        height = height.max(y.saturating_add(image.height()));
        placed.push((image, x, y));
    }
    let (width, height) = (width.max(1), height.max(1));
    let mut out = RgbaImage::from_pixel(width, height, image::Rgba([0, 0, 0, 0]));
    for (image, x, y) in placed {
        image::imageops::replace(&mut out, &image, i64::from(x), i64::from(y));
    }
    let bounds = Bounds::new(
        point(px(0.), px(0.)),
        size(px(width as f32 / scale), px(height as f32 / scale)),
    );
    (out, bounds)
}

fn local_point(bounds: Bounds<Pixels>, position: Point<Pixels>) -> [f32; 2] {
    [
        (position.x - bounds.origin.x)
            .as_f32()
            .clamp(0.0, bounds.size.width.as_f32()),
        (position.y - bounds.origin.y)
            .as_f32()
            .clamp(0.0, bounds.size.height.as_f32()),
    ]
}

#[cfg(test)]
mod tests {
    use super::{
        Hover, HoverElement, ResizeHandle, compose_at_positions, is_snappable, preferred_level,
        preserved_level, region_bounds, resized_bounds, rgba_to_bgra, sample_block,
        snap_window_region, stitch_vertical, swap_rgba_bgra_chunk, window_index_at,
        with_fullscreen_level,
    };
    use gpui_kit::{Bounds, Pixels, point, px, size};
    use snapit_capture::{ElementRole, Region, RgbaImage, WindowInfo};

    fn bounds() -> Bounds<Pixels> {
        Bounds::new(point(px(10.), px(20.)), size(px(100.), px(80.)))
    }

    #[test]
    fn tiny_elements_are_not_snappable() {
        let element = |width: f32, height: f32| {
            Bounds::new(point(px(0.), px(0.)), size(px(width), px(height)))
        };
        assert!(!is_snappable(&element(12.0, 12.0)));
        assert!(is_snappable(&element(16.0, 16.0)));
        assert!(!is_snappable(&element(15.9, 40.0)));
        assert!(!is_snappable(&element(40.0, 15.9)));
        assert!(is_snappable(&element(40.0, 24.0)));
    }

    #[test]
    fn preview_starts_at_the_first_control() {
        let element = |role| HoverElement {
            bounds: Bounds::new(point(px(0.), px(0.)), size(px(20.), px(20.))),
            role,
        };
        let chain = [
            element(ElementRole::Text),
            element(ElementRole::Container),
            element(ElementRole::Control),
            element(ElementRole::Container),
        ];
        assert_eq!(preferred_level(chain.iter()), 2);
        let no_control = [element(ElementRole::Text), element(ElementRole::Container)];
        assert_eq!(preferred_level(no_control.iter()), 0);
    }

    #[test]
    fn keeps_the_scrolled_level_across_chain_changes() {
        let rect = |x: f32, y: f32, width: f32, height: f32| {
            Bounds::new(point(px(x), px(y)), size(px(width), px(height)))
        };
        let window = rect(0., 0., 300., 200.);
        let levels = [rect(10., 10., 50., 20.), rect(5., 5., 200., 100.), window];
        // The pointer drifted, but the highlighted container is still in the
        // rebuilt chain, so the preview stays on it instead of jumping back.
        assert_eq!(
            preserved_level(&levels, Some(rect(5.2, 5., 199.8, 100.2)), 0),
            1
        );
        // A rectangle from another element falls back to the preferred level.
        assert_eq!(
            preserved_level(&levels, Some(rect(800., 800., 10., 10.)), 0),
            0
        );
        assert_eq!(preserved_level(&levels, None, 0), 0);
        // The whole window is always the topmost level.
        assert_eq!(preserved_level(&levels, Some(window), 0), 2);
    }

    #[test]
    fn wheel_chain_ends_at_the_whole_display() {
        let rect = |x: f32, y: f32, width: f32, height: f32| {
            Bounds::new(point(px(x), px(y)), size(px(width), px(height)))
        };
        let display = size(px(800.), px(600.));
        let window = rect(0., 0., 300., 200.);
        let levels = with_fullscreen_level(vec![rect(10., 10., 50., 20.), window], display);
        assert_eq!(levels.len(), 3);
        assert_eq!(levels[1], window, "window stays right below full screen");
        assert_eq!(levels[2], Bounds::new(point(px(0.), px(0.)), display));
        let outermost = Hover {
            frame_ix: 0,
            window_id: 1,
            levels: levels.clone(),
            level: levels.len() - 1,
        };
        assert_eq!(outermost.bounds(), levels[2]);
        // A window that already spans the display is not duplicated.
        let full = Bounds::new(point(px(0.), px(0.)), display);
        assert_eq!(with_fullscreen_level(vec![full], display).len(), 1);
    }

    #[test]
    fn stitched_regions_stack_without_overlap() {
        let red = RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]));
        let green = RgbaImage::from_pixel(1, 1, image::Rgba([0, 255, 0, 255]));
        let stitched = stitch_vertical(vec![red, green]);
        assert_eq!(stitched.dimensions(), (2, 3));
        assert_eq!(stitched.get_pixel(0, 0), &image::Rgba([255, 0, 0, 255]));
        assert_eq!(stitched.get_pixel(1, 1), &image::Rgba([255, 0, 0, 255]));
        assert_eq!(stitched.get_pixel(0, 2), &image::Rgba([0, 255, 0, 255]));
        // The shorter image leaves the remaining width transparent.
        assert_eq!(stitched.get_pixel(1, 2), &image::Rgba([0, 0, 0, 0]));
    }

    #[test]
    fn positioned_regions_keep_their_offsets() {
        let red = RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]));
        let green = RgbaImage::from_pixel(2, 2, image::Rgba([0, 255, 0, 255]));
        let crops = vec![
            (
                Bounds::new(point(px(10.), px(20.)), size(px(2.), px(2.))),
                red,
            ),
            (
                Bounds::new(point(px(15.), px(26.)), size(px(2.), px(2.))),
                green,
            ),
        ];
        let (image, bounds) = compose_at_positions(crops, 1.0);
        assert_eq!(image.dimensions(), (7, 8));
        assert_eq!(bounds.size.width.as_f32(), 7.0);
        assert_eq!(bounds.size.height.as_f32(), 8.0);
        assert_eq!(image.get_pixel(0, 0), &image::Rgba([255, 0, 0, 255]));
        assert_eq!(image.get_pixel(6, 7), &image::Rgba([0, 255, 0, 255]));
        // Between the regions the canvas stays transparent.
        assert_eq!(image.get_pixel(5, 0), &image::Rgba([0, 0, 0, 0]));
        assert_eq!(image.get_pixel(0, 3), &image::Rgba([0, 0, 0, 0]));
    }

    #[test]
    fn positioned_regions_scale_to_physical_pixels() {
        let red = RgbaImage::from_pixel(4, 4, image::Rgba([255, 0, 0, 255]));
        let green = RgbaImage::from_pixel(4, 4, image::Rgba([0, 255, 0, 255]));
        let crops = vec![
            (
                Bounds::new(point(px(10.), px(20.)), size(px(2.), px(2.))),
                red,
            ),
            (
                Bounds::new(point(px(15.), px(20.)), size(px(2.), px(2.))),
                green,
            ),
        ];
        let (image, bounds) = compose_at_positions(crops, 2.0);
        assert_eq!(image.dimensions(), (14, 4));
        assert_eq!(bounds.size.width.as_f32(), 7.0);
        assert_eq!(bounds.size.height.as_f32(), 2.0);
        assert_eq!(image.get_pixel(0, 0), &image::Rgba([255, 0, 0, 255]));
        assert_eq!(image.get_pixel(10, 0), &image::Rgba([0, 255, 0, 255]));
        assert_eq!(image.get_pixel(9, 0), &image::Rgba([0, 0, 0, 0]));
    }

    #[test]
    fn color_sample_clamps_at_the_edges() {
        let image = RgbaImage::from_raw(
            2,
            2,
            vec![
                255, 0, 0, 255, // red
                0, 255, 0, 255, // green
                0, 0, 255, 255, // blue
                255, 255, 255, 255, // white
            ],
        )
        .unwrap();
        let sample = sample_block(&image, 0, 0, 3);
        assert_eq!(sample.center, 0xFF0000);
        assert_eq!(sample.cells[2], 0x00FF00);
        assert_eq!(sample.cells[6], 0x0000FF);
        assert_eq!(sample.cells[8], 0xFFFFFF);
    }

    #[test]
    fn resize_handles_hit_corners_edges_and_ignore_the_interior() {
        assert_eq!(
            ResizeHandle::at(bounds(), point(px(10.), px(20.))),
            Some(ResizeHandle::NorthWest)
        );
        assert_eq!(
            ResizeHandle::at(bounds(), point(px(110.), px(100.))),
            Some(ResizeHandle::SouthEast)
        );
        assert_eq!(
            ResizeHandle::at(bounds(), point(px(60.), px(20.))),
            Some(ResizeHandle::North)
        );
        assert_eq!(
            ResizeHandle::at(bounds(), point(px(110.), px(60.))),
            Some(ResizeHandle::East)
        );
        assert_eq!(ResizeHandle::at(bounds(), point(px(60.), px(60.))), None);
        assert_eq!(ResizeHandle::at(bounds(), point(px(200.), px(60.))), None);
    }

    #[test]
    fn resize_clamps_to_display_and_keeps_minimum() {
        let limit = size(px(200.), px(150.));
        let grown = resized_bounds(
            bounds(),
            ResizeHandle::SouthEast,
            point(px(500.), px(500.)),
            limit,
        );
        assert_eq!(grown.origin.x.as_f32(), 10.0);
        assert_eq!(grown.origin.y.as_f32(), 20.0);
        assert_eq!(grown.size.width.as_f32(), 190.0);
        assert_eq!(grown.size.height.as_f32(), 130.0);

        let shrunk = resized_bounds(bounds(), ResizeHandle::West, point(px(500.), px(0.)), limit);
        assert_eq!(shrunk.size.width.as_f32(), 4.0);

        let expanded = resized_bounds(bounds(), ResizeHandle::North, point(px(0.), px(0.)), limit);
        assert_eq!(expanded.origin.y.as_f32(), 0.0);
        assert_eq!(expanded.size.height.as_f32(), 100.0);
    }

    fn window(region: Region) -> WindowInfo {
        WindowInfo::new(region, 1, 2)
    }

    fn window_of(region: Region, id: u64, pid: u32) -> WindowInfo {
        WindowInfo::new(region, id, pid)
    }

    #[test]
    fn fullscreen_app_windows_merge_into_one_snap_target() {
        let display = size(px(2056.), px(1329.));
        // Chrome fullscreen: tab strip, toolbar and page content are separate
        // platform windows of the same process, stacked without gaps.
        let windows = [
            window_of(Region::new(0.0, 43.0, 2056.0, 41.0), 2, 7),
            window_of(Region::new(0.0, 84.0, 2056.0, 81.0), 3, 7),
            window_of(Region::new(0.0, 165.0, 2056.0, 1164.0), 4, 7),
        ];
        let whole = Region::new(0.0, 43.0, 2056.0, 1286.0);
        // From the content area the wheel now reaches the whole browser.
        assert_eq!(snap_window_region(&windows, display, 2), whole);
        // Hovering the toolbar snaps to the same rectangle.
        assert_eq!(snap_window_region(&windows, display, 1), whole);

        let separate_process = [
            window_of(Region::new(0.0, 43.0, 2056.0, 41.0), 2, 7),
            window_of(Region::new(0.0, 84.0, 2056.0, 1245.0), 4, 8),
        ];
        assert_eq!(
            snap_window_region(&separate_process, display, 0),
            Region::new(0.0, 43.0, 2056.0, 41.0)
        );
    }

    #[test]
    fn overlapping_windows_of_one_app_stay_separate() {
        let display = size(px(1000.), px(800.));
        let windows = [
            window_of(Region::new(0.0, 0.0, 500.0, 400.0), 1, 9),
            window_of(Region::new(10.0, 10.0, 500.0, 400.0), 2, 9),
        ];
        assert_eq!(
            snap_window_region(&windows, display, 0),
            Region::new(0.0, 0.0, 500.0, 400.0)
        );
    }

    #[test]
    fn window_hit_test_picks_frontmost() {
        let windows = [
            window(Region::new(10.0, 10.0, 100.0, 100.0)),
            window(Region::new(50.0, 50.0, 100.0, 100.0)),
        ];
        assert_eq!(
            window_index_at(&windows, 1.0, point(px(60.0), px(60.0))),
            Some(0)
        );
        assert_eq!(
            window_index_at(&windows, 1.0, point(px(160.0), px(160.0))),
            None
        );
    }

    #[test]
    fn window_hit_test_scales_and_clamps_to_viewport() {
        let windows = [window(Region::new(100.0, 100.0, 400.0, 400.0))];
        assert_eq!(
            window_index_at(&windows, 2.0, point(px(60.0), px(60.0))),
            Some(0)
        );
        let bounds =
            region_bounds(windows[0].region, 0.0, 0.0, 2.0, size(px(150.0), px(150.0))).unwrap();
        assert_eq!(bounds.origin.x.as_f32(), 50.0);
        assert_eq!(bounds.size.width.as_f32(), 100.0);
        assert_eq!(bounds.size.height.as_f32(), 100.0);
    }

    #[test]
    fn element_bounds_subtract_the_monitor_origin() {
        let bounds = region_bounds(
            Region::new(1100.0, 250.0, 200.0, 100.0),
            1000.0,
            100.0,
            2.0,
            size(px(400.0), px(400.0)),
        )
        .unwrap();
        assert_eq!(bounds.origin.x.as_f32(), 50.0);
        assert_eq!(bounds.origin.y.as_f32(), 75.0);
        assert_eq!(bounds.size.width.as_f32(), 100.0);
        assert_eq!(bounds.size.height.as_f32(), 50.0);
    }

    #[test]
    fn rgba_to_bgra_swaps_red_and_blue() {
        let src = [10, 20, 30, 255, 1, 2, 3, 4];
        let mut dst = [0; 8];
        rgba_to_bgra(&src, &mut dst);
        assert_eq!(dst, [30, 20, 10, 255, 3, 2, 1, 4]);
    }

    #[test]
    fn swap_chunk_keeps_alpha_and_green() {
        let src = [9, 8, 7, 6];
        let mut dst = [0; 4];
        swap_rgba_bgra_chunk(&src, &mut dst);
        assert_eq!(dst, [7, 8, 9, 6]);
    }

    #[test]
    fn rgba_to_bgra_parallel_path_matches_serial() {
        let src: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
        let mut parallel = vec![0u8; src.len()];
        let mut serial = vec![0u8; src.len()];
        rgba_to_bgra(&src, &mut parallel);
        swap_rgba_bgra_chunk(&src, &mut serial);
        assert_eq!(parallel, serial);
    }
}
