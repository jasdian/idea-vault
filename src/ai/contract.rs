//! Output contracts (docs/adr/0023): deterministic checks that a model's answer has the shape its
//! skill promised, plus the repair that strips the chatter local models wrap answers in.
//!
//! Pure functions, no I/O and no model calls. The evaluator-optimizer loop lives with the callers:
//! a single interactive skill call validates, and on a [`Violation`] asks the model ONCE more with
//! [`retry_note`] appended; a fan-out agent only repairs (a retry per agent would double the
//! fan-out's cost). Compaction uses the heading helpers warn-only.

use crate::ai::provenance::PromptTemplate;
use crate::domain::evidence::MIN_QUOTE_WORDS;
use crate::domain::frontmatter::parse_skill;
use crate::domain::{slug, OutputContract, SkillStage};

/// Why an answer failed its contract — phrased so it can be read back to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    Empty,
    NotBullets,
    NoNumberedList,
    NoFencedBlock,
    /// A sectioned answer lacked these `## ` headings (canonical spelling, in contract order).
    MissingSections(Vec<String>),
    /// A build plan carried its headings but the plan parser finds no task in it.
    NoUsablePlan,
    /// A Ground reader's answer held no line with a backticked `path:N` anchor.
    NoClaims,
    /// A Panel scorer's answer held no `C<i>: <0|1|2>` line.
    NoScores,
    /// A make-skill draft (docs/adr/0042) had no `~~~skill` block the skill loader would accept;
    /// the loader's own error is read back to the model.
    NotASkillFile(String),
    /// A make-skill draft had no `## Evidence` bullet carrying a quote of at least
    /// `MIN_QUOTE_WORDS` words.
    NoEvidence,
    /// The call ran out of room (docs/adr/0037): `output` when generation hit its length limit,
    /// so the answer's tail is missing; `input` when the prompt filled the window, so its head was
    /// dropped. Only an output truncation earns the retry — the same window would drop the same
    /// head again.
    Truncated {
        output: bool,
        input: bool,
    },
}

/// How one answer met its output contract (docs/adr/0023, recorded per ADR-0037 rather than only
/// logged): valid as returned, valid after repair, valid on the one retry, or kept although it
/// never met the contract, with the violation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", content = "violation", rename_all = "snake_case")]
pub enum ContractOutcome {
    Clean,
    Repaired,
    Retried,
    OffContract(String),
}

impl ContractOutcome {
    /// The outcome of a first answer that validated: `Clean` when validation kept it as the model
    /// wrote it, `Repaired` when it had to strip or reshape something.
    pub fn of_valid(raw: &str, validated: &str) -> Self {
        if raw.trim() == validated {
            ContractOutcome::Clean
        } else {
            ContractOutcome::Repaired
        }
    }

    pub fn is_clean(&self) -> bool {
        matches!(self, ContractOutcome::Clean)
    }

    /// `<lens>: off-contract: <violation>` for a kept answer that never met its contract,
    /// truncations included — the line an artifact's recipe carries (ADR-0040). Read from the
    /// outcome the call recorded, never re-derived from the kept text: a truncated answer is often
    /// shape-valid. `None` for every on-contract outcome.
    pub fn note(&self, lens: &str) -> Option<String> {
        match self {
            ContractOutcome::OffContract(violation) => {
                Some(format!("{lens}: off-contract: {violation}"))
            }
            _ => None,
        }
    }
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Violation::Empty => f.write_str("the answer was empty"),
            Violation::NotBullets => f.write_str(
                "the answer must be markdown bullet lines (\"- ...\"), or nothing at all if there is nothing to list",
            ),
            Violation::NoNumberedList => f.write_str(
                "the answer must be a numbered list (\"1. ...\", \"2. ...\"), most important first",
            ),
            Violation::NoFencedBlock => {
                f.write_str("the answer must be exactly one fenced ```markdown code block")
            }
            Violation::MissingSections(missing) => write!(
                f,
                "the answer must contain every section heading, and these were missing: {} \
                 (write each as its own `## ` heading, with `- none` under it when it is empty)",
                missing.join(", ")
            ),
            Violation::NoUsablePlan => f.write_str(
                "the plan needs at least one task under `## Plan`",
            ),
            Violation::NoClaims => f.write_str(
                "the answer must be claim lines of the form - `path:N` | `symbol` | claim, citing a real file and line",
            ),
            Violation::NoScores => f.write_str(
                "the answer must be one line per criterion of the form C1: 0|1|2 — reason",
            ),
            Violation::NotASkillFile(why) => write!(
                f,
                "the answer must hold one skill file between a line `~~~skill` and a line `~~~`, \
                 and that file was not valid: {why}"
            ),
            Violation::NoEvidence => f.write_str(
                "the answer must end with a `## Evidence` heading over bullets, each holding a \
                 double-quoted passage of at least three words copied from the discussion",
            ),
            // An input truncation is never read back to the model (it earns no retry), so its
            // wording is the short label the run journal and the off-contract badge show.
            Violation::Truncated { input: true, .. } => f.write_str("input truncated"),
            Violation::Truncated { input: false, .. } => f.write_str(
                "the answer was cut off at the output limit; answer more briefly so it ends where \
                 you mean it to",
            ),
        }
    }
}

/// The instruction appended to the original prompt for the one retry. The failed answer is not
/// resent — it would only spend the context budget on the mistake.
pub fn retry_note(violation: &Violation) -> String {
    RETRY_NOTE
        .text
        .replace("{violation}", &violation.to_string())
}

