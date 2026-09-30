//! The Panel stage (docs/adr/0034, D36): competing proposals, each scored cold against a weighted
//! rubric, the winner picked and the runners-up's better parts grafted — all ranking in code.
//!
//! Each proposer answers from its own skill or angle with a `## Proposal` of at most eight
//! bullets. Each scorer is the hidden `panel-score` skill called as the Auditor (the cold judging
//! role — no new role, and the name "judge" stays with `swarm::judge`'s deterministic dedupe),
//! and sees exactly ONE proposal plus the rubric: no other proposal to anchor on, no position to
//! be biased by, no related-ideas block. A score is 0, 1 or 2 per criterion; a missing or garbled
//! line counts 1 and is flagged. Totals, tie-breaks and grafts are [`aggregate`]'s pure
//! arithmetic, so a model never decides who won.

use futures::future::join_all;

use crate::ai::contract::{self, score_line, ContractOutcome};
use crate::concepts::agents::{run_agent, AgentResult, AgentRole, AgentTask};
use crate::concepts::audit;
use crate::concepts::swarm::fan_out;
use crate::concepts::workflows::ground::cell;
use crate::concepts::workflows::run::{
    stage_context, PendingArtifact, RunCtx, StageOutcome, StageStatus,
};
use crate::concepts::workflows::PanelStage;
use crate::concepts::ConceptError;
use crate::domain::workflow::CriterionSpec;
use crate::domain::{ArtifactKind, OutputContract};
use crate::vault::store;

/// The stage budget divisor the carried scorecard may take (a sixth): it is a table and a line
/// or two, and the synthesizer after it needs the room for the proposals themselves.
const SCORECARD_DIVISOR: usize = 6;

/// The skill every scorer runs through.
const SCORER_SKILL: &str = "panel-score";

/// Every proposer's answer shape, whatever its skill says: the stage parses proposals as bullets.
const PROPOSAL_FORMAT: &str = "## Answer format\nAnswer with a `## Proposal` heading over at most \
8 markdown bullets: one concrete proposal for how to take this idea forward, from your angle. No \
preamble.";

/// One scorer's grades for one proposal: a score per criterion, and which were not given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoreRow {
    pub scores: Vec<u8>,
    pub unscored: Vec<bool>,
}

impl ScoreRow {
    /// A row nobody filled in — a failed scorer call — every cell 1 and flagged.
    pub fn unscored(n_criteria: usize) -> Self {
        ScoreRow {
            scores: vec![1; n_criteria],
            unscored: vec![true; n_criteria],
        }
    }
}

/// Parse a scorer's `C<i>: <0|1|2> — reason` lines for `n_criteria` criteria. The first line per
/// criterion wins and line order does not matter; a criterion with no usable line scores 1 and is
/// flagged unscored.
pub fn parse_scorecard(raw: &str, n_criteria: usize) -> ScoreRow {
    let mut row = ScoreRow::unscored(n_criteria);
    for (id, score, _) in raw.lines().filter_map(score_line) {
        if (1..=n_criteria).contains(&id) && row.unscored[id - 1] {
            row.scores[id - 1] = score;
            row.unscored[id - 1] = false;
        }
    }
    row
}

/// A runner-up's bullets taken into the winner on one criterion it beat the winner on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Graft {
    /// The runner-up's position among the scored proposals.
    pub from: usize,
    pub criterion: usize,
}

/// The panel's result, by position among the scored proposals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ranking {
    /// The combined score per proposal per criterion.
    pub cells: Vec<Vec<u8>>,
    /// Cells a scorer left unscored (counted as 1).
    pub unscored: Vec<Vec<bool>>,
    /// Σ weight·score per proposal.
    pub totals: Vec<u32>,
    /// Positions best first.
    pub order: Vec<usize>,
    pub winner: usize,
    pub grafts: Vec<Graft>,
    /// Every cell of every proposal the same score: a scorecard that ranked nothing.
    pub uniform: bool,
}

