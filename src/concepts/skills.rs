//! Skills: named, reusable ideation moves — parameterized prompt templates the AI can apply to
//! an idea on demand (docs/06-concepts/skills.md D18).

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use chrono::Utc;
use tokio::sync::Semaphore;

use crate::ai::budget::{
    assemble_context, related_allowance, AssembledContext, ContextBudget, ContextInput,
};
use crate::ai::call::CallMeta;
use crate::ai::contract::{self, ContractOutcome};
use crate::ai::ollama::ChatMessage;
use crate::ai::provenance::{self, digest12};
use crate::ai::verdict::ParserKind;
use crate::ai::LlmBackend;
use crate::concepts::agents::AgentRole;
use crate::concepts::build_plan;
use crate::concepts::build_plan::finish::{finish_as, Finished, PlanInputs, PlanMode};
use crate::concepts::build_plan::gates::AuditView;
use crate::concepts::ConceptError;
use crate::domain::frontmatter::parse_skill;
use crate::domain::{slug, OutputContract, Recipe, SkillRole, SkillStage};
use crate::vault::store;

/// Largest skill file the loader accepts. A skill is a prompt template; anything bigger is a
/// mistake (a pasted transcript, a binary) and would eat the context budget of every run.
pub const MAX_SKILL_FILE_BYTES: usize = 32 * 1024;

/// The built-in skill files, compiled into the binary (docs/adr/0022). Order is registration
/// order, which is the order the move chips render in.
const BUILTIN: &[(&str, &str)] = &[
    ("steelman", include_str!("skills/steelman.md")),
    ("premortem", include_str!("skills/premortem.md")),
    (
        "cheapest-disproof",
        include_str!("skills/cheapest-disproof.md"),
    ),
    ("devils-advocate", include_str!("skills/devils-advocate.md")),
    ("pr-faq", include_str!("skills/pr-faq.md")),
    (
        "dialectical-inquiry",
        include_str!("skills/dialectical-inquiry.md"),
    ),
    ("constraints", include_str!("skills/constraints.md")),
    (
        "second-order-effects",
        include_str!("skills/second-order-effects.md"),
    ),
    ("market-size", include_str!("skills/market-size.md")),
    ("triz", include_str!("skills/triz.md")),
    ("converge", include_str!("skills/converge.md")),
    ("build-prompt", include_str!("skills/build-prompt.md")),
    (
        "extract-key-decisions",
        include_str!("skills/extract-key-decisions.md"),
    ),
    (
        "extract-durable-facts",
        include_str!("skills/extract-durable-facts.md"),
    ),
    (
        "extract-open-questions",
        include_str!("skills/extract-open-questions.md"),
    ),
    (
        "extract-risks-assumptions",
        include_str!("skills/extract-risks-assumptions.md"),
    ),
    (
        "extract-next-actions",
        include_str!("skills/extract-next-actions.md"),
    ),
    ("ground-read", include_str!("skills/ground-read.md")),
    ("panel-score", include_str!("skills/panel-score.md")),
];

/// Skills only the workflow engine runs: Ground's readers and Panel's scorers (docs/adr/0034).
/// Their output feeds code that parses and checks it, so an owner never runs one as a move, a
/// swarm angle or a workflow step. An owner file may still override the prompt.
pub const INTERNAL_SKILLS: [&str; 2] = ["ground-read", "panel-score"];

/// Where a registered skill's definition came from — shown on the skill book.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    BuiltIn,
    /// An owner file in `vault/.skills/` that replaced the built-in of the same name.
    VaultOverride,
    /// An owner file in `vault/.skills/` adding a new skill.
    Vault,
}

impl SkillSource {
    pub fn as_str(self) -> &'static str {
        match self {
            SkillSource::BuiltIn => "built-in",
            SkillSource::VaultOverride => "vault override",
            SkillSource::Vault => "vault",
        }
    }
}

/// A skill is data, not code: a markdown file whose frontmatter names the move and whose body is
/// the prompt template, with a `{context}` slot filled by `ai::budget` at invocation time.
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub stage: SkillStage,
    pub role: SkillRole,
    pub contract: OutputContract,
    pub use_when: String,
    pub avoid_when: String,
    pub hidden: bool,
    /// One of [`INTERNAL_SKILLS`]: engine-only, never offered or accepted as an owner step.
    pub internal: bool,
    pub prompt: String,
    pub source: SkillSource,
    /// [`digest12`] of the skill file's raw bytes, before `{context}` is filled — what an
    /// artifact's recipe records and the skill book shows (ADR-0040).
    pub digest: String,
}

