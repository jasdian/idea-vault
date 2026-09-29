//! G4 anchor and symbol, G5 tokens and names, G6 figures and units, G12 freshness.
//!
//! Each Settled, Fence and Kill item moves at most once: the first gate to fault it decides where
//! it goes, and later gates never see it. Plan tasks are never moved, only marked, since a task
//! may legitimately create a new name or refer to a figure the discussion never stated. A token
//! or anchor the probe could not check is unknown, never absent: it goes to Verify first, and
//! only a complete scan (or no attached source at all) can quarantine.

use super::{GateInputs, GateReport};
use crate::ai::sources::{AnchorCheck, SourceProbe};
use crate::concepts::build_plan::plan::{BuildPlan, Item, Provenance};

const NEGATION_CUES: [&str; 7] = [
    "never", "no", "not", "missing", "absent", "unused", "nothing",
];
const SUBTRACTION_CUES: [&str; 5] = ["excluding", "remaining", "minus", "without", "less"];
const FRESHNESS_CUES: [&str; 3] = ["next free", "current max", "as of"];
const FILE_EXTENSIONS: [&str; 12] = [
    "rs", "md", "toml", "json", "js", "ts", "py", "html", "yml", "yaml", "sh", "txt",
];
const RECHECK: &str = "freshness: re-check at bootstrap";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Claim,
    Fence,
}

enum Verdict {
    Keep,
    Verify {
        check: Option<String>,
        marker: String,
    },
    Quarantine(String),
}

struct Ctx<'a> {
    probe: &'a SourceProbe,
    owner_idea: String,
    foil: String,
    all: String,
}

impl<'a> Ctx<'a> {
    fn new(inputs: &GateInputs<'a>) -> Self {
        let evidence = inputs.evidence;
        let join = |who: &[Provenance]| {
            who.iter()
                .flat_map(|w| evidence.text_by(*w))
                .collect::<Vec<_>>()
                .join("\n")
                .to_lowercase()
        };
        Ctx {
            probe: inputs.probe,
            owner_idea: join(&[Provenance::Owner, Provenance::Idea]),
            foil: join(&[Provenance::Foil]),
            all: join(&[Provenance::Owner, Provenance::Idea, Provenance::Foil]),
        }
    }
}

pub fn apply(plan: &mut BuildPlan, inputs: &GateInputs, report: &mut GateReport) {
    let ctx = Ctx::new(inputs);
    for item in &mut plan.verify {
        match g4(item, ctx.probe) {
            Some(G4::Unpaired(anchor)) => item.markers.push(unpaired_marker(&anchor)),
            Some(G4::Faulty { marker, .. }) => item.markers.push(marker),
            Some(G4::Resolved(markers)) => item.markers.extend(markers),
            None => {}
        }
    }
    let settled = std::mem::take(&mut plan.settled);
    plan.settled = gate_items(plan, settled, Kind::Claim, &ctx, report);
    let kills = std::mem::take(&mut plan.kills);
    plan.kills = gate_items(plan, kills, Kind::Claim, &ctx, report);
    let fence = std::mem::take(&mut plan.fence);
    plan.fence = gate_items(plan, fence, Kind::Fence, &ctx, report);
    for task in &mut plan.tasks {
        mark_task(task, &ctx);
    }
}

fn gate_items(
    plan: &mut BuildPlan,
    items: Vec<Item>,
    kind: Kind,
    ctx: &Ctx,
    report: &mut GateReport,
) -> Vec<Item> {
    let mut kept = Vec::new();
    for mut item in items {
        match judge(&mut item, kind, ctx, report) {
            Verdict::Keep => kept.push(item),
            Verdict::Verify { check, marker } => plan.verify_first(item, check, marker),
            Verdict::Quarantine(reason) => {
                plan.quarantine(item, reason);
                report.count("quarantined");
            }
        }
    }
    kept
}

