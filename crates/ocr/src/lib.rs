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

/// Recognize text across an arbitrarily tall image by slicing into overlapping
/// vertical strips above 4096 px and deduplicating lines across strip seams.
pub fn recognize_tall(engine: &dyn OcrEngine, image: &RgbaImage) -> Result<Vec<TextBlock>> {
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 {
        return Ok(Vec::new());
    }
    if height <= TALL_OCR_THRESHOLD {
        return engine.recognize(image);
    }

    let step = STRIP_HEIGHT - STRIP_OVERLAP;
    let mut merged: Vec<TextBlock> = Vec::new();
    let mut y = 0u32;

    while y < height {
        let h = STRIP_HEIGHT.min(height - y);
        let strip = image::imageops::crop_imm(image, 0, y, width, h).to_image();
        let mut blocks = engine.recognize(&strip)?;
        let seam_start = y as f32;
        let seam_end = (y + STRIP_OVERLAP) as f32;
        for block in &mut blocks {
            block.bounds.y += seam_start;
        }

        if y == 0 {
            merged.extend(blocks);
        } else {
            let prev_tail_start = merged.len().saturating_sub(32);
            for block in blocks {
                let in_overlap = block.bounds.y < seam_end;
                let duplicate = in_overlap
                    && merged[prev_tail_start..].iter().any(|existing| {
                        let tol = existing.bounds.height.max(block.bounds.height).max(16.0) * 0.75;
                        let same_y = (existing.bounds.y - block.bounds.y).abs() <= tol;
                        if !same_y {
                            return false;
                        }
                        let a = existing.text.trim();
                        let b = block.text.trim();
                        !a.is_empty() && (a == b || a.contains(b) || b.contains(a))
                    });
                if !duplicate {
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
}
