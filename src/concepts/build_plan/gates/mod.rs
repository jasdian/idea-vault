//! The deterministic build-plan gates G1–G12 (docs/adr/0030). Each gate reads the parsed
//! [`BuildPlan`] plus the evidence it was given and moves claims between sections — Settled to
//! Open questions, Verify first or Quarantined — adding `⟨…⟩` markers and header notes. No gate
//! calls a model or runs a command; the only I/O is the bounded [`SourceProbe`], so [`run`] is
//! blocking and belongs in `spawn_blocking`.
//!
//! - [`claims`]: G1 quote and provenance, G2 open collision, G3 audit carry-through.
//! - [`sources`]: G4 anchor and symbol, G5 tokens and names, G6 figures and units, G12 freshness.
//! - [`tasks`]: G7 scope fence, G8 dependency repair, G9 executable task, G10 kill wiring,
//!   G11 shape and caps.

pub mod claims;
pub mod sources;
pub mod tasks;

use std::collections::BTreeMap;

use crate::ai::sources::SourceProbe;
use crate::concepts::audit::{AuditReport, Finding, Label};
use crate::concepts::build_plan::plan::{BuildPlan, Provenance};
use crate::domain::evidence::{locate, normalize_for_match};
use crate::vault::store::{
    is_capstone_turn, parse_turn_heading, split_turns, turn_role, TurnSource,
};

/// One evidence turn: who wrote it, its raw text and its normalized form (for quote matching).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceTurn {
    pub speaker: Provenance,
    pub raw: String,
    pub normalized: String,
}

/// What a plan may be grounded in: the idea statement and the discussion's turns, minus the
/// capstone turns (earlier build plans and their pointer turns), which would ground a plan in
/// itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Evidence {
    turns: Vec<EvidenceTurn>,
}

impl Evidence {
    /// Evidence from `idea_body` and `conversation` (the raw `conversation.md`). Exactly the turns
    /// [`is_capstone_turn`] flags are excluded; `## user` turns are the owner's, every other turn
    /// is the foil's.
    pub fn new(idea_body: &str, conversation: &str) -> Self {
        let mut turns = Vec::new();
        if !idea_body.trim().is_empty() {
            turns.push(EvidenceTurn {
                speaker: Provenance::Idea,
                raw: idea_body.to_string(),
                normalized: normalize_for_match(idea_body),
            });
        }
        for turn in split_turns(conversation) {
            if is_capstone_turn(&turn) {
                continue;
            }
            let speaker = match parse_turn_heading(turn_role(&turn)) {
                TurnSource::User => Provenance::Owner,
                _ => Provenance::Foil,
            };
            turns.push(EvidenceTurn {
                speaker,
                normalized: normalize_for_match(&turn),
                raw: turn,
            });
        }
        Evidence { turns }
    }

    pub fn turns(&self) -> &[EvidenceTurn] {
        self.turns.iter().as_slice()
    }

    /// Where `quote` grounds, as (turn index, byte offset into that turn's normalized text).
    /// The owner's turns are searched first, then the idea statement, then the foil's, so a claim
    /// both sides wrote is credited to the owner.
    pub fn locate(&self, quote: &str) -> Option<(usize, usize)> {
        [Provenance::Owner, Provenance::Idea, Provenance::Foil]
            .into_iter()
            .find_map(|who| {
                self.turns
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| t.speaker == who)
                    .find_map(|(i, t)| locate(quote, &t.normalized).map(|at| (i, at)))
            })
    }

    /// The raw text of every turn by `speaker`.
    pub fn text_by(&self, speaker: Provenance) -> impl Iterator<Item = &str> {
        self.turns
            .iter()
            .filter(move |t| t.speaker == speaker)
            .map(|t| t.raw.as_str())
    }
}

/// One harvested finding with the auditor's verdict on it.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditedFinding {
    pub text: String,
    /// The harvest lenses that produced it (e.g. `extract-open-questions`).
    pub lenses: Vec<String>,
    pub label: Label,
    pub reason: String,
}

/// The audited harvest the planner worked from (audited mode only).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AuditView {
    pub findings: Vec<AuditedFinding>,
    /// The auditor's answer was unusable — every verdict is a default.
    pub failed: bool,
    /// Nearly every finding was confirmed — a rubber stamp worth distrusting.
    pub uniform_pass: bool,
}

