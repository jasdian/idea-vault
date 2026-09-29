//! Offline embeddings experiment harness for cross-idea retrieval, bound by
//! `vault/.eval/xidea/PREREGISTRATION.md` and its amendments.
//!
//! Compares three idea-level retrievers over a frozen corpus of idea folders
//! (`<slug>/idea.md`, `<slug>/conversation.md`, `<slug>/memory/*.md`):
//!
//! - `lexical`: `index::queries::lexical_baseline` over an in-memory reindex of the corpus;
//! - `embed`: one embedding per fact and per idea body, corpus centroid subtracted, idea-to-idea
//!   score = max cosine over cross-idea unit pairs;
//! - `fused`: reciprocal rank fusion (k = 60) of the full lexical and embed rankings.
//!
//! It scores each retriever's top-3 proposals against a labelled pair set, evaluates the phase-2
//! kill criterion on the full corpus and on a copy without the contaminated fact files, and
//! writes a markdown report plus a JSON sidecar.
//!
//! ```text
//! IDEA_VAULT_OLLAMA_URL=<ollama base url> cargo run --release --example xidea_bench -- \
//!     --corpus <dir> --labels <csv> --cache <dir> --out <results.md> \
//!     [--weak <csv>] [--model embeddinggemma] [--offline] \
//!     [--expect-contaminated 6] [--expect-sensitivity 2]
//! ```
//!
//! - The labels CSV has the header `a,b,label,…` and must label every pair of corpus ideas and
//!   nothing else. Pairs flagged `weak` (1/true/yes) come from a `weak` column of the labels CSV
//!   and/or of `--weak <csv>` (`a,b,…,weak,…`); if neither file has a `weak` column the run fails.
//! - The contamination and own-text sets must have exactly the `--expect-*` sizes.
//! - Online, the model digest is read from `/api/tags`, recorded in the report and in
//!   `<cache>/model-digests.json`, and every cache key is hex
//!   `sha256(model + "\0" + digest + "\0" + text)`. `--offline` never contacts Ollama: it keys
//!   the cache with the digest recorded there, reports the digest as `offline: unknown`, and
//!   fails on any cache miss.
//!
//! Evaluation tooling only: it is not part of the app binary and never exposed to the model.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use idea_vault::domain::frontmatter;
use idea_vault::index::{queries, reindex, schema};
use idea_vault::vault::{store, walk};
use sha2::{Digest, Sha256};

const TOP_K: usize = 3;
const RRF_K: f64 = 60.0;
const EMBED_BATCH: usize = 16;
const EMBED_TIMEOUT_SECS: u64 = 300;
const BOOTSTRAP_DRAWS: usize = 1000;
const BOOTSTRAP_FLIP_P: f64 = 0.2;
const BOOTSTRAP_SEED: u64 = 0x1DEA_5EED_2026_0928;
const COND1_MIN_MARGIN: i64 = 2;
const COND2_MIN_MARGIN: i64 = 1;
const DEFAULT_MODEL: &str = "embeddinggemma";
const DEFAULT_EXPECT_CONTAMINATED: usize = 6;
const DEFAULT_EXPECT_SENSITIVITY: usize = 2;
const OLLAMA_URL_ENV: &str = "IDEA_VAULT_OLLAMA_URL";
const CONTAMINATION_REGEX: &str = "(?i)cheapest[ -]disproof";
const OFFLINE_DIGEST: &str = "offline: unknown";
const DIGESTS_FILE: &str = "model-digests.json";
const PRECISION_NOTE: &str =
    "P@3 = TP / proposed pairs in the scored set (pairs labelled related or unrelated; \
uncertain pairs are excluded from both P@3 and R@3). R@3 = TP / pairs labelled related.";

const USAGE: &str =
    "usage: xidea_bench --corpus <dir> --labels <csv> --cache <dir> --out <results.md> \
[--weak <csv>] [--model embeddinggemma] [--offline] [--expect-contaminated 6] [--expect-sensitivity 2]";

type Pair = (String, String);

fn pair(a: &str, b: &str) -> Pair {
    if a <= b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

#[derive(Debug, Clone)]
struct Args {
    corpus: PathBuf,
    labels: PathBuf,
    weak: Option<PathBuf>,
    cache: PathBuf,
    out: PathBuf,
    model: String,
    offline: bool,
    expect_contaminated: usize,
    expect_sensitivity: usize,
}

fn parse_args(argv: impl IntoIterator<Item = String>) -> Result<Option<Args>> {
    let mut corpus = None;
    let mut labels = None;
    let mut weak = None;
    let mut cache = None;
    let mut out = None;
    let mut model = DEFAULT_MODEL.to_string();
    let mut offline = false;
    let mut expect_contaminated = DEFAULT_EXPECT_CONTAMINATED;
    let mut expect_sensitivity = DEFAULT_EXPECT_SENSITIVITY;
    let mut it = argv.into_iter();
    while let Some(flag) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| anyhow!("{name} needs a value"));
        let count = |name: &str, v: String| {
            v.parse::<usize>()
                .with_context(|| format!("{name} takes a count, got {v:?}"))
        };
        match flag.as_str() {
            "--corpus" => corpus = Some(PathBuf::from(value("--corpus")?)),
            "--labels" => labels = Some(PathBuf::from(value("--labels")?)),
            "--weak" => weak = Some(PathBuf::from(value("--weak")?)),
            "--cache" => cache = Some(PathBuf::from(value("--cache")?)),
            "--out" => out = Some(PathBuf::from(value("--out")?)),
            "--model" => model = value("--model")?,
            "--offline" => offline = true,
            "--expect-contaminated" => {
                expect_contaminated =
                    count("--expect-contaminated", value("--expect-contaminated")?)?
            }
            "--expect-sensitivity" => {
                expect_sensitivity = count("--expect-sensitivity", value("--expect-sensitivity")?)?
            }
            "-h" | "--help" => return Ok(None),
            other => bail!("unknown argument {other:?}\n{USAGE}"),
        }
    }
    let need = |v: Option<PathBuf>, name: &str| v.ok_or_else(|| anyhow!("missing {name}\n{USAGE}"));
    if model.trim().is_empty() {
        bail!("--model must not be empty");
    }
    Ok(Some(Args {
        corpus: need(corpus, "--corpus")?,
        labels: need(labels, "--labels")?,
        weak,
        cache: need(cache, "--cache")?,
        out: need(out, "--out")?,
        model,
        offline,
        expect_contaminated,
        expect_sensitivity,
    }))
}

fn main() -> Result<()> {
    let Some(args) = parse_args(std::env::args().skip(1))? else {
        println!("{USAGE}");
        return Ok(());
    };
    let embedder = if args.offline {
        None
    } else {
        Some(OllamaEmbedder::from_env()?)
    };
    let report = run(&args, embedder.as_ref().map(|e| e as &dyn Embedder))?;
    let json_path = args.out.with_extension("json");
    std::fs::write(&args.out, render_markdown(&report))
        .with_context(|| format!("writing {}", args.out.display()))?;
    let json = serde_json::to_string_pretty(&render_json(&report))?;
    std::fs::write(&json_path, json + "\n")
        .with_context(|| format!("writing {}", json_path.display()))?;
    println!("{}", verdict_line(&report));
    println!("wrote {} and {}", args.out.display(), json_path.display());
    Ok(())
}

#[derive(Debug, Clone)]
struct CorpusIdea {
    slug: String,
    body: String,
    facts: Vec<CorpusFact>,
}

#[derive(Debug, Clone)]
struct CorpusFact {
    slug: String,
    text: String,
}

// Byte-identical to the `memory` row reindex writes into `search_fts`, `[[…]]` slugs included.
fn fact_text(title: &str, body: &str) -> String {
    format!("{title}\n\n{body}")
}

fn load_corpus(dir: &Path) -> Result<Vec<CorpusIdea>> {
    let mut ideas = Vec::new();
    for entry in walk::walk_ideas(dir)? {
        let idea = store::read_idea(dir, &entry.slug)
            .with_context(|| format!("reading {}/idea.md", entry.slug))?;
        let facts = store::read_memory_facts(dir, &entry.slug)
            .with_context(|| format!("reading {}/memory", entry.slug))?;
        ideas.push(CorpusIdea {
            slug: entry.slug,
            body: idea.body,
            facts: facts
                .into_iter()
                .map(|f| CorpusFact {
                    text: fact_text(&f.frontmatter.title, &f.body),
                    slug: f.frontmatter.slug,
                })
                .collect(),
        });
    }
    if ideas.is_empty() {
        bail!("no ideas under {}", dir.display());
    }
    Ok(ideas)
}

fn corpus_slugs(dir: &Path) -> Result<Vec<String>> {
    Ok(walk::walk_ideas(dir)?.into_iter().map(|e| e.slug).collect())
}

#[derive(Debug, Clone)]
struct Unit {
    idea: usize,
    fact: Option<String>,
    text: String,
}

fn corpus_units(corpus: &[CorpusIdea]) -> Vec<Unit> {
    let mut units = Vec::new();
    for (i, idea) in corpus.iter().enumerate() {
        if !idea.body.trim().is_empty() {
            units.push(Unit {
                idea: i,
                fact: None,
                text: idea.body.clone(),
            });
        }
        for fact in &idea.facts {
            units.push(Unit {
                idea: i,
                fact: Some(fact.slug.clone()),
                text: fact.text.clone(),
            });
        }
    }
    units
}

fn check_units(dir: &Path, corpus: &[CorpusIdea], units: &[Unit]) -> Result<()> {
    let empty: Vec<&str> = corpus
        .iter()
        .enumerate()
        .filter(|(i, _)| !units.iter().any(|u| u.idea == *i))
        .map(|(_, idea)| idea.slug.as_str())
        .collect();
    if !empty.is_empty() {
        bail!(
            "ideas with no embedding units (blank body and no facts) in {}: {}",
            dir.display(),
            empty.join(", ")
        );
    }
    Ok(())
}

fn copy_corpus_without(src: &Path, dst: &Path, exclude: &BTreeSet<PathBuf>) -> Result<usize> {
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
            std::fs::copy(entry.path(), &target)
                .with_context(|| format!("copying {}", entry.path().display()))?;
        }
    }
    if removed != exclude.len() {
        bail!(
            "expected to remove {} files from the corpus copy, removed {removed}",
            exclude.len()
        );
    }
    Ok(removed)
}

// A hand-written `(?i)cheapest[ -]disproof`. Simple Unicode case folding adds exactly one
// non-ASCII match for these letters: U+017F LATIN SMALL LETTER LONG S folds to `s`.
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

// An unclosed `[[` is kept, matching the link grammar of `domain::links`.
fn strip_link_tokens(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find("[[") {
        let Some(close) = rest[open + 2..].find("]]") else {
            break;
        };
        out.push_str(&rest[..open]);
        rest = &rest[open + 2 + close + 2..];
    }
    out.push_str(rest);
    out
}

