//! Make skill (docs/adr/0042, D42): distil the move that worked in one discussion into a draft
//! owner skill file, held as a `skill_draft` artifact until the owner reviews and saves it.
//!
//! The pure half decides everything that is not the model call: the code-built move trace, the
//! distillable precheck, evidence grounding, finalization (the code-placed `{context}` slot and
//! `origin`), placement against the skill book, similarity hints, the review diff and the Save
//! rules. The run journal is never read here (ADR-0037).

use std::path::Path;

use crate::ai::contract;
use crate::ai::provenance::digest12;
use crate::concepts::skills::{
    check_candidate, Skill, SkillRegistry, SkillSource, INTERNAL_SKILLS,
};
use crate::domain::evidence::{content_words, grounded, normalize_for_match};
use crate::domain::frontmatter::{emit_skill, parse_skill};
use crate::vault::store::{self, TurnSource};

/// Most bytes the move trace adds to the distiller's prompt.
pub const TRACE_BYTES: usize = 1500;
/// Most bytes the skill-book list adds to the distiller's prompt.
pub const BOOK_BYTES: usize = 1024;
/// Owner turns an idea needs before a distil is worth a model call.
pub const MIN_OWNER_TURNS: usize = 2;
/// Named move turns (skill, swarm, workflow, knowledge) an idea needs before a distil.
pub const MIN_MOVE_TURNS: usize = 1;
/// Similarity (thousandths, token Jaccard) at which a registered skill is shown as a hint.
pub const SIMILAR_MILLI: u16 = 500;
/// Longest excerpt of one turn in the move trace, in characters.
const EXCERPT_CHARS: usize = 160;
/// Most similarity hints shown.
const SIMILAR_TOP: usize = 3;

/// One transcript turn as the move trace shows it: its 0-based index, who wrote it, its first
/// line, and the audit labels its text carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceLine {
    pub index: usize,
    pub source: String,
    pub excerpt: String,
    pub confirmed: u16,
    pub uncertain: u16,
    pub refuted: u16,
}

/// The move trace of a transcript: every turn except build-plan turns and their pointers
/// ([`store::is_capstone_turn`], the same exclusion every evidence haystack uses).
pub fn move_trace(conversation: &str) -> Vec<TraceLine> {
    store::split_turns(conversation)
        .iter()
        .enumerate()
        .filter(|(_, turn)| !store::is_capstone_turn(turn))
        .map(|(index, turn)| {
            let body = turn.split_once('\n').map_or("", |(_, rest)| rest);
            let count =
                |label: &str| u16::try_from(body.matches(label).count()).unwrap_or(u16::MAX);
            TraceLine {
                index,
                source: source_label(&store::parse_turn_heading(store::turn_role(turn))),
                excerpt: body
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .map(|l| l.chars().take(EXCERPT_CHARS).collect())
                    .unwrap_or_default(),
                confirmed: count("CONFIRMED"),
                uncertain: count("UNCERTAIN"),
                refuted: count("REFUTED"),
            }
        })
        .collect()
}

fn source_label(source: &TurnSource) -> String {
    match source {
        TurnSource::User => "user".to_string(),
        TurnSource::Chat => "chat".to_string(),
        TurnSource::Skill(name) => format!("skill: {name}"),
        TurnSource::Swarm(angles) if angles.is_empty() => "swarm".to_string(),
        TurnSource::Swarm(angles) => format!("swarm: {}", angles.join(", ")),
        TurnSource::Workflow(name) => format!("workflow: {name}"),
        TurnSource::Knowledge => "knowledge".to_string(),
        TurnSource::Other(role) => role.clone(),
    }
}

const TRACE_HEADING: &str = "## Move trace (code-built — not evidence)\n";

/// The trace as a prompt block under `max_bytes`, headed as code-built and not evidence. When
/// it does not fit, the newest turns are kept: the move that worked is usually the latest.
pub fn trace_block(trace: &[TraceLine], max_bytes: usize) -> String {
    let lines: Vec<String> = trace
        .iter()
        .map(|t| {
            let mut labels = Vec::new();
            for (label, n) in [
                ("CONFIRMED", t.confirmed),
                ("UNCERTAIN", t.uncertain),
                ("REFUTED", t.refuted),
            ] {
                if n > 0 {
                    labels.push(format!("{label} {n}"));
                }
            }
            let tail = if labels.is_empty() {
                String::new()
            } else {
                format!(" [{}]", labels.join(", "))
            };
            format!("- #{} {} — {}{tail}\n", t.index, t.source, t.excerpt)
        })
        .collect();
    let mut room = max_bytes.saturating_sub(TRACE_HEADING.len());
    let mut kept: Vec<&str> = Vec::new();
    for line in lines.iter().rev() {
        if line.len() > room {
            break;
        }
        room -= line.len();
        kept.push(line);
    }
    if kept.is_empty() {
        return String::new();
    }
    kept.reverse();
    format!("{TRACE_HEADING}{}", kept.concat())
}

