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
const FINE_MAX_WIDTH: usize = 512;
const COARSE_STRIDE: usize = 4;
const MAX_FINE_ROWS: usize = 64;
const MAX_COARSE_CANDIDATES: usize = 10;
const MAX_MATCH_MAD: f32 = 7.5;
const MIN_INLIER_RATIO: f32 = 0.88;
const INLIER_THRESHOLD: u8 = 12;
const INITIAL_MARGIN_VOTES: u32 = 2;
const MARGIN_CHANGE_VOTES: u32 = 6;
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

struct FrameProfile {
    image: Arc<RgbaImage>,
    width: u32,
    height: u32,
    grid: [u32; 64],
    fine_w: usize,
    fine_luma: Vec<u8>,
    row_grad: Vec<f32>,
    row_edge_columns: Vec<u32>,
    coarse_w: usize,
    coarse_h: usize,
    coarse_luma: Vec<u8>,
    coarse_grad: Vec<f32>,
}

impl FrameProfile {
    #[cfg(test)]
    fn from_image(image: RgbaImage, scale: f32) -> Self {
        let (w, h) = image.dimensions();
        let grid = sample_grid(image.as_raw(), w as usize, h as usize);
        Self::from_image_with_grid(Arc::new(image), scale, grid)
    }

    fn from_image_with_grid(image: Arc<RgbaImage>, scale: f32, grid: [u32; 64]) -> Self {
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
        let mut row_edge_columns = vec![0u32; h];

        for y in 0..h {
            let row_off = y * w * 4;
            let dst_row = &mut fine_luma[y * fine_w..(y + 1) * fine_w];
            let mut prev_l = 0i32;
            let mut grad_sum = 0u32;
            let mut edge_columns = 0u32;
            for (ix, x) in interior.clone().step_by(fine_step).enumerate() {
                let p = row_off + x * 4;
                let l = luma(raw[p], raw[p + 1], raw[p + 2]);
                dst_row[ix] = l;
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
            if fine_w > 1 {
                row_grad[y] = grad_sum as f32 / (fine_w - 1) as f32;
            }
        }

        let coarse_w = fine_w.div_ceil(COARSE_STRIDE).max(1);
        let coarse_h = h.div_ceil(COARSE_STRIDE).max(1);
        let mut coarse_luma = vec![0u8; coarse_w * coarse_h];
        let mut coarse_grad = vec![0.0_f32; coarse_h];
        for cy in 0..coarse_h {
            let y0 = cy * COARSE_STRIDE;
            let y1 = (y0 + COARSE_STRIDE).min(h);
            let mut grad_acc = 0.0_f32;
            for &g in &row_grad[y0..y1] {
                grad_acc += g;
            }
            coarse_grad[cy] = grad_acc / (y1 - y0) as f32;
            for cx in 0..coarse_w {
                let x0 = cx * COARSE_STRIDE;
                let x1 = (x0 + COARSE_STRIDE).min(fine_w);
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

        Self {
            image,
            width,
            height,
            grid,
            fine_w,
            fine_luma,
            row_grad,
            row_edge_columns,
            coarse_w,
            coarse_h,
            coarse_luma,
            coarse_grad,
        }
    }

    #[allow(dead_code)]
    fn matches_grid(&self, other: &Self) -> bool {
        self.grid == other.grid
    }

    fn has_broad_texture(&self, y: usize) -> bool {
        self.row_grad[y] >= 1.0 && self.row_edge_columns[y] * 12 >= self.fine_w as u32
    }

    fn row_bytes(&self) -> usize {
        self.width as usize * 4
    }

    fn raw_rows(&self, y0: u32, y1: u32) -> &[u8] {
        let stride = self.row_bytes();
        let start = y0 as usize * stride;
        let end = (y1 as usize * stride).min(self.image.as_raw().len());
        &self.image.as_raw()[start..end]
    }
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

#[derive(Clone)]
struct Anchor {
    profile: Arc<FrameProfile>,
    /// Virtual y coordinate (in canvas active-band space) where `profile`'s
    /// `active_top` currently lands.
    y: i32,
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
    header_rows: Vec<u8>,
    footer_rows: Vec<u8>,
    rows: PagedRows,
    below_rows: u32,
    above_rows: u32,
    committed: Option<Anchor>,
    prev: Option<Anchor>,
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
        let (start, rows) = self.preview_range;
        let prev_w = self.width.clamp(1, PREVIEW_MAX_WIDTH);
        let image = Arc::new(self.render_preview_slice(prev_w, start, start + rows)?);
        self.preview_revision = NEXT_PREVIEW_REVISION.fetch_add(1, Ordering::Relaxed);
        self.preview = Some(PreviewTile {
            id: 0,
            revision: self.preview_revision,
            image,
        });
        self.preview_dirty = false;
        Ok(())
    }

    /// Feed one captured SDR RGBA frame. Returns what changed.
    pub fn feed(&mut self, image: RgbaImage) -> FeedOutcome {
        self.feed_shared(Arc::new(image), true)
    }

    pub(crate) fn feed_shared(
        &mut self,
        image: Arc<RgbaImage>,
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
        image: Arc<RgbaImage>,
        grid: [u32; 64],
        refresh_preview: bool,
    ) -> FeedOutcome {
        let (w, h) = image.dimensions();
        if w == 0 || h == 0 {
            return FeedOutcome::Unchanged;
        }
        if self.committed.is_none() {
            return match self.initialize(Arc::new(FrameProfile::from_image_with_grid(
                image,
                self.config.scale,
                grid,
            ))) {
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
            prev.profile.grid == grid && prev.profile.image.as_raw() == image.as_raw()
        }) {
            return FeedOutcome::Unchanged;
        }
        let curr = Arc::new(FrameProfile::from_image_with_grid(
            image,
            self.config.scale,
            grid,
        ));

        self.frames += 1;
        let (eff_top, eff_bottom) =
            if let Some(prev_profile) = self.prev.as_ref().map(|p| p.profile.clone()) {
                match self.update_fixed_margins(&prev_profile, &curr) {
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
            let prev_match = self.prev.as_ref().map(|prev| {
                (
                    prev.y,
                    estimate_shift(
                        &prev.profile,
                        &curr,
                        search_top,
                        search_bottom,
                        self.last_dy,
                    ),
                )
            });
            match prev_match {
                Some((prev_y, Ok(dy))) => {
                    if let Some(prev_profile) = self.prev.as_ref().map(|p| p.profile.clone())
                        && let Err(error) = self.commit_frame(prev_profile, prev_y)
                    {
                        return FeedOutcome::Failed(format!("{error:#}"));
                    }
                    (prev_y, dy)
                }
                Some((_, Err(prev_warn))) => {
                    let committed_match = self.committed.as_ref().map(|anchor| {
                        (
                            anchor.y,
                            estimate_shift(
                                &anchor.profile,
                                &curr,
                                search_top,
                                search_bottom,
                                committed_expected_dy,
                            ),
                        )
                    });
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
            let committed_match = self.committed.as_ref().map(|anchor| {
                let match_res = estimate_shift(
                    &anchor.profile,
                    &curr,
                    search_top,
                    search_bottom,
                    committed_expected_dy,
                );
                (anchor.y, match_res)
            });

            match committed_match {
                Some((anchor_y, Ok(dy))) => (anchor_y, dy),
                Some((_, Err(committed_warn))) => {
                    let prev_match = self.prev.as_ref().map(|prev| {
                        (
                            prev.y,
                            estimate_shift(
                                &prev.profile,
                                &curr,
                                search_top,
                                search_bottom,
                                self.last_dy,
                            ),
                        )
                    });
                    match prev_match {
                        Some((prev_y, Ok(dy))) => {
                            if let Some(prev_profile) =
                                self.prev.as_ref().map(|p| p.profile.clone())
                                && let Err(error) = self.commit_frame(prev_profile, prev_y)
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
            self.prev = Some(Anchor {
                profile: curr,
                y: base_y,
            });
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
            let previous: Anchor = self.prev.as_ref().expect("checked pending anchor").clone();
            if let Err(error) = self.commit_frame(previous.profile, previous.y) {
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
            if let Err(error) = self.commit_frame(curr.clone(), next_y) {
                return FeedOutcome::Failed(format!("{error:#}"));
            }
        } else {
            // Even before committing a brand new anchor, update footer if we
            // are at the bottommost position so preview/end stays fresh.
            if next_y + active_h as i32 >= self.below_rows as i32 {
                self.update_footer(&curr);
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
                self.update_header(&curr);
                if self.fixed_top > 0
                    && let Err(error) = self.refresh_preview_range(0, self.fixed_top)
                {
                    return FeedOutcome::Failed(format!("{error:#}"));
                }
            }
        }

        self.prev = Some(Anchor {
            profile: curr,
            y: next_y,
        });
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
        let y = prev.y;
        self.commit_frame(profile, y)?;
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
        let mut row = vec![0u8; stride];
        for y in -(self.above_rows as i32)..0 {
            row.copy_from_slice(&self.rows.read_row(y)?);
            out.extend_from_slice(&row);
        }
        let mut active = vec![0u8; self.below_rows as usize * stride];
        self.rows.copy_to(0, self.below_rows, &mut active)?;
        out.extend_from_slice(&active);
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

    fn initialize(&mut self, first: Arc<FrameProfile>) -> Result<()> {
        self.width = first.width;
        self.frame_height = first.height;
        self.frames = 1;
        self.rows = PagedRows::new(first.width);
        self.rows.append_down(0, first.image.as_raw())?;
        self.below_rows = first.height;
        self.above_rows = 0;
        self.header_rows.clear();
        self.footer_rows.clear();
        self.fixed_top = 0;
        self.fixed_bottom = 0;
        self.cand_top_votes = None;
        self.cand_bottom_votes = None;
        self.viewport_y = 0;
        self.last_dy = None;
        self.upward_established = false;
        self.committed = Some(Anchor {
            profile: first.clone(),
            y: 0,
        });
        self.prev = Some(Anchor {
            profile: first,
            y: 0,
        });
        self.preview_dirty = true;
        self.refresh_preview()
    }

    /// Update consensus on fixed top/bottom sticky bars. Only textured static
    /// bands qualify so plain whitespace is never misclassified.
    /// Returns temporary `(search_top, search_bottom)` margins to shield shift
    /// estimation even on the very first moving frame.
    fn update_fixed_margins(
        &mut self,
        prev: &FrameProfile,
        curr: &FrameProfile,
    ) -> Result<(u32, u32)> {
        let h = self.frame_height as usize;
        if h < 96 || prev.fine_w == 0 {
            return Ok((self.fixed_top, self.fixed_bottom));
        }
        let max_band = (h * 35 / 100).min(h / 2 - 24);
        if max_band < 8 {
            return Ok((self.fixed_top, self.fixed_bottom));
        }
        let mut row_diff = vec![0.0_f32; h];
        for (y, diff) in row_diff.iter_mut().enumerate().take(h) {
            let p_row = &prev.fine_luma[y * prev.fine_w..(y + 1) * prev.fine_w];
            let c_row = &curr.fine_luma[y * curr.fine_w..(y + 1) * curr.fine_w];
            let sum: u32 = p_row
                .iter()
                .zip(c_row)
                .map(|(a, b)| u32::from(a.abs_diff(*b)))
                .sum();
            *diff = sum as f32 / prev.fine_w as f32;
        }
        let moving_rows = row_diff.iter().filter(|&&d| d > 5.0).count();
        if moving_rows < h / 6 {
            return Ok((self.fixed_top, self.fixed_bottom));
        }

        // Top sticky scan: stop at 4 consecutive moving rows. Include textured
        // static rows plus up to 8 uniform padding/border rows immediately
        // before motion starts.
        let mut cand_top = 0usize;
        let mut seen_texture = false;
        let mut last_textured = 0usize;
        let mut consecutive_moving = 0usize;
        for (y, &diff) in row_diff.iter().enumerate().take(max_band) {
            if diff > 5.0 {
                consecutive_moving += 1;
                if consecutive_moving >= 4 {
                    break;
                }
            } else {
                consecutive_moving = 0;
                if diff <= 2.5 {
                    if curr.has_broad_texture(y) {
                        seen_texture = true;
                        last_textured = y + 1;
                        cand_top = y + 1;
                    } else if seen_texture && y + 1 - last_textured <= 8 {
                        cand_top = y + 1;
                    }
                }
            }
        }

        // Bottom sticky scan: symmetric upward scan.
        let mut cand_bottom = 0usize;
        seen_texture = false;
        last_textured = 0;
        consecutive_moving = 0;
        for offset in 0..max_band {
            let y = h - 1 - offset;
            if row_diff[y] > 5.0 {
                consecutive_moving += 1;
                if consecutive_moving >= 4 {
                    break;
                }
            } else {
                consecutive_moving = 0;
                if row_diff[y] <= 2.5 {
                    if curr.has_broad_texture(y) {
                        seen_texture = true;
                        last_textured = offset + 1;
                        cand_bottom = offset + 1;
                    } else if seen_texture && offset + 1 - last_textured <= 8 {
                        cand_bottom = offset + 1;
                    }
                }
            }
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
            self.apply_fixed_margins(next_top, next_bottom, curr)?;
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
        if let Some((v, n)) = votes {
            let compatible = if v == 0 || candidate == 0 {
                v == candidate
            } else {
                v.abs_diff(candidate) <= 4
            };
            if compatible {
                return Some((v, (n + 1).min(MARGIN_CHANGE_VOTES)));
            }
        }
        Some((candidate, 1))
    }

    /// Re-slice already-captured canvas rows when sticky margins change.
    fn apply_fixed_margins(
        &mut self,
        new_top: u32,
        new_bottom: u32,
        curr: &FrameProfile,
    ) -> Result<()> {
        let old_top = self.fixed_top;
        let old_bottom = self.fixed_bottom;
        if new_top > old_top {
            let take = (new_top - old_top).min(self.above_rows + self.below_rows);
            if take > 0 {
                let from_above = take.min(self.above_rows);
                self.rows.trim_top_with_origin_shift(take, from_above)?;
                self.above_rows -= from_above;
                self.below_rows -= take - from_above;
                self.fixed_top += take;
                self.update_header(curr);
            }
        } else if new_top < old_top {
            let restored = old_top - new_top;
            let stride = curr.row_bytes();
            let exposed =
                self.header_rows[new_top as usize * stride..old_top as usize * stride].to_vec();
            self.rows.prepend_up(-(self.above_rows as i32), &exposed)?;
            self.above_rows += restored;
            self.fixed_top = new_top;
            self.rebase_anchors(-(restored as i32));
            self.update_header(curr);
        }
        if new_bottom > old_bottom {
            let take = (new_bottom - old_bottom).min(self.below_rows);
            if take > 0 {
                self.rows.trim_bottom(take)?;
                self.below_rows -= take;
                self.fixed_bottom += take;
                self.update_footer(curr);
            }
        } else if new_bottom < old_bottom {
            let restored = old_bottom - new_bottom;
            let stride = curr.row_bytes();
            let exposed = self.footer_rows[..restored as usize * stride].to_vec();
            self.rows.append_down(self.below_rows as i32, &exposed)?;
            self.below_rows += restored;
            self.fixed_bottom = new_bottom;
            self.update_footer(curr);
        }
        log::info!(
            "scroll fixed margins updated: top={} bottom={} (active_h={})",
            self.fixed_top,
            self.fixed_bottom,
            self.frame_height - self.fixed_top - self.fixed_bottom
        );
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

    fn update_header(&mut self, curr: &FrameProfile) {
        if self.fixed_top == 0 {
            self.header_rows.clear();
            return;
        }
        let rows = curr.raw_rows(0, self.fixed_top);
        self.header_rows.clear();
        self.header_rows.extend_from_slice(rows);
    }

    fn update_footer(&mut self, curr: &FrameProfile) {
        if self.fixed_bottom == 0 {
            self.footer_rows.clear();
            return;
        }
        let start = self.frame_height - self.fixed_bottom;
        let rows = curr.raw_rows(start, self.frame_height);
        self.footer_rows.clear();
        self.footer_rows.extend_from_slice(rows);
    }

    /// Splice `frame` whose active top lands at virtual `y` into `below`/`above`.
    fn commit_frame(&mut self, frame: Arc<FrameProfile>, y: i32) -> Result<()> {
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
            self.committed = Some(Anchor { profile: frame, y });
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
                self.best_seam_down(&frame, y, active_top, overlap_start, overlap_end)?
            } else {
                bottom_extent
            };

            // Overwrite existing `below` rows from `seam_y.max(0)`..`bottom_extent`,
            // then append new rows `bottom_extent`..`y_end`.
            let overwrite_start = seam_y.max(0);
            if overwrite_start < bottom_extent {
                let src_start = (overwrite_start - y) as u32 + active_top;
                let src_end = (bottom_extent - y) as u32 + active_top;
                self.rows.write_rows(
                    overwrite_start,
                    (bottom_extent - overwrite_start) as u32,
                    frame.raw_rows(src_start, src_end),
                )?;
            }
            let src_start = (bottom_extent - y) as u32 + active_top;
            let src_end = (y_end - y) as u32 + active_top;
            self.rows
                .append_down(bottom_extent, frame.raw_rows(src_start, src_end))?;
            self.below_rows += (y_end - bottom_extent) as u32;
            self.update_footer(&frame);
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
                    self.best_seam_up(&frame, y, active_top, overlap_start, overlap_end)?
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
                        frame.raw_rows(src_start, src_end),
                    )?;
                }
                // Prepend the newly exposed rows in top-to-bottom order.
                let mut pixels = Vec::with_capacity(allow_new as usize * stride);
                for cy in allow_y..top_extent {
                    let src_row = (cy - y) as u32 + active_top;
                    pixels.extend_from_slice(frame.raw_rows(src_row, src_row + 1));
                }
                self.rows.prepend_up(top_extent, &pixels)?;
                self.above_rows += allow_new;
                if !self.preview_follow {
                    self.preview_range.0 = self.preview_range.0.saturating_add(allow_new);
                }
                self.update_header(&frame);
                if self.preview_follow {
                    self.sync_live_preview_range();
                }
                self.rebuild_all_preview_tiles()?;
                did_splice = true;
            }
        }

        if did_splice {
            self.committed = Some(Anchor { profile: frame, y });
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
        frame: &FrameProfile,
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
        let interior = interior_columns(self.width as usize, self.config.scale);
        let mut existing = vec![0u8; self.width as usize * 4];
        if hi - lo > 64 {
            let step_y = ((hi - lo) / 64).max(1);
            for cy in (lo..hi).step_by(step_y as usize) {
                let src_row = (cy - frame_y) as u32 + active_top;
                let src = frame.raw_rows(src_row, src_row + 1);
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
                let src = frame.raw_rows(src_row, src_row + 1);
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
                let src = frame.raw_rows(src_row, src_row + 1);
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
        frame: &FrameProfile,
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
            return self.rows.copy_to(row as i32, 1, dst);
        }
        let y = y - above_len;
        let below_len = self.below_rows as usize;
        if y < below_len {
            return self.rows.copy_to(y as i32, 1, dst);
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

    fn render_preview_slice(
        &mut self,
        prev_w: u32,
        source_y0: u32,
        source_y1: u32,
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
        let mut rgba = vec![0u8; prev_w as usize * tile_h as usize * 4];
        let mut src_row = vec![0u8; full_w * 4];
        let mut cached_source_y = None;
        for dy in 0..tile_h {
            let source_y = source_y0
                + ((dy as u64 * full_w.max(1) as u64 / prev_w as u64) as u32).min(source_h - 1);
            if cached_source_y != Some(source_y) {
                self.canvas_row_into(source_y, &mut src_row)?;
                cached_source_y = Some(source_y);
            }
            let dst_off = dy as usize * prev_w as usize * 4;
            for px in 0..prev_w {
                let src_x =
                    ((px as u64 * full_w as u64) / prev_w as u64).min((full_w - 1) as u64) as usize;
                let s = src_x * 4;
                let d = dst_off + px as usize * 4;
                rgba[d..d + 4].copy_from_slice(&src_row[s..s + 4]);
            }
        }
        RgbaImage::from_raw(prev_w, tile_h, rgba).context("preview tile buffer size mismatch")
    }

    fn rebuild_all_preview_tiles(&mut self) -> Result<()> {
        self.preview_dirty = true;
        self.refresh_preview()
    }

    fn refresh_preview_range(&mut self, _start: u32, _end: u32) -> Result<()> {
        self.preview_dirty = true;
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
}

impl FineScore {
    fn qualifies_trimmed(&self) -> bool {
        self.textured_rows >= 6
            && self.trimmed_grad_share >= 0.60
            && self.trimmed_mad <= 4.5
            && self.trimmed_inlier >= 0.93
    }

    fn effective_mad(&self) -> f32 {
        if self.qualifies_trimmed() {
            self.mad.min(self.trimmed_mad)
        } else {
            self.mad
        }
    }

    fn standard_ok(&self) -> bool {
        self.mad <= MAX_MATCH_MAD
            && self.inlier >= MIN_INLIER_RATIO
            && self.textured_mad <= 15.0
            && self.textured_inlier >= 0.78
    }
}

fn coarse_band_scores(
    prev: &FrameProfile,
    curr: &FrameProfile,
    c_top: i32,
    c_bot: i32,
    c_lo: i32,
    c_hi: i32,
) -> Vec<(i32, f32)> {
    let mut scores = Vec::with_capacity((c_hi - c_lo + 1).max(0) as usize);
    for c_dy in c_lo..=c_hi {
        let ov_start = c_top.max(c_top + c_dy);
        let ov_end = c_bot.min(c_bot + c_dy);
        let ov_len = ov_end - ov_start;
        if ov_len < 4 {
            continue;
        }
        let mut weighted_err = 0.0_f32;
        let mut weight_sum = 0.0_f32;
        let row_step = (ov_len / 64).max(1) as usize;
        for y in (ov_start..ov_end).step_by(row_step) {
            let py = y as usize;
            let cy = (y - c_dy) as usize;
            let w = prev.coarse_grad[py].max(0.5);
            let p_row = &prev.coarse_luma[py * prev.coarse_w..(py + 1) * prev.coarse_w];
            let c_row = &curr.coarse_luma[cy * curr.coarse_w..(cy + 1) * curr.coarse_w];
            let err: u32 = p_row
                .iter()
                .zip(c_row)
                .map(|(a, b)| u32::from(a.abs_diff(*b)))
                .sum();
            weighted_err += (err as f32 / prev.coarse_w as f32) * w;
            weight_sum += w;
        }
        if weight_sum > 0.0 {
            scores.push((c_dy, weighted_err / weight_sum));
        }
    }
    scores
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

fn refine_coarse_candidates(
    prev: &FrameProfile,
    curr: &FrameProfile,
    active_top: i32,
    active_bottom: i32,
    max_shift: i32,
    candidates: &[(i32, f32)],
) -> Vec<(i32, FineScore)> {
    let mut fine_results = Vec::with_capacity(candidates.len());
    for &(c_dy, _) in candidates {
        let center = c_dy * COARSE_STRIDE as i32;
        let lo = (center - COARSE_STRIDE as i32).max(-max_shift);
        let hi = (center + COARSE_STRIDE as i32).min(max_shift);
        let mut best_local: Option<(i32, FineScore)> = None;
        for dy in lo..=hi {
            if let Some(score) = score_fine_shift(prev, curr, active_top, active_bottom, dy)
                && best_local.is_none_or(|(_, b)| score.effective_mad() < b.effective_mad())
            {
                best_local = Some((dy, score));
            }
        }
        if let Some(best) = best_local {
            fine_results.push(best);
        }
    }
    fine_results
}

/// Estimate vertical displacement `dy` of `curr` relative to `prev` within
/// `[active_top .. active_bottom]`.
///
/// Convention: `dy > 0` means the user scrolled **down** (content moved up on
/// screen, so `curr` sits `dy` pixels below `prev` on the stitched canvas).
fn estimate_shift(
    prev: &FrameProfile,
    curr: &FrameProfile,
    active_top: u32,
    active_bottom: u32,
    last_dy: Option<i32>,
) -> Result<i32, ScrollWarning> {
    let active_h = active_bottom.saturating_sub(active_top) as i32;
    if active_h < 32 || prev.fine_w == 0 || curr.fine_w == 0 {
        return Err(ScrollWarning::LowTexture);
    }

    // Check whether the active band actually has enough texture to match.
    let textured_rows = curr.row_grad[active_top as usize..active_bottom as usize]
        .iter()
        .filter(|&&g| g >= 1.5)
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

    // Momentum-guided narrow-window fast path for steady scrolling.
    if let Some(pred) = last_dy
        && pred != 0
        && pred.abs() <= active_h * 80 / 100
    {
        let c_pred = (pred as f32 / COARSE_STRIDE as f32).round() as i32;
        let win_lo = (c_pred - 16).max(-c_max);
        let win_hi = (c_pred + 16).min(c_max);
        let narrow_coarse = coarse_band_scores(prev, curr, c_top, c_bot, win_lo, win_hi);
        let narrow_cands = coarse_minima(&narrow_coarse, 3);
        let mut narrow_fine = refine_coarse_candidates(
            prev,
            curr,
            active_top as i32,
            active_bottom as i32,
            max_shift,
            &narrow_cands,
        );
        narrow_fine.sort_by(|a, b| a.1.effective_mad().total_cmp(&b.1.effective_mad()));
        if let Some(&(best_dy, best_score)) = narrow_fine.first() {
            let best_eff = best_score.effective_mad();
            let unique_in_window = !narrow_fine
                .iter()
                .any(|&(dy, s)| (dy - best_dy).abs() >= 12 && s.effective_mad() <= best_eff + 2.0);
            let sharp_peak = [-2, 2].into_iter().any(|delta| {
                let probe = best_dy + delta;
                (-max_shift..=max_shift).contains(&probe)
                    && score_fine_shift(prev, curr, active_top as i32, active_bottom as i32, probe)
                        .is_some_and(|s| s.mad >= best_score.mad + 1.5)
            });
            if best_score.mad <= 2.5
                && best_score.inlier >= 0.96
                && best_score.textured_rows >= 8
                && unique_in_window
                && sharp_peak
            {
                return Ok(best_dy);
            }
        }
    }

    let coarse_scores = coarse_band_scores(prev, curr, c_top, c_bot, -c_max, c_max);
    if coarse_scores.is_empty() {
        return Err(ScrollWarning::TooFast);
    }

    let mut raw_coarse: Vec<f32> = coarse_scores.iter().map(|&(_, s)| s).collect();
    let mid = raw_coarse.len() / 2;
    let (_, median_coarse_score, _) = raw_coarse.select_nth_unstable_by(mid, f32::total_cmp);
    let median_coarse_score = *median_coarse_score;

    let candidates = coarse_minima(&coarse_scores, MAX_COARSE_CANDIDATES);
    let mut fine_results = refine_coarse_candidates(
        prev,
        curr,
        active_top as i32,
        active_bottom as i32,
        max_shift,
        &candidates,
    );

    if fine_results.is_empty() {
        return Err(ScrollWarning::TooFast);
    }

    fine_results.sort_by(|a, b| a.1.effective_mad().total_cmp(&b.1.effective_mad()));
    let (best_dy, best_score) = fine_results[0];
    let best_eff = best_score.effective_mad();

    let accepted = if best_dy.abs() > active_h * 80 / 100 {
        let strong_match = (best_score.mad <= 5.0 && best_score.inlier >= 0.92)
            || (best_score.trimmed_grad_share >= 0.65
                && best_score.trimmed_mad <= 3.5
                && best_score.trimmed_inlier >= 0.95);
        let unique = !fine_results
            .iter()
            .any(|&(dy, s)| (dy - best_dy).abs() >= 12 && s.effective_mad() < best_eff + 3.0);
        best_score.textured_rows >= 6 && strong_match && unique
    } else {
        let standard_ok = best_score.standard_ok();
        let trimmed_ok = best_score.qualifies_trimmed();
        let sharp_aa_ok = best_score.textured_rows >= 6
            && best_score.mad <= 10.5
            && best_score.inlier >= 0.84
            && best_score.textured_mad <= 18.0
            && best_score.textured_inlier >= 0.75
            && median_coarse_score >= 12.0
            && median_coarse_score >= best_score.mad * 2.5;
        standard_ok || trimmed_ok || sharp_aa_ok
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

    // Disambiguate repetitive UI rows (e.g. uniform list separators) when a
    // second distant candidate scores nearly as well as the top candidate.
    let near_ties: Vec<(i32, FineScore)> = fine_results
        .iter()
        .copied()
        .filter(|(dy, score)| {
            *dy == best_dy
                || ((score.standard_ok() || score.qualifies_trimmed())
                    && score.effective_mad() <= best_eff + 1.5)
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

    Ok(best_dy)
}

fn score_fine_shift(
    prev: &FrameProfile,
    curr: &FrameProfile,
    active_top: i32,
    active_bottom: i32,
    dy: i32,
) -> Option<FineScore> {
    let ov_start = active_top.max(active_top + dy);
    let ov_end = active_bottom.min(active_bottom + dy);
    let ov_len = ov_end - ov_start;
    if ov_len < 16 {
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

    let use_bands = prev.fine_w >= 64;
    let mut band_weighted_err = [0.0_f32; 8];
    let mut band_weight_sum = [0.0_f32; 8];
    let mut band_inliers = [0usize; 8];
    let mut band_samples = [0usize; 8];
    let mut band_grad = [0u64; 8];

    for b in 0..buckets {
        let r0 = ov_start + (b as i32 * ov_len) / buckets as i32;
        let r1 = (ov_start + ((b + 1) as i32 * ov_len) / buckets as i32).max(r0 + 1);
        let mut pick_y = r0;
        let mut best_g = -1.0_f32;
        for y in r0..r1 {
            let g = prev.row_grad[y as usize];
            if g > best_g {
                best_g = g;
                pick_y = y;
            }
        }
        let py = pick_y as usize;
        let cy = (pick_y - dy) as usize;
        if prev.row_grad[py] >= 1.5 {
            textured_rows += 1;
        }
        let p_row = &prev.fine_luma[py * prev.fine_w..(py + 1) * prev.fine_w];
        let c_row = &curr.fine_luma[cy * curr.fine_w..(cy + 1) * curr.fine_w];
        let w = prev.row_grad[py].max(1.0);

        if use_bands {
            let mut row_err = 0u32;
            for band in 0..8 {
                let bx0 = band * prev.fine_w / 8;
                let bx1 = (band + 1) * prev.fine_w / 8;
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
                    if x > 0 {
                        b_grad += u64::from(pa.abs_diff(p_row[x - 1]));
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
            weighted_err += (row_err as f32 / prev.fine_w as f32) * w;
            weight_sum += w;
        } else {
            let mut row_err = 0u32;
            for (&pa, &ca) in p_row.iter().zip(c_row) {
                let d = pa.abs_diff(ca);
                row_err += u32::from(d);
                if d <= INLIER_THRESHOLD {
                    inliers += 1;
                }
                samples += 1;
            }
            weighted_err += (row_err as f32 / prev.fine_w as f32) * w;
            weight_sum += w;
        }
    }

    if samples == 0 || weight_sum <= 0.0 {
        return None;
    }
    let mad = weighted_err / weight_sum;
    let inlier = inliers as f32 / samples as f32;
    let (textured_mad, textured_inlier, trimmed_mad, trimmed_inlier, trimmed_grad_share) =
        if use_bands {
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

            let mut kept_err = 0.0_f32;
            let mut kept_w = 0.0_f32;
            let mut kept_inl = 0usize;
            let mut kept_samples = 0usize;
            let mut kept_grad = 0u64;
            for &band in &order[..6] {
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
            (tx_mad, tx_inlier, t_mad, t_inlier, t_grad)
        } else {
            (mad, inlier, mad, inlier, 0.0)
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
        stitcher.committed = Some(Anchor {
            profile: initial.clone(),
            y: 0,
        });
        stitcher.rows = PagedRows::new(4);
        stitcher.rows.append_down(0, &vec![0; 100 * 16]).unwrap();
        stitcher.below_rows = 100;

        let frame = Arc::new(FrameProfile::from_image(RgbaImage::new(4, 100), 1.0));
        assert!(stitcher.commit_frame(frame.clone(), 150).is_err());
        assert_eq!(stitcher.total_height(), 100);
        assert_eq!(stitcher.committed.as_ref().unwrap().y, 0);

        stitcher.rows = PagedRows::new(4);
        stitcher.rows.prepend_up(0, &vec![0; 100 * 16]).unwrap();
        stitcher.below_rows = 0;
        stitcher.above_rows = 100;
        assert!(stitcher.commit_frame(frame, -250).is_err());
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
        let profile = FrameProfile::from_image(frame, 1.0);

        let seam = stitcher.best_seam_down(&profile, 0, 0, 0, 100).unwrap();
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
        stitcher.committed = Some(Anchor {
            profile: profile.clone(),
            y: 17,
        });
        stitcher.prev = Some(Anchor {
            profile: profile.clone(),
            y: 23,
        });
        stitcher.viewport_y = 23;
        stitcher.header_rows = profile.raw_rows(0, 40).to_vec();
        stitcher.footer_rows = profile.raw_rows(280, 300).to_vec();

        stitcher.apply_fixed_margins(24, 8, &profile).unwrap();

        assert_eq!(stitcher.fixed_margins(), (24, 8));
        assert_eq!(stitcher.above_rows, 76);
        assert_eq!(stitcher.below_rows, 192);
        assert_eq!(stitcher.total_height(), 300);
        assert_eq!(
            stitcher.rows.read_rows(-60, 240).unwrap().as_raw(),
            active.as_slice()
        );
        assert_eq!(stitcher.header_rows, profile.raw_rows(0, 24));
        assert_eq!(
            stitcher.rows.read_rows(-76, 16).unwrap().as_raw(),
            profile.raw_rows(24, 40)
        );
        assert_eq!(stitcher.footer_rows, profile.raw_rows(292, 300));
        assert_eq!(
            stitcher.rows.read_rows(180, 12).unwrap().as_raw(),
            profile.raw_rows(280, 292)
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
        let prev = FrameProfile::from_image(viewport_frame(&doc, 0, 300, 48, 0, false, false), 1.0);
        let curr = FrameProfile::from_image(viewport_frame(&doc, 40, 300, 0, 0, false, false), 1.0);
        let mut stitcher = Stitcher::new(StitchConfig::default());
        stitcher.width = 400;
        stitcher.frame_height = 300;
        stitcher.fixed_top = 48;
        stitcher.cand_top_votes = Some((48, 2));
        stitcher.cand_bottom_votes = Some((0, 2));
        stitcher.rows = PagedRows::new(400);
        stitcher
            .rows
            .append_down(0, prev.raw_rows(48, 300))
            .unwrap();
        stitcher.below_rows = 252;
        stitcher.header_rows = prev.raw_rows(0, 48).to_vec();

        stitcher.update_fixed_margins(&prev, &curr).unwrap();
        assert_eq!(stitcher.cand_top_votes, Some((0, 1)));
        assert_eq!(stitcher.fixed_margins(), (48, 0));
        for _ in 1..MARGIN_CHANGE_VOTES - 1 {
            stitcher.update_fixed_margins(&prev, &curr).unwrap();
            assert_eq!(stitcher.fixed_margins(), (48, 0));
        }
        stitcher.update_fixed_margins(&prev, &curr).unwrap();

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
        let profile = Arc::new(FrameProfile::from_image(frame.clone(), 1.0));
        let mut stitcher = Stitcher::new(StitchConfig::default());
        stitcher.width = 4;
        stitcher.frame_height = 300;
        stitcher.rows = PagedRows::new(4);
        stitcher.rows.append_down(-100, frame.as_raw()).unwrap();
        stitcher.above_rows = 100;
        stitcher.below_rows = 200;

        stitcher.apply_fixed_margins(40, 20, &profile).unwrap();

        assert_eq!(stitcher.fixed_margins(), (40, 20));
        assert_eq!(stitcher.above_rows, 60);
        assert_eq!(stitcher.content_shift(), 60);
        assert_eq!(stitcher.below_rows, 180);
        assert_eq!(stitcher.total_height(), 300);
        assert_eq!(stitcher.header_rows, profile.raw_rows(0, 40));
        assert_eq!(stitcher.footer_rows, profile.raw_rows(280, 300));
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
}