/// The retry note's wording: parse-coupled, so registered and pinned by a golden (ADR-0040).
pub const RETRY_NOTE: PromptTemplate = PromptTemplate {
    id: "retry-note",
    version: 1,
    text: "\n\nIMPORTANT — a previous answer to this request was rejected because {violation}. \
           Answer again, following the required format exactly, with no preamble.",
};

fn is_bullet(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("- ") || t.starts_with("* ") || t.starts_with("+ ")
}

fn is_numbered(line: &str) -> bool {
    let t = line.trim_start();
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && {
        let rest = &t[digits..];
        rest.starts_with(". ") || rest.starts_with(") ")
    }
}

/// The lines from the first line matching `is_item` through the last one, trimmed. Everything
/// before (a "Here are the causes:" preamble) and after (a "Let me know if…" sign-off) goes;
/// continuation lines between items stay.
fn item_block(text: &str, is_item: fn(&str) -> bool) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let first = lines.iter().position(|l| is_item(l))?;
    let last = lines.iter().rposition(|l| is_item(l))?;
    Some(lines[first..=last].join("\n").trim().to_string())
}

fn is_list_item(line: &str) -> bool {
    is_bullet(line) || is_numbered(line)
}

fn is_opening_fence(line: &str) -> bool {
    let t = line.trim().to_ascii_lowercase();
    t == "```markdown" || t == "```md" || t == "```"
}

/// The inner lines of the fenced block: the first opening fence line, closed by the LAST bare
/// ```` ``` ```` line after it — a build prompt routinely contains nested code fences, so the
/// first closing fence is usually the wrong one. `None` when there is no such block, or it is
/// empty.
fn fenced_inner(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let open = lines.iter().position(|l| is_opening_fence(l))?;
    let close = lines.iter().rposition(|l| l.trim() == "```")?;
    if close <= open {
        return None;
    }
    let inner = lines[open + 1..close].join("\n");
    (!inner.trim().is_empty()).then_some(inner)
}

/// The fenced block (see [`fenced_inner`]), its opening normalized to ```` ```markdown ````.
fn fenced_block(text: &str) -> Option<String> {
    fenced_inner(text).map(|inner| format!("```markdown\n{inner}\n```"))
}

/// The sections a [`OutputContract::BuildPlan`] answer must carry, canonical spelling, in order.
pub const BUILD_PLAN_SECTIONS: &[&str] = &[
    "## Goal",
    "## Settled",
    "## Verify first",
    "## Open questions",
    "## Plan",
    "## Kill criteria",
];

/// The sections of [`BUILD_PLAN_SECTIONS`] a build-plan answer is retried for. `## Verify first`
/// and `## Kill criteria` may be absent: the plan parser records and the finish step flags them.
pub const BUILD_PLAN_REQUIRED: &[&str] = &["## Goal", "## Settled", "## Open questions", "## Plan"];

/// How a line names a build-plan section.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HeadingForm {
    /// A `#` heading of any level.
    Hash,
    /// A whole-line bold label such as `**Goal**`.
    Bold,
}

/// The canonical build-plan section a heading-shaped line names, if any. Only a line that starts
/// in column 0 counts, so an indented or quoted heading stays body text. Numbering, a trailing
/// colon and case are ignored, and the synonyms a small model reaches for are folded in; bare
/// words that are common as body labels ("steps", "open", "verify") deliberately are not.
fn build_plan_heading(line: &str) -> Option<(&'static str, HeadingForm)> {
    let t = line.trim_end();
    let (label, form) = if let Some(rest) = t.strip_prefix('#') {
        (rest.trim_start_matches('#'), HeadingForm::Hash)
    } else if t.len() > 4 && t.starts_with("**") && t.ends_with("**") {
        (&t[2..t.len() - 2], HeadingForm::Bold)
    } else {
        return None;
    };
    let label = label
        .trim()
        .trim_end_matches(':')
        .trim()
        .trim_start_matches(|c: char| c.is_ascii_digit())
        .trim_start_matches(['.', ')'])
        .trim()
        .to_ascii_lowercase();
    let canonical = match label.as_str() {
        "goal" | "objective" | "goal and deliverable" | "goal / deliverable" => "## Goal",
        "settled" | "settled decisions" => "## Settled",
        "verify first" | "premises" | "bootstrap checks" | "assumptions to verify" => {
            "## Verify first"
        }
        "open questions" | "unresolved questions" => "## Open questions",
        "plan" | "tasks" | "ordered plan" | "task list" => "## Plan",
        "kill criteria" | "kill criterion" | "stop conditions" => "## Kill criteria",
        _ => return None,
    };
    Some((canonical, form))
}

/// The section headings of a build-plan answer, as (line index, canonical heading), skipping
/// anything inside a code fence. Bold labels count only when the answer has no `#` section
/// heading at all, so a bold sub-label inside a task never opens a section.
fn build_plan_headings(lines: &[&str]) -> Vec<(usize, &'static str)> {
    let mut in_fence = false;
    let mut found = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        } else if !in_fence {
            if let Some((canonical, form)) = build_plan_heading(line) {
                found.push((i, canonical, form));
            }
        }
    }
    let bold_only = found.iter().all(|(_, _, f)| *f == HeadingForm::Bold);
    found
        .into_iter()
        .filter(|(_, _, f)| bold_only || *f == HeadingForm::Hash)
        .map(|(i, c, _)| (i, c))
        .collect()
}

