pub mod admin;
pub mod chat;
pub mod ideas;
pub mod memory;
pub mod settings;

use crate::app::AppState;

/// Byte budget for one AI prompt (D21), shared by chat, store, and reopen. Sized for small
/// local models; the idea body always survives, memory and older turns trim first.
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
