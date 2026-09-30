//! The Ground stage (docs/adr/0034, D35): before anyone argues about an idea that changes code,
//! map the attached sources and verify every anchor a reader cites — in code, never by a model.
//!
//! Three steps. A code map with no model call: the top of the tree plus every path-like token the
//! idea and the recent discussion name, each resolved against the files. Then up to three readers
//! (the hidden `ground-read` skill on a 2×2 tool budget), each answering from one angle with at
//! most eight ``- `path:N` | `symbol` | claim`` lines. Then parse, dedupe and verify: every anchor
//! goes through [`SourceProbe::check_anchor`]. Only verified anchors are carried forward; a claim
//! whose file or symbol is not there is carried as a path that does not exist, never as its text.
//! Ground verifies existence, not meaning — the audit stays the check on meaning.

use futures::future::join_all;

use crate::ai::contract::{backtick_spans, parse_anchor, MAX_STAGE_LINES};
use crate::ai::sources::{AnchorCheck, PathResolution, SourceProbe};
use crate::concepts::agents::{build_prompt, AgentRole, AgentTask};
use crate::concepts::audit;
use crate::concepts::skills::ask_on_contract;
use crate::concepts::workflows::run::{
    stage_context, PendingArtifact, RunCtx, StageOutcome, StageStatus,
};
use crate::concepts::ConceptError;
use crate::domain::evidence::normalize_for_match;
use crate::domain::workflow::GroundSpec;
use crate::domain::{ArtifactKind, OutputContract};
use crate::vault::store;

/// The stage budget divisor the carried grounded map may take (a quarter), so the discussion and
/// the later stages' own blocks keep the rest.
pub const GROUND_DIVISOR: usize = 4;

/// How deep the code outline goes, and how many lines it keeps.
const OUTLINE_DEPTH: usize = 2;
const OUTLINE_MAX: usize = 80;

/// How many of the latest discussion turns the token miner reads, next to the idea statement.
const MINED_TURNS: usize = 6;

/// Most path-like and identifier tokens the miner keeps, so a pasted log cannot flood the probe.
const MAX_MINED_TOKENS: usize = 24;

/// Extensions that make a bare word a path (`chat.rs`), matching the build-plan gates' notion of
/// a file-like token.
const KNOWN_EXTENSIONS: [&str; 12] = [
    "rs", "md", "sh", "toml", "html", "py", "ts", "js", "json", "yml", "yaml", "css",
];

/// Widest cited range (in lines) the verifier will test; a wider anchor stays unverified.
const MAX_ANCHOR_SPAN: usize = 40;

/// Symbols that occur in nearly every source file, so finding one proves nothing about a claim.
const COMMON_SYMBOLS: [&str; 24] = [
    "use", "pub", "let", "mut", "new", "self", "Self", "impl", "mod", "for", "and", "the", "not",
    "struct", "enum", "async", "await", "return", "match", "const", "true", "false", "None",
    "Some",
];

/// The skill every reader runs through.
const READER_SKILL: &str = "ground-read";

/// Each reader's tool budget: at most two tool calls in each of `tool_rounds` rounds.
const READER_CALLS_PER_ROUND: usize = 2;

/// The readers' angles when a definition names none, in reader order.
pub const DEFAULT_ANGLES: [&str; 3] = [
    "where the code this idea changes lives",
    "entry points and routes",
    "scripts, config and tests",
];

/// One cited anchor: `source` is empty until [`dedupe`] resolves the path to one attached file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub source: String,
    pub path: String,
    pub first: usize,
    pub last: usize,
    pub symbol: String,
    pub claim: String,
}

impl Claim {
    fn anchor(&self) -> String {
        if self.first == self.last {
            format!("{}:{}", self.path, self.first)
        } else {
            format!("{}:{}-{}", self.path, self.first, self.last)
        }
    }
}

/// What the probe made of one claim's anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimVerdict {
    /// The symbol is on the cited lines.
    Verified,
    /// The file has the symbol on another line; the claim is re-anchored there.
    VerifiedMoved,
    /// No such file, no such symbol in it, or the path fits more than one file.
    Disproved,
    /// The probe could not settle it (capped walk, unreadable file, no symbol).
    Unverified,
}

impl ClaimVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            ClaimVerdict::Verified => "verified",
            ClaimVerdict::VerifiedMoved => "verified (moved)",
            ClaimVerdict::Disproved => "disproved",
            ClaimVerdict::Unverified => "unverified",
        }
    }

    fn is_verified(self) -> bool {
        matches!(self, ClaimVerdict::Verified | ClaimVerdict::VerifiedMoved)
    }
}

/// A claim with its verdict and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedClaim {
    pub claim: Claim,
    pub verdict: ClaimVerdict,
    pub note: String,
}

