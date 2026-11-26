use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::fs::write_atomic;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePreference {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemePreference {
    pub const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|theme| theme.as_str() == value)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    #[default]
    Png,
    Jpg,
}

impl ImageFormat {
    pub const ALL: [Self; 2] = [Self::Png, Self::Jpg];

    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpg => "jpg",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "png" => Some(Self::Png),
            "jpg" | "jpeg" => Some(Self::Jpg),
            _ => None,
        }
    }

    pub fn from_path(path: &Path) -> Option<Self> {
        path.extension()
            .and_then(|ext| ext.to_str())
            .and_then(Self::parse)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct GeneralConfig {
    pub theme: ThemePreference,
    pub launch_at_login: bool,
    /// `auto` follows the system; otherwise `zh-CN` or `en`.
    pub language: String,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            theme: ThemePreference::System,
            launch_at_login: true,
            language: "auto".into(),
        }
    }
}

/// Global shortcuts in `global-hotkey` syntax, e.g. `Alt+Shift+A`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct Hotkeys {
    pub screenshot: String,
    pub scroll: String,
    pub ocr: String,
    /// Record an animated GIF.
    pub gif: String,
    /// Record an MP4 video.
    pub video: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            screenshot: "Alt+Shift+A".into(),
            scroll: "Alt+Shift+S".into(),
            ocr: "Alt+Shift+O".into(),
            gif: "Alt+Shift+G".into(),
            video: "Alt+Shift+V".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct CaptureConfig {
    /// `None` uses the platform pictures folder.
    pub save_dir: Option<PathBuf>,
    pub image_format: ImageFormat,
    pub jpeg_quality: u8,
    /// Hover highlights the window under the cursor; a click selects it.
    pub window_snap: bool,
    /// Prefer the smallest accessibility element under the cursor.
    pub element_snap: bool,
    /// Opacity (0-100) of the dim mask outside the selection. Higher values
    /// make the selection stand out more against its surroundings.
    pub mask_opacity: u8,
    /// Draw a soft drop shadow around the selection to separate it further
    /// from the dimmed background.
    pub selection_shadow: bool,
}

