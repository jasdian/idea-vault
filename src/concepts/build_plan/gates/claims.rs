//! G1 quote and provenance, G2 open collision, G3 audit carry-through.

use super::{AuditView, AuditedFinding, GateInputs, GateReport};
use crate::concepts::audit::{clip, Label};
use crate::concepts::build_plan::plan::{BuildPlan, Item, Provenance};
use crate::domain::evidence::{content_overlap, normalize_for_match, MIN_QUOTE_WORDS};

const COLLIDE_RATIO: f64 = 0.6;
const COLLIDE_SHARED: usize = 3;

const MARKER_WINDOW: usize = 300;

const MAX_FINDING_BYTES: usize = 300;

const MAX_REASON_QUOTE_BYTES: usize = 160;

const OPEN_QUESTIONS_LENS: &str = "extract-open-questions";

const HEDGES: &[&str] = &["pick one", "unresolved", "tbd", "not yet decided"];

const OPEN_MARKERS: &[&str] = &[
    "fork",
    "pick one",
    "unresolved",
    "never answered",
    "never established",
    "never agreed",
    "not yet decided",
    "not yet settled",
    "remain live",
    "remains live",
    "tension",
    "undecided",
    "unsettled",
    "open question",
];

/// G1–G3 over `plan`: ground each Settled claim in the evidence, then move out of Settled any
/// claim the audit refuted or doubted, or that collides with an open question; finally carry the
/// audit's open findings into Open questions.
pub fn apply(plan: &mut BuildPlan, inputs: &GateInputs, report: &mut GateReport) {
    let model_open: Vec<Item> = plan.open.clone();
    let audit = inputs.audit;
    let grounded = quote_and_provenance(plan, inputs, report);
    let grounded = audit_verdicts(plan, audit.filter(|a| !a.failed), grounded, report);
    open_collision(plan, inputs, audit, &model_open, grounded, report);
    if let Some(view) = audit {
        carry_open_findings(plan, view, report);
        if view.failed {
            report.note("audit unavailable — verdicts are defaults, treat as unaudited");
        }
        if view.uniform_pass {
            report.note("audit confirmed nearly everything — treat as a rubber stamp");
        }
    }
    for item in &plan.settled {
        match item.provenance {
            Some(Provenance::Owner) => report.count("settled_owner"),
            Some(Provenance::Idea) => report.count("settled_idea"),
            Some(Provenance::Foil) => report.count("settled_foil"),
            None => {}
        }
    }
}

struct Grounded {
    item: Item,
    at: Span,
}

#[derive(Clone, Copy)]
struct Span {
    turn: usize,
    start: usize,
    len: usize,
}

fn strip_quotes(quote: &str) -> &str {
    quote
        .trim()
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '“' | '”' | '‘' | '’' | '„'))
        .trim()
}

fn quote_and_provenance(
    plan: &mut BuildPlan,
    inputs: &GateInputs,
    report: &mut GateReport,
) -> Vec<Grounded> {
    let mut kept = Vec::new();
    for mut item in std::mem::take(&mut plan.settled) {
        let quote = item.field("quote").map(strip_quotes).map(str::to_string);
        let too_short = quote.as_deref().is_some_and(|q| {
            normalize_for_match(q)
                .split_whitespace()
                .filter(|w| w.chars().any(char::is_alphanumeric))
                .count()
                < MIN_QUOTE_WORDS
        });
        let quote = quote.filter(|_| !too_short);
        let ground = quote.as_deref().unwrap_or(item.text.as_str()).to_string();
        match inputs.evidence.locate(&ground) {
            Some((turn, start)) => {
                item.provenance = Some(inputs.evidence.turns()[turn].speaker);
                let at = Span {
                    turn,
                    start,
                    len: normalize_for_match(&ground).len(),
                };
                kept.push(Grounded { item, at });
            }
            None => {
                item.provenance = None;
                match quote {
                    Some(q) => {
                        let q = clip(&q, MAX_REASON_QUOTE_BYTES);
                        plan.quarantine(
                            item,
                            format!("claimed quote is not in the discussion: \"{q}\""),
                        );
                        report.count("quarantined");
                    }
                    None if too_short => {
                        plan.open_from(item, "opened: quote too short to prove anything");
                        report.count("opened");
                    }
                    None => {
                        plan.open_from(item, "opened: no supporting quote");
                        report.count("opened");
                    }
                }
            }
        }
    }
    kept
}

