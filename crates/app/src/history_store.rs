//! Capture history as an app-wide entity: files are written off the UI thread,
//! the index is updated on it.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Result;
use gpui_kit::{App, AppContext as _, Context, Entity, Global, Task};
use snapit_capture::RgbaImage;
use snapit_store::{EntryKind, History, HistoryEntry, Limits, NewEntry};

use crate::export::ExportComposition;
use crate::settings::AppSettings;

pub struct HistoryStore {
    dir: PathBuf,
    history: Option<History>,
    error: Option<String>,
    generation: u64,
    history_enabled: bool,
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
        let history_enabled = AppSettings::get(cx).history.enabled;
        let store = cx.new(|cx| {
            let mut store = Self {
                dir,
                history,
                error,
                generation: 0,
                history_enabled,
            };
            store.enforce_limits(cx);
            store
        });
        cx.set_global(GlobalHistory(store.clone()));
        cx.observe_global::<AppSettings>(move |cx| {
            HistoryStore::global(cx).update(cx, |store, cx| {
                if store.sync_history_enabled(cx) {
                    cx.notify();
                }
            });
        })
        .detach();
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
        self.safe_entry_path(entry, false)
    }

    pub fn thumbnail_path(&self, entry: &HistoryEntry) -> PathBuf {
        self.safe_entry_path(entry, true)
    }

    fn safe_entry_path(&self, entry: &HistoryEntry, thumbnail: bool) -> PathBuf {
        let resolved = self.history.as_ref().and_then(|history| {
            let result = if thumbnail {
                history.thumbnail_path(entry)
            } else {
                history.file_path(entry)
            };
            match result {
                Ok(path) => Some(path),
                Err(err) => {
                    log::warn!("ignoring unsafe history path for {}: {err:#}", entry.id);
                    None
                }
            }
        });
        resolved.unwrap_or_else(|| self.dir.join(".invalid-history-entry"))
    }

    /// Newest capture, preferring the original PNG; animations use their PNG
    /// thumbnail so pinning does not need an animation decoder.
    pub fn latest_image(&self) -> Option<(PathBuf, f32)> {
        self.entries().next().map(|entry| self.pin_source(entry))
    }

    pub fn pin_source(&self, entry: &HistoryEntry) -> (PathBuf, f32) {
        if entry.kind.is_media() {
            // Animations and videos are pinned as their first-frame still.
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

    /// Track enable/disable transitions so work started before history was
    /// disabled cannot reappear if the setting is re-enabled before it finishes.
    fn sync_history_enabled(&mut self, cx: &App) -> bool {
        let enabled = AppSettings::get(cx).history.enabled;
        let changed = self.history_enabled && !enabled;
        if changed {
            self.invalidate_pending();
            log::info!("history disabled; invalidating pending inserts");
        }
        self.history_enabled = enabled;
        changed
    }

    fn invalidate_pending(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    fn current_limits(&mut self, cx: &App) -> Option<Limits> {
        self.sync_history_enabled(cx);
        Self::limits(cx)
    }

    /// Store the same composed pixels as Copy and Save, including HDR originals.
    pub fn add_image(
        &mut self,
        composition: Arc<ExportComposition>,
        scale_factor: f32,
        ocr_text: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.current_limits(cx).is_none() || self.history.is_none() {
            return;
        }
        let generation = self.generation;
        let dir = self.dir.clone();
        let task = cx.background_spawn(async move {
            let image = composition.sdr_preview().map_err(anyhow::Error::msg)?;
            if composition.has_hdr_original() {
                log::info!("history: storing shared HDR original (16-bit BT.2020/PQ)");
                let png = composition.png().map_err(anyhow::Error::msg)?;
                NewEntry::image_hdr(&dir, &png, &image, scale_factor, ocr_text)
            } else {
                log::info!("history: storing composed SDR original");
                NewEntry::image(&dir, &image, scale_factor, ocr_text)
            }
        });
        self.insert_when_ready(task, generation, cx).detach();
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
        if self.current_limits(cx).is_none() || self.history.is_none() {
            return Task::ready(());
        }
        let generation = self.generation;
        let dir = self.dir.clone();
        let task = cx.background_spawn(async move {
            NewEntry::file(&dir, &path, kind, &preview, scale_factor)
        });
        self.insert_when_ready(task, generation, cx)
    }

    fn insert_when_ready(
        &mut self,
        task: Task<Result<NewEntry>>,
        generation: u64,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this, cx| {
            let entry = task.await;
            this.update(cx, |this, cx| {
                this.sync_history_enabled(cx);
                let limits = Self::limits(cx);
                match entry {
                    Ok(entry)
                        if insertion_is_current(
                            generation,
                            this.generation,
                            this.history.is_some(),
                            limits,
                        ) =>
                    {
                        let limits = limits.expect("limits checked above");
                        if let Some(history) = this.history.as_mut()
                            && let Err(err) = history.insert(entry, limits)
                        {
                            log::error!("failed to add history entry: {err:#}");
                        }
                    }
                    Ok(entry) => {
                        log::info!(
                            "discarding stale history entry {} (task generation {generation}, current {})",
                            entry.entry().id,
                            this.generation
                        );
                        if let Some(history) = this.history.as_ref() {
                            history.delete_files(entry.entry());
                        }
                    }
                    Err(err) => log::error!("failed to prepare history entry: {err:#}"),
                }
                cx.notify();
            })
            .ok();
        })
    }

    pub fn remove(&mut self, id: &str, cx: &mut Context<Self>) {
        self.sync_history_enabled(cx);
        let removed = match self.history.as_mut() {
            Some(history) => history.remove(id),
            None => Ok(None),
        };
        match removed {
            Ok(Some(_)) | Ok(None) => {}
            Err(err) => log::error!("failed to delete history entry: {err:#}"),
        }
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.sync_history_enabled(cx);
        // Clear invalidates outstanding work even if the index write fails: a
        // task started before the user's clear must never reappear afterwards.
        self.invalidate_pending();
        if let Some(history) = self.history.as_mut()
            && let Err(err) = history.clear()
        {
            log::error!("failed to clear history: {err:#}");
        }
        cx.notify();
    }

    pub fn enforce_limits(&mut self, cx: &mut Context<Self>) {
        let Some(limits) = self.current_limits(cx) else {
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

fn insertion_is_current(
    generation: u64,
    current_generation: u64,
    history_available: bool,
    limits: Option<Limits>,
) -> bool {
    generation == current_generation && history_available && limits.is_some()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_insert_requires_current_generation_and_enabled_history() {
        let limits = Limits::new(10, u64::MAX);
        assert!(insertion_is_current(0, 0, true, Some(limits)));
        assert!(!insertion_is_current(0, 1, true, Some(limits)));
        assert!(!insertion_is_current(0, 0, false, Some(limits)));
        // Disabled history must never fall back to an unlimited insert.
        assert!(!insertion_is_current(0, 0, true, None));
    }
}
