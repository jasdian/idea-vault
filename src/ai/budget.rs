//! Assembles a prompt for a local model within its context budget
//! (docs/06-concepts/swarm.md D21).
//!
//! TODO(budget): see docs/06-concepts/swarm.md D21 — assemble context in strict priority order
//! so small local models are never handed more than they can use: (1) the idea body — always
//! included in full; (2) top-ranked memory facts (from `memory/*.md` via `MEMORY.md`), most
//! relevant first, trimmed to fit; (3) recent conversation turns, trimmed to whatever budget
//! remains. `ai` does not read the vault itself — callers pass in already-loaded idea
//! body/memory/conversation text; this module only trims/orders/joins it.

use crate::ai::AiError;

/// Placeholder for the context-budgeting policy (target token/char budget, per-section caps).
///
/// TODO(budget): see docs/06-concepts/swarm.md D21.
pub struct ContextBudget;

/// Assemble a prompt string within [`ContextBudget`]'s limits from idea body, memory facts, and
/// recent conversation, in that priority order.
///
/// TODO(budget): see docs/06-concepts/swarm.md D21.
pub fn assemble_context() -> Result<String, AiError> {
    Err(AiError::NotImplemented("ai::budget::assemble_context"))
}
