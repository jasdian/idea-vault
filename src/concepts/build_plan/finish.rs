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
use crate::concepts::audit::Label;
use crate::concepts::build_plan::gates::{
    self, AuditView, Evidence, GateInputs, GateReport, OpenArtifact,
};
use crate::concepts::build_plan::lineage;
use crate::concepts::build_plan::plan::{self, BuildPlan, Provenance};
use crate::concepts::ConceptError;
use crate::domain::evidence::POINTER_PREFIX;
use crate::domain::frontmatter::ArtifactFrontmatter;
use crate::domain::{slug, Artifact, ArtifactKind, Recipe};
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
    /// What made the plan (ADR-0040); `None` stamps nothing, as for a plan from before recipes.
    pub recipe: Option<Recipe>,
    pub model: String,
    /// The audited harvest the planner worked from; `None` for a quick (unaudited) plan.
    pub audit: Option<&'a AuditView>,
    pub probe: &'a SourceProbe,
    pub now: DateTime<Utc>,
}

/// Which pipeline produced the plan, so the mode label can say what ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlanMode<'a> {
    /// The quick build prompt: one planner call, unaudited unless the caller passes a view.
    #[default]
    Quick,
    /// The multi-step `ready-to-build` workflow; `skipped` is why no audit ran, when none did.
    ReadyToBuild { skipped: Option<&'a str> },
    /// A version the plan workbench made from the owner's answers on `base`, re-gated without a
    /// model call or an audit (docs/adr/0032).
    Answered { base: &'a str },
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
pub(crate) fn latest_open_questions(
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
pub(crate) fn tally_line(plan: &BuildPlan, report: &GateReport) -> String {
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

pub(crate) fn mode_label(mode: PlanMode, audit: Option<&AuditView>, version: u32) -> String {
    match (mode, audit) {
        (PlanMode::Quick, None) => "quick · unaudited".into(),
        (PlanMode::Quick, Some(a)) if a.failed => "audited · audit unavailable".into(),
        (PlanMode::Quick, Some(_)) => "audited".into(),
        (PlanMode::ReadyToBuild { skipped }, None) => format!(
            "ready-to-build · audit skipped ({})",
            skipped.unwrap_or("no audit ran")
        ),
        (PlanMode::ReadyToBuild { .. }, Some(a)) if a.failed => {
            "ready-to-build · audit failed".into()
        }
        (PlanMode::ReadyToBuild { .. }, Some(a)) if a.uniform_pass => {
            "audited · uniform pass (weak)".into()
        }
        (PlanMode::ReadyToBuild { .. }, Some(_)) => "audited".into(),
        (PlanMode::Answered { base }, _) => {
            format!("v{version} · answers on {base} · audit not re-run")
        }
    }
}

/// `attached` when reference sources backed the anchor checks, else `none`.
fn sources_label(probe: &SourceProbe) -> &'static str {
    if probe.is_empty() {
        "none"
    } else {
        "attached"
    }
}

/// `2 confirmed, 1 uncertain, 1 refuted`; `failed` for an unusable audit, `none` without one.
fn audit_tally(audit: Option<&AuditView>) -> String {
    match audit {
        None => "none".into(),
        Some(a) if a.failed => "failed".into(),
        Some(a) => {
            let n = |l: Label| a.findings.iter().filter(|f| f.label == l).count();
            format!(
                "{} confirmed, {} uncertain, {} refuted",
                n(Label::Confirmed),
                n(Label::Uncertain),
                n(Label::Refuted)
            )
        }
    }
}

/// What the two code-owned header lines of a plan artifact record about the run.
pub(crate) struct RunLine<'a> {
    pub title: &'a str,
    pub mode: PlanMode<'a>,
    pub version: u32,
    pub model: &'a str,
    pub now: DateTime<Utc>,
    pub excluded: usize,
    pub consulted: &'a str,
    pub probe: &'a SourceProbe,
    pub audit: Option<&'a AuditView>,
}

/// The artifact body: a title, the two code-owned header lines, any gate notes, then the plan in
/// the canonical grammar.
pub(crate) fn artifact_body(run: &RunLine, plan: &BuildPlan, report: &GateReport) -> String {
    let mut out = format!(
        "# Build plan — {}\n_{} · {} · {} · {} capstone turn(s) excluded from evidence · consulted: {} · sources: {} · audit: {}_\n_gates: {}_\n\n",
        run.title,
        mode_label(run.mode, run.audit, run.version),
        run.model,
        run.now.format("%Y-%m-%d %H:%M"),
        run.excluded,
        run.consulted,
        sources_label(run.probe),
        audit_tally(run.audit),
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

/// How many capstone turns (earlier plans and their pointers) the evidence leaves out.
pub(crate) fn excluded_turns(conversation: &str) -> usize {
    store::split_turns(conversation)
        .iter()
        .filter(|t| store::is_capstone_turn(t))
        .count()
}

/// A new plan version to persist: its place in the lineage (docs/adr/0032) and its body.
pub(crate) struct NewPlan<'a> {
    pub vault_dir: &'a Path,
    pub idea_slug: &'a str,
    pub idea_title: &'a str,
    pub lens: Option<String>,
    pub recipe: Option<Recipe>,
    pub model: String,
    pub now: DateTime<Utc>,
    pub revises: Option<String>,
    pub version: u32,
    pub answered: Vec<String>,
    pub body: String,
}

/// Write `plan` as a fresh `<stamp>-build-plan` artifact (never over an existing one) and return
/// its stem. A version-1 root carries no lineage fields, like a plan written before lineage.
pub(crate) fn write_plan(plan: NewPlan) -> Result<String, ConceptError> {
    let stamp = plan.now.format("%Y%m%d-%H%M%S").to_string();
    let taken = |candidate: &str| {
        store::artifact_exists(plan.vault_dir, plan.idea_slug, candidate).unwrap_or(false)
    };
    let file_slug = slug::disambiguate(&format!("{stamp}-build-plan"), taken);
    store::write_artifact(
        plan.vault_dir,
        plan.idea_slug,
        &Artifact {
            frontmatter: ArtifactFrontmatter {
                slug: file_slug.clone(),
                title: format!("Build plan — {}", plan.idea_title),
                kind: ArtifactKind::BuildPlan,
                lens: plan.lens,
                created: plan.now,
                model: plan.model,
                revises: plan.revises,
                version: (plan.version > 1).then_some(plan.version),
                answered: plan.answered,
                recipe: plan.recipe,
            },
            body: plan.body,
        },
    )?;
    Ok(file_slug)
}

fn pointer_turn(
    inputs: &PlanInputs,
    mode: PlanMode,
    version: u32,
    file_slug: &str,
    plan: &BuildPlan,
    report: &GateReport,
) -> String {
    let slug = inputs.idea_slug;
    let mut out = format!(
        "{POINTER_PREFIX}{file_slug}](/idea/{slug}/artifact/{file_slug}.md) · {}\n\n{}\n",
        mode_label(mode, inputs.audit, version),
        tally_line(plan, report),
    );
    if !plan.open.is_empty() {
        // Answers go to the plan workbench, which versions the plan deterministically
        // (docs/adr/0032); the anchors are the ones the plan page renders.
        out.push_str(&format!(
            "\n**Open questions for you** — answer them on [the plan](/idea/{slug}/artifact/{file_slug}.md#work):\n"
        ));
        for q in &plan.open {
            out.push_str(&format!(
                "- [{id}](/idea/{slug}/artifact/{file_slug}.md#q-{id}): {}\n",
                q.text,
                id = q.id
            ));
        }
    }
    out
}

/// Parse, gate and persist one build plan: every read first, then the artifact, then the
/// pointer turn. An answer with neither a goal nor a task is [`ConceptError::PlanUnusable`] and
/// persists nothing. Being blocking, it runs to completion once started — aborting the job that
/// spawned it does not stop it, so a cancel after the model call still lands the plan.
pub fn finish(inputs: PlanInputs) -> Result<Finished, ConceptError> {
    finish_as(inputs, PlanMode::Quick)
}

/// [`finish`] with the pipeline that produced the plan named, so the pointer turn and the
/// artifact header label a skipped, failed or uniform audit loudly.
///
/// Every run joins the idea's plan lineage (docs/adr/0032): the new plan revises the current
/// head, the owner answers recorded on the head's chain are carried into it, and an Open
/// question re-asking one of them is dropped before the gates run.
pub fn finish_as(inputs: PlanInputs, mode: PlanMode) -> Result<Finished, ConceptError> {
    let mut plan = plan::parse(inputs.answer).map_err(|_| ConceptError::PlanUnusable)?;
    let idea = store::read_idea(inputs.vault_dir, inputs.idea_slug)?;
    let conversation = store::read_conversation(inputs.vault_dir, inputs.idea_slug)?;
    let evidence = Evidence::new(&idea.body, &conversation);
    let excluded = excluded_turns(&conversation);
    let (open_artifact, open_note) = latest_open_questions(inputs.vault_dir, inputs.idea_slug)?;
    let plans = lineage::list_plans(inputs.vault_dir, inputs.idea_slug)?;
    let head = lineage::head(&plans).cloned();
    let answers = match &head {
        Some(h) => lineage::answered_in_lineage(inputs.vault_dir, inputs.idea_slug, &h.stem)?,
        None => Vec::new(),
    };
    let mut suppressed = GateReport::default();
    lineage::carry_answers(&mut plan, &answers);
    lineage::suppress_answered(&mut plan, &answers, &mut suppressed);
    lineage::renumber_reused_questions(&mut plan, &answers);
    let mut report = gates::run(
        &mut plan,
        &GateInputs {
            evidence: &evidence,
            open_artifact: open_artifact.as_ref(),
            audit: inputs.audit,
            probe: inputs.probe,
            answered: &answers,
        },
    );
    report.notes.splice(0..0, suppressed.notes);
    if let Some(note) = &open_note {
        report.note(format!("open-questions artifact {note}"));
    }
    let consulted = open_artifact.as_ref().map_or("none", |a| a.name.as_str());
    let version = head.as_ref().map_or(1, |h| h.version + 1);
    let body = artifact_body(
        &RunLine {
            title: &idea.frontmatter.title,
            mode,
            version,
            model: &inputs.model,
            now: inputs.now,
            excluded,
            consulted,
            probe: inputs.probe,
            audit: inputs.audit,
        },
        &plan,
        &report,
    );
    let file_slug = write_plan(NewPlan {
        vault_dir: inputs.vault_dir,
        idea_slug: inputs.idea_slug,
        idea_title: &idea.frontmatter.title,
        lens: Some(inputs.lens.to_string()),
        recipe: inputs.recipe.clone(),
        model: inputs.model.clone(),
        now: inputs.now,
        revises: head.map(|h| h.stem),
        version,
        answered: Vec::new(),
        body,
    })?;
    let pointer = pointer_turn(&inputs, mode, version, &file_slug, &plan, &report);
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
    use crate::concepts::build_plan::gates::AuditedFinding;
    use crate::concepts::workflows::WorkflowRegistry;
    use crate::domain::evidence::CAPSTONE_TURNS;

    #[test]
    fn every_workflow_chaining_the_planner_is_a_capstone() {
        let skills = crate::concepts::skills::SkillRegistry::builtin();
        for w in WorkflowRegistry::builtin(&skills).list() {
            let chains_planner = w.stages.iter().any(|s| {
                matches!(s, crate::concepts::workflows::Stage::Chain(step) if step.skill.as_deref() == Some("build-prompt"))
            });
            assert_eq!(chains_planner, w.capstone, "{}", w.name);
            if chains_planner {
                assert!(
                    CAPSTONE_TURNS.contains(&w.name.as_str()),
                    "{} chains build-prompt",
                    w.name
                );
            }
        }
        assert!(CAPSTONE_TURNS.contains(&"build-prompt"));
    }

    #[test]
    fn a_ready_to_build_plan_is_never_labelled_quick() {
        let skipped = PlanMode::ReadyToBuild {
            skipped: Some("audit off in Settings"),
        };
        assert_eq!(
            mode_label(skipped, None, 1),
            "ready-to-build · audit skipped (audit off in Settings)"
        );
        let failed = AuditView {
            failed: true,
            ..AuditView::default()
        };
        assert_eq!(
            mode_label(skipped, Some(&failed), 1),
            "ready-to-build · audit failed"
        );
        let uniform = AuditView {
            uniform_pass: true,
            ..AuditView::default()
        };
        assert_eq!(
            mode_label(skipped, Some(&uniform), 1),
            "audited · uniform pass (weak)"
        );
        assert_eq!(mode_label(PlanMode::Quick, None, 1), "quick · unaudited");
    }

    #[test]
    fn header_names_sources_and_the_audit_tally() {
        assert_eq!(sources_label(&SourceProbe::default()), "none");
        assert_eq!(audit_tally(None), "none");
        let finding = |label| AuditedFinding {
            text: "x".into(),
            lenses: vec![],
            label,
            reason: String::new(),
        };
        let view = AuditView {
            findings: vec![
                finding(Label::Confirmed),
                finding(Label::Confirmed),
                finding(Label::Uncertain),
                finding(Label::Refuted),
            ],
            ..AuditView::default()
        };
        assert_eq!(
            audit_tally(Some(&view)),
            "2 confirmed, 1 uncertain, 1 refuted"
        );
        let failed = AuditView {
            failed: true,
            ..view
        };
        assert_eq!(audit_tally(Some(&failed)), "failed");
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

    #[test]
    fn capstone_run_links_to_head_and_suppresses_answered() {
        use crate::concepts::build_plan::workbench::tests::{
            at, seeded, submit, BASE, Q1_ANSWER, SLUG,
        };
        let dir = seeded();
        let v2 = submit(dir.path(), BASE, &[("Q1", Q1_ANSWER)], 1).unwrap();
        // The model renumbers, re-asks the answered question and drops the owner's answer.
        let replan = "## Goal\nShip the zone snapshot tool.\n\n## Settled\n\
- S1: Disproof comes before any code.\n  quote: \"the cheapest disproof before any Rust exists\"\n\n\
## Open questions\n- Q1: Which exchange feeds the backtest data?\n\
- Q2: Should the zone snapshot freeze at entry or dwell on the live label?\n\n\
## Plan\n- [ ] T1: Build the snapshot freezer\n  depends: Q2 (which rule)\n  touches: `src/snap.rs`\n  accept: `cargo test snap` → exit 0\n\n\
## Kill criteria\n- K1: The freezer loses fills → stop\n  checked by: T1\n  gates: T1\n";
        let probe = SourceProbe::default();
        let done = finish(PlanInputs {
            vault_dir: dir.path(),
            idea_slug: SLUG,
            answer: replan,
            turn_role: "assistant (skill: build-prompt)",
            lens: "build-prompt",
            recipe: None,
            model: "llama3.2".into(),
            audit: None,
            probe: &probe,
            now: at(2),
        })
        .unwrap();
        let fm = store::read_artifact(dir.path(), SLUG, &done.artifact_slug)
            .unwrap()
            .frontmatter;
        assert_eq!(fm.revises.as_deref(), Some(v2.stem.as_str()));
        assert_eq!(fm.version, Some(3));
        assert!(fm.answered.is_empty());
        let open: Vec<&str> = done.plan.open.iter().map(|q| q.text.as_str()).collect();
        assert_eq!(open, ["Which exchange feeds the backtest data?"]);
        assert_eq!(done.plan.tasks[0].field("depends"), None);
        assert!(!done.plan.tasks[0].needs_owner, "{:?}", done.plan.tasks[0]);
        let carried = done
            .plan
            .settled
            .iter()
            .find(|s| s.field("answers") == Some("Q1"))
            .expect("the owner's answer is carried");
        assert_eq!(carried.provenance, Some(Provenance::Owner));
        assert_eq!(carried.field("quote"), Some(Q1_ANSWER));
        assert!(
            done.report.notes.contains(&format!(
                "Q2 dropped: already answered in {} ({BASE})",
                carried.id
            )),
            "{:?}",
            done.report.notes
        );
    }
}
