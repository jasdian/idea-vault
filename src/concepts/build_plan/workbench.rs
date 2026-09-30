//! The plan workbench (docs/adr/0032): the owner answers a build plan's open questions and
//! owner-blocked tasks in place, and each submission makes a new plan version deterministically —
//! no model call, no job slot, no semaphore permit. Each answer lands as a plain `## user` turn,
//! so it is Owner evidence for this and every later plan; the new version re-parses its base,
//! folds the answers in and re-runs the gates (without the audit) against fresh evidence. The
//! base artifact is never modified.
//!
//! [`answer`] is blocking (vault reads and writes, the bounded source probe); callers run it in
//! `spawn_blocking`. Refusing an answer while a model job is running for the idea is the
//! caller's check: `concepts` knows nothing of the web layer's jobs (D4).

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use chrono::{DateTime, Utc};

use crate::ai::provenance;
use crate::ai::sources::SourceProbe;
use crate::concepts::build_plan::finish::{
    artifact_body, excluded_turns, latest_open_questions, mode_label, write_plan, NewPlan,
    PlanMode, RunLine,
};
use crate::concepts::build_plan::gates::{
    self, claims, Answered, Evidence, GateInputs, GateReport,
};
use crate::concepts::build_plan::lineage::{self, answer_item, drop_question_deps, PlanRef};
use crate::concepts::build_plan::plan::{
    self, next_free_id, parse_header, reset_derived, BuildPlan, Item, RunHeader, OWNER_MODEL,
};
use crate::concepts::ConceptError;
use crate::domain::evidence::{
    content_overlap, normalize_for_match, MIN_QUOTE_WORDS, POINTER_PREFIX,
};
use crate::domain::{Artifact, ArtifactKind, Recipe};
use crate::vault::{store, VaultError};

/// Serialises the head check and the writes of every answer, so two submissions on one plan
/// cannot both pass the head check and fork the lineage.
pub static WORKBENCH_LOCK: Mutex<()> = Mutex::new(());

/// The longest answer accepted, in bytes.
pub const MAX_ANSWER_BYTES: usize = 2000;

/// How much of the question an answer may repeat before it is the question, not an answer.
const COPIED_RATIO: f64 = 0.8;

/// The markers that hold a task for its owner; any other marker is advisory.
const OWNER_REASONS: &[&str] = &[
    "blocked by ",
    "touches fenced ",
    "dependency cycle",
    "cycle:",
    "no runnable accept",
    "destructive command",
    "needs you",
    "depends on quarantined",
];

/// The one owner-hold reason an answer in the owner's words can lift: the task itself asks for
/// the owner's hand (G9's `needs you`). Every other reason is structural and needs a re-plan.
const ANSWERABLE_REASON: &str = "needs you";

/// Why a task shows `[?]` when the model wrote the box.
const MODEL_REASON: &str = "the plan asks you to decide";

/// Where an answer was submitted from, noted in the pointer turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerChannel {
    Web,
    Mcp,
}

/// One submission of owner answers on the plan `base`.
pub struct AnswerRequest<'a> {
    pub vault_dir: &'a Path,
    pub idea_slug: &'a str,
    pub base: &'a str,
    /// `("Q6" | "T4", owner words)`, in id order. An empty answer is ignored.
    pub answers: &'a [(String, String)],
    pub probe: &'a SourceProbe,
    pub now: DateTime<Utc>,
    pub via: AnswerChannel,
}

/// The version an answer made (or, for an identical resubmission, found).
#[derive(Debug, Clone, PartialEq)]
pub struct Versioned {
    pub stem: String,
    pub version: u32,
    pub revises: String,
    pub answered: Vec<String>,
    /// Tasks that held `[?]` on the base and no longer do.
    pub unblocked: Vec<String>,
    pub still_open: Vec<String>,
    pub plan: BuildPlan,
    pub report: GateReport,
    /// An identical earlier submission already made this version; nothing was written but its
    /// pointer turn, when that earlier submission failed before writing it.
    pub reused: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkbenchError {
    #[error("{0} is not a build plan")]
    NotAPlan(String),
    #[error("no build plan {0}")]
    NotFound(String),
    #[error("this plan was superseded — answer on {head}")]
    Superseded { head: String },
    #[error("{0} is not an open question or a task on this plan")]
    UnknownId(String),
    #[error("{0} cannot be answered here — re-plan with the model instead")]
    NotAnswerable(String),
    #[error("{0}: answer in at least {min} words of your own", min = MIN_QUOTE_WORDS)]
    TooShort(String),
    #[error("{0}: answers are at most {max} bytes", max = MAX_ANSWER_BYTES)]
    TooLong(String),
    #[error("{0}: that repeats the question — answer it in your own words")]
    NotOwnWords(String),
    #[error("no answer given")]
    NothingToAnswer,
    #[error("{0} is answered twice — give one answer per id")]
    DuplicateId(String),
    #[error(transparent)]
    Concept(#[from] ConceptError),
}

impl From<VaultError> for WorkbenchError {
    fn from(e: VaultError) -> Self {
        WorkbenchError::Concept(ConceptError::Vault(e))
    }
}

/// An open question as the workbench shows it: the tasks it holds back included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenQ {
    pub id: String,
    pub text: String,
    pub markers: Vec<String>,
    pub blocks: Vec<String>,
}

/// A task held for the owner: what holds it, why, and whether an answer can release it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedT {
    pub id: String,
    pub text: String,
    pub blocked_by: Vec<String>,
    pub reasons: Vec<String>,
    pub answerable: bool,
}

/// One plan version as the workbench (web page and MCP `get_plan`) shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanView {
    pub stem: String,
    pub version: u32,
    pub revises: Option<String>,
    pub superseded_by: Vec<String>,
    pub head: String,
    pub is_head: bool,
    /// Root first, this version last.
    pub lineage: Vec<(String, u32)>,
    /// The `Q#`/`T#` ids answered to make this version.
    pub answered: Vec<String>,
    pub header: RunHeader,
    pub plan: BuildPlan,
    pub open: Vec<OpenQ>,
    pub blocked: Vec<BlockedT>,
}

