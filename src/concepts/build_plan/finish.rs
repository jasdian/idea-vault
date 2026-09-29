//! The build-plan persist boundary (docs/adr/0030): parse the planner's answer, run the gates
//! over it against the idea's evidence, write the gated plan as `artifacts/<stamp>-build-plan.md`
//! and append a short pointer turn to `conversation.md`. The plan body never enters the
//! transcript, and build-plan turns (named capstone turns or pointer-shaped skill and workflow
//! turns, [`POINTER_PREFIX`]) are excluded from every evidence haystack, so neither a later plan
//! nor store-time extraction grounds in plan text.
//!
//! [`finish`] is blocking (vault reads and writes, the bounded source probe) and holds no
//! permit; callers run it after the model call, in `spawn_blocking`.

use std::path::Path;

use chrono::{DateTime, Utc};

use crate::ai::contract;
use crate::ai::sources::SourceProbe;
use crate::concepts::build_plan::gates::{
    self, AuditView, Evidence, GateInputs, GateReport, OpenArtifact,
};
use crate::concepts::build_plan::plan::{self, BuildPlan, Provenance};
use crate::concepts::ConceptError;
use crate::domain::evidence::POINTER_PREFIX;
use crate::domain::frontmatter::ArtifactFrontmatter;
use crate::domain::{slug, Artifact, ArtifactKind};
use crate::vault::store;

/// The harvest lens whose artifacts list an idea's open questions.
const OPEN_QUESTIONS_LENS: &str = "open-questions";

/// Everything [`finish`] needs besides the vault itself.
pub struct PlanInputs<'a> {
    pub vault_dir: &'a Path,
    pub idea_slug: &'a str,
    /// The planner's raw answer.
    pub answer: &'a str,
    /// The pointer turn's heading role, e.g. `assistant (skill: build-prompt)`.
    pub turn_role: &'a str,
    /// The registry name that produced the plan (`build-prompt` or `ready-to-build`).
    pub lens: &'a str,
    pub model: String,
    /// The audited harvest the planner worked from; `None` for a quick (unaudited) plan.
    pub audit: Option<&'a AuditView>,
    pub probe: &'a SourceProbe,
    pub now: DateTime<Utc>,
}

/// What [`finish`] persisted.
#[derive(Debug, Clone, PartialEq)]
pub struct Finished {
    /// The artifact's file stem (`<stamp>-build-plan`).
    pub artifact_slug: String,
    /// The pointer turn's body, as appended.
    pub pointer: String,
    pub report: GateReport,
    pub plan: BuildPlan,
}

/// `<stamp>-open-questions` or a same-second `<stamp>-open-questions-<n>`.
fn is_open_questions_stem(stem: &str) -> bool {
    let suffix = format!("-{OPEN_QUESTIONS_LENS}");
    stem.ends_with(&suffix)
        || stem.rsplit_once('-').is_some_and(|(base, n)| {
            base.ends_with(&suffix) && !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())
        })
}

/// The idea's latest readable open-questions artifact (stems sort by their fixed-width run
/// stamp), and a note when a newer one could not be read. A plan is never lost to a bad sibling
/// artifact: unreadable files are skipped.
fn latest_open_questions(
    vault_dir: &Path,
    idea_slug: &str,
) -> Result<(Option<OpenArtifact>, Option<String>), ConceptError> {
    let mut stems: Vec<String> = store::list_artifact_files(vault_dir, idea_slug)?
        .into_iter()
        .filter(|f| f.ext == store::ArtifactExt::Md && is_open_questions_stem(&f.slug))
        .map(|f| f.slug)
        .collect();
    stems.sort();
    let mut skipped = Vec::new();
    for stem in stems.into_iter().rev() {
        match store::read_artifact(vault_dir, idea_slug, &stem) {
            Ok(a) => {
                let note = (!skipped.is_empty())
                    .then(|| format!("unreadable, skipped: {}", skipped.join(", ")));
                return Ok((
                    Some(OpenArtifact {
                        items: contract::items(&a.body),
                        name: stem,
                    }),
                    note,
                ));
            }
            Err(_) => skipped.push(stem),
        }
    }
    let note = (!skipped.is_empty()).then(|| format!("unreadable: {}", skipped.join(", ")));
    Ok((None, note))
}

/// `settled 2 (1 you · 1 foil) · opened 1 · quarantined 1 · premises 2 · tasks 4 (2 need you) ·
/// kill criteria 1` — `opened` is the gates' own count of Settled claims moved to Open.
fn tally_line(plan: &BuildPlan, report: &GateReport) -> String {
    let by = |p: Provenance| {
        plan.settled
            .iter()
            .filter(|i| i.provenance == Some(p))
            .count()
    };
    let who: Vec<String> = [Provenance::Owner, Provenance::Foil, Provenance::Idea]
        .into_iter()
        .map(|p| (by(p), p.label()))
        .filter(|(n, _)| *n > 0)
        .map(|(n, l)| format!("{n} {l}"))
        .collect();
    let settled = if who.is_empty() {
        format!("settled {}", plan.settled.len())
    } else {
        format!("settled {} ({})", plan.settled.len(), who.join(" · "))
    };
    let opened = report.tally.get("opened").copied().unwrap_or(0);
    let owner_tasks = plan.tasks.iter().filter(|t| t.needs_owner).count();
    let tasks = if owner_tasks > 0 {
        format!("tasks {} ({owner_tasks} need you)", plan.tasks.len())
    } else {
        format!("tasks {}", plan.tasks.len())
    };
    [
        settled,
        format!("opened {opened}"),
        format!("quarantined {}", plan.quarantined.len()),
        format!("premises {}", plan.verify.len()),
        tasks,
        format!("kill criteria {}", plan.kills.len()),
    ]
    .join(" · ")
}

