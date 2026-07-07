//! Read/write `idea.md`, `conversation.md`, `memory/*.md`, and `MEMORY.md` — the vault's on-disk
//! contract (docs/03-data-model.md D7). Write order is always markdown (truth) first, then any
//! index upsert happens in the caller (`index` module); this module never touches SQLite
//! (docs/03-data-model.md "Consistency & failure model", docs/adr/0002).

use std::path::Path;

use crate::domain::{Idea, MemoryFact, MemoryIndex};
use crate::vault::VaultError;

/// Ensure `dir` exists, creating all missing parent components. Idempotent — succeeds if the
/// directory already exists. This is the one real (non-stub) function in `vault::store`.
pub fn ensure_vault_dir(dir: &Path) -> Result<(), VaultError> {
    std::fs::create_dir_all(dir)?;
    Ok(())
}

/// Parse `vault/<slug>/idea.md` into an `Idea` (frontmatter + body).
pub fn read_idea(vault_dir: &Path, slug: &str) -> Result<Idea, VaultError> {
    let _ = (vault_dir, slug);
    // TODO(scaffold): see docs/03-data-model.md D7/D8 — read `vault/<slug>/idea.md`, split the
    // frontmatter fence via `domain::frontmatter::parse_idea`, and return the resulting `Idea`.
    Err(VaultError::NotImplemented("vault::store::read_idea"))
}

/// Write `vault/<slug>/idea.md` from an `Idea` (frontmatter + body). Truth-first: this must
/// complete before any caller performs an index upsert (docs/03-data-model.md "Write order").
pub fn write_idea(vault_dir: &Path, idea: &Idea) -> Result<(), VaultError> {
    let _ = (vault_dir, idea);
    // TODO(scaffold): see docs/03-data-model.md D7/D8 and D22 (slug is the folder name and is
    // never changed by this call) — render via `domain::frontmatter::emit_idea` and write
    // `vault/<slug>/idea.md`, creating the idea directory on first write (Draft creation).
    Err(VaultError::NotImplemented("vault::store::write_idea"))
}

/// Append one turn of markdown to `vault/<slug>/conversation.md`. `conversation.md` is
/// append-only across every discussion state (docs/04-state-machine.md Invariants) — Store and
/// Reopen only ever append here, never rewrite or truncate.
pub fn append_conversation(
    vault_dir: &Path,
    slug: &str,
    turn_markdown: &str,
) -> Result<(), VaultError> {
    let _ = (vault_dir, slug, turn_markdown);
    // TODO(scaffold): see docs/03-data-model.md D7 and docs/04-state-machine.md invariants —
    // open `vault/<slug>/conversation.md` in append mode (create if absent) and write
    // `turn_markdown`; never truncate or rewrite existing content.
    Err(VaultError::NotImplemented(
        "vault::store::append_conversation",
    ))
}

/// Write one `vault/<idea_slug>/memory/<fact-slug>.md` file. Memory only appears on the
/// transition to `Stored` — `Draft` has no memory (docs/04-state-machine.md Invariants).
pub fn write_memory_fact(
    vault_dir: &Path,
    idea_slug: &str,
    fact: &MemoryFact,
) -> Result<(), VaultError> {
    let _ = (vault_dir, idea_slug, fact);
    // TODO(scaffold): see docs/03-data-model.md D7/D8 and docs/06-concepts/memory.md D12 — render
    // via `domain::frontmatter::emit_memory_fact` and write to
    // `vault/<idea_slug>/memory/<fact.frontmatter.slug>.md`, creating `memory/` on first write
    // (markdown first, per the write-order invariant); merging/dedupe against existing facts on
    // re-store is the caller's (`memory::extract`) responsibility.
    Err(VaultError::NotImplemented(
        "vault::store::write_memory_fact",
    ))
}

/// Rebuild `vault/<idea_slug>/MEMORY.md` (the one-line-per-fact pointer index) by scanning
/// `vault/<idea_slug>/memory/*.md`, and return the resulting `MemoryIndex`.
pub fn rebuild_memory_index(vault_dir: &Path, idea_slug: &str) -> Result<MemoryIndex, VaultError> {
    let _ = (vault_dir, idea_slug);
    // TODO(scaffold): see docs/03-data-model.md D7 and docs/04-state-machine.md (rebuild MEMORY.md
    // is a Store-entry side effect) — enumerate `vault/<idea_slug>/memory/*.md`, parse each via
    // `domain::frontmatter::parse_memory_fact`, write one summary line per fact to
    // `vault/<idea_slug>/MEMORY.md`, and return the parsed `MemoryIndex`.
    Err(VaultError::NotImplemented(
        "vault::store::rebuild_memory_index",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_vault_dir_creates_nested_subpath_and_is_idempotent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let target = tmp.path().join("nested").join("vault");
        assert!(!target.exists());

        ensure_vault_dir(&target).expect("first create should succeed");
        assert!(target.is_dir());

        // Idempotent: calling again on an already-existing directory must still succeed.
        ensure_vault_dir(&target).expect("second call should be idempotent");
        assert!(target.is_dir());
    }
}
