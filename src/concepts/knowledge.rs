//! Knowledge extraction: fan out one Researcher per extraction lens against one idea, persist
//! each lens's findings as an `artifacts/*.md` truth file, then converge a synthesis that is
//! both persisted as an artifact and appended to the conversation (docs/adr/0015, D30).
//!
//! This is the in-product mirror of a dev-side knowledge-extractor: where the classic `swarm`
//! deliberately discards intermediate agent outputs, extraction exists precisely to keep them —
//! the per-lens findings are the primary deliverable, the synthesis is the digest. Bounding is
//! identical to `swarm` (shared `fan_out`, ADR-0006: at most K model calls in flight).

use std::path::Path;

use chrono::Utc;
use tokio::sync::Semaphore;

use crate::ai::budget::ContextBudget;
use crate::ai::LlmBackend;
use crate::concepts::agents::{AgentRole, AgentTask};
use crate::concepts::skills::{hydrate_context, SkillRegistry};
use crate::concepts::swarm::{fan_out, judge, synthesize};
use crate::concepts::ConceptError;
use crate::domain::{slug, Artifact, ArtifactFrontmatter, ArtifactKind};
use crate::vault::store;

/// The built-in extraction lenses — the default angle set for `extract_knowledge`, all
/// registered in `SkillRegistry::builtin()` (and hidden from the moves chip row).
pub const LENSES: [&str; 5] = [
    "extract-key-decisions",
    "extract-durable-facts",
    "extract-open-questions",
    "extract-risks-assumptions",
    "extract-next-actions",
];

/// One per-lens artifact file that was actually written.
#[derive(Debug, Clone, PartialEq)]
pub struct PersistedFinding {
    pub lens: String,
    pub file_slug: String,
}

/// What an extraction run produced. `synthesis` may be empty (findings are still persisted —
/// they are the primary deliverable); `synthesis_slug` is `None` exactly then.
#[derive(Debug)]
pub struct KnowledgeOutcome {
    pub synthesis: String,
    pub findings: Vec<PersistedFinding>,
    pub synthesis_slug: Option<String>,
    /// The shared `%Y%m%d-%H%M%S` stem prefix of this run's files — the web layer derives the
    /// opt-in `.html` report stem from it.
    pub run_stamp: String,
}

/// `extract-key-decisions` → `key-decisions` — the lens's short name used in file stems,
/// progress notes, and the web layer's provenance lines (the one place the reserved prefix is
/// stripped, so a prefix change can't drift).
pub fn lens_short(lens: &str) -> &str {
    lens.strip_prefix("extract-").unwrap_or(lens)
}

/// `key-decisions` → `Key decisions` — the artifact title.
fn lens_title(lens: &str) -> String {
    let short = lens_short(lens).replace('-', " ");
    let mut chars = short.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => short,
    }
}

