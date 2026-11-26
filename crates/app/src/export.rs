use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak},
};

use anyhow::{Result, ensure};
use snapit_capture::{
    HdrImage, ImageSource, ImageWatermark, RgbaImage, TextWatermark, WatermarkElement,
    WatermarkElementKind, WatermarkPlacement, WatermarkSpec, load_watermark_image,
};
use snapit_store::{
    WatermarkKind as ConfigKind, WatermarkPlacement as ConfigPlacement, WatermarkPreset,
};

use crate::annotate::{Shape, bake, bake_hdr};

pub fn expand_watermark_variables_at(
    template: &str,
    now: chrono::DateTime<chrono::Local>,
) -> String {
    if !template.contains('{') {
        return template.to_string();
    }
    template
        .replace("{datetime}", &now.format("%Y-%m-%d %H:%M").to_string())
        .replace("{date}", &now.format("%Y-%m-%d").to_string())
        .replace("{time}", &now.format("%H:%M").to_string())
        .replace("{year}", &now.format("%Y").to_string())
}

fn rgb_channels(color: u32) -> [u8; 3] {
    [(color >> 16) as u8, (color >> 8) as u8, color as u8]
}

pub fn watermark_spec(config: &WatermarkPreset, image_dir: &Path) -> Result<WatermarkSpec> {
    let now = chrono::Local::now();
    let mut loaded_images: HashMap<&str, Arc<RgbaImage>> = HashMap::new();
    let mut elements = Vec::with_capacity(config.elements.len());
    for element in &config.elements {
        let kind = match element.kind {
            ConfigKind::Text => WatermarkElementKind::Text(TextWatermark {
                content: expand_watermark_variables_at(&element.text.content, now),
                font_size: element.text.font_size,
                color: rgb_channels(element.text.color),
                bold: element.text.bold,
                shadow: element.text.shadow,
                stroke_width: element.text.stroke_width,
                stroke_color: rgb_channels(element.text.stroke_color),
                background: element.text.background,
                background_color: rgb_channels(element.text.background_color),
                background_opacity: element.text.background_opacity,
            }),
            ConfigKind::Image => {
                let source_name = element.image.source.as_str();
                let source = std::path::Path::new(source_name);
                ensure!(
                    !source_name.is_empty()
                        && source.file_name().and_then(|name| name.to_str()) == Some(source_name),
                    "watermark image source must be a managed file name"
                );
                let image = if let Some(cached) = loaded_images.get(source_name) {
                    cached.clone()
                } else {
                    let loaded = load_watermark_image(&image_dir.join(source))?;
                    loaded_images.insert(source_name, loaded.clone());
                    loaded
                };
                WatermarkElementKind::Image(ImageWatermark {
                    image,
                    width: element.image.width,
                    corner_radius: element.image.corner_radius,
                })
            }
        };
        elements.push(WatermarkElement {
            kind,
            x: element.x,
            y: element.y,
            rotation_degrees: element.rotation_degrees,
            scale: element.scale,
            opacity: element.opacity,
        });
    }
    Ok(WatermarkSpec {
        elements,
        placement: match config.placement {
            ConfigPlacement::TopLeft => WatermarkPlacement::TopLeft,
            ConfigPlacement::TopCenter => WatermarkPlacement::TopCenter,
            ConfigPlacement::TopRight => WatermarkPlacement::TopRight,
            ConfigPlacement::MiddleLeft => WatermarkPlacement::MiddleLeft,
            ConfigPlacement::Center => WatermarkPlacement::Center,
            ConfigPlacement::MiddleRight => WatermarkPlacement::MiddleRight,
            ConfigPlacement::BottomLeft => WatermarkPlacement::BottomLeft,
            ConfigPlacement::BottomCenter => WatermarkPlacement::BottomCenter,
            ConfigPlacement::BottomRight => WatermarkPlacement::BottomRight,
        },
        margin_x: config.margin_x,
        margin_y: config.margin_y,
        rotation_degrees: config.rotation_degrees,
        scale: config.scale,
        tiled: config.tiled,
        tile_spacing_x: config.tile_spacing_x,
        tile_spacing_y: config.tile_spacing_y,
    })
}