fn collides(a: &str, b: &str) -> bool {
    content_overlap(a, b).at_least(COLLIDE_RATIO, COLLIDE_SHARED)
}

fn audit_verdicts(
    plan: &mut BuildPlan,
    audit: Option<&AuditView>,
    grounded: Vec<Grounded>,
    report: &mut GateReport,
) -> Vec<Grounded> {
    let Some(audit) = audit else {
        return grounded;
    };
    let hit = |item: &Item, label: Label| -> Option<String> {
        audit
            .findings
            .iter()
            .find(|f| f.label == label && collides(&item.text, &f.text))
            .map(|f| f.reason.clone())
    };
    let mut kept = Vec::new();
    for g in grounded {
        if let Some(reason) = hit(&g.item, Label::Refuted) {
            plan.quarantine(g.item, format!("refuted by the audit: {reason}"));
            report.count("quarantined");
        } else if let Some(reason) = hit(&g.item, Label::Uncertain) {
            plan.open_from(g.item, format!("audit: UNCERTAIN — {reason}"));
            report.count("opened");
        } else {
            kept.push(g);
        }
    }
    kept
}

fn whole_word_at(text: &str, at: usize, len: usize) -> bool {
    let before = text[..at].chars().next_back();
    let after = text[at + len..].chars().next();
    !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
}

fn find_phrase(text: &str, phrase: &str) -> Option<usize> {
    text.match_indices(phrase)
        .map(|(at, _)| at)
        .find(|&at| whole_word_at(text, at, phrase.len()))
}

fn hedge(text: &str) -> Option<&'static str> {
    let norm = normalize_for_match(text);
    if let Some(h) = HEDGES.iter().find(|h| find_phrase(&norm, h).is_some()) {
        return Some(h);
    }
    let either = find_phrase(&norm, "either")?;
    find_phrase(&norm[either..], "or").map(|_| "either … or")
}

// A marker inside the quote itself is the claim's own wording ("fork the upstream repo"), not a
// sign that the discussion left it open, so only markers outside the quote's span count.
fn marker_beside(inputs: &GateInputs, at: Span) -> Option<&'static str> {
    let text = &inputs.evidence.turns()[at.turn].normalized;
    let quote_end = at.start + at.len;
    let from = at.start.saturating_sub(MARKER_WINDOW);
    let to = quote_end + MARKER_WINDOW;
    OPEN_MARKERS.iter().copied().find(|m| {
        text.match_indices(m).any(|(i, _)| {
            let end = i + m.len();
            let before = i >= from && end <= at.start;
            let after = i >= quote_end && end <= to;
            (before || after) && whole_word_at(text, i, m.len())
        })
    })
}

