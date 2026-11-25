//! `Windows.Media.Ocr` backend.
//!
//! The WinRT engine (`OcrEngine`, `SoftwareBitmap`) is compiled only on Windows.
//! Line → [`TextBlock`] mapping is shared so fixtures can run on any host.
//!
//! # Language
//!
//! Windows.Media.Ocr uses **one** recognizer language per engine:
//! [`TryCreateFromLanguage`](https://learn.microsoft.com/uwp/api/windows.media.ocr.ocrengine.trycreatefromlanguage)
//! when the app setting is a supported BCP-47 tag (via
//! `IsLanguageSupported` / `AvailableRecognizerLanguages`), otherwise
//! [`TryCreateFromUserProfileLanguages`](https://learn.microsoft.com/uwp/api/windows.media.ocr.ocrengine.trycreatefromuserprofilelanguages)
//! (`auto`, or when the requested pack is not installed).
//!
//! Tags like `en-US` / `ja-JP` are tried as-is and then as the primary subtag
//! (`en`, `ja`). `zh-Hans` and `zh-Hant` stay distinct; there is no collapse
//! to `zh`. The matching OCR language pack must be installed (Settings → Time
//! & language → Language, or Optional features → OCR).
//!
//! # Other limits
//!
//! - Bounding boxes are image pixels, origin top-left (unlike Vision’s
//!   normalized, lower-left boxes).
//! - No per-line confidence is exposed; [`TextBlock::confidence`] stays `None`.
//! - Images larger than `OcrEngine::MaxImageDimension` are rejected.
//! - If the WinRT engine cannot be created, [`crate::engine_with`] falls back
//!   to `ocrs`.

#[cfg(windows)]
use anyhow::Context as _;
use anyhow::{Result, bail};

use crate::{OcrEngine, RgbaImage, TextBlock};

/// Pixel-space OCR lines, independent of WinRT, so fixtures run on any host.
#[cfg_attr(not(windows), allow(dead_code))]
mod mapping {
    use crate::{RgbaImage, TextBlock, TextBounds};

    #[derive(Clone, Debug)]
    pub struct MappedWord {
        pub text: String,
        pub x: f32,
        pub y: f32,
        pub width: f32,
        pub height: f32,
    }

    #[derive(Clone, Debug)]
    pub struct MappedLine {
        pub text: String,
        pub words: Vec<MappedWord>,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum HanScript {
        Simplified,
        Traditional,
    }

    pub fn rgba_to_bgra(image: &RgbaImage) -> Vec<u8> {
        let raw = image.as_raw();
        let mut bgra = Vec::with_capacity(raw.len());
        for &[r, g, b, a] in raw.as_chunks::<4>().0 {
            bgra.extend_from_slice(&[b, g, r, a]);
        }
        bgra
    }

    /// Tags to try with `OcrEngine::IsLanguageSupported`, in order.
    pub fn language_tag_candidates(tag: &str) -> Vec<&str> {
        let tag = tag.trim();
        if tag.is_empty() {
            return Vec::new();
        }
        let mut tags = vec![tag];
        let Some((primary, _)) = tag.split_once('-').or_else(|| tag.split_once('_')) else {
            return tags;
        };
        // Keep zh-Hans / zh-Hant / zh-CN as written; "zh" is ambiguous.
        if !primary.eq_ignore_ascii_case("zh") && !primary.is_empty() {
            tags.push(primary);
        }
        tags
    }

    pub fn language_tags_match(requested: &str, available: &str) -> bool {
        let requested = requested.trim();
        let available = available.trim();
        if requested.is_empty() || available.is_empty() {
            return false;
        }
        if requested.eq_ignore_ascii_case(available) {
            return true;
        }
        let req_primary = primary_subtag(requested);
        let avail_primary = primary_subtag(available);
        if !req_primary.eq_ignore_ascii_case(avail_primary) {
            return false;
        }
        match (han_script(requested), han_script(available)) {
            (Some(left), Some(right)) => left == right,
            (None, None) => true,
            _ => false,
        }
    }

