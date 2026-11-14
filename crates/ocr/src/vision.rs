use std::io::Cursor;

use anyhow::{Context as _, Result, anyhow};
use image::ImageFormat;
use objc2::{AnyThread as _, rc::autoreleasepool};
use objc2_foundation::{NSArray, NSData, NSDictionary, NSString};
use objc2_vision::{
    VNImageOption, VNImageRequestHandler, VNRecognizeTextRequest, VNRequest,
    VNRequestTextRecognitionLevel,
};

use crate::{OcrEngine, RgbaImage, TextBlock, TextBounds};

const DEFAULT_LANGUAGES: &[&str] = &["zh-Hans", "zh-Hant", "en-US"];

/// Apple Vision (`VNRecognizeTextRequest`), macOS 11+. Supports Chinese.
pub struct VisionEngine {
    languages: Vec<String>,
}

impl Default for VisionEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl VisionEngine {
    pub fn new() -> Self {
        Self::with_languages(Vec::new())
    }

    /// Recognition languages in priority order; empty means Chinese + English.
    pub fn with_languages(languages: Vec<String>) -> Self {
        let languages = if languages.is_empty() {
            DEFAULT_LANGUAGES.iter().map(|l| l.to_string()).collect()
        } else {
            languages
        };
        Self { languages }
    }
}

impl OcrEngine for VisionEngine {
    fn name(&self) -> &'static str {
        "Apple Vision"
    }

    /// 16×16 白图足以让 Vision 完成一次性的模型编译。
    fn prewarm(&self) -> Result<()> {
        let image = RgbaImage::from_pixel(16, 16, image::Rgba([255, 255, 255, 255]));
        self.recognize(&image).map(|_| ())
    }

    fn recognize(&self, image: &RgbaImage) -> Result<Vec<TextBlock>> {
        let mut png = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .context("failed to encode image for Vision")?;
        let (width, height) = (image.width() as f64, image.height() as f64);

        autoreleasepool(|_| {
            let data = NSData::with_bytes(&png);
            let options = NSDictionary::<VNImageOption, objc2::runtime::AnyObject>::new();
            let handler = VNImageRequestHandler::initWithData_options(
                VNImageRequestHandler::alloc(),
                &data,
                &options,
            );

            let request = VNRecognizeTextRequest::new();
            request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
            request.setUsesLanguageCorrection(true);
            let languages: Vec<_> = self
                .languages
                .iter()
                .map(|l| NSString::from_str(l))
                .collect();
            request.setRecognitionLanguages(&NSArray::from_retained_slice(&languages));

            let base: &VNRequest = &request;
            handler
                .performRequests_error(&NSArray::from_slice(&[base]))
                .map_err(|err| anyhow!("Vision OCR failed: {}", err.localizedDescription()))?;

            let mut blocks = Vec::new();
            let Some(results) = request.results() else {
                return Ok(blocks);
            };
            for observation in results.iter() {
                let Some(candidate) = observation.topCandidates(1).firstObject() else {
                    continue;
                };
                // Normalized, origin at the lower-left corner.
                let rect = unsafe { observation.boundingBox() };
                let bounds = TextBounds::new(
                    (rect.origin.x * width) as f32,
                    ((1.0 - rect.origin.y - rect.size.height) * height) as f32,
                    (rect.size.width * width) as f32,
                    (rect.size.height * height) as f32,
                );
                blocks.push(
                    TextBlock::new(candidate.string().to_string(), bounds)
                        .with_confidence(candidate.confidence()),
                );
            }
            Ok(blocks)
        })
    }
}
