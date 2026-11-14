use std::path::Path;

use anyhow::{Context as _, Result, anyhow};
use ocrs::{ImageSource, OcrEngineParams, TextItem as _};
use rten::Model;

use crate::{OcrEngine, RgbaImage, TextBlock, TextBounds};

pub const DETECTION_MODEL: &str = "text-detection.rten";
pub const RECOGNITION_MODEL: &str = "text-recognition.rten";

/// Pure-Rust OCR via `ocrs`. Latin script only.
pub struct OcrsEngine {
    engine: ocrs::OcrEngine,
}

impl OcrsEngine {
    pub fn from_dir(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        let load = |name: &str| {
            let path = dir.join(name);
            Model::load_file(&path).with_context(|| {
                format!(
                    "failed to load OCR model {}; see README for download instructions",
                    path.display()
                )
            })
        };
        let engine = ocrs::OcrEngine::new(OcrEngineParams {
            detection_model: Some(load(DETECTION_MODEL)?),
            recognition_model: Some(load(RECOGNITION_MODEL)?),
            ..Default::default()
        })
        .map_err(|err| anyhow!("failed to initialize ocrs: {err}"))?;
        Ok(Self { engine })
    }
}

impl OcrEngine for OcrsEngine {
    fn name(&self) -> &'static str {
        "ocrs"
    }

    fn recognize(&self, image: &RgbaImage) -> Result<Vec<TextBlock>> {
        let rgb: Vec<u8> = image
            .pixels()
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2]])
            .collect();
        let source = ImageSource::from_bytes(&rgb, image.dimensions())
            .map_err(|err| anyhow!("invalid OCR input: {err}"))?;
        let input = self
            .engine
            .prepare_input(source)
            .map_err(|err| anyhow!("{err}"))?;
        let words = self
            .engine
            .detect_words(&input)
            .map_err(|err| anyhow!("{err}"))?;
        let lines = self.engine.find_text_lines(&input, &words);
        let recognized = self
            .engine
            .recognize_text(&input, &lines)
            .map_err(|err| anyhow!("{err}"))?;

        Ok(recognized
            .into_iter()
            .flatten()
            .filter(|line| !line.to_string().trim().is_empty())
            .map(|line| {
                let rect = line.bounding_rect();
                TextBlock::new(
                    line.to_string(),
                    TextBounds::new(
                        rect.left() as f32,
                        rect.top() as f32,
                        rect.width() as f32,
                        rect.height() as f32,
                    ),
                )
            })
            .collect())
    }
}