/// Why an idea is not worth a distil yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotDistillable {
    TooFewOwnerTurns(usize),
    NoMoves,
}

impl std::fmt::Display for NotDistillable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("nothing to distil yet — run a move or two first")
    }
}

/// The distillable precheck (docs/adr/0042): at least [`MIN_OWNER_TURNS`] owner turns and
/// [`MIN_MOVE_TURNS`] named move turns, so a paid call never runs on an empty idea.
pub fn distillable(conversation: &str) -> Result<(), NotDistillable> {
    let turns = store::split_turns(conversation);
    let sources: Vec<TurnSource> = turns
        .iter()
        .filter(|t| !store::is_capstone_turn(t))
        .map(|t| store::parse_turn_heading(store::turn_role(t)))
        .collect();
    let owner = sources.iter().filter(|s| **s == TurnSource::User).count();
    if owner < MIN_OWNER_TURNS {
        return Err(NotDistillable::TooFewOwnerTurns(owner));
    }
    let moves = sources
        .iter()
        .filter(|s| match s {
            TurnSource::Skill(_)
            | TurnSource::Swarm(_)
            | TurnSource::Workflow(_)
            | TurnSource::Knowledge => true,
            TurnSource::User | TurnSource::Chat | TurnSource::Other(_) => false,
        })
        .count();
    if moves < MIN_MOVE_TURNS {
        return Err(NotDistillable::NoMoves);
    }
    Ok(())
}

/// One evidence quote, checked against the discussion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceLine {
    pub quote: String,
    /// Found (normalized) in a non-capstone turn.
    pub grounded: bool,
    /// Found in one of the owner's own `## user` turns.
    pub owner: bool,
}

/// Every quote of a `## Evidence` list, grounded against the non-capstone turns and, for
/// `owner`, against the owner's turns alone (ADR-0023's quote gate, advisory here: D3).
pub fn check_evidence(evidence_md: &str, conversation: &str) -> Vec<EvidenceLine> {
    let turns: Vec<String> = store::split_turns(conversation)
        .into_iter()
        .filter(|t| !store::is_capstone_turn(t))
        .collect();
    let all = normalize_for_match(&turns.concat());
    let owner = normalize_for_match(
        &turns
            .iter()
            .filter(|t| store::parse_turn_heading(store::turn_role(t)) == TurnSource::User)
            .map(String::as_str)
            .collect::<String>(),
    );
    contract::evidence_quotes(evidence_md)
        .into_iter()
        .map(|quote| EvidenceLine {
            grounded: grounded(&quote, &all),
            owner: grounded(&quote, &owner),
            quote,
        })
        .collect()
}

/// Finalize a drafted skill file for idea `idea_slug`: strip any `{context}` the model wrote,
/// append the one code-placed slot (a literal slot in the distiller's own prompt would be filled
/// with the discussion, so the model is never asked to write one), set `origin`, and check the
/// result by the loader's rules.
pub fn finalize(file: &str, idea_slug: &str) -> Result<String, String> {
    let (mut fm, prompt) = parse_skill(file).map_err(|e| e.to_string())?;
    fm.origin = Some(idea_slug.to_string());
    let prompt = format!(
        "{}\n{{context}}\n",
        prompt.replace("{context}", "").trim_end()
    );
    let raw = emit_skill(&fm, &prompt).map_err(|e| e.to_string())?;
    check_candidate(&raw)?;
    Ok(raw)
}

/// Where a drafted skill would land in the skill book.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// A new owner skill.
    Add,
    /// An owner skill of this name exists and would be replaced; its raw text and digest are the
    /// review diff's base and Save's stale check.
    Update {
        current_raw: String,
        current_digest: String,
    },
    /// The name belongs to a built-in or engine-only skill, which this path never overwrites.
    Rename { reason: String },
}