// `(full, own_text)`: the whole file matches; the title or body matches once `[[…]]` tokens are
// removed (frontmatter slug, tags and links never count).
fn classify_fact_file(raw: &str) -> Result<(bool, bool)> {
    let (fm, body) = frontmatter::parse_memory_fact(raw)?;
    let own = strip_link_tokens(&fact_text(&fm.title, &body));
    Ok((matches_contamination(raw), matches_contamination(&own)))
}

#[derive(Debug, Clone, Default)]
struct Contamination {
    full: BTreeSet<PathBuf>,
    own_text: BTreeSet<PathBuf>,
    full_facts: Vec<(String, String)>,
}

fn scan_contamination(dir: &Path) -> Result<Contamination> {
    let mut out = Contamination::default();
    for entry in walk::walk_ideas(dir)? {
        let mem = entry.path.join("memory");
        let read = match std::fs::read_dir(&mem) {
            Ok(read) => read,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        let mut files: Vec<PathBuf> = read
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<_>>()?;
        files.sort();
        for path in files {
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let raw = std::fs::read_to_string(&path)?;
            let (full, own) =
                classify_fact_file(&raw).with_context(|| format!("parsing {}", path.display()))?;
            let rel = path.strip_prefix(dir)?.to_path_buf();
            if full {
                let (fm, _) = frontmatter::parse_memory_fact(&raw)?;
                out.full_facts.push((entry.slug.clone(), fm.slug));
                out.full.insert(rel.clone());
            }
            if own {
                out.own_text.insert(rel);
            }
        }
    }
    Ok(out)
}

fn check_expected(c: &Contamination, contaminated: usize, sensitivity: usize) -> Result<()> {
    if c.full.len() != contaminated {
        bail!(
            "contamination set has {} files, expected {contaminated} (--expect-contaminated)",
            c.full.len()
        );
    }
    if c.own_text.len() != sensitivity {
        bail!(
            "own-text set has {} files, expected {sensitivity} (--expect-sensitivity)",
            c.own_text.len()
        );
    }
    Ok(())
}

// Zero when either vector has zero norm.
fn cosine(a: &[f64], b: &[f64]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na = a.iter().map(|x| x * x).sum::<f64>().sqrt();
    let nb = b.iter().map(|x| x * x).sum::<f64>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

fn centroid(vectors: &[Vec<f64>]) -> Vec<f64> {
    let dim = vectors.first().map_or(0, Vec::len);
    let mut c = vec![0.0; dim];
    for v in vectors {
        for (acc, x) in c.iter_mut().zip(v) {
            *acc += x;
        }
    }
    let n = vectors.len().max(1) as f64;
    c.iter_mut().for_each(|x| *x /= n);
    c
}

fn subtract_centroid(vectors: &[Vec<f64>]) -> Vec<Vec<f64>> {
    let c = centroid(vectors);
    vectors
        .iter()
        .map(|v| v.iter().zip(&c).map(|(x, m)| x - m).collect())
        .collect()
}

fn mean_pairwise_cosine(vectors: &[&[f64]]) -> Option<f64> {
    let mut sum = 0.0;
    let mut n = 0usize;
    for (i, a) in vectors.iter().enumerate() {
        for b in &vectors[i + 1..] {
            sum += cosine(a, b);
            n += 1;
        }
    }
    (n > 0).then(|| sum / n as f64)
}

// `units` are `(idea index, centred vector)`. Same-idea unit pairs never score.
fn embed_rankings(
    slugs: &[String],
    units: &[(usize, Vec<f64>)],
) -> BTreeMap<String, Vec<(String, f64)>> {
    let mut best: BTreeMap<(usize, usize), f64> = BTreeMap::new();
    for (i, (ia, va)) in units.iter().enumerate() {
        for (ib, vb) in &units[i + 1..] {
            if ia == ib {
                continue;
            }
            let c = cosine(va, vb);
            for key in [(*ia, *ib), (*ib, *ia)] {
                let e = best.entry(key).or_insert(f64::NEG_INFINITY);
                *e = e.max(c);
            }
        }
    }
    slugs
        .iter()
        .enumerate()
        .map(|(a, slug)| {
            let mut ranked: Vec<(String, f64)> = best
                .range((a, 0)..(a + 1, 0))
                .map(|(&(_, b), &score)| (slugs[b].clone(), score))
                .collect();
            sort_scored(&mut ranked);
            (slug.clone(), ranked)
        })
        .collect()
}

// 1-based ranks; an item absent from a ranking gets nothing from it.
fn rrf(rankings: &[&[String]], k: f64) -> Vec<(String, f64)> {
    let mut scores: BTreeMap<String, f64> = BTreeMap::new();
    for ranking in rankings {
        for (i, slug) in ranking.iter().enumerate() {
            *scores.entry(slug.clone()).or_insert(0.0) += 1.0 / (k + (i + 1) as f64);
        }
    }
    let mut out: Vec<(String, f64)> = scores.into_iter().collect();
    sort_scored(&mut out);
    out
}

fn fused_top(slug: &str, lexical_full: &[String], embed_full: &[String]) -> Vec<String> {
    rrf(&[lexical_full, embed_full], RRF_K)
        .into_iter()
        .filter(|(s, _)| s != slug)
        .take(TOP_K)
        .map(|(s, _)| s)
        .collect()
}

fn sort_scored(v: &mut [(String, f64)]) {
    v.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
}

trait Embedder {
    fn digest(&self, model: &str) -> Result<String>;
    fn embed(&self, model: &str, texts: &[String]) -> Result<Vec<Vec<f64>>>;
}

struct OllamaEmbedder {
    base: String,
    client: reqwest::Client,
    rt: tokio::runtime::Runtime,
}

impl OllamaEmbedder {
    fn from_env() -> Result<Self> {
        let base = std::env::var(OLLAMA_URL_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| anyhow!("{OLLAMA_URL_ENV} is unset; set it or pass --offline"))?;
        Self::new(&base)
    }

    fn new(base: &str) -> Result<Self> {
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(EMBED_TIMEOUT_SECS))
                .build()?,
            rt: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?,
        })
    }

    fn send<T: serde::de::DeserializeOwned>(&self, req: reqwest::RequestBuilder) -> Result<T> {
        self.rt.block_on(async {
            let resp = req.send().await?;
            let status = resp.status();
            let url = resp.url().to_string();
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                bail!("{url} returned {status}: {text}");
            }
            Ok::<_, anyhow::Error>(resp.json::<T>().await?)
        })
    }
}

#[derive(serde::Deserialize)]
struct EmbedResponse {
    embeddings: Vec<Vec<f64>>,
}

#[derive(serde::Deserialize)]
struct TagsResponse {
    models: Vec<TagModel>,
}

#[derive(serde::Deserialize)]
struct TagModel {
    name: String,
    #[serde(default)]
    model: String,
    digest: String,
}

// An untagged model name means `:latest`, as in the Ollama CLI.
fn find_digest(tags: &TagsResponse, model: &str) -> Result<String> {
    let latest = format!("{model}:latest");
    tags.models
        .iter()
        .find(|m| {
            [m.name.as_str(), m.model.as_str()]
                .iter()
                .any(|n| *n == model || (!model.contains(':') && *n == latest))
        })
        .map(|m| m.digest.clone())
        .ok_or_else(|| anyhow!("model {model} is not in /api/tags; pull it first"))
}

fn embed_request_body(model: &str, texts: &[String]) -> serde_json::Value {
    serde_json::json!({ "model": model, "input": texts, "truncate": false })
}

impl Embedder for OllamaEmbedder {
    fn digest(&self, model: &str) -> Result<String> {
        let tags: TagsResponse = self.send(self.client.get(format!("{}/api/tags", self.base)))?;
        find_digest(&tags, model)
    }

    fn embed(&self, model: &str, texts: &[String]) -> Result<Vec<Vec<f64>>> {
        let url = format!("{}/api/embed", self.base);
        let body = embed_request_body(model, texts);
        let resp: EmbedResponse = self.send(self.client.post(url).json(&body))?;
        Ok(resp.embeddings)
    }
}

