//! Factored audit (docs/adr/0023): after a swarm or workflow fans out, one Auditor call judges
//! each finding against the idea, its memory and the discussion — `CONFIRMED`, `UNCERTAIN` or
//! `REFUTED` — before the synthesizer converges them.
//!
//! "Factored" because the auditor sees only the numbered findings, never the critics' framing or
//! each other's reasoning, and it is told to prefer `UNCERTAIN` over `CONFIRMED` when in doubt.
//! A refuted finding is downgraded, never dropped: the synthesis carries it in a code-appended
//! "Disproven objections" section. A near-uniform pass is itself flagged, since an auditor that
//! confirms everything has probably audited nothing. Parsing is deterministic and degrades: a
//! garbled or failed audit leaves every finding `UNCERTAIN` rather than aborting the run.

use std::path::Path;

use tokio::sync::Semaphore;

use crate::ai::budget::ContextBudget;
use crate::ai::contract;
use crate::ai::provenance::PromptTemplate;
use crate::ai::LlmBackend;
use crate::concepts::agents::{run_agent, AgentResult, AgentRole, AgentTask};
use crate::concepts::skills::{hydrate_context, SkillRegistry};
use crate::concepts::ConceptError;

/// Most findings one swarm/workflow run carries into audit and synthesis. Items are taken
/// round-robin across lenses, so every lens keeps its top-ranked findings under the cap.
pub const MAX_AUDIT_FINDINGS: usize = 20;

/// Share of findings confirmed above which a pass counts as suspiciously uniform.
const UNIFORM_PASS_PERCENT: usize = 90;

/// Below this many findings a uniform pass is unremarkable.
const UNIFORM_PASS_MIN_FINDINGS: usize = 4;

/// How a consumer of audited findings must treat each verdict; shared by the swarm synthesizer
/// and chained workflow steps.
pub const VERDICT_GUIDANCE: &str = "Each finding carries an auditor's verdict. Build the position \
on CONFIRMED findings, present UNCERTAIN ones as open questions, and do not build on REFUTED \
ones.";

const AUDIT_INSTRUCTION: &str = "Below are numbered findings other agents produced about an \
idea, then the idea itself, its memory, and the discussion so far. Judge each finding ONLY \
against that material and plain reasoning:\n\
- CONFIRMED — it holds up: it follows from the idea as stated, or the discussion supports it.\n\
- REFUTED — it is wrong about the idea, contradicts what the discussion established, or raises \
an objection the discussion already answered (say where).\n\
- UNCERTAIN — it may be true, but nothing here settles it (say what would).\n\
When in doubt choose UNCERTAIN, never CONFIRMED. Reply with exactly one line per finding, in \
order, and nothing else:\n\
F1: CONFIRMED|UNCERTAIN|REFUTED — <one-sentence reason>";

/// The audit instruction as a registered, golden-pinned template (ADR-0040): `parse_audit`
/// reads the line shape it asks for.
pub const AUDIT_TEMPLATE: PromptTemplate = PromptTemplate {
    id: "audit",
    version: 1,
    text: AUDIT_INSTRUCTION,
};

/// The one targeted re-ask after a malformed or partial audit (ADR-0023 amendment): appended to
/// the same prompt, naming only the findings still without a verdict. `{ids}` is the list.
pub const REASK_TEMPLATE: PromptTemplate = PromptTemplate {
    id: "audit-reask",
    version: 1,
    text: "\n\nYour previous answer gave no usable verdict for {ids}. Reply with exactly one \
line for each of those findings and nothing else:\n\
F<n>: CONFIRMED|UNCERTAIN|REFUTED — <one-sentence reason>",
};

/// One atomic finding: a list item from an agent's answer, tagged with every lens that produced
/// it (near-duplicates from different lenses merge into one finding).
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub lenses: Vec<String>,
    pub role: AgentRole,
    pub text: String,
}

