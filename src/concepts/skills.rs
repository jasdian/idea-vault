//! Skills: named, reusable ideation moves — parameterized prompt templates the AI can apply to
//! an idea on demand (docs/06-concepts/skills.md D18).

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use tokio::sync::Semaphore;

use crate::ai::budget::{assemble_context, AssembledContext, ContextBudget, ContextInput};
use crate::ai::contract;
use crate::ai::ollama::ChatMessage;
use crate::ai::LlmBackend;
use crate::concepts::ConceptError;
use crate::domain::frontmatter::parse_skill;
use crate::domain::{slug, OutputContract, SkillRole, SkillStage};
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
];

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
    pub prompt: String,
    pub source: SkillSource,
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
    Ok(Skill {
        name: fm.name,
        description: fm.description,
        stage: fm.stage,
        role: fm.role,
        contract: fm.contract,
        use_when: fm.use_when,
        avoid_when: fm.avoid_when,
        hidden: fm.hidden,
        prompt,
        source,
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

/// One model call held to an output contract (docs/adr/0023): call, validate/repair, and on a
/// violation ask exactly ONCE more — under the same permit — with the violation read back. If the
/// retry still misses (or fails), the best answer is kept and a warning logged: a wrong-shaped
/// answer beats none. Used by single interactive skill calls and by a workflow's chained step;
/// fan-out agents never retry (`agents::run_agent` repairs only). Callers must NOT hold a permit.
pub(crate) async fn ask_on_contract(
    llm: &LlmBackend,
    ai_semaphore: &Semaphore,
    prompt: String,
    contract: OutputContract,
    label: &str,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<String, ConceptError> {
    let ask = |content: String| {
        llm.chat(vec![ChatMessage {
            role: "user".to_string(),
            content,
        }])
    };
    let _permit = ai_semaphore
        .acquire()
        .await
        .map_err(|_| ConceptError::SemaphoreClosed)?;
    let first = ask(prompt.clone()).await?;
    let violation = match contract::validate(contract, &first) {
        Ok(repaired) => return Ok(repaired),
        Err(violation) => violation,
    };
    progress(&format!("{label} · reshaping the answer"));
    tracing::info!(label, %violation, "contract violated; retrying once");
    let retried = ask(format!("{prompt}{}", contract::retry_note(&violation))).await;
    match retried.as_deref().map(|r| contract::validate(contract, r)) {
        Ok(Ok(repaired)) => Ok(repaired),
        outcome => {
            tracing::warn!(
                label,
                ?outcome,
                "retry did not produce an on-contract answer; keeping the best one"
            );
            Ok(match retried {
                Ok(second) if !second.trim().is_empty() => second.trim().to_string(),
                _ => first.trim().to_string(),
            })
        }
    }
    // permit released on return — before any vault write, which needs no AI slot
}

/// Hydrate a skill's `{context}` slot and run it against the AI, appending the result as an
/// assistant turn (docs/06-concepts/skills.md §D18).
///
/// The `{context}` slot is filled by `ai::budget` (idea body + memory + recent conversation,
/// under `budget` — never the raw full history). The call is gated by the process-wide
/// `ai_semaphore` (ADR-0006: chat, skills, and swarm share one bound). Callers must NOT already
/// hold a permit from that semaphore when calling this — `invoke` acquires its own, and a held
/// permit plus a small configured bound would deadlock.
///
/// The answer is held to the skill's output contract with at most one retry
/// ([`ask_on_contract`]). Stateless: the output is appended as an assistant turn only after the
/// calls complete (nothing partial ever reaches `conversation.md`); idea state is not changed.
pub async fn invoke(
    ollama: &LlmBackend,
    ai_semaphore: &Semaphore,
    vault_dir: &Path,
    idea_slug: &str,
    skill: &Skill,
    budget: ContextBudget,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<String, ConceptError> {
    progress(&format!("running {}", skill.name));
    let context = hydrate_context(vault_dir, idea_slug, budget)?;
    let prompt = skill.prompt.replace("{context}", &context.text);
    let output = ask_on_contract(
        ollama,
        ai_semaphore,
        prompt,
        skill.contract,
        &skill.name,
        progress,
    )
    .await?;

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
                skill.stage == SkillStage::Extract,
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
}
