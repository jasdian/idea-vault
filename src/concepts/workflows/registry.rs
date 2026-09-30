//! The workflow registry (ADR-0035): built-in workflow files compiled into the binary plus the
//! owner's `vault/.workflows/*.md`, each validated against the skill registry it is paired with.
//! Loading follows the skill book exactly (ADR-0022): an owner file whose name matches a built-in
//! replaces it in place, a new name is appended in file-name order, and an invalid file is an
//! issue on the book while any built-in of the same name stays active.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::concepts::agents::AgentRole;
use crate::concepts::skills::{LiveSkills, SkillRegistry, INTERNAL_SKILLS};
use crate::concepts::swarm::MAX_ANGLES;
use crate::concepts::workflows::{
    LoopStage, PanelStage, RefineStage, Stage, Workflow, WorkflowIssue, WorkflowSource,
    WorkflowStep, READY_TO_BUILD, WORKFLOW_MAX_CALLS,
};
use crate::domain::frontmatter::parse_workflow;
use crate::domain::workflow::{StageKind, StageSpec, StepSpec, WorkflowFrontmatter};
use crate::domain::{slug, OutputContract, SkillRole, SkillStage};

/// Largest workflow file the loader accepts — the skill file cap, for the same reason: anything
/// bigger is a mistake, not a definition.
pub const MAX_WORKFLOW_FILE_BYTES: usize = 32 * 1024;

/// Most stages one workflow may chain (ADR-0034).
pub const MAX_STAGES: usize = 8;

/// Longest free-text angle a step or Ground reader may carry, in characters.
const MAX_ANGLE_CHARS: usize = 120;

/// The built-in workflow files, compiled into the binary. Order is the chip order.
const BUILTIN: &[(&str, &str)] = &[
    ("interrogate", include_str!("interrogate.md")),
    (
        "steelman-then-attack",
        include_str!("steelman-then-attack.md"),
    ),
    ("design-panel", include_str!("design-panel.md")),
    ("exhaust", include_str!("exhaust.md")),
    (READY_TO_BUILD, include_str!("ready-to-build.md")),
];

impl From<&StepSpec> for WorkflowStep {
    fn from(s: &StepSpec) -> Self {
        WorkflowStep {
            role: s.role.into(),
            skill: s.skill.clone(),
            angle: s.angle.clone(),
        }
    }
}

fn step(role: SkillRole, skill: Option<&String>) -> WorkflowStep {
    WorkflowStep {
        role: AgentRole::from(role),
        skill: skill.cloned(),
        angle: None,
    }
}

/// The engine's view of one parsed stage. Pure: skills are checked by [`validate`], not here.
fn resolve(spec: &StageSpec) -> Stage {
    match spec {
        StageSpec::FanOut(f) => Stage::FanOut(f.steps.iter().map(WorkflowStep::from).collect()),
        StageSpec::Chain(c) => Stage::Chain(step(c.role, c.skill.as_ref())),
        StageSpec::Audit => Stage::Audit,
        StageSpec::Synthesize => Stage::Synthesize,
        StageSpec::Ground(g) => Stage::Ground(g.clone()),
        StageSpec::Panel(p) => Stage::Panel(PanelStage {
            proposers: p.proposers.iter().map(WorkflowStep::from).collect(),
            criteria: p.criteria.clone(),
            judges: p.judges,
        }),
        StageSpec::Loop(l) => Stage::Loop(LoopStage {
            steps: l.steps.iter().map(WorkflowStep::from).collect(),
            dry_rounds: l.dry_rounds,
            max_rounds: l.max_rounds,
            max_calls: l.max_calls,
        }),
        StageSpec::Refine(r) => Stage::Refine(RefineStage {
            step: step(r.role, Some(&r.skill)),
            max_rounds: r.max_rounds,
        }),
    }
}

/// A stage's owner-named steps as (role, skill, angle), for the cross-registry rules.
fn spec_steps(spec: &StageSpec) -> Vec<(SkillRole, Option<&str>, Option<&str>)> {
    fn of(s: &StepSpec) -> (SkillRole, Option<&str>, Option<&str>) {
        (s.role, s.skill.as_deref(), s.angle.as_deref())
    }
    match spec {
        StageSpec::FanOut(f) => f.steps.iter().map(of).collect(),
        StageSpec::Panel(p) => p.proposers.iter().map(of).collect(),
        StageSpec::Loop(l) => l.steps.iter().map(of).collect(),
        StageSpec::Chain(c) => vec![(c.role, c.skill.as_deref(), None)],
        StageSpec::Refine(r) => vec![(r.role, Some(r.skill.as_str()), None)],
        StageSpec::Audit | StageSpec::Synthesize | StageSpec::Ground(_) => Vec::new(),
    }
}

