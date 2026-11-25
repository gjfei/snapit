use std::{
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result};

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

/// Write a buffered sibling temporary file, flush and sync it, then atomically
/// publish it at `path`. Existing files stay untouched until the rename succeeds.
///
/// Write directly to the provided writer: the helper owns the buffer, flushes
/// it even when the callback does not, and only then syncs and renames the file.
/// A partially written image is therefore never installed as the destination.
pub fn atomic_write(path: &Path, write: impl FnOnce(&mut dyn Write) -> Result<()>) -> Result<()> {
    let (temporary, file) = create_sibling(path)?;
    let mut temporary = TemporaryFile::new(temporary, file);
    {
        let mut buffered = BufWriter::new(temporary.file());
        let result = write(&mut buffered)
            .and_then(|_| buffered.flush().context("failed to flush temporary file"));
        drop(buffered);
        let result = result.and_then(|_| {
            temporary
                .file()
                .sync_all()
                .context("failed to sync temporary file")
        });
        // Close before publishing (required on Windows) and before failure
        // cleanup so an open handle cannot keep the temporary path alive.
        temporary.close();
        result?;
    }
    temporary.publish(path)
}

fn create_sibling(path: &Path) -> Result<(PathBuf, File)> {
    let name = path
        .file_name()
        .with_context(|| format!("failed to create a temporary file for {}", path.display()))?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for _ in 0..128 {
        let sequence = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
        let mut temporary_name = name.to_os_string();
        temporary_name.push(format!(
            ".snapit-{}-{nonce}-{sequence}.tmp",
            std::process::id()
        ));
        let temporary = parent.join(temporary_name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to create {}", temporary.display()));
            }
        }
    }
    anyhow::bail!(
        "failed to allocate a unique temporary file beside {}",
        path.display()
    )
}

fn publish(temporary: &Path, path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        use windows::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };
        use windows::core::PCWSTR;

        let from: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // std::fs::rename cannot replace an existing file on Windows. Doing the
        // replacement in one OS call avoids a remove-then-rename data-loss gap.
        unsafe {
            MoveFileExW(
                PCWSTR(from.as_ptr()),
                PCWSTR(to.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
        .with_context(|| format!("failed to publish {}", path.display()))?;
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(temporary, path)
            .with_context(|| format!("failed to publish {}", path.display()))?;
    }
    Ok(())
}

struct TemporaryFile {
    // Fields are dropped in declaration order: close the handle before the
    // cleanup guard removes the path, including while unwinding a panic.
    file: Option<File>,
    cleanup: TemporaryPath,
}

impl TemporaryFile {
    fn new(path: PathBuf, file: File) -> Self {
        Self {
            file: Some(file),
            cleanup: TemporaryPath::new(path),
        }
    }

    fn file(&mut self) -> &mut File {
        self.file.as_mut().expect("temporary file is closed")
    }

    fn close(&mut self) {
        self.file.take();
    }

    fn publish(&mut self, path: &Path) -> Result<()> {
        publish(&self.cleanup.path, path)?;
        self.cleanup.path.clear();
        self.close();
        Ok(())
    }
}

struct TemporaryPath {
    path: PathBuf,
}

impl TemporaryPath {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for TemporaryPath {
    fn drop(&mut self) {
        if self.path.as_os_str().is_empty() {
            return;
        }
        if let Err(err) = std::fs::remove_file(&self.path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!(
                "failed to remove temporary file {}: {err}",
                self.path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(dir: &Path) -> Vec<PathBuf> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        entries.sort();
        entries
    }

    #[test]
    fn atomic_write_replaces_an_existing_file_and_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capture.png");
        std::fs::write(&path, b"old").unwrap();

        atomic_write(&path, |file| {
            file.write_all(b"new complete image")?;
            Ok(())
        })
        .unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"new complete image");
        assert_eq!(entries(dir.path()), vec![path]);
    }

    #[test]
    fn atomic_write_flushes_callback_writes_before_publishing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capture.png");
        let bytes = vec![0x5a; 64 * 1024 + 17];

        atomic_write(&path, |writer| {
            writer.write_all(&bytes)?;
            Ok(())
        })
        .unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(entries(dir.path()), vec![path]);
    }

    #[test]
    fn atomic_write_failure_preserves_the_old_file_and_removes_partial_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capture.png");
        std::fs::write(&path, b"old complete image").unwrap();

        let error = atomic_write(&path, |file| {
            file.write_all(b"partial")?;
            anyhow::bail!("encoder failed");
        })
        .unwrap_err();

        assert!(error.to_string().contains("encoder failed"));
        assert_eq!(std::fs::read(&path).unwrap(), b"old complete image");
        assert_eq!(entries(dir.path()), vec![path]);
    }

    #[test]
    fn atomic_write_publish_failure_removes_the_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capture.png");
        std::fs::create_dir(&path).unwrap();

        assert!(
            atomic_write(&path, |file| {
                file.write_all(b"new")?;
                Ok(())
            })
            .is_err()
        );
        assert!(path.is_dir());
        assert_eq!(entries(dir.path()), vec![path]);
    }

    #[test]
    fn atomic_write_panic_closes_and_removes_the_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capture.png");
        std::fs::write(&path, b"old complete image").unwrap();

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = atomic_write(&path, |writer| {
                writer.write_all(b"partial")?;
                panic!("encoder panicked");
            });
        }));

        assert!(panic.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"old complete image");
        assert_eq!(entries(dir.path()), vec![path]);
    }

    #[test]
    fn sibling_temporary_files_are_unique_and_in_the_destination_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capture.png");
        let (first, first_file) = create_sibling(&path).unwrap();
        let (second, second_file) = create_sibling(&path).unwrap();
        drop(first_file);
        drop(second_file);

        assert_ne!(first, second);
        assert_eq!(first.parent(), second.parent());
        assert_eq!(first.parent(), Some(dir.path()));
        std::fs::remove_file(first).unwrap();
        std::fs::remove_file(second).unwrap();
        assert!(entries(dir.path()).is_empty());
    }
}
