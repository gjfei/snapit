//! Annotation tools on the frozen selection: live GPUI canvas + tiny-skia bake.
//!
//! Coordinates are selection-local logical pixels. `bake` maps them onto the
//! cropped physical bitmap (`scale` = physical px per logical px). Mosaic is a
//! real block average, not a blur.

use std::{
    ops::Range,
    sync::{Arc, OnceLock},
};

use gpui_kit::{
    App, Bounds, PathBuilder, Pixels, Point, SharedString, TextAlign, TextRun, Window, fill, font,
    point, px, rgb, size,
};
use snapit_capture::RgbaImage;
use snapit_store::{AnnotationConfig, AnnotationStyle, AnnotationTool};
use tiny_skia::{
    FillRule, IntSize, LineCap, LineJoin, Paint as SkiaPaint, PathBuilder as SkiaPath, Pixmap,
    Stroke, Transform,
};

/// 可选标注颜色（RGB）。
pub const COLORS: [u32; 6] = [0xEF4444, 0xF59E0B, 0x22C55E, 0x3B82F6, 0xFFFFFF, 0x1F2937];
const WHITE: [u8; 4] = [255, 255, 255, 255];
/// Arrow head length = base + stroke width × per-width. A purely linear rule
/// keeps the head growing at *every* slider step; the old `max(14, width * 2)`
/// left sizes 1–7 identical, which read as "the triangle never changes".
const ARROW_HEAD_BASE: f32 = 6.0;
const ARROW_HEAD_PER_WIDTH: f32 = 1.8;
const MIN_SPAN: f32 = 2.0;
/// Extra hit-test slack so thin strokes are still easy to pick.
const HIT_SLACK: f32 = 6.0;
/// Half-diagonal slack when grabbing a resize handle on a picked annotation.
const HANDLE_HIT: f32 = 7.0;
/// Side of the square handle drawn on a picked annotation.
const HANDLE_SIZE: f32 = 7.0;
/// Bounded snapshot history; each entry is one pre-edit copy of the shapes.
const HISTORY_LIMIT: usize = 64;

/// 5×7 digit bitmaps (rows; low 5 bits, MSB = left).
const DIGITS: [[u8; 7]; 10] = [
    [
        0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
    ],
    [
        0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
    ],
    [
        0b01110, 0b10001, 0b00001, 0b00110, 0b01000, 0b10000, 0b11111,
    ],
    [
        0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110,
    ],
    [
        0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
    ],
    [
        0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
    ],
    [
        0b01110, 0b10000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
    ],
    [
        0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
    ],
    [
        0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
    ],
    [
        0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00001, 0b01110,
    ],
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tool {
    #[default]
    Rect,
    Arrow,
    Line,
    Pen,
    Mosaic,
    Text,
    Step,
}

impl Tool {
    pub fn key(self) -> AnnotationTool {
        match self {
            Self::Rect => AnnotationTool::Rect,
            Self::Arrow => AnnotationTool::Arrow,
            Self::Line => AnnotationTool::Line,
            Self::Pen => AnnotationTool::Pen,
            Self::Mosaic => AnnotationTool::Mosaic,
            Self::Text => AnnotationTool::Text,
            Self::Step => AnnotationTool::Step,
        }
    }

    pub fn uses_color(self) -> bool {
        self.key().uses_color()
    }

    pub fn size_range(self) -> (u32, u32) {
        self.key().size_range()
    }

    /// Index into the per-tool slider states; matches `AnnotationTool::ALL`.
    pub fn style_index(self) -> usize {
        match self {
            Self::Rect => 0,
            Self::Arrow => 1,
            Self::Line => 2,
            Self::Pen => 3,
            Self::Mosaic => 4,
            Self::Text => 5,
            Self::Step => 6,
        }
    }
}

/// A per-shape snapshot. `size` is a line width for strokes, a font size for
/// text, a circle radius for steps and a block size for mosaic.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Style {
    pub color: u32,
    pub size: f32,
}

impl Style {
    fn from_config(style: AnnotationStyle) -> Self {
        Self {
            color: style.color,
            size: style.size as f32,
        }
    }
}

impl Default for Style {
    fn default() -> Self {
        Self::from_config(AnnotationTool::Rect.default_style())
    }
}

/// Grab points on a picked annotation. Rectangles and mosaic blocks expose the
/// eight box handles; arrows and lines expose their two endpoints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    NorthWest,
    North,
    NorthEast,
    East,
    SouthEast,
    South,
    SouthWest,
    West,
    Start,
    End,
}

/// The grab points of one shape, in a fixed buffer. Hit-testing and painting
/// ask for these on every pointer move and every frame, and a `Vec` there was
/// a heap allocation per call for at most eight entries.
pub struct Handles {
    items: [(Handle, [f32; 2]); 8],
    len: usize,
}

impl Handles {
    const fn new() -> Self {
        Self {
            items: [(Handle::NorthWest, [0.0, 0.0]); 8],
            len: 0,
        }
    }

    fn push(&mut self, handle: Handle, at: [f32; 2]) {
        if let Some(slot) = self.items.get_mut(self.len) {
            *slot = (handle, at);
            self.len += 1;
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = &(Handle, [f32; 2])> {
        self.items[..self.len].iter()
    }
}

impl IntoIterator for Handles {
    type Item = (Handle, [f32; 2]);
    type IntoIter = std::iter::Take<std::array::IntoIter<(Handle, [f32; 2]), 8>>;

    fn into_iter(self) -> Self::IntoIter {
        IntoIterator::into_iter(self.items).take(self.len)
    }
}

/// What a pointer press on an annotation landed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditStart {
    /// Nothing was hit; the pick was cleared.
    None,
    /// The picked shape now follows the pointer.
    Shape,
    /// A resize handle of the picked shape is being dragged.
    Handle,
}

#[derive(Clone, Debug)]
pub enum Shape {
    Rect {
        from: [f32; 2],
        to: [f32; 2],
        style: Style,
    },
    Arrow {
        from: [f32; 2],
        to: [f32; 2],
        style: Style,
    },
    Line {
        from: [f32; 2],
        to: [f32; 2],
        style: Style,
    },
    Pen {
        points: Vec<[f32; 2]>,
        style: Style,
    },
    Mosaic {
        from: [f32; 2],
        to: [f32; 2],
        style: Style,
    },
    Text {
        at: [f32; 2],
        text: String,
        style: Style,
    },
    Step {
        at: [f32; 2],
        number: u32,
        style: Style,
    },
}

impl Shape {
    fn is_meaningful(&self) -> bool {
        match self {
            Self::Rect { from, to, .. } | Self::Mosaic { from, to, .. } => {
                let (w, h) = span(*from, *to);
                w >= MIN_SPAN && h >= MIN_SPAN
            }
            Self::Arrow { from, to, .. } | Self::Line { from, to, .. } => {
                length(*from, *to) >= MIN_SPAN * 2.0
            }
            Self::Pen { points, .. } => !points.is_empty(),
            Self::Text { text, .. } => !text.is_empty(),
            Self::Step { .. } => true,
        }
    }

    fn tool(&self) -> Tool {
        match self {
            Self::Rect { .. } => Tool::Rect,
            Self::Arrow { .. } => Tool::Arrow,
            Self::Line { .. } => Tool::Line,
            Self::Pen { .. } => Tool::Pen,
            Self::Mosaic { .. } => Tool::Mosaic,
            Self::Text { .. } => Tool::Text,
            Self::Step { .. } => Tool::Step,
        }
    }

    fn style(&self) -> Style {
        match self {
            Self::Rect { style, .. }
            | Self::Arrow { style, .. }
            | Self::Line { style, .. }
            | Self::Pen { style, .. }
            | Self::Mosaic { style, .. }
            | Self::Text { style, .. }
            | Self::Step { style, .. } => *style,
        }
    }

    fn style_mut(&mut self) -> &mut Style {
        match self {
            Self::Rect { style, .. }
            | Self::Arrow { style, .. }
            | Self::Line { style, .. }
            | Self::Pen { style, .. }
            | Self::Mosaic { style, .. }
            | Self::Text { style, .. }
            | Self::Step { style, .. } => style,
        }
    }

    /// Painted extents without the selection padding; used to keep a dragged
    /// shape inside the captured region.
    fn extents(&self) -> (f32, f32, f32, f32) {
        match self {
            Self::Rect { from, to, .. }
            | Self::Arrow { from, to, .. }
            | Self::Line { from, to, .. }
            | Self::Mosaic { from, to, .. } => aabb(*from, *to),
            Self::Pen { points, .. } => {
                let (mut x0, mut y0, mut x1, mut y1) = match points.first() {
                    Some(p) => (p[0], p[1], p[0], p[1]),
                    None => return (0.0, 0.0, 0.0, 0.0),
                };
                for p in points {
                    x0 = x0.min(p[0]);
                    y0 = y0.min(p[1]);
                    x1 = x1.max(p[0]);
                    y1 = y1.max(p[1]);
                }
                (x0, y0, x1 - x0, y1 - y0)
            }
            Self::Text { at, text, style } => {
                let ems: f32 = text
                    .chars()
                    .map(|c| if c.is_ascii() { 0.6 } else { 1.0 })
                    .sum();
                (
                    at[0],
                    at[1],
                    (ems * style.size).max(style.size),
                    style.size * 1.3,
                )
            }
            Self::Step { at, style, .. } => (
                at[0] - style.size,
                at[1] - style.size,
                style.size * 2.0,
                style.size * 2.0,
            ),
        }
    }

    fn shift(&mut self, dx: f32, dy: f32) {
        match self {
            Self::Rect { from, to, .. }
            | Self::Arrow { from, to, .. }
            | Self::Line { from, to, .. }
            | Self::Mosaic { from, to, .. } => {
                from[0] += dx;
                from[1] += dy;
                to[0] += dx;
                to[1] += dy;
            }
            Self::Pen { points, .. } => {
                for p in points {
                    p[0] += dx;
                    p[1] += dy;
                }
            }
            Self::Text { at, .. } | Self::Step { at, .. } => {
                at[0] += dx;
                at[1] += dy;
            }
        }
    }

    /// Grab points this shape exposes; empty for pen strokes and the tools
    /// whose geometry is a single anchor (text, step).
    pub fn handles(&self) -> Handles {
        let mut handles = Handles::new();
        match self {
            Self::Rect { from, to, .. } | Self::Mosaic { from, to, .. } => {
                let (x, y, w, h) = aabb(*from, *to);
                let (cx, cy) = (x + w / 2.0, y + h / 2.0);
                let (rx, by) = (x + w, y + h);
                handles.push(Handle::NorthWest, [x, y]);
                handles.push(Handle::North, [cx, y]);
                handles.push(Handle::NorthEast, [rx, y]);
                handles.push(Handle::East, [rx, cy]);
                handles.push(Handle::SouthEast, [rx, by]);
                handles.push(Handle::South, [cx, by]);
                handles.push(Handle::SouthWest, [x, by]);
                handles.push(Handle::West, [x, cy]);
            }
            Self::Arrow { from, to, .. } | Self::Line { from, to, .. } => {
                handles.push(Handle::Start, *from);
                handles.push(Handle::End, *to);
            }
            _ => {}
        }
        handles
    }

    /// Whether a picked shape gets the blue bounding box. Tools whose own
    /// silhouette already reads as "selected" skip it: strokes and lines have
    /// grab handles on their corners/endpoints, a filled mosaic block is its
    /// own rectangle, and a numbered badge sits under the style panel. Pen
    /// strokes and text are irregular, so they keep the frame.
    fn shows_bounds_outline(&self) -> bool {
        matches!(self, Self::Pen { .. } | Self::Text { .. })
    }

    fn handle_at(&self, x: f32, y: f32) -> Option<Handle> {
        self.handles()
            .iter()
            .find(|(_, at)| length(*at, [x, y]) <= HANDLE_HIT)
            .map(|(handle, _)| *handle)
    }

    /// Move one handle to `(x, y)`. Box handles keep the opposite side fixed
    /// and stop at `MIN_SPAN`, so a rectangle never collapses into a line.
    fn drag_handle(&mut self, handle: Handle, x: f32, y: f32) {
        match self {
            Self::Rect { from, to, .. } | Self::Mosaic { from, to, .. } => {
                let (bx, by, w, h) = aabb(*from, *to);
                let (mut left, mut top, mut right, mut bottom) = (bx, by, bx + w, by + h);
                let min = MIN_SPAN;
                match handle {
                    Handle::NorthWest => {
                        left = x.min(right - min);
                        top = y.min(bottom - min);
                    }
                    Handle::North => top = y.min(bottom - min),
                    Handle::NorthEast => {
                        right = x.max(left + min);
                        top = y.min(bottom - min);
                    }
                    Handle::East => right = x.max(left + min),
                    Handle::SouthEast => {
                        right = x.max(left + min);
                        bottom = y.max(top + min);
                    }
                    Handle::South => bottom = y.max(top + min),
                    Handle::SouthWest => {
                        left = x.min(right - min);
                        bottom = y.max(top + min);
                    }
                    Handle::West => left = x.min(right - min),
                    Handle::Start | Handle::End => {}
                }
                *from = [left, top];
                *to = [right, bottom];
            }
            Self::Arrow { from, to, .. } | Self::Line { from, to, .. } => match handle {
                Handle::Start => *from = [x, y],
                Handle::End => *to = [x, y],
                _ => {}
            },
            _ => {}
        }
    }