    fn primary_subtag(tag: &str) -> &str {
        tag.split(['-', '_']).next().unwrap_or(tag)
    }

    fn han_script(tag: &str) -> Option<HanScript> {
        if !primary_subtag(tag).eq_ignore_ascii_case("zh") {
            return None;
        }
        for part in tag.split(['-', '_']) {
            if part.eq_ignore_ascii_case("Hans")
                || part.eq_ignore_ascii_case("CN")
                || part.eq_ignore_ascii_case("SG")
            {
                return Some(HanScript::Simplified);
            }
            if part.eq_ignore_ascii_case("Hant")
                || part.eq_ignore_ascii_case("TW")
                || part.eq_ignore_ascii_case("HK")
                || part.eq_ignore_ascii_case("MO")
            {
                return Some(HanScript::Traditional);
            }
        }
        None
    }

    pub fn text_blocks_from_mapped_lines(
        lines: impl IntoIterator<Item = MappedLine>,
    ) -> Vec<TextBlock> {
        lines
            .into_iter()
            .filter_map(text_block_from_mapped_line)
            .collect()
    }

    fn text_block_from_mapped_line(line: MappedLine) -> Option<TextBlock> {
        let mut text = line.text;
        if text.trim().is_empty() {
            text = line
                .words
                .iter()
                .map(|word| word.text.as_str())
                .filter(|word| !word.trim().is_empty())
                .collect::<Vec<_>>()
                .join(" ");
        }
        if text.trim().is_empty() {
            return None;
        }

        let mut left = f32::MAX;
        let mut top = f32::MAX;
        let mut right = f32::MIN;
        let mut bottom = f32::MIN;
        let mut any_box = false;
        for word in &line.words {
            if word.text.trim().is_empty() {
                continue;
            }
            any_box = true;
            left = left.min(word.x);
            top = top.min(word.y);
            right = right.max(word.x + word.width);
            bottom = bottom.max(word.y + word.height);
        }
        let bounds = if any_box {
            TextBounds::new(left, top, (right - left).max(0.0), (bottom - top).max(0.0))
        } else {
            TextBounds::default()
        };
        Some(TextBlock::new(text, bounds))
    }
}

/// `Windows.Media.Ocr` (`OcrEngine` + BGRA `SoftwareBitmap`).
pub struct WindowsOcrEngine {
    #[cfg(windows)]
    engine: windows::Media::Ocr::OcrEngine,
    #[cfg(not(windows))]
    _private: (),
}

impl WindowsOcrEngine {
    pub fn new() -> Result<Self> {
        Self::with_languages(Vec::new())
    }

    /// `languages` are BCP-47 tags in priority order; empty uses the user
    /// profile languages.
    pub fn with_languages(languages: Vec<String>) -> Result<Self> {
        #[cfg(not(windows))]
        {
            let _ = languages;
            bail!("Windows.Media.Ocr is only available on Windows")
        }
        #[cfg(windows)]
        {
            Ok(Self {
                engine: create_media_engine(&languages)?,
            })
        }
    }
}

impl OcrEngine for WindowsOcrEngine {
    fn name(&self) -> &'static str {
        "Windows.Media.Ocr"
    }

    fn recognize(&self, image: &RgbaImage) -> Result<Vec<TextBlock>> {
        #[cfg(not(windows))]
        {
            let _ = image;
            bail!("Windows.Media.Ocr is only available on Windows")
        }
        #[cfg(windows)]
        {
            recognize_with_engine(&self.engine, image)
        }
    }
}