fn is_build_plan_chain(spec: &StageSpec, skills: &SkillRegistry) -> bool {
    matches!(spec, StageSpec::Chain(c) if c
        .skill
        .as_deref()
        .and_then(|s| skills.get(s))
        .is_some_and(|s| s.contract == OutputContract::BuildPlan))
}

/// Whether a definition is a capstone: derived from its stages, never declared (ADR-0035).
fn is_capstone(specs: &[StageSpec], skills: &SkillRegistry) -> bool {
    specs.iter().any(|s| is_build_plan_chain(s, skills))
}

/// `lo..=hi` as a rule: `None` when `n` is in range, else the message naming the field.
fn out_of(field: &str, n: usize, lo: usize, hi: usize) -> Option<String> {
    (!(lo..=hi).contains(&n)).then(|| format!("{field} is {n}; allowed {lo}..={hi}"))
}

/// The cross-registry rules (ADR-0034, ADR-0035), one message per broken rule: every stage's caps,
/// every skill resolving and sitting where its stage allows, the stage-order rules, the derived
/// capstone rule and the call ceiling. Pure over the definition and `skills`.
pub fn validate(
    fm: &WorkflowFrontmatter,
    specs: &[StageSpec],
    skills: &SkillRegistry,
) -> Vec<String> {
    let mut issues = Vec::new();
    if let Some(m) = out_of("the stage count", specs.len(), 1, MAX_STAGES) {
        issues.push(m);
    }
    let last = specs.len().saturating_sub(1);
    for (i, spec) in specs.iter().enumerate() {
        let kind = spec.kind();
        let at = format!("stages[{i}] ({})", kind.as_str());
        let mut push = |m: String| issues.push(format!("{at}: {m}"));
        for (role, skill, angle) in spec_steps(spec) {
            match skill.map(|s| (s, skills.get(s))) {
                Some((s, _)) if INTERNAL_SKILLS.contains(&s) => {
                    push(format!("{s} is internal to the engine, not a step"))
                }
                Some((s, None)) => push(format!("unknown skill {s}")),
                Some((s, Some(found)))
                    if found.stage == SkillStage::Extract
                        && !(matches!(kind, StageKind::FanOut | StageKind::Loop)
                            && role == SkillRole::Harvester) =>
                {
                    push(format!(
                        "{s} is an extract lens; only a harvester step in a fan_out or loop may run it"
                    ))
                }
                _ => {}
            }
            match angle {
                Some(_) if kind != StageKind::Panel => {
                    push("an angle is read only by panel proposers".into())
                }
                Some(a) if a.chars().count() > MAX_ANGLE_CHARS => {
                    push(format!("an angle is over {MAX_ANGLE_CHARS} characters"))
                }
                _ => {}
            }
        }
        let prior = &specs[..i];
        match spec {
            StageSpec::FanOut(f) => {
                push_some(&mut push, out_of("steps", f.steps.len(), 1, MAX_ANGLES));
            }
            StageSpec::Chain(_) => {
                if is_build_plan_chain(spec, skills) && i != last {
                    push("a build-plan chain must be the last stage".into());
                }
            }
            StageSpec::Audit => {
                let producer = prior.iter().any(|p| {
                    matches!(
                        p,
                        StageSpec::FanOut(_) | StageSpec::Loop(_) | StageSpec::Panel(_)
                    )
                });
                if !producer {
                    push("an audit needs a fan_out, loop or panel before it".into());
                }
            }
            StageSpec::Synthesize => {}
            StageSpec::Ground(g) => {
                if i != 0 {
                    push("ground must be the first stage, and only one may run".into());
                }
                push_some(&mut push, out_of("readers", g.readers, 0, 3));
                push_some(&mut push, out_of("tool_rounds", g.tool_rounds, 1, 2));
                if !g.angles.is_empty() && g.angles.len() != g.readers {
                    push(format!(
                        "{} angles for {} readers; give one per reader or none",
                        g.angles.len(),
                        g.readers
                    ));
                }
                if g.angles.iter().any(|a| a.chars().count() > MAX_ANGLE_CHARS) {
                    push(format!("an angle is over {MAX_ANGLE_CHARS} characters"));
                }
            }
            StageSpec::Panel(p) => {
                push_some(&mut push, out_of("proposers", p.proposers.len(), 2, 4));
                if p.proposers
                    .iter()
                    .any(|s| s.skill.is_none() && s.angle.is_none())
                {
                    push("every proposer needs a skill or an angle".into());
                }
                push_some(&mut push, out_of("criteria", p.criteria.len(), 2, 5));
                for (k, c) in p.criteria.iter().enumerate() {
                    if !slug::is_valid(&c.name) {
                        push(format!("criterion {:?} is not a slug", c.name));
                    } else if p.criteria[..k].iter().any(|o| o.name == c.name) {
                        push(format!("criterion {} is named twice", c.name));
                    }
                    if !(1..=3).contains(&c.weight) {
                        push(format!(
                            "criterion {} weight is {}; allowed 1..=3",
                            c.name, c.weight
                        ));
                    }
                }
                push_some(&mut push, out_of("judges", p.judges, 1, 2));
                let next: Vec<StageKind> =
                    specs[i + 1..].iter().take(2).map(StageSpec::kind).collect();
                if !matches!(
                    next.as_slice(),
                    [StageKind::Synthesize, ..] | [StageKind::Audit, StageKind::Synthesize]
                ) {
                    push("a panel must be followed by synthesize, or audit then synthesize".into());
                }
            }
            StageSpec::Loop(l) => {
                push_some(&mut push, out_of("steps", l.steps.len(), 1, 4));
                if l.steps.iter().any(|s| s.skill.is_none()) {
                    push("every loop step needs a skill".into());
                }
                push_some(&mut push, out_of("dry_rounds", l.dry_rounds, 1, 2));
                push_some(&mut push, out_of("max_rounds", l.max_rounds, 2, 4));
                push_some(
                    &mut push,
                    out_of("max_calls", l.max_calls, l.steps.len().max(1), 16),
                );
            }
            StageSpec::Refine(r) => {
                if !matches!(prior.last(), Some(StageSpec::Audit)) {
                    push("a refine must come right after an audit".into());
                }
                push_some(&mut push, out_of("max_rounds", r.max_rounds, 1, 2));
            }
        }
    }
    if is_capstone(specs, skills) && fm.name != READY_TO_BUILD {
        issues.push(format!(
            "it chains a build-plan skill, which makes it a capstone; only {READY_TO_BUILD} may \
             be one — override {READY_TO_BUILD} instead"
        ));
    }
    let ceiling = specs
        .iter()
        .map(|s| resolve(s).call_ceiling())
        .fold(0, u32::saturating_add);
    if ceiling > WORKFLOW_MAX_CALLS {
        issues.push(format!(
            "its worst case is {ceiling} model calls; the limit is {WORKFLOW_MAX_CALLS}"
        ));
    }
    issues
}