pub struct ExportComposition {
    clean_sdr: Arc<ImageSource>,
    hdr_source: Option<Arc<HdrImage>>,
    shapes: Arc<Vec<Shape>>,
    watermark: Arc<WatermarkSpec>,
    layout_scale: f32,
    result: OnceLock<std::result::Result<Arc<ExportResult>, String>>,
    result_lock: Mutex<()>,
    frozen_png: Option<Arc<Vec<u8>>>,
    frozen_hdr: bool,
}

pub enum ExportResult {
    Sdr(Arc<ImageSource>),
    Hdr(HdrExport),
}

pub struct HdrExport {
    image: Arc<HdrImage>,
    preview: Mutex<Weak<RgbaImage>>,
    png: OnceLock<std::result::Result<Arc<Vec<u8>>, String>>,
}

impl ExportComposition {
    pub fn new(
        clean_sdr: impl Into<ImageSource>,
        hdr: Option<Arc<HdrImage>>,
        shapes: Arc<Vec<Shape>>,
        watermark: Arc<WatermarkSpec>,
        layout_scale: f32,
    ) -> Self {
        Self {
            clean_sdr: Arc::new(clean_sdr.into()),
            hdr_source: hdr,
            shapes,
            watermark,
            layout_scale,
            result: OnceLock::new(),
            result_lock: Mutex::new(()),
            frozen_png: None,
            frozen_hdr: false,
        }
    }

    pub fn frozen(png: Arc<Vec<u8>>, preview: Arc<RgbaImage>, hdr: bool) -> Self {
        let result = Arc::new(ExportResult::Sdr(Arc::new(ImageSource::Memory(
            preview.clone(),
        ))));
        Self {
            clean_sdr: Arc::new(ImageSource::Memory(preview)),
            hdr_source: None,
            shapes: Arc::new(Vec::new()),
            watermark: Arc::new(WatermarkSpec::default()),
            layout_scale: 1.0,
            result: OnceLock::from(Ok::<_, String>(result)),
            result_lock: Mutex::new(()),
            frozen_png: Some(png),
            frozen_hdr: hdr,
        }
    }

    pub fn has_hdr_original(&self) -> bool {
        self.frozen_hdr || matches!(self.result().map(|result| result.is_hdr()), Ok(true))
    }

    pub fn dimensions(&self) -> (u32, u32) {
        self.hdr_source
            .as_ref()
            .map(|image| (image.width(), image.height()))
            .unwrap_or_else(|| self.clean_sdr.dimensions())
    }

