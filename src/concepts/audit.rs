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

/// Lowercased alphanumeric words — the key near-duplicate detection compares.
fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Two findings say the same thing when their word sets overlap by at least 80% (Jaccard).
fn near_duplicate(a: &[String], b: &[String]) -> bool {
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
/// that raised it), and stop at `cap`.
pub fn findings_from(results: &[&AgentResult], cap: usize) -> Vec<Finding> {
    let per_agent: Vec<Vec<String>> = results
        .iter()
        .map(|r| contract::items(&r.content))
        .collect();
    let rounds = per_agent.iter().map(Vec::len).max().unwrap_or(0);
    let mut findings: Vec<Finding> = Vec::new();
    let mut keys: Vec<Vec<String>> = Vec::new();
    'rounds: for round in 0..rounds {
        for (result, items) in results.iter().zip(&per_agent) {
            let Some(text) = items.get(round) else {
                continue;
            };
            let key = words(text);
            if let Some(i) = keys.iter().position(|k| near_duplicate(k, &key)) {
                if let Some(lens) = &result.lens {
                    if !findings[i].lenses.contains(lens) {
                        findings[i].lenses.push(lens.clone());
                    }
                }
                continue;
            }
            findings.push(Finding {
                lenses: result.lens.iter().cloned().collect(),
                role: result.role,
                text: text.clone(),
            });
            keys.push(key);
            if findings.len() == cap {
                break 'rounds;
            }
        }
    }
    findings
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
    match run_agent(llm, ai_semaphore, registry, task).await {
        Ok(answer) => {
            let report = parse_audit(&answer.content, findings.len());
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

/// The code-owned tail appended to a synthesis: the audit tally (with the uniform-pass warning)
/// and every refuted finding with the auditor's reason — downgraded, never dropped.
pub fn appendix(findings: &[Finding], report: &AuditReport) -> String {
    if report.failed {
        return "\n\n_Audit: unavailable — the findings above are unverified._".to_string();
    }
    let mut out = format!(
        "\n\n_Audit: {} confirmed · {} uncertain · {} refuted_",
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
        let findings = findings_from(&[&a, &b], 10);
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
        assert_eq!(findings_from(&[&a, &b], 2).len(), 2);
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
        let out = appendix(&findings, &report);
        assert!(out.contains("1 uncertain · 1 refuted"));
        assert!(out.contains("### Disproven objections"));
        assert!(out.contains("~~Nobody pays~~ (premortem · critic) — three pilots paid"));
        assert!(!out.contains("Churn~~"));
        let failed = AuditReport::all_uncertain(2, "x");
        assert!(appendix(&findings, &failed).contains("unverified"));
    }

    #[test]
    fn clip_marks_the_cut_on_a_char_boundary() {
        assert_eq!(clip("short", 10), "short");
        let clipped = clip("héllo wörld and more", 9);
        assert!(clipped.ends_with('…') && clipped.len() <= 9 + '…'.len_utf8());
    }
}