/// Repair a build-plan answer without altering any body line: unwrap it when the whole plan sits
/// in a fence that opens right before its first section, drop the preamble before that section
/// and a trailing prose sign-off paragraph after the last one, and rewrite each section heading
/// to its canonical spelling.
pub fn repair_build_plan(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if let Some(first) = lines.iter().position(|l| build_plan_heading(l).is_some()) {
        let opener = lines[..first]
            .iter()
            .rposition(|l| !l.trim().is_empty())
            .filter(|o| is_opening_fence(lines[*o]));
        if let Some(inner) = opener.and_then(|o| fenced_inner(&lines[o..].join("\n"))) {
            return repair_build_plan(&inner);
        }
    }
    let headings = build_plan_headings(&lines);
    let Some(&(start, _)) = headings.first() else {
        return text.to_string();
    };
    let mut end = lines.len();
    let last = headings.last().map_or(start, |(i, _)| *i);
    if let Some(blank) = lines[last + 1..end]
        .iter()
        .rposition(|l| l.trim().is_empty())
        .map(|b| b + last + 1)
    {
        let body_before = lines[last + 1..blank].iter().any(|l| !l.trim().is_empty());
        let tail = &lines[blank + 1..end];
        let sign_off = !tail.is_empty()
            && tail.iter().all(|l| {
                !is_list_item(l)
                    && !l.starts_with([' ', '\t'])
                    && !l.trim_start().starts_with("```")
            });
        if body_before && sign_off {
            end = blank;
        }
    }
    let mut next = headings.iter().peekable();
    let mut out: Vec<&str> = Vec::with_capacity(end - start);
    for (i, line) in lines.iter().enumerate().take(end).skip(start) {
        match next.peek() {
            Some(&&(at, canonical)) if at == i => {
                out.push(canonical);
                next.next();
            }
            _ => out.push(line),
        }
    }
    out.join("\n").trim().to_string()
}

/// Most claim lines a Ground reader's answer keeps, and most bullets a Panel proposal keeps
/// (ADR-0034): a small model's ninth line is rarely better than its first eight, and the stage
/// budgets assume the cap.
pub const MAX_STAGE_LINES: usize = 8;

/// The backticked spans of `line`, in order; an unclosed tick ends the scan.
pub fn backtick_spans(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(open) = rest.find('`') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('`') else {
            break;
        };
        out.push(&after[..close]);
        rest = &after[close + 1..];
    }
    out
}

/// `path:N` or `path:N-M` as (path, first, last) — the anchor grammar a Ground reader and the
/// build-plan G4 gate share. The path is anything non-empty without whitespace; the range must
/// run forwards.
pub fn parse_anchor(span: &str) -> Option<(&str, usize, usize)> {
    let (path, range) = span.trim().rsplit_once(':')?;
    if path.is_empty() || path.contains(char::is_whitespace) {
        return None;
    }
    let (a, b) = range.split_once('-').unwrap_or((range, range));
    let (first, last): (usize, usize) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
    (first <= last).then_some((path, first, last))
}

/// True when `line` carries a backticked anchor — the one shape a Ground claim must have.
fn is_claim_line(line: &str) -> bool {
    backtick_spans(line)
        .iter()
        .any(|s| parse_anchor(s).is_some())
}

/// One `C<i>: <score> — reason` line as (criterion number, score, reason). Case-insensitive,
/// tolerant of list markers and emphasis; a score outside 0..=2 is no score.
pub fn score_line(line: &str) -> Option<(usize, u8, &str)> {
    let t = line
        .trim()
        .trim_start_matches(['-', '*', ' '])
        .trim_start_matches("**");
    let rest = t.strip_prefix(['C', 'c'])?;
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    let id: usize = rest[..digits].parse().ok()?;
    let after = rest[digits..].trim_start_matches(['*', ':', '.', ')', ' ', '=']);
    let score = after.chars().next()?.to_digit(10)?;
    if score > 2 || after[1..].starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let reason = after[1..]
        .trim_start_matches(['*', ' ', ':', '-', '—', '–', '/'])
        .trim();
    Some((id, score as u8, reason))
}

