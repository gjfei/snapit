use std::{
    fs,
    path::Path,
    sync::{Arc, OnceLock},
};

use anyhow::{Context as _, Result, ensure};
use fontdue::{
    Font,
    layout::{CoordinateSystem, Layout, LayoutSettings, TextStyle},
};
use image::{ImageReader, RgbaImage, imageops};
use tiny_skia::{Pixmap, PixmapPaint, Transform};
use xcap::HdrImage;

const MAX_IMAGE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_IMAGE_PIXELS: u64 = 64 * 1024 * 1024;
const MAX_IMAGE_SIDE: u32 = 8192;
const MAX_TILES: usize = 4096;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WatermarkPlacement {
    #[default]
    TopLeft,
    TopCenter,
    TopRight,
    MiddleLeft,
    Center,
    MiddleRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TextWatermark {
    pub content: String,
    pub font_size: f32,
    pub color: [u8; 3],
    pub bold: bool,
    pub shadow: bool,
    pub stroke_width: f32,
    pub stroke_color: [u8; 3],
    pub background: bool,
    pub background_color: [u8; 3],
    pub background_opacity: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImageWatermark {
    pub image: Arc<RgbaImage>,
    pub width: f32,
    pub corner_radius: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum WatermarkElementKind {
    Text(TextWatermark),
    Image(ImageWatermark),
}

#[derive(Clone, Debug, PartialEq)]
pub struct WatermarkElement {
    pub kind: WatermarkElementKind,
    pub x: f32,
    pub y: f32,
    pub rotation_degrees: f32,
    pub scale: f32,
    pub opacity: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ElementBounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Debug)]
pub struct ComposedStamp {
    pub image: RgbaImage,
    pub logical_origin_x: f32,
    pub logical_origin_y: f32,
    pub logical_width: f32,
    pub logical_height: f32,
    pub rendered_element_bounds: Vec<ElementBounds>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WatermarkSpec {
    pub elements: Vec<WatermarkElement>,
    pub placement: WatermarkPlacement,
    pub margin_x: f32,
    pub margin_y: f32,
    pub rotation_degrees: f32,
    pub scale: f32,
    pub tiled: bool,
    pub tile_spacing_x: f32,
    pub tile_spacing_y: f32,
}

impl Default for WatermarkSpec {
    fn default() -> Self {
        Self {
            elements: Vec::new(),
            placement: WatermarkPlacement::TopLeft,
            margin_x: 24.0,
            margin_y: 24.0,
            rotation_degrees: 0.0,
            scale: 1.0,
            tiled: false,
            tile_spacing_x: 160.0,
            tile_spacing_y: 96.0,
        }
    }
}

struct PlacedElement {
    image: RgbaImage,
    left: f32,
    top: f32,
}

impl WatermarkSpec {
    pub fn is_enabled(&self) -> bool {
        !self.elements.is_empty()
    }

    pub fn compose_stamp(&self, layout_scale: f32) -> Result<ComposedStamp> {
        ensure!(self.is_enabled(), "watermark has no elements");
        let scale = if layout_scale.is_finite() && layout_scale > 0.0 {
            layout_scale
        } else {
            1.0
        };
        let design_scale = if self.scale.is_finite() && self.scale > 0.0 {
            self.scale
        } else {
            1.0
        };
        let combined_scale = scale * design_scale;

        let mut placed = Vec::with_capacity(self.elements.len());
        let mut min_x = f32::INFINITY;
        let mut min_y = f32::INFINITY;
        let mut max_x = f32::NEG_INFINITY;
        let mut max_y = f32::NEG_INFINITY;

        for element in &self.elements {
            let element_scale = if element.scale.is_finite() && element.scale > 0.0 {
                element.scale
            } else {
                1.0
            };
            let opacity = if element.opacity.is_finite() {
                element.opacity.clamp(0.0, 1.0)
            } else {
                0.65
            };
            let unrotated = match &element.kind {
                WatermarkElementKind::Text(text) => {
                    render_text(text, combined_scale * element_scale, opacity)?
                }
                WatermarkElementKind::Image(image) => {
                    render_image(image, combined_scale * element_scale, opacity)?
                }
            };
            let (uw, uh) = unrotated.dimensions();
            let image =
                if element.rotation_degrees.is_finite() && element.rotation_degrees.abs() > 0.01 {
                    rotate_rgba(&unrotated, element.rotation_degrees)?
                } else {
                    unrotated
                };
            let (rw, rh) = image.dimensions();
            let ex = if element.x.is_finite() {
                element.x
            } else {
                0.0
            };
            let ey = if element.y.is_finite() {
                element.y
            } else {
                0.0
            };
            let left = ex * combined_scale + (uw as f32 - rw as f32) * 0.5;
            let top = ey * combined_scale + (uh as f32 - rh as f32) * 0.5;
            min_x = min_x.min(left);
            min_y = min_y.min(top);
            max_x = max_x.max(left + rw as f32);
            max_y = max_y.max(top + rh as f32);
            placed.push(PlacedElement { image, left, top });
        }

        let origin_x = min_x.floor();
        let origin_y = min_y.floor();
        let right = max_x.ceil();
        let bottom = max_y.ceil();
        let stamp_width_f = (right - origin_x).max(1.0);
        let stamp_height_f = (bottom - origin_y).max(1.0);
        ensure!(
            stamp_width_f <= MAX_IMAGE_SIDE as f32 && stamp_height_f <= MAX_IMAGE_SIDE as f32,
            "watermark elements are too far apart (exceeds {MAX_IMAGE_SIDE} pixels)"
        );
        let stamp_width = stamp_width_f as u32;
        let stamp_height = stamp_height_f as u32;
        ensure!(
            u64::from(stamp_width) * u64::from(stamp_height) <= MAX_IMAGE_PIXELS,
            "composed watermark exceeds {MAX_IMAGE_PIXELS} pixels"
        );

        let mut canvas = RgbaImage::new(stamp_width, stamp_height);
        let mut local_boxes = Vec::with_capacity(placed.len());
        for item in &placed {
            let x = (item.left - origin_x).round().max(0.0) as u32;
            let y = (item.top - origin_y).round().max(0.0) as u32;
            composite_rgba_onto(&mut canvas, &item.image, x, y);
            local_boxes.push((
                item.left - origin_x,
                item.top - origin_y,
                item.image.width() as f32,
                item.image.height() as f32,
            ));
        }

        let (final_image, rendered_element_bounds) =
            if self.rotation_degrees.is_finite() && self.rotation_degrees.abs() > 0.01 {
                let (rotated, rot_min_x, rot_min_y) =
                    rotate_rgba_with_offset(&canvas, self.rotation_degrees)?;
                let transform = Transform::from_translate(-rot_min_x, -rot_min_y)
                    .pre_concat(Transform::from_rotate(self.rotation_degrees));
                let bounds = local_boxes
                    .into_iter()
                    .map(|(bx, by, bw, bh)| {
                        let corners = [(bx, by), (bx + bw, by), (bx, by + bh), (bx + bw, by + bh)];
                        let mut b_min_x = f32::INFINITY;
                        let mut b_min_y = f32::INFINITY;
                        let mut b_max_x = f32::NEG_INFINITY;
                        let mut b_max_y = f32::NEG_INFINITY;
                        for (cx, cy) in corners {
                            let mut pt = tiny_skia::Point::from_xy(cx, cy);
                            transform.map_point(&mut pt);
                            b_min_x = b_min_x.min(pt.x);
                            b_min_y = b_min_y.min(pt.y);
                            b_max_x = b_max_x.max(pt.x);
                            b_max_y = b_max_y.max(pt.y);
                        }
                        ElementBounds {
                            x: b_min_x / scale,
                            y: b_min_y / scale,
                            width: (b_max_x - b_min_x).max(1.0) / scale,
                            height: (b_max_y - b_min_y).max(1.0) / scale,
                        }
                    })
                    .collect();
                (rotated, bounds)
            } else {
                let bounds = local_boxes
                    .into_iter()
                    .map(|(bx, by, bw, bh)| ElementBounds {
                        x: bx / scale,
                        y: by / scale,
                        width: bw / scale,
                        height: bh / scale,
                    })
                    .collect();
                (canvas, bounds)
            };

        Ok(ComposedStamp {
            image: final_image,
            logical_origin_x: origin_x / combined_scale,
            logical_origin_y: origin_y / combined_scale,
            logical_width: (right - origin_x) / combined_scale,
            logical_height: (bottom - origin_y) / combined_scale,
            rendered_element_bounds,
        })
    }

    pub fn rasterize(&self, width: u32, height: u32, layout_scale: f32) -> Result<RasterLayer> {
        if !self.is_enabled() || width == 0 || height == 0 {
            return Ok(RasterLayer::default());
        }
        let scale = if layout_scale.is_finite() && layout_scale > 0.0 {
            layout_scale
        } else {
            1.0
        };
        let design_scale = if self.scale.is_finite() && self.scale > 0.0 {
            self.scale
        } else {
            1.0
        };
        let started = std::time::Instant::now();
        let stamp = self.compose_stamp(scale)?;
        let layer = layout_tiles(
            &stamp.image,
            width,
            height,
            scale,
            self.placement,
            self.margin_x,
            self.margin_y,
            self.tiled,
            self.tile_spacing_x,
            self.tile_spacing_y,
        )?;
        log::info!(
            "watermark raster: canvas={width}x{height}, scale={scale:.3}, design_scale={design_scale:.3}, elements={}, tile={}x{}, tiles={}, {:.2} ms",
            self.elements.len(),
            stamp.image.width(),
            stamp.image.height(),
            layer.tiles.len(),
            started.elapsed().as_secs_f64() * 1000.0
        );
        Ok(layer)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl PixelRect {
    pub fn right(self) -> u32 {
        self.x.saturating_add(self.width)
    }

    pub fn bottom(self) -> u32 {
        self.y.saturating_add(self.height)
    }
}

#[derive(Clone, Debug)]
pub struct RasterTile {
    pub bounds: PixelRect,
    pub pixels: Vec<[u8; 4]>,
}

#[derive(Clone, Debug, Default)]
pub struct RasterLayer {
    pub tiles: Vec<RasterTile>,
}

impl RasterLayer {
    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }

    pub fn apply_sdr(self: &Arc<Self>, image: Arc<RgbaImage>) -> Arc<RgbaImage> {
        if self.is_empty() {
            return image;
        }
        if Arc::strong_count(&image) != 1 {
            let mut copy = (*image).clone();
            self.apply_sdr_mut(&mut copy);
            return Arc::new(copy);
        }
        let mut image = Arc::try_unwrap(image).expect("checked unique above");
        self.apply_sdr_mut(&mut image);
        Arc::new(image)
    }

    pub fn apply_sdr_mut(&self, image: &mut RgbaImage) {
        let stride = image.width() as usize * 4;
        let raw = image.as_mut();
        for tile in &self.tiles {
            for row in 0..tile.bounds.height as usize {
                let start = (tile.bounds.y as usize + row) * stride + tile.bounds.x as usize * 4;
                let dst = &mut raw[start..start + tile.bounds.width as usize * 4];
                let src =
                    &tile.pixels[row * tile.bounds.width as usize..][..tile.bounds.width as usize];
                for (dst, src) in dst.as_chunks_mut::<4>().0.iter_mut().zip(src) {
                    blend_straight_rgba_pixel(dst, *src);
                }
            }
        }
    }

    pub fn apply_hdr(self: &Arc<Self>, image: Arc<HdrImage>) -> Arc<HdrImage> {
        if self.is_empty() {
            return image;
        }
        match Arc::try_unwrap(image) {
            Ok(image) => Arc::new(self.apply_hdr_owned(image)),
            Err(image) => Arc::new(self.apply_hdr_ref(&image)),
        }
    }

    pub fn apply_hdr_owned(&self, image: HdrImage) -> HdrImage {
        if self.is_empty() {
            return image;
        }
        let width = image.width();
        let height = image.height();
        let reference_white = image.reference_white_nits();
        let sdr_white = image.sdr_white_nits();
        let white_scale = sdr_white / reference_white.max(1.0);
        let mut raw = image.into_bytes();
        self.blend_hdr_raw(&mut raw, width, white_scale);
        HdrImage::new(width, height, raw, reference_white).with_sdr_white(sdr_white)
    }

    fn apply_hdr_ref(&self, image: &HdrImage) -> HdrImage {
        let width = image.width();
        let white_scale = image.sdr_white_nits() / image.reference_white_nits().max(1.0);
        let mut raw = image.as_bytes().to_vec();
        self.blend_hdr_raw(&mut raw, width, white_scale);
        HdrImage::new(width, image.height(), raw, image.reference_white_nits())
            .with_sdr_white(image.sdr_white_nits())
    }

    fn blend_hdr_raw(&self, raw: &mut [u8], width: u32, white_scale: f32) {
        for tile in &self.tiles {
            blend_hdr_pixels_raw(
                raw,
                width,
                white_scale,
                tile.bounds,
                tile.pixels.iter().copied(),
            );
        }
    }

    pub fn scaled_sample_regions(
        &self,
        source_width: u32,
        source_height: u32,
        target_width: u32,
        target_height: u32,
    ) -> Vec<PixelRect> {
        if source_width == 0 || source_height == 0 {
            return Vec::new();
        }
        self.tiles
            .iter()
            .map(|tile| {
                let x = tile.bounds.x * target_width / source_width;
                let y = tile.bounds.y * target_height / source_height;
                let right = tile.bounds.right() * target_width / source_width;
                let bottom = tile.bounds.bottom() * target_height / source_height;
                PixelRect {
                    x,
                    y,
                    width: right.saturating_sub(x).max(1),
                    height: bottom.saturating_sub(y).max(1),
                }
            })
            .collect()
    }
}

/// Composite straight-alpha sRGB pixels onto raw linear HDR samples.
pub fn blend_hdr_srgb<I>(
    raw: &mut [u8],
    image_width: u32,
    reference_white_nits: f32,
    sdr_white_nits: f32,
    bounds: PixelRect,
    pixels: I,
) where
    I: IntoIterator<Item = [u8; 4]>,
{
    let white_scale = sdr_white_nits / reference_white_nits.max(1.0);
    blend_hdr_pixels_raw(raw, image_width, white_scale, bounds, pixels);
}

fn blend_hdr_pixels_raw<I>(
    raw: &mut [u8],
    image_width: u32,
    white_scale: f32,
    bounds: PixelRect,
    pixels: I,
) where
    I: IntoIterator<Item = [u8; 4]>,
{
    if bounds.width == 0 || bounds.height == 0 {
        return;
    }
    let height = (raw.len() / (image_width as usize * 6)) as u32;
    let x = bounds.x.min(image_width);
    let y = bounds.y.min(height);
    let right = bounds.right().min(image_width);
    let bottom = bounds.bottom().min(height);
    if right <= x || bottom <= y {
        return;
    }
    let visible_width = (right - x) as usize;
    let visible_height = (bottom - y) as usize;
    let left_skip = bounds.x.saturating_sub(x) as usize;
    let right_skip = bounds.width as usize - left_skip - visible_width;
    let mut pixels = pixels.into_iter();
    for _ in 0..bounds.y.saturating_sub(y) {
        for _ in 0..bounds.width {
            if pixels.next().is_none() {
                return;
            }
        }
    }
    for row in 0..visible_height {
        for _ in 0..left_skip {
            if pixels.next().is_none() {
                return;
            }
        }
        for col in 0..visible_width {
            let Some(src) = pixels.next() else {
                return;
            };
            let alpha = u32::from(src[3]);
            if alpha != 0 {
                blend_hdr_pixel(
                    raw,
                    image_width,
                    white_scale,
                    x + col as u32,
                    y + row as u32,
                    src,
                    alpha,
                );
            }
        }
        for _ in 0..right_skip {
            if pixels.next().is_none() {
                return;
            }
        }
    }
}

fn blend_hdr_pixel(
    raw: &mut [u8],
    image_width: u32,
    white_scale: f32,
    x: u32,
    y: u32,
    src: [u8; 4],
    alpha: u32,
) {
    let start = (y as usize * image_width as usize + x as usize) * 6;
    let alpha = alpha as f32 / 255.0;
    let inverse = 1.0 - alpha;
    let linear = srgb_to_linear_table();
    for channel in 0..3 {
        let at = start + channel * 2;
        let old = f16_bits_to_f32(u16::from_le_bytes([raw[at], raw[at + 1]]));
        let source = linear[src[channel] as usize] * white_scale;
        let mixed = if alpha == 1.0 {
            source
        } else {
            source * alpha + old * inverse
        };
        let bits = f32_to_f16_bits(mixed);
        raw[at..at + 2].copy_from_slice(&bits.to_le_bytes());
    }
}

pub fn load_watermark_image(path: &Path) -> Result<Arc<RgbaImage>> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to read watermark image {}", path.display()))?;
    ensure!(
        metadata.len() <= MAX_IMAGE_BYTES,
        "watermark image exceeds 16 MiB"
    );
    let reader = ImageReader::open(path)
        .with_context(|| format!("failed to open watermark image {}", path.display()))?
        .with_guessed_format()?;
    let format = reader.format().context("watermark image has no format")?;
    ensure!(
        matches!(format, image::ImageFormat::Png | image::ImageFormat::Jpeg),
        "watermark image must be PNG or JPEG"
    );
    let (width, height) = reader.into_dimensions()?;
    validate_image_dimensions(width, height)?;
    let image = ImageReader::open(path)?
        .with_guessed_format()?
        .decode()
        .with_context(|| format!("failed to decode watermark image {}", path.display()))?
        .to_rgba8();
    log::info!(
        "watermark image loaded: {}x{} {}, {} bytes",
        image.width(),
        image.height(),
        format_args!("{format:?}"),
        metadata.len()
    );
    Ok(Arc::new(image))
}

fn validate_image_dimensions(width: u32, height: u32) -> Result<()> {
    ensure!(width > 0 && height > 0, "watermark image is empty");
    ensure!(
        width <= MAX_IMAGE_SIDE && height <= MAX_IMAGE_SIDE,
        "watermark image side exceeds {MAX_IMAGE_SIDE} pixels"
    );
    ensure!(
        u64::from(width) * u64::from(height) <= MAX_IMAGE_PIXELS,
        "watermark image exceeds {MAX_IMAGE_PIXELS} pixels"
    );
    Ok(())
}

fn blend_straight_rgba_pixel(dst: &mut [u8; 4], src: [u8; 4]) {
    let source_alpha = u32::from(src[3]);
    if source_alpha == 0 {
        return;
    }
    let destination_alpha = u32::from(dst[3]);
    if destination_alpha == 0 {
        dst.copy_from_slice(&src);
        return;
    }
    let inverse = 255 - source_alpha;
    let alpha = source_alpha * 255 + destination_alpha * inverse;
    for channel in 0..3 {
        let source = u32::from(src[channel]) * source_alpha * 255;
        let destination = u32::from(dst[channel]) * destination_alpha * inverse;
        dst[channel] = ((source + destination + alpha / 2) / alpha) as u8;
    }
    dst[3] = ((alpha + 127) / 255) as u8;
}

fn composite_rgba_onto(canvas: &mut RgbaImage, overlay: &RgbaImage, dst_x: u32, dst_y: u32) {
    let canvas_w = canvas.width() as usize;
    let canvas_h = canvas.height() as usize;
    let overlay_w = overlay.width() as usize;
    let overlay_h = overlay.height() as usize;
    let x0 = dst_x as usize;
    let y0 = dst_y as usize;
    if x0 >= canvas_w || y0 >= canvas_h {
        return;
    }
    let copy_w = overlay_w.min(canvas_w - x0);
    let copy_h = overlay_h.min(canvas_h - y0);
    let dst_raw = canvas.as_mut();
    let src_raw = overlay.as_raw();
    for row in 0..copy_h {
        let dst_start = ((y0 + row) * canvas_w + x0) * 4;
        let src_start = row * overlay_w * 4;
        let dst_slice = &mut dst_raw[dst_start..dst_start + copy_w * 4];
        let src_slice = &src_raw[src_start..src_start + copy_w * 4];
        for (dst, src) in dst_slice
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(src_slice.as_chunks::<4>().0)
        {
            blend_straight_rgba_pixel(dst, *src);
        }
    }
}

fn render_image(image: &ImageWatermark, scale: f32, opacity: f32) -> Result<RgbaImage> {
    let (source_width, source_height) = image.image.dimensions();
    validate_image_dimensions(source_width, source_height)?;
    let target_width =
        ((image.width.max(1.0) as f64 * scale as f64).round() as u32).clamp(1, MAX_IMAGE_SIDE);
    let target_height = ((target_width as f64 * source_height as f64 / source_width as f64).round()
        as u32)
        .clamp(1, MAX_IMAGE_SIDE);
    let mut rendered = imageops::resize(
        &*image.image,
        target_width,
        target_height,
        imageops::FilterType::Lanczos3,
    );
    let opacity = opacity.clamp(0.0, 1.0);
    let corner_percent = if image.corner_radius.is_finite() {
        image.corner_radius.clamp(0.0, 50.0)
    } else {
        0.0
    };
    let radius = (target_width.min(target_height) as f32) * (corner_percent / 100.0);
    for (y, row) in rendered.rows_mut().enumerate() {
        for (x, pixel) in row.enumerate() {
            let coverage = if radius > 0.25 {
                rounded_rect_coverage(
                    x as f32 + 0.5,
                    y as f32 + 0.5,
                    target_width as f32,
                    target_height as f32,
                    radius,
                )
            } else {
                1.0
            };
            pixel[3] = (f32::from(pixel[3]) * opacity * coverage)
                .round()
                .clamp(0.0, 255.0) as u8;
        }
    }
    log::debug!(
        "watermark image raster: {source_width}x{source_height} -> {target_width}x{target_height}, scale={scale:.3}, corner_radius={corner_percent:.1}%"
    );
    Ok(rendered)
}

fn rounded_rect_coverage(px: f32, py: f32, width: f32, height: f32, radius: f32) -> f32 {
    let cx = px.clamp(radius, (width - radius).max(radius));
    let cy = py.clamp(radius, (height - radius).max(radius));
    let dx = px - cx;
    let dy = py - cy;
    let dist = (dx * dx + dy * dy).sqrt() - radius;
    (0.5 - dist).clamp(0.0, 1.0)
}

fn render_text(text: &TextWatermark, scale: f32, opacity: f32) -> Result<RgbaImage> {
    let fonts = if text.bold {
        bold_font_chain()
    } else {
        font_chain()
    };
    ensure!(
        !fonts.is_empty(),
        "no system font is available for the watermark"
    );
    let normalized = text.content.replace("\r\n", "\n").replace('\r', "\n");
    let font_size = (text.font_size.max(1.0) * scale).clamp(1.0, 512.0);
    let mut runs = Vec::new();
    let mut current_font = 0usize;
    let mut current = String::new();
    for character in normalized.chars() {
        let Some(font_index) = fonts
            .iter()
            .position(|font| font.has_glyph(character) || character.is_whitespace())
        else {
            let replacement = ['\u{fffd}', '?']
                .into_iter()
                .find(|candidate| fonts[0].has_glyph(*candidate));
            let replacement = replacement.context("no font can render a missing-glyph marker")?;
            log::warn!(
                "watermark font chain lacks U+{:04X}; using a visible replacement",
                character as u32
            );
            if font_index_for(fonts, replacement) != current_font && !current.is_empty() {
                runs.push((current_font, std::mem::take(&mut current)));
                current_font = font_index_for(fonts, replacement);
            }
            current.push(replacement);
            continue;
        };
        if font_index != current_font && !current.is_empty() {
            runs.push((current_font, std::mem::take(&mut current)));
            current_font = font_index;
        }
        current.push(character);
    }
    if !current.is_empty() {
        runs.push((current_font, current));
    }
    ensure!(!runs.is_empty(), "watermark text is empty");

    let mut layout = Layout::new(CoordinateSystem::PositiveYDown);
    layout.reset(&LayoutSettings::default());
    for (font_index, run) in &runs {
        layout.append(fonts, &TextStyle::new(run, font_size, *font_index));
    }
    let glyphs = layout.glyphs();
    ensure!(!glyphs.is_empty(), "watermark text produced no glyphs");

    let stroke_px = if text.stroke_width.is_finite() && text.stroke_width > 0.01 {
        (text.stroke_width * scale).clamp(0.0, 32.0)
    } else {
        0.0
    };
    let shadow_blur = if text.shadow {
        (2.0 * scale).round().clamp(1.0, 12.0) as usize
    } else {
        0
    };
    let shadow_dy = if text.shadow {
        (1.5 * scale).round().max(1.0) as usize
    } else {
        0
    };
    let bold_extra = if text.bold && font_size >= 12.0 {
        1.0
    } else {
        0.0
    };
    let base_pad = 2.0 + stroke_px.ceil() + bold_extra;
    let pad_left = base_pad
        + if text.background {
            (font_size * 0.55).round()
        } else {
            shadow_blur as f32
        };
    let pad_right = pad_left;
    let pad_top = base_pad
        + if text.background {
            (font_size * 0.28).round()
        } else {
            shadow_blur as f32
        };
    let pad_bottom = if text.background {
        pad_top + shadow_dy as f32 * 0.5
    } else {
        base_pad + (shadow_blur + shadow_dy) as f32
    };

    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for glyph in glyphs {
        if glyph.width == 0 || glyph.height == 0 {
            continue;
        }
        min_x = min_x.min(glyph.x);
        min_y = min_y.min(glyph.y);
        max_x = max_x.max(glyph.x + glyph.width as f32);
        max_y = max_y.max(glyph.y + glyph.height as f32);
    }
    if !min_x.is_finite() {
        min_x = 0.0;
        min_y = 0.0;
        max_x = font_size * 0.5;
        max_y = layout.height().max(font_size);
    }
    min_x = min_x.floor() - pad_left;
    min_y = min_y.floor() - pad_top;
    max_x = max_x.ceil() + pad_right;
    max_y = max_y.ceil() + pad_bottom;
    let width = (max_x - min_x).ceil().clamp(1.0, MAX_IMAGE_SIDE as f32) as u32;
    let height = (max_y - min_y).ceil().clamp(1.0, MAX_IMAGE_SIDE as f32) as u32;

    let total = width as usize * height as usize;
    let mut glyph_mask = vec![0u8; total];
    let w_usize = width as usize;
    let h_usize = height as usize;
    for glyph in glyphs {
        if glyph.width == 0 || glyph.height == 0 {
            continue;
        }
        let (_, coverage) = fonts[glyph.font_index].rasterize_config(glyph.key);
        let left = (glyph.x - min_x).round().max(0.0) as i32;
        let top = (glyph.y - min_y).round().max(0.0) as i32;
        for (row, values) in coverage.chunks(glyph.width.max(1)).enumerate() {
            for (column, coverage) in values.iter().copied().enumerate() {
                if coverage == 0 {
                    continue;
                }
                let x = left + column as i32;
                let y = top + row as i32;
                if x >= 0 && y >= 0 && (x as usize) < w_usize && (y as usize) < h_usize {
                    let idx = y as usize * w_usize + x as usize;
                    glyph_mask[idx] = glyph_mask[idx].max(coverage);
                }
                if text.bold && font_size >= 12.0 {
                    let x2 = x + 1;
                    if x2 >= 0 && y >= 0 && (x2 as usize) < w_usize && (y as usize) < h_usize {
                        let idx2 = y as usize * w_usize + x2 as usize;
                        let thickened = (f32::from(coverage) * 0.65).round() as u8;
                        glyph_mask[idx2] = glyph_mask[idx2].max(thickened);
                    }
                }
            }
        }
    }

    let stroke_mask = if stroke_px > 0.01 {
        dilate_mask(&glyph_mask, w_usize, h_usize, stroke_px)
    } else {
        Vec::new()
    };

    let shadow_mask = if text.shadow {
        let mut silhouette = glyph_mask.clone();
        if !stroke_mask.is_empty() {
            for (dst, src) in silhouette.iter_mut().zip(&stroke_mask) {
                *dst = (*dst).max(*src);
            }
        }
        let blurred = box_blur_mask(&silhouette, w_usize, h_usize, shadow_blur);
        if shadow_dy > 0 {
            let mut shifted = vec![0u8; total];
            for y in shadow_dy..h_usize {
                let src_row = (y - shadow_dy) * w_usize;
                let dst_row = y * w_usize;
                shifted[dst_row..dst_row + w_usize]
                    .copy_from_slice(&blurred[src_row..src_row + w_usize]);
            }
            shifted
        } else {
            blurred
        }
    } else {
        Vec::new()
    };

    let mut image = RgbaImage::new(width, height);
    let opacity = opacity.clamp(0.0, 1.0);
    let bg_opacity = if text.background_opacity.is_finite() {
        text.background_opacity.clamp(0.0, 1.0)
    } else {
        0.55
    };
    let pill_r = (font_size * 0.35).min(width.min(height) as f32 * 0.5);
    let raw = image.as_mut();

    for y in 0..h_usize {
        for x in 0..w_usize {
            let idx = y * w_usize + x;
            let dst = &mut raw[idx * 4..idx * 4 + 4];
            let dst_arr: &mut [u8; 4] = dst.try_into().expect("4-byte pixel slice");

            if text.background {
                let cov = rounded_rect_coverage(
                    x as f32 + 0.5,
                    y as f32 + 0.5,
                    width as f32,
                    height as f32,
                    pill_r,
                );
                let bg_alpha = (255.0 * bg_opacity * opacity * cov)
                    .round()
                    .clamp(0.0, 255.0) as u8;
                if bg_alpha > 0 {
                    blend_straight_rgba_pixel(
                        dst_arr,
                        [
                            text.background_color[0],
                            text.background_color[1],
                            text.background_color[2],
                            bg_alpha,
                        ],
                    );
                }
            }

            if !shadow_mask.is_empty() {
                let s_alpha = (f32::from(shadow_mask[idx]) * 0.55 * opacity)
                    .round()
                    .clamp(0.0, 255.0) as u8;
                if s_alpha > 0 {
                    blend_straight_rgba_pixel(dst_arr, [0, 0, 0, s_alpha]);
                }
            }

            if !stroke_mask.is_empty() {
                let st_alpha = (f32::from(stroke_mask[idx]) * opacity)
                    .round()
                    .clamp(0.0, 255.0) as u8;
                if st_alpha > 0 {
                    blend_straight_rgba_pixel(
                        dst_arr,
                        [
                            text.stroke_color[0],
                            text.stroke_color[1],
                            text.stroke_color[2],
                            st_alpha,
                        ],
                    );
                }
            }

            let fg_alpha = (f32::from(glyph_mask[idx]) * opacity)
                .round()
                .clamp(0.0, 255.0) as u8;
            if fg_alpha > 0 {
                blend_straight_rgba_pixel(
                    dst_arr,
                    [text.color[0], text.color[1], text.color[2], fg_alpha],
                );
            }
        }
    }
    Ok(image)
}

fn dilate_mask(mask: &[u8], width: usize, height: usize, radius: f32) -> Vec<u8> {
    let r = radius.ceil() as i32;
    let mut offsets = Vec::new();
    for dy in -r..=r {
        for dx in -r..=r {
            let dist = ((dx * dx + dy * dy) as f32).sqrt();
            let weight = (radius + 0.5 - dist).clamp(0.0, 1.0);
            if weight > 0.01 {
                offsets.push((dx, dy, weight));
            }
        }
    }
    let mut out = vec![0u8; mask.len()];
    for y in 0..height {
        for x in 0..width {
            let val = mask[y * width + x];
            if val == 0 {
                continue;
            }
            for &(dx, dy, weight) in &offsets {
                let nx = x as i32 + dx;
                let ny = y as i32 + dy;
                if nx >= 0 && ny >= 0 && (nx as usize) < width && (ny as usize) < height {
                    let dst_idx = ny as usize * width + nx as usize;
                    let candidate = if weight >= 0.999 {
                        val
                    } else {
                        (f32::from(val) * weight).round() as u8
                    };
                    out[dst_idx] = out[dst_idx].max(candidate);
                }
            }
        }
    }
    out
}

fn box_blur_mask(mask: &[u8], width: usize, height: usize, radius: usize) -> Vec<u8> {
    if radius == 0 || width == 0 || height == 0 {
        return mask.to_vec();
    }
    let mut horiz = vec![0u16; mask.len()];
    let r = radius as i32;
    let window = (2 * radius + 1) as u32;
    for y in 0..height {
        let row = y * width;
        for x in 0..width {
            let mut sum = 0u32;
            for dx in -r..=r {
                let sx = (x as i32 + dx).clamp(0, width as i32 - 1) as usize;
                sum += u32::from(mask[row + sx]);
            }
            horiz[row + x] = (sum / window) as u16;
        }
    }
    let mut out = vec![0u8; mask.len()];
    for y in 0..height {
        for x in 0..width {
            let mut sum = 0u32;
            for dy in -r..=r {
                let sy = (y as i32 + dy).clamp(0, height as i32 - 1) as usize;
                sum += u32::from(horiz[sy * width + x]);
            }
            out[y * width + x] = (sum / window).min(255) as u8;
        }
    }
    out
}

fn bold_font_chain() -> &'static Vec<Font> {
    static BOLD_FONTS: OnceLock<Vec<Font>> = OnceLock::new();
    BOLD_FONTS.get_or_init(|| {
        #[cfg(target_os = "macos")]
        let paths = [
            "/System/Library/Fonts/STHeiti Medium.ttc",
            "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
            "/System/Library/Fonts/PingFang.ttc",
            "/System/Library/Fonts/Helvetica.ttc",
        ];
        #[cfg(target_os = "windows")]
        let paths = [
            r"C:\Windows\Fonts\msyhbd.ttc",
            r"C:\Windows\Fonts\simhei.ttf",
            r"C:\Windows\Fonts\arialbd.ttf",
            r"C:\Windows\Fonts\segoeuib.ttf",
        ];
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let paths = [
            "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/truetype/liberation2/LiberationSans-Bold.ttf",
        ];
        let loaded = load_fonts_from_paths(&paths);
        if loaded.is_empty() {
            font_chain().clone()
        } else {
            loaded
        }
    })
}

fn load_fonts_from_paths(paths: &[&str]) -> Vec<Font> {
    let mut fonts = Vec::new();
    for path in paths {
        match fs::read(path) {
            Ok(bytes) => match Font::from_bytes(bytes, fontdue::FontSettings::default()) {
                Ok(font) => {
                    log::debug!("watermark font loaded: {path}");
                    fonts.push(font);
                }
                Err(err) => log::warn!("watermark font {path} is invalid: {err}"),
            },
            Err(err) => log::debug!("watermark font {path} unavailable: {err}"),
        }
    }
    fonts
}

fn font_index_for(fonts: &[Font], character: char) -> usize {
    fonts
        .iter()
        .position(|font| font.has_glyph(character) || character.is_whitespace())
        .unwrap_or(0)
}

fn font_chain() -> &'static Vec<Font> {
    static FONTS: OnceLock<Vec<Font>> = OnceLock::new();
    FONTS.get_or_init(|| {
        #[cfg(target_os = "macos")]
        let paths = [
            "/System/Library/Fonts/PingFang.ttc",
            "/System/Library/Fonts/STHeiti Medium.ttc",
            "/System/Library/Fonts/Helvetica.ttc",
            "/System/Library/Fonts/Supplemental/Arial.ttf",
        ];
        #[cfg(target_os = "windows")]
        let paths = [
            r"C:\Windows\Fonts\msyh.ttc",
            r"C:\Windows\Fonts\simhei.ttf",
            r"C:\Windows\Fonts\arial.ttf",
            r"C:\Windows\Fonts\segoeui.ttf",
        ];
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let paths = [
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/liberation2/LiberationSans-Regular.ttf",
        ];
        let mut fonts = Vec::new();
        for path in paths {
            match fs::read(path) {
                Ok(bytes) => match Font::from_bytes(bytes, fontdue::FontSettings::default()) {
                    Ok(font) => {
                        log::debug!("watermark font loaded: {path}");
                        fonts.push(font);
                    }
                    Err(err) => log::warn!("watermark font {path} is invalid: {err}"),
                },
                Err(err) => log::debug!("watermark font {path} unavailable: {err}"),
            }
        }
        fonts
    })
}

fn rotate_rgba(image: &RgbaImage, degrees: f32) -> Result<RgbaImage> {
    rotate_rgba_with_offset(image, degrees).map(|(rotated, _, _)| rotated)
}

fn rotate_rgba_with_offset(image: &RgbaImage, degrees: f32) -> Result<(RgbaImage, f32, f32)> {
    let mut premultiplied = image.as_raw().clone();
    for pixel in premultiplied.as_chunks_mut::<4>().0 {
        let alpha = pixel[3];
        for channel in &mut pixel[..3] {
            *channel = ((u32::from(*channel) * u32::from(alpha) + 127) / 255) as u8;
        }
    }
    let source = Pixmap::from_vec(
        premultiplied,
        tiny_skia::IntSize::from_wh(image.width(), image.height())
            .context("watermark tile is too large to rotate")?,
    )
    .context("failed to prepare watermark rotation")?;
    let transform = Transform::from_rotate(degrees);
    let map = |x: f32, y: f32| {
        let mut point = tiny_skia::Point::from_xy(x, y);
        transform.map_point(&mut point);
        point
    };
    let corners = [
        map(0.0, 0.0),
        map(image.width() as f32, 0.0),
        map(0.0, image.height() as f32),
        map(image.width() as f32, image.height() as f32),
    ];
    let min_x = corners
        .iter()
        .map(|point| point.x)
        .fold(f32::INFINITY, f32::min);
    let min_y = corners
        .iter()
        .map(|point| point.y)
        .fold(f32::INFINITY, f32::min);
    let max_x = corners
        .iter()
        .map(|point| point.x)
        .fold(f32::NEG_INFINITY, f32::max);
    let max_y = corners
        .iter()
        .map(|point| point.y)
        .fold(f32::NEG_INFINITY, f32::max);
    let width = (max_x - min_x).ceil().max(1.0) as u32;
    let height = (max_y - min_y).ceil().max(1.0) as u32;
    ensure!(
        width <= MAX_IMAGE_SIDE && height <= MAX_IMAGE_SIDE,
        "rotated watermark tile is too large"
    );
    let mut rotated = Pixmap::new(width, height).context("failed to allocate rotated watermark")?;
    let draw = Transform::from_translate(-min_x, -min_y).pre_concat(transform);
    rotated
        .as_mut()
        .draw_pixmap(0, 0, source.as_ref(), &PixmapPaint::default(), draw, None);
    let raw = rotated.take_demultiplied();
    let image =
        RgbaImage::from_raw(width, height, raw).context("invalid rotated watermark buffer")?;
    Ok((image, min_x, min_y))
}

#[allow(clippy::too_many_arguments)]
fn layout_tiles(
    base: &RgbaImage,
    canvas_width: u32,
    canvas_height: u32,
    scale: f32,
    placement: WatermarkPlacement,
    margin_x: f32,
    margin_y: f32,
    tiled: bool,
    spacing_x: f32,
    spacing_y: f32,
) -> Result<RasterLayer> {
    let tile_width = base.width();
    let tile_height = base.height();
    let margin_x = margin_x.max(0.0) * scale;
    let margin_y = margin_y.max(0.0) * scale;
    let max_x = canvas_width.saturating_sub(tile_width) as f32;
    let max_y = canvas_height.saturating_sub(tile_height) as f32;
    let anchor = |extent: f32, size: f32, margin: f32| match placement {
        WatermarkPlacement::TopLeft
        | WatermarkPlacement::MiddleLeft
        | WatermarkPlacement::BottomLeft => margin,
        WatermarkPlacement::TopCenter
        | WatermarkPlacement::Center
        | WatermarkPlacement::BottomCenter => (extent - size) * 0.5,
        WatermarkPlacement::TopRight
        | WatermarkPlacement::MiddleRight
        | WatermarkPlacement::BottomRight => extent - size - margin,
    };
    let anchor_y = |extent: f32, size: f32, margin: f32| match placement {
        WatermarkPlacement::TopLeft
        | WatermarkPlacement::TopCenter
        | WatermarkPlacement::TopRight => margin,
        WatermarkPlacement::MiddleLeft
        | WatermarkPlacement::Center
        | WatermarkPlacement::MiddleRight => (extent - size) * 0.5,
        WatermarkPlacement::BottomLeft
        | WatermarkPlacement::BottomCenter
        | WatermarkPlacement::BottomRight => extent - size - margin,
    };
    let mut positions = Vec::new();
    if tiled {
        let step_x = tile_width as f32 + spacing_x.max(0.0) * scale;
        let step_y = tile_height as f32 + spacing_y.max(0.0) * scale;
        ensure!(
            step_x >= 1.0 && step_y >= 1.0,
            "watermark tile spacing is invalid"
        );
        let columns = if margin_x >= canvas_width as f32 {
            0
        } else if tile_width as f32 >= canvas_width as f32 - margin_x {
            1
        } else {
            (((canvas_width as f32 - margin_x - tile_width as f32) / step_x).floor() as usize)
                .saturating_add(1)
        };
        let rows = if margin_y >= canvas_height as f32 {
            0
        } else if tile_height as f32 >= canvas_height as f32 - margin_y {
            1
        } else {
            (((canvas_height as f32 - margin_y - tile_height as f32) / step_y).floor() as usize)
                .saturating_add(1)
        };
        ensure!(
            columns.saturating_mul(rows) <= MAX_TILES,
            "watermark tiling would create too many tiles"
        );
        for row in 0..rows {
            for column in 0..columns {
                positions.push((
                    margin_x + column as f32 * step_x,
                    margin_y + row as f32 * step_y,
                ));
            }
        }
    } else {
        positions.push((
            anchor(canvas_width as f32, tile_width as f32, margin_x).clamp(0.0, max_x),
            anchor_y(canvas_height as f32, tile_height as f32, margin_y).clamp(0.0, max_y),
        ));
    }
    let mut layer = RasterLayer::default();
    for (x, y) in positions {
        if let Some(tile) = clip_tile(
            base,
            x.round() as i64,
            y.round() as i64,
            canvas_width,
            canvas_height,
        ) {
            layer.tiles.push(tile);
        }
    }
    Ok(layer)
}

fn clip_tile(
    base: &RgbaImage,
    x: i64,
    y: i64,
    canvas_width: u32,
    canvas_height: u32,
) -> Option<RasterTile> {
    let left = x.max(0).min(canvas_width as i64) as u32;
    let top = y.max(0).min(canvas_height as i64) as u32;
    let right = (x + base.width() as i64).max(0).min(canvas_width as i64) as u32;
    let bottom = (y + base.height() as i64).max(0).min(canvas_height as i64) as u32;
    if right <= left || bottom <= top {
        return None;
    }
    let source_x = (left as i64 - x) as u32;
    let source_y = (top as i64 - y) as u32;
    let width = right - left;
    let height = bottom - top;
    let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
    for row in 0..height {
        let start = ((source_y + row) as usize * base.width() as usize + source_x as usize) * 4;
        pixels.extend_from_slice(&base.as_raw()[start..start + width as usize * 4]);
    }
    Some(RasterTile {
        bounds: PixelRect {
            x: left,
            y: top,
            width,
            height,
        },
        pixels: pixels
            .as_chunks::<4>()
            .0
            .iter()
            .map(|pixel| [pixel[0], pixel[1], pixel[2], pixel[3]])
            .collect(),
    })
}

fn srgb_to_linear_table() -> [f32; 256] {
    let mut table = [0.0; 256];
    for (value, slot) in table.iter_mut().enumerate() {
        let value = value as f32 / 255.0;
        *slot = if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        };
    }
    table
}