fn judge(item: &mut Item, kind: Kind, ctx: &Ctx, report: &mut GateReport) -> Verdict {
    if kind == Kind::Claim {
        match g4(item, ctx.probe) {
            Some(G4::Unpaired(anchor)) => {
                return Verdict::Verify {
                    check: None,
                    marker: unpaired_marker(&anchor),
                }
            }
            Some(G4::Faulty { check, marker }) => {
                return Verdict::Verify {
                    check: Some(check),
                    marker,
                }
            }
            Some(G4::Resolved(markers)) => {
                for _ in &markers {
                    report.count("anchors_ok");
                }
                item.markers.extend(markers);
            }
            None => {}
        }
    }
    if let Some(verdict) = g5(item, ctx, report) {
        return verdict;
    }
    if kind == Kind::Claim {
        if let Some(verdict) = g6(item, ctx) {
            return verdict;
        }
        if let Some(verdict) = g12(item) {
            return verdict;
        }
    }
    Verdict::Keep
}

fn mark_task(task: &mut Item, ctx: &Ctx) {
    let mut found = Vec::new();
    for token in tokens(&task.text) {
        let scan = ctx.probe.find_tokens(std::slice::from_ref(&token));
        if !scan.found.contains(&token) && !word_in(&ctx.owner_idea, &token.to_lowercase()) {
            found.push(format!("new: {token}"));
        }
    }
    for figure in figures(&task.text) {
        if !word_in(&ctx.all, &figure.num) {
            found.push(format!("figure not in the discussion: {}", figure.num));
        }
    }
    if freshness_cue(&task.text) {
        found.push(RECHECK.to_string());
    }
    for marker in found {
        if !task.markers.contains(&marker) {
            task.markers.push(marker);
        }
    }
}

/// Backticked spans of `text` and the text outside them.
fn spans(text: &str) -> (Vec<String>, String) {
    let mut ticks = Vec::new();
    let mut outside = String::new();
    for (i, part) in text.split('`').enumerate() {
        if i % 2 == 1 {
            ticks.push(part.trim().to_string());
        } else {
            outside.push_str(part);
            outside.push(' ');
        }
    }
    (ticks, outside)
}

fn claim_text(item: &Item) -> String {
    match item.field("quote") {
        Some(quote) => format!("{} {quote}", item.text),
        None => item.text.clone(),
    }
}

fn word_in(hay: &str, needle: &str) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let (Some(head), Some(tail)) = (needle.chars().next(), needle.chars().last()) else {
        return false;
    };
    hay.match_indices(needle).any(|(at, _)| {
        let before = hay[..at].chars().next_back();
        let after = hay[at + needle.len()..].chars().next();
        let glued_before = ident(head) && before.is_some_and(ident);
        let glued_after = ident(tail) && after.is_some_and(ident);
        !glued_before && !glued_after
    })
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

struct Anchor {
    label: String,
    path: String,
    first: usize,
    last: usize,
}

fn parse_anchor(span: &str) -> Option<Anchor> {
    let (path, range) = span.rsplit_once(':')?;
    if path.is_empty() || path.contains(char::is_whitespace) {
        return None;
    }
    let (a, b) = range.split_once('-').unwrap_or((range, range));
    Some(Anchor {
        label: span.to_string(),
        path: path.to_string(),
        first: a.parse().ok()?,
        last: b.parse().ok()?,
    })
}

enum G4 {
    Unpaired(String),
    Faulty { check: String, marker: String },
    Resolved(Vec<String>),
}

fn unpaired_marker(anchor: &str) -> String {
    format!("name the symbol at {anchor}")
}

fn g4(item: &Item, probe: &SourceProbe) -> Option<G4> {
    let (ticks, _) = spans(&claim_text(item));
    let anchors: Vec<Anchor> = ticks.iter().filter_map(|t| parse_anchor(t)).collect();
    let first = anchors.first()?;
    let symbol = ticks.iter().find(|t| {
        !t.is_empty()
            && !t.contains(char::is_whitespace)
            && parse_anchor(t).is_none()
            && anchors.iter().all(|a| a.path != **t)
    });
    let Some(symbol) = symbol else {
        return Some(G4::Unpaired(first.label.clone()));
    };
    let mut ok = Vec::new();
    for a in &anchors {
        let faulty = match probe.check_anchor(&a.path, a.first, a.last, symbol) {
            AnchorCheck::Resolved { source, path } => {
                ok.push(format!("✓ {path} resolved in {source}"));
                continue;
            }
            AnchorCheck::Moved { line, .. } => format!("moved: symbol found at line {line}"),
            AnchorCheck::SymbolMissing { path, .. } => {
                format!("symbol missing: {symbol} not in {path}")
            }
            AnchorCheck::NoFile => format!("no such file: {}", a.path),
            AnchorCheck::Ambiguous(candidates) => {
                format!("ambiguous path: {}", candidates.join(", "))
            }
            AnchorCheck::Unverified if probe.is_empty() => {
                "unverified: no source attached".to_string()
            }
            AnchorCheck::Unverified => format!("unverified: could not check {}", a.path),
        };
        return Some(G4::Faulty {
            check: format!(
                "`sed -n {},{}p {} | grep -nF {symbol}`",
                a.first, a.last, a.path
            ),
            marker: faulty,
        });
    }
    Some(G4::Resolved(ok))
}