impl Skill {
    /// The recipe of an artifact this skill wrote: its name, digest and source, this build, and
    /// the lens's contract note when the call's recorded `outcome` is off contract (ADR-0040).
    pub fn recipe(&self, outcome: &ContractOutcome) -> Recipe {
        Recipe {
            skill: Some(self.name.clone()),
            skill_digest: Some(self.digest.clone()),
            skill_source: Some(self.source.as_str().to_string()),
            contract: outcome.note(&self.name).into_iter().collect(),
            ..provenance::recipe(&[])
        }
    }
}

/// A skill file that failed to load. The registry keeps running on everything else; the skill
/// book lists these so the owner can fix the file.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillIssue {
    pub file: String,
    pub message: String,
}

/// Parse and validate one skill document. The name must be a canonical slug: it is spliced into
/// the `## assistant (skill: <name>)` turn heading and the route path, so anything outside
/// `[a-z0-9-]` could forge a transcript turn.
fn parse_skill_doc(raw: &str, source: SkillSource) -> Result<Skill, String> {
    if raw.len() > MAX_SKILL_FILE_BYTES {
        return Err(format!(
            "file is {} bytes; the limit is {MAX_SKILL_FILE_BYTES}",
            raw.len()
        ));
    }
    let (fm, prompt) = parse_skill(raw).map_err(|e| e.to_string())?;
    if !slug::is_valid(&fm.name) {
        return Err(format!(
            "name {:?} must use only lowercase letters, digits and '-'",
            fm.name
        ));
    }
    if !prompt.contains("{context}") {
        return Err("prompt has no {context} slot for the idea".to_string());
    }
    let internal = INTERNAL_SKILLS.contains(&fm.name.as_str());
    Ok(Skill {
        name: fm.name,
        description: fm.description,
        stage: fm.stage,
        role: fm.role,
        contract: fm.contract,
        use_when: fm.use_when,
        avoid_when: fm.avoid_when,
        // An internal skill is never a move, whatever an owner override's frontmatter says:
        // guard_skill refuses it, so surfacing it as a chip would only offer a 404.
        hidden: fm.hidden || internal,
        internal,
        prompt,
        source,
        digest: digest12(raw.as_bytes()),
    })
}

/// The set of skills available at runtime: the built-ins, then the owner's `vault/.skills/`.
#[derive(Debug, Clone)]
pub struct SkillRegistry {
    skills: Vec<Skill>,
}

impl SkillRegistry {
    /// The built-in skills that ship with the binary (docs/06-concepts/skills.md). Never panics:
    /// a built-in that fails to parse is logged and skipped (a unit test keeps the set clean).
    pub fn builtin() -> Self {
        let (registry, issues) = Self::builtin_with_issues();
        for issue in issues {
            tracing::error!(file = %issue.file, error = %issue.message, "built-in skill invalid");
        }
        registry
    }

    fn builtin_with_issues() -> (Self, Vec<SkillIssue>) {
        let mut skills = Vec::with_capacity(BUILTIN.len());
        let mut issues = Vec::new();
        for (stem, raw) in BUILTIN {
            match parse_skill_doc(raw, SkillSource::BuiltIn) {
                Ok(skill) if skill.name == *stem => skills.push(skill),
                Ok(skill) => issues.push(SkillIssue {
                    file: format!("{stem}.md"),
                    message: format!("name {:?} does not match the file name", skill.name),
                }),
                Err(message) => issues.push(SkillIssue {
                    file: format!("{stem}.md"),
                    message,
                }),
            }
        }
        (Self { skills }, issues)
    }

    /// The built-ins plus every valid `*.md` in `dir` (the owner's `vault/.skills/`), in file-name
    /// order. A file whose name matches a built-in replaces it in place; a new name is appended.
    /// A missing directory is no skills, not an error. Invalid files come back as issues and the
    /// built-in (if any) stays active — a broken owner file never takes a move away or stops boot.
    pub fn load(dir: &Path) -> (Self, Vec<SkillIssue>) {
        let (mut registry, mut issues) = Self::builtin_with_issues();
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (registry, issues),
            Err(e) => {
                issues.push(SkillIssue {
                    file: dir.display().to_string(),
                    message: format!("skills directory unreadable: {e}"),
                });
                return (registry, issues);
            }
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "md"))
            .collect();
        paths.sort();
        for path in paths {
            let file = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let stem = path
                .file_stem()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let loaded = std::fs::read_to_string(&path)
                .map_err(|e| format!("unreadable: {e}"))
                .and_then(|raw| parse_skill_doc(&raw, SkillSource::Vault))
                .and_then(|skill| {
                    if skill.name == stem {
                        Ok(skill)
                    } else {
                        Err(format!(
                            "name {:?} does not match the file name {file:?}",
                            skill.name
                        ))
                    }
                });
            match loaded {
                Ok(mut skill) => match registry.skills.iter_mut().find(|s| s.name == skill.name) {
                    Some(existing) => {
                        skill.source = SkillSource::VaultOverride;
                        *existing = skill;
                    }
                    None => registry.skills.push(skill),
                },
                Err(message) => issues.push(SkillIssue { file, message }),
            }
        }
        (registry, issues)
    }

    /// Look up a skill by exact name.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// All registered skills, in registration order.
    pub fn list(&self) -> &[Skill] {
        &self.skills
    }

    /// Skills offered to the owner (every non-`hidden` one), in registration order.
    pub fn visible(&self) -> impl Iterator<Item = &Skill> {
        self.skills.iter().filter(|s| !s.hidden)
    }

    /// Names of the skills surfaced as interactive moves. The `extract-*` lenses are
    /// knowledge-extraction angles driven by `concepts::knowledge` (docs/adr/0015), marked
    /// `hidden` — registered (so `run_agent` can resolve them) but excluded here.
    pub fn move_names(&self) -> Vec<String> {
        self.visible().map(|s| s.name.clone()).collect()
    }
}