impl Finding {
    /// `premortem · critic` — the provenance shown to the synthesizer and in the appendix.
    pub fn provenance(&self) -> String {
        if self.lenses.is_empty() {
            self.role.as_str().to_string()
        } else {
            format!("{} · {}", self.lenses.join(" + "), self.role.as_str())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Label {
    Confirmed,
    Uncertain,
    Refuted,
}

impl Label {
    pub fn as_str(self) -> &'static str {
        match self {
            Label::Confirmed => "CONFIRMED",
            Label::Uncertain => "UNCERTAIN",
            Label::Refuted => "REFUTED",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub label: Label,
    pub reason: String,
}

/// The auditor's verdicts, index-aligned with the findings it was given.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditReport {
    pub verdicts: Vec<Verdict>,
    /// How many findings the auditor actually answered for (the rest default to `UNCERTAIN`).
    pub answered: usize,
    /// The auditor's answer was missing or unusable — every verdict is a default.
    pub failed: bool,
}

impl AuditReport {
    fn count(&self, label: Label) -> usize {
        self.verdicts.iter().filter(|v| v.label == label).count()
    }

    /// More than 90% of a reasonable number of findings confirmed: worth distrusting.
    pub fn uniform_pass(&self) -> bool {
        let n = self.verdicts.len();
        n >= UNIFORM_PASS_MIN_FINDINGS
            && self.count(Label::Confirmed) * 100 > n * UNIFORM_PASS_PERCENT
    }

    /// Every finding `UNCERTAIN` — what a failed or unparseable audit degrades to.
    fn all_uncertain(n: usize, reason: &str) -> Self {
        Self {
            verdicts: vec![
                Verdict {
                    label: Label::Uncertain,
                    reason: reason.to_string(),
                };
                n
            ],
            answered: 0,
            failed: true,
        }
    }
}

/// Lowercased alphanumeric words — the key near-duplicate detection compares. Shared with the
/// workflow Loop stage's novelty test (docs/adr/0034).
pub(crate) fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Two findings say the same thing when their word sets overlap by at least 80% (Jaccard).
pub(crate) fn near_duplicate(a: &[String], b: &[String]) -> bool {
    if a.is_empty() || b.is_empty() {
        return a.is_empty() && b.is_empty();
    }
    let a: std::collections::HashSet<&String> = a.iter().collect();
    let b: std::collections::HashSet<&String> = b.iter().collect();
    let shared = a.intersection(&b).count();
    let union = a.union(&b).count();
    shared * 5 >= union * 4
}

/// Split each agent's answer into atomic findings (its list items), interleave them round-robin
/// across agents so every lens keeps its top items, merge near-duplicates (keeping every lens
/// that raised it), and stop keeping at `cap`. Also returns how many distinct findings the cap
/// left out, counted in the same round-robin order. Past the cap a near-duplicate of a kept
/// finding still adds its lens to that finding, and a near-duplicate of a left-out finding is
/// not counted again.
pub fn findings_from(results: &[&AgentResult], cap: usize) -> (Vec<Finding>, usize) {
    let per_agent: Vec<Vec<String>> = results
        .iter()
        .map(|r| contract::items(&r.content))
        .collect();
    let rounds = per_agent.iter().map(Vec::len).max().unwrap_or(0);
    let mut findings: Vec<Finding> = Vec::new();
    let mut keys: Vec<Vec<String>> = Vec::new();
    let mut dropped = 0;
    for round in 0..rounds {
        for (result, items) in results.iter().zip(&per_agent) {
            let Some(text) = items.get(round) else {
                continue;
            };
            let key = words(text);
            if let Some(i) = keys.iter().position(|k| near_duplicate(k, &key)) {
                if let (Some(lens), Some(kept)) = (&result.lens, findings.get_mut(i)) {
                    if !kept.lenses.contains(lens) {
                        kept.lenses.push(lens.clone());
                    }
                }
                continue;
            }
            if findings.len() >= cap {
                dropped += 1;
                keys.push(key);
                continue;
            }
            findings.push(Finding {
                lenses: result.lens.iter().cloned().collect(),
                role: result.role,
                text: text.clone(),
            });
            keys.push(key);
        }
    }
    (findings, dropped)
}

/// Shorten `text` to at most `max` bytes on a char boundary, marking the cut.
pub(crate) fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max.saturating_sub(3);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", text[..end].trim_end())
}

/// The per-finding byte allowance that keeps `n` findings plus the idea within `budget`.
pub(crate) fn finding_allowance(budget: ContextBudget, n: usize) -> usize {
    (budget.max_bytes / (n + 2)).max(200)
}

/// Parse `F<n>: LABEL — reason` lines into one slot per finding (`None` = no usable line).
/// Case-insensitive, tolerant of list markers and emphasis; the first verdict per finding wins.
pub fn parse_audit_slots(raw: &str, n: usize) -> Vec<Option<Verdict>> {
    let mut verdicts: Vec<Option<Verdict>> = vec![None; n];
    for line in raw.lines() {
        let t = line
            .trim()
            .trim_start_matches(['-', '*', ' '])
            .trim_start_matches("**");
        let Some(rest) = t.strip_prefix(['F', 'f']) else {
            continue;
        };
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        let Ok(id) = rest[..digits].parse::<usize>() else {
            continue;
        };
        if id == 0 || id > n || verdicts[id - 1].is_some() {
            continue;
        }
        let after = rest[digits..].trim_start_matches(['*', ':', '.', ')', ' ']);
        let upper = after.to_ascii_uppercase();
        let (label, word_len) = if upper.starts_with("CONFIRMED") {
            (Label::Confirmed, "CONFIRMED".len())
        } else if upper.starts_with("REFUTED") {
            (Label::Refuted, "REFUTED".len())
        } else if upper.starts_with("UNCERTAIN") {
            (Label::Uncertain, "UNCERTAIN".len())
        } else {
            continue;
        };
        let reason = after[word_len..]
            .trim_start_matches(['*', ' ', ':', '-', '—', '–'])
            .trim()
            .to_string();
        verdicts[id - 1] = Some(Verdict { label, reason });
    }
    verdicts
}

/// Fill the empty slots of `a` from `b`; a slot `a` already holds keeps its verdict, so the
/// re-ask can only add verdicts, never overturn one (first verdict wins, as within one answer).
pub fn merge_first_wins(a: &mut [Option<Verdict>], b: Vec<Option<Verdict>>) {
    for (slot, other) in a.iter_mut().zip(b) {
        if slot.is_none() {
            *slot = other;
        }
    }
}

/// The report for a set of slots: a finding with no verdict defaults to `UNCERTAIN`, and no
/// verdict at all is a failed audit.
fn report_from_slots(slots: Vec<Option<Verdict>>) -> AuditReport {
    let n = slots.len();
    let answered = slots.iter().filter(|v| v.is_some()).count();
    AuditReport {
        verdicts: slots
            .into_iter()
            .map(|v| {
                v.unwrap_or(Verdict {
                    label: Label::Uncertain,
                    reason: "not assessed by the auditor".to_string(),
                })
            })
            .collect(),
        answered,
        failed: answered == 0 && n > 0,
    }
}

/// Parse `F<n>: LABEL — reason` lines into verdicts for `n` findings; a finding with no line
/// defaults to `UNCERTAIN`.
pub fn parse_audit(raw: &str, n: usize) -> AuditReport {
    report_from_slots(parse_audit_slots(raw, n))
}

/// The 1-based ids of the findings `slots` holds no verdict for.
fn missing_ids(slots: &[Option<Verdict>]) -> Vec<usize> {
    slots
        .iter()
        .enumerate()
        .filter(|(_, v)| v.is_none())
        .map(|(i, _)| i + 1)
        .collect()
}

/// The re-ask suffix naming the findings (1-based) still without a verdict.
pub fn reask_suffix(missing: &[usize]) -> String {
    let ids = missing
        .iter()
        .map(|i| format!("F{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    REASK_TEMPLATE.text.replace("{ids}", &ids)
}

/// The auditor's prompt body: the instruction, the numbered findings, then the idea material.
pub fn audit_context(numbered: &str, material: &str) -> String {
    format!(
        "{}\n\n## Findings\n{numbered}\n\n{material}",
        AUDIT_TEMPLATE.text
    )
}

/// What one audit cost: the report plus the model calls it made (0 with no findings, 1, or 2
/// with the re-ask), so a workflow can charge its call budget (ADR-0034).
#[derive(Debug, Clone, PartialEq)]
pub struct AuditRun {
    pub report: AuditReport,
    pub calls: u32,
}

/// What one audit judges: `findings` against the idea `idea_slug`, within `budget`, and whether
/// the caller can fund the one re-ask.
#[derive(Debug, Clone, Copy)]
pub struct AuditTarget<'a> {
    pub vault_dir: &'a Path,
    pub idea_slug: &'a str,
    pub findings: &'a [Finding],
    pub budget: ContextBudget,
    pub may_reask: bool,
}

/// Run the Auditor over `findings` for `idea_slug`: one bounded model call (its own permit via
/// `run_agent` — callers must not hold one) whose context is the numbered findings plus the
/// idea/memory/discussion, budgeted to what is left after the findings. A malformed or partial
/// answer gets at most one targeted re-ask naming only the findings still missing, when
/// `may_reask` (the caller's call budget can fund it, ADR-0023 amendment). Never fails the run: a
/// model error or an answer still unparseable yields `UNCERTAIN` for what is missing, and an
/// audit with no verdict at all is marked `failed`.
pub async fn audit(
    llm: &LlmBackend,
    ai_semaphore: &Semaphore,
    registry: &SkillRegistry,
    target: AuditTarget<'_>,
) -> Result<AuditRun, ConceptError> {
    let AuditTarget {
        vault_dir,
        idea_slug,
        findings,
        budget,
        may_reask,
    } = target;
    if findings.is_empty() {
        return Ok(AuditRun {
            report: AuditReport {
                verdicts: Vec::new(),
                answered: 0,
                failed: false,
            },
            calls: 0,
        });
    }
    let allowance = finding_allowance(budget, findings.len());
    let numbered = findings
        .iter()
        .enumerate()
        .map(|(i, f)| format!("F{}: {}", i + 1, clip(&f.text, allowance)))
        .collect::<Vec<_>>()
        .join("\n");
    let rest = ContextBudget::new(budget.max_bytes.saturating_sub(numbered.len()));
    // No related-ideas block: a verdict must rest on this idea's material alone.
    let material = hydrate_context(vault_dir, idea_slug, rest)?;
    let context = audit_context(&numbered, &material.text);
    let ask = |context: String| async move {
        let task = AgentTask {
            role: AgentRole::Auditor,
            skill: None,
            context,
        };
        run_agent(llm, ai_semaphore, registry, task)
            .await
            .map(|answer| answer.content)
    };
    audit_rounds(findings.len(), context, may_reask, idea_slug, ask).await
}

/// The audit's call sequence over any `ask`: the first answer, then — only when it left some
/// finding without a verdict and `may_reask` — one re-ask with [`reask_suffix`], merged
/// first-verdict-wins. A closed semaphore is the only error that escapes.
async fn audit_rounds<F, Fut>(
    n: usize,
    context: String,
    may_reask: bool,
    idea_slug: &str,
    mut ask: F,
) -> Result<AuditRun, ConceptError>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<String, ConceptError>>,
{
    let mut slots = match ask(context.clone()).await {
        Ok(answer) => parse_audit_slots(&answer, n),
        Err(ConceptError::SemaphoreClosed) => return Err(ConceptError::SemaphoreClosed),
        Err(e) => {
            // A failed call is not a malformed audit: the re-ask is for answers, not outages.
            tracing::warn!(idea_slug, error = %e, "audit call failed; findings left uncertain");
            return Ok(AuditRun {
                report: AuditReport::all_uncertain(n, "the audit could not run"),
                calls: 1,
            });
        }
    };
    let missing = missing_ids(&slots);
    if missing.is_empty() {
        return Ok(AuditRun {
            report: report_from_slots(slots),
            calls: 1,
        });
    }
    tracing::warn!(
        idea_slug,
        missing = missing.len(),
        of = n,
        may_reask,
        "audit answer malformed or partial"
    );
    if !may_reask {
        return Ok(AuditRun {
            report: report_from_slots(slots),
            calls: 1,
        });
    }
    match ask(format!("{context}{}", reask_suffix(&missing))).await {
        Ok(answer) => merge_first_wins(&mut slots, parse_audit_slots(&answer, n)),
        Err(ConceptError::SemaphoreClosed) => return Err(ConceptError::SemaphoreClosed),
        Err(e) => {
            tracing::warn!(idea_slug, error = %e, "audit re-ask failed; first answer kept");
        }
    }
    let report = report_from_slots(slots);
    if report.failed {
        tracing::warn!(
            idea_slug,
            "audit answer unparseable after re-ask; findings left uncertain"
        );
    }
    Ok(AuditRun { report, calls: 2 })
}

/// The code-owned line naming findings the cap kept out of synthesis when no audit ran; empty
/// when nothing was left out.
pub fn unaudited_cap_note(dropped: usize) -> String {
    if dropped == 0 {
        return String::new();
    }
    format!(
        "\n\n_{} left out (cap {MAX_AUDIT_FINDINGS})_",
        further(dropped)
    )
}

fn further(n: usize) -> String {
    if n == 1 {
        "1 further finding".to_string()
    } else {
        format!("{n} further findings")
    }
}

/// The code-owned tail appended to a synthesis: the audit tally (with the uniform-pass warning)
/// and every refuted finding with the auditor's reason — downgraded, never dropped.
pub fn appendix(findings: &[Finding], report: &AuditReport, dropped: usize) -> String {
    let cap_note = if dropped > 0 {
        format!(
            "\n\n_{} not audited (cap {MAX_AUDIT_FINDINGS})_",
            further(dropped)
        )
    } else {
        String::new()
    };
    if report.failed {
        return format!("\n\n_Audit: unavailable — the findings above are unverified._{cap_note}");
    }
    let mut out = format!(
        "\n\n_Audit: {} confirmed · {} uncertain · {} refuted_{cap_note}",
        report.count(Label::Confirmed),
        report.count(Label::Uncertain),
        report.count(Label::Refuted)
    );
    if report.uniform_pass() {
        out.push_str(
            "\n\n_Nearly every finding was confirmed — a uniform pass is a warning sign; \
             treat the confirmations with suspicion._",
        );
    }
    let refuted: Vec<String> = findings
        .iter()
        .zip(&report.verdicts)
        .filter(|(_, v)| v.label == Label::Refuted)
        .map(|(f, v)| format!("- ~~{}~~ ({}) — {}", f.text, f.provenance(), v.reason))
        .collect();
    if !refuted.is_empty() {
        out.push_str("\n\n### Disproven objections\n\n");
        out.push_str(&refuted.join("\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(lens: &str, role: AgentRole, content: &str) -> AgentResult {
        AgentResult {
            role,
            lens: Some(lens.to_string()),
            content: content.to_string(),
        }
    }

    fn finding(text: &str) -> Finding {
        Finding {
            lenses: vec!["premortem".into()],
            role: AgentRole::Critic,
            text: text.into(),
        }
    }

    #[test]
    fn findings_interleave_lenses_merge_near_duplicates_and_cap() {
        let a = result(
            "premortem",
            AgentRole::Critic,
            "1. Nobody pays for it\n2. Churn",
        );
        let b = result(
            "constraints",
            AgentRole::Researcher,
            "- nobody pays for it!\n- Needs a licence\n- Needs a team",
        );
        let (findings, dropped) = findings_from(&[&a, &b], 10);
        assert_eq!(dropped, 0);
        let texts: Vec<&str> = findings.iter().map(|f| f.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "Nobody pays for it",
                "Churn",
                "Needs a licence",
                "Needs a team"
            ]
        );
        assert_eq!(findings[0].lenses, ["premortem", "constraints"]);
        assert_eq!(findings[2].role, AgentRole::Researcher);
        let (kept, dropped) = findings_from(&[&a, &b], 2);
        assert_eq!(kept.len(), 2);
        assert_eq!(dropped, 2);
    }

    #[test]
    fn findings_from_merges_a_post_cap_duplicate_lens_into_a_kept_finding() {
        let a = result(
            "alpha",
            AgentRole::Critic,
            "- shared risk one two three four five\n- extra six seven",
        );
        let b = result(
            "beta",
            AgentRole::Critic,
            "- other eight nine\n- shared risk one two three four five",
        );
        let (kept, dropped) = findings_from(&[&a, &b], 1);
        assert_eq!(kept.len(), 1);
        assert_eq!(
            kept[0].lenses,
            vec!["alpha".to_string(), "beta".to_string()]
        );
        assert_eq!(dropped, 2);
    }

    #[test]
    fn findings_from_does_not_count_a_duplicate_of_a_left_out_finding_again() {
        let a = result(
            "alpha",
            AgentRole::Critic,
            "- kept one two\n- left out three four",
        );
        let b = result("beta", AgentRole::Critic, "- left out three four");
        let c = result("gamma", AgentRole::Critic, "- left out three four");
        let (kept, dropped) = findings_from(&[&a, &b, &c], 1);
        assert_eq!(kept.len(), 1);
        assert_eq!(dropped, 1);
    }

    #[test]
    fn the_cap_lines_agree_on_singular_and_plural() {
        assert_eq!(unaudited_cap_note(0), "");
        assert_eq!(
            unaudited_cap_note(1),
            "\n\n_1 further finding left out (cap 20)_"
        );
        assert_eq!(
            unaudited_cap_note(3),
            "\n\n_3 further findings left out (cap 20)_"
        );
        let report = AuditReport::all_uncertain(0, "x");
        let one = appendix(&[], &report, 1);
        assert!(
            one.contains("_1 further finding not audited (cap 20)_"),
            "{one}"
        );
        let many = appendix(&[], &report, 2);
        assert!(
            many.contains("_2 further findings not audited (cap 20)_"),
            "{many}"
        );
    }

    #[test]
    fn findings_from_counts_only_distinct_findings_the_cap_left_out() {
        let a = result(
            "premortem",
            AgentRole::Critic,
            "1. Alpha one\n2. Beta two\n3. Gamma",
        );
        let b = result(
            "constraints",
            AgentRole::Researcher,
            "- alpha one\n- Delta four",
        );
        let (kept, dropped) = findings_from(&[&a, &b], 2);
        let texts: Vec<&str> = kept.iter().map(|f| f.text.as_str()).collect();
        assert_eq!(texts, ["Alpha one", "Beta two"]);
        assert_eq!(kept[0].lenses, ["premortem", "constraints"]);
        assert_eq!(dropped, 2);
    }

    #[test]
    fn parse_audit_reads_labels_tolerantly_and_defaults_the_rest() {
        let raw = "Here you go:\n\
                   F1: CONFIRMED — follows from the pricing.\n\
                   - **F2**: refuted: the discussion settled this at turn 3\n\
                   F2: CONFIRMED — second opinion ignored\n\
                   F9: CONFIRMED — out of range\n";
        let report = parse_audit(raw, 3);
        assert_eq!(report.answered, 2);
        assert!(!report.failed);
        assert_eq!(report.verdicts[0].label, Label::Confirmed);
        assert_eq!(report.verdicts[0].reason, "follows from the pricing.");
        assert_eq!(
            report.verdicts[1].label,
            Label::Refuted,
            "first verdict wins"
        );
        assert_eq!(
            report.verdicts[1].reason,
            "the discussion settled this at turn 3"
        );
        assert_eq!(report.verdicts[2].label, Label::Uncertain);
    }

    #[test]
    fn a_garbled_audit_is_a_failed_all_uncertain_report() {
        let report = parse_audit("I think these are all great points!", 2);
        assert!(report.failed);
        assert!(report.verdicts.iter().all(|v| v.label == Label::Uncertain));
    }

    #[test]
    fn uniform_pass_needs_enough_findings_and_over_ninety_percent() {
        let all = |n: usize| {
            parse_audit(
                &(1..=n)
                    .map(|i| format!("F{i}: CONFIRMED — ok"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                n,
            )
        };
        assert!(!all(3).uniform_pass(), "three findings is too few to judge");
        assert!(all(4).uniform_pass());
        let mut mixed = all(10);
        mixed.verdicts[0].label = Label::Uncertain;
        assert!(!mixed.uniform_pass(), "exactly 90% is not over 90%");
    }

    #[test]
    fn appendix_keeps_refuted_findings_struck_through_with_the_reason() {
        let findings = vec![finding("Nobody pays"), finding("Churn")];
        let report = parse_audit(
            "F1: REFUTED — three pilots paid\nF2: UNCERTAIN — unknown",
            2,
        );
        let out = appendix(&findings, &report, 0);
        assert!(out.contains("1 uncertain · 1 refuted"));
        assert!(out.contains("### Disproven objections"));
        assert!(out.contains("~~Nobody pays~~ (premortem · critic) — three pilots paid"));
        assert!(!out.contains("Churn~~"));
        let failed = AuditReport::all_uncertain(2, "x");
        assert!(appendix(&findings, &failed, 0).contains("unverified"));
    }

    #[test]
    fn golden_audit_prompt_for_fixed_input() {
        crate::ai::provenance::assert_golden(
            &audit_context(
                "F1: Nobody pays for it\nF2: The licence takes a year",
                "## Idea\nA tool library for one street.",
            ),
            include_str!("../../tests/fixtures/prompt-goldens/audit.txt"),
            "audit.txt",
        );
    }

    #[test]
    fn golden_reask_suffix() {
        crate::ai::provenance::assert_golden(
            &reask_suffix(&[2, 5]),
            include_str!("../../tests/fixtures/prompt-goldens/audit-reask.txt"),
            "audit-reask.txt",
        );
    }

    #[test]
    fn registered_template_ids_are_unique() {
        let all = [
            AUDIT_TEMPLATE,
            REASK_TEMPLATE,
            crate::ai::contract::RETRY_NOTE,
        ];
        let mut ids: Vec<&str> = all.iter().map(|t| t.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), all.len());
    }

    /// Drive `audit_rounds` with scripted answers; returns the run and every prompt it sent.
    fn scripted(
        n: usize,
        may_reask: bool,
        answers: Vec<Result<&'static str, ConceptError>>,
    ) -> (AuditRun, Vec<String>) {
        let answers = std::sync::Mutex::new(answers.into_iter());
        let prompts = std::sync::Mutex::new(Vec::new());
        let ask = |prompt: String| {
            prompts.lock().expect("test lock").push(prompt);
            let next = answers
                .lock()
                .expect("test lock")
                .next()
                .expect("more calls than scripted");
            async move { next.map(str::to_string) }
        };
        let run = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(audit_rounds(n, "CTX".into(), may_reask, "idea", ask))
            .expect("audit");
        (run, prompts.into_inner().expect("test lock"))
    }

    #[test]
    fn garbled_then_valid_merges_to_full() {
        let (run, prompts) = scripted(
            2,
            true,
            vec![
                Ok("these all look great"),
                Ok("F1: CONFIRMED — ok\nF2: REFUTED — no"),
            ],
        );
        assert_eq!(run.calls, 2);
        assert!(!run.report.failed);
        assert_eq!(run.report.answered, 2);
        assert_eq!(run.report.verdicts[1].label, Label::Refuted);
        assert_eq!(prompts[1], format!("CTX{}", reask_suffix(&[1, 2])));
    }

    #[test]
    fn partial_then_fills_gaps_first_wins() {
        let (run, prompts) = scripted(
            3,
            true,
            vec![
                Ok("F1: REFUTED — first\nF3: CONFIRMED — kept"),
                Ok("F1: CONFIRMED — overturn attempt\nF2: UNCERTAIN — filled"),
            ],
        );
        assert_eq!(run.calls, 2);
        assert_eq!(run.report.answered, 3);
        assert_eq!(run.report.verdicts[0].label, Label::Refuted, "first wins");
        assert_eq!(run.report.verdicts[0].reason, "first");
        assert_eq!(run.report.verdicts[1].reason, "filled");
        assert!(
            prompts[1].ends_with(&reask_suffix(&[2])),
            "only the missing id"
        );
    }

    #[test]
    fn reask_error_keeps_first_report() {
        let (run, _) = scripted(
            2,
            true,
            vec![
                Ok("F1: CONFIRMED — ok"),
                Err(ConceptError::UnknownSkill("boom".into())),
            ],
        );
        assert_eq!(run.calls, 2);
        assert_eq!(run.report.answered, 1);
        assert!(!run.report.failed);
        assert_eq!(run.report.verdicts[1].label, Label::Uncertain);
    }

    #[test]
    fn reask_also_garbled_stays_failed_uncertain() {
        let (run, _) = scripted(2, true, vec![Ok("nope"), Ok("still nope")]);
        assert_eq!(run.calls, 2);
        assert!(run.report.failed);
        assert!(run
            .report
            .verdicts
            .iter()
            .all(|v| v.label == Label::Uncertain));
    }

    #[test]
    fn a_complete_audit_or_an_unfunded_one_is_never_reasked() {
        let (full, prompts) = scripted(1, true, vec![Ok("F1: CONFIRMED — ok")]);
        assert_eq!((full.calls, prompts.len()), (1, 1));
        let (unfunded, prompts) = scripted(2, false, vec![Ok("F1: CONFIRMED — ok")]);
        assert_eq!((unfunded.calls, prompts.len()), (1, 1));
        assert_eq!(unfunded.report.answered, 1);
    }

    #[test]
    fn a_failed_call_is_not_reasked() {
        let (run, prompts) = scripted(2, true, vec![Err(ConceptError::UnknownSkill("x".into()))]);
        assert_eq!((run.calls, prompts.len()), (1, 1));
        assert!(run.report.failed);
    }

    #[test]
    fn clip_marks_the_cut_on_a_char_boundary() {
        assert_eq!(clip("short", 10), "short");
        let clipped = clip("héllo wörld and more", 9);
        assert!(clipped.ends_with('…') && clipped.len() <= 9 + '…'.len_utf8());
    }
}
