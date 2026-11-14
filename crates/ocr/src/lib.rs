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
