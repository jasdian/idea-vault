//! G4 anchor and symbol, G5 tokens and names, G6 figures and units, G12 freshness.
//!
//! Each Settled, Fence and Kill item moves at most once: the first gate to fault it decides where
//! it goes, and later gates never see it. Plan tasks are never moved, only marked, since a task
//! may legitimately create a new name or refer to a figure the discussion never stated. A token
//! or anchor the probe could not check is unknown, never absent: it goes to Verify first, and
//! only a complete scan (or no attached source at all) can quarantine.

use super::{GateInputs, GateReport};
use crate::ai::sources::{AnchorCheck, SourceProbe, TokenScan};
use crate::concepts::build_plan::plan::{BuildPlan, Item, Provenance};

const NEGATION_CUES: [&str; 7] = [
    "never", "no", "not", "missing", "absent", "unused", "nothing",
];
const SUBTRACTION_CUES: [&str; 5] = ["excluding", "remaining", "minus", "without", "less"];
const FRESHNESS_CUES: [&str; 3] = ["next free", "current max", "as of"];
const NEGATION_WINDOW: usize = 4;
const KNOWN_UNITS: [&str; 30] = [
    "ms", "s", "sec", "secs", "second", "seconds", "min", "mins", "minute", "minutes", "h", "hr",
    "hrs", "hour", "hours", "day", "days", "week", "weeks", "month", "months", "year", "kb", "mb",
    "gb", "tb", "byte", "bytes", "line", "lines",
];
const NOT_PLURALS: [&str; 8] = [
    "does",
    "goes",
    "less",
    "plus",
    "always",
    "perhaps",
    "sometimes",
    "versus",
];
const FILE_EXTENSIONS: [&str; 12] = [
    "rs", "md", "toml", "json", "js", "ts", "py", "html", "yml", "yaml", "sh", "txt",
];
const PLAINLY: &str = "name it plainly: no safe check for this text";
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
    scan: TokenScan,
    complete: bool,
}

impl<'a> Ctx<'a> {
    fn new(inputs: &GateInputs<'a>, plan: &BuildPlan) -> Self {
        let evidence = inputs.evidence;
        let join = |who: &[Provenance]| {
            who.iter()
                .flat_map(|w| evidence.text_by(*w))
                .collect::<Vec<_>>()
                .join("\n")
                .to_lowercase()
        };
        let gated = plan
            .settled
            .iter()
            .chain(&plan.kills)
            .chain(&plan.fence)
            .flat_map(|i| tokens(&claim_text(i)));
        let planned = plan.tasks.iter().flat_map(|t| tokens(&t.text));
        let mut wanted: Vec<String> = gated.chain(planned).collect();
        wanted.sort();
        wanted.dedup();
        let scan = inputs.probe.find_tokens(&wanted);
        Ctx {
            probe: inputs.probe,
            complete: inputs.probe.is_empty() || scan.complete,
            scan,
            owner_idea: join(&[Provenance::Owner, Provenance::Idea]),
            foil: join(&[Provenance::Foil]),
            all: join(&[Provenance::Owner, Provenance::Idea, Provenance::Foil]),
        }
    }
}

pub fn apply(plan: &mut BuildPlan, inputs: &GateInputs, report: &mut GateReport) {
    let ctx = Ctx::new(inputs, plan);
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
            Some(G4::Faulty { check, marker }) => return Verdict::Verify { check, marker },
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
        let name_like = token
            .chars()
            .any(|c| c == '.' || c == '/' || c.is_uppercase());
        if name_like && !known(ctx, &token) && !word_in(&ctx.owner_idea, &token.to_lowercase()) {
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
    if path.is_empty() || path.contains(char::is_whitespace) || !file_like(path) || host_like(path)
    {
        return None;
    }
    let (a, b) = range.split_once('-').unwrap_or((range, range));
    let (first, last): (usize, usize) = (a.parse().ok()?, b.parse().ok()?);
    (first <= last).then(|| Anchor {
        label: span.to_string(),
        path: path.to_string(),
        first,
        last,
    })
}

fn has_extension(path: &str) -> bool {
    path.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty() && FILE_EXTENSIONS.contains(&ext.to_lowercase().as_str())
    })
}

