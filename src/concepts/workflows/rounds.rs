//! The Loop and Refine stages (docs/adr/0034, D37): bounded repetition, with every stop decided
//! in code.
//!
//! A Loop reruns its steps until a round adds nothing new (`dry_rounds` in a row), a cap is hit,
//! or its first round fails outright. "New" is code's call: an item is new unless it is a near
//! duplicate of something already found or of an earlier item in the same round, judged in step
//! order so the answer never depends on which call finished first. A round in which every agent
//! failed spends its calls but resets nothing — it neither ends nor extends the dry streak.
//!
//! A Refine follows an Audit: it asks one step to rewrite the REFUTED and UNCERTAIN findings by
//! id, swaps the rewrites in by id, and re-audits — at most `max_rounds` times, stopping as soon
//! as nothing is refuted or uncertain.

use crate::ai::contract;
use crate::concepts::agents::{run_agent, AgentResult, AgentRole, AgentTask};
use crate::concepts::audit::{self, AuditReport, Finding, Label};
use crate::concepts::swarm::fan_out;
use crate::concepts::workflows::ground::cell;
use crate::concepts::workflows::run::{
    stage_context, CallBudget, PendingArtifact, RunCtx, StageOutcome, StageStatus,
};
use crate::concepts::workflows::{LoopStage, RefineStage, WorkflowStep};
use crate::concepts::ConceptError;
use crate::domain::{ArtifactKind, OutputContract};

/// How much of each found item the "already found" block repeats back.
const SEEN_ITEM_CHARS: usize = 80;

/// The stage budget divisor the "already found" block may take (a quarter).
const SEEN_DIVISOR: usize = 4;

/// Why a Loop stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// `dry_rounds` rounds in a row found nothing new.
    Dry,
    /// The next round would pass `max_rounds`, `max_calls` or the run's call budget.
    Cap,
    /// The first round failed outright.
    Failed,
}

impl StopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            StopReason::Dry => "dry",
            StopReason::Cap => "cap",
            StopReason::Failed => "failed",
        }
    }
}

/// One distinct item a Loop found.
#[derive(Debug, Clone, PartialEq)]
pub struct LoopItem {
    pub text: String,
    pub round: usize,
    pub lens: String,
    pub role: AgentRole,
    key: Vec<String>,
}

/// A Loop's running state: what it found, and the counters its stop rules read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LoopTally {
    pub items: Vec<LoopItem>,
    pub rounds: usize,
    pub calls: usize,
    dry_streak: usize,
    pub stop: Option<StopReason>,
}

impl LoopTally {
    /// Whether another round may start: it must fit `max_calls` and `max_rounds`, and the run's
    /// call budget (`run_can_fund`) must still cover it after reserving the stages that follow.
    pub fn can_start_round(&self, spec: &LoopStage, run_can_fund: bool) -> bool {
        self.stop.is_none()
            && self.calls + spec.steps.len() <= spec.max_calls
            && self.rounds < spec.max_rounds
            && run_can_fund
    }

    /// Fold one round's results (index-aligned with `steps`) in, returning how many new items it
    /// added and setting [`stop`](Self::stop) when a stop rule fires.
    pub fn absorb(
        &mut self,
        steps: &[WorkflowStep],
        results: &[Option<AgentResult>],
        dry_rounds: usize,
    ) -> usize {
        self.rounds += 1;
        self.calls += steps.len();
        if results.iter().all(Option::is_none) {
            if self.rounds == 1 {
                self.stop = Some(StopReason::Failed);
            }
            return 0;
        }
        let before = self.items.len();
        for (step, result) in steps.iter().zip(results) {
            let Some(result) = result else { continue };
            for text in contract::items(&result.content) {
                let key = audit::words(&text);
                if self
                    .items
                    .iter()
                    .any(|seen| audit::near_duplicate(&seen.key, &key))
                {
                    continue;
                }
                self.items.push(LoopItem {
                    text,
                    round: self.rounds,
                    lens: step.label().to_string(),
                    role: step.role,
                    key,
                });
            }
        }
        let new = self.items.len() - before;
        self.dry_streak = if new == 0 { self.dry_streak + 1 } else { 0 };
        if self.dry_streak >= dry_rounds.max(1) {
            self.stop = Some(StopReason::Dry);
        }
        new
    }