/// Check `raw` against `contract`, returning the repaired answer (chatter stripped, shape
/// normalized) or why it cannot be repaired. `BulletsOrEmpty` accepts an empty answer.
pub fn validate(contract: OutputContract, raw: &str) -> Result<String, Violation> {
    let text = raw.trim();
    match contract {
        OutputContract::Free => {
            if text.is_empty() {
                Err(Violation::Empty)
            } else {
                Ok(text.to_string())
            }
        }
        OutputContract::BulletsOrEmpty => {
            if text.is_empty() {
                Ok(String::new())
            } else {
                item_block(text, is_bullet).ok_or(Violation::NotBullets)
            }
        }
        OutputContract::RankedList => {
            if text.is_empty() {
                return Err(Violation::Empty);
            }
            // Only the preamble goes: a ranked list may close with a verdict line on purpose.
            let lines: Vec<&str> = text.lines().collect();
            let first = lines
                .iter()
                .position(|l| is_numbered(l))
                .ok_or(Violation::NoNumberedList)?;
            Ok(lines[first..].join("\n").trim().to_string())
        }
        OutputContract::FencedMarkdown => {
            if text.is_empty() {
                Err(Violation::Empty)
            } else {
                fenced_block(text).ok_or(Violation::NoFencedBlock)
            }
        }
        OutputContract::BuildPlan => {
            if text.is_empty() {
                return Err(Violation::Empty);
            }
            let plan = repair_build_plan(text);
            let missing: Vec<String> = BUILD_PLAN_REQUIRED
                .iter()
                .filter(|h| !plan.lines().any(|l| l == **h))
                .map(|h| (*h).to_string())
                .collect();
            if !missing.is_empty() {
                Err(Violation::MissingSections(missing))
            } else {
                Ok(plan)
            }
        }
        OutputContract::GroundClaims => {
            if text.is_empty() {
                return Err(Violation::Empty);
            }
            let claims: Vec<&str> = text
                .lines()
                .filter(|l| is_claim_line(l))
                .map(str::trim)
                .take(MAX_STAGE_LINES)
                .collect();
            if claims.is_empty() {
                Err(Violation::NoClaims)
            } else {
                Ok(claims.join("\n"))
            }
        }
        OutputContract::Proposal => {
            if text.is_empty() {
                return Err(Violation::Empty);
            }
            // The heading is code-owned: a proposal is its bullets, whatever heading the model
            // wrote (or forgot) above them.
            let block = item_block(text, is_bullet).ok_or(Violation::NotBullets)?;
            let bullets: Vec<String> = items(&block)
                .into_iter()
                .take(MAX_STAGE_LINES)
                .map(|b| format!("- {b}"))
                .collect();
            Ok(format!("## Proposal\n{}", bullets.join("\n")))
        }
        OutputContract::Scorecard => {
            if text.is_empty() {
                return Err(Violation::Empty);
            }
            let scored: Vec<&str> = text
                .lines()
                .filter(|l| score_line(l).is_some())
                .map(str::trim)
                .collect();
            if scored.is_empty() {
                Err(Violation::NoScores)
            } else {
                Ok(scored.join("\n"))
            }
        }
        OutputContract::SkillDraft => {
            if text.is_empty() {
                return Err(Violation::Empty);
            }
            let (file, evidence) = split_skill_draft(text)?;
            check_drafted_skill(&file).map_err(Violation::NotASkillFile)?;
            let quotes = evidence_quotes(&evidence);
            if quotes.is_empty() {
                return Err(Violation::NoEvidence);
            }
            let bullets: Vec<String> = quotes
                .iter()
                .take(MAX_STAGE_LINES)
                .map(|q| format!("- \"{q}\""))
                .collect();
            Ok(format!(
                "~~~skill\n{file}\n~~~\n\n{EVIDENCE_HEADING}\n{}",
                bullets.join("\n")
            ))
        }
    }
}

/// The heading a make-skill draft's evidence list sits under (docs/adr/0042).
pub const EVIDENCE_HEADING: &str = "## Evidence";

/// The contracts a drafted skill may promise: the owner-facing shapes only. The engine-only ones
/// are parsed by a workflow stage's code, and `skill_draft` would make the draft a distiller.
const DRAFTABLE_CONTRACTS: [OutputContract; 4] = [
    OutputContract::Free,
    OutputContract::BulletsOrEmpty,
    OutputContract::RankedList,
    OutputContract::FencedMarkdown,
];

/// Split a make-skill answer into the skill file inside its `~~~skill` fence and the text after
/// the `## Evidence` heading. The fence closes at the last bare `~~~` line before that heading,
/// so a stray tilde line inside the drafted prompt cannot cut it short.
pub fn split_skill_draft(text: &str) -> Result<(String, String), Violation> {
    let lines: Vec<&str> = text.lines().collect();
    let is_open = |l: &str| {
        l.trim()
            .strip_prefix("~~~")
            .is_some_and(|rest| rest.trim().eq_ignore_ascii_case("skill"))
    };
    let Some(open) = lines.iter().position(|l| is_open(l)) else {
        return Err(Violation::NotASkillFile(
            "there is no line `~~~skill` opening the file (a backtick fence does not count)".into(),
        ));
    };
    let heading = lines
        .iter()
        .skip(open + 1)
        .position(|l| is_evidence_heading(l))
        .map(|i| i + open + 1);
    let end = heading.unwrap_or(lines.len());
    let Some(close) = lines[open + 1..end]
        .iter()
        .rposition(|l| l.trim() == "~~~")
        .map(|i| i + open + 1)
    else {
        return Err(Violation::NotASkillFile(
            "there is no line `~~~` closing the file".into(),
        ));
    };
    let file = lines[open + 1..close].join("\n").trim().to_string();
    let evidence = heading
        .map(|h| lines[h + 1..].join("\n"))
        .unwrap_or_default();
    Ok((file, evidence))
}

fn is_evidence_heading(line: &str) -> bool {
    let t = line.trim().trim_end_matches(':');
    t.eq_ignore_ascii_case(EVIDENCE_HEADING)
}

