//! Real-time bidirectional vertical screenshot stitcher.
//!
//! Designed for live screen capture streams:
//! - Detects fixed sticky headers/footers (ignoring plain uniform whitespace)
//!   so sticky bars appear only once in the stitched result.
//! - Excludes scrollbar margins from motion matching while keeping full image width.
//! - Uses coarse-to-fine gradient-weighted MAD matching across both scroll
//!   directions (downward and upward), disambiguating repetitive patterns with
//!   scroll momentum.
//! - Maintains a dual-anchor commit model (`committed` + `prev`) to avoid
//!   micro-scroll drift while handling fast jumps cleanly.
//! - Places stitch seams in the minimum-difference row near the middle of the
//!   overlap band, avoiding drop shadows cast by sticky headers/footers.
//! - Absorbs macOS/Windows rubber-band bounce and publishes one bounded,
//!   downscaled preview of the currently visible canvas range.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use crate::paged_rows::PagedRows;
use anyhow::{Context as _, Result, bail};
use image::RgbaImage;
use snapit_capture::DiskImage;

const PREVIEW_MAX_WIDTH: u32 = 320;
const FINE_MAX_WIDTH: usize = 384;
const COARSE_STRIDE: usize = 4;
const MAX_FINE_ROWS: usize = 64;
const MAX_COARSE_CANDIDATES: usize = 10;
const MAX_MATCH_MAD: f32 = 7.5;
const MIN_INLIER_RATIO: f32 = 0.88;
const INLIER_THRESHOLD: u8 = 12;
const INITIAL_MARGIN_VOTES: u32 = 3;
const INITIAL_SIDEBAR_VOTES: u32 = 4;
const MARGIN_CHANGE_VOTES: u32 = 8;
static NEXT_PREVIEW_REVISION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScrollDirection {
    #[default]
    Down,
    Up,
}