/// Rank the proposals (docs/adr/0034). `rows[p]` holds every scorer's row for proposal `p`; with
/// two scorers each cell is their median, which on a split is the lower score. The total is
/// Σ weight·score; ties go to fewer zeros, then the higher score on the first top-weight
/// criterion, then the lower position. A graft is taken for each criterion where a runner-up's
/// cell beats the winner's — the best such runner-up, ties to the higher-ranked. Pure.
pub fn aggregate(rows: &[Vec<ScoreRow>], criteria: &[CriterionSpec]) -> Ranking {
    let n_criteria = criteria.len();
    let (cells, unscored): (Vec<Vec<u8>>, Vec<Vec<bool>>) = rows
        .iter()
        .map(|judged| {
            (0..n_criteria)
                .map(|c| {
                    let score = judged
                        .iter()
                        .map(|r| r.scores.get(c).copied().unwrap_or(1))
                        .min()
                        .unwrap_or(1);
                    let flagged = judged.is_empty()
                        || judged
                            .iter()
                            .any(|r| r.unscored.get(c).copied().unwrap_or(true));
                    (score, flagged)
                })
                .unzip()
        })
        .unzip();
    let totals: Vec<u32> = cells
        .iter()
        .map(|row| {
            row.iter()
                .zip(criteria)
                .map(|(s, c)| u32::from(*s) * u32::from(c.weight))
                .sum()
        })
        .collect();
    let top = criteria
        .iter()
        .enumerate()
        .max_by(|(i, a), (j, b)| a.weight.cmp(&b.weight).then(j.cmp(i)))
        .map_or(0, |(i, _)| i);
    let zeros = |p: usize| cells[p].iter().filter(|s| **s == 0).count();
    let mut order: Vec<usize> = (0..cells.len()).collect();
    order.sort_by(|&a, &b| {
        totals[b]
            .cmp(&totals[a])
            .then(zeros(a).cmp(&zeros(b)))
            .then(cells[b].get(top).cmp(&cells[a].get(top)))
            .then(a.cmp(&b))
    });
    let winner = order.first().copied().unwrap_or(0);
    let grafts = (0..n_criteria)
        .filter_map(|c| {
            let best = order[1..]
                .iter()
                .filter(|&&k| cells[k][c] > cells[winner][c])
                .max_by(|&&a, &&b| {
                    let rank = |p| order.iter().position(|o| *o == p);
                    cells[a][c].cmp(&cells[b][c]).then(rank(b).cmp(&rank(a)))
                })?;
            Some(Graft {
                from: *best,
                criterion: c,
            })
        })
        .collect();
    let first = cells.first().and_then(|r| r.first()).copied();
    let uniform = cells.len() >= 2 && cells.iter().flatten().all(|s| Some(*s) == first);
    Ranking {
        cells,
        unscored,
        totals,
        order,
        winner,
        grafts,
        uniform,
    }
}

/// Which proposals go on to be scored: `Contest` with every usable one when at least two
/// answered, else `NoContest` with whatever survived — nothing to rank, so no scorer is called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Contest {
    Contest(Vec<usize>),
    NoContest(Vec<usize>),
}

/// Sort the proposers' answers into a [`Contest`].
pub fn contest(proposals: &[Option<String>]) -> Contest {
    let survivors: Vec<usize> = proposals
        .iter()
        .enumerate()
        .filter(|(_, p)| p.as_deref().is_some_and(|t| !t.trim().is_empty()))
        .map(|(i, _)| i)
        .collect();
    if survivors.len() >= 2 {
        Contest::Contest(survivors)
    } else {
        Contest::NoContest(survivors)
    }
}

/// Drop every `Grafted from P<k>` line whose `k` is not one of the scored proposal `labels` or is
/// the `winner` itself — the synthesizer cannot graft from a proposal that does not exist, or
/// graft the spine into itself. Returns the text and the dropped lines.
pub fn strip_invalid_grafts(text: &str, labels: &[usize], winner: usize) -> (String, Vec<String>) {
    let mut kept: Vec<&str> = Vec::new();
    let mut dropped: Vec<String> = Vec::new();
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        let named = lower.find("grafted from p").map(|at| {
            let digits: String = lower[at + "grafted from p".len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            digits.parse::<usize>().ok()
        });
        match named {
            Some(Some(k)) if labels.contains(&k) && k != winner => kept.push(line),
            Some(_) => dropped.push(line.trim().to_string()),
            None => kept.push(line),
        }
    }
    (kept.join("\n"), dropped)
}