/// The code map built before any reader runs: the outline, each path-like token the discussion
/// names with where it lands, and the backticked identifiers the sources do not contain.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodeMap {
    pub outline: Vec<String>,
    pub paths: Vec<(String, PathResolution)>,
    pub missing_symbols: Vec<String>,
}

/// Everything the stage learned: the code map, every claim with its verdict, and whether the
/// probe's walk was complete (an incomplete walk leaves misses unverified, never disproved).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroundMap {
    pub code: CodeMap,
    pub claims: Vec<CheckedClaim>,
    pub complete: bool,
}

impl GroundMap {
    fn count(&self, verdict: ClaimVerdict) -> usize {
        self.claims.iter().filter(|c| c.verdict == verdict).count()
    }

    /// `verified V of T (M moved, D disproved)` — the progress note and the run record detail.
    pub fn tally(&self) -> String {
        let moved = self.count(ClaimVerdict::VerifiedMoved);
        format!(
            "verified {} of {} ({moved} moved, {} disproved)",
            self.count(ClaimVerdict::Verified) + moved,
            self.claims.len(),
            self.count(ClaimVerdict::Disproved)
        )
    }
}

fn has_known_extension(token: &str) -> bool {
    token.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty() && KNOWN_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
    })
}

fn is_path_like(token: &str) -> bool {
    if token.contains("://") || token.contains(char::is_whitespace) || token.len() < 3 {
        return false;
    }
    let slashed = token.contains('/') && token.chars().any(char::is_alphanumeric);
    has_known_extension(token) || slashed
}

/// A bare prose word that is slashed but has no known extension (`src/web`, but also `and/or`,
/// `client/server`, `24/7`): only a candidate. Every component must be path-shaped and at least one
/// must carry a letter; [`code_map`] then keeps it only if it resolves to an attached file.
fn is_loose_path(token: &str) -> bool {
    let parts: Vec<&str> = token
        .trim_start_matches("./")
        .trim_matches('/')
        .split('/')
        .collect();
    parts.len() >= 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        })
        && parts
            .iter()
            .any(|p| p.chars().any(|c| c.is_ascii_alphabetic()))
}

/// What [`mine_tokens`] found: `paths` the author plainly meant as paths (backticked, or carrying
/// a [`KNOWN_EXTENSIONS`] extension), `loose` the slashed prose words that might be directories,
/// and `idents` the backticked identifiers. Each list is deduped in first-seen order and capped
/// on its own, so prose like `and/or` can never crowd a real path out.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Mined {
    pub paths: Vec<String>,
    pub loose: Vec<String>,
    pub idents: Vec<String>,
}

fn is_identifier(token: &str) -> bool {
    token.len() >= 3
        && token.chars().any(char::is_alphabetic)
        && token
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == ':')
}

/// Path-like tokens and backticked identifiers in `text`, sorted into [`Mined`]. A path is
/// backticked text that looks like one, or a whitespace token ending in a [`KNOWN_EXTENSIONS`]
/// extension; a slashed whitespace token without one is only a loose candidate; an identifier is
/// backticked text that is not a path.
pub fn mine_tokens(text: &str) -> Mined {
    let mut mined = Mined::default();
    let push = |into: &mut Vec<String>, t: &str| {
        if !into.iter().any(|o| o == t) && into.len() < MAX_MINED_TOKENS {
            into.push(t.to_string());
        }
    };
    for line in text.lines() {
        for span in backtick_spans(line) {
            let span = span.trim();
            let span = parse_anchor(span).map_or(span, |(p, _, _)| p);
            if is_path_like(span) {
                push(&mut mined.paths, span);
            } else if is_identifier(span) {
                push(&mut mined.idents, span);
            }
        }
        for word in line.split_whitespace() {
            if word.contains('`') {
                continue;
            }
            let word = word.trim_matches(|c: char| {
                matches!(
                    c,
                    ',' | ';' | ':' | '(' | ')' | '"' | '\'' | '[' | ']' | '*'
                )
            });
            let word = word.trim_end_matches(['.', '!', '?']);
            if !is_path_like(word) {
                continue;
            }
            if has_known_extension(word) {
                push(&mut mined.paths, word);
            } else if is_loose_path(word) {
                push(&mut mined.loose, word);
            }
        }
    }
    mined
}