fn push_some(push: &mut impl FnMut(String), message: Option<String>) {
    if let Some(m) = message {
        push(m);
    }
}

/// Parse, check and resolve one workflow document. The name must be a canonical slug equal to the
/// file stem: it is spliced into the `## assistant (workflow: <name>)` turn heading and the route
/// path, so anything outside `[a-z0-9-]` could forge a transcript turn.
fn build_workflow(
    raw: &str,
    stem: &str,
    source: WorkflowSource,
    skills: &SkillRegistry,
) -> Result<Workflow, Vec<String>> {
    if raw.len() > MAX_WORKFLOW_FILE_BYTES {
        return Err(vec![format!(
            "file is {} bytes; the limit is {MAX_WORKFLOW_FILE_BYTES}",
            raw.len()
        )]);
    }
    let (fm, specs, body) = parse_workflow(raw).map_err(|e| vec![e.to_string()])?;
    let mut issues = Vec::new();
    if !slug::is_valid(&fm.name) {
        issues.push(format!(
            "name {:?} must use only lowercase letters, digits and '-'",
            fm.name
        ));
    } else if fm.name != stem {
        issues.push(format!(
            "name {:?} does not match the file name {stem:?}",
            fm.name
        ));
    }
    issues.extend(validate(&fm, &specs, skills));
    if !issues.is_empty() {
        return Err(issues);
    }
    Ok(Workflow {
        capstone: is_capstone(&specs, skills),
        stages: specs.iter().map(resolve).collect(),
        name: fm.name,
        description: fm.description,
        use_when: fm.use_when,
        avoid_when: fm.avoid_when,
        hidden: fm.hidden,
        body,
        source,
        raw: raw.to_string(),
    })
}

