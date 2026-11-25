//! Text recognition for SnapIt.
//!
//! [`OcrEngine`] is the backend seam. [`default_engine`] picks the best backend
//! available on this platform: Apple Vision on macOS, Windows.Media.Ocr on
//! Windows, otherwise `ocrs` with models from [`models_dir`].

use std::path::PathBuf;

use anyhow::Result;
pub use image::RgbaImage;

#[cfg(feature = "ocrs")]
mod ocrs_engine;
#[cfg(target_os = "macos")]
mod vision;
mod windows_ocr;

#[cfg(feature = "ocrs")]
pub use ocrs_engine::OcrsEngine;
#[cfg(target_os = "macos")]
pub use vision::VisionEngine;
pub use windows_ocr::WindowsOcrEngine;

/// Axis-aligned box in image pixels, origin at the top-left.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct TextBounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl TextBounds {
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// One recognized line of text.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct TextBlock {
    pub text: String,
    pub bounds: TextBounds,
    pub confidence: Option<f32>,
}

impl TextBlock {
    pub fn new(text: impl Into<String>, bounds: TextBounds) -> Self {
        Self {
            text: text.into(),
            bounds,
            confidence: None,
        }
    }

    pub fn with_confidence(mut self, confidence: f32) -> Self {
        self.confidence = Some(confidence);
        self
    }
}

pub trait OcrEngine: Send + Sync {
    fn name(&self) -> &'static str;

    /// Recognize text lines in `image`, in reading order.
    fn recognize(&self, image: &RgbaImage) -> Result<Vec<TextBlock>>;

    /// 提前支付后端的首次调用开销；默认什么都不做。
    fn prewarm(&self) -> Result<()> {
        Ok(())
    }
}

