use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use anyhow::{Context as _, Result, bail};
use chrono::{DateTime, Local, Utc};
use image::RgbaImage;
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

use crate::fs::write_atomic;

const INDEX_FILE: &str = "index.json";
const THUMBNAIL_SIZE: u32 = 320;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    Png,
    Gif,
    Apng,
    Mp4,
}

impl EntryKind {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png | Self::Apng => "png",
            Self::Gif => "gif",
            Self::Mp4 => "mp4",
        }
    }

    /// Whether the entry needs a still thumbnail instead of a full decode.
    pub fn is_media(self) -> bool {
        matches!(self, Self::Gif | Self::Apng | Self::Mp4)
    }
}

fn validate_file_name(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." {
        bail!("history file name must not be empty, . or ..");
    }
    // Backslash is not a separator on Unix, but rejecting it on every platform
    // keeps an index portable and prevents Windows paths from slipping through.
    if name.contains('/') || name.contains('\\') {
        bail!("history file name must not contain path separators: {name:?}");
    }
    let path = Path::new(name);
    if path.is_absolute()
        || path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
    {
        bail!("history file name must be one ordinary path component: {name:?}");
    }
    Ok(())
}

fn deserialize_file_name<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let name = String::deserialize(deserializer)?;
    validate_file_name(&name).map_err(D::Error::custom)?;
    Ok(name)
}