fn issues_for(file: &str, messages: Vec<String>) -> impl Iterator<Item = WorkflowIssue> + '_ {
    messages.into_iter().map(move |message| WorkflowIssue {
        file: file.to_string(),
        message,
    })
}

/// The workflows available at runtime, in chip order: the built-ins, then the owner's additions.
#[derive(Debug, Clone)]
pub struct WorkflowRegistry {
    workflows: Vec<Workflow>,
}

impl WorkflowRegistry {
    /// The built-in workflows validated against `skills`. Never panics: a built-in that fails is
    /// logged and left out (a unit test keeps the set clean against the built-in skills).
    pub fn builtin(skills: &SkillRegistry) -> Self {
        let (registry, issues) = Self::builtin_with_issues(skills);
        for issue in issues {
            tracing::error!(file = %issue.file, error = %issue.message, "built-in workflow invalid");
        }
        registry
    }

    fn builtin_with_issues(skills: &SkillRegistry) -> (Self, Vec<WorkflowIssue>) {
        let mut workflows = Vec::with_capacity(BUILTIN.len());
        let mut issues = Vec::new();
        for (stem, raw) in BUILTIN {
            match build_workflow(raw, stem, WorkflowSource::BuiltIn, skills) {
                Ok(w) => workflows.push(w),
                Err(m) => issues.extend(issues_for(&format!("{stem}.md"), m)),
            }
        }
        (Self { workflows }, issues)
    }

