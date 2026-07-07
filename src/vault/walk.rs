//! Scan `vault/**` for reindex (docs/03-data-model.md D15) — enumerates idea directories without
//! interpreting their contents; parsing is `domain::frontmatter`'s job, upserting is `index`'s.

use std::path::{Path, PathBuf};

use crate::vault::VaultError;

/// One discovered idea directory under `vault/`.
#[derive(Debug, Clone)]
pub struct IdeaDirEntry {
    pub slug: String,
    pub path: PathBuf,
}

/// Enumerate every `vault/<slug>/` idea directory, for reindex to walk (docs/03-data-model.md
/// D15 sequence: `Walk` enumerates `vault/*/`, `Reidx` parses and upserts each).
pub fn walk_ideas(vault_dir: &Path) -> Result<Vec<IdeaDirEntry>, VaultError> {
    let _ = vault_dir;
    // TODO(scaffold): see docs/03-data-model.md D15 — list immediate subdirectories of
    // `vault_dir`, treat each directory name as the idea `slug` (D22: slug == folder name), and
    // return one `IdeaDirEntry` per idea directory (order unspecified; `index::reindex` decides
    // any sort order it needs).
    Err(VaultError::NotImplemented("vault::walk::walk_ideas"))
}