pub fn f16_bits_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits & 0x8000) << 16;
    let exponent = (bits >> 10) & 0x1f;
    let mantissa = u32::from(bits & 0x03ff);
    f32::from_bits(match exponent {
        0 if mantissa == 0 => sign,
        0 => {
            let mut exponent = 127 - 15 + 1;
            let mut mantissa = mantissa;
            while mantissa & 0x400 == 0 {
                mantissa <<= 1;
                exponent -= 1;
            }
            sign | (exponent << 23) | ((mantissa & 0x3ff) << 13)
        }
        31 => sign | 0x7f80_0000 | (mantissa << 13),
        _ => sign | ((u32::from(exponent) + 112) << 23) | (mantissa << 13),
    })
}

pub fn f32_to_f16_bits(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32;
    let mantissa = bits & 0x007f_ffff;
    if exponent == 255 {
        return if mantissa == 0 { sign | 0x7bff } else { 0 };
    }
    let half_exponent = exponent - 127 + 15;
    if half_exponent <= 0 {
        if half_exponent < -10 {
            return sign;
        }
        let mantissa = mantissa | 0x0080_0000;
        let shift = (14 - half_exponent) as u32;
        let truncated = mantissa >> shift;
        let remainder = mantissa & ((1u32 << shift) - 1);
        let halfway = 1u32 << (shift - 1);
        let rounded = truncated
            + u32::from(remainder > halfway || (remainder == halfway && truncated & 1 == 1));
        return sign | rounded as u16;
    }
    if half_exponent >= 31 {
        return sign | 0x7bff;
    }
    let mut rounded = sign | ((half_exponent as u16) << 10) | ((mantissa >> 13) as u16);
    let remainder = mantissa & 0x1fff;
    if remainder > 0x1000 || (remainder == 0x1000 && rounded & 1 == 1) {
        rounded += 1;
        if rounded & 0x7c00 == 0x7c00 {
            rounded = sign | 0x7bff;
        }
    }
    rounded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_conversion_rounds_subnormals_and_saturates() {
        assert_eq!(f32_to_f16_bits(0.0), 0);
        assert_eq!(f32_to_f16_bits(-0.0), 0x8000);
        assert_eq!(f32_to_f16_bits(1.0), 0x3c00);
        assert_eq!(f32_to_f16_bits(f32::from_bits(0x3f80_1000)), 0x3c00);
        assert_eq!(f32_to_f16_bits(f32::from_bits(0x3f80_1001)), 0x3c01);
        assert_eq!(f32_to_f16_bits(f32::from_bits(0x3380_0000)), 1);
        assert_eq!(f32_to_f16_bits(65504.0), 0x7bff);
        assert_eq!(f32_to_f16_bits(1.0e10), 0x7bff);
        assert_eq!(f32_to_f16_bits(-1.0e10), 0xfbff);
        assert_eq!(f32_to_f16_bits(f32::INFINITY), 0x7bff);
        assert_eq!(f32_to_f16_bits(f32::NEG_INFINITY), 0xfbff);
        assert_eq!(f32_to_f16_bits(f32::NAN), 0);
        for bits in [0u16, 1, 0x3c00, 0x7bff, 0x8000, 0xfbff] {
            assert_eq!(f32_to_f16_bits(f16_bits_to_f32(bits)), bits);
        }
    }

    fn sample_text(content: &str) -> TextWatermark {
        TextWatermark {
            content: content.to_string(),
            font_size: 32.0,
            color: [250, 204, 21],
            bold: false,
            shadow: false,
            stroke_width: 0.0,
            stroke_color: [0, 0, 0],
            background: false,
            background_color: [15, 23, 42],
            background_opacity: 0.55,
        }
    }

    #[test]
    fn text_watermark_raster_has_visible_colored_pixels() {
        let spec = WatermarkSpec {
            elements: vec![WatermarkElement {
                kind: WatermarkElementKind::Text(sample_text("SnapIt")),
                x: 0.0,
                y: 0.0,
                rotation_degrees: 0.0,
                scale: 1.0,
                opacity: 1.0,
            }],
            placement: WatermarkPlacement::BottomRight,
            ..WatermarkSpec::default()
        };
        let layer = spec.rasterize(320, 120, 1.0).unwrap();
        assert_eq!(layer.tiles.len(), 1);
        assert!(layer.tiles[0].pixels.iter().any(|pixel| pixel[3] == 255));
        assert!(
            layer.tiles[0]
                .pixels
                .iter()
                .any(|pixel| pixel[3] > 0 && pixel[..3] == [250, 204, 21])
        );
    }

    #[test]
    fn multi_element_stamp_composites_texts_and_images_in_z_order() {
        let red_image = Arc::new(RgbaImage::from_pixel(10, 10, image::Rgba([255, 0, 0, 255])));
        let blue_image = Arc::new(RgbaImage::from_pixel(10, 10, image::Rgba([0, 0, 255, 255])));
        let spec = WatermarkSpec {
            elements: vec![
                WatermarkElement {
                    kind: WatermarkElementKind::Image(ImageWatermark {
                        image: red_image,
                        width: 10.0,
                        corner_radius: 0.0,
                    }),
                    x: 0.0,
                    y: 0.0,
                    rotation_degrees: 0.0,
                    scale: 1.0,
                    opacity: 1.0,
                },
                WatermarkElement {
                    kind: WatermarkElementKind::Image(ImageWatermark {
                        image: blue_image,
                        width: 10.0,
                        corner_radius: 0.0,
                    }),
                    x: 4.0,
                    y: 4.0,
                    rotation_degrees: 0.0,
                    scale: 1.0,
                    opacity: 1.0,
                },
                WatermarkElement {
                    kind: WatermarkElementKind::Text(TextWatermark {
                        font_size: 16.0,
                        color: [0, 255, 0],
                        ..sample_text("A")
                    }),
                    x: 20.0,
                    y: 0.0,
                    rotation_degrees: 0.0,
                    scale: 1.0,
                    opacity: 1.0,
                },
                WatermarkElement {
                    kind: WatermarkElementKind::Text(TextWatermark {
                        font_size: 16.0,
                        color: [255, 255, 0],
                        ..sample_text("B")
                    }),
                    x: 20.0,
                    y: 22.0,
                    rotation_degrees: 0.0,
                    scale: 1.0,
                    opacity: 1.0,
                },
            ],
            ..WatermarkSpec::default()
        };
        let stamp = spec.compose_stamp(1.0).unwrap();
        assert_eq!(stamp.rendered_element_bounds.len(), 4);
        assert_eq!(stamp.image.get_pixel(1, 1).0, [255, 0, 0, 255]);
        assert_eq!(stamp.image.get_pixel(6, 6).0, [0, 0, 255, 255]);
        assert!(stamp.rendered_element_bounds[3].y > stamp.rendered_element_bounds[2].y);

        let mut rotated_spec = spec.clone();
        rotated_spec.rotation_degrees = 30.0;
        let rotated_stamp = rotated_spec.compose_stamp(1.0).unwrap();
        assert_eq!(rotated_stamp.rendered_element_bounds.len(), 4);
        for bounds in &rotated_stamp.rendered_element_bounds {
            assert!(bounds.x >= -0.5);
            assert!(bounds.y >= -0.5);
            assert!(bounds.x + bounds.width <= rotated_stamp.image.width() as f32 + 0.5);
            assert!(bounds.y + bounds.height <= rotated_stamp.image.height() as f32 + 0.5);
        }
    }

    #[test]
    fn text_effects_and_multiline_and_image_corners_render_cleanly() {
        let single = render_text(&sample_text("A"), 1.0, 1.0).unwrap();
        let multi = render_text(&sample_text("A\nB"), 1.0, 1.0).unwrap();
        assert!(multi.height() > single.height());

        let shadowed = render_text(
            &TextWatermark {
                shadow: true,
                color: [255, 255, 255],
                ..sample_text("S")
            },
            1.0,
            1.0,
        )
        .unwrap();
        assert!(
            shadowed
                .pixels()
                .any(|p| p[3] > 0 && p[0] < 32 && p[1] < 32 && p[2] < 32)
        );

        let stroked = render_text(
            &TextWatermark {
                stroke_width: 2.0,
                stroke_color: [255, 0, 0],
                color: [255, 255, 255],
                ..sample_text("O")
            },
            1.0,
            1.0,
        )
        .unwrap();
        assert!(
            stroked
                .pixels()
                .any(|p| p[3] > 128 && p[0] > 200 && p[1] < 40 && p[2] < 40)
        );

        let pill = render_text(
            &TextWatermark {
                background: true,
                background_color: [10, 20, 30],
                background_opacity: 0.8,
                color: [255, 255, 255],
                ..sample_text("Badge")
            },
            1.0,
            1.0,
        )
        .unwrap();
        assert_eq!(pill.get_pixel(0, 0)[3], 0);
        assert!(pill.get_pixel(pill.width() / 2, 2)[3] > 150);

        let square = Arc::new(RgbaImage::from_pixel(
            40,
            40,
            image::Rgba([255, 255, 255, 255]),
        ));
        let circle = render_image(
            &ImageWatermark {
                image: square,
                width: 40.0,
                corner_radius: 50.0,
            },
            1.0,
            1.0,
        )
        .unwrap();
        assert_eq!(circle.get_pixel(0, 0)[3], 0);
        assert_eq!(circle.get_pixel(20, 20).0, [255, 255, 255, 255]);
    }

    #[test]
    fn oversized_element_separation_returns_error() {
        let img = Arc::new(RgbaImage::from_pixel(
            4,
            4,
            image::Rgba([255, 255, 255, 255]),
        ));
        let spec = WatermarkSpec {
            elements: vec![
                WatermarkElement {
                    kind: WatermarkElementKind::Image(ImageWatermark {
                        image: img.clone(),
                        width: 4.0,
                        corner_radius: 0.0,
                    }),
                    x: 0.0,
                    y: 0.0,
                    rotation_degrees: 0.0,
                    scale: 1.0,
                    opacity: 1.0,
                },
                WatermarkElement {
                    kind: WatermarkElementKind::Image(ImageWatermark {
                        image: img,
                        width: 4.0,
                        corner_radius: 0.0,
                    }),
                    x: 10_000.0,
                    y: 0.0,
                    rotation_degrees: 0.0,
                    scale: 1.0,
                    opacity: 1.0,
                },
            ],
            ..WatermarkSpec::default()
        };
        assert!(spec.compose_stamp(1.0).is_err());
    }

    #[test]
    fn placement_margins_and_clipping_are_pixel_exact() {
        let base = RgbaImage::from_raw(2, 2, vec![255; 16]).unwrap();
        let layer = layout_tiles(
            &base,
            10,
            8,
            1.0,
            WatermarkPlacement::BottomRight,
            1.0,
            2.0,
            false,
            10.0,
            10.0,
        )
        .unwrap();
        assert_eq!(layer.tiles.len(), 1);
        assert_eq!(
            layer.tiles[0].bounds,
            PixelRect {
                x: 7,
                y: 4,
                width: 2,
                height: 2
            }
        );
        let oversized = RgbaImage::new(20, 20);
        let layer = layout_tiles(
            &oversized,
            10,
            8,
            1.0,
            WatermarkPlacement::Center,
            0.0,
            0.0,
            false,
            10.0,
            10.0,
        )
        .unwrap();
        assert_eq!(
            layer.tiles[0].bounds,
            PixelRect {
                x: 0,
                y: 0,
                width: 10,
                height: 8
            }
        );
    }

    #[test]
    fn tiling_respects_spacing() {
        let base = RgbaImage::from_raw(2, 2, vec![255; 16]).unwrap();
        let layer = layout_tiles(
            &base,
            8,
            6,
            1.0,
            WatermarkPlacement::TopLeft,
            0.0,
            0.0,
            true,
            2.0,
            1.0,
        )
        .unwrap();
        assert_eq!(layer.tiles.len(), 4);
        assert_eq!(
            layer.tiles[3].bounds,
            PixelRect {
                x: 4,
                y: 3,
                width: 2,
                height: 2
            }
        );
    }

    #[test]
    fn straight_alpha_is_applied_once() {
        let mut tile = RgbaImage::from_raw(1, 1, vec![255, 0, 0, 128]).unwrap();
        for pixel in tile.pixels_mut() {
            pixel[3] = 128;
        }
        let layer = layout_tiles(
            &tile,
            1,
            1,
            1.0,
            WatermarkPlacement::TopLeft,
            0.0,
            0.0,
            false,
            1.0,
            1.0,
        )
        .unwrap();
        let mut image = RgbaImage::from_raw(1, 1, vec![0, 0, 0, 255]).unwrap();
        layer.apply_sdr_mut(&mut image);
        assert_eq!(image.get_pixel(0, 0).0, [128, 0, 0, 255]);
    }

    #[test]
    fn translucent_watermarks_keep_straight_alpha_on_transparent_pixels() {
        let tile = RgbaImage::from_raw(1, 1, vec![200, 100, 50, 128]).unwrap();
        let layer = layout_tiles(
            &tile,
            1,
            1,
            1.0,
            WatermarkPlacement::TopLeft,
            0.0,
            0.0,
            false,
            1.0,
            1.0,
        )
        .unwrap();
        let mut transparent = RgbaImage::from_raw(1, 1, vec![0, 0, 0, 0]).unwrap();
        layer.apply_sdr_mut(&mut transparent);
        assert_eq!(transparent.get_pixel(0, 0).0, [200, 100, 50, 128]);

        let mut opaque = RgbaImage::from_raw(1, 1, vec![0, 0, 0, 255]).unwrap();
        layer.apply_sdr_mut(&mut opaque);
        assert_eq!(opaque.get_pixel(0, 0).0, [100, 50, 25, 255]);
    }

    #[test]
    fn an_oversized_tiled_watermark_is_clipped_instead_of_rejected() {
        let base = RgbaImage::from_pixel(20, 20, image::Rgba([10, 20, 30, 255]));
        let layer = layout_tiles(
            &base,
            10,
            8,
            1.0,
            WatermarkPlacement::TopLeft,
            0.0,
            0.0,
            true,
            1.0,
            1.0,
        )
        .unwrap();
        assert_eq!(layer.tiles.len(), 1);
        assert_eq!(
            layer.tiles[0].bounds,
            PixelRect {
                x: 0,
                y: 0,
                width: 10,
                height: 8
            }
        );
    }

    #[test]
    fn hdr_blend_preserves_uncovered_samples_and_white_point() {
        let mut raw = vec![0u8; 6 * 4];
        for channel in 0..3 {
            let at = channel * 2;
            raw[at..at + 2].copy_from_slice(&f32_to_f16_bits(4.0).to_le_bytes());
            raw[6 + at..8 + at].copy_from_slice(&f32_to_f16_bits(1.0).to_le_bytes());
        }
        let image = HdrImage::new(2, 2, raw, 80.0).with_sdr_white(200.0);
        let mut base = RgbaImage::new(1, 1);
        base.put_pixel(0, 0, image::Rgba([255, 255, 255, 128]));
        let layer = layout_tiles(
            &base,
            2,
            2,
            1.0,
            WatermarkPlacement::TopLeft,
            0.0,
            0.0,
            false,
            1.0,
            1.0,
        )
        .unwrap();
        let result = layer.apply_hdr_owned(image);
        assert_eq!(result.reference_white_nits(), 80.0);
        assert_eq!(result.sdr_white_nits(), 200.0);
        let value = f16_bits_to_f32(u16::from_le_bytes([
            result.as_bytes()[0],
            result.as_bytes()[1],
        ]));
        assert!((value - 3.25).abs() < 0.01, "{value}");
        assert_eq!(&result.as_bytes()[6..12], &[0, 60, 0, 60, 0, 60]);
        assert!(result.as_bytes()[12..].iter().all(|byte| *byte == 0));
    }
}