/// The skill-loader rules a drafted file must already meet, minus the `{context}` slot, which
/// code places (docs/adr/0042): it parses with no unknown key, has a slug name, a prompt, an
/// owner-facing stage and contract, is not hidden and carries no `origin` (code sets it).
fn check_drafted_skill(file: &str) -> Result<(), String> {
    let (fm, prompt) = parse_skill(file).map_err(|e| e.to_string())?;
    if !slug::is_valid(&fm.name) {
        return Err(format!(
            "name {:?} must use only lowercase letters, digits and '-'",
            fm.name
        ));
    }
    if fm.stage == SkillStage::Extract {
        return Err("stage must be steelman, attack, consequence, converge or capstone".into());
    }
    if !DRAFTABLE_CONTRACTS.contains(&fm.contract) {
        return Err(
            "contract must be free, bullets_or_empty, ranked_list or fenced_markdown".into(),
        );
    }
    if fm.hidden {
        return Err("a drafted skill must not be hidden".into());
    }
    if fm.origin.is_some() {
        return Err("leave out origin; it is filled in automatically".into());
    }
    if prompt.replace("{context}", "").trim().is_empty() {
        return Err("the prompt under the frontmatter is empty".into());
    }
    Ok(())
}

/// The quoted passage of each bullet in a make-skill `## Evidence` list, in order: the text
/// between the first opening quote (straight or curly) and the last closing one, kept only when
/// it holds at least [`MIN_QUOTE_WORDS`] words. Shared by the contract and by
/// `concepts::make_skill`, which grounds each quote against the discussion.
pub fn evidence_quotes(evidence_md: &str) -> Vec<String> {
    evidence_md
        .lines()
        .filter(|l| is_bullet(l))
        .filter_map(|l| {
            let open = l.find(['"', '“'])?;
            let after = &l[open + l[open..].chars().next()?.len_utf8()..];
            let close = after.rfind(['"', '”'])?;
            let quote = after[..close].trim();
            (quote.split_whitespace().count() >= MIN_QUOTE_WORDS).then(|| quote.to_string())
        })
        .collect()
}

/// The canonical verdict line for `raw` checked against `contract` (docs/adr/0038): `pass=1` with
/// the repaired answer's item and line counts, or `pass=0` with the violation. The violation is
/// its variant, not its prose, so rewording a retry note is not a flip.
pub fn summarize_contract(contract: OutputContract, raw: &str) -> String {
    match validate(contract, raw) {
        Ok(repaired) => format!(
            "pass=1 items={} lines={}",
            items(&repaired).len(),
            repaired.lines().count()
        ),
        Err(violation) => format!(
            "pass=0 violation={}",
            format!("{violation:?}").replace([' ', '"'], "")
        ),
    }
}

/// The list items of a bullets or numbered answer, markers stripped — how an orchestrator splits
/// one agent's answer into separately auditable findings. Text with no list becomes one item.
pub fn items(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut in_list = false;
    for line in text.lines() {
        let t = line.trim();
        if is_bullet(line) || is_numbered(line) {
            let body = if is_bullet(line) {
                t[2..].trim()
            } else {
                let digits = t.chars().take_while(char::is_ascii_digit).count();
                t[digits + 2..].trim()
            };
            out.push(body.to_string());
            in_list = true;
        } else if in_list && !t.is_empty() && line.starts_with([' ', '\t']) {
            if let Some(last) = out.last_mut() {
                last.push(' ');
                last.push_str(t);
            }
        } else if !t.is_empty() {
            in_list = false;
        }
    }
    if out.is_empty() && !text.trim().is_empty() {
        out.push(text.trim().to_string());
    }
    out.retain(|i| !i.is_empty());
    out
}

/// The `## ` headings from `required` that `text` does not carry as a line of its own.
pub fn missing_headings<'a>(text: &str, required: &[&'a str]) -> Vec<&'a str> {
    required
        .iter()
        .filter(|h| !text.lines().any(|l| l.trim() == **h))
        .copied()
        .collect()
}