/// The placement of a skill named `name` (D6): a built-in (overridden or not) or internal name
/// must be renamed, an existing owner file is an update, anything else an add.
pub fn placement(name: &str, registry: &SkillRegistry, skills_dir: &Path) -> Placement {
    if let Some(reason) = reserved_name(name, registry).map(|r| r.to_string()) {
        return Placement::Rename { reason };
    }
    match store::read_owner_skill(skills_dir, name) {
        Ok(Some(current_raw)) => Placement::Update {
            current_digest: digest12(current_raw.as_bytes()),
            current_raw,
        },
        Ok(None) => Placement::Add,
        Err(e) => Placement::Rename {
            reason: format!("the owner skill file {name}.md cannot be read: {e}"),
        },
    }
}

/// The refusal for a name this path may never write: an engine-only skill, or a built-in
/// whether or not an owner file overrides it (D6; overrides stay hand-written, ADR-0022).
fn reserved_name(name: &str, registry: &SkillRegistry) -> Option<SaveRefusal> {
    if INTERNAL_SKILLS.contains(&name) {
        return Some(SaveRefusal::InternalName(name.to_string()));
    }
    match registry.get(name).map(|s| s.source) {
        Some(SkillSource::BuiltIn | SkillSource::VaultOverride) => {
            Some(SaveRefusal::BuiltInName(name.to_string()))
        }
        Some(SkillSource::Vault) | None => None,
    }
}

/// A registered skill similar to the draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Similar {
    pub name: String,
    pub score_milli: u16,
}

/// The visible registered skills (other than the candidate's own name) whose description and
/// use-when share at least [`SIMILAR_MILLI`] of their content words with the candidate's, best
/// first, at most three. Hints only; they never block a Save.
pub fn similar(candidate: &Skill, registry: &SkillRegistry) -> Vec<Similar> {
    let words = |s: &Skill| content_words(&format!("{} {}", s.description, s.use_when));
    let mine = words(candidate);
    let mut hits: Vec<Similar> = registry
        .visible()
        .filter(|s| s.name != candidate.name)
        .filter_map(|s| {
            let theirs = words(s);
            let union = mine.union(&theirs).count();
            if union == 0 {
                return None;
            }
            let shared = mine.intersection(&theirs).count();
            let score_milli = u16::try_from(shared * 1000 / union).unwrap_or(u16::MAX);
            (score_milli >= SIMILAR_MILLI).then(|| Similar {
                name: s.name.clone(),
                score_milli,
            })
        })
        .collect();
    hits.sort_by_key(|s| std::cmp::Reverse(s.score_milli));
    hits.truncate(SIMILAR_TOP);
    hits
}

/// One line of a review diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    Same(String),
    Added(String),
    Removed(String),
}

/// A longest-common-subsequence line diff of `old` to `new`. Skill files are at most 32 KiB,
/// so the quadratic table is cheap and no diff dependency is needed.
pub fn line_diff(old: &str, new: &str) -> Vec<DiffLine> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    // lcs[i][j]: the LCS length of a[i..] and b[j..].
    let mut lcs = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::with_capacity(a.len().max(b.len()));
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            out.push(DiffLine::Same(a[i].to_string()));
            i += 1;
            j += 1;
        } else if i < a.len() && (j == b.len() || lcs[i + 1][j] >= lcs[i][j + 1]) {
            // Removals before additions on a tie, as a reader expects a changed line to read.
            out.push(DiffLine::Removed(a[i].to_string()));
            i += 1;
        } else {
            out.push(DiffLine::Added(b[j].to_string()));
            j += 1;
        }
    }
    out
}

/// A skill draft as its artifact holds it: the finalized file and its checked evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub raw: String,
    pub evidence: Vec<EvidenceLine>,
}

/// The artifact body of a draft: the file in a `~~~skill` fence, then the evidence list with a
/// ✓ (and `owner`) or ✗ on each quote.
pub fn render_draft_body(d: &Draft) -> String {
    let evidence: Vec<String> = d
        .evidence
        .iter()
        .map(|e| {
            let mark = match (e.grounded, e.owner) {
                (true, true) => "✓ owner",
                (true, false) => "✓",
                (false, _) => "✗",
            };
            format!("- {mark} \"{}\"", e.quote)
        })
        .collect();
    format!(
        "~~~skill\n{}\n~~~\n\n{}\n{}\n",
        d.raw.trim_end(),
        contract::EVIDENCE_HEADING,
        evidence.join("\n")
    )
}

