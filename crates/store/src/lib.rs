//! Persistent state: user settings and the capture history index.

mod config;
mod fs;
mod history;
mod paths;

pub use config::{
    AnnotationConfig, AnnotationStyle, AnnotationTool, CaptureConfig, Config, ConfigStore,
    GeneralConfig, HistoryConfig, Hotkeys, ImageFormat, OcrConfig, RecordingConfig,
    ThemePreference,
};
pub use history::{EntryKind, History, HistoryEntry, Limits, NewEntry};
pub use paths::AppDirs;