fn trim_edges(word: &str) -> &str {
    word.trim_start_matches(['(', '[', '{', '"', '\''])
        .trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}', '"', '\''])
}

fn is_camel(w: &str) -> bool {
    w.chars().all(|c| c.is_ascii_alphanumeric())
        && w.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && w.chars().filter(char::is_ascii_uppercase).count() >= 2
        && w.chars().any(|c| c.is_ascii_lowercase())
}

fn is_dotted(w: &str) -> bool {
    let parts: Vec<&str> = w.split('.').collect();
    let name = |p: &&str| {
        !p.is_empty()
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    };
    parts.len() >= 2
        && parts.iter().all(name)
        && parts[0].len() >= 2
        && parts[0].chars().any(|c| c.is_ascii_alphabetic())
        && (parts.len() >= 3 || parts.last().is_some_and(|e| FILE_EXTENSIONS.contains(e)))
}

fn is_slashed(w: &str) -> bool {
    let parts: Vec<&str> = w.split('/').collect();
    parts.len() >= 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        })
        && (parts.len() >= 3 || parts.last().is_some_and(|l| l.contains('.')))
}

/// Code tokens in `text`: backticked spans, plus unbackticked CamelCase, dotted or slashed names.
fn tokens(text: &str) -> Vec<String> {
    let (ticks, outside) = spans(text);
    let mut out: Vec<String> = Vec::new();
    let mut push = |t: &str| {
        if !out.iter().any(|o| o == t) {
            out.push(t.to_string());
        }
    };
    for t in ticks {
        let numeric = t.chars().all(|c| c.is_ascii_digit() || c == '.');
        if t.chars().count() >= 2
            && !t.contains(char::is_whitespace)
            && !numeric
            && parse_anchor(&t).is_none()
        {
            push(&t);
        }
    }
    for w in outside.split_whitespace() {
        let w = trim_edges(w);
        if is_camel(w) || is_dotted(w) || is_slashed(w) {
            push(w);
        }
    }
    out
}

fn grep_check(token: &str) -> String {
    format!("`grep -rnF {token} .`")
}

fn g5(item: &Item, ctx: &Ctx, report: &mut GateReport) -> Option<Verdict> {
    let found_tokens = tokens(&claim_text(item));
    if found_tokens.is_empty() {
        return None;
    }
    let scan = ctx.probe.find_tokens(&found_tokens);
    let complete = ctx.probe.is_empty() || scan.complete;
    let mut nowhere = Vec::new();
    let mut foil = Vec::new();
    for t in &found_tokens {
        let lower = t.to_lowercase();
        if scan.found.contains(t) || word_in(&ctx.owner_idea, &lower) {
            continue;
        }
        if word_in(&ctx.foil, &lower) {
            foil.push(t);
        } else {
            nowhere.push(t);
        }
    }
    let unproven = nowhere.first().or(foil.first()).copied()?;
    if words(&item.text)
        .iter()
        .any(|w| NEGATION_CUES.contains(&w.as_str()))
    {
        report.count("premises");
        return Some(Verdict::Verify {
            check: Some(grep_check(unproven)),
            marker: "absence claim: a premise, not a fact".to_string(),
        });
    }
    if let Some(token) = nowhere.first() {
        if complete {
            return Some(Verdict::Quarantine(format!(
                "{token} appears nowhere in the discussion or the sources"
            )));
        }
        return Some(Verdict::Verify {
            check: Some(grep_check(token)),
            marker: format!("unverified: {token} not found, source scan incomplete"),
        });
    }
    let token = foil.first()?;
    Some(Verdict::Verify {
        check: Some(grep_check(token)),
        marker: format!("foil-coined: {token} — only the foil wrote it"),
    })
}