/// The live skill registry held in `AppState`: the owner edits `vault/.skills/*.md` and presses
/// reload on the skill book — no restart, no file watcher. A handler takes one [`snapshot`]
/// and moves it into its job, so a reload never changes a run already in flight.
///
/// [`snapshot`]: LiveSkills::snapshot
pub struct LiveSkills {
    dir: PathBuf,
    current: RwLock<Arc<SkillRegistry>>,
    issues: RwLock<Vec<SkillIssue>>,
}

impl LiveSkills {
    /// Load the built-ins plus `dir` (usually `<vault>/.skills`), logging any issues.
    pub fn load(dir: PathBuf) -> Self {
        let (registry, issues) = SkillRegistry::load(&dir);
        for issue in &issues {
            tracing::warn!(file = %issue.file, error = %issue.message, "skill file skipped");
        }
        Self {
            dir,
            current: RwLock::new(Arc::new(registry)),
            issues: RwLock::new(issues),
        }
    }

    /// The directory owner skills are read from.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The registry as of now.
    pub fn snapshot(&self) -> Arc<SkillRegistry> {
        match self.current.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// The files that failed to load on the last (re)load.
    pub fn issues(&self) -> Vec<SkillIssue> {
        match self.issues.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Re-read the skills directory and swap the registry in.
    pub fn reload(&self) {
        let (registry, issues) = SkillRegistry::load(&self.dir);
        for issue in &issues {
            tracing::warn!(file = %issue.file, error = %issue.message, "skill file skipped");
        }
        match self.current.write() {
            Ok(mut guard) => *guard = Arc::new(registry),
            Err(poisoned) => *poisoned.into_inner() = Arc::new(registry),
        }
        match self.issues.write() {
            Ok(mut guard) => *guard = issues,
            Err(poisoned) => *poisoned.into_inner() = issues,
        }
    }
}

/// Supplies the cross-idea "related ideas" block (`memory::related`) for one prompt: given the
/// most bytes it may take, returns the block or `""`. The web layer builds it over the index so
/// `concepts` stays DB-free; callers without an index pass `&|_| String::new()`.
pub type RelatedProvider<'a> = &'a (dyn Fn(usize) -> String + Sync);

/// How [`invoke`] fills a skill's `{context}` slot: the idea's own context assembled under
/// `budget`, prefixed with the block `related` supplies within whatever of `budget` the own
/// context leaves.
#[derive(Clone, Copy)]
pub struct ContextSlot<'a> {
    /// Budget for the idea's own context; the related block only takes what it leaves.
    pub budget: ContextBudget,
    /// Source of the related-ideas block.
    pub related: RelatedProvider<'a>,
}

/// Gather the D18 skill inputs (`idea_body`, `memory`, `recent_conversation`) via `vault::store`
/// and assemble them under `budget` with `ai::budget` directly — per D4, `concepts` composes
/// `vault` + `ai` itself rather than reaching through `memory` (whose `load_context` is the
/// D13 reopen path; the gathering logic is intentionally parallel, not shared).
/// `pub(crate)`: `swarm` hydrates the same budgeted block once per fan-out (D14/D21).
pub(crate) fn hydrate_context(
    vault_dir: &Path,
    idea_slug: &str,
    budget: ContextBudget,
) -> Result<AssembledContext, ConceptError> {
    let idea = store::read_idea(vault_dir, idea_slug)?;
    let conversation = store::read_conversation(vault_dir, idea_slug)?;

    let index = store::read_memory_index(vault_dir, idea_slug)?;
    let mut memory: Vec<String> = index
        .entries
        .iter()
        .map(|e| format!("[[{}]] — {}", e.slug, e.summary))
        .collect();
    let mut facts = store::read_memory_facts(vault_dir, idea_slug)?;
    facts.sort_by_key(|b| std::cmp::Reverse(b.frontmatter.created));
    memory.extend(
        facts
            .iter()
            .map(|f| format!("{}: {}", f.frontmatter.title, f.body.trim())),
    );

    let turns = store::split_turns(&conversation);
    let compacted = store::read_compacted(vault_dir, idea_slug)?;
    let win = crate::memory::compact::effective_window(&turns, compacted.as_ref());
    let (summary, tail): (Option<&str>, &[String]) = match win.applied {
        Some(k) => (compacted.as_ref().map(|c| c.summary.as_str()), &turns[k..]),
        None => (None, &turns[..]),
    };
    Ok(assemble_context(
        budget,
        ContextInput {
            idea_body: &idea.body,
            memory: &memory,
            summary,
            turns: tail,
        },
    ))
}

/// The most bytes the prior-plan block adds to a capstone prompt (docs/adr/0032).
pub(crate) const PRIOR_PLAN_BYTES: usize = 1500;

/// For a capstone (build-plan) prompt only: the lineage head's open question ids and texts and
/// every owner answer on its chain, so a re-plan keeps the ids and never re-asks an answered
/// question (docs/adr/0032). Prompt context, never evidence — the gates ground only in the
/// idea and the discussion, where each answer already is an owner turn. Empty when the idea has
/// no plan or it cannot be read: a missing hint must never fail the planner. Capped at
/// [`PRIOR_PLAN_BYTES`].
pub(crate) fn prior_plan_block(vault_dir: &Path, idea_slug: &str) -> String {
    let Ok(plans) = build_plan::lineage::list_plans(vault_dir, idea_slug) else {
        return String::new();
    };
    let Some(head) = build_plan::lineage::head(&plans) else {
        return String::new();
    };
    let open = store::read_artifact(vault_dir, idea_slug, &head.stem)
        .ok()
        .and_then(|a| build_plan::plan::parse_artifact(&a.body).ok())
        .map(|p| p.open)
        .unwrap_or_default();
    let answered = build_plan::lineage::answered_in_lineage(vault_dir, idea_slug, &head.stem)
        .unwrap_or_default();
    let mut out = format!(
        "## Prior plan (ids only — not evidence)\n{} · v{} — keep these ids; do not re-ask answered questions.\n",
        head.stem, head.version
    );
    for q in &open {
        out.push_str(&format!("- open {}: {}\n", q.id, q.text));
    }
    for a in &answered {
        out.push_str(&format!("- answered {} → {}\n", a.qid, a.answer));
    }
    out = crate::concepts::audit::clip(&out, PRIOR_PLAN_BYTES);
    out.push_str("\n\n");
    out
}

/// How usable a build-plan answer is, judged by the parser that later reads it, on three levels:
/// `None` when it does not parse, `(0, settled)` when it parses with no task, else
/// `(tasks, settled)`. Higher is more usable; a plan is usable only with at least one task.
fn plan_score(answer: &str) -> Option<(usize, usize)> {
    let plan = build_plan::plan::parse(answer).ok()?;
    Some((plan.tasks.len(), plan.settled.len()))
}

/// Whether a [`plan_score`] holds at least one task.
fn plan_usable(score: Option<(usize, usize)>) -> bool {
    score.is_some_and(|(tasks, _)| tasks >= 1)
}

/// One model call held to an output contract (docs/adr/0023): call, validate/repair, and on a
/// violation ask exactly ONCE more — under the same permit — with the violation read back. If the
/// retry still misses (or fails), the best answer is kept: a wrong-shaped answer beats none. For
/// [`OutputContract::BuildPlan`] a validated answer with no parsed task also counts as a
/// violation; the first answer is then kept only on a strictly higher (tasks, settled) score and a
/// tie goes to the retry. Used by single interactive skill calls and by a workflow's chained step;
/// fan-out agents never retry (`agents::run_agent` repairs only). Callers must NOT hold a permit.
///
/// Truncation is a violation too (docs/adr/0037): an answer cut off at the output limit gets the
/// one retry; a prompt that filled the window gets none, because the same window would drop the
/// same head again. The [`ContractOutcome`] comes back with the answer and is recorded in the run
/// journal against the call whose text was kept.
pub(crate) async fn ask_on_contract(
    llm: &LlmBackend,
    ai_semaphore: &Semaphore,
    prompt: String,
    contract: OutputContract,
    label: &str,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<(String, ContractOutcome), ConceptError> {
    let ask = |content: String| {
        llm.chat_meta(vec![ChatMessage {
            role: "user".to_string(),
            content,
        }])
    };
    let _permit = ai_semaphore
        .acquire()
        .await
        .map_err(|_| ConceptError::SemaphoreClosed)?;
    let (first, first_meta) = ask(prompt.clone()).await?;
    llm.record_contract_verdict(&first_meta, contract, &first);
    let settle = |kept: String, meta: &CallMeta, outcome: ContractOutcome| {
        llm.record_contract(meta, contract, &outcome);
        Ok((kept, outcome))
    };
    let plan_contract = contract == OutputContract::BuildPlan;
    let first_score = plan_contract.then(|| plan_score(&first));
    let validated = contract::validate(contract, &first);
    // The first answer as it would be kept: repaired when it validated. A truncated first answer
    // can be shape-valid yet still earn the retry, and then it outranks a retry that is not
    // (except for a build plan, which the plan score decides).
    let first_kept = validated
        .as_ref()
        .map_or_else(|_| first.trim().to_string(), Clone::clone);
    let first_shape_wins = validated.is_ok() && !plan_contract;
    if first_meta.input_truncated() {
        tracing::warn!(
            label,
            "the prompt filled the context window; answer kept without a retry"
        );
        let outcome = ContractOutcome::OffContract(
            contract::Violation::Truncated {
                output: first_meta.output_truncated(),
                input: true,
            }
            .to_string(),
        );
        return settle(first_kept, &first_meta, outcome);
    }
    let violation = match validated {
        _ if first_meta.output_truncated() => contract::Violation::Truncated {
            output: true,
            input: false,
        },
        Ok(_) if first_score.is_some_and(|score| !plan_usable(score)) => {
            contract::Violation::NoUsablePlan
        }
        Ok(repaired) => {
            let outcome = ContractOutcome::of_valid(&first, &repaired);
            return settle(repaired, &first_meta, outcome);
        }
        Err(violation) => violation,
    };
    progress(&format!("{label} · reshaping the answer"));
    tracing::info!(label, %violation, "contract violated; retrying once");
    let retried = ask(format!("{prompt}{}", contract::retry_note(&violation))).await;
    if let Ok((second, meta)) = &retried {
        llm.record_contract_verdict(meta, contract, second);
    }
    let first_wins = |second: &str| first_score.is_some_and(|before| before > plan_score(second));
    let off = ContractOutcome::OffContract(violation.to_string());
    if let Ok((second, meta)) = &retried {
        let checked = if meta.output_truncated() {
            Err(contract::Violation::Truncated {
                output: true,
                input: false,
            })
        } else {
            contract::validate(contract, second)
        };
        match checked {
            Ok(_) if first_wins(second) => return settle(first_kept, &first_meta, off),
            Ok(repaired) => return settle(repaired, meta, ContractOutcome::Retried),
            Err(again) => {
                tracing::warn!(label, %again, "retry did not produce an on-contract answer; keeping the best one")
            }
        }
    } else {
        tracing::warn!(label, "retry failed; keeping the first answer");
    }
    match retried {
        Ok((second, meta))
            if !second.trim().is_empty() && !first_wins(&second) && !first_shape_wins =>
        {
            settle(second.trim().to_string(), &meta, off)
        }
        _ => settle(first_kept, &first_meta, off),
    }
    // permit released on return — before any vault write, which needs no AI slot
}

/// Gate a planner's answer and persist it as a build-plan artifact plus a pointer turn
/// ([`finish_as`]) on the blocking pool. Called after the model call returned, so no permit is
/// held; the probe and model label come from `llm`, the turn- and role-scoped backend that
/// produced `answer`. `lens` names the skill (quick) or workflow (ready-to-build) that ran, and
/// with `mode` decides the pointer turn's role; `recipe` is stamped on the plan (ADR-0040).
pub(crate) async fn persist_plan(
    llm: &LlmBackend,
    vault_dir: &Path,
    idea_slug: &str,
    answer: String,
    (lens, recipe): (&str, Recipe),
    audit: Option<AuditView>,
    mode: PlanMode<'_>,
) -> Result<Finished, ConceptError> {
    // The mode borrows; the blocking task needs it owned.
    enum Owned {
        Quick,
        ReadyToBuild(Option<String>),
        Answered(String),
    }
    let vault_dir = vault_dir.to_path_buf();
    let idea_slug = idea_slug.to_string();
    let (turn_role, owned) = match mode {
        PlanMode::Quick => (format!("assistant (skill: {lens})"), Owned::Quick),
        PlanMode::ReadyToBuild { skipped } => (
            format!("assistant (workflow: {lens})"),
            Owned::ReadyToBuild(skipped.map(str::to_string)),
        ),
        PlanMode::Answered { base } => (
            format!("assistant (skill: {lens})"),
            Owned::Answered(base.to_string()),
        ),
    };
    let lens = lens.to_string();
    let model = llm.model();
    let probe = llm.source_probe();
    let joined = tokio::task::spawn_blocking(move || {
        let mode = match &owned {
            Owned::Quick => PlanMode::Quick,
            Owned::ReadyToBuild(skipped) => PlanMode::ReadyToBuild {
                skipped: skipped.as_deref(),
            },
            Owned::Answered(base) => PlanMode::Answered { base },
        };
        finish_as(
            PlanInputs {
                vault_dir: &vault_dir,
                idea_slug: &idea_slug,
                answer: &answer,
                turn_role: &turn_role,
                lens: &lens,
                recipe: Some(recipe),
                model,
                audit: audit.as_ref(),
                probe: &probe,
                now: Utc::now(),
            },
            mode,
        )
    })
    .await;
    match joined {
        Ok(result) => {
            if let Ok(done) = &result {
                // The gates judged the kept planner answer; the journal pins that verdict to the
                // call the BuildPlan contract settled on (docs/adr/0038). A workbench version made
                // no call, so nothing is recorded for it.
                llm.record_verdict_on_contract(
                    OutputContract::BuildPlan,
                    ParserKind::PlanGates,
                    done.verdict.clone(),
                    Some(done.evidence.clone()),
                );
            }
            result
        }
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => Err(ConceptError::Vault(crate::vault::VaultError::Io(
            std::io::Error::other(format!("build-plan task did not finish: {e}")),
        ))),
    }
}

/// Hydrate a skill's `{context}` slot and run it against the AI, appending the result as an
/// assistant turn (docs/06-concepts/skills.md §D18).
///
/// The `{context}` slot is filled per [`ContextSlot`]: `ai::budget` (idea body + memory + recent
/// conversation, under its budget — never the raw full history), prefixed with the related-ideas
/// block. The call is gated by the process-wide `ai_semaphore` (ADR-0006: chat, skills, and swarm
/// share one bound). Callers must NOT already hold a permit from that semaphore when calling this
/// — `invoke` acquires its own, and a held permit plus a small configured bound would deadlock.
///
/// The answer is held to the skill's output contract with at most one retry
/// ([`ask_on_contract`]). Stateless: the output is appended as an assistant turn only after the
/// calls complete (nothing partial ever reaches `conversation.md`); idea state is not changed.
/// A [`OutputContract::BuildPlan`] skill instead lands its gated plan as an artifact and returns
/// the pointer turn it appended; an answer with neither a goal nor a task is
/// [`ConceptError::PlanUnusable`] and persists nothing.
pub async fn invoke(
    ollama: &LlmBackend,
    ai_semaphore: &Semaphore,
    vault_dir: &Path,
    idea_slug: &str,
    skill: &Skill,
    slot: ContextSlot<'_>,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<String, ConceptError> {
    progress(&format!("running {}", skill.name));
    let context = hydrate_context(vault_dir, idea_slug, slot.budget)?;
    let prior = if skill.contract == OutputContract::BuildPlan {
        prior_plan_block(vault_dir, idea_slug)
    } else {
        String::new()
    };
    let block = (slot.related)(related_allowance(
        slot.budget,
        context.text.len() + prior.len(),
    ));
    let prompt = skill
        .prompt
        .replace("{context}", &format!("{block}{prior}{}", context.text));
    let llm = ollama.for_role(AgentRole::from(skill.role).as_str());
    // The outcome is already in the run journal (docs/adr/0037); a single skill turn shows no
    // badge of its own, but a build plan's recipe carries it (ADR-0040).
    let (output, outcome) = ask_on_contract(
        &llm,
        ai_semaphore,
        prompt,
        skill.contract,
        &skill.name,
        progress,
    )
    .await?;

    if skill.contract == OutputContract::BuildPlan {
        let recipe = skill.recipe(&outcome);
        let finished = persist_plan(
            &llm,
            vault_dir,
            idea_slug,
            output,
            (&skill.name, recipe),
            None,
            PlanMode::Quick,
        )
        .await?;
        return Ok(finished.pointer);
    }
    if output.is_empty() {
        // A "successful" call with nothing to say is usually a model misfire — surface it
        // rather than silently appending nothing (D24: surface, not swallow).
        tracing::warn!(skill = %skill.name, idea_slug, "skill invocation returned empty output");
    } else {
        // append_turn owns the heading grammar and escapes any embedded "## " lines the model
        // may emit, so its output can never forge a turn boundary.
        store::append_turn(
            vault_dir,
            idea_slug,
            &format!("assistant (skill: {})", skill.name),
            &output,
        )?;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_of_raw_file_not_filled_prompt() {
        let raw = include_str!("skills/premortem.md");
        let registry = SkillRegistry::builtin();
        let skill = registry.get("premortem").unwrap();
        assert_eq!(skill.digest, digest12(raw.as_bytes()));
        assert_ne!(skill.digest, digest12(skill.prompt.as_bytes()));
        let filled = skill.prompt.replace("{context}", "an idea");
        assert_ne!(skill.digest, digest12(filled.as_bytes()));

        // One byte more in an override moves the digest, and the recipe says where it came from.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("premortem.md"), format!("{raw}\n")).unwrap();
        let (owned, issues) = SkillRegistry::load(tmp.path());
        assert!(issues.is_empty(), "{issues:?}");
        let over = owned.get("premortem").unwrap();
        assert_ne!(over.digest, skill.digest);
        let recipe = over.recipe(&ContractOutcome::Repaired);
        assert_eq!(recipe.skill_digest.as_deref(), Some(over.digest.as_str()));
        assert_eq!(recipe.skill_source.as_deref(), Some("vault override"));
        assert!(
            recipe.contract.is_empty(),
            "a repaired answer met its contract"
        );
        assert_eq!(
            over.recipe(&ContractOutcome::OffContract(
                contract::Violation::NoNumberedList.to_string()
            ))
            .contract,
            [format!(
                "premortem: off-contract: {}",
                contract::Violation::NoNumberedList
            )]
        );
        // A truncated answer is often shape-valid; the recorded outcome still flags it.
        let truncated = contract::Violation::Truncated {
            output: true,
            input: false,
        }
        .to_string();
        assert_eq!(
            over.recipe(&ContractOutcome::OffContract(truncated.clone()))
                .contract,
            [format!("premortem: off-contract: {truncated}")]
        );
    }

    #[test]
    fn builtin_registry_contains_premortem_and_get_finds_it() {
        let registry = SkillRegistry::builtin();
        assert!(registry.list().iter().any(|s| s.name == "premortem"));
        let found = registry
            .get("premortem")
            .expect("premortem should be registered");
        assert_eq!(found.name, "premortem");
    }

    #[test]
    fn move_names_excludes_extraction_lenses_but_they_stay_resolvable() {
        let registry = SkillRegistry::builtin();
        let moves = registry.move_names();
        assert!(moves.iter().any(|n| n == "premortem"));
        assert!(
            !moves.iter().any(|n| n.starts_with("extract-")),
            "extraction lenses must not appear as move chips: {moves:?}"
        );
        // Still registered — the knowledge orchestrator resolves them like any skill.
        for lens in crate::concepts::knowledge::LENSES {
            assert!(registry.get(lens).is_some(), "unregistered lens: {lens}");
        }
    }

    fn write(dir: &Path, file: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(file), body).unwrap();
    }

    fn skill_doc(name: &str, stage: &str, prompt: &str) -> String {
        format!("---\nname: {name}\ndescription: d\nstage: {stage}\n---\n\n{prompt}\n")
    }

    #[test]
    fn every_builtin_parses_clean_with_unique_slug_names_and_a_context_slot() {
        let (registry, issues) = SkillRegistry::builtin_with_issues();
        assert!(issues.is_empty(), "built-in skill issues: {issues:?}");
        assert_eq!(registry.list().len(), BUILTIN.len());
        let mut names: Vec<&str> = registry.list().iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), BUILTIN.len(), "duplicate built-in names");
        for skill in registry.list() {
            assert!(slug::is_valid(&skill.name), "{}", skill.name);
            assert_eq!(
                skill.prompt.matches("{context}").count(),
                1,
                "{} must have exactly one {{context}} slot",
                skill.name
            );
            assert!(
                !skill.description.is_empty(),
                "{} has no description",
                skill.name
            );
            assert!(
                !skill.prompt.ends_with('\n'),
                "{} keeps a trailing newline",
                skill.name
            );
            assert_eq!(skill.source, SkillSource::BuiltIn);
            assert_eq!(
                skill.hidden,
                skill.stage == SkillStage::Extract || skill.internal,
                "{}",
                skill.name
            );
        }
    }

