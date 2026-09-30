//! Offline experiment harness for query-driven fact retrieval, bound by the pre-registration
//! recorded in ADR-0031 (sha256 `ae894f21…`).
//!
//! Compares two retrievers over a frozen corpus of idea folders, one item per pushed idea per
//! turn:
//!
//! - `graph`: `memory::related::related_entries(A)`, the shipped ADR-0027 block (same list on
//!   every turn of A);
//! - `query`: `index::queries::turn_fact_hits(A, turn)`, the turn as the query and the other
//!   ideas' memory facts as the documents.
//!
//! It scores both against an idea-pair label set, evaluates the ship criterion (C1–C5) on the
//! owner turns of the full corpus and of a copy without the contaminated fact files, and writes a
//! markdown report with every owner turn's hits and snippets.
//!
//! ```text
//! cargo run --release --example xidea_query_bench -- \
//!     --corpus <dir> --labels <csv> --out <results.md> [--expect-contaminated 6]
//! ```
//!
//! Evaluation tooling only: it is not part of the app binary and never exposed to the model.
#![allow(
    clippy::unwrap_used,
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "offline experiment harness run by hand: it reports on stdout and a panic is an acceptable abort"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use idea_vault::index::{queries, reindex, schema};
use idea_vault::memory::related;
use idea_vault::vault::store;

const C1_MIN_GAIN: f64 = 0.10;
const C2_MAX_NOISE: f64 = 1.0;
const C3_MIN_TP: usize = 3;
const BOOTSTRAP_DRAWS: usize = 1000;
const BOOTSTRAP_FLIP_P: f64 = 0.2;
const BOOTSTRAP_PASS: f64 = 0.8;
const BOOTSTRAP_SEED: u64 = 0x1DEA_5EED_2026_0929;
const DEFAULT_EXPECT_CONTAMINATED: usize = 6;
const USAGE: &str = "usage: xidea_query_bench --corpus <dir> --labels <csv> --out <results.md> \
[--expect-contaminated 6]";

type Pair = (String, String);

fn pair(a: &str, b: &str) -> Pair {
    if a <= b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Label {
    Related,
    Unrelated,
    Uncertain,
}

type Labels = BTreeMap<Pair, Label>;

fn read_labels(path: &Path) -> Result<Labels> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut lines = text.lines();
    let header: Vec<&str> = lines.next().unwrap_or("").split(',').collect();
    if header.first() != Some(&"a")
        || header.get(1) != Some(&"b")
        || header.get(2) != Some(&"label")
    {
        bail!("labels header must start with a,b,label");
    }
    let mut labels = Labels::new();
    for line in lines.filter(|l| !l.trim().is_empty()) {
        let cells: Vec<&str> = line.split(',').collect();
        let label = match cells.get(2).copied() {
            Some("related") => Label::Related,
            Some("unrelated") => Label::Unrelated,
            Some("uncertain") => Label::Uncertain,
            other => bail!("bad label {other:?} in {line}"),
        };
        labels.insert(pair(cells[0], cells[1]), label);
    }
    Ok(labels)
}

struct Args {
    corpus: PathBuf,
    labels: PathBuf,
    out: PathBuf,
    expect_contaminated: usize,
}

fn parse_args() -> Result<Args> {
    let mut corpus = None;
    let mut labels = None;
    let mut out = None;
    let mut expect_contaminated = DEFAULT_EXPECT_CONTAMINATED;
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .ok_or_else(|| anyhow!("{flag} needs a value\n{USAGE}"))
        };
        match flag.as_str() {
            "--corpus" => corpus = Some(PathBuf::from(value()?)),
            "--labels" => labels = Some(PathBuf::from(value()?)),
            "--out" => out = Some(PathBuf::from(value()?)),
            "--expect-contaminated" => expect_contaminated = value()?.parse()?,
            _ => bail!("unknown flag {flag}\n{USAGE}"),
        }
    }
    Ok(Args {
        corpus: corpus.ok_or_else(|| anyhow!(USAGE))?,
        labels: labels.ok_or_else(|| anyhow!(USAGE))?,
        out: out.ok_or_else(|| anyhow!(USAGE))?,
        expect_contaminated,
    })
}

/// One turn run as a query: its idea, whether the owner wrote it, its text, and both retrievers'
/// pushed ideas (plus the query hits for the report).
struct TurnRun {
    idea: String,
    owner: bool,
    text: String,
    graph: Vec<String>,
    hits: Vec<queries::TurnFactHit>,
}

