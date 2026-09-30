//! The parser corpus (docs/adr/0038, D40): every curated raw model output under
//! `tests/fixtures/raw-outputs/<parser>/` runs through today's parser, detector or gate, and the
//! verdict lines must equal the committed `tests/fixtures/parser-corpus.snap`. A parser change
//! therefore shows up as a snapshot diff in the same commit, with the flips printed.
//!
//! `PARSER_CORPUS_BLESS=1 cargo test --test parser_corpus` rewrites the snapshot after an
//! intended change; blessing is an ask-user change, so `scripts/gate.sh` refuses to run while the
//! variable is set, and the rewritten snapshot must be declared under `## Expectation changes`.

mod support;

use std::path::Path;

use idea_vault::ai::contract;
use idea_vault::ai::verdict::ParserKind;
use idea_vault::domain::OutputContract;
use idea_vault::regrade::{corpus_snapshot, read_corpus, snapshot_flips};

const CORPUS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/raw-outputs");
const SNAPSHOT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/parser-corpus.snap"
);

#[test]
fn corpus_matches_committed_snapshot() {
    let today = corpus_snapshot(Path::new(CORPUS)).expect("every fixture parses");
    if std::env::var_os("PARSER_CORPUS_BLESS").is_some() {
        std::fs::write(SNAPSHOT, &today).expect("snapshot is writable");
        return;
    }
    let committed = std::fs::read_to_string(SNAPSHOT).expect("the snapshot is committed");
    let flips = snapshot_flips(&committed, &today);
    assert!(
        flips.is_empty() && committed == today,
        "parser corpus drift ({} flip(s)); review, then bless with \
         PARSER_CORPUS_BLESS=1 cargo test --test parser_corpus:\n{}",
        flips.len(),
        flips.join("\n")
    );
}

#[test]
fn every_fixture_parses_header() {
    let corpus = read_corpus(Path::new(CORPUS)).unwrap();
    assert!(
        corpus.len() >= 9,
        "the hand-seeded corpus is present ({} fixtures)",
        corpus.len()
    );
    for family in ["audit", "facts", "contract", "plan-gates"] {
        assert!(
            corpus
                .iter()
                .any(|(name, _)| name.starts_with(&format!("{family}/"))),
            "no {family} fixture"
        );
    }
    for (name, fixture) in corpus {
        let fixture = fixture.unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(!fixture.raw.trim().is_empty(), "{name}: empty raw output");
        assert!(
            fixture.summary().starts_with("pass="),
            "{name}: a verdict line opens with pass="
        );
    }
}

/// The build-plan verdict is journaled from the kept (repaired) answer but replayed over the raw
/// one, so repair must not change what the gates see.
#[test]
fn plan_verdict_is_the_same_for_the_raw_and_the_kept_answer() {
    for (name, fixture) in read_corpus(Path::new(CORPUS)).unwrap() {
        let fixture = fixture.unwrap();
        if fixture.kind != ParserKind::PlanGates {
            continue;
        }
        let kept = contract::validate(OutputContract::BuildPlan, &fixture.raw)
            .unwrap_or_else(|_| fixture.raw.trim().to_string());
        let kept_summary = idea_vault::regrade::summarize(&fixture.kind, &kept, fixture.haystack());
        assert_eq!(fixture.summary(), kept_summary, "{name}");
    }
}
