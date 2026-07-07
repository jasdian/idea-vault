//! Workflows: deterministic multi-step orchestrations over an idea — a fixed DAG of skill/agent
//! steps, as opposed to free-form chat (docs/06-concepts/workflows.md D19).

use crate::concepts::ConceptError;

/// Run a named workflow by its canonical DAG.
///
/// TODO(workflows): see docs/06-concepts/workflows.md §D19 — the canonical "interrogate an idea"
/// workflow fans out diverse critics/researchers (delegating to `concepts::swarm`'s bounded
/// fan-out, D21), judges/ranks/dedupes their findings (a failed step drops to a null result and
/// is skipped by the judge rather than aborting the run), then a Synthesizer merges into one
/// position that is appended as an assistant turn to `conversation.md` via `vault::store`.
pub async fn run_workflow(_name: &str) -> Result<String, ConceptError> {
    Err(ConceptError::NotImplemented(
        "concepts::workflows::run_workflow",
    ))
}
