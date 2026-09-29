//! G13 leaf gate: every planned task is checked against the leaf invariants — one commit subject
//! (L1), one top-level root (L2), one counted check (L3) — and the split triggers. The gate only
//! adds `⟨…⟩` markers and tally counts; it never demotes a task to `[?]`, so a weak model's plan
//! stays buildable and the executor decides whether to split.

use super::{GateInputs, GateReport};
use crate::concepts::build_plan::plan::{BuildPlan, Item};

pub const SPLIT_ONE_COMMIT: &str = "split: one commit";
pub const CROSSES_ROOTS: &str = "crosses roots";
pub const COMPOUND_ACCEPT: &str = "compound accept";
pub const NO_COUNT: &str = "no count: a filter matching 0 tests exits 0";
pub const NO_RED: &str = "no red-first proof";
pub const SWEEP: &str = "sweep: end with a grep printing 0";

const MAX_NON_TEST_TOUCHES: usize = 3;
const MAX_READS_AND_TOUCHES: usize = 6;

const COMMIT_VERBS: &[&str] = &[
    "add",
    "adds",
    "fix",
    "fixes",
    "remove",
    "removes",
    "delete",
    "rename",
    "move",
    "wire",
    "document",
    "update",
    "refactor",
    "extract",
    "replace",
    "drop",
    "write",
    "render",
    "gate",
    "merge",
    "implement",
    "create",
    "handle",
    "support",
    "make",
    "change",
    "reject",
    "prevent",
    "expose",
    "emit",
    "store",
    "load",
    "persist",
    "validate",
    "migrate",
    "introduce",
    "bump",
    "clean",
    "test",
    "then",
    "also",
    "split",
    "port",
    "cache",
    "log",
    "show",
    "hide",
    "pin",
];

const BEHAVIOUR_VERBS: &[&str] = &[
    "fix", "fixes", "reject", "rejects", "prevent", "prevents", "must", "change", "changes",
];

const SWEEP_PHRASES: &[&str] = &[
    "sweep",
    "everywhere",
    "across all",
    "across the codebase",
    "in all files",
    "in every file",
    "every call site",
    "all call sites",
    "all callers",
    "every caller",
];

const COMMAND_STARTS: &[&str] = &[
    "cargo", "npm", "pnpm", "yarn", "npx", "bun", "deno", "node", "pytest", "python", "python3",
    "go", "make", "just", "grep", "rg", "bash", "sh", "curl", "diff", "git", "docker", "jest",
    "vitest", "mvn", "gradle", "test",
];

const TEST_RUNNERS: &[&[&str]] = &[
    &["cargo", "test"],
    &["cargo", "nextest"],
    &["npm", "test"],
    &["npm", "run", "test"],
    &["pnpm", "test"],
    &["pnpm", "run", "test"],
    &["yarn", "test"],
    &["bun", "test"],
    &["deno", "test"],
    &["go", "test"],
    &["make", "test"],
    &["mvn", "test"],
    &["gradle", "test"],
    &["pytest"],
    &["python", "-m", "pytest"],
    &["python3", "-m", "pytest"],
    &["jest"],
    &["vitest"],
    &["npx", "jest"],
    &["npx", "vitest"],
];

/// Mark every task of `plan` against the leaf invariants and split triggers, and tally each task
/// once as `leaf_ok` (no finding), `leaf_split` (a `split:` marker) or `leaf_notes` (any other
/// finding, including empty touches, which is also tallied as `unscoped` and never marked).
pub fn apply(plan: &mut BuildPlan, _inputs: &GateInputs, report: &mut GateReport) {
    let floor = plan.tasks.len() == 1 && plan.tasks[0].list("touches").len() <= 1;
    for task in &mut plan.tasks {
        let findings = leaf_findings(task, floor);
        let unscoped = task.list("touches").is_empty();
        if unscoped {
            report.count("unscoped");
        }
        let split = findings.iter().any(|m| m.starts_with("split:"));
        for marker in findings {
            if !task.markers.contains(&marker) {
                task.markers.push(marker.clone());
            }
        }
        report.count(if split {
            "leaf_split"
        } else if unscoped || task.markers.iter().any(|m| is_leaf_marker(m)) {
            "leaf_notes"
        } else {
            "leaf_ok"
        });
    }
}

fn is_leaf_marker(marker: &str) -> bool {
    [
        SPLIT_ONE_COMMIT,
        CROSSES_ROOTS,
        COMPOUND_ACCEPT,
        NO_COUNT,
        NO_RED,
        SWEEP,
        "justify:",
        "split:",
    ]
    .iter()
    .any(|p| marker.starts_with(p))
}