    #[test]
    fn structured_dissent_skills_are_moves_but_not_default_swarm_angles() {
        let registry = SkillRegistry::builtin();
        let moves = registry.move_names();
        for name in ["pr-faq", "dialectical-inquiry", "triz"] {
            assert!(moves.iter().any(|n| n == name), "missing move: {name}");
            // Opt-in via the swarm angle picker; the canonical four stay the default.
            assert!(!crate::concepts::swarm::DEFAULT_ANGLES.contains(&name));
        }
    }

    #[test]
    fn missing_skills_dir_is_just_the_builtins() {
        let tmp = tempfile::tempdir().unwrap();
        let (registry, issues) = SkillRegistry::load(&tmp.path().join("absent"));
        assert!(issues.is_empty());
        assert_eq!(registry.list().len(), BUILTIN.len());
    }

    #[test]
    fn vault_file_overrides_a_builtin_in_place_and_adds_new_skills_at_the_end() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "premortem.md",
            &skill_doc("premortem", "attack", "Mine.\n{context}"),
        );
        write(
            tmp.path(),
            "my-move.md",
            &skill_doc("my-move", "consequence", "New.\n{context}"),
        );
        write(tmp.path(), "notes.txt", "ignored");
        let (registry, issues) = SkillRegistry::load(tmp.path());
        assert!(issues.is_empty(), "{issues:?}");
        let builtin_pos = SkillRegistry::builtin()
            .list()
            .iter()
            .position(|s| s.name == "premortem");
        let pos = registry.list().iter().position(|s| s.name == "premortem");
        assert_eq!(pos, builtin_pos, "override keeps chip position");
        let premortem = registry.get("premortem").unwrap();
        assert_eq!(premortem.prompt, "Mine.\n{context}");
        assert_eq!(premortem.source, SkillSource::VaultOverride);
        let added = registry.list().last().unwrap();
        assert_eq!(added.name, "my-move");
        assert_eq!(added.source, SkillSource::Vault);
        assert_eq!(registry.list().len(), BUILTIN.len() + 1);
    }

    #[test]
    fn an_internal_override_without_hidden_stays_off_the_move_list() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "panel-score.md",
            &skill_doc("panel-score", "converge", "Score it.\n{context}"),
        );
        let (registry, issues) = SkillRegistry::load(tmp.path());
        assert!(issues.is_empty(), "{issues:?}");
        let s = registry.get("panel-score").unwrap();
        assert_eq!(s.source, SkillSource::VaultOverride);
        assert!(s.internal && s.hidden);
        assert!(registry.visible().all(|s| !s.internal));
        assert!(!registry.move_names().contains(&"panel-score".to_string()));
    }

    #[test]
    fn broken_vault_files_are_issues_and_the_builtin_stays_active() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "premortem.md", "no frontmatter at all");
        write(
            tmp.path(),
            "renamed.md",
            &skill_doc("other", "attack", "{context}"),
        );
        write(
            tmp.path(),
            "no-slot.md",
            &skill_doc("no-slot", "attack", "Forgot it."),
        );
        write(
            tmp.path(),
            "bad-name.md",
            &skill_doc("Bad Name", "attack", "{context}"),
        );
        write(
            tmp.path(),
            "huge.md",
            &skill_doc(
                "huge",
                "attack",
                &format!("{}{{context}}", "x".repeat(MAX_SKILL_FILE_BYTES)),
            ),
        );
        let (registry, issues) = SkillRegistry::load(tmp.path());
        let files: Vec<&str> = issues.iter().map(|i| i.file.as_str()).collect();
        assert_eq!(
            files,
            [
                "bad-name.md",
                "huge.md",
                "no-slot.md",
                "premortem.md",
                "renamed.md"
            ]
        );
        assert_eq!(
            registry.get("premortem").unwrap().source,
            SkillSource::BuiltIn
        );
        assert_eq!(registry.list().len(), BUILTIN.len());
    }

    #[test]
    fn live_reload_swaps_the_registry_but_an_old_snapshot_is_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let live = LiveSkills::load(tmp.path().to_path_buf());
        let before = live.snapshot();
        write(
            tmp.path(),
            "my-move.md",
            &skill_doc("my-move", "attack", "{context}"),
        );
        write(tmp.path(), "broken.md", "---\n");
        live.reload();
        assert!(live.snapshot().get("my-move").is_some());
        assert!(
            before.get("my-move").is_none(),
            "an in-flight snapshot must not change"
        );
        assert_eq!(live.issues().len(), 1);
    }

    #[test]
    fn prior_plan_block_names_the_head_open_ids_and_answers() {
        use crate::concepts::build_plan::workbench::tests::{
            seeded, submit, BASE, Q1_ANSWER, SLUG,
        };
        let dir = seeded();
        let v2 = submit(dir.path(), BASE, &[("Q1", Q1_ANSWER)], 1).unwrap();
        let block = prior_plan_block(dir.path(), SLUG);
        assert!(block.starts_with("## Prior plan (ids only — not evidence)\n"));
        assert!(block.contains(&format!("{} · v2", v2.stem)), "{block}");
        assert!(block.contains("do not re-ask answered questions"));
        assert!(block.contains("- open Q2: Which exchange feeds the backtest data?"));
        assert!(block.contains(&format!("- answered Q1 → {Q1_ANSWER}")));
        assert!(block.len() <= PRIOR_PLAN_BYTES + 2);
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(prior_plan_block(empty.path(), SLUG), "");
    }
}