#[cfg(windows)]
fn create_media_engine(languages: &[String]) -> Result<windows::Media::Ocr::OcrEngine> {
    use windows::Media::Ocr::OcrEngine as MediaOcrEngine;

    ensure_winrt();
    if languages.is_empty() {
        return MediaOcrEngine::TryCreateFromUserProfileLanguages().context(
            "Windows.Media.Ocr has no engine for the user-profile languages; install an OCR language pack in Settings > Time & language > Language",
        );
    }
    for tag in languages {
        if let Some(engine) = try_create_for_tag(tag) {
            return Ok(engine);
        }
    }
    log::warn!(
        "Windows.Media.Ocr does not support requested languages {languages:?}; using user-profile languages"
    );
    MediaOcrEngine::TryCreateFromUserProfileLanguages().with_context(|| {
        format!(
            "Windows.Media.Ocr does not support {languages:?} and has no user-profile engine; install an OCR language pack"
        )
    })
}

#[cfg(windows)]
fn try_create_for_tag(tag: &str) -> Option<windows::Media::Ocr::OcrEngine> {
    use windows::Globalization::Language;
    use windows::Media::Ocr::OcrEngine as MediaOcrEngine;
    use windows::core::HSTRING;

    for candidate in mapping::language_tag_candidates(tag) {
        let Ok(language) = Language::CreateLanguage(&HSTRING::from(candidate)) else {
            continue;
        };
        if !MediaOcrEngine::IsLanguageSupported(&language).unwrap_or(false) {
            continue;
        }
        if let Ok(engine) = MediaOcrEngine::TryCreateFromLanguage(&language) {
            return Some(engine);
        }
    }
    let Ok(available) = MediaOcrEngine::AvailableRecognizerLanguages() else {
        return None;
    };
    for language in available {
        let Ok(available_tag) = language.LanguageTag() else {
            continue;
        };
        if !mapping::language_tags_match(tag, &available_tag.to_string()) {
            continue;
        }
        if let Ok(engine) = MediaOcrEngine::TryCreateFromLanguage(&language) {
            return Some(engine);
        }
    }
    None
}

#[cfg(windows)]
fn recognize_with_engine(
    engine: &windows::Media::Ocr::OcrEngine,
    image: &RgbaImage,
) -> Result<Vec<TextBlock>> {
    ensure_winrt();
    let bitmap = software_bitmap(image)?;
    // `windows` 0.62 dropped the blocking `IAsyncOperation::get`; the operation
    // is `IntoFuture`, and the completion handler only wakes this thread.
    use std::future::IntoFuture;

    let operation = engine
        .RecognizeAsync(&bitmap)
        .context("Windows.Media.Ocr RecognizeAsync failed to start")?;
    let result = futures::executor::block_on(operation.into_future())
        .context("Windows.Media.Ocr RecognizeAsync failed")?;
    let lines = result
        .Lines()
        .context("Windows.Media.Ocr OcrResult.Lines failed")?;
    let mut mapped = Vec::new();
    for line in lines {
        mapped.push(mapped_line_from_ocr_line(&line)?);
    }
    Ok(mapping::text_blocks_from_mapped_lines(mapped))
}

#[cfg(windows)]
fn mapped_line_from_ocr_line(line: &windows::Media::Ocr::OcrLine) -> Result<mapping::MappedLine> {
    let text = line
        .Text()
        .map(|value| value.to_string())
        .unwrap_or_default();
    let mut words = Vec::new();
    let ocr_words = line
        .Words()
        .context("Windows.Media.Ocr OcrLine.Words failed")?;
    for word in ocr_words {
        let word_text = word
            .Text()
            .map(|value| value.to_string())
            .unwrap_or_default();
        let Ok(rect) = word.BoundingRect() else {
            continue;
        };
        words.push(mapping::MappedWord {
            text: word_text,
            x: rect.X,
            y: rect.Y,
            width: rect.Width,
            height: rect.Height,
        });
    }
    Ok(mapping::MappedLine { text, words })
}

