use std::{io::Write as _, path::Path};

use anyhow::{Context as _, Result};

/// Write via a sibling temp file and rename, so readers never see a torn file.
pub(crate) fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::File::create(&tmp)
        .with_context(|| format!("failed to create {}", tmp.display()))?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path).with_context(|| format!("failed to write {}", path.display()))
}