    /// The items as one result per step lens, in step order — how they join the run's findings.
    fn results(&self, steps: &[WorkflowStep]) -> Vec<Option<AgentResult>> {
        let mut lenses: Vec<(&str, AgentRole)> = Vec::new();
        for s in steps {
            if !lenses.iter().any(|(l, _)| *l == s.label()) {
                lenses.push((s.label(), s.role));
            }
        }
        lenses
            .into_iter()
            .filter_map(|(lens, role)| {
                let bullets: Vec<String> = self
                    .items
                    .iter()
                    .filter(|i| i.lens == lens)
                    .map(|i| format!("- {}", i.text))
                    .collect();
                (!bullets.is_empty()).then(|| {
                    Some(AgentResult {
                        role,
                        lens: Some(lens.to_string()),
                        content: bullets.join("\n"),
                    })
                })
            })
            .collect()
    }

    fn seen_block(&self, cap: usize) -> String {
        let lines: Vec<String> = self
            .items
            .iter()
            .map(|i| format!("- {}", audit::clip(&i.text, SEEN_ITEM_CHARS)))
            .collect();
        audit::clip(
            &format!("## Already found (do not repeat)\n{}", lines.join("\n")),
            cap,
        )
    }

    fn artifact_body(&self) -> String {
        let stop = self.stop.map_or("cap", StopReason::as_str);
        let mut out = format!(
            "# Loop findings\n\nStopped: {stop} after {} round(s), {} call(s), {} distinct item(s).\n\n| Item | First round | Lens |\n|---|---|---|\n",
            self.rounds,
            self.calls,
            self.items.len()
        );
        for i in &self.items {
            out.push_str(&format!(
                "| {} | {} | {} |\n",
                cell(&i.text),
                i.round,
                cell(&i.lens)
            ));
        }
        out
    }
}

/// What one Loop stage hands the engine.
pub(crate) struct LoopRun {
    pub outcome: StageOutcome,
    /// The distinct items, one result per step lens, for the run's findings.
    pub results: Vec<Option<AgentResult>>,
}

/// Run one Loop stage over the `carried` blocks so far.
pub(crate) async fn run_loop(
    ctx: &RunCtx<'_>,
    spec: &LoopStage,
    carried: &[String],
    calls: &CallBudget,
    note: &(dyn Fn(&str) + Sync),
) -> Result<LoopRun, ConceptError> {
    let mut tally = LoopTally::default();
    let width = u32::try_from(spec.steps.len()).unwrap_or(u32::MAX);
    while tally.can_start_round(spec, calls.can_fund(width)) {
        let mut blocks = carried.to_vec();
        if !tally.items.is_empty() {
            blocks.push(tally.seen_block(ctx.budget.max_bytes / SEEN_DIVISOR));
        }
        let context = stage_context(
            ctx.vault_dir,
            ctx.idea_slug,
            ctx.budget,
            &blocks,
            ctx.related,
        )?;
        let tasks = spec
            .steps
            .iter()
            .map(|s| AgentTask {
                role: s.role,
                skill: s.skill.clone(),
                context: context.clone(),
            })
            .collect();
        let round = tally.rounds + 1;
        let on_done = |done: usize, of: usize, _: &str| {
            calls.charge(1);
            note(&format!("round {round}/{} · {done}/{of}", spec.max_rounds));
        };
        let results = fan_out(ctx.llm, ctx.sem, &ctx.book.skills, tasks, &on_done).await;
        let new = tally.absorb(&spec.steps, &results, spec.dry_rounds);
        note(&format!(
            "round {round}/{} · +{new} new ({} total)",
            spec.max_rounds,
            tally.items.len()
        ));
    }
    let stop = *tally.stop.get_or_insert(StopReason::Cap);
    let detail = format!(
        "stopped {} after {} round(s) · {} item(s)",
        stop.as_str(),
        tally.rounds,
        tally.items.len()
    );
    let status = match stop {
        StopReason::Failed => StageStatus::Degraded("the first round failed".into()),
        StopReason::Dry | StopReason::Cap => StageStatus::Ran,
    };
    let artifact = (!tally.items.is_empty()).then(|| PendingArtifact {
        kind: ArtifactKind::Finding,
        title: "Loop findings".into(),
        lens: Some("loop".into()),
        body: tally.artifact_body(),
    });
    Ok(LoopRun {
        results: tally.results(&spec.steps),
        outcome: StageOutcome {
            status,
            detail,
            artifact,
        },
    })
}