    /// Selection-local bounding box, padded so the highlight is visible.
    fn bounds(&self) -> (f32, f32, f32, f32) {
        match self {
            Self::Rect {
                from, to, style, ..
            }
            | Self::Line {
                from, to, style, ..
            }
            | Self::Mosaic { from, to, style } => {
                let (x, y, w, h) = aabb(*from, *to);
                let pad = style.size.max(4.0) / 2.0 + 4.0;
                (x - pad, y - pad, w + pad * 2.0, h + pad * 2.0)
            }
            Self::Arrow {
                from, to, style, ..
            } => {
                let (x, y, w, h) = aabb(*from, *to);
                // The head's wings extend well past the stroke at large sizes.
                let pad = arrow_head(style.size) * 0.45 + style.size / 2.0 + 4.0;
                (x - pad, y - pad, w + pad * 2.0, h + pad * 2.0)
            }
            Self::Pen { points, style } => {
                let (mut x0, mut y0, mut x1, mut y1) = match points.first() {
                    Some(p) => (p[0], p[1], p[0], p[1]),
                    None => return (0.0, 0.0, 0.0, 0.0),
                };
                for p in points {
                    x0 = x0.min(p[0]);
                    y0 = y0.min(p[1]);
                    x1 = x1.max(p[0]);
                    y1 = y1.max(p[1]);
                }
                let pad = style.size / 2.0 + 4.0;
                (x0 - pad, y0 - pad, x1 - x0 + pad * 2.0, y1 - y0 + pad * 2.0)
            }
            Self::Text { at, text, style } => {
                // Full-width (CJK) glyphs take about one em, ASCII about 0.6.
                let ems: f32 = text
                    .chars()
                    .map(|c| if c.is_ascii() { 0.6 } else { 1.0 })
                    .sum();
                let width = ems * style.size;
                (
                    at[0] - 4.0,
                    at[1] - 4.0,
                    width.max(style.size) + 8.0,
                    style.size * 1.3 + 8.0,
                )
            }
            Self::Step { at, style, .. } => {
                let r = style.size;
                (
                    at[0] - r - 4.0,
                    at[1] - r - 4.0,
                    r * 2.0 + 8.0,
                    r * 2.0 + 8.0,
                )
            }
        }
    }

