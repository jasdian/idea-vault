//! Plan lineage (docs/adr/0032): every build plan of an idea is a version in one linear chain,
//! linked by the `revises` frontmatter field. A plan written before lineage existed has no
//! `revises` and reads as a version-1 root. The owner answers recorded on the chain's Settled
//! items travel forward into every later version, so an answered question is never asked again.

use std::collections::BTreeSet;
use std::path::Path;

use chrono::{DateTime, Utc};

use crate::concepts::audit::clip;
use crate::concepts::build_plan::gates::claims::collides;
use crate::concepts::build_plan::gates::{Answered, GateReport};
use crate::concepts::build_plan::plan::{self, next_free_id, refs_of, BuildPlan, Item};
use crate::concepts::ConceptError;
use crate::domain::ArtifactKind;
use crate::vault::store;

/// The most bytes of an owner answer a Settled item's one-line text carries; the full answer is
/// its `quote`.
pub const ANSWER_TEXT_BYTES: usize = 200;

/// One build-plan artifact's place in its lineage, read from its frontmatter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRef {
    pub stem: String,
    pub revises: Option<String>,
    pub version: u32,
    pub answered: Vec<String>,
    pub created: DateTime<Utc>,
    pub lens: Option<String>,
}

fn by_age(a: &PlanRef, b: &PlanRef) -> std::cmp::Ordering {
    a.created.cmp(&b.created).then_with(|| a.stem.cmp(&b.stem))
}

/// Every readable build-plan artifact of `slug`, oldest first. An unreadable artifact is skipped,
/// never fatal: a plan is never lost to a bad sibling file.
pub fn list_plans(vault_dir: &Path, slug: &str) -> Result<Vec<PlanRef>, ConceptError> {
    let mut plans = Vec::new();
    for file in store::list_artifact_files(vault_dir, slug)?
        .into_iter()
        .filter(|f| f.ext == store::ArtifactExt::Md)
    {
        let Ok(artifact) = store::read_artifact(vault_dir, slug, &file.slug) else {
            continue;
        };
        let fm = artifact.frontmatter;
        if fm.kind != ArtifactKind::BuildPlan {
            continue;
        }
        plans.push(PlanRef {
            stem: file.slug,
            revises: fm.revises,
            version: fm.version.unwrap_or(1),
            answered: fm.answered,
            created: fm.created,
            lens: fm.lens,
        });
    }
    plans.sort_by(by_age);
    Ok(plans)
}

/// The lineage's head: the newest plan no other plan revises. Several unrevised plans (legacy
/// roots) resolve to the newest of them.
pub fn head(plans: &[PlanRef]) -> Option<&PlanRef> {
    let revised: BTreeSet<&str> = plans.iter().filter_map(|p| p.revises.as_deref()).collect();
    plans
        .iter()
        .filter(|p| !revised.contains(p.stem.as_str()))
        .max_by(|a, b| by_age(a, b))
        .or_else(|| plans.iter().max_by(|a, b| by_age(a, b)))
}

/// `stem` and every plan it revises, back to its root, newest first. Stops at a missing link or
/// a loop.
pub fn chain<'a>(plans: &'a [PlanRef], stem: &str) -> Vec<&'a PlanRef> {
    let mut out: Vec<&PlanRef> = Vec::new();
    let mut at = Some(stem);
    while let Some(s) = at {
        let Some(p) = plans.iter().find(|p| p.stem == s) else {
            break;
        };
        if out.iter().any(|q| q.stem == p.stem) {
            break;
        }
        out.push(p);
        at = p.revises.as_deref();
    }
    out
}

/// The stems of the plans that revise `stem`.
pub fn successors(plans: &[PlanRef], stem: &str) -> Vec<String> {
    plans
        .iter()
        .filter(|p| p.revises.as_deref() == Some(stem))
        .map(|p| p.stem.clone())
        .collect()
}