#[cfg(windows)]
fn software_bitmap(image: &RgbaImage) -> Result<windows::Graphics::Imaging::SoftwareBitmap> {
    use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap};
    use windows::Media::Ocr::OcrEngine as MediaOcrEngine;
    use windows::Security::Cryptography::CryptographicBuffer;

    let width = image.width();
    let height = image.height();
    if width == 0 || height == 0 {
        bail!("cannot run Windows.Media.Ocr on an empty image");
    }
    let width_i =
        i32::try_from(width).with_context(|| format!("OCR image width {width} is too large"))?;
    let height_i =
        i32::try_from(height).with_context(|| format!("OCR image height {height} is too large"))?;
    let max_edge = MediaOcrEngine::MaxImageDimension().unwrap_or(0);
    if max_edge > 0 && (width > max_edge || height > max_edge) {
        bail!(
            "Windows.Media.Ocr accepts at most {max_edge} px per edge; the image is {width}x{height}"
        );
    }

    let bgra = mapping::rgba_to_bgra(image);
    let buffer = CryptographicBuffer::CreateFromByteArray(&bgra)
        .context("CryptographicBuffer::CreateFromByteArray failed")?;
    SoftwareBitmap::CreateCopyWithAlphaFromBuffer(
        &buffer,
        BitmapPixelFormat::Bgra8,
        width_i,
        height_i,
        BitmapAlphaMode::Premultiplied,
    )
    .context("SoftwareBitmap::CreateCopyWithAlphaFromBuffer failed")
}

/// WinRT `RecognizeAsync` needs an apartment on this thread.
/// `S_FALSE` / already-initialized errors are ignored so a GPUI worker can
/// call this more than once. The apartment is not uninitialized: this runs
/// on a thread-pool thread.
#[cfg(windows)]
fn ensure_winrt() {
    use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};

    // SAFETY: `RoInitialize` is the documented way to enter a WinRT apartment.
    // Failure means the thread already has an apartment (including STA); WinRT
    // calls may still succeed via marshalling.
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
}

#[cfg(test)]
mod tests {
    use super::mapping::{
        MappedLine, MappedWord, language_tag_candidates, language_tags_match, rgba_to_bgra,
        text_blocks_from_mapped_lines,
    };
    use super::*;
    use crate::TextBounds;

    /// Fixture shaped like two `OcrLine`s whose word `BoundingRect`s are in
    /// image pixels (top-left origin), matching Windows.Media.Ocr.
    fn sample_lines() -> Vec<MappedLine> {
        vec![
            MappedLine {
                text: "Hello world".into(),
                words: vec![
                    MappedWord {
                        text: "Hello".into(),
                        x: 10.0,
                        y: 20.0,
                        width: 40.0,
                        height: 12.0,
                    },
                    MappedWord {
                        text: "world".into(),
                        x: 55.0,
                        y: 18.0,
                        width: 42.0,
                        height: 16.0,
                    },
                ],
            },
            MappedLine {
                text: "SnapIt".into(),
                words: vec![MappedWord {
                    text: "SnapIt".into(),
                    x: 10.0,
                    y: 40.0,
                    width: 50.0,
                    height: 14.0,
                }],
            },
        ]
    }

    #[test]
    fn maps_ocr_line_word_rects_to_text_blocks() {
        let blocks = text_blocks_from_mapped_lines(sample_lines());
        assert_eq!(blocks.len(), 2);

        assert_eq!(blocks[0].text, "Hello world");
        // Union: x=10, y=18, right=97, bottom=34 → 87×16.
        assert_eq!(blocks[0].bounds, TextBounds::new(10.0, 18.0, 87.0, 16.0));
        assert!(blocks[0].confidence.is_none());

        assert_eq!(blocks[1].text, "SnapIt");
        assert_eq!(blocks[1].bounds, TextBounds::new(10.0, 40.0, 50.0, 14.0));
    }