impl PlanView {
    pub fn to_json(&self) -> serde_json::Value {
        let settled: Vec<serde_json::Value> = self
            .plan
            .settled
            .iter()
            .map(|s| {
                serde_json::json!({
                    "id": s.id,
                    "text": s.text,
                    "by": s.provenance.map(|p| p.label()),
                    "answers": s.field("answers").or(s.field("unblocks")),
                })
            })
            .collect();
        serde_json::json!({
            "plan": self.stem,
            "version": self.version,
            "revises": self.revises,
            "superseded_by": self.superseded_by,
            "head": self.head,
            "is_head": self.is_head,
            "lineage": self.lineage.iter().map(|(s, v)| serde_json::json!({"plan": s, "version": v})).collect::<Vec<_>>(),
            "answered": self.answered,
            "mode": self.header.mode,
            "gates": self.header.gates,
            "goal": self.plan.goal,
            "settled": settled,
            "open": self.open.iter().map(|q| serde_json::json!({
                "id": q.id, "text": q.text, "markers": q.markers, "blocks": q.blocks,
            })).collect::<Vec<_>>(),
            "blocked": self.blocked.iter().map(|t| serde_json::json!({
                "id": t.id, "text": t.text, "blocked_by": t.blocked_by,
                "reasons": t.reasons, "answerable": t.answerable,
            })).collect::<Vec<_>>(),
            "tasks": self.plan.tasks.iter().map(|t| serde_json::json!({
                "id": t.id, "text": t.text, "needs_owner": t.needs_owner,
            })).collect::<Vec<_>>(),
        })
    }
}

/// The inline warning for an answer that hedges: G2 keeps a hedged answer open, so the owner
/// hears it before submitting.
pub fn hedge_warning(answer: &str) -> Option<&'static str> {
    claims::hedge(answer)
}

/// Whether the model wrote `task`'s `[?]`: an `owner: model` field, or on a plan from before that
/// field existed a `[?]` no gate marker explains.
fn model_owned(task: &Item) -> bool {
    task.field("owner") == Some(OWNER_MODEL) || (task.needs_owner && task.markers.is_empty())
}

/// A `[?]` task's hold as the workbench shows it; `None` for a task nothing holds.
fn blocked_task(task: &Item) -> Option<BlockedT> {
    if !task.needs_owner {
        return None;
    }
    let held: Vec<&String> = task
        .markers
        .iter()
        .filter(|m| OWNER_REASONS.iter().any(|r| m.starts_with(r)))
        .collect();
    let mut reasons: Vec<String> = Vec::new();
    if model_owned(task) {
        reasons.push(MODEL_REASON.to_string());
    }
    reasons.extend(held.iter().map(|m| m.to_string()));
    let answerable = !reasons.is_empty()
        && held.iter().all(|m| m.as_str() == ANSWERABLE_REASON)
        && (model_owned(task) || !held.is_empty());
    let blocked_by = held
        .iter()
        .filter_map(|m| m.strip_prefix("blocked by "))
        .map(|id| id.trim().to_string())
        .collect();
    Some(BlockedT {
        id: task.id.clone(),
        text: task.text.clone(),
        blocked_by,
        reasons,
        answerable,
    })
}

fn open_questions(plan: &BuildPlan) -> Vec<OpenQ> {
    plan.open
        .iter()
        .map(|q| OpenQ {
            id: q.id.clone(),
            text: q.text.clone(),
            markers: q.markers.clone(),
            blocks: plan
                .tasks
                .iter()
                .filter(|t| t.depends_questions().contains(&q.id))
                .map(|t| t.id.clone())
                .collect(),
        })
        .collect()
}

/// Read `stem` and require a build plan.
fn read_plan(vault_dir: &Path, slug: &str, stem: &str) -> Result<Artifact, WorkbenchError> {
    let artifact = match store::read_artifact(vault_dir, slug, stem) {
        Ok(a) => a,
        Err(VaultError::ArtifactNotFound(_)) => {
            return Err(WorkbenchError::NotFound(stem.to_string()))
        }
        Err(e) => return Err(e.into()),
    };
    if artifact.frontmatter.kind != ArtifactKind::BuildPlan {
        return Err(WorkbenchError::NotAPlan(stem.to_string()));
    }
    Ok(artifact)
}

fn parse_plan(artifact: &Artifact) -> Result<BuildPlan, WorkbenchError> {
    plan::parse_artifact(&artifact.body).map_err(|_| ConceptError::PlanUnusable.into())
}

/// One plan version of `slug` for the workbench: `stem`, or the lineage head when `None`.
pub fn plan_view(
    vault_dir: &Path,
    slug: &str,
    stem: Option<&str>,
) -> Result<PlanView, WorkbenchError> {
    let plans = lineage::list_plans(vault_dir, slug)?;
    let head = lineage::head(&plans)
        .map(|h| h.stem.clone())
        .ok_or_else(|| WorkbenchError::NotFound("yet".to_string()))?;
    let stem = stem.unwrap_or(&head).to_string();
    let artifact = read_plan(vault_dir, slug, &stem)?;
    let parsed = parse_plan(&artifact)?;
    let fm = &artifact.frontmatter;
    let mut chain: Vec<(String, u32)> = lineage::chain(&plans, &stem)
        .iter()
        .map(|p| (p.stem.clone(), p.version))
        .collect();
    chain.reverse();
    Ok(PlanView {
        version: fm.version.unwrap_or(1),
        revises: fm.revises.clone(),
        superseded_by: lineage::successors(&plans, &stem),
        is_head: stem == head,
        head,
        lineage: chain,
        answered: fm.answered.clone(),
        header: parse_header(&artifact.body),
        open: open_questions(&parsed),
        blocked: parsed.tasks.iter().filter_map(blocked_task).collect(),
        plan: parsed,
        stem,
    })
}