    pub fn result(&self) -> std::result::Result<Arc<ExportResult>, String> {
        let _serial = self
            .result_lock
            .lock()
            .expect("export result lock poisoned");
        if let Some(result) = self.result.get() {
            return result.clone();
        }
        let started = std::time::Instant::now();
        let has_shapes = !self.shapes.is_empty();
        let has_watermark = self.watermark.is_enabled();
        let result = match self.hdr_source.as_ref() {
            Some(hdr) if has_shapes || has_watermark => {
                let image = bake_hdr((**hdr).clone(), &self.shapes, self.layout_scale)?;
                let layer = self
                    .watermark
                    .rasterize(image.width(), image.height(), self.layout_scale)
                    .map_err(|err| format!("{err:#}"))?;
                let image = Arc::new(layer.apply_hdr_owned(image));
                log::info!(
                    "export composition: HDR annotations={has_shapes} watermark={has_watermark} {:.1} ms",
                    started.elapsed().as_secs_f64() * 1000.0
                );
                ExportResult::Hdr(HdrExport {
                    image,
                    preview: Mutex::new(Weak::new()),
                    png: OnceLock::new(),
                })
            }
            Some(hdr) => ExportResult::Hdr(HdrExport {
                image: hdr.clone(),
                preview: Mutex::new(Weak::new()),
                png: OnceLock::new(),
            }),
            None => {
                let image = if self.clean_sdr.is_disk() && (has_shapes || has_watermark) {
                    let output = snapit_capture::DiskImage::create(
                        self.clean_sdr.width(),
                        self.clean_sdr.height(),
                    )
                    .map_err(|error| format!("{error:#}"))?;
                    let rows_per_batch = (8 * 1024 * 1024
                        / (self.clean_sdr.width() as usize * 4).max(1))
                    .clamp(1, 2048) as u32;
                    for start in (0..self.clean_sdr.height()).step_by(rows_per_batch as usize) {
                        let rows = rows_per_batch.min(self.clean_sdr.height() - start);
                        let strip = crate::annotate::bake_strip(
                            &self.clean_sdr,
                            start,
                            rows,
                            &self.shapes,
                            &self.watermark,
                            self.layout_scale,
                        )
                        .map_err(|error| format!("{error:#}"))?;
                        output
                            .write_rows_at(start, &strip)
                            .map_err(|error| format!("{error:#}"))?;
                    }
                    Arc::new(ImageSource::Disk(Arc::new(output)))
                } else if has_shapes || has_watermark {
                    let mut image = self
                        .clean_sdr
                        .memory()
                        .expect("disk composition handled above")
                        .as_ref()
                        .clone();
                    bake(&mut image, &self.shapes, self.layout_scale);
                    let layer = self
                        .watermark
                        .rasterize(image.width(), image.height(), self.layout_scale)
                        .map_err(|err| format!("{err:#}"))?;
                    {
                        layer.apply_sdr_mut(&mut image);
                        Arc::new(ImageSource::Memory(Arc::new(image)))
                    }
                } else {
                    self.clean_sdr.clone()
                };
                log::info!(
                    "export composition: SDR annotations={has_shapes} watermark={has_watermark} {:.1} ms",
                    started.elapsed().as_secs_f64() * 1000.0
                );
                ExportResult::Sdr(image)
            }
        };
        let result = Ok::<_, String>(Arc::new(result));
        let _ = self.result.set(result.clone());
        result
    }

    pub fn sdr_preview(&self) -> std::result::Result<Arc<RgbaImage>, String> {
        Ok(match self.result()?.as_ref() {
            ExportResult::Sdr(image) => match image.as_ref() {
                ImageSource::Memory(image) => image.clone(),
                ImageSource::Disk(image) => Arc::new(
                    image
                        .thumbnail(2048, 2048)
                        .map_err(|error| format!("{error:#}"))?,
                ),
            },
            ExportResult::Hdr(export) => export.sdr_preview(),
        })
    }

    pub fn write_png(&self, path: &std::path::Path) -> std::result::Result<(), String> {
        if let Some(png) = &self.frozen_png {
            return std::fs::write(path, png.as_slice()).map_err(|error| format!("{error:#}"));
        }
        match self.result()?.as_ref() {
            ExportResult::Sdr(image) => {
                image.encode_png(path).map_err(|error| format!("{error:#}"))
            }
            ExportResult::Hdr(export) => {
                std::fs::write(path, export.png()?.as_slice()).map_err(|error| format!("{error:#}"))
            }
        }
    }

    pub fn clipboard_uses_file(&self) -> bool {
        match self.result().ok().as_deref() {
            Some(ExportResult::Sdr(image)) => matches!(image.as_ref(), ImageSource::Disk(_)),
            _ => false,
        }
    }