/// Shrink `text` to at most `max` bytes without ever cutting a `## ` heading: body lines are
/// dropped from the end of whichever section is currently largest until it fits, and a section's
/// last remaining line is shortened (on a char boundary) rather than dropped. Only if the headings
/// alone exceed `max` does it fall back to a plain char-boundary cut.
pub fn trim_sections(text: &str, max: usize) -> String {
    fn cut(s: &str, max: usize) -> &str {
        let mut end = max.min(s.len());
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].trim_end()
    }
    if text.len() <= max {
        return text.to_string();
    }
    let mut sections: Vec<(Option<String>, Vec<String>)> = vec![(None, Vec::new())];
    for line in text.lines() {
        if line.starts_with("## ") {
            sections.push((Some(line.to_string()), Vec::new()));
        } else if let Some(last) = sections.last_mut() {
            last.1.push(line.to_string());
        }
    }
    let render = |sections: &[(Option<String>, Vec<String>)]| -> String {
        let mut out: Vec<&str> = Vec::new();
        for (heading, body) in sections {
            if let Some(h) = heading {
                out.push(h);
            }
            out.extend(body.iter().map(String::as_str));
        }
        out.join("\n").trim().to_string()
    };
    loop {
        let rendered = render(&sections);
        if rendered.len() <= max {
            return rendered;
        }
        let overflow = rendered.len() - max;
        let largest = sections
            .iter_mut()
            .filter(|(_, body)| body.iter().any(|l| !l.is_empty()))
            .max_by_key(|(_, body)| body.iter().map(|l| l.len() + 1).sum::<usize>());
        let Some((_, body)) = largest else {
            return cut(&rendered, max).to_string();
        };
        let non_empty = body.iter().filter(|l| !l.is_empty()).count();
        if non_empty > 1 {
            body.pop();
        } else if let Some(line) = body.iter_mut().rev().find(|l| !l.is_empty()) {
            let keep = line.len().saturating_sub(overflow);
            *line = cut(line, keep).to_string();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_accepts_any_text_and_rejects_empty() {
        assert_eq!(validate(OutputContract::Free, "  hi \n").unwrap(), "hi");
        assert_eq!(validate(OutputContract::Free, " \n"), Err(Violation::Empty));
    }

    #[test]
    fn bullets_or_empty_strips_preamble_and_signoff_and_accepts_nothing() {
        let raw = "Here are the decisions:\n\n- Ship v1 solo\n  because time\n- Drop B2B\n\nHope this helps!";
        assert_eq!(
            validate(OutputContract::BulletsOrEmpty, raw).unwrap(),
            "- Ship v1 solo\n  because time\n- Drop B2B"
        );
        assert_eq!(validate(OutputContract::BulletsOrEmpty, "").unwrap(), "");
        assert_eq!(
            validate(OutputContract::BulletsOrEmpty, "No decisions were settled."),
            Err(Violation::NotBullets)
        );
    }

    #[test]
    fn ranked_list_strips_only_the_preamble() {
        let raw = "Sure! Causes:\n1. **Nobody pays** — x\n2) Churn — y\nMost dangerous: 1.";
        assert_eq!(
            validate(OutputContract::RankedList, raw).unwrap(),
            "1. **Nobody pays** — x\n2) Churn — y\nMost dangerous: 1."
        );
        assert_eq!(
            validate(OutputContract::RankedList, "- a bullet, not a rank"),
            Err(Violation::NoNumberedList)
        );
        assert_eq!(
            validate(OutputContract::RankedList, "2026 was a year."),
            Err(Violation::NoNumberedList),
            "a leading number without a list marker is not an item"
        );
    }

    #[test]
    fn fenced_markdown_pairs_the_opening_with_the_last_bare_fence() {
        let raw =
            "Here you go:\n```markdown\n# Build it\n```bash\ncargo test\n```\nDone.\n```\nThanks!";
        assert_eq!(
            validate(OutputContract::FencedMarkdown, raw).unwrap(),
            "```markdown\n# Build it\n```bash\ncargo test\n```\nDone.\n```"
        );
        assert_eq!(
            validate(OutputContract::FencedMarkdown, "```\nplain fence\n```").unwrap(),
            "```markdown\nplain fence\n```"
        );
        assert_eq!(
            validate(OutputContract::FencedMarkdown, "no fence at all"),
            Err(Violation::NoFencedBlock)
        );
        assert_eq!(
            validate(OutputContract::FencedMarkdown, "```markdown\n```"),
            Err(Violation::NoFencedBlock)
        );
    }

    const PLAN: &str = "## Goal\nShip it.\n\n## Settled\n- S1: x\n\n## Verify first\n- P1: y\n\n## Open questions\n- none\n\n## Plan\n- T1: do it\n\n## Kill criteria\n- none";

    #[test]
    fn build_plan_accepts_a_complete_plan_unchanged() {
        assert_eq!(validate(OutputContract::BuildPlan, PLAN).unwrap(), PLAN);
    }

    #[test]
    fn build_plan_strips_preamble_and_unwraps_a_fence() {
        let raw = format!("Sure! Here is the plan:\n\n```markdown\n{PLAN}\n```\nGood luck.");
        assert_eq!(validate(OutputContract::BuildPlan, &raw).unwrap(), PLAN);
        let bare = format!("Here you go.\n{PLAN}");
        assert_eq!(validate(OutputContract::BuildPlan, &bare).unwrap(), PLAN);
    }

    #[test]
    fn build_plan_normalizes_heading_aliases() {
        let raw = "# Objective\nShip it.\n### Settled decisions:\n- S1: x\n#### Premises\n- P1: y\n## Open Questions\n- none\n## 5. Tasks\n- T1: do it\n## Kill criterion\n- none";
        let out = validate(OutputContract::BuildPlan, raw).unwrap();
        for h in BUILD_PLAN_SECTIONS {
            assert!(out.lines().any(|l| l == *h), "missing {h} in:\n{out}");
        }
        assert!(out.contains("- T1: do it"), "bodies survive: {out}");
    }

    const PLAN_WITH_CODE: &str = "## Goal\nShip it.\n\n## Settled\n- S1: x\n\n## Verify first\n- P1: y\n\n## Open questions\n- none\n\n## Plan\n- T1: run it\n  **Steps**\n  ```sh\n  # steps\n  ls\n  ```\n  ### Verify\n\n## Kill criteria\n- none\n```sh\nexit 1\n```";

    #[test]
    fn build_plan_never_alters_a_body_line() {
        for raw in [
            PLAN_WITH_CODE.to_string(),
            format!("Here you go:\n```markdown\n{PLAN_WITH_CODE}\n```\nGood luck."),
            format!("Intro with a snippet:\n```\nnot the plan\n```\nNow the plan.\n```markdown\n{PLAN_WITH_CODE}\n```"),
        ] {
            assert_eq!(
                validate(OutputContract::BuildPlan, &raw).unwrap(),
                PLAN_WITH_CODE,
                "from:\n{raw}"
            );
        }
    }

    #[test]
    fn build_plan_rewrites_only_section_headings_outside_fences() {
        let raw = "## Goal\nx\n## Settled\n- a\n## Verify first\n- b\n## Open questions\n- c\n## Plan\n**Tasks**\n- T1: x\n```md\n## Plan\n```\n## Kill criteria\n- d";
        let out = validate(OutputContract::BuildPlan, raw).unwrap();
        assert_eq!(
            out, raw,
            "a bold label under # headings and a fenced heading stay body text"
        );
    }

    #[test]
    fn build_plan_accepts_bold_labels_when_the_model_uses_no_hash_headings() {
        let raw = "**Goal**\nx\n**Settled decisions:**\n- a\n**Verify first**\n- b\n**Open questions**\n- c\n**Tasks**\n- T1\n**Kill criteria**\n- none";
        let out = validate(OutputContract::BuildPlan, raw).unwrap();
        assert_eq!(
            out,
            "## Goal\nx\n## Settled\n- a\n## Verify first\n- b\n## Open questions\n- c\n## Plan\n- T1\n## Kill criteria\n- none"
        );
    }

    #[test]
    fn build_plan_drops_a_trailing_sign_off_but_keeps_a_prose_body() {
        let raw = format!("{PLAN}\n\nCheers! Let me know if you need more.");
        assert_eq!(validate(OutputContract::BuildPlan, &raw).unwrap(), PLAN);
        let prose = "## Goal\nShip it.\n## Settled\n- a\n## Verify first\n- b\n## Open questions\n- c\n## Plan\n- T1\n## Kill criteria\nNone for this plan.";
        assert_eq!(validate(OutputContract::BuildPlan, prose).unwrap(), prose);
    }

    #[test]
    fn build_plan_does_not_invent_a_section_from_a_body_label() {
        let raw = "## Goal\nx\n## Settled\n- a\n## Plan\n- T1\n  **Open questions**\n## Kill criteria\n- none";
        assert_eq!(
            validate(OutputContract::BuildPlan, raw),
            Err(Violation::MissingSections(vec!["## Open questions".into()]))
        );
    }

    #[test]
    fn build_plan_names_every_missing_section() {
        let raw = "## Goal\nShip it.\n## Plan\n- T1: do it";
        assert_eq!(
            validate(OutputContract::BuildPlan, raw),
            Err(Violation::MissingSections(vec![
                "## Settled".into(),
                "## Open questions".into(),
            ]))
        );
        assert_eq!(
            validate(OutputContract::BuildPlan, "  "),
            Err(Violation::Empty)
        );
        let note = retry_note(&Violation::MissingSections(vec!["## Plan".into()]));
        assert!(note.contains("## Plan"), "{note}");
    }

    #[test]
    fn build_plan_accepts_a_plan_missing_only_optional_sections() {
        let raw = "## Goal\nShip it.\n## Settled\n- S1: x\n## Open questions\n- none\n## Plan\n- T1: do it";
        assert_eq!(validate(OutputContract::BuildPlan, raw).unwrap(), raw);
    }

    #[test]
    fn build_plan_required_sections_are_a_subset_of_the_canonical_ones() {
        assert!(BUILD_PLAN_REQUIRED
            .iter()
            .all(|h| BUILD_PLAN_SECTIONS.contains(h)));
    }

    #[test]
    fn ground_claims_keep_only_anchored_lines_up_to_the_cap() {
        let raw = "Here is what I found:\n- `src/a.rs:10-12` | `run` | runs it\n- no anchor here\n- `/abs/b.rs:3` `Thing` defined here";
        assert_eq!(
            validate(OutputContract::GroundClaims, raw).unwrap(),
            "- `src/a.rs:10-12` | `run` | runs it\n- `/abs/b.rs:3` `Thing` defined here"
        );
        let many: String = (1..=12)
            .map(|i| format!("- `a.rs:{i}` | `x` | c\n"))
            .collect();
        let kept = validate(OutputContract::GroundClaims, &many).unwrap();
        assert_eq!(kept.lines().count(), MAX_STAGE_LINES);
        assert_eq!(
            validate(OutputContract::GroundClaims, "- nothing to cite"),
            Err(Violation::NoClaims)
        );
        assert_eq!(parse_anchor("a.rs:9-3"), None, "a range runs forwards");
    }

    #[test]
    fn proposal_is_a_code_owned_heading_over_at_most_eight_bullets() {
        let raw = "Sure!\n# My proposal\n- one\n  more\n- two\nThanks";
        assert_eq!(
            validate(OutputContract::Proposal, raw).unwrap(),
            "## Proposal\n- one more\n- two"
        );
        let many: String = (1..=10).map(|i| format!("- b{i}\n")).collect();
        let kept = validate(OutputContract::Proposal, &many).unwrap();
        assert_eq!(kept.lines().count(), 1 + MAX_STAGE_LINES);
        assert_eq!(
            validate(OutputContract::Proposal, "just prose"),
            Err(Violation::NotBullets)
        );
    }

    #[test]
    fn scorecard_keeps_score_lines_and_rejects_out_of_range_scores() {
        assert_eq!(score_line("C2: 1 — fine"), Some((2, 1, "fine")));
        assert_eq!(score_line("- **C1**: 2 - strong"), Some((1, 2, "strong")));
        assert_eq!(score_line("C3: 3 — too high"), None);
        assert_eq!(score_line("C3: 10"), None);
        assert_eq!(
            validate(OutputContract::Scorecard, "Scores:\nC1: 2 — a\nC2: 0 — b").unwrap(),
            "C1: 2 — a\nC2: 0 — b"
        );
        assert_eq!(
            validate(OutputContract::Scorecard, "all good"),
            Err(Violation::NoScores)
        );
    }

    const DRAFT_FILE: &str = "---\nname: hostile-regulator\ndescription: \"Attack an idea as a regulator who wants it dead.\"\nstage: attack\nrole: critic\ncontract: ranked_list\n---\n\nAssume a regulator hates the idea below.\n```sh\nnot a fence of ours\n```";

    fn draft(file: &str, evidence: &str) -> String {
        format!("Here is the draft:\n\n~~~skill\n{file}\n~~~\n\n## Evidence\n{evidence}\n\nHope it helps!")
    }

    #[test]
    fn skill_draft_accepts_a_tilde_fenced_file_with_quoted_evidence() {
        let raw = draft(
            DRAFT_FILE,
            "- \"now assume a regulator hates it\" — the owner's move\n- “walk every hostile rule like that”\n- no quote here",
        );
        assert_eq!(
            validate(OutputContract::SkillDraft, &raw).unwrap(),
            format!(
                "~~~skill\n{DRAFT_FILE}\n~~~\n\n## Evidence\n- \"now assume a regulator hates it\"\n- \"walk every hostile rule like that\""
            )
        );
    }

    #[test]
    fn skill_draft_without_quoted_evidence_is_no_evidence() {
        let no_heading = format!("~~~skill\n{DRAFT_FILE}\n~~~\n");
        assert_eq!(
            validate(OutputContract::SkillDraft, &no_heading),
            Err(Violation::NoEvidence)
        );
        let short = draft(DRAFT_FILE, "- \"too short\"\n- unquoted words only here");
        assert_eq!(
            validate(OutputContract::SkillDraft, &short),
            Err(Violation::NoEvidence)
        );
    }

    #[test]
    fn skill_draft_rejects_what_the_skill_loader_or_the_engine_would() {
        let ev = "- \"now assume a regulator hates it\"";
        let unknown = DRAFT_FILE.replace("role: critic", "role: critic\nmood: grim");
        let engine = DRAFT_FILE.replace("contract: ranked_list", "contract: scorecard");
        let extract = DRAFT_FILE.replace("stage: attack", "stage: extract");
        let hidden = DRAFT_FILE.replace("role: critic", "role: critic\nhidden: true");
        let origin = DRAFT_FILE.replace("role: critic", "role: critic\norigin: other-idea");
        let bad_name = DRAFT_FILE.replace("hostile-regulator", "Hostile Regulator");
        let empty = DRAFT_FILE.split("\n---\n").next().unwrap().to_string() + "\n---\n";
        for file in [unknown, engine, extract, hidden, origin, bad_name, empty] {
            assert!(
                matches!(
                    validate(OutputContract::SkillDraft, &draft(&file, ev)),
                    Err(Violation::NotASkillFile(_))
                ),
                "accepted:\n{file}"
            );
        }
        let backticks = format!("```skill\n{DRAFT_FILE}\n```\n\n## Evidence\n{ev}");
        assert!(matches!(
            validate(OutputContract::SkillDraft, &backticks),
            Err(Violation::NotASkillFile(_))
        ));
        assert_eq!(
            validate(OutputContract::SkillDraft, " "),
            Err(Violation::Empty)
        );
        let note = retry_note(&Violation::NotASkillFile("unknown field `mood`".into()));
        assert!(
            note.contains("unknown field `mood`") && note.contains("~~~skill"),
            "{note}"
        );
    }

    #[test]
    fn evidence_quotes_reads_straight_and_curly_quotes_per_bullet() {
        assert_eq!(
            evidence_quotes("- \"one two three\" (owner)\n* “four five six”\n- none\n- \"a b\""),
            ["one two three", "four five six"].map(String::from)
        );
    }

    #[test]
    fn retry_note_names_the_violation() {
        assert!(retry_note(&Violation::NoNumberedList).contains("numbered list"));
    }

    #[test]
    fn items_splits_bullets_and_numbers_and_joins_continuations() {
        let text = "Intro line\n1. First\n   more of first\n2. Second\n- third\n\nOutro";
        assert_eq!(
            items(text),
            ["First more of first", "Second", "third"].map(String::from)
        );
        assert_eq!(items("just prose"), ["just prose".to_string()]);
        assert!(items("  ").is_empty());
    }

    #[test]
    fn missing_headings_lists_only_absent_ones() {
        let text = "## Decisions\n- a\n## Open threads\n- b";
        assert_eq!(
            missing_headings(text, &["## Decisions", "## Rejected forks"]),
            ["## Rejected forks"]
        );
    }

    #[test]
    fn trim_sections_keeps_every_heading() {
        let text = "## Decisions\n- d1\n- d2 long long long long\n## Open threads\n- o1\n## Rejected forks\n- r1\n## Key facts & constraints\n- k1";
        let trimmed = trim_sections(text, 90);
        assert!(trimmed.len() <= 90, "{} bytes", trimmed.len());
        for h in [
            "## Decisions",
            "## Open threads",
            "## Rejected forks",
            "## Key facts & constraints",
        ] {
            assert!(trimmed.contains(h), "lost {h}: {trimmed}");
        }
        assert_eq!(trim_sections("short", 90), "short");
    }

    #[test]
    fn trim_sections_shortens_a_lone_line_on_a_char_boundary() {
        let out = trim_sections("héllo wörld", 3);
        assert!("héllo wörld".starts_with(&out) && !out.is_empty() && out.len() <= 3);
        let out = trim_sections("## Decisions\n- héllo wörld", 16);
        assert!(out.starts_with("## Decisions") && out.len() <= 16, "{out}");
    }
}