fn cache_key(model: &str, digest: &str, text: &str) -> String {
    let mut h = Sha256::new();
    h.update(model.as_bytes());
    h.update([0u8]);
    h.update(digest.as_bytes());
    h.update([0u8]);
    h.update(text.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

struct Cache {
    dir: PathBuf,
}

impl Cache {
    fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating cache {}", dir.display()))?;
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }

    fn path(&self, model: &str, digest: &str, text: &str) -> PathBuf {
        self.dir
            .join(format!("{}.json", cache_key(model, digest, text)))
    }

    fn get(&self, model: &str, digest: &str, text: &str) -> Result<Option<Vec<f64>>> {
        let path = self.path(model, digest, text);
        match std::fs::read_to_string(&path) {
            Ok(raw) => Ok(Some(
                serde_json::from_str(&raw)
                    .with_context(|| format!("corrupt {}", path.display()))?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn put(&self, model: &str, digest: &str, text: &str, vector: &[f64]) -> Result<()> {
        write_atomic(
            &self.path(model, digest, text),
            &serde_json::to_string(vector)?,
        )
    }

    fn digests(&self) -> Result<BTreeMap<String, String>> {
        let path = self.dir.join(DIGESTS_FILE);
        match std::fs::read_to_string(&path) {
            Ok(raw) => {
                serde_json::from_str(&raw).with_context(|| format!("corrupt {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(e.into()),
        }
    }

    fn record_digest(&self, model: &str, digest: &str) -> Result<()> {
        let mut all = self.digests()?;
        all.insert(model.to_string(), digest.to_string());
        write_atomic(
            &self.dir.join(DIGESTS_FILE),
            &serde_json::to_string_pretty(&all)?,
        )
    }
}

fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelDigest {
    key: String,
    reported: String,
}

fn resolve_digest(
    model: &str,
    cache: &Cache,
    embedder: Option<&dyn Embedder>,
) -> Result<ModelDigest> {
    match embedder {
        Some(embedder) => {
            let digest = embedder.digest(model)?;
            cache.record_digest(model, &digest)?;
            Ok(ModelDigest {
                key: digest.clone(),
                reported: digest,
            })
        }
        None => {
            let key = cache.digests()?.remove(model).ok_or_else(|| {
                anyhow!("offline: the cache records no digest for model {model}; run once online")
            })?;
            Ok(ModelDigest {
                key,
                reported: OFFLINE_DIGEST.to_string(),
            })
        }
    }
}

// Misses are embedded in sequential batches of `EMBED_BATCH`; `embedder == None` is offline
// mode, where any miss is an error.
fn embed_texts(
    texts: &[String],
    model: &str,
    digest: &str,
    cache: &Cache,
    embedder: Option<&dyn Embedder>,
) -> Result<Vec<Vec<f64>>> {
    let mut misses: Vec<String> = Vec::new();
    let mut seen = BTreeSet::new();
    for text in texts {
        if cache.get(model, digest, text)?.is_none() && seen.insert(text.clone()) {
            misses.push(text.clone());
        }
    }
    if !misses.is_empty() {
        let Some(embedder) = embedder else {
            bail!(
                "offline: {} of {} texts are not cached (first key {})",
                misses.len(),
                texts.len(),
                cache_key(model, digest, &misses[0])
            );
        };
        for batch in misses.chunks(EMBED_BATCH) {
            let vectors = embedder.embed(model, batch)?;
            if vectors.len() != batch.len() {
                bail!(
                    "embedder returned {} vectors for {} texts",
                    vectors.len(),
                    batch.len()
                );
            }
            for (text, vector) in batch.iter().zip(&vectors) {
                cache.put(model, digest, text, vector)?;
            }
        }
    }
    let mut out = Vec::with_capacity(texts.len());
    for text in texts {
        out.push(
            cache
                .get(model, digest, text)?
                .ok_or_else(|| anyhow!("cache lost {}", cache_key(model, digest, text)))?,
        );
    }
    let dim = out.first().map_or(0, Vec::len);
    if out.iter().any(|v| v.len() != dim || v.is_empty()) {
        bail!("embedding dimensions are empty or inconsistent");
    }
    Ok(out)
}

#[derive(Debug, Clone, Default)]
struct Proposals {
    lexical: BTreeSet<Pair>,
    embed: BTreeSet<Pair>,
    fused: BTreeSet<Pair>,
}

type Tops = BTreeMap<String, Vec<String>>;

#[derive(Debug, Clone)]
struct VariantRun {
    ideas: usize,
    facts: usize,
    units: Vec<Unit>,
    slugs: Vec<String>,
    raw: Vec<Vec<f64>>,
    centred: Vec<Vec<f64>>,
    rankings: BTreeMap<&'static str, Tops>,
    top: BTreeMap<&'static str, Tops>,
    proposals: Proposals,
}

fn proposed_pairs(top: &Tops) -> BTreeSet<Pair> {
    top.iter()
        .flat_map(|(a, bs)| bs.iter().map(move |b| pair(a, b)))
        .collect()
}

fn short_proposals(top: &Tops) -> usize {
    top.values().filter(|bs| bs.len() < TOP_K).count()
}

fn run_variant(
    dir: &Path,
    model: &str,
    digest: &str,
    cache: &Cache,
    embedder: Option<&dyn Embedder>,
) -> Result<VariantRun> {
    let corpus = load_corpus(dir)?;
    let slugs: Vec<String> = corpus.iter().map(|i| i.slug.clone()).collect();
    let units = corpus_units(&corpus);
    check_units(dir, &corpus, &units)?;

    let mut conn = rusqlite::Connection::open_in_memory()?;
    schema::apply_schema(&conn)?;
    reindex::reindex(&mut conn, dir)?;
    let mut lexical_top = Tops::new();
    let mut lexical_full = Tops::new();
    for slug in &slugs {
        let top = queries::lexical_baseline(&conn, slug, TOP_K)?;
        lexical_top.insert(slug.clone(), top.into_iter().map(|h| h.idea_slug).collect());
        let full = queries::lexical_baseline(&conn, slug, slugs.len())?;
        lexical_full.insert(
            slug.clone(),
            full.into_iter().map(|h| h.idea_slug).collect(),
        );
    }

    let texts: Vec<String> = units.iter().map(|u| u.text.clone()).collect();
    let raw = embed_texts(&texts, model, digest, cache, embedder)?;
    let centred = subtract_centroid(&raw);
    let indexed: Vec<(usize, Vec<f64>)> = units
        .iter()
        .zip(&centred)
        .map(|(u, v)| (u.idea, v.clone()))
        .collect();
    let embed_full: Tops = embed_rankings(&slugs, &indexed)
        .into_iter()
        .map(|(s, r)| (s, r.into_iter().map(|(b, _)| b).collect()))
        .collect();

    let mut embed_top = Tops::new();
    let mut fused = Tops::new();
    for slug in &slugs {
        embed_top.insert(
            slug.clone(),
            embed_full[slug].iter().take(TOP_K).cloned().collect(),
        );
        fused.insert(
            slug.clone(),
            fused_top(slug, &lexical_full[slug], &embed_full[slug]),
        );
    }

    let proposals = Proposals {
        lexical: proposed_pairs(&lexical_top),
        embed: proposed_pairs(&embed_top),
        fused: proposed_pairs(&fused),
    };
    let rankings = BTreeMap::from([("lexical", lexical_full), ("embed", embed_full)]);
    let top = BTreeMap::from([
        ("lexical", lexical_top),
        ("embed", embed_top),
        ("fused", fused),
    ]);
    Ok(VariantRun {
        ideas: corpus.len(),
        facts: corpus.iter().map(|i| i.facts.len()).sum(),
        units,
        slugs,
        raw,
        centred,
        rankings,
        top,
        proposals,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Label {
    Related,
    Unrelated,
    Uncertain,
}

impl Label {
    fn as_str(self) -> &'static str {
        match self {
            Label::Related => "related",
            Label::Unrelated => "unrelated",
            Label::Uncertain => "uncertain",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        match s.trim() {
            "related" => Ok(Label::Related),
            "unrelated" => Ok(Label::Unrelated),
            "uncertain" => Ok(Label::Uncertain),
            other => bail!("unknown label {other:?}"),
        }
    }
}

type Labels = BTreeMap<Pair, Label>;

fn parse_csv(text: &str) -> Result<Vec<Vec<String>>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => quoted = false,
                _ => field.push(c),
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => quoted = true,
            ',' => row.push(std::mem::take(&mut field)),
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            _ => field.push(c),
        }
    }
    if quoted {
        bail!("unterminated quoted CSV field");
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows.retain(|r| !(r.len() == 1 && r[0].trim().is_empty()));
    Ok(rows)
}

fn is_truthy(v: Option<&String>) -> bool {
    matches!(
        v.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes")
    )
}

// `None` means the CSV has no `weak` column, which is not the same as no weak pairs.
fn parse_labels(text: &str) -> Result<(Labels, Option<BTreeSet<Pair>>)> {
    let rows = parse_csv(text)?;
    let header = rows.first().ok_or_else(|| anyhow!("empty labels CSV"))?;
    if header.len() < 3 || header[0] != "a" || header[1] != "b" || header[2] != "label" {
        bail!("labels CSV header must start with a,b,label");
    }
    let weak_col = header.iter().position(|h| h == "weak");
    let mut labels = Labels::new();
    let mut weak = BTreeSet::new();
    for (n, row) in rows.iter().enumerate().skip(1) {
        if row.len() < 3 {
            bail!("labels CSV row {} has {} fields", n + 1, row.len());
        }
        let (a, b) = (row[0].trim(), row[1].trim());
        if a == b {
            bail!("labels CSV row {} pairs {a} with itself", n + 1);
        }
        let key = pair(a, b);
        let label = Label::parse(&row[2]).with_context(|| format!("labels CSV row {}", n + 1))?;
        if labels.insert(key.clone(), label).is_some() {
            bail!("labels CSV lists {a},{b} twice");
        }
        if weak_col.is_some_and(|col| is_truthy(row.get(col))) {
            weak.insert(key);
        }
    }
    Ok((labels, weak_col.map(|_| weak)))
}

fn parse_weak(text: &str) -> Result<BTreeSet<Pair>> {
    let rows = parse_csv(text)?;
    let header = rows.first().ok_or_else(|| anyhow!("empty weak CSV"))?;
    if header.len() < 2 || header[0] != "a" || header[1] != "b" {
        bail!("weak CSV header must start with a,b");
    }
    let col = header
        .iter()
        .position(|h| h == "weak")
        .ok_or_else(|| anyhow!("weak CSV has no `weak` column"))?;
    Ok(rows
        .iter()
        .skip(1)
        .filter(|row| row.len() >= 2 && is_truthy(row.get(col)))
        .map(|row| pair(row[0].trim(), row[1].trim()))
        .collect())
}

fn resolve_weak(
    labels: &Labels,
    from_labels: Option<BTreeSet<Pair>>,
    from_flag: Option<BTreeSet<Pair>>,
) -> Result<BTreeSet<Pair>> {
    if from_labels.is_none() && from_flag.is_none() {
        bail!("no `weak` column in the labels CSV and no --weak <csv>; weak pairs cannot be assumed absent");
    }
    let weak: BTreeSet<Pair> = from_labels.into_iter().chain(from_flag).flatten().collect();
    if let Some((a, b)) = weak.iter().find(|p| !labels.contains_key(*p)) {
        bail!("weak pair {a},{b} has no label");
    }
    Ok(weak)
}

fn check_label_coverage(labels: &Labels, slugs: &[String]) -> Result<()> {
    let known: BTreeSet<&String> = slugs.iter().collect();
    let outside: Vec<String> = labels
        .keys()
        .filter(|(a, b)| !known.contains(a) || !known.contains(b))
        .map(|(a, b)| format!("{a},{b}"))
        .collect();
    if !outside.is_empty() {
        bail!(
            "labels name slugs outside the corpus: {}",
            outside.join("; ")
        );
    }
    let missing: Vec<String> = slugs
        .iter()
        .enumerate()
        .flat_map(|(i, a)| slugs[i + 1..].iter().map(move |b| pair(a, b)))
        .filter(|p| !labels.contains_key(p))
        .map(|(a, b)| format!("{a},{b}"))
        .collect();
    if !missing.is_empty() {
        bail!("corpus pairs without a label: {}", missing.join("; "));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Metric {
    tp: usize,
    proposed: usize,
    scored: usize,
    related: usize,
}

impl Metric {
    fn precision(&self) -> Option<f64> {
        (self.scored > 0).then(|| self.tp as f64 / self.scored as f64)
    }

    fn recall(&self) -> Option<f64> {
        (self.related > 0).then(|| self.tp as f64 / self.related as f64)
    }
}

fn metric(proposed: &BTreeSet<Pair>, labels: &Labels) -> Metric {
    let mut m = Metric {
        proposed: proposed.len(),
        related: labels.values().filter(|&&l| l == Label::Related).count(),
        ..Metric::default()
    };
    for p in proposed {
        match labels.get(p) {
            Some(Label::Related) => {
                m.tp += 1;
                m.scored += 1;
            }
            Some(Label::Unrelated) => m.scored += 1,
            Some(Label::Uncertain) | None => {}
        }
    }
    m
}

fn tp(proposed: &BTreeSet<Pair>, labels: &Labels) -> usize {
    metric(proposed, labels).tp
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tps {
    lexical: usize,
    embed: usize,
    fused: usize,
}

impl Tps {
    fn of(p: &Proposals, labels: &Labels) -> Self {
        Self {
            lexical: tp(&p.lexical, labels),
            embed: tp(&p.embed, labels),
            fused: tp(&p.fused, labels),
        }
    }

    fn margin(self) -> i64 {
        self.embed.max(self.fused) as i64 - self.lexical as i64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Pass,
    Killed,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Killed => "KILLED",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KillOutcome {
    full: Tps,
    removed: Tps,
    cond1: bool,
    cond2: bool,
    verdict: Verdict,
}

fn kill(full: Tps, removed: Tps) -> KillOutcome {
    let cond1 = full.margin() >= COND1_MIN_MARGIN;
    let cond2 = removed.margin() >= COND2_MIN_MARGIN;
    KillOutcome {
        full,
        removed,
        cond1,
        cond2,
        verdict: if cond1 && cond2 {
            Verdict::Pass
        } else {
            Verdict::Killed
        },
    }
}

fn kill_for(labels: &Labels, full: &Proposals, removed: &Proposals) -> KillOutcome {
    kill(Tps::of(full, labels), Tps::of(removed, labels))
}

fn phase2_built(point: Verdict, passing: usize, draws: usize) -> bool {
    point == Verdict::Pass && passing * 5 >= draws * 4
}

#[derive(Debug, Clone)]
struct Flip {
    pair: Pair,
    from: Label,
    to: Label,
    outcome: KillOutcome,
}

// Uncertain pairs go both ways, related pairs to unrelated, weak unrelated pairs to related.
fn single_flips(
    labels: &Labels,
    weak: &BTreeSet<Pair>,
    full: &Proposals,
    removed: &Proposals,
) -> Vec<Flip> {
    let mut flips = Vec::new();
    for (p, &from) in labels {
        let targets: &[Label] = match from {
            Label::Uncertain => &[Label::Related, Label::Unrelated],
            Label::Related => &[Label::Unrelated],
            Label::Unrelated if weak.contains(p) => &[Label::Related],
            Label::Unrelated => &[],
        };
        for &to in targets {
            let mut flipped = labels.clone();
            flipped.insert(p.clone(), to);
            flips.push(Flip {
                pair: p.clone(),
                from,
                to,
                outcome: kill_for(&flipped, full, removed),
            });
        }
    }
    flips
}

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

// Rankings stay fixed; uncertain labels never flip and consume no random draw.
fn bootstrap(
    labels: &Labels,
    full: &Proposals,
    removed: &Proposals,
    draws: usize,
    p: f64,
    seed: u64,
) -> usize {
    let mut rng = SplitMix64(seed);
    let mut passing = 0;
    for _ in 0..draws {
        let mut drawn = labels.clone();
        for label in drawn.values_mut() {
            let flipped = match *label {
                Label::Related => Label::Unrelated,
                Label::Unrelated => Label::Related,
                Label::Uncertain => continue,
            };
            if rng.next_f64() < p {
                *label = flipped;
            }
        }
        if kill_for(&drawn, full, removed).verdict == Verdict::Pass {
            passing += 1;
        }
    }
    passing
}

#[derive(Debug, Clone)]
struct Safeguard {
    n: usize,
    before: Option<f64>,
    after: Option<f64>,
}

struct Report {
    args: Args,
    labels: Labels,
    weak: BTreeSet<Pair>,
    digest: ModelDigest,
    contamination: Contamination,
    full: VariantRun,
    removed: VariantRun,
    sensitivity: VariantRun,
    point: KillOutcome,
    sensitivity_margin: i64,
    flips: Vec<Flip>,
    bootstrap_pass: usize,
    safeguard: Safeguard,
}

impl Report {
    fn share(&self) -> f64 {
        self.bootstrap_pass as f64 / BOOTSTRAP_DRAWS as f64
    }

    fn built(&self) -> bool {
        phase2_built(self.point.verdict, self.bootstrap_pass, BOOTSTRAP_DRAWS)
    }
}

fn safeguard(run: &VariantRun, facts: &[(String, String)]) -> Result<Safeguard> {
    let mut idx = Vec::new();
    for (idea, fact) in facts {
        let i = run
            .units
            .iter()
            .position(|u| run.slugs[u.idea] == *idea && u.fact.as_deref() == Some(fact))
            .ok_or_else(|| anyhow!("contaminated fact {idea}/{fact} has no embedding"))?;
        idx.push(i);
    }
    let before: Vec<&[f64]> = idx.iter().map(|&i| run.raw[i].as_slice()).collect();
    let after: Vec<&[f64]> = idx.iter().map(|&i| run.centred[i].as_slice()).collect();
    Ok(Safeguard {
        n: idx.len(),
        before: mean_pairwise_cosine(&before),
        after: mean_pairwise_cosine(&after),
    })
}

fn run(args: &Args, embedder: Option<&dyn Embedder>) -> Result<Report> {
    let read =
        |p: &Path| std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()));
    let (labels, labels_weak) = parse_labels(&read(&args.labels)?)?;
    let flag_weak = match &args.weak {
        Some(p) => Some(parse_weak(&read(p)?).with_context(|| format!("parsing {}", p.display()))?),
        None => None,
    };
    let weak = resolve_weak(&labels, labels_weak, flag_weak)?;
    check_label_coverage(&labels, &corpus_slugs(&args.corpus)?)?;
    let contamination = scan_contamination(&args.corpus)?;
    check_expected(
        &contamination,
        args.expect_contaminated,
        args.expect_sensitivity,
    )?;
    let cache = Cache::open(&args.cache)?;
    let digest = resolve_digest(&args.model, &cache, embedder)?;

    let variant = |dir: &Path| run_variant(dir, &args.model, &digest.key, &cache, embedder);
    let full = variant(&args.corpus)?;
    let removed_dir = tempfile::tempdir()?;
    copy_corpus_without(&args.corpus, removed_dir.path(), &contamination.full)?;
    let removed = variant(removed_dir.path())?;
    let sensitivity_dir = tempfile::tempdir()?;
    copy_corpus_without(
        &args.corpus,
        sensitivity_dir.path(),
        &contamination.own_text,
    )?;
    let sensitivity = variant(sensitivity_dir.path())?;

    let point = kill_for(&labels, &full.proposals, &removed.proposals);
    let sensitivity_margin = Tps::of(&sensitivity.proposals, &labels).margin();
    let flips = single_flips(&labels, &weak, &full.proposals, &removed.proposals);
    let bootstrap_pass = bootstrap(
        &labels,
        &full.proposals,
        &removed.proposals,
        BOOTSTRAP_DRAWS,
        BOOTSTRAP_FLIP_P,
        BOOTSTRAP_SEED,
    );
    let safeguard = safeguard(&full, &contamination.full_facts)?;
    Ok(Report {
        args: args.clone(),
        labels,
        weak,
        digest,
        contamination,
        full,
        removed,
        sensitivity,
        point,
        sensitivity_margin,
        flips,
        bootstrap_pass,
        safeguard,
    })
}

fn fmt_ratio(v: Option<f64>) -> String {
    v.map_or_else(|| "n/a".to_string(), |v| format!("{v:.3}"))
}

fn fmt_cos(v: Option<f64>) -> String {
    v.map_or_else(|| "n/a".to_string(), |v| format!("{v:.4}"))
}

fn yes(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

fn verdict_line(r: &Report) -> String {
    format!(
        "VERDICT: {} (cond1 {}: margin {} >= {COND1_MIN_MARGIN}; cond2 {}: margin {} >= {COND2_MIN_MARGIN}) \
         | bootstrap {}/{} = {:.1}% | PHASE 2: {}",
        r.point.verdict.as_str(),
        yes(r.point.cond1),
        r.point.full.margin(),
        yes(r.point.cond2),
        r.point.removed.margin(),
        r.bootstrap_pass,
        BOOTSTRAP_DRAWS,
        r.share() * 100.0,
        if r.built() { "BUILT" } else { "KILLED" }
    )
}

const RETRIEVERS: [&str; 3] = ["lexical", "embed", "fused"];

fn retriever_pairs<'a>(p: &'a Proposals, name: &str) -> &'a BTreeSet<Pair> {
    match name {
        "lexical" => &p.lexical,
        "embed" => &p.embed,
        _ => &p.fused,
    }
}

fn variants(r: &Report) -> [(&'static str, &VariantRun); 3] {
    [
        ("full corpus", &r.full),
        ("minus contamination set (kill cond2)", &r.removed),
        ("minus own-text set (sensitivity)", &r.sensitivity),
    ]
}

fn render_markdown(r: &Report) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let count = |l: Label| r.labels.values().filter(|&&v| v == l).count();
    let _ = writeln!(s, "# xidea embeddings experiment results\n");
    let _ = writeln!(s, "{}\n", verdict_line(r));
    let _ = writeln!(s, "## Inputs\n");
    let _ = writeln!(s, "- corpus: `{}`", r.args.corpus.display());
    let _ = writeln!(s, "- labels: `{}`", r.args.labels.display());
    if let Some(weak) = &r.args.weak {
        let _ = writeln!(s, "- weak flags: `{}`", weak.display());
    }
    let _ = writeln!(s, "- model: `{}`", r.args.model);
    let _ = writeln!(
        s,
        "- model digest: `{}` (cache keys use `{}`)",
        r.digest.reported, r.digest.key
    );
    let _ = writeln!(
        s,
        "- labels: {} related, {} unrelated, {} uncertain over {} pairs; {} weak pairs: {}",
        count(Label::Related),
        count(Label::Unrelated),
        count(Label::Uncertain),
        r.labels.len(),
        r.weak.len(),
        list_pairs(&r.weak)
    );
    let _ = writeln!(
        s,
        "- top-{TOP_K} per idea; RRF k = {RRF_K}; centroid = mean of every fact and idea-body vector of the run's corpus"
    );
    let _ = writeln!(
        s,
        "- contamination regex: `{CONTAMINATION_REGEX}` over the full fact file"
    );
    let _ = writeln!(
        s,
        "- contamination set ({} files): {}",
        r.contamination.full.len(),
        list_paths(&r.contamination.full)
    );
    let _ = writeln!(
        s,
        "- own-text set, title/body without `[[…]]` ({} files): {}\n",
        r.contamination.own_text.len(),
        list_paths(&r.contamination.own_text)
    );

    let _ = writeln!(s, "## Scores\n");
    let _ = writeln!(s, "{PRECISION_NOTE}\n");
    let _ = writeln!(
        s,
        "| corpus | ideas | facts | retriever | TP | proposed | scored | related | P@3 | R@3 | ideas with < {TOP_K} proposals |"
    );
    let _ = writeln!(s, "|---|---|---|---|---|---|---|---|---|---|---|");
    for (name, v) in variants(r) {
        for ret in RETRIEVERS {
            let m = metric(retriever_pairs(&v.proposals, ret), &r.labels);
            let _ = writeln!(
                s,
                "| {name} | {} | {} | {ret} | {} | {} | {} | {} | {} | {} | {} |",
                v.ideas,
                v.facts,
                m.tp,
                m.proposed,
                m.scored,
                m.related,
                fmt_ratio(m.precision()),
                fmt_ratio(m.recall()),
                short_proposals(&v.top[ret])
            );
        }
    }
    let _ = writeln!(
        s,
        "\nMargins, max(TP_embed, TP_fused) - TP_lexical: full {}, minus contamination set {}, minus own-text set {} (sensitivity, not in the verdict).\n",
        r.point.full.margin(),
        r.point.removed.margin(),
        r.sensitivity_margin
    );

    let _ = writeln!(s, "## Proposed pairs\n");
    for (name, v) in variants(r) {
        let _ = writeln!(s, "### {name}\n");
        for ret in RETRIEVERS {
            let _ = writeln!(s, "**{ret}**\n");
            for (a, bs) in &v.top[ret] {
                let _ = writeln!(s, "- {a} → {}", bs.join(", "));
            }
            let _ = writeln!(s);
            for (a, b) in retriever_pairs(&v.proposals, ret) {
                let label = r
                    .labels
                    .get(&(a.clone(), b.clone()))
                    .map_or("unlabelled", |l| l.as_str());
                let _ = writeln!(s, "- `{a}` × `{b}`: {label}");
            }
            let _ = writeln!(s);
        }
    }

    let _ = writeln!(s, "## Centroid safeguard\n");
    let _ = writeln!(
        s,
        "Mean pairwise cosine among the {} contaminated facts' vectors: before centroid subtraction {}, after {}.\n",
        r.safeguard.n,
        fmt_cos(r.safeguard.before),
        fmt_cos(r.safeguard.after)
    );

    let _ = writeln!(s, "## Single flips\n");
    let _ = writeln!(s, "| pair | from | to | TP full lex/emb/fus | TP cond2 lex/emb/fus | cond1 | cond2 | verdict |");
    let _ = writeln!(s, "|---|---|---|---|---|---|---|---|");
    for f in &r.flips {
        let o = &f.outcome;
        let _ = writeln!(
            s,
            "| {} × {} | {} | {} | {}/{}/{} | {}/{}/{} | {} | {} | {} |",
            f.pair.0,
            f.pair.1,
            f.from.as_str(),
            f.to.as_str(),
            o.full.lexical,
            o.full.embed,
            o.full.fused,
            o.removed.lexical,
            o.removed.embed,
            o.removed.fused,
            yes(o.cond1),
            yes(o.cond2),
            o.verdict.as_str()
        );
    }
    if r.flips.is_empty() {
        let _ = writeln!(s, "| (no flippable pairs) | | | | | | | |");
    }

    let _ = writeln!(s, "\n## Bootstrap\n");
    let _ = writeln!(
        s,
        "{} draws, each scored label flipped independently with p = {BOOTSTRAP_FLIP_P}, rankings fixed, splitmix64 seed `{BOOTSTRAP_SEED:#018x}`: {} draws pass both conditions ({:.1}%). Phase 2 needs the point verdict to pass and at least 80% of draws (passing * 5 >= draws * 4).\n",
        BOOTSTRAP_DRAWS,
        r.bootstrap_pass,
        r.share() * 100.0,
    );
    let _ = writeln!(s, "## Verdict\n");
    let _ = writeln!(s, "{}", verdict_line(r));
    s
}

fn list_paths(paths: &BTreeSet<PathBuf>) -> String {
    if paths.is_empty() {
        return "none".to_string();
    }
    paths
        .iter()
        .map(|p| format!("`{}`", p.display()))
        .collect::<Vec<_>>()
        .join(", ")
}

fn list_pairs(pairs: &BTreeSet<Pair>) -> String {
    if pairs.is_empty() {
        return "none".to_string();
    }
    pairs
        .iter()
        .map(|(a, b)| format!("`{a}` × `{b}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_json(r: &Report) -> serde_json::Value {
    use serde_json::json;
    let tps = |t: &Tps| json!({ "lexical": t.lexical, "embed": t.embed, "fused": t.fused, "margin": t.margin() });
    let variant = |v: &VariantRun| {
        let retrievers: serde_json::Map<String, serde_json::Value> = RETRIEVERS
            .iter()
            .map(|&ret| {
                let pairs = retriever_pairs(&v.proposals, ret);
                let m = metric(pairs, &r.labels);
                (
                    ret.to_string(),
                    json!({
                        "tp": m.tp,
                        "proposed": m.proposed,
                        "scored": m.scored,
                        "related": m.related,
                        "precision_at_3": m.precision(),
                        "recall_at_3": m.recall(),
                        "short_proposals": short_proposals(&v.top[ret]),
                        "top": v.top[ret],
                        "pairs": pairs.iter().map(|(a, b)| [a, b]).collect::<Vec<_>>(),
                    }),
                )
            })
            .collect();
        json!({ "ideas": v.ideas, "facts": v.facts, "units": v.units.len(), "full_rankings": v.rankings, "retrievers": retrievers })
    };
    let paths = |p: &BTreeSet<PathBuf>| {
        p.iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
    };
    json!({
        "corpus": r.args.corpus.display().to_string(),
        "labels": r.args.labels.display().to_string(),
        "weak_file": r.args.weak.as_ref().map(|p| p.display().to_string()),
        "weak_pairs": r.weak.iter().map(|(a, b)| [a, b]).collect::<Vec<_>>(),
        "model": r.args.model,
        "model_digest": r.digest.reported,
        "cache_digest": r.digest.key,
        "top_k": TOP_K,
        "rrf_k": RRF_K,
        "precision_denominator": PRECISION_NOTE,
        "contamination_regex": CONTAMINATION_REGEX,
        "contamination_set": paths(&r.contamination.full),
        "own_text_set": paths(&r.contamination.own_text),
        "full": variant(&r.full),
        "minus_contamination": variant(&r.removed),
        "minus_own_text": variant(&r.sensitivity),
        "kill": {
            "full": tps(&r.point.full),
            "minus_contamination": tps(&r.point.removed),
            "minus_own_text_margin": r.sensitivity_margin,
            "cond1": r.point.cond1,
            "cond2": r.point.cond2,
            "verdict": r.point.verdict.as_str(),
        },
        "safeguard": {
            "n": r.safeguard.n,
            "mean_pairwise_cosine_before": r.safeguard.before,
            "mean_pairwise_cosine_after": r.safeguard.after,
        },
        "single_flips": r.flips.iter().map(|f| json!({
            "a": f.pair.0,
            "b": f.pair.1,
            "from": f.from.as_str(),
            "to": f.to.as_str(),
            "full": tps(&f.outcome.full),
            "minus_contamination": tps(&f.outcome.removed),
            "cond1": f.outcome.cond1,
            "cond2": f.outcome.cond2,
            "verdict": f.outcome.verdict.as_str(),
        })).collect::<Vec<_>>(),
        "bootstrap": {
            "draws": BOOTSTRAP_DRAWS,
            "flip_p": BOOTSTRAP_FLIP_P,
            "seed": format!("{BOOTSTRAP_SEED:#018x}"),
            "passing": r.bootstrap_pass,
            "share": r.share(),
        },
        "verdict": r.point.verdict.as_str(),
        "phase2": if r.built() { "BUILT" } else { "KILLED" },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn pairs(v: &[(&str, &str)]) -> BTreeSet<Pair> {
        v.iter().map(|(a, b)| pair(a, b)).collect()
    }

    fn labels(v: &[(&str, &str, Label)]) -> Labels {
        v.iter().map(|(a, b, l)| (pair(a, b), *l)).collect()
    }

    #[test]
    fn cosine_and_centroid_subtraction_match_hand_computed_values() {
        assert!(close(cosine(&[1.0, 0.0], &[1.0, 1.0]), 1.0 / 2f64.sqrt()));
        assert!(close(cosine(&[1.0, 2.0], &[-2.0, 1.0]), 0.0));
        assert!(close(cosine(&[3.0, 4.0], &[6.0, 8.0]), 1.0));
        assert_eq!(
            cosine(&[0.0, 0.0], &[1.0, 0.0]),
            0.0,
            "a zero vector has cosine 0"
        );

        let vs = vec![vec![1.0, 0.0], vec![3.0, 2.0], vec![2.0, 4.0]];
        assert_eq!(centroid(&vs), vec![2.0, 2.0]);
        let centred = subtract_centroid(&vs);
        assert_eq!(
            centred,
            vec![vec![-1.0, -2.0], vec![1.0, 0.0], vec![0.0, 2.0]]
        );
        assert!(close(cosine(&centred[0], &centred[1]), -1.0 / 5f64.sqrt()));
        assert!(close(cosine(&centred[1], &centred[2]), 0.0));
        assert!(close(cosine(&centred[0], &centred[2]), -2.0 / 5f64.sqrt()));
    }

    #[test]
    fn centroid_subtraction_removes_a_shared_offset() {
        let big = 100.0;
        let vs = vec![
            vec![1.0, 0.0, big],
            vec![0.0, 1.0, big],
            vec![-1.0, 0.0, big],
            vec![0.0, -1.0, big],
        ];
        let before = cosine(&vs[0], &vs[1]);
        assert!(before > 0.9999, "shared offset dominates before: {before}");
        let centred = subtract_centroid(&vs);
        let after = cosine(&centred[0], &centred[1]);
        assert!(
            after.abs() < 1e-9,
            "orthogonal again after subtraction: {after}"
        );
        let three = vec![
            vec![1.0, 0.0, 0.0, big],
            vec![0.0, 1.0, 0.0, big],
            vec![0.0, 0.0, 1.0, big],
        ];
        let c = subtract_centroid(&three);
        assert!(
            close(cosine(&c[0], &c[1]), -0.5),
            "no offset survives in the pairwise angle"
        );
        assert!(close(c[0][3], 0.0) && close(c[1][3], 0.0) && close(c[2][3], 0.0));
    }

    #[test]
    fn idea_score_is_the_max_over_cross_idea_pairs_and_own_idea_is_never_proposed() {
        let slugs = s(&["a", "b", "c", "d"]);
        let units = vec![
            (0, vec![1.0, 0.0]),
            (0, vec![1.0, 0.0]),
            (1, vec![0.0, 1.0]),
            (1, vec![0.6, 0.8]),
            (2, vec![-1.0, 0.0]),
            (2, vec![0.8, 0.6]),
        ];
        let r = embed_rankings(&slugs, &units);
        let a = &r["a"];
        assert_eq!(
            a.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(),
            ["c", "b"],
            "d has no units and a is never proposed for itself"
        );
        assert!(
            close(a[0].1, 0.8),
            "c scores by its best unit, not its mean: {}",
            a[0].1
        );
        assert!(close(a[1].1, 0.6));
        for (slug, ranking) in &r {
            assert!(
                ranking.iter().all(|(o, _)| o != slug),
                "{slug} proposed itself"
            );
        }
        assert!(r["d"].is_empty());
        let b = &r["b"];
        assert!(
            close(b[0].1, 0.96),
            "b-c max is 0.6*0.8 + 0.8*0.6 = 0.96: {:?}",
            b
        );
    }

    #[test]
    fn embed_ranking_ties_break_by_slug() {
        let slugs = s(&["x", "m", "k"]);
        let units = vec![
            (0, vec![1.0, 0.0]),
            (1, vec![0.0, 1.0]),
            (2, vec![0.0, 1.0]),
        ];
        let r = embed_rankings(&slugs, &units);
        assert_eq!(
            r["x"].iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(),
            ["k", "m"]
        );
    }

    #[test]
    fn rrf_k60_sums_reciprocal_ranks_and_breaks_ties_by_slug() {
        let lexical = s(&["b", "c", "d"]);
        let embed = s(&["c", "e", "b"]);
        let fused = rrf(&[&lexical, &embed], 60.0);
        let got: Vec<&str> = fused.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(got, ["c", "b", "e", "d"]);
        let score = |slug: &str| fused.iter().find(|(s, _)| s == slug).unwrap().1;
        assert!(close(score("b"), 1.0 / 61.0 + 1.0 / 63.0));
        assert!(close(score("c"), 1.0 / 62.0 + 1.0 / 61.0));
        assert!(
            close(score("d"), 1.0 / 63.0),
            "d is missing from embed: only its lexical term"
        );
        assert!(close(score("e"), 1.0 / 62.0));

        let tie = rrf(&[&s(&["y", "x"]), &s(&["x", "y"])], 60.0);
        assert_eq!(
            tie.iter().map(|(s, _)| s.as_str()).collect::<Vec<_>>(),
            ["x", "y"]
        );
        assert!(close(tie[0].1, tie[1].1));
    }

    #[test]
    fn fused_top_uses_the_full_lexical_ranking() {
        let lexical = s(&["a", "b", "c", "d", "e", "f", "g"]);
        let embed = s(&["d", "g", "f", "e", "c", "b", "a"]);
        assert_eq!(
            fused_top("q", &lexical, &embed),
            ["d", "a", "b"],
            "d: lexical rank 4 + embed rank 1 = 1/64 + 1/61 beats a = 1/61 + 1/67"
        );
        assert_eq!(
            fused_top("q", &lexical[..TOP_K], &embed),
            ["a", "b", "c"],
            "a lexical top-3 alone would drop d's lexical term"
        );
    }

    #[test]
    fn metric_excludes_uncertain_from_precision_and_recall() {
        let l = labels(&[
            ("a", "b", Label::Related),
            ("a", "c", Label::Related),
            ("a", "d", Label::Uncertain),
            ("b", "c", Label::Unrelated),
            ("b", "d", Label::Related),
            ("c", "d", Label::Uncertain),
        ]);
        let proposed = pairs(&[("b", "a"), ("a", "d"), ("b", "c"), ("c", "d"), ("a", "z")]);
        let m = metric(&proposed, &l);
        assert_eq!(
            m,
            Metric {
                tp: 1,
                proposed: 5,
                scored: 2,
                related: 3
            }
        );
        assert!(close(m.precision().unwrap(), 0.5));
        assert!(close(m.recall().unwrap(), 1.0 / 3.0));
    }

    #[test]
    fn labels_csv_parses_extra_columns_quotes_and_weak() {
        let csv = "a,b,label,note,weak\nb,a,related,\"x, y\",0\na,c,uncertain,,1\nc,b,unrelated,\"say \"\"hi\"\"\",true\n";
        let (l, weak) = parse_labels(csv).unwrap();
        assert_eq!(l[&pair("a", "b")], Label::Related);
        assert_eq!(l[&pair("a", "c")], Label::Uncertain);
        assert_eq!(l[&pair("b", "c")], Label::Unrelated);
        assert_eq!(weak, Some(pairs(&[("a", "c"), ("b", "c")])));
        let (_, none) = parse_labels("a,b,label\na,b,related\n").unwrap();
        assert_eq!(none, None, "no weak column is not an empty weak set");
        assert!(parse_labels("x,y,label\n").is_err());
        assert!(parse_labels("a,b,label\na,b,maybe\n").is_err());

        let w = parse_weak("a,b,label,L1,weak\nb,a,unrelated,0,1\nc,a,unrelated,0,0\n").unwrap();
        assert_eq!(w, pairs(&[("a", "b")]));
        assert!(parse_weak("a,b,label\n").is_err());
    }

    #[test]
    fn resolve_weak_merges_both_sources_and_needs_at_least_one() {
        let l = labels(&[("a", "b", Label::Unrelated), ("a", "c", Label::Unrelated)]);
        let err = resolve_weak(&l, None, None).expect_err("no weak column anywhere must fail");
        assert!(err.to_string().contains("weak"), "{err}");
        assert_eq!(
            resolve_weak(&l, Some(BTreeSet::new()), None).unwrap(),
            BTreeSet::new()
        );
        assert_eq!(
            resolve_weak(&l, None, Some(pairs(&[("a", "b")]))).unwrap(),
            pairs(&[("a", "b")])
        );
        assert_eq!(
            resolve_weak(&l, Some(pairs(&[("a", "c")])), Some(pairs(&[("a", "b")]))).unwrap(),
            pairs(&[("a", "b"), ("a", "c")])
        );
        assert!(
            resolve_weak(&l, None, Some(pairs(&[("a", "z")]))).is_err(),
            "a weak pair must be a labelled pair"
        );
    }

    fn tps(lexical: usize, embed: usize, fused: usize) -> Tps {
        Tps {
            lexical,
            embed,
            fused,
        }
    }

    #[test]
    fn kill_needs_both_conditions() {
        let o = kill(tps(1, 3, 2), tps(2, 2, 2));
        assert!(o.cond1 && !o.cond2);
        assert_eq!(o.verdict, Verdict::Killed, "cond1 alone does not pass");

        let o = kill(tps(1, 2, 3), tps(1, 2, 1));
        assert!(o.cond1 && o.cond2);
        assert_eq!(o.verdict, Verdict::Pass);

        let o = kill(tps(2, 3, 3), tps(0, 5, 5));
        assert!(!o.cond1 && o.cond2);
        assert_eq!(
            o.verdict,
            Verdict::Killed,
            "a margin of 1 on the full corpus is not enough"
        );
    }

    #[test]
    fn phase2_build_rule_uses_integer_arithmetic() {
        assert!(phase2_built(Verdict::Pass, 800, 1000));
        assert!(!phase2_built(Verdict::Pass, 799, 1000));
        assert!(!phase2_built(Verdict::Killed, 1000, 1000));
        assert!(
            !phase2_built(
                Verdict::Pass,
                40_000_000_000_000_000 - 1,
                50_000_000_000_000_000
            ),
            "just under 4/5, which f64 division rounds up to 0.8"
        );
    }

    fn fixture() -> (Labels, Proposals, Proposals) {
        let l = labels(&[
            ("a", "b", Label::Related),
            ("a", "c", Label::Related),
            ("b", "c", Label::Related),
            ("a", "d", Label::Unrelated),
            ("b", "d", Label::Uncertain),
            ("c", "d", Label::Unrelated),
        ]);
        let full = Proposals {
            lexical: pairs(&[("a", "d")]),
            embed: pairs(&[("a", "b"), ("a", "c"), ("b", "c")]),
            fused: pairs(&[("a", "b")]),
        };
        let removed = Proposals {
            lexical: pairs(&[("a", "b")]),
            embed: pairs(&[("a", "b"), ("b", "c")]),
            fused: pairs(&[("a", "b")]),
        };
        (l, full, removed)
    }

    #[test]
    fn single_flips_cover_uncertain_both_ways_and_related_to_unrelated() {
        let (l, full, removed) = fixture();
        assert_eq!(kill_for(&l, &full, &removed).verdict, Verdict::Pass);
        let weak = pairs(&[("c", "d")]);
        let flips = single_flips(&l, &weak, &full, &removed);
        let seen: Vec<(String, Label, Label, Verdict)> = flips
            .iter()
            .map(|f| {
                (
                    format!("{}-{}", f.pair.0, f.pair.1),
                    f.from,
                    f.to,
                    f.outcome.verdict,
                )
            })
            .collect();
        assert_eq!(
            seen,
            vec![
                (
                    "a-b".into(),
                    Label::Related,
                    Label::Unrelated,
                    Verdict::Pass
                ),
                (
                    "a-c".into(),
                    Label::Related,
                    Label::Unrelated,
                    Verdict::Pass
                ),
                (
                    "b-c".into(),
                    Label::Related,
                    Label::Unrelated,
                    Verdict::Killed
                ),
                (
                    "b-d".into(),
                    Label::Uncertain,
                    Label::Related,
                    Verdict::Pass
                ),
                (
                    "b-d".into(),
                    Label::Uncertain,
                    Label::Unrelated,
                    Verdict::Pass
                ),
                (
                    "c-d".into(),
                    Label::Unrelated,
                    Label::Related,
                    Verdict::Pass
                ),
            ]
        );
    }

    #[test]
    fn bootstrap_is_deterministic_and_p0_matches_the_point_verdict() {
        let (l, full, removed) = fixture();
        let a = bootstrap(&l, &full, &removed, 1000, 0.2, 42);
        let b = bootstrap(&l, &full, &removed, 1000, 0.2, 42);
        assert_eq!(a, b, "same seed, same share");
        assert_eq!(
            a, 714,
            "golden pass count for the fixture, p = 0.2, seed 42"
        );
        assert_eq!(
            bootstrap(
                &l,
                &full,
                &removed,
                BOOTSTRAP_DRAWS,
                BOOTSTRAP_FLIP_P,
                BOOTSTRAP_SEED
            ),
            741,
            "golden pass count for the fixture under the pre-registered seed"
        );
        assert_eq!(bootstrap(&l, &full, &removed, 1000, 0.0, 7), 1000);

        let mut killed = l.clone();
        killed.insert(pair("b", "c"), Label::Unrelated);
        assert_eq!(kill_for(&killed, &full, &removed).verdict, Verdict::Killed);
        assert_eq!(bootstrap(&killed, &full, &removed, 1000, 0.0, 7), 0);
        assert_eq!(
            bootstrap(&l, &full, &removed, 1000, 1.0, 7),
            0,
            "p = 1 flips every scored label and loses every TP"
        );
    }

    #[test]
    fn splitmix64_matches_reference_values() {
        let mut r = SplitMix64(0);
        assert_eq!(r.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(r.next_u64(), 0x6E78_9E6A_A1B9_65F4);
        let mut u = SplitMix64(BOOTSTRAP_SEED);
        let x = u.next_f64();
        assert!((0.0..1.0).contains(&x));
    }

    const FM_ONLY: &str = "---\nslug: sweet-wine\ntitle: Sweet wine is a trap\ntags: []\ncreated: 2026-09-28T20:00:00Z\nlinks:\n- cheapest-disproof-comes-first\n---\n\nThe label says otherwise.\n";

    #[test]
    fn contamination_regex_matches_the_preregistered_phrase() {
        assert!(matches_contamination("Run the Cheapest disproof first"));
        assert!(matches_contamination("see cheapest-disproof-comes-first"));
        assert!(matches_contamination("CHEAPEST DISPROOF"));
        assert!(matches_contamination("cheape\u{17F}t disproof"));
        assert!(!matches_contamination("cheapest  disproof"), "two spaces");
        assert!(!matches_contamination("cheapest_disproof"));
        assert!(!matches_contamination("cheapest disproo"));
        assert!(!matches_contamination("the cheap disproof"));
    }

    #[test]
    fn contamination_sets_split_frontmatter_links_from_own_text() {
        assert_eq!(classify_fact_file(FM_ONLY).unwrap(), (true, false));

        let link_only = FM_ONLY
            .replace("links:\n- cheapest-disproof-comes-first\n", "links: []\n")
            .replace(
                "otherwise.",
                "otherwise, see [[cheapest-disproof-comes-first]].",
            );
        assert_eq!(classify_fact_file(&link_only).unwrap(), (true, false));

        let own = FM_ONLY.replace("otherwise.", "otherwise: run the cheapest disproof.");
        assert_eq!(classify_fact_file(&own).unwrap(), (true, true));

        let title = FM_ONLY
            .replace("links:\n- cheapest-disproof-comes-first\n", "links: []\n")
            .replace("Sweet wine is a trap", "Cheapest-disproof first");
        assert_eq!(classify_fact_file(&title).unwrap(), (true, true));

        let clean = FM_ONLY.replace("links:\n- cheapest-disproof-comes-first\n", "links: []\n");
        assert_eq!(classify_fact_file(&clean).unwrap(), (false, false));

        assert_eq!(strip_link_tokens("a [[x-y]] b [[z"), "a  b [[z");
    }

    #[test]
    fn cache_key_is_pinned_and_differs_by_model_digest_and_text() {
        assert_eq!(
            cache_key("m", "d", "t"),
            "93ca2334c8b1fd8ea6de7b9c073005794bc149748b0c0eed32cfa97d53d204d6"
        );
        assert_eq!(
            cache_key("embeddinggemma", "sha256:abc", "hello"),
            "a231f5789787d5b70573e7157f7be11523097187aa971dea97a2cedd3f5a13ec"
        );
        let k = cache_key("embeddinggemma", "sha256:abc", "hello");
        assert_ne!(k, cache_key("other-model", "sha256:abc", "hello"));
        assert_ne!(k, cache_key("embeddinggemma", "sha256:abd", "hello"));
        assert_ne!(k, cache_key("embeddinggemma", "sha256:abc", "hello "));
        assert_ne!(
            cache_key("ab", "c", "t"),
            cache_key("a", "bc", "t"),
            "NUL separators keep model and digest apart"
        );
    }

    struct FakeEmbedder {
        calls: RefCell<Vec<usize>>,
    }

    impl FakeEmbedder {
        fn new() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    const FAKE_VOCAB: [&str; 6] = ["ledger", "idempotency", "wine", "dinner", "agent", "prompt"];
    const FAKE_DIGEST: &str = "sha256:fake";

    impl Embedder for FakeEmbedder {
        fn digest(&self, _model: &str) -> Result<String> {
            Ok(FAKE_DIGEST.to_string())
        }

        fn embed(&self, _model: &str, texts: &[String]) -> Result<Vec<Vec<f64>>> {
            self.calls.borrow_mut().push(texts.len());
            Ok(texts
                .iter()
                .map(|t| {
                    let lower = t.to_lowercase();
                    let mut v: Vec<f64> = FAKE_VOCAB
                        .iter()
                        .map(|w| lower.matches(w).count() as f64)
                        .collect();
                    v.push(10.0);
                    v
                })
                .collect())
        }
    }

    #[test]
    fn embed_texts_batches_sequentially_and_reuses_the_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path()).unwrap();
        let texts: Vec<String> = (0..20)
            .map(|i| format!("text {i}"))
            .chain(["text 0".to_string()])
            .collect();
        let fake = FakeEmbedder::new();
        let v = embed_texts(&texts, "m", "d", &cache, Some(&fake)).unwrap();
        assert_eq!(v.len(), 21);
        assert_eq!(
            *fake.calls.borrow(),
            vec![16, 4],
            "20 distinct misses in batches of 16"
        );
        let again = embed_texts(&texts, "m", "d", &cache, None).unwrap();
        assert_eq!(v, again, "offline run is served from the cache");
        let err = embed_texts(&s(&["never seen"]), "m", "d", &cache, None).unwrap_err();
        assert!(err.to_string().contains("offline"), "{err}");
        let other = embed_texts(&texts[..1], "other", "d", &cache, None);
        assert!(other.is_err(), "a different model is a different cache key");
    }

    #[test]
    fn embed_request_disables_truncation() {
        let body = embed_request_body("embeddinggemma", &s(&["a", "b"]));
        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            r#"{"input":["a","b"],"model":"embeddinggemma","truncate":false}"#
        );
    }

    #[test]
    fn find_digest_resolves_untagged_names_to_latest() {
        let tags: TagsResponse = serde_json::from_value(json!({"models": [
            {"name": "embeddinggemma:latest", "model": "embeddinggemma:latest", "digest": "sha256:aaa"},
            {"name": "embeddinggemma:300m", "digest": "sha256:bbb"},
        ]}))
        .unwrap();
        assert_eq!(find_digest(&tags, "embeddinggemma").unwrap(), "sha256:aaa");
        assert_eq!(
            find_digest(&tags, "embeddinggemma:300m").unwrap(),
            "sha256:bbb"
        );
        assert!(find_digest(&tags, "nomic-embed-text").is_err());
    }

    type Seen = Vec<(String, String)>;

    fn stub_server(responses: Vec<String>) -> (String, std::thread::JoinHandle<Seen>) {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut seen = Vec::new();
            for body in responses {
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if Instant::now() > deadline {
                                return seen;
                            }
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(e) => panic!("accept: {e}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let head_end = loop {
                    let n = stream.read(&mut chunk).unwrap();
                    assert!(n > 0, "client closed before the request head");
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let len = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while buf.len() < head_end + len {
                    let n = stream.read(&mut chunk).unwrap();
                    assert!(n > 0, "client closed before the request body");
                    buf.extend_from_slice(&chunk[..n]);
                }
                let line = head.lines().next().unwrap_or_default().to_string();
                let req_body = String::from_utf8_lossy(&buf[head_end..head_end + len]).to_string();
                seen.push((line, req_body));
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
            seen
        });
        (url, handle)
    }

    #[test]
    fn ollama_embedder_reads_the_digest_and_posts_untruncated_batches() {
        let tags = json!({"models": [
            {"name": "other:latest", "model": "other:latest", "digest": "sha256:000"},
            {"name": "embeddinggemma:latest", "model": "embeddinggemma:latest", "digest": "sha256:abc"},
        ]});
        let embed = json!({"model": "embeddinggemma", "embeddings": [[1.0, 2.0], [3.0, 4.0]]});
        let (url, server) = stub_server(vec![tags.to_string(), embed.to_string()]);
        let e = OllamaEmbedder::new(&url).unwrap();
        let digest = e.digest("embeddinggemma");
        let vectors = e.embed("embeddinggemma", &s(&["a", "b"]));
        let seen = server.join().unwrap();
        assert_eq!(digest.unwrap(), "sha256:abc");
        assert_eq!(vectors.unwrap(), vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
        assert_eq!(seen.len(), 2);
        assert!(seen[0].0.starts_with("GET /api/tags "), "{:?}", seen[0]);
        assert!(seen[1].0.starts_with("POST /api/embed "), "{:?}", seen[1]);
        let body: serde_json::Value = serde_json::from_str(&seen[1].1).unwrap();
        assert_eq!(
            body,
            json!({"model": "embeddinggemma", "input": ["a", "b"], "truncate": false})
        );
    }

    fn write_idea(
        root: &Path,
        slug: &str,
        tags: &[&str],
        body: &str,
        facts: &[(&str, &str, &str)],
    ) {
        let dir = root.join(slug);
        std::fs::create_dir_all(dir.join("memory")).unwrap();
        let tags = if tags.is_empty() {
            "tags: []\n".to_string()
        } else {
            format!(
                "tags:\n{}",
                tags.iter().map(|t| format!("- {t}\n")).collect::<String>()
            )
        };
        std::fs::write(
            dir.join("idea.md"),
            format!("---\ntitle: {slug}\nslug: {slug}\nstate: stored\n{tags}created: 2026-09-01T00:00:00Z\nupdated: 2026-09-01T00:00:00Z\n---\n\n{body}\n"),
        )
        .unwrap();
        std::fs::write(dir.join("conversation.md"), "## user\n\nhi\n").unwrap();
        for (fact, title, text) in facts {
            std::fs::write(
                dir.join("memory").join(format!("{fact}.md")),
                format!("---\nslug: {fact}\ntitle: {title}\ntags: []\ncreated: 2026-09-01T00:00:00Z\nlinks: []\n---\n\n{text}\n"),
            )
            .unwrap();
        }
    }

    const ALL_LABELS: &str = "a,b,label,note
bank,payments,related,
agents,tooling,related,
bank,dinner,unrelated,
dinner,payments,unrelated,
agents,bank,uncertain,
agents,dinner,unrelated,
agents,payments,unrelated,
bank,tooling,unrelated,
dinner,tooling,unrelated,
payments,tooling,unrelated,
";

    // Five ideas, every pair labelled. `quuxite` is a lexical-only term (outside the fake
    // embedder's vocabulary) shared by tooling's body and bank's contaminated fact.
    fn setup(root: &Path) -> Args {
        let corpus = root.join("corpus");
        write_idea(
            &corpus,
            "bank",
            &["ledger"],
            "A ledger service with idempotency keys.",
            &[
                (
                    "idem",
                    "Idempotency is a unique constraint",
                    "The ledger needs idempotency in one transaction.",
                ),
                (
                    "disproof",
                    "Run the cheapest disproof",
                    "Try the cheapest disproof before building the ledger quuxite.",
                ),
            ],
        );
        write_idea(
            &corpus,
            "payments",
            &["ledger"],
            "Payment ledger with idempotency.",
            &[("keys", "Ledger keys", "Idempotency keys guard the ledger.")],
        );
        write_idea(
            &corpus,
            "dinner",
            &["food"],
            "A historical dinner with sweet wine.",
            &[(
                "wine",
                "Wine choice",
                "The wine must match the dinner; see [[cheapest-disproof-first]].",
            )],
        );
        write_idea(
            &corpus,
            "agents",
            &["ai"],
            "Prompt agents that run unattended.",
            &[(
                "prompt",
                "Prompt contracts",
                "Each agent gets a prompt contract.",
            )],
        );
        write_idea(
            &corpus,
            "tooling",
            &["ai"],
            "Agent prompt tooling quuxite.",
            &[],
        );
        let labels = root.join("labels.csv");
        std::fs::write(&labels, ALL_LABELS).unwrap();
        let weak = root.join("weak.csv");
        std::fs::write(
            &weak,
            "a,b,label,L1_votes,L2_votes,weak\nbank,tooling,unrelated,0,1,1\nbank,dinner,unrelated,0,0,0\n",
        )
        .unwrap();
        Args {
            corpus,
            labels,
            weak: Some(weak),
            cache: root.join("cache"),
            out: root.join("results.md"),
            model: "fake".into(),
            offline: false,
            expect_contaminated: 2,
            expect_sensitivity: 1,
        }
    }

    #[test]
    fn synthetic_corpus_runs_end_to_end_with_a_fake_embedder() {
        let tmp = tempfile::tempdir().unwrap();
        let args = setup(tmp.path());
        let fake = FakeEmbedder::new();
        let report = run(&args, Some(&fake)).unwrap();

        assert_eq!(report.contamination.full.len(), 2);
        assert_eq!(report.contamination.own_text.len(), 1);
        assert_eq!(report.full.facts, 5);
        assert_eq!(report.removed.facts, 3);
        assert_eq!(report.sensitivity.facts, 4);
        assert_eq!(report.full.ideas, 5);
        for v in [&report.full, &report.removed, &report.sensitivity] {
            for ret in RETRIEVERS {
                for (a, bs) in &v.top[ret] {
                    assert!(bs.len() <= TOP_K && !bs.contains(a), "{ret} {a} {bs:?}");
                }
            }
            for slug in &v.slugs {
                let lexical = &v.rankings["lexical"][slug];
                assert_eq!(
                    &lexical[..lexical.len().min(TOP_K)],
                    v.top["lexical"][slug].as_slice(),
                    "lexical top-3 is the head of the full lexical ranking"
                );
                assert_eq!(
                    v.top["fused"][slug],
                    fused_top(slug, lexical, &v.rankings["embed"][slug]),
                    "fused is RRF over the full rankings"
                );
            }
        }
        assert!(report
            .full
            .proposals
            .embed
            .contains(&pair("bank", "payments")));
        assert!(report
            .full
            .proposals
            .embed
            .contains(&pair("agents", "tooling")));
        assert_eq!(report.safeguard.n, 2);
        assert!(report.safeguard.before.unwrap() > report.safeguard.after.unwrap());
        assert_eq!(report.digest.reported, FAKE_DIGEST);
        assert_eq!(report.digest.key, FAKE_DIGEST);

        assert_eq!(
            report.point,
            KillOutcome {
                full: tps(2, 2, 2),
                removed: tps(2, 2, 2),
                cond1: false,
                cond2: false,
                verdict: Verdict::Killed,
            },
            "golden kill outcome for the synthetic corpus"
        );
        assert_eq!(
            report.bootstrap_pass, 97,
            "golden: label noise alone lifts some draws past both margins"
        );
        assert!(!report.built());

        let md = render_markdown(&report);
        assert!(md.contains("VERDICT: KILLED"), "{md}");
        assert!(md.contains("PHASE 2: KILLED"), "{md}");
        assert!(md.contains(&format!("{BOOTSTRAP_SEED:#018x}")));
        assert!(md.contains(FAKE_DIGEST));
        let json = render_json(&report);
        assert_eq!(json["bootstrap"]["draws"], 1000);
        assert_eq!(json["verdict"], "KILLED");
        assert_eq!(json["phase2"], "KILLED");
        assert_eq!(json["model_digest"], FAKE_DIGEST);
        assert_eq!(
            json["full"]["full_rankings"]["lexical"]["tooling"],
            json!(report.full.rankings["lexical"]["tooling"])
        );

        let offline = Args {
            offline: true,
            ..args.clone()
        };
        let again = run(&offline, None).unwrap();
        assert_eq!(again.full.proposals.embed, report.full.proposals.embed);
        assert_eq!(again.bootstrap_pass, report.bootstrap_pass);
        assert_eq!(again.digest.reported, OFFLINE_DIGEST);
        assert_eq!(again.digest.key, FAKE_DIGEST);
        assert_eq!(render_json(&again)["model_digest"], OFFLINE_DIGEST);
    }

    #[test]
    fn report_states_the_precision_denominator() {
        let tmp = tempfile::tempdir().unwrap();
        let args = setup(tmp.path());
        let report = run(&args, Some(&FakeEmbedder::new())).unwrap();
        let md = render_markdown(&report);
        let scores = md.split("## Scores").nth(1).unwrap();
        assert!(
            scores.contains("P@3 = TP / proposed pairs in the scored set"),
            "{scores}"
        );
        assert!(scores.contains("uncertain pairs are excluded"), "{scores}");
    }

    #[test]
    fn short_proposals_counts_ideas_with_fewer_than_three() {
        let top: Tops = [
            ("a", s(&["b", "c", "d"])),
            ("b", s(&["a", "c"])),
            ("c", Vec::new()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        assert_eq!(short_proposals(&top), 2);

        let tmp = tempfile::tempdir().unwrap();
        let args = setup(tmp.path());
        let report = run(&args, Some(&FakeEmbedder::new())).unwrap();
        let lexical_short = short_proposals(&report.full.top["lexical"]);
        assert!(
            lexical_short >= 1,
            "tooling matches at most agents and bank lexically"
        );
        let json = render_json(&report);
        assert_eq!(
            json["full"]["retrievers"]["lexical"]["short_proposals"],
            lexical_short
        );
        assert_eq!(json["full"]["retrievers"]["embed"]["short_proposals"], 0);
        let md = render_markdown(&report);
        let row = md
            .lines()
            .find(|l| l.starts_with("| full corpus | 5 | 5 | lexical |"))
            .unwrap();
        assert!(row.ends_with(&format!("| {lexical_short} |")), "{row}");
    }

    #[test]
    fn run_fails_when_an_idea_has_no_embedding_units_in_any_variant() {
        let tmp = tempfile::tempdir().unwrap();
        let args = setup(tmp.path());
        write_idea(&args.corpus, "tooling", &["ai"], "", &[]);
        let err = run(&args, Some(&FakeEmbedder::new()))
            .err()
            .expect("an idea with no body and no facts must fail the run");
        assert!(format!("{err:#}").contains("tooling"), "{err:#}");

        let tmp = tempfile::tempdir().unwrap();
        let args = setup(tmp.path());
        let idea = args.corpus.join("dinner/idea.md");
        let text = std::fs::read_to_string(&idea).unwrap();
        std::fs::write(
            &idea,
            text.replace("A historical dinner with sweet wine.", ""),
        )
        .unwrap();
        let err = run(&args, Some(&FakeEmbedder::new()))
            .err()
            .expect("dinner loses its only unit in the contamination-removed variant");
        assert!(format!("{err:#}").contains("dinner"), "{err:#}");
    }

    #[test]
    fn run_fails_without_any_weak_column() {
        let tmp = tempfile::tempdir().unwrap();
        let args = Args {
            weak: None,
            ..setup(tmp.path())
        };
        let err = run(&args, Some(&FakeEmbedder::new()))
            .err()
            .expect("no weak column in the labels and no --weak must fail");
        assert!(format!("{err:#}").contains("weak"), "{err:#}");
    }

    #[test]
    fn weak_flag_adds_the_weak_unrelated_pair_to_single_flips() {
        let tmp = tempfile::tempdir().unwrap();
        let args = setup(tmp.path());
        let report = run(&args, Some(&FakeEmbedder::new())).unwrap();
        assert_eq!(report.weak, pairs(&[("bank", "tooling")]));
        let flip = report
            .flips
            .iter()
            .find(|f| f.pair == pair("bank", "tooling"));
        let flip = flip.expect("the weak unrelated pair is flipped");
        assert_eq!((flip.from, flip.to), (Label::Unrelated, Label::Related));
        assert!(
            !report
                .flips
                .iter()
                .any(|f| f.pair == pair("bank", "dinner")),
            "weak = 0 is not weak"
        );
    }

    #[test]
    fn labels_must_cover_exactly_the_corpus_pairs() {
        let tmp = tempfile::tempdir().unwrap();
        let args = setup(tmp.path());
        std::fs::write(
            &args.labels,
            ALL_LABELS.replace("payments,tooling,unrelated,\n", ""),
        )
        .unwrap();
        let err = run(&args, Some(&FakeEmbedder::new()))
            .err()
            .expect("an unlabelled corpus pair must fail");
        assert!(format!("{err:#}").contains("payments"), "{err:#}");

        std::fs::write(&args.labels, format!("{ALL_LABELS}bank,ghost,unrelated,\n")).unwrap();
        let err = run(&args, Some(&FakeEmbedder::new()))
            .err()
            .expect("a label naming a slug outside the corpus must fail");
        assert!(format!("{err:#}").contains("ghost"), "{err:#}");
    }

    #[test]
    fn expected_contamination_counts_are_enforced() {
        let tmp = tempfile::tempdir().unwrap();
        let base = setup(tmp.path());
        for (c, s) in [(6, 2), (2, 2), (3, 1)] {
            let args = Args {
                expect_contaminated: c,
                expect_sensitivity: s,
                ..base.clone()
            };
            let err = run(&args, Some(&FakeEmbedder::new()))
                .err()
                .unwrap_or_else(|| panic!("expecting {c}/{s} against 2/1 must fail"));
            assert!(format!("{err:#}").contains("expected"), "{err:#}");
        }
    }

    #[test]
    fn embed_side_ignores_conversation_text() {
        let tmp = tempfile::tempdir().unwrap();
        let args = setup(tmp.path());
        let plain = run(&args, Some(&FakeEmbedder::new())).unwrap();
        std::fs::write(
            args.corpus.join("tooling/conversation.md"),
            format!("## user\n\n{}\n", "wine dinner ledger ".repeat(40)),
        )
        .unwrap();
        let noisy = run(&args, Some(&FakeEmbedder::new())).unwrap();
        for (a, b) in [
            (&plain.full, &noisy.full),
            (&plain.removed, &noisy.removed),
            (&plain.sensitivity, &noisy.sensitivity),
        ] {
            assert_eq!(a.raw, b.raw, "no conversation text is embedded");
            assert_eq!(a.top["embed"], b.top["embed"]);
            assert_eq!(a.proposals.embed, b.proposals.embed);
            assert_eq!(a.top["lexical"], b.top["lexical"]);
        }
    }

    #[test]
    fn removed_variant_reindexes_lexical() {
        let tmp = tempfile::tempdir().unwrap();
        let args = setup(tmp.path());
        let report = run(&args, Some(&FakeEmbedder::new())).unwrap();
        let bank = "bank".to_string();
        assert!(
            report.full.top["lexical"]["tooling"].contains(&bank),
            "quuxite links tooling to bank's contaminated fact: {:?}",
            report.full.top["lexical"]
        );
        assert!(
            !report.removed.top["lexical"]["tooling"].contains(&bank),
            "the removed variant must be reindexed without that fact: {:?}",
            report.removed.top["lexical"]
        );
        assert_ne!(report.full.top["lexical"], report.removed.top["lexical"]);
    }

    #[test]
    fn args_parse_paths_counts_and_defaults() {
        let a = parse_args(s(&[
            "--corpus", "c", "--labels", "l", "--cache", "k", "--out", "o.md",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(a.model, DEFAULT_MODEL);
        assert!(!a.offline);
        assert_eq!(a.weak, None);
        assert_eq!((a.expect_contaminated, a.expect_sensitivity), (6, 2));
        let b = parse_args(s(&[
            "--corpus",
            "c",
            "--labels",
            "l",
            "--cache",
            "k",
            "--out",
            "o.md",
            "--weak",
            "w.csv",
            "--expect-contaminated",
            "4",
            "--expect-sensitivity",
            "1",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(b.weak, Some(PathBuf::from("w.csv")));
        assert_eq!((b.expect_contaminated, b.expect_sensitivity), (4, 1));
        assert!(parse_args(s(&["--corpus", "c"])).is_err());
        assert!(parse_args(s(&["--bogus"])).is_err());
        assert!(parse_args(s(&["--expect-sensitivity", "two"])).is_err());
        assert!(parse_args(s(&["--help"])).unwrap().is_none());
    }
}