/// Every owner answer recorded on `stem`'s chain: each item carrying `answers`, wherever the
/// gates left it (Settled, Verify first, Open or Quarantined, the order the workbench's
/// `answer_holder` reads), so an answer a gate moved out of Settled is still never re-asked
/// (docs/adr/0032). When a question id was answered more than once, the newest answer wins: the
/// chain is walked newest plan first and, within a plan, last item first, since an answer is
/// appended after any item an older version carried under the same id. Oldest answer first.
pub fn answered_in_lineage(
    vault_dir: &Path,
    slug: &str,
    stem: &str,
) -> Result<Vec<Answered>, ConceptError> {
    let plans = list_plans(vault_dir, slug)?;
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    for p in chain(&plans, stem) {
        let Ok(parsed) = store::read_artifact(vault_dir, slug, &p.stem)
            .map_err(|_| ())
            .and_then(|a| plan::parse_artifact(&a.body).map_err(|_| ()))
        else {
            continue;
        };
        let holders = parsed
            .settled
            .iter()
            .chain(&parsed.verify)
            .chain(&parsed.open)
            .chain(&parsed.quarantined);
        for item in holders.rev() {
            let Some(qid) = item.field("answers") else {
                continue;
            };
            if !seen.insert(qid.to_string()) {
                continue;
            }
            out.push(Answered {
                qid: qid.to_string(),
                asked: item.field("asked").unwrap_or_default().to_string(),
                answer: item.field("quote").unwrap_or(&item.text).to_string(),
                in_stem: item.field("in").unwrap_or(&p.stem).to_string(),
            });
        }
    }
    out.reverse();
    Ok(out)
}

/// A Settled item holding `answer` as the owner's answer to `answer.qid`.
pub fn answer_item(id: &str, answer: &Answered) -> Item {
    let mut item = Item::new(id, &clip(&answer.answer, ANSWER_TEXT_BYTES));
    item.fields
        .insert("quote".to_string(), answer.answer.clone());
    item.fields
        .insert("answers".to_string(), answer.qid.clone());
    item.fields
        .insert("asked".to_string(), answer.asked.clone());
    item.fields.insert("in".to_string(), answer.in_stem.clone());
    item
}

fn unquoted(text: &str) -> &str {
    text.trim()
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '“' | '”'))
        .trim()
}

/// Keep every owner answer in a fresh model plan: a Settled item quoting the answer gets its
/// owner fields back (the untrusted parse dropped them), and an answer the model left out is
/// re-inserted as its own Settled item.
pub fn carry_answers(plan: &mut BuildPlan, answers: &[Answered]) {
    for a in answers {
        if plan
            .settled
            .iter()
            .any(|s| s.field("answers") == Some(a.qid.as_str()))
        {
            continue;
        }
        let copied = plan.settled.iter_mut().find(|s| {
            s.field("quote")
                .is_some_and(|q| unquoted(q) == a.answer.trim())
        });
        match copied {
            Some(item) => {
                for (k, v) in [("answers", &a.qid), ("asked", &a.asked), ("in", &a.in_stem)] {
                    item.fields.insert(k.to_string(), v.clone());
                }
            }
            None => {
                let id = next_free_id(&plan.settled, 'S');
                plan.settled.push(answer_item(&id, a));
            }
        }
    }
}

/// Give a fresh `Q#` to every open question of a fresh model plan whose id an owner answer in
/// the lineage already holds, and follow the rename in the tasks' `depends`. Runs after
/// [`carry_answers`] and [`suppress_answered`]: a question still standing then is a different
/// question, and the model's reuse of the number (the prompt asks it to keep ids, nothing
/// enforces it) would otherwise make its answer collide with the carried one.
pub fn renumber_reused_questions(plan: &mut BuildPlan, answers: &[Answered]) {
    let taken: BTreeSet<&str> = answers
        .iter()
        .map(|a| a.qid.as_str())
        .chain(plan.settled.iter().filter_map(|s| s.field("answers")))
        .collect();
    let answered_max = taken
        .iter()
        .filter_map(|q| q.strip_prefix('Q')?.parse::<usize>().ok())
        .max()
        .unwrap_or(0);
    let clashing: Vec<usize> = (0..plan.open.len())
        .filter(|&i| taken.contains(plan.open[i].id.as_str()))
        .collect();
    if clashing.is_empty() {
        return;
    }
    let mut next = plan
        .open
        .iter()
        .filter_map(|q| q.id.strip_prefix('Q')?.parse::<usize>().ok())
        .max()
        .unwrap_or(0)
        .max(answered_max);
    let mut renamed: Vec<(String, String)> = Vec::new();
    for i in clashing {
        next += 1;
        let old = std::mem::replace(&mut plan.open[i].id, format!("Q{next}"));
        renamed.push((old, format!("Q{next}")));
    }
    plan.rename_question_refs(&renamed);
}