/// Read a draft back from its artifact body; the inverse of [`render_draft_body`].
pub fn parse_draft_body(body: &str) -> Option<Draft> {
    let (file, evidence) = contract::split_skill_draft(body).ok()?;
    let evidence = evidence
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("- ")?;
            let (grounded, owner, rest) = if let Some(r) = rest.strip_prefix("✓ owner ") {
                (true, true, r)
            } else if let Some(r) = rest.strip_prefix("✓ ") {
                (true, false, r)
            } else {
                (false, false, rest.strip_prefix("✗ ")?)
            };
            let quote = rest.strip_prefix('"')?.strip_suffix('"')?;
            Some(EvidenceLine {
                quote: quote.to_string(),
                grounded,
                owner,
            })
        })
        .collect();
    Some(Draft {
        raw: format!("{file}\n"),
        evidence,
    })
}

/// What a Save writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveTarget {
    Add,
    Update,
}

/// Why a Save was refused. Grounding never refuses (owner decision D3: warn only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveRefusal {
    /// The edited text fails the skill loader's rules.
    Invalid(String),
    BuiltInName(String),
    InternalName(String),
    /// The owner file changed since the review panel showed it.
    Superseded,
}

impl std::fmt::Display for SaveRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveRefusal::Invalid(why) => write!(f, "not a valid skill file: {why}"),
            SaveRefusal::BuiltInName(name) => write!(
                f,
                "{name} is a built-in skill; pick another name (a built-in is never overwritten from here)"
            ),
            SaveRefusal::InternalName(name) => {
                write!(f, "{name} is an engine-only skill; pick another name")
            }
            SaveRefusal::Superseded => f.write_str(
                "the owner skill changed since this draft was shown; review the new diff and save again",
            ),
        }
    }
}