/// The code map over `probe` for `text` (the idea statement and recent discussion). A loose
/// candidate is kept only when it lands on an attached file, so prose such as `client/server` is
/// never listed as a path that does not exist. Blocking file I/O — call from `spawn_blocking`.
pub fn code_map(probe: &SourceProbe, text: &str) -> CodeMap {
    let Mined {
        paths,
        loose,
        idents,
    } = mine_tokens(text);
    let scan = probe.find_tokens(&idents);
    let missing_symbols = if scan.complete {
        idents
            .into_iter()
            .filter(|t| !scan.found.contains(t))
            .collect()
    } else {
        Vec::new()
    };
    let mut resolved: Vec<(String, PathResolution)> = paths
        .into_iter()
        .map(|p| {
            let at = probe.resolve_path(&p);
            (p, at)
        })
        .collect();
    for p in loose {
        if resolved.len() >= MAX_MINED_TOKENS {
            break;
        }
        let at = probe.resolve_path(&p);
        if matches!(
            at,
            PathResolution::Unique(..) | PathResolution::Ambiguous(_)
        ) {
            resolved.push((p, at));
        }
    }
    CodeMap {
        outline: probe.outline(OUTLINE_DEPTH, OUTLINE_MAX),
        paths: resolved,
        missing_symbols,
    }
}

/// Parse a reader's answer into claims: each line with a backticked `path:N[-M]` anchor and a
/// second backticked span as its symbol. The claim text is the third `|` cell of the pipe form,
/// else whatever the line says outside its backticks (the build-plan G4 form). At most
/// [`MAX_STAGE_LINES`] claims per answer.
pub fn parse_claims(raw: &str) -> Vec<Claim> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let spans = backtick_spans(line);
        let Some((path, first, last)) = spans.iter().find_map(|s| parse_anchor(s)) else {
            continue;
        };
        let symbol = spans.iter().map(|s| s.trim()).find(|s| {
            !s.is_empty() && !s.contains(char::is_whitespace) && parse_anchor(s).is_none()
        });
        let Some(symbol) = symbol else {
            continue;
        };
        let body = line.trim().trim_start_matches(['-', '*', '+']).trim_start();
        let cells: Vec<&str> = body.split('|').collect();
        let claim = if cells.len() >= 3 {
            cells[2..].join("|")
        } else {
            let mut outside = String::new();
            for (k, piece) in body.split('`').enumerate() {
                if k % 2 == 0 {
                    outside.push_str(piece);
                }
            }
            outside
        };
        let claim = claim
            .trim()
            .trim_start_matches(['|', '—', '–', '-', ':'])
            .trim()
            .to_string();
        out.push(Claim {
            source: String::new(),
            path: path.trim_start_matches("./").to_string(),
            first,
            last,
            symbol: symbol.to_string(),
            claim,
        });
        if out.len() == MAX_STAGE_LINES {
            break;
        }
    }
    out
}

/// Resolve each claim's path to (source, root-relative path) where it lands on one file — an
/// absolute claude-code path included — then merge claims that share source, path, symbol and
/// normalized text and whose line ranges overlap. Sorted, so the same answers always give the
/// same map whatever order the readers finished in. Blocking — call from `spawn_blocking`.
pub fn dedupe(claims: Vec<Claim>, probe: &SourceProbe) -> Vec<Claim> {
    let mut normalized: Vec<Claim> = claims
        .into_iter()
        .map(|mut c| {
            if let Some((source, rel)) = probe.normalize(&c.path) {
                (c.source, c.path) = (source, rel);
            }
            c
        })
        .collect();
    let key = |c: &Claim| {
        (
            c.source.clone(),
            c.path.clone(),
            c.symbol.clone(),
            normalize_for_match(&c.claim)
                .trim_end_matches(['.', '!', ';', ','])
                .to_string(),
        )
    };
    normalized.sort_by(|a, b| (key(a), a.first, a.last).cmp(&(key(b), b.first, b.last)));
    let mut out: Vec<Claim> = Vec::new();
    for c in normalized {
        match out.last_mut() {
            Some(prev) if key(prev) == key(&c) && c.first <= prev.last => {
                prev.last = prev.last.max(c.last);
            }
            _ => out.push(c),
        }
    }
    out.sort_by(|a, b| {
        (&a.source, &a.path, a.first, a.last, &a.symbol, &a.claim)
            .cmp(&(&b.source, &b.path, b.first, b.last, &b.symbol, &b.claim))
    });
    out
}