struct Figure {
    num: String,
    unit: Option<String>,
}

/// Counts in `text` outside backticks: a number of at least two digits, or a number with a unit.
fn figures(text: &str) -> Vec<Figure> {
    let (_, outside) = spans(text);
    let ws: Vec<&str> = outside.split_whitespace().collect();
    let mut out = Vec::new();
    for (i, raw) in ws.iter().enumerate() {
        let w = trim_edges(raw);
        let (num, percent) = match w.strip_suffix('%') {
            Some(n) => (n, true),
            None => (w, false),
        };
        let numeric = num.chars().next().is_some_and(|c| c.is_ascii_digit())
            && num
                .chars()
                .all(|c| c.is_ascii_digit() || c == '.' || c == ',');
        if !numeric {
            continue;
        }
        let unit = if percent {
            Some("%".to_string())
        } else {
            ws.get(i + 1)
                .map(|n| trim_edges(n).to_lowercase())
                .filter(|n| n.len() >= 2 && n.chars().all(|c| c.is_ascii_alphabetic()))
                .filter(|n| !matches!(n.as_str(), "of" | "or" | "and" | "to" | "in" | "on"))
        };
        let digits = num.chars().filter(char::is_ascii_digit).count();
        if digits >= 2 || unit.is_some() {
            out.push(Figure {
                num: num.to_string(),
                unit,
            });
        }
    }
    out
}

fn mixed_units(text: &str) -> Option<String> {
    let (_, outside) = spans(text);
    for sentence in outside
        .split(['!', '?', ';', '\n'])
        .flat_map(|s| s.split(". "))
    {
        let cued = words(sentence)
            .iter()
            .any(|w| SUBTRACTION_CUES.contains(&w.as_str()))
            || sentence.to_lowercase().contains("after removing");
        if !cued {
            continue;
        }
        let mut plurals: Vec<String> = Vec::new();
        for f in figures(sentence) {
            if let Some(u) = f.unit {
                if u.len() >= 3 && u.ends_with('s') && !u.ends_with("ss") && !plurals.contains(&u) {
                    plurals.push(u);
                }
            }
        }
        if plurals.len() >= 2 {
            return Some(format!("mixed units: {} vs {}", plurals[0], plurals[1]));
        }
    }
    None
}

fn g6(item: &Item, ctx: &Ctx) -> Option<Verdict> {
    let verify = |marker: String| {
        Some(Verdict::Verify {
            check: None,
            marker,
        })
    };
    if let Some(marker) = mixed_units(&item.text) {
        return verify(marker);
    }
    let figs = figures(&item.text);
    if let Some(f) = figs.iter().find(|f| !word_in(&ctx.all, &f.num)) {
        return verify(format!("figure not in the discussion: {}", f.num));
    }
    if !figs.is_empty() && item.field("count").is_none() {
        return verify("recount: no count command".to_string());
    }
    None
}

fn freshness_cue(text: &str) -> bool {
    let lower = text.to_lowercase();
    FRESHNESS_CUES.iter().any(|c| lower.contains(c)) || words(&lower).iter().any(|w| w == "latest")
}