/// The rubric every scorer sees, criteria numbered `C1..`.
fn rubric(criteria: &[CriterionSpec]) -> String {
    let lines: Vec<String> = criteria
        .iter()
        .enumerate()
        .map(|(i, c)| {
            format!(
                "C{} {} (weight {}): 0 = {}; 2 = {}",
                i + 1,
                c.name,
                c.weight,
                c.zero.trim(),
                c.two.trim()
            )
        })
        .collect();
    format!("## Rubric\n{}", lines.join("\n"))
}

fn max_total(criteria: &[CriterionSpec]) -> u32 {
    criteria.iter().map(|c| u32::from(c.weight) * 2).sum()
}

/// The scorecard as a markdown table, one row per scored proposal (by label), unscored cells
/// starred.
fn table(ranking: &Ranking, labels: &[usize], criteria: &[CriterionSpec]) -> String {
    let head: Vec<String> = criteria
        .iter()
        .map(|c| format!("{} ×{}", c.name, c.weight))
        .collect();
    let mut out = format!(
        "| | {} | total |\n|---|{}---|\n",
        head.join(" | "),
        "---|".repeat(criteria.len())
    );
    for (p, label) in labels.iter().enumerate() {
        let cells: Vec<String> = ranking.cells[p]
            .iter()
            .zip(&ranking.unscored[p])
            .map(|(s, u)| if *u { format!("{s}*") } else { s.to_string() })
            .collect();
        out.push_str(&format!(
            "| P{label} | {} | {}/{} |\n",
            cells.join(" | "),
            ranking.totals[p],
            max_total(criteria)
        ));
    }
    out
}

/// The winner, the grafts and any flags, as prose lines.
fn verdict_lines(ranking: &Ranking, labels: &[usize], criteria: &[CriterionSpec]) -> String {
    let w = ranking.winner;
    let mut out = format!(
        "Winner: P{} ({}/{}).",
        labels[w],
        ranking.totals[w],
        max_total(criteria)
    );
    if ranking.grafts.is_empty() {
        out.push_str(" Grafts: none.");
    } else {
        let grafts: Vec<String> = ranking
            .grafts
            .iter()
            .map(|g| format!("P{} on {}", labels[g.from], criteria[g.criterion].name))
            .collect();
        out.push_str(&format!(" Grafts: {}.", grafts.join(", ")));
    }
    if ranking.uniform {
        out.push_str(
            "\nFlag: uniform scorecard — every cell the same, so the ranking is by tie-break only.",
        );
    }
    if ranking.unscored.iter().flatten().any(|u| *u) {
        out.push_str("\nFlag: cells marked * were not scored and count as 1.");
    }
    out
}

/// What the Synthesize stage after a Panel runs under (graft mode): the winner as the spine, only
/// the listed grafts added, each marked so code can check it names a real runner-up.
fn graft_directive(ranking: &Ranking, labels: &[usize], criteria: &[CriterionSpec]) -> String {
    let w = labels[ranking.winner];
    let mut out = format!(
        "## How to converge this panel\nThe findings come from competing proposals; each finding's \
         lens names its proposal (panel-p<k>). A scored panel chose P{w}.\n\
         - Take P{w} as the spine of the position.\n"
    );
    if ranking.grafts.is_empty() {
        out.push_str("- Add nothing from the other proposals.\n");
    } else {
        out.push_str(
            "- Add only these grafts, and start each line you add with `Grafted from P<k>:`\n",
        );
        for g in &ranking.grafts {
            out.push_str(&format!(
                "  - from P{} on {}\n",
                labels[g.from], criteria[g.criterion].name
            ));
        }
    }
    out.push_str("- Drop anything the audit REFUTED.");
    out
}

/// The graft-mode directive and the labels a `Grafted from P<k>` line may name, for the
/// Synthesize stage after this Panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PanelVerdict {
    pub labels: Vec<usize>,
    pub winner: usize,
    pub directive: String,
}