/// The 0-based positions of the findings a Refine reworks: every REFUTED or UNCERTAIN verdict of
/// an audit that ran. None when the audit is off, failed (its verdicts are only defaults) or
/// clean.
pub fn refine_targets(report: Option<&AuditReport>) -> Vec<usize> {
    report
        .filter(|r| !r.failed)
        .map(|r| {
            r.verdicts
                .iter()
                .enumerate()
                .filter(|(_, v)| matches!(v.label, Label::Refuted | Label::Uncertain))
                .map(|(i, _)| i)
                .collect()
        })
        .unwrap_or_default()
}

/// The findings the next Refine round reworks, or `None` when the stage is done: after
/// `max_rounds` rounds, or once nothing is refuted or uncertain.
pub fn next_refine_round(
    rounds_done: usize,
    max_rounds: usize,
    report: Option<&AuditReport>,
) -> Option<Vec<usize>> {
    let targets = refine_targets(report);
    (rounds_done < max_rounds && !targets.is_empty()).then_some(targets)
}

/// Swap in the rewrites a Refine step returned: each `- F<k>: text` bullet whose `k` names one of
/// `targets` (0-based positions) replaces that finding's text and marks it `refined r<round>`.
/// Any other line is ignored — code, not the model, decides which finding a rewrite replaces.
/// Returns how many findings were replaced.
pub fn apply_replacements(
    findings: &mut [Finding],
    targets: &[usize],
    raw: &str,
    round: usize,
) -> usize {
    let mut replaced: Vec<usize> = Vec::new();
    for item in contract::items(raw) {
        let t = item.trim().trim_start_matches("**");
        let Some(rest) = t.strip_prefix(['F', 'f']) else {
            continue;
        };
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        let Ok(id) = rest[..digits].parse::<usize>() else {
            continue;
        };
        let text = rest[digits..]
            .trim_start_matches(['*', ':', '.', ')', ' ', '—', '–', '-'])
            .trim();
        let Some(at) = id.checked_sub(1).filter(|i| targets.contains(i)) else {
            continue;
        };
        if text.is_empty() || replaced.contains(&at) {
            continue;
        }
        let finding = &mut findings[at];
        finding.text = text.to_string();
        finding.lenses.retain(|l| !l.starts_with("refined r"));
        finding.lenses.push(format!("refined r{round}"));
        replaced.push(at);
    }
    replaced.len()
}

/// The rework request a Refine step sees ahead of the idea's own context.
fn rework_block(findings: &[Finding], report: &AuditReport, targets: &[usize]) -> String {
    let lines: Vec<String> = targets
        .iter()
        .map(|&i| {
            let v = &report.verdicts[i];
            format!(
                "F{} [{} — {}]: {}",
                i + 1,
                v.label.as_str(),
                v.reason.trim(),
                findings[i].text
            )
        })
        .collect();
    format!(
        "## Findings to rework\nAn audit rejected or could not settle these findings. Rewrite each \
         one so it holds up against the discussion, or leave it out. Reply with markdown bullets \
         only, one per rewritten finding, each starting with its id:\n- F<k>: the rewritten \
         finding\n\n{}",
        lines.join("\n")
    )
}