impl ScrollDirection {
    pub fn opposite(self) -> Self {
        match self {
            Self::Down => Self::Up,
            Self::Up => Self::Down,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollWarning {
    TooFast,
    LowTexture,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeedOutcome {
    Initial,
    Unchanged,
    Moved {
        dy: i32,
    },
    Stitched {
        dy: i32,
        total_height: u32,
    },
    Warning(ScrollWarning),
    DimensionsChanged {
        expected: (u32, u32),
        actual: (u32, u32),
    },
    Failed(String),
}

/// One vertical slice of the downscaled live preview, with a stable id and
/// monotonic revision so UI views only re-upload changed tiles.
#[derive(Clone, Debug)]
pub struct PreviewTile {
    pub id: u64,
    pub revision: u64,
    pub source_start: u32,
    pub source_rows: u32,
    pub total_height: u32,
    pub image: Arc<RgbaImage>,
}

#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct StitchConfig {
    /// Physical pixels per logical point.
    pub scale: f32,
}

impl Default for StitchConfig {
    fn default() -> Self {
        Self { scale: 1.0 }
    }
}

impl StitchConfig {
    pub fn with_scale(mut self, scale: f32) -> Self {
        self.scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        self
    }
}

pub(crate) struct PooledImage {
    image: Option<RgbaImage>,
    pool: Option<snapit_capture::FrameBufferPool>,
}

impl PooledImage {
    pub(crate) fn new(image: RgbaImage, pool: Option<snapit_capture::FrameBufferPool>) -> Self {
        Self {
            image: Some(image),
            pool,
        }
    }

    pub(crate) fn unpooled(image: RgbaImage) -> Self {
        Self {
            image: Some(image),
            pool: None,
        }
    }
}

impl std::ops::Deref for PooledImage {
    type Target = RgbaImage;

    fn deref(&self) -> &Self::Target {
        self.image.as_ref().expect("pooled image live until drop")
    }
}

impl Drop for PooledImage {
    fn drop(&mut self) {
        if let (Some(img), Some(pool)) = (self.image.take(), &self.pool) {
            pool.recycle(img.into_raw());
        }
    }
}

pub(crate) struct FrameProfile {
    width: u32,
    height: u32,
    grid: [u32; 64],
    fine_origin_x: usize,
    fine_step: usize,
    fine_w: usize,
    fine_luma: Vec<u8>,
    row_grad: Vec<f32>,
    row_vert_grad: Vec<f32>,
    row_edge_columns: Vec<u32>,
    coarse_w: usize,
    coarse_h: usize,
    coarse_luma: Vec<u8>,
    coarse_vert_grad: Vec<f32>,
}

impl FrameProfile {
    #[cfg(test)]
    fn from_image(image: RgbaImage, scale: f32) -> Self {
        let (w, h) = image.dimensions();
        let grid = sample_grid(image.as_raw(), w as usize, h as usize);
        Self::from_image_with_grid(&image, scale, grid)
    }

    pub(crate) fn from_image_with_grid(image: &RgbaImage, scale: f32, grid: [u32; 64]) -> Self {
        let (width, height) = image.dimensions();
        let w = width as usize;
        let h = height as usize;
        let raw = image.as_raw();

        let interior = interior_columns(w, scale);
        let interior_w = interior.end - interior.start;

        let fine_step = (interior_w / FINE_MAX_WIDTH).max(1);
        let fine_w = interior_w.div_ceil(fine_step).max(1);
        let mut fine_luma = vec![0u8; fine_w * h];
        let mut row_grad = vec![0.0_f32; h];
        let mut row_vert_grad = vec![0.0_f32; h];
        let mut row_edge_columns = vec![0u32; h];

        for y in 0..h {
            let row_off = y * w * 4;
            let dst_start = y * fine_w;
            let mut prev_l = 0i32;
            let mut grad_sum = 0u32;
            let mut vert_sum = 0u32;
            let mut edge_columns = 0u32;
            for (ix, x) in interior.clone().step_by(fine_step).enumerate() {
                let p = row_off + x * 4;
                let l = luma(raw[p], raw[p + 1], raw[p + 2]);
                fine_luma[dst_start + ix] = l;
                if y > 0 {
                    vert_sum += u32::from(l.abs_diff(fine_luma[dst_start - fine_w + ix]));
                }
                if ix > 0 {
                    let delta = (i32::from(l) - prev_l).unsigned_abs();
                    grad_sum += delta;
                    if delta >= 12 {
                        edge_columns += 1;
                    }
                }
                prev_l = i32::from(l);
            }
            row_edge_columns[y] = edge_columns;
            if y > 0 {
                row_vert_grad[y] = vert_sum as f32 / fine_w as f32;
            }
            if fine_w > 1 {
                row_grad[y] = grad_sum as f32 / (fine_w - 1) as f32;
            }
        }

        let coarse_w = fine_w.div_ceil(COARSE_STRIDE).max(1);
        let coarse_h = h.div_ceil(COARSE_STRIDE).max(1);
        let mut coarse_luma = vec![0u8; coarse_w * coarse_h];
        let mut coarse_vert_grad = vec![0.0_f32; coarse_h];
        for cy in 0..coarse_h {
            let y0 = cy * COARSE_STRIDE;
            let y1 = (y0 + COARSE_STRIDE).min(h);
            let mut vert_max = 0.0_f32;
            for y in y0..y1 {
                let vg = row_vert_grad[y].max(if y + 1 < h { row_vert_grad[y + 1] } else { 0.0 });
                vert_max = vert_max.max(row_grad[y].min(vg));
            }
            coarse_vert_grad[cy] = vert_max;
            let full_y = y1 - y0 == COARSE_STRIDE;
            for cx in 0..coarse_w {
                let x0 = cx * COARSE_STRIDE;
                let x1 = (x0 + COARSE_STRIDE).min(fine_w);
                if full_y && x1 - x0 == 4 {
                    let mut sum = 0u32;
                    for y in y0..y1 {
                        let r = y * fine_w + x0;
                        sum += u32::from(fine_luma[r])
                            + u32::from(fine_luma[r + 1])
                            + u32::from(fine_luma[r + 2])
                            + u32::from(fine_luma[r + 3]);
                    }
                    coarse_luma[cy * coarse_w + cx] = (sum >> 4) as u8;
                } else {
                    let mut sum = 0u32;
                    let mut count = 0u32;
                    for y in y0..y1 {
                        let row = &fine_luma[y * fine_w..];
                        for &v in &row[x0..x1] {
                            sum += u32::from(v);
                            count += 1;
                        }
                    }
                    coarse_luma[cy * coarse_w + cx] = (sum / count.max(1)) as u8;
                }
            }
        }

        Self {
            width,
            height,
            grid,
            fine_origin_x: interior.start,
            fine_step,
            fine_w,
            fine_luma,
            row_grad,
            row_vert_grad,
            row_edge_columns,
            coarse_w,
            coarse_h,
            coarse_luma,
            coarse_vert_grad,
        }
    }

    fn fine_x_range(&self, fixed_left: u32, fixed_right: u32) -> std::ops::Range<usize> {
        if fixed_left == 0 && fixed_right == 0 {
            return 0..self.fine_w;
        }
        let step = self.fine_step.max(1);
        let start_px = (fixed_left as usize).max(self.fine_origin_x);
        let max_px = self.fine_origin_x + self.fine_w * step;
        let end_px = (self.width as usize)
            .saturating_sub(fixed_right as usize)
            .min(max_px);
        let fx0 = start_px.saturating_sub(self.fine_origin_x).div_ceil(step);
        let fx1 = end_px.saturating_sub(self.fine_origin_x) / step;
        let fx0 = fx0.min(self.fine_w);
        let fx1 = fx1.clamp(fx0, self.fine_w);
        if fx1 - fx0 >= 16 {
            fx0..fx1
        } else {
            0..self.fine_w
        }
    }

    fn coarse_x_range(&self, fine_x: &std::ops::Range<usize>) -> std::ops::Range<usize> {
        let cx0 = (fine_x.start / COARSE_STRIDE).min(self.coarse_w);
        let cx1 = fine_x.end.div_ceil(COARSE_STRIDE).clamp(cx0, self.coarse_w);
        if cx1 - cx0 >= 4 {
            cx0..cx1
        } else {
            0..self.coarse_w
        }
    }

    fn row_vert_grad_in(&self, y: usize, fine_x: &std::ops::Range<usize>) -> f32 {
        if y == 0 || y >= self.height as usize || fine_x.is_empty() {
            return 0.0;
        }
        if fine_x.start == 0 && fine_x.end == self.fine_w {
            return self.row_vert_grad[y];
        }
        let span = fine_x.len();
        let prev_row =
            &self.fine_luma[(y - 1) * self.fine_w + fine_x.start..(y - 1) * self.fine_w + fine_x.end];
        let curr_row = &self.fine_luma[y * self.fine_w + fine_x.start..y * self.fine_w + fine_x.end];
        let sum: u32 = prev_row
            .iter()
            .zip(curr_row)
            .map(|(a, b)| u32::from(a.abs_diff(*b)))
            .sum();
        sum as f32 / span as f32
    }

    #[allow(dead_code)]
    fn matches_grid(&self, other: &Self) -> bool {
        self.grid == other.grid
    }

    fn has_broad_texture(&self, y: usize) -> bool {
        self.row_grad[y] >= 1.0 && self.row_edge_columns[y] * 12 >= self.fine_w as u32
    }

    fn has_sticky_row_texture(&self, y: usize, fine_x: &std::ops::Range<usize>) -> bool {
        let has_horiz = if fine_x.start == 0 && fine_x.end == self.fine_w {
            self.has_broad_texture(y)
        } else {
            let span = fine_x.len();
            if span <= 1 {
                false
            } else {
                let row = &self.fine_luma[y * self.fine_w + fine_x.start..y * self.fine_w + fine_x.end];
                let mut grad_sum = 0u32;
                let mut edge_cols = 0u32;
                for pair in row.windows(2) {
                    let delta = u32::from(pair[1].abs_diff(pair[0]));
                    grad_sum += delta;
                    if delta >= 12 {
                        edge_cols += 1;
                    }
                }
                (grad_sum as f32 / (span - 1) as f32) >= 1.0 && edge_cols * 12 >= span as u32
            }
        };
        if !has_horiz {
            return false;
        }
        self.row_vert_grad_in(y, fine_x) >= 1.0
            || (y + 1 < self.height as usize && self.row_vert_grad_in(y + 1, fine_x) >= 1.0)
    }

    /// Reject pure vertical stripes (e.g. git-graph branch rails) whose columns
    /// have high horizontal contrast but zero vertical variation across rows.
    fn band_has_vertical_texture_in(
        &self,
        y0: usize,
        y1: usize,
        fine_x: &std::ops::Range<usize>,
    ) -> bool {
        let start = y0.max(1);
        let end = y1.min(self.height as usize);
        if end <= start {
            return false;
        }
        let span = end - start;
        let textured = (start..end)
            .filter(|&y| self.row_vert_grad_in(y, fine_x) >= 1.5)
            .count();
        if span < 12 {
            textured >= 2
        } else {
            textured >= 3 && textured * 4 >= span
        }
    }

    fn row_bytes(&self) -> usize {
        self.width as usize * 4
    }
}

fn raw_rows(image: &RgbaImage, y0: u32, y1: u32) -> &[u8] {
    let stride = image.width() as usize * 4;
    let start = y0 as usize * stride;
    let end = (y1 as usize * stride).min(image.as_raw().len());
    &image.as_raw()[start..end]
}

fn interior_columns(width: usize, scale: f32) -> std::ops::Range<usize> {
    let margin_l = ((4.0 * scale).round() as usize).min(width / 16);
    let margin_r = ((20.0 * scale).round() as usize).min(width / 6);
    let start = margin_l;
    let end = width.saturating_sub(margin_r).max(start + 1).min(width);
    start..end
}

fn luma(r: u8, g: u8, b: u8) -> u8 {
    ((u16::from(r) * 77 + u16::from(g) * 150 + u16::from(b) * 29) >> 8) as u8
}

pub(crate) fn sample_grid(raw: &[u8], w: usize, h: usize) -> [u32; 64] {
    let mut grid = [0u32; 64];
    if w == 0 || h == 0 {
        return grid;
    }
    for gy in 0..8 {
        let y = ((gy * 2 + 1) * h / 16).min(h - 1);
        for gx in 0..8 {
            let x = ((gx * 2 + 1) * w / 16).min(w - 1);
            let p = (y * w + x) * 4;
            grid[gy * 8 + gx] =
                (u32::from(raw[p]) << 16) | (u32::from(raw[p + 1]) << 8) | u32::from(raw[p + 2]);
        }
    }
    grid
}

fn cached_band_weights(
    slot: &mut Option<Arc<BandWeights>>,
    profile: &FrameProfile,
    fine_x: &std::ops::Range<usize>,
) -> Arc<BandWeights> {
    if let Some(cached) = slot
        && &cached.fine_x == fine_x
    {
        return cached.clone();
    }
    let computed = Arc::new(BandWeights::new(profile, fine_x));
    *slot = Some(computed.clone());
    computed
}

#[derive(Clone)]
struct CommittedAnchor {
    profile: Arc<FrameProfile>,
    /// Virtual y coordinate (in canvas active-band space) where `profile`'s
    /// `active_top` currently lands.
    y: i32,
    weights_cache: Option<Arc<BandWeights>>,
}

impl CommittedAnchor {
    fn new(profile: Arc<FrameProfile>, y: i32) -> Self {
        Self {
            profile,
            y,
            weights_cache: None,
        }
    }

    fn weights_for(&mut self, fine_x: &std::ops::Range<usize>) -> Arc<BandWeights> {
        cached_band_weights(&mut self.weights_cache, &self.profile, fine_x)
    }
}

#[derive(Clone)]
struct PendingAnchor {
    profile: Arc<FrameProfile>,
    image: Arc<PooledImage>,
    /// Virtual y coordinate (in canvas active-band space) where `profile`'s
    /// `active_top` currently lands.
    y: i32,
    weights_cache: Option<Arc<BandWeights>>,
}

impl PendingAnchor {
    fn new(profile: Arc<FrameProfile>, image: Arc<PooledImage>, y: i32) -> Self {
        Self {
            profile,
            image,
            y,
            weights_cache: None,
        }
    }

    fn weights_for(&mut self, fine_x: &std::ops::Range<usize>) -> Arc<BandWeights> {
        cached_band_weights(&mut self.weights_cache, &self.profile, fine_x)
    }
}

struct SidebarLayout {
    width: u32,
    top_h: u32,
    bottom_h: u32,
    top_rows: Vec<u8>,
    fill_row: Vec<u8>,
    bottom_rows: Vec<u8>,
}

#[derive(Default)]
struct SidebarScratch {
    col_diff_sum: Vec<u32>,
    col_moved_rows: Vec<usize>,
    col_init_diff_sum: Vec<u32>,
    col_init_moved_rows: Vec<usize>,
    col_vert_sum: Vec<u32>,
    col_vert_edges: Vec<usize>,
    col_horiz_edges: Vec<usize>,
    prev_moving: Vec<bool>,
}

impl SidebarScratch {
    fn reset(&mut self, len: usize) {
        self.col_diff_sum.resize(len, 0);
        self.col_diff_sum.fill(0);
        self.col_moved_rows.resize(len, 0);
        self.col_moved_rows.fill(0);
        self.col_init_diff_sum.resize(len, 0);
        self.col_init_diff_sum.fill(0);
        self.col_init_moved_rows.resize(len, 0);
        self.col_init_moved_rows.fill(0);
        self.col_vert_sum.resize(len, 0);
        self.col_vert_sum.fill(0);
        self.col_vert_edges.resize(len, 0);
        self.col_vert_edges.fill(0);
        self.col_horiz_edges.resize(len, 0);
        self.col_horiz_edges.fill(0);
        self.prev_moving.resize(len, false);
        self.prev_moving.fill(false);
    }
}

/// Real-time stateful stitcher for one scrolling screenshot session.
pub struct Stitcher {
    config: StitchConfig,
    width: u32,
    frame_height: u32,
    fixed_top: u32,
    fixed_bottom: u32,
    cand_top_votes: Option<(u32, u32)>,
    cand_bottom_votes: Option<(u32, u32)>,
    fixed_left: u32,
    fixed_right: u32,
    cand_left_votes: Option<(u32, u32)>,
    cand_right_votes: Option<(u32, u32)>,
    left_sidebar: Option<SidebarLayout>,
    right_sidebar: Option<SidebarLayout>,
    initial_fine_luma: Vec<u8>,
    col_moved_hits: Vec<u8>,
    col_static_streak: Vec<u8>,
    sidebar_scratch: SidebarScratch,
    margin_recheck_skip: u8,
    sidebar_recheck_skip: u8,
    header_rows: Vec<u8>,
    footer_rows: Vec<u8>,
    rows: PagedRows,
    below_rows: u32,
    above_rows: u32,
    committed: Option<CommittedAnchor>,
    prev: Option<PendingAnchor>,
    viewport_y: i32,
    last_dy: Option<i32>,
    direction: ScrollDirection,
    upward_established: bool,
    frames: usize,
    preview_revision: u64,
    preview_range: (u32, u32),
    preview_follow: bool,
    preview_window_rows: u32,
    preview: Option<PreviewTile>,
    preview_dirty: bool,
    preview_dirty_range: Option<(u32, u32)>,
    preview_row_scratch: Vec<u8>,
}

impl Stitcher {
    pub fn new(config: StitchConfig) -> Self {
        Self {
            config,
            width: 0,
            frame_height: 0,
            fixed_top: 0,
            fixed_bottom: 0,
            cand_top_votes: None,
            cand_bottom_votes: None,
            fixed_left: 0,
            fixed_right: 0,
            cand_left_votes: None,
            cand_right_votes: None,
            left_sidebar: None,
            right_sidebar: None,
            initial_fine_luma: Vec::new(),
            col_moved_hits: Vec::new(),
            col_static_streak: Vec::new(),
            sidebar_scratch: SidebarScratch::default(),
            margin_recheck_skip: 0,
            sidebar_recheck_skip: 0,
            header_rows: Vec::new(),
            footer_rows: Vec::new(),
            rows: PagedRows::new(0),
            below_rows: 0,
            above_rows: 0,
            committed: None,
            prev: None,
            viewport_y: 0,
            last_dy: None,
            direction: ScrollDirection::Down,
            upward_established: false,
            frames: 0,
            preview_revision: 0,
            preview_range: (0, 1),
            preview_follow: false,
            preview_window_rows: 0,
            preview: None,
            preview_dirty: true,
            preview_dirty_range: Some((0, u32::MAX)),
            preview_row_scratch: Vec::new(),
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn frame_height(&self) -> u32 {
        self.frame_height
    }

    pub fn total_height(&self) -> u32 {
        self.fixed_top + self.above_rows + self.below_rows + self.fixed_bottom
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    pub fn direction(&self) -> ScrollDirection {
        self.direction
    }

    pub fn fixed_margins(&self) -> (u32, u32) {
        (self.fixed_top, self.fixed_bottom)
    }

    pub fn fixed_sidebars(&self) -> (u32, u32) {
        (self.fixed_left, self.fixed_right)
    }

    pub(crate) fn active_height(&self) -> u32 {
        self.frame_height
            .saturating_sub(self.fixed_top + self.fixed_bottom)
    }

    pub(crate) fn preview_dirty(&self) -> bool {
        self.preview_dirty
    }

    pub fn content_shift(&self) -> u32 {
        self.above_rows
    }

    pub fn viewport_origin_y(&self) -> i64 {
        self.above_rows as i64 + self.viewport_y as i64
    }

    pub fn annotation_frame(&mut self) -> Result<Option<(RgbaImage, i64)>> {
        self.annotation_frame_at(self.viewport_origin_y())
    }

    pub fn annotation_frame_at(&mut self, origin: i64) -> Result<Option<(RgbaImage, i64)>> {
        if self.width == 0 || self.frame_height == 0 {
            return Ok(None);
        }
        let origin = origin.clamp(
            0,
            self.total_height().saturating_sub(self.frame_height) as i64,
        );
        let stride = self.width as usize * 4;
        let start_y = origin as u32;
        let end_y = start_y + self.frame_height;
        let active_start = self.fixed_top;
        let active_end = self.total_height() - self.fixed_bottom;
        let mut pixels = vec![0u8; self.frame_height as usize * stride];
        let mut dst_row = 0usize;

        if start_y < active_start {
            let take = active_start.min(end_y) - start_y;
            let src_off = start_y as usize * stride;
            let bytes = take as usize * stride;
            pixels[..bytes].copy_from_slice(&self.header_rows[src_off..src_off + bytes]);
            dst_row += take as usize;
        }
        let body_start = start_y.max(active_start);
        let body_end = end_y.min(active_end);
        if body_start < body_end {
            let take = body_end - body_start;
            let canvas_y = (body_start - active_start) as i32 - self.above_rows as i32;
            let dst_off = dst_row * stride;
            let bytes = take as usize * stride;
            self.rows
                .copy_to(canvas_y, take, &mut pixels[dst_off..dst_off + bytes])?;
            self.overlay_sidebars_on_active_rows(
                body_start - active_start,
                take,
                &mut pixels[dst_off..dst_off + bytes],
            );
            dst_row += take as usize;
        }
        if end_y > active_end {
            let foot_start = start_y.max(active_end);
            let take = end_y - foot_start;
            let src_off = (foot_start - active_end) as usize * stride;
            let dst_off = dst_row * stride;
            let bytes = take as usize * stride;
            pixels[dst_off..dst_off + bytes]
                .copy_from_slice(&self.footer_rows[src_off..src_off + bytes]);
        }
        Ok(RgbaImage::from_raw(self.width, self.frame_height, pixels).map(|image| (image, origin)))
    }

    pub fn preview_revision(&self) -> u64 {
        self.preview_revision
    }

    pub fn preview_tile(&self) -> Option<PreviewTile> {
        self.preview.clone()
    }

    pub fn preview_range(&self) -> (u32, u32) {
        self.preview_range
    }

    pub fn set_preview_range(&mut self, start: u32, rows: u32) {
        self.preview_follow = false;
        self.preview_window_rows = rows.max(1);
        let height = self.total_height();
        let start = if height == 0 {
            0
        } else {
            start.min(height.saturating_sub(1))
        };
        let rows = rows.max(1).min(height.saturating_sub(start).max(1));
        if self.preview_range != (start, rows) {
            self.preview_range = (start, rows);
            self.preview_dirty = true;
        }
    }

    pub fn set_preview_range_with_follow(&mut self, start: u32, rows: u32, follow: bool) {
        self.preview_follow = follow;
        self.preview_window_rows = rows.max(1);
        if follow {
            self.sync_live_preview_range();
        } else {
            self.set_preview_range(start, rows);
        }
    }

    fn sync_live_preview_range(&mut self) {
        let total = self.total_height();
        if total == 0 {
            return;
        }
        let window = self
            .preview_window_rows
            .max(self.frame_height.saturating_mul(2))
            .max(1)
            .min(total);
        let vp_top = self
            .viewport_origin_y()
            .clamp(0, total.saturating_sub(1) as i64) as u32;
        let vp_bottom = vp_top.saturating_add(self.frame_height).min(total);
        let (start, rows) = if vp_bottom >= total {
            let start = total.saturating_sub(window);
            (start, total - start)
        } else if vp_top == 0 {
            (0, window)
        } else {
            let pad = window.saturating_sub(self.frame_height) / 2;
            let start = vp_top.saturating_sub(pad).min(total.saturating_sub(window));
            (start, window.min(total - start))
        };
        if self.preview_range != (start, rows) {
            self.preview_range = (start, rows);
            self.preview_dirty = true;
        }
    }

    pub fn refresh_preview(&mut self) -> Result<()> {
        if !self.preview_dirty || self.width == 0 {
            return Ok(());
        }
        let total = self.total_height();
        let start = self.preview_range.0.min(total.saturating_sub(1));
        let rows = self
            .preview_range
            .1
            .max(1)
            .min(total.saturating_sub(start).max(1));
        let prev_w = self.width.clamp(1, PREVIEW_MAX_WIDTH);
        let prev_tile = self.preview.clone();
        let dirty_range = self.preview_dirty_range.take();
        let image = Arc::new(self.render_preview_slice_with_reuse(
            prev_w,
            start,
            start + rows,
            prev_tile.as_ref(),
            dirty_range,
        )?);
        self.preview_revision = NEXT_PREVIEW_REVISION.fetch_add(1, Ordering::Relaxed);
        self.preview = Some(PreviewTile {
            id: 0,
            revision: self.preview_revision,
            source_start: start,
            source_rows: rows,
            total_height: total,
            image,
        });
        self.preview_dirty = false;
        Ok(())
    }

    /// Feed one captured SDR RGBA frame. Returns what changed.
    pub fn feed(&mut self, image: RgbaImage) -> FeedOutcome {
        self.feed_shared(Arc::new(PooledImage::unpooled(image)), true)
    }

    pub(crate) fn feed_shared(
        &mut self,
        image: Arc<PooledImage>,
        refresh_preview: bool,
    ) -> FeedOutcome {
        let (w, h) = image.dimensions();
        let grid = if w == 0 || h == 0 {
            [0; 64]
        } else {
            sample_grid(image.as_raw(), w as usize, h as usize)
        };
        self.feed_shared_with_grid(image, grid, refresh_preview)
    }

    pub(crate) fn feed_shared_with_grid(
        &mut self,
        image: Arc<PooledImage>,
        grid: [u32; 64],
        refresh_preview: bool,
    ) -> FeedOutcome {
        let (w, h) = image.dimensions();
        if w == 0 || h == 0 {
            return FeedOutcome::Unchanged;
        }
        if self.committed.is_some()
            && self.prev.as_ref().is_some_and(|prev| {
                prev.profile.grid == grid && prev.image.as_raw() == image.as_raw()
            })
        {
            return FeedOutcome::Unchanged;
        }
        let curr = Arc::new(FrameProfile::from_image_with_grid(
            &image,
            self.config.scale,
            grid,
        ));
        self.feed_profile(image, curr, refresh_preview)
    }

    pub(crate) fn feed_profile(
        &mut self,
        image: Arc<PooledImage>,
        curr: Arc<FrameProfile>,
        refresh_preview: bool,
    ) -> FeedOutcome {
        let (w, h) = (curr.width, curr.height);
        if w == 0 || h == 0 {
            return FeedOutcome::Unchanged;
        }
        if self.committed.is_none() {
            return match self.initialize(image, curr) {
                Ok(()) => FeedOutcome::Initial,
                Err(error) => FeedOutcome::Failed(format!("{error:#}")),
            };
        }

        if w != self.width || h != self.frame_height {
            log::warn!(
                "scroll frame dimension changed {}x{} -> {}x{}; aborting session",
                self.width,
                self.frame_height,
                w,
                h
            );
            return FeedOutcome::DimensionsChanged {
                expected: (self.width, self.frame_height),
                actual: (w, h),
            };
        }
        if self.prev.as_ref().is_some_and(|prev| {
            prev.profile.grid == curr.grid && prev.image.as_raw() == image.as_raw()
        }) {
            return FeedOutcome::Unchanged;
        }

        self.frames += 1;
        let (eff_top, eff_bottom) =
            if let Some(prev_profile) = self.prev.as_ref().map(|p| p.profile.clone()) {
                match self.update_fixed_margins(&prev_profile, &curr, &image) {
                    Ok(margins) => margins,
                    Err(error) => return FeedOutcome::Failed(format!("{error:#}")),
                }
            } else {
                (self.fixed_top, self.fixed_bottom)
            };

        let active_top = self.fixed_top;
        let active_bottom = self.frame_height - self.fixed_bottom;
        let active_h = active_bottom.saturating_sub(active_top);
        let search_top = eff_top.max(active_top);
        let search_bottom = (self.frame_height - eff_bottom).min(active_bottom);
        let (eff_left, eff_right) =
            if let Some(prev_profile) = self.prev.as_ref().map(|p| p.profile.clone()) {
                match self.update_fixed_sidebars(
                    &prev_profile,
                    &curr,
                    &image,
                    search_top,
                    search_bottom,
                ) {
                    Ok(sidebars) => sidebars,
                    Err(error) => return FeedOutcome::Failed(format!("{error:#}")),
                }
            } else {
                (self.fixed_left, self.fixed_right)
            };
        let fine_x = curr.fine_x_range(eff_left, eff_right);
        let prev_weights = self.prev.as_mut().map(|prev| prev.weights_for(&fine_x));
        let committed_weights = self
            .committed
            .as_mut()
            .map(|anchor| anchor.weights_for(&fine_x));
        let committed_expected_dy = self
            .last_dy
            .zip(self.prev.as_ref())
            .zip(self.committed.as_ref())
            .map(|((dy, prev), comm)| (prev.y - comm.y).saturating_add(dy));
        let prev_offset = self
            .prev
            .as_ref()
            .zip(self.committed.as_ref())
            .map_or(0, |(prev, comm)| (prev.y - comm.y).abs());
        let prefer_prev = prev_offset > active_h as i32 / 2
            || committed_expected_dy.is_some_and(|dy| dy.abs() > active_h as i32 * 75 / 100);

        let (base_y, shift) = if prefer_prev {
            let prev_match = self.prev.as_ref().zip(prev_weights.as_ref()).map(|(prev, weights)| {
                (
                    prev.y,
                    estimate_shift(
                        &prev.profile,
                        &curr,
                        search_top,
                        search_bottom,
                        &fine_x,
                        self.last_dy,
                        weights,
                    ),
                )
            });
            match prev_match {
                Some((prev_y, Ok(dy))) => {
                    if let Some((prev_profile, prev_image)) =
                        self.prev.as_ref().map(|p| (p.profile.clone(), p.image.clone()))
                        && let Err(error) = self.commit_frame(prev_profile, &prev_image, prev_y)
                    {
                        return FeedOutcome::Failed(format!("{error:#}"));
                    }
                    (prev_y, dy)
                }
                Some((_, Err(prev_warn))) => {
                    let committed_match = self.committed.as_ref().zip(committed_weights.as_ref()).map(
                        |(anchor, weights)| {
                            (
                                anchor.y,
                                estimate_shift(
                                    &anchor.profile,
                                    &curr,
                                    search_top,
                                    search_bottom,
                                    &fine_x,
                                    committed_expected_dy,
                                    weights,
                                ),
                            )
                        },
                    );
                    match committed_match {
                        Some((anchor_y, Ok(dy))) => (anchor_y, dy),
                        Some((_, Err(committed_warn))) => {
                            let warning = if prev_warn == ScrollWarning::LowTexture
                                && committed_warn == ScrollWarning::LowTexture
                            {
                                ScrollWarning::LowTexture
                            } else {
                                ScrollWarning::TooFast
                            };
                            log::debug!("scroll match warning: {warning:?}");
                            return FeedOutcome::Warning(warning);
                        }
                        None => return FeedOutcome::Warning(prev_warn),
                    }
                }
                None => unreachable!(),
            }
        } else {
            // Try matching against committed anchor first (zero drift); if out of
            // overlap range, match against previous frame and commit previous first.
            let committed_match =
                self.committed
                    .as_ref()
                    .zip(committed_weights.as_ref())
                    .map(|(anchor, weights)| {
                        let match_res = estimate_shift(
                            &anchor.profile,
                            &curr,
                            search_top,
                            search_bottom,
                            &fine_x,
                            committed_expected_dy,
                            weights,
                        );
                        (anchor.y, match_res)
                    });

            match committed_match {
                Some((anchor_y, Ok(dy))) => {
                    let reverses_direction = self.last_dy.is_some_and(|prev_dy| {
                        prev_dy != 0
                            && self.prev.as_ref().is_some_and(|prev| {
                                let step = (anchor_y + dy) - prev.y;
                                step != 0 && step.signum() != prev_dy.signum()
                            })
                    });
                    if reverses_direction
                        && let Some((prev_y, Ok(p_dy))) =
                            self.prev.as_ref().zip(prev_weights.as_ref()).map(|(prev, weights)| {
                                (
                                    prev.y,
                                    estimate_shift(
                                        &prev.profile,
                                        &curr,
                                        search_top,
                                        search_bottom,
                                        &fine_x,
                                        self.last_dy,
                                        weights,
                                    ),
                                )
                            })
                        && self
                            .last_dy
                            .is_some_and(|prev_dy| p_dy != 0 && p_dy.signum() == prev_dy.signum())
                    {
                        if let Some((prev_profile, prev_image)) =
                            self.prev.as_ref().map(|p| (p.profile.clone(), p.image.clone()))
                            && let Err(error) = self.commit_frame(prev_profile, &prev_image, prev_y)
                        {
                            return FeedOutcome::Failed(format!("{error:#}"));
                        }
                        (prev_y, p_dy)
                    } else {
                        (anchor_y, dy)
                    }
                }
                Some((_, Err(committed_warn))) => {
                    let prev_match =
                        self.prev.as_ref().zip(prev_weights.as_ref()).map(|(prev, weights)| {
                            (
                                prev.y,
                                estimate_shift(
                                    &prev.profile,
                                    &curr,
                                    search_top,
                                    search_bottom,
                                    &fine_x,
                                    self.last_dy,
                                    weights,
                                ),
                            )
                        });
                    match prev_match {
                        Some((prev_y, Ok(dy))) => {
                            if let Some((prev_profile, prev_image)) =
                                self.prev.as_ref().map(|p| (p.profile.clone(), p.image.clone()))
                                && let Err(error) =
                                    self.commit_frame(prev_profile, &prev_image, prev_y)
                            {
                                return FeedOutcome::Failed(format!("{error:#}"));
                            }
                            (prev_y, dy)
                        }
                        Some((_, Err(prev_warn))) => {
                            let warning = if prev_warn == ScrollWarning::LowTexture
                                && committed_warn == ScrollWarning::LowTexture
                            {
                                ScrollWarning::LowTexture
                            } else {
                                ScrollWarning::TooFast
                            };
                            log::debug!("scroll match warning: {warning:?}");
                            return FeedOutcome::Warning(warning);
                        }
                        None => return FeedOutcome::Warning(committed_warn),
                    }
                }
                None => unreachable!(),
            }
        };

        if shift == 0 {
            self.prev = Some(PendingAnchor::new(curr, image, base_y));
            return FeedOutcome::Unchanged;
        }

        let next_y = base_y + shift;
        let step_from_prev = next_y - self.viewport_y;
        let before_h = self.total_height();
        let pending_direction = self
            .prev
            .as_ref()
            .zip(self.committed.as_ref())
            .map(|(prev, committed)| (prev.y - committed.y).signum())
            .filter(|direction| *direction != 0);
        if pending_direction
            .is_some_and(|direction| step_from_prev != 0 && direction != step_from_prev.signum())
        {
            let (prev_profile, prev_image, prev_y) = {
                let previous = self.prev.as_ref().expect("checked pending anchor");
                (previous.profile.clone(), previous.image.clone(), previous.y)
            };
            if let Err(error) = self.commit_frame(prev_profile, &prev_image, prev_y) {
                return FeedOutcome::Failed(format!("{error:#}"));
            }
        }
        if step_from_prev > 0 {
            self.direction = ScrollDirection::Down;
            self.last_dy = Some(step_from_prev);
        } else if step_from_prev < 0 {
            self.direction = ScrollDirection::Up;
            self.last_dy = Some(step_from_prev);
        }
        self.viewport_y = next_y;

        // Hysteresis: wait until enough scroll accumulates relative to the
        // committed anchor before splicing pixels into the canvas.
        let commit_threshold = (active_h as i32 / 3).clamp(48, 480);
        let anchor_y = self.committed.as_ref().map_or(0, |a| a.y);
        let delta_from_anchor = next_y - anchor_y;

        if delta_from_anchor.abs() >= commit_threshold {
            if let Err(error) = self.commit_frame(curr.clone(), &image, next_y) {
                return FeedOutcome::Failed(format!("{error:#}"));
            }
        } else {
            // Even before committing a brand new anchor, update footer if we
            // are at the bottommost position so preview/end stays fresh.
            if next_y + active_h as i32 >= self.below_rows as i32 {
                self.update_footer(&image);
                if self.fixed_bottom > 0 {
                    let footer_start = self.total_height() - self.fixed_bottom;
                    if let Err(error) =
                        self.refresh_preview_range(footer_start, self.total_height())
                    {
                        return FeedOutcome::Failed(format!("{error:#}"));
                    }
                }
            }
            if next_y <= -(self.above_rows as i32) {
                self.update_header(&image);
                if self.fixed_top > 0
                    && let Err(error) = self.refresh_preview_range(0, self.fixed_top)
                {
                    return FeedOutcome::Failed(format!("{error:#}"));
                }
            }
        }

        self.prev = Some(PendingAnchor::new(curr, image, next_y));
        if self.preview_follow {
            self.sync_live_preview_range();
        }
        if refresh_preview && let Err(error) = self.refresh_preview() {
            return FeedOutcome::Failed(format!("{error:#}"));
        }

        let after_h = self.total_height();
        if after_h != before_h {
            FeedOutcome::Stitched {
                dy: step_from_prev,
                total_height: after_h,
            }
        } else {
            FeedOutcome::Moved { dy: step_from_prev }
        }
    }

    /// Commit any un-spliced trailing/leading scroll delta (called after idle or
    /// right before assembling the final image).
    pub fn flush_pending(&mut self) -> Result<bool> {
        let Some(prev) = self.prev.as_ref() else {
            return Ok(false);
        };
        let committed_y = self.committed.as_ref().map_or(prev.y, |a| a.y);
        if prev.y == committed_y {
            return Ok(false);
        }
        let before = self.total_height();
        let profile = prev.profile.clone();
        let image = prev.image.clone();
        let y = prev.y;
        self.commit_frame(profile, &image, y)?;
        Ok(self.total_height() != before)
    }

    /// Assemble the complete stitched RGBA image. Intended for tests and small
    /// legacy callers; production uses `assemble_disk` to keep RAM bounded.
    pub fn assemble(&mut self) -> Result<Option<RgbaImage>> {
        self.flush_pending()?;
        if self.width == 0 || self.total_height() == 0 {
            return Ok(None);
        }
        let stride = self.width as usize * 4;
        let total_h = self.total_height();
        let mut out = Vec::with_capacity(total_h as usize * stride);
        out.extend_from_slice(&self.header_rows);
        let active_rows = self.above_rows + self.below_rows;
        if active_rows > 0 {
            let mut active = vec![0u8; active_rows as usize * stride];
            self.rows
                .copy_to(-(self.above_rows as i32), active_rows, &mut active)?;
            self.overlay_sidebars_on_active_rows(0, active_rows, &mut active);
            out.extend_from_slice(&active);
        }
        out.extend_from_slice(&self.footer_rows);
        Ok(RgbaImage::from_raw(self.width, total_h, out))
    }

    pub fn assemble_disk(&mut self) -> Result<Option<DiskImage>> {
        self.flush_pending()?;
        if self.width == 0 || self.total_height() == 0 {
            return Ok(None);
        }
        let output = DiskImage::create(self.width, self.total_height())?;
        if self.fixed_top > 0 {
            output.write_raw_rows_at(0, self.fixed_top, &self.header_rows)?;
        }
        let active_rows = self.above_rows + self.below_rows;
        if active_rows > 0 {
            let stride = self.width as usize * 4;
            let batch_rows = ((8 * 1024 * 1024 / stride.max(1)).max(1) as u32).min(active_rows);
            let mut buffer = vec![0u8; batch_rows as usize * stride];
            let top = -(self.above_rows as i32);
            for start in (0..active_rows).step_by(batch_rows as usize) {
                let rows = batch_rows.min(active_rows - start);
                let len = rows as usize * stride;
                self.rows
                    .copy_to(top + start as i32, rows, &mut buffer[..len])?;
                self.overlay_sidebars_on_active_rows(start, rows, &mut buffer[..len]);
                output.write_raw_rows_at(self.fixed_top + start, rows, &buffer[..len])?;
            }
        }
        if self.fixed_bottom > 0 {
            output.write_raw_rows_at(
                self.total_height() - self.fixed_bottom,
                self.fixed_bottom,
                &self.footer_rows,
            )?;
        }
        Ok(Some(output))
    }

    fn initialize(&mut self, first_image: Arc<PooledImage>, first: Arc<FrameProfile>) -> Result<()> {
        self.width = first.width;
        self.frame_height = first.height;
        self.frames = 1;
        self.rows = PagedRows::new(first.width);
        self.rows.append_down(0, first_image.as_raw())?;
        self.below_rows = first.height;
        self.above_rows = 0;
        self.header_rows.clear();
        self.footer_rows.clear();
        self.fixed_top = 0;
        self.fixed_bottom = 0;
        self.cand_top_votes = None;
        self.cand_bottom_votes = None;
        self.fixed_left = 0;
        self.fixed_right = 0;
        self.cand_left_votes = None;
        self.cand_right_votes = None;
        self.left_sidebar = None;
        self.right_sidebar = None;
        self.initial_fine_luma = first.fine_luma.clone();
        self.col_moved_hits = vec![0u8; first.fine_w];
        self.col_static_streak = vec![0u8; first.fine_w];
        self.viewport_y = 0;
        self.last_dy = None;
        self.upward_established = false;
        self.committed = Some(CommittedAnchor::new(first.clone(), 0));
        self.prev = Some(PendingAnchor::new(first, first_image, 0));
        self.margin_recheck_skip = 0;
        self.sidebar_recheck_skip = 0;
        self.preview_dirty_range = Some((0, u32::MAX));
        self.preview_dirty = true;
        self.refresh_preview()
    }

    fn votes_settled(votes: Option<(u32, u32)>, current: u32, tolerance: u32) -> bool {
        let Some((v, n)) = votes else {
            return false;
        };
        if n < MARGIN_CHANGE_VOTES {
            return false;
        }
        if v == 0 || current == 0 {
            v == current
        } else {
            v.abs_diff(current) <= tolerance
        }
    }

    /// Update consensus on fixed top/bottom sticky bars. Only textured static
    /// bands qualify so plain whitespace is never misclassified.
    /// Returns temporary `(search_top, search_bottom)` margins to shield shift
    /// estimation even on the very first moving frame.
    fn update_fixed_margins(
        &mut self,
        prev: &FrameProfile,
        curr: &FrameProfile,
        curr_image: &RgbaImage,
    ) -> Result<(u32, u32)> {
        if self.frames > 24
            && Self::votes_settled(self.cand_top_votes, self.fixed_top, 4)
            && Self::votes_settled(self.cand_bottom_votes, self.fixed_bottom, 4)
            && self.margin_recheck_skip < 3
        {
            self.margin_recheck_skip += 1;
            return Ok((self.fixed_top, self.fixed_bottom));
        }
        self.margin_recheck_skip = 0;

        let h = self.frame_height as usize;
        if h < 96 || prev.fine_w == 0 {
            return Ok((self.fixed_top, self.fixed_bottom));
        }
        let max_band = (h * 35 / 100).min(h / 2 - 24);
        if max_band < 8 {
            return Ok((self.fixed_top, self.fixed_bottom));
        }
        let fine_x = curr.fine_x_range(self.fixed_left, self.fixed_right);
        let fine_x = if fine_x.is_empty() {
            0..prev.fine_w
        } else {
            fine_x
        };
        let fx0 = fine_x.start;
        let fx1 = fine_x.end;
        let fine_span = (fx1 - fx0).max(1);
        // Quick strided check across the frame so static periods do not compute
        // full-height row differences.
        let mut sampled_moving = 0usize;
        let mut sampled_total = 0usize;
        for y in (0..h).step_by(4) {
            let p_row = &prev.fine_luma[y * prev.fine_w + fx0..y * prev.fine_w + fx1];
            let c_row = &curr.fine_luma[y * curr.fine_w + fx0..y * curr.fine_w + fx1];
            let sum: u32 = p_row
                .iter()
                .zip(c_row)
                .map(|(a, b)| u32::from(a.abs_diff(*b)))
                .sum();
            if sum as f32 / fine_span as f32 > 5.0 {
                sampled_moving += 1;
            }
            sampled_total += 1;
        }
        if sampled_moving * 6 < sampled_total {
            return Ok((self.fixed_top, self.fixed_bottom));
        }

        let row_diff_at = |y: usize| -> f32 {
            let p_row = &prev.fine_luma[y * prev.fine_w + fx0..y * prev.fine_w + fx1];
            let c_row = &curr.fine_luma[y * curr.fine_w + fx0..y * curr.fine_w + fx1];
            let sum: u32 = p_row
                .iter()
                .zip(c_row)
                .map(|(a, b)| u32::from(a.abs_diff(*b)))
                .sum();
            sum as f32 / fine_span as f32
        };

        // Top sticky scan: stop at 4 consecutive moving rows. Include textured
        // static rows plus up to 8 uniform padding/border rows immediately
        // before motion starts.
        let mut cand_top = 0usize;
        let mut seen_texture = false;
        let mut last_textured = 0usize;
        let mut consecutive_moving = 0usize;
        for y in 0..max_band {
            let diff = row_diff_at(y);
            if diff > 5.0 {
                consecutive_moving += 1;
                if consecutive_moving >= 4 {
                    break;
                }
            } else {
                consecutive_moving = 0;
                if diff <= 2.5 {
                    if curr.has_sticky_row_texture(y, &fine_x) {
                        seen_texture = true;
                        last_textured = y + 1;
                        cand_top = y + 1;
                    } else if seen_texture && y + 1 - last_textured <= 8 {
                        cand_top = y + 1;
                    }
                }
            }
        }
        if cand_top > 0 && !curr.band_has_vertical_texture_in(0, cand_top, &fine_x) {
            cand_top = 0;
        }

        // Bottom sticky scan: symmetric upward scan.
        let mut cand_bottom = 0usize;
        seen_texture = false;
        last_textured = 0;
        consecutive_moving = 0;
        for offset in 0..max_band {
            let y = h - 1 - offset;
            let diff = row_diff_at(y);
            if diff > 5.0 {
                consecutive_moving += 1;
                if consecutive_moving >= 4 {
                    break;
                }
            } else {
                consecutive_moving = 0;
                if diff <= 2.5 {
                    if curr.has_sticky_row_texture(y, &fine_x) {
                        seen_texture = true;
                        last_textured = offset + 1;
                        cand_bottom = offset + 1;
                    } else if seen_texture && offset + 1 - last_textured <= 8 {
                        cand_bottom = offset + 1;
                    }
                }
            }
        }
        if cand_bottom > 0 && !curr.band_has_vertical_texture_in(h - cand_bottom, h, &fine_x) {
            cand_bottom = 0;
        }

        let cand_top = cand_top as u32;
        let cand_bottom = cand_bottom as u32;

        let previous_top_votes = self.cand_top_votes;
        let previous_bottom_votes = self.cand_bottom_votes;
        self.cand_top_votes = Self::next_margin_vote(self.cand_top_votes, cand_top);
        self.cand_bottom_votes = Self::next_margin_vote(self.cand_bottom_votes, cand_bottom);
        if self.cand_top_votes != previous_top_votes
            || self.cand_bottom_votes != previous_bottom_votes
        {
            log::debug!(
                "scroll fixed margin votes: top={:?} bottom={:?} (fixed={:?})",
                self.cand_top_votes,
                self.cand_bottom_votes,
                (self.fixed_top, self.fixed_bottom)
            );
        }

        let top_votes = if self.fixed_top == 0 {
            INITIAL_MARGIN_VOTES
        } else {
            MARGIN_CHANGE_VOTES
        };
        let bottom_votes = if self.fixed_bottom == 0 {
            INITIAL_MARGIN_VOTES
        } else {
            MARGIN_CHANGE_VOTES
        };
        let next_top = match self.cand_top_votes {
            Some((v, n)) if n >= top_votes => v,
            _ => self.fixed_top,
        };
        let next_bottom = match self.cand_bottom_votes {
            Some((v, n)) if n >= bottom_votes => v,
            _ => self.fixed_bottom,
        };

        let min_active = (self.frame_height / 3).max(64);
        if (next_top != self.fixed_top || next_bottom != self.fixed_bottom)
            && next_top + next_bottom + min_active <= self.frame_height
        {
            self.flush_pending()?;
            self.apply_fixed_margins(next_top, next_bottom, curr_image)?;
        }

        let eff_top = self.fixed_top.max(cand_top);
        let eff_bottom = self.fixed_bottom.max(cand_bottom);
        if eff_top + eff_bottom + min_active <= self.frame_height {
            Ok((eff_top, eff_bottom))
        } else {
            Ok((self.fixed_top, self.fixed_bottom))
        }
    }

    fn next_margin_vote(votes: Option<(u32, u32)>, candidate: u32) -> Option<(u32, u32)> {
        Self::next_margin_vote_tol(votes, candidate, 4)
    }

    fn next_margin_vote_tol(
        votes: Option<(u32, u32)>,
        candidate: u32,
        tolerance: u32,
    ) -> Option<(u32, u32)> {
        if let Some((v, n)) = votes {
            let compatible = if v == 0 || candidate == 0 {
                v == candidate
            } else {
                v.abs_diff(candidate) <= tolerance
            };
            if compatible {
                return Some((v, (n + 1).min(MARGIN_CHANGE_VOTES)));
            }
        }
        Some((candidate, 1))
    }

    fn next_sidebar_vote(
        votes: Option<(u32, u32)>,
        candidate: u32,
        tolerance: u32,
    ) -> Option<(u32, u32)> {
        if let Some((v, n)) = votes {
            let compatible = if v == 0 || candidate == 0 {
                v == candidate
            } else {
                v.abs_diff(candidate) <= tolerance
            };
            if compatible {
                let stored = if v > 0 && candidate > 0 {
                    v.min(candidate)
                } else {
                    v
                };
                return Some((stored, (n + 1).min(MARGIN_CHANGE_VOTES)));
            }
        }
        Some((candidate, 1))
    }

    /// Detect fixed vertical sidebars (left navigation / right outline) that stay
    /// static while the center active band scrolls vertically. Only textured
    /// static sidebars qualify so uniform page margins are left untouched.
    fn update_fixed_sidebars(
        &mut self,
        prev: &FrameProfile,
        curr: &FrameProfile,
        curr_image: &RgbaImage,
        search_top: u32,
        search_bottom: u32,
    ) -> Result<(u32, u32)> {
        let active_h = search_bottom.saturating_sub(search_top) as usize;
        if self.width < 160 || active_h < 48 || prev.fine_w < 32 {
            return Ok((self.fixed_left, self.fixed_right));
        }

        let side_tol = (prev.fine_step as u32 * 2).max(12);
        if self.frames > 24
            && Self::votes_settled(self.cand_left_votes, self.fixed_left, side_tol)
            && Self::votes_settled(self.cand_right_votes, self.fixed_right, side_tol)
            && self.sidebar_recheck_skip < 3
        {
            self.sidebar_recheck_skip += 1;
            return Ok((self.fixed_left, self.fixed_right));
        }
        self.sidebar_recheck_skip = 0;

        let fine_w = prev.fine_w;
        if self.col_moved_hits.len() != fine_w {
            self.col_moved_hits.resize(fine_w, 0);
        }
        if self.col_static_streak.len() != fine_w {
            self.col_static_streak.resize(fine_w, 0);
        }
        let step_y = (active_h / 96).max(1);
        let top_u = search_top as usize;
        let bot_u = search_bottom as usize;
        let aligned_top = {
            let rem = top_u % step_y;
            if rem == 0 {
                top_u
            } else {
                top_u + step_y - rem
            }
        };
        let y_start = if aligned_top < bot_u && (bot_u - aligned_top).div_ceil(step_y) >= 4 {
            aligned_top
        } else {
            top_u
        };
        let has_init = self.initial_fine_luma.len() == curr.fine_luma.len();
        let mut sampled_rows = 0usize;
        let mut scratch = std::mem::take(&mut self.sidebar_scratch);
        scratch.reset(fine_w);
        let col_diff_sum = &mut scratch.col_diff_sum;
        let col_moved_rows = &mut scratch.col_moved_rows;
        let col_init_diff_sum = &mut scratch.col_init_diff_sum;
        let col_init_moved_rows = &mut scratch.col_init_moved_rows;
        let col_vert_sum = &mut scratch.col_vert_sum;
        let col_vert_edges = &mut scratch.col_vert_edges;
        let col_horiz_edges = &mut scratch.col_horiz_edges;
        let prev_moving = &mut scratch.prev_moving;
        let mut prev_sample_row: Option<usize> = None;

        for y in (y_start..bot_u).step_by(step_y) {
            let p_row = &prev.fine_luma[y * fine_w..(y + 1) * fine_w];
            let c_row = &curr.fine_luma[y * fine_w..(y + 1) * fine_w];
            for fx in 0..fine_w {
                let d = p_row[fx].abs_diff(c_row[fx]);
                col_diff_sum[fx] += u32::from(d);
                if d >= 12 {
                    col_moved_rows[fx] += 1;
                }
                if fx > 0 && c_row[fx].abs_diff(c_row[fx - 1]) >= 10 {
                    col_horiz_edges[fx] += 1;
                }
            }
            if has_init {
                let i_row = &self.initial_fine_luma[y * fine_w..(y + 1) * fine_w];
                for fx in 0..fine_w {
                    let d = i_row[fx].abs_diff(c_row[fx]);
                    col_init_diff_sum[fx] += u32::from(d);
                    if d >= 12 {
                        col_init_moved_rows[fx] += 1;
                    }
                }
            }
            if let Some(py) = prev_sample_row {
                let prev_c_row = &curr.fine_luma[py * fine_w..(py + 1) * fine_w];
                for fx in 0..fine_w {
                    let vd = c_row[fx].abs_diff(prev_c_row[fx]);
                    col_vert_sum[fx] += u32::from(vd);
                    if vd >= 10 {
                        col_vert_edges[fx] += 1;
                    }
                }
            }
            prev_sample_row = Some(y);
            sampled_rows += 1;
        }
        if sampled_rows < 4 {
            self.sidebar_scratch = scratch;
            return Ok((self.fixed_left, self.fixed_right));
        }

        let mut moving_cols = 0usize;
        for fx in 0..fine_w {
            let col_diff = col_diff_sum[fx] as f32 / sampled_rows as f32;
            let moving = col_diff > 4.0 || col_moved_rows[fx] * 100 >= sampled_rows * 8;
            prev_moving[fx] = moving;
            if moving {
                moving_cols += 1;
            }
        }
        if moving_cols * 20 < fine_w * 3 {
            self.sidebar_scratch = scratch;
            return Ok((self.fixed_left, self.fixed_right));
        }
        for fx in 0..fine_w {
            if prev_moving[fx] {
                self.col_moved_hits[fx] = self.col_moved_hits[fx].saturating_add(1).min(20);
                self.col_static_streak[fx] = 0;
            } else if col_diff_sum[fx] as f32 / sampled_rows as f32 <= 1.5
                && col_moved_rows[fx] == 0
            {
                self.col_static_streak[fx] = self.col_static_streak[fx].saturating_add(1);
                let reset_streak = if self.col_moved_hits[fx] >= 8 {
                    24
                } else if self.col_moved_hits[fx] >= 5 {
                    12
                } else {
                    6
                };
                if self.col_static_streak[fx] >= reset_streak {
                    self.col_moved_hits[fx] = 0;
                }
            }
        }

        let vert_pairs = (sampled_rows - 1).max(1) as f32;
        let min_vert_edges = (sampled_rows / 16).max(3);
        let max_side_fine = (fine_w * 42 / 100).min(fine_w.saturating_sub(24) / 2);
        let pad_cols = (((16.0 * self.config.scale) / prev.fine_step.max(1) as f32).ceil()
            as usize)
            .clamp(3, 8);
        let col_diff = |fx: usize| col_diff_sum[fx] as f32 / sampled_rows as f32;
        let col_init_diff = |fx: usize| col_init_diff_sum[fx] as f32 / sampled_rows as f32;
        let proven_static = |fx: usize| {
            let needed = if self.col_moved_hits[fx] >= 8 {
                24
            } else if self.col_moved_hits[fx] >= 5 {
                12
            } else {
                6
            };
            self.col_static_streak[fx] >= needed
        };
        let init_moving = |fx: usize| {
            has_init
                && !proven_static(fx)
                && (col_init_diff(fx) > 5.0 || col_init_moved_rows[fx] * 100 >= sampled_rows * 10)
        };
        let confirmed_moving = |fx: usize| {
            prev_moving[fx] || init_moving(fx) || self.col_moved_hits[fx] >= 2
        };
        let strongly_moving = |fx: usize| {
            col_diff(fx) > 8.0
                || col_moved_rows[fx] * 100 >= sampled_rows * 15
                || (has_init
                    && !proven_static(fx)
                    && col_init_moved_rows[fx] * 100 >= sampled_rows * 18)
                || self.col_moved_hits[fx] >= 3
        };
        let is_static = |fx: usize| {
            !confirmed_moving(fx)
                && col_diff(fx) <= 2.5
                && (!has_init || proven_static(fx) || col_init_diff(fx) <= 3.0)
                && col_moved_rows[fx] * 100 < sampled_rows * 5
        };
        let col_has_texture = |fx: usize| {
            (col_vert_sum[fx] as f32 / vert_pairs) >= 1.2 && col_vert_edges[fx] >= min_vert_edges
        };

        let mut cand_left_fine = 0usize;
        let mut left_static_end_fine = 0usize;
        let mut left_textured_cols = 0usize;
        let mut left_horiz_cols = 0usize;
        let mut last_textured = 0usize;
        let mut consecutive_moving = 0usize;
        for (fx, &horiz_edges) in col_horiz_edges.iter().enumerate().take(max_side_fine) {
            if strongly_moving(fx) {
                break;
            }
            if confirmed_moving(fx) {
                consecutive_moving += 1;
                if consecutive_moving >= 2 {
                    break;
                }
            } else {
                consecutive_moving = 0;
                if is_static(fx) {
                    left_static_end_fine = fx + 1;
                    if col_has_texture(fx) {
                        left_textured_cols += 1;
                        if horiz_edges >= 3 {
                            left_horiz_cols += 1;
                        }
                        last_textured = fx + 1;
                        cand_left_fine = fx + 1;
                    } else if left_textured_cols > 0 && fx + 1 - last_textured <= pad_cols {
                        if horiz_edges >= 3 {
                            left_horiz_cols += 1;
                        }
                        cand_left_fine = fx + 1;
                    }
                }
            }
        }
        if left_textured_cols < 3 || left_horiz_cols < 2 {
            cand_left_fine = 0;
        }

        let mut cand_right_fine = 0usize;
        let mut right_static_end_fine = 0usize;
        let mut right_textured_cols = 0usize;
        let mut right_horiz_cols = 0usize;
        last_textured = 0;
        consecutive_moving = 0;
        for offset in 0..max_side_fine {
            let fx = fine_w - 1 - offset;
            if strongly_moving(fx) {
                break;
            }
            if confirmed_moving(fx) {
                consecutive_moving += 1;
                if consecutive_moving >= 2 {
                    break;
                }
            } else {
                consecutive_moving = 0;
                if is_static(fx) {
                    right_static_end_fine = offset + 1;
                    if col_has_texture(fx) {
                        right_textured_cols += 1;
                        if col_horiz_edges[fx] >= 3 {
                            right_horiz_cols += 1;
                        }
                        last_textured = offset + 1;
                        cand_right_fine = offset + 1;
                    } else if right_textured_cols > 0 && offset + 1 - last_textured <= pad_cols {
                        if col_horiz_edges[fx] >= 3 {
                            right_horiz_cols += 1;
                        }
                        cand_right_fine = offset + 1;
                    }
                }
            }
        }
        if right_textured_cols < 3 || right_horiz_cols < 2 {
            cand_right_fine = 0;
        }

        let min_sidebar_px = (24.0 * self.config.scale).round() as u32;
        let mut cand_left = if cand_left_fine > 0 {
            let px = (prev.fine_origin_x + cand_left_fine * prev.fine_step).min(self.width as usize)
                as u32;
            if px >= min_sidebar_px { px } else { 0 }
        } else {
            0
        };
        let mut cand_right = if cand_right_fine > 0 {
            let boundary_x = prev.fine_origin_x + (fine_w - cand_right_fine) * prev.fine_step;
            let px = self.width.saturating_sub(boundary_x as u32);
            if px >= min_sidebar_px { px } else { 0 }
        } else {
            0
        };

        let max_side_px = ((self.width as usize * 42) / 100).min(self.width as usize / 2);
        let max_extend = (220.0 * self.config.scale).round() as usize;
        if cand_left > 0 && left_static_end_fine > 0 {
            let x_lo = (cand_left as usize)
                .saturating_sub(prev.fine_step * 2)
                .max(min_sidebar_px as usize);
            let static_limit = (prev.fine_origin_x
                + left_static_end_fine.saturating_sub(1) * prev.fine_step)
                .min(max_side_px);
            let x_hi = static_limit
                .max(cand_left as usize)
                .min(max_side_px)
                .min(cand_left as usize + max_extend);
            if x_lo < x_hi
                && let Some(snapped) = snap_sidebar_to_divider(
                    curr_image,
                    search_top,
                    search_bottom,
                    x_lo,
                    x_hi,
                    false,
                )
            {
                cand_left = (snapped as u32).clamp(min_sidebar_px, max_side_px as u32);
            }
        }
        if cand_right > 0 && right_static_end_fine > 0 {
            let cur_boundary = self.width.saturating_sub(cand_right) as usize;
            let x_hi = (cur_boundary + prev.fine_step * 2)
                .min(self.width as usize - min_sidebar_px as usize);
            let static_boundary = (prev.fine_origin_x
                + (fine_w.saturating_sub(right_static_end_fine) + 1).min(fine_w)
                    * prev.fine_step)
                .max(self.width as usize - max_side_px);
            let x_lo = static_boundary
                .min(cur_boundary)
                .max(self.width as usize - max_side_px)
                .max(cur_boundary.saturating_sub(max_extend));
            if x_lo < x_hi
                && let Some(snapped) = snap_sidebar_to_divider(
                    curr_image,
                    search_top,
                    search_bottom,
                    x_lo,
                    x_hi,
                    true,
                )
            {
                cand_right = self
                    .width
                    .saturating_sub(snapped as u32)
                    .clamp(min_sidebar_px, max_side_px as u32);
            }
        }

        // Anti-flapping guard: keep an already-locked sidebar stable unless
        // columns inside that region genuinely start moving.
        if self.fixed_left > 0 && cand_left == 0 {
            let existing_left_fine = (self.fixed_left as usize)
                .saturating_sub(prev.fine_origin_x)
                .div_ceil(prev.fine_step.max(1))
                .min(fine_w);
            if existing_left_fine >= 3 {
                let moved_inside = (0..existing_left_fine)
                    .filter(|&fx| confirmed_moving(fx))
                    .count();
                if moved_inside <= 1 {
                    cand_left = self.fixed_left;
                }
            }
        }
        if self.fixed_right > 0 && cand_right == 0 {
            let boundary_x = self.width.saturating_sub(self.fixed_right) as usize;
            let start_fx = boundary_x
                .saturating_sub(prev.fine_origin_x)
                / prev.fine_step.max(1);
            let start_fx = start_fx.min(fine_w);
            let existing_right_fine = fine_w.saturating_sub(start_fx);
            if existing_right_fine >= 3 {
                let moved_inside = (start_fx..fine_w)
                    .filter(|&fx| confirmed_moving(fx))
                    .count();
                if moved_inside <= 1 {
                    cand_right = self.fixed_right;
                }
            }
        }

        let prev_left_votes = self.cand_left_votes;
        let prev_right_votes = self.cand_right_votes;
        self.cand_left_votes = Self::next_sidebar_vote(self.cand_left_votes, cand_left, side_tol);
        self.cand_right_votes =
            Self::next_sidebar_vote(self.cand_right_votes, cand_right, side_tol);
        if self.cand_left_votes != prev_left_votes || self.cand_right_votes != prev_right_votes {
            log::debug!(
                "scroll fixed sidebar votes: left={:?} right={:?} (fixed={:?}, tol={side_tol})",
                self.cand_left_votes,
                self.cand_right_votes,
                (self.fixed_left, self.fixed_right)
            );
        }

        self.sidebar_scratch = scratch;

        let left_needed = if self.fixed_left == 0 {
            INITIAL_SIDEBAR_VOTES
        } else {
            MARGIN_CHANGE_VOTES
        };
        let right_needed = if self.fixed_right == 0 {
            INITIAL_SIDEBAR_VOTES
        } else {
            MARGIN_CHANGE_VOTES
        };
        let next_left = match self.cand_left_votes {
            Some((v, n)) if n >= left_needed => v,
            _ => self.fixed_left,
        };
        let next_right = match self.cand_right_votes {
            Some((v, n)) if n >= right_needed => v,
            _ => self.fixed_right,
        };

        let min_active_w = (self.width / 3).max(96);
        if (next_left != self.fixed_left || next_right != self.fixed_right)
            && next_left + next_right + min_active_w <= self.width
        {
            self.flush_pending()?;
            self.apply_fixed_sidebars(next_left, next_right, curr_image)?;
        }

        let eff_left = self.fixed_left.max(cand_left);
        let eff_right = self.fixed_right.max(cand_right);
        if eff_left + eff_right + min_active_w <= self.width {
            Ok((eff_left, eff_right))
        } else {
            Ok((self.fixed_left, self.fixed_right))
        }
    }

    fn apply_fixed_sidebars(
        &mut self,
        new_left: u32,
        new_right: u32,
        curr: &RgbaImage,
    ) -> Result<()> {
        self.fixed_left = new_left;
        self.fixed_right = new_right;
        self.apply_fixed_sidebars_quiet(curr)?;
        let left_info = self
            .left_sidebar
            .as_ref()
            .map_or((0, 0, 0), |s| (s.width, s.top_h, s.bottom_h));
        let right_info = self
            .right_sidebar
            .as_ref()
            .map_or((0, 0, 0), |s| (s.width, s.top_h, s.bottom_h));
        log::info!(
            "scroll fixed sidebars updated: left={left_info:?} right={right_info:?} (active_w={})",
            self.width - self.fixed_left - self.fixed_right
        );
        self.rebuild_all_preview_tiles()
    }

    fn apply_fixed_sidebars_quiet(&mut self, curr: &RgbaImage) -> Result<()> {
        let active_top = self.fixed_top;
        let active_bottom = self.frame_height - self.fixed_bottom;
        self.preview_dirty_range = Some((0, u32::MAX));
        self.sidebar_recheck_skip = 0;
        self.left_sidebar = if self.fixed_left > 0 {
            Some(build_sidebar_layout(
                curr,
                0,
                self.fixed_left as usize,
                active_top,
                active_bottom,
            ))
        } else {
            None
        };
        self.right_sidebar = if self.fixed_right > 0 {
            Some(build_sidebar_layout(
                curr,
                (self.width - self.fixed_right) as usize,
                self.width as usize,
                active_top,
                active_bottom,
            ))
        } else {
            None
        };
        Ok(())
    }

    fn overlay_sidebars_on_active_rows(
        &self,
        start_active_idx: u32,
        row_count: u32,
        buf: &mut [u8],
    ) {
        if (self.left_sidebar.is_none() && self.right_sidebar.is_none()) || row_count == 0 {
            return;
        }
        let total_active = self.above_rows + self.below_rows;
        let stride = self.width as usize * 4;
        if stride == 0 {
            return;
        }
        let rows_in_buf = (buf.len() / stride).min(row_count as usize);

        for i in 0..rows_in_buf {
            let active_idx = start_active_idx + i as u32;
            let row = &mut buf[i * stride..(i + 1) * stride];
            if let Some(left) = &self.left_sidebar
                && left.width > 0
            {
                let bytes = (left.width as usize * 4).min(stride);
                copy_sidebar_row_slice(left, active_idx, total_active, bytes, &mut row[..bytes]);
            }
            if let Some(right) = &self.right_sidebar
                && right.width > 0
            {
                let left_bytes = self
                    .left_sidebar
                    .as_ref()
                    .map_or(0, |l| l.width as usize * 4)
                    .min(stride);
                let bytes = (right.width as usize * 4).min(stride.saturating_sub(left_bytes));
                if bytes > 0 {
                    let off = stride - bytes;
                    copy_sidebar_row_slice(right, active_idx, total_active, bytes, &mut row[off..]);
                }
            }
        }
    }

    /// Re-slice already-captured canvas rows when sticky margins change.
    fn apply_fixed_margins(
        &mut self,
        new_top: u32,
        new_bottom: u32,
        curr: &RgbaImage,
    ) -> Result<()> {
        let old_top = self.fixed_top;
        let old_bottom = self.fixed_bottom;
        let old_active_h = self.frame_height.saturating_sub(old_top + old_bottom);
        let at_canvas_top =
            self.committed.is_none() || self.viewport_y <= -(self.above_rows as i32);
        let at_canvas_bottom = self.committed.is_none()
            || self.viewport_y + old_active_h as i32 >= self.below_rows as i32;
        let stride = curr.width() as usize * 4;

        if new_top > old_top {
            let take = (new_top - old_top).min(self.above_rows + self.below_rows);
            if take > 0 {
                let from_above = take.min(self.above_rows);
                if at_canvas_top {
                    self.rows.trim_top_with_origin_shift(take, from_above)?;
                    self.above_rows -= from_above;
                    self.below_rows -= take - from_above;
                    self.fixed_top += take;
                    if from_above > 0 {
                        self.rebase_anchors(from_above as i32);
                    }
                    self.update_header(curr);
                } else {
                    let mut preserved = self
                        .rows
                        .read_rows(-(self.above_rows as i32), take)?
                        .into_raw();
                    self.overlay_sidebars_on_active_rows(0, take, &mut preserved);
                    self.rows.trim_top_with_origin_shift(take, from_above)?;
                    self.above_rows -= from_above;
                    self.below_rows -= take - from_above;
                    self.fixed_top += take;
                    if from_above > 0 {
                        self.rebase_anchors(from_above as i32);
                    }
                    self.header_rows.extend_from_slice(&preserved);
                }
            }
        } else if new_top < old_top {
            let restored = old_top - new_top;
            let exposed =
                self.header_rows[new_top as usize * stride..old_top as usize * stride].to_vec();
            self.rows.prepend_up(-(self.above_rows as i32), &exposed)?;
            self.above_rows += restored;
            self.fixed_top = new_top;
            self.rebase_anchors(-(restored as i32));
            if at_canvas_top {
                self.update_header(curr);
            } else {
                self.header_rows.truncate(new_top as usize * stride);
            }
        }
        if new_bottom > old_bottom {
            let take = (new_bottom - old_bottom).min(self.below_rows);
            if take > 0 {
                if at_canvas_bottom {
                    self.rows.trim_bottom(take)?;
                    self.below_rows -= take;
                    self.fixed_bottom += take;
                    self.update_footer(curr);
                } else {
                    let mut preserved = self
                        .rows
                        .read_rows((self.below_rows - take) as i32, take)?
                        .into_raw();
                    let start_active = self.above_rows + self.below_rows - take;
                    self.overlay_sidebars_on_active_rows(start_active, take, &mut preserved);
                    self.rows.trim_bottom(take)?;
                    self.below_rows -= take;
                    self.fixed_bottom += take;
                    let mut next_footer = preserved;
                    next_footer.extend_from_slice(&self.footer_rows);
                    self.footer_rows = next_footer;
                }
            }
        } else if new_bottom < old_bottom {
            let restored = old_bottom - new_bottom;
            let exposed = self.footer_rows[..restored as usize * stride].to_vec();
            self.rows.append_down(self.below_rows as i32, &exposed)?;
            self.below_rows += restored;
            self.fixed_bottom = new_bottom;
            if at_canvas_bottom {
                self.update_footer(curr);
            } else {
                self.footer_rows.drain(..restored as usize * stride);
            }
        }
        log::info!(
            "scroll fixed margins updated: top={} bottom={} (active_h={})",
            self.fixed_top,
            self.fixed_bottom,
            self.frame_height - self.fixed_top - self.fixed_bottom
        );
        if self.fixed_left > 0 || self.fixed_right > 0 {
            self.apply_fixed_sidebars_quiet(curr)?;
        }
        self.margin_recheck_skip = 0;
        self.preview_dirty_range = Some((0, u32::MAX));
        if self.preview_follow {
            self.sync_live_preview_range();
        }
        self.rebuild_all_preview_tiles()
    }

    fn rebase_anchors(&mut self, delta_y: i32) {
        if let Some(anchor) = &mut self.committed {
            anchor.y += delta_y;
        }
        if let Some(anchor) = &mut self.prev {
            anchor.y += delta_y;
        }
        self.viewport_y += delta_y;
    }

    fn update_header(&mut self, curr: &RgbaImage) {
        if self.fixed_top == 0 {
            self.header_rows.clear();
            return;
        }
        let rows = raw_rows(curr, 0, self.fixed_top);
        self.header_rows.clear();
        self.header_rows.extend_from_slice(rows);
    }

    fn update_footer(&mut self, curr: &RgbaImage) {
        if self.fixed_bottom == 0 {
            self.footer_rows.clear();
            return;
        }
        let start = self.frame_height - self.fixed_bottom;
        let rows = raw_rows(curr, start, self.frame_height);
        self.footer_rows.clear();
        self.footer_rows.extend_from_slice(rows);
    }

    /// Splice `frame` whose active top lands at virtual `y` into `below`/`above`.
    fn commit_frame(
        &mut self,
        frame: Arc<FrameProfile>,
        image: &RgbaImage,
        y: i32,
    ) -> Result<()> {
        let active_top = self.fixed_top;
        let active_bottom = self.frame_height - self.fixed_bottom;
        let active_h = active_bottom.saturating_sub(active_top);
        if active_h == 0 {
            return Ok(());
        }
        let stride = frame.row_bytes();
        let top_extent = -(self.above_rows as i32);
        let bottom_extent = self.below_rows as i32;
        let y_end = y + active_h as i32;

        // Rubber-band / in-bounds protection: if entirely inside already-stitched
        // span, move anchor without adding duplicate rows.
        if y >= top_extent && y_end <= bottom_extent {
            self.committed = Some(CommittedAnchor::new(frame, y));
            return Ok(());
        }

        let mut did_splice = false;

        // Downward growth: y_end > bottom_extent
        if y_end > bottom_extent {
            if y > bottom_extent {
                bail!("scroll frame has no downward overlap: y={y} canvas_bottom={bottom_extent}");
            }
            let overlap_start = y.max(top_extent);
            let overlap_end = bottom_extent;
            let seam_y = if overlap_end > overlap_start + 8 {
                self.best_seam_down(image, y, active_top, overlap_start, overlap_end)?
            } else {
                bottom_extent
            };
            // Overwrite existing `below` rows from `seam_y.max(0)`..`bottom_extent`,
            // then append new rows `bottom_extent`..`y_end`.
            let overwrite_start = seam_y.max(0);
            if overwrite_start < bottom_extent {
                let src_start = (overwrite_start - y) as u32 + active_top;
                let src_end = (bottom_extent - y) as u32 + active_top;
                let raw = raw_rows(image, src_start, src_end);
                self.rows.write_rows(
                    overwrite_start,
                    (bottom_extent - overwrite_start) as u32,
                    raw,
                )?;
            }
            let src_start = (bottom_extent - y) as u32 + active_top;
            let src_end = (y_end - y) as u32 + active_top;
            let raw = raw_rows(image, src_start, src_end);
            self.rows.append_down(bottom_extent, raw)?;
            self.below_rows += (y_end - bottom_extent) as u32;
            self.update_footer(image);
            let dirty_start = self.active_to_canvas(overwrite_start.max(0));
            let dirty_end = self.active_to_canvas(y_end);
            self.refresh_preview_range(dirty_start, dirty_end)?;
            if self.fixed_bottom > 0 {
                let footer_start = self.total_height() - self.fixed_bottom;
                self.refresh_preview_range(footer_start, self.total_height())?;
            }
            did_splice = true;
        }

        // Upward growth: y < top_extent
        if y < top_extent {
            let allow_new = (top_extent - y) as u32;
            if y_end < top_extent {
                bail!(
                    "scroll frame has no upward overlap: frame_bottom={y_end} canvas_top={top_extent}"
                );
            }
            // Guard against macOS top rubber-band solid background: require at
            // least 24 px or non-trivial texture in the newly exposed top band.
            let allow_y = top_extent - allow_new as i32;
            let newly_exposed_has_texture = {
                let start_r = (allow_y.max(y) - y) as u32 + active_top;
                let end_r = (top_extent.min(y_end) - y) as u32 + active_top;
                frame.row_grad[start_r as usize..end_r as usize]
                    .iter()
                    .any(|&g| g >= 1.5)
            };
            if self.upward_established || allow_new >= 24 || newly_exposed_has_texture {
                self.upward_established = true;
                let overlap_start = top_extent;
                let overlap_end = y_end.min(bottom_extent);
                let seam_y = if overlap_end > overlap_start + 8 {
                    self.best_seam_up(image, y, active_top, overlap_start, overlap_end)?
                } else {
                    top_extent
                };

                // Overwrite existing rows in [top_extent .. seam_y] (which may
                // span `above` and/or top of `below`).
                if top_extent < seam_y {
                    let src_start = (top_extent - y) as u32 + active_top;
                    let src_end = (seam_y - y) as u32 + active_top;
                    self.rows.write_rows(
                        top_extent,
                        (seam_y - top_extent) as u32,
                        raw_rows(image, src_start, src_end),
                    )?;
                }
                // Prepend the newly exposed rows in top-to-bottom order.
                let mut pixels = Vec::with_capacity(allow_new as usize * stride);
                for cy in allow_y..top_extent {
                    let src_row = (cy - y) as u32 + active_top;
                    pixels.extend_from_slice(raw_rows(image, src_row, src_row + 1));
                }
                self.rows.prepend_up(top_extent, &pixels)?;
                self.above_rows += allow_new;
                if !self.preview_follow {
                    self.preview_range.0 = self.preview_range.0.saturating_add(allow_new);
                }
                self.update_header(image);
                if self.preview_follow {
                    self.sync_live_preview_range();
                }
                self.rebuild_all_preview_tiles()?;
                did_splice = true;
            }
        }

        if did_splice {
            self.committed = Some(CommittedAnchor::new(frame, y));
            if self.preview_follow {
                self.sync_live_preview_range();
            }
        }
        Ok(())
    }

    fn active_to_canvas(&self, y: i32) -> u32 {
        let active_index = if y < 0 {
            (self.above_rows as i64 + i64::from(y)).clamp(0, self.above_rows as i64) as u32
        } else {
            (self.above_rows as i64 + i64::from(y))
                .clamp(0, (self.above_rows + self.below_rows) as i64) as u32
        };
        self.fixed_top.saturating_add(active_index)
    }

    fn best_seam_down(
        &mut self,
        frame: &RgbaImage,
        frame_y: i32,
        active_top: u32,
        overlap_start: i32,
        overlap_end: i32,
    ) -> Result<i32> {
        let span = overlap_end - overlap_start;
        let lo = overlap_start + span * 3 / 10;
        let hi = (overlap_start + span * 7 / 10).max(lo + 1);
        let mut best_y = (lo + hi) / 2;
        let mut best_diff = u64::MAX;
        let step_x = (self.width as usize / 64).max(1);
        let mut interior = interior_columns(self.width as usize, self.config.scale);
        let narrowed_start = interior.start.max(self.fixed_left as usize);
        let narrowed_end = interior
            .end
            .min(self.width.saturating_sub(self.fixed_right) as usize);
        if narrowed_end > narrowed_start + 16 {
            interior = narrowed_start..narrowed_end;
        }
        let mut existing = vec![0u8; self.width as usize * 4];
        if hi - lo > 64 {
            let step_y = ((hi - lo) / 64).max(1);
            for cy in (lo..hi).step_by(step_y as usize) {
                let src_row = (cy - frame_y) as u32 + active_top;
                let src = raw_rows(frame, src_row, src_row + 1);
                self.rows.copy_to(cy, 1, &mut existing)?;
                let diff = sampled_row_diff(&existing, src, step_x, interior.clone());
                if diff < best_diff {
                    best_diff = diff;
                    best_y = cy;
                }
            }
            let refine_lo = (best_y - step_y).max(lo);
            let refine_hi = (best_y + step_y + 1).min(hi);
            for cy in refine_lo..refine_hi {
                let src_row = (cy - frame_y) as u32 + active_top;
                let src = raw_rows(frame, src_row, src_row + 1);
                self.rows.copy_to(cy, 1, &mut existing)?;
                let diff = sampled_row_diff(&existing, src, step_x, interior.clone());
                if diff < best_diff {
                    best_diff = diff;
                    best_y = cy;
                }
            }
        } else {
            for cy in lo..hi {
                let src_row = (cy - frame_y) as u32 + active_top;
                let src = raw_rows(frame, src_row, src_row + 1);
                self.rows.copy_to(cy, 1, &mut existing)?;
                let diff = sampled_row_diff(&existing, src, step_x, interior.clone());
                if diff < best_diff {
                    best_diff = diff;
                    best_y = cy;
                }
            }
        }
        log::debug!(
            "scroll seam y={best_y} diff={best_diff} overlap={overlap_start}..{overlap_end} interior={}..{}",
            interior.start,
            interior.end
        );
        Ok(best_y)
    }

    fn best_seam_up(
        &mut self,
        frame: &RgbaImage,
        frame_y: i32,
        active_top: u32,
        overlap_start: i32,
        overlap_end: i32,
    ) -> Result<i32> {
        // Same mid-overlap minimum-difference search.
        self.best_seam_down(frame, frame_y, active_top, overlap_start, overlap_end)
    }

    /// Read one full-canvas row (0..total_height) into `dst`.
    fn canvas_row_into(&mut self, y: u32, dst: &mut [u8]) -> Result<()> {
        let stride = self.width as usize * 4;
        let y = y as usize;
        let h_top = self.fixed_top as usize;
        if y < h_top {
            let off = y * stride;
            dst.copy_from_slice(&self.header_rows[off..off + stride]);
            return Ok(());
        }
        let y = y - h_top;
        let above_len = self.above_rows as usize;
        if y < above_len {
            let row = -(above_len as i64) + y as i64;
            self.rows.copy_to(row as i32, 1, dst)?;
            self.overlay_sidebars_on_active_rows(y as u32, 1, dst);
            return Ok(());
        }
        let y = y - above_len;
        let below_len = self.below_rows as usize;
        if y < below_len {
            self.rows.copy_to(y as i32, 1, dst)?;
            self.overlay_sidebars_on_active_rows((above_len + y) as u32, 1, dst);
            return Ok(());
        }
        let y = y - below_len;
        let off = y * stride;
        dst.copy_from_slice(&self.footer_rows[off..off + stride]);
        Ok(())
    }

    /// Read one full-canvas row (0..total_height) without assembling the full
    /// output buffer.
    #[cfg(test)]
    fn canvas_row(&mut self, y: u32) -> Result<Vec<u8>> {
        let mut dst = vec![0u8; self.width as usize * 4];
        self.canvas_row_into(y, &mut dst)?;
        Ok(dst)
    }

    #[cfg(test)]
    fn render_preview_slice(
        &mut self,
        prev_w: u32,
        source_y0: u32,
        source_y1: u32,
    ) -> Result<RgbaImage> {
        self.render_preview_slice_with_reuse(prev_w, source_y0, source_y1, None, Some((0, u32::MAX)))
    }

    fn render_preview_slice_with_reuse(
        &mut self,
        prev_w: u32,
        source_y0: u32,
        source_y1: u32,
        reuse: Option<&PreviewTile>,
        dirty_range: Option<(u32, u32)>,
    ) -> Result<RgbaImage> {
        let total_height = self.total_height();
        if total_height == 0 {
            return Ok(RgbaImage::new(prev_w, 1));
        }
        let source_y0 = source_y0.min(total_height - 1);
        let source_y1 = source_y1.max(source_y0 + 1).min(total_height);
        let full_w = self.width as usize;
        let source_h = source_y1 - source_y0;
        let tile_h =
            ((source_h as u64 * prev_w as u64).div_ceil(full_w.max(1) as u64) as u32).max(1);
        let prev_stride = prev_w as usize * 4;
        let mut rgba = vec![0u8; prev_stride * tile_h as usize];

        let mut src_x_offsets = Vec::with_capacity(prev_w as usize);
        for px in 0..prev_w {
            let src_x =
                ((px as u64 * full_w as u64) / prev_w as u64).min((full_w - 1) as u64) as usize;
            src_x_offsets.push(src_x * 4);
        }

        let tail_chrome = self.fixed_bottom.saturating_add(
            self.left_sidebar
                .as_ref()
                .map_or(0, |s| s.bottom_h)
                .max(self.right_sidebar.as_ref().map_or(0, |s| s.bottom_h)),
        );
        let reusable = reuse.filter(|old| {
            old.image.width() == prev_w && old.source_rows > 0 && old.total_height >= tail_chrome
        });

        let mut src_row = std::mem::take(&mut self.preview_row_scratch);
        src_row.resize(full_w * 4, 0);
        let mut cached_source_y = None;
        for dy in 0..tile_h {
            let source_y = source_y0
                + ((dy as u64 * full_w.max(1) as u64 / prev_w as u64) as u32).min(source_h - 1);
            let dst_off = dy as usize * prev_stride;
            let dst_slice = &mut rgba[dst_off..dst_off + prev_stride];

            let in_dirty = dirty_range.is_some_and(|(d0, d1)| source_y >= d0 && source_y < d1);
            if !in_dirty
                && let Some(old) = reusable
                && source_y >= old.source_start
                && source_y < old.source_start + old.source_rows
                && source_y < old.total_height.saturating_sub(tail_chrome)
                && source_y < total_height.saturating_sub(tail_chrome)
            {
                let old_tile_h = old.image.height();
                let guess = (((source_y - old.source_start) as u64 * prev_w as u64)
                    / full_w.max(1) as u64) as u32;
                let mut copied = false;
                for cand_dy in [guess, guess.saturating_add(1)] {
                    if cand_dy < old_tile_h {
                        let old_src_y = old.source_start
                            + ((cand_dy as u64 * full_w.max(1) as u64 / prev_w as u64) as u32)
                                .min(old.source_rows - 1);
                        if old_src_y == source_y {
                            let src_off = cand_dy as usize * prev_stride;
                            dst_slice
                                .copy_from_slice(&old.image.as_raw()[src_off..src_off + prev_stride]);
                            copied = true;
                            break;
                        }
                    }
                }
                if copied {
                    continue;
                }
            }

            if cached_source_y != Some(source_y) {
                if let Err(error) = self.canvas_row_into(source_y, &mut src_row) {
                    self.preview_row_scratch = src_row;
                    return Err(error);
                }
                cached_source_y = Some(source_y);
            }
            for (px, &s) in src_x_offsets.iter().enumerate() {
                let d = px * 4;
                dst_slice[d..d + 4].copy_from_slice(&src_row[s..s + 4]);
            }
        }
        self.preview_row_scratch = src_row;
        RgbaImage::from_raw(prev_w, tile_h, rgba).context("preview tile buffer size mismatch")
    }

    fn rebuild_all_preview_tiles(&mut self) -> Result<()> {
        self.preview_dirty_range = Some((0, u32::MAX));
        self.preview_dirty = true;
        self.refresh_preview()
    }

    fn refresh_preview_range(&mut self, start: u32, end: u32) -> Result<()> {
        self.preview_dirty = true;
        self.preview_dirty_range = Some(match self.preview_dirty_range {
            Some((cur_start, cur_end)) => (cur_start.min(start), cur_end.max(end)),
            None => (start, end),
        });
        Ok(())
    }
}

fn sampled_row_diff(a: &[u8], b: &[u8], step_x: usize, interior: std::ops::Range<usize>) -> u64 {
    let mut sum = 0u64;
    let pixels = a.len().min(b.len()) / 4;
    let start = interior.start.min(pixels);
    let end = interior.end.min(pixels).max(start);
    for x in (start..end).step_by(step_x.max(1)) {
        let p = x * 4;
        sum += u64::from(a[p].abs_diff(b[p]))
            + u64::from(a[p + 1].abs_diff(b[p + 1]))
            + u64::from(a[p + 2].abs_diff(b[p + 2]));
    }
    sum
}

fn extract_sidebar_rows(
    frame: &RgbaImage,
    x_start: usize,
    x_end: usize,
    y_start: u32,
    y_end: u32,
) -> Vec<u8> {
    let span_bytes = x_end.saturating_sub(x_start) * 4;
    if span_bytes == 0 || y_end <= y_start {
        return Vec::new();
    }
    let mut out = Vec::with_capacity((y_end - y_start) as usize * span_bytes);
    let x_off = x_start * 4;
    for y in y_start..y_end {
        let row = raw_rows(frame, y, y + 1);
        out.extend_from_slice(&row[x_off..x_off + span_bytes]);
    }
    out
}

/// Analyze a fixed sidebar (`[x_start .. x_end]` over `active_top .. active_bottom`)
/// and split it into top-anchored rows, a middle background+divider fill row,
/// and bottom-anchored rows so bottom controls land at the bottom of long stitches.
fn build_sidebar_layout(
    frame: &RgbaImage,
    x_start: usize,
    x_end: usize,
    active_top: u32,
    active_bottom: u32,
) -> SidebarLayout {
    let x_end = x_end.min(frame.width() as usize);
    let x_start = x_start.min(x_end);
    let span = x_end - x_start;
    let active_h = active_bottom.saturating_sub(active_top);
    if span == 0 || active_h == 0 {
        return SidebarLayout {
            width: span as u32,
            top_h: 0,
            bottom_h: 0,
            top_rows: Vec::new(),
            fill_row: Vec::new(),
            bottom_rows: Vec::new(),
        };
    }

    let step_y = (active_h / 64).max(1);
    let mut hist_count = vec![0u32; 4096];
    let mut hist_sum = vec![[0u64; 3]; 4096];
    let inner_start = if span > 6 { x_start + 3 } else { x_start };
    let inner_end = if span > 6 { x_end - 3 } else { x_end };
    let mut sampled_ys = Vec::with_capacity(64);

    let mut y = active_top;
    while y < active_bottom {
        sampled_ys.push(y);
        let row = raw_rows(frame, y, y + 1);
        for x in inner_start..inner_end {
            let p = x * 4;
            let r = row[p];
            let g = row[p + 1];
            let b = row[p + 2];
            let key = (usize::from(r >> 4) << 8) | (usize::from(g >> 4) << 4) | usize::from(b >> 4);
            hist_count[key] += 1;
            hist_sum[key][0] += u64::from(r);
            hist_sum[key][1] += u64::from(g);
            hist_sum[key][2] += u64::from(b);
        }
        y = y.saturating_add(step_y);
    }

    let mut best_key = 0usize;
    let mut best_count = 0u32;
    for (key, &count) in hist_count.iter().enumerate() {
        if count > best_count {
            best_count = count;
            best_key = key;
        }
    }
    let global_bg = if best_count > 0 {
        let sum = hist_sum[best_key];
        [
            (sum[0] / u64::from(best_count)) as u8,
            (sum[1] / u64::from(best_count)) as u8,
            (sum[2] / u64::from(best_count)) as u8,
            255,
        ]
    } else {
        let first = raw_rows(frame, active_top, active_top + 1);
        let p = x_start * 4;
        [first[p], first[p + 1], first[p + 2], 255]
    };

    // Per-column dominant background preserves multi-pane sidebars and dividers.
    let mut col_bg_row = Vec::with_capacity(span * 4);
    let mut col_counts = [0u16; 4096];
    let mut col_sums = [[0u32; 3]; 4096];
    let mut touched = Vec::with_capacity(64);
    let sample_count = sampled_ys.len().max(1);
    for x in x_start..x_end {
        let p = x * 4;
        touched.clear();
        for &sy in &sampled_ys {
            let row = raw_rows(frame, sy, sy + 1);
            let r = row[p];
            let g = row[p + 1];
            let b = row[p + 2];
            let key = (usize::from(r >> 4) << 8) | (usize::from(g >> 4) << 4) | usize::from(b >> 4);
            if col_counts[key] == 0 {
                touched.push(key);
            }
            col_counts[key] += 1;
            col_sums[key][0] += u32::from(r);
            col_sums[key][1] += u32::from(g);
            col_sums[key][2] += u32::from(b);
        }
        let mut c_best_key = 0usize;
        let mut c_best_count = 0u16;
        for &key in &touched {
            let count = col_counts[key];
            if count > c_best_count {
                c_best_count = count;
                c_best_key = key;
            }
        }
        let mut px = if usize::from(c_best_count) * 100 >= sample_count * 45 && c_best_count > 0 {
            let sum = col_sums[c_best_key];
            [
                (sum[0] / u32::from(c_best_count)) as u8,
                (sum[1] / u32::from(c_best_count)) as u8,
                (sum[2] / u32::from(c_best_count)) as u8,
                255,
            ]
        } else {
            global_bg
        };
        for &key in &touched {
            col_counts[key] = 0;
            col_sums[key] = [0; 3];
        }

        let near_edge = x - x_start < 3 || x_end - 1 - x < 3;
        if near_edge && let Some(&first_y) = sampled_ys.first() {
            let first_row = raw_rows(frame, first_y, first_y + 1);
            let ref_px = [
                first_row[p],
                first_row[p + 1],
                first_row[p + 2],
                first_row[p + 3],
            ];
            let uniform = sampled_ys.iter().all(|&sy| {
                let row = raw_rows(frame, sy, sy + 1);
                row[p].abs_diff(ref_px[0]) <= 6
                    && row[p + 1].abs_diff(ref_px[1]) <= 6
                    && row[p + 2].abs_diff(ref_px[2]) <= 6
            });
            if uniform {
                px = ref_px;
            }
        }
        col_bg_row.extend_from_slice(&px);
    }

    let inner_span = inner_end.saturating_sub(inner_start).max(1);
    let step_x = (inner_span / 128).max(1);
    let active_len = active_h as usize;
    let mut is_bg = Vec::with_capacity(active_len);
    for r in 0..active_len {
        let row = raw_rows(frame, active_top + r as u32, active_top + r as u32 + 1);
        let mut matched = 0usize;
        let mut total = 0usize;
        for x in (inner_start..inner_end).step_by(step_x) {
            let p = x * 4;
            let bg_p = (x - x_start) * 4;
            total += 1;
            if row[p].abs_diff(col_bg_row[bg_p]) <= 12
                && row[p + 1].abs_diff(col_bg_row[bg_p + 1]) <= 12
                && row[p + 2].abs_diff(col_bg_row[bg_p + 2]) <= 12
            {
                matched += 1;
            }
        }
        is_bg.push(total > 0 && matched * 100 >= total * 97);
    }

    let first_content = is_bg.iter().position(|&b| !b);
    let last_content = is_bg.iter().rposition(|&b| !b);
    let (top_h, bottom_h, fill_row) = match (first_content, last_content) {
        (None, _) | (_, None) => (0u32, 0u32, col_bg_row),
        (Some(fc), Some(lc)) => {
            let trail_len = active_len - 1 - lc;
            let mut runs = Vec::new();
            let mut run_start: Option<usize> = None;
            for (r, &bg) in is_bg.iter().enumerate().take(lc + 1).skip(fc) {
                if bg && run_start.is_none() {
                    run_start = Some(r);
                } else if !bg && let Some(rs) = run_start.take() {
                    runs.push((rs, r));
                }
            }

            let longest = runs.iter().copied().max_by_key(|(rs, re)| re - rs);
            let sig_gap = (active_len / 12).max(32);
            if let Some((bs, be)) = longest
                && be - bs >= sig_gap
                && active_len > be
            {
                let mid = active_top + ((bs + be) / 2) as u32;
                let fill = extract_sidebar_rows(frame, x_start, x_end, mid, mid + 1);
                (bs as u32, (active_len - be) as u32, fill)
            } else if trail_len >= (active_len / 16).max(24) {
                let mid = active_top + (lc + 1 + trail_len / 2) as u32;
                let fill = extract_sidebar_rows(frame, x_start, x_end, mid, mid + 1);
                ((lc + 1) as u32, 0u32, fill)
            } else {
                let target = active_len * 3 / 4;
                let pick_run = |lo: usize, hi: usize| {
                    runs.iter()
                        .copied()
                        .filter(|&(rs, re)| re > lo && rs < hi)
                        .max_by(|&(a_s, a_e), &(b_s, b_e)| {
                            (a_e - a_s).cmp(&(b_e - b_s)).then_with(|| {
                                let a_dist = ((a_s + a_e) / 2).abs_diff(target);
                                let b_dist = ((b_s + b_e) / 2).abs_diff(target);
                                b_dist.cmp(&a_dist)
                            })
                        })
                };
                if let Some((rs, re)) = pick_run(active_len * 55 / 100, active_len * 92 / 100)
                    .or_else(|| pick_run(active_len * 20 / 100, active_len * 95 / 100))
                {
                    let fill = if re - rs >= 2 {
                        let mid = active_top + ((rs + re) / 2) as u32;
                        extract_sidebar_rows(frame, x_start, x_end, mid, mid + 1)
                    } else {
                        col_bg_row
                    };
                    (rs as u32, (active_len - re) as u32, fill)
                } else {
                    let th = target.min(active_len) as u32;
                    (th, active_h - th, col_bg_row)
                }
            }
        }
    };

    let top_rows = extract_sidebar_rows(frame, x_start, x_end, active_top, active_top + top_h);
    let bottom_rows = extract_sidebar_rows(
        frame,
        x_start,
        x_end,
        active_bottom.saturating_sub(bottom_h),
        active_bottom,
    );
    SidebarLayout {
        width: span as u32,
        top_h,
        bottom_h,
        top_rows,
        fill_row,
        bottom_rows,
    }
}

fn copy_sidebar_row_slice(
    layout: &SidebarLayout,
    active_idx: u32,
    total_active: u32,
    bytes: usize,
    dst: &mut [u8],
) {
    let row_bytes = layout.width as usize * 4;
    if bytes == 0 || row_bytes < bytes {
        return;
    }
    if active_idx < layout.top_h {
        let off = active_idx as usize * row_bytes;
        if off + bytes <= layout.top_rows.len() {
            dst[..bytes].copy_from_slice(&layout.top_rows[off..off + bytes]);
            return;
        }
    }
    let bot_start = total_active.saturating_sub(layout.bottom_h);
    if layout.bottom_h > 0 && active_idx >= bot_start {
        let off = (active_idx - bot_start) as usize * row_bytes;
        if off + bytes <= layout.bottom_rows.len() {
            dst[..bytes].copy_from_slice(&layout.bottom_rows[off..off + bytes]);
            return;
        }
    }
    if bytes <= layout.fill_row.len() {
        dst[..bytes].copy_from_slice(&layout.fill_row[..bytes]);
    }
}

fn snap_sidebar_to_divider(
    image: &RgbaImage,
    search_top: u32,
    search_bottom: u32,
    x_lo: usize,
    x_hi: usize,
    from_right: bool,
) -> Option<usize> {
    const SAMPLE_ROWS: usize = 32;
    let w = image.width() as usize;
    let span = search_bottom.saturating_sub(search_top) as usize;
    if span <= 16 || w < 16 || x_lo < 3 || x_lo >= x_hi || x_hi + 3 >= w {
        return None;
    }

    let raw = image.as_raw();
    let stride = w * 4;
    let mut ys = [0u32; SAMPLE_ROWS];
    for (i, slot) in ys.iter_mut().enumerate() {
        *slot = search_top + (i as u32 * (span as u32 - 1)) / (SAMPLE_ROWS as u32 - 1);
    }

    let col_median = |x: usize, tol: u8| -> ([u8; 3], usize) {
        let mut rs = [0u8; SAMPLE_ROWS];
        let mut gs = [0u8; SAMPLE_ROWS];
        let mut bs = [0u8; SAMPLE_ROWS];
        for (i, &y) in ys.iter().enumerate() {
            let p = y as usize * stride + x * 4;
            rs[i] = raw[p];
            gs[i] = raw[p + 1];
            bs[i] = raw[p + 2];
        }
        rs.sort_unstable();
        gs.sort_unstable();
        bs.sort_unstable();
        let med = [rs[SAMPLE_ROWS / 2], gs[SAMPLE_ROWS / 2], bs[SAMPLE_ROWS / 2]];
        let mut uniform = 0usize;
        for &y in &ys {
            let p = y as usize * stride + x * 4;
            if raw[p].abs_diff(med[0]) <= tol
                && raw[p + 1].abs_diff(med[1]) <= tol
                && raw[p + 2].abs_diff(med[2]) <= tol
            {
                uniform += 1;
            }
        }
        (med, uniform)
    };

    let min_uniform = (SAMPLE_ROWS * 85).div_ceil(100);
    let max_rgb_diff = |a: [u8; 3], b: [u8; 3]| -> u8 {
        a[0].abs_diff(b[0])
            .max(a[1].abs_diff(b[1]))
            .max(a[2].abs_diff(b[2]))
    };
    let l1_rgb_diff = |a: [u8; 3], b: [u8; 3]| -> u16 {
        u16::from(a[0].abs_diff(b[0]))
            + u16::from(a[1].abs_diff(b[1]))
            + u16::from(a[2].abs_diff(b[2]))
    };

    let is_divider_line = |x: usize| -> Option<[u8; 3]> {
        let (med, uniform) = col_median(x, 6);
        if uniform < min_uniform {
            return None;
        }
        let mut contrasted = 0usize;
        for &y in &ys {
            let row = y as usize * stride;
            let pc = row + x * 4;
            let pl = row + (x - 3) * 4;
            let pr = row + (x + 3) * 4;
            let col_px = [raw[pc], raw[pc + 1], raw[pc + 2]];
            let left_px = [raw[pl], raw[pl + 1], raw[pl + 2]];
            let right_px = [raw[pr], raw[pr + 1], raw[pr + 2]];
            if max_rgb_diff(col_px, left_px) >= 8 && max_rgb_diff(col_px, right_px) >= 8 {
                contrasted += 1;
            }
        }
        (contrasted >= min_uniform).then_some(med)
    };

    // Pass 1: crisp 1-2 px full-height vertical divider line.
    let total = x_hi - x_lo + 1;
    for step in 0..total {
        let x = if from_right { x_hi - step } else { x_lo + step };
        if let Some(med) = is_divider_line(x) {
            if !from_right {
                let mut outer_end = x;
                for extra in 1..=2 {
                    let nx = x + extra;
                    if nx <= x_hi + 2 {
                        let (nmed, nuni) = col_median(nx, 6);
                        if nuni >= min_uniform && max_rgb_diff(nmed, med) <= 6 {
                            outer_end = nx;
                            continue;
                        }
                    }
                    break;
                }
                return Some(outer_end + 1);
            } else {
                let mut outer_start = x;
                for extra in 1..=2 {
                    let nx = x - extra;
                    if nx + 2 >= x_lo {
                        let (nmed, nuni) = col_median(nx, 6);
                        if nuni >= min_uniform && max_rgb_diff(nmed, med) <= 6 {
                            outer_start = nx;
                            continue;
                        }
                    }
                    break;
                }
                return Some(outer_start);
            }
        }
    }

    // Pass 2: borderless vertical background step between sidebar and main pane.
    for step in 0..total {
        let x = if from_right { x_hi - step } else { x_lo + step };
        let (med_l, uni_l) = col_median(x - 2, 5);
        let (med_r, uni_r) = col_median(x + 2, 5);
        if uni_l >= min_uniform && uni_r >= min_uniform && max_rgb_diff(med_l, med_r) >= 4 {
            let (inner_med, outer_med) = if from_right {
                (med_r, med_l)
            } else {
                (med_l, med_r)
            };
            for sub in 0..=4 {
                let bx = if from_right { x + 2 - sub } else { x - 2 + sub };
                let (bmed, _) = col_median(bx, 6);
                if l1_rgb_diff(bmed, outer_med) < l1_rgb_diff(bmed, inner_med) {
                    return Some(if from_right { bx + 1 } else { bx });
                }
            }
        }
    }

    None
}

#[derive(Clone, Copy, Debug)]
struct FineScore {
    mad: f32,
    inlier: f32,
    textured_mad: f32,
    textured_inlier: f32,
    trimmed_mad: f32,
    trimmed_inlier: f32,
    trimmed_grad_share: f32,
    textured_rows: usize,
    vert_textured_rows: usize,
    worst_scrolling_band_mad: f32,
    worst_scrolling_band_inlier: f32,
}

impl FineScore {
    fn qualifies_trimmed(&self) -> bool {
        self.textured_rows >= 6
            && self.vert_textured_rows >= 2
            && self.trimmed_grad_share >= 0.60
            && self.trimmed_mad <= 4.5
            && self.trimmed_inlier >= 0.93
    }

    fn effective_mad(&self) -> f32 {
        if self.qualifies_trimmed() {
            self.mad.min(self.trimmed_mad + 0.75)
        } else {
            self.mad
        }
    }

    fn standard_ok(&self) -> bool {
        self.vert_textured_rows >= 2
            && self.mad <= MAX_MATCH_MAD
            && self.inlier >= MIN_INLIER_RATIO
            && self.textured_mad <= 15.0
            && self.textured_inlier >= 0.78
            && self.worst_scrolling_band_mad <= 18.0
            && self.worst_scrolling_band_inlier >= 0.72
    }
}

struct BandWeights {
    fine_x: std::ops::Range<usize>,
    row_grad: Vec<f32>,
    row_match_weight: Vec<f32>,
    coarse_weight: Vec<f32>,
}

impl BandWeights {
    fn new(profile: &FrameProfile, fine_x: &std::ops::Range<usize>) -> Self {
        let h = profile.height as usize;
        if fine_x.start == 0 && fine_x.end == profile.fine_w {
            let row_match_weight: Vec<f32> = (0..h)
                .map(|y| {
                    let vg = profile.row_vert_grad[y].max(if y + 1 < h {
                        profile.row_vert_grad[y + 1]
                    } else {
                        0.0
                    });
                    profile.row_grad[y].min(vg)
                })
                .collect();
            return Self {
                fine_x: fine_x.clone(),
                row_grad: profile.row_grad.clone(),
                row_match_weight,
                coarse_weight: profile.coarse_vert_grad.clone(),
            };
        }

        let fx0 = fine_x.start.min(profile.fine_w);
        let fx1 = fine_x.end.clamp(fx0, profile.fine_w);
        let span = fx1 - fx0;
        let mut row_grad = vec![0.0_f32; h];
        let mut row_vert_grad = vec![0.0_f32; h];
        if span > 0 {
            for y in 0..h {
                let row = &profile.fine_luma[y * profile.fine_w + fx0..y * profile.fine_w + fx1];
                if span > 1 {
                    let grad_sum: u32 = row.windows(2).map(|p| u32::from(p[1].abs_diff(p[0]))).sum();
                    row_grad[y] = grad_sum as f32 / (span - 1) as f32;
                }
                if y > 0 {
                    let prev_row =
                        &profile.fine_luma[(y - 1) * profile.fine_w + fx0..(y - 1) * profile.fine_w + fx1];
                    let vert_sum: u32 = prev_row
                        .iter()
                        .zip(row)
                        .map(|(a, b)| u32::from(a.abs_diff(*b)))
                        .sum();
                    row_vert_grad[y] = vert_sum as f32 / span as f32;
                }
            }
        }

        let row_match_weight: Vec<f32> = (0..h)
            .map(|y| {
                let vg = row_vert_grad[y].max(if y + 1 < h {
                    row_vert_grad[y + 1]
                } else {
                    0.0
                });
                row_grad[y].min(vg)
            })
            .collect();

        let mut coarse_weight = vec![0.0_f32; profile.coarse_h];
        for (cy, slot) in coarse_weight.iter_mut().enumerate() {
            let y0 = cy * COARSE_STRIDE;
            let y1 = (y0 + COARSE_STRIDE).min(h);
            let mut max_w = 0.0_f32;
            for &mw in &row_match_weight[y0..y1] {
                max_w = max_w.max(mw);
            }
            *slot = max_w;
        }

        Self {
            fine_x: fx0..fx1,
            row_grad,
            row_match_weight,
            coarse_weight,
        }
    }
}

fn eval_coarse_shift(
    prev: &FrameProfile,
    curr: &FrameProfile,
    coarse_x: &std::ops::Range<usize>,
    c_top: i32,
    c_bot: i32,
    c_dy: i32,
    weights: &BandWeights,
) -> Option<(i32, f32)> {
    let coarse_span = coarse_x.len().max(1);
    let ov_start = c_top.max(c_top + c_dy);
    let ov_end = c_bot.min(c_bot + c_dy);
    let ov_len = ov_end - ov_start;
    if ov_len < 4 {
        return None;
    }
    let mut weighted_err = 0.0_f32;
    let mut weight_sum = 0.0_f32;
    let row_step = (ov_len / 64).max(1) as usize;
    for y in (ov_start..ov_end).step_by(row_step) {
        let py = y as usize;
        let cy = (y - c_dy) as usize;
        let w = weights.coarse_weight[py].max(0.25);
        let p_row =
            &prev.coarse_luma[py * prev.coarse_w + coarse_x.start..py * prev.coarse_w + coarse_x.end];
        let c_row =
            &curr.coarse_luma[cy * curr.coarse_w + coarse_x.start..cy * curr.coarse_w + coarse_x.end];
        let err: u32 = p_row
            .iter()
            .zip(c_row)
            .map(|(a, b)| u32::from(a.abs_diff(*b)))
            .sum();
        weighted_err += (err as f32 / coarse_span as f32) * w;
        weight_sum += w;
    }
    (weight_sum > 0.0).then(|| (c_dy, weighted_err / weight_sum))
}

fn coarse_band_scores(
    prev: &FrameProfile,
    curr: &FrameProfile,
    c_top: i32,
    c_bot: i32,
    c_lo: i32,
    c_hi: i32,
    weights: &BandWeights,
) -> Vec<(i32, f32)> {
    let coarse_x = prev.coarse_x_range(&weights.fine_x);
    let mut scores = Vec::with_capacity((c_hi - c_lo + 1).max(0) as usize);
    for c_dy in c_lo..=c_hi {
        if let Some(entry) = eval_coarse_shift(prev, curr, &coarse_x, c_top, c_bot, c_dy, weights) {
            scores.push(entry);
        }
    }
    scores
}

fn coarse_band_scores_two_step(
    prev: &FrameProfile,
    curr: &FrameProfile,
    c_top: i32,
    c_bot: i32,
    c_lo: i32,
    c_hi: i32,
    weights: &BandWeights,
) -> (Vec<(i32, f32)>, f32) {
    let coarse_x = prev.coarse_x_range(&weights.fine_x);
    let mut scores = Vec::with_capacity(((c_hi - c_lo).max(0) as usize) / 2 + 2);
    let mut c_dy = c_lo;
    while c_dy <= c_hi {
        if let Some(entry) = eval_coarse_shift(prev, curr, &coarse_x, c_top, c_bot, c_dy, weights) {
            scores.push(entry);
        }
        c_dy += 2;
    }
    if (c_hi - c_lo) % 2 != 0
        && let Some(entry) = eval_coarse_shift(prev, curr, &coarse_x, c_top, c_bot, c_hi, weights)
    {
        scores.push(entry);
    }
    if scores.is_empty() {
        return (Vec::new(), 0.0);
    }

    let mut raw: Vec<f32> = scores.iter().map(|&(_, s)| s).collect();
    let mid = raw.len() / 2;
    let (_, median_ref, _) = raw.select_nth_unstable_by(mid, f32::total_cmp);
    let median_score = *median_ref;

    let mut ranked: Vec<(usize, f32)> = scores
        .iter()
        .enumerate()
        .map(|(i, &(_, s))| (i, s))
        .collect();
    ranked.sort_by(|a, b| a.1.total_cmp(&b.1));

    let mut neighbors = Vec::new();
    for (idx, _) in ranked.into_iter().take(32) {
        let base_dy = scores[idx].0;
        for delta in [-1, 1] {
            let n_dy = base_dy + delta;
            if (c_lo..=c_hi).contains(&n_dy) {
                neighbors.push(n_dy);
            }
        }
    }
    for i in 0..scores.len() {
        let (base_dy, s) = scores[i];
        let left_ok = i == 0 || s <= scores[i - 1].1;
        let right_ok = i + 1 == scores.len() || s <= scores[i + 1].1;
        if left_ok && right_ok {
            for delta in [-1, 1] {
                let n_dy = base_dy + delta;
                if (c_lo..=c_hi).contains(&n_dy) {
                    neighbors.push(n_dy);
                }
            }
        }
    }
    neighbors.sort_unstable();
    neighbors.dedup();

    let mut pass2 = Vec::new();
    for n_dy in neighbors {
        if scores.binary_search_by_key(&n_dy, |&(d, _)| d).is_err()
            && let Some(entry) =
                eval_coarse_shift(prev, curr, &coarse_x, c_top, c_bot, n_dy, weights)
        {
            pass2.push(entry);
        }
    }
    if !pass2.is_empty() {
        scores.extend(pass2);
        scores.sort_unstable_by_key(|&(d, _)| d);
    }
    (scores, median_score)
}

fn coarse_minima(scores: &[(i32, f32)], limit: usize) -> Vec<(i32, f32)> {
    let mut candidates = Vec::new();
    for i in 0..scores.len() {
        let (dy, score) = scores[i];
        let left_ok = i == 0 || score <= scores[i - 1].1;
        let right_ok = i + 1 == scores.len() || score <= scores[i + 1].1;
        if left_ok && right_ok {
            candidates.push((dy, score));
        }
    }
    candidates.sort_by(|a, b| a.1.total_cmp(&b.1));
    candidates.truncate(limit);
    candidates
}

fn select_coarse_candidates(scores: &[(i32, f32)], c_normal_max: i32) -> Vec<(i32, f32)> {
    let mut normal = Vec::new();
    let mut extreme = Vec::new();
    for i in 0..scores.len() {
        let (dy, score) = scores[i];
        let left_ok = i == 0 || score <= scores[i - 1].1;
        let right_ok = i + 1 == scores.len() || score <= scores[i + 1].1;
        if left_ok && right_ok {
            if dy.abs() <= c_normal_max {
                normal.push((dy, score));
            } else {
                extreme.push((dy, score));
            }
        }
    }
    normal.sort_by(|a, b| a.1.total_cmp(&b.1));
    normal.truncate(MAX_COARSE_CANDIDATES);
    extreme.sort_by(|a, b| a.1.total_cmp(&b.1));
    extreme.truncate(4);
    normal.extend(extreme);
    normal
}

struct FineSearch<'a> {
    prev: &'a FrameProfile,
    curr: &'a FrameProfile,
    active_top: i32,
    active_bottom: i32,
    max_shift: i32,
    weights: &'a BandWeights,
    cache: Vec<(i32, Option<FineScore>)>,
}

impl<'a> FineSearch<'a> {
    fn new(
        prev: &'a FrameProfile,
        curr: &'a FrameProfile,
        active_top: i32,
        active_bottom: i32,
        max_shift: i32,
        weights: &'a BandWeights,
    ) -> Self {
        Self {
            prev,
            curr,
            active_top,
            active_bottom,
            max_shift,
            weights,
            cache: Vec::with_capacity(64),
        }
    }

    fn score_at(&mut self, dy: i32) -> Option<FineScore> {
        if let Some(&(_, cached)) = self.cache.iter().find(|&&(d, _)| d == dy) {
            return cached;
        }
        let score = score_fine_shift(
            self.prev,
            self.curr,
            self.active_top,
            self.active_bottom,
            dy,
            self.weights,
        );
        self.cache.push((dy, score));
        score
    }

    fn refine_one(&mut self, c_dy: i32) -> Option<(i32, FineScore)> {
        let center = c_dy * COARSE_STRIDE as i32;
        let lo = (center - COARSE_STRIDE as i32).max(-self.max_shift);
        let hi = (center + COARSE_STRIDE as i32).min(self.max_shift);
        let mut best_local: Option<(i32, FineScore)> = None;
        for dy in lo..=hi {
            if let Some(score) = self.score_at(dy)
                && best_local.is_none_or(|(_, b)| score.effective_mad() < b.effective_mad())
            {
                best_local = Some((dy, score));
            }
        }
        best_local
    }

    fn refine_all(&mut self, candidates: &[(i32, f32)]) -> Vec<(i32, FineScore)> {
        let mut fine_results = Vec::with_capacity(candidates.len());
        for &(c_dy, _) in candidates {
            if let Some(best) = self.refine_one(c_dy) {
                fine_results.push(best);
            }
        }
        fine_results
    }
}

fn narrow_strict_ok(score: FineScore) -> bool {
    score.mad <= 3.5
        && score.inlier >= 0.94
        && score.textured_rows >= 8
        && score.vert_textured_rows >= 4
        && score.textured_mad <= 5.0
        && score.textured_inlier >= 0.92
        && score.worst_scrolling_band_mad <= 6.5
        && score.worst_scrolling_band_inlier >= 0.88
}

/// Estimate vertical displacement `dy` of `curr` relative to `prev` within
/// `[active_top .. active_bottom]` and horizontally across `fine_x`.
///
/// Convention: `dy > 0` means the user scrolled **down** (content moved up on
/// screen, so `curr` sits `dy` pixels below `prev` on the stitched canvas).
fn estimate_shift(
    prev: &FrameProfile,
    curr: &FrameProfile,
    active_top: u32,
    active_bottom: u32,
    fine_x: &std::ops::Range<usize>,
    last_dy: Option<i32>,
    prev_weights: &BandWeights,
) -> Result<i32, ScrollWarning> {
    let active_h = active_bottom.saturating_sub(active_top) as i32;
    if active_h < 32 || fine_x.len() < 8 {
        return Err(ScrollWarning::LowTexture);
    }

    // Check whether the active band actually has enough vertical texture to match.
    let textured_rows = (active_top as usize..active_bottom as usize)
        .filter(|&y| curr.row_vert_grad_in(y, fine_x) >= 1.0)
        .take(6)
        .count();
    if textured_rows < 6 {
        return Err(ScrollWarning::LowTexture);
    }

    let min_overlap = (active_h / 10).clamp(32, 120);
    let max_shift = (active_h * 90 / 100).min(active_h - min_overlap);
    if max_shift < 4 {
        return Err(ScrollWarning::LowTexture);
    }

    // Coarse search on 4x downsampled luma over both signs: [-max_c ..= +max_c].
    let c_top = (active_top as usize / COARSE_STRIDE) as i32;
    let c_bot = (active_bottom as usize / COARSE_STRIDE).min(prev.coarse_h) as i32;
    let c_active_h = c_bot - c_top;
    if c_active_h < 6 {
        return Err(ScrollWarning::LowTexture);
    }
    let c_max = (max_shift / COARSE_STRIDE as i32).max(1);
    let mut fine_search = FineSearch::new(
        prev,
        curr,
        active_top as i32,
        active_bottom as i32,
        max_shift,
        prev_weights,
    );

    // Momentum-guided narrow-window fast path for steady scrolling.
    if let Some(pred) = last_dy
        && pred != 0
        && pred.abs() <= active_h * 80 / 100
    {
        let c_pred = (pred as f32 / COARSE_STRIDE as f32).round() as i32;
        let half_win = (c_pred.abs() / 3).clamp(16, 48);
        let win_lo = (c_pred - half_win).max(-c_max);
        let win_hi = (c_pred + half_win).min(c_max);
        let narrow_coarse = coarse_band_scores(
            prev,
            curr,
            c_top,
            c_bot,
            win_lo,
            win_hi,
            prev_weights,
        );
        let narrow_cands = coarse_minima(&narrow_coarse, 5);
        if let Some(&(cand0_c, cand0_coarse_score)) = narrow_cands.first() {
            let cand0_fine = fine_search.refine_one(cand0_c);
            let cand0_sharp = cand0_fine.is_some_and(|(best_dy, best_score)| {
                narrow_strict_ok(best_score)
                    && [-2, 2].into_iter().any(|delta| {
                        let probe = best_dy + delta;
                        (-max_shift..=max_shift).contains(&probe)
                            && fine_search
                                .score_at(probe)
                                .is_some_and(|s| s.mad >= best_score.mad + 1.5)
                    })
            });

            if cand0_sharp {
                let (best_dy, best_score) = cand0_fine.unwrap();
                let best_eff = best_score.effective_mad();
                let mut conflict = false;
                for &(other_c, other_coarse) in &narrow_cands[1..] {
                    // Any refined dy from `other_c` stays within `other_c * 4 ± 4`; if
                    // `|other_c * 4 - best_dy| <= 7`, it cannot reach `|dy - best_dy| >= 12`.
                    if (other_c * COARSE_STRIDE as i32 - best_dy).abs() <= 7
                        || other_coarse > cand0_coarse_score * 3.0 + 15.0
                    {
                        continue;
                    }
                    if let Some((other_dy, other_score)) = fine_search.refine_one(other_c)
                        && (other_dy - best_dy).abs() >= 12
                        && (other_score.effective_mad() <= best_eff + 2.0
                            || other_score.textured_mad <= best_score.textured_mad + 3.0)
                    {
                        conflict = true;
                        break;
                    }
                }
                if !conflict {
                    return Ok(best_dy);
                }
            } else {
                let mut narrow_fine = Vec::with_capacity(narrow_cands.len());
                if let Some(first) = cand0_fine {
                    narrow_fine.push(first);
                }
                for &(c_dy, _) in &narrow_cands[1..] {
                    if let Some(res) = fine_search.refine_one(c_dy) {
                        narrow_fine.push(res);
                    }
                }
                narrow_fine.sort_by(|a, b| a.1.effective_mad().total_cmp(&b.1.effective_mad()));
                if let Some(&(best_dy, best_score)) = narrow_fine.first()
                    && narrow_strict_ok(best_score)
                {
                    let best_eff = best_score.effective_mad();
                    let unique_in_window = !narrow_fine.iter().any(|&(dy, s)| {
                        (dy - best_dy).abs() >= 12
                            && (s.effective_mad() <= best_eff + 2.0
                                || s.textured_mad <= best_score.textured_mad + 3.0)
                    });
                    let sharp_peak = [-2, 2].into_iter().any(|delta| {
                        let probe = best_dy + delta;
                        (-max_shift..=max_shift).contains(&probe)
                            && fine_search
                                .score_at(probe)
                                .is_some_and(|s| s.mad >= best_score.mad + 1.5)
                    });
                    if unique_in_window && sharp_peak {
                        return Ok(best_dy);
                    }
                }
            }
        }
    }

    let (coarse_scores, median_coarse_score) = if c_max >= 32 {
        coarse_band_scores_two_step(prev, curr, c_top, c_bot, -c_max, c_max, prev_weights)
    } else {
        let scores = coarse_band_scores(prev, curr, c_top, c_bot, -c_max, c_max, prev_weights);
        if scores.is_empty() {
            return Err(ScrollWarning::TooFast);
        }
        let mut raw_coarse: Vec<f32> = scores.iter().map(|&(_, s)| s).collect();
        let mid = raw_coarse.len() / 2;
        let (_, median_ref, _) = raw_coarse.select_nth_unstable_by(mid, f32::total_cmp);
        let median = *median_ref;
        (scores, median)
    };
    if coarse_scores.is_empty() {
        return Err(ScrollWarning::TooFast);
    }

    let normal_limit = active_h * 80 / 100;
    let c_normal_max = normal_limit / COARSE_STRIDE as i32;
    let candidates = select_coarse_candidates(&coarse_scores, c_normal_max);
    let mut fine_results = fine_search.refine_all(&candidates);

    if fine_results.is_empty() {
        return Err(ScrollWarning::TooFast);
    }

    fine_results.sort_by(|a, b| a.1.effective_mad().total_cmp(&b.1.effective_mad()));

    let normal_ok = |s: &FineScore| {
        let standard_ok = s.standard_ok();
        let trimmed_ok = s.qualifies_trimmed();
        let sharp_aa_ok = s.textured_rows >= 6
            && s.vert_textured_rows >= 2
            && s.mad <= 10.5
            && s.inlier >= 0.84
            && s.textured_mad <= 18.0
            && s.textured_inlier >= 0.75
            && s.worst_scrolling_band_mad <= 22.0
            && s.worst_scrolling_band_inlier >= 0.68
            && median_coarse_score >= 12.0
            && median_coarse_score >= s.mad * 2.5;
        standard_ok || trimmed_ok || sharp_aa_ok
    };

    // Prefer an accepted normal-range match (large overlap) before considering
    // tiny-overlap extreme shifts (> 80% of viewport), so a localized hover change
    // or transient partial glitch never causes an extreme jump or false TooFast.
    if let Some(&(best_dy, best_score)) = fine_results
        .iter()
        .find(|&&(dy, ref s)| dy.abs() <= normal_limit && normal_ok(s))
    {
        let best_eff = best_score.effective_mad();
        let near_ties: Vec<(i32, FineScore)> = fine_results
            .iter()
            .copied()
            .filter(|(dy, score)| {
                dy.abs() <= normal_limit
                    && (*dy == best_dy
                        || ((score.standard_ok() || score.qualifies_trimmed())
                            && score.effective_mad() <= best_eff + 1.5
                            && score.textured_mad <= best_score.textured_mad + 2.0
                            && score.worst_scrolling_band_mad
                                <= best_score.worst_scrolling_band_mad + 3.0))
            })
            .collect();

        if near_ties.len() > 1
            && near_ties.iter().any(|(dy, _)| (dy - best_dy).abs() >= 12)
            && let Some(prev_dy) = last_dy
        {
            let chosen = near_ties
                .iter()
                .copied()
                .min_by(|(dy_a, s_a), (dy_b, s_b)| {
                    let cost_a = s_a.effective_mad() + ((*dy_a - prev_dy).abs() as f32) * 0.004;
                    let cost_b = s_b.effective_mad() + ((*dy_b - prev_dy).abs() as f32) * 0.004;
                    cost_a.total_cmp(&cost_b)
                })
                .unwrap();
            return Ok(chosen.0);
        }

        return Ok(best_dy);
    }

    let (best_dy, best_score) = fine_results[0];
    let best_eff = best_score.effective_mad();
    let accepted = if best_dy.abs() > normal_limit {
        let strong_match = (best_score.mad <= 5.0
            && best_score.inlier >= 0.92
            && best_score.textured_mad <= 6.0
            && best_score.worst_scrolling_band_mad <= 12.0
            && best_score.worst_scrolling_band_inlier >= 0.80)
            || (best_score.trimmed_grad_share >= 0.65
                && best_score.trimmed_mad <= 3.5
                && best_score.trimmed_inlier >= 0.95);
        let unique = !fine_results.iter().any(|&(dy, s)| {
            (dy - best_dy).abs() >= 12
                && (s.standard_ok() || s.qualifies_trimmed())
                && s.effective_mad() < best_eff + 3.0
        });
        best_score.textured_rows >= 6
            && best_score.vert_textured_rows >= 2
            && strong_match
            && unique
    } else {
        false
    };

    if !accepted {
        log::debug!(
            "scroll estimate rejected: best_dy={best_dy} mad={:.2} inlier={:.3} trimmed_mad={:.2} trimmed_inlier={:.3} trimmed_grad_share={:.2} textured_rows={} median_coarse={:.2} max_shift={max_shift} active_h={active_h}",
            best_score.mad,
            best_score.inlier,
            best_score.trimmed_mad,
            best_score.trimmed_inlier,
            best_score.trimmed_grad_share,
            best_score.textured_rows,
            median_coarse_score
        );
        return Err(ScrollWarning::TooFast);
    }

    Ok(best_dy)
}

fn score_fine_shift(
    prev: &FrameProfile,
    curr: &FrameProfile,
    active_top: i32,
    active_bottom: i32,
    dy: i32,
    weights: &BandWeights,
) -> Option<FineScore> {
    let ov_start = active_top.max(active_top + dy);
    let ov_end = active_bottom.min(active_bottom + dy);
    let ov_len = ov_end - ov_start;
    let fx0 = weights.fine_x.start;
    let fx1 = weights.fine_x.end;
    let span_w = fx1.saturating_sub(fx0);
    if ov_len < 16 || span_w < 8 {
        return None;
    }

    // Collect up to MAX_FINE_ROWS stratified rows across the overlap, keeping
    // the highest-gradient row within each bucket so text/edges drive alignment.
    let buckets = MAX_FINE_ROWS.min(ov_len as usize);
    let mut weighted_err = 0.0_f32;
    let mut weight_sum = 0.0_f32;
    let mut inliers = 0usize;
    let mut samples = 0usize;
    let mut textured_rows = 0usize;
    let mut vert_textured_rows = 0usize;

    let use_bands = span_w >= 64;
    let mut band_weighted_err = [0.0_f32; 8];
    let mut band_weight_sum = [0.0_f32; 8];
    let mut band_inliers = [0usize; 8];
    let mut band_samples = [0usize; 8];
    let mut band_grad = [0u64; 8];
    let mut band_edge_total = [0usize; 8];
    let mut band_edge_static = [0usize; 8];
    let mut band_same_inliers = [0usize; 8];

    for b in 0..buckets {
        let r0 = ov_start + (b as i32 * ov_len) / buckets as i32;
        let r1 = (ov_start + ((b + 1) as i32 * ov_len) / buckets as i32).max(r0 + 1);
        let mut pick_y = r0;
        let mut best_w = -1.0_f32;
        let mut best_hg = -1.0_f32;
        for y in r0..r1 {
            let mw = weights.row_match_weight[y as usize];
            let hg = weights.row_grad[y as usize];
            if mw > best_w || (mw == best_w && hg > best_hg) {
                best_w = mw;
                best_hg = hg;
                pick_y = y;
            }
        }
        let py = pick_y as usize;
        let cy = (pick_y - dy) as usize;
        let row_mw = weights.row_match_weight[py];
        let row_hg = weights.row_grad[py];
        if row_hg >= 1.5 {
            textured_rows += 1;
        }
        if row_mw >= 1.0 {
            vert_textured_rows += 1;
        }
        let p_row = &prev.fine_luma[py * prev.fine_w..(py + 1) * prev.fine_w];
        let prev_y = py.saturating_sub(1);
        let p_above = &prev.fine_luma[prev_y * prev.fine_w..(prev_y + 1) * prev.fine_w];
        let next_y = (py + 1).min(prev.height as usize - 1);
        let p_below = &prev.fine_luma[next_y * prev.fine_w..(next_y + 1) * prev.fine_w];
        let c_row = &curr.fine_luma[cy * curr.fine_w..(cy + 1) * curr.fine_w];
        let c_same_row = &curr.fine_luma[py * curr.fine_w..(py + 1) * curr.fine_w];
        let w = row_mw.max(row_hg * 0.25).max(0.5);

        if use_bands {
            let mut row_err = 0u32;
            for band in 0..8 {
                let bx0 = fx0 + band * span_w / 8;
                let bx1 = fx0 + (band + 1) * span_w / 8;
                let band_len = bx1 - bx0;
                let mut b_err = 0u32;
                let mut b_inl = 0usize;
                let mut b_grad = 0u64;
                for x in bx0..bx1 {
                    let pa = p_row[x];
                    let d = pa.abs_diff(c_row[x]);
                    b_err += u32::from(d);
                    if d <= INLIER_THRESHOLD {
                        b_inl += 1;
                    }
                    let same_static = pa.abs_diff(c_same_row[x]) <= INLIER_THRESHOLD;
                    if same_static {
                        band_same_inliers[band] += 1;
                    }
                    let v_edge = pa.abs_diff(p_above[x]).max(pa.abs_diff(p_below[x]));
                    b_grad += u64::from(v_edge);
                    if v_edge >= 12 || (x > fx0 && pa.abs_diff(p_row[x - 1]) >= 12) {
                        band_edge_total[band] += 1;
                        if same_static {
                            band_edge_static[band] += 1;
                        }
                    }
                }
                row_err += b_err;
                inliers += b_inl;
                samples += band_len;
                band_weighted_err[band] += (b_err as f32 / band_len as f32) * w;
                band_weight_sum[band] += w;
                band_inliers[band] += b_inl;
                band_samples[band] += band_len;
                band_grad[band] += b_grad;
            }
            weighted_err += (row_err as f32 / span_w as f32) * w;
            weight_sum += w;
        } else {
            let mut row_err = 0u32;
            for (&pa, &ca) in p_row[fx0..fx1].iter().zip(&c_row[fx0..fx1]) {
                let d = pa.abs_diff(ca);
                row_err += u32::from(d);
                if d <= INLIER_THRESHOLD {
                    inliers += 1;
                }
                samples += 1;
            }
            weighted_err += (row_err as f32 / span_w as f32) * w;
            weight_sum += w;
        }
    }

    if samples == 0 || weight_sum <= 0.0 {
        return None;
    }
    let mad = weighted_err / weight_sum;
    let inlier = inliers as f32 / samples as f32;
    let (
        textured_mad,
        textured_inlier,
        trimmed_mad,
        trimmed_inlier,
        trimmed_grad_share,
        worst_scrolling_band_mad,
        worst_scrolling_band_inlier,
    ) = if use_bands {
        let mut order: [usize; 8] = [0, 1, 2, 3, 4, 5, 6, 7];
        order.sort_by(|&a, &b| {
            let mad_a = band_weighted_err[a] / band_weight_sum[a].max(1.0);
            let mad_b = band_weighted_err[b] / band_weight_sum[b].max(1.0);
            mad_a.total_cmp(&mad_b)
        });
        let total_grad: u64 = band_grad.iter().sum();
        let min_band_grad = (total_grad / 24).max(1);
        let mut tex_err = 0.0_f32;
        let mut tex_w = 0.0_f32;
        let mut tex_inl = 0usize;
        let mut tex_samples = 0usize;
        if total_grad > 0 {
            for band in 0..8 {
                if band_grad[band] >= min_band_grad {
                    tex_err += band_weighted_err[band];
                    tex_w += band_weight_sum[band];
                    tex_inl += band_inliers[band];
                    tex_samples += band_samples[band];
                }
            }
        }
        let tx_mad = if tex_w > 0.0 { tex_err / tex_w } else { mad };
        let tx_inlier = if tex_samples > 0 {
            tex_inl as f32 / tex_samples as f32
        } else {
            inlier
        };

        let trimmable = |band: usize| {
            band_edge_total[band] > 0
                && band_samples[band] > 0
                && band_edge_static[band] * 100 >= band_edge_total[band] * 75
                && band_same_inliers[band] * 100 >= band_samples[band] * 85
        };
        let mut worst_scroll_mad = 0.0_f32;
        let mut worst_scroll_inlier = 1.0_f32;
        let mut any_scroll_band = false;
        if total_grad > 0 {
            for band in 0..8 {
                if band_grad[band] >= min_band_grad && !trimmable(band) && band_samples[band] > 0 {
                    let b_mad = band_weighted_err[band] / band_weight_sum[band].max(1.0);
                    let b_inl = band_inliers[band] as f32 / band_samples[band] as f32;
                    worst_scroll_mad = worst_scroll_mad.max(b_mad);
                    worst_scroll_inlier = worst_scroll_inlier.min(b_inl);
                    any_scroll_band = true;
                }
            }
        }
        let worst_scroll_mad = if any_scroll_band {
            worst_scroll_mad
        } else {
            mad
        };
        let worst_scroll_inlier = if any_scroll_band {
            worst_scroll_inlier
        } else {
            inlier
        };

        let mut kept_err = 0.0_f32;
        let mut kept_w = 0.0_f32;
        let mut kept_inl = 0usize;
        let mut kept_samples = 0usize;
        let mut kept_grad = 0u64;
        for (idx, &band) in order.iter().enumerate() {
            if idx >= 6 && trimmable(band) {
                continue;
            }
            kept_err += band_weighted_err[band];
            kept_w += band_weight_sum[band];
            kept_inl += band_inliers[band];
            kept_samples += band_samples[band];
            kept_grad += band_grad[band];
        }
        let t_mad = if kept_w > 0.0 { kept_err / kept_w } else { mad };
        let t_inlier = if kept_samples > 0 {
            kept_inl as f32 / kept_samples as f32
        } else {
            inlier
        };
        let t_grad = if total_grad > 0 {
            kept_grad as f32 / total_grad as f32
        } else {
            0.0
        };
        (
            tx_mad,
            tx_inlier,
            t_mad,
            t_inlier,
            t_grad,
            worst_scroll_mad,
            worst_scroll_inlier,
        )
    } else {
        (mad, inlier, mad, inlier, 0.0, mad, inlier)
    };

    Some(FineScore {
        mad,
        inlier,
        textured_mad,
        textured_inlier,
        trimmed_mad,
        trimmed_inlier,
        trimmed_grad_share,
        textured_rows,
        vert_textured_rows,
        worst_scrolling_band_mad,
        worst_scrolling_band_inlier,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a rich synthetic long document with horizontal bars, text-like
    /// vertical glyphs, optional sticky header/footer, and an optional moving
    /// scrollbar on the right edge.
    fn make_document(width: u32, content_height: u32) -> RgbaImage {
        let mut img = RgbaImage::new(width, content_height);
        for y in 0..content_height {
            let band = y / 40;
            let bg = if band % 2 == 0 { 246u8 } else { 234u8 };
            for x in 0..width {
                let mut r = bg;
                let mut g = bg;
                let mut b = bg;
                // Distinct per-row color/pattern bars so shifts are unambiguous.
                let local_y = y % 40;
                if (10..26).contains(&local_y) && x >= 24 && x < width - 40 {
                    // Pseudo-text blocks seeded by `band` and `x`.
                    let cell = x / 10;
                    let hash = band
                        .wrapping_mul(2654435761)
                        .wrapping_add(cell.wrapping_mul(1597334677));
                    if hash % 3 != 0 && x % 10 < 7 {
                        r = ((band * 37) % 160 + 20) as u8;
                        g = ((band * 73) % 160 + 20) as u8;
                        b = ((band * 113) % 160 + 20) as u8;
                    }
                }
                if local_y == 0 {
                    r = 210;
                    g = 215;
                    b = 225;
                }
                img.put_pixel(x, y, image::Rgba([r, g, b, 255]));
            }
        }
        img
    }

    fn viewport_frame(
        doc: &RgbaImage,
        scroll_y: u32,
        view_h: u32,
        sticky_top: u32,
        sticky_bottom: u32,
        shadow: bool,
        scrollbar: bool,
    ) -> RgbaImage {
        let w = doc.width();
        let mut out = RgbaImage::new(w, view_h);
        let active_h = view_h - sticky_top - sticky_bottom;
        for y in 0..view_h {
            for x in 0..w {
                let mut px = if y < sticky_top {
                    // Textured sticky header
                    let v = if y == sticky_top - 1 {
                        80
                    } else {
                        42 + ((x / 12 + y) % 5) as u8 * 15
                    };
                    image::Rgba([v, v + 10, v + 25, 255])
                } else if y >= view_h - sticky_bottom {
                    // Textured sticky footer
                    let ly = y - (view_h - sticky_bottom);
                    let v = if ly == 0 {
                        90
                    } else {
                        52 + ((x / 12 + ly) % 4) as u8 * 14
                    };
                    image::Rgba([v + 15, v, v + 5, 255])
                } else {
                    let doc_y = (scroll_y + (y - sticky_top)).min(doc.height() - 1);
                    let mut p = *doc.get_pixel(x, doc_y);
                    // Simulate sticky header drop shadow on the top 6 px of content.
                    if shadow && sticky_top > 0 && y - sticky_top < 6 {
                        let fade = (6 - (y - sticky_top)) as u8 * 8;
                        p.0[0] = p.0[0].saturating_sub(fade);
                        p.0[1] = p.0[1].saturating_sub(fade);
                        p.0[2] = p.0[2].saturating_sub(fade);
                    }
                    p
                };
                if scrollbar && x >= w - 12 && x < w - 2 {
                    let thumb_y = (scroll_y * (view_h - 60) / doc.height().max(1)).min(view_h - 60);
                    if y >= thumb_y && y < thumb_y + 50 {
                        px = image::Rgba([110, 110, 120, 255]);
                    } else {
                        px = image::Rgba([225, 225, 228, 255]);
                    }
                }
                out.put_pixel(x, y, px);
            }
        }
        let _ = active_h;
        out
    }

    #[test]
    fn downward_scroll_stitches_exact_height_and_pixels() {
        let doc = make_document(400, 1800);
        let view_h = 480;
        let mut stitcher = Stitcher::new(StitchConfig::default());
        for scroll_y in [0, 20, 45, 110, 260, 450, 680, 910, 1120, 1320] {
            let frame = viewport_frame(&doc, scroll_y, view_h, 0, 0, false, false);
            let _ = stitcher.feed(frame);
        }
        let result = stitcher.assemble().unwrap().unwrap();
        assert_eq!(result.dimensions(), (400, 1800));
        assert_eq!(result.as_raw(), doc.as_raw());
    }

    #[test]
    fn upward_scroll_stitches_exact_height_and_pixels() {
        let doc = make_document(360, 1400);
        let view_h = 440;
        let mut stitcher = Stitcher::new(StitchConfig::default());
        for scroll_y in [960, 820, 640, 460, 280, 120, 0] {
            let frame = viewport_frame(&doc, scroll_y, view_h, 0, 0, false, false);
            let _ = stitcher.feed(frame);
        }
        let result = stitcher.assemble().unwrap().unwrap();
        assert_eq!(result.dimensions(), (360, 1400));
        let first = result
            .as_raw()
            .iter()
            .zip(doc.as_raw())
            .position(|(a, b)| a != b);
        assert_eq!(
            first.map(|index| (index / 4, index % 4)),
            None,
            "pixel mismatch"
        );
    }

    #[test]
    fn changing_direction_preserves_the_complete_document() {
        let doc = make_document(360, 1800);
        let view_h = 440;
        let mut stitcher = Stitcher::new(StitchConfig::default());
        for scroll_y in [520, 400, 220, 60, 0, 240, 600, 820, 1000, 1180, 1360] {
            let _ = stitcher.feed(viewport_frame(&doc, scroll_y, view_h, 0, 0, false, false));
        }
        assert!(stitcher.content_shift() > 0);
        let result = stitcher.assemble().unwrap().unwrap();
        assert_eq!(result.dimensions(), doc.dimensions());
        assert_eq!(result.as_raw(), doc.as_raw());
    }

    #[test]
    fn upward_scroll_keeps_sticky_margins_once() {
        let doc = make_document(420, 1300);
        let mut stitcher = Stitcher::new(StitchConfig::default());
        for scroll_y in [700, 884, 700, 520, 340, 120, 0] {
            let _ = stitcher.feed(viewport_frame(&doc, scroll_y, 500, 48, 36, true, true));
        }
        let result = stitcher.assemble().unwrap().unwrap();
        assert_eq!(
            result.height(),
            1384,
            "margins={:?}",
            stitcher.fixed_margins()
        );
        assert_eq!(stitcher.fixed_margins(), (48, 36));
    }

    #[test]
    fn sticky_header_footer_with_shadow_and_scrollbar_never_repeat() {
        let active_doc = make_document(420, 1300);
        let view_h = 500;
        let sticky_top = 48;
        let sticky_bottom = 36;
        let mut stitcher = Stitcher::new(StitchConfig::default());
        for scroll_y in [0, 90, 210, 360, 520, 700, 884] {
            let frame = viewport_frame(
                &active_doc,
                scroll_y,
                view_h,
                sticky_top,
                sticky_bottom,
                true,
                true,
            );
            let _ = stitcher.feed(frame);
        }
        let result = stitcher.assemble().unwrap().unwrap();
        let (ft, fb) = stitcher.fixed_margins();
        assert!(ft >= sticky_top - 4 && ft <= sticky_top + 4, "ft={ft}");
        assert!(
            fb >= sticky_bottom - 4 && fb <= sticky_bottom + 4,
            "fb={fb}"
        );
        assert_eq!(
            result.height(),
            1300 + sticky_top + sticky_bottom,
            "sticky bars should appear only once"
        );
    }

    #[test]
    fn plain_white_header_is_not_misclassified_as_sticky_bar() {
        let mut doc = make_document(320, 1200);
        // Make top 100 rows plain white.
        for y in 0..100 {
            for x in 0..320 {
                doc.put_pixel(x, y, image::Rgba([255, 255, 255, 255]));
            }
        }
        let mut stitcher = Stitcher::new(StitchConfig::default());
        for scroll_y in [0, 30, 60, 150, 300] {
            let frame = viewport_frame(&doc, scroll_y, 420, 0, 0, false, false);
            let _ = stitcher.feed(frame);
        }
        assert_eq!(stitcher.fixed_margins(), (0, 0));
    }

    #[test]
    fn git_graph_vertical_lines_are_not_a_sticky_header() {
        let mut doc = RgbaImage::new(320, 1200);
        for y in 0..1200 {
            for x in 0..320 {
                let mut pixel = image::Rgba([20, 22, 28, 255]);
                if (24..300).contains(&x) && (x - 24) % 40 < 2 {
                    let branch = (x - 24) / 40;
                    pixel = image::Rgba([(60 + branch * 18) as u8, 90, 180, 255]);
                }
                if y % 80 < 8 {
                    let band = y / 80;
                    let cell = x / 12;
                    pixel = image::Rgba([
                        40 + ((band * 17) % 160) as u8,
                        70 + (cell * 5) as u8,
                        120,
                        255,
                    ]);
                }
                doc.put_pixel(x, y, pixel);
            }
        }
        let mut stitcher = Stitcher::new(StitchConfig::default());
        for scroll_y in (0..=400).step_by(40) {
            let _ = stitcher.feed(viewport_frame(&doc, scroll_y, 400, 0, 0, false, false));
        }

        assert_eq!(stitcher.fixed_margins(), (0, 0));
    }

    #[test]
    fn rubber_band_bounce_produces_no_duplication() {
        let doc = make_document(360, 1000);
        let view_h = 400;
        let mut stitcher = Stitcher::new(StitchConfig::default());
        // Scroll down to bottom (600), bounce back to 540, then settle at 600.
        for scroll_y in [0, 150, 320, 500, 600, 540, 570, 600] {
            let frame = viewport_frame(&doc, scroll_y, view_h, 0, 0, false, false);
            let _ = stitcher.feed(frame);
        }
        let result = stitcher.assemble().unwrap().unwrap();
        assert_eq!(result.dimensions(), (360, 1000));
        assert_eq!(result.as_raw(), doc.as_raw());
    }

    #[test]
    fn fast_scroll_warns_and_recovers_when_scrolled_back() {
        let doc = make_document(360, 1600);
        let view_h = 400;
        let mut stitcher = Stitcher::new(StitchConfig::default());
        assert!(matches!(
            stitcher.feed(viewport_frame(&doc, 0, view_h, 0, 0, false, false)),
            FeedOutcome::Initial
        ));
        assert!(matches!(
            stitcher.feed(viewport_frame(&doc, 160, view_h, 0, 0, false, false)),
            FeedOutcome::Stitched { .. } | FeedOutcome::Moved { .. }
        ));
        // Jump far past overlap -> TooFast warning
        let outcome = stitcher.feed(viewport_frame(&doc, 800, view_h, 0, 0, false, false));
        assert_eq!(outcome, FeedOutcome::Warning(ScrollWarning::TooFast));
        // Scroll back into overlap with anchor (340) -> recovers cleanly
        let recover = stitcher.feed(viewport_frame(&doc, 340, view_h, 0, 0, false, false));
        assert!(
            matches!(
                recover,
                FeedOutcome::Stitched { .. } | FeedOutcome::Moved { .. }
            ),
            "expected recovery, got {recover:?}"
        );
    }

    #[test]
    fn dimension_change_is_an_explicit_failure() {
        let mut stitcher = Stitcher::new(StitchConfig::default());
        assert_eq!(
            stitcher.feed(RgbaImage::new(320, 240)),
            FeedOutcome::Initial
        );
        assert_eq!(
            stitcher.feed(RgbaImage::new(321, 240)),
            FeedOutcome::DimensionsChanged {
                expected: (320, 240),
                actual: (321, 240),
            }
        );
    }

    #[test]
    fn reset_preview_never_reuses_revisions() {
        let mut stitcher = Stitcher::new(StitchConfig::default());
        stitcher.feed(RgbaImage::new(320, 1200));
        let old_revision = stitcher.preview_revision();
        stitcher = Stitcher::new(StitchConfig::default());
        stitcher.feed(RgbaImage::new(320, 1200));
        assert_ne!(stitcher.preview_revision(), old_revision);
        assert_eq!(
            stitcher.preview_tile().unwrap().image.dimensions(),
            (320, 1)
        );
    }

    #[test]
    fn canvas_accepts_frames_beyond_the_legacy_height_limit() {
        let mut stitcher = Stitcher::new(StitchConfig::default());
        assert!(matches!(
            stitcher.feed(RgbaImage::new(4, 30_001)),
            FeedOutcome::Initial
        ));
        assert_eq!(stitcher.total_height(), 30_001);
    }

    #[test]
    fn disjoint_commits_are_rejected() {
        let mut stitcher = Stitcher::new(StitchConfig::default());
        stitcher.width = 4;
        stitcher.frame_height = 100;
        let initial = Arc::new(FrameProfile::from_image(RgbaImage::new(4, 100), 1.0));
        stitcher.committed = Some(CommittedAnchor::new(initial, 0));
        stitcher.rows = PagedRows::new(4);
        stitcher.rows.append_down(0, &vec![0; 100 * 16]).unwrap();
        stitcher.below_rows = 100;

        let raw_img = RgbaImage::new(4, 100);
        let frame = Arc::new(FrameProfile::from_image(raw_img.clone(), 1.0));
        assert!(stitcher.commit_frame(frame.clone(), &raw_img, 150).is_err());
        assert_eq!(stitcher.total_height(), 100);
        assert_eq!(stitcher.committed.as_ref().unwrap().y, 0);

        stitcher.rows = PagedRows::new(4);
        stitcher.rows.prepend_up(0, &vec![0; 100 * 16]).unwrap();
        stitcher.below_rows = 0;
        stitcher.above_rows = 100;
        assert!(stitcher.commit_frame(frame, &raw_img, -250).is_err());
        assert_eq!(stitcher.total_height(), 100);
        assert_eq!(stitcher.committed.as_ref().unwrap().y, 0);
    }

    #[test]
    fn seam_scoring_ignores_scrollbar_pixels() {
        let mut frame = RgbaImage::new(400, 100);
        for y in 0..100 {
            for x in 0..400 {
                frame.put_pixel(x, y, image::Rgba([120, 120, 120, 255]));
            }
        }
        let mut stitcher = Stitcher::new(StitchConfig::default());
        stitcher.width = 400;
        stitcher.frame_height = 100;
        stitcher.rows = PagedRows::new(400);
        stitcher
            .rows
            .append_down(0, &vec![0; 100 * 400 * 4])
            .unwrap();
        stitcher.below_rows = 100;
        let mut row = frame.as_raw()[50 * 400 * 4..51 * 400 * 4].to_vec();
        row[390 * 4..391 * 4].fill(0);
        stitcher.rows.write_row(50, &row).unwrap();

        let seam = stitcher.best_seam_down(&frame, 0, 0, 0, 100).unwrap();
        assert_eq!(seam, 50);
    }

    #[test]
    fn preview_at_canvas_end_uses_last_row() {
        let mut image = RgbaImage::new(320, 1200);
        for y in 0..1200 {
            for x in 0..320 {
                image.put_pixel(x, y, image::Rgba([x as u8, y as u8, 80, 255]));
            }
        }
        let mut stitcher = Stitcher::new(StitchConfig::default());
        stitcher.feed(image.clone());
        stitcher.set_preview_range(1200, 100);
        stitcher.refresh_preview().unwrap();

        assert_eq!(stitcher.preview_range(), (1199, 1));
        let tile = stitcher.preview_tile().unwrap();
        assert_eq!(tile.image.dimensions(), (320, 1));
        assert_eq!(tile.image.as_raw(), &image.as_raw()[1199 * 320 * 4..]);
    }

    #[test]
    fn fixed_margins_can_shrink_without_losing_active_rows() {
        let mut frame = RgbaImage::new(4, 300);
        for y in 0..300 {
            let value = (y % 251) as u8;
            for x in 0..4 {
                frame.put_pixel(x, y, image::Rgba([value, x as u8, y as u8, 255]));
            }
        }
        let profile = Arc::new(FrameProfile::from_image(frame.clone(), 1.0));
        let mut stitcher = Stitcher::new(StitchConfig::default());
        stitcher.width = 4;
        stitcher.frame_height = 300;
        stitcher.fixed_top = 40;
        stitcher.fixed_bottom = 20;
        stitcher.above_rows = 60;
        stitcher.below_rows = 180;
        let mut active = Vec::new();
        for y in -60i32..180 {
            for x in 0..4 {
                active.extend_from_slice(&[y as u8, x as u8, 90, 255]);
            }
        }
        stitcher.rows = PagedRows::new(4);
        stitcher.rows.append_down(-60, &active).unwrap();
        stitcher.committed = Some(CommittedAnchor::new(profile.clone(), 17));
        stitcher.prev = Some(PendingAnchor::new(
            profile,
            Arc::new(PooledImage::unpooled(frame.clone())),
            23,
        ));
        stitcher.viewport_y = 23;
        stitcher.header_rows = raw_rows(&frame, 0, 40).to_vec();
        stitcher.footer_rows = raw_rows(&frame, 280, 300).to_vec();

        stitcher.apply_fixed_margins(24, 8, &frame).unwrap();

        assert_eq!(stitcher.fixed_margins(), (24, 8));
        assert_eq!(stitcher.above_rows, 76);
        assert_eq!(stitcher.below_rows, 192);
        assert_eq!(stitcher.total_height(), 300);
        assert_eq!(
            stitcher.rows.read_rows(-60, 240).unwrap().as_raw(),
            active.as_slice()
        );
        assert_eq!(stitcher.header_rows, raw_rows(&frame, 0, 24));
        assert_eq!(
            stitcher.rows.read_rows(-76, 16).unwrap().as_raw(),
            raw_rows(&frame, 24, 40)
        );
        assert_eq!(stitcher.footer_rows, raw_rows(&frame, 292, 300));
        assert_eq!(
            stitcher.rows.read_rows(180, 12).unwrap().as_raw(),
            raw_rows(&frame, 280, 292)
        );
        assert_eq!(stitcher.committed.as_ref().unwrap().y, 1);
        assert_eq!(stitcher.prev.as_ref().unwrap().y, 7);
        assert_eq!(stitcher.viewport_y, 7);
    }

    #[test]
    fn small_sticky_margins_require_exact_votes() {
        assert_eq!(Stitcher::next_margin_vote(Some((2, 2)), 0), Some((0, 1)));
        assert_eq!(Stitcher::next_margin_vote(Some((0, 2)), 2), Some((2, 1)));
        assert_eq!(Stitcher::next_margin_vote(Some((0, 1)), 0), Some((0, 2)));
        assert_eq!(
            Stitcher::next_margin_vote(Some((48, MARGIN_CHANGE_VOTES)), 47),
            Some((48, MARGIN_CHANGE_VOTES))
        );
    }

    #[test]
    fn zero_sticky_votes_clear_an_existing_header() {
        let doc = make_document(400, 1200);
        let prev_img = viewport_frame(&doc, 0, 300, 48, 0, false, false);
        let curr_img = viewport_frame(&doc, 40, 300, 0, 0, false, false);
        let prev = FrameProfile::from_image(prev_img.clone(), 1.0);
        let curr = FrameProfile::from_image(curr_img.clone(), 1.0);
        let mut stitcher = Stitcher::new(StitchConfig::default());
        stitcher.width = 400;
        stitcher.frame_height = 300;
        stitcher.fixed_top = 48;
        stitcher.cand_top_votes = Some((48, 2));
        stitcher.cand_bottom_votes = Some((0, 2));
        stitcher.rows = PagedRows::new(400);
        stitcher
            .rows
            .append_down(0, raw_rows(&prev_img, 48, 300))
            .unwrap();
        stitcher.below_rows = 252;
        stitcher.header_rows = raw_rows(&prev_img, 0, 48).to_vec();

        stitcher.update_fixed_margins(&prev, &curr, &curr_img).unwrap();
        assert_eq!(stitcher.cand_top_votes, Some((0, 1)));
        assert_eq!(stitcher.fixed_margins(), (48, 0));
        for _ in 1..MARGIN_CHANGE_VOTES - 1 {
            stitcher.update_fixed_margins(&prev, &curr, &curr_img).unwrap();
            assert_eq!(stitcher.fixed_margins(), (48, 0));
        }
        stitcher.update_fixed_margins(&prev, &curr, &curr_img).unwrap();

        assert_eq!(stitcher.cand_top_votes, Some((0, MARGIN_CHANGE_VOTES)));
        assert_eq!(stitcher.fixed_margins(), (0, 0));
        assert!(stitcher.header_rows.is_empty());
        assert_eq!(stitcher.total_height(), 300);
    }

    #[test]
    fn delayed_sticky_margins_keep_upward_origin() {
        let mut frame = RgbaImage::new(4, 300);
        for y in 0..300 {
            let value = (y % 251) as u8;
            for x in 0..4 {
                frame.put_pixel(x, y, image::Rgba([value, x as u8, y as u8, 255]));
            }
        }
        let mut stitcher = Stitcher::new(StitchConfig::default());
        stitcher.width = 4;
        stitcher.frame_height = 300;
        stitcher.rows = PagedRows::new(4);
        stitcher.rows.append_down(-100, frame.as_raw()).unwrap();
        stitcher.above_rows = 100;
        stitcher.below_rows = 200;

        stitcher.apply_fixed_margins(40, 20, &frame).unwrap();

        assert_eq!(stitcher.fixed_margins(), (40, 20));
        assert_eq!(stitcher.above_rows, 60);
        assert_eq!(stitcher.content_shift(), 60);
        assert_eq!(stitcher.below_rows, 180);
        assert_eq!(stitcher.total_height(), 300);
        assert_eq!(stitcher.header_rows, raw_rows(&frame, 0, 40));
        assert_eq!(stitcher.footer_rows, raw_rows(&frame, 280, 300));
    }

    #[test]
    fn upward_canvas_rows_and_flush_preserve_final_content_shift() {
        let doc = make_document(360, 1400);
        let view_h = 440;
        let mut stitcher = Stitcher::new(StitchConfig::default());
        for scroll_y in [960, 820, 640, 460, 280, 120, 0, 35] {
            let _ = stitcher.feed(viewport_frame(&doc, scroll_y, view_h, 0, 0, false, false));
        }
        let shift_before_flush = stitcher.content_shift();
        stitcher.flush_pending().unwrap();
        let shift = stitcher.content_shift();
        assert!(shift >= shift_before_flush);
        assert_eq!(shift, stitcher.above_rows);
        let assembled = stitcher.assemble().unwrap().unwrap();
        for y in 0..assembled.height() {
            let stride = assembled.width() as usize * 4;
            let offset = y as usize * stride;
            assert_eq!(
                stitcher.canvas_row(y).unwrap(),
                assembled.as_raw()[offset..offset + stride],
                "canvas row {y}"
            );
        }
    }

    #[test]
    fn initial_frame_has_no_configured_height_limit() {
        let mut stitcher = Stitcher::new(StitchConfig::default());
        assert!(matches!(
            stitcher.feed(RgbaImage::new(32, 301)),
            FeedOutcome::Initial
        ));
        assert_eq!(stitcher.total_height(), 301);
        assert_eq!(stitcher.width(), 32);
    }

    #[test]
    fn visible_preview_matches_its_assembled_source_rows() {
        let doc = make_document(640, 3200);
        let mut stitcher = Stitcher::new(StitchConfig::default());
        for scroll_y in (0..=2700).step_by(200) {
            let _ = stitcher.feed(viewport_frame(&doc, scroll_y, 500, 0, 0, false, false));
        }
        stitcher.set_preview_range(700, 900);
        stitcher.refresh_preview().unwrap();
        let assembled = stitcher.assemble().unwrap().unwrap();
        let tile = stitcher.preview_tile().unwrap();
        assert_eq!(stitcher.preview_range(), (700, 900));
        assert!(tile.image.width() <= PREVIEW_MAX_WIDTH);
        for py in 0..tile.image.height() {
            let sy = 700
                + (u64::from(py) * u64::from(assembled.width()) / u64::from(tile.image.width()))
                    as u32;
            for px in 0..tile.image.width() {
                let sx = (u64::from(px) * u64::from(assembled.width())
                    / u64::from(tile.image.width())) as u32;
                assert_eq!(
                    tile.image.get_pixel(px, py),
                    assembled.get_pixel(sx, sy.min(1599)),
                    "pixel ({px}, {py})"
                );
            }
        }
    }

    #[test]
    fn annotation_frame_matches_the_final_canvas_with_sticky_margins() {
        let doc = make_document(320, 2400);
        let mut stitcher = Stitcher::new(StitchConfig::default());
        for scroll_y in (0..=1000).step_by(100) {
            let _ = stitcher.feed(viewport_frame(&doc, scroll_y, 500, 40, 20, true, true));
        }
        stitcher.flush_pending().unwrap();
        let (paused, origin) = stitcher.annotation_frame().unwrap().unwrap();
        let assembled = stitcher.assemble().unwrap().unwrap();
        assert_eq!(paused.dimensions(), (320, 500));
        assert_eq!(
            paused,
            image::imageops::crop_imm(
                &assembled,
                0,
                origin.clamp(0, assembled.height().saturating_sub(500) as i64) as u32,
                320,
                500,
            )
            .to_image()
        );
    }

    #[test]
    fn annotation_frame_at_scrolls_back_to_earlier_content() {
        let doc = make_document(320, 2400);
        let mut stitcher = Stitcher::new(StitchConfig::default());
        for scroll_y in (0..=1000).step_by(100) {
            let _ = stitcher.feed(viewport_frame(&doc, scroll_y, 500, 0, 0, false, false));
        }
        stitcher.flush_pending().unwrap();
        let assembled = stitcher.assemble().unwrap().unwrap();
        let live_origin = stitcher.viewport_origin_y();
        assert!(live_origin > 0, "downward capture must end below the top");

        let (top, origin) = stitcher.annotation_frame_at(0).unwrap().unwrap();
        assert_eq!(origin, 0);
        assert_eq!(
            top,
            image::imageops::crop_imm(&assembled, 0, 0, 320, 500).to_image()
        );

        let mid_origin = live_origin / 2;
        let (mid, origin) = stitcher.annotation_frame_at(mid_origin).unwrap().unwrap();
        assert_eq!(origin, mid_origin);
        assert_eq!(
            mid,
            image::imageops::crop_imm(&assembled, 0, mid_origin as u32, 320, 500).to_image()
        );

        let (_, clamped) = stitcher.annotation_frame_at(i64::MAX).unwrap().unwrap();
        assert_eq!(clamped, assembled.height().saturating_sub(500) as i64);
    }

    #[test]
    fn long_canvas_keeps_the_visible_preview_bounded() {
        let doc = make_document(640, 32_000);
        let mut stitcher = Stitcher::new(StitchConfig::default());
        for scroll_y in (0..=31_500).step_by(200) {
            let _ = stitcher.feed(viewport_frame(&doc, scroll_y, 500, 0, 0, false, false));
        }
        stitcher.flush_pending().unwrap();
        let assembled = stitcher.assemble().unwrap().unwrap();
        assert!(assembled.height() > 30_000);
        let tile = stitcher.preview_tile().unwrap();
        assert!(tile.image.width() <= PREVIEW_MAX_WIDTH);
        assert_eq!(tile.image.dimensions(), (320, 1));
    }

    #[test]
    fn large_86_percent_jump_stitches_cleanly_when_unambiguous() {
        let doc = make_document(400, 1400);
        let view_h = 500;
        let mut stitcher = Stitcher::new(StitchConfig::default());
        assert!(matches!(
            stitcher.feed(viewport_frame(&doc, 0, view_h, 0, 0, false, false)),
            FeedOutcome::Initial
        ));
        let outcome = stitcher.feed(viewport_frame(&doc, 430, view_h, 0, 0, false, false));
        assert!(
            matches!(
                outcome,
                FeedOutcome::Stitched { dy: 430, .. } | FeedOutcome::Moved { dy: 430 }
            ),
            "expected 86% jump to match, got {outcome:?}"
        );
        let outcome2 = stitcher.feed(viewport_frame(&doc, 860, view_h, 0, 0, false, false));
        assert!(
            matches!(
                outcome2,
                FeedOutcome::Stitched { dy: 430, .. } | FeedOutcome::Moved { dy: 430 }
            ),
            "expected second 86% jump to match, got {outcome2:?}"
        );
        let result = stitcher.assemble().unwrap().unwrap();
        assert_eq!(result.dimensions(), (400, 1360));
    }

    #[test]
    fn floating_sidebar_overlay_does_not_trigger_too_fast() {
        let doc = make_document(480, 1400);
        let view_h = 440;
        let mut stitcher = Stitcher::new(StitchConfig::default());
        let with_floating_widget = |scroll_y: u32| {
            let mut frame = viewport_frame(&doc, scroll_y, view_h, 0, 0, false, false);
            // Draw a textured fixed widget covering ~18% on the right side.
            for y in 40..380 {
                for x in 380..468 {
                    let v = if (x + y) % 10 < 5 { 30 } else { 220 };
                    frame.put_pixel(x, y, image::Rgba([v, 60, 180, 255]));
                }
            }
            frame
        };
        assert!(matches!(
            stitcher.feed(with_floating_widget(0)),
            FeedOutcome::Initial
        ));
        for scroll_y in [110, 230, 360, 500, 640] {
            let outcome = stitcher.feed(with_floating_widget(scroll_y));
            assert!(
                matches!(
                    outcome,
                    FeedOutcome::Stitched { .. } | FeedOutcome::Moved { .. }
                ),
                "expected match despite floating widget at {scroll_y}, got {outcome:?}"
            );
        }
    }

    #[test]
    fn narrow_centered_text_with_white_margins_still_rejects_disjoint_jump() {
        let mut doc = RgbaImage::from_pixel(480, 1400, image::Rgba([255, 255, 255, 255]));
        for y in 0..1400u32 {
            let band = y / 32;
            let local_y = y % 32;
            if (8..22).contains(&local_y) {
                for x in 190..290u32 {
                    let mut h = band
                        .wrapping_mul(2654435761)
                        .wrapping_add((x / 8).wrapping_mul(1597334677));
                    h ^= h >> 15;
                    h = h.wrapping_mul(2246822519);
                    if (h >> 16) % 3 != 0 {
                        let r = ((band * 53) % 160 + 15) as u8;
                        let g = ((band * 97) % 160 + 15) as u8;
                        let b = ((band * 139) % 160 + 15) as u8;
                        doc.put_pixel(x, y, image::Rgba([r, g, b, 255]));
                    }
                }
            }
        }
        let view_h = 400;
        let mut stitcher = Stitcher::new(StitchConfig::default());
        assert!(matches!(
            stitcher.feed(viewport_frame(&doc, 0, view_h, 0, 0, false, false)),
            FeedOutcome::Initial
        ));
        let outcome = stitcher.feed(viewport_frame(&doc, 700, view_h, 0, 0, false, false));
        assert_eq!(outcome, FeedOutcome::Warning(ScrollWarning::TooFast));
    }

    #[test]
    fn subpixel_blur_still_matches_sharp_peak() {
        let doc = make_document(400, 1200);
        let view_h = 440;
        let mut stitcher = Stitcher::new(StitchConfig::default());
        assert!(matches!(
            stitcher.feed(viewport_frame(&doc, 0, view_h, 0, 0, false, false)),
            FeedOutcome::Initial
        ));
        // Simulate mild subpixel vertical AA/jitter on the moving frame.
        let sharp = viewport_frame(&doc, 140, view_h, 0, 0, false, false);
        let mut blurred = sharp.clone();
        for y in 1..view_h - 1 {
            for x in 0..400 {
                let p0 = sharp.get_pixel(x, y).0;
                let p1 = sharp.get_pixel(x, y + 1).0;
                let mix = |a: u8, b: u8| ((u16::from(a) * 3 + u16::from(b)) / 4) as u8;
                blurred.put_pixel(
                    x,
                    y,
                    image::Rgba([mix(p0[0], p1[0]), mix(p0[1], p1[1]), mix(p0[2], p1[2]), 255]),
                );
            }
        }
        let outcome = stitcher.feed(blurred);
        assert!(
            matches!(
                outcome,
                FeedOutcome::Stitched { dy, .. } | FeedOutcome::Moved { dy } if (dy - 140).abs() <= 1
            ),
            "expected subpixel blurred frame to match around 140, got {outcome:?}"
        );
    }

    #[test]
    fn live_preview_follow_tracks_bottom_without_lag() {
        let doc = make_document(400, 1800);
        let view_h = 440;
        let mut stitcher = Stitcher::new(StitchConfig::default());
        stitcher.feed(viewport_frame(&doc, 0, view_h, 0, 0, false, false));
        stitcher.set_preview_range_with_follow(0, 600, true);
        for scroll_y in [160, 360, 620, 900, 1180, 1360] {
            let _ = stitcher.feed(viewport_frame(&doc, scroll_y, view_h, 0, 0, false, false));
            let (start, rows) = stitcher.preview_range();
            assert_eq!(start + rows, stitcher.total_height());
        }
    }

    #[test]
    fn vertical_rails_do_not_trigger_fixed_margins() {
        let width = 480u32;
        let height = 2200u32;
        let mut doc = RgbaImage::from_pixel(width, height, image::Rgba([24, 26, 32, 255]));
        // 16 vertical git-graph rails covering the left half plus sparsely
        // spaced commit message rows (simulating dense branch lines where top
        // 40px can happen to contain only rails + 1 thin row).
        for y in 0..height {
            for lane in 0..16u32 {
                let rx = 20 + lane * 14;
                let r = 60 + ((lane * 37) % 170) as u8;
                let g = 80 + ((lane * 73) % 160) as u8;
                let b = 90 + ((lane * 113) % 150) as u8;
                for dx in 0..3 {
                    doc.put_pixel(rx + dx, y, image::Rgba([r, g, b, 255]));
                }
            }
            if y % 56 >= 18 && y % 56 < 32 {
                let commit = y / 56;
                for x in 260..440u32 {
                    if (x / 6 + commit) % 3 != 0 {
                        let c = 150 + ((commit * 29 + x) % 90) as u8;
                        doc.put_pixel(x, y, image::Rgba([c, c, 210, 255]));
                    }
                }
            }
        }
        let mut stitcher = Stitcher::new(StitchConfig::default());
        for scroll_y in (0..=1680).step_by(56) {
            let _ = stitcher.feed(viewport_frame(&doc, scroll_y, 500, 0, 0, false, false));
        }
        let _ = stitcher.feed(viewport_frame(&doc, 1700, 500, 0, 0, false, false));
        assert_eq!(stitcher.fixed_margins(), (0, 0));
        let assembled = stitcher.assemble().unwrap().unwrap();
        assert_eq!(assembled.dimensions(), doc.dimensions());
        assert_eq!(assembled.as_raw(), doc.as_raw());
    }

    #[test]
    fn toggling_fixed_top_mid_scroll_preserves_top_and_alignment() {
        let doc = make_document(400, 2400);
        let view_h = 500u32;
        let mut stitcher = Stitcher::new(StitchConfig::default());
        for scroll_y in [0u32, 140, 300, 480, 680, 900] {
            let _ = stitcher.feed(viewport_frame(&doc, scroll_y, view_h, 0, 0, false, false));
        }
        stitcher.flush_pending().unwrap();
        let mid_frame = viewport_frame(&doc, 900, view_h, 0, 0, false, false);
        // Simulate transient top/bottom detection mid-page and back to (0, 0),
        // including growing while `above_rows > 0`.
        stitcher.apply_fixed_margins(24, 16, &mid_frame).unwrap();
        stitcher.apply_fixed_margins(0, 0, &mid_frame).unwrap();
        stitcher.apply_fixed_margins(32, 0, &mid_frame).unwrap();
        stitcher.apply_fixed_margins(0, 0, &mid_frame).unwrap();

        for scroll_y in [1120u32, 1360, 1620, 1900] {
            let _ = stitcher.feed(viewport_frame(&doc, scroll_y, view_h, 0, 0, false, false));
        }
        let assembled = stitcher.assemble().unwrap().unwrap();
        assert_eq!(assembled.dimensions(), doc.dimensions());
        assert_eq!(assembled.as_raw(), doc.as_raw());
    }

    #[test]
    fn fixed_sidebars_keep_top_once_and_extend_background_below() {
        let width = 800u32;
        let view_h = 480u32;
        let doc_h = 1600u32;
        let left_w = 200u32;
        let right_w = 160u32;
        let center_doc = make_document(width, doc_h);
        let left_bg = image::Rgba([242, 244, 248, 255]);
        let right_bg = image::Rgba([245, 246, 250, 255]);
        let divider = image::Rgba([210, 214, 222, 255]);

        let make_frame = |scroll_y: u32| -> RgbaImage {
            let mut img = viewport_frame(&center_doc, scroll_y, view_h, 0, 0, false, false);
            for y in 0..view_h {
                // Static left sidebar with menu items & 1 px right divider.
                for x in 0..left_w {
                    let mut p = left_bg;
                    if x == left_w - 1 {
                        p = divider;
                    } else if y % 36 >= 10 && y % 36 < 22 && (16..left_w - 16).contains(&x) {
                        let item = y / 36;
                        if x % 9 < 6 {
                            p = image::Rgba([
                                40 + ((item * 19) % 80) as u8,
                                55,
                                95 + ((item * 23) % 90) as u8,
                                255,
                            ]);
                        }
                    }
                    img.put_pixel(x, y, p);
                }
                // Static right outline sidebar with 1 px left divider.
                let right_start = width - right_w;
                for x in right_start..width {
                    let mut p = right_bg;
                    if x == right_start {
                        p = divider;
                    } else if y % 32 >= 8
                        && y % 32 < 18
                        && (right_start + 12..width - 24).contains(&x)
                    {
                        let item = y / 32;
                        if x % 8 < 5 {
                            p = image::Rgba([70, 80 + ((item * 17) % 80) as u8, 120, 255]);
                        }
                    }
                    img.put_pixel(x, y, p);
                }
            }
            img
        };

        let mut stitcher = Stitcher::new(StitchConfig::default());
        for scroll_y in [0u32, 90, 200, 340, 500, 680, 880, 1020, 1120] {
            let _ = stitcher.feed(make_frame(scroll_y));
        }

        let (fl, fr) = stitcher.fixed_sidebars();
        assert!(fl >= left_w - 12 && fl <= left_w + 12, "fl={fl}");
        assert!(fr >= right_w - 12 && fr <= right_w + 12, "fr={fr}");

        let assembled = stitcher.assemble().unwrap().unwrap();
        assert_eq!(assembled.dimensions(), (width, doc_h));

        let top_frame = make_frame(0);
        let left_layout = stitcher.left_sidebar.as_ref().expect("left layout");
        let right_layout = stitcher.right_sidebar.as_ref().expect("right layout");
        assert!(left_layout.top_h > 0 && left_layout.bottom_h > 0);
        assert!(right_layout.top_h > 0 && right_layout.bottom_h > 0);

        for y in 0..left_layout.top_h {
            for x in 0..left_w {
                assert_eq!(assembled.get_pixel(x, y), top_frame.get_pixel(x, y));
            }
        }
        for y in left_layout.top_h..doc_h - left_layout.bottom_h {
            assert_eq!(*assembled.get_pixel(40, y), left_bg, "y={y}");
            assert_eq!(*assembled.get_pixel(left_w - 1, y), divider, "y={y}");
        }
        for i in 0..left_layout.bottom_h {
            let fy = view_h - left_layout.bottom_h + i;
            let ay = doc_h - left_layout.bottom_h + i;
            for x in 0..left_w {
                assert_eq!(
                    assembled.get_pixel(x, ay),
                    top_frame.get_pixel(x, fy),
                    "left bottom mismatch at ({x}, {ay})"
                );
            }
        }

        for y in 0..right_layout.top_h {
            for x in width - right_w..width {
                assert_eq!(assembled.get_pixel(x, y), top_frame.get_pixel(x, y));
            }
        }
        for y in right_layout.top_h..doc_h - right_layout.bottom_h {
            assert_eq!(*assembled.get_pixel(width - 40, y), right_bg, "y={y}");
            assert_eq!(*assembled.get_pixel(width - right_w, y), divider, "y={y}");
        }
        for i in 0..right_layout.bottom_h {
            let fy = view_h - right_layout.bottom_h + i;
            let ay = doc_h - right_layout.bottom_h + i;
            for x in width - right_w..width {
                assert_eq!(
                    assembled.get_pixel(x, ay),
                    top_frame.get_pixel(x, fy),
                    "right bottom mismatch at ({x}, {ay})"
                );
            }
        }

        for y in 0..doc_h {
            for x in left_w + 16..width - right_w - 24 {
                assert_eq!(
                    assembled.get_pixel(x, y),
                    center_doc.get_pixel(x, y),
                    "center mismatch at ({x}, {y})"
                );
            }
        }
    }

    #[test]
    fn plain_white_margins_are_not_treated_as_fixed_sidebars() {
        let width = 800u32;
        let doc_h = 1400u32;
        let mut doc = RgbaImage::from_pixel(width, doc_h, image::Rgba([255, 255, 255, 255]));
        for y in 0..doc_h {
            let band = y / 40;
            // Centered column 240..560 for most of the document, plus a full-width
            // textured banner at y=900..960 that must stay untouched.
            let x_range = if (900..960).contains(&y) {
                20..width - 20
            } else {
                240..560
            };
            for x in x_range {
                if y % 40 == 0 {
                    doc.put_pixel(x, y, image::Rgba([220, 224, 232, 255]));
                } else if y % 40 >= 10 && y % 40 < 24 && x % 10 < 6 {
                    let row_shade = ((y % 40 - 10) * 11) as u8;
                    doc.put_pixel(
                        x,
                        y,
                        image::Rgba([
                            40 + ((band * 31 + u32::from(row_shade)) % 160) as u8,
                            60 + ((band * 67) % 150) as u8,
                            110,
                            255,
                        ]),
                    );
                }
            }
        }
        let mut stitcher = Stitcher::new(StitchConfig::default());
        for scroll_y in [0u32, 120, 260, 420, 600, 780, 920] {
            let _ = stitcher.feed(viewport_frame(&doc, scroll_y, 480, 0, 0, false, false));
        }
        assert_eq!(stitcher.fixed_sidebars(), (0, 0));
        let assembled = stitcher.assemble().unwrap().unwrap();
        assert_eq!(assembled.dimensions(), doc.dimensions());
        assert_eq!(assembled.as_raw(), doc.as_raw());
    }

    #[test]
    fn git_graph_adjacent_to_left_sidebar_is_not_swallowed_into_fixed_left() {
        // UGit layout: static left file tree (0..180), scrolling multi-lane git
        // graph (180..360) with vertical rails + horizontal connectors + dots,
        // scrolling commit list (360..760), and static right inspector (760..960).
        let width = 960u32;
        let view_h = 480u32;
        let doc_h = 1800u32;
        let left_w = 180u32;
        let right_w = 200u32;
        let bg = image::Rgba([28, 30, 36, 255]);
        let left_bg = image::Rgba([34, 36, 44, 255]);
        let right_bg = image::Rgba([32, 34, 42, 255]);
        let divider = image::Rgba([62, 66, 78, 255]);

        let mut center_doc = RgbaImage::from_pixel(width, doc_h, bg);
        for y in 0..doc_h {
            let row_idx = y / 36;
            // 10 dense vertical branch rails right next to the left sidebar.
            for lane in 0..10u32 {
                let rx = left_w + 16 + lane * 16;
                let color = image::Rgba([
                    80 + ((lane * 47) % 160) as u8,
                    90 + ((lane * 83) % 150) as u8,
                    110 + ((lane * 113) % 130) as u8,
                    255,
                ]);
                for dx in 0..2 {
                    center_doc.put_pixel(rx + dx, y, color);
                }
            }
            // Commit dot + horizontal branch line every 36 px.
            if y % 36 >= 14 && y % 36 < 20 {
                let active_lane = row_idx % 10;
                let dot_x = left_w + 16 + active_lane * 16;
                for x in dot_x..left_w + 176 {
                    center_doc.put_pixel(x, y, image::Rgba([220, 180, 90, 255]));
                }
            }
            // Commit message / author / hash columns.
            if y % 36 >= 10 && y % 36 < 24 {
                for x in 380..730u32 {
                    let cell = x / 8;
                    if (row_idx.wrapping_mul(31) + cell) % 3 != 0 && x % 8 < 5 {
                        center_doc.put_pixel(
                            x,
                            y,
                            image::Rgba([
                                180 + ((row_idx * 23) % 60) as u8,
                                185 + ((cell * 17) % 55) as u8,
                                205,
                                255,
                            ]),
                        );
                    }
                }
            }
        }

        let make_frame = |scroll_y: u32| -> RgbaImage {
            let mut img = viewport_frame(&center_doc, scroll_y, view_h, 0, 0, false, false);
            for y in 0..view_h {
                for x in 0..left_w {
                    let mut p = left_bg;
                    if x == left_w - 1 {
                        p = divider;
                    } else if y % 32 >= 8 && y % 32 < 20 && (16..left_w - 18).contains(&x) && x % 8 < 5 {
                        let item = y / 32;
                        p = image::Rgba([140 + ((item * 17) % 80) as u8, 175, 210, 255]);
                    }
                    img.put_pixel(x, y, p);
                }
                let r0 = width - right_w;
                for x in r0..width {
                    let mut p = right_bg;
                    if x == r0 {
                        p = divider;
                    } else if y % 40 >= 10 && y % 40 < 22 && (r0 + 16..width - 20).contains(&x) && x % 7 < 4 {
                        let item = y / 40;
                        p = image::Rgba([170, 150 + ((item * 29) % 80) as u8, 200, 255]);
                    }
                    img.put_pixel(x, y, p);
                }
            }
            img
        };

        let mut stitcher = Stitcher::new(StitchConfig::default());
        for scroll_y in [0u32, 90, 200, 340, 500, 680, 880, 1100, 1320] {
            let _ = stitcher.feed(make_frame(scroll_y));
        }
        let (fl, fr) = stitcher.fixed_sidebars();
        assert!(fl <= left_w + 8, "git graph swallowed into fixed_left={fl}");
        assert!(fr >= right_w - 12 && fr <= right_w + 12, "fr={fr}");

        let assembled = stitcher.assemble().unwrap().unwrap();
        assert_eq!(assembled.dimensions(), (width, doc_h));
        // Verify git-graph rails and commit dots remain intact below viewport.
        for y in view_h..doc_h {
            for x in left_w + 16..left_w + 176 {
                assert_eq!(
                    assembled.get_pixel(x, y),
                    center_doc.get_pixel(x, y),
                    "git graph pixel lost at ({x}, {y})"
                );
            }
        }
    }

    #[test]
    fn narrow_static_right_sidebar_does_not_flap_to_zero() {
        let width = 760u32;
        let view_h = 480u32;
        let doc_h = 1600u32;
        let sticky_top = 48u32;
        let right_w = 72u32;
        let center_doc = make_document(width, doc_h);
        let right_bg = image::Rgba([28, 30, 36, 255]);
        let divider = image::Rgba([70, 74, 86, 255]);

        let make_frame = |scroll_y: u32| -> RgbaImage {
            let mut img = viewport_frame(&center_doc, scroll_y, view_h, sticky_top, 0, true, false);
            let r0 = width - right_w;
            for y in sticky_top..view_h {
                for x in r0..width {
                    let mut p = right_bg;
                    if x == r0 {
                        p = divider;
                    } else if y % 48 >= 14 && y % 48 < 26 && (r0 + 10..width - 10).contains(&x) && x % 7 < 4 {
                        let item = y / 48;
                        p = image::Rgba([180, 160 + ((item * 19) % 70) as u8, 210, 255]);
                    }
                    img.put_pixel(x, y, p);
                }
            }
            img
        };

        let mut stitcher = Stitcher::new(StitchConfig::default());
        let mut locked_right = None;
        for scroll_y in [0u32, 80, 170, 270, 380, 500, 620, 740, 860, 980, 1100] {
            let _ = stitcher.feed(make_frame(scroll_y));
            let (_, fr) = stitcher.fixed_sidebars();
            if fr > 0 {
                if let Some(prev_fr) = locked_right {
                    assert_eq!(fr, prev_fr, "right sidebar flapped from {prev_fr} to {fr}");
                }
                locked_right = Some(fr);
            } else {
                assert!(locked_right.is_none(), "right sidebar flapped back to 0");
            }
        }
        assert!(locked_right.is_some(), "expected right sidebar to lock");
    }

    #[test]
    fn dense_git_graph_matches_true_shift_without_false_negative_or_rail_ties() {
        // GitKraken center-pane layout: wide dark left branch-label zone,
        // dense 16-lane vertical rails across center, and narrow commit text on right.
        let width = 800u32;
        let view_h = 500u32;
        let doc_h = 2600u32;
        let sticky_top = 48u32;
        let mut doc = RgbaImage::from_pixel(width, doc_h, image::Rgba([24, 26, 32, 255]));
        for y in 0..doc_h {
            let row = y / 34;
            for lane in 0..16u32 {
                let rx = 240 + lane * 20;
                let c = image::Rgba([
                    70 + ((lane * 43) % 170) as u8,
                    80 + ((lane * 79) % 160) as u8,
                    120 + ((lane * 107) % 120) as u8,
                    255,
                ]);
                for dx in 0..2 {
                    doc.put_pixel(rx + dx, y, c);
                }
            }
            if row % 5 == 0 && y % 34 >= 10 && y % 34 < 22 {
                for x in 32..190u32 {
                    let mut h = row
                        .wrapping_mul(2654435761)
                        .wrapping_add((x / 6).wrapping_mul(1597334677));
                    h ^= h >> 15;
                    h = h.wrapping_mul(2246822519);
                    if (h >> 16) % 3 != 0 && x % 6 < 4 {
                        doc.put_pixel(x, y, image::Rgba([90, 140, 210, 255]));
                    }
                }
            }
            if y % 34 >= 12 && y % 34 < 22 {
                let lane = (row.wrapping_mul(7) ^ (row >> 2)) % 16;
                let cx = 240 + lane * 20;
                for x in cx..580 {
                    doc.put_pixel(
                        x,
                        y,
                        image::Rgba([
                            150 + ((row * 41) % 90) as u8,
                            120 + ((row * 67) % 100) as u8,
                            100 + ((row * 89) % 110) as u8,
                            255,
                        ]),
                    );
                }
                for x in 600..780u32 {
                    let mut h = row
                        .wrapping_mul(2246822519)
                        .wrapping_add((x / 6).wrapping_mul(3266489917));
                    h ^= h >> 15;
                    h = h.wrapping_mul(2246822519);
                    if (h >> 16) % 2 == 0 && x % 6 < 4 {
                        doc.put_pixel(
                            x,
                            y,
                            image::Rgba([
                                150 + ((row * 31) % 90) as u8,
                                160 + ((row * 53) % 85) as u8,
                                210,
                                255,
                            ]),
                        );
                    }
                }
            }
        }

        let mut stitcher = Stitcher::new(StitchConfig::default());
        // Include steady scrolls and a large 83% active-height jump (375 / 452 = 83%).
        for scroll_y in [0u32, 80, 160, 250, 625, 850, 1100, 1450, 1800, 2148] {
            let outcome = stitcher.feed(viewport_frame(&doc, scroll_y, view_h, sticky_top, 0, true, false));
            match outcome {
                FeedOutcome::Initial => {}
                FeedOutcome::Moved { dy } | FeedOutcome::Stitched { dy, .. } => {
                    assert!(dy > 0, "unexpected negative dy={dy} at scroll_y={scroll_y}");
                }
                other => panic!("unexpected outcome at scroll_y={scroll_y}: {other:?}"),
            }
        }

        // A truly disjoint jump (no overlap) must still be rejected.
        let disjoint = stitcher.feed(viewport_frame(&doc, 0, view_h, sticky_top, 0, true, false));
        assert_eq!(disjoint, FeedOutcome::Warning(ScrollWarning::TooFast));

        let assembled = stitcher.assemble().unwrap().unwrap();
        assert_eq!(assembled.height(), doc_h + sticky_top);
    }

    #[test]
    fn fixed_sidebars_place_bottom_chrome_at_bottom_and_fill_middle() {
        let width = 840u32;
        let view_h = 480u32;
        let doc_h = 1600u32;
        let left_w = 210u32;
        let right_w = 180u32;
        let rail_w = 40u32;
        let center_doc = make_document(width, doc_h);
        let rail_bg = image::Rgba([26, 28, 34, 255]);
        let left_bg = image::Rgba([38, 41, 49, 255]);
        let right_bg = image::Rgba([34, 37, 45, 255]);
        let divider = image::Rgba([72, 76, 88, 255]);

        let make_frame = |scroll_y: u32| -> RgbaImage {
            let mut img = viewport_frame(&center_doc, scroll_y, view_h, 0, 0, false, false);
            for y in 0..view_h {
                for x in 0..left_w {
                    let mut p = if x < rail_w {
                        rail_bg
                    } else if x == rail_w || x == left_w - 1 {
                        divider
                    } else {
                        left_bg
                    };
                    if y < 140
                        && y % 28 >= 8
                        && y % 28 < 18
                        && (rail_w + 14..left_w - 16).contains(&x)
                        && x % 8 < 5
                    {
                        p = image::Rgba([140, 175, 220, 255]);
                    } else if (420..464).contains(&y)
                        && (10..left_w - 14).contains(&x)
                        && x % 10 < 6
                    {
                        p = image::Rgba([200, 160, 90, 255]);
                    }
                    img.put_pixel(x, y, p);
                }
                let r0 = width - right_w;
                for x in r0..width {
                    let mut p = if x == r0 { divider } else { right_bg };
                    if y < 150
                        && y % 30 >= 8
                        && y % 30 < 18
                        && (r0 + 14..width - 14).contains(&x)
                        && x % 7 < 4
                    {
                        p = image::Rgba([160, 200, 170, 255]);
                    } else if (430..466).contains(&y)
                        && (r0 + 12..width - 12).contains(&x)
                        && x % 9 < 5
                    {
                        p = image::Rgba([220, 120, 140, 255]);
                    }
                    img.put_pixel(x, y, p);
                }
            }
            img
        };

        let mut stitcher = Stitcher::new(StitchConfig::default());
        for scroll_y in [0u32, 90, 200, 340, 500, 680, 880, 1020, 1120] {
            let _ = stitcher.feed(make_frame(scroll_y));
        }

        let assembled = stitcher.assemble().unwrap().unwrap();
        let disk = stitcher
            .assemble_disk()
            .unwrap()
            .unwrap()
            .read_rows(0, doc_h)
            .unwrap();
        assert_eq!(assembled.as_raw(), disk.as_raw());
        assert_eq!(assembled.dimensions(), (width, doc_h));

        let top_frame = make_frame(0);
        let left_layout = stitcher.left_sidebar.as_ref().expect("left layout");
        let right_layout = stitcher.right_sidebar.as_ref().expect("right layout");
        assert_eq!(left_layout.top_h, 130);
        assert_eq!(left_layout.bottom_h, 60);
        assert_eq!(right_layout.top_h, 138);
        assert_eq!(right_layout.bottom_h, 50);

        // Top remains at top; middle preserves dual-pane fill + dividers; bottom chrome moves to bottom.
        for y in 0..left_layout.top_h {
            for x in 0..left_w {
                assert_eq!(assembled.get_pixel(x, y), top_frame.get_pixel(x, y));
            }
        }
        for y in left_layout.top_h..doc_h - left_layout.bottom_h {
            assert_eq!(*assembled.get_pixel(16, y), rail_bg, "y={y}");
            assert_eq!(*assembled.get_pixel(rail_w, y), divider, "y={y}");
            assert_eq!(*assembled.get_pixel(100, y), left_bg, "y={y}");
            assert_eq!(*assembled.get_pixel(left_w - 1, y), divider, "y={y}");
        }
        for i in 0..left_layout.bottom_h {
            let fy = view_h - left_layout.bottom_h + i;
            let ay = doc_h - left_layout.bottom_h + i;
            for x in 0..left_w {
                assert_eq!(assembled.get_pixel(x, ay), top_frame.get_pixel(x, fy));
            }
        }
        for y in right_layout.top_h..doc_h - right_layout.bottom_h {
            assert_eq!(*assembled.get_pixel(width - right_w, y), divider, "y={y}");
            assert_eq!(*assembled.get_pixel(width - 40, y), right_bg, "y={y}");
        }
        for i in 0..right_layout.bottom_h {
            let fy = view_h - right_layout.bottom_h + i;
            let ay = doc_h - right_layout.bottom_h + i;
            for x in width - right_w..width {
                assert_eq!(assembled.get_pixel(x, ay), top_frame.get_pixel(x, fy));
            }
        }
        let (paused, origin) = stitcher.annotation_frame().unwrap().unwrap();
        assert_eq!(
            paused,
            image::imageops::crop_imm(&assembled, 0, origin as u32, width, view_h).to_image()
        );
    }

    #[test]
    fn fixed_sidebar_with_empty_bottom_keeps_top_and_extends_fill() {
        let width = 760u32;
        let view_h = 480u32;
        let doc_h = 1500u32;
        let left_w = 180u32;
        let left_bg = image::Rgba([34, 36, 44, 255]);
        let divider = image::Rgba([70, 74, 86, 255]);
        let center_doc = make_document(width, doc_h);

        let make_frame = |scroll_y: u32| -> RgbaImage {
            let mut img = viewport_frame(&center_doc, scroll_y, view_h, 0, 0, false, false);
            for y in 0..view_h {
                for x in 0..left_w {
                    let mut p = if x == left_w - 1 { divider } else { left_bg };
                    if y < 220
                        && y % 28 >= 8
                        && y % 28 < 18
                        && (14..left_w - 14).contains(&x)
                        && x % 8 < 5
                    {
                        p = image::Rgba([150, 180, 220, 255]);
                    }
                    img.put_pixel(x, y, p);
                }
            }
            img
        };

        let mut stitcher = Stitcher::new(StitchConfig::default());
        for scroll_y in [0u32, 90, 200, 340, 500, 680, 860, 1020] {
            let _ = stitcher.feed(make_frame(scroll_y));
        }
        let assembled = stitcher.assemble().unwrap().unwrap();
        let (left_top_h, left_bottom_h) = stitcher
            .left_sidebar
            .as_ref()
            .map_or((0, 0), |s| (s.top_h, s.bottom_h));
        assert_eq!(left_bottom_h, 0);
        let top_frame = make_frame(0);
        for y in 0..left_top_h {
            for x in 0..left_w {
                assert_eq!(assembled.get_pixel(x, y), top_frame.get_pixel(x, y));
            }
        }
        for y in left_top_h..doc_h {
            assert_eq!(*assembled.get_pixel(40, y), left_bg, "y={y}");
            assert_eq!(*assembled.get_pixel(left_w - 1, y), divider, "y={y}");
        }
    }

    #[test]
    fn right_sidebar_recovers_after_pre_scroll_change_and_cursor_hover() {
        let width = 960u32;
        let view_h = 480u32;
        let doc_h = 2400u32;
        let right_w = 240u32;
        let right_bg = image::Rgba([33, 35, 42, 255]);
        let divider = image::Rgba([72, 76, 88, 255]);
        let center_doc = make_document(width, doc_h);

        let draw_right = |img: &mut RgbaImage, variant: u32, hover_band: Option<u32>| {
            let r0 = width - right_w;
            for y in 0..view_h {
                for x in r0..width {
                    let mut p = if x == r0 { divider } else { right_bg };
                    if y < 180
                        && y % 30 >= 8
                        && y % 30 < 18
                        && (r0 + 16..width - 16).contains(&x)
                        && x % 8 < 5
                    {
                        p = if variant == 0 {
                            image::Rgba([140, 190, 230, 255])
                        } else {
                            image::Rgba([225, 175, 110, 255])
                        };
                    } else if (430..466).contains(&y)
                        && (r0 + 16..width - 16).contains(&x)
                        && x % 8 < 5
                    {
                        p = image::Rgba([180, 210, 160, 255]);
                    }
                    if let Some(hy) = hover_band
                        && (hy..hy + 44).contains(&y)
                        && (r0 + 12..width - 12).contains(&x)
                    {
                        p = image::Rgba([95, 105, 125, 255]);
                    }
                    img.put_pixel(x, y, p);
                }
            }
        };

        let mut stitcher = Stitcher::new(StitchConfig::default());
        // Frame 0: initial state before a user clicks a different commit & hovers.
        let mut frame0 = viewport_frame(&center_doc, 0, view_h, 0, 0, false, false);
        draw_right(&mut frame0, 0, None);
        assert!(matches!(stitcher.feed(frame0), FeedOutcome::Initial));

        // Frame 1 (idle): right sidebar switched to variant 1 and hovered before scrolling starts.
        let mut idle = viewport_frame(&center_doc, 0, view_h, 0, 0, false, false);
        draw_right(&mut idle, 1, Some(100));
        let _ = stitcher.feed(idle);

        // Next 2 scrolling frames have transient cursor hover across the right sidebar, then 12 clean scrolling frames.
        for (idx, scroll_y) in (80..=1880).step_by(120).enumerate() {
            let mut frame = viewport_frame(&center_doc, scroll_y as u32, view_h, 0, 0, false, false);
            let hover = (idx < 2).then_some(140 + (idx as u32) * 60);
            draw_right(&mut frame, 1, hover);
            let _ = stitcher.feed(frame);
        }

        let (_, fr) = stitcher.fixed_sidebars();
        assert!(
            fr >= right_w - 16 && fr <= right_w + 16,
            "expected right sidebar to recover and lock, got fr={fr}"
        );
        let assembled = stitcher.assemble().unwrap().unwrap();
        let right_layout = stitcher.right_sidebar.as_ref().expect("right layout");
        assert!(right_layout.bottom_h > 0);
        // Middle of the stitched right sidebar must stay uniformly filled, never repeating variant 1 items.
        for y in right_layout.top_h..assembled.height() - right_layout.bottom_h {
            assert_eq!(*assembled.get_pixel(width - 80, y), right_bg, "y={y}");
        }
    }

    #[test]
    fn wide_retina_right_sidebar_locks_despite_one_fine_column_scrollbar_jitter() {
        let width = 2400u32;
        let view_h = 600u32;
        let doc_h = 2400u32;
        let right_w = 520u32;
        let right_bg = image::Rgba([33, 33, 33, 255]);
        let divider = image::Rgba([68, 68, 68, 255]);
        let center_doc = make_document(width, doc_h);

        let mut stitcher = Stitcher::new(StitchConfig::default().with_scale(2.0));
        for (idx, scroll_y) in (0..=1800).step_by(150).enumerate() {
            let mut frame = viewport_frame(&center_doc, scroll_y as u32, view_h, 0, 0, false, false);
            let r0 = width - right_w;
            for y in 0..view_h {
                for x in r0..width {
                    let mut p = if x == r0 { divider } else { right_bg };
                    if y < 260
                        && y % 34 >= 8
                        && y % 34 < 20
                        && (r0 + 24..width - 24).contains(&x)
                        && x % 10 < 6
                    {
                        p = image::Rgba([190, 175, 120, 255]);
                    }
                    frame.put_pixel(x, y, p);
                }
                // Scrollbar thumb inside the right-sidebar edge jitters in width by 12 px across frames.
                let sb_w = if idx % 2 == 0 { 4 } else { 16 };
                if (60 + idx as u32 * 20..180 + idx as u32 * 20).contains(&y) {
                    for x in r0 + 2..r0 + 2 + sb_w {
                        frame.put_pixel(x, y, image::Rgba([130, 130, 130, 255]));
                    }
                }
            }
            let _ = stitcher.feed(frame);
        }

        let (_, fr) = stitcher.fixed_sidebars();
        assert!(
            fr >= right_w - 24 && fr <= right_w + 16,
            "expected wide right sidebar to lock despite scrollbar jitter, got fr={fr}"
        );
    }

    #[test]
    fn preview_tile_carries_its_rendered_range_across_unflushed_growth() {
        let doc = make_document(400, 1600);
        let view_h = 440u32;
        let mut stitcher = Stitcher::new(StitchConfig::default());
        stitcher.feed(viewport_frame(&doc, 0, view_h, 0, 0, false, false));
        stitcher.set_preview_range_with_follow(0, 600, true);
        stitcher.refresh_preview().unwrap();

        let first_tile = stitcher.preview_tile().unwrap();
        assert_eq!(first_tile.source_start, 0);
        assert_eq!(first_tile.source_rows, view_h);
        assert_eq!(first_tile.total_height, view_h);

        // Grow without refreshing preview; existing tile must still carry its rendered bounds.
        let _ = stitcher.feed_shared(
            Arc::new(PooledImage::unpooled(viewport_frame(
                &doc, 220, view_h, 0, 0, false, false,
            ))),
            false,
        );
        let stale_tile = stitcher.preview_tile().unwrap();
        assert_eq!(stale_tile.source_start, 0);
        assert_eq!(stale_tile.source_rows, view_h);
        assert_eq!(stale_tile.total_height, view_h);
        assert!(stitcher.total_height() > view_h);
    }

    fn make_vertical_graph_doc(width: u32, height: u32, unique_amplitude: u8) -> RgbaImage {
        let mut doc = RgbaImage::new(width, height);
        for y in 0..height {
            let row_idx = y / 32;
            let ly = y % 32;
            for x in 0..width {
                let mut px = image::Rgba([28, 30, 34, 255]);
                // Left 80% is continuous colored vertical lanes (constant along y).
                if x < width * 4 / 5 && x >= 40 && x % 24 < 4 {
                    let lane = (x / 24) as u8;
                    px = image::Rgba([
                        40 + (lane.wrapping_mul(53) % 180),
                        60 + (lane.wrapping_mul(97) % 170),
                        100 + (lane.wrapping_mul(131) % 140),
                        255,
                    ]);
                } else if x >= width - 140 && x < width - 20 {
                    // Shared periodic row structure so wrong row multiples have sharp local minima.
                    if ly == 8 && x < width - 80 {
                        px = image::Rgba([90, 96, 110, 255]);
                    }
                    // Narrow unique per-row payload inside band 7 only (~64 px wide, 8% of frame).
                    if (10..26).contains(&ly) && x >= width - 92 && x < width - 28 {
                        let cell = (x - (width - 92)) / 4;
                        let hash = row_idx
                            .wrapping_mul(2654435761)
                            .wrapping_add(cell.wrapping_mul(1597334677));
                        let bucket = (hash >> 2) % 3;
                        if bucket != 0 {
                            let delta = ((hash >> 4) as u8) % unique_amplitude.max(1);
                            let base = if bucket == 1 { 70 } else { 130 };
                            px = image::Rgba([
                                base + delta,
                                base + delta / 2,
                                base.saturating_add(delta / 3),
                                255,
                            ]);
                        }
                    }
                }
                doc.put_pixel(x, y, px);
            }
        }
        doc
    }

    #[test]
    fn narrow_window_rejects_vertical_line_periodic_false_match() {
        let w = 800;
        let view_h = 640;
        let doc = make_vertical_graph_doc(w, 1200, 90);
        let prev = FrameProfile::from_image(viewport_frame(&doc, 0, view_h, 0, 0, false, false), 1.0);
        let curr =
            FrameProfile::from_image(viewport_frame(&doc, 224, view_h, 0, 0, false, false), 1.0);
        let fine_x = curr.fine_x_range(0, 0);
        let prev_weights = BandWeights::new(&prev, &fine_x);

        let dy = estimate_shift(&prev, &curr, 0, view_h, &fine_x, Some(128), &prev_weights)
            .expect("full search should recover shift outside narrow window");
        assert_eq!(dy, 224);
    }

    #[test]
    fn near_ties_does_not_prefer_band_mismatched_periodic_shift() {
        let w = 800;
        let view_h = 640;
        // Moderate unique payload inside band 7: wrong periodic shift fails narrow-window
        // band quality (> 6.5) and stays within `standard_ok()` (~12 MAD), while true shift is 0.0.
        let doc = make_vertical_graph_doc(w, 1200, 32);
        let prev = FrameProfile::from_image(viewport_frame(&doc, 0, view_h, 0, 0, false, false), 1.0);
        let curr =
            FrameProfile::from_image(viewport_frame(&doc, 384, view_h, 0, 0, false, false), 1.0);
        let fine_x = curr.fine_x_range(0, 0);
        let prev_weights = BandWeights::new(&prev, &fine_x);

        let dy = estimate_shift(&prev, &curr, 0, view_h, &fine_x, Some(128), &prev_weights)
            .expect("should keep exact shift instead of band-mismatched near tie");
        assert_eq!(dy, 384);
    }

    #[test]
    fn band_weights_subrange_matches_direct_pixel_gradients_and_stitches_with_sidebars() {
        let w = 640u32;
        let view_h = 480u32;
        let doc = make_document(w, 1200);
        let prev = FrameProfile::from_image(viewport_frame(&doc, 0, view_h, 0, 0, false, false), 1.0);
        let curr =
            FrameProfile::from_image(viewport_frame(&doc, 160, view_h, 0, 0, false, false), 1.0);

        let left_margin = 96u32;
        let right_margin = 80u32;
        let subrange = prev.fine_x_range(left_margin, right_margin);
        assert!(subrange.start > 0 && subrange.end < prev.fine_w);

        let weights = BandWeights::new(&prev, &subrange);
        let span = subrange.len();
        let mut expected_vg = vec![0.0_f32; view_h as usize];
        for (y, slot) in expected_vg.iter_mut().enumerate() {
            let row = &prev.fine_luma[y * prev.fine_w + subrange.start..y * prev.fine_w + subrange.end];
            let expected_hg = if span > 1 {
                row.windows(2)
                    .map(|p| f32::from(p[1].abs_diff(p[0])))
                    .sum::<f32>()
                    / (span - 1) as f32
            } else {
                0.0
            };
            assert!(
                (weights.row_grad[y] - expected_hg).abs() < 1e-5,
                "row_grad[{y}] mismatch: {} vs {expected_hg}",
                weights.row_grad[y]
            );
            if y > 0 {
                let prev_row = &prev.fine_luma
                    [(y - 1) * prev.fine_w + subrange.start..(y - 1) * prev.fine_w + subrange.end];
                *slot = prev_row
                    .iter()
                    .zip(row)
                    .map(|(a, b)| f32::from(a.abs_diff(*b)))
                    .sum::<f32>()
                    / span as f32;
            }
        }
        for y in 0..view_h as usize {
            let vg = expected_vg[y].max(if y + 1 < view_h as usize {
                expected_vg[y + 1]
            } else {
                0.0
            });
            let expected_mw = weights.row_grad[y].min(vg);
            assert!(
                (weights.row_match_weight[y] - expected_mw).abs() < 1e-5,
                "row_match_weight[{y}] mismatch: {} vs {expected_mw}",
                weights.row_match_weight[y]
            );
        }
        for cy in 0..prev.coarse_h {
            let y0 = cy * COARSE_STRIDE;
            let y1 = (y0 + COARSE_STRIDE).min(view_h as usize);
            let expected_cw = weights.row_match_weight[y0..y1]
                .iter()
                .copied()
                .fold(0.0_f32, f32::max);
            assert!(
                (weights.coarse_weight[cy] - expected_cw).abs() < 1e-5,
                "coarse_weight[{cy}] mismatch: {} vs {expected_cw}",
                weights.coarse_weight[cy]
            );
        }

        let dy = estimate_shift(&prev, &curr, 0, view_h, &subrange, Some(120), &weights)
            .expect("narrowed fine_x should estimate shift cleanly");
        assert_eq!(dy, 160);
    }

    #[test]
    fn normal_range_match_beats_tiny_overlap_extreme_shift() {
        let w = 640u32;
        let view_h = 500u32;
        let doc = make_document(w, 1200);
        let prev_img = viewport_frame(&doc, 0, view_h, 0, 0, false, false);
        let mut curr_img = viewport_frame(&doc, 100, view_h, 0, 0, false, false);
        // Copy bottom 70 rows of `prev` into top 70 rows of `curr` so `dy = 430` (86%)
        // has a verbatim 0.0-MAD tiny overlap.
        for y in 0..70u32 {
            for x in 0..w {
                curr_img.put_pixel(x, y, *prev_img.get_pixel(x, 430 + y));
            }
        }
        // Apply a localized hover tint in the middle of `curr` (~8% of viewport height)
        // so `dy = 100` has higher global MAD than `430`'s 0.0 yet still passes `standard_ok()`.
        for y in 220..260u32 {
            for x in 16..w - 16 {
                let mut p = *curr_img.get_pixel(x, y);
                p.0[0] = p.0[0].saturating_sub(18);
                p.0[1] = p.0[1].saturating_sub(12);
                p.0[2] = p.0[2].saturating_sub(8);
                curr_img.put_pixel(x, y, p);
            }
        }
        let prev = FrameProfile::from_image(prev_img, 1.0);
        let curr = FrameProfile::from_image(curr_img, 1.0);
        let fine_x = curr.fine_x_range(0, 0);
        let prev_weights = BandWeights::new(&prev, &fine_x);

        let dy = estimate_shift(
            &prev,
            &curr,
            0,
            view_h,
            &fine_x,
            Some(100),
            &prev_weights,
        )
        .expect("normal-range match should win over tiny-overlap extreme jump");
        assert_eq!(dy, 100);
    }

    #[test]
    fn incremental_preview_matches_full_preview_slice_pixel_for_pixel() {
        let width = 760u32;
        let view_h = 460u32;
        let left_w = 120u32;
        let right_w = 140u32;
        let doc = make_document(width, 2800);
        let mut stitcher = Stitcher::new(StitchConfig::default());
        stitcher.set_preview_range_with_follow(0, 1200, true);

        let build_frame = |scroll_y: u32| {
            let mut frame = viewport_frame(&doc, scroll_y, view_h, 44, 32, false, false);
            for y in 44..view_h - 32 {
                let is_chrome = y < 120 || y >= view_h - 80;
                for x in 0..left_w {
                    let px = if x == left_w - 1 {
                        image::Rgba([65, 65, 70, 255])
                    } else if is_chrome && y % 20 < 8 && (16..left_w - 16).contains(&x) {
                        image::Rgba([140, 160, 190, 255])
                    } else {
                        image::Rgba([28, 30, 34, 255])
                    };
                    frame.put_pixel(x, y, px);
                }
                let r0 = width - right_w;
                for x in r0..width {
                    let px = if x == r0 {
                        image::Rgba([65, 65, 70, 255])
                    } else if is_chrome && y % 24 < 10 && (r0 + 16..width - 16).contains(&x) {
                        image::Rgba([180, 150, 110, 255])
                    } else {
                        image::Rgba([32, 34, 38, 255])
                    };
                    frame.put_pixel(x, y, px);
                }
            }
            frame
        };

        for scroll_y in (0..=2200).step_by(110) {
            let _ = stitcher.feed(build_frame(scroll_y as u32));
            let tile = stitcher.preview_tile().expect("preview tile present");
            let full = stitcher
                .render_preview_slice(
                    tile.image.width(),
                    tile.source_start,
                    tile.source_start + tile.source_rows,
                )
                .expect("full preview render succeeds");
            assert_eq!(
                tile.image.dimensions(),
                full.dimensions(),
                "dimensions mismatch at scroll_y={scroll_y}"
            );
            assert_eq!(
                tile.image.as_raw(),
                full.as_raw(),
                "incremental preview diverged at scroll_y={scroll_y} (total_h={})",
                stitcher.total_height()
            );
        }
    }

    #[test]
    fn committed_anchor_releases_rgba_image_to_pool() {
        let pool = snapit_capture::FrameBufferPool::new(4);
        let doc = make_document(400, 1600);
        let view_h = 480u32;
        let mut stitcher = Stitcher::new(StitchConfig::default());

        let img0 = Arc::new(PooledImage::new(
            viewport_frame(&doc, 0, view_h, 0, 0, false, false),
            Some(pool.clone()),
        ));
        let weak0 = Arc::downgrade(&img0);
        let ptr0 = img0.as_raw().as_ptr();
        let len0 = img0.as_raw().len();
        assert_eq!(
            stitcher.feed_shared(img0, false),
            FeedOutcome::Initial
        );
        // After initialization, stitcher.prev holds the only strong ref.
        assert_eq!(weak0.strong_count(), 1);

        // Scroll far enough to trigger commit_frame (commit_threshold = 480 / 3 = 160),
        // then one more frame so `prev` is replaced by img2.
        let img1 = Arc::new(PooledImage::new(
            viewport_frame(&doc, 200, view_h, 0, 0, false, false),
            Some(pool.clone()),
        ));
        let weak1 = Arc::downgrade(&img1);
        let ptr1 = img1.as_raw().as_ptr();
        assert!(matches!(
            stitcher.feed_shared(img1, false),
            FeedOutcome::Stitched { .. }
        ));
        // `img1` is now `committed` AND `prev`; `committed` does not hold `PooledImage`,
        // only `prev` does, and `img0` has already been released.
        assert_eq!(weak0.strong_count(), 0);
        assert_eq!(weak1.strong_count(), 1);

        let img2 = Arc::new(PooledImage::new(
            viewport_frame(&doc, 260, view_h, 0, 0, false, false),
            Some(pool.clone()),
        ));
        let weak2 = Arc::downgrade(&img2);
        assert!(matches!(
            stitcher.feed_shared(img2, false),
            FeedOutcome::Moved { .. }
        ));

        // After `img2` becomes `prev`, committed `img1` also has 0 image references left
        // and both recycled buffers sit in `pool`.
        assert_eq!(weak1.strong_count(), 0);
        assert_eq!(weak2.strong_count(), 1);
        let reused_a = pool.take(len0);
        let reused_b = pool.take(len0);
        assert_eq!(reused_a.as_ptr(), ptr1);
        assert_eq!(reused_b.as_ptr(), ptr0);
    }

    #[test]
    fn anchor_caches_and_invalidates_band_weights_by_fine_x() {
        let doc = make_document(640, 800);
        let profile = Arc::new(FrameProfile::from_image(
            viewport_frame(&doc, 0, 480, 0, 0, false, false),
            1.0,
        ));
        let mut anchor = CommittedAnchor::new(profile.clone(), 0);
        let full = 0..profile.fine_w;
        let sub = 8..profile.fine_w.saturating_sub(8);

        let w1 = anchor.weights_for(&full);
        let w2 = anchor.weights_for(&full);
        assert!(Arc::ptr_eq(&w1, &w2));

        let w3 = anchor.weights_for(&sub);
        assert!(!Arc::ptr_eq(&w1, &w3));
        let w4 = anchor.weights_for(&sub);
        assert!(Arc::ptr_eq(&w3, &w4));
    }

    #[test]
    fn left_sidebar_with_sparse_long_item_snaps_to_divider_without_swallowing_git_rail() {
        let width = 1920u32;
        let view_h = 600u32;
        let doc_h = 2600u32;
        let left_w = 250u32; // Divider at x=248..249; first rail at x=262..263
        let rail_x = left_w + 12;
        let left_bg = image::Rgba([38, 39, 39, 255]);
        let main_bg = image::Rgba([33, 33, 33, 255]);
        let divider = image::Rgba([53, 53, 53, 255]);
        let rail_color = image::Rgba([64, 85, 154, 255]);

        let mut center_doc = RgbaImage::from_pixel(width, doc_h, main_bg);
        for y in 0..doc_h {
            // Vertical blue git rail in the scrolling canvas.
            center_doc.put_pixel(rail_x, y, rail_color);
            center_doc.put_pixel(rail_x + 1, y, rail_color);

            // Commit dots at top (y < 700) and bottom (y > 1900), leaving a long
            // dot-free vertical-line stretch in y = 700..1900.
            let row_idx = y / 36;
            if !(700..=1900).contains(&y) && (y % 36 >= 14 && y % 36 < 22) {
                for x in rail_x.saturating_sub(4)..=rail_x + 160 {
                    center_doc.put_pixel(x, y, image::Rgba([115, 160, 220, 255]));
                }
            }
            // Rich commit list content to the right of the git rail.
            if y % 36 >= 10 && y % 36 < 24 {
                for x in (rail_x + 180)..(width - 40) {
                    let cell = x / 8;
                    if (row_idx.wrapping_mul(29) + cell) % 3 != 0 && x % 8 < 5 {
                        center_doc.put_pixel(
                            x,
                            y,
                            image::Rgba([
                                175 + ((row_idx * 19) % 60) as u8,
                                185 + ((cell * 13) % 55) as u8,
                                205,
                                255,
                            ]),
                        );
                    }
                }
            }
        }

        let make_frame = |scroll_y: u32| -> RgbaImage {
            let mut img = viewport_frame(&center_doc, scroll_y, view_h, 48, 0, true, false);
            for y in 48..view_h {
                for x in 0..left_w {
                    let mut p = if x >= left_w - 2 { divider } else { left_bg };
                    // Short tree items ending at x=180 (70 px short of the divider).
                    if (64..380).contains(&y)
                        && y % 32 >= 10
                        && y % 32 < 22
                        && (24..180).contains(&x)
                        && x % 7 < 4
                    {
                        let item = y / 32;
                        p = image::Rgba([150 + ((item * 17) % 60) as u8, 175, 205, 255]);
                    }
                    // One sparse long filename extending up to x=225 (`earth-web @ 9e304d1`),
                    // leaving its trailing end outside `x = 180 + pad_cols` unless snapped!
                    if (170..182).contains(&y) && (180..225).contains(&x) && x % 6 < 4 {
                        p = image::Rgba([195, 195, 200, 255]);
                    }
                    // Bottom fixed gear icon.
                    if (view_h - 40..view_h - 12).contains(&y) && (20..48).contains(&x) {
                        p = image::Rgba([140, 145, 155, 255]);
                    }
                    img.put_pixel(x, y, p);
                }
            }
            img
        };

        let mut stitcher = Stitcher::new(StitchConfig::default().with_scale(2.0));
        for scroll_y in (0..=2000).step_by(80) {
            let _ = stitcher.feed(make_frame(scroll_y as u32));
        }

        let (fl, fr) = stitcher.fixed_sidebars();
        assert!(
            (left_w - 2..=left_w + 1).contains(&fl),
            "expected left sidebar to snap to divider at {left_w}, got fixed_left={fl}"
        );
        assert_eq!(fr, 0);

        let assembled = stitcher.assemble().unwrap().unwrap();
        assert_eq!(assembled.dimensions(), (width, doc_h));
        // The sparse long filename's trailing tip (x = 215..247) must NOT repeat down the canvas.
        for y in 700..doc_h - 150 {
            for x in 215..left_w - 2 {
                assert_eq!(
                    *assembled.get_pixel(x, y),
                    left_bg,
                    "repeated sidebar glyph leaked at ({x}, {y})"
                );
            }
        }
        // The blue vertical git rail and commit lines at x=262..263 must remain untouched
        // (shifted down by the 48 px sticky header in the assembled image).
        for y in view_h..doc_h {
            assert_eq!(
                *assembled.get_pixel(rail_x, y),
                *center_doc.get_pixel(rail_x, y - 48),
                "git rail corrupted at ({rail_x}, {y})"
            );
        }
    }
}