/// What one Panel stage hands the engine.
pub(crate) struct PanelRun {
    pub outcome: StageOutcome,
    /// One per proposer, in proposer order: the proposal as a finding source (lens
    /// `panel-p<k>`, the winner tagged), `None` for a proposer that failed or said nothing.
    pub results: Vec<Option<AgentResult>>,
    pub carry: Option<String>,
    pub verdict: Option<PanelVerdict>,
}

/// Run one Panel stage over the `carried` blocks so far.
pub(crate) async fn run_panel(
    ctx: &RunCtx<'_>,
    panel: &PanelStage,
    carried: &[String],
    note: &(dyn Fn(&str) + Sync),
) -> Result<PanelRun, ConceptError> {
    let n = panel.proposers.len();
    note(&format!("{n} proposals"));
    let shared = stage_context(
        ctx.vault_dir,
        ctx.idea_slug,
        ctx.budget,
        carried,
        ctx.related,
    )?;
    let tasks = panel
        .proposers
        .iter()
        .map(|p| {
            let angle = p
                .angle
                .as_deref()
                .map_or(String::new(), |a| format!("## Your angle\n{a}\n\n"));
            AgentTask {
                role: p.role,
                skill: p.skill.clone(),
                context: format!("{angle}{PROPOSAL_FORMAT}\n\n{shared}"),
            }
        })
        .collect();
    let on_done = |done: usize, of: usize, _: &str| {
        note(&format!("proposal {done}/{of}"));
    };
    let raw = fan_out(ctx.llm, ctx.sem, &ctx.book.skills, tasks, &on_done).await;
    let allowance = audit::finding_allowance(ctx.budget, n);
    let proposals: Vec<Option<String>> = raw
        .iter()
        .map(|r| {
            let r = r.as_ref()?;
            let shaped = contract::validate(OutputContract::Proposal, &r.content)
                .unwrap_or_else(|_| r.content.trim().to_string());
            Some(audit::clip(&shaped, allowance)).filter(|t| !t.trim().is_empty())
        })
        .collect();
    let as_result = |i: usize, winner: bool| {
        proposals[i].as_ref().map(|text| AgentResult {
            role: panel.proposers[i].role,
            lens: Some(if winner {
                format!("panel-p{} (winner)", i + 1)
            } else {
                format!("panel-p{}", i + 1)
            }),
            content: text.clone(),
            contract: ContractOutcome::Clean,
        })
    };

    let survivors = match contest(&proposals) {
        Contest::Contest(s) => s,
        Contest::NoContest(s) => {
            let why = format!("no contest — {} of {n} proposals answered", s.len());
            note(&why);
            return Ok(PanelRun {
                outcome: StageOutcome {
                    status: StageStatus::Degraded(why.clone()),
                    detail: why,
                    artifact: None,
                },
                results: (0..n).map(|i| as_result(i, false)).collect(),
                carry: None,
                verdict: None,
            });
        }
    };

    // Scorers: each sees one proposal, the rubric and the idea — never another proposal and never
    // the related-ideas block.
    let statement = store::read_idea(ctx.vault_dir, ctx.idea_slug)?.body;
    let idea = audit::clip(statement.trim(), ctx.budget.max_bytes / 4);
    let rubric = rubric(&panel.criteria);
    let judges = panel.judges.max(1);
    let total = survivors.len() * judges;
    let done = std::sync::atomic::AtomicUsize::new(0);
    let scored = join_all(
        survivors
            .iter()
            .flat_map(|&p| (0..judges).map(move |_| p))
            .map(|p| {
                let task = AgentTask {
                    role: AgentRole::Auditor,
                    skill: Some(SCORER_SKILL.to_string()),
                    context: format!(
                        "## The idea\n{idea}\n\n{rubric}\n\n{}",
                        proposals[p].as_deref().unwrap_or_default()
                    ),
                };
                let done = &done;
                async move {
                    let answer = run_agent(ctx.llm, ctx.sem, &ctx.book.skills, task).await;
                    let k = done.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    note(&format!("scoring {k}/{total}"));
                    answer
                }
            }),
    )
    .await;
    let mut rows: Vec<Vec<ScoreRow>> = vec![Vec::new(); survivors.len()];
    for (k, answer) in scored.into_iter().enumerate() {
        let row = match answer {
            Ok(r) => parse_scorecard(&r.content, panel.criteria.len()),
            Err(ConceptError::SemaphoreClosed) => return Err(ConceptError::SemaphoreClosed),
            Err(e) => {
                tracing::warn!(error = %e, "panel scorer failed; row left unscored");
                ScoreRow::unscored(panel.criteria.len())
            }
        };
        rows[k / judges].push(row);
    }
    let ranking = aggregate(&rows, &panel.criteria);
    let labels: Vec<usize> = survivors.iter().map(|p| p + 1).collect();
    let winner = survivors[ranking.winner];
    note(&format!(
        "P{} wins {}/{}",
        winner + 1,
        ranking.totals[ranking.winner],
        max_total(&panel.criteria)
    ));

    let table = table(&ranking, &labels, &panel.criteria);
    let verdict = verdict_lines(&ranking, &labels, &panel.criteria);
    let carry = audit::clip(
        &format!("## Prior stage: panel scorecard\n{table}\n{verdict}"),
        ctx.budget.max_bytes / SCORECARD_DIVISOR,
    );
    let mut body = format!("# Panel scorecard\n\n{}\n\n{table}\n{verdict}\n", rubric);
    for (i, p) in panel.proposers.iter().enumerate() {
        let tag = if i == winner { " (winner)" } else { "" };
        let who = match (&p.skill, &p.angle) {
            (Some(s), Some(a)) => format!("{} · {s} · {a}", p.role.as_str()),
            (Some(s), None) => format!("{} · {s}", p.role.as_str()),
            (None, Some(a)) => format!("{} · {a}", p.role.as_str()),
            (None, None) => p.role.as_str().to_string(),
        };
        let text = proposals[i]
            .as_deref()
            .map_or("_no proposal_".to_string(), |t| {
                t.trim_start_matches("## Proposal").trim().to_string()
            });
        body.push_str(&format!(
            "\n## P{}{tag} — {}\n\n{text}\n",
            i + 1,
            cell(&who)
        ));
    }
    let status = if ranking.uniform {
        StageStatus::Degraded("uniform scorecard".into())
    } else {
        StageStatus::Ran
    };
    Ok(PanelRun {
        outcome: StageOutcome {
            status,
            detail: verdict.lines().next().unwrap_or_default().to_string(),
            artifact: Some(PendingArtifact {
                kind: ArtifactKind::Scorecard,
                title: "Panel scorecard".into(),
                lens: None,
                body,
            }),
        },
        results: (0..n).map(|i| as_result(i, i == winner)).collect(),
        carry: Some(carry),
        verdict: Some(PanelVerdict {
            directive: graft_directive(&ranking, &labels, &panel.criteria),
            labels,
            winner: winner + 1,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn criteria() -> Vec<CriterionSpec> {
        [("cost", 2), ("risk", 2), ("fit", 1), ("evidence", 1)]
            .into_iter()
            .map(|(name, weight)| CriterionSpec {
                name: name.into(),
                weight,
                zero: "bad".into(),
                two: "good".into(),
            })
            .collect()
    }

    fn row(scores: &[u8]) -> ScoreRow {
        ScoreRow {
            scores: scores.to_vec(),
            unscored: vec![false; scores.len()],
        }
    }

    #[test]
    fn parse_scorecard_missing_scores_one_flagged() {
        let raw = "C1: 2 — cheap\nC3: banana\nC4: 0 — none\nC9: 2 — no such criterion\nC1: 0 — second line ignored";
        let parsed = parse_scorecard(raw, 4);
        assert_eq!(parsed.scores, [2, 1, 1, 0]);
        assert_eq!(parsed.unscored, [false, true, true, false]);
        assert_eq!(parse_scorecard("", 2), ScoreRow::unscored(2));
    }

    #[test]
    fn aggregate_weighted_totals_and_tiebreak_order() {
        let c = criteria();
        // Every total is 6. P1 has no zero; P2 and P3 have one each and P3 scores higher on
        // cost, the first top-weight criterion; P0 has two.
        let rows = vec![
            vec![row(&[2, 0, 2, 0])],
            vec![row(&[1, 1, 1, 1])],
            vec![row(&[0, 2, 1, 1])],
            vec![row(&[2, 0, 1, 1])],
        ];
        let ranking = aggregate(&rows, &c);
        assert_eq!(ranking.totals, [6, 6, 6, 6]);
        assert_eq!(ranking.order, [1, 3, 2, 0]);
        assert_eq!(ranking.winner, 1);
        let rows = vec![vec![row(&[1, 1, 1, 1])], vec![row(&[1, 1, 1, 1])]];
        let tied = aggregate(&rows, &c);
        assert_eq!(tied.winner, 0, "a full tie goes to the lower position");
        assert!(tied.uniform);
    }

    #[test]
    fn median_of_two_takes_lower_on_split() {
        let c = criteria();
        let rows = vec![
            vec![row(&[2, 1, 0, 2]), row(&[1, 1, 2, 2])],
            vec![row(&[0, 0, 0, 0]), row(&[0, 0, 0, 0])],
        ];
        let ranking = aggregate(&rows, &c);
        assert_eq!(ranking.cells[0], [1, 1, 0, 2]);
        let mut flagged = row(&[2, 2, 2, 2]);
        flagged.unscored[3] = true;
        let ranking = aggregate(&[vec![row(&[2, 2, 2, 2]), flagged]], &c);
        assert_eq!(ranking.unscored[0], [false, false, false, true]);
    }

    #[test]
    fn grafts_only_where_runner_up_beats_winner() {
        let c = criteria();
        let rows = vec![
            vec![row(&[2, 2, 0, 0])], // winner: 8
            vec![row(&[1, 1, 2, 0])], // 6: beats the winner on fit
            vec![row(&[0, 1, 1, 2])], // 4: beats it on fit (less) and evidence
        ];
        let ranking = aggregate(&rows, &c);
        assert_eq!(ranking.winner, 0);
        assert_eq!(
            ranking.grafts,
            [
                Graft {
                    from: 1,
                    criterion: 2
                },
                Graft {
                    from: 2,
                    criterion: 3
                },
            ]
        );
    }

    #[test]
    fn invalid_graft_ids_stripped() {
        let text = "Spine from P2.\nGrafted from P1: cheaper storage\n- **Grafted from P7:** made up\ngrafted from p2: itself\nGrafted from P3: risk hedge\nGrafted from Px: garbled";
        let (kept, dropped) = strip_invalid_grafts(text, &[1, 2, 3], 2);
        assert_eq!(
            kept,
            "Spine from P2.\nGrafted from P1: cheaper storage\nGrafted from P3: risk hedge"
        );
        assert_eq!(dropped.len(), 3, "{dropped:?}");
    }

    #[test]
    fn fewer_than_two_proposals_is_no_contest() {
        assert_eq!(
            contest(&[Some("- a".into()), None, Some("  ".into())]),
            Contest::NoContest(vec![0])
        );
        assert_eq!(contest(&[None, None]), Contest::NoContest(vec![]));
        assert_eq!(
            contest(&[Some("- a".into()), None, Some("- c".into())]),
            Contest::Contest(vec![0, 2])
        );
    }

    #[test]
    fn shuffled_score_lines_give_identical_ranking() {
        let c = criteria();
        let answers = [
            ["C1: 2 — a", "C2: 1 — b", "C3: 0 — c", "C4: 2 — d"],
            ["C1: 1 — a", "C2: 2 — b", "C3: 2 — c", "C4: 0 — d"],
        ];
        let rows = |shuffle: bool| -> Vec<Vec<ScoreRow>> {
            answers
                .iter()
                .map(|lines| {
                    let mut lines = lines.to_vec();
                    if shuffle {
                        lines.reverse();
                        lines.swap(0, 2);
                    }
                    vec![parse_scorecard(&lines.join("\n"), c.len())]
                })
                .collect()
        };
        assert_eq!(aggregate(&rows(false), &c), aggregate(&rows(true), &c));
    }
}