/// Run one Refine stage over `findings` and the `report` of the Audit right before it, replacing
/// both in place.
pub(crate) async fn run_refine(
    ctx: &RunCtx<'_>,
    spec: &RefineStage,
    findings: &mut [Finding],
    report: &mut Option<AuditReport>,
    calls: &CallBudget,
    note: &(dyn Fn(&str) + Sync),
) -> Result<StageOutcome, ConceptError> {
    let mut rounds = 0;
    let mut replaced_total = 0;
    while let Some(targets) = next_refine_round(rounds, spec.max_rounds, report.as_ref()) {
        let Some(current) = report.as_ref() else {
            break;
        };
        rounds += 1;
        note(&format!(
            "round {rounds}/{} · reworking {} findings",
            spec.max_rounds,
            targets.len()
        ));
        let block = rework_block(findings, current, &targets);
        let context = stage_context(
            ctx.vault_dir,
            ctx.idea_slug,
            ctx.budget,
            &[block],
            ctx.related,
        )?;
        let task = AgentTask {
            role: spec.step.role,
            skill: spec.step.skill.clone(),
            context,
        };
        // One call, repair only: the stage's ceiling is one rewrite and one re-audit per round.
        let answer = run_agent(ctx.llm, ctx.sem, &ctx.book.skills, task).await;
        calls.charge(1);
        let answer = match answer {
            Ok(a) => a.content,
            Err(ConceptError::SemaphoreClosed) => return Err(ConceptError::SemaphoreClosed),
            Err(e) => {
                tracing::warn!(error = %e, "refine step failed; findings kept as audited");
                break;
            }
        };
        let answer = contract::validate(OutputContract::BulletsOrEmpty, &answer).unwrap_or(answer);
        let replaced = apply_replacements(findings, &targets, &answer, rounds);
        replaced_total += replaced;
        if replaced == 0 {
            break;
        }
        note(&format!(
            "round {rounds} · re-auditing {} findings",
            findings.len()
        ));
        let target = audit::AuditTarget {
            vault_dir: ctx.vault_dir,
            idea_slug: ctx.idea_slug,
            findings,
            budget: ctx.budget,
            // The re-audit's re-ask runs only on slack, as for an Audit stage (ADR-0023 amendment).
            may_reask: calls.can_fund(2),
        };
        let run = audit::audit(ctx.llm, ctx.sem, &ctx.book.skills, target).await?;
        calls.charge(run.calls);
        *report = Some(run.report);
    }
    if rounds == 0 {
        let why = if report.as_ref().is_none_or(|r| r.failed) {
            "no audit to refine"
        } else {
            "nothing refuted or uncertain"
        };
        note(&format!("{why} — refine skipped"));
        return Ok(StageOutcome::skipped(why));
    }
    let left = refine_targets(report.as_ref()).len();
    Ok(StageOutcome {
        status: StageStatus::Ran,
        detail: format!(
            "{rounds} round(s) · {replaced_total} rewritten · {left} still refuted or uncertain"
        ),
        artifact: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::concepts::audit::Verdict;

    fn step(skill: &str) -> WorkflowStep {
        WorkflowStep {
            role: AgentRole::Critic,
            skill: Some(skill.into()),
            angle: None,
        }
    }

    fn said(text: &str) -> Option<AgentResult> {
        Some(AgentResult {
            role: AgentRole::Critic,
            lens: None,
            content: text.into(),
        })
    }

    fn spec(steps: usize, dry_rounds: usize, max_rounds: usize, max_calls: usize) -> LoopStage {
        LoopStage {
            steps: (0..steps).map(|i| step(&format!("s{i}"))).collect(),
            dry_rounds,
            max_rounds,
            max_calls,
        }
    }

    #[test]
    fn loop_stops_dry_near_duplicates_not_new() {
        let s = spec(2, 1, 3, 16);
        let mut tally = LoopTally::default();
        let new = tally.absorb(
            &s.steps,
            &[
                said("- the market is too small\n- churn will be high"),
                said("- The market is too small!\n- pricing is unproven"),
            ],
            s.dry_rounds,
        );
        assert_eq!(new, 3, "a near duplicate within the round is not new");
        assert_eq!(tally.stop, None);
        let new = tally.absorb(
            &s.steps,
            &[said("- churn will be high"), said("- Pricing is unproven.")],
            s.dry_rounds,
        );
        assert_eq!(new, 0);
        assert_eq!(tally.stop, Some(StopReason::Dry));
        assert!(!tally.can_start_round(&s, true));
        assert_eq!(
            tally.items.iter().map(|i| i.round).collect::<Vec<_>>(),
            [1, 1, 1]
        );
    }

    #[test]
    fn failed_round_does_not_extend_dry_streak() {
        let s = spec(1, 2, 4, 16);
        let mut tally = LoopTally::default();
        assert_eq!(tally.absorb(&s.steps, &[said("- a new risk")], 2), 1);
        assert_eq!(tally.absorb(&s.steps, &[said("- a new risk")], 2), 0);
        assert_eq!(tally.absorb(&s.steps, &[None], 2), 0);
        assert_eq!(
            tally.stop, None,
            "a failed round neither ends nor extends the streak"
        );
        assert_eq!(
            (tally.rounds, tally.calls),
            (3, 3),
            "but it spends its calls"
        );
        assert_eq!(tally.absorb(&s.steps, &[said("- a new risk")], 2), 0);
        assert_eq!(tally.stop, Some(StopReason::Dry));

        let mut first = LoopTally::default();
        first.absorb(&s.steps, &[None], 2);
        assert_eq!(first.stop, Some(StopReason::Failed));
    }

    #[test]
    fn precheck_prevents_round_over_max_calls() {
        let s = spec(3, 2, 4, 7);
        let mut tally = LoopTally::default();
        assert!(tally.can_start_round(&s, true));
        tally.absorb(&s.steps, &[said("- a"), said("- b"), said("- c")], 2);
        assert!(tally.can_start_round(&s, true), "6 of 7 calls");
        assert!(
            !tally.can_start_round(&s, false),
            "the run budget has the last word"
        );
        tally.absorb(&s.steps, &[said("- d"), said("- e"), said("- f")], 2);
        assert_eq!(tally.calls, 6);
        assert!(
            !tally.can_start_round(&s, true),
            "a third round would make 9 > 7"
        );
        let rounds = spec(1, 2, 2, 16);
        let mut t = LoopTally::default();
        t.absorb(&rounds.steps, &[said("- a")], 2);
        t.absorb(&rounds.steps, &[said("- b")], 2);
        assert!(!t.can_start_round(&rounds, true), "max_rounds");
    }

    fn finding(text: &str) -> Finding {
        Finding {
            lenses: vec!["premortem".into()],
            role: AgentRole::Critic,
            text: text.into(),
        }
    }

    fn report(labels: &[Label]) -> AuditReport {
        AuditReport {
            verdicts: labels
                .iter()
                .map(|l| Verdict {
                    label: *l,
                    reason: "r".into(),
                })
                .collect(),
            answered: labels.len(),
            failed: false,
        }
    }

    #[tokio::test]
    async fn refine_skips_with_zero_calls_when_clean() {
        use crate::ai::{LlmBackend, OllamaClient};
        let dead = LlmBackend::ollama_only(OllamaClient::new("http://127.0.0.1:9", "m").unwrap());
        let sem = tokio::sync::Semaphore::new(1);
        let book = crate::concepts::workflows::Book::builtin();
        let tmp = tempfile::tempdir().unwrap();
        let ctx = RunCtx {
            llm: &dead,
            sem: &sem,
            book: &book,
            vault_dir: tmp.path(),
            idea_slug: "i",
            budget: crate::ai::budget::ContextBudget::new(4096),
            audit_on: true,
            related: &|_| String::new(),
            progress: &|_: &str| {},
        };
        let spec = RefineStage {
            step: WorkflowStep {
                role: AgentRole::Advocate,
                skill: Some("steelman".into()),
                angle: None,
            },
            max_rounds: 2,
        };
        let calls = CallBudget::new(&[2]);
        let mut findings = vec![finding("fine")];
        for mut audited in [
            Some(report(&[Label::Confirmed])),
            None,
            Some(AuditReport {
                failed: true,
                ..report(&[Label::Uncertain])
            }),
        ] {
            let out = run_refine(&ctx, &spec, &mut findings, &mut audited, &calls, &|_| {})
                .await
                .unwrap();
            assert!(
                matches!(out.status, StageStatus::Skipped(_)),
                "{:?}",
                out.status
            );
        }
        assert_eq!(calls.used(), 0);
        assert_eq!(findings[0].text, "fine");
    }

    #[test]
    fn refine_replaces_by_id_and_caps_rounds() {
        let audited = report(&[Label::Confirmed, Label::Refuted, Label::Uncertain]);
        let targets = refine_targets(Some(&audited));
        assert_eq!(targets, [1, 2]);
        let mut findings = vec![finding("one"), finding("two"), finding("three")];
        let raw = "Here:\n- F1: rewrite a confirmed one\n- F3: three, now grounded\n- **F2**: two, reworked\n- F2: a second answer for two\n- F9: out of range\n- no id";
        assert_eq!(apply_replacements(&mut findings, &targets, raw, 1), 2);
        assert_eq!(
            findings[0].text, "one",
            "a CONFIRMED finding is never rewritten"
        );
        assert_eq!(
            findings[1].text, "two, reworked",
            "the first answer per id wins"
        );
        assert_eq!(findings[2].text, "three, now grounded");
        assert_eq!(findings[2].lenses, ["premortem", "refined r1"]);
        apply_replacements(&mut findings, &targets, "- F3: again", 2);
        assert_eq!(findings[2].lenses, ["premortem", "refined r2"]);
        assert!(refine_targets(Some(&report(&[Label::Confirmed]))).is_empty());
        let still = report(&[Label::Refuted]);
        assert_eq!(next_refine_round(0, 2, Some(&still)), Some(vec![0]));
        assert_eq!(next_refine_round(1, 2, Some(&still)), Some(vec![0]));
        assert_eq!(
            next_refine_round(2, 2, Some(&still)),
            None,
            "capped at max_rounds"
        );
        assert_eq!(
            next_refine_round(1, 2, Some(&report(&[Label::Confirmed]))),
            None
        );
    }
}