/// Check every claim's anchor against the files (docs/adr/0034): resolved is verified, a symbol
/// on another line of the file is verified and re-anchored there, a missing file or symbol or an
/// ambiguous path is disproved, and anything the probe cannot settle stays unverified. Blocking —
/// call from `spawn_blocking`.
pub fn verify(claims: Vec<Claim>, probe: &SourceProbe) -> GroundMap {
    let checked = claims
        .into_iter()
        .map(|mut claim| {
            if let Some(why) = too_loose_to_check(&claim) {
                return CheckedClaim {
                    claim,
                    verdict: ClaimVerdict::Unverified,
                    note: why.to_string(),
                };
            }
            let (verdict, note) =
                match probe.check_anchor(&claim.path, claim.first, claim.last, &claim.symbol) {
                    AnchorCheck::Resolved { .. } => (ClaimVerdict::Verified, String::new()),
                    AnchorCheck::Moved { line, .. } => {
                        let note = format!("moved from {}", claim.anchor());
                        (claim.first, claim.last) = (line, line);
                        (ClaimVerdict::VerifiedMoved, note)
                    }
                    AnchorCheck::SymbolMissing { path, .. } => (
                        ClaimVerdict::Disproved,
                        format!("{} is not in {path}", claim.symbol),
                    ),
                    AnchorCheck::NoFile => (ClaimVerdict::Disproved, "no such file".to_string()),
                    AnchorCheck::Ambiguous(candidates) => (
                        ClaimVerdict::Disproved,
                        format!("ambiguous: {}", candidates.join(", ")),
                    ),
                    AnchorCheck::Unverified => {
                        (ClaimVerdict::Unverified, "could not be checked".to_string())
                    }
                };
            CheckedClaim {
                claim,
                verdict,
                note,
            }
        })
        .collect();
    GroundMap {
        code: CodeMap::default(),
        claims: checked,
        complete: probe.is_complete(),
    }
}

/// Why an anchor proves nothing even if the probe finds it: a range so wide it covers most of a
/// file, or a symbol so short or common (`e`, `use`, `self`) it occurs in almost any file. Such a
/// claim stays unverified — never carried under "verified anchors" (docs/adr/0034).
fn too_loose_to_check(c: &Claim) -> Option<&'static str> {
    if c.first.abs_diff(c.last) >= MAX_ANCHOR_SPAN {
        return Some("range too wide to check");
    }
    let symbol = c.symbol.as_str();
    let identifier_like = symbol.chars().count() >= 3
        && symbol.chars().any(char::is_alphabetic)
        && symbol
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '_' | ':' | '.' | '-'));
    if !identifier_like || COMMON_SYMBOLS.contains(&symbol) {
        return Some("symbol too common to check");
    }
    None
}

fn located(c: &Claim) -> String {
    if c.source.is_empty() {
        c.anchor()
    } else {
        format!("{}:{}", c.source, c.anchor())
    }
}

/// The map as a carried `## Prior stage: grounded map` block of at most `cap` bytes: the outline,
/// the verified anchors (with where each discussion path really is), the paths that do not
/// exist, and how many claims stayed unverified. Over the cap, outline lines go first, then
/// does-not-exist lines, and verified anchors last.
pub fn carried_block(map: &GroundMap, cap: usize) -> String {
    let mut outline: Vec<String> = map.code.outline.iter().map(|l| format!("- {l}")).collect();
    let mut verified: Vec<String> = map
        .code
        .paths
        .iter()
        .filter_map(|(token, at)| match at {
            PathResolution::Unique(_, rel) if rel != token => {
                Some(format!("- `{token}` is `{rel}`"))
            }
            PathResolution::Ambiguous(candidates) => Some(format!(
                "- `{token}` is ambiguous: {}",
                candidates.join(", ")
            )),
            _ => None,
        })
        .collect();
    verified.extend(
        map.claims
            .iter()
            .filter(|c| c.verdict.is_verified())
            .map(|c| {
                format!(
                    "- `{}` | `{}` | {}",
                    c.claim.anchor(),
                    c.claim.symbol,
                    c.claim.claim
                )
            }),
    );
    let mut absent: Vec<String> = map
        .code
        .paths
        .iter()
        .filter(|(_, at)| *at == PathResolution::Absent)
        .map(|(token, _)| format!("- `{token}` (named in the discussion)"))
        .collect();
    absent.extend(
        map.code
            .missing_symbols
            .iter()
            .map(|s| format!("- `{s}` (no source mentions it)")),
    );
    // A disproved claim's text is never carried: only its anchor and why it failed.
    absent.extend(
        map.claims
            .iter()
            .filter(|c| c.verdict == ClaimVerdict::Disproved)
            .map(|c| format!("- `{}` `{}` — {}", c.claim.anchor(), c.claim.symbol, c.note)),
    );
    let unverified = map.count(ClaimVerdict::Unverified);
    let (total_outline, total_absent) = (outline.len(), absent.len());
    let render = |outline: &[String], verified: &[String], absent: &[String]| {
        let mut out = String::from(
            "## Prior stage: grounded map (anchors verified, claims not)\n\
             Each anchor below was checked against the attached files; what a claim says about the code was not.\n",
        );
        let shown = |kept: usize, of: usize| {
            if kept < of {
                format!(" (first {kept} of {of})")
            } else {
                String::new()
            }
        };
        if !outline.is_empty() {
            out.push_str(&format!(
                "\nCode outline{}:\n{}\n",
                shown(outline.len(), total_outline),
                outline.join("\n")
            ));
        }
        out.push_str("\nVerified anchors:\n");
        if verified.is_empty() {
            out.push_str("- none\n");
        } else {
            out.push_str(&verified.join("\n"));
            out.push('\n');
        }
        if !absent.is_empty() {
            out.push_str(&format!(
                "\nDoes not exist{}:\n{}\n",
                shown(absent.len(), total_absent),
                absent.join("\n")
            ));
        }
        if unverified > 0 {
            out.push_str(&format!(
                "\nUnverified: {unverified} claim(s) could not be checked and are left out.\n"
            ));
        }
        out.trim_end().to_string()
    };
    loop {
        let block = render(&outline, &verified, &absent);
        if block.len() <= cap {
            return block;
        }
        if outline.pop().is_some() || absent.pop().is_some() || verified.pop().is_some() {
            continue;
        }
        return audit::clip(&block, cap);
    }
}

