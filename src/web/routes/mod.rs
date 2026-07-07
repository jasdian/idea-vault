pub mod admin;
pub mod chat;
pub mod compact;
pub mod ideas;
pub mod memory;
pub mod settings;

use crate::app::AppState;

/// Byte budget for one AI prompt (D21), shared by chat, store, and reopen. Sized for small
/// local models; the idea body always survives, memory and older turns trim first. Auto-compact's
/// internal fractions live in `memory::compact` (kept there to honour D4: `memory` never depends
/// on `web`); the test below guards the two copies of this base against drift.
pub(crate) const AI_BUDGET_BYTES: usize = 16 * 1024;

/// Rebuild the index, logging instead of failing the request — markdown truth already landed
/// and the next reindex reconciles (docs/03 "Consistency & failure model").
pub(crate) fn reindex_logged(state: &AppState) {
    match state.db.lock() {
        Ok(mut conn) => {
            if let Err(e) = crate::index::reindex::reindex(&mut conn, &state.config.vault_dir) {
                tracing::warn!(error = %e, "reindex after vault write failed; truth intact");
            }
        }
        Err(e) => tracing::warn!(error = %e, "db mutex poisoned; skipping reindex"),
    }
}

#[cfg(test)]
mod tests {
    /// The AI budget base is duplicated in `memory::compact` (D4 layering); keep them equal so the
    /// compaction thresholds stay fractions of the *same* budget the meter and prompts use.
    #[test]
    fn compact_budget_base_matches_ai_budget_bytes() {
        assert_eq!(
            crate::memory::compact::AI_BUDGET_BYTES,
            super::AI_BUDGET_BYTES
        );
        assert_eq!(
            crate::memory::compact::COMPACT_SUMMARIZER_INPUT_BYTES,
            super::AI_BUDGET_BYTES
        );
    }
}