/// Run the D30 pipeline for `idea_slug`: one Researcher per extraction lens, bounded fan-out,
/// then persist every non-empty finding as `artifacts/<stamp>-<lens>.md`, converge a synthesis
/// (`artifacts/<stamp>-synthesis.md` + one `## assistant (knowledge)` conversation turn).
///
/// Unknown lenses fail fast before any model call. If every lens fails or comes back empty the
/// run errors with [`ConceptError::NothingToSynthesize`] and nothing is written. All vault
/// writes happen in one await-free block after the last model call, so a cancelled job persists
/// either the whole `.md` set or nothing (ADR-0010 abort safety).
#[allow(clippy::too_many_arguments)]
pub async fn extract_knowledge(
    ollama: &LlmBackend,
    ai_semaphore: &Semaphore,
    registry: &SkillRegistry,
    vault_dir: &Path,
    idea_slug: &str,
    lenses: Vec<String>,
    budget: ContextBudget,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<KnowledgeOutcome, ConceptError> {
    // Fail fast on a misconfigured request — before any AI call.
    for lens in &lenses {
        if registry.get(lens).is_none() {
            return Err(ConceptError::UnknownSkill(lens.clone()));
        }
    }

    // One budgeted context block for every lens (D21; hydrated once, lenses differ per task).
    let context = hydrate_context(vault_dir, idea_slug, budget)?;

    // Bounded fan-out (ADR-0006): one Researcher per lens over the shared context block.
    let tasks = lenses
        .iter()
        .map(|lens| AgentTask {
            role: AgentRole::Researcher,
            skill: Some(lens.clone()),
            context: context.text.clone(),
        })
        .collect();
    let on_progress = |done: usize, total: usize, angle: &str| {
        progress(&format!(
            "extraction · harvesting {done}/{total}: {}",
            lens_short(angle)
        ));
    };
    let agent_results = fan_out(ollama, ai_semaphore, registry, tasks, &on_progress).await;

    // The synthesis shortlist drops failures/empties/duplicates; persistence below works from
    // the raw per-lens results instead (judge deliberately loses the lens identity).
    let shortlist = judge(&agent_results);
    if shortlist.is_empty() {
        return Err(ConceptError::NothingToSynthesize);
    }

    progress(&format!(
        "extraction · converging {} findings",
        shortlist.len()
    ));
    let synthesis = synthesize(ollama, ai_semaphore, registry, &shortlist).await?;

    // Persist boundary: one await-free block — a cancel can no longer land between these writes
    // (tokio aborts only at await points), so the .md set + turn are all-or-nothing.
    let now = Utc::now();
    let run_stamp = now.format("%Y%m%d-%H%M%S").to_string();
    let model = ollama.model();
    let taken =
        |candidate: &str| store::artifact_exists(vault_dir, idea_slug, candidate).unwrap_or(false);

    let mut findings = Vec::new();
    for (lens, result) in lenses.iter().zip(&agent_results) {
        let Some(result) = result else { continue };
        if result.content.is_empty() {
            continue;
        }
        let file_slug = slug::disambiguate(&format!("{run_stamp}-{}", lens_short(lens)), taken);
        store::write_artifact(
            vault_dir,
            idea_slug,
            &Artifact {
                frontmatter: ArtifactFrontmatter {
                    slug: file_slug.clone(),
                    title: lens_title(lens),
                    kind: ArtifactKind::Finding,
                    lens: Some(lens.clone()),
                    created: now,
                    model: model.clone(),
                },
                body: result.content.clone(),
            },
        )?;
        findings.push(PersistedFinding {
            lens: lens.clone(),
            file_slug,
        });
    }

    let synthesis_slug = if synthesis.is_empty() {
        // Findings are the primary deliverable — an Ok-but-empty synthesizer response is
        // surfaced and skipped, not fatal (D24; divergence from swarm, docs/adr/0015).
        tracing::warn!(
            idea_slug,
            "extraction synthesizer returned empty output; findings persisted without a synthesis"
        );
        None
    } else {
        let file_slug = slug::disambiguate(&format!("{run_stamp}-synthesis"), taken);
        store::write_artifact(
            vault_dir,
            idea_slug,
            &Artifact {
                frontmatter: ArtifactFrontmatter {
                    slug: file_slug.clone(),
                    title: "Knowledge synthesis".to_string(),
                    kind: ArtifactKind::Synthesis,
                    lens: None,
                    created: now,
                    model,
                },
                body: synthesis.clone(),
            },
        )?;
        // append_turn owns the heading grammar and escapes embedded "## " lines (no forged
        // turn boundaries from model output).
        store::append_turn(vault_dir, idea_slug, "assistant (knowledge)", &synthesis)?;
        Some(file_slug)
    };

    Ok(KnowledgeOutcome {
        synthesis,
        findings,
        synthesis_slug,
        run_stamp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lens_short_strips_the_reserved_prefix_only() {
        assert_eq!(lens_short("extract-key-decisions"), "key-decisions");
        assert_eq!(lens_short("premortem"), "premortem");
    }

    #[test]
    fn lens_title_humanizes() {
        assert_eq!(lens_title("extract-key-decisions"), "Key decisions");
        assert_eq!(lens_title("extract-risks-assumptions"), "Risks assumptions");
    }
}