    #[test]
    fn skips_empty_lines_and_joins_words_when_line_text_missing() {
        let lines = vec![
            MappedLine {
                text: "   ".into(),
                words: vec![],
            },
            MappedLine {
                text: String::new(),
                words: vec![
                    MappedWord {
                        text: "joined".into(),
                        x: 0.0,
                        y: 0.0,
                        width: 10.0,
                        height: 8.0,
                    },
                    MappedWord {
                        text: "words".into(),
                        x: 12.0,
                        y: 1.0,
                        width: 14.0,
                        height: 8.0,
                    },
                ],
            },
        ];
        let blocks = text_blocks_from_mapped_lines(lines);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "joined words");
        assert_eq!(blocks[0].bounds, TextBounds::new(0.0, 0.0, 26.0, 9.0));
    }

    #[test]
    fn language_tag_candidates_keep_chinese_script() {
        assert_eq!(language_tag_candidates("en-US"), ["en-US", "en"]);
        assert_eq!(language_tag_candidates("ja-JP"), ["ja-JP", "ja"]);
        assert_eq!(language_tag_candidates("zh-Hans"), ["zh-Hans"]);
        assert_eq!(language_tag_candidates("zh-Hant"), ["zh-Hant"]);
        assert_eq!(language_tag_candidates("ja"), ["ja"]);
        assert!(language_tag_candidates("  ").is_empty());
    }

    #[test]
    fn language_tags_match_english_and_chinese_variants() {
        assert!(language_tags_match("en-US", "en"));
        assert!(language_tags_match("ja-JP", "ja"));
        assert!(language_tags_match("zh-Hans", "zh-Hans"));
        assert!(language_tags_match("zh-CN", "zh-Hans"));
        assert!(!language_tags_match("zh-Hans", "zh-Hant"));
        assert!(!language_tags_match("fr-FR", "de"));
        assert!(!language_tags_match("zh-Hans", "en"));
        assert!(language_tags_match("en-SG", "en"));
    }

    #[test]
    fn rgba_to_bgra_swaps_channels() {
        let image = RgbaImage::from_raw(1, 1, vec![1, 2, 3, 4]).expect("1×1 RGBA");
        assert_eq!(rgba_to_bgra(&image), vec![3, 2, 1, 4]);
    }

    #[test]
    fn windows_engine_implements_ocr_engine() {
        fn assert_ocr_engine<T: OcrEngine>() {}
        assert_ocr_engine::<WindowsOcrEngine>();
    }

    #[cfg(not(windows))]
    #[test]
    fn windows_engine_unavailable_off_windows() {
        match WindowsOcrEngine::new() {
            Ok(_) => panic!("Windows.Media.Ocr should not construct off Windows"),
            Err(err) => assert!(format!("{err}").contains("Windows")),
        }
        assert_eq!(
            WindowsOcrEngine { _private: () }.name(),
            "Windows.Media.Ocr"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_media_ocr_constructs_or_explains() {
        match WindowsOcrEngine::new() {
            Ok(engine) => {
                assert_eq!(engine.name(), "Windows.Media.Ocr");
                let image = RgbaImage::from_pixel(8, 8, image::Rgba([255, 255, 255, 255]));
                let blocks = engine
                    .recognize(&image)
                    .expect("constructed Windows.Media.Ocr engine should recognize a bitmap");
                for block in blocks {
                    assert!(!block.text.trim().is_empty());
                    assert!(block.bounds.x.is_finite() && block.bounds.x >= 0.0);
                    assert!(block.bounds.y.is_finite() && block.bounds.y >= 0.0);
                    assert!(block.bounds.width.is_finite() && block.bounds.width >= 0.0);
                    assert!(block.bounds.height.is_finite() && block.bounds.height >= 0.0);
                }
            }
            Err(err) => {
                let msg = format!("{err:#}");
                assert!(
                    msg.contains("Windows.Media.Ocr") || msg.contains("language"),
                    "unexpected constructor error: {msg}"
                );
            }
        }
    }
}