    /// The built-ins plus every valid `*.md` in `dir` (the owner's `vault/.workflows/`), in
    /// file-name order, all validated against `skills`. A missing directory is no workflows, not
    /// an error.
    pub fn load(dir: &Path, skills: &SkillRegistry) -> (Self, Vec<WorkflowIssue>) {
        let (mut registry, mut issues) = Self::builtin_with_issues(skills);
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (registry, issues),
            Err(e) => {
                issues.push(WorkflowIssue {
                    file: dir.display().to_string(),
                    message: format!("workflows directory unreadable: {e}"),
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
                .map_err(|e| vec![format!("unreadable: {e}")])
                .and_then(|raw| build_workflow(&raw, &stem, WorkflowSource::Vault, skills));
            match loaded {
                Ok(mut w) => match registry.workflows.iter_mut().find(|x| x.name == w.name) {
                    Some(existing) => {
                        w.source = WorkflowSource::VaultOverride;
                        *existing = w;
                    }
                    None => registry.workflows.push(w),
                },
                Err(m) => issues.extend(issues_for(&file, m)),
            }
        }
        (registry, issues)
    }

    /// Look up a workflow by exact name.
    pub fn get(&self, name: &str) -> Option<&Workflow> {
        self.workflows.iter().find(|w| w.name == name)
    }

    /// All registered workflows, in chip order.
    pub fn list(&self) -> &[Workflow] {
        &self.workflows
    }

    /// Workflows offered to the owner (every non-`hidden` one), in chip order.
    pub fn visible(&self) -> impl Iterator<Item = &Workflow> {
        self.workflows.iter().filter(|w| !w.hidden)
    }
}

/// The skill and workflow registries as one pair (ADR-0035): a job takes one `Book` and runs
/// entirely against it, so the workflows it resolves were validated against exactly the skills
/// it runs.
#[derive(Debug, Clone)]
pub struct Book {
    pub skills: Arc<SkillRegistry>,
    pub workflows: Arc<WorkflowRegistry>,
}

impl Book {
    /// The built-in skills and workflows alone — no owner files.
    pub fn builtin() -> Self {
        let skills = SkillRegistry::builtin();
        let workflows = WorkflowRegistry::builtin(&skills);
        Self {
            skills: Arc::new(skills),
            workflows: Arc::new(workflows),
        }
    }
}

/// The live workflow book held in `AppState`: the owner edits `vault/.workflows/*.md` or a skill
/// and presses reload on the skill book — no restart, no file watcher. A handler takes one
/// [`snapshot`] and moves it into its job.
///
/// [`snapshot`]: LiveWorkflows::snapshot
pub struct LiveWorkflows {
    dir: PathBuf,
    current: RwLock<Arc<Book>>,
    issues: RwLock<Vec<WorkflowIssue>>,
}

fn load_book(dir: &Path, skills: Arc<SkillRegistry>) -> (Book, Vec<WorkflowIssue>) {
    let (workflows, issues) = WorkflowRegistry::load(dir, &skills);
    for issue in &issues {
        tracing::warn!(file = %issue.file, error = %issue.message, "workflow file skipped");
    }
    let book = Book {
        skills,
        workflows: Arc::new(workflows),
    };
    (book, issues)
}

impl LiveWorkflows {
    /// Load the built-ins plus `dir` (usually `<vault>/.workflows`) against the skills as of now.
    pub fn load(dir: PathBuf, skills: &LiveSkills) -> Self {
        let (book, issues) = load_book(&dir, skills.snapshot());
        Self {
            dir,
            current: RwLock::new(Arc::new(book)),
            issues: RwLock::new(issues),
        }
    }

    /// The one per-job snapshot: skills and workflows that were validated together.
    pub fn snapshot(&self) -> Arc<Book> {
        match self.current.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// The files that failed to load or validate on the last (re)load.
    pub fn issues(&self) -> Vec<WorkflowIssue> {
        match self.issues.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// The directory owner workflows are read from.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Reload the skills, then revalidate every workflow against that fresh skill snapshot and
    /// swap the pair in whole, so a snapshot never pairs old workflows with new skills.
    pub fn reload(&self, skills: &LiveSkills) {
        skills.reload();
        let (book, issues) = load_book(&self.dir, skills.snapshot());
        match self.current.write() {
            Ok(mut guard) => *guard = Arc::new(book),
            Err(poisoned) => *poisoned.into_inner() = Arc::new(book),
        }
        match self.issues.write() {
            Ok(mut guard) => *guard = issues,
            Err(poisoned) => *poisoned.into_inner() = issues,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin_skills() -> SkillRegistry {
        SkillRegistry::builtin()
    }

    #[test]
    fn every_builtin_parses_and_validates_clean() {
        let skills = builtin_skills();
        let (registry, issues) = WorkflowRegistry::builtin_with_issues(&skills);
        assert!(issues.is_empty(), "{issues:?}");
        let names: Vec<&str> = registry.list().iter().map(|w| w.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "interrogate",
                "steelman-then-attack",
                "design-panel",
                "exhaust",
                READY_TO_BUILD
            ]
        );
        for w in registry.list() {
            assert_eq!(w.source, WorkflowSource::BuiltIn);
            assert!(w.call_ceiling() <= WORKFLOW_MAX_CALLS, "{}", w.name);
            assert!(!w.body.is_empty() && !w.use_when.is_empty(), "{}", w.name);
            for s in w.skills() {
                assert!(skills.get(s).is_some(), "{}: {s}", w.name);
            }
            assert_eq!(w.capstone, w.name == READY_TO_BUILD, "{}", w.name);
            assert_eq!(
                w.needs_sources(),
                matches!(w.name.as_str(), "design-panel" | READY_TO_BUILD),
                "{}",
                w.name
            );
        }
    }

    #[test]
    fn call_ceilings_of_builtins() {
        let registry = WorkflowRegistry::builtin(&builtin_skills());
        let ceiling = |name: &str| registry.get(name).expect(name).call_ceiling();
        assert_eq!(
            ceiling("interrogate"),
            6,
            "fan-out 4, audit 1, synthesize 1"
        );
        assert_eq!(
            ceiling("steelman-then-attack"),
            7,
            "chain 2, fan-out 3, audit, synthesize"
        );
        assert_eq!(
            ceiling("design-panel"),
            4 + 3 + 3 + 1 + 1,
            "ground 2×2, 3 proposals, 3 scores, audit, synthesize"
        );
        assert_eq!(
            ceiling("exhaust"),
            9 + 1 + 2 + 1,
            "loop min(12 calls, 3 rounds × 3 steps), audit, refine 1×2, synthesize"
        );
        assert_eq!(ceiling(READY_TO_BUILD), 4 + 5 + 1 + 2, "ground adds 4");
    }

    type Steps<'a> = Vec<(AgentRole, Option<&'a str>)>;

    fn shape(w: &Workflow) -> Vec<(StageKind, Steps<'_>)> {
        w.stages
            .iter()
            .map(|s| {
                let steps = s
                    .steps()
                    .iter()
                    .map(|st| (st.role, st.skill.as_deref()))
                    .collect();
                (s.kind(), steps)
            })
            .collect()
    }

    /// The definitions as they were compiled in before workflows became markdown: same stages,
    /// same roles and skills in the same order, same descriptions.
    #[test]
    fn migration_equivalence_interrogate_and_steelman_match_pre_migration_shape() {
        use AgentRole::*;
        let registry = WorkflowRegistry::builtin(&builtin_skills());
        let interrogate = registry.get("interrogate").expect("built-in");
        assert_eq!(
            shape(interrogate),
            vec![
                (
                    StageKind::FanOut,
                    vec![
                        (Critic, Some("premortem")),
                        (Critic, Some("cheapest-disproof")),
                        (Researcher, Some("constraints")),
                        (Critic, Some("second-order-effects")),
                    ]
                ),
                (StageKind::Audit, vec![]),
                (StageKind::Synthesize, vec![]),
            ]
        );
        assert_eq!(
            interrogate.description,
            "Fan out diverse critics + a researcher, audit the findings, synthesize one position \
             (the canonical D19 run-it-into-the-ground pass)"
        );
        let steelman = registry.get("steelman-then-attack").expect("built-in");
        assert_eq!(
            shape(steelman),
            vec![
                (StageKind::Chain, vec![(Advocate, Some("steelman"))]),
                (
                    StageKind::FanOut,
                    vec![
                        (Critic, Some("premortem")),
                        (Critic, Some("cheapest-disproof")),
                        (Critic, Some("devils-advocate")),
                    ]
                ),
                (StageKind::Audit, vec![]),
                (StageKind::Synthesize, vec![]),
            ]
        );
        assert_eq!(
            steelman.description,
            "Build the strongest case for the idea first, then send three critics at that \
             steelman, audit what they find, and synthesize"
        );
        let rtb = registry.get(READY_TO_BUILD).expect("built-in");
        assert_eq!(rtb.stages[0].kind(), StageKind::Ground);
        let harvest: Vec<&str> = rtb.stages[1]
            .steps()
            .iter()
            .filter_map(|s| s.skill.as_deref())
            .collect();
        assert_eq!(harvest, crate::concepts::knowledge::LENSES);
        assert!(rtb.stages[1].steps().iter().all(|s| s.role == Harvester));
        assert_eq!(
            shape(rtb)[2..],
            [
                (StageKind::Audit, vec![]),
                (StageKind::Chain, vec![(Synthesizer, Some("build-prompt"))]),
            ]
        );
        assert!(registry
            .list()
            .iter()
            .filter(|w| w.name != "design-panel")
            .flat_map(|w| w.stages.iter().flat_map(Stage::steps))
            .all(|s| s.angle.is_none()));
    }

    fn doc(name: &str, stages: &str) -> String {
        format!("---\nname: {name}\ndescription: \"d\"\nstages:\n{stages}---\n\nBody.\n")
    }

    const SIMPLE: &str = "  - kind: fan_out\n    steps:\n      - {role: critic, skill: premortem}\n  - kind: synthesize\n";

    #[test]
    fn vault_override_keeps_position_and_invalid_override_keeps_builtin() {
        let skills = builtin_skills();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("steelman-then-attack.md"),
            doc("steelman-then-attack", SIMPLE),
        )
        .unwrap();
        std::fs::write(tmp.path().join("zz-mine.md"), doc("zz-mine", SIMPLE)).unwrap();
        std::fs::write(
            tmp.path().join("interrogate.md"),
            doc("interrogate", "  - kind: synthesize\n    oops: 1\n"),
        )
        .unwrap();
        let (registry, issues) = WorkflowRegistry::load(tmp.path(), &skills);
        let listed: Vec<(&str, WorkflowSource)> = registry
            .list()
            .iter()
            .map(|w| (w.name.as_str(), w.source))
            .collect();
        assert_eq!(
            listed,
            [
                ("interrogate", WorkflowSource::BuiltIn),
                ("steelman-then-attack", WorkflowSource::VaultOverride),
                ("design-panel", WorkflowSource::BuiltIn),
                ("exhaust", WorkflowSource::BuiltIn),
                (READY_TO_BUILD, WorkflowSource::BuiltIn),
                ("zz-mine", WorkflowSource::Vault),
            ]
        );
        assert_eq!(
            registry.get("steelman-then-attack").unwrap().stages.len(),
            2
        );
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].file, "interrogate.md");
        assert!(
            issues[0].message.contains("stages[0] (synthesize)"),
            "{issues:?}"
        );
    }

    #[test]
    fn validation_rules_each_yield_one_issue() {
        let skills = builtin_skills();
        let fan = |n: usize| {
            let steps: String = (0..n)
                .map(|_| "      - {role: critic, skill: premortem}\n")
                .collect();
            format!("  - kind: fan_out\n    steps:\n{steps}")
        };
        let harvest = "  - kind: fan_out\n    steps:\n      - {role: harvester, skill: extract-key-decisions}\n";
        let plan = "  - kind: chain\n    role: synthesizer\n    skill: build-prompt\n";
        let big = format!(
            "{}{}",
            doc("big", SIMPLE),
            "x".repeat(MAX_WORKFLOW_FILE_BYTES)
        );
        let cases: Vec<(&str, &str, String, &str)> = vec![
            ("unknown kind", "w", doc("w", "  - kind: judge\n"), "stages[0] (judge)"),
            (
                "unknown top-level key",
                "w",
                format!("---\nname: w\ndescription: d\ncolour: red\nstages:\n{SIMPLE}---\n"),
                "colour",
            ),
            (
                "unknown stage key",
                "w",
                doc("w", &format!("{SIMPLE}  - kind: audit\n    deep: true\n")),
                "stages[2] (audit)",
            ),
            (
                "unknown skill",
                "w",
                doc("w", "  - kind: fan_out\n    steps:\n      - {role: critic, skill: nope}\n  - kind: synthesize\n"),
                "stages[0] (fan_out): unknown skill nope",
            ),
            (
                "internal skill",
                "w",
                doc("w", "  - kind: fan_out\n    steps:\n      - {role: researcher, skill: ground-read}\n  - kind: synthesize\n"),
                "ground-read is internal",
            ),
            ("non-slug name", "Bad Name", doc("Bad Name", SIMPLE), "lowercase"),
            ("stem differs", "other", doc("w", SIMPLE), "does not match the file name"),
            ("over 32 KiB", "big", big, "the limit is 32768"),
            (
                "build plan not last",
                READY_TO_BUILD,
                doc(READY_TO_BUILD, &format!("{harvest}{plan}  - kind: synthesize\n")),
                "stages[1] (chain): a build-plan chain must be the last stage",
            ),
            (
                "capstone under another name",
                "my-plan",
                doc("my-plan", &format!("{harvest}  - kind: audit\n{plan}")),
                "capstone",
            ),
            (
                "fan_out of 9",
                "w",
                doc("w", &format!("{}  - kind: synthesize\n", fan(9))),
                "stages[0] (fan_out): steps is 9; allowed 1..=8",
            ),
            (
                "ceiling over 32",
                "w",
                doc("w", &format!("{}  - kind: audit\n  - kind: synthesize\n", fan(8).repeat(5))),
                "42 model calls",
            ),
        ];
        for (label, stem, raw, needle) in cases {
            let issues =
                build_workflow(&raw, stem, WorkflowSource::Vault, &skills).expect_err(label);
            assert_eq!(issues.len(), 1, "{label}: {issues:?}");
            assert!(issues[0].contains(needle), "{label}: {issues:?}");
        }
    }

    #[test]
    fn stage_order_and_cap_rules_hold_for_the_new_kinds() {
        let skills = builtin_skills();
        let panel = "  - kind: panel\n    proposers:\n      - {role: advocate, skill: steelman}\n      - {role: researcher, angle: \"ship it this week\"}\n    criteria:\n      - {name: cost, weight: 2, zero: a, two: b}\n      - {name: fit, weight: 1, zero: a, two: b}\n";
        let ground = "  - kind: ground\n";
        let loop_ = "  - kind: loop\n    steps:\n      - {role: critic, skill: premortem}\n    max_calls: 4\n";
        let refine = "  - kind: refine\n    role: advocate\n    skill: steelman\n";
        let ok = |stages: String| {
            build_workflow(&doc("w", &stages), "w", WorkflowSource::Vault, &skills)
        };
        let w = ok(format!(
            "{ground}{panel}  - kind: audit\n  - kind: synthesize\n"
        ))
        .unwrap();
        assert!(w.needs_sources());
        assert_eq!(w.call_ceiling(), 4 + 4 + 1 + 1);
        let w = ok(format!(
            "{loop_}  - kind: audit\n{refine}  - kind: synthesize\n"
        ))
        .unwrap();
        assert_eq!(
            w.call_ceiling(),
            3 + 1 + 2 + 1,
            "loop is min(max_calls, rounds·steps)"
        );
        for (stages, needle) in [
            (
                format!("{SIMPLE}{ground}"),
                "ground must be the first stage",
            ),
            (
                format!("{panel}  - kind: chain\n    role: critic\n"),
                "a panel must be followed",
            ),
            (
                format!("{SIMPLE}{refine}"),
                "a refine must come right after an audit",
            ),
            (
                "  - kind: audit\n  - kind: synthesize\n".to_string(),
                "an audit needs",
            ),
        ] {
            let issues = ok(stages).expect_err(needle);
            assert_eq!(issues.len(), 1, "{issues:?}");
            assert!(issues[0].contains(needle), "{issues:?}");
        }
    }

    fn write_skill(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(format!("{name}.md")),
            format!(
                "---\nname: {name}\ndescription: \"d\"\nstage: attack\n---\n\nX\n{{context}}\n"
            ),
        )
        .unwrap();
    }