/// An answer on one line, for a field value: the owner's words with their whitespace collapsed.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The question text an answer to `id` is checked against: an open question's text or an
/// answerable task's.
fn asked_for(plan: &BuildPlan, id: &str) -> Result<String, WorkbenchError> {
    if let Some(q) = plan.open.iter().find(|q| q.id == id) {
        let text = q.text.strip_prefix("proposed:").unwrap_or(&q.text).trim();
        return Ok(one_line(text));
    }
    match plan.tasks.iter().find(|t| t.id == id) {
        Some(t) if blocked_task(t).is_some_and(|b| b.answerable) => Ok(one_line(&t.text)),
        Some(_) => Err(WorkbenchError::NotAnswerable(id.to_string())),
        None => Err(WorkbenchError::UnknownId(id.to_string())),
    }
}

fn validate(id: &str, answer: &str, asked: &str) -> Result<(), WorkbenchError> {
    if answer.len() > MAX_ANSWER_BYTES {
        return Err(WorkbenchError::TooLong(id.to_string()));
    }
    let words = normalize_for_match(answer)
        .split_whitespace()
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .count();
    if words < MIN_QUOTE_WORDS {
        return Err(WorkbenchError::TooShort(id.to_string()));
    }
    if content_overlap(answer, asked).at_least(COPIED_RATIO, MIN_QUOTE_WORDS) {
        return Err(WorkbenchError::NotOwnWords(id.to_string()));
    }
    Ok(())
}

/// Any item of `plan` recording the owner's answer to `id`, wherever the gates left it. The
/// newest such item wins: an answer is appended after any older item that carried one to the
/// same id, so a plan written before ids were kept unique still reports its latest answer.
fn answer_holder<'a>(plan: &'a BuildPlan, id: &str) -> Option<&'a Item> {
    plan.settled
        .iter()
        .chain(&plan.verify)
        .chain(&plan.open)
        .chain(&plan.quarantined)
        .rfind(|i| i.field("answers") == Some(id) || i.field("unblocks") == Some(id))
}

/// The successor of the base an identical earlier submission made, if there is one.
fn reused(
    req: &AnswerRequest,
    plans: &[PlanRef],
    answers: &[(String, String)],
) -> Result<Option<Versioned>, WorkbenchError> {
    let ids: BTreeSet<&str> = answers.iter().map(|(id, _)| id.as_str()).collect();
    for succ in plans
        .iter()
        .filter(|p| p.revises.as_deref() == Some(req.base))
    {
        let theirs: BTreeSet<&str> = succ.answered.iter().map(String::as_str).collect();
        if theirs != ids {
            continue;
        }
        let Ok(parsed) =
            read_plan(req.vault_dir, req.idea_slug, &succ.stem).and_then(|a| parse_plan(&a))
        else {
            continue;
        };
        let same = answers.iter().all(|(id, text)| {
            answer_holder(&parsed, id).and_then(|i| i.field("quote")) == Some(text.as_str())
        });
        if same {
            return Ok(Some(Versioned {
                stem: succ.stem.clone(),
                version: succ.version,
                revises: req.base.to_string(),
                answered: succ.answered.clone(),
                unblocked: Vec::new(),
                still_open: parsed.open.iter().map(|q| q.id.clone()).collect(),
                plan: parsed,
                report: GateReport::default(),
                reused: true,
            }));
        }
    }
    Ok(None)
}

/// Fold the answers into a reset plan: an answered question leaves Open for a Settled item
/// holding the owner's words and every `depends` citing it; an answered task loses its `[?]` and
/// gains a Settled item that says what unblocked it.
fn apply_answers(plan: &mut BuildPlan, answers: &[Answered]) {
    for a in answers {
        if a.qid.starts_with('Q') {
            plan.open.retain(|q| q.id != a.qid);
            for task in &mut plan.tasks {
                drop_question_deps(task, std::slice::from_ref(&a.qid));
            }
            let id = next_free_id(&plan.settled, 'S');
            plan.settled.push(answer_item(&id, a));
            continue;
        }
        if let Some(task) = plan.tasks.iter_mut().find(|t| t.id == a.qid) {
            task.fields
                .insert("unblocked".to_string(), a.answer.clone());
            task.fields.remove("owner");
            task.needs_owner = false;
        }
        let id = next_free_id(&plan.settled, 'S');
        let mut item = Item::new(
            &id,
            &crate::concepts::audit::clip(&a.answer, lineage::ANSWER_TEXT_BYTES),
        );
        item.fields.insert("quote".to_string(), a.answer.clone());
        item.fields.insert("unblocks".to_string(), a.qid.clone());
        item.fields.insert("in".to_string(), a.in_stem.clone());
        plan.settled.push(item);
    }
}

/// Put every owner answer found in Verify first back in Settled, unmarked, before the gates run:
/// no gate moves an owner answer (ADR-0030 §G6), but `reset_derived` keeps a premise's move
/// reason, so an answer stored in Verify first would otherwise stay a premise for good
/// (docs/adr/0032).
fn restore_answers(plan: &mut BuildPlan) {
    let (answers, kept): (Vec<Item>, Vec<Item>) = std::mem::take(&mut plan.verify)
        .into_iter()
        .partition(|i| i.field("answers").is_some() || i.field("unblocks").is_some());
    plan.verify = kept;
    for mut item in answers {
        item.id = next_free_id(&plan.settled, 'S');
        item.markers.clear();
        item.fields.remove("check");
        plan.settled.push(item);
    }
}