/// The Save rules (docs/adr/0042 §Save): the edited text must load, its name must not be a
/// built-in or internal skill, and an update must name the digest of the owner file as it is
/// now (`base_digest`), so a file changed meanwhile is never overwritten unseen.
pub fn save_check(
    edited: &str,
    registry: &SkillRegistry,
    skills_dir: &Path,
    base_digest: Option<&str>,
) -> Result<(SaveTarget, Skill), SaveRefusal> {
    let skill = check_candidate(edited).map_err(SaveRefusal::Invalid)?;
    if let Some(refusal) = reserved_name(&skill.name, registry) {
        return Err(refusal);
    }
    match store::read_owner_skill(skills_dir, &skill.name) {
        Ok(None) => Ok((SaveTarget::Add, skill)),
        Ok(Some(current)) if base_digest == Some(digest12(current.as_bytes()).as_str()) => {
            Ok((SaveTarget::Update, skill))
        }
        Ok(Some(_)) => Err(SaveRefusal::Superseded),
        Err(e) => Err(SaveRefusal::Invalid(format!(
            "the owner skill file cannot be read: {e}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONVO: &str = "## user\nA peer-tutoring marketplace for high-schoolers.\n\n\
## assistant (skill: premortem)\n1. **Regulators ban it** — CONFIRMED by precedent\n2. Churn — REFUTED\n\n\
## user\nNow assume a regulator hates it and wants it dead within a year.\n\n\
## assistant\nA hostile regulator would first classify the app as an employer.\n\n\
## assistant (skill: build-prompt)\nplan text that is never evidence at all\n\n\
## assistant (skill: steelman)\n**Build plan** → [p](/idea/x/artifact/p.md) · quick\n";

    fn file(name: &str) -> String {
        format!(
            "---\nname: {name}\ndescription: \"Attack an idea as a hostile regulator would.\"\nstage: attack\nrole: critic\ncontract: ranked_list\nuse_when: \"Regulated markets.\"\n---\n\nAssume a regulator hates the idea."
        )
    }

    #[test]
    fn move_trace_excludes_capstone_turns_and_counts_labels() {
        let trace = move_trace(CONVO);
        let sources: Vec<&str> = trace.iter().map(|t| t.source.as_str()).collect();
        assert_eq!(sources, ["user", "skill: premortem", "user", "chat"]);
        assert_eq!(trace[1].index, 1);
        assert_eq!((trace[1].confirmed, trace[1].refuted), (1, 1));
        assert_eq!(
            trace[2].excerpt,
            "Now assume a regulator hates it and wants it dead within a year."
        );
        let long = format!(
            "## user\n{}\n## assistant (skill: steelman)\nok\n",
            "x".repeat(400)
        );
        assert!(move_trace(&long)[0].excerpt.chars().count() <= EXCERPT_CHARS);
    }

    #[test]
    fn trace_block_is_headed_bounded_and_keeps_the_newest() {
        let trace = move_trace(CONVO);
        let block = trace_block(&trace, TRACE_BYTES);
        assert!(
            block.starts_with("## Move trace (code-built — not evidence)\n"),
            "{block}"
        );
        assert!(block.contains("skill: premortem") && block.contains("CONFIRMED 1"));
        let small = trace_block(&trace, 200);
        assert!(small.len() <= 200, "{} bytes", small.len());
        assert!(small.contains("chat"), "the newest turn survives: {small}");
        assert!(!small.contains("#0 "), "the oldest goes first: {small}");
    }

    #[test]
    fn distillable_needs_owner_turns_and_a_move() {
        assert_eq!(distillable(CONVO), Ok(()));
        assert_eq!(
            distillable("## user\nhi\n## assistant (skill: premortem)\n1. x\n"),
            Err(NotDistillable::TooFewOwnerTurns(1))
        );
        assert_eq!(
            distillable("## user\nhi\n## assistant\nyo\n## user\nmore\n## assistant (skill: build-prompt)\nplan\n"),
            Err(NotDistillable::NoMoves),
            "a capstone turn is not a move"
        );
    }

    #[test]
    fn check_evidence_marks_owner_grounded_and_fabricated_quotes() {
        let ev = "- \"assume a regulator hates it\"\n- \"first classify the app as an employer\"\n- \"regulators love this idea deeply\"\n- \"plan text that is never evidence\"";
        let lines = check_evidence(ev, CONVO);
        let flags: Vec<(bool, bool)> = lines.iter().map(|l| (l.grounded, l.owner)).collect();
        assert_eq!(
            flags,
            [(true, true), (true, false), (false, false), (false, false)]
        );
        assert_eq!(lines[0].quote, "assume a regulator hates it");
    }

    #[test]
    fn finalize_places_one_slot_sets_origin_and_passes_the_loader() {
        let drafted = file("hostile-regulator").replace("the idea.", "the idea.\n{context}\nMore.");
        let raw = finalize(&drafted, "tutoring").unwrap();
        assert_eq!(raw.matches("{context}").count(), 1);
        let skill = check_candidate(&raw).unwrap();
        assert!(skill.prompt.ends_with("{context}"), "{}", skill.prompt);
        assert!(skill.prompt.contains("More."));
        assert_eq!(skill.origin.as_deref(), Some("tutoring"));
        assert!(finalize("not a skill", "tutoring").is_err());
        assert!(
            finalize(&file("x"), "../bad").is_err(),
            "a bad origin never passes"
        );
    }

    #[test]
    fn placement_renames_builtin_and_internal_updates_owner_files_else_adds() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        store::write_owner_skill(
            dir,
            "premortem",
            &file("premortem").replace("Assume", "{context} Assume"),
        )
        .unwrap();
        store::write_owner_skill(dir, "mine", &format!("{}\n{{context}}", file("mine"))).unwrap();
        let (registry, issues) = SkillRegistry::load(dir);
        assert!(issues.is_empty(), "{issues:?}");
        assert!(
            matches!(
                placement("premortem", &registry, dir),
                Placement::Rename { .. }
            ),
            "an overridden built-in"
        );
        assert!(matches!(
            placement("steelman", &registry, dir),
            Placement::Rename { .. }
        ));
        assert!(matches!(
            placement("distill-skill", &registry, dir),
            Placement::Rename { .. }
        ));
        let current = store::read_owner_skill(dir, "mine").unwrap().unwrap();
        assert_eq!(
            placement("mine", &registry, dir),
            Placement::Update {
                current_digest: digest12(current.as_bytes()),
                current_raw: current
            }
        );
        assert_eq!(placement("brand-new", &registry, dir), Placement::Add);
    }

    #[test]
    fn similar_reports_only_close_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let near = "---\nname: regulator-attack\ndescription: \"Attack an idea as a hostile regulator would.\"\nstage: attack\nuse_when: \"Regulated markets.\"\n---\n{context}";
        store::write_owner_skill(tmp.path(), "regulator-attack", near).unwrap();
        let (registry, _) = SkillRegistry::load(tmp.path());
        let candidate =
            check_candidate(&finalize(&file("hostile-regulator"), "idea").unwrap()).unwrap();
        let hits = similar(&candidate, &registry);
        assert_eq!(
            hits.first().map(|s| s.name.as_str()),
            Some("regulator-attack")
        );
        assert!(hits.iter().all(|s| s.score_milli >= SIMILAR_MILLI));
        assert!(hits.len() <= SIMILAR_TOP);
        let same = check_candidate(near).unwrap();
        assert!(
            similar(&same, &registry)
                .iter()
                .all(|s| s.name != "regulator-attack"),
            "never itself"
        );
    }

    #[test]
    fn line_diff_marks_changes_and_rebuilds_both_sides() {
        let (old, new) = ("a\nb\nc\nd", "a\nc\nx\nd");
        let diff = line_diff(old, new);
        assert_eq!(
            diff,
            [
                DiffLine::Same("a".into()),
                DiffLine::Removed("b".into()),
                DiffLine::Same("c".into()),
                DiffLine::Added("x".into()),
                DiffLine::Same("d".into()),
            ]
        );
        let side = |keep_added: bool| -> Vec<String> {
            diff.iter()
                .filter_map(|l| match l {
                    DiffLine::Same(s) => Some(s.clone()),
                    DiffLine::Added(s) if keep_added => Some(s.clone()),
                    DiffLine::Removed(s) if !keep_added => Some(s.clone()),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(side(false).join("\n"), old);
        assert_eq!(side(true).join("\n"), new);
    }

    #[test]
    fn draft_body_round_trips() {
        let d = Draft {
            raw: finalize(&file("hostile-regulator"), "tutoring").unwrap(),
            evidence: vec![
                EvidenceLine {
                    quote: "assume a regulator hates it".into(),
                    grounded: true,
                    owner: true,
                },
                EvidenceLine {
                    quote: "first classify the app".into(),
                    grounded: true,
                    owner: false,
                },
                EvidenceLine {
                    quote: "made up words here".into(),
                    grounded: false,
                    owner: false,
                },
            ],
        };
        let body = render_draft_body(&d);
        assert!(body.starts_with("~~~skill\n---\n"), "{body}");
        assert!(body.contains("- ✓ owner \"assume a regulator hates it\""));
        assert!(body.contains("- ✗ \"made up words here\""));
        assert_eq!(parse_draft_body(&body), Some(d));
        assert_eq!(parse_draft_body("no fence here"), None);
    }

    #[test]
    fn save_check_refuses_stale_builtin_internal_and_invalid_but_not_ungrounded() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let registry = SkillRegistry::builtin();
        let add = finalize(&file("hostile-regulator"), "tutoring").unwrap();
        let (target, skill) = save_check(&add, &registry, dir, None).unwrap();
        assert_eq!(
            (target, skill.name.as_str()),
            (SaveTarget::Add, "hostile-regulator")
        );

        store::write_owner_skill(dir, "hostile-regulator", &add).unwrap();
        let edited = add.replace("Assume", "Suppose");
        assert_eq!(
            save_check(&edited, &registry, dir, None).unwrap_err(),
            SaveRefusal::Superseded
        );
        assert_eq!(
            save_check(&edited, &registry, dir, Some("stale")).unwrap_err(),
            SaveRefusal::Superseded
        );
        let digest = digest12(add.as_bytes());
        assert_eq!(
            save_check(&edited, &registry, dir, Some(&digest))
                .unwrap()
                .0,
            SaveTarget::Update
        );

        let builtin = add.replace("name: hostile-regulator", "name: premortem");
        assert_eq!(
            save_check(&builtin, &registry, dir, None).unwrap_err(),
            SaveRefusal::BuiltInName("premortem".into())
        );
        let internal = add.replace("name: hostile-regulator", "name: panel-score");
        assert_eq!(
            save_check(&internal, &registry, dir, None).unwrap_err(),
            SaveRefusal::InternalName("panel-score".into())
        );
        assert!(matches!(
            save_check("---\nname: x\n---\n", &registry, dir, None),
            Err(SaveRefusal::Invalid(_))
        ));
    }
}
