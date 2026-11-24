//! Capture history as an app-wide entity: files are written off the UI thread,
//! the index is updated on it.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Result;
use gpui_kit::{App, AppContext as _, Context, Entity, Global, Task};
use snapit_capture::{HdrImage, RgbaImage, encode_png_hdr};
use snapit_store::{EntryKind, History, HistoryEntry, Limits, NewEntry};

use crate::settings::AppSettings;

pub struct HistoryStore {
    dir: PathBuf,
    history: Option<History>,
    error: Option<String>,
}

struct GlobalHistory(Entity<HistoryStore>);

impl Global for GlobalHistory {}

impl HistoryStore {
    pub fn init(dir: PathBuf, cx: &mut App) -> Entity<Self> {
        let (history, error) = match History::open(&dir) {
            Ok(history) => (Some(history), None),
            Err(err) => {
                log::error!("history unavailable: {err:#}");
                (None, Some(format!("{err:#}")))
            }
        };
        let store = cx.new(|cx| {
            let mut store = Self {
                dir,
                history,
                error,
            };
            store.enforce_limits(cx);
            store
        });
        cx.set_global(GlobalHistory(store.clone()));
        store
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalHistory>().0.clone()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn entries(&self) -> impl Iterator<Item = &HistoryEntry> {
        self.history.iter().flat_map(|history| history.entries())
    }

    pub fn len(&self) -> usize {
        self.history.as_ref().map_or(0, History::len)
    }

    pub fn total_bytes(&self) -> u64 {
        self.history.as_ref().map_or(0, History::total_bytes)
    }

    pub fn file_path(&self, entry: &HistoryEntry) -> PathBuf {
        self.dir.join(&entry.file)
    }

    pub fn thumbnail_path(&self, entry: &HistoryEntry) -> PathBuf {
        self.dir
            .join(entry.thumbnail.as_deref().unwrap_or(&entry.file))
    }

    /// Newest capture, preferring the original PNG; GIFs use their PNG thumbnail
    /// so pinning does not need a GIF decoder.
    pub fn latest_image(&self) -> Option<(PathBuf, f32)> {
        self.entries().next().map(|entry| self.pin_source(entry))
    }

    pub fn pin_source(&self, entry: &HistoryEntry) -> (PathBuf, f32) {
        if entry.kind.is_media() {
            // GIF and MP4 are pinned as their first-frame still, so pinning
            // never needs to decode a video.
            (self.thumbnail_path(entry), 1.0)
        } else {
            (self.file_path(entry), entry.scale_factor)
        }
    }

    fn limits(cx: &App) -> Option<Limits> {
        let config = &AppSettings::get(cx).history;
        // `0` means "keep every capture"; the store expresses that as an
        // unbounded count limit rather than a special case of its own.
        let max_items = if config.max_items == 0 {
            usize::MAX
        } else {
            config.max_items
        };
        config.enabled.then(|| Limits::new(max_items, u64::MAX))
    }

    /// `hdr` is the 16-bit source of `image`; when present it is encoded and
    /// becomes the stored original so history matches what Save/Copy produce.
    /// Encoding happens on the background task, never on the UI thread.
    pub fn add_image(
        &mut self,
        image: Arc<RgbaImage>,
        hdr: Option<Arc<HdrImage>>,
        scale_factor: f32,
        ocr_text: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if Self::limits(cx).is_none() || self.history.is_none() {
            return;
        }
        let dir = self.dir.clone();
        let task = cx.background_spawn(async move {
            match hdr {
                Some(hdr) => {
                    log::info!(
                        "history: storing HDR original {}x{} (16-bit BT.2020/PQ)",
                        hdr.width(),
                        hdr.height()
                    );
                    match encode_png_hdr(&hdr) {
                        Ok(bytes) => {
                            NewEntry::image_hdr(&dir, &bytes, &image, scale_factor, ocr_text)
                        }
                        Err(err) => {
                            log::warn!(
                                "failed to encode HDR PNG for history, storing SDR: {err:#}"
                            );
                            NewEntry::image(&dir, &image, scale_factor, ocr_text)
                        }
                    }
                }
                None => {
                    log::info!("history: storing SDR original (no HDR frame)");
                    NewEntry::image(&dir, &image, scale_factor, ocr_text)
                }
            }
        });
        self.insert_when_ready(task, cx).detach();
    }

    /// The returned task finishes once `path` has been copied, so the caller
    /// may move or delete it afterwards.
    pub fn add_recording(
        &mut self,
        path: PathBuf,
        kind: EntryKind,
        preview: RgbaImage,
        scale_factor: f32,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        if Self::limits(cx).is_none() || self.history.is_none() {
            return Task::ready(());
        }
        let dir = self.dir.clone();
        let task = cx.background_spawn(async move {
            NewEntry::file(&dir, &path, kind, &preview, scale_factor)
        });
        self.insert_when_ready(task, cx)
    }

    fn insert_when_ready(
        &mut self,
        task: Task<Result<NewEntry>>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            let entry = task.await;
            this.update(cx, |this, cx| {
                let result = entry.and_then(|entry| {
                    let limits = Self::limits(cx).unwrap_or(Limits::new(usize::MAX, u64::MAX));
                    match this.history.as_mut() {
                        Some(history) => history.insert(entry, limits).map(|_| ()),
                        None => Ok(()),
                    }
                });
                if let Err(err) = result {
                    log::error!("failed to add history entry: {err:#}");
                }
                cx.notify();
            })
            .ok();
        })
    }

    pub fn remove(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(history) = self.history.as_mut()
            && let Err(err) = history.remove(id)
        {
            log::error!("failed to delete history entry: {err:#}");
        }
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        if let Some(history) = self.history.as_mut()
            && let Err(err) = history.clear()
        {
            log::error!("failed to clear history: {err:#}");
        }
        cx.notify();
    }

    pub fn enforce_limits(&mut self, cx: &mut Context<Self>) {
        let Some(limits) = Self::limits(cx) else {
            return;
        };
        if let Some(history) = self.history.as_mut() {
            match history.prune(limits) {
                Ok(pruned) if !pruned.is_empty() => {
                    log::info!("pruned {} history entries", pruned.len());
                    cx.notify();
                }
                Ok(_) => {}
                Err(err) => log::error!("failed to prune history: {err:#}"),
            }
        }
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let bytes = bytes as f64;
    if bytes < KB {
        format!("{bytes} B")
    } else if bytes < KB * KB {
        format!("{:.0} KB", bytes / KB)
    } else {
        format!("{:.1} MB", bytes / KB / KB)
    }
}