/// The leaf markers `task` earns; `floor` (a one-task, one-file plan) suppresses split notes.
fn leaf_findings(task: &Item, floor: bool) -> Vec<String> {
    let mut out = Vec::new();
    let subject = unticked(&task.text).to_lowercase();
    let touches = task.list("touches");
    let accept = task.field("accept").unwrap_or_default();
    let command = accept_command(accept);

    if !floor && two_subjects(&subject) {
        out.push(SPLIT_ONE_COMMIT.to_string());
    }
    let roots = roots(&touches);
    if roots.len() > 1 {
        out.push(format!("{CROSSES_ROOTS}: {}", roots.join(", ")));
    }
    if !accept.is_empty() && is_compound(accept, &command) {
        out.push(COMPOUND_ACCEPT.to_string());
    }
    if is_test_runner(&command) && !has_count(accept_condition(accept)) {
        out.push(NO_COUNT.to_string());
    }

    let non_test = touches.iter().filter(|p| !is_test_path(p)).count();
    let reads = task.list("reads").len();
    let mut triggers = Vec::new();
    if non_test > MAX_NON_TEST_TOUCHES {
        triggers.push(format!("{non_test} non-test files"));
    }
    if reads + touches.len() > MAX_READS_AND_TOUCHES {
        triggers.push(format!("{} reads + touches", reads + touches.len()));
    }
    if !floor {
        match triggers.len() {
            0 => {}
            1 if task.field("exempt").is_some() => {}
            1 => out.push(format!("justify: {} — add exempt: or split", triggers[0])),
            _ => out.push(format!("split: {}", triggers.join(", "))),
        }
    }

    if task.field("red").is_none() && words(&subject).any(|w| BEHAVIOUR_VERBS.contains(&w)) {
        out.push(NO_RED.to_string());
    }
    let first = command.split_whitespace().next().unwrap_or_default();
    let grep = matches!(first, "grep" | "rg") || command.starts_with("git grep");
    if SWEEP_PHRASES.iter().any(|p| subject.contains(p)) && !grep {
        out.push(SWEEP.to_string());
    }
    out
}

fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !c.is_alphanumeric() && c != '-')
        .filter(|w| !w.is_empty())
}

/// `text` without its backticked spans (names and paths, not wording).
fn unticked(text: &str) -> String {
    text.split('`').step_by(2).collect::<Vec<_>>().join(" ")
}

/// L1: the subject joins two changes with `and`, or lists a second verb after a comma.
fn two_subjects(subject: &str) -> bool {
    if subject.contains(" and ") {
        return true;
    }
    subject.split(',').skip(1).any(|piece| {
        words(piece)
            .next()
            .is_some_and(|w| COMMIT_VERBS.contains(&w))
    })
}

/// L2: the distinct top-level directories of `touches`, test trees and repo-root files aside.
fn roots(touches: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for path in touches.iter().filter(|p| !is_test_path(p)) {
        let path = path.trim_start_matches("./").trim_start_matches('/');
        let Some((root, _)) = path.split_once('/') else {
            continue;
        };
        if !root.is_empty() && !out.iter().any(|r| r == root) {
            out.push(root.to_string());
        }
    }
    out.sort();
    out
}

fn is_test_path(path: &str) -> bool {
    let path = path.to_lowercase();
    let name = path.rsplit('/').next().unwrap_or_default();
    path.split('/')
        .any(|seg| matches!(seg, "tests" | "test" | "__tests__" | "spec" | "specs"))
        || name.starts_with("test_")
        || ["_test.", ".test.", "_spec.", ".spec.", "_tests."]
            .iter()
            .any(|p| name.contains(p))
}

/// The accept's command: its first backticked span, else the text before the arrow.
fn accept_command(accept: &str) -> String {
    let spans: Vec<&str> = accept.split('`').skip(1).step_by(2).collect();
    let raw = match spans.first() {
        Some(first) if accept.matches('`').count() >= 2 => first,
        _ => accept
            .split_once('→')
            .or_else(|| accept.split_once("->"))
            .map_or(accept, |(cmd, _)| cmd),
    };
    raw.trim().to_lowercase()
}

/// The pass condition: what follows the arrow, else what follows the command.
fn accept_condition(accept: &str) -> &str {
    if let Some((_, cond)) = accept.split_once('→').or_else(|| accept.split_once("->")) {
        return cond;
    }
    accept.splitn(3, '`').nth(2).unwrap_or_default()
}

/// L3: the command chains or pipes outside quotes, or a second backticked span is a command.
fn is_compound(accept: &str, command: &str) -> bool {
    if chains_unquoted(command) {
        return true;
    }
    accept.split('`').skip(1).step_by(2).skip(1).any(|span| {
        span.split_whitespace()
            .next()
            .is_some_and(|w| COMMAND_STARTS.contains(&w.to_lowercase().as_str()))
            && span.trim().contains(char::is_whitespace)
    })
}

fn chains_unquoted(command: &str) -> bool {
    let mut quote: Option<char> = None;
    let mut prev = ' ';
    for ch in command.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(ch),
            (None, ';' | '|') => return true,
            (None, '&') if prev == '&' => return true,
            _ => {}
        }
        prev = ch;
    }
    false
}