    pub fn write_clipboard_png(&self, dir: &Path) -> std::result::Result<PathBuf, String> {
        std::fs::create_dir_all(dir).map_err(|error| format!("{error:#}"))?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for attempt in 0..100 {
            let path = dir.join(format!(
                "snapit-copy-{}-{nonce}-{attempt}.png",
                std::process::id()
            ));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => drop(file),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(format!("{error:#}")),
            }
            if let Err(error) = self.write_png(&path) {
                let _ = std::fs::remove_file(&path);
                return Err(error);
            }
            return Ok(path);
        }
        Err("failed to reserve a clipboard image file".to_string())
    }

    pub fn write_jpeg(&self, path: &Path, quality: u8) -> std::result::Result<(), String> {
        const MAX_JPEG_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
        let (width, height) = self.dimensions();
        let source_bytes = u64::from(width)
            .saturating_mul(u64::from(height))
            .saturating_mul(4);
        if source_bytes > MAX_JPEG_SOURCE_BYTES {
            return Err(
                "JPEG export of this image exceeds the bounded in-memory encoder budget; save as PNG instead"
                    .to_string(),
            );
        }
        match self.result()?.as_ref() {
            ExportResult::Sdr(image) => match image.as_ref() {
                ImageSource::Memory(image) => snapit_capture::save_jpeg(image, path, quality)
                    .map_err(|error| format!("{error:#}")),
                ImageSource::Disk(image) => {
                    let rgba = image
                        .read_rows(0, height)
                        .map_err(|error| format!("{error:#}"))?;
                    snapit_capture::save_jpeg(&rgba, path, quality)
                        .map_err(|error| format!("{error:#}"))
                }
            },
            ExportResult::Hdr(export) => {
                let preview = export.sdr_preview();
                snapit_capture::save_jpeg(&preview, path, quality)
                    .map_err(|error| format!("{error:#}"))
            }
        }
    }

    pub fn png(&self) -> std::result::Result<Arc<Vec<u8>>, String> {
        if let Some(png) = &self.frozen_png {
            return Ok(png.clone());
        }
        match self.result()?.as_ref() {
            ExportResult::Sdr(image) => match image.as_ref() {
                ImageSource::Memory(image) => snapit_capture::encode_png(image)
                    .map(Arc::new)
                    .map_err(|err| format!("{err:#}")),
                ImageSource::Disk(_) => Err(
                    "disk-backed PNG must be written to a file instead of copied into memory"
                        .to_string(),
                ),
            },
            ExportResult::Hdr(export) => export.png(),
        }
    }
}

impl ExportResult {
    pub fn is_hdr(&self) -> bool {
        matches!(self, Self::Hdr(_))
    }
}

impl HdrExport {
    fn sdr_preview(&self) -> Arc<RgbaImage> {
        let mut cached = self
            .preview
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(preview) = cached.upgrade() {
            return preview;
        }
        let preview = Arc::new(self.image.to_sdr());
        *cached = Arc::downgrade(&preview);
        preview
    }