    fn hits(&self, x: f32, y: f32) -> bool {
        match self {
            Self::Rect { from, to, style } => {
                // A rect is a hollow outline: only its four edges are pickable,
                // so clicking the empty middle deselects instead.
                let (bx, by, w, h) = aabb(*from, *to);
                let slack = (style.size / 2.0 + HIT_SLACK).max(8.0);
                let corners = [[bx, by], [bx + w, by], [bx + w, by + h], [bx, by + h]];
                corners.iter().enumerate().any(|(ix, start)| {
                    point_to_segment(x, y, *start, corners[(ix + 1) % 4]) <= slack
                })
            }
            Self::Mosaic { from, to, style } => {
                let (bx, by, w, h) = aabb(*from, *to);
                let slack = (style.size / 2.0 + HIT_SLACK).max(8.0);
                (bx - slack..=bx + w + slack).contains(&x)
                    && (by - slack..=by + h + slack).contains(&y)
            }
            Self::Arrow { from, to, style } => {
                point_to_segment(x, y, *from, *to) <= style.size / 2.0 + HIT_SLACK
                    || arrow_geometry(*from, *to, arrow_head(style.size))
                        .is_some_and(|(_, head)| point_in_triangle([x, y], *to, head[0], head[1]))
            }
            Self::Line { from, to, style } => {
                point_to_segment(x, y, *from, *to) <= style.size / 2.0 + HIT_SLACK
            }
            Self::Pen { points, style } => {
                let slack = style.size / 2.0 + HIT_SLACK;
                points
                    .windows(2)
                    .any(|pair| point_to_segment(x, y, pair[0], pair[1]) <= slack)
                    || points.first().is_some_and(|p| length(*p, [x, y]) <= slack)
            }
            Self::Text { .. } | Self::Step { .. } => {
                let (bx, by, w, h) = self.bounds();
                (bx..=bx + w).contains(&x) && (by..=by + h).contains(&y)
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Layer {
    tool: Tool,
    /// Copy-on-write: the canvas clones the whole layer on every painted frame
    /// and the undo history holds one reference per edit, so both are refcount
    /// bumps here and pay for a copy only when a shape actually changes.
    shapes: Arc<Vec<Shape>>,
    draft: Option<Shape>,
    typing: bool,
    /// IME 预编辑区，draft 文本中的字节范围。
    marked: Option<Range<usize>>,
    /// Remembered defaults for the next annotation of each tool.
    defaults: AnnotationConfig,
    /// A picked shape; color and size edits then apply to it alone.
    selected: Option<usize>,
    /// The text annotation being re-edited. It stays in `shapes` as the
    /// pre-edit copy but is hidden from painting until the draft is committed.
    editing: Option<usize>,
    /// In-flight move/resize of the picked shape.
    shift: Option<ShapeShift>,
    /// Pre-edit copies of `shapes`, newest last, so every geometry and text
    /// edit can be undone instead of only the last new annotation.
    history: Vec<Arc<Vec<Shape>>>,
    /// The shape whose style is being edited right now. A palette click or a
    /// slider drag emits one change per pixel, and each of those must not cost
    /// its own undo step: the run shares the snapshot taken by its first edit.
    style_run: Option<usize>,
}

/// Absolute drag state: every move rebuilds the shape from `original`, so the
/// result never drifts and clamps cleanly against the captured region.
#[derive(Clone, Debug)]
struct ShapeShift {
    index: usize,
    handle: Option<Handle>,
    origin: [f32; 2],
    original: Shape,
    /// Offset applied by the previous move. A pointer pushed past the region's
    /// edge keeps producing the same clamped offset, which must not repaint.
    applied: [f32; 2],
    moved: bool,
}

impl Layer {
    pub fn new(defaults: AnnotationConfig) -> Self {
        Self {
            defaults,
            ..Self::default()
        }
    }

    pub fn tool(&self) -> Tool {
        self.tool
    }

    /// The tool whose style the toolbar currently edits: the picked shape's
    /// tool when one is selected, otherwise the active drawing tool.
    pub fn active_tool(&self) -> Tool {
        self.selected
            .and_then(|ix| self.shapes.get(ix))
            .map_or(self.tool, Shape::tool)
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// Drop the pick. Clicking a tool button edits that tool's defaults, so
    /// the panel must not keep styling the shape that was picked before.
    pub fn deselect(&mut self) -> bool {
        self.style_run = None;
        self.selected.take().is_some()
    }

    /// Whether an annotation is picked. Picking is independent of the armed
    /// drawing tool: the outline and the grab handles follow the pick alone.
    fn shows_pick(&self) -> bool {
        self.selected.is_some()
    }

    /// Selection-local bounds of the picked shape, used to anchor its style
    /// panel right above or below it.
    pub fn selected_bounds(&self) -> Option<(f32, f32, f32, f32)> {
        self.selected
            .and_then(|ix| self.shapes.get(ix))
            .map(Shape::bounds)
    }

    pub fn style(&self) -> Style {
        self.selected
            .and_then(|ix| self.shapes.get(ix))
            .map_or_else(|| self.default_style(self.tool), Shape::style)
    }

    pub fn config(&self) -> &AnnotationConfig {
        &self.defaults
    }

    fn default_style(&self, tool: Tool) -> Style {
        Style::from_config(self.defaults.get(tool.key()))
    }

    /// Restyle the picked shape, or change the remembered default for the
    /// active tool. Existing shapes keep the style they were created with.
    pub fn set_color(&mut self, color: u32) {
        if !self.active_tool().uses_color() {
            return;
        }
        let color = color & 0x00FF_FFFF;
        if self.selected.is_some() {
            if self.style().color == color {
                return;
            }
            self.push_style_undo();
            if let Some(shape) = self.selected_mut() {
                shape.style_mut().color = color;
            }
        } else {
            let key = self.tool.key();
            let mut style = self.defaults.get(key);
            style.color = color;
            self.defaults.set(key, style);
        }
    }

    pub fn set_size(&mut self, size: f32) {
        let (min, max) = self.size_range();
        let clamped = size.round().clamp(min as f32, max as f32);
        if self.selected.is_some() {
            if self.style().size == clamped {
                return;
            }
            self.push_style_undo();
            if let Some(shape) = self.selected_mut() {
                shape.style_mut().size = clamped;
            }
        } else {
            let key = self.tool.key();
            let mut style = self.defaults.get(key);
            style.size = clamped as u32;
            self.defaults.set(key, style);
        }
    }

    /// One undo step per style gesture. A palette click and every tick of a
    /// slider drag arrive as separate changes; without this, undoing a restyle
    /// would walk back through the drag a pixel at a time, and the history
    /// would be full of steps that only differ in colour.
    fn push_style_undo(&mut self) {
        let Some(ix) = self.selected else {
            return;
        };
        if self.style_run == Some(ix) {
            return;
        }
        self.push_history();
        self.style_run = Some(ix);
    }

    pub fn size_range(&self) -> (u32, u32) {
        self.active_tool().size_range()
    }

    fn selected_mut(&mut self) -> Option<&mut Shape> {
        let ix = self.selected?;
        Arc::make_mut(&mut self.shapes).get_mut(ix)
    }

    /// Whether an annotation sits under the point, without changing the pick.
    pub fn hits_at(&self, x: f32, y: f32) -> bool {
        self.shapes.iter().rev().any(|shape| shape.hits(x, y))
    }

    /// Pick the topmost annotation under the cursor. Returns whether one was hit.
    pub fn select_at(&mut self, x: f32, y: f32) -> bool {
        let picked = self
            .shapes
            .iter()
            .enumerate()
            .rev()
            .find(|(_, shape)| shape.hits(x, y))
            .map(|(ix, _)| ix);
        if picked != self.selected {
            // A style run's snapshot belongs to the shape it started on.
            self.style_run = None;
        }
        self.selected = picked;
        self.selected.is_some()
    }

    pub fn delete_selected(&mut self) -> bool {
        let Some(ix) = self.selected.take() else {
            return false;
        };
        if ix < self.shapes.len() {
            self.snapshot();
            Arc::make_mut(&mut self.shapes).remove(ix);
            self.editing = None;
            // A press that deleted mid-drag must not keep dragging the slot
            // this shape used to occupy, which now holds its neighbour.
            self.shift = None;
            return true;
        }
        false
    }

    /// Remember the current shapes so the next mutation can be undone.
    fn snapshot(&mut self) {
        self.push_history();
        // Anything else ends a style run: the next style edit is a new step.
        self.style_run = None;
    }

    fn push_history(&mut self) {
        if self.history.len() >= HISTORY_LIMIT {
            self.history.remove(0);
        }
        self.history.push(Arc::clone(&self.shapes));
    }

    fn discard_snapshot(&mut self) {
        self.history.pop();
    }

    pub fn is_editing(&self) -> bool {
        self.shift.is_some()
    }

    /// Handles drawn on the picked shape (live preview only).
    pub fn selected_handles(&self) -> Handles {
        self.selected
            .and_then(|ix| self.shapes.get(ix))
            .map(Shape::handles)
            .unwrap_or_else(Handles::new)
    }

    /// Start moving the picked shape, or the shape under the pointer. Handles
    /// take priority so a corner can be grabbed without reselecting.
    pub fn begin_edit(&mut self, x: f32, y: f32) -> EditStart {
        // A lost mouse-up would otherwise leave a stale drag behind.
        self.end_edit();
        self.commit();
        // A handle of the picked shape wins: its corner sticks out past the
        // shape and is often the only thing under the pointer.
        if let Some(ix) = self.selected
            && let Some(handle) = self.shapes.get(ix).and_then(|shape| shape.handle_at(x, y))
        {
            let original = self.shapes[ix].clone();
            self.snapshot();
            self.shift = Some(ShapeShift {
                index: ix,
                handle: Some(handle),
                origin: [x, y],
                original,
                applied: [0.0, 0.0],
                moved: false,
            });
            return EditStart::Handle;
        }
        // Otherwise take the topmost shape under the pointer, which is the one
        // the user sees. Preferring the picked shape here meant an overlap
        // dragged whichever was selected, even when another sat on top of it.
        if !self.select_at(x, y) {
            return EditStart::None;
        }
        let ix = self.selected.expect("select_at just set the pick");
        let original = self.shapes[ix].clone();
        self.snapshot();
        self.shift = Some(ShapeShift {
            index: ix,
            handle: None,
            origin: [x, y],
            original,
            applied: [0.0, 0.0],
            moved: false,
        });
        EditStart::Shape
    }

    /// Follow the pointer. `viewport` is the captured region in the same
    /// local coordinates, so a shape can never be dragged out of the image.
    /// Returns whether the shape actually moved: a pointer clamped against the
    /// edge must not repaint the canvas for nothing.
    pub fn drag_edit(&mut self, x: f32, y: f32, viewport: [f32; 2]) -> bool {
        let Some(shift) = self.shift.as_mut() else {
            return false;
        };
        let index = shift.index;
        if index >= self.shapes.len() {
            return false;
        }
        match shift.handle {
            Some(handle) => {
                let x = x.clamp(0.0, viewport[0]);
                let y = y.clamp(0.0, viewport[1]);
                let mut shape = shift.original.clone();
                shape.drag_handle(handle, x, y);
                Arc::make_mut(&mut self.shapes)[index] = shape;
                shift.moved = true;
                true
            }
            None => {
                let (ex, ey, ew, eh) = shift.original.extents();
                let dx = clamp_shift(x - shift.origin[0], ex, viewport[0] - ew);
                let dy = clamp_shift(y - shift.origin[1], ey, viewport[1] - eh);
                // The clamp pins the shape at the edge: past that point every
                // pointer move yields the offset already applied.
                if dx == shift.applied[0] && dy == shift.applied[1] {
                    return false;
                }
                shift.applied = [dx, dy];
                rebuild_shifted(
                    &mut Arc::make_mut(&mut self.shapes)[index],
                    &shift.original,
                    dx,
                    dy,
                );
                shift.moved = true;
                true
            }
        }
    }

    pub fn end_edit(&mut self) -> bool {
        let Some(shift) = self.shift.take() else {
            return false;
        };
        if !shift.moved {
            // A plain click on the picked shape must not add a history entry.
            self.discard_snapshot();
        }
        shift.moved
    }

    /// Double-clicking a text annotation reopens it in place. The original
    /// stays in `shapes` (hidden while typing) so cancelling restores it.
    pub fn begin_text_edit(&mut self, x: f32, y: f32) -> bool {
        if self.typing {
            // The first click of a double click may already have opened an
            // empty draft on top of the text the user wants to edit.
            let empty = matches!(
                self.draft,
                Some(Shape::Text { ref text, .. }) if text.is_empty()
            );
            if !empty {
                return false;
            }
            self.cancel_draft();
        }
        self.end_edit();
        self.commit();
        let Some(ix) = self
            .shapes
            .iter()
            .enumerate()
            .rev()
            .find(|(_, shape)| shape.hits(x, y))
            .map(|(ix, _)| ix)
        else {
            return false;
        };
        if !matches!(self.shapes[ix], Shape::Text { .. }) {
            return false;
        }
        self.snapshot();
        self.draft = Some(self.shapes[ix].clone());
        self.editing = Some(ix);
        self.selected = None;
        self.shift = None;
        self.typing = true;
        self.marked = None;
        true
    }

    pub fn set_tool(&mut self, tool: Tool) {
        if self.tool == tool {
            return;
        }
        self.end_edit();
        self.commit();
        self.selected = None;
        self.tool = tool;
    }

    pub fn is_typing(&self) -> bool {
        self.typing
    }

    pub fn is_dragging(&self) -> bool {
        !self.typing
            && matches!(
                self.draft,
                Some(
                    Shape::Rect { .. }
                        | Shape::Arrow { .. }
                        | Shape::Line { .. }
                        | Shape::Pen { .. }
                        | Shape::Mosaic { .. }
                )
            )
    }

    pub fn can_undo(&self) -> bool {
        self.draft.is_some() || !self.shapes.is_empty() || !self.history.is_empty()
    }

    pub fn committed(&self) -> &[Shape] {
        &self.shapes
    }

    pub fn pointer_down(&mut self, x: f32, y: f32) {
        self.end_edit();
        self.commit();
        let p = [x, y];
        self.selected = None;
        let style = self.default_style(self.tool);
        match self.tool {
            Tool::Rect => {
                self.draft = Some(Shape::Rect {
                    from: p,
                    to: p,
                    style,
                })
            }
            Tool::Arrow => {
                self.draft = Some(Shape::Arrow {
                    from: p,
                    to: p,
                    style,
                })
            }
            Tool::Line => {
                self.draft = Some(Shape::Line {
                    from: p,
                    to: p,
                    style,
                })
            }
            Tool::Pen => {
                self.draft = Some(Shape::Pen {
                    points: vec![p],
                    style,
                })
            }
            Tool::Mosaic => {
                self.draft = Some(Shape::Mosaic {
                    from: p,
                    to: p,
                    style,
                })
            }
            Tool::Text => {
                self.draft = Some(Shape::Text {
                    at: p,
                    text: String::new(),
                    style,
                });
                self.typing = true;
                self.marked = None;
            }
            Tool::Step => {
                let number = self.next_step();
                self.snapshot();
                Arc::make_mut(&mut self.shapes).push(Shape::Step {
                    at: p,
                    number,
                    style,
                });
                // A stamp is finished the moment it lands: pick it like the
                // drag-drawn shapes so it can be restyled or deleted at once.
                self.selected = Some(self.shapes.len() - 1);
            }
        }
    }

    pub fn pointer_move(&mut self, x: f32, y: f32) {
        match self.draft.as_mut() {
            Some(
                Shape::Rect { to, .. }
                | Shape::Arrow { to, .. }
                | Shape::Line { to, .. }
                | Shape::Mosaic { to, .. },
            ) => {
                *to = [x, y];
            }
            Some(Shape::Pen { points, .. }) => {
                let p = [x, y];
                if points.last().is_none_or(|last| length(*last, p) >= 1.0) {
                    points.push(p);
                }
            }
            _ => {}
        }
    }

    pub fn pointer_up(&mut self) {
        if !self.typing {
            self.commit();
        }
    }

    pub fn insert_text(&mut self, text: &str) {
        if !self.typing {
            return;
        }
        if let Some(Shape::Text { text: buffer, .. }) = self.draft.as_mut() {
            buffer.push_str(text);
            self.marked = None;
        }
    }

    pub fn backspace(&mut self) {
        if !self.typing {
            return;
        }
        if let Some(Shape::Text { text, .. }) = self.draft.as_mut() {
            text.pop();
            self.marked = None;
        }
    }

    /// 正在输入的文本草稿。
    pub fn draft_text(&self) -> Option<&str> {
        if !self.typing {
            return None;
        }
        match self.draft.as_ref()? {
            Shape::Text { text, .. } => Some(text),
            _ => None,
        }
    }

    /// Style snapshot of the in-progress draft, for the IME caret.
    pub fn draft_style(&self) -> Option<Style> {
        self.draft.as_ref().map(Shape::style)
    }

    /// 正在输入的文本草稿的插入点，选区局部坐标。
    pub fn draft_origin(&self) -> Option<[f32; 2]> {
        if !self.typing {
            return None;
        }
        match self.draft.as_ref()? {
            Shape::Text { at, .. } => Some(*at),
            _ => None,
        }
    }

    pub fn text_length_utf16(&self) -> Option<usize> {
        Some(self.draft_text()?.encode_utf16().count())
    }

    /// 取 UTF-16 范围的文本，并返回按字符边界调整后的范围。
    pub fn text_for_range(&self, range: Range<usize>) -> Option<(String, Range<usize>)> {
        let text = self.draft_text()?;
        let range = utf16_range_to_byte(text, range);
        Some((
            text[range.clone()].to_string(),
            byte_to_utf16(text, range.start)..byte_to_utf16(text, range.end),
        ))
    }

    pub fn marked_text_range(&self) -> Option<Range<usize>> {
        let text = self.draft_text()?;
        let marked = self.marked.as_ref()?;
        Some(byte_to_utf16(text, marked.start)..byte_to_utf16(text, marked.end))
    }

    /// 输入法提交或直接输入：替换 `range`（缺省为预编辑区，再缺省为末尾）。
    pub fn replace_text(&mut self, range: Option<Range<usize>>, new_text: &str) {
        self.splice_text(range, new_text, false);
    }

    /// 输入法组合中：替换预编辑区并标记新文本。
    pub fn replace_and_mark_text(&mut self, range: Option<Range<usize>>, new_text: &str) {
        self.splice_text(range, new_text, true);
    }

    pub fn unmark_text(&mut self) {
        self.marked = None;
    }

    fn splice_text(&mut self, range: Option<Range<usize>>, new_text: &str, mark: bool) {
        if !self.typing {
            return;
        }
        let marked = self.marked.clone();
        let Some(Shape::Text { text, .. }) = self.draft.as_mut() else {
            return;
        };
        let range = range
            .map(|range| utf16_range_to_byte(text, range))
            .or_else(|| marked.map(|range| range.start.min(text.len())..range.end.min(text.len())))
            .unwrap_or(text.len()..text.len());
        text.replace_range(range.clone(), new_text);
        self.marked = mark.then(|| range.start..range.start + new_text.len());
    }

    pub fn undo(&mut self) {
        self.selected = None;
        if let Some(shift) = self.shift.take()
            && !shift.moved
        {
            self.discard_snapshot();
        }
        if self.draft.take().is_some() {
            self.typing = false;
            self.marked = None;
            // An in-place text edit never mutated `shapes`; its snapshot is
            // still untouched, so drop it instead of leaving a no-op step.
            if self.editing.take().is_some() {
                self.discard_snapshot();
            }
            return;
        }
        if let Some(previous) = self.history.pop() {
            self.shapes = previous;
        } else {
            Arc::make_mut(&mut self.shapes).pop();
        }
        self.editing = None;
        // The run's snapshot was just consumed, so the next style edit needs
        // a fresh one or it could never be undone.
        self.style_run = None;
    }

    pub fn clear(&mut self) {
        self.shapes = Arc::default();
        self.draft = None;
        self.typing = false;
        self.marked = None;
        self.selected = None;
        self.editing = None;
        self.shift = None;
        self.history.clear();
        self.style_run = None;
    }

    /// Shift every stroke so annotations stay anchored to the frozen screen
    /// when the selection's top-left corner moves.
    pub fn translate(&mut self, dx: f32, dy: f32) {
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        for shape in Arc::make_mut(&mut self.shapes).iter_mut() {
            shape.shift(dx, dy);
        }
        if let Some(draft) = self.draft.as_mut() {
            draft.shift(dx, dy);
        }
        // Keep an in-flight drag anchored to the same screen pixels.
        if let Some(shift) = self.shift.as_mut() {
            shift.original.shift(dx, dy);
            shift.origin[0] += dx;
            shift.origin[1] += dy;
        }
        // Every snapshot still holds the old coordinates, so a style run that
        // started before the move must not reuse one.
        self.style_run = None;
    }

    pub fn commit(&mut self) {
        self.typing = false;
        self.marked = None;
        let Some(shape) = self.draft.take() else {
            return;
        };
        if let Some(ix) = self.editing.take() {
            if ix < self.shapes.len() {
                if shape.is_meaningful() {
                    Arc::make_mut(&mut self.shapes)[ix] = shape;
                    // The shape just edited stays picked, matching a new one.
                    self.selected = Some(ix);
                } else {
                    // Emptying a text annotation removes it.
                    Arc::make_mut(&mut self.shapes).remove(ix);
                    self.selected = None;
                }
            }
            return;
        }
        if shape.is_meaningful() {
            self.snapshot();
            Arc::make_mut(&mut self.shapes).push(shape);
            // The last annotation is picked as soon as it is finished, so its
            // style can be tweaked or it can be deleted right away.
            self.selected = Some(self.shapes.len() - 1);
        }
    }

    pub fn cancel_draft(&mut self) {
        self.draft = None;
        self.typing = false;
        self.marked = None;
        if self.editing.take().is_some() {
            self.discard_snapshot();
            return;
        }
        self.shift = None;
    }

    fn next_step(&self) -> u32 {
        self.shapes
            .iter()
            .filter_map(|shape| match shape {
                Shape::Step { number, .. } => Some(*number),
                _ => None,
            })
            .max()
            .unwrap_or(0)
            + 1
    }

    fn visible(&self) -> impl Iterator<Item = &Shape> {
        self.shapes
            .iter()
            .enumerate()
            .filter(|(ix, _)| Some(*ix) != self.editing)
            .map(|(_, shape)| shape)
            .chain(self.draft.as_ref())
    }

    pub(crate) fn needs_source(&self) -> bool {
        self.visible()
            .any(|shape| matches!(shape, Shape::Mosaic { .. }))
    }
}

fn utf16_to_byte(text: &str, offset: usize) -> usize {
    let mut used = 0;
    for (ix, ch) in text.char_indices() {
        if used >= offset {
            return ix;
        }
        used += ch.len_utf16();
    }
    text.len()
}

fn byte_to_utf16(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())].encode_utf16().count()
}

fn utf16_range_to_byte(text: &str, range: Range<usize>) -> Range<usize> {
    let start = utf16_to_byte(text, range.start);
    start..utf16_to_byte(text, range.end).max(start)
}

/// A lazy annotation bake shared by history, clipboard and save workers.
pub struct BakedImage {
    source: RgbaImage,
    shapes: Arc<Vec<Shape>>,
    scale: f32,
    result: OnceLock<Arc<RgbaImage>>,
}

impl BakedImage {
    pub fn new(source: RgbaImage, shapes: Arc<Vec<Shape>>, scale: f32) -> Self {
        Self {
            source,
            shapes,
            scale,
            result: OnceLock::new(),
        }
    }

    pub fn get(&self) -> Arc<RgbaImage> {
        self.result
            .get_or_init(|| {
                let mut image = self.source.clone();
                bake(&mut image, &self.shapes, self.scale);
                Arc::new(image)
            })
            .clone()
    }
}

/// Rasterize committed shapes onto a cropped RGBA bitmap.
pub fn bake(image: &mut RgbaImage, shapes: &[Shape], scale: f32) {
    if shapes.is_empty() {
        return;
    }
    let scale = scale.max(0.01);
    let Some(mut pixmap) = pixmap_from_rgba(image) else {
        return;
    };
    for shape in shapes {
        match shape {
            Shape::Mosaic { from, to, style } => {
                let (x, y, w, h) = scaled_aabb(*from, *to, scale);
                let block = (style.size * scale).round().max(2.0) as u32;
                let width = pixmap.width();
                let height = pixmap.height();
                pixelate(pixmap.data_mut(), width, height, (x, y, w, h), block);
            }
            Shape::Rect { from, to, style } => stroke_rect(&mut pixmap, *from, *to, scale, *style),
            Shape::Arrow { from, to, style } => {
                stroke_arrow(&mut pixmap, *from, *to, scale, *style)
            }
            // A straight line is a two-point pen stroke: same round caps and
            // width handling in both the baked and the live renderers.
            Shape::Line { from, to, style } => {
                stroke_pen(&mut pixmap, &[*from, *to], scale, *style)
            }
            Shape::Pen { points, style } => stroke_pen(&mut pixmap, points, scale, *style),
            Shape::Text { at, text, style } => {
                draw_text(&mut pixmap, *at, text, scale, style.size, rgba(style.color))
            }
            Shape::Step { at, number, style } => {
                draw_step(&mut pixmap, *at, *number, scale, style.size, style.color)
            }
        }
    }
    if let Some(baked) = rgba_from_pixmap(pixmap) {
        *image = baked;
    }
}

/// The region pixels a mosaic samples, plus a stamp that changes whenever
/// those pixels do. The block cache keys on the stamp so a new region can
/// never inherit the previous frame's colours.
#[derive(Clone, Copy)]
pub(crate) struct MosaicSource<'a> {
    pub image: &'a RgbaImage,
    pub stamp: u64,
}

/// One block of a mosaic and the colour sampled for it.
struct MosaicBlock {
    /// Local to the selection, like the shape it belongs to.
    at: [f32; 2],
    size: [f32; 2],
    color: [u8; 4],
}

/// What a set of block colours was derived from.
#[derive(Clone, Copy, PartialEq, Eq)]
struct MosaicKey {
    index: usize,
    rect: [i64; 4],
    block: i64,
    scale: i64,
    stamp: u64,
}

impl MosaicKey {
    fn new(index: usize, from: [f32; 2], to: [f32; 2], block: f32, scale: f32, stamp: u64) -> Self {
        let (x, y, w, h) = aabb(from, to);
        let quantise = |value: f32| (f64::from(value) * 1000.0).round() as i64;
        Self {
            index,
            rect: [quantise(x), quantise(y), quantise(w), quantise(h)],
            block: quantise(block),
            scale: quantise(scale),
            stamp,
        }
    }
}

/// Block colours kept between painted frames. Averaging every block of a large
/// mosaic reads the whole region once per frame, and the canvas repaints on
/// every pointer move while the mosaic is dragged; the colours only change
/// when its rectangle, its block size or the source pixels do.
#[derive(Default)]
pub(crate) struct MosaicCache {
    entries: Vec<(MosaicKey, Vec<MosaicBlock>)>,
}

impl MosaicCache {
    /// Enough for the mosaic being dragged plus its neighbours.
    const KEEP: usize = 4;

    fn blocks(
        &mut self,
        key: MosaicKey,
        compute: impl FnOnce() -> Vec<MosaicBlock>,
    ) -> &[MosaicBlock] {
        if let Some(ix) = self.entries.iter().position(|(hit, _)| *hit == key) {
            // Refresh the hit so the least recently painted one is evicted.
            let entry = self.entries.remove(ix);
            self.entries.push(entry);
        } else {
            if self.entries.len() >= Self::KEEP {
                self.entries.remove(0);
            }
            self.entries.push((key, compute()));
        }
        &self.entries.last().expect("an entry was just pushed").1
    }

    /// Nothing on screen needs the source pixels any more, so the sampled
    /// colours can go too: a large mosaic holds a few megabytes of them.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
}

pub(crate) fn paint_layer(
    layer: &Layer,
    origin: Point<Pixels>,
    scale: f32,
    source: Option<MosaicSource<'_>>,
    mosaics: &mut MosaicCache,
    window: &mut Window,
    cx: &mut App,
) {
    let typing = layer.is_typing();
    // Only the draft can be last, and only a draft ever gets a caret; skip
    // counting the shapes when nothing is being typed.
    let last = if typing {
        layer.visible().count().saturating_sub(1)
    } else {
        usize::MAX
    };
    for (ix, shape) in layer.visible().enumerate() {
        match shape {
            Shape::Rect { from, to, style } => {
                if let Some(bounds) = gpui_aabb(origin, *from, *to) {
                    stroke_gpui_rect(window, bounds, px(style.size), rgb(style.color));
                }
            }
            Shape::Arrow { from, to, style } => {
                paint_gpui_arrow(window, origin, *from, *to, *style);
            }
            Shape::Line { from, to, style } => paint_gpui_pen(
                window,
                origin,
                &[*from, *to],
                px(style.size),
                rgb(style.color),
            ),
            Shape::Pen { points, style } => {
                paint_gpui_pen(window, origin, points, px(style.size), rgb(style.color))
            }
            Shape::Mosaic { from, to, style } => {
                if let Some(source) = source {
                    let key = MosaicKey::new(ix, *from, *to, style.size, scale, source.stamp);
                    let blocks = mosaics.blocks(key, || {
                        mosaic_blocks(*from, *to, style.size, scale, source.image)
                    });
                    paint_gpui_mosaic(window, origin, blocks);
                }
            }
            Shape::Text { at, text, style } => {
                paint_gpui_text(window, cx, origin, *at, text, typing && ix == last, *style);
            }
            Shape::Step { at, number, style } => {
                paint_gpui_step(window, cx, origin, *at, *number, *style);
            }
        }
    }
    // The picked annotation gets a live-only outline; `bake` never sees it.
    // Shapes that already show their own bounds skip it to avoid a second
    // rectangle on top of the drawing.
    let picked = layer.shows_pick();
    if picked
        && let Some(shape) = layer
            .selected
            .and_then(|ix| layer.shapes.get(ix))
            .filter(|shape| shape.shows_bounds_outline())
    {
        let (x, y, w, h) = shape.bounds();
        let outline = Bounds::new(
            point(origin.x + px(x), origin.y + px(y)),
            size(px(w), px(h)),
        );
        stroke_gpui_box(window, outline, rgb(0x3B82F6), px(1.));
    }
    // Grab points for the second-pass edit; live only, like the outline. Only
    // a picked shape has any, so this doubles as the pick check.
    let handles = layer.selected_handles();
    if !handles.is_empty() {
        for (_, at) in handles {
            let center = point(origin.x + px(at[0]), origin.y + px(at[1]));
            let box_ = Bounds::new(
                point(
                    center.x - px(HANDLE_SIZE / 2.0),
                    center.y - px(HANDLE_SIZE / 2.0),
                ),
                size(px(HANDLE_SIZE), px(HANDLE_SIZE)),
            );
            window.paint_quad(fill(box_, rgb(0xFFFFFF)));
            stroke_gpui_box(window, box_, rgb(0x3B82F6), px(1.));
        }
    }
}

/// Live-only rectangle outline, shared by the picked-shape highlight and the
/// text editing frame. Neither is part of the baked screenshot.
fn stroke_gpui_box(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    color: gpui_kit::Rgba,
    width: Pixels,
) {
    let mut path = PathBuilder::stroke(width);
    path.move_to(bounds.origin);
    path.line_to(point(bounds.origin.x + bounds.size.width, bounds.origin.y));
    path.line_to(point(
        bounds.origin.x + bounds.size.width,
        bounds.origin.y + bounds.size.height,
    ));
    path.line_to(point(bounds.origin.x, bounds.origin.y + bounds.size.height));
    path.close();
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

/// Editing frame around the in-progress text. Its width comes from the shaped
/// line, so CJK glyphs, emoji and an empty draft all fit; the extra room on
/// the right leaves space for the caret.
fn text_edit_box(pos: Point<Pixels>, text_width: Pixels, font_size: Pixels) -> Bounds<Pixels> {
    let pad = px(4.);
    let width = px(text_width.as_f32().max(6.0)) + pad * 2. + px(2.);
    let height = font_size * 1.25 + pad * 2.;
    Bounds::new(point(pos.x - pad, pos.y - pad), size(width, height))
}

fn paint_gpui_arrow(
    window: &mut Window,
    origin: Point<Pixels>,
    from: [f32; 2],
    to: [f32; 2],
    style: Style,
) {
    let start = gpui_pt(origin, from);
    let end = gpui_pt(origin, to);
    let Some((shaft, head)) = arrow_geometry(from, to, arrow_head(style.size)) else {
        return;
    };
    let color = rgb(style.color);
    let mut line = PathBuilder::stroke(px(style.size));
    line.move_to(start);
    line.line_to(gpui_pt(origin, shaft));
    if let Ok(path) = line.build() {
        window.paint_path(path, color);
    }
    let mut fill_head = PathBuilder::fill();
    fill_head.move_to(end);
    fill_head.line_to(gpui_pt(origin, head[0]));
    fill_head.line_to(gpui_pt(origin, head[1]));
    fill_head.close();
    if let Ok(path) = fill_head.build() {
        window.paint_path(path, color);
    }
}

fn paint_gpui_pen(
    window: &mut Window,
    origin: Point<Pixels>,
    points: &[[f32; 2]],
    stroke: Pixels,
    color: gpui_kit::Rgba,
) {
    match points {
        [] => {}
        [only] => {
            let r = stroke / 2.;
            let center = gpui_pt(origin, *only);
            window.paint_quad(
                fill(
                    Bounds::new(point(center.x - r, center.y - r), size(r * 2., r * 2.)),
                    color,
                )
                .corner_radii(r),
            );
        }
        points => {
            let mut builder = PathBuilder::stroke(stroke);
            builder.move_to(gpui_pt(origin, points[0]));
            for point in &points[1..] {
                builder.line_to(gpui_pt(origin, *point));
            }
            if let Ok(path) = builder.build() {
                window.paint_path(path, color);
            }
        }
    }
}

/// Sample every block of a mosaic, in painting order. Split from the painting
/// so the result can be cached: the sums touch the whole region, the quads do
/// not, and only the first has to run again when nothing moved.
///
/// Blocks that sample to the same colour and touch are emitted as one wider
/// block: flat areas of a screenshot are the common case, and a mosaic at block
/// size 4 over a large region is tens of thousands of quads to paint on every
/// pointer move otherwise. The painted pixels are identical either way.
fn mosaic_blocks(
    from: [f32; 2],
    to: [f32; 2],
    block_size: f32,
    scale: f32,
    source: &RgbaImage,
) -> Vec<MosaicBlock> {
    let (x, y, w, h) = aabb(from, to);
    if w < MIN_SPAN || h < MIN_SPAN {
        return Vec::new();
    }
    let block = block_size.max(2.0);
    let mut blocks: Vec<MosaicBlock> = Vec::new();
    let mut row = y;
    while row < y + h {
        let bh = block.min(y + h - row);
        let mut col = x;
        while col < x + w {
            let bw = block.min(x + w - col);
            let px0 = (col * scale).round() as i32;
            let py0 = (row * scale).round() as i32;
            let px1 = ((col + bw) * scale).round() as i32;
            let py1 = ((row + bh) * scale).round() as i32;
            let color = average_rgba(source, px0, py0, px1, py1);
            let joined = match blocks.last_mut() {
                Some(last)
                    if last.color == color
                        && last.at[1] == row
                        && last.size[1] == bh
                        && (last.at[0] + last.size[0] - col).abs() < 0.01 =>
                {
                    last.size[0] += bw;
                    true
                }
                _ => false,
            };
            if !joined {
                blocks.push(MosaicBlock {
                    at: [col, row],
                    size: [bw, bh],
                    color,
                });
            }
            col += bw;
        }
        row += bh;
    }
    blocks
}

fn paint_gpui_mosaic(window: &mut Window, origin: Point<Pixels>, blocks: &[MosaicBlock]) {
    for block in blocks {
        let bounds = Bounds::new(
            point(origin.x + px(block.at[0]), origin.y + px(block.at[1])),
            size(px(block.size[0]), px(block.size[1])),
        );
        window.paint_quad(fill(
            bounds,
            gpui_kit::Rgba {
                r: block.color[0] as f32 / 255.0,
                g: block.color[1] as f32 / 255.0,
                b: block.color[2] as f32 / 255.0,
                a: 1.0,
            },
        ));
    }
}

fn paint_gpui_text(
    window: &mut Window,
    cx: &mut App,
    origin: Point<Pixels>,
    at: [f32; 2],
    text: &str,
    caret: bool,
    style: Style,
) {
    let pos = gpui_pt(origin, at);
    let font_size = px(style.size);
    let color = rgb(style.color);
    let shown = if text.is_empty() {
        if caret { " " } else { return }
    } else {
        text
    };
    let run = TextRun {
        len: shown.len(),
        font: font(".SystemUIFont"),
        color: color.into(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let line = window.text_system().shape_line(
        SharedString::from(shown.to_string()),
        font_size,
        &[run],
        None,
    );
    // The frame is drawn under the glyphs and only while this draft is being
    // edited; it is never part of the exported image.
    if caret {
        stroke_gpui_box(
            window,
            text_edit_box(pos, line.width(), font_size),
            rgb(0x3B82F6),
            px(1.),
        );
    }
    let _ = line.paint(pos, font_size * 1.25, TextAlign::Left, None, window, cx);
    if caret {
        let caret_x = pos.x
            + if text.is_empty() {
                px(0.)
            } else {
                line.width()
            };
        window.paint_quad(fill(
            Bounds::new(point(caret_x, pos.y), size(px(1.5), font_size * 1.2)),
            color,
        ));
    }
}

fn paint_gpui_step(
    window: &mut Window,
    cx: &mut App,
    origin: Point<Pixels>,
    at: [f32; 2],
    number: u32,
    style: Style,
) {
    let r = px(style.size);
    let color = rgb(style.color);
    let center = gpui_pt(origin, at);
    let bounds = Bounds::new(point(center.x - r, center.y - r), size(r * 2., r * 2.));
    window.paint_quad(fill(bounds, color).corner_radii(r));
    let label = number.to_string();
    let run = TextRun {
        len: label.len(),
        font: font(".SystemUIFont").bold(),
        color: rgb(0xFFFFFF).into(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let font_size = px((style.size * 0.9).max(8.0));
    let line = window
        .text_system()
        .shape_line(label.into(), font_size, &[run], None);
    let text_origin = point(center.x - r, center.y - font_size / 2. - px(1.));
    let _ = line.paint(
        text_origin,
        r * 1.2,
        TextAlign::Center,
        Some(r * 2.),
        window,
        cx,
    );
}

fn stroke_gpui_rect(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    stroke: Pixels,
    color: gpui_kit::Rgba,
) {
    let mut builder = PathBuilder::stroke(stroke);
    builder.move_to(bounds.origin);
    builder.line_to(point(bounds.origin.x + bounds.size.width, bounds.origin.y));
    builder.line_to(point(
        bounds.origin.x + bounds.size.width,
        bounds.origin.y + bounds.size.height,
    ));
    builder.line_to(point(bounds.origin.x, bounds.origin.y + bounds.size.height));
    builder.close();
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

fn gpui_pt(origin: Point<Pixels>, p: [f32; 2]) -> Point<Pixels> {
    point(origin.x + px(p[0]), origin.y + px(p[1]))
}

fn gpui_aabb(origin: Point<Pixels>, from: [f32; 2], to: [f32; 2]) -> Option<Bounds<Pixels>> {
    let (x, y, w, h) = aabb(from, to);
    if w < 0.5 || h < 0.5 {
        return None;
    }
    Some(Bounds::new(
        point(origin.x + px(x), origin.y + px(y)),
        size(px(w), px(h)),
    ))
}

fn stroke_style(scale: f32, width: f32) -> Stroke {
    Stroke {
        width: (width * scale).max(1.0),
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Stroke::default()
    }
}

fn paint_color(color: u32) -> SkiaPaint<'static> {
    let [r, g, b, a] = rgba(color);
    let mut paint = SkiaPaint::default();
    paint.set_color_rgba8(r, g, b, a);
    paint.anti_alias = true;
    paint
}

fn rgba(color: u32) -> [u8; 4] {
    [(color >> 16) as u8, (color >> 8) as u8, color as u8, 255]
}

fn arrow_head(width: f32) -> f32 {
    ARROW_HEAD_BASE + width.max(0.0) * ARROW_HEAD_PER_WIDTH
}

fn stroke_rect(pixmap: &mut Pixmap, from: [f32; 2], to: [f32; 2], scale: f32, style: Style) {
    let (x, y, w, h) = scaled_aabb(from, to, scale);
    let Some(rect) =
        tiny_skia::Rect::from_xywh(x as f32, y as f32, w.max(1) as f32, h.max(1) as f32)
    else {
        return;
    };
    let mut path = SkiaPath::new();
    path.push_rect(rect);
    let Some(path) = path.finish() else {
        return;
    };
    pixmap.stroke_path(
        &path,
        &paint_color(style.color),
        &stroke_style(scale, style.size),
        Transform::identity(),
        None,
    );
}

fn stroke_arrow(pixmap: &mut Pixmap, from: [f32; 2], to: [f32; 2], scale: f32, style: Style) {
    let Some((shaft, head)) = arrow_geometry(from, to, arrow_head(style.size)) else {
        return;
    };
    let mut line = SkiaPath::new();
    line.move_to(from[0] * scale, from[1] * scale);
    line.line_to(shaft[0] * scale, shaft[1] * scale);
    if let Some(path) = line.finish() {
        pixmap.stroke_path(
            &path,
            &paint_color(style.color),
            &stroke_style(scale, style.size),
            Transform::identity(),
            None,
        );
    }
    let mut tri = SkiaPath::new();
    tri.move_to(to[0] * scale, to[1] * scale);
    tri.line_to(head[0][0] * scale, head[0][1] * scale);
    tri.line_to(head[1][0] * scale, head[1][1] * scale);
    tri.close();
    if let Some(path) = tri.finish() {
        pixmap.fill_path(
            &path,
            &paint_color(style.color),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

fn stroke_pen(pixmap: &mut Pixmap, points: &[[f32; 2]], scale: f32, style: Style) {
    match points {
        [] => {}
        [only] => {
            let r = (style.size * scale / 2.0).max(1.0);
            if let Some(path) = SkiaPath::from_circle(only[0] * scale, only[1] * scale, r) {
                pixmap.fill_path(
                    &path,
                    &paint_color(style.color),
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
        }
        points => {
            let mut path = SkiaPath::new();
            path.move_to(points[0][0] * scale, points[0][1] * scale);
            for p in &points[1..] {
                path.line_to(p[0] * scale, p[1] * scale);
            }
            if let Some(path) = path.finish() {
                pixmap.stroke_path(
                    &path,
                    &paint_color(style.color),
                    &stroke_style(scale, style.size),
                    Transform::identity(),
                    None,
                );
            }
        }
    }
}

fn draw_step(pixmap: &mut Pixmap, at: [f32; 2], number: u32, scale: f32, radius: f32, color: u32) {
    let cx = at[0] * scale;
    let cy = at[1] * scale;
    let r = (radius * scale).max(6.0);
    if let Some(path) = SkiaPath::from_circle(cx, cy, r) {
        pixmap.fill_path(
            &path,
            &paint_color(color),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    let label = number.to_string();
    let label_size = (radius * 0.9 * scale).max(8.0);
    draw_label(pixmap, cx, cy, &label, label_size, WHITE, true);
}

fn draw_text(
    pixmap: &mut Pixmap,
    at: [f32; 2],
    text: &str,
    scale: f32,
    font_size: f32,
    color: [u8; 4],
) {
    if text.is_empty() {
        return;
    }
    let size = (font_size * scale).max(8.0);
    draw_label(
        pixmap,
        at[0] * scale,
        at[1] * scale + size * 0.8,
        text,
        size,
        color,
        false,
    );
}

fn draw_label(
    pixmap: &mut Pixmap,
    x: f32,
    y: f32,
    text: &str,
    size: f32,
    color: [u8; 4],
    center: bool,
) {
    if let Some(font) = annotation_font() {
        stamp_fontdue(pixmap, font, [x, y], text, size, color, center);
        return;
    }
    if text.chars().all(|c| c.is_ascii_digit()) {
        stamp_digits(pixmap, x, y, text, size, color, center);
    }
}

fn stamp_fontdue(
    pixmap: &mut Pixmap,
    font: &fontdue::Font,
    at: [f32; 2],
    text: &str,
    size: f32,
    color: [u8; 4],
    center: bool,
) {
    let mut x = at[0];
    let baseline = at[1];
    if center {
        let width: f32 = text
            .chars()
            .map(|ch| font.metrics(ch, size).advance_width)
            .sum();
        x -= width / 2.0;
    }
    for ch in text.chars() {
        let (metrics, bitmap) = font.rasterize(ch, size);
        let gx = x + metrics.xmin as f32;
        let gy = baseline - metrics.height as f32 - metrics.ymin as f32;
        stamp_coverage(
            pixmap,
            gx.round() as i32,
            gy.round() as i32,
            metrics.width as u32,
            metrics.height as u32,
            &bitmap,
            color,
        );
        x += metrics.advance_width;
    }
}

fn stamp_digits(
    pixmap: &mut Pixmap,
    mut x: f32,
    y: f32,
    text: &str,
    size: f32,
    color: [u8; 4],
    center: bool,
) {
    let cell = (size / 7.0).round().max(1.0) as i32;
    let glyph_w = 6 * cell;
    let digits: Vec<u8> = text
        .bytes()
        .filter_map(|b| b.is_ascii_digit().then_some(b - b'0'))
        .collect();
    if digits.is_empty() {
        return;
    }
    if center {
        x -= (digits.len() as i32 * glyph_w) as f32 / 2.0;
        // `y` is baseline when centered on a step; lift to the glyph top.
        let top = y - size * 0.5;
        stamp_digit_row(
            pixmap,
            x.round() as i32,
            top.round() as i32,
            &digits,
            cell,
            color,
        );
        return;
    }
    stamp_digit_row(
        pixmap,
        x.round() as i32,
        y.round() as i32,
        &digits,
        cell,
        color,
    );
}

fn stamp_digit_row(pixmap: &mut Pixmap, x: i32, y: i32, digits: &[u8], cell: i32, color: [u8; 4]) {
    for (i, digit) in digits.iter().enumerate() {
        let ox = x + i as i32 * 6 * cell;
        let glyph = DIGITS[*digit as usize];
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..5 {
                if bits & (1 << (4 - col)) == 0 {
                    continue;
                }
                for dy in 0..cell {
                    for dx in 0..cell {
                        put_opaque(
                            pixmap,
                            ox + col * cell + dx,
                            y + row as i32 * cell + dy,
                            color,
                        );
                    }
                }
            }
        }
    }
}

fn stamp_coverage(
    pixmap: &mut Pixmap,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    coverage: &[u8],
    color: [u8; 4],
) {
    let pw = pixmap.width();
    let ph = pixmap.height() as i32;
    let stride = pw;
    let data = pixmap.data_mut();
    for row in 0..height as i32 {
        for col in 0..width as i32 {
            let cover = coverage[(row as u32 * width + col as u32) as usize];
            if cover == 0 {
                continue;
            }
            let dest_x = x + col;
            let dest_y = y + row;
            if dest_x < 0 || dest_y < 0 || dest_x >= pw as i32 || dest_y >= ph {
                continue;
            }
            let i = ((dest_y as u32 * stride + dest_x as u32) * 4) as usize;
            blend_over(&mut data[i..i + 4], color, cover);
        }
    }
}

fn put_opaque(pixmap: &mut Pixmap, x: i32, y: i32, color: [u8; 4]) {
    let w = pixmap.width() as i32;
    let h = pixmap.height() as i32;
    if x < 0 || y < 0 || x >= w || y >= h {
        return;
    }
    let i = ((y as u32 * pixmap.width() + x as u32) * 4) as usize;
    pixmap.data_mut()[i..i + 4].copy_from_slice(&color);
}

fn blend_over(dest: &mut [u8], src: [u8; 4], cover: u8) {
    let a = src[3] as u16 * cover as u16 / 255;
    if a == 0 {
        return;
    }
    let ia = 255 - a;
    dest[0] = ((src[0] as u16 * a + dest[0] as u16 * ia) / 255) as u8;
    dest[1] = ((src[1] as u16 * a + dest[1] as u16 * ia) / 255) as u8;
    dest[2] = ((src[2] as u16 * a + dest[2] as u16 * ia) / 255) as u8;
    dest[3] = (a + dest[3] as u16 * ia / 255) as u8;
}

fn pixelate(data: &mut [u8], width: u32, height: u32, region: (i32, i32, u32, u32), block: u32) {
    let (x, y, w, h) = region;
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x.saturating_add_unsigned(w)).min(width as i32);
    let y1 = (y.saturating_add_unsigned(h)).min(height as i32);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let block = block.max(2) as i32;
    let mut row = y0;
    while row < y1 {
        let bh = block.min(y1 - row);
        let mut col = x0;
        while col < x1 {
            let bw = block.min(x1 - col);
            let color = average_buf(data, width, col, row, col + bw, row + bh);
            for py in row..row + bh {
                for px in col..col + bw {
                    let i = ((py as u32 * width + px as u32) * 4) as usize;
                    data[i..i + 4].copy_from_slice(&color);
                }
            }
            col += bw;
        }
        row += bh;
    }
}

fn average_buf(data: &[u8], width: u32, x0: i32, y0: i32, x1: i32, y1: i32) -> [u8; 4] {
    let mut sum = [0u64; 4];
    let mut n = 0u64;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = ((y as u32 * width + x as u32) * 4) as usize;
            sum[0] += data[i] as u64;
            sum[1] += data[i + 1] as u64;
            sum[2] += data[i + 2] as u64;
            sum[3] += data[i + 3] as u64;
            n += 1;
        }
    }
    if n == 0 {
        return [0, 0, 0, 255];
    }
    [
        (sum[0] / n) as u8,
        (sum[1] / n) as u8,
        (sum[2] / n) as u8,
        (sum[3] / n) as u8,
    ]
}

fn average_rgba(image: &RgbaImage, x0: i32, y0: i32, x1: i32, y1: i32) -> [u8; 4] {
    let w = image.width() as i32;
    let h = image.height() as i32;
    let x0 = x0.clamp(0, w);
    let y0 = y0.clamp(0, h);
    let x1 = x1.clamp(x0, w);
    let y1 = y1.clamp(y0, h);
    let mut sum = [0u64; 4];
    let mut n = 0u64;
    for y in y0..y1 {
        for x in x0..x1 {
            let p = image.get_pixel(x as u32, y as u32).0;
            sum[0] += p[0] as u64;
            sum[1] += p[1] as u64;
            sum[2] += p[2] as u64;
            sum[3] += p[3] as u64;
            n += 1;
        }
    }
    if n == 0 {
        return [0, 0, 0, 255];
    }
    [
        (sum[0] / n) as u8,
        (sum[1] / n) as u8,
        (sum[2] / n) as u8,
        (sum[3] / n) as u8,
    ]
}

fn pixmap_from_rgba(image: &RgbaImage) -> Option<Pixmap> {
    let size = IntSize::from_wh(image.width(), image.height())?;
    let mut data = image.as_raw().clone();
    for px in data.as_chunks_mut::<4>().0 {
        let a = px[3];
        if a != 255 {
            px[0] = ((px[0] as u16 * a as u16) / 255) as u8;
            px[1] = ((px[1] as u16 * a as u16) / 255) as u8;
            px[2] = ((px[2] as u16 * a as u16) / 255) as u8;
        }
    }
    Pixmap::from_vec(data, size)
}

fn rgba_from_pixmap(pixmap: Pixmap) -> Option<RgbaImage> {
    let width = pixmap.width();
    let height = pixmap.height();
    let mut data = pixmap.take();
    for px in data.as_chunks_mut::<4>().0 {
        let a = px[3];
        if a > 0 && a < 255 {
            px[0] = ((px[0] as u16 * 255 + a as u16 / 2) / a as u16) as u8;
            px[1] = ((px[1] as u16 * 255 + a as u16 / 2) / a as u16) as u8;
            px[2] = ((px[2] as u16 * 255 + a as u16 / 2) / a as u16) as u8;
        }
    }
    RgbaImage::from_raw(width, height, data)
}

fn annotation_font() -> Option<&'static fontdue::Font> {
    static FONT: OnceLock<Option<fontdue::Font>> = OnceLock::new();
    FONT.get_or_init(load_font).as_ref()
}

fn load_font() -> Option<fontdue::Font> {
    const CANDIDATES: &[&str] = &[
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        r"C:\Windows\Fonts\arialuni.ttf",
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\arial.ttf",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
    ];
    for path in CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        if let Ok(font) = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()) {
            return Some(font);
        }
    }
    None
}

fn arrow_geometry(from: [f32; 2], to: [f32; 2], head: f32) -> Option<([f32; 2], [[f32; 2]; 2])> {
    let dx = to[0] - from[0];
    let dy = to[1] - from[1];
    let len = length(from, to);
    if len < MIN_SPAN {
        return None;
    }
    // A head longer than the drag would push the shaft past its start and turn
    // the arrow inside out. Shrink short arrows while keeping the proportions.
    let head = head.min(len * 0.8);
    let ux = dx / len;
    let uy = dy / len;
    let px = -uy;
    let py = ux;
    let back = [to[0] - ux * head, to[1] - uy * head];
    let shaft = [to[0] - ux * (head * 0.55), to[1] - uy * (head * 0.55)];
    let wing = head * 0.45;
    Some((
        shaft,
        [
            [back[0] + px * wing, back[1] + py * wing],
            [back[0] - px * wing, back[1] - py * wing],
        ],
    ))
}

fn aabb(from: [f32; 2], to: [f32; 2]) -> (f32, f32, f32, f32) {
    let x = from[0].min(to[0]);
    let y = from[1].min(to[1]);
    (x, y, (from[0] - to[0]).abs(), (from[1] - to[1]).abs())
}

fn scaled_aabb(from: [f32; 2], to: [f32; 2], scale: f32) -> (i32, i32, u32, u32) {
    let (x, y, w, h) = aabb(from, to);
    (
        (x * scale).round() as i32,
        (y * scale).round() as i32,
        (w * scale).round().max(1.0) as u32,
        (h * scale).round().max(1.0) as u32,
    )
}

fn span(from: [f32; 2], to: [f32; 2]) -> (f32, f32) {
    ((from[0] - to[0]).abs(), (from[1] - to[1]).abs())
}

/// Clamp a drag delta so a shape of size `extent` keeps its whole body inside
/// `[0, limit]`. A shape wider than the region is pinned to the near edge
/// instead of collapsing the clamp range.
fn clamp_shift(delta: f32, start: f32, max_start: f32) -> f32 {
    let low = -start;
    let high = max_start - start;
    if high >= low {
        delta.clamp(low, high)
    } else {
        low
    }
}

/// Rewrite `slot` as `original` moved by `(dx, dy)`. The result comes from the
/// original every time, so a drag cannot drift, but a freehand stroke rebuilds
/// its point list in the slot's own buffer: copying the whole stroke on every
/// pointer move was the last allocation left in the drag path.
fn rebuild_shifted(slot: &mut Shape, original: &Shape, dx: f32, dy: f32) {
    if let (
        Shape::Pen {
            points: dst,
            style: dst_style,
        },
        Shape::Pen {
            points: src,
            style: src_style,
        },
    ) = (&mut *slot, original)
    {
        *dst_style = *src_style;
        dst.clear();
        dst.extend(src.iter().map(|p| [p[0] + dx, p[1] + dy]));
        return;
    }
    *slot = original.clone();
    slot.shift(dx, dy);
}

fn length(from: [f32; 2], to: [f32; 2]) -> f32 {
    let dx = from[0] - to[0];
    let dy = from[1] - to[1];
    dx.hypot(dy)
}

fn point_to_segment(x: f32, y: f32, from: [f32; 2], to: [f32; 2]) -> f32 {
    let dx = to[0] - from[0];
    let dy = to[1] - from[1];
    let squared = dx * dx + dy * dy;
    if squared < f32::EPSILON {
        return length(from, [x, y]);
    }
    let t = (((x - from[0]) * dx + (y - from[1]) * dy) / squared).clamp(0.0, 1.0);
    length([from[0] + t * dx, from[1] + t * dy], [x, y])
}

/// Winding-order agnostic: the point is inside when all three edge cross
/// products share a sign (zero counts as inside, so the edges are pickable).
fn point_in_triangle(p: [f32; 2], a: [f32; 2], b: [f32; 2], c: [f32; 2]) -> bool {
    let cross = |u: [f32; 2], v: [f32; 2], w: [f32; 2]| {
        (v[0] - u[0]) * (w[1] - u[1]) - (v[1] - u[1]) * (w[0] - u[0])
    };
    let sides = [cross(a, b, p), cross(b, c, p), cross(c, a, p)];
    sides.iter().all(|d| *d >= 0.0) || sides.iter().all(|d| *d <= 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    fn solid(width: u32, height: u32, color: [u8; 4]) -> RgbaImage {
        RgbaImage::from_pixel(width, height, Rgba(color))
    }

    #[test]
    fn bake_rect_changes_pixels() {
        let mut image = solid(32, 32, [10, 20, 30, 255]);
        let before = image.clone();
        bake(
            &mut image,
            &[Shape::Rect {
                from: [4.0, 4.0],
                to: [20.0, 16.0],
                style: Style::default(),
            }],
            1.0,
        );
        assert_ne!(image.as_raw(), before.as_raw());
        assert_eq!(image.get_pixel(0, 0), before.get_pixel(0, 0));
        assert_eq!(image.get_pixel(4, 4).0[0], rgba(Style::default().color)[0]);
    }

    #[test]
    fn bake_uses_per_shape_style() {
        let bg = [10, 20, 30, 255];
        let blue = 0x3B82F6;
        let mut image = solid(48, 48, bg);
        bake(
            &mut image,
            &[Shape::Rect {
                from: [12.0, 12.0],
                to: [36.0, 36.0],
                style: Style {
                    color: blue,
                    size: 6.0,
                },
            }],
            1.0,
        );
        let stroke = image.get_pixel(12, 24).0;
        assert_eq!(&stroke[..3], &rgba(blue)[..3]);
        assert_eq!(image.get_pixel(24, 24).0, bg, "rect interior stays clear");
    }

    #[test]
    fn wider_stroke_covers_more_pixels() {
        fn stroke_pixels(width: f32) -> usize {
            let mut image = solid(48, 48, [0, 0, 0, 255]);
            bake(
                &mut image,
                &[Shape::Rect {
                    from: [12.0, 12.0],
                    to: [36.0, 36.0],
                    style: Style {
                        color: Style::default().color,
                        size: width,
                    },
                }],
                1.0,
            );
            image.pixels().filter(|p| p.0 != [0, 0, 0, 255]).count()
        }
        assert!(stroke_pixels(8.0) > stroke_pixels(2.0));
    }

    #[test]
    fn mosaic_blocks_are_sampled_once_per_geometry() {
        // Two 8 px colour fields, so a block never straddles the seam.
        let mut image = RgbaImage::new(64, 64);
        for (x, _, pixel) in image.enumerate_pixels_mut() {
            *pixel = Rgba(if x < 32 {
                [0, 0, 0, 255]
            } else {
                [255, 0, 0, 255]
            });
        }
        let blocks = mosaic_blocks([0.0, 0.0], [64.0, 64.0], 8.0, 1.0, &image);
        // Each row is two flat fields, so its four blocks per field merge into
        // one wide quad: 8 rows paint as 16 blocks instead of 64.
        assert_eq!(blocks.len(), 16, "flat neighbours are one block");
        assert!(
            blocks.iter().all(|block| block.size == [32.0, 8.0]),
            "a merged block spans exactly the field it sampled"
        );
        let covered: f32 = blocks
            .iter()
            .map(|block| block.size[0] * block.size[1])
            .sum();
        assert!(
            (covered - 64.0 * 64.0).abs() < 0.01,
            "the same area is painted"
        );
        assert!(
            blocks
                .iter()
                .all(|block| block.color == [0, 0, 0, 255] || block.color == [255, 0, 0, 255]),
            "each block takes the average of its own pixels"
        );

        let mut cache = MosaicCache::default();
        let mut sampled = 0;
        let key = MosaicKey::new(0, [0.0, 0.0], [64.0, 64.0], 8.0, 1.0, 1);
        let first = cache
            .blocks(key, || {
                sampled += 1;
                mosaic_blocks([0.0, 0.0], [64.0, 64.0], 8.0, 1.0, &image)
            })
            .len();
        // The next frame asks for the same geometry: no pixel reads at all.
        let second = cache
            .blocks(key, || {
                sampled += 1;
                mosaic_blocks([0.0, 0.0], [64.0, 64.0], 8.0, 1.0, &image)
            })
            .len();
        assert_eq!(first, second);
        assert_eq!(sampled, 1, "the painted frames must reuse the samples");

        // New source pixels must resample even though the rectangle is the same.
        cache.blocks(
            MosaicKey::new(0, [0.0, 0.0], [64.0, 64.0], 8.0, 1.0, 2),
            || {
                sampled += 1;
                Vec::new()
            },
        );
        assert_eq!(sampled, 2);

        // A mosaic too small to draw paints nothing rather than one block.
        assert!(mosaic_blocks([0.0, 0.0], [1.0, 1.0], 8.0, 1.0, &image).is_empty());
    }

    #[test]
    fn mosaic_blocks_merge_only_equal_neighbours() {
        // Every 8 px block is flat but its own colour, so nothing may merge and
        // the grid stays exactly as the geometry asks for.
        let mut image = RgbaImage::new(64, 64);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = Rgba([((x / 8) * 32) as u8, ((y / 8) * 32) as u8, 0, 255]);
        }
        let blocks = mosaic_blocks([0.0, 0.0], [64.0, 64.0], 8.0, 1.0, &image);
        assert_eq!(blocks.len(), 64, "distinct colours never merge");
        assert!(blocks.iter().all(|block| block.size == [8.0, 8.0]));
    }

    #[test]
    fn mosaic_cache_keeps_a_bounded_number_of_entries() {
        let mut cache = MosaicCache::default();
        for index in 0..MosaicCache::KEEP + 3 {
            cache.blocks(
                MosaicKey::new(index, [0.0, 0.0], [16.0, 16.0], 8.0, 1.0, 1),
                Vec::new,
            );
        }
        assert_eq!(cache.entries.len(), MosaicCache::KEEP);
    }

    #[test]
    fn bake_mosaic_is_block_average_not_a_blur() {
        // Two 12×12 color fields so the default mosaic block does not straddle them.
        let mut image = RgbaImage::new(24, 24);
        for y in 0..24 {
            for x in 0..24 {
                let v = if x < 12 { 0 } else { 255 };
                image.put_pixel(x, y, Rgba([v, 0, 0, 255]));
            }
        }
        bake(
            &mut image,
            &[Shape::Mosaic {
                from: [0.0, 0.0],
                to: [24.0, 24.0],
                style: Style::from_config(AnnotationTool::Mosaic.default_style()),
            }],
            1.0,
        );
        let a = image.get_pixel(0, 0).0;
        let b = image.get_pixel(11, 11).0;
        let c = image.get_pixel(12, 0).0;
        let d = image.get_pixel(23, 23).0;
        assert_eq!(a, b, "block interior must be uniform");
        assert_eq!(c, d, "block interior must be uniform");
        assert_ne!(a, c, "adjacent mosaic blocks keep distinct averages");
        assert_eq!(a, [0, 0, 0, 255]);
        assert_eq!(c, [255, 0, 0, 255]);
    }

    #[test]
    fn undo_and_clear() {
        let mut layer = Layer::default();
        layer.pointer_down(1.0, 1.0);
        layer.pointer_move(12.0, 10.0);
        layer.pointer_up();
        assert_eq!(layer.committed().len(), 1);
        layer.undo();
        assert!(layer.committed().is_empty());
        layer.pointer_down(2.0, 2.0);
        layer.pointer_move(8.0, 9.0);
        layer.pointer_up();
        layer.clear();
        assert!(!layer.can_undo());
    }

    #[test]
    fn ime_composition_replaces_marked_text() {
        let mut layer = Layer::default();
        layer.set_tool(Tool::Text);
        layer.pointer_down(1.0, 1.0);
        layer.replace_and_mark_text(None, "ni");
        layer.replace_and_mark_text(None, "nihao");
        assert_eq!(layer.draft_text(), Some("nihao"));
        assert_eq!(layer.marked_text_range(), Some(0..5));
        layer.replace_text(Some(0..5), "你好");
        assert_eq!(layer.draft_text(), Some("你好"));
        assert_eq!(layer.marked_text_range(), None);

        let (text, range) = layer.text_for_range(0..1).unwrap();
        assert_eq!(text, "你");
        assert_eq!(range, 0..1);

        layer.replace_text(None, "！");
        assert_eq!(layer.draft_text(), Some("你好！"));
        layer.commit();
        assert_eq!(layer.committed().len(), 1);
        assert!(layer.draft_text().is_none());
    }

    fn rect(layer: &mut Layer, from: (f32, f32), to: (f32, f32)) {
        layer.pointer_down(from.0, from.1);
        layer.pointer_move(to.0, to.1);
        layer.pointer_up();
    }

    /// Every tool draws through the same `bake` match, and a silently skipped
    /// arm is invisible until someone exports a screenshot. Draw one of each on
    /// a noisy background and require the canvas to change.
    #[test]
    fn every_tool_leaves_a_mark_in_the_export() {
        let mut image = RgbaImage::from_fn(960, 640, |x, y| {
            let wave = ((x as f32 / 37.0).sin() * 26.0 + (y as f32 / 23.0).cos() * 18.0) as i32;
            let grain = ((x * 7 + y * 13) % 41) as i32 - 20;
            Rgba([
                (70 + wave + grain).clamp(0, 255) as u8,
                (120 + wave / 2 + grain).clamp(0, 255) as u8,
                (190 - wave + grain).clamp(0, 255) as u8,
                255,
            ])
        });
        let before = image.clone();
        let mut layer = Layer::default();

        layer.set_tool(Tool::Rect);
        layer.set_color(0xFF3B30);
        rect(&mut layer, (60.0, 70.0), (300.0, 200.0));

        layer.set_tool(Tool::Arrow);
        layer.set_color(0x34C759);
        rect(&mut layer, (520.0, 90.0), (860.0, 190.0));

        layer.set_tool(Tool::Line);
        layer.set_color(0xFFCC00);
        layer.set_size(6.0);
        rect(&mut layer, (520.0, 230.0), (860.0, 300.0));

        layer.set_tool(Tool::Pen);
        layer.set_color(0xAF52DE);
        layer.set_size(4.0);
        layer.pointer_down(60.0, 300.0);
        for step in 0..240 {
            let t = step as f32 / 10.0;
            layer.pointer_move(60.0 + step as f32 * 1.6, 380.0 + t.sin() * 40.0);
        }
        layer.pointer_up();

        layer.set_tool(Tool::Mosaic);
        layer.set_size(14.0);
        rect(&mut layer, (60.0, 480.0), (420.0, 600.0));

        layer.set_tool(Tool::Step);
        layer.set_color(0xFF9500);
        layer.set_size(18.0);
        layer.pointer_down(470.0, 470.0);
        layer.pointer_up();
        layer.pointer_down(540.0, 470.0);
        layer.pointer_up();

        layer.set_tool(Tool::Text);
        layer.set_color(0xFFFFFF);
        layer.set_size(28.0);
        layer.pointer_down(600.0, 500.0);
        layer.insert_text("SnapIt 标注 1");
        layer.commit();

        bake(&mut image, layer.committed(), 1.0);
        assert_eq!(layer.committed().len(), 8);
        let changed = image
            .pixels()
            .zip(before.pixels())
            .filter(|(after, was)| after != was)
            .count();
        assert!(
            changed > 4000,
            "each tool must paint something, got {changed}"
        );
    }

    #[test]
    fn the_last_drawn_shape_stays_picked() {
        let mut layer = Layer::default();
        rect(&mut layer, (4.0, 4.0), (20.0, 20.0));
        assert_eq!(layer.selected(), Some(0));
        rect(&mut layer, (60.0, 60.0), (80.0, 80.0));
        assert_eq!(layer.selected(), Some(1));

        // The style controls now restyle the shape just drawn.
        layer.set_size(12.0);
        assert_eq!(layer.committed()[1].style().size, 12.0);
        assert_eq!(layer.committed()[0].style(), Style::default());
        assert_eq!(layer.config().rect, AnnotationTool::Rect.default_style());

        // Starting the next shape drops the pick, so its own drag is clean.
        layer.pointer_down(200.0, 200.0);
        assert_eq!(layer.selected(), None);
    }

    #[test]
    fn a_stamp_and_finished_text_stay_picked_too() {
        let mut layer = Layer::default();
        layer.set_tool(Tool::Step);
        layer.pointer_down(30.0, 30.0);
        assert_eq!(layer.selected(), Some(0));

        let mut layer = Layer::default();
        layer.set_tool(Tool::Text);
        layer.pointer_down(20.0, 20.0);
        layer.insert_text("hi");
        layer.commit();
        assert_eq!(layer.selected(), Some(0));
        assert_eq!(layer.committed().len(), 1);
    }

    #[test]
    fn picking_is_independent_of_the_armed_tool() {
        let mut layer = Layer::default();
        rect(&mut layer, (4.0, 4.0), (40.0, 40.0));
        assert_eq!(layer.selected(), Some(0));
        // The rectangle tool is still armed, so the handles can be grabbed.
        assert_eq!(layer.tool(), Tool::Rect);
        assert!(layer.shows_pick());
        assert!(!layer.selected_handles().is_empty());

        assert!(layer.deselect());
        assert!(!layer.shows_pick());
        assert!(!layer.deselect());

        // Empty canvas is reported without changing the pick.
        assert!(!layer.hits_at(300.0, 300.0));
        assert!(layer.hits_at(4.0, 20.0));
        assert!(layer.shows_pick() == layer.selected().is_some());
    }

    #[test]
    fn empty_canvas_press_drops_the_pick_and_starts_a_shape() {
        let mut layer = Layer::default();
        rect(&mut layer, (4.0, 4.0), (40.0, 40.0));
        assert_eq!(layer.selected(), Some(0));
        layer.pointer_down(200.0, 200.0);
        assert_eq!(layer.selected(), None);
        assert!(layer.is_dragging());
    }

    #[test]
    fn restyling_a_picked_shape_leaves_the_others_alone() {
        let mut layer = Layer::default();
        rect(&mut layer, (4.0, 4.0), (20.0, 20.0));
        rect(&mut layer, (60.0, 60.0), (80.0, 80.0));
        assert_eq!(layer.committed().len(), 2);

        assert!(layer.select_at(5.0, 5.0));
        assert_eq!(layer.selected(), Some(0));
        layer.set_color(0x3B82F6);
        layer.set_size(12.0);

        assert_eq!(layer.committed()[0].style().color, 0x3B82F6);
        assert_eq!(layer.committed()[0].style().size, 12.0);
        assert_eq!(layer.committed()[1].style(), Style::default());
        assert_eq!(layer.config().rect, AnnotationTool::Rect.default_style());

        assert!(layer.delete_selected());
        assert_eq!(layer.committed().len(), 1);
        assert_eq!(layer.selected(), None);
    }

    #[test]
    fn a_restyle_is_one_undo_step() {
        let mut layer = Layer::default();
        rect(&mut layer, (4.0, 4.0), (20.0, 20.0));
        let original = layer.committed()[0].style();
        assert_eq!(layer.selected(), Some(0));

        // A slider drag arrives as one change per tick, then a palette click.
        for size in 5..=18 {
            layer.set_size(size as f32);
        }
        layer.set_color(0x3B82F6);
        assert_eq!(layer.committed()[0].style().size, 18.0);
        assert_eq!(layer.committed()[0].style().color, 0x3B82F6);

        layer.undo();
        assert_eq!(layer.committed().len(), 1, "the shape must survive undo");
        assert_eq!(layer.committed()[0].style(), original);
    }

    #[test]
    fn a_new_gesture_starts_a_new_style_step() {
        let mut layer = Layer::default();
        rect(&mut layer, (4.0, 4.0), (104.0, 104.0));
        let original = layer.committed()[0].style();

        layer.set_size(10.0);
        // Moving the shape ends the style run: the next restyle needs its own
        // snapshot, or undo would step back past the move as well.
        assert_eq!(layer.begin_edit(4.0, 40.0), EditStart::Shape);
        layer.drag_edit(14.0, 50.0, [400.0, 400.0]);
        assert!(layer.end_edit());

        layer.set_color(0x22C55E);
        assert_eq!(layer.committed()[0].style().color, 0x22C55E);

        layer.undo();
        assert_eq!(layer.committed()[0].style().color, original.color);
        assert_eq!(layer.committed()[0].style().size, 10.0);
    }

    #[test]
    fn a_press_drags_the_topmost_annotation() {
        let mut layer = Layer::default();
        rect(&mut layer, (4.0, 4.0), (204.0, 204.0));
        rect(&mut layer, (60.0, 60.0), (140.0, 140.0));
        // Pick the big one first, away from the small one on top of it.
        assert!(layer.select_at(4.0, 100.0));
        assert_eq!(layer.selected(), Some(0));
        assert_eq!(layer.begin_edit(60.0, 100.0), EditStart::Shape);
        assert_eq!(layer.selected(), Some(1), "the overlap drags the top shape");
    }

    #[test]
    fn deleting_during_a_drag_drops_the_drag() {
        let mut layer = Layer::default();
        rect(&mut layer, (4.0, 4.0), (104.0, 104.0));
        rect(&mut layer, (200.0, 200.0), (300.0, 300.0));
        assert_eq!(layer.begin_edit(4.0, 40.0), EditStart::Shape);
        layer.drag_edit(14.0, 50.0, [400.0, 400.0]);
        assert!(layer.delete_selected());
        assert!(!layer.is_editing());

        // The neighbour moved into the deleted slot; the stale drag must not
        // follow it there.
        let neighbour = layer.committed()[0].bounds();
        layer.drag_edit(80.0, 80.0, [400.0, 400.0]);
        assert_eq!(layer.committed()[0].bounds(), neighbour);
    }

    #[test]
    fn hit_test_picks_the_topmost_shape_and_clears_on_empty_space() {
        let mut layer = Layer::default();
        rect(&mut layer, (4.0, 4.0), (40.0, 40.0));
        rect(&mut layer, (10.0, 10.0), (24.0, 24.0));
        assert!(layer.select_at(12.0, 12.0));
        assert_eq!(layer.selected(), Some(1));
        assert!(!layer.select_at(300.0, 300.0));
        assert_eq!(layer.selected(), None);
    }

    #[test]
    fn arrow_head_grows_at_every_slider_step() {
        let (min, max) = AnnotationTool::Arrow.size_range();
        let mut previous = 0.0;
        for size in min..=max {
            let head = arrow_head(size as f32);
            assert!(head > previous, "size {size} must enlarge the arrow head");
            previous = head;
        }
        // The default still looks like the old 14px head.
        let default = AnnotationTool::Arrow.default_style().size as f32;
        assert!((arrow_head(default) - 14.0).abs() < 2.0);
    }

    #[test]
    fn baked_arrow_head_follows_the_size() {
        // Both renderers derive the head from `arrow_head(style.size)`; bake
        // proves the enlarged triangle really reaches the exported bitmap.
        fn pixel(size: f32) -> [u8; 4] {
            let mut image = solid(96, 56, [0, 0, 0, 255]);
            bake(
                &mut image,
                &[Shape::Arrow {
                    from: [4.0, 20.0],
                    to: [60.0, 20.0],
                    style: Style {
                        color: 0xFFFFFF,
                        size,
                    },
                }],
                1.0,
            );
            image.get_pixel(38, 28).0
        }
        let bg = [0, 0, 0, 255];
        assert_eq!(pixel(2.0), bg, "thin arrow head must not reach (38, 28)");
        assert_ne!(pixel(12.0), bg, "thick arrow head must reach (38, 28)");
    }

    #[test]
    fn short_arrow_never_turns_inside_out() {
        let (shaft, head) = arrow_geometry([0.0, 0.0], [10.0, 0.0], arrow_head(24.0)).unwrap();
        assert!(shaft[0] > 0.0 && shaft[0] < 10.0);
        assert!(head.iter().all(|p| (0.0..=10.0).contains(&p[0])));
    }

    #[test]
    fn arrow_head_wing_is_pickable() {
        let shape = Shape::Arrow {
            from: [4.0, 20.0],
            to: [80.0, 20.0],
            style: Style {
                color: 0xFFFFFF,
                size: 24.0,
            },
        };
        // Outside the shaft's hit slack, so only the triangle can pick it.
        assert!(shape.hits(36.0, 38.5));
        assert!(!shape.hits(36.0, 50.0));
    }

    #[test]
    fn size_changes_text_and_step_in_the_baked_bitmap() {
        fn painted_pixels(font_size: f32) -> usize {
            let mut image = solid(120, 40, [0, 0, 0, 255]);
            let style = Style {
                color: 0xFFFFFF,
                size: font_size,
            };
            bake(
                &mut image,
                &[Shape::Text {
                    at: [2.0, 2.0],
                    text: "8".into(),
                    style,
                }],
                1.0,
            );
            image.pixels().filter(|p| p.0 != [0, 0, 0, 255]).count()
        }
        assert!(painted_pixels(32.0) > painted_pixels(12.0));
    }

    #[test]
    fn only_irregular_shapes_keep_the_selection_outline() {
        let style = Style {
            color: 0xFFFFFF,
            size: 4.0,
        };
        let outlined = |shape: Shape| shape.shows_bounds_outline();
        assert!(outlined(Shape::Pen {
            points: vec![[0.0, 0.0], [10.0, 10.0]],
            style,
        }));
        assert!(outlined(Shape::Text {
            at: [0.0, 0.0],
            text: "hi".into(),
            style,
        }));
        // These show their own silhouette or their grab handles instead.
        assert!(!outlined(Shape::Rect {
            from: [0.0, 0.0],
            to: [40.0, 20.0],
            style,
        }));
        assert!(!outlined(Shape::Mosaic {
            from: [0.0, 0.0],
            to: [40.0, 20.0],
            style,
        }));
        assert!(!outlined(Shape::Arrow {
            from: [0.0, 0.0],
            to: [40.0, 0.0],
            style,
        }));
        assert!(!outlined(Shape::Line {
            from: [0.0, 0.0],
            to: [40.0, 0.0],
            style,
        }));
        assert!(!outlined(Shape::Step {
            at: [20.0, 20.0],
            number: 1,
            style,
        }));
    }

    #[test]
    fn text_edit_box_fits_the_line_and_the_font() {
        let pos = point(px(20.), px(30.));
        let empty = text_edit_box(pos, px(0.), px(16.));
        assert!(
            empty.size.width > px(8.),
            "empty draft still needs a visible box"
        );

        let filled = text_edit_box(pos, px(120.), px(16.));
        assert!(filled.size.width >= px(120.));
        assert!(filled.size.width > empty.size.width);
        assert!(filled.origin.x < pos.x && filled.origin.y < pos.y);
        // The caret at the end of the line stays inside the frame.
        assert!(pos.x + px(120.) < filled.origin.x + filled.size.width);

        let larger_font = text_edit_box(pos, px(120.), px(32.));
        assert!(larger_font.size.height > filled.size.height);
    }

    #[test]
    fn line_tool_bakes_a_straight_stroke() {
        let mut layer = Layer::default();
        layer.set_tool(Tool::Line);
        layer.pointer_down(4.0, 10.0);
        layer.pointer_move(40.0, 10.0);
        layer.pointer_up();
        assert_eq!(layer.committed().len(), 1);
        let line = &layer.committed()[0];
        assert_eq!(line.tool(), Tool::Line);
        assert_eq!(
            line.style().size,
            AnnotationTool::Line.default_style().size as f32
        );
        assert!(line.hits(20.0, 10.0));
        assert!(!line.hits(20.0, 20.0));

        let mut image = solid(48, 24, [0, 0, 0, 255]);
        bake(&mut image, layer.committed(), 1.0);
        assert_ne!(image.get_pixel(20, 10).0, [0, 0, 0, 255]);
        assert_eq!(image.get_pixel(20, 20).0, [0, 0, 0, 255]);
    }

    #[test]
    fn line_click_without_drag_is_discarded_and_lines_move_with_the_selection() {
        let mut layer = Layer::default();
        layer.set_tool(Tool::Line);
        layer.pointer_down(5.0, 5.0);
        layer.pointer_up();
        assert!(layer.committed().is_empty(), "a click must not leave a dot");

        layer.pointer_down(4.0, 10.0);
        layer.pointer_move(40.0, 10.0);
        layer.pointer_up();
        assert!(layer.select_at(20.0, 10.0));
        layer.set_size(9.0);
        assert_eq!(layer.committed()[0].style().size, 9.0);
        assert_eq!(layer.config().line, AnnotationTool::Line.default_style());

        layer.translate(3.0, 4.0);
        let Shape::Line { from, to, .. } = &layer.committed()[0] else {
            panic!("line tool must produce a Line shape");
        };
        assert_eq!(*from, [7.0, 14.0]);
        assert_eq!(*to, [43.0, 14.0]);
    }

    #[test]
    fn dragging_a_picked_line_moves_it_and_stays_inside_the_capture() {
        let mut layer = Layer::default();
        layer.set_tool(Tool::Line);
        layer.pointer_down(10.0, 10.0);
        layer.pointer_move(30.0, 10.0);
        layer.pointer_up();

        assert!(layer.select_at(20.0, 10.0));
        assert_eq!(layer.begin_edit(20.0, 10.0), EditStart::Shape);
        // 40 wide viewport, line is 20 wide and starts at 10: at most +10.
        layer.drag_edit(75.0, 18.0, [40.0, 40.0]);
        assert!(layer.end_edit());

        let Shape::Line { from, to, .. } = &layer.committed()[0] else {
            panic!("line tool must produce a Line shape");
        };
        assert_eq!(*from, [20.0, 18.0]);
        assert_eq!(*to, [40.0, 18.0]);
    }

    #[test]
    fn a_drag_pinned_at_the_edge_reports_no_move() {
        let mut layer = Layer::default();
        layer.set_tool(Tool::Pen);
        layer.pointer_down(30.0, 30.0);
        layer.pointer_move(60.0, 30.0);
        layer.pointer_up();

        assert_eq!(layer.begin_edit(45.0, 30.0), EditStart::Shape);
        assert!(layer.drag_edit(-40.0, -40.0, [100.0, 100.0]));
        // Past the corner the clamp yields the offset already applied, so the
        // canvas must not be repainted again for the same position.
        assert!(!layer.drag_edit(-80.0, -80.0, [100.0, 100.0]));
        let (x, y, _, _) = layer.committed()[0].extents();
        assert_eq!((x, y), (0.0, 0.0), "the stroke stops at the capture edge");
        assert!(layer.end_edit());
    }

    #[test]
    fn a_plain_click_on_the_pick_adds_no_history_step() {
        let mut layer = Layer::default();
        layer.pointer_down(10.0, 10.0);
        layer.pointer_move(60.0, 50.0);
        layer.pointer_up();
        let history = layer.history.len();

        layer.select_at(10.0, 10.0);
        assert_eq!(layer.begin_edit(45.0, 10.0), EditStart::Shape);
        assert!(!layer.end_edit());
        assert_eq!(layer.history.len(), history);
    }

    #[test]
    fn corner_handle_resizes_a_rect_around_the_opposite_corner() {
        let mut layer = Layer::default();
        layer.pointer_down(10.0, 10.0);
        layer.pointer_move(60.0, 50.0);
        layer.pointer_up();

        layer.select_at(10.0, 10.0);
        assert_eq!(layer.begin_edit(10.0, 10.0), EditStart::Handle);
        layer.drag_edit(20.0, 24.0, [100.0, 100.0]);
        assert!(layer.end_edit());

        let Shape::Rect { from, to, .. } = &layer.committed()[0] else {
            panic!("rect tool must produce a Rect shape");
        };
        assert_eq!(*from, [20.0, 24.0]);
        assert_eq!(*to, [60.0, 50.0], "the opposite corner must not move");
    }

    #[test]
    fn a_handle_drag_cannot_collapse_or_escape_the_capture() {
        let mut layer = Layer::default();
        layer.pointer_down(10.0, 10.0);
        layer.pointer_move(60.0, 50.0);
        layer.pointer_up();

        layer.select_at(10.0, 10.0);
        assert_eq!(layer.begin_edit(10.0, 10.0), EditStart::Handle);
        // Way past the opposite corner and outside the viewport.
        layer.drag_edit(300.0, -40.0, [100.0, 100.0]);
        layer.end_edit();

        let Shape::Rect { from, to, .. } = &layer.committed()[0] else {
            panic!("rect tool must produce a Rect shape");
        };
        assert_eq!(*from, [58.0, 0.0], "kept MIN_SPAN and clamped to the top");
        assert_eq!(*to, [60.0, 50.0]);
    }

    #[test]
    fn double_click_text_edits_in_place_instead_of_appending() {
        let mut layer = Layer::default();
        layer.set_tool(Tool::Text);
        layer.pointer_down(10.0, 10.0);
        layer.insert_text("hi");
        layer.pointer_up();
        layer.commit();
        assert_eq!(layer.committed().len(), 1);

        assert!(layer.begin_text_edit(12.0, 12.0));
        assert_eq!(layer.draft_text(), Some("hi"));
        layer.insert_text("!");
        layer.commit();

        assert_eq!(layer.committed().len(), 1, "edit must replace, not append");
        let Shape::Text { text, .. } = &layer.committed()[0] else {
            panic!("text tool must produce a Text shape");
        };
        assert_eq!(text, "hi!");
    }

    #[test]
    fn double_click_from_the_text_tool_reopens_the_existing_text() {
        let mut layer = Layer::default();
        layer.set_tool(Tool::Text);
        layer.pointer_down(10.0, 10.0);
        layer.insert_text("old");
        layer.commit();

        // First click of the double click opens an empty draft on top.
        layer.pointer_down(12.0, 12.0);
        assert_eq!(layer.draft_text(), Some(""));
        // Second click reopens the text underneath instead of the empty draft.
        assert!(layer.begin_text_edit(12.0, 12.0));
        assert_eq!(layer.draft_text(), Some("old"));
        layer.insert_text("!");
        layer.commit();

        assert_eq!(layer.committed().len(), 1);
        let Shape::Text { text, .. } = &layer.committed()[0] else {
            panic!("text tool must produce a Text shape");
        };
        assert_eq!(text, "old!");
    }

    #[test]
    fn cancelling_a_text_edit_restores_the_original() {
        let mut layer = Layer::default();
        layer.set_tool(Tool::Text);
        layer.pointer_down(10.0, 10.0);
        layer.insert_text("keep");
        layer.pointer_up();
        layer.commit();

        assert!(layer.begin_text_edit(12.0, 12.0));
        layer.insert_text(" and drop this");
        layer.cancel_draft();

        let Shape::Text { text, .. } = &layer.committed()[0] else {
            panic!("text tool must produce a Text shape");
        };
        assert_eq!(text, "keep");
    }

    #[test]
    fn undo_reverts_a_move_and_a_text_edit() {
        let mut layer = Layer::default();
        layer.set_tool(Tool::Line);
        layer.pointer_down(10.0, 10.0);
        layer.pointer_move(30.0, 10.0);
        layer.pointer_up();

        layer.select_at(20.0, 10.0);
        layer.begin_edit(20.0, 10.0);
        layer.drag_edit(25.0, 15.0, [80.0, 80.0]);
        layer.end_edit();
        let moved = layer.committed()[0].extents();
        assert_eq!(moved, (15.0, 15.0, 20.0, 0.0));

        layer.undo();
        assert_eq!(layer.committed()[0].extents(), (10.0, 10.0, 20.0, 0.0));

        layer.set_tool(Tool::Text);
        layer.pointer_down(40.0, 40.0);
        layer.insert_text("mm");
        layer.pointer_up();
        layer.commit();
        layer.begin_text_edit(42.0, 42.0);
        layer.insert_text("!");
        layer.commit();
        layer.undo();
        let Shape::Text { text, .. } = &layer.committed()[1] else {
            panic!("text tool must produce a Text shape");
        };
        assert_eq!(text, "mm");
    }
}