fn run_corpus(dir: &Path) -> Result<Vec<TurnRun>> {
    let mut conn = rusqlite::Connection::open_in_memory()?;
    schema::apply_schema(&conn)?;
    reindex::reindex(&mut conn, dir)?;
    let mut slugs: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.path().join("idea.md").is_file() {
            slugs.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    slugs.sort();
    let mut runs = Vec::new();
    for slug in &slugs {
        let graph: Vec<String> = related::related_entries(&conn, slug)?
            .into_iter()
            .map(|e| e.slug)
            .collect();
        let conversation = store::read_conversation(dir, slug).unwrap_or_default();
        for turn in store::split_turns(&conversation) {
            let role = store::turn_role(&turn).to_string();
            if role.is_empty() {
                continue;
            }
            let text: String = turn.lines().skip(1).collect::<Vec<_>>().join("\n");
            let hits = queries::turn_fact_hits(&conn, slug, &text, queries::TURN_FACT_MAX_HITS)?;
            runs.push(TurnRun {
                idea: slug.clone(),
                owner: role == "user",
                text,
                graph: graph.clone(),
                hits,
            });
        }
    }
    Ok(runs)
}

#[derive(Debug, Default, Clone, Copy)]
struct Score {
    turns: usize,
    tp: usize,
    fp: usize,
    uncertain: usize,
    novel_tp: usize,
}

impl Score {
    fn precision(&self) -> f64 {
        if self.tp + self.fp == 0 {
            0.0
        } else {
            self.tp as f64 / (self.tp + self.fp) as f64
        }
    }
    fn noise(&self) -> f64 {
        if self.turns == 0 {
            0.0
        } else {
            self.fp as f64 / self.turns as f64
        }
    }
}

fn score(runs: &[&TurnRun], labels: &Labels, query: bool) -> Result<Score> {
    let mut s = Score {
        turns: runs.len(),
        ..Score::default()
    };
    for run in runs {
        let items: Vec<&str> = if query {
            run.hits.iter().map(|h| h.idea_slug.as_str()).collect()
        } else {
            run.graph.iter().map(String::as_str).collect()
        };
        for other in items {
            let label = labels
                .get(&pair(&run.idea, other))
                .ok_or_else(|| anyhow!("no label for {} × {other}", run.idea))?;
            match label {
                Label::Related => {
                    s.tp += 1;
                    if query && !run.graph.iter().any(|g| g == other) {
                        s.novel_tp += 1;
                    }
                }
                Label::Unrelated => s.fp += 1,
                Label::Uncertain => s.uncertain += 1,
            }
        }
    }
    Ok(s)
}

struct Verdict {
    c1: bool,
    c2: bool,
    c3: bool,
}

fn verdict(graph: &Score, query: &Score) -> Verdict {
    Verdict {
        c1: query.precision() >= graph.precision() + C1_MIN_GAIN,
        c2: query.noise() <= C2_MAX_NOISE,
        c3: query.tp >= C3_MIN_TP,
    }
}

// Deterministic PRNG for the label bootstrap; matches the xidea_bench choice.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

// Flip each scored label with probability p; uncertain labels never flip and draw nothing.
fn bootstrap(runs: &[&TurnRun], labels: &Labels) -> Result<usize> {
    let mut rng = SplitMix64(BOOTSTRAP_SEED);
    let mut passes = 0;
    for _ in 0..BOOTSTRAP_DRAWS {
        let mut flipped = labels.clone();
        for label in flipped.values_mut() {
            *label = match *label {
                Label::Uncertain => Label::Uncertain,
                l if rng.next_f64() >= BOOTSTRAP_FLIP_P => l,
                Label::Related => Label::Unrelated,
                Label::Unrelated => Label::Related,
            };
        }
        let v = verdict(
            &score(runs, &flipped, false)?,
            &score(runs, &flipped, true)?,
        );
        if v.c1 && v.c2 {
            passes += 1;
        }
    }
    Ok(passes)
}

// A hand-written `(?i)cheapest[ -]disproof`, as in xidea_bench: U+017F folds to `s`.
fn matches_contamination(text: &str) -> bool {
    const PATTERN: [char; 17] = [
        'c', 'h', 'e', 'a', 'p', 'e', 's', 't', '-', 'd', 'i', 's', 'p', 'r', 'o', 'o', 'f',
    ];
    let fold = |c: char| match c {
        '\u{17F}' => 's',
        c => c.to_ascii_lowercase(),
    };
    let chars: Vec<char> = text.chars().collect();
    chars.windows(PATTERN.len()).any(|w| {
        w.iter().zip(PATTERN).all(|(&c, p)| match p {
            '-' => c == ' ' || c == '-',
            p => fold(c) == p,
        })
    })
}

fn contaminated(corpus: &Path) -> Result<BTreeSet<PathBuf>> {
    let mut set = BTreeSet::new();
    for entry in walkdir::WalkDir::new(corpus).min_depth(3).max_depth(3) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(corpus)?.to_path_buf();
        let in_memory = rel
            .components()
            .nth(1)
            .is_some_and(|c| c.as_os_str() == "memory");
        if in_memory
            && entry.file_type().is_file()
            && rel.extension().is_some_and(|e| e == "md")
            && matches_contamination(&std::fs::read_to_string(entry.path())?)
        {
            set.insert(rel);
        }
    }
    Ok(set)
}