    fn png(&self) -> std::result::Result<Arc<Vec<u8>>, String> {
        self.png
            .get_or_init(|| {
                snapit_capture::encode_png_hdr(&self.image)
                    .map(Arc::new)
                    .map_err(|err| format!("{err:#}"))
            })
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snapit_capture::{WatermarkPlacement, f32_to_f16_bits};

    fn hdr_sample(width: u32, height: u32, value: f32) -> HdrImage {
        let mut raw = Vec::with_capacity(width as usize * height as usize * 6);
        for _ in 0..width * height {
            for _ in 0..3 {
                raw.extend_from_slice(&f32_to_f16_bits(value).to_le_bytes());
            }
        }
        HdrImage::new(width, height, raw, 203.0)
    }

    fn plain_text(content: &str, font_size: f32, color: [u8; 3]) -> TextWatermark {
        TextWatermark {
            content: content.to_string(),
            font_size,
            color,
            bold: false,
            shadow: false,
            stroke_width: 0.0,
            stroke_color: [0, 0, 0],
            background: false,
            background_color: [15, 23, 42],
            background_opacity: 0.55,
        }
    }

    fn text_watermark() -> WatermarkSpec {
        WatermarkSpec {
            elements: vec![WatermarkElement {
                kind: WatermarkElementKind::Text(plain_text("SnapIt", 12.0, [255, 255, 255])),
                x: 0.0,
                y: 0.0,
                rotation_degrees: 0.0,
                scale: 1.0,
                opacity: 1.0,
            }],
            placement: WatermarkPlacement::TopLeft,
            margin_x: 1.0,
            margin_y: 1.0,
            ..WatermarkSpec::default()
        }
    }

    #[test]
    fn watermark_variables_expand_with_local_timestamp() {
        use chrono::TimeZone as _;
        let now = chrono::Local
            .with_ymd_and_hms(2026, 11, 25, 14, 5, 0)
            .unwrap();
        assert_eq!(
            expand_watermark_variables_at("SnapIt · {date} {time} ({year}) {datetime}", now),
            "SnapIt · 2026-11-25 14:05 (2026) 2026-11-25 14:05"
        );
        assert_eq!(expand_watermark_variables_at("Plain", now), "Plain");
    }

    #[test]
    fn hdr_multi_element_watermark_keeps_hdr_output() {
        let source = hdr_sample(64, 48, 2.0);
        let badge = Arc::new(RgbaImage::from_pixel(8, 8, image::Rgba([255, 0, 0, 255])));
        let spec = WatermarkSpec {
            elements: vec![
                WatermarkElement {
                    kind: WatermarkElementKind::Image(ImageWatermark {
                        image: badge,
                        width: 8.0,
                        corner_radius: 50.0,
                    }),
                    x: 0.0,
                    y: 0.0,
                    rotation_degrees: 0.0,
                    scale: 1.0,
                    opacity: 1.0,
                },
                WatermarkElement {
                    kind: WatermarkElementKind::Text(TextWatermark {
                        shadow: true,
                        stroke_width: 1.0,
                        background: true,
                        ..plain_text("Hi", 12.0, [255, 255, 255])
                    }),
                    x: 12.0,
                    y: 0.0,
                    rotation_degrees: 0.0,
                    scale: 1.0,
                    opacity: 0.8,
                },
            ],
            placement: WatermarkPlacement::TopLeft,
            margin_x: 2.0,
            margin_y: 2.0,
            ..WatermarkSpec::default()
        };
        let composition = ExportComposition::new(
            Arc::new(source.to_sdr()),
            Some(Arc::new(source)),
            Arc::new(Vec::new()),
            Arc::new(spec),
            1.0,
        );
        assert!(composition.has_hdr_original());
        let png = composition.png().unwrap();
        assert!(
            snapit_capture::decode_hdr_png_preview(&png)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn hdr_annotations_and_watermark_share_one_encoded_original() {
        let source = hdr_sample(48, 32, 0.25);
        let original = snapit_capture::encode_png_hdr(&source).unwrap();
        let composition = ExportComposition::new(
            Arc::new(source.to_sdr()),
            Some(Arc::new(source)),
            Arc::new(vec![Shape::Step {
                at: [20.0, 16.0],
                number: 2,
                style: crate::annotate::Style {
                    color: 0xEF4444,
                    size: 4.0,
                },
            }]),
            Arc::new(text_watermark()),
            1.0,
        );
        assert!(composition.result().unwrap().is_hdr());
        let first = composition.png().unwrap();
        let second = composition.png().unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_ne!(first.as_slice(), original);
        assert_eq!(composition.sdr_preview().unwrap().dimensions(), (48, 32));
        let result = composition.result().unwrap();
        let ExportResult::Hdr(export) = result.as_ref() else {
            panic!("HDR source must stay HDR after composition");
        };
        assert!(export.preview.lock().unwrap().upgrade().is_none());
        assert!(
            snapit_capture::decode_hdr_png_preview(&first)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn concurrent_hdr_composition_keeps_one_hdr_result() {
        let source = hdr_sample(256, 192, 2.0);
        let composition = Arc::new(ExportComposition::new(
            Arc::new(source.to_sdr()),
            Some(Arc::new(source)),
            Arc::new(Vec::new()),
            Arc::new(text_watermark()),
            1.0,
        ));
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let threads = (0..8)
            .map(|_| {
                let composition = composition.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    composition.result().unwrap()
                })
            })
            .collect::<Vec<_>>();
        let results = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        let first = &results[0];
        for result in &results {
            assert!(result.is_hdr());
            assert!(Arc::ptr_eq(first, result));
        }
    }

    #[test]
    fn frozen_history_png_is_returned_verbatim() {
        let bytes = Arc::new(vec![1_u8, 2, 3, 4]);
        let preview = Arc::new(RgbaImage::new(2, 2));
        let composition = ExportComposition::frozen(bytes.clone(), preview, true);
        let exported = composition.png().unwrap();
        assert!(Arc::ptr_eq(&bytes, &exported));
        assert!(composition.has_hdr_original());
    }

    #[test]
    fn sdr_watermark_changes_only_the_composed_result() {
        let clean = Arc::new(RgbaImage::from_pixel(48, 32, image::Rgba([0, 0, 0, 255])));
        let composition = ExportComposition::new(
            clean.clone(),
            None,
            Arc::new(Vec::new()),
            Arc::new(text_watermark()),
            1.0,
        );
        let preview = composition.sdr_preview().unwrap();
        assert_ne!(preview.as_raw(), clean.as_raw());
        assert_eq!(preview.get_pixel(47, 31).0, [0, 0, 0, 255]);
    }

    #[test]
    fn disk_composition_matches_memory_composition() {
        let source = RgbaImage::from_fn(96, 160, |x, y| {
            image::Rgba([x as u8 * 2, y as u8, (x + y) as u8, 255])
        });
        let shapes = Arc::new(vec![Shape::Rect {
            from: [8.0, 20.0],
            to: [70.0, 130.0],
            style: crate::annotate::Style::default(),
        }]);
        let watermark = Arc::new(text_watermark());
        let memory =
            ExportComposition::new(source.clone(), None, shapes.clone(), watermark.clone(), 1.0);
        let disk = ExportComposition::new(
            ImageSource::Disk(Arc::new(
                snapit_capture::DiskImage::from_image(&source).unwrap(),
            )),
            None,
            shapes,
            watermark,
            1.0,
        );
        let expected = memory.result().unwrap();
        let actual = disk.result().unwrap();
        let ExportResult::Sdr(expected) = expected.as_ref() else {
            panic!("expected SDR");
        };
        let ExportResult::Sdr(actual) = actual.as_ref() else {
            panic!("expected SDR");
        };
        assert!(actual.is_disk());
        for start in [0, 31, 97, 159] {
            let rows = (160 - start).min(37);
            assert_eq!(
                actual.read_rows(start, rows).unwrap(),
                expected.read_rows(start, rows).unwrap()
            );
        }
    }

    #[test]
    fn disk_clipboard_export_keeps_png_on_disk() {
        let source = RgbaImage::from_pixel(32, 64, image::Rgba([20, 40, 60, 255]));
        let composition = ExportComposition::new(
            ImageSource::Disk(Arc::new(
                snapit_capture::DiskImage::from_image(&source).unwrap(),
            )),
            None,
            Arc::new(Vec::new()),
            Arc::new(WatermarkSpec::default()),
            1.0,
        );
        assert!(composition.clipboard_uses_file());
        assert!(composition.png().is_err());
        let dir = std::env::temp_dir().join(format!(
            "snapit-export-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let path = composition.write_clipboard_png(&dir).unwrap();
        assert!(path.exists());
        assert_eq!(&std::fs::read(&path).unwrap()[..8], b"\x89PNG\r\n\x1a\n");
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn oversized_jpeg_export_fails_before_materializing_the_image() {
        let image = snapit_capture::DiskImage::create(8192, 2049).unwrap();
        let composition = ExportComposition::new(
            ImageSource::Disk(Arc::new(image)),
            None,
            Arc::new(Vec::new()),
            Arc::new(WatermarkSpec::default()),
            1.0,
        );
        let error = composition
            .write_jpeg(Path::new("unused.jpg"), 90)
            .unwrap_err();
        assert!(error.contains("save as PNG"));
    }

    #[test]
    fn hdr_export_sdr_preview_reuses_cached_arc() {
        let hdr = snapit_capture::HdrImage::new(2, 2, vec![0; 2 * 2 * 6], 203.0);
        let export = HdrExport {
            image: Arc::new(hdr),
            preview: Mutex::new(Weak::new()),
            png: OnceLock::new(),
        };
        let first = export.sdr_preview();
        let second = export.sdr_preview();
        assert!(Arc::ptr_eq(&first, &second));
    }
}