fn is_test_runner(command: &str) -> bool {
    let words: Vec<&str> = command.split_whitespace().collect();
    TEST_RUNNERS
        .iter()
        .any(|runner| words.len() >= runner.len() && words[..runner.len()] == **runner)
}

/// A number tied to passing tests (`3 passed`, `≥8 tests`, `at least 2`), not an exit code.
fn has_count(condition: &str) -> bool {
    let spaced = condition
        .to_lowercase()
        .replace('≥', " >= ")
        .replace(">=", " >= ")
        .replace([',', '(', ')', ';', '`'], " ");
    let toks: Vec<&str> = spaced.split_whitespace().collect();
    toks.iter().enumerate().any(|(i, tok)| {
        let digits = tok.trim_end_matches(|c: char| !c.is_ascii_digit());
        if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        let before = i.checked_sub(1).map(|j| toks[j]).unwrap_or_default();
        let after = toks.get(i + 1).copied().unwrap_or_default();
        matches!(before, ">=" | ">" | "least")
            || ["pass", "test", "case", "ok"]
                .iter()
                .any(|p| after.starts_with(p))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(subject: &str, fields: &[(&str, &str)]) -> Item {
        let mut it = Item::new("T1", subject);
        for (k, v) in fields {
            it.fields.insert((*k).to_string(), (*v).to_string());
        }
        it
    }

    #[test]
    fn a_count_is_a_number_tied_to_passing_tests_not_the_exit_code() {
        assert!(!has_count(" exit 0"));
        assert!(!has_count(" exit 0, 0 failed"));
        assert!(has_count(" exit 0, ≥8 passed"));
        assert!(has_count(" exit 0 and 3 tests pass"));
        assert!(has_count(" at least 2 (see log)"));
        assert!(has_count(" exit 0, >=4 passed"));
    }

    #[test]
    fn a_pipe_or_chain_inside_quotes_is_data() {
        assert!(chains_unquoted("cargo test a && cargo test b"));
        assert!(chains_unquoted("cargo test | tail -1"));
        assert!(chains_unquoted("make; make test"));
        assert!(!chains_unquoted("grep -e 'a|b' x"));
        assert!(!chains_unquoted("grep -e \"a;b\" x 2>&1"));
    }

    #[test]
    fn test_runners_are_matched_by_leading_words() {
        assert!(is_test_runner("cargo test --quiet x"));
        assert!(is_test_runner("python -m pytest tests/x.py"));
        assert!(is_test_runner("npx vitest run"));
        assert!(!is_test_runner("cargo build"));
        assert!(!is_test_runner("grep -c test x"));
    }

    #[test]
    fn roots_skip_tests_and_repo_root_files() {
        let paths = |ps: &[&str]| ps.iter().map(|p| (*p).to_string()).collect::<Vec<_>>();
        assert_eq!(
            roots(&paths(&[
                "src/a.rs",
                "tests/a.rs",
                "Cargo.toml",
                "./src/b.rs"
            ])),
            ["src"]
        );
        assert_eq!(roots(&paths(&["src/a.rs", "docs/a.md"])), ["docs", "src"]);
        assert!(is_test_path("web/src/__tests__/x.ts"));
        assert!(is_test_path("pkg/x_test.go"));
        assert!(!is_test_path("src/testing.rs"));
    }

    #[test]
    fn two_subjects_reads_and_and_a_second_verb_only() {
        assert!(two_subjects("add the parser and the writer"));
        assert!(two_subjects("add the parser, then wire it"));
        assert!(two_subjects("add the parser, wire the route"));
        assert!(!two_subjects("parse ids, aliases, dependencies"));
        assert!(!two_subjects(&unticked("add `a and b`").to_lowercase()));
    }

    #[test]
    fn the_floor_drops_split_notes_but_keeps_the_rest() {
        let t = task(
            "Fix the reader and the writer",
            &[
                ("touches", "`src/a.rs`"),
                ("reads", "a, b, c, d, e, f"),
                ("accept", "`cargo test x` → exit 0"),
            ],
        );
        let floored = leaf_findings(&t, true);
        assert_eq!(floored, [NO_COUNT, NO_RED], "{floored:?}");
        let full = leaf_findings(&t, false);
        assert!(full.contains(&SPLIT_ONE_COMMIT.to_string()), "{full:?}");
        assert!(full.iter().any(|m| m.starts_with("justify:")), "{full:?}");
    }

    #[test]
    fn a_second_backticked_command_is_compound_but_a_value_is_not() {
        let accept = "`cargo test a` → 2 passed, then `cargo clippy --all` → exit 0";
        assert!(is_compound(accept, &accept_command(accept)));
        let accept = "`cargo test a` → prints `ok`, 2 passed";
        assert!(!is_compound(accept, &accept_command(accept)));
    }
}