impl AuditView {
    /// Pair each finding with its index-aligned verdict.
    pub fn new(findings: &[Finding], report: &AuditReport) -> Self {
        AuditView {
            findings: findings
                .iter()
                .zip(&report.verdicts)
                .map(|(f, v)| AuditedFinding {
                    text: f.text.clone(),
                    lenses: f.lenses.clone(),
                    label: v.label,
                    reason: v.reason.clone(),
                })
                .collect(),
            failed: report.failed,
            uniform_pass: report.uniform_pass(),
        }
    }
}

/// The idea's latest `*-open-questions.md` artifact: its file stem and its question items.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenArtifact {
    pub name: String,
    pub items: Vec<String>,
}

/// Everything the gates read besides the plan itself.
#[derive(Debug, Clone, Copy)]
pub struct GateInputs<'a> {
    pub evidence: &'a Evidence,
    pub open_artifact: Option<&'a OpenArtifact>,
    /// `None` in quick (unaudited) mode.
    pub audit: Option<&'a AuditView>,
    pub probe: &'a SourceProbe,
}

/// What the gates did: header notes for the owner and a tally of each action, keyed by a short
/// label (`opened`, `quarantined`, `premises`, …).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GateReport {
    pub notes: Vec<String>,
    pub tally: BTreeMap<&'static str, usize>,
}

impl GateReport {
    pub fn note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }

    pub fn count(&mut self, key: &'static str) {
        *self.tally.entry(key).or_default() += 1;
    }
}

/// Run every gate over `plan`, in order: claims (G1–G3), then sources (G4–G6, G12), then tasks
/// (G7–G11). Blocking (the source probe reads files).
pub fn run(plan: &mut BuildPlan, inputs: &GateInputs) -> GateReport {
    let mut report = GateReport::default();
    claims::apply(plan, inputs, &mut report);
    sources::apply(plan, inputs, &mut report);
    tasks::apply(plan, inputs, &mut report);
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONVERSATION: &str = "## user\nWe will ship the parser before the probe.\n\n\
## assistant\nThe probe must stay read only, always.\n\n\
## assistant (skill: build-prompt)\nThe parser ships after the probe, settled.\n";

    #[test]
    fn evidence_credits_the_owner_first_and_skips_capstone_turns() {
        let ev = Evidence::new("An idea about the parser before the probe.", CONVERSATION);
        assert_eq!(
            ev.turns().iter().map(|t| t.speaker).collect::<Vec<_>>(),
            [Provenance::Idea, Provenance::Owner, Provenance::Foil]
        );
        let (turn, _) = ev.locate("the parser before the probe").unwrap();
        assert_eq!(ev.turns()[turn].speaker, Provenance::Owner);
        let (turn, _) = ev.locate("must stay read only").unwrap();
        assert_eq!(ev.turns()[turn].speaker, Provenance::Foil);
        assert_eq!(
            ev.locate("parser ships after the probe"),
            None,
            "an earlier build plan never grounds a new one"
        );
    }

    #[test]
    fn evidence_keeps_the_owner_and_chat_turns_and_drops_capstone_and_pointer_turns() {
        let conversation = format!(
            "{CONVERSATION}\n## assistant (skill: house-plan)\n**Build plan** → [p](/idea/x/artifact/p.md) · quick\n\n\
## assistant\n**Build plan** → [a chat reply that echoes it](/x)\n"
        );
        let ev = Evidence::new("", &conversation);
        assert_eq!(
            ev.turns().iter().map(|t| t.speaker).collect::<Vec<_>>(),
            [Provenance::Owner, Provenance::Foil, Provenance::Foil]
        );
        assert_eq!(ev.locate("p](/idea/x/artifact/p.md"), None);
        assert_eq!(ev.locate("parser ships after the probe"), None);
        let (turn, _) = ev.locate("a chat reply that echoes it").unwrap();
        assert_eq!(ev.turns()[turn].speaker, Provenance::Foil);
    }

    #[test]
    fn audit_view_pairs_findings_with_their_verdicts() {
        let findings = vec![Finding {
            lenses: vec!["extract-open-questions".into()],
            role: crate::concepts::agents::AgentRole::Harvester,
            text: "Freeze at entry or dwell?".into(),
        }];
        let report = crate::concepts::audit::parse_audit("F1: UNCERTAIN — never answered", 1);
        let view = AuditView::new(&findings, &report);
        assert_eq!(view.findings[0].label, Label::Uncertain);
        assert_eq!(view.findings[0].lenses, ["extract-open-questions"]);
        assert!(!view.uniform_pass);
    }
}
