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
use crate::ai::verdict::ParserKind;
use crate::ai::LlmBackend;
use crate::concepts::agents::{run_agent_meta, AgentResult, AgentRole, AgentTask};
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

/// Parse `F<n>: LABEL — reason` lines into verdicts for `n` findings. Case-insensitive, tolerant
/// of list markers and emphasis; the first verdict per finding wins; a finding with no line
/// defaults to `UNCERTAIN`.
pub fn parse_audit(raw: &str, n: usize) -> AuditReport {
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
    let answered = verdicts.iter().filter(|v| v.is_some()).count();
    AuditReport {
        verdicts: verdicts
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

/// The canonical verdict line for an audit answer over `n` findings (docs/adr/0038): `pass` is how
/// many findings the auditor answered, then one `F<i>=LABEL` per finding, `unanswered` where
/// [`parse_audit`] fell back to its default. The run journal and the parser corpus both record
/// this line, so a parser change shows up as a flip in either.
pub fn summarize_audit(raw: &str, n: usize) -> String {
    let report = parse_audit(raw, n);
    let mut line = format!("pass={} n={n} failed={}", report.answered, report.failed);
    let answered = answered_ids(raw, n);
    for (i, v) in report.verdicts.iter().enumerate() {
        let label = if answered.contains(&(i + 1)) {
            v.label.as_str()
        } else {
            "unanswered"
        };
        line.push_str(&format!(" F{}={label}", i + 1));
    }
    line
}

/// The finding ids `raw` gave a verdict line for, by the same rules as [`parse_audit`].
fn answered_ids(raw: &str, n: usize) -> Vec<usize> {
    (1..=n)
        .filter(|&id| {
            // Answered iff its own lines, parsed alone, still yield a verdict: this reuses
            // parse_audit's label rules instead of restating them.
            let own: Vec<&str> = raw
                .lines()
                .filter(|l| audit_line_id(l) == Some(id))
                .collect();
            !own.is_empty() && parse_audit(&own.join("\n"), n).answered == 1
        })
        .collect()
}

/// The `F<i>` id a line starts with, after the markers [`parse_audit`] tolerates.
fn audit_line_id(line: &str) -> Option<usize> {
    let t = line
        .trim()
        .trim_start_matches(['-', '*', ' '])
        .trim_start_matches("**");
    let rest = t.strip_prefix(['F', 'f'])?;
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    rest[..digits].parse().ok()
}

/// Run the Auditor over `findings` for `idea_slug`: one bounded model call (its own permit via
/// `run_agent` — callers must not hold one) whose context is the numbered findings plus the
/// idea/memory/discussion, budgeted to what is left after the findings. Never fails the run: a
/// model error or an unparseable answer yields an all-`UNCERTAIN` report marked `failed`.
pub async fn audit(
    llm: &LlmBackend,
    ai_semaphore: &Semaphore,
    registry: &SkillRegistry,
    vault_dir: &Path,
    idea_slug: &str,
    findings: &[Finding],
    budget: ContextBudget,
) -> Result<AuditReport, ConceptError> {
    if findings.is_empty() {
        return Ok(AuditReport {
            verdicts: Vec::new(),
            answered: 0,
            failed: false,
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
    let task = AgentTask {
        role: AgentRole::Auditor,
        skill: None,
        context: format!(
            "{AUDIT_INSTRUCTION}\n\n## Findings\n{numbered}\n\n{}",
            material.text
        ),
    };
    match run_agent_meta(llm, ai_semaphore, registry, task).await {
        Ok((answer, meta)) => {
            let report = parse_audit(&answer.content, findings.len());
            llm.record_verdict(
                &meta,
                ParserKind::Audit { n: findings.len() },
                summarize_audit(&answer.content, findings.len()),
                None,
            );
            if report.failed {
                tracing::warn!(
                    idea_slug,
                    "audit answer unparseable; findings left uncertain"
                );
            }
            Ok(report)
        }
        Err(ConceptError::SemaphoreClosed) => Err(ConceptError::SemaphoreClosed),
        Err(e) => {
            tracing::warn!(idea_slug, error = %e, "audit call failed; findings left uncertain");
            Ok(AuditReport::all_uncertain(
                findings.len(),
                "the audit could not run",
            ))
        }
    }
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
    use crate::ai::contract::ContractOutcome;

    fn result(lens: &str, role: AgentRole, content: &str) -> AgentResult {
        AgentResult {
            role,
            lens: Some(lens.to_string()),
            content: content.to_string(),
            contract: ContractOutcome::Clean,
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
    fn clip_marks_the_cut_on_a_char_boundary() {
        assert_eq!(clip("short", 10), "short");
        let clipped = clip("héllo wörld and more", 9);
        assert!(clipped.ends_with('…') && clipped.len() <= 9 + '…'.len_utf8());
    }
}