/// Remove `ids` from `task`'s `depends`: an entry whose question ids are all in `ids` goes whole,
/// its annotation with it.
pub(crate) fn drop_question_deps(task: &mut Item, ids: &[String]) {
    let Some(value) = task.field("depends") else {
        return;
    };
    let kept: Vec<String> = value
        .split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .filter(|e| {
            let refs = refs_of(e, 'Q');
            refs.is_empty() || !refs.iter().all(|r| ids.contains(r))
        })
        .map(str::to_string)
        .collect();
    if kept.is_empty() {
        task.fields.remove("depends");
    } else {
        task.fields.insert("depends".to_string(), kept.join(", "));
    }
}

/// Before the gates run, drop every Open question that re-asks an answered one (it collides with
/// the question as asked or with the owner's answer), rewrite the task `depends` that cited it
/// and note each drop in `report`. Runs after [`carry_answers`], so the answer's Settled item is
/// in the plan to name.
pub fn suppress_answered(plan: &mut BuildPlan, answers: &[Answered], report: &mut GateReport) {
    let mut dropped: Vec<String> = Vec::new();
    let mut kept = Vec::new();
    for q in std::mem::take(&mut plan.open) {
        let own = q.text.strip_prefix("proposed:").unwrap_or(&q.text).trim();
        let hit = answers
            .iter()
            .find(|a| (!a.asked.is_empty() && collides(own, &a.asked)) || collides(own, &a.answer));
        let Some(a) = hit else {
            kept.push(q);
            continue;
        };
        let holder = plan
            .settled
            .iter()
            .find(|s| s.field("answers") == Some(a.qid.as_str()))
            .map_or("Settled", |s| s.id.as_str());
        report.note(format!(
            "{} dropped: already answered in {holder} ({})",
            q.id, a.in_stem
        ));
        dropped.push(q.id);
    }
    plan.open = kept;
    if dropped.is_empty() {
        return;
    }
    for task in &mut plan.tasks {
        drop_question_deps(task, &dropped);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::frontmatter::ArtifactFrontmatter;
    use crate::domain::{Artifact, Idea, IdeaFrontmatter, IdeaState};
    use chrono::TimeZone;

    fn at(minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 29, 12, minute, 0).unwrap()
    }

    fn plan_ref(stem: &str, revises: Option<&str>, minute: u32) -> PlanRef {
        PlanRef {
            stem: stem.into(),
            revises: revises.map(str::to_string),
            version: 1,
            answered: vec![],
            created: at(minute),
            lens: None,
        }
    }

    fn answered() -> Answered {
        Answered {
            qid: "Q6".into(),
            asked: "Freeze the zone snapshot at entry, or dwell on the live label?".into(),
            answer: "We freeze the zone snapshot at entry and never dwell.".into(),
            in_stem: "20260929-120000-build-plan".into(),
        }
    }

    #[test]
    fn head_is_newest_unrevised() {
        let plans = vec![
            plan_ref("a", None, 0),
            plan_ref("b", Some("a"), 1),
            plan_ref("c", Some("b"), 2),
            plan_ref("old", None, 3),
        ];
        assert_eq!(head(&plans).unwrap().stem, "old");
        let linked = &plans[..3];
        assert_eq!(head(linked).unwrap().stem, "c");
        let stems: Vec<&str> = chain(linked, "c").iter().map(|p| p.stem.as_str()).collect();
        assert_eq!(stems, ["c", "b", "a"]);
        assert_eq!(successors(linked, "a"), ["b"]);
        assert!(successors(linked, "c").is_empty());
        let forked = vec![
            plan_ref("a", None, 0),
            plan_ref("b", Some("a"), 1),
            plan_ref("b2", Some("a"), 5),
        ];
        assert_eq!(head(&forked).unwrap().stem, "b2");
        assert!(head(&[]).is_none());
    }

    #[test]
    fn legacy_plans_are_version_one_roots() {
        let dir = tempfile::tempdir().unwrap();
        store::write_idea(
            dir.path(),
            &Idea {
                frontmatter: IdeaFrontmatter {
                    title: "Legacy".into(),
                    slug: "legacy".into(),
                    state: IdeaState::InDiscussion,
                    tags: vec![],
                    sources: vec![],
                    created: at(0),
                    updated: at(0),
                },
                body: "An idea.\n".into(),
            },
        )
        .unwrap();
        for (stem, kind, minute) in [
            ("20260929-120000-build-plan", ArtifactKind::BuildPlan, 0),
            ("20260929-120500-open-questions", ArtifactKind::Finding, 5),
            ("20260929-121000-build-plan", ArtifactKind::BuildPlan, 10),
        ] {
            store::write_artifact(
                dir.path(),
                "legacy",
                &Artifact {
                    frontmatter: ArtifactFrontmatter {
                        slug: stem.into(),
                        title: "x".into(),
                        kind,
                        lens: None,
                        created: at(minute),
                        model: "m".into(),
                        revises: None,
                        version: None,
                        answered: Vec::new(),
                        recipe: None,
                    },
                    body: "## Goal\nx\n".into(),
                },
            )
            .unwrap();
        }
        let plans = list_plans(dir.path(), "legacy").unwrap();
        assert_eq!(plans.len(), 2, "only build plans: {plans:?}");
        assert!(plans.iter().all(|p| p.version == 1 && p.revises.is_none()));
        assert_eq!(head(&plans).unwrap().stem, "20260929-121000-build-plan");
        assert_eq!(chain(&plans, "20260929-121000-build-plan").len(), 1);
    }

    #[test]
    fn suppress_answered_drops_reasked_q_and_rewrites_depends() {
        let mut plan = plan::parse(
            "## Goal\nShip it.\n\n## Settled\n- S1: The parser is first.\n  quote: \"parser first\"\n\n\
## Open questions\n- Q1: Should the zone snapshot freeze at entry or dwell on the label?\n\
- Q2: Which exchange feeds the backtest?\n\n\
## Plan\n- [ ] T1: Build the zone snapshot\n  depends: Q1 (which spread), Q2\n\
- [ ] T2: Wire it\n  depends: T1, Q1\n",
        )
        .unwrap();
        let answers = [answered()];
        carry_answers(&mut plan, &answers);
        let mut report = GateReport::default();
        suppress_answered(&mut plan, &answers, &mut report);
        let open: Vec<&str> = plan.open.iter().map(|q| q.id.as_str()).collect();
        assert_eq!(open, ["Q2"]);
        assert_eq!(plan.tasks[0].field("depends"), Some("Q2"));
        assert_eq!(plan.tasks[1].field("depends"), Some("T1"));
        assert_eq!(
            report.notes,
            ["Q1 dropped: already answered in S2 (20260929-120000-build-plan)"]
        );
    }

    /// An answer to Q2, as a re-plan carries it into the model's fresh plan.
    fn answered_q2() -> Answered {
        Answered {
            qid: "Q2".into(),
            asked: "Which exchange feeds the backtest data?".into(),
            answer: "Binance spot data, the free daily candles only.".into(),
            in_stem: "20260929-120000-build-plan".into(),
        }
    }

    #[test]
    fn replan_reusing_an_answered_id_gets_a_fresh_one() {
        // The model re-numbered: its Q2 is a new question, not the answered exchange one.
        let mut plan = plan::parse(
            "## Goal\nShip it.\n\n## Open questions\n- Q1: Freeze at entry or dwell on the label?\n\
- Q2: How many markets does the first release cover?\n\n\
## Plan\n- [ ] T1: Build the scanner\n  depends: Q2 (market count), Q1\n- [ ] T2: Wire it\n  depends: T1\n",
        )
        .unwrap();
        let answers = [answered_q2()];
        carry_answers(&mut plan, &answers);
        let mut report = GateReport::default();
        suppress_answered(&mut plan, &answers, &mut report);
        renumber_reused_questions(&mut plan, &answers);
        let open: Vec<&str> = plan.open.iter().map(|q| q.id.as_str()).collect();
        assert_eq!(open, ["Q1", "Q3"]);
        assert_eq!(
            plan.open[1].text,
            "How many markets does the first release cover?"
        );
        assert_eq!(
            plan.tasks[0].field("depends"),
            Some("Q3 (market count), Q1")
        );
        assert_eq!(plan.tasks[1].field("depends"), Some("T1"));
        let holders: Vec<&str> = plan
            .settled
            .iter()
            .filter_map(|s| s.field("answers"))
            .collect();
        assert_eq!(holders, ["Q2"], "the carried answer keeps its id alone");

        // A gate-opened question never takes an answered id either.
        let mut gated = BuildPlan::default();
        gated.settled.push(answer_item("S1", &answered_q2()));
        gated.open_from(Item::new("S2", "Is the feed licensed?"), "opened: test");
        assert_eq!(gated.open[0].id, "Q3");
    }

    #[test]
    fn newest_answer_to_a_reused_id_wins_within_a_plan() {
        let dir = tempfile::tempdir().unwrap();
        store::write_idea(
            dir.path(),
            &Idea {
                frontmatter: IdeaFrontmatter {
                    title: "Reused".into(),
                    slug: "reused".into(),
                    state: IdeaState::InDiscussion,
                    tags: vec![],
                    sources: vec![],
                    created: at(0),
                    updated: at(0),
                },
                body: "An idea.\n".into(),
            },
        )
        .unwrap();
        // Written before ids were kept unique: a carried Q2 answer, then a newer one under Q2.
        let body = "## Goal\nShip it.\n\n## Settled\n\
- S1: Binance spot data.\n  quote: Binance spot data, the free daily candles only.\n  answers: Q2\n  asked: Which exchange?\n  in: a\n\
- S2: Three markets.\n  quote: Three markets in the first release, no more.\n  answers: Q2\n  asked: How many markets?\n  in: b\n\n\
## Plan\n- [ ] T1: Build it\n  touches: `a.rs`\n  accept: `true` → exit 0\n";
        store::write_artifact(
            dir.path(),
            "reused",
            &Artifact {
                frontmatter: ArtifactFrontmatter {
                    slug: "b".into(),
                    title: "x".into(),
                    kind: ArtifactKind::BuildPlan,
                    lens: None,
                    created: at(1),
                    model: "m".into(),
                    revises: None,
                    version: None,
                    answered: Vec::new(),
                    recipe: None,
                },
                body: body.into(),
            },
        )
        .unwrap();
        let got = answered_in_lineage(dir.path(), "reused", "b").unwrap();
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(
            got[0].answer,
            "Three markets in the first release, no more."
        );
        assert_eq!(got[0].in_stem, "b");
    }

    #[test]
    fn carry_answers_reinserts_dropped_owner_item() {
        let answer = answered();
        let mut plan = plan::parse(
            "## Goal\nShip it.\n\n## Settled\n- S1: The parser is first.\n  quote: \"parser first\"\n\
  answers: Q6\n\n## Plan\n- [ ] T1: Build it\n",
        )
        .unwrap();
        assert_eq!(
            plan.settled[0].field("answers"),
            None,
            "a model cannot claim an owner answer"
        );
        carry_answers(&mut plan, std::slice::from_ref(&answer));
        assert_eq!(plan.settled.len(), 2);
        let s = &plan.settled[1];
        assert_eq!(s.id, "S2");
        assert_eq!(s.field("quote"), Some(answer.answer.as_str()));
        assert_eq!(s.field("answers"), Some("Q6"));
        assert_eq!(s.field("asked"), Some(answer.asked.as_str()));
        assert_eq!(s.field("in"), Some("20260929-120000-build-plan"));
        carry_answers(&mut plan, std::slice::from_ref(&answer));
        assert_eq!(plan.settled.len(), 2, "carrying twice adds nothing");

        let mut copied = plan::parse(&format!(
            "## Goal\nShip it.\n\n## Settled\n- S1: Freeze at entry.\n  quote: \"{}\"\n\n## Plan\n- [ ] T1: Build it\n",
            answer.answer
        ))
        .unwrap();
        carry_answers(&mut copied, std::slice::from_ref(&answer));
        assert_eq!(
            copied.settled.len(),
            1,
            "the model's own copy is re-labelled"
        );
        assert_eq!(copied.settled[0].field("answers"), Some("Q6"));
    }

    #[test]
    fn answered_in_lineage_reads_answers_left_in_verify() {
        use crate::concepts::build_plan::workbench::tests::{
            demote_to_verify, seeded, submit, Q1_ANSWER, SLUG,
        };
        let dir = seeded();
        let v2 = submit(
            dir.path(),
            super::super::workbench::tests::BASE,
            &[("Q1", Q1_ANSWER)],
            1,
        )
        .unwrap();
        let sid = v2
            .plan
            .settled
            .iter()
            .find(|s| s.field("answers") == Some("Q1"))
            .unwrap()
            .id
            .clone();
        demote_to_verify(dir.path(), &v2.stem, &sid, "recount: no count command");
        let answers = answered_in_lineage(dir.path(), SLUG, &v2.stem).unwrap();
        assert_eq!(
            answers.iter().map(|a| a.qid.as_str()).collect::<Vec<_>>(),
            ["Q1"]
        );
        assert_eq!(answers[0].answer, Q1_ANSWER);
    }
}
