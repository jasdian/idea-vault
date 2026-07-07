//! `memory` — the feature that makes an idea resumable: distil durable facts on Store (D12),
//! reload them as context on Reopen (D13), resolve `[[slug]]` backlinks (D23).
//! See docs/06-concepts/memory.md.

pub mod backlinks;
pub mod compact;
pub mod extract;
pub mod load;

/// Errors produced by the memory extraction/reload/backlink pipeline.
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("not yet implemented: {0}")]
    NotImplemented(&'static str),
    #[error("vault error: {0}")]
    Vault(#[from] crate::vault::VaultError),
    #[error("ai error: {0}")]
    Ai(#[from] crate::ai::AiError),
    /// The compaction fold call returned no usable summary — the previous `compacted.md` is left
    /// intact (mirrors `extract_and_store`'s "model failure aborts with truth intact").
    #[error("compaction produced no summary")]
    EmptyCompaction,
    /// The process-wide AI semaphore was closed — only happens during shutdown.
    #[error("ai concurrency semaphore closed")]
    SemaphoreClosed,
}