fn file_like(path: &str) -> bool {
    path.contains('/') || has_extension(path)
}

fn host_like(path: &str) -> bool {
    path.contains("://") || path.chars().all(|c| c.is_ascii_digit() || c == '.')
}

fn host_port(token: &str) -> bool {
    token.rsplit_once(':').is_some_and(|(host, port)| {
        !host.is_empty() && !port.is_empty() && port.chars().all(|c| c.is_ascii_digit())
    })
}

fn path_like(token: &str) -> bool {
    !token.contains("://")
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/'))
        && file_like(token)
}

fn shell_safe(text: &str) -> bool {
    !text.is_empty()
        && !text
            .chars()
            .any(|c| c == '\n' || c == '\\' || "`;|&$()<>\"'*?!".contains(c))
}

fn quote(text: &str) -> String {
    format!("'{text}'")
}

/// Whether the token is in the scanned sources, by content or (for a path) by existence.
fn known(ctx: &Ctx, token: &str) -> bool {
    ctx.scan.found.contains(token) || (path_like(token) && ctx.probe.has_path(token) == Some(true))
}

enum G4 {
    Unpaired(String),
    Faulty {
        check: Option<String>,
        marker: String,
    },
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
        let check = (shell_safe(&a.path) && shell_safe(symbol)).then(|| {
            format!(
                "`sed -n {},{}p -- {} | grep -nF -- {}`",
                a.first,
                a.last,
                quote(&a.path),
                quote(symbol)
            )
        });
        let marker = if check.is_some() {
            faulty
        } else {
            format!("{faulty}; {PLAINLY}")
        };
        return Some(G4::Faulty { check, marker });
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
            && !host_port(&t)
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

fn grep_verdict(token: &str, marker: String) -> Verdict {
    if shell_safe(token) {
        Verdict::Verify {
            check: Some(format!("`grep -rnF -- {} .`", quote(token))),
            marker,
        }
    } else {
        Verdict::Verify {
            check: None,
            marker: format!("{marker}; {PLAINLY}"),
        }
    }
}

/// Whether a negation cue sits within a few words of the token in the item's own words.
fn negated_near(text: &str, token: &str) -> bool {
    let ws = words(text);
    let needle = words(token);
    if needle.is_empty() || ws.len() < needle.len() {
        return false;
    }
    let cue = |w: &String| NEGATION_CUES.contains(&w.as_str());
    (0..=ws.len() - needle.len())
        .filter(|&at| ws[at..at + needle.len()] == needle[..])
        .any(|at| {
            let end = at + needle.len();
            ws[at.saturating_sub(NEGATION_WINDOW)..at].iter().any(cue)
                || ws[end..(end + NEGATION_WINDOW).min(ws.len())]
                    .iter()
                    .any(cue)
        })
}

fn g5(item: &Item, ctx: &Ctx, report: &mut GateReport) -> Option<Verdict> {
    let text = claim_text(item);
    let mut nowhere: Vec<(String, bool)> = Vec::new();
    let mut foil: Vec<String> = Vec::new();
    for t in tokens(&text) {
        let lower = t.to_lowercase();
        if known(ctx, &t) || word_in(&ctx.owner_idea, &lower) {
            continue;
        }
        if word_in(&ctx.foil, &lower) {
            foil.push(t);
        } else {
            let unknown_path =
                path_like(&t) && !ctx.probe.is_empty() && ctx.probe.has_path(&t).is_none();
            nowhere.push((t, ctx.complete && !unknown_path));
        }
    }
    let unproven = nowhere.iter().map(|(t, _)| t).chain(&foil);
    if let Some(token) = unproven.into_iter().find(|t| negated_near(&text, t)) {
        report.count("premises");
        return Some(grep_verdict(
            token,
            "absence claim: a premise, not a fact".to_string(),
        ));
    }
    if let Some((token, complete)) = nowhere.first() {
        if *complete {
            return Some(Verdict::Quarantine(format!(
                "{token} appears nowhere in the discussion or the sources"
            )));
        }
        return Some(grep_verdict(
            token,
            format!("unverified: {token} not found, source scan incomplete"),
        ));
    }
    let token = foil.first()?;
    Some(grep_verdict(
        token,
        format!("foil-coined: {token} — only the foil wrote it"),
    ))
}

struct Figure {
    num: String,
    unit: Option<String>,
}

fn is_unit(word: &str) -> bool {
    KNOWN_UNITS.contains(&word)
        || (word.len() >= 4
            && word.ends_with('s')
            && !["ss", "us", "is"].iter().any(|e| word.ends_with(e))
            && !NOT_PLURALS.contains(&word))
}

fn numeric(piece: &str) -> bool {
    piece.chars().next().is_some_and(|c| c.is_ascii_digit())
        && piece
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == ',')
}