fn open_collision(
    plan: &mut BuildPlan,
    inputs: &GateInputs,
    audit: Option<&AuditView>,
    model_open: &[Item],
    grounded: Vec<Grounded>,
    report: &mut GateReport,
) {
    let audit_open: Vec<&AuditedFinding> = audit
        .map(|a| {
            a.findings
                .iter()
                .filter(|f| is_open_finding(f, a.failed))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for Grounded { mut item, at } in grounded {
        let signal = model_open
            .iter()
            .find(|q| collides(&item.text, &q.text))
            .map(|q| format!("matches {}", q.id))
            .or_else(|| {
                audit_open
                    .iter()
                    .any(|f| collides(&item.text, &f.text))
                    .then(|| "matches an open question from the audit".to_string())
            })
            .or_else(|| hedge(&item.text).map(|h| format!("hedged (\"{h}\")")))
            .or_else(|| {
                marker_beside(inputs, at).map(|m| format!("its quote sits beside \"{m}\""))
            });
        let listed = inputs
            .open_artifact
            .filter(|a| a.items.iter().any(|q| collides(&item.text, q)))
            .map(|a| a.name.clone());
        match (signal, listed) {
            (Some(why), _) => {
                plan.open_from(item, format!("opened from Settled: {why}"));
                report.count("opened");
            }
            (None, Some(name)) if item.provenance == Some(Provenance::Owner) => {
                item.markers.push(format!("listed open in {name}"));
                plan.settled.push(item);
            }
            (None, Some(name)) => {
                plan.open_from(item, format!("opened from Settled: listed in {name}"));
                report.count("opened");
            }
            (None, None) => plan.settled.push(item),
        }
    }
}

// A failed audit's labels are defaults, but its harvested findings and their lenses are real.
fn is_open_finding(f: &AuditedFinding, audit_failed: bool) -> bool {
    let open_lens = f.lenses.iter().any(|l| l == OPEN_QUESTIONS_LENS);
    if audit_failed {
        return open_lens;
    }
    f.label == Label::Uncertain || (f.label != Label::Refuted && open_lens)
}

fn carry_open_findings(plan: &mut BuildPlan, audit: &AuditView, report: &mut GateReport) {
    for f in audit
        .findings
        .iter()
        .filter(|f| is_open_finding(f, audit.failed))
    {
        let text = clip(f.text.trim(), MAX_FINDING_BYTES);
        let already = plan.open.iter().any(|q| {
            let own = q.text.strip_prefix("proposed:").unwrap_or(&q.text);
            collides(own, &text)
        });
        if already {
            continue;
        }
        let n = plan
            .open
            .iter()
            .filter_map(|q| q.id.strip_prefix('Q')?.parse::<usize>().ok())
            .max()
            .unwrap_or(0);
        let mut item = Item::new(&format!("Q{}", n + 1), &text);
        item.markers.push("from the audit".to_string());
        plan.open.push(item);
        report.count("audit_open");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::sources::SourceProbe;
    use crate::concepts::audit::Label;
    use crate::concepts::build_plan::gates::{AuditView, AuditedFinding, Evidence, OpenArtifact};
    use crate::concepts::build_plan::plan::{Item, Provenance};

    const CONVERSATION: &str =
        "## user\nWe will ship the parser before the probe, no question.\n\n\
## assistant\nThe probe must stay read only at all times.\n\n\
## assistant (skill: build-prompt)\nSettled: the cache lives in redis forever.\n";

    fn settled(text: &str, quote: Option<&str>) -> Item {
        let mut item = Item::new("S1", text);
        if let Some(q) = quote {
            item.fields.insert("quote".into(), q.into());
        }
        item
    }

    fn gate(
        plan: &mut BuildPlan,
        evidence: &Evidence,
        artifact: Option<&OpenArtifact>,
        audit: Option<&AuditView>,
    ) -> GateReport {
        let probe = SourceProbe::default();
        let inputs = GateInputs {
            evidence,
            open_artifact: artifact,
            audit,
            probe: &probe,
        };
        let mut report = GateReport::default();
        apply(plan, &inputs, &mut report);
        report
    }

    fn finding(text: &str, lens: &str, label: Label, reason: &str) -> AuditedFinding {
        AuditedFinding {
            text: text.into(),
            lenses: vec![lens.into()],
            label,
            reason: reason.into(),
        }
    }

    #[test]
    fn g1_an_owner_quote_keeps_the_item_settled_labelled_you() {
        let ev = Evidence::new("", CONVERSATION);
        let mut plan = BuildPlan {
            settled: vec![settled(
                "The parser ships first",
                Some("\u{201c}ship the parser before the probe\u{201d}"),
            )],
            ..BuildPlan::default()
        };
        let report = gate(&mut plan, &ev, None, None);
        assert_eq!(plan.settled.len(), 1, "{plan:?}");
        assert_eq!(plan.settled[0].provenance, Some(Provenance::Owner));
        assert_eq!(report.tally.get("settled_owner"), Some(&1));
    }

    #[test]
    fn g1_capstone_turns_are_not_evidence() {
        let ev = Evidence::new("", CONVERSATION);
        let mut plan = BuildPlan {
            settled: vec![settled(
                "The cache lives in redis",
                Some("the cache lives in redis forever"),
            )],
            ..BuildPlan::default()
        };
        let report = gate(&mut plan, &ev, None, None);
        assert!(plan.settled.is_empty(), "{plan:?}");
        assert_eq!(plan.quarantined.len(), 1);
        assert_eq!(report.tally.get("quarantined"), Some(&1));
    }

    #[test]
    fn g1_a_missing_quote_is_quarantined() {
        let ev = Evidence::new("", CONVERSATION);
        let mut item = settled(
            "The owner chose freeze at entry",
            Some("\"we freeze the snapshot at entry\""),
        );
        item.provenance = Some(Provenance::Owner);
        let mut plan = BuildPlan {
            settled: vec![item],
            ..BuildPlan::default()
        };
        gate(&mut plan, &ev, None, None);
        assert!(plan.settled.is_empty());
        assert_eq!(
            plan.quarantined[0].provenance, None,
            "a model-claimed label does not survive"
        );
        assert_eq!(
            plan.quarantined[0].field("reason"),
            Some("claimed quote is not in the discussion: \"we freeze the snapshot at entry\"")
        );
    }

    #[test]
    fn g1_a_foil_quote_stays_settled_labelled_foil() {
        let ev = Evidence::new("", CONVERSATION);
        let mut plan = BuildPlan {
            settled: vec![settled(
                "The probe is read only",
                Some("probe must stay read only"),
            )],
            ..BuildPlan::default()
        };
        let report = gate(&mut plan, &ev, None, None);
        assert_eq!(plan.settled.len(), 1, "{plan:?}");
        assert_eq!(plan.settled[0].provenance, Some(Provenance::Foil));
        assert_eq!(report.tally.get("settled_foil"), Some(&1));
    }

    #[test]
    fn g1_no_quote_is_opened() {
        let ev = Evidence::new("An idea about a parser.", CONVERSATION);
        let mut unproven = settled("We hire a team of five first", None);
        unproven.provenance = Some(Provenance::Owner);
        let mut plan = BuildPlan {
            settled: vec![unproven, settled("an idea about a parser", None)],
            ..BuildPlan::default()
        };
        let report = gate(&mut plan, &ev, None, None);
        assert_eq!(plan.open.len(), 1, "{plan:?}");
        assert_eq!(plan.open[0].provenance, None);
        assert_eq!(plan.open[0].text, "proposed: We hire a team of five first");
        assert_eq!(plan.open[0].markers, ["opened: no supporting quote"]);
        assert_eq!(plan.settled.len(), 1);
        assert_eq!(
            plan.settled[0].provenance,
            Some(Provenance::Idea),
            "an item whose own text grounds is kept"
        );
        assert_eq!(report.tally.get("opened"), Some(&1));
    }

    #[test]
    fn g1_a_short_quote_is_opened_not_quarantined() {
        let ev = Evidence::new("An idea about a parser.", CONVERSATION);
        let mut plan = BuildPlan {
            settled: vec![
                settled("The owner froze the scope", Some("\"Freeze it.\"")),
                settled("an idea about a parser", Some("")),
            ],
            ..BuildPlan::default()
        };
        gate(&mut plan, &ev, None, None);
        assert!(plan.quarantined.is_empty(), "{plan:?}");
        assert_eq!(
            plan.open[0].markers,
            ["opened: quote too short to prove anything"]
        );
        assert_eq!(plan.settled.len(), 1);
        assert_eq!(plan.settled[0].provenance, Some(Provenance::Idea));
    }

    #[test]
    fn g2_a_quote_beside_pick_one_is_opened() {
        let conv = "## assistant\nZone snapshots freeze at entry for every position we take. \
That is the choice here: pick one explicitly before building.\n";
        let ev = Evidence::new("", conv);
        let mut plan = BuildPlan {
            settled: vec![settled(
                "Snapshots freeze at entry",
                Some("zone snapshots freeze at entry"),
            )],
            ..BuildPlan::default()
        };
        gate(&mut plan, &ev, None, None);
        assert!(plan.settled.is_empty(), "{plan:?}");
        assert_eq!(
            plan.open[0].markers,
            ["opened from Settled: its quote sits beside \"pick one\""]
        );
    }

    #[test]
    fn g2_a_marker_far_from_the_quote_or_inside_a_word_is_ignored() {
        let filler = "we talked about unrelated matters at length. ".repeat(10);
        let conv = format!(
            "## assistant\nZone snapshots freeze at entry for every forklift extension. {filler} \
Separately, the fork in the road is lunch.\n"
        );
        let ev = Evidence::new("", &conv);
        let mut plan = BuildPlan {
            settled: vec![settled(
                "Snapshots freeze at entry",
                Some("zone snapshots freeze at entry"),
            )],
            ..BuildPlan::default()
        };
        gate(&mut plan, &ev, None, None);
        assert_eq!(plan.settled.len(), 1, "{plan:?}");
    }

    #[test]
    fn g2_an_owner_decision_to_fork_stays_settled() {
        let conv = "## user\nFork the upstream repo and patch it, that part is decided.\n";
        let ev = Evidence::new("", conv);
        let mut plan = BuildPlan {
            settled: vec![settled(
                "Fork the upstream repo and patch it",
                Some("fork the upstream repo and patch it"),
            )],
            ..BuildPlan::default()
        };
        gate(&mut plan, &ev, None, None);
        assert_eq!(plan.settled.len(), 1, "{plan:?}");
    }

    #[test]
    fn g2_a_marker_just_before_the_quote_is_seen_and_a_far_one_is_not() {
        let near = "## assistant\nUnresolved: zone snapshots freeze at entry for every position.\n";
        let ev = Evidence::new("", near);
        let item = || {
            settled(
                "Snapshots freeze at entry",
                Some("zone snapshots freeze at entry"),
            )
        };
        let mut plan = BuildPlan {
            settled: vec![item()],
            ..BuildPlan::default()
        };
        gate(&mut plan, &ev, None, None);
        assert_eq!(
            plan.open[0].markers,
            ["opened from Settled: its quote sits beside \"unresolved\""]
        );
        let filler = "we talked about unrelated matters at length. ".repeat(10);
        let far = format!(
            "## assistant\nUnresolved lunch. {filler} Zone snapshots freeze at entry for every position.\n"
        );
        let ev = Evidence::new("", &far);
        let mut plan = BuildPlan {
            settled: vec![item()],
            ..BuildPlan::default()
        };
        gate(&mut plan, &ev, None, None);
        assert_eq!(plan.settled.len(), 1, "{plan:?}");
    }

    #[test]
    fn g2_a_hedged_item_is_opened() {
        let ev = Evidence::new("", CONVERSATION);
        let mut plan = BuildPlan {
            settled: vec![settled(
                "Either the parser or the probe ships first",
                Some("ship the parser before the probe"),
            )],
            ..BuildPlan::default()
        };
        gate(&mut plan, &ev, None, None);
        assert!(plan.settled.is_empty(), "{plan:?}");
        assert_eq!(
            plan.open[0].markers,
            ["opened from Settled: hedged (\"either … or\")"]
        );
    }

    #[test]
    fn g2_an_item_matching_its_own_open_question_is_opened() {
        let ev = Evidence::new("", CONVERSATION);
        let mut plan = BuildPlan {
            settled: vec![settled(
                "The parser ships before the probe",
                Some("ship the parser before the probe"),
            )],
            open: vec![Item::new(
                "Q1",
                "Which ships first, the parser or the probe?",
            )],
            ..BuildPlan::default()
        };
        gate(&mut plan, &ev, None, None);
        assert!(plan.settled.is_empty(), "{plan:?}");
        assert_eq!(plan.open[1].markers, ["opened from Settled: matches Q1"]);
    }

    #[test]
    fn g2_an_owner_quote_only_gets_a_marker_from_the_artifact() {
        let ev = Evidence::new("", CONVERSATION);
        let artifact = OpenArtifact {
            name: "2026-09-01-open-questions".into(),
            items: vec!["Which ships first, the parser or the probe?".into()],
        };
        let mut plan = BuildPlan {
            settled: vec![settled(
                "The parser ships before the probe",
                Some("ship the parser before the probe"),
            )],
            ..BuildPlan::default()
        };
        gate(&mut plan, &ev, Some(&artifact), None);
        assert_eq!(plan.settled.len(), 1, "{plan:?}");
        assert_eq!(
            plan.settled[0].markers,
            ["listed open in 2026-09-01-open-questions"]
        );
        assert!(plan.open.is_empty());
    }

    #[test]
    fn g2_a_foil_item_listed_in_the_artifact_is_opened() {
        let ev = Evidence::new("", CONVERSATION);
        let artifact = OpenArtifact {
            name: "2026-09-01-open-questions".into(),
            items: vec!["Must the probe stay read only?".into()],
        };
        let mut plan = BuildPlan {
            settled: vec![settled(
                "The probe must stay read only",
                Some("probe must stay read only"),
            )],
            ..BuildPlan::default()
        };
        let report = gate(&mut plan, &ev, Some(&artifact), None);
        assert!(plan.settled.is_empty(), "{plan:?}");
        assert_eq!(
            plan.open[0].markers,
            ["opened from Settled: listed in 2026-09-01-open-questions"]
        );
        assert_eq!(plan.open[0].provenance, Some(Provenance::Foil));
        assert_eq!(report.tally.get("opened"), Some(&1));
    }

    #[test]
    fn g3_a_refuted_settled_claim_is_quarantined_with_the_reason() {
        let ev = Evidence::new("", CONVERSATION);
        let audit = AuditView {
            findings: vec![finding(
                "The probe must stay read only forever",
                "premortem",
                Label::Refuted,
                "the owner allowed a write mode",
            )],
            ..AuditView::default()
        };
        let mut plan = BuildPlan {
            settled: vec![settled(
                "The probe must stay read only",
                Some("probe must stay read only"),
            )],
            ..BuildPlan::default()
        };
        gate(&mut plan, &ev, None, Some(&audit));
        assert!(plan.settled.is_empty(), "{plan:?}");
        assert_eq!(
            plan.quarantined[0].field("reason"),
            Some("refuted by the audit: the owner allowed a write mode")
        );
    }

    #[test]
    fn g3_an_uncertain_settled_claim_is_opened_with_the_reason() {
        let ev = Evidence::new("", CONVERSATION);
        let audit = AuditView {
            findings: vec![finding(
                "The probe must stay read only forever",
                "premortem",
                Label::Uncertain,
                "nothing settles the write mode",
            )],
            ..AuditView::default()
        };
        let mut plan = BuildPlan {
            settled: vec![settled(
                "The probe must stay read only",
                Some("probe must stay read only"),
            )],
            ..BuildPlan::default()
        };
        gate(&mut plan, &ev, None, Some(&audit));
        assert!(plan.settled.is_empty(), "{plan:?}");
        assert_eq!(
            plan.open[0].markers,
            ["audit: UNCERTAIN — nothing settles the write mode"]
        );
        assert_eq!(
            plan.open.len(),
            1,
            "the finding is represented by the opened item"
        );
    }

    #[test]
    fn g3_uncertain_findings_missing_from_open_are_added() {
        let ev = Evidence::new("", CONVERSATION);
        let long = format!("Which exchange feeds the backtest {}", "é".repeat(400));
        let audit = AuditView {
            findings: vec![
                finding(&long, "premortem", Label::Uncertain, "never answered"),
                finding(
                    "Freeze the zone snapshot at entry or use dwell?",
                    "extract-open-questions",
                    Label::Confirmed,
                    "open",
                ),
                finding(
                    "Should the zone snapshot freeze at entry?",
                    "extract-open-questions",
                    Label::Confirmed,
                    "open",
                ),
                finding("Ship on Friday", "premortem", Label::Confirmed, "fine"),
            ],
            ..AuditView::default()
        };
        let mut plan = BuildPlan::default();
        gate(&mut plan, &ev, None, Some(&audit));
        assert_eq!(plan.open.len(), 2, "{plan:?}");
        assert_eq!(plan.open[0].id, "Q1");
        assert!(plan.open[0]
            .text
            .starts_with("Which exchange feeds the backtest"));
        assert!(plan.open[0].text.len() <= 300, "clipped to 300 bytes");
        assert_eq!(plan.open[0].markers, ["from the audit"]);
        assert_eq!(
            plan.open[1].text, "Freeze the zone snapshot at entry or use dwell?",
            "a near-duplicate open-question finding is added once"
        );
    }

    #[test]
    fn g3_an_audit_opened_item_is_not_appended_again() {
        let conv = "## user\nSnapshots freeze entry zones daily for every desk.\n";
        let ev = Evidence::new("", conv);
        let audit = AuditView {
            findings: vec![finding(
                "Snapshots freeze entry, but maybe weekly bins matter",
                "premortem",
                Label::Uncertain,
                "unclear",
            )],
            ..AuditView::default()
        };
        let mut plan = BuildPlan {
            settled: vec![settled(
                "Snapshots freeze entry zones daily",
                Some("snapshots freeze entry zones daily"),
            )],
            ..BuildPlan::default()
        };
        gate(&mut plan, &ev, None, Some(&audit));
        assert_eq!(plan.open.len(), 1, "{plan:?}");
    }

    #[test]
    fn g3_a_failed_audit_still_carries_open_question_findings() {
        let ev = Evidence::new("", CONVERSATION);
        let audit = AuditView {
            findings: vec![
                finding(
                    "The probe must stay read only forever",
                    "premortem",
                    Label::Uncertain,
                    "default",
                ),
                finding(
                    "Whether the parser ships before the probe release",
                    "extract-open-questions",
                    Label::Uncertain,
                    "default",
                ),
                finding(
                    "Which exchange feeds the backtest data?",
                    "extract-open-questions",
                    Label::Uncertain,
                    "default",
                ),
            ],
            failed: true,
            ..AuditView::default()
        };
        let mut plan = BuildPlan {
            settled: vec![
                settled(
                    "The probe must stay read only",
                    Some("probe must stay read only"),
                ),
                settled(
                    "The parser ships before the probe release",
                    Some("ship the parser before the probe"),
                ),
            ],
            ..BuildPlan::default()
        };
        gate(&mut plan, &ev, None, Some(&audit));
        assert_eq!(plan.settled.len(), 1, "{plan:?}");
        assert_eq!(plan.settled[0].text, "The probe must stay read only");
        assert_eq!(plan.open.len(), 2, "{plan:?}");
        assert_eq!(
            plan.open[0].markers,
            ["opened from Settled: matches an open question from the audit"]
        );
        assert_eq!(plan.open[1].text, "Which exchange feeds the backtest data?");
    }

    #[test]
    fn g3_a_failed_audit_is_noted() {
        let ev = Evidence::new("", CONVERSATION);
        let failed = AuditView {
            failed: true,
            ..AuditView::default()
        };
        let report = gate(&mut BuildPlan::default(), &ev, None, Some(&failed));
        assert_eq!(
            report.notes,
            ["audit unavailable — verdicts are defaults, treat as unaudited"]
        );
        let stamp = AuditView {
            uniform_pass: true,
            ..AuditView::default()
        };
        let report = gate(&mut BuildPlan::default(), &ev, None, Some(&stamp));
        assert_eq!(
            report.notes,
            ["audit confirmed nearly everything — treat as a rubber stamp"]
        );
        let report = gate(&mut BuildPlan::default(), &ev, None, None);
        assert!(report.notes.is_empty(), "quick mode adds no audit notes");
    }
}