/// `**Build plan** → [<stem>](…) · v3 · answers on <base> · audit not re-run`, then what the
/// answers did.
fn answer_pointer(req: &AnswerRequest, stem: &str, version: u32, v: &Versioned) -> String {
    let moved: Vec<String> = v
        .answered
        .iter()
        .map(|id| match answer_holder(&v.plan, id) {
            Some(item) => format!("{id}→{}", item.id),
            None => id.clone(),
        })
        .collect();
    let mut parts = vec![format!("answered {}", moved.join(", "))];
    if !v.unblocked.is_empty() {
        parts.push(format!("unblocked {}", v.unblocked.join(", ")));
    }
    parts.push(if v.still_open.is_empty() {
        "open: none".to_string()
    } else {
        format!("open: {}", v.still_open.join(", "))
    });
    if req.via == AnswerChannel::Mcp {
        parts.push("via MCP".to_string());
    }
    format!(
        "{POINTER_PREFIX}{stem}](/idea/{}/artifact/{stem}.md) · {}\n\n{}\n",
        req.idea_slug,
        mode_label(PlanMode::Answered { base: req.base }, None, version),
        parts.join(" · ")
    )
}

/// Answer open questions and owner-blocked tasks on the plan `req.base`, making its next version
/// (docs/adr/0032, decision order in its §answer): an identical earlier submission is returned
/// as is; only the lineage head takes answers; every answer is validated before anything is
/// written; then one `## user` turn per answer, the new artifact and one pointer turn.
pub fn answer(req: AnswerRequest) -> Result<Versioned, WorkbenchError> {
    let _held = WORKBENCH_LOCK
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let answers: Vec<(String, String)> = req
        .answers
        .iter()
        .map(|(id, text)| (id.trim().to_ascii_uppercase(), text.trim().to_string()))
        .filter(|(_, text)| !text.is_empty())
        .collect();
    if answers.is_empty() {
        return Err(WorkbenchError::NothingToAnswer);
    }
    // `q6` and `Q6` normalize to one id: two answers to it would write two owner turns and two
    // Settled items, and no later identical submission could be recognised as a repeat.
    let mut ids_seen: BTreeSet<&str> = BTreeSet::new();
    if let Some((dup, _)) = answers.iter().find(|(id, _)| !ids_seen.insert(id.as_str())) {
        return Err(WorkbenchError::DuplicateId(dup.clone()));
    }
    let base = read_plan(req.vault_dir, req.idea_slug, req.base)?;
    let plans = lineage::list_plans(req.vault_dir, req.idea_slug)?;
    let flat: Vec<(String, String)> = answers
        .iter()
        .map(|(id, text)| (id.clone(), one_line(text)))
        .collect();
    if let Some(found) = reused(&req, &plans, &flat)? {
        // The earlier submission may have written its version and then failed on the pointer
        // turn; the retry lands the pointer so the transcript still links the version.
        let conversation = store::read_conversation(req.vault_dir, req.idea_slug)?;
        if !conversation.contains(&format!("{POINTER_PREFIX}{}]", found.stem)) {
            let pointer = answer_pointer(&req, &found.stem, found.version, &found);
            store::append_turn(req.vault_dir, req.idea_slug, pointer_role(&base), &pointer)?;
        }
        return Ok(found);
    }
    let head = lineage::head(&plans).map_or(req.base, |h| h.stem.as_str());
    if head != req.base {
        return Err(WorkbenchError::Superseded {
            head: head.to_string(),
        });
    }
    let parsed = parse_plan(&base)?;
    let mut asked = Vec::new();
    for (id, text) in &answers {
        let question = asked_for(&parsed, id)?;
        validate(id, text, &question)?;
        asked.push(question);
    }

    // Every read that can fail runs before the first turn is written, so an I/O error leaves
    // the transcript untouched rather than holding answers no version records.
    let idea = store::read_idea(req.vault_dir, req.idea_slug)?;
    let (open_artifact, open_note) = latest_open_questions(req.vault_dir, req.idea_slug)?;
    let mut answered = lineage::answered_in_lineage(req.vault_dir, req.idea_slug, req.base)?;
    let mut conversation = store::read_conversation(req.vault_dir, req.idea_slug)?;
    // One write for the whole batch. A retry after the artifact write failed finds the batch
    // already at the transcript's tail (the base is still the head and nothing else may append
    // while the workbench lock is held and no job runs) and does not write the answers twice.
    let turns: String = answers
        .iter()
        .map(|(id, text)| store::format_turn("user", &format!("Re {id} ({}): {text}", req.base)))
        .collect();
    if !conversation.ends_with(&turns) {
        store::append_conversation(req.vault_dir, req.idea_slug, &turns)?;
        conversation.push_str(&turns);
    }
    let this: Vec<Answered> = flat
        .iter()
        .zip(asked)
        .map(|((id, text), asked)| Answered {
            qid: id.clone(),
            asked,
            answer: text.clone(),
            in_stem: req.base.to_string(),
        })
        .collect();
    let held_before: Vec<String> = parsed
        .tasks
        .iter()
        .filter(|t| t.needs_owner)
        .map(|t| t.id.clone())
        .collect();
    let mut plan = parsed;
    reset_derived(&mut plan);
    restore_answers(&mut plan);
    apply_answers(&mut plan, &this);

    let evidence = Evidence::new(&idea.body, &conversation);
    answered.retain(|a| !this.iter().any(|t| t.qid == a.qid));
    answered.extend(this.iter().cloned());
    let mut report = gates::run(
        &mut plan,
        &GateInputs {
            evidence: &evidence,
            open_artifact: open_artifact.as_ref(),
            audit: None,
            probe: req.probe,
            answered: &answered,
        },
    );
    if let Some(note) = &open_note {
        report.note(format!("open-questions artifact {note}"));
    }

    let version = base.frontmatter.version.unwrap_or(1) + 1;
    let ids: Vec<String> = flat.iter().map(|(id, _)| id.clone()).collect();
    let body = artifact_body(
        &RunLine {
            title: &idea.frontmatter.title,
            mode: PlanMode::Answered { base: req.base },
            version,
            model: &base.frontmatter.model,
            now: req.now,
            excluded: excluded_turns(&conversation),
            consulted: open_artifact.as_ref().map_or("none", |a| a.name.as_str()),
            probe: req.probe,
            audit: None,
        },
        &plan,
        &report,
    );
    let stem = write_plan(NewPlan {
        vault_dir: req.vault_dir,
        idea_slug: req.idea_slug,
        idea_title: &idea.frontmatter.title,
        lens: base.frontmatter.lens.clone(),
        // The version is the base's plan with the owner's answers folded in by code: it keeps
        // the base's recipe, stamped with the build that folded them (ADR-0040).
        recipe: base.frontmatter.recipe.clone().map(|r| Recipe {
            build: provenance::build_id(),
            ..r
        }),
        model: base.frontmatter.model.clone(),
        now: req.now,
        revises: Some(req.base.to_string()),
        version,
        answered: ids.clone(),
        body,
    })?;
    let versioned = Versioned {
        unblocked: held_before
            .into_iter()
            .filter(|id| plan.tasks.iter().any(|t| &t.id == id && !t.needs_owner))
            .collect(),
        still_open: plan.open.iter().map(|q| q.id.clone()).collect(),
        stem,
        version,
        revises: req.base.to_string(),
        answered: ids,
        plan,
        report,
        reused: false,
    };
    let pointer = answer_pointer(&req, &versioned.stem, version, &versioned);
    store::append_turn(req.vault_dir, req.idea_slug, pointer_role(&base), &pointer)?;
    Ok(versioned)
}

