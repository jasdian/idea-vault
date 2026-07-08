pub mod admin;
pub mod artifacts;
pub mod chat;
pub mod compact;
pub mod ideas;
pub mod mcp;
pub mod memory;
pub mod settings;

use crate::app::AppState;

// The byte budget for one AI prompt (D21) is no longer a constant here: every route reads the
// live, backend/model-derived `state.llm.context_budget()` (ADR-0014), so chat, store, reopen,
// the meter, and `memory::compact`'s fold targets all derive from the same single source.

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