/// A bare number that names a port, a year or an identifier rather than a count.
fn label_number(num: &str, prev: Option<&str>) -> bool {
    let year = num.len() == 4 && (num.starts_with("19") || num.starts_with("20"));
    let padded = num.len() >= 2 && num.starts_with('0');
    year || padded || matches!(prev, Some("port" | "ports"))
}

/// Counts in `text` outside backticks: a number with a unit, or a bare count of at least two
/// digits that is not a port, year or identifier. Comparator and currency prefixes are stripped
/// and `/` separates pieces, so `>5 failures/15 min` holds two figures.
fn figures(text: &str) -> Vec<Figure> {
    let (_, outside) = spans(text);
    let mut pieces: Vec<String> = Vec::new();
    for raw in outside.split_whitespace() {
        let w =
            trim_edges(raw).trim_start_matches(['>', '<', '=', '~', '$', '€', '£', '≥', '≤', '+']);
        pieces.extend(
            w.split('/')
                .filter(|p| !p.is_empty())
                .map(str::to_lowercase),
        );
    }
    let mut out = Vec::new();
    for (i, piece) in pieces.iter().enumerate() {
        let percent = piece.ends_with('%');
        let body = piece.strip_suffix('%').unwrap_or(piece);
        let (num, glued) = match body.find(|c: char| c.is_alphabetic()) {
            Some(at) if at > 0 && KNOWN_UNITS.contains(&&body[at..]) => {
                (&body[..at], Some(body[at..].to_string()))
            }
            _ => (body, None),
        };
        if !numeric(num) {
            continue;
        }
        let unit = if percent {
            Some("%".to_string())
        } else {
            glued.or_else(|| {
                pieces
                    .get(i + 1)
                    .map(|n| trim_edges(n).to_string())
                    .filter(|n| n.chars().all(|c| c.is_ascii_alphabetic()) && is_unit(n))
            })
        };
        let digits = num.chars().filter(char::is_ascii_digit).count();
        let prev = i.checked_sub(1).map(|p| pieces[p].as_str());
        if unit.is_some() || (digits >= 2 && !label_number(num, prev)) {
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
    let joined = format!(" {} ", words(text).join(" "));
    FRESHNESS_CUES
        .iter()
        .chain(&["latest"])
        .any(|c| joined.contains(&format!(" {c} ")))
}

fn g12(item: &Item) -> Option<Verdict> {
    if !freshness_cue(&item.text) {
        return None;
    }
    let (ticks, _) = spans(&item.text);
    let dir = ticks
        .iter()
        .find(|t| {
            t.contains('/')
                && !t.starts_with('/')
                && !t.contains("://")
                && !t.contains(char::is_whitespace)
                && parse_anchor(t).is_none()
        })
        .map(|t| {
            let t = t.trim_end_matches('/');
            match t.rsplit_once('/') {
                Some((parent, last)) if last.contains('.') => parent.to_string(),
                _ => t.to_string(),
            }
        });
    Some(Verdict::Verify {
        check: dir
            .filter(|d| shell_safe(d))
            .map(|d| format!("`ls -- {} | tail -1`", quote(&d))),
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
            Some("`sed -n 1,2p -- 'src/store.rs' | grep -nF -- 'parse_turn'`")
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
        assert_eq!(
            item.field("check"),
            Some("`grep -rnF -- '[[slug#fact]]' .`")
        );
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
        assert_eq!(item.field("check"), Some("`grep -rnF -- 'ACCOUNT_MODE' .`"));
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
        assert_eq!(item.field("check"), Some("`ls -- 'docs/adr' | tail -1`"));
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

    #[test]
    fn g4_a_host_port_in_a_settled_item_is_not_an_anchor() {
        let mut plan = settled(
            "The app binds `10.0.0.7:8080`, talks to `db.internal:5432` and logs at `12:30`.",
        );
        run(&mut plan, &SourceProbe::default());
        assert_eq!(plan.settled.len(), 1, "{:?}", plan.verify);
    }

    #[test]
    fn g4_a_reversed_range_is_not_an_anchor() {
        assert!(parse_anchor("src/store.rs:20-10").is_none());
        assert!(parse_anchor("src/store.rs:10-20").is_some());
        assert!(parse_anchor("store.rs:7").is_some());
    }

    #[test]
    fn g4_a_verify_first_item_gets_the_marker() {
        let (_d, probe) = probe_over(&[("src/store.rs", CODE)]);
        let mut plan = BuildPlan {
            verify: vec![Item::new(
                "V1",
                "Turns parse at `src/store.rs:3-3` in `parse_turn`.",
            )],
            ..BuildPlan::default()
        };
        run(&mut plan, &probe);
        assert!(markers(&plan.verify[0]).contains("resolved in code"));
    }

    #[test]
    fn g4_a_fence_item_is_not_anchor_gated() {
        let mut plan = BuildPlan {
            fence: vec![Item::new("F1", "Leave `src/store.rs:3-4` alone.")],
            ..BuildPlan::default()
        };
        run(&mut plan, &SourceProbe::default());
        assert_eq!(plan.fence.len(), 1);
    }

    #[test]
    fn g5_an_unrelated_not_does_not_shield_an_invented_token() {
        let mut plan =
            settled("The loader runs `FrobnicateAll` and then the cache is not warm at start.");
        run(&mut plan, &SourceProbe::default());
        assert!(plan.verify.is_empty(), "{:?}", plan.verify);
        assert_eq!(plan.quarantined.len(), 1);
    }

    #[test]
    fn g5_a_token_with_shell_characters_gets_no_check() {
        let mut plan = settled("`x;rm${IFS}-rf` is never queried by the loader.");
        run(&mut plan, &SourceProbe::default());
        let item = &plan.verify[0];
        assert_eq!(item.field("check"), None);
        assert!(markers(item).contains("name it plainly"));
    }

    #[test]
    fn g5_a_dash_token_is_quoted_after_a_double_dash() {
        let mut plan = settled("`-rf` is never queried by the loader.");
        run(&mut plan, &SourceProbe::default());
        assert_eq!(
            plan.verify[0].field("check"),
            Some("`grep -rnF -- '-rf' .`")
        );
    }

    #[test]
    fn g5_a_real_path_in_a_source_is_not_coined() {
        let (_d, probe) = probe_over(&[("src/gate/mod.rs", "// nothing here\n")]);
        let mut plan = settled("The gates live in `src/gate/mod.rs`.");
        run(&mut plan, &probe);
        assert_eq!(plan.settled.len(), 1, "{:?}", plan.quarantined);
    }

    #[test]
    fn g6_a_kill_threshold_with_a_comparator_is_seen() {
        let mut plan = BuildPlan {
            kills: vec![Item::new("K1", "Stop at >5 failures/15 min.")],
            ..BuildPlan::default()
        };
        run(&mut plan, &SourceProbe::default());
        assert!(plan.kills.is_empty());
        assert!(markers(&plan.verify[0]).contains("figure not in the discussion: 5"));
    }

    #[test]
    fn g6_ports_years_ids_and_step_numbers_are_not_counts() {
        let mut plan = settled("In 2026 step 3 is done on port 3000 under ADR 0030.");
        run(&mut plan, &SourceProbe::default());
        assert_eq!(plan.settled.len(), 1, "{:?}", plan.verify);
    }

    #[test]
    fn g12_alias_of_is_not_a_freshness_cue() {
        let mut plan = settled("The alias of the store is the vault.");
        run(&mut plan, &SourceProbe::default());
        assert_eq!(plan.settled.len(), 1, "{:?}", plan.verify);
    }
}