fn copy_corpus_without(src: &Path, dst: &Path, exclude: &BTreeSet<PathBuf>) -> Result<()> {
    let mut removed = 0;
    for entry in walkdir::WalkDir::new(src).min_depth(1) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src)?.to_path_buf();
        let target = dst.join(&rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if exclude.contains(&rel) {
            removed += 1;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    if removed != exclude.len() {
        bail!(
            "expected to remove {} files, removed {removed}",
            exclude.len()
        );
    }
    Ok(())
}

fn row(name: &str, set: &str, retriever: &str, s: &Score) -> String {
    format!(
        "| {name} | {set} | {retriever} | {} | {} | {} | {} | {} | {:.3} | {:.3} |\n",
        s.turns,
        s.tp,
        s.fp,
        s.uncertain,
        s.novel_tp,
        s.precision(),
        s.noise()
    )
}

fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &flat[..cut]),
        None => flat,
    }
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let labels = read_labels(&args.labels)?;
    let dirty = contaminated(&args.corpus)?;
    if dirty.len() != args.expect_contaminated {
        bail!(
            "contamination set has {} files, expected {}: {dirty:?}",
            dirty.len(),
            args.expect_contaminated
        );
    }
    let clean_dir = tempfile::tempdir()?;
    copy_corpus_without(&args.corpus, clean_dir.path(), &dirty)?;

    let full = run_corpus(&args.corpus)?;
    let clean = run_corpus(clean_dir.path())?;

    let mut report = String::from("# Query-driven fact retrieval: results\n\n");
    let mut table = String::from(
        "| corpus | queries | retriever | turns | TP | FP | uncertain | novel TP | P | FP/turn |\n|---|---|---|---|---|---|---|---|---|---|\n",
    );
    let mut verdicts = Vec::new();
    for (name, runs) in [("full", &full), ("minus contamination", &clean)] {
        for (set, owner_only) in [("owner", true), ("all", false)] {
            let chosen: Vec<&TurnRun> = runs.iter().filter(|r| r.owner || !owner_only).collect();
            let g = score(&chosen, &labels, false)?;
            let q = score(&chosen, &labels, true)?;
            table.push_str(&row(name, set, "graph", &g));
            table.push_str(&row(name, set, "query", &q));
            if owner_only {
                verdicts.push((name, verdict(&g, &q), g, q));
            }
        }
    }
    let owner_full: Vec<&TurnRun> = full.iter().filter(|r| r.owner).collect();
    let passes = bootstrap(&owner_full, &labels)?;
    let c5 = passes as f64 / BOOTSTRAP_DRAWS as f64 >= BOOTSTRAP_PASS;
    let (_, v_full, _, _) = &verdicts[0];
    let (_, v_clean, _, _) = &verdicts[1];
    let c4 = v_clean.c1 && v_clean.c2 && v_clean.c3;
    let pass = v_full.c1 && v_full.c2 && v_full.c3 && c4 && c5;
    let yn = |b: bool| if b { "yes" } else { "no" };
    report.push_str(&format!(
        "VERDICT: {} | C1 precision gain ≥ {C1_MIN_GAIN}: {} | C2 FP/turn ≤ {C2_MAX_NOISE}: {} | C3 TP ≥ {C3_MIN_TP}: {} | C4 minus contamination: {} | C5 bootstrap {passes}/{BOOTSTRAP_DRAWS}: {}\n\n",
        if pass { "PASS" } else { "KILLED" },
        yn(v_full.c1),
        yn(v_full.c2),
        yn(v_full.c3),
        yn(c4),
        yn(c5)
    ));
    report.push_str(&format!(
        "Contamination set ({} files): {}\n\n",
        dirty.len(),
        dirty
            .iter()
            .map(|p| format!("`{}`", p.display()))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    report.push_str(&table);
    report.push_str("\n## Owner turns, full corpus\n\n");
    for run in &owner_full {
        report.push_str(&format!(
            "- **{}**: \"{}\"\n  graph: {}\n",
            run.idea,
            one_line(&run.text, 140),
            if run.graph.is_empty() {
                "none".to_string()
            } else {
                run.graph.join(", ")
            }
        ));
        if run.hits.is_empty() {
            report.push_str("  query: none\n");
        }
        for hit in &run.hits {
            let label = labels
                .get(&pair(&run.idea, &hit.idea_slug))
                .copied()
                .unwrap_or(Label::Uncertain);
            report.push_str(&format!(
                "  query: {} — {} [{:?}; shared: {}] \"{}\"\n",
                hit.idea_slug,
                hit.fact_title,
                label,
                hit.shared.join(" "),
                hit.snippet
            ));
        }
    }
    std::fs::write(&args.out, &report)
        .with_context(|| format!("writing {}", args.out.display()))?;
    print!("{}", report.lines().next().unwrap_or(""));
    println!();
    Ok(())
}
