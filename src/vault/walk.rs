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
/// D15). An idea directory is an immediate subdirectory of `vault_dir` containing an `idea.md`
/// (D22: slug == folder name); stray files and non-idea directories are skipped. Entries are
/// sorted by slug so reindex passes are deterministic. A missing `vault_dir` is an empty vault.
pub fn walk_ideas(vault_dir: &Path) -> Result<Vec<IdeaDirEntry>, VaultError> {
    let entries = match std::fs::read_dir(vault_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };

    let mut ideas = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() || !path.join("idea.md").is_file() {
            continue;
        }
        // Skip (rather than lossily mangle) non-UTF-8 dir names: a mangled slug would no longer
        // match the real folder on later path joins. Real slugs are ASCII `[a-z0-9-]` (D22).
        let Some(slug) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        ideas.push(IdeaDirEntry { slug, path });
    }
    ideas.sort_by(|a, b| a.slug.cmp(&b.slug));
    Ok(ideas)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_only_idea_dirs_sorted_by_slug() {
        let tmp = tempfile::tempdir().unwrap();

        // Two real ideas, created out of order.
        std::fs::create_dir_all(tmp.path().join("zeta-idea")).unwrap();
        std::fs::write(tmp.path().join("zeta-idea/idea.md"), "---\n---\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("alpha-idea")).unwrap();
        std::fs::write(tmp.path().join("alpha-idea/idea.md"), "---\n---\n").unwrap();

        // Distractors: a dir without idea.md and a stray file.
        std::fs::create_dir_all(tmp.path().join("not-an-idea")).unwrap();
        std::fs::write(tmp.path().join("stray.md"), "x").unwrap();

        let ideas = walk_ideas(tmp.path()).unwrap();
        let slugs: Vec<_> = ideas.iter().map(|i| i.slug.as_str()).collect();
        assert_eq!(slugs, ["alpha-idea", "zeta-idea"]);
        assert!(ideas[0].path.ends_with("alpha-idea"));
    }

    #[test]
    fn missing_vault_dir_is_an_empty_vault() {
        let tmp = tempfile::tempdir().unwrap();
        let ideas = walk_ideas(&tmp.path().join("does-not-exist")).unwrap();
        assert!(ideas.is_empty());
    }
}
