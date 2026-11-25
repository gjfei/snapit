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
//! - Absorbs macOS/Windows rubber-band bounce and emits incremental downscaled
//!   preview tiles capped at 1024 px height per tile.

use std::sync::Arc;

use image::RgbaImage;

const PREVIEW_MAX_WIDTH: u32 = 320;
const PREVIEW_TILE_MAX_HEIGHT: u32 = 1024;
const FINE_MAX_WIDTH: usize = 512;
const COARSE_STRIDE: usize = 4;
const MAX_FINE_ROWS: usize = 64;
const MAX_COARSE_CANDIDATES: usize = 6;
const MAX_MATCH_MAD: f32 = 7.5;
const MIN_INLIER_RATIO: f32 = 0.88;
const INLIER_THRESHOLD: u8 = 12;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScrollDirection {
    #[default]
    Down,
    Up,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollWarning {
    TooFast,
    LowTexture,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedOutcome {
    Initial,
    Unchanged,
    Moved,
    Stitched { dy: i32, total_height: u32 },
    Warning(ScrollWarning),
    MaxHeightReached,
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
    pub max_height: u32,
    /// Physical pixels per logical point.
    pub scale: f32,
}

impl Default for StitchConfig {
    fn default() -> Self {
        Self {
            max_height: 30_000,
            scale: 1.0,
        }
    }
}

impl StitchConfig {
    pub fn with_max_height(mut self, max_height: u32) -> Self {
        self.max_height = max_height.max(256);
        self
    }

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
    image: RgbaImage,
    width: u32,
    height: u32,
    grid: [u32; 64],
    fine_w: usize,
    fine_luma: Vec<u8>,
    row_grad: Vec<f32>,
    coarse_w: usize,
    coarse_h: usize,
    coarse_luma: Vec<u8>,
    coarse_grad: Vec<f32>,
}

impl FrameProfile {
    fn from_image(image: RgbaImage, scale: f32) -> Self {
        let (width, height) = image.dimensions();
        let w = width as usize;
        let h = height as usize;
        let raw = image.as_raw();

        let grid = sample_grid(raw, w, h);

        let margin_l = ((4.0 * scale).round() as usize).min(w / 16);
        let margin_r = ((20.0 * scale).round() as usize).min(w / 6);
        let interior_start = margin_l;
        let interior_end = w.saturating_sub(margin_r).max(interior_start + 1).min(w);
        let interior_w = interior_end - interior_start;

        let fine_step = (interior_w / FINE_MAX_WIDTH).max(1);
        let fine_w = interior_w.div_ceil(fine_step).max(1);
        let mut fine_luma = vec![0u8; fine_w * h];
        let mut row_grad = vec![0.0_f32; h];

        for y in 0..h {
            let row_off = y * w * 4;
            let dst_row = &mut fine_luma[y * fine_w..(y + 1) * fine_w];
            let mut prev_l = 0i32;
            let mut grad_sum = 0u32;
            for (ix, x) in (interior_start..interior_end)
                .step_by(fine_step)
                .enumerate()
            {
                let p = row_off + x * 4;
                let l = luma(raw[p], raw[p + 1], raw[p + 2]);
                dst_row[ix] = l;
                if ix > 0 {
                    grad_sum += (i32::from(l) - prev_l).unsigned_abs();
                }
                prev_l = i32::from(l);
            }
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
            coarse_w,
            coarse_h,
            coarse_luma,
            coarse_grad,
        }
    }

    fn matches_grid(&self, other: &Self) -> bool {
        self.grid == other.grid
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

fn luma(r: u8, g: u8, b: u8) -> u8 {
    ((u16::from(r) * 77 + u16::from(g) * 150 + u16::from(b) * 29) >> 8) as u8
}

fn sample_grid(raw: &[u8], w: usize, h: usize) -> [u32; 64] {
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

struct Anchor {
    profile: Arc<FrameProfile>,
    /// Virtual y coordinate (in canvas active-band space) where `profile`'s
    /// `active_top` currently lands.
    y: i32,
}

struct TileChunk {
    id: u64,
    revision: u64,
    /// Start row in full assembled canvas (from top of header).
    /// Stored lazily via slice bounds relative to active/header/footer.
    image: Arc<RgbaImage>,
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
    /// Active rows at y >= 0, stored top-to-bottom.
    below: Vec<u8>,
    below_rows: u32,
    /// Active rows at y < 0, stored bottom-to-top (row `-1` is at index `0`)
    /// so prepending upwards is an O(1) append.
    above: Vec<u8>,
    above_rows: u32,
    committed: Option<Anchor>,
    prev: Option<Anchor>,
    viewport_y: i32,
    last_dy: Option<i32>,
    direction: ScrollDirection,
    upward_established: bool,
    frames: usize,
    max_height_hit: bool,
    preview_revision: u64,
    next_tile_id: u64,
    tiles: Vec<TileChunk>,
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
            below: Vec::new(),
            below_rows: 0,
            above: Vec::new(),
            above_rows: 0,
            committed: None,
            prev: None,
            viewport_y: 0,
            last_dy: None,
            direction: ScrollDirection::Down,
            upward_established: false,
            frames: 0,
            max_height_hit: false,
            preview_revision: 0,
            next_tile_id: 1,
            tiles: Vec::new(),
        }
    }

    pub fn width(&self) -> u32 {
        self.width
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

    pub fn preview_revision(&self) -> u64 {
        self.preview_revision
    }

    pub fn preview_tiles(&self) -> Vec<PreviewTile> {
        self.tiles
            .iter()
            .map(|tile| PreviewTile {
                id: tile.id,
                revision: tile.revision,
                image: tile.image.clone(),
            })
            .collect()
    }

    /// Feed one captured SDR RGBA frame. Returns what changed.
    pub fn feed(&mut self, image: RgbaImage) -> FeedOutcome {
        let (w, h) = image.dimensions();
        if w == 0 || h == 0 {
            return FeedOutcome::Unchanged;
        }
        let curr = Arc::new(FrameProfile::from_image(image, self.config.scale));

        if self.committed.is_none() {
            self.initialize(curr);
            return FeedOutcome::Initial;
        }

        if w != self.width || h != self.frame_height {
            log::warn!(
                "scroll frame dimension changed {}x{} -> {}x{}; ignoring frame",
                self.width,
                self.frame_height,
                w,
                h
            );
            return FeedOutcome::Unchanged;
        }

        if self
            .prev
            .as_ref()
            .is_some_and(|prev| frames_identical(&prev.profile, &curr))
        {
            return FeedOutcome::Unchanged;
        }

        self.frames += 1;
        let (eff_top, eff_bottom) =
            if let Some(prev_profile) = self.prev.as_ref().map(|p| p.profile.clone()) {
                self.update_fixed_margins(&prev_profile, &curr)
            } else {
                (self.fixed_top, self.fixed_bottom)
            };

        let active_top = self.fixed_top;
        let active_bottom = self.frame_height - self.fixed_bottom;
        let active_h = active_bottom.saturating_sub(active_top);
        let search_top = eff_top.max(active_top);
        let search_bottom = (self.frame_height - eff_bottom).min(active_bottom);

        // Try matching against committed anchor first (zero drift); if out of
        // overlap range, match against previous frame and commit previous first.
        let committed_match = self.committed.as_ref().map(|anchor| {
            let match_res = estimate_shift(
                &anchor.profile,
                &curr,
                search_top,
                search_bottom,
                self.last_dy,
            );
            (anchor.y, match_res)
        });

        let mut hit_cap = false;
        let (base_y, shift) = match committed_match {
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
                        if let Some(prev_profile) = self.prev.as_ref().map(|p| p.profile.clone()) {
                            hit_cap |= self.commit_frame(prev_profile, prev_y);
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
        let commit_threshold = (active_h as i32 / 4).clamp(48, 240);
        let anchor_y = self.committed.as_ref().map_or(0, |a| a.y);
        let delta_from_anchor = next_y - anchor_y;

        let before_h = self.total_height();
        if delta_from_anchor.abs() >= commit_threshold {
            hit_cap |= self.commit_frame(curr.clone(), next_y);
        } else {
            // Even before committing a brand new anchor, update footer if we
            // are at the bottommost position so preview/end stays fresh.
            if next_y + active_h as i32 >= self.below_rows as i32 {
                self.update_footer(&curr);
            }
            if next_y <= -(self.above_rows as i32) {
                self.update_header(&curr);
            }
        }

        self.prev = Some(Anchor {
            profile: curr,
            y: next_y,
        });

        let after_h = self.total_height();
        if hit_cap || self.max_height_hit {
            self.max_height_hit = true;
            return FeedOutcome::MaxHeightReached;
        }
        if after_h != before_h {
            FeedOutcome::Stitched {
                dy: step_from_prev,
                total_height: after_h,
            }
        } else {
            FeedOutcome::Moved
        }
    }

    /// Commit any un-spliced trailing/leading scroll delta (called after idle or
    /// right before assembling the final image).
    pub fn flush_pending(&mut self) -> bool {
        let Some(prev) = self.prev.as_ref() else {
            return false;
        };
        let committed_y = self.committed.as_ref().map_or(prev.y, |a| a.y);
        if prev.y == committed_y {
            return false;
        }
        let before = self.total_height();
        let profile = prev.profile.clone();
        let y = prev.y;
        self.commit_frame(profile, y);
        self.total_height() != before
    }

    /// Assemble the complete stitched RGBA image.
    pub fn assemble(&mut self) -> Option<RgbaImage> {
        self.flush_pending();
        if self.width == 0 || self.total_height() == 0 {
            return None;
        }
        let stride = self.width as usize * 4;
        let total_h = self.total_height();
        let mut out = Vec::with_capacity(total_h as usize * stride);
        out.extend_from_slice(&self.header_rows);
        for row in (0..self.above_rows as usize).rev() {
            let start = row * stride;
            out.extend_from_slice(&self.above[start..start + stride]);
        }
        out.extend_from_slice(&self.below);
        out.extend_from_slice(&self.footer_rows);
        RgbaImage::from_raw(self.width, total_h, out)
    }

    fn initialize(&mut self, first: Arc<FrameProfile>) {
        self.width = first.width;
        self.frame_height = first.height;
        self.frames = 1;
        let stride = first.row_bytes();
        self.below.clear();
        self.below.extend_from_slice(first.image.as_raw());
        self.below_rows = first.height;
        self.above.clear();
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
        self.max_height_hit = false;
        self.committed = Some(Anchor {
            profile: first.clone(),
            y: 0,
        });
        self.prev = Some(Anchor {
            profile: first,
            y: 0,
        });
        let _ = stride;
        self.rebuild_all_preview_tiles();
    }

    /// Update consensus on fixed top/bottom sticky bars. Only textured static
    /// bands qualify so plain whitespace is never misclassified.
    /// Returns temporary `(search_top, search_bottom)` margins to shield shift
    /// estimation even on the very first moving frame.
    fn update_fixed_margins(&mut self, prev: &FrameProfile, curr: &FrameProfile) -> (u32, u32) {
        let h = self.frame_height as usize;
        if h < 96 || prev.fine_w == 0 {
            return (self.fixed_top, self.fixed_bottom);
        }
        let max_band = (h * 35 / 100).min(h / 2 - 24);
        if max_band < 8 {
            return (self.fixed_top, self.fixed_bottom);
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
            return (self.fixed_top, self.fixed_bottom);
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
                    if curr.row_grad[y] >= 1.0 {
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
                    if curr.row_grad[y] >= 1.0 {
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

        if cand_top > 0 {
            self.cand_top_votes = Some(match self.cand_top_votes {
                Some((v, n)) if v.abs_diff(cand_top) <= 4 => (v.min(cand_top), n + 1),
                _ => (cand_top, 1),
            });
        }
        if cand_bottom > 0 {
            self.cand_bottom_votes = Some(match self.cand_bottom_votes {
                Some((v, n)) if v.abs_diff(cand_bottom) <= 4 => (v.min(cand_bottom), n + 1),
                _ => (cand_bottom, 1),
            });
        }

        let next_top = match self.cand_top_votes {
            Some((v, n)) if n >= 2 => self.fixed_top.max(v),
            _ => self.fixed_top,
        };
        let next_bottom = match self.cand_bottom_votes {
            Some((v, n)) if n >= 2 => self.fixed_bottom.max(v),
            _ => self.fixed_bottom,
        };

        let min_active = (self.frame_height / 3).max(64);
        if (next_top != self.fixed_top || next_bottom != self.fixed_bottom)
            && next_top + next_bottom + min_active <= self.frame_height
        {
            self.apply_fixed_margins(next_top, next_bottom, curr);
        }

        let eff_top = self.fixed_top.max(cand_top);
        let eff_bottom = self.fixed_bottom.max(cand_bottom);
        if eff_top + eff_bottom + min_active <= self.frame_height {
            (eff_top, eff_bottom)
        } else {
            (self.fixed_top, self.fixed_bottom)
        }
    }

    /// Re-slice the already-captured initial canvas rows once fixed top/bottom
    /// margins are discovered on the first moving frames.
    fn apply_fixed_margins(&mut self, new_top: u32, new_bottom: u32, curr: &FrameProfile) {
        let stride = curr.row_bytes();
        let old_top = self.fixed_top;
        let old_bottom = self.fixed_bottom;
        if new_top > old_top {
            let delta = (new_top - old_top) as usize;
            let take = delta.min(self.below_rows as usize);
            if take > 0 {
                // In `below`, row 0 was `old_top`. Moving `take` rows to
                // `header_rows` shifts canvas row indices by `-take`, while each
                // anchor's `active_top` shifts by `+take` — they cancel out, so
                // `committed.y`, `prev.y`, and `viewport_y` remain unchanged.
                self.header_rows
                    .extend_from_slice(&self.below[..take * stride]);
                self.below.drain(..take * stride);
                self.below_rows -= take as u32;
            }
            self.fixed_top = new_top;
        }
        if new_bottom > old_bottom {
            let delta = (new_bottom - old_bottom) as usize;
            let take = delta.min(self.below_rows as usize);
            if take > 0 {
                let split = (self.below_rows as usize - take) * stride;
                let mut new_footer = self.below.split_off(split);
                new_footer.extend_from_slice(&self.footer_rows);
                self.footer_rows = new_footer;
                self.below_rows -= take as u32;
            }
            self.fixed_bottom = new_bottom;
        }
        log::info!(
            "scroll fixed margins updated: top={} bottom={} (active_h={})",
            self.fixed_top,
            self.fixed_bottom,
            self.frame_height - self.fixed_top - self.fixed_bottom
        );
        self.rebuild_all_preview_tiles();
    }

    fn update_header(&mut self, curr: &FrameProfile) {
        if self.fixed_top == 0 {
            return;
        }
        let rows = curr.raw_rows(0, self.fixed_top);
        self.header_rows.clear();
        self.header_rows.extend_from_slice(rows);
    }

    fn update_footer(&mut self, curr: &FrameProfile) {
        if self.fixed_bottom == 0 {
            return;
        }
        let start = self.frame_height - self.fixed_bottom;
        let rows = curr.raw_rows(start, self.frame_height);
        self.footer_rows.clear();
        self.footer_rows.extend_from_slice(rows);
    }

    /// Splice `frame` whose active top lands at virtual `y` into `below`/`above`.
    /// Returns true if the height cap was reached.
    fn commit_frame(&mut self, frame: Arc<FrameProfile>, y: i32) -> bool {
        let active_top = self.fixed_top;
        let active_bottom = self.frame_height - self.fixed_bottom;
        let active_h = active_bottom.saturating_sub(active_top);
        if active_h == 0 {
            return false;
        }
        let stride = frame.row_bytes();
        let top_extent = -(self.above_rows as i32);
        let bottom_extent = self.below_rows as i32;
        let y_end = y + active_h as i32;

        // Rubber-band / in-bounds protection: if entirely inside already-stitched
        // span, move anchor without adding duplicate rows.
        if y >= top_extent && y_end <= bottom_extent {
            self.committed = Some(Anchor { profile: frame, y });
            return false;
        }

        let max_active = self
            .config
            .max_height
            .saturating_sub(self.fixed_top + self.fixed_bottom);
        let mut hit_cap = false;

        // Downward growth: y_end > bottom_extent
        if y_end > bottom_extent {
            let room = max_active.saturating_sub(self.above_rows + self.below_rows);
            let needed = (y_end - bottom_extent) as u32;
            if room == 0 {
                hit_cap = true;
            } else {
                let allow_end = if needed > room {
                    hit_cap = true;
                    bottom_extent + room as i32
                } else {
                    y_end
                };

                // Choose minimum-difference seam row in the middle 30%..70% of the
                // overlap between [0..bottom_extent] and [y..allow_end].
                let overlap_start = y.max(top_extent);
                let overlap_end = bottom_extent.min(allow_end);
                let seam_y = if overlap_end > overlap_start + 8 {
                    self.best_seam_down(&frame, y, active_top, overlap_start, overlap_end)
                } else {
                    bottom_extent
                };

                // Overwrite existing `below` rows from `seam_y.max(0)`..`bottom_extent`,
                // then append new rows `bottom_extent`..`allow_end`.
                let overwrite_start = seam_y.max(0);
                for cy in overwrite_start..bottom_extent {
                    let src_row = (cy - y) as u32 + active_top;
                    let src = frame.raw_rows(src_row, src_row + 1);
                    let dst_off = cy as usize * stride;
                    if dst_off + stride <= self.below.len() {
                        self.below[dst_off..dst_off + stride].copy_from_slice(src);
                    }
                }
                if allow_end > bottom_extent {
                    let src_start = (bottom_extent - y) as u32 + active_top;
                    let src_end = (allow_end - y) as u32 + active_top;
                    self.below
                        .extend_from_slice(frame.raw_rows(src_start, src_end));
                    self.below_rows += (allow_end - bottom_extent) as u32;
                }
                self.update_footer(&frame);
                self.refresh_bottom_preview_tiles();
            }
        }

        // Upward growth: y < top_extent
        if y < top_extent {
            let delta_up = (top_extent - y) as u32;
            let room = max_active.saturating_sub(self.above_rows + self.below_rows);
            let allow_new = if delta_up > room {
                hit_cap = true;
                room
            } else {
                delta_up
            };
            // Guard against macOS top rubber-band solid background: require at
            // least 24 px or non-trivial texture in the newly exposed top band.
            let allow_y = top_extent - allow_new as i32;
            let newly_exposed_has_texture = {
                let start_r = active_top as usize;
                let end_r = (active_top + allow_new).min(active_bottom) as usize;
                frame.row_grad[start_r..end_r].iter().any(|&g| g >= 1.5)
            };
            if allow_new > 0
                && (self.upward_established || allow_new >= 24 || newly_exposed_has_texture)
            {
                self.upward_established = true;
                let overlap_start = top_extent;
                let overlap_end = (y + active_h as i32).min(bottom_extent);
                let seam_y = if overlap_end > overlap_start + 8 {
                    self.best_seam_up(&frame, y, active_top, overlap_start, overlap_end)
                } else {
                    top_extent
                };

                // Overwrite existing rows in [top_extent .. seam_y] (which may
                // span `above` and/or top of `below`).
                for cy in top_extent..seam_y {
                    let src_row = (cy - y) as u32 + active_top;
                    let src = frame.raw_rows(src_row, src_row + 1);
                    if cy < 0 {
                        let idx = (-cy - 1) as usize;
                        let dst_off = idx * stride;
                        if dst_off + stride <= self.above.len() {
                            self.above[dst_off..dst_off + stride].copy_from_slice(src);
                        }
                    } else {
                        let dst_off = cy as usize * stride;
                        if dst_off + stride <= self.below.len() {
                            self.below[dst_off..dst_off + stride].copy_from_slice(src);
                        }
                    }
                }

                // Append rows y = -above_rows - 1 down to allow_y into `above`.
                for cy in (allow_y..top_extent).rev() {
                    let src_row = (cy - y) as u32 + active_top;
                    self.above
                        .extend_from_slice(frame.raw_rows(src_row, src_row + 1));
                    self.above_rows += 1;
                }
                self.update_header(&frame);
                self.rebuild_all_preview_tiles();
            }
        }

        self.committed = Some(Anchor { profile: frame, y });
        hit_cap
    }

    fn best_seam_down(
        &self,
        frame: &FrameProfile,
        frame_y: i32,
        active_top: u32,
        overlap_start: i32,
        overlap_end: i32,
    ) -> i32 {
        let span = overlap_end - overlap_start;
        let lo = overlap_start + span * 3 / 10;
        let hi = (overlap_start + span * 7 / 10).max(lo + 1);
        let stride = frame.row_bytes();
        let mut best_y = (lo + hi) / 2;
        let mut best_diff = u64::MAX;
        let step_x = (self.width as usize / 64).max(1);
        for cy in lo..hi {
            let src_row = (cy - frame_y) as u32 + active_top;
            let src = frame.raw_rows(src_row, src_row + 1);
            let existing = if cy >= 0 {
                let off = cy as usize * stride;
                &self.below[off..off + stride]
            } else {
                let idx = (-cy - 1) as usize;
                let off = idx * stride;
                &self.above[off..off + stride]
            };
            let diff = sampled_row_diff(existing, src, step_x);
            if diff < best_diff {
                best_diff = diff;
                best_y = cy;
            }
        }
        best_y
    }

    fn best_seam_up(
        &self,
        frame: &FrameProfile,
        frame_y: i32,
        active_top: u32,
        overlap_start: i32,
        overlap_end: i32,
    ) -> i32 {
        // Same mid-overlap minimum-difference search.
        self.best_seam_down(frame, frame_y, active_top, overlap_start, overlap_end)
    }

    /// Read one full-canvas row (0..total_height) without assembling the full
    /// output buffer.
    fn canvas_row(&self, y: u32) -> &[u8] {
        let stride = self.width as usize * 4;
        let y = y as usize;
        let h_top = self.fixed_top as usize;
        if y < h_top {
            let off = y * stride;
            return &self.header_rows[off..off + stride];
        }
        let y = y - h_top;
        let above_len = self.above_rows as usize;
        if y < above_len {
            let idx = above_len - 1 - y;
            let off = idx * stride;
            return &self.above[off..off + stride];
        }
        let y = y - above_len;
        let below_len = self.below_rows as usize;
        if y < below_len {
            let off = y * stride;
            return &self.below[off..off + stride];
        }
        let y = y - below_len;
        let off = y * stride;
        &self.footer_rows[off..off + stride]
    }

    fn preview_dimensions(&self) -> (u32, u32, u32) {
        let full_w = self.width.max(1);
        let full_h = self.total_height().max(1);
        let prev_w = full_w.clamp(1, PREVIEW_MAX_WIDTH);
        let prev_h = ((full_h as f64 * prev_w as f64 / full_w as f64).round() as u32).max(1);
        (prev_w, prev_h, full_h)
    }

    fn render_preview_slice(
        &self,
        prev_w: u32,
        prev_h: u32,
        full_h: u32,
        py0: u32,
        py1: u32,
    ) -> RgbaImage {
        let tile_h = (py1 - py0).max(1);
        let mut rgba = vec![0u8; prev_w as usize * tile_h as usize * 4];
        let full_w = self.width as usize;
        for dy in 0..tile_h {
            let py = py0 + dy;
            let src_y =
                ((py as u64 * full_h as u64) / prev_h as u64).min((full_h - 1) as u64) as u32;
            let src_row = self.canvas_row(src_y);
            let dst_off = dy as usize * prev_w as usize * 4;
            for px in 0..prev_w {
                let src_x =
                    ((px as u64 * full_w as u64) / prev_w as u64).min((full_w - 1) as u64) as usize;
                let s = src_x * 4;
                let d = dst_off + px as usize * 4;
                rgba[d..d + 4].copy_from_slice(&src_row[s..s + 4]);
            }
        }
        RgbaImage::from_raw(prev_w, tile_h, rgba).expect("preview tile buffer size matches")
    }

    fn rebuild_all_preview_tiles(&mut self) {
        let (prev_w, prev_h, full_h) = self.preview_dimensions();
        let count = prev_h.div_ceil(PREVIEW_TILE_MAX_HEIGHT) as usize;
        self.tiles.clear();
        for ix in 0..count {
            let py0 = ix as u32 * PREVIEW_TILE_MAX_HEIGHT;
            let py1 = (py0 + PREVIEW_TILE_MAX_HEIGHT).min(prev_h);
            let id = self.next_tile_id;
            self.next_tile_id += 1;
            let image = Arc::new(self.render_preview_slice(prev_w, prev_h, full_h, py0, py1));
            self.tiles.push(TileChunk {
                id,
                revision: 1,
                image,
            });
        }
        self.preview_revision = self.preview_revision.wrapping_add(1);
    }

    /// Update only the bottom tile(s) when scrolling downward.
    fn refresh_bottom_preview_tiles(&mut self) {
        if self.tiles.is_empty() {
            self.rebuild_all_preview_tiles();
            return;
        }
        let (prev_w, prev_h, full_h) = self.preview_dimensions();
        let needed = prev_h.div_ceil(PREVIEW_TILE_MAX_HEIGHT) as usize;
        let first_dirty = self.tiles.len().saturating_sub(1);
        for ix in first_dirty..needed {
            let py0 = ix as u32 * PREVIEW_TILE_MAX_HEIGHT;
            let py1 = (py0 + PREVIEW_TILE_MAX_HEIGHT).min(prev_h);
            let image = Arc::new(self.render_preview_slice(prev_w, prev_h, full_h, py0, py1));
            if let Some(existing) = self.tiles.get_mut(ix) {
                existing.revision = existing.revision.wrapping_add(1);
                existing.image = image;
            } else {
                let id = self.next_tile_id;
                self.next_tile_id += 1;
                self.tiles.push(TileChunk {
                    id,
                    revision: 1,
                    image,
                });
            }
        }
        self.preview_revision = self.preview_revision.wrapping_add(1);
    }
}

fn sampled_row_diff(a: &[u8], b: &[u8], step_x: usize) -> u64 {
    let mut sum = 0u64;
    let pixels = a.len().min(b.len()) / 4;
    for x in (0..pixels).step_by(step_x.max(1)) {
        let p = x * 4;
        sum += u64::from(a[p].abs_diff(b[p]))
            + u64::from(a[p + 1].abs_diff(b[p + 1]))
            + u64::from(a[p + 2].abs_diff(b[p + 2]));
    }
    sum
}

fn frames_identical(a: &FrameProfile, b: &FrameProfile) -> bool {
    if !a.matches_grid(b) {
        return false;
    }
    a.image.as_raw() == b.image.as_raw()
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

    let min_overlap = (active_h / 6).clamp(32, 160);
    let max_shift = (active_h * 82 / 100).min(active_h - min_overlap);
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
    let mut coarse_scores: Vec<(i32, f32)> = Vec::with_capacity((c_max * 2 + 1) as usize);

    for c_dy in -c_max..=c_max {
        let ov_start = c_top.max(c_top + c_dy);
        let ov_end = c_bot.min(c_bot + c_dy);
        let ov_len = ov_end - ov_start;
        if ov_len < 4 {
            continue;
        }
        let mut weighted_err = 0.0_f32;
        let mut weight_sum = 0.0_f32;
        for y in ov_start..ov_end {
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
            coarse_scores.push((c_dy, weighted_err / weight_sum));
        }
    }

    if coarse_scores.is_empty() {
        return Err(ScrollWarning::TooFast);
    }

    // Extract local minima + overall best candidates.
    let mut candidates: Vec<(i32, f32)> = Vec::new();
    for i in 0..coarse_scores.len() {
        let (dy, score) = coarse_scores[i];
        let left_ok = i == 0 || score <= coarse_scores[i - 1].1;
        let right_ok = i + 1 == coarse_scores.len() || score <= coarse_scores[i + 1].1;
        if left_ok && right_ok {
            candidates.push((dy, score));
        }
    }
    candidates.sort_by(|a, b| a.1.total_cmp(&b.1));
    candidates.truncate(MAX_COARSE_CANDIDATES);

    // Fine search on 1x physical pixels around each coarse candidate.
    let mut fine_results: Vec<(i32, f32, f32)> = Vec::new();
    for (c_dy, _) in candidates {
        let center = c_dy * COARSE_STRIDE as i32;
        let lo = (center - COARSE_STRIDE as i32).max(-max_shift);
        let hi = (center + COARSE_STRIDE as i32).min(max_shift);
        let mut best_local: Option<(i32, f32, f32)> = None;
        for dy in lo..=hi {
            if let Some((mad, inlier)) =
                score_fine_shift(prev, curr, active_top as i32, active_bottom as i32, dy)
                && best_local.is_none_or(|(_, b_mad, _)| mad < b_mad)
            {
                best_local = Some((dy, mad, inlier));
            }
        }
        if let Some(best) = best_local {
            fine_results.push(best);
        }
    }

    if fine_results.is_empty() {
        return Err(ScrollWarning::TooFast);
    }

    fine_results.sort_by(|a, b| a.1.total_cmp(&b.1));
    let (best_dy, best_mad, best_inlier) = fine_results[0];

    if best_mad > MAX_MATCH_MAD || best_inlier < MIN_INLIER_RATIO {
        return Err(ScrollWarning::TooFast);
    }

    // Disambiguate repetitive UI rows (e.g. uniform list separators) when a
    // second distant candidate scores nearly as well as the top candidate.
    let near_ties: Vec<(i32, f32, f32)> = fine_results
        .iter()
        .copied()
        .filter(|(dy, mad, inlier)| {
            *inlier >= MIN_INLIER_RATIO && (*mad <= best_mad + 1.5 || (*dy == best_dy))
        })
        .collect();

    if near_ties.len() > 1
        && near_ties
            .iter()
            .any(|(dy, _, _)| (dy - best_dy).abs() >= 12)
        && let Some(prev_dy) = last_dy
    {
        let chosen = near_ties
            .iter()
            .copied()
            .min_by(|(dy_a, mad_a, _), (dy_b, mad_b, _)| {
                let cost_a = (*mad_a) + ((*dy_a - prev_dy).abs() as f32) * 0.004;
                let cost_b = (*mad_b) + ((*dy_b - prev_dy).abs() as f32) * 0.004;
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
) -> Option<(f32, f32)> {
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
        let p_row = &prev.fine_luma[py * prev.fine_w..(py + 1) * prev.fine_w];
        let c_row = &curr.fine_luma[cy * curr.fine_w..(cy + 1) * curr.fine_w];
        let mut row_err = 0u32;
        for (&pa, &ca) in p_row.iter().zip(c_row) {
            let d = pa.abs_diff(ca);
            row_err += u32::from(d);
            if d <= INLIER_THRESHOLD {
                inliers += 1;
            }
            samples += 1;
        }
        let w = prev.row_grad[py].max(1.0);
        weighted_err += (row_err as f32 / prev.fine_w as f32) * w;
        weight_sum += w;
    }

    if samples == 0 || weight_sum <= 0.0 {
        return None;
    }
    Some((weighted_err / weight_sum, inliers as f32 / samples as f32))
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
                        42 + ((x / 16 + y) % 5) as u8 * 15
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
        let result = stitcher.assemble().unwrap();
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
        let result = stitcher.assemble().unwrap();
        assert_eq!(result.dimensions(), (360, 1400));
        assert_eq!(result.as_raw(), doc.as_raw());
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
        let result = stitcher.assemble().unwrap();
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
    fn rubber_band_bounce_produces_no_duplication() {
        let doc = make_document(360, 1000);
        let view_h = 400;
        let mut stitcher = Stitcher::new(StitchConfig::default());
        // Scroll down to bottom (600), bounce back to 540, then settle at 600.
        for scroll_y in [0, 150, 320, 500, 600, 540, 570, 600] {
            let frame = viewport_frame(&doc, scroll_y, view_h, 0, 0, false, false);
            let _ = stitcher.feed(frame);
        }
        let result = stitcher.assemble().unwrap();
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
            FeedOutcome::Stitched { .. } | FeedOutcome::Moved
        ));
        // Jump far past overlap -> TooFast warning
        let outcome = stitcher.feed(viewport_frame(&doc, 800, view_h, 0, 0, false, false));
        assert_eq!(outcome, FeedOutcome::Warning(ScrollWarning::TooFast));
        // Scroll back into overlap with anchor (340) -> recovers cleanly
        let recover = stitcher.feed(viewport_frame(&doc, 340, view_h, 0, 0, false, false));
        assert!(
            matches!(recover, FeedOutcome::Stitched { .. } | FeedOutcome::Moved),
            "expected recovery, got {recover:?}"
        );
    }

    #[test]
    fn max_height_and_incremental_preview_tiles_work() {
        let doc = make_document(640, 3200);
        let mut stitcher = Stitcher::new(StitchConfig::default().with_max_height(1500));
        for scroll_y in (0..=2700).step_by(200) {
            let _ = stitcher.feed(viewport_frame(&doc, scroll_y, 500, 0, 0, false, false));
        }
        let assembled = stitcher.assemble().unwrap();
        assert!(assembled.height() <= 1500);
        let tiles = stitcher.preview_tiles();
        assert!(!tiles.is_empty());
        for tile in &tiles {
            assert!(tile.image.width() <= PREVIEW_MAX_WIDTH);
            assert!(tile.image.height() <= PREVIEW_TILE_MAX_HEIGHT);
        }
    }
}