/// Join blocks into plain text, one line per block.
pub fn blocks_to_text(blocks: &[TextBlock]) -> String {
    blocks
        .iter()
        .map(|block| block.text.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
}

const TALL_OCR_THRESHOLD: u32 = 4096;
const STRIP_HEIGHT: u32 = 3600;
const STRIP_OVERLAP: u32 = 240;
const STRIP_BYTES: usize = 64 * 1024 * 1024;

/// Recognize text across an arbitrarily tall image by slicing into overlapping
/// vertical strips above 4096 px and deduplicating lines across strip seams.
pub fn recognize_tall(engine: &dyn OcrEngine, image: &RgbaImage) -> Result<Vec<TextBlock>> {
    recognize_rows(engine, image.width(), image.height(), |start, rows| {
        Ok(image::imageops::crop_imm(image, 0, start, image.width(), rows).to_image())
    })
}

/// Recognize a tall image whose rows are supplied on demand in bounded strips.
pub fn recognize_rows(
    engine: &dyn OcrEngine,
    width: u32,
    height: u32,
    mut read_rows: impl FnMut(u32, u32) -> Result<RgbaImage>,
) -> Result<Vec<TextBlock>> {
    if width == 0 || height == 0 {
        return Ok(Vec::new());
    }
    let row_bytes = width as usize * 4;
    let strip_height = STRIP_HEIGHT
        .min((STRIP_BYTES / row_bytes.max(1)) as u32)
        .max(1);
    if height <= TALL_OCR_THRESHOLD && height <= strip_height {
        return engine.recognize(&read_rows(0, height)?);
    }

    let overlap = STRIP_OVERLAP.min(strip_height / 4);
    let step = (strip_height - overlap).max(1);
    let mut merged: Vec<TextBlock> = Vec::new();
    let mut y = 0u32;

    while y < height {
        let h = strip_height.min(height - y);
        let strip = read_rows(y, h)?;
        let mut blocks = engine.recognize(&strip)?;
        let seam_start = y as f32;
        let seam_end = (y + overlap) as f32;
        for block in &mut blocks {
            block.bounds.y += seam_start;
        }

        if y == 0 {
            merged.extend(blocks);
        } else {
            for block in blocks {
                if !intersects_vertical_span(block.bounds, seam_start, seam_end) {
                    merged.push(block);
                    continue;
                }

                // Search every block that intersects this seam. A busy overlap
                // can contain far more than 32 lines, and reading order is not
                // a safe bound on where a duplicate appears.
                let mut duplicate: Option<(usize, &TextBlock)> = None;
                for (index, existing) in merged.iter().enumerate() {
                    if !intersects_vertical_span(existing.bounds, seam_start, seam_end)
                        || !is_seam_duplicate(existing, &block, seam_start, seam_end)
                    {
                        continue;
                    }
                    if duplicate.is_none_or(|(_, current)| {
                        text_rank(&existing.text, existing.confidence)
                            > text_rank(&current.text, current.confidence)
                    }) {
                        duplicate = Some((index, existing));
                    }
                }

                if let Some((index, existing)) = duplicate {
                    if text_rank(&block.text, block.confidence)
                        > text_rank(&existing.text, existing.confidence)
                    {
                        merged[index] = block;
                    }
                } else {
                    merged.push(block);
                }
            }
        }

        if y + h >= height {
            break;
        }
        y += step;
    }

    Ok(merged)
}

fn normalized_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn text_rank(text: &str, confidence: Option<f32>) -> (usize, f32) {
    (
        normalized_text(text).chars().count(),
        confidence.filter(|value| value.is_finite()).unwrap_or(0.0),
    )
}

fn intersects_vertical_span(bounds: TextBounds, start: f32, end: f32) -> bool {
    bounds.height > 0.0 && bounds.y < end && bounds.y + bounds.height > start
}

fn overlap_ratio(a_start: f32, a_size: f32, b_start: f32, b_size: f32) -> f32 {
    if a_size <= 0.0 || b_size <= 0.0 {
        return 0.0;
    }
    let overlap = ((a_start + a_size).min(b_start + b_size) - a_start.max(b_start)).max(0.0);
    overlap / a_size.min(b_size)
}

fn bounds_overlap(a: TextBounds, b: TextBounds) -> bool {
    overlap_ratio(a.y, a.height, b.y, b.height) >= 0.5
        && overlap_ratio(a.x, a.width, b.x, b.width) >= 0.5
}

fn same_horizontal_span(a: TextBounds, b: TextBounds) -> bool {
    let tolerance = (a.height.max(b.height).max(1.0) * 0.75).max(4.0);
    let aligned =
        (a.x - b.x).abs() <= tolerance || ((a.x + a.width) - (b.x + b.width)).abs() <= tolerance;
    aligned && overlap_ratio(a.x, a.width, b.x, b.width) >= 0.8
}

fn clipped_at_bottom(bounds: TextBounds, strip_end: f32) -> bool {
    let tolerance = bounds.height.max(1.0) * 0.25;
    bounds.y + bounds.height >= strip_end - tolerance
}

fn clipped_at_top(bounds: TextBounds, strip_start: f32) -> bool {
    let tolerance = bounds.height.max(1.0) * 0.25;
    bounds.y <= strip_start + tolerance
}

fn is_seam_duplicate(existing: &TextBlock, candidate: &TextBlock, start: f32, end: f32) -> bool {
    if !bounds_overlap(existing.bounds, candidate.bounds) {
        return false;
    }
    let a = normalized_text(&existing.text);
    let b = normalized_text(&candidate.text);
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    if !same_horizontal_span(existing.bounds, candidate.bounds)
        || (!a.contains(&b) && !b.contains(&a))
    {
        return false;
    }

    // A prefix/suffix alone is not enough: adjacent real lines often have
    // related text. Substring dedupe is only safe when the shorter recognition
    // is visibly cut by the strip edge and the longer one is not.
    let existing_clipped = clipped_at_bottom(existing.bounds, end);
    let candidate_clipped = clipped_at_top(candidate.bounds, start);
    if a.chars().count() < b.chars().count() {
        existing_clipped && !candidate_clipped
    } else {
        candidate_clipped && !existing_clipped
    }
}

/// Where the `ocrs` backend looks for `text-detection.rten` and
/// `text-recognition.rten`: `$SNAPIT_OCR_MODELS`, then `models/` next to the
/// executable, then `~/.snapit/models`.
pub fn models_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SNAPIT_OCR_MODELS") {
        return PathBuf::from(dir);
    }
    let beside_exe = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("models")));
    if let Some(dir) = beside_exe.filter(|dir| dir.is_dir()) {
        return dir;
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    home.map(PathBuf::from)
        .unwrap_or_default()
        .join(".snapit")
        .join("models")
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct OcrOptions {
    /// BCP-47 tags in priority order; empty picks the backend default.
    /// Apple Vision and Windows.Media.Ocr honour it; `ocrs` only reads Latin
    /// script. Windows.Media.Ocr uses a single recognizer language (first
    /// supported tag, otherwise the user-profile languages).
    pub languages: Vec<String>,
}

impl OcrOptions {
    pub fn with_languages(mut self, languages: Vec<String>) -> Self {
        self.languages = languages;
        self
    }
}

/// The preferred engine for this platform with default options.
pub fn default_engine() -> Result<Box<dyn OcrEngine>> {
    engine_with(&OcrOptions::default())
}

/// The preferred engine for this platform.
pub fn engine_with(options: &OcrOptions) -> Result<Box<dyn OcrEngine>> {
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(VisionEngine::with_languages(
            options.languages.clone(),
        )))
    }
    #[cfg(not(target_os = "macos"))]
    {
        #[cfg(windows)]
        match WindowsOcrEngine::with_languages(options.languages.clone()) {
            Ok(engine) => return Ok(Box::new(engine)),
            Err(err) => {
                log::warn!("Windows.Media.Ocr unavailable ({err}); falling back to ocrs");
            }
        }
        #[cfg(not(windows))]
        let _ = options;
        #[cfg(feature = "ocrs")]
        {
            Ok(Box::new(OcrsEngine::from_dir(models_dir())?))
        }
        #[cfg(not(feature = "ocrs"))]
        {
            anyhow::bail!("no OCR backend available; enable the `ocrs` feature")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockOcr {
        calls: std::sync::Mutex<Vec<(u32, u32)>>,
    }

    impl OcrEngine for MockOcr {
        fn name(&self) -> &'static str {
            "mock"
        }

        fn recognize(&self, image: &RgbaImage) -> Result<Vec<TextBlock>> {
            let call_ix = {
                let mut calls = self.calls.lock().unwrap();
                calls.push(image.dimensions());
                calls.len() - 1
            };
            if image.height() == 1200 {
                return Ok(vec![TextBlock::new(
                    "single",
                    TextBounds::new(10.0, 20.0, 100.0, 24.0),
                )]);
            }
            if call_ix == 0 {
                Ok(vec![
                    TextBlock::new("top-line", TextBounds::new(10.0, 100.0, 100.0, 24.0)),
                    TextBlock::new(
                        "seam-line",
                        TextBounds::new(
                            10.0,
                            (STRIP_HEIGHT - STRIP_OVERLAP + 60) as f32,
                            100.0,
                            24.0,
                        ),
                    ),
                ])
            } else {
                Ok(vec![
                    TextBlock::new("seam-line", TextBounds::new(10.0, 60.0, 100.0, 24.0)),
                    TextBlock::new("bottom-line", TextBounds::new(10.0, 500.0, 100.0, 24.0)),
                ])
            }
        }
    }

    struct FnOcr<F>(F);

    impl<F> OcrEngine for FnOcr<F>
    where
        F: Fn(&RgbaImage) -> Result<Vec<TextBlock>> + Send + Sync,
    {
        fn name(&self) -> &'static str {
            "scripted"
        }

        fn recognize(&self, image: &RgbaImage) -> Result<Vec<TextBlock>> {
            self.0(image)
        }
    }

    #[test]
    fn recognize_tall_passes_short_images_through_directly() {
        let engine = MockOcr {
            calls: std::sync::Mutex::new(Vec::new()),
        };
        let img = RgbaImage::new(400, 1200);
        let blocks = recognize_tall(&engine, &img).unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "single");
        assert_eq!(*engine.calls.lock().unwrap(), vec![(400, 1200)]);
    }

    #[test]
    fn recognize_rows_adapts_strip_height_to_the_byte_budget() {
        struct RecordingOcr(std::sync::Mutex<Vec<(u32, u32)>>);

        impl OcrEngine for RecordingOcr {
            fn name(&self) -> &'static str {
                "recording"
            }

            fn recognize(&self, image: &RgbaImage) -> Result<Vec<TextBlock>> {
                self.0.lock().unwrap().push(image.dimensions());
                Ok(Vec::new())
            }
        }

        let engine = RecordingOcr(std::sync::Mutex::new(Vec::new()));
        let starts = std::sync::Mutex::new(Vec::new());
        recognize_rows(&engine, 16_384, 5_000, |start, rows| {
            starts.lock().unwrap().push(start);
            assert!(rows as usize * 16_384 * 4 <= STRIP_BYTES);
            Ok(RgbaImage::new(16_384, rows))
        })
        .unwrap();

        let calls = engine.0.lock().unwrap();
        assert!(calls.len() > 1);
        assert_eq!(calls[0].1, 1_024);
        let starts = starts.lock().unwrap();
        assert!(
            starts.windows(2).all(|pair| pair[1] > pair[0]),
            "{starts:?}"
        );
    }

    #[test]
    fn recognize_tall_slices_deduplicates_and_restores_y() {
        let engine = MockOcr {
            calls: std::sync::Mutex::new(Vec::new()),
        };
        let img = RgbaImage::new(400, 5000);
        let blocks = recognize_tall(&engine, &img).unwrap();
        let texts: Vec<_> = blocks.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts, vec!["top-line", "seam-line", "bottom-line"]);
        assert_eq!(
            blocks[2].bounds.y,
            (STRIP_HEIGHT - STRIP_OVERLAP + 500) as f32
        );
    }

    #[test]
    fn substring_text_does_not_delete_adjacent_or_unclipped_lines() {
        let step = STRIP_HEIGHT - STRIP_OVERLAP;
        for second_y in [STRIP_HEIGHT - 34, STRIP_HEIGHT - 40] {
            let calls = std::sync::atomic::AtomicUsize::new(0);
            let engine = FnOcr(|_: &RgbaImage| {
                let call = calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let (text, y) = if call == 0 {
                    ("foo", (STRIP_HEIGHT - 40) as f32)
                } else {
                    ("foobar", (second_y - step) as f32)
                };
                Ok(vec![TextBlock::new(
                    text,
                    TextBounds::new(10.0, y, 40.0, 5.0),
                )])
            });
            let blocks = recognize_rows(&engine, 400, 5_000, |_start, rows| {
                Ok(RgbaImage::new(400, rows))
            })
            .unwrap();
            let texts: Vec<_> = blocks.iter().map(|block| block.text.as_str()).collect();
            assert_eq!(texts, vec!["foo", "foobar"], "second y = {second_y}");
        }
    }

    #[test]
    fn clearly_clipped_substring_is_replaced_by_the_more_complete_line() {
        let step = STRIP_HEIGHT - STRIP_OVERLAP;
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let engine = FnOcr(|_: &RgbaImage| {
            let call = calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if call == 0 {
                Ok(vec![
                    TextBlock::new(
                        "foo",
                        TextBounds::new(10.0, (STRIP_HEIGHT - 12) as f32, 40.0, 12.0),
                    )
                    .with_confidence(0.99),
                ])
            } else {
                Ok(vec![
                    TextBlock::new(
                        "foobar",
                        TextBounds::new(10.0, (STRIP_HEIGHT - 12 - step) as f32, 70.0, 24.0),
                    )
                    .with_confidence(0.40),
                ])
            }
        });
        let blocks = recognize_rows(&engine, 400, 5_000, |_start, rows| {
            Ok(RgbaImage::new(400, rows))
        })
        .unwrap();

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "foobar");
        assert_eq!(blocks[0].confidence, Some(0.40));
    }

    #[test]
    fn exact_normalized_duplicate_prefers_higher_confidence() {
        let step = STRIP_HEIGHT - STRIP_OVERLAP;
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let engine = FnOcr(|_: &RgbaImage| {
            let call = calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let block = if call == 0 {
                TextBlock::new(
                    "  seam   line  ",
                    TextBounds::new(10.0, (STRIP_HEIGHT - 20) as f32, 80.0, 20.0),
                )
                .with_confidence(0.50)
            } else {
                TextBlock::new(
                    "seam line",
                    TextBounds::new(10.0, (STRIP_HEIGHT - 20 - step) as f32, 80.0, 20.0),
                )
                .with_confidence(0.90)
            };
            Ok(vec![block])
        });
        let blocks = recognize_rows(&engine, 400, 5_000, |_start, rows| {
            Ok(RgbaImage::new(400, rows))
        })
        .unwrap();

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "seam line");
        assert_eq!(blocks[0].confidence, Some(0.90));
    }

    #[test]
    fn seam_deduplication_searches_more_than_32_repeated_lines() {
        const LINES: usize = 40;
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let engine = FnOcr(|_: &RgbaImage| {
            let call = calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let offset = if call == 0 { STRIP_HEIGHT - 200 } else { 40 };
            Ok((0..LINES)
                .map(|line| {
                    TextBlock::new(
                        "repeated",
                        TextBounds::new(10.0, (offset + line as u32 * 4) as f32, 70.0, 4.0),
                    )
                })
                .collect())
        });
        let blocks = recognize_rows(&engine, 400, 5_000, |_start, rows| {
            Ok(RgbaImage::new(400, rows))
        })
        .unwrap();

        assert_eq!(blocks.len(), LINES);
        assert_eq!(blocks[0].bounds.y, (STRIP_HEIGHT - 200) as f32);
        assert_eq!(blocks[LINES - 1].bounds.y, (STRIP_HEIGHT - 44) as f32);
        assert!(
            blocks
                .windows(2)
                .all(|pair| pair[0].bounds.y < pair[1].bounds.y)
        );
    }

    #[test]
    fn recognize_rows_propagates_read_errors_with_the_next_strip_offset() {
        let engine = FnOcr(|_: &RgbaImage| Ok(Vec::new()));
        let starts = std::sync::Mutex::new(Vec::new());
        let error = recognize_rows(&engine, 400, 5_000, |start, rows| {
            starts.lock().unwrap().push(start);
            if start > 0 {
                anyhow::bail!("rows unavailable at {start}");
            }
            Ok(RgbaImage::new(400, rows))
        })
        .unwrap_err();

        assert!(error.to_string().contains("rows unavailable at 3360"));
        assert_eq!(
            *starts.lock().unwrap(),
            vec![0, STRIP_HEIGHT - STRIP_OVERLAP]
        );
    }

    #[test]
    fn recognize_rows_propagates_backend_errors() {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let engine = FnOcr(|_: &RgbaImage| {
            if calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0 {
                Ok(Vec::new())
            } else {
                anyhow::bail!("recognition failed")
            }
        });
        let error = recognize_rows(&engine, 400, 5_000, |_start, rows| {
            Ok(RgbaImage::new(400, rows))
        })
        .unwrap_err();
        assert!(error.to_string().contains("recognition failed"));
    }
}