fn g12(item: &Item) -> Option<Verdict> {
    if !freshness_cue(&item.text) {
        return None;
    }
    let (ticks, _) = spans(&item.text);
    let dir = ticks
        .iter()
        .find(|t| t.contains('/') && !t.contains(char::is_whitespace) && parse_anchor(t).is_none())
        .map(|t| {
            let t = t.trim_end_matches('/');
            match t.rsplit_once('/') {
                Some((parent, last)) if last.contains('.') => parent.to_string(),
                _ => t.to_string(),
            }
        });
    Some(Verdict::Verify {
        check: dir.map(|d| format!("`ls {d} | tail -1`")),
        marker: RECHECK.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::sources::SourceProbe;
    use crate::concepts::build_plan::gates::Evidence;
    use crate::concepts::build_plan::plan::Item;
    use crate::domain::Name;
    use crate::sources::ResolvedSource;

    const CONVERSATION: &str =
        "## user\nThe vault has 56 facts and 30 pairs today. ADRs live in docs/adr.\n\n\
## assistant\nWe could cite [[slug#fact]] syntax and a MaxLeverage cap.\n";

    fn probe_over(files: &[(&str, &str)]) -> (tempfile::TempDir, SourceProbe) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for (rel, body) in files {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }
        let probe = SourceProbe::new(&[ResolvedSource {
            name: Name::try_from("code").unwrap(),
            root,
        }]);
        (dir, probe)
    }

    fn run(plan: &mut BuildPlan, probe: &SourceProbe) -> GateReport {
        let evidence = Evidence::new("An idea about the vault.", CONVERSATION, &[]);
        let inputs = GateInputs {
            evidence: &evidence,
            open_artifact: None,
            audit: None,
            probe,
        };
        let mut report = GateReport::default();
        apply(plan, &inputs, &mut report);
        report
    }

    fn settled(text: &str) -> BuildPlan {
        BuildPlan {
            settled: vec![Item::new("S1", text)],
            ..BuildPlan::default()
        }
    }

    fn markers(item: &Item) -> String {
        item.markers.join(" | ")
    }

    const CODE: &str = "line one\nline two\nfn parse_turn() {}\nline four\n";

    #[test]
    fn g4_a_resolved_anchor_stays_settled() {
        let (_d, probe) = probe_over(&[("src/store.rs", CODE)]);
        let mut plan = settled("Turns parse at `src/store.rs:3-3` in `parse_turn`.");
        let report = run(&mut plan, &probe);
        assert_eq!(plan.settled.len(), 1);
        assert!(plan.verify.is_empty());
        assert!(markers(&plan.settled[0]).contains("✓ src/store.rs resolved in code"));
        assert_eq!(report.tally.get("anchors_ok"), Some(&1));
    }

    #[test]
    fn g4_a_moved_anchor_goes_to_verify_first_with_a_sed_check() {
        let (_d, probe) = probe_over(&[("src/store.rs", CODE)]);
        let mut plan = settled("Turns parse at `src/store.rs:1-2` in `parse_turn`.");
        run(&mut plan, &probe);
        assert!(plan.settled.is_empty());
        let item = &plan.verify[0];
        assert!(markers(item).contains("moved: symbol found at line 3"));
        assert_eq!(
            item.field("check"),
            Some("`sed -n 1,2p src/store.rs | grep -nF parse_turn`")
        );
    }

    #[test]
    fn g4_an_unpaired_anchor_asks_for_the_symbol() {
        let (_d, probe) = probe_over(&[("src/store.rs", CODE)]);
        let mut plan = settled("Turns parse at `src/store.rs:3-4`.");
        run(&mut plan, &probe);
        assert!(plan.settled.is_empty());
        assert!(markers(&plan.verify[0]).contains("name the symbol at src/store.rs:3-4"));
    }

    #[test]
    fn g4_without_sources_anchors_are_unverified() {
        let mut plan = settled("Turns parse at `src/store.rs:3-4` in `parse_turn`.");
        run(&mut plan, &SourceProbe::default());
        assert!(plan.settled.is_empty());
        assert!(plan.quarantined.is_empty());
        assert!(markers(&plan.verify[0]).contains("unverified: no source attached"));
    }

    #[test]
    fn g5_a_foil_coined_token_moves_to_verify_first() {
        let mut plan = settled("Notes link with `[[slug#fact]]` syntax.");
        run(&mut plan, &SourceProbe::default());
        assert!(plan.settled.is_empty());
        assert!(plan.quarantined.is_empty());
        let item = &plan.verify[0];
        assert!(markers(item).contains("foil-coined: [[slug#fact]] — only the foil wrote it"));
        assert_eq!(item.field("check"), Some("`grep -rnF [[slug#fact]] .`"));
    }

    #[test]
    fn g5_an_invented_token_is_quarantined() {
        let mut plan = settled("The loader calls `FrobnicateAll` on start.");
        let report = run(&mut plan, &SourceProbe::default());
        assert!(plan.settled.is_empty());
        assert_eq!(
            plan.quarantined[0].field("reason"),
            Some("FrobnicateAll appears nowhere in the discussion or the sources")
        );
        assert_eq!(report.tally.get("quarantined"), Some(&1));
    }

    #[test]
    fn g5_a_negative_claim_becomes_a_grep_premise() {
        let mut plan = settled("`ACCOUNT_MODE` is never queried by the loader.");
        let report = run(&mut plan, &SourceProbe::default());
        assert!(plan.quarantined.is_empty());
        let item = &plan.verify[0];
        assert!(markers(item).contains("absence claim: a premise, not a fact"));
        assert_eq!(item.field("check"), Some("`grep -rnF ACCOUNT_MODE .`"));
        assert_eq!(report.tally.get("premises"), Some(&1));
    }

    #[test]
    fn g5_an_incomplete_scan_never_quarantines() {
        let (_d, probe) = probe_over(&[("big.txt", &"x".repeat(1_100_000))]);
        let mut plan = settled("The loader calls `FrobnicateAll` on start.");
        run(&mut plan, &probe);
        assert!(plan.quarantined.is_empty());
        assert_eq!(plan.verify.len(), 1);
    }

    #[test]
    fn g5_a_token_in_a_source_stays() {
        let (_d, probe) = probe_over(&[("src/a.rs", "struct Widget;")]);
        let mut plan = settled("`Widget` holds the state.");
        run(&mut plan, &probe);
        assert_eq!(plan.settled.len(), 1);
    }

    #[test]
    fn g5_a_task_token_found_nowhere_is_marked_new() {
        let mut plan = BuildPlan {
            tasks: vec![Item::new("T1", "Add `GateThing` to the loader.")],
            ..BuildPlan::default()
        };
        run(&mut plan, &SourceProbe::default());
        assert_eq!(plan.tasks.len(), 1);
        assert!(markers(&plan.tasks[0]).contains("new: GateThing"));
    }

    #[test]
    fn g6_facts_minus_pairs_is_mixed_units() {
        let mut plan = settled("That leaves 56 facts excluding 30 pairs.");
        plan.settled[0]
            .fields
            .insert("count".into(), "`wc -l facts`".into());
        run(&mut plan, &SourceProbe::default());
        assert!(plan.settled.is_empty());
        assert!(markers(&plan.verify[0]).contains("mixed units: facts vs pairs"));
    }

    #[test]
    fn g6_an_uncounted_figure_asks_for_a_recount() {
        let mut plan = settled("The vault has 56 facts.");
        run(&mut plan, &SourceProbe::default());
        assert!(plan.settled.is_empty());
        assert!(markers(&plan.verify[0]).contains("recount: no count command"));
    }

    #[test]
    fn g6_a_figure_absent_from_the_discussion_is_flagged() {
        let mut plan = settled("The vault has 91 facts.");
        plan.settled[0]
            .fields
            .insert("count".into(), "`wc -l facts`".into());
        run(&mut plan, &SourceProbe::default());
        assert!(markers(&plan.verify[0]).contains("figure not in the discussion: 91"));
    }

    #[test]
    fn g6_a_task_figure_is_only_marked() {
        let mut plan = BuildPlan {
            tasks: vec![Item::new("T1", "Trim to 77 lines.")],
            ..BuildPlan::default()
        };
        run(&mut plan, &SourceProbe::default());
        assert_eq!(plan.tasks.len(), 1);
        assert!(markers(&plan.tasks[0]).contains("figure not in the discussion: 77"));
    }

    #[test]
    fn g12_next_free_goes_to_verify_first() {
        let mut plan = settled("The next free ADR is in `docs/adr`.");
        run(&mut plan, &SourceProbe::default());
        assert!(plan.settled.is_empty());
        let item = &plan.verify[0];
        assert_eq!(item.field("check"), Some("`ls docs/adr | tail -1`"));
        assert!(markers(item).contains("freshness"));
    }

    #[test]
    fn g12_a_task_with_latest_is_only_marked() {
        let mut plan = BuildPlan {
            tasks: vec![Item::new("T1", "Read the latest build log.")],
            ..BuildPlan::default()
        };
        run(&mut plan, &SourceProbe::default());
        assert_eq!(plan.tasks.len(), 1);
        assert!(markers(&plan.tasks[0]).contains("freshness"));
    }
}