fn mode_label(audit: Option<&AuditView>) -> &'static str {
    match audit {
        None => "quick · unaudited",
        Some(a) if a.failed => "audited · audit unavailable",
        Some(_) => "audited",
    }
}

/// The artifact body: a title, the two code-owned header lines, any gate notes, then the plan in
/// the canonical grammar.
fn artifact_body(
    title: &str,
    inputs: &PlanInputs,
    excluded: usize,
    consulted: &str,
    plan: &BuildPlan,
    report: &GateReport,
) -> String {
    let mut out = format!(
        "# Build plan — {title}\n_{} · {} · {} · {excluded} capstone turn(s) excluded from evidence · consulted: {consulted}_\n_gates: {}_\n\n",
        mode_label(inputs.audit),
        inputs.model,
        inputs.now.format("%Y-%m-%d %H:%M"),
        tally_line(plan, report),
    );
    for note in &report.notes {
        out.push_str(&format!("> {note}\n"));
    }
    if !report.notes.is_empty() {
        out.push('\n');
    }
    out.push_str(&plan::render(plan));
    out.push('\n');
    out
}

fn pointer_turn(
    inputs: &PlanInputs,
    file_slug: &str,
    plan: &BuildPlan,
    report: &GateReport,
) -> String {
    let mut out = format!(
        "{POINTER_PREFIX}{file_slug}](/idea/{}/artifact/{file_slug}.md) · {}\n\n{}\n",
        inputs.idea_slug,
        mode_label(inputs.audit),
        tally_line(plan, report),
    );
    if !plan.open.is_empty() {
        out.push_str("\n**Open questions for you** — answer in chat, then build again:\n");
        for q in &plan.open {
            out.push_str(&format!("- {}: {}\n", q.id, q.text));
        }
    }
    out
}

/// Parse, gate and persist one build plan: every read first, then the artifact, then the
/// pointer turn. An answer with neither a goal nor a task is [`ConceptError::PlanUnusable`] and
/// persists nothing. Being blocking, it runs to completion once started — aborting the job that
/// spawned it does not stop it, so a cancel after the model call still lands the plan.
pub fn finish(inputs: PlanInputs) -> Result<Finished, ConceptError> {
    let mut plan = plan::parse(inputs.answer).map_err(|_| ConceptError::PlanUnusable)?;
    let idea = store::read_idea(inputs.vault_dir, inputs.idea_slug)?;
    let conversation = store::read_conversation(inputs.vault_dir, inputs.idea_slug)?;
    let evidence = Evidence::new(&idea.body, &conversation);
    let excluded = store::split_turns(&conversation)
        .iter()
        .filter(|t| store::is_capstone_turn(t))
        .count();
    let (open_artifact, open_note) = latest_open_questions(inputs.vault_dir, inputs.idea_slug)?;
    let mut report = gates::run(
        &mut plan,
        &GateInputs {
            evidence: &evidence,
            open_artifact: open_artifact.as_ref(),
            audit: inputs.audit,
            probe: inputs.probe,
        },
    );
    if let Some(note) = &open_note {
        report.note(format!("open-questions artifact {note}"));
    }
    let consulted = open_artifact.as_ref().map_or("none", |a| a.name.as_str());

    let stamp = inputs.now.format("%Y%m%d-%H%M%S").to_string();
    let taken = |candidate: &str| {
        store::artifact_exists(inputs.vault_dir, inputs.idea_slug, candidate).unwrap_or(false)
    };
    let file_slug = slug::disambiguate(&format!("{stamp}-build-plan"), taken);
    let body = artifact_body(
        &idea.frontmatter.title,
        &inputs,
        excluded,
        consulted,
        &plan,
        &report,
    );
    store::write_artifact(
        inputs.vault_dir,
        inputs.idea_slug,
        &Artifact {
            frontmatter: ArtifactFrontmatter {
                slug: file_slug.clone(),
                title: format!("Build plan — {}", idea.frontmatter.title),
                kind: ArtifactKind::BuildPlan,
                lens: Some(inputs.lens.to_string()),
                created: inputs.now,
                model: inputs.model.clone(),
            },
            body,
        },
    )?;
    let pointer = pointer_turn(&inputs, &file_slug, &plan, &report);
    store::append_turn(
        inputs.vault_dir,
        inputs.idea_slug,
        inputs.turn_role,
        &pointer,
    )?;
    Ok(Finished {
        artifact_slug: file_slug,
        pointer,
        report,
        plan,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::concepts::workflows::builtin_workflows;
    use crate::domain::evidence::CAPSTONE_TURNS;

    #[test]
    fn every_workflow_chaining_the_planner_is_a_capstone() {
        for w in builtin_workflows() {
            let chains_planner = w.stages.iter().any(|s| {
                matches!(s, crate::concepts::workflows::Stage::Chain(step) if step.skill == Some("build-prompt"))
            });
            if chains_planner {
                assert!(
                    CAPSTONE_TURNS.contains(&w.name),
                    "{} chains build-prompt",
                    w.name
                );
            }
        }
        assert!(CAPSTONE_TURNS.contains(&"build-prompt"));
    }

    #[test]
    fn open_questions_stems_include_same_second_suffixes() {
        assert!(is_open_questions_stem("20260901-100000-open-questions"));
        assert!(is_open_questions_stem("20260901-100000-open-questions-2"));
        assert!(!is_open_questions_stem(
            "20260901-100000-open-questions-draft"
        ));
        assert!(!is_open_questions_stem("20260901-100000-build-plan"));
    }
}