/// The pointer sits under the base's own capstone heading, so it stays out of evidence
/// (ADR-0030) and counts toward the same skill or workflow (ADR-0022).
fn pointer_role(base: &Artifact) -> &'static str {
    match base.frontmatter.lens.as_deref() {
        Some("ready-to-build") => "assistant (workflow: ready-to-build)",
        _ => "assistant (skill: build-prompt)",
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::concepts::build_plan::finish::{finish, PlanInputs};
    use crate::domain::{Idea, IdeaFrontmatter, IdeaState};
    use chrono::TimeZone;

    pub(crate) const SLUG: &str = "trader";
    pub(crate) const BASE: &str = "20260929-120000-build-plan";

    pub(crate) const Q1_ASKED: &str =
        "Freeze the zone snapshot at entry, or dwell on the live label?";
    pub(crate) const Q1_ANSWER: &str =
        "Freeze it at entry; dwell makes the backtest lie about fills.";

    pub(crate) const PLAN: &str = "## Goal
Ship the zone snapshot tool.

## Settled
- S1: Disproof comes before any code.
  quote: \"the cheapest disproof before any Rust exists\"

## Open questions
- Q1: Freeze the zone snapshot at entry, or dwell on the live label?
- Q2: Which exchange feeds the backtest data?

## Plan
- [ ] T1: Write the spec
  touches: `SPEC.md`
  accept: `test -s SPEC.md` → exit 0
- [ ] T2: Build the snapshot freezer
  depends: T1, Q1
  touches: `src/snap.rs`
  accept: `cargo test snap` → exit 0
- [ ] T3: Wire the freezer into the runner
  depends: T2
  touches: `src/run.rs`
  accept: `cargo test run` → exit 0
- [?] T4: Pick the broker account to trade on
  touches: `config.toml`
  accept: `test -s config.toml` → exit 0

## Kill criteria
- K1: The spec cannot name a dated kill → stop
  checked by: T1
  gates: T2";

    pub(crate) fn at(minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 29, 12, minute, 0).unwrap()
    }

    /// An idea with an owner turn and a first plan, `BASE`, made from [`PLAN`].
    pub(crate) fn seeded() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        store::write_idea(
            dir.path(),
            &Idea {
                frontmatter: IdeaFrontmatter {
                    title: "Trader".into(),
                    slug: SLUG.into(),
                    state: IdeaState::InDiscussion,
                    tags: vec![],
                    sources: vec![],
                    created: at(0),
                    updated: at(0),
                },
                body: "A zone snapshot trading tool.\n".into(),
            },
        )
        .unwrap();
        store::append_turn(
            dir.path(),
            SLUG,
            "user",
            "We run the cheapest disproof before any Rust exists.",
        )
        .unwrap();
        let probe = SourceProbe::default();
        let done = finish(PlanInputs {
            vault_dir: dir.path(),
            idea_slug: SLUG,
            answer: PLAN,
            turn_role: "assistant (skill: build-prompt)",
            lens: "build-prompt",
            recipe: None,
            model: "llama3.2".into(),
            audit: None,
            probe: &probe,
            now: at(0),
        })
        .unwrap();
        assert_eq!(done.artifact_slug, BASE);
        dir
    }

    pub(crate) fn submit(
        dir: &Path,
        base: &str,
        answers: &[(&str, &str)],
        minute: u32,
    ) -> Result<Versioned, WorkbenchError> {
        let answers: Vec<(String, String)> = answers
            .iter()
            .map(|(id, a)| (id.to_string(), a.to_string()))
            .collect();
        answer(AnswerRequest {
            vault_dir: dir,
            idea_slug: SLUG,
            base,
            answers: &answers,
            probe: &SourceProbe::default(),
            now: at(minute),
            via: AnswerChannel::Web,
        })
    }

    /// Rewrite `stem` on disk as a plan written before owner answers were exempt from G6: the
    /// Settled item `sid` sits in Verify first as `P1`, with `marker`.
    pub(crate) fn demote_to_verify(dir: &Path, stem: &str, sid: &str, marker: &str) {
        let path = dir.join(SLUG).join("artifacts").join(format!("{stem}.md"));
        let text = std::fs::read_to_string(&path).unwrap();
        let mut kept = Vec::new();
        let mut block = Vec::new();
        let mut inside = false;
        for line in text.lines() {
            if line.starts_with(&format!("- {sid}: ")) {
                inside = true;
                let rest = line.trim_start_matches(&format!("- {sid}: "));
                block.push(format!("- P1: {rest} ⟨{marker}⟩"));
                continue;
            }
            if inside && line.starts_with("  ") {
                block.push(line.to_string());
                continue;
            }
            inside = false;
            if line == "## Open questions" {
                kept.push("## Verify first".to_string());
                kept.append(&mut block);
                kept.push(String::new());
            }
            kept.push(line.to_string());
        }
        assert!(block.is_empty(), "no Open questions section in {stem}");
        std::fs::write(&path, kept.join("\n") + "\n").unwrap();
    }

    fn snapshot(dir: &Path) -> (String, Vec<store::ArtifactFile>) {
        (
            store::read_conversation(dir, SLUG).unwrap(),
            store::list_artifact_files(dir, SLUG).unwrap(),
        )
    }

    fn task<'a>(plan: &'a BuildPlan, id: &str) -> &'a Item {
        plan.tasks.iter().find(|t| t.id == id).unwrap()
    }

    #[test]
    fn answer_q_moves_to_settled_owner_and_unblocks() {
        let dir = seeded();
        let base = plan_view(dir.path(), SLUG, None).unwrap();
        assert!(task(&base.plan, "T2").needs_owner && task(&base.plan, "T3").needs_owner);
        assert_eq!(base.open[0].blocks, ["T2"]);

        let v = submit(dir.path(), BASE, &[("Q1", Q1_ANSWER)], 1).unwrap();
        assert!(!v.reused);
        assert_eq!((v.version, v.revises.as_str()), (2, BASE));
        assert_eq!(v.answered, ["Q1"]);
        assert_eq!(v.still_open, ["Q2"]);
        assert_eq!(v.unblocked, ["T2", "T3"]);
        let s = v
            .plan
            .settled
            .iter()
            .find(|s| s.field("answers") == Some("Q1"))
            .expect("the answer is Settled");
        assert_eq!(
            s.provenance,
            Some(crate::concepts::build_plan::plan::Provenance::Owner)
        );
        assert_eq!(s.field("quote"), Some(Q1_ANSWER));
        assert_eq!(s.field("asked"), Some(Q1_ASKED));
        assert_eq!(s.field("in"), Some(BASE));
        for id in ["T2", "T3"] {
            let t = task(&v.plan, id);
            assert!(!t.needs_owner, "{t:?}");
            assert!(
                t.markers.iter().all(|m| !m.starts_with("blocked by")),
                "{t:?}"
            );
        }
        assert_eq!(
            task(&v.plan, "T2").depends_questions(),
            Vec::<String>::new()
        );

        let stored = store::read_artifact(dir.path(), SLUG, &v.stem).unwrap();
        assert_eq!(stored.frontmatter.revises.as_deref(), Some(BASE));
        assert_eq!(stored.frontmatter.version, Some(2));
        assert_eq!(stored.frontmatter.answered, ["Q1"]);
        assert!(stored.body.contains(&format!(
            "_v2 · answers on {BASE} · audit not re-run · llama3.2"
        )));
        let view = plan_view(dir.path(), SLUG, None).unwrap();
        assert_eq!(view.stem, v.stem);
        assert_eq!(view.lineage, [(BASE.to_string(), 1), (v.stem.clone(), 2)]);
        let old = plan_view(dir.path(), SLUG, Some(BASE)).unwrap();
        assert!(!old.is_head && old.superseded_by == [v.stem.clone()]);

        let last = store::split_turns(&store::read_conversation(dir.path(), SLUG).unwrap())
            .pop()
            .unwrap();
        assert!(
            last.starts_with("## assistant (skill: build-prompt)\n"),
            "{last}"
        );
        assert!(
            last.contains("answered Q1→S2 · unblocked T2, T3 · open: Q2"),
            "{last}"
        );
        assert!(store::is_capstone_turn(&last));
    }

    #[test]
    fn answer_keeps_untouched_ids_stable() {
        let dir = seeded();
        let before = plan_view(dir.path(), SLUG, None).unwrap().plan;
        let v = submit(dir.path(), BASE, &[("Q1", Q1_ANSWER)], 1).unwrap();
        let ids = |items: &[Item]| {
            items
                .iter()
                .map(|i| (i.id.clone(), i.text.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&v.plan.tasks), ids(&before.tasks));
        assert_eq!(ids(&v.plan.kills), ids(&before.kills));
        assert_eq!(v.plan.open[0].id, "Q2");
        assert_eq!(v.plan.open[0].text, before.open[1].text);
        assert_eq!(v.plan.settled[0].id, "S1");
    }

    #[test]
    fn answer_turn_holds_owner_words_not_question() {
        let dir = seeded();
        submit(dir.path(), BASE, &[("Q1", Q1_ANSWER)], 1).unwrap();
        let turns = store::split_turns(&store::read_conversation(dir.path(), SLUG).unwrap());
        let user = &turns[turns.len() - 2];
        assert_eq!(user, &format!("## user\nRe Q1 ({BASE}): {Q1_ANSWER}\n"));
        assert!(!user.contains("dwell on the live label"));
    }

    #[test]
    fn base_file_bytes_unchanged() {
        let dir = seeded();
        let path = dir
            .path()
            .join(SLUG)
            .join("artifacts")
            .join(format!("{BASE}.md"));
        let before = std::fs::read(&path).unwrap();
        submit(dir.path(), BASE, &[("Q1", Q1_ANSWER)], 1).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn short_long_or_copied_answer_writes_nothing() {
        let dir = seeded();
        let before = snapshot(dir.path());
        let long = "word ".repeat(500);
        for (answers, want) in [
            (vec![("Q1", "yes do")], "TooShort"),
            (vec![("Q1", long.as_str())], "TooLong"),
            (vec![("Q1", Q1_ASKED)], "NotOwnWords"),
            (
                vec![("Q2", "Binance spot, daily candles."), ("Q1", "no")],
                "TooShort",
            ),
            (vec![("Q9", "Binance spot, daily candles.")], "UnknownId"),
            (vec![("Q1", "   ")], "NothingToAnswer"),
            (
                vec![
                    ("Q1", Q1_ANSWER),
                    ("q1", "Dwell on the label, then measure fills."),
                ],
                "DuplicateId",
            ),
        ] {
            let err = submit(dir.path(), BASE, &answers, 1).unwrap_err();
            assert!(format!("{err:?}").starts_with(want), "{err:?}");
            assert_eq!(snapshot(dir.path()), before, "{want} wrote something");
        }
    }

    #[test]
    fn non_head_is_superseded() {
        let dir = seeded();
        let v2 = submit(dir.path(), BASE, &[("Q1", Q1_ANSWER)], 1).unwrap();
        let before = snapshot(dir.path());
        let err = submit(
            dir.path(),
            BASE,
            &[("Q2", "Binance spot, daily candles.")],
            2,
        )
        .unwrap_err();
        match err {
            WorkbenchError::Superseded { head } => assert_eq!(head, v2.stem),
            other => panic!("{other:?}"),
        }
        assert_eq!(snapshot(dir.path()), before);
        let v3 = submit(
            dir.path(),
            &v2.stem,
            &[("Q2", "Binance spot, daily candles.")],
            2,
        )
        .unwrap();
        assert_eq!((v3.version, v3.revises), (3, v2.stem));
        assert!(v3.still_open.is_empty());
    }

    #[test]
    fn identical_resubmit_returns_existing_version() {
        let dir = seeded();
        let first = submit(dir.path(), BASE, &[("Q1", Q1_ANSWER)], 1).unwrap();
        let before = snapshot(dir.path());
        let again = submit(dir.path(), BASE, &[("q1", &format!("  {Q1_ANSWER}\n"))], 2).unwrap();
        assert!(again.reused);
        assert_eq!(again.stem, first.stem);
        assert_eq!(snapshot(dir.path()), before);
    }

    #[test]
    fn retry_after_a_partial_write_does_not_duplicate_answer_turns() {
        let dir = seeded();
        // A first attempt wrote its answer turn, then failed before the artifact landed.
        let turn = store::format_turn("user", &format!("Re Q1 ({BASE}): {Q1_ANSWER}"));
        store::append_conversation(dir.path(), SLUG, &turn).unwrap();
        let v = submit(dir.path(), BASE, &[("Q1", Q1_ANSWER)], 1).unwrap();
        assert!(!v.reused);
        let conversation = store::read_conversation(dir.path(), SLUG).unwrap();
        assert_eq!(conversation.matches(&format!("Re Q1 ({BASE})")).count(), 1);
        assert_eq!(
            conversation
                .matches(&format!("{POINTER_PREFIX}{}]", v.stem))
                .count(),
            1
        );
    }

    #[test]
    fn retry_after_a_lost_pointer_lands_it_once() {
        let dir = seeded();
        let first = submit(dir.path(), BASE, &[("Q1", Q1_ANSWER)], 1).unwrap();
        // The version landed, but the pointer turn failed to append.
        let conversation = store::read_conversation(dir.path(), SLUG).unwrap();
        let mut turns = store::split_turns(&conversation);
        let pointer = turns.pop().unwrap();
        assert!(pointer.contains(&first.stem), "{pointer}");
        let path = dir.path().join(SLUG).join("conversation.md");
        std::fs::write(&path, turns.concat()).unwrap();

        let again = submit(dir.path(), BASE, &[("Q1", Q1_ANSWER)], 2).unwrap();
        assert!(again.reused);
        assert_eq!(again.stem, first.stem);
        let conversation = store::read_conversation(dir.path(), SLUG).unwrap();
        let link = format!("{POINTER_PREFIX}{}]", first.stem);
        assert_eq!(conversation.matches(&link).count(), 1, "{conversation}");
        assert_eq!(conversation.matches(&format!("Re Q1 ({BASE})")).count(), 1);
        // A third identical submission writes nothing more.
        let before = snapshot(dir.path());
        assert!(
            submit(dir.path(), BASE, &[("Q1", Q1_ANSWER)], 3)
                .unwrap()
                .reused
        );
        assert_eq!(snapshot(dir.path()), before);
    }

    #[test]
    fn structural_block_not_answerable() {
        let dir = seeded();
        let view = plan_view(dir.path(), SLUG, None).unwrap();
        let t2 = view.blocked.iter().find(|b| b.id == "T2").unwrap();
        assert!(!t2.answerable);
        assert_eq!(t2.blocked_by, ["Q1"]);
        let before = snapshot(dir.path());
        for id in ["T2", "T3"] {
            let err = submit(
                dir.path(),
                BASE,
                &[(id, "Just build it with the defaults.")],
                1,
            )
            .unwrap_err();
            assert!(
                matches!(err, WorkbenchError::NotAnswerable(ref t) if t == id),
                "{err:?}"
            );
        }
        let err = submit(
            dir.path(),
            BASE,
            &[("T1", "Just build it with the defaults.")],
            1,
        )
        .unwrap_err();
        assert!(
            matches!(err, WorkbenchError::NotAnswerable(_)),
            "an unheld task: {err:?}"
        );
        assert_eq!(snapshot(dir.path()), before);
    }

    #[test]
    fn model_question_block_answerable_as_t() {
        let dir = seeded();
        let view = plan_view(dir.path(), SLUG, None).unwrap();
        let t4 = view.blocked.iter().find(|b| b.id == "T4").unwrap();
        assert!(t4.answerable, "{t4:?}");
        let v = submit(
            dir.path(),
            BASE,
            &[("T4", "Use the paper account at the broker.")],
            1,
        )
        .unwrap();
        let t = task(&v.plan, "T4");
        assert!(!t.needs_owner, "{t:?}");
        assert_eq!(
            t.field("unblocked"),
            Some("Use the paper account at the broker.")
        );
        assert_eq!(t.field("owner"), None);
        let s = v
            .plan
            .settled
            .iter()
            .find(|s| s.field("unblocks") == Some("T4"))
            .unwrap();
        assert_eq!(
            s.provenance,
            Some(crate::concepts::build_plan::plan::Provenance::Owner)
        );
        assert!(v.unblocked.contains(&"T4".to_string()));
        assert_eq!(
            hedge_warning("Either the paper account or live, pick one"),
            Some("pick one")
        );
        assert_eq!(hedge_warning("Use the paper account."), None);
    }

    const FIGURE_ANSWER: &str = "Freeze at entry; measured on claude 2.1.285, a 24h TTL holds.";

    #[test]
    fn answer_with_a_figure_stays_settled() {
        let dir = seeded();
        let v = submit(dir.path(), BASE, &[("Q1", FIGURE_ANSWER)], 1).unwrap();
        let s = answer_holder(&v.plan, "Q1").unwrap();
        assert!(s.id.starts_with('S'), "{s:?} / verify {:?}", v.plan.verify);
        assert!(s.markers.is_empty(), "{s:?}");
    }

    #[test]
    fn answer_restores_owner_answer_moved_by_g6() {
        let dir = seeded();
        let v2 = submit(dir.path(), BASE, &[("Q1", FIGURE_ANSWER)], 1).unwrap();
        let sid = answer_holder(&v2.plan, "Q1").unwrap().id.clone();
        demote_to_verify(
            dir.path(),
            &v2.stem,
            &sid,
            "figure not in the discussion: 24",
        );
        let demoted = plan_view(dir.path(), SLUG, None).unwrap().plan;
        assert_eq!(answer_holder(&demoted, "Q1").unwrap().id, "P1");

        let v3 = submit(
            dir.path(),
            &v2.stem,
            &[("Q2", "Binance spot, daily candles.")],
            2,
        )
        .unwrap();
        let s = answer_holder(&v3.plan, "Q1").unwrap();
        assert!(s.id.starts_with('S'), "{s:?} / verify {:?}", v3.plan.verify);
        assert!(s.markers.is_empty(), "{s:?}");
        assert!(v3.plan.verify.is_empty(), "{:?}", v3.plan.verify);
        assert_eq!(
            v3.plan
                .settled
                .iter()
                .filter(|i| i.field("answers") == Some("Q1"))
                .count(),
            1
        );
    }

    #[test]
    fn answer_with_a_freshness_cue_stays_settled_across_versions() {
        let dir = seeded();
        let fresh = "Freeze at entry, using the latest snapshot as of the run.";
        let v2 = submit(dir.path(), BASE, &[("Q1", fresh)], 1).unwrap();
        let s2 = answer_holder(&v2.plan, "Q1").unwrap().clone();
        assert!(
            s2.id.starts_with('S'),
            "{s2:?} / verify {:?}",
            v2.plan.verify
        );
        assert!(
            s2.markers.iter().any(|m| m.starts_with("freshness")),
            "{s2:?}"
        );
        let v3 = submit(
            dir.path(),
            &v2.stem,
            &[("Q2", "Binance spot, daily candles.")],
            2,
        )
        .unwrap();
        let s3 = answer_holder(&v3.plan, "Q1").unwrap();
        assert_eq!(s3.id, s2.id, "the answer keeps its id: {:?}", v3.plan);
        assert!(v3.plan.verify.is_empty(), "{:?}", v3.plan.verify);
    }

    const GATED_PLAN: &str = "## Goal
Ship the zone snapshot tool.

## Settled
- S1: Disproof comes before any code.
  quote: \"the cheapest disproof before any Rust exists\"

## Open questions
- Q1: Freeze the zone snapshot at entry, or dwell on the live label?

## Plan
- [ ] T1: Write the spec
  touches: `SPEC.md`
  accept: `test -s SPEC.md` → exit 0";

    #[test]
    fn answering_g10_question_does_not_reopen_it() {
        let dir = seeded();
        store::append_turn(
            dir.path(),
            SLUG,
            "user",
            "Ship the freezer only if the backtest holds up on last year.",
        )
        .unwrap();
        let probe = SourceProbe::default();
        let first = finish(PlanInputs {
            vault_dir: dir.path(),
            idea_slug: SLUG,
            answer: GATED_PLAN,
            turn_role: "assistant (skill: build-prompt)",
            lens: "build-prompt",
            recipe: None,
            model: "llama3.2".into(),
            audit: None,
            probe: &probe,
            now: at(1),
        })
        .unwrap();
        let base = plan_view(dir.path(), SLUG, None).unwrap();
        assert_eq!(base.stem, first.artifact_slug);
        let gate = base
            .plan
            .open
            .iter()
            .find(|q| q.text.contains("gate language without a kill row"))
            .expect("G10 asks about the owner's gate language")
            .id
            .clone();

        let v = submit(
            dir.path(),
            &base.stem,
            &[(&gate, "No kill row: the freezer is cheap to throw away.")],
            2,
        )
        .unwrap();
        assert!(
            v.plan
                .open
                .iter()
                .all(|q| !q.text.contains("gate language without a kill row")),
            "{:?}",
            v.plan.open
        );
        let v3 = submit(dir.path(), &v.stem, &[("Q1", Q1_ANSWER)], 3).unwrap();
        assert!(
            v3.plan
                .open
                .iter()
                .all(|q| !q.text.contains("gate language without a kill row")),
            "the G10 answer holds on later versions: {:?}",
            v3.plan.open
        );
    }
}