/// A markdown table cell: pipes escaped, newlines flattened.
pub(crate) fn cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

/// The full map as a `ground_map` artifact body: every claim with its verdict — disproved and
/// unverified ones included — the discussion's paths, and the outline.
pub fn artifact_body(map: &GroundMap) -> String {
    let mut out = format!(
        "# Grounded map\n\n{} · probe walk {}\n\n## Claims\n\n",
        map.tally(),
        if map.complete {
            "complete"
        } else {
            "incomplete (misses are unverified, not disproved)"
        }
    );
    if map.claims.is_empty() {
        out.push_str("The readers produced no verifiable claims.\n");
    } else {
        out.push_str("| Verdict | Anchor | Symbol | Claim | Note |\n|---|---|---|---|---|\n");
        for c in &map.claims {
            out.push_str(&format!(
                "| {} | `{}` | `{}` | {} | {} |\n",
                c.verdict.as_str(),
                cell(&located(&c.claim)),
                cell(&c.claim.symbol),
                cell(&c.claim.claim),
                cell(&c.note)
            ));
        }
    }
    if !map.code.paths.is_empty() || !map.code.missing_symbols.is_empty() {
        out.push_str("\n## Named in the discussion\n\n");
        for (token, at) in &map.code.paths {
            let at = match at {
                PathResolution::Unique(source, rel) => format!("{source}:{rel}"),
                PathResolution::Ambiguous(c) => format!("ambiguous: {}", c.join(", ")),
                PathResolution::Absent => "does not exist".to_string(),
                PathResolution::Unknown => "could not be checked".to_string(),
            };
            out.push_str(&format!("- `{token}` → {at}\n"));
        }
        for s in &map.code.missing_symbols {
            out.push_str(&format!("- `{s}` → no source mentions it\n"));
        }
    }
    out.push_str("\n## Code outline\n\n");
    for line in &map.code.outline {
        out.push_str(&format!("- {line}\n"));
    }
    out
}

/// Run `probe`-bound blocking work off the async runtime and hand the probe back, so its one
/// walk is reused by the next step.
async fn blocking<T: Send + 'static>(
    probe: SourceProbe,
    work: impl FnOnce(&SourceProbe) -> T + Send + 'static,
) -> Result<(SourceProbe, T), ConceptError> {
    let joined = tokio::task::spawn_blocking(move || {
        let out = work(&probe);
        (probe, out)
    })
    .await;
    match joined {
        Ok(pair) => Ok(pair),
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => Err(ConceptError::Vault(crate::vault::VaultError::Io(
            std::io::Error::other(format!("ground probe task did not finish: {e}")),
        ))),
    }
}

/// The idea statement plus the latest [`MINED_TURNS`] discussion turns — what the token miner
/// reads.
fn mined_text(ctx: &RunCtx<'_>) -> Result<String, ConceptError> {
    let idea = store::read_idea(ctx.vault_dir, ctx.idea_slug)?;
    let turns = store::split_turns(&store::read_conversation(ctx.vault_dir, ctx.idea_slug)?);
    let tail = &turns[turns.len().saturating_sub(MINED_TURNS)..];
    Ok(format!("{}\n{}", idea.body, tail.join("\n")))
}

