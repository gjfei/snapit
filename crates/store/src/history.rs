use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use chrono::{DateTime, Local, Utc};
use image::RgbaImage;
use serde::{Deserialize, Serialize};

use crate::fs::write_atomic;

const INDEX_FILE: &str = "index.json";
const THUMBNAIL_SIZE: u32 = 320;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    Png,
    Gif,
    Mp4,
}

impl EntryKind {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Gif => "gif",
            Self::Mp4 => "mp4",
        }
    }

    /// Whether the entry is a video container that needs a still thumbnail
    /// instead of being decodable as an image.
    pub fn is_media(self) -> bool {
        matches!(self, Self::Gif | Self::Mp4)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct HistoryEntry {
    pub id: String,
    pub kind: EntryKind,
    pub created_at: DateTime<Utc>,
    /// File name inside the history directory.
    pub file: String,
    pub thumbnail: Option<String>,
    pub width: u32,
    pub height: u32,
    /// Size of the file plus its thumbnail.
    pub bytes: u64,
    /// Physical pixels per logical pixel at capture time.
    #[serde(default = "default_scale")]
    pub scale_factor: f32,
    #[serde(default)]
    pub ocr_text: Option<String>,
}

fn default_scale() -> f32 {
    1.0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_items: usize,
    pub max_bytes: u64,
}

impl Limits {
    pub fn new(max_items: usize, max_bytes: u64) -> Self {
        Self {
            max_items: max_items.max(1),
            max_bytes: max_bytes.max(1),
        }
    }
}

/// Files written to the history directory but not yet indexed. Building one is
/// blocking I/O; call it off the UI thread and then [`History::insert`] it.
#[derive(Debug)]
pub struct NewEntry(HistoryEntry);

impl NewEntry {
    pub fn image(
        dir: &Path,
        image: &RgbaImage,
        scale_factor: f32,
        ocr_text: Option<String>,
    ) -> Result<Self> {
        let (id, file) = reserve(dir, EntryKind::Png)?;
        image
            .save_with_format(dir.join(&file), image::ImageFormat::Png)
            .with_context(|| format!("failed to write {file}"))?;
        Self::finish(dir, id, EntryKind::Png, file, image, scale_factor, ocr_text)
    }

    /// Copy an existing file (e.g. a finished GIF) into the history directory.
    pub fn file(
        dir: &Path,
        source: &Path,
        kind: EntryKind,
        preview: &RgbaImage,
        scale_factor: f32,
    ) -> Result<Self> {
        let (id, file) = reserve(dir, kind)?;
        std::fs::copy(source, dir.join(&file))
            .with_context(|| format!("failed to copy {}", source.display()))?;
        Self::finish(dir, id, kind, file, preview, scale_factor, None)
    }

    fn finish(
        dir: &Path,
        id: String,
        kind: EntryKind,
        file: String,
        preview: &RgbaImage,
        scale_factor: f32,
        ocr_text: Option<String>,
    ) -> Result<Self> {
        let thumbnail = format!("{id}.thumb.png");
        let (width, height) = preview.dimensions();
        let scale = (THUMBNAIL_SIZE as f32 / width.max(height).max(1) as f32).min(1.0);
        let thumb = image::imageops::thumbnail(
            preview,
            ((width as f32 * scale).round() as u32).max(1),
            ((height as f32 * scale).round() as u32).max(1),
        );
        thumb
            .save_with_format(dir.join(&thumbnail), image::ImageFormat::Png)
            .with_context(|| format!("failed to write {thumbnail}"))?;
        let bytes = file_size(&dir.join(&file)) + file_size(&dir.join(&thumbnail));
        Ok(Self(HistoryEntry {
            id,
            kind,
            created_at: Utc::now(),
            file,
            thumbnail: Some(thumbnail),
            width,
            height,
            bytes,
            scale_factor: if scale_factor > 0.0 {
                scale_factor
            } else {
                1.0
            },
            ocr_text: ocr_text.filter(|text| !text.is_empty()),
        }))
    }

    pub fn entry(&self) -> &HistoryEntry {
        &self.0
    }

    #[cfg(test)]
    fn with_created_at(mut self, created_at: DateTime<Utc>) -> Self {
        self.0.created_at = created_at;
        self
    }
}

fn reserve(dir: &Path, kind: EntryKind) -> Result<(String, String)> {
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let base = Local::now().format("%Y%m%d-%H%M%S-%3f").to_string();
    let mut id = base.clone();
    let mut n = 1;
    while dir.join(format!("{id}.{}", kind.extension())).exists() {
        id = format!("{base}-{n}");
        n += 1;
    }
    let file = format!("{id}.{}", kind.extension());
    Ok((id, file))
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

/// The capture history: files in one directory plus a JSON index, oldest first.
#[derive(Debug)]
pub struct History {
    dir: PathBuf,
    entries: Vec<HistoryEntry>,
}

impl History {
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
        let index = dir.join(INDEX_FILE);
        let mut entries: Vec<HistoryEntry> = match std::fs::read(&index) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|err| {
                log::warn!("ignoring corrupt history index {}: {err}", index.display());
                Vec::new()
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", index.display()));
            }
        };
        let before = entries.len();
        entries.retain(|entry| dir.join(&entry.file).exists());
        entries.sort_by_key(|entry| entry.created_at);
        let history = Self { dir, entries };
        if history.entries.len() != before {
            history.save()?;
        }
        Ok(history)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|entry| entry.bytes).sum()
    }

    /// Newest first.
    pub fn entries(&self) -> impl DoubleEndedIterator<Item = &HistoryEntry> {
        self.entries.iter().rev()
    }

    pub fn get(&self, id: &str) -> Option<&HistoryEntry> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    pub fn latest(&self, kind: EntryKind) -> Option<&HistoryEntry> {
        self.entries().find(|entry| entry.kind == kind)
    }

    pub fn file_path(&self, entry: &HistoryEntry) -> PathBuf {
        self.dir.join(&entry.file)
    }

    pub fn thumbnail_path(&self, entry: &HistoryEntry) -> PathBuf {
        self.dir
            .join(entry.thumbnail.as_deref().unwrap_or(&entry.file))
    }

    /// Index a new entry and prune to `limits`. Returns the pruned entries.
    pub fn insert(&mut self, entry: NewEntry, limits: Limits) -> Result<Vec<HistoryEntry>> {
        self.entries.push(entry.0);
        self.entries.sort_by_key(|entry| entry.created_at);
        let pruned = self.prune_entries(limits);
        self.save()?;
        Ok(pruned)
    }

    /// Drop the oldest entries until both caps hold. The newest entry is always kept.
    pub fn prune(&mut self, limits: Limits) -> Result<Vec<HistoryEntry>> {
        let pruned = self.prune_entries(limits);
        if !pruned.is_empty() {
            self.save()?;
        }
        Ok(pruned)
    }

    fn prune_entries(&mut self, limits: Limits) -> Vec<HistoryEntry> {
        let mut total = self.total_bytes();
        let mut remove = 0;
        while self.entries.len() - remove > 1
            && (self.entries.len() - remove > limits.max_items || total > limits.max_bytes)
        {
            total -= self.entries[remove].bytes;
            remove += 1;
        }
        let pruned: Vec<_> = self.entries.drain(..remove).collect();
        for entry in &pruned {
            self.delete_files(entry);
        }
        pruned
    }

    pub fn set_ocr_text(&mut self, id: &str, text: String) -> Result<bool> {
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) else {
            return Ok(false);
        };
        entry.ocr_text = Some(text).filter(|text| !text.is_empty());
        self.save()?;
        Ok(true)
    }

    pub fn remove(&mut self, id: &str) -> Result<Option<HistoryEntry>> {
        let Some(ix) = self.entries.iter().position(|entry| entry.id == id) else {
            return Ok(None);
        };
        let entry = self.entries.remove(ix);
        self.delete_files(&entry);
        self.save()?;
        Ok(Some(entry))
    }

    pub fn clear(&mut self) -> Result<()> {
        for entry in std::mem::take(&mut self.entries) {
            self.delete_files(&entry);
        }
        self.save()
    }

    fn delete_files(&self, entry: &HistoryEntry) {
        for name in std::iter::once(&entry.file).chain(entry.thumbnail.as_ref()) {
            let path = self.dir.join(name);
            if let Err(err) = std::fs::remove_file(&path)
                && err.kind() != std::io::ErrorKind::NotFound
            {
                log::warn!("failed to delete {}: {err}", path.display());
            }
        }
    }

    fn save(&self) -> Result<()> {
        let json = serde_json::to_vec_pretty(&self.entries)?;
        write_atomic(&self.dir.join(INDEX_FILE), &json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    fn image(width: u32, height: u32) -> RgbaImage {
        RgbaImage::from_fn(width, height, |x, y| {
            image::Rgba([(x * 7) as u8, (y * 13) as u8, ((x + y) * 3) as u8, 255])
        })
    }

    fn add(history: &mut History, minutes: i64, limits: Limits) -> (String, Vec<HistoryEntry>) {
        let created = Utc::now() + TimeDelta::minutes(minutes);
        let entry = NewEntry::image(history.dir(), &image(64, 48), 2.0, None)
            .unwrap()
            .with_created_at(created);
        let id = entry.entry().id.clone();
        (id, history.insert(entry, limits).unwrap())
    }

    const UNLIMITED: Limits = Limits {
        max_items: usize::MAX,
        max_bytes: u64::MAX,
    };

    #[test]
    fn entries_round_trip_through_index() {
        let dir = tempfile::tempdir().unwrap();
        let mut history = History::open(dir.path()).unwrap();
        let (first, _) = add(&mut history, 0, UNLIMITED);
        let (second, _) = add(&mut history, 1, UNLIMITED);
        history.set_ocr_text(&first, "hello".into()).unwrap();

        let reopened = History::open(dir.path()).unwrap();
        let ids: Vec<_> = reopened.entries().map(|entry| entry.id.clone()).collect();
        assert_eq!(ids, [second.clone(), first.clone()]);
        let entry = reopened.get(&first).unwrap();
        assert_eq!(entry.ocr_text.as_deref(), Some("hello"));
        assert_eq!((entry.width, entry.height), (64, 48));
        assert_eq!(entry.scale_factor, 2.0);
        assert!(reopened.file_path(entry).exists());
        assert!(reopened.thumbnail_path(entry).exists());
        assert!(entry.bytes > 0);
    }

    #[test]
    fn an_index_with_extra_fields_still_reads() {
        let dir = tempfile::tempdir().unwrap();
        let mut history = History::open(dir.path()).unwrap();
        let (id, _) = add(&mut history, 0, UNLIMITED);
        let mut index: Vec<serde_json::Value> =
            serde_json::from_slice(&std::fs::read(dir.path().join(INDEX_FILE)).unwrap()).unwrap();
        // Older builds wrote a `poster` field; unknown keys must not break loading.
        index[0].as_object_mut().unwrap().insert(
            "poster".into(),
            serde_json::Value::String(format!("{id}.poster.png")),
        );
        std::fs::write(
            dir.path().join(INDEX_FILE),
            serde_json::to_vec(&index).unwrap(),
        )
        .unwrap();

        let reopened = History::open(dir.path()).unwrap();
        assert!(reopened.get(&id).is_some());
    }

    #[test]
    fn prunes_oldest_beyond_count() {
        let dir = tempfile::tempdir().unwrap();
        let mut history = History::open(dir.path()).unwrap();
        let limits = Limits::new(3, u64::MAX);
        let ids: Vec<_> = (0..5).map(|i| add(&mut history, i, limits).0).collect();

        assert_eq!(history.len(), 3);
        let kept: Vec<_> = history.entries().map(|entry| entry.id.clone()).collect();
        assert_eq!(kept, [ids[4].clone(), ids[3].clone(), ids[2].clone()]);
        for pruned in &ids[..2] {
            assert!(!dir.path().join(format!("{pruned}.png")).exists());
            assert!(!dir.path().join(format!("{pruned}.thumb.png")).exists());
        }
    }

    #[test]
    fn prunes_oldest_beyond_total_size() {
        let dir = tempfile::tempdir().unwrap();
        let mut history = History::open(dir.path()).unwrap();
        let (first, _) = add(&mut history, 0, UNLIMITED);
        let one = history.get(&first).unwrap().bytes;
        add(&mut history, 1, UNLIMITED);
        add(&mut history, 2, UNLIMITED);

        let pruned = history
            .prune(Limits::new(usize::MAX, one * 2 + one / 2))
            .unwrap();
        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0].id, first);
        assert_eq!(history.len(), 2);
        assert!(history.total_bytes() <= one * 2 + one / 2);
    }

    #[test]
    fn newest_entry_survives_tiny_caps() {
        let dir = tempfile::tempdir().unwrap();
        let mut history = History::open(dir.path()).unwrap();
        add(&mut history, 0, UNLIMITED);
        let (newest, _) = add(&mut history, 1, Limits::new(1, 1));
        assert_eq!(history.len(), 1);
        assert_eq!(history.entries().next().unwrap().id, newest);
    }

    #[test]
    fn remove_and_clear_delete_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut history = History::open(dir.path()).unwrap();
        let (first, _) = add(&mut history, 0, UNLIMITED);
        add(&mut history, 1, UNLIMITED);

        let removed = history.remove(&first).unwrap().unwrap();
        assert!(!history.file_path(&removed).exists());
        assert_eq!(history.len(), 1);

        history.clear().unwrap();
        assert!(history.is_empty());
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name() != INDEX_FILE)
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        assert!(History::open(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn missing_files_are_dropped_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let mut history = History::open(dir.path()).unwrap();
        let (first, _) = add(&mut history, 0, UNLIMITED);
        add(&mut history, 1, UNLIMITED);
        std::fs::remove_file(dir.path().join(format!("{first}.png"))).unwrap();

        let reopened = History::open(dir.path()).unwrap();
        assert_eq!(reopened.len(), 1);
        assert!(reopened.get(&first).is_none());
    }
}