fn deserialize_optional_file_name<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let name = Option::<String>::deserialize(deserializer)?;
    if let Some(name) = &name {
        validate_file_name(name).map_err(D::Error::custom)?;
    }
    Ok(name)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct HistoryEntry {
    pub id: String,
    pub kind: EntryKind,
    pub created_at: DateTime<Utc>,
    /// Ordinary file name inside the history directory, never a path.
    #[serde(deserialize_with = "deserialize_file_name")]
    pub file: String,
    #[serde(default, deserialize_with = "deserialize_optional_file_name")]
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
    /// `file` holds a 16-bit BT.2020/PQ PNG (cICP 9,16,0,1), not an 8-bit
    /// sRGB one. The thumbnail stays 8-bit SDR for the UI and pin preview.
    #[serde(default)]
    pub hdr: bool,
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

/// Metadata for the original file backing a new entry, shared by the three
/// constructors so `finish` stays a small function.
struct FinishSpec {
    id: String,
    kind: EntryKind,
    file: String,
    hdr: bool,
}

impl NewEntry {
    pub fn image(
        dir: &Path,
        image: &RgbaImage,
        scale_factor: f32,
        ocr_text: Option<String>,
    ) -> Result<Self> {
        let (id, file) = reserve(dir, EntryKind::Png)?;
        let writing = Instant::now();
        image
            .save_with_format(dir.join(&file), image::ImageFormat::Png)
            .with_context(|| format!("failed to write {file}"))?;
        let written = writing.elapsed();
        let previewing = Instant::now();
        let spec = FinishSpec {
            id,
            kind: EntryKind::Png,
            file,
            hdr: false,
        };
        let entry = Self::finish(dir, spec, image, scale_factor, ocr_text)?;
        log::info!(
            target: "timing",
            "history: image write {:.1} ms, thumbnail {:.1} ms",
            written.as_secs_f64() * 1e3,
            previewing.elapsed().as_secs_f64() * 1e3
        );
        Ok(entry)
    }

    /// Store a 16-bit BT.2020/PQ PNG (already encoded by the capture crate) as
    /// the original file, keeping an 8-bit SDR `preview` for the UI thumbnail
    /// and pin window. `store` deliberately takes encoded bytes so it does not
    /// depend on the capture crate or on HDR colour math.
    pub fn image_hdr(
        dir: &Path,
        png_bytes: &[u8],
        preview: &RgbaImage,
        scale_factor: f32,
        ocr_text: Option<String>,
    ) -> Result<Self> {
        let (id, file) = reserve(dir, EntryKind::Png)?;
        let writing = Instant::now();
        std::fs::write(dir.join(&file), png_bytes)
            .with_context(|| format!("failed to write {file}"))?;
        let written = writing.elapsed();
        let previewing = Instant::now();
        let spec = FinishSpec {
            id,
            kind: EntryKind::Png,
            file,
            hdr: true,
        };
        let entry = Self::finish(dir, spec, preview, scale_factor, ocr_text)?;
        log::info!(
            target: "timing",
            "history: HDR PNG write {:.1} ms, thumbnail {:.1} ms",
            written.as_secs_f64() * 1e3,
            previewing.elapsed().as_secs_f64() * 1e3
        );
        Ok(entry)
    }

    /// Copy an existing file into history, preserving its name unless it collides.
    pub fn file(
        dir: &Path,
        source: &Path,
        kind: EntryKind,
        preview: &RgbaImage,
        scale_factor: f32,
    ) -> Result<Self> {
        let (id, file) = reserve_import(dir, source, kind)?;
        let copying = Instant::now();
        let destination = dir.join(&file);
        let mut source_file = std::fs::File::open(source)
            .with_context(|| format!("failed to open {}", source.display()))?;
        (|| -> Result<()> {
            let mut destination_file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)
                .with_context(|| format!("failed to create {}", destination.display()))?;
            if let Err(err) = std::io::copy(&mut source_file, &mut destination_file) {
                let _ = std::fs::remove_file(&destination);
                return Err(err).with_context(|| format!("failed to copy {}", source.display()));
            }
            Ok(())
        })()?;
        let copied = copying.elapsed();
        let previewing = Instant::now();
        let spec = FinishSpec {
            id,
            kind,
            file,
            hdr: false,
        };
        let entry = Self::finish(dir, spec, preview, scale_factor, None)?;
        log::info!(
            target: "timing",
            "history: copy {:.1} ms, thumbnail {:.1} ms",
            copied.as_secs_f64() * 1e3,
            previewing.elapsed().as_secs_f64() * 1e3
        );
        Ok(entry)
    }

    fn finish(
        dir: &Path,
        spec: FinishSpec,
        preview: &RgbaImage,
        scale_factor: f32,
        ocr_text: Option<String>,
    ) -> Result<Self> {
        let FinishSpec {
            id,
            kind,
            file,
            hdr,
        } = spec;
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
            hdr,
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

fn reserve_import(dir: &Path, source: &Path, kind: EntryKind) -> Result<(String, String)> {
    let (id, fallback) = reserve(dir, kind)?;
    let Some(preferred) = source
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| validate_file_name(name).is_ok())
    else {
        return Ok((id, fallback));
    };
    if !dir.join(preferred).exists() {
        return Ok((id, preferred.to_string()));
    }

    let path = Path::new(preferred);
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(preferred);
    let extension = path.extension().and_then(|extension| extension.to_str());
    for n in 2.. {
        let candidate = match extension {
            Some(extension) => format!("{stem}-{n}.{extension}"),
            None => format!("{stem}-{n}"),
        };
        if !dir.join(&candidate).exists() {
            return Ok((id, candidate));
        }
    }
    unreachable!("file-name collision search is unbounded")
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

fn validate_entry(entry: &HistoryEntry) -> Result<()> {
    validate_file_name(&entry.file)?;
    if let Some(thumbnail) = &entry.thumbnail {
        validate_file_name(thumbnail)?;
    }
    Ok(())
}

/// Resolve one indexed name without allowing `..`, absolute paths, separators,
/// or a symlink whose target escapes the history directory.
fn resolve_path(dir: &Path, name: &str) -> Result<PathBuf> {
    validate_file_name(name)?;
    let root = dir
        .canonicalize()
        .with_context(|| format!("failed to resolve history directory {}", dir.display()))?;
    let candidate = root.join(name);
    match std::fs::symlink_metadata(&candidate) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let resolved = candidate
                .canonicalize()
                .with_context(|| format!("failed to resolve {}", candidate.display()))?;
            if !resolved.starts_with(&root) {
                bail!(
                    "history path {} escapes {}",
                    resolved.display(),
                    root.display()
                );
            }
            // Keep the directory entry itself rather than the symlink target so
            // deleting an indexed symlink never deletes another history file.
            Ok(candidate)
        }
        Ok(_) => Ok(candidate),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(candidate),
        Err(err) => Err(err).with_context(|| format!("failed to resolve {}", candidate.display())),
    }
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
        entries.retain(|entry| {
            if let Err(err) = validate_entry(entry) {
                log::warn!("ignoring unsafe history entry {}: {err:#}", entry.id);
                return false;
            }
            match resolve_path(&dir, &entry.file) {
                Ok(path) => path.exists(),
                Err(err) => {
                    log::warn!("ignoring history entry {}: {err:#}", entry.id);
                    false
                }
            }
        });
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

    pub fn file_path(&self, entry: &HistoryEntry) -> Result<PathBuf> {
        resolve_path(&self.dir, &entry.file)
    }

    pub fn thumbnail_path(&self, entry: &HistoryEntry) -> Result<PathBuf> {
        resolve_path(&self.dir, entry.thumbnail.as_deref().unwrap_or(&entry.file))
    }

    /// Index a new entry and prune to `limits`. Returns the pruned entries.
    /// The candidate index is persisted before memory changes or file deletion.
    pub fn insert(&mut self, entry: NewEntry, limits: Limits) -> Result<Vec<HistoryEntry>> {
        validate_entry(entry.entry())?;
        let mut next = self.entries.clone();
        next.push(entry.0);
        next.sort_by_key(|entry| entry.created_at);
        let pruned = Self::prune_entries(&mut next, limits);
        self.save_entries(&next)?;
        self.entries = next;
        self.delete_pruned_files(&pruned);
        Ok(pruned)
    }

    /// Drop the oldest entries until both caps hold. The newest entry is always kept.
    pub fn prune(&mut self, limits: Limits) -> Result<Vec<HistoryEntry>> {
        let mut next = self.entries.clone();
        let pruned = Self::prune_entries(&mut next, limits);
        if !pruned.is_empty() {
            self.save_entries(&next)?;
            self.entries = next;
            self.delete_pruned_files(&pruned);
        }
        Ok(pruned)
    }

    fn prune_entries(entries: &mut Vec<HistoryEntry>, limits: Limits) -> Vec<HistoryEntry> {
        let mut total = entries.iter().map(|entry| entry.bytes).sum::<u64>();
        let mut remove = 0;
        while entries.len() - remove > 1
            && (entries.len() - remove > limits.max_items || total > limits.max_bytes)
        {
            total -= entries[remove].bytes;
            remove += 1;
        }
        entries.drain(..remove).collect()
    }

    pub fn set_ocr_text(&mut self, id: &str, text: String) -> Result<bool> {
        let Some(ix) = self.entries.iter().position(|entry| entry.id == id) else {
            return Ok(false);
        };
        let mut next = self.entries.clone();
        next[ix].ocr_text = Some(text).filter(|text| !text.is_empty());
        self.save_entries(&next)?;
        self.entries = next;
        Ok(true)
    }

    pub fn remove(&mut self, id: &str) -> Result<Option<HistoryEntry>> {
        let Some(ix) = self.entries.iter().position(|entry| entry.id == id) else {
            return Ok(None);
        };
        let mut next = self.entries.clone();
        let entry = next.remove(ix);
        self.save_entries(&next)?;
        self.entries = next;
        self.delete_files(&entry);
        Ok(Some(entry))
    }

    pub fn clear(&mut self) -> Result<()> {
        self.save_entries(&[])?;
        let removed = std::mem::take(&mut self.entries);
        self.delete_pruned_files(&removed);
        Ok(())
    }

    /// Delete the files backing an entry that is not (or is no longer) indexed.
    /// Unsafe names and symlinks escaping the history directory are ignored.
    pub fn delete_files(&self, entry: &HistoryEntry) {
        if let Err(err) = validate_entry(entry) {
            log::warn!(
                "refusing to delete unsafe history entry {}: {err:#}",
                entry.id
            );
            return;
        }
        for name in std::iter::once(&entry.file).chain(entry.thumbnail.as_ref()) {
            let path = match resolve_path(&self.dir, name) {
                Ok(path) => path,
                Err(err) => {
                    log::warn!("refusing to delete history file {name:?}: {err:#}");
                    continue;
                }
            };
            if let Err(err) = std::fs::remove_file(&path)
                && err.kind() != std::io::ErrorKind::NotFound
            {
                log::warn!("failed to delete {}: {err}", path.display());
            }
        }
    }

    fn delete_pruned_files(&self, entries: &[HistoryEntry]) {
        for entry in entries {
            self.delete_files(entry);
        }
    }

    fn save_entries(&self, entries: &[HistoryEntry]) -> Result<()> {
        let json = serde_json::to_vec_pretty(entries)?;
        write_atomic(&self.dir.join(INDEX_FILE), &json)
    }

    fn save(&self) -> Result<()> {
        self.save_entries(&self.entries)
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
        assert!(reopened.file_path(entry).unwrap().exists());
        assert!(reopened.thumbnail_path(entry).unwrap().exists());
        assert!(entry.bytes > 0);
    }

    #[test]
    fn imported_recording_keeps_its_clipboard_file_name() {
        let root = tempfile::tempdir().unwrap();
        let history_dir = root.path().join("history");
        let source_dir = root.path().join("save");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source = source_dir.join("SnapIt 2026-11-24 10.30.00.123-42-1.mp4");
        std::fs::write(&source, b"recorded video").unwrap();
        let history = History::open(&history_dir).unwrap();

        let entry = NewEntry::file(&history_dir, &source, EntryKind::Mp4, &image(64, 48), 1.0)
            .unwrap()
            .entry()
            .clone();

        assert_eq!(entry.file, source.file_name().unwrap().to_str().unwrap());
        assert_eq!(
            std::fs::read(history.file_path(&entry).unwrap()).unwrap(),
            b"recorded video"
        );
    }

    #[test]
    fn apng_and_still_png_imports_with_the_same_name_do_not_overwrite() {
        let root = tempfile::tempdir().unwrap();
        let history_dir = root.path().join("history");
        let first_source = root.path().join("still/clip.png");
        let second_source = root.path().join("animation/clip.png");
        std::fs::create_dir_all(first_source.parent().unwrap()).unwrap();
        std::fs::create_dir_all(second_source.parent().unwrap()).unwrap();
        std::fs::write(&first_source, b"still").unwrap();
        std::fs::write(&second_source, b"animation").unwrap();
        let history = History::open(&history_dir).unwrap();

        let still = NewEntry::file(
            &history_dir,
            &first_source,
            EntryKind::Png,
            &image(8, 8),
            1.0,
        )
        .unwrap()
        .entry()
        .clone();
        let animation = NewEntry::file(
            &history_dir,
            &second_source,
            EntryKind::Apng,
            &image(8, 8),
            1.0,
        )
        .unwrap()
        .entry()
        .clone();

        assert_eq!(still.file, "clip.png");
        assert_eq!(animation.file, "clip-2.png");
        assert!(animation.kind.is_media());
        assert_eq!(animation.kind.extension(), "png");
        assert_eq!(
            std::fs::read(history.file_path(&still).unwrap()).unwrap(),
            b"still"
        );
        assert_eq!(
            std::fs::read(history.file_path(&animation).unwrap()).unwrap(),
            b"animation"
        );
    }

    #[test]
    fn imported_recordings_with_the_same_name_do_not_overwrite_each_other() {
        let root = tempfile::tempdir().unwrap();
        let history_dir = root.path().join("history");
        let first_source = root.path().join("first/clip.gif");
        let second_source = root.path().join("second/clip.gif");
        std::fs::create_dir_all(first_source.parent().unwrap()).unwrap();
        std::fs::create_dir_all(second_source.parent().unwrap()).unwrap();
        std::fs::write(&first_source, b"first").unwrap();
        std::fs::write(&second_source, b"second").unwrap();

        let first = NewEntry::file(
            &history_dir,
            &first_source,
            EntryKind::Gif,
            &image(8, 8),
            1.0,
        )
        .unwrap()
        .entry()
        .clone();
        let second = NewEntry::file(
            &history_dir,
            &second_source,
            EntryKind::Gif,
            &image(8, 8),
            1.0,
        )
        .unwrap()
        .entry()
        .clone();

        assert_eq!(first.file, "clip.gif");
        assert_eq!(second.file, "clip-2.gif");
        assert_eq!(
            std::fs::read(history_dir.join(&first.file)).unwrap(),
            b"first"
        );
        assert_eq!(
            std::fs::read(history_dir.join(&second.file)).unwrap(),
            b"second"
        );
    }

    #[test]
    fn hdr_flag_round_trips_and_older_entries_default_to_sdr() {
        let dir = tempfile::tempdir().unwrap();
        let mut history = History::open(dir.path()).unwrap();
        let preview = image(64, 48);
        // Pretend HDR bytes; the store treats them as opaque and only records
        // the flag, so a short byte slice is enough for this persistence test.
        let hdr_entry =
            NewEntry::image_hdr(history.dir(), b"fake-hdr-png", &preview, 1.0, None).unwrap();
        let hdr_id = hdr_entry.entry().id.clone();
        history.insert(hdr_entry, UNLIMITED).unwrap();
        // A normal SDR entry alongside it.
        let (sdr_id, _) = add(&mut history, 1, UNLIMITED);

        let reopened = History::open(dir.path()).unwrap();
        assert!(reopened.get(&hdr_id).unwrap().hdr);
        assert!(!reopened.get(&sdr_id).unwrap().hdr);
        assert!(
            reopened
                .file_path(reopened.get(&hdr_id).unwrap())
                .unwrap()
                .exists()
        );

        // An index entry without the field (older build) reads as SDR.
        let mut index: Vec<serde_json::Value> =
            serde_json::from_slice(&std::fs::read(dir.path().join(INDEX_FILE)).unwrap()).unwrap();
        index[0].as_object_mut().unwrap().remove("hdr");
        std::fs::write(
            dir.path().join(INDEX_FILE),
            serde_json::to_vec(&index).unwrap(),
        )
        .unwrap();
        let reopened = History::open(dir.path()).unwrap();
        assert!(reopened.entries().all(|entry| !entry.hdr));
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
    fn an_unbounded_count_keeps_every_entry() {
        let dir = tempfile::tempdir().unwrap();
        let mut history = History::open(dir.path()).unwrap();
        let limits = Limits::new(usize::MAX, u64::MAX);
        for i in 0..20 {
            add(&mut history, i, limits);
        }
        assert_eq!(history.len(), 20);
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
        assert!(!history.file_path(&removed).unwrap().exists());
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
    fn ordinary_names_resolve_but_traversing_names_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let history_dir = dir.path().join("history");
        let mut history = History::open(&history_dir).unwrap();
        let (id, _) = add(&mut history, 0, UNLIMITED);
        let entry = history.get(&id).unwrap().clone();
        let root = history_dir.canonicalize().unwrap();
        assert_eq!(history.file_path(&entry).unwrap(), root.join(&entry.file));
        assert_eq!(
            history.thumbnail_path(&entry).unwrap(),
            root.join(entry.thumbnail.as_deref().unwrap())
        );

        let mut nullable = serde_json::to_value(&entry).unwrap();
        nullable["thumbnail"] = serde_json::Value::Null;
        assert!(
            serde_json::from_value::<HistoryEntry>(nullable)
                .unwrap()
                .thumbnail
                .is_none()
        );

        let outside = dir.path().join("outside.png");
        std::fs::write(&outside, b"outside").unwrap();
        let unsafe_names = [
            "",
            ".",
            "..",
            "../outside.png",
            "nested/file.png",
            "nested\\file.png",
            "/absolute.png",
            "C:\\absolute.png",
            "C:/absolute.png",
        ];
        for name in unsafe_names {
            assert!(validate_file_name(name).is_err(), "accepted {name:?}");
            let mut value = serde_json::to_value(&entry).unwrap();
            value["file"] = name.into();
            assert!(
                serde_json::from_value::<HistoryEntry>(value).is_err(),
                "deserialized unsafe file name {name:?}"
            );

            let mut value = serde_json::to_value(&entry).unwrap();
            value["thumbnail"] = name.into();
            assert!(
                serde_json::from_value::<HistoryEntry>(value).is_err(),
                "deserialized unsafe thumbnail name {name:?}"
            );
        }

        let mut unsafe_entry = entry.clone();
        unsafe_entry.file = "../outside.png".into();
        unsafe_entry.thumbnail = Some(outside.to_string_lossy().into_owned());
        assert!(history.file_path(&unsafe_entry).is_err());
        assert!(history.thumbnail_path(&unsafe_entry).is_err());
        history.delete_files(&unsafe_entry);
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside");

        // Loading an index containing an unsafe name rejects the corrupt index
        // instead of exposing a path outside the history directory.
        let mut value = serde_json::to_value(&entry).unwrap();
        value["file"] = "../outside.png".into();
        std::fs::write(
            history_dir.join(INDEX_FILE),
            serde_json::to_vec(&[value]).unwrap(),
        )
        .unwrap();
        assert!(History::open(&history_dir).unwrap().is_empty());
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected_and_not_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let history_dir = dir.path().join("history");
        let history = History::open(&history_dir).unwrap();
        let outside = dir.path().join("outside.png");
        std::fs::write(&outside, b"outside").unwrap();
        let link = history_dir.join("linked.png");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let new_entry = NewEntry::image(history.dir(), &image(8, 8), 1.0, None).unwrap();
        let mut entry = new_entry.entry().clone();
        entry.file = "linked.png".into();
        entry.thumbnail = None;
        assert!(history.file_path(&entry).is_err());
        history.delete_files(&entry);
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
        assert!(link.is_symlink());
    }

    #[test]
    fn failed_saves_roll_back_memory_and_keep_indexed_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut history = History::open(dir.path()).unwrap();
        let (first, _) = add(&mut history, 0, UNLIMITED);
        add(&mut history, 1, UNLIMITED);
        let pending = NewEntry::image(history.dir(), &image(64, 48), 2.0, None).unwrap();
        let pending_id = pending.entry().id.clone();
        let before: Vec<_> = history.entries().cloned().collect();
        let index_before = std::fs::read(dir.path().join(INDEX_FILE)).unwrap();
        let original_paths: Vec<_> = before
            .iter()
            .map(|entry| {
                (
                    history.file_path(entry).unwrap(),
                    history.thumbnail_path(entry).unwrap(),
                )
            })
            .collect();

        // Atomic writes use index.tmp; a directory there makes every save fail
        // without disturbing the existing index or capture files.
        std::fs::create_dir(dir.path().join("index.tmp")).unwrap();
        assert!(history.insert(pending, Limits::new(1, 1)).is_err());
        assert_eq!(history.entries().cloned().collect::<Vec<_>>(), before);
        assert!(history.prune(Limits::new(1, 1)).is_err());
        assert_eq!(history.entries().cloned().collect::<Vec<_>>(), before);
        assert!(history.remove(&first).is_err());
        assert_eq!(history.entries().cloned().collect::<Vec<_>>(), before);
        assert!(history.clear().is_err());
        assert_eq!(history.entries().cloned().collect::<Vec<_>>(), before);
        assert!(
            history
                .set_ocr_text(&first, "not persisted".into())
                .is_err()
        );
        assert_eq!(history.entries().cloned().collect::<Vec<_>>(), before);

        assert_eq!(
            std::fs::read(dir.path().join(INDEX_FILE)).unwrap(),
            index_before
        );
        for (file, thumbnail) in original_paths {
            assert!(file.exists());
            assert!(thumbnail.exists());
        }
        // A failed insert did not delete the newly written, still-unindexed
        // entry, nor any old files that the candidate prune would have removed.
        assert!(dir.path().join(format!("{pending_id}.png")).exists());
        assert!(dir.path().join(format!("{pending_id}.thumb.png")).exists());
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