/// Run one Ground stage. With no source attached it skips with no call, no carried block and no
/// artifact, so a workflow over an idea without sources runs exactly as it would without Ground.
/// Otherwise the map comes back for the caller to carry and stage as an artifact, with the
/// off-contract note of every reader that kept an off-contract answer (ADR-0040).
pub(crate) async fn run_ground(
    ctx: &RunCtx<'_>,
    spec: &GroundSpec,
    note: &(dyn Fn(&str) + Sync),
) -> Result<(StageOutcome, Option<GroundMap>, Vec<String>), ConceptError> {
    let probe = ctx.llm.source_probe();
    if probe.is_empty() {
        note("no sources attached — ground skipped");
        return Ok((
            StageOutcome::skipped("no sources attached"),
            None,
            Vec::new(),
        ));
    }
    note("mapping the sources");
    let text = mined_text(ctx)?;
    let (probe, code) = blocking(probe, move |p| code_map(p, &text)).await?;

    let mut claims: Vec<Claim> = Vec::new();
    let mut contract_notes: Vec<String> = Vec::new();
    let readers = spec.readers.min(DEFAULT_ANGLES.len());
    if readers > 0 {
        let outline = format!("## Code outline\n{}", code.outline.join("\n"));
        let reader_llm = ctx
            .llm
            .for_role(AgentRole::Researcher.as_str())
            .with_tool_budget(spec.tool_rounds, READER_CALLS_PER_ROUND);
        let prompts = (0..readers)
            .map(|r| {
                let angle = spec.angles.get(r).map_or(DEFAULT_ANGLES[r], String::as_str);
                let carried = [outline.clone(), format!("## Your angle\n{angle}")];
                let context =
                    stage_context(ctx.vault_dir, ctx.idea_slug, ctx.budget, &carried, &|_| {
                        String::new()
                    })?;
                build_prompt(
                    &ctx.book.skills,
                    &AgentTask {
                        role: AgentRole::Researcher,
                        skill: Some(READER_SKILL.to_string()),
                        context,
                    },
                )
            })
            .collect::<Result<Vec<String>, ConceptError>>()?;
        let done = std::sync::atomic::AtomicUsize::new(0);
        let answers = join_all(prompts.into_iter().map(|prompt| {
            let (reader_llm, done) = (&reader_llm, &done);
            async move {
                let answer = ask_on_contract(
                    reader_llm,
                    ctx.sem,
                    prompt,
                    OutputContract::GroundClaims,
                    READER_SKILL,
                    &|_: &str| {},
                )
                .await;
                let k = done.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                note(&format!("reader {k}/{readers}"));
                answer
            }
        }))
        .await;
        for (r, answer) in answers.into_iter().enumerate() {
            match answer {
                Ok((text, outcome)) => {
                    contract_notes.extend(outcome.note(&format!("{READER_SKILL} {}", r + 1)));
                    claims.extend(parse_claims(&text));
                }
                Err(ConceptError::SemaphoreClosed) => return Err(ConceptError::SemaphoreClosed),
                Err(e) => tracing::warn!(error = %e, "ground reader failed; counted as no claims"),
            }
        }
    }

    let (_, mut map) = blocking(probe, move |p| verify(dedupe(claims, p), p)).await?;
    map.code = code;
    let status = if readers > 0 && map.claims.iter().all(|c| !c.verdict.is_verified()) {
        note("readers produced no verifiable claims — code map only");
        StageStatus::Degraded("readers produced no verifiable claims — code map only".into())
    } else {
        note(&map.tally());
        StageStatus::Ran
    };
    let outcome = StageOutcome {
        status,
        detail: map.tally(),
        artifact: Some(PendingArtifact {
            kind: ArtifactKind::GroundMap,
            title: "Grounded map".into(),
            lens: None,
            body: artifact_body(&map),
        }),
    };
    Ok((outcome, Some(map), contract_notes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Name;
    use crate::sources::ResolvedSource;

    fn fixture() -> (tempfile::TempDir, SourceProbe) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src/web/routes")).unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::write(
            root.join("src/web/routes/chat.rs"),
            "use x;\n\npub async fn post_chat() {}\n\nfn helper() {}\n",
        )
        .unwrap();
        std::fs::write(root.join("scripts/gate.sh"), "#!/bin/sh\ncargo test\n").unwrap();
        let probe = SourceProbe::new(&[ResolvedSource {
            name: Name::try_from("app").unwrap(),
            root,
        }]);
        (dir, probe)
    }

    fn claim(path: &str, first: usize, last: usize, symbol: &str, text: &str) -> Claim {
        Claim {
            source: String::new(),
            path: path.into(),
            first,
            last,
            symbol: symbol.into(),
            claim: text.into(),
        }
    }

    #[test]
    fn parses_pipe_backtick_and_absolute_anchors() {
        let raw = "Here you go:\n\
            - `src/web/routes/chat.rs:3` | `post_chat` | the chat route handler\n\
            * `/mnt/sources/app/scripts/gate.sh:2-2` | `cargo` | runs the tests\n\
            - `src/lib.rs:10-12` `run` wires the router\n\
            - `src/x.rs:4` | no symbol here\n\
            - plain prose line";
        let claims = parse_claims(raw);
        assert_eq!(
            claims,
            vec![
                claim(
                    "src/web/routes/chat.rs",
                    3,
                    3,
                    "post_chat",
                    "the chat route handler"
                ),
                claim(
                    "/mnt/sources/app/scripts/gate.sh",
                    2,
                    2,
                    "cargo",
                    "runs the tests"
                ),
                claim("src/lib.rs", 10, 12, "run", "wires the router"),
            ]
        );
        let many: String = (1..=12)
            .map(|i| format!("- `a.rs:{i}` | `x` | c\n"))
            .collect();
        assert_eq!(parse_claims(&many).len(), MAX_STAGE_LINES);
    }

    #[test]
    fn dedupe_merges_overlapping_ranges_deterministically() {
        let (_dir, probe) = fixture();
        let a = vec![
            claim("chat.rs", 3, 4, "post_chat", "The chat handler."),
            claim(
                "src/web/routes/chat.rs",
                2,
                3,
                "post_chat",
                "the chat handler",
            ),
            claim(
                "src/web/routes/chat.rs",
                9,
                9,
                "post_chat",
                "the chat handler",
            ),
            claim("scripts/gate.sh", 2, 2, "cargo", "runs tests"),
        ];
        let mut b = a.clone();
        b.reverse();
        let (da, db) = (dedupe(a, &probe), dedupe(b, &probe));
        assert_eq!(
            da.iter()
                .map(|c| (c.source.as_str(), c.path.as_str(), c.first, c.last))
                .collect::<Vec<_>>(),
            [
                ("app", "scripts/gate.sh", 2, 2),
                ("app", "src/web/routes/chat.rs", 2, 4),
                ("app", "src/web/routes/chat.rs", 9, 9),
            ]
        );
        assert_eq!(
            da.iter()
                .map(|c| (&c.path, c.first, c.last))
                .collect::<Vec<_>>(),
            db.iter()
                .map(|c| (&c.path, c.first, c.last))
                .collect::<Vec<_>>(),
            "reader order does not change the map"
        );
    }

    #[test]
    fn probe_outcomes_map_to_verdicts() {
        let (_dir, probe) = fixture();
        let claims = dedupe(
            vec![
                claim("src/web/routes/chat.rs", 3, 3, "post_chat", "handler"),
                claim("src/web/routes/chat.rs", 1, 1, "helper", "a helper"),
                claim("src/web/routes/chat.rs", 1, 3, "no_such_fn", "missing"),
                claim("src/chat.rs", 1, 1, "post_chat", "wrong place"),
                claim(
                    "/elsewhere/chat.rs",
                    1,
                    1,
                    "post_chat",
                    "outside every source",
                ),
            ],
            &probe,
        );
        let map = verify(claims, &probe);
        let verdicts: Vec<(&str, ClaimVerdict)> = map
            .claims
            .iter()
            .map(|c| (c.claim.symbol.as_str(), c.verdict))
            .collect();
        assert!(verdicts.contains(&("post_chat", ClaimVerdict::Verified)));
        assert!(verdicts.contains(&("helper", ClaimVerdict::VerifiedMoved)));
        assert!(verdicts.contains(&("no_such_fn", ClaimVerdict::Disproved)));
        let wrong: Vec<_> = map
            .claims
            .iter()
            .filter(|c| c.claim.claim != "handler" && c.claim.symbol == "post_chat")
            .map(|c| c.verdict)
            .collect();
        assert_eq!(wrong, [ClaimVerdict::Disproved, ClaimVerdict::Disproved]);
        let moved = map
            .claims
            .iter()
            .find(|c| c.claim.symbol == "helper")
            .unwrap();
        assert_eq!((moved.claim.first, moved.claim.last), (5, 5), "re-anchored");
        assert!(map.complete);
        assert_eq!(map.tally(), "verified 2 of 5 (1 moved, 3 disproved)");

        // An incomplete walk leaves a miss unverified, never disproved.
        let (_dir2, capped) = fixture();
        let capped = capped.with_max_files(1);
        let map = verify(
            vec![
                claim("gone.rs", 1, 1, "x", "absent"),
                claim("chat.rs", 3, 3, "post_chat", "h"),
            ],
            &capped,
        );
        assert!(map
            .claims
            .iter()
            .all(|c| c.verdict == ClaimVerdict::Unverified));
        assert!(!map.complete);
    }

    #[test]
    fn wide_ranges_and_trivial_symbols_stay_unverified() {
        let (_dir, probe) = fixture();
        let map = verify(
            vec![
                claim("src/web/routes/chat.rs", 1, 99_999, "post_chat", "wide"),
                claim("src/web/routes/chat.rs", 1, 3, "e", "one letter"),
                claim("src/web/routes/chat.rs", 1, 1, "use", "keyword"),
                claim("src/web/routes/chat.rs", 1, 5, "post_chat", "tight"),
            ],
            &probe,
        );
        let verdicts: Vec<(&str, ClaimVerdict)> = map
            .claims
            .iter()
            .map(|c| (c.claim.claim.as_str(), c.verdict))
            .collect();
        assert_eq!(
            verdicts,
            [
                ("wide", ClaimVerdict::Unverified),
                ("one letter", ClaimVerdict::Unverified),
                ("keyword", ClaimVerdict::Unverified),
                ("tight", ClaimVerdict::Verified),
            ]
        );
    }

    #[test]
    fn token_mining_resolves_chat_rs_suffix_and_lists_src_chat_rs_absent() {
        let (_dir, probe) = fixture();
        let text = "Change chat.rs so the route streams, and move `src/chat.rs` logic.\n\
                    Also check scripts/gate.sh, `post_chat` and `missing_symbol`; see https://x.io/a.rs.";
        let mined = mine_tokens(text);
        assert_eq!(mined.paths, ["src/chat.rs", "chat.rs", "scripts/gate.sh"]);
        assert_eq!(mined.idents, ["post_chat", "missing_symbol"]);
        let map = code_map(&probe, text);
        assert_eq!(
            map.paths,
            vec![
                ("src/chat.rs".to_string(), PathResolution::Absent),
                (
                    "chat.rs".to_string(),
                    PathResolution::Unique("app".into(), "src/web/routes/chat.rs".into())
                ),
                (
                    "scripts/gate.sh".to_string(),
                    PathResolution::Unique("app".into(), "scripts/gate.sh".into())
                ),
            ]
        );
        assert_eq!(map.missing_symbols, ["missing_symbol"]);
        assert!(map.outline.contains(&"src/web/".to_string()));
        let block = carried_block(
            &GroundMap {
                code: map,
                claims: vec![],
                complete: true,
            },
            4096,
        );
        assert!(
            block.contains("- `chat.rs` is `src/web/routes/chat.rs`"),
            "{block}"
        );
        let absent = block.split("Does not exist").nth(1).unwrap();
        assert!(absent.contains("`src/chat.rs`"), "{block}");
        assert!(absent.contains("`missing_symbol`"), "{block}");
    }

    #[test]
    fn slashed_prose_never_crowds_out_a_real_path_nor_is_listed_absent() {
        let (_dir, probe) = fixture();
        let prose: String = (0..30).map(|i| format!("a{i}/b{i} ")).collect();
        let text = format!(
            "It is client/server, and/or 24/7 over TCP/IP. {prose}\n\
             Then edit src/web/routes/chat.rs and look in src/web."
        );
        let mined = mine_tokens(&text);
        assert_eq!(mined.paths, ["src/web/routes/chat.rs"]);
        assert!(
            !mined.loose.contains(&"24/7".to_string()),
            "{:?}",
            mined.loose
        );
        let map = code_map(&probe, &text);
        let listed: Vec<&str> = map.paths.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(listed, ["src/web/routes/chat.rs"], "{:?}", map.paths);
        assert!(map
            .paths
            .iter()
            .all(|(_, at)| !matches!(at, PathResolution::Absent)));
    }

    #[test]
    fn carried_block_respects_budget_quarter_dropping_outline_first() {
        let checked = |symbol: &str, verdict: ClaimVerdict| CheckedClaim {
            claim: claim("src/a.rs", 1, 1, symbol, "claim text about the code"),
            verdict,
            note: "no such file".into(),
        };
        let map = GroundMap {
            code: CodeMap {
                outline: (0..60).map(|i| format!("dir{i:02}/")).collect(),
                paths: vec![("gone.rs".into(), PathResolution::Absent)],
                missing_symbols: vec![],
            },
            claims: vec![
                checked("kept_one", ClaimVerdict::Verified),
                checked("kept_two", ClaimVerdict::VerifiedMoved),
                checked("bad_one", ClaimVerdict::Disproved),
                checked("unsure", ClaimVerdict::Unverified),
            ],
            complete: true,
        };
        let budget = 2400;
        let cap = budget / GROUND_DIVISOR;
        let block = carried_block(&map, cap);
        assert!(block.len() <= cap, "{} > {cap}", block.len());
        assert!(block.starts_with("## Prior stage: grounded map (anchors verified, claims not)"));
        assert!(
            block.contains("`kept_one`") && block.contains("`kept_two`"),
            "{block}"
        );
        assert!(
            block.contains("gone.rs") && block.contains("bad_one"),
            "{block}"
        );
        assert!(block.contains("Unverified: 1 claim"), "{block}");
        assert!(
            !block.contains("unsure"),
            "an unverified claim is only counted"
        );
        assert!(
            block.contains("Code outline (first "),
            "outline dropped first: {block}"
        );
        assert!(
            !block.contains("claim text about the code — no such file"),
            "a disproved claim's text is never carried"
        );
        let tiny = carried_block(&map, 420);
        assert!(tiny.len() <= 420);
        assert!(!tiny.contains("Code outline"), "{tiny}");
        assert!(
            tiny.contains("`kept_one`"),
            "verified anchors go last: {tiny}"
        );
    }
}
