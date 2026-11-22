use std::path::{Path, PathBuf};

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct RecordingConfig {
    /// Frame rate for GIF recordings: a palette-based container where small
    /// files matter more than smoothness.
    pub gif_fps: u32,
    /// Frame rate for MP4 recordings: a real video codec handles 30 fps well,
    /// so MP4 is not held back by the GIF-friendly default.
    pub mp4_fps: u32,
    pub max_duration_secs: u32,
    /// Container recorded last, so the overlay's single record button repeats
    /// it on a plain click. `"gif"` or `"mp4"`; anything else reads as GIF.
    pub last_format: String,
    pub captures_cursor: bool,
}

impl RecordingConfig {
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
            last_format: "gif".into(),
            captures_cursor: false,
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

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct Config {
    pub general: GeneralConfig,
    pub hotkeys: Hotkeys,
    pub capture: CaptureConfig,
    pub recording: RecordingConfig,
    pub ocr: OcrConfig,
    pub history: HistoryConfig,
    pub annotations: AnnotationConfig,
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
        self.config = next;
        self.save()?;
        Ok(true)
    }

    pub fn save(&self) -> Result<()> {
        write_atomic(&self.path, self.config.to_toml()?.as_bytes())
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
    fn gif_and_mp4_frame_rates_are_independent() {
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
}