    fn uses_mine() -> String {
        doc(
            "uses-mine",
            "  - kind: fan_out\n    steps:\n      - {role: critic, skill: my-lens}\n  - kind: synthesize\n",
        )
    }

    #[test]
    fn skill_reload_invalidates_dependent_workflow() {
        let tmp = tempfile::tempdir().unwrap();
        let (skills_dir, flows_dir) = (tmp.path().join(".skills"), tmp.path().join(".workflows"));
        write_skill(&skills_dir, "my-lens");
        std::fs::create_dir_all(&flows_dir).unwrap();
        std::fs::write(flows_dir.join("uses-mine.md"), uses_mine()).unwrap();
        let skills = LiveSkills::load(skills_dir.clone());
        let live = LiveWorkflows::load(flows_dir, &skills);
        assert!(live.snapshot().workflows.get("uses-mine").is_some());
        assert!(live.issues().is_empty(), "{:?}", live.issues());

        std::fs::remove_file(skills_dir.join("my-lens.md")).unwrap();
        live.reload(&skills);
        let book = live.snapshot();
        assert!(book.workflows.get("uses-mine").is_none());
        assert!(
            book.skills.get("my-lens").is_none(),
            "reload re-read the skills too"
        );
        let issues = live.issues();
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].file, "uses-mine.md");
        assert!(issues[0].message.contains("unknown skill my-lens"));
    }

    #[test]
    fn snapshot_pair_is_stable_across_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let (skills_dir, flows_dir) = (tmp.path().join(".skills"), tmp.path().join(".workflows"));
        let skills = LiveSkills::load(skills_dir.clone());
        let live = LiveWorkflows::load(flows_dir.clone(), &skills);
        let before = live.snapshot();

        write_skill(&skills_dir, "my-lens");
        std::fs::create_dir_all(&flows_dir).unwrap();
        std::fs::write(flows_dir.join("uses-mine.md"), uses_mine()).unwrap();
        live.reload(&skills);

        assert!(before.skills.get("my-lens").is_none());
        assert!(
            before.workflows.get("uses-mine").is_none(),
            "the old pair is untouched"
        );
        let after = live.snapshot();
        assert!(after.skills.get("my-lens").is_some());
        assert!(
            after.workflows.get("uses-mine").is_some(),
            "the new pair agrees"
        );
        for skill in after.workflows.list().iter().flat_map(Workflow::skills) {
            assert!(after.skills.get(skill).is_some(), "{skill}");
        }
    }
}