impl CaptureConfig {
    /// Element snapping needs window snapping and a platform accessibility API.
    pub fn element_snap_enabled(&self) -> bool {
        self.window_snap
            && self.element_snap
            && cfg!(any(target_os = "macos", target_os = "windows"))
    }
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            save_dir: None,
            image_format: ImageFormat::Png,
            jpeg_quality: 90,
            window_snap: true,
            element_snap: true,
            mask_opacity: 45,
            selection_shadow: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScrollSpeed {
    Slow,
    #[default]
    Medium,
    Fast,
}

impl ScrollSpeed {
    pub const ALL: [Self; 3] = [Self::Slow, Self::Medium, Self::Fast];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Slow => "slow",
            Self::Medium => "medium",
            Self::Fast => "fast",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|speed| speed.as_str() == value.trim().to_ascii_lowercase())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct ScrollConfig {
    pub auto_scroll_speed: ScrollSpeed,
    pub show_preview: bool,
}

impl Default for ScrollConfig {
    fn default() -> Self {
        Self {
            auto_scroll_speed: ScrollSpeed::Medium,
            show_preview: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct RecordingConfig {
    /// Frame rate for GIF and lossless APNG recordings.
    pub gif_fps: u32,
    /// Frame rate for MP4 recordings: a real video codec handles 30 fps well,
    /// so MP4 is not held back by the animation-friendly default.
    pub mp4_fps: u32,
    pub max_duration_secs: u32,
    /// Animation container used by the animation shortcut, tray and overlay.
    pub animation_format: String,
    pub captures_cursor: bool,
    pub click_effects: bool,
    /// Grouped click style identifier.
    pub click_effect: String,
    /// Capture and encode MP4 in HDR10 when the display is an HDR panel. GIF
    /// and APNG have no extended range, so this only applies to the MP4 container.
    pub hdr: bool,
    /// MP4 target bitrate in Kbps. None or zero uses the automatic formula.
    pub mp4_bitrate_kbps: Option<u32>,
}

impl RecordingConfig {
    pub const CLICK_EFFECTS: &'static [&'static str] = &[
        "ring", "dot", "outline", "cross", "burst", "ripple", "pulse", "sparkle", "firework",
        "comet", "confetti", "bloom", "aurora", "prism", "diamond", "corners", "orbit", "radar",
        "stardust", "rocket", "nebula", "rainbow",
    ];
    pub const FPS_RANGE: (u32, u32) = (1, 30);
    /// Zero means "record until stopped"; the upper bound guards against a
    /// hand-edited config leaving a runaway recording.
    pub const DURATION_RANGE: (u32, u32) = (0, 600);
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            gif_fps: 12,
            mp4_fps: 30,
            max_duration_secs: 0,
            animation_format: "gif".into(),
            captures_cursor: true,
            click_effects: true,
            click_effect: "ring".into(),
            hdr: true,
            mp4_bitrate_kbps: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct OcrConfig {
    /// `auto` or a BCP-47 tag such as `zh-Hans` / `en-US`.
    pub language: String,
}

impl Default for OcrConfig {
    fn default() -> Self {
        Self {
            language: "auto".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct HistoryConfig {
    pub enabled: bool,
    /// Entries to keep on disk. `0` keeps every capture.
    pub max_items: usize,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_items: 200,
        }
    }
}

/// One annotation tool's remembered defaults. `size` means line width for the
/// stroke tools, font size for text, circle radius for steps and block size for
/// mosaic; the valid range lives on [`AnnotationTool::size_range`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AnnotationStyle {
    #[serde(with = "hex_color")]
    pub color: u32,
    pub size: u32,
}

impl Default for AnnotationStyle {
    fn default() -> Self {
        AnnotationTool::Rect.default_style()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnnotationTool {
    #[default]
    Rect,
    Arrow,
    Line,
    Pen,
    Mosaic,
    Text,
    Step,
}

impl AnnotationTool {
    pub const ALL: [Self; 7] = [
        Self::Rect,
        Self::Arrow,
        Self::Line,
        Self::Pen,
        Self::Mosaic,
        Self::Text,
        Self::Step,
    ];

    pub fn default_style(self) -> AnnotationStyle {
        let size = match self {
            Self::Rect | Self::Arrow | Self::Line | Self::Pen => 4,
            Self::Mosaic => 12,
            Self::Text => 16,
            Self::Step => 12,
        };
        AnnotationStyle {
            color: 0xEF4444,
            size,
        }
    }

    /// Inclusive range accepted by the size slider and by `sanitize`.
    pub fn size_range(self) -> (u32, u32) {
        match self {
            Self::Rect | Self::Arrow | Self::Line | Self::Pen => (1, 24),
            Self::Mosaic => (4, 48),
            Self::Text => (10, 72),
            Self::Step => (8, 40),
        }
    }

    /// Mosaic pixelates the source bitmap; it has no paint color.
    pub fn uses_color(self) -> bool {
        !matches!(self, Self::Mosaic)
    }
}

fn default_rect() -> AnnotationStyle {
    AnnotationTool::Rect.default_style()
}
fn default_arrow() -> AnnotationStyle {
    AnnotationTool::Arrow.default_style()
}
fn default_line() -> AnnotationStyle {
    AnnotationTool::Line.default_style()
}
fn default_pen() -> AnnotationStyle {
    AnnotationTool::Pen.default_style()
}
fn default_mosaic() -> AnnotationStyle {
    AnnotationTool::Mosaic.default_style()
}
fn default_text() -> AnnotationStyle {
    AnnotationTool::Text.default_style()
}
fn default_step() -> AnnotationStyle {
    AnnotationTool::Step.default_style()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct AnnotationConfig {
    #[serde(default = "default_rect")]
    pub rect: AnnotationStyle,
    #[serde(default = "default_arrow")]
    pub arrow: AnnotationStyle,
    #[serde(default = "default_line")]
    pub line: AnnotationStyle,
    #[serde(default = "default_pen")]
    pub pen: AnnotationStyle,
    #[serde(default = "default_mosaic")]
    pub mosaic: AnnotationStyle,
    #[serde(default = "default_text")]
    pub text: AnnotationStyle,
    #[serde(default = "default_step")]
    pub step: AnnotationStyle,
}

impl AnnotationConfig {
    pub fn get(&self, tool: AnnotationTool) -> AnnotationStyle {
        match tool {
            AnnotationTool::Rect => self.rect,
            AnnotationTool::Arrow => self.arrow,
            AnnotationTool::Line => self.line,
            AnnotationTool::Pen => self.pen,
            AnnotationTool::Mosaic => self.mosaic,
            AnnotationTool::Text => self.text,
            AnnotationTool::Step => self.step,
        }
    }

    pub fn set(&mut self, tool: AnnotationTool, style: AnnotationStyle) {
        let slot = match tool {
            AnnotationTool::Rect => &mut self.rect,
            AnnotationTool::Arrow => &mut self.arrow,
            AnnotationTool::Line => &mut self.line,
            AnnotationTool::Pen => &mut self.pen,
            AnnotationTool::Mosaic => &mut self.mosaic,
            AnnotationTool::Text => &mut self.text,
            AnnotationTool::Step => &mut self.step,
        };
        *slot = style;
    }

    pub fn sanitize(&mut self) {
        for tool in AnnotationTool::ALL {
            let mut style = self.get(tool);
            let (min, max) = tool.size_range();
            style.size = style.size.clamp(min, max);
            // An unreadable colour keeps this tool's default and leaves the
            // rest of the file alone.
            if style.color == hex_color::INVALID {
                style.color = tool.default_style().color;
            }
            style.color &= 0x00FF_FFFF;
            self.set(tool, style);
        }
    }
}

impl Default for AnnotationConfig {
    fn default() -> Self {
        let style = |tool| AnnotationTool::default_style(tool);
        Self {
            rect: style(AnnotationTool::Rect),
            arrow: style(AnnotationTool::Arrow),
            line: style(AnnotationTool::Line),
            pen: style(AnnotationTool::Pen),
            mosaic: style(AnnotationTool::Mosaic),
            text: style(AnnotationTool::Text),
            step: style(AnnotationTool::Step),
        }
    }
}

/// Colors are stored as readable `#RRGGBB` strings rather than decimal RGB.
mod hex_color {
    use std::fmt;

    use serde::{Deserializer, Serializer, de::Visitor};

    /// A colour the file spelled wrong. [`super::AnnotationConfig::sanitize`]
    /// swaps it for the tool's default: a single bad line must not discard
    /// every other setting in the file.
    pub const INVALID: u32 = u32::MAX;

    pub fn serialize<S: Serializer>(color: &u32, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("#{:06X}", color & 0x00FF_FFFF))
    }

    /// Never fails: anything unreadable becomes [`INVALID`]. Only a broken
    /// file structure fails the parse now.
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
        deserializer.deserialize_any(Color)
    }

    struct Color;

    impl<'de> Visitor<'de> for Color {
        type Value = u32;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a #RRGGBB colour")
        }

        fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<u32, E> {
            let digits = text.strip_prefix('#').unwrap_or(text);
            if digits.len() != 6 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Ok(INVALID);
            }
            Ok(u32::from_str_radix(digits, 16).unwrap_or(INVALID))
        }

        /// A bare number is still a colour: hand-edited files often carry the
        /// decimal form of the same value.
        fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<u32, E> {
            Ok(if value <= 0x00FF_FFFF {
                value as u32
            } else {
                INVALID
            })
        }

        /// TOML hands over integers as `i64`; keep the same rule.
        fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<u32, E> {
            match u32::try_from(value) {
                Ok(value) => self.visit_u64(value as u64),
                Err(_) => Ok(INVALID),
            }
        }
    }
}

/// App updates from GitHub Releases.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct UpdateConfig {
    /// Check for a newer release in the background after launch.
    pub check: bool,
    /// Unix time of the last automatic check, so launches share one 24-hour window.
    pub last_check: Option<i64>,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            check: true,
            last_check: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WatermarkKind {
    #[default]
    Text,
    Image,
}

impl WatermarkKind {
    pub const ALL: [Self; 2] = [Self::Text, Self::Image];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == value.trim().to_ascii_lowercase())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
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

impl WatermarkPlacement {
    pub const ALL: [Self; 9] = [
        Self::TopLeft,
        Self::TopCenter,
        Self::TopRight,
        Self::MiddleLeft,
        Self::Center,
        Self::MiddleRight,
        Self::BottomLeft,
        Self::BottomCenter,
        Self::BottomRight,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::TopLeft => "top-left",
            Self::TopCenter => "top-center",
            Self::TopRight => "top-right",
            Self::MiddleLeft => "middle-left",
            Self::Center => "center",
            Self::MiddleRight => "middle-right",
            Self::BottomLeft => "bottom-left",
            Self::BottomCenter => "bottom-center",
            Self::BottomRight => "bottom-right",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|placement| placement.as_str() == value.trim().to_ascii_lowercase())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct TextWatermarkConfig {
    pub content: String,
    pub font_size: f32,
    #[serde(with = "hex_color")]
    pub color: u32,
    pub bold: bool,
    pub shadow: bool,
    pub stroke_width: f32,
    #[serde(with = "hex_color")]
    pub stroke_color: u32,
    pub background: bool,
    #[serde(with = "hex_color")]
    pub background_color: u32,
    pub background_opacity: f32,
}

impl Default for TextWatermarkConfig {
    fn default() -> Self {
        Self {
            content: "SnapIt".into(),
            font_size: 24.0,
            color: 0xFF_FF_FF,
            bold: false,
            shadow: false,
            stroke_width: 0.0,
            stroke_color: 0x00_00_00,
            background: false,
            background_color: 0x0F_17_2A,
            background_opacity: 0.55,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct ImageWatermarkConfig {
    /// Controlled file name inside [`AppDirs::watermark_dir`](crate::AppDirs::watermark_dir).
    pub source: String,
    /// Logical width in points/pixels before element and whole-stamp scaling.
    pub width: f32,
    /// Corner radius as a percentage of `min(width, height)` (`0` = square, `50` = circle).
    pub corner_radius: f32,
}

impl Default for ImageWatermarkConfig {
    fn default() -> Self {
        Self {
            source: String::new(),
            width: 120.0,
            corner_radius: 0.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct WatermarkElement {
    pub id: String,
    #[serde(deserialize_with = "deserialize_watermark_kind")]
    pub kind: WatermarkKind,
    pub x: f32,
    pub y: f32,
    pub rotation_degrees: f32,
    pub scale: f32,
    pub opacity: f32,
    pub text: TextWatermarkConfig,
    pub image: ImageWatermarkConfig,
}

impl Default for WatermarkElement {
    fn default() -> Self {
        Self {
            id: "el-1".into(),
            kind: WatermarkKind::Text,
            x: 0.0,
            y: 0.0,
            rotation_degrees: 0.0,
            scale: 1.0,
            opacity: 0.65,
            text: TextWatermarkConfig::default(),
            image: ImageWatermarkConfig::default(),
        }
    }
}

impl WatermarkElement {
    pub fn new_text(id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: WatermarkKind::Text,
            text: TextWatermarkConfig {
                content: content.into(),
                ..TextWatermarkConfig::default()
            },
            ..Self::default()
        }
    }

    pub fn new_image(id: impl Into<String>, source: impl Into<String>, width: f32) -> Self {
        Self {
            id: id.into(),
            kind: WatermarkKind::Image,
            image: ImageWatermarkConfig {
                source: source.into(),
                width,
                ..ImageWatermarkConfig::default()
            },
            ..Self::default()
        }
    }

    pub fn sanitize(&mut self, fallback_id: &str) {
        let trimmed = self.id.trim();
        self.id = if trimmed.is_empty() {
            fallback_id.into()
        } else {
            trimmed.chars().take(64).collect()
        };
        self.x = finite_or(self.x, 0.0).clamp(-4096.0, 4096.0);
        self.y = finite_or(self.y, 0.0).clamp(-4096.0, 4096.0);
        self.rotation_degrees = finite_or(self.rotation_degrees, 0.0).clamp(-180.0, 180.0);
        self.scale = finite_or(self.scale, 1.0).clamp(0.1, 8.0);
        self.opacity = finite_or(self.opacity, 0.65).clamp(0.0, 1.0);
        self.text.font_size = finite_or(self.text.font_size, 24.0).clamp(8.0, 256.0);
        if self.text.color == hex_color::INVALID {
            self.text.color = TextWatermarkConfig::default().color;
        }
        self.text.color &= 0x00FF_FFFF;
        self.text.stroke_width = finite_or(self.text.stroke_width, 0.0).clamp(0.0, 12.0);
        if self.text.stroke_color == hex_color::INVALID {
            self.text.stroke_color = TextWatermarkConfig::default().stroke_color;
        }
        self.text.stroke_color &= 0x00FF_FFFF;
        if self.text.background_color == hex_color::INVALID {
            self.text.background_color = TextWatermarkConfig::default().background_color;
        }
        self.text.background_color &= 0x00FF_FFFF;
        self.text.background_opacity =
            finite_or(self.text.background_opacity, 0.55).clamp(0.0, 1.0);
        if self.text.content.chars().count() > 512 {
            self.text.content = self.text.content.chars().take(512).collect();
        }
        if self.kind == WatermarkKind::Text && self.text.content.trim().is_empty() {
            self.text.content = TextWatermarkConfig::default().content;
        }
        self.image.width = finite_or(self.image.width, 120.0).clamp(8.0, 2048.0);
        self.image.corner_radius = finite_or(self.image.corner_radius, 0.0).clamp(0.0, 50.0);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatermarkTarget {
    Screenshot,
    Animation,
    Video,
}

impl WatermarkTarget {
    pub const ALL: [Self; 3] = [Self::Screenshot, Self::Animation, Self::Video];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Screenshot => "screenshot",
            Self::Animation => "animation",
            Self::Video => "video",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|target| target.as_str() == value.trim().to_ascii_lowercase())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct WatermarkPreset {
    pub elements: Vec<WatermarkElement>,
    #[serde(deserialize_with = "deserialize_watermark_placement")]
    pub placement: WatermarkPlacement,
    pub margin_x: f32,
    pub margin_y: f32,
    pub rotation_degrees: f32,
    pub scale: f32,
    pub tiled: bool,
    pub tile_spacing_x: f32,
    pub tile_spacing_y: f32,
}

impl Default for WatermarkPreset {
    fn default() -> Self {
        Self {
            elements: vec![WatermarkElement::default()],
            placement: WatermarkPlacement::BottomRight,
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

impl WatermarkPreset {
    pub const MAX_ELEMENTS: usize = 32;

    pub fn next_element_id(&self) -> String {
        let mut suffix = 1usize;
        loop {
            let candidate = format!("el-{suffix}");
            if self.elements.iter().all(|el| el.id != candidate) {
                return candidate;
            }
            suffix += 1;
        }
    }

    pub fn normalize_element_origins(&mut self) {
        if self.elements.is_empty() {
            return;
        }
        let min_x = self
            .elements
            .iter()
            .map(|el| el.x)
            .filter(|value| value.is_finite())
            .fold(f32::INFINITY, f32::min);
        let min_y = self
            .elements
            .iter()
            .map(|el| el.y)
            .filter(|value| value.is_finite())
            .fold(f32::INFINITY, f32::min);
        if min_x.is_finite() {
            for el in &mut self.elements {
                el.x -= min_x;
            }
        }
        if min_y.is_finite() {
            for el in &mut self.elements {
                el.y -= min_y;
            }
        }
    }

    pub fn sanitize(&mut self) {
        self.margin_x = finite_or(self.margin_x, 24.0).clamp(0.0, 2048.0);
        self.margin_y = finite_or(self.margin_y, 24.0).clamp(0.0, 2048.0);
        self.rotation_degrees = finite_or(self.rotation_degrees, 0.0).clamp(-180.0, 180.0);
        self.scale = finite_or(self.scale, 1.0).clamp(0.1, 8.0);
        self.tile_spacing_x = finite_or(self.tile_spacing_x, 160.0).clamp(8.0, 4096.0);
        self.tile_spacing_y = finite_or(self.tile_spacing_y, 96.0).clamp(8.0, 4096.0);
        self.elements.truncate(Self::MAX_ELEMENTS);
        if self.elements.is_empty() {
            self.elements.push(WatermarkElement::default());
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut next_suffix = 1usize;
        for (index, element) in self.elements.iter_mut().enumerate() {
            element.sanitize(&format!("el-{}", index + 1));
            if !seen.insert(element.id.clone()) {
                loop {
                    let candidate = format!("el-{next_suffix}");
                    next_suffix += 1;
                    if seen.insert(candidate.clone()) {
                        element.id = candidate;
                        break;
                    }
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct WatermarkTemplate {
    pub name: String,
    pub preset: WatermarkPreset,
}

impl Default for WatermarkTemplate {
    fn default() -> Self {
        Self {
            name: "SnapIt".into(),
            preset: WatermarkPreset::default(),
        }
    }
}

impl WatermarkTemplate {
    pub const MAX_NAME_CHARS: usize = 64;

    pub fn sanitize(&mut self) {
        let name = normalize_template_name(&self.name);
        self.name = if name.is_empty() {
            "Watermark".into()
        } else {
            name
        };
        self.preset.sanitize();
    }
}

fn normalize_template_name(name: &str) -> String {
    name.trim()
        .chars()
        .take(WatermarkTemplate::MAX_NAME_CHARS)
        .collect()
}

fn format_numbered_template_name(base: &str, suffix: usize) -> String {
    let suffix_str = format!(" {suffix}");
    let max_prefix = WatermarkTemplate::MAX_NAME_CHARS.saturating_sub(suffix_str.chars().count());
    let prefix: String = base.trim().chars().take(max_prefix).collect();
    let prefix = prefix.trim_end();
    if prefix.is_empty() {
        format!("Watermark{suffix_str}")
    } else {
        format!("{prefix}{suffix_str}")
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct WatermarkConfig {
    pub templates: BTreeMap<String, WatermarkTemplate>,
    pub screenshots_template: Option<String>,
    pub animations_template: Option<String>,
    pub videos_template: Option<String>,
}

impl Default for WatermarkConfig {
    fn default() -> Self {
        Self {
            templates: BTreeMap::from([("default".into(), WatermarkTemplate::default())]),
            screenshots_template: None,
            animations_template: None,
            videos_template: None,
        }
    }
}

impl WatermarkConfig {
    pub const DEFAULT_TEMPLATE_ID: &'static str = "default";

    /// A target is watermarked exactly when it has a template selected.
    pub fn target_enabled(&self, target: WatermarkTarget) -> bool {
        self.selected_template_id(target).is_some()
    }

    fn selection(&self, target: WatermarkTarget) -> &Option<String> {
        match target {
            WatermarkTarget::Screenshot => &self.screenshots_template,
            WatermarkTarget::Animation => &self.animations_template,
            WatermarkTarget::Video => &self.videos_template,
        }
    }

    fn selection_mut(&mut self, target: WatermarkTarget) -> &mut Option<String> {
        match target {
            WatermarkTarget::Screenshot => &mut self.screenshots_template,
            WatermarkTarget::Animation => &mut self.animations_template,
            WatermarkTarget::Video => &mut self.videos_template,
        }
    }

    pub fn selected_template_id(&self, target: WatermarkTarget) -> Option<&str> {
        self.selection(target).as_deref()
    }

    pub fn template(&self, target: WatermarkTarget) -> Option<&WatermarkTemplate> {
        self.selected_template_id(target)
            .and_then(|id| self.templates.get(id))
    }

    pub fn preset(&self, target: WatermarkTarget) -> Option<&WatermarkPreset> {
        self.template(target).map(|template| &template.preset)
    }

    pub fn template_mut(&mut self, id: &str) -> Option<&mut WatermarkTemplate> {
        self.templates.get_mut(id)
    }

    pub fn select_template(&mut self, target: WatermarkTarget, id: Option<String>) {
        if id
            .as_ref()
            .is_some_and(|id| !self.templates.contains_key(id))
        {
            return;
        }
        *self.selection_mut(target) = id;
    }

    pub fn template_name_exists(&self, name: &str, except: Option<&str>) -> bool {
        let name = normalize_template_name(name).to_lowercase();
        self.templates.iter().any(|(id, template)| {
            Some(id.as_str()) != except
                && normalize_template_name(&template.name).to_lowercase() == name
        })
    }

    /// `base`, or the first free `"base N"` when that name is taken.
    pub fn unique_template_name(&self, base: &str) -> String {
        let base = normalize_template_name(base);
        let base = if base.is_empty() {
            "Watermark"
        } else {
            base.as_str()
        };
        if !self.template_name_exists(base, None) {
            return base.to_string();
        }
        let mut suffix = 2usize;
        loop {
            let candidate = format_numbered_template_name(base, suffix);
            if !self.template_name_exists(&candidate, None) {
                return candidate;
            }
            suffix += 1;
        }
    }

    /// Choose a non-colliding copy name for `current` without producing `"X 2 2"`.
    pub fn next_duplicate_name(&self, current: &str) -> String {
        let current = normalize_template_name(current);
        let current_ref = if current.is_empty() {
            "Watermark"
        } else {
            current.as_str()
        };
        let (base, min_suffix) = match current_ref.rsplit_once(' ') {
            Some((stem, suffix))
                if !stem.trim_end().is_empty()
                    && suffix.parse::<usize>().is_ok_and(|n| n >= 2)
                    && self.template_name_exists(stem.trim_end(), None) =>
            {
                let stem = stem.trim_end();
                if let Ok(n) = suffix.parse::<usize>() {
                    (stem, n.saturating_add(1))
                } else {
                    (current_ref, 2)
                }
            }
            _ => (current_ref, 2),
        };
        let current_lower = current_ref.to_lowercase();
        let mut suffix = min_suffix;
        loop {
            let candidate = format_numbered_template_name(base, suffix);
            if candidate.to_lowercase() != current_lower
                && !self.template_name_exists(&candidate, None)
            {
                return candidate;
            }
            suffix += 1;
        }
    }

    pub fn duplicate_template(&mut self, source_id: &str) -> Option<String> {
        let source = self.templates.get(source_id)?.clone();
        let name = self.next_duplicate_name(&source.name);
        Some(self.save_template(None, name, source.preset))
    }

    pub fn rename_template(&mut self, id: &str, name: String) -> bool {
        let normalized = normalize_template_name(&name);
        if normalized.is_empty() || self.template_name_exists(&normalized, Some(id)) {
            return false;
        }
        let Some(template) = self.templates.get_mut(id) else {
            return false;
        };
        template.name = normalized;
        template.sanitize();
        true
    }

    /// Insert a template under `id` (or a generated one) without selecting it
    /// for any output; the Settings dropdown owns that choice.
    pub fn save_template(
        &mut self,
        id: Option<String>,
        name: String,
        preset: WatermarkPreset,
    ) -> String {
        let id = id
            .filter(|id| self.templates.contains_key(id))
            .unwrap_or_else(|| {
                let mut suffix = self.templates.len() + 1;
                loop {
                    let candidate = format!("watermark-{suffix}");
                    if !self.templates.contains_key(&candidate) {
                        break candidate;
                    }
                    suffix += 1;
                }
            });
        let mut template = WatermarkTemplate { name, preset };
        template.sanitize();
        self.templates.insert(id.clone(), template);
        id
    }

    /// Save the stamp composition from the designer. An existing template keeps
    /// its name (renaming is a separate action).
    pub fn save_template_design(
        &mut self,
        id: Option<String>,
        fallback_name: String,
        mut design: WatermarkPreset,
    ) -> String {
        design.normalize_element_origins();
        design.sanitize();
        if let Some(existing_id) = id.filter(|id| self.templates.contains_key(id)) {
            if let Some(template) = self.templates.get_mut(&existing_id) {
                template.preset = design;
                template.sanitize();
            }
            return existing_id;
        }
        let name = self.unique_template_name(&fallback_name);
        self.save_template(None, name, design)
    }

    pub fn delete_template(&mut self, id: &str) -> bool {
        if id == Self::DEFAULT_TEMPLATE_ID || self.templates.remove(id).is_none() {
            return false;
        }
        for target in WatermarkTarget::ALL {
            if self.selection(target).as_deref() == Some(id) {
                *self.selection_mut(target) = None;
            }
        }
        true
    }

    pub fn sanitize(&mut self) {
        if !self.templates.contains_key(Self::DEFAULT_TEMPLATE_ID) {
            self.templates.insert(
                Self::DEFAULT_TEMPLATE_ID.into(),
                WatermarkTemplate::default(),
            );
        }
        for template in self.templates.values_mut() {
            template.sanitize();
        }
        for target in WatermarkTarget::ALL {
            let stale = self
                .selection(target)
                .as_ref()
                .is_some_and(|id| !self.templates.contains_key(id));
            if stale {
                *self.selection_mut(target) = None;
            }
        }
    }
}

fn finite_or(value: f32, default: f32) -> f32 {
    if value.is_finite() { value } else { default }
}

fn deserialize_watermark_kind<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<WatermarkKind, D::Error> {
    let value = String::deserialize(deserializer)?;
    Ok(WatermarkKind::parse(&value).unwrap_or_default())
}

fn deserialize_watermark_placement<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<WatermarkPlacement, D::Error> {
    let value = String::deserialize(deserializer)?;
    Ok(WatermarkPlacement::parse(&value).unwrap_or(WatermarkPlacement::BottomRight))
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct Config {
    pub general: GeneralConfig,
    pub hotkeys: Hotkeys,
    pub capture: CaptureConfig,
    pub scroll: ScrollConfig,
    pub recording: RecordingConfig,
    pub ocr: OcrConfig,
    pub history: HistoryConfig,
    pub annotations: AnnotationConfig,
    pub watermark: WatermarkConfig,
    pub update: UpdateConfig,
}

impl Config {
    /// Clamp values a hand-edited file may have pushed out of range.
    pub fn sanitize(&mut self) {
        let (min_fps, max_fps) = RecordingConfig::FPS_RANGE;
        let (min_secs, max_secs) = RecordingConfig::DURATION_RANGE;
        self.recording.gif_fps = self.recording.gif_fps.clamp(min_fps, max_fps);
        self.recording.mp4_fps = self.recording.mp4_fps.clamp(min_fps, max_fps);
        self.recording.max_duration_secs =
            self.recording.max_duration_secs.clamp(min_secs, max_secs);
        let animation_format = self.recording.animation_format.trim().to_ascii_lowercase();
        self.recording.animation_format = if matches!(animation_format.as_str(), "gif" | "apng") {
            animation_format
        } else {
            RecordingConfig::default().animation_format
        };
        let click_effect = self.recording.click_effect.trim().to_ascii_lowercase();
        self.recording.click_effect = if RecordingConfig::CLICK_EFFECTS
            .iter()
            .any(|valid| *valid == click_effect)
        {
            click_effect
        } else {
            RecordingConfig::default().click_effect
        };
        self.capture.jpeg_quality = self.capture.jpeg_quality.clamp(1, 100);
        self.capture.mask_opacity = self.capture.mask_opacity.min(100);
        if !matches!(self.general.language.as_str(), "auto" | "zh-CN" | "en") {
            self.general.language = GeneralConfig::default().language;
        }
        if self.ocr.language.trim().is_empty() {
            self.ocr.language = OcrConfig::default().language;
        }
        if self
            .capture
            .save_dir
            .as_ref()
            .is_some_and(|dir| dir.as_os_str().is_empty())
        {
            self.capture.save_dir = None;
        }
        self.annotations.sanitize();
        self.watermark.sanitize();
    }

    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("failed to serialize settings")
    }

    pub fn from_toml(text: &str) -> Result<Self> {
        let mut config: Self = toml::from_str(text).context("invalid settings file")?;
        config.sanitize();
        Ok(config)
    }
}

/// Settings bound to their file. Every successful update is written back.
#[derive(Debug)]
pub struct ConfigStore {
    path: PathBuf,
    config: Config,
}

impl ConfigStore {
    /// Load `path`, creating it with defaults on first run. An unreadable file is
    /// moved aside to `*.bak` and replaced with defaults.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let (config, drop_legacy_keys) = match std::fs::read_to_string(&path) {
            Ok(text) => {
                // Drop removed keys (e.g. open_window_on_launch) so old configs
                // cannot revive behavior we stopped wiring.
                let drop_legacy_keys =
                    text.contains("open_window_on_launch") || text.contains("show_dock_icon");
                match Config::from_toml(&text) {
                    Ok(config) => (config, drop_legacy_keys),
                    Err(err) => {
                        let backup = path.with_extension("toml.bak");
                        log::warn!("{err:#}; moving {} to {}", path.display(), backup.display());
                        std::fs::rename(&path, &backup).ok();
                        (Config::default(), false)
                    }
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => (Config::default(), false),
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", path.display()));
            }
        };
        let store = Self { path, config };
        if drop_legacy_keys {
            log::info!("rewriting {} without legacy settings", store.path.display());
            store.save()?;
        } else if !store.path.exists() {
            store.save()?;
        }
        Ok(store)
    }

    /// In-memory settings when the config file cannot be used.
    pub fn detached(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            config: Config::default(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Apply `f`, sanitize, and persist if anything changed. Returns whether it did.
    pub fn update(&mut self, f: impl FnOnce(&mut Config)) -> Result<bool> {
        let mut next = self.config.clone();
        f(&mut next);
        next.sanitize();
        if next == self.config {
            return Ok(false);
        }
        Self::save_config(&self.path, &next)?;
        self.config = next;
        Ok(true)
    }

    pub fn save(&self) -> Result<()> {
        Self::save_config(&self.path, &self.config)
    }

    fn save_config(path: &Path, config: &Config) -> Result<()> {
        write_atomic(path, config.to_toml()?.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_opacity_is_clamped_and_shadow_round_trips() {
        let mut config =
            Config::from_toml("[capture]\nmask_opacity = 250\nselection_shadow = false\n").unwrap();
        assert_eq!(config.capture.mask_opacity, 100);
        assert!(!config.capture.selection_shadow);
        config.capture.mask_opacity = 45;
        config.capture.selection_shadow = true;
        let written = config.to_toml().unwrap();
        let reopened = Config::from_toml(&written).unwrap();
        assert_eq!(reopened.capture.mask_opacity, 45);
        assert!(reopened.capture.selection_shadow);
    }

    #[test]
    fn first_run_writes_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/config.toml");
        let store = ConfigStore::open(&path).unwrap();
        assert_eq!(store.config(), &Config::default());
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(Config::from_toml(&written).unwrap(), Config::default());
    }

    #[test]
    fn updates_persist_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut store = ConfigStore::open(&path).unwrap();
        let changed = store
            .update(|config| {
                config.general.theme = ThemePreference::Dark;
                config.hotkeys.screenshot = "Ctrl+Shift+S".into();
                config.capture.image_format = ImageFormat::Jpg;
                config.capture.save_dir = Some(dir.path().join("shots"));
                config.recording.gif_fps = 15;
                config.recording.mp4_fps = 24;
                config.recording.max_duration_secs = 60;
                config.ocr.language = "zh-Hans".into();
                config.history.max_items = 10;
            })
            .unwrap();
        assert!(changed);
        assert!(!store.update(|_| {}).unwrap());

        let reopened = ConfigStore::open(&path).unwrap();
        assert_eq!(reopened.config(), store.config());
        assert_eq!(reopened.config().general.theme, ThemePreference::Dark);
        assert_eq!(reopened.config().capture.image_format, ImageFormat::Jpg);
        assert_eq!(reopened.config().recording.gif_fps, 15);
        assert_eq!(reopened.config().recording.mp4_fps, 24);
    }

    #[test]
    fn failed_update_keeps_the_previous_config_in_memory_and_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut store = ConfigStore::open(&path).unwrap();
        let before = store.config().clone();
        let disk_before = std::fs::read_to_string(&path).unwrap();

        // `write_atomic` uses config.tmp for config.toml.
        std::fs::create_dir(path.with_extension("tmp")).unwrap();
        let result = store.update(|config| {
            config.general.theme = ThemePreference::Dark;
            config.history.enabled = false;
        });
        assert!(result.is_err());
        assert_eq!(store.config(), &before);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), disk_before);
    }

    #[test]
    fn annotation_defaults_cover_every_tool() {
        let config = AnnotationConfig::default();
        assert_eq!(AnnotationTool::ALL.len(), 7);
        for tool in AnnotationTool::ALL {
            assert_eq!(config.get(tool), tool.default_style());
        }
        let mut changed = config;
        for tool in AnnotationTool::ALL {
            changed.set(
                tool,
                AnnotationStyle {
                    color: 0x112233,
                    size: 9,
                },
            );
        }
        for tool in AnnotationTool::ALL {
            assert_eq!(changed.get(tool).color, 0x112233, "{tool:?} set");
            assert_eq!(changed.get(tool).size, 9, "{tool:?} set");
        }
        changed.sanitize();
        assert_eq!(changed.get(AnnotationTool::Line).size, 9);
    }

    #[test]
    fn missing_line_style_falls_back_to_defaults() {
        let config =
            Config::from_toml("[annotations.arrow]\ncolor = \"#00FF00\"\nsize = 8\n").unwrap();
        assert_eq!(config.annotations.arrow.color, 0x00FF00);
        assert_eq!(
            config.annotations.line,
            AnnotationTool::Line.default_style()
        );
    }

    #[test]
    fn a_bad_colour_only_costs_its_own_field() {
        let config = Config::from_toml(
            "[recording]\nmp4_fps = 20\n\
             [annotations.rect]\ncolor = \"ff00\"\nsize = 9\n\
             [annotations.arrow]\ncolor = 16711680\n",
        )
        .unwrap();
        // The rest of the file still loads instead of falling back wholesale.
        assert_eq!(config.recording.mp4_fps, 20);
        assert_eq!(config.recording.gif_fps, RecordingConfig::default().gif_fps);
        assert_eq!(config.annotations.rect.size, 9);
        assert_eq!(
            config.annotations.rect.color,
            AnnotationTool::Rect.default_style().color,
            "an unreadable colour keeps the tool default"
        );
        assert_eq!(
            config.annotations.arrow.color, 0x00_FF_00_00,
            "a decimal colour is still a colour"
        );
    }

    #[test]
    fn a_file_from_before_video_recording_still_binds_it() {
        // Older configs have no `video` key at all; the action must still be
        // reachable rather than silently unbound.
        let config =
            Config::from_toml("[hotkeys]\ngif = \"Alt+Shift+G\"\nocr = \"Alt+Shift+O\"\n").unwrap();
        assert_eq!(config.hotkeys.video, Hotkeys::default().video);
        assert_eq!(config.hotkeys.gif, "Alt+Shift+G");
    }

    #[test]
    fn partial_file_fills_defaults() {
        let config = Config::from_toml("[recording]\ngif_fps = 20\n").unwrap();
        assert_eq!(config.recording.gif_fps, 20);
        assert_eq!(config.recording.mp4_fps, RecordingConfig::default().mp4_fps);
        assert_eq!(
            config.recording.max_duration_secs,
            RecordingConfig::default().max_duration_secs
        );
        assert_eq!(config.hotkeys, Hotkeys::default());
    }

    #[test]
    fn recording_pointer_options_default_on_and_persist_explicit_choices() {
        let defaults = RecordingConfig::default();
        assert!(defaults.captures_cursor);
        assert!(defaults.click_effects);
        assert_eq!(defaults.animation_format, "gif");
        assert_eq!(defaults.click_effect, "ring");
        let old = Config::from_toml("[recording]\ngif_fps = 15\n").unwrap();
        assert!(old.recording.captures_cursor);
        assert!(old.recording.click_effects);
        assert_eq!(old.recording.animation_format, "gif");
        assert_eq!(old.recording.click_effect, "ring");

        let configured = Config::from_toml(
            "[recording]\ncaptures_cursor = false\nclick_effects = false\n\
             animation_format = \"apng\"\nclick_effect = \"burst\"\n",
        )
        .unwrap();
        assert!(!configured.recording.captures_cursor);
        assert!(!configured.recording.click_effects);
        assert_eq!(configured.recording.animation_format, "apng");
        assert_eq!(configured.recording.click_effect, "burst");
        let round_trip = toml::to_string(&configured).unwrap();
        let reloaded = Config::from_toml(&round_trip).unwrap();
        assert_eq!(reloaded.recording, configured.recording);
    }

    #[test]
    fn grouped_click_effects_round_trip_through_config() {
        for effect in RecordingConfig::CLICK_EFFECTS {
            let config =
                Config::from_toml(&format!("[recording]\nclick_effect = \"{effect}\"\n")).unwrap();
            assert_eq!(config.recording.click_effect, *effect);
        }
    }

    #[test]
    fn invalid_animation_and_click_options_fall_back_to_defaults() {
        let config = Config::from_toml(
            "[recording]\nanimation_format = \"webm\"\nclick_effect = \"unknown\"\n",
        )
        .unwrap();
        assert_eq!(config.recording.animation_format, "gif");
        assert_eq!(config.recording.click_effect, "ring");
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let config = Config::from_toml(
            "[recording]\ngif_fps = 500\nmp4_fps = 500\nmax_duration_secs = 9999\n[history]\nmax_items = 0\n",
        )
        .unwrap();
        assert_eq!(config.recording.gif_fps, 30);
        assert_eq!(config.recording.mp4_fps, 30);
        assert_eq!(config.recording.max_duration_secs, 600);
        // Zero is meaningful for history: it means "keep every capture".
        assert_eq!(config.history.max_items, 0);
    }

    #[test]
    fn zero_duration_means_unlimited() {
        let default = Config::default();
        assert_eq!(default.recording.max_duration_secs, 0);
        let explicit = Config::from_toml("[recording]\nmax_duration_secs = 0\n").unwrap();
        assert_eq!(explicit.recording.max_duration_secs, 0);
    }

    #[test]
    fn corrupt_file_is_backed_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "general = [").unwrap();
        let store = ConfigStore::open(&path).unwrap();
        assert_eq!(store.config(), &Config::default());
        assert!(dir.path().join("config.toml.bak").exists());
        assert!(path.exists());
    }

    #[test]
    fn legacy_general_keys_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[general]\ntheme = \"dark\"\nopen_window_on_launch = true\nshow_dock_icon = true\n",
        )
        .unwrap();
        let store = ConfigStore::open(&path).unwrap();
        assert_eq!(store.config().general.theme, ThemePreference::Dark);
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            !written.contains("open_window_on_launch"),
            "legacy key should be stripped on load: {written}"
        );
        assert!(
            !written.contains("show_dock_icon"),
            "removed key should be stripped on load: {written}"
        );
        assert_eq!(
            Config::from_toml(&written).unwrap().general.theme,
            ThemePreference::Dark
        );
    }

    #[test]
    fn animation_and_mp4_frame_rates_are_independent() {
        let mut config = Config::default();
        config.recording.gif_fps = 10;
        config.recording.mp4_fps = 28;
        let written = config.to_toml().unwrap();
        let reopened = Config::from_toml(&written).unwrap();
        assert_eq!(reopened.recording.gif_fps, 10);
        assert_eq!(reopened.recording.mp4_fps, 28);

        // The pre-split `fps` key is gone for good: it is ignored instead of
        // being folded into either container.
        let old = Config::from_toml("[recording]\nfps = 20\n").unwrap();
        assert_eq!(old.recording, RecordingConfig::default());
    }

    #[test]
    fn mp4_bitrate_is_optional_and_zero_is_automatic() {
        assert_eq!(RecordingConfig::default().mp4_bitrate_kbps, None);
        let missing = Config::from_toml("[recording]\ngif_fps = 20\n").unwrap();
        assert_eq!(missing.recording.mp4_bitrate_kbps, None);
        let zero = Config::from_toml("[recording]\nmp4_bitrate_kbps = 0\n").unwrap();
        assert_eq!(zero.recording.mp4_bitrate_kbps, Some(0));

        for kbps in [0, 1, 12_000, u32::MAX] {
            let mut config = Config::default();
            config.recording.mp4_bitrate_kbps = Some(kbps);
            let written = config.to_toml().unwrap();
            assert!(written.contains(&format!("mp4_bitrate_kbps = {kbps}")));
            let reopened = Config::from_toml(&written).unwrap();
            assert_eq!(reopened.recording.mp4_bitrate_kbps, Some(kbps));
        }

        let written = Config::default().to_toml().unwrap();
        assert!(!written.contains("mp4_bitrate_kbps"));
    }

    #[test]
    fn watermark_defaults_select_no_target() {
        let config = Config::from_toml("[recording]\ngif_fps = 20\n").unwrap();
        assert_eq!(config.watermark, WatermarkConfig::default());
        assert_eq!(
            config
                .watermark
                .selected_template_id(WatermarkTarget::Screenshot),
            None
        );
        assert_eq!(
            config
                .watermark
                .selected_template_id(WatermarkTarget::Animation),
            None
        );
        assert_eq!(
            config
                .watermark
                .selected_template_id(WatermarkTarget::Video),
            None
        );
        assert!(
            WatermarkTarget::ALL
                .into_iter()
                .all(|target| !config.watermark.target_enabled(target))
        );
        assert_eq!(config.watermark.templates.len(), 1);
        assert!(
            config
                .watermark
                .templates
                .contains_key(WatermarkConfig::DEFAULT_TEMPLATE_ID)
        );
    }

    #[test]
    fn watermark_application_targets_round_trip_independently() {
        let default_id = WatermarkConfig::DEFAULT_TEMPLATE_ID.to_string();
        for screenshots in [false, true] {
            for animations in [false, true] {
                for videos in [false, true] {
                    let mut config = Config::default();
                    let selections = [
                        (WatermarkTarget::Screenshot, screenshots),
                        (WatermarkTarget::Animation, animations),
                        (WatermarkTarget::Video, videos),
                    ];
                    for (target, selected) in selections {
                        let id = selected.then(|| default_id.clone());
                        config.watermark.select_template(target, id);
                    }

                    let reloaded = Config::from_toml(&config.to_toml().unwrap()).unwrap();
                    assert_eq!(reloaded.watermark, config.watermark);
                    for (target, selected) in selections {
                        assert_eq!(
                            reloaded.watermark.selected_template_id(target),
                            selected.then_some(default_id.as_str())
                        );
                        assert_eq!(reloaded.watermark.target_enabled(target), selected);
                    }
                    assert_eq!(
                        WatermarkTarget::ALL
                            .into_iter()
                            .any(|target| reloaded.watermark.target_enabled(target)),
                        screenshots || animations || videos
                    );
                }
            }
        }
    }

    #[test]
    fn watermark_templates_round_trip_and_unknown_preset_values_fall_back() {
        let config = Config::from_toml(
            "[watermark]\nscreenshots_template = \"brand\"\n\
             [watermark.templates.brand]\nname = \"Brand\"\n\
             [watermark.templates.brand.preset]\nplacement = \"top-center\"\n\
             margin_x = 12.5\nrotation_degrees = -15.0\nscale = 1.75\ntiled = true\n\
             [[watermark.templates.brand.preset.elements]]\nid = \"title\"\nkind = \"text\"\n\
             x = 10.0\ny = 20.0\nopacity = 0.5\n\
             [watermark.templates.brand.preset.elements.text]\ncontent = \"测试\"\n\
             font_size = 32.0\ncolor = \"#112233\"\n\
             [[watermark.templates.brand.preset.elements]]\nid = \"badge\"\nkind = \"image\"\n\
             x = 40.0\ny = 60.0\nopacity = 0.75\n\
             [watermark.templates.brand.preset.elements.image]\nsource = \"logo.png\"\nwidth = 96.0\n",
        )
        .unwrap();
        let preset = config
            .watermark
            .preset(WatermarkTarget::Screenshot)
            .unwrap();
        assert_eq!(preset.elements.len(), 2);
        assert_eq!(preset.elements[0].kind, WatermarkKind::Text);
        assert_eq!(preset.elements[0].text.content, "测试");
        assert_eq!(preset.elements[0].text.color, 0x11_22_33);
        assert_eq!(preset.elements[1].kind, WatermarkKind::Image);
        assert_eq!(preset.elements[1].image.source, "logo.png");
        assert_eq!(preset.elements[1].image.width, 96.0);
        assert_eq!(preset.placement, WatermarkPlacement::TopCenter);
        assert_eq!(preset.scale, 1.75);
        assert!(preset.tiled);
        assert_eq!(
            config
                .watermark
                .selected_template_id(WatermarkTarget::Screenshot),
            Some("brand")
        );
        assert_eq!(
            config
                .watermark
                .selected_template_id(WatermarkTarget::Animation),
            None
        );
        assert_eq!(
            config
                .watermark
                .selected_template_id(WatermarkTarget::Video),
            None
        );
        let reloaded = Config::from_toml(&config.to_toml().unwrap()).unwrap();
        assert_eq!(reloaded.watermark, config.watermark);

        let fallback = Config::from_toml(
            "[watermark.templates.brand.preset]\nplacement = \"diagonal\"\n\
             [[watermark.templates.brand.preset.elements]]\nkind = \"video\"\n\
             [watermark.templates.brand.preset.elements.text]\ncolor = \"nope\"\n",
        )
        .unwrap();
        let preset = fallback
            .watermark
            .templates
            .get("brand")
            .unwrap()
            .preset
            .clone();
        assert_eq!(preset.elements[0].kind, WatermarkKind::Text);
        assert_eq!(preset.placement, WatermarkPlacement::BottomRight);
        assert_eq!(
            preset.elements[0].text.color,
            TextWatermarkConfig::default().color
        );
    }

    #[test]
    fn legacy_inline_watermark_preset_is_not_migrated() {
        let config = Config::from_toml(
            "[watermark]\nenabled = true\nkind = \"image\"\ncontent = \"Old\"\n\
             margin_x = 2.0\napply_to_videos = true\n",
        )
        .unwrap();
        assert_eq!(
            config
                .watermark
                .selected_template_id(WatermarkTarget::Screenshot),
            None
        );
        assert_eq!(
            config
                .watermark
                .selected_template_id(WatermarkTarget::Animation),
            None
        );
        assert_eq!(
            config
                .watermark
                .selected_template_id(WatermarkTarget::Video),
            None
        );
        assert!(
            WatermarkTarget::ALL
                .into_iter()
                .all(|target| !config.watermark.target_enabled(target))
        );
        assert_eq!(
            config.watermark.templates,
            WatermarkConfig::default().templates
        );
        assert!(
            config
                .watermark
                .preset(WatermarkTarget::Screenshot)
                .is_none()
        );
    }

    #[test]
    fn watermark_template_non_finite_and_out_of_range_values_are_sanitized() {
        let mut config = Config::default();
        config.watermark.select_template(
            WatermarkTarget::Screenshot,
            Some(WatermarkConfig::DEFAULT_TEMPLATE_ID.into()),
        );
        let preset = &mut config
            .watermark
            .template_mut(WatermarkConfig::DEFAULT_TEMPLATE_ID)
            .unwrap()
            .preset;
        preset.margin_x = f32::NAN;
        preset.margin_y = f32::INFINITY;
        preset.rotation_degrees = f32::NEG_INFINITY;
        preset.scale = 100.0;
        preset.tile_spacing_x = -10.0;
        let mut second = WatermarkElement::new_image("el-1", "logo.png", f32::NAN);
        second.x = f32::INFINITY;
        second.y = -9999.0;
        second.opacity = -1.0;
        preset.elements[0].text.font_size = 1000.0;
        preset.elements[0].opacity = 4.0;
        preset.elements.push(second);
        config.sanitize();
        let preset = config
            .watermark
            .preset(WatermarkTarget::Screenshot)
            .unwrap();
        assert_eq!(preset.margin_x, WatermarkPreset::default().margin_x);
        assert_eq!(preset.margin_y, WatermarkPreset::default().margin_y);
        assert_eq!(preset.rotation_degrees, 0.0);
        assert_eq!(preset.scale, 8.0);
        assert_eq!(preset.tile_spacing_x, 8.0);
        assert_eq!(preset.elements[0].text.font_size, 256.0);
        assert_eq!(preset.elements[0].opacity, 1.0);
        assert_ne!(preset.elements[0].id, preset.elements[1].id);
        assert_eq!(preset.elements[1].image.width, 120.0);
        assert_eq!(preset.elements[1].x, 0.0);
        assert_eq!(preset.elements[1].y, -4096.0);
        assert_eq!(preset.elements[1].opacity, 0.0);
    }

    #[test]
    fn watermark_templates_are_selected_independently_per_target() {
        let mut watermark = WatermarkConfig::default();

        let mut screenshot = WatermarkPreset::default();
        screenshot.elements[0].text.content = "Screenshot".into();
        let screenshot_id = watermark.save_template(None, "Screenshot".into(), screenshot);

        let mut animation = WatermarkPreset::default();
        animation.elements[0].text.content = "Animation".into();
        let animation_id = watermark.save_template(None, "Animation".into(), animation);

        let mut video = WatermarkPreset::default();
        video.elements[0].text.content = "Video".into();
        let video_id = watermark.save_template(None, "Video".into(), video);

        // Saving a template never selects it; the choice belongs to each output.
        assert!(
            WatermarkTarget::ALL
                .into_iter()
                .all(|target| !watermark.target_enabled(target))
        );
        assert_eq!(
            watermark.selected_template_id(WatermarkTarget::Screenshot),
            None
        );

        watermark.select_template(WatermarkTarget::Screenshot, Some(screenshot_id.clone()));
        watermark.select_template(WatermarkTarget::Animation, Some(animation_id.clone()));
        watermark.select_template(WatermarkTarget::Video, Some(video_id.clone()));

        assert_eq!(
            watermark.selected_template_id(WatermarkTarget::Screenshot),
            Some(screenshot_id.as_str())
        );
        assert_eq!(
            watermark.selected_template_id(WatermarkTarget::Animation),
            Some(animation_id.as_str())
        );
        assert_eq!(
            watermark.selected_template_id(WatermarkTarget::Video),
            Some(video_id.as_str())
        );
        assert!(
            WatermarkTarget::ALL
                .into_iter()
                .any(|target| watermark.target_enabled(target))
        );
        assert_eq!(
            watermark
                .preset(WatermarkTarget::Screenshot)
                .unwrap()
                .elements[0]
                .text
                .content,
            "Screenshot"
        );
        assert_eq!(
            watermark
                .preset(WatermarkTarget::Animation)
                .unwrap()
                .elements[0]
                .text
                .content,
            "Animation"
        );
        assert_eq!(
            watermark.preset(WatermarkTarget::Video).unwrap().elements[0]
                .text
                .content,
            "Video"
        );

        // Deselecting one output leaves the others untouched; there is no shared fallback.
        watermark.select_template(WatermarkTarget::Animation, None);
        assert!(!watermark.target_enabled(WatermarkTarget::Animation));
        assert!(watermark.preset(WatermarkTarget::Animation).is_none());
        assert_eq!(
            watermark
                .preset(WatermarkTarget::Screenshot)
                .unwrap()
                .elements[0]
                .text
                .content,
            "Screenshot"
        );
        assert_eq!(
            watermark.preset(WatermarkTarget::Video).unwrap().elements[0]
                .text
                .content,
            "Video"
        );
    }

    #[test]
    fn watermark_rename_and_design_save_replace_preset() {
        let mut watermark = WatermarkConfig::default();
        let template = watermark
            .template_mut(WatermarkConfig::DEFAULT_TEMPLATE_ID)
            .unwrap();
        template.preset.placement = WatermarkPlacement::TopLeft;
        template.preset.margin_x = 40.0;
        template.preset.margin_y = 50.0;
        template.preset.tiled = true;
        template.preset.tile_spacing_x = 200.0;
        template.preset.tile_spacing_y = 140.0;

        assert!(watermark.rename_template(WatermarkConfig::DEFAULT_TEMPLATE_ID, "Primary".into()));
        assert!(!watermark.rename_template(WatermarkConfig::DEFAULT_TEMPLATE_ID, "  ".into()));

        let design = WatermarkPreset {
            elements: vec![
                WatermarkElement {
                    x: 20.0,
                    y: 30.0,
                    ..WatermarkElement::new_text("el-1", "Line A")
                },
                WatermarkElement {
                    x: 50.0,
                    y: 70.0,
                    ..WatermarkElement::new_text("el-2", "Line B")
                },
            ],
            rotation_degrees: 15.0,
            scale: 1.5,
            placement: WatermarkPlacement::Center,
            margin_x: 0.0,
            tiled: false,
            ..WatermarkPreset::default()
        };

        let saved_id = watermark.save_template_design(
            Some(WatermarkConfig::DEFAULT_TEMPLATE_ID.into()),
            "Ignored".into(),
            design,
        );
        assert_eq!(saved_id, WatermarkConfig::DEFAULT_TEMPLATE_ID);
        let updated = watermark
            .templates
            .get(WatermarkConfig::DEFAULT_TEMPLATE_ID)
            .unwrap();
        assert_eq!(updated.name, "Primary");
        assert_eq!(updated.preset.placement, WatermarkPlacement::Center);
        assert_eq!(updated.preset.margin_x, 0.0);
        assert_eq!(updated.preset.margin_y, 24.0);
        assert!(!updated.preset.tiled);
        assert_eq!(updated.preset.tile_spacing_x, 160.0);
        assert_eq!(updated.preset.tile_spacing_y, 96.0);
        assert_eq!(updated.preset.rotation_degrees, 15.0);
        assert_eq!(updated.preset.scale, 1.5);
        assert_eq!(updated.preset.elements[0].x, 0.0);
        assert_eq!(updated.preset.elements[0].y, 0.0);
        assert_eq!(updated.preset.elements[1].x, 30.0);
        assert_eq!(updated.preset.elements[1].y, 40.0);
        // Saving a design no longer selects it for any output.
        assert_eq!(
            watermark.selected_template_id(WatermarkTarget::Screenshot),
            None
        );
        assert!(
            WatermarkTarget::ALL
                .into_iter()
                .all(|target| !watermark.target_enabled(target))
        );
    }

    #[test]
    fn watermark_template_names_are_unique_and_delete_clears_references() {
        let mut watermark = WatermarkConfig::default();
        assert!(watermark.template_name_exists(" snapIT ", None));
        assert!(!watermark.template_name_exists("snapIT", Some("default")));
        let id = watermark.save_template(None, "Brand".into(), WatermarkPreset::default());
        assert!(watermark.template_name_exists("brand", None));
        assert!(!watermark.template_name_exists("brand", Some(&id)));
        assert_eq!(watermark.unique_template_name("Brand"), "Brand 2");
        // Saving does not automatically select the template for an output.
        assert_eq!(
            watermark.selected_template_id(WatermarkTarget::Screenshot),
            None
        );
        assert!(!watermark.delete_template(WatermarkConfig::DEFAULT_TEMPLATE_ID));

        watermark.select_template(WatermarkTarget::Screenshot, Some(id.clone()));
        assert_eq!(
            watermark.selected_template_id(WatermarkTarget::Screenshot),
            Some(id.as_str())
        );
        assert!(watermark.delete_template(&id));
        assert!(!watermark.templates.contains_key(&id));
        assert_eq!(
            watermark.selected_template_id(WatermarkTarget::Screenshot),
            None
        );
        assert!(
            WatermarkTarget::ALL
                .into_iter()
                .all(|target| !watermark.target_enabled(target))
        );
    }

    #[test]
    fn watermark_invalid_template_references_are_cleared_after_sanitize() {
        let mut watermark = WatermarkConfig {
            screenshots_template: Some("missing".into()),
            animations_template: Some("missing".into()),
            videos_template: Some("missing".into()),
            ..WatermarkConfig::default()
        };
        watermark.sanitize();
        for target in WatermarkTarget::ALL {
            assert_eq!(watermark.selected_template_id(target), None);
            assert!(!watermark.target_enabled(target));
        }
        assert!(
            WatermarkTarget::ALL
                .into_iter()
                .all(|target| !watermark.target_enabled(target))
        );
        assert!(
            watermark
                .templates
                .contains_key(WatermarkConfig::DEFAULT_TEMPLATE_ID)
        );
    }

    #[test]
    fn watermark_styles_duplicate_and_usages_work_correctly() {
        let mut watermark = WatermarkConfig::default();
        let text = &mut watermark
            .template_mut(WatermarkConfig::DEFAULT_TEMPLATE_ID)
            .unwrap()
            .preset
            .elements[0]
            .text;
        text.bold = true;
        text.shadow = true;
        text.stroke_width = 2.5;
        text.stroke_color = 0x11_22_33;
        text.background = true;
        text.background_color = 0x44_55_66;
        text.background_opacity = 0.8;

        let mut img = WatermarkElement::new_image("el-2", "badge.png", 64.0);
        img.image.corner_radius = 50.0;
        watermark
            .template_mut(WatermarkConfig::DEFAULT_TEMPLATE_ID)
            .unwrap()
            .preset
            .elements
            .push(img);

        // Which outputs use a template is now an explicit per-target selection.
        for target in WatermarkTarget::ALL {
            watermark.select_template(target, Some(WatermarkConfig::DEFAULT_TEMPLATE_ID.into()));
        }
        let default_id = WatermarkConfig::DEFAULT_TEMPLATE_ID;
        let usages = |config: &WatermarkConfig, id: &str| -> Vec<WatermarkTarget> {
            WatermarkTarget::ALL
                .into_iter()
                .filter(|target| config.selected_template_id(*target) == Some(id))
                .collect()
        };

        let config = Config {
            watermark: watermark.clone(),
            ..Config::default()
        };
        let round_tripped = Config::from_toml(&config.to_toml().unwrap()).unwrap();
        let loaded = round_tripped
            .watermark
            .preset(WatermarkTarget::Screenshot)
            .unwrap();
        assert!(loaded.elements[0].text.bold);
        assert!(loaded.elements[0].text.shadow);
        assert_eq!(loaded.elements[0].text.stroke_width, 2.5);
        assert_eq!(loaded.elements[0].text.stroke_color, 0x11_22_33);
        assert!(loaded.elements[0].text.background);
        assert_eq!(loaded.elements[0].text.background_color, 0x44_55_66);
        assert_eq!(loaded.elements[0].text.background_opacity, 0.8);
        assert_eq!(loaded.elements[1].image.corner_radius, 50.0);

        let mut bad = config.watermark.clone();
        let bad_el = &mut bad
            .template_mut(WatermarkConfig::DEFAULT_TEMPLATE_ID)
            .unwrap()
            .preset
            .elements[0];
        bad_el.text.stroke_width = 999.0;
        bad_el.text.background_opacity = f32::NAN;
        bad_el.text.stroke_color = hex_color::INVALID;
        bad_el.text.background_color = hex_color::INVALID;
        bad.template_mut(WatermarkConfig::DEFAULT_TEMPLATE_ID)
            .unwrap()
            .preset
            .elements[1]
            .image
            .corner_radius = 200.0;
        bad.sanitize();
        let sanitized = bad.preset(WatermarkTarget::Screenshot).unwrap();
        assert_eq!(sanitized.elements[0].text.stroke_width, 12.0);
        assert_eq!(sanitized.elements[0].text.background_opacity, 0.55);
        assert_eq!(
            sanitized.elements[0].text.stroke_color,
            TextWatermarkConfig::default().stroke_color
        );
        assert_eq!(
            sanitized.elements[0].text.background_color,
            TextWatermarkConfig::default().background_color
        );
        assert_eq!(sanitized.elements[1].image.corner_radius, 50.0);

        assert_eq!(
            usages(&watermark, default_id),
            WatermarkTarget::ALL.to_vec()
        );

        // Deselect one output, then duplicate: the copy is not selected anywhere.
        watermark.select_template(WatermarkTarget::Video, None);
        let dup_id = watermark
            .duplicate_template(WatermarkConfig::DEFAULT_TEMPLATE_ID)
            .unwrap();
        assert_eq!(watermark.templates.get(&dup_id).unwrap().name, "SnapIt 2");
        assert_eq!(
            usages(&watermark, default_id),
            vec![WatermarkTarget::Screenshot, WatermarkTarget::Animation]
        );
        assert_eq!(usages(&watermark, &dup_id), Vec::<WatermarkTarget>::new());
        watermark.select_template(WatermarkTarget::Video, Some(dup_id.clone()));
        assert_eq!(usages(&watermark, &dup_id), vec![WatermarkTarget::Video]);
    }

    #[test]
    fn scroll_config_and_hotkey_defaults_sanitize_and_round_trip() {
        let legacy = Config::from_toml("[hotkeys]\ngif = \"Alt+Shift+G\"\n").unwrap();
        assert_eq!(legacy.hotkeys.scroll, Hotkeys::default().scroll);
        assert_eq!(legacy.scroll, ScrollConfig::default());

        let mut custom =
            Config::from_toml("[scroll]\nauto_scroll_speed = \"fast\"\nshow_preview = false\n")
                .unwrap();
        assert_eq!(custom.scroll.auto_scroll_speed, ScrollSpeed::Fast);
        assert!(!custom.scroll.show_preview);

        custom.sanitize();

        let reloaded = Config::from_toml(&custom.to_toml().unwrap()).unwrap();
        assert_eq!(reloaded.scroll, custom.scroll);
    }

    #[test]
    fn watermark_name_dedup_and_rename_respect_64_char_limit() {
        let mut watermark = WatermarkConfig::default();
        let long_a = format!("{}A", "x".repeat(64));
        let long_b = format!("{}B", "x".repeat(64));
        let id_a = watermark.save_template(None, long_a.clone(), WatermarkPreset::default());
        assert_eq!(
            watermark.templates.get(&id_a).unwrap().name.chars().count(),
            WatermarkTemplate::MAX_NAME_CHARS
        );
        // Second name sharing the 64-char prefix must be recognized as duplicate.
        assert!(watermark.template_name_exists(&long_b, None));
        let id_b = watermark.save_template(None, "Other".into(), WatermarkPreset::default());
        assert!(!watermark.rename_template(&id_b, long_b));

        // Generating a unique name from a full-length 64-char name stays within 64 chars
        // and remains distinct after sanitize().
        let generated = watermark.unique_template_name(&long_a);
        assert!(generated.chars().count() <= WatermarkTemplate::MAX_NAME_CHARS);
        assert_ne!(generated, watermark.templates.get(&id_a).unwrap().name);
        assert!(!watermark.template_name_exists(&generated, None));
    }

    #[test]
    fn watermark_next_duplicate_name_avoids_repeated_suffixes() {
        let mut watermark = WatermarkConfig::default();
        let first_dup = watermark.next_duplicate_name("SnapIt");
        assert_eq!(first_dup, "SnapIt 2");
        let id2 = watermark.save_template(None, first_dup, WatermarkPreset::default());
        assert_eq!(watermark.next_duplicate_name("SnapIt 2"), "SnapIt 3");
        // Unsaved draft with an existing stem also increments cleanly instead of appending "2".
        watermark.delete_template(&id2);
        assert_eq!(watermark.next_duplicate_name("SnapIt 2"), "SnapIt 3");
        // Unsaved draft without an existing stem still avoids colliding with itself.
        assert_eq!(watermark.next_duplicate_name("Draft"), "Draft 2");
        // Trailing year numbers remain part of the base when no matching stem exists.
        assert_eq!(watermark.next_duplicate_name("Brand 2026"), "Brand 2026 2");
    }
}
