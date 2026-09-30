//! G7 scope fence, G8 dependency repair, G9 executable task, G10 kill wiring, G11 shape and caps.

use std::collections::BTreeSet;

use super::claims::whole_word_at;
use super::{EvidenceTurn, GateInputs, GateReport};
use crate::concepts::build_plan::plan::{refs_of, render, BuildPlan, Item, Provenance};
use crate::domain::evidence::{locate, normalize_for_match};

const MAX_TASKS: usize = 15;
const MAX_SETTLED: usize = 12;
const MAX_RENDERED_BYTES: usize = 12 * 1024;

const READ_ONLY_VERBS: &[&str] = &[
    "grep",
    "rg",
    "sed -n",
    "ls",
    "cat",
    "head",
    "tail",
    "test",
    "find",
    "wc",
    "git log",
    "git show",
    "git diff",
    "cargo test",
    "diff",
    "jq",
];

const DESTRUCTIVE_STARTS: &[&str] = &["rm", "sudo", "dd"];

const DESTRUCTIVE_PAIRS: &[(&str, &str)] = &[
    ("git", "push"),
    ("git", "reset"),
    ("down", "-v"),
    ("drop", "table"),
];

const OWNER_WORK: &[&str] = &[
    "hand-label",
    "owner fills",
    "manually",
    "on the owner's host",
    "you fill",
];

// Matched as whole words (ADR-0032), so each inflection a stem used to cover is listed in full.
const GATE_LANGUAGE: &[&str] = &[
    "must not be built",
    "only if",
    "precondition",
    "preconditions",
    "kill criterion",
    "kill criteria",
    "before any",
];

/// Words of context either side of a G10 match in the question it opens.
const WINDOW_BEFORE: usize = 8;
const WINDOW_AFTER: usize = 14;

const CONTINUE_ANYWAY: &[&str] = &["continue anyway", "proceed anyway", "keep going"];

const GATE_MARKER: &str = "gate language without a kill row";

pub fn apply(plan: &mut BuildPlan, inputs: &GateInputs, report: &mut GateReport) {
    scope_fence(plan, report);
    dependency_repair(plan, report);
    executable_tasks(plan, report);
    kill_wiring(plan, inputs, report);
    shape_and_caps(plan, report);
}

fn mark(item: &mut Item, marker: impl Into<String>) {
    let marker = marker.into();
    if !item.markers.contains(&marker) {
        item.markers.push(marker);
    }
}

fn need_owner(item: &mut Item, report: &mut GateReport) {
    if !item.needs_owner {
        item.needs_owner = true;
        report.count("needs_owner");
    }
}

fn next_open_id(plan: &BuildPlan) -> String {
    let max = plan
        .open
        .iter()
        .filter_map(|i| i.id.strip_prefix('Q')?.parse::<usize>().ok())
        .max()
        .unwrap_or(0);
    format!("Q{}", max + 1)
}

fn ascii_quotes(text: &str) -> String {
    text.replace(['\u{2019}', '\u{2018}'], "'")
}

fn norm_path(p: &str) -> String {
    let mut p = p.trim();
    if let Some(head) = p.strip_suffix("(new)") {
        p = head.trim_end();
    }
    if let Some((head, tail)) = p.rsplit_once(':') {
        let anchor = tail.split('-').count() <= 2
            && tail
                .split('-')
                .all(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        if anchor {
            p = head;
        }
    }
    let p = p
        .strip_suffix("/**")
        .or_else(|| p.strip_suffix("/*"))
        .or_else(|| p.strip_suffix('*'))
        .unwrap_or(p);
    p.trim_start_matches("./").trim_end_matches('/').to_string()
}

fn paths_overlap(a: &str, b: &str) -> bool {
    let (a, b) = (norm_path(a), norm_path(b));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let under = |long: &str, short: &str| {
        long.strip_prefix(short)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    };
    under(&a, &b) || under(&b, &a)
}

fn backticked(text: &str) -> Vec<String> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && !s.contains(char::is_whitespace))
        .collect()
}

fn fenced_paths(plan: &BuildPlan) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in &plan.fence {
        let mut spans = backticked(&item.text);
        for v in item.fields.values() {
            spans.extend(backticked(v));
        }
        if spans.is_empty() && !item.text.contains(char::is_whitespace) {
            spans.push(item.text.trim().to_string());
        }
        out.extend(spans);
    }
    for item in &plan.settled {
        let t = ascii_quotes(&item.text.to_lowercase());
        let fencing = [
            "do not touch",
            "do not modify",
            "do not edit",
            "don't touch",
            "don't modify",
            "don't edit",
            "out of scope",
        ]
        .iter()
        .any(|p| t.contains(p))
            || (t.contains("leave") && t.contains("unchanged"));
        if fencing {
            out.extend(
                backticked(&item.text)
                    .into_iter()
                    .filter(|s| s.contains('/') || s.contains('.')),
            );
        }
    }
    let mut seen = BTreeSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

fn scope_fence(plan: &mut BuildPlan, report: &mut GateReport) {
    let fenced = fenced_paths(plan);
    if fenced.is_empty() {
        return;
    }
    let mut questions = Vec::new();
    for task in &mut plan.tasks {
        for touched in task.list("touches") {
            for path in &fenced {
                if !paths_overlap(&touched, path) {
                    continue;
                }
                let marker = format!("touches fenced {path}");
                if task.markers.contains(&marker) {
                    continue;
                }
                mark(task, marker);
                need_owner(task, report);
                questions.push(format!("{} touches fenced `{path}` — which wins?", task.id));
            }
        }
    }
    // A re-gated plan (docs/adr/0032) already holds the question it asked the first time.
    questions.retain(|text| !plan.open.iter().any(|q| &q.text == text));
    for text in questions {
        let mut q = Item::new("", &text);
        q.id = next_open_id(plan);
        plan.open.push(q);
    }
}

fn task_deps(task: &Item) -> Vec<String> {
    task.depends_tasks()
}

// A self-edge is not a cycle: the task-graph gate drops it as a repair.
fn depends_of(plan: &BuildPlan, id: &str) -> Vec<String> {
    plan.tasks
        .iter()
        .find(|t| t.id == id)
        .map(task_deps)
        .unwrap_or_default()
        .into_iter()
        .filter(|dep| dep != id)
        .collect()
}

fn reaches(plan: &BuildPlan, from: &str, to: &str) -> bool {
    let mut seen = BTreeSet::new();
    let mut stack = depends_of(plan, from);
    while let Some(id) = stack.pop() {
        if id == to {
            return true;
        }
        if seen.insert(id.clone()) {
            stack.extend(depends_of(plan, &id));
        }
    }
    false
}

fn write_depends(task: &mut Item, refs: &[String]) {
    let mut refs = refs.to_vec();
    refs.extend(task.depends_free());
    if refs.is_empty() {
        task.fields.remove("depends");
    } else {
        task.fields.insert("depends".to_string(), refs.join(", "));
    }
}

fn set_depends(task: &mut Item, deps: &[String]) {
    let mut refs = deps.to_vec();
    refs.extend(task.depends_premises());
    refs.extend(task.depends_questions());
    write_depends(task, &refs);
}

fn add_dependency(task: &mut Item, dep: &str) {
    let mut deps = task_deps(task);
    deps.push(dep.to_string());
    set_depends(task, &deps);
}

fn dependency_repair(plan: &mut BuildPlan, report: &mut GateReport) {
    let ids = |items: &[Item]| -> BTreeSet<String> { items.iter().map(|t| t.id.clone()).collect() };
    let (known, premises, questions) = (ids(&plan.tasks), ids(&plan.verify), ids(&plan.open));
    for task in &mut plan.tasks {
        let refs: Vec<(String, bool)> = task_deps(task)
            .into_iter()
            .map(|d| (d.clone(), known.contains(&d)))
            .chain(
                task.depends_premises()
                    .into_iter()
                    .map(|d| (d.clone(), premises.contains(&d))),
            )
            .chain(
                task.depends_questions()
                    .into_iter()
                    .map(|d| (d.clone(), questions.contains(&d))),
            )
            .collect();
        if refs.iter().all(|(_, ok)| *ok) {
            continue;
        }
        for (dropped, _) in refs.iter().filter(|(_, ok)| !ok) {
            mark(task, format!("unknown dependency {dropped}"));
            report.count("repaired");
        }
        let kept: Vec<String> = refs
            .into_iter()
            .filter(|(_, ok)| *ok)
            .map(|(d, _)| d)
            .collect();
        write_depends(task, &kept);
    }

    for task in &mut plan.tasks {
        for q in task.depends_questions() {
            if questions.contains(&q) {
                mark(task, format!("blocked by {q}"));
                need_owner(task, report);
            }
        }
    }

    let cyclic: Vec<bool> = plan
        .tasks
        .iter()
        .map(|t| reaches(plan, &t.id, &t.id))
        .collect();
    for (task, cyclic) in plan.tasks.iter_mut().zip(&cyclic) {
        if *cyclic {
            mark(task, "dependency cycle");
            need_owner(task, report);
        }
    }

    for later in 1..plan.tasks.len() {
        for earlier in (0..later).rev() {
            let (a, b) = (plan.tasks[earlier].clone(), plan.tasks[later].clone());
            let b_touches = b.list("touches");
            let shared = a
                .list("touches")
                .into_iter()
                .find(|x| b_touches.iter().any(|y| paths_overlap(x, y)));
            let Some(path) = shared else { continue };
            if reaches(plan, &b.id, &a.id) || reaches(plan, &a.id, &b.id) {
                continue;
            }
            let task = &mut plan.tasks[later];
            add_dependency(task, &a.id);
            mark(task, format!("added: shares {path} with {}", a.id));
            report.count("repaired");
        }
    }
}

fn segments(text: &str) -> Vec<(bool, Vec<String>)> {
    let lowered: String = text
        .to_lowercase()
        .chars()
        .map(|c| match c {
            ';' | '&' | '\n' => " ; ".to_string(),
            '|' => " | ".to_string(),
            '`' | '"' | '\'' | '(' | ')' => " ".to_string(),
            c => c.to_string(),
        })
        .collect();
    let mut out: Vec<(bool, Vec<String>)> = vec![(false, Vec::new())];
    for tok in lowered.split_whitespace() {
        let after_shell = out
            .last()
            .and_then(|(_, t)| t.first())
            .is_some_and(|first| matches!(first.as_str(), "sh" | "bash" | "zsh" | "dash"));
        match tok {
            ";" => out.push((false, Vec::new())),
            "-c" if after_shell => out.push((false, Vec::new())),
            "|" => out.push((true, Vec::new())),
            t => out.last_mut().expect("seeded").1.push(t.to_string()),
        }
    }
    out.retain(|(_, toks)| !toks.is_empty());
    out
}

// Segment-start matching cannot see wrappers such as `xargs rm` or `env rm`.
fn is_destructive(text: &str) -> bool {
    segments(text).iter().any(|(piped, toks)| {
        let first = toks[0].as_str();
        DESTRUCTIVE_STARTS.contains(&first)
            || (*piped && matches!(first, "sh" | "bash"))
            || toks
                .windows(2)
                .any(|w| DESTRUCTIVE_PAIRS.contains(&(w[0].as_str(), w[1].as_str())))
    })
}

fn read_only(check: &str) -> bool {
    let c = check
        .trim()
        .trim_matches('`')
        .trim()
        .to_lowercase()
        .replace("2>&1", "");
    if c.contains("-delete") || c.contains("-exec") {
        return false;
    }
    let Some(segs) = unquoted_segments(&c) else {
        return false;
    };
    segs.iter()
        .map(|seg| seg.trim().trim_matches('`').trim())
        .filter(|seg| !seg.is_empty())
        .all(|seg| {
            READ_ONLY_VERBS.iter().any(|v| {
                seg.strip_prefix(v)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
            })
        })
}

// Separators and `>` inside quotes are data; an unquoted `>`, an unclosed quote, or a command
// substitution outside single quotes makes the check not read-only.
fn unquoted_segments(command: &str) -> Option<Vec<String>> {
    let mut out = vec![String::new()];
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (Some('\''), '\'') => quote = None,
            (Some('\''), _) => {}
            (_, '\\') => {
                out.last_mut().expect("seeded").push(ch);
                if let Some(next) = chars.next() {
                    out.last_mut().expect("seeded").push(next);
                }
                continue;
            }
            // Command substitution still runs outside single quotes.
            (_, '`') => return None,
            (_, '$') if chars.peek() == Some(&'(') => return None,
            (Some(q), c) if c == q => quote = None,
            (None, '\'' | '"') => quote = Some(ch),
            (None, '>') => return None,
            (None, ';' | '|' | '&' | '\n') => {
                out.push(String::new());
                continue;
            }
            _ => {}
        }
        out.last_mut().expect("seeded").push(ch);
    }
    quote.is_none().then_some(out)
}

fn runnable_accept(accept: &str) -> bool {
    let Some(rest) = accept.trim().strip_prefix('`') else {
        return false;
    };
    let Some((cmd, rest)) = rest.split_once('`') else {
        return false;
    };
    let condition = rest
        .find('→')
        .map(|at| &rest[at + '→'.len_utf8()..])
        .or_else(|| rest.find("->").map(|at| &rest[at + 2..]));
    !cmd.trim().is_empty() && condition.is_some_and(|c| !c.trim().is_empty())
}

const RUNNERS: &[&str] = &[
    "cargo",
    "npm",
    "pnpm",
    "yarn",
    "pytest",
    "go",
    "make",
    "grep",
    "rg",
    "bash",
    "sh",
    "python",
    "node",
    "just",
    "docker compose",
];

const GO_SUBCOMMANDS: &[&str] = &[
    "test", "build", "run", "vet", "fmt", "mod", "generate", "install", "get", "list",
];

const MAKE_TARGETS: &[&str] = &[
    "test", "check", "build", "lint", "fmt", "all", "clean", "ci", "install", "run", "dev",
];

const PROSE_WORDS: &[&str] = &[
    "the", "is", "are", "should", "then", "that", "it", "this", "you", "your",
];

const COMMAND_WORDS: usize = 12;

// Quote-aware scan: the byte offset of the first `→`/`->` outside quotes, or None when there is
// none or a quote is left open.
fn unquoted_arrow(text: &str) -> Option<(usize, usize)> {
    let mut quote: Option<char> = None;
    for (at, ch) in text.char_indices() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(ch),
            (None, '→') => return Some((at, '→'.len_utf8())),
            (None, '-') if text[at..].starts_with("->") => return Some((at, 2)),
            _ => {}
        }
    }
    None
}

fn without_quoted(text: &str) -> String {
    let mut quote: Option<char> = None;
    let mut out = String::new();
    for ch in text.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(ch),
            _ => out.push(ch),
        }
    }
    out
}

fn target_shaped(token: &str) -> bool {
    token.starts_with('-') || token.contains(['-', '_', '.', '/', ':', '='])
}

// A word closed by sentence punctuation after a letter or digit; `./...` and `--` are paths.
fn sentence_end(token: &str) -> bool {
    let mut rev = token.chars().rev();
    rev.next()
        .is_some_and(|c| matches!(c, '.' | '!' | '?' | ',' | ';'))
        && rev.next().is_some_and(char::is_alphanumeric)
}

fn command_shaped(command: &str, runner: &str) -> bool {
    let bare = without_quoted(command);
    let tokens: Vec<&str> = bare.split_whitespace().collect();
    if command.split_whitespace().count() > COMMAND_WORDS
        || tokens.iter().any(|t| sentence_end(t))
        || tokens
            .iter()
            .any(|t| PROSE_WORDS.contains(&t.to_lowercase().as_str()))
    {
        return false;
    }
    let subs = match runner {
        "go" => GO_SUBCOMMANDS,
        "make" | "just" => MAKE_TARGETS,
        _ => return true,
    };
    let mut rest = tokens.iter().skip(runner.split_whitespace().count());
    let first_ok = rest
        .next()
        .is_some_and(|t| subs.contains(t) || target_shaped(t));
    first_ok
        && rest
            .all(|t| target_shaped(t) || t.chars().any(|c| c.is_uppercase() || c.is_ascii_digit()))
}

/// An accept that opens with a known runner command and carries `→`/`->` (outside quotes) and a
/// condition, but no backticks, as the backticked form. Anything that is not command-shaped, and
/// any destructive command, is left for the runnable-accept and destructive gates.
fn repaired_accept(accept: &str) -> Option<String> {
    let text = accept.trim();
    if text.contains('`') || is_destructive(text) {
        return None;
    }
    let runner = RUNNERS.iter().find(|r| {
        text.strip_prefix(**r)
            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
    })?;
    let (at, width) = unquoted_arrow(text)?;
    let (command, condition) = (text[..at].trim(), text[at + width..].trim());
    if condition.is_empty()
        || command[runner.len()..].trim().is_empty()
        || !command_shaped(command, runner)
    {
        return None;
    }
    let written = &text[at..at + width];
    Some(format!("`{command}` {written} {condition}"))
}

fn executable_tasks(plan: &mut BuildPlan, report: &mut GateReport) {
    for task in &mut plan.tasks {
        if let Some(fixed) = task.field("accept").and_then(repaired_accept) {
            task.fields.insert("accept".to_string(), fixed);
            mark(task, "accept repaired");
            report.count("repaired");
        }
        if !task.field("accept").is_some_and(runnable_accept) {
            mark(task, "no runnable accept");
            need_owner(task, report);
        }
        let commands = format!(
            "{} ; {}",
            task.field("accept").unwrap_or_default(),
            task.field("check").unwrap_or_default()
        );
        if is_destructive(&commands) {
            mark(task, "destructive command");
            need_owner(task, report);
        }
        let everything = ascii_quotes(
            &format!(
                "{} {}",
                task.text,
                task.fields.values().cloned().collect::<Vec<_>>().join(" ")
            )
            .to_lowercase(),
        );
        // A task the owner answered on the plan workbench (docs/adr/0032) no longer waits on
        // them, however its text reads.
        let answered = task.fields.contains_key("unblocked");
        if !answered && OWNER_WORK.iter().any(|p| everything.contains(p)) {
            mark(task, "needs you");
            need_owner(task, report);
        }
    }
    for item in &mut plan.verify {
        let Some(check) = item.field("check").map(str::to_string) else {
            continue;
        };
        if is_destructive(&check) {
            mark(item, "destructive command");
            need_owner(item, report);
        } else if !read_only(&check) {
            mark(item, "check is not read-only");
        }
    }
}

fn task_refs(text: &str) -> Vec<String> {
    refs_of(text, 'T')
}

fn kill_wiring(plan: &mut BuildPlan, inputs: &GateInputs, report: &mut GateReport) {
    for k in 0..plan.kills.len() {
        let kill = plan.kills[k].clone();
        let text = format!(
            "{} {}",
            kill.text,
            kill.fields.values().cloned().collect::<Vec<_>>().join(" ")
        )
        .to_lowercase();
        let checkers = kill.field("checked by").map(task_refs).unwrap_or_default();
        let gated = kill.field("gates").map(task_refs).unwrap_or_default();
        let mut missing = Vec::new();
        if checkers.is_empty() {
            missing.push("checked by");
        }
        if gated.is_empty() {
            missing.push("gates");
        }
        if !missing.is_empty() {
            mark(
                &mut plan.kills[k],
                format!("incomplete kill wiring: {}", missing.join(", ")),
            );
        }
        if CONTINUE_ANYWAY.iter().any(|p| text.contains(p)) {
            mark(&mut plan.kills[k], "kill criterion says continue anyway");
            report.note(format!("{}: kill criterion says continue anyway", kill.id));
        }
        for id in checkers.iter().chain(&gated) {
            if !plan.tasks.iter().any(|t| &t.id == id) {
                mark(
                    &mut plan.kills[k],
                    format!("incomplete kill wiring: unknown {id}"),
                );
            }
        }
        for gate in &gated {
            for checker in &checkers {
                let both = plan.tasks.iter().any(|t| &t.id == gate)
                    && plan.tasks.iter().any(|t| &t.id == checker);
                if !both || gate == checker || reaches(plan, gate, checker) {
                    continue;
                }
                if reaches(plan, checker, gate) {
                    mark(
                        &mut plan.kills[k],
                        format!("incomplete kill wiring: {checker} depends on {gate}"),
                    );
                    continue;
                }
                if let Some(task) = plan.tasks.iter_mut().find(|t| &t.id == gate) {
                    add_dependency(task, checker);
                    mark(task, format!("added: {} gates {gate}", kill.id));
                    report.count("repaired");
                }
            }
        }
    }

    let already_asked = plan
        .open
        .iter()
        .any(|q| q.markers.iter().any(|m| m == GATE_MARKER));
    if !plan.kills.is_empty() || already_asked {
        return;
    }
    // Only what the owner said (or wrote in the idea) is a gate the owner set; a foil turn
    // musing "only if" is not a requirement the plan owes a kill row (ADR-0030, ADR-0032).
    // An owner answer to an earlier G10 question settles exactly what that question quoted and
    // the answer turn itself; gate language the question never named is still asked about
    // (ADR-0030 §G10).
    let settled = G10Settled::from(inputs);
    let owner_turns = inputs
        .evidence
        .turns()
        .iter()
        .filter(|t| matches!(t.speaker, Provenance::Owner | Provenance::Idea));
    for turn in owner_turns {
        let text = turn_body(turn);
        if settled.is_answer_turn(text) {
            continue;
        }
        let mut answered = answered_spans(plan, text);
        answered.extend(settled.window_spans(text));
        let Some(at) = GATE_LANGUAGE
            .iter()
            .filter_map(|p| gate_phrase_at(text, p, &answered))
            .min()
        else {
            continue;
        };
        let window = word_window(text, at);
        plan.open_from(
            Item::new(
                "",
                &format!("gate language without a kill row: \"{window}\""),
            ),
            GATE_MARKER,
        );
        break;
    }
}

/// A turn's normalized text without its `## user` heading, so the quoted window is only what the
/// owner wrote. The idea statement has no heading and is returned whole.
fn turn_body(turn: &EvidenceTurn) -> &str {
    let text = turn.normalized.as_str();
    match turn.speaker {
        Provenance::Owner => turn
            .raw
            .lines()
            .next()
            .map(normalize_for_match)
            .and_then(|heading| text.strip_prefix(heading.as_str()))
            .map_or(text, str::trim_start),
        Provenance::Idea | Provenance::Foil => text,
    }
}

/// Byte ranges of `text` (a normalized turn body) that an Owner-provenance Settled item quotes. Gate
/// language there is already the owner's settled answer, so asking about it again would re-open
/// what the owner closed (ADR-0032).
fn answered_spans(plan: &BuildPlan, text: &str) -> Vec<(usize, usize)> {
    plan.settled
        .iter()
        .filter(|s| s.provenance == Some(Provenance::Owner))
        .filter_map(|s| {
            let quote = s.field("quote").unwrap_or(s.text.as_str());
            let start = locate(quote, text)?;
            Some((start, start + normalize_for_match(quote).len()))
        })
        .collect()
}

/// What the owner's answers to earlier G10 questions settled. A G10 question's `asked` is the
/// code-generated `gate language without a kill row: "<window>"` (possibly behind the `proposed:`
/// prefix it was rendered with), so the window it named is read back from it verbatim.
struct G10Settled {
    /// The normalized windows the answered questions quoted.
    windows: Vec<String>,
    /// The normalized `re <qid> (<stem>):` opening of each answer turn.
    answer_heads: Vec<String>,
}

impl G10Settled {
    fn from(inputs: &GateInputs) -> Self {
        let mut settled = G10Settled {
            windows: Vec::new(),
            answer_heads: Vec::new(),
        };
        for a in inputs.answered {
            let asked = a.asked.strip_prefix("proposed:").unwrap_or(&a.asked);
            let Some(rest) = asked.trim_start().strip_prefix(GATE_MARKER) else {
                continue;
            };
            let window = rest.trim_start_matches([':', ' ']).trim();
            let window = window.strip_prefix('"').unwrap_or(window);
            let window = window.strip_suffix('"').unwrap_or(window);
            let window = normalize_for_match(window);
            if !window.is_empty() {
                settled.windows.push(window);
            }
            settled.answer_heads.push(normalize_for_match(&format!(
                "Re {} ({}):",
                a.qid, a.in_stem
            )));
        }
        settled
    }

    /// True for the owner turn that answers a G10 question: its gate language is the answer.
    fn is_answer_turn(&self, body: &str) -> bool {
        self.answer_heads
            .iter()
            .any(|h| body.starts_with(h.as_str()))
    }

    /// Every place in `body` (a normalized turn body) where an answered question's window occurs:
    /// the turn the question was asked about, and any later turn that pastes the window back.
    fn window_spans<'a>(&'a self, body: &'a str) -> impl Iterator<Item = (usize, usize)> + 'a {
        self.windows.iter().flat_map(move |w| {
            body.match_indices(w.as_str())
                .map(|(at, _)| (at, at + w.len()))
        })
    }
}

/// The first whole-word occurrence of `phrase` in `text` outside every `answered` span and not
/// wrapped in a pair of the same quote mark. Whole words, so "commonly if" and "monopoly if" never
/// read as "only if"; a quoted bare phrase (`"only if"`, `'kill criteria'`) is the owner naming
/// the phrase, not setting a gate. `text` is normalized, so curly quotes are already straight.
fn gate_phrase_at(text: &str, phrase: &str, answered: &[(usize, usize)]) -> Option<usize> {
    let mentioned = |at: usize| {
        let before = text[..at].chars().next_back();
        let after = text[at + phrase.len()..].chars().next();
        matches!(
            (before, after),
            (Some('"'), Some('"')) | (Some('\''), Some('\''))
        )
    };
    text.match_indices(phrase)
        .map(|(at, _)| at)
        .filter(|&at| whole_word_at(text, at, phrase.len()))
        .filter(|&at| !mentioned(at))
        .find(|&at| !answered.iter().any(|&(s, e)| at >= s && at < e))
}

/// The words around the match at `at`: up to [`WINDOW_BEFORE`] whole words before the word the
/// match starts in and [`WINDOW_AFTER`] from it, so the quoted window never cuts a word in half.
fn word_window(text: &str, at: usize) -> String {
    let word_start = text[..at].rfind(char::is_whitespace).map_or(0, |i| i + 1);
    let before: Vec<&str> = text[..word_start].split_whitespace().collect();
    let before = &before[before.len().saturating_sub(WINDOW_BEFORE)..];
    before
        .iter()
        .copied()
        .chain(text[word_start..].split_whitespace().take(WINDOW_AFTER))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shape_and_caps(plan: &BuildPlan, report: &mut GateReport) {
    for name in &plan.missing {
        report.note(format!("missing section: {name} — placeholder"));
    }
    if plan.tasks.len() > MAX_TASKS
        || plan.settled.len() > MAX_SETTLED
        || render(plan).len() > MAX_RENDERED_BYTES
    {
        report.note("too large: split into phases");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::sources::SourceProbe;
    use crate::concepts::build_plan::gates::{Answered, Evidence};
    use crate::concepts::build_plan::plan::Item;

    fn item(id: &str, text: &str, fields: &[(&str, &str)]) -> Item {
        let mut it = Item::new(id, text);
        for (k, v) in fields {
            it.fields.insert((*k).to_string(), (*v).to_string());
        }
        it
    }

    fn run_with(plan: &mut BuildPlan, conversation: &str) -> GateReport {
        run_with_answered(plan, conversation, &[])
    }

    fn run_with_answered(
        plan: &mut BuildPlan,
        conversation: &str,
        answered: &[Answered],
    ) -> GateReport {
        let evidence = Evidence::new("", conversation);
        let probe = SourceProbe::default();
        let inputs = GateInputs {
            evidence: &evidence,
            open_artifact: None,
            audit: None,
            probe: &probe,
            answered,
        };
        let mut report = GateReport::default();
        apply(plan, &inputs, &mut report);
        report
    }

    fn run(plan: &mut BuildPlan) -> GateReport {
        run_with(plan, "")
    }

    fn has_marker(it: &Item, marker: &str) -> bool {
        it.markers.iter().any(|m| m == marker)
    }

    const RUNNABLE: &str = "`cargo test x` → exit 0";

    #[test]
    fn g7_a_task_touching_a_fenced_path_needs_the_owner() {
        let mut plan = BuildPlan::default();
        plan.fence
            .push(item("F1", "`src/domain/links.rs` is frozen", &[]));
        plan.settled
            .push(item("S1", "Do not touch `templates/idea.html`.", &[]));
        plan.tasks.push(item(
            "T1",
            "Edit links",
            &[("touches", "`src/domain/links.rs`"), ("accept", RUNNABLE)],
        ));
        plan.tasks.push(item(
            "T2",
            "Edit templates",
            &[("touches", "templates/"), ("accept", RUNNABLE)],
        ));
        plan.tasks.push(item(
            "T3",
            "Edit sibling",
            &[
                ("touches", "src/domain/links_extra.rs"),
                ("accept", RUNNABLE),
            ],
        ));
        let report = run(&mut plan);
        assert!(plan.tasks[0].needs_owner);
        assert!(has_marker(
            &plan.tasks[0],
            "touches fenced src/domain/links.rs"
        ));
        assert!(plan.tasks[1].needs_owner);
        assert!(!plan.tasks[2].needs_owner, "segment-aware prefix match");
        assert_eq!(
            plan.open[0].text,
            "T1 touches fenced `src/domain/links.rs` — which wins?"
        );
        assert_eq!(plan.open.len(), 2);
        assert_eq!(report.tally.get("needs_owner"), Some(&2));
    }

    #[test]
    fn g8_shared_touches_get_a_dependency() {
        let mut plan = BuildPlan::default();
        plan.tasks.push(item(
            "T1",
            "First",
            &[("touches", "src/x.rs"), ("accept", RUNNABLE)],
        ));
        plan.tasks.push(item(
            "T2",
            "Second",
            &[("touches", "src/"), ("depends", "T9"), ("accept", RUNNABLE)],
        ));
        let report = run(&mut plan);
        assert_eq!(plan.tasks[1].list("depends"), ["T1"]);
        assert!(has_marker(&plan.tasks[1], "added: shares src/x.rs with T1"));
        assert!(has_marker(&plan.tasks[1], "unknown dependency T9"));
        assert_eq!(report.tally.get("repaired"), Some(&2));
    }

    #[test]
    fn g8_premise_and_question_refs_are_not_dropped() {
        let mut plan = BuildPlan::default();
        plan.verify.push(item("P1", "Scaler exists", &[]));
        plan.tasks.push(item(
            "T1",
            "First",
            &[("depends", "P1, P9, Q9"), ("accept", RUNNABLE)],
        ));
        plan.tasks.push(item(
            "T2",
            "Second",
            &[("depends", "T1, P1"), ("accept", RUNNABLE)],
        ));
        run(&mut plan);
        assert_eq!(plan.tasks[0].depends_premises(), ["P1"]);
        assert!(plan.tasks[0].depends_questions().is_empty());
        assert!(has_marker(&plan.tasks[0], "unknown dependency P9"));
        assert!(has_marker(&plan.tasks[0], "unknown dependency Q9"));
        assert!(!has_marker(&plan.tasks[0], "unknown dependency P1"));
        assert_eq!(plan.tasks[1].depends_tasks(), ["T1"]);
        assert_eq!(plan.tasks[1].depends_premises(), ["P1"]);
        assert!(!plan.tasks[1]
            .markers
            .iter()
            .any(|m| m.starts_with("unknown")));
    }

    #[test]
    fn g8_an_edge_added_beside_a_premise_keeps_the_premise() {
        let mut plan = BuildPlan::default();
        plan.verify.push(item("P1", "Scaler exists", &[]));
        plan.tasks.push(item(
            "T1",
            "First",
            &[("touches", "src/x.rs"), ("accept", RUNNABLE)],
        ));
        plan.tasks.push(item(
            "T2",
            "Second",
            &[
                ("touches", "src/x.rs"),
                ("depends", "P1"),
                ("accept", RUNNABLE),
            ],
        ));
        run(&mut plan);
        assert_eq!(plan.tasks[1].depends_tasks(), ["T1"]);
        assert_eq!(plan.tasks[1].depends_premises(), ["P1"]);
        assert_eq!(plan.tasks[1].field("depends"), Some("T1, P1"));
    }

    #[test]
    fn g8_free_text_in_depends_is_not_an_id_and_survives_a_rewrite() {
        let mut plan = BuildPlan::default();
        plan.tasks.push(item("T1", "A", &[("accept", RUNNABLE)]));
        plan.tasks.push(item(
            "T2",
            "B",
            &[
                ("depends", "T1, P95 latency check, Q4 planning, T9"),
                ("accept", RUNNABLE),
            ],
        ));
        run(&mut plan);
        let t2 = &plan.tasks[1];
        assert!(has_marker(t2, "unknown dependency T9"));
        assert!(
            !has_marker(t2, "unknown dependency P95"),
            "{:?}",
            t2.markers
        );
        assert!(!has_marker(t2, "unknown dependency Q4"), "{:?}", t2.markers);
        assert!(!t2.needs_owner, "{t2:?}");
        assert_eq!(
            t2.field("depends"),
            Some("T1, P95 latency check, Q4 planning")
        );
    }

    #[test]
    fn g8_an_annotated_question_id_still_blocks_the_task() {
        let mut plan = BuildPlan::default();
        let mut q = Item::new("Q1", "Which spread?");
        q.id = "Q1".to_string();
        plan.open.push(q);
        for (n, entry) in ["Q1 (which spread)", "Q1 — which spread", "Q1: which spread"]
            .into_iter()
            .enumerate()
        {
            plan.tasks.push(item(
                &format!("T{}", n + 1),
                "A",
                &[("depends", entry), ("accept", RUNNABLE)],
            ));
        }
        run(&mut plan);
        for t in &plan.tasks {
            assert!(has_marker(t, "blocked by Q1"), "{:?}", t.markers);
            assert!(t.needs_owner, "{t:?}");
            assert!(!has_marker(t, "unknown dependency"), "{:?}", t.markers);
        }
    }

    #[test]
    fn g8_a_task_depending_on_an_open_question_is_blocked() {
        let mut plan = BuildPlan::default();
        plan.open.push(item("Q1", "Which spread?", &[]));
        plan.tasks.push(item(
            "T1",
            "First",
            &[("depends", "Q1"), ("accept", RUNNABLE)],
        ));
        plan.tasks
            .push(item("T2", "Second", &[("accept", RUNNABLE)]));
        run(&mut plan);
        assert!(plan.tasks[0].needs_owner);
        assert!(has_marker(&plan.tasks[0], "blocked by Q1"));
        assert_eq!(plan.tasks[0].depends_questions(), ["Q1"]);
        assert!(!plan.tasks[1].needs_owner);
    }

    #[test]
    fn g8_a_transitive_path_needs_no_new_edge() {
        let mut plan = BuildPlan::default();
        plan.tasks.push(item(
            "T1",
            "A",
            &[("touches", "src/x.rs"), ("accept", RUNNABLE)],
        ));
        plan.tasks
            .push(item("T2", "B", &[("depends", "T1"), ("accept", RUNNABLE)]));
        plan.tasks.push(item(
            "T3",
            "C",
            &[
                ("touches", "src/x.rs"),
                ("depends", "T2"),
                ("accept", RUNNABLE),
            ],
        ));
        run(&mut plan);
        assert_eq!(plan.tasks[2].list("depends"), ["T2"]);
        assert!(plan.tasks[2].markers.is_empty());
    }

    #[test]
    fn g8_a_cycle_needs_the_owner() {
        let mut plan = BuildPlan::default();
        plan.tasks
            .push(item("T1", "A", &[("depends", "T2"), ("accept", RUNNABLE)]));
        plan.tasks
            .push(item("T2", "B", &[("depends", "T1"), ("accept", RUNNABLE)]));
        plan.tasks.push(item("T3", "C", &[("accept", RUNNABLE)]));
        run(&mut plan);
        assert!(plan.tasks[0].needs_owner && plan.tasks[1].needs_owner);
        assert!(has_marker(&plan.tasks[0], "dependency cycle"));
        assert!(!plan.tasks[2].needs_owner);
    }

    #[test]
    fn g9_prose_accept_needs_the_owner() {
        let mut plan = BuildPlan::default();
        plan.tasks
            .push(item("T1", "Prose", &[("accept", "it works well")]));
        plan.tasks.push(item("T2", "Missing", &[]));
        plan.tasks
            .push(item("T3", "Arrow", &[("accept", "`ls` -> lists files")]));
        plan.tasks
            .push(item("T4", "Empty", &[("accept", "`ls` →")]));
        run(&mut plan);
        assert!(plan.tasks[0].needs_owner);
        assert!(has_marker(&plan.tasks[0], "no runnable accept"));
        assert!(plan.tasks[1].needs_owner);
        assert!(!plan.tasks[2].needs_owner);
        assert!(plan.tasks[3].needs_owner);
    }

    #[test]
    fn g9_a_destructive_check_is_flagged() {
        let mut plan = BuildPlan::default();
        plan.verify
            .push(item("P1", "Wipe", &[("check", "`rm -rf target`")]));
        plan.verify
            .push(item("P2", "Mutating", &[("check", "`touch x`")]));
        plan.verify.push(item(
            "P3",
            "Fine",
            &[("check", "`grep -n foo src/a.rs | wc -l`")],
        ));
        plan.tasks.push(item(
            "T1",
            "Ship",
            &[("accept", "`git push origin main` → exit 0")],
        ));
        run(&mut plan);
        assert!(has_marker(&plan.verify[0], "destructive command"));
        assert!(has_marker(&plan.verify[1], "check is not read-only"));
        assert!(plan.verify[2].markers.is_empty());
        assert!(plan.tasks[0].needs_owner);
        assert!(has_marker(&plan.tasks[0], "destructive command"));
    }

    #[test]
    fn g9_owner_work_is_marked() {
        let mut plan = BuildPlan::default();
        plan.tasks.push(item(
            "T1",
            "Label rows and write docs",
            &[("accept", "`cargo test` → hand-label 20 rows")],
        ));
        run(&mut plan);
        assert!(plan.tasks[0].needs_owner);
        assert!(has_marker(&plan.tasks[0], "needs you"));
        assert!(!has_marker(&plan.tasks[0], "split: one task per commit"));
    }

    fn kill_plan() -> BuildPlan {
        let mut plan = BuildPlan::default();
        plan.tasks
            .push(item("T1", "Measure", &[("accept", RUNNABLE)]));
        plan.tasks
            .push(item("T2", "Build", &[("accept", RUNNABLE)]));
        plan.kills.push(item(
            "K1",
            "Stop the build if the measure fails",
            &[("checked by", "T1"), ("gates", "T2")],
        ));
        plan
    }

    #[test]
    fn g10_a_gated_task_gets_the_checking_edge() {
        let mut plan = kill_plan();
        plan.kills.push(item("K2", "Nothing wired", &[]));
        run(&mut plan);
        assert_eq!(plan.tasks[1].list("depends"), ["T1"]);
        assert!(has_marker(&plan.tasks[1], "added: K1 gates T2"));
        assert!(has_marker(
            &plan.kills[1],
            "incomplete kill wiring: checked by, gates"
        ));
        assert!(plan.kills[0].markers.is_empty());
    }

    #[test]
    fn g10_continue_anyway_is_flagged() {
        let mut plan = kill_plan();
        plan.kills[0].text = "Stop the build, then continue anyway".to_string();
        let report = run(&mut plan);
        assert!(has_marker(
            &plan.kills[0],
            "kill criterion says continue anyway"
        ));
        assert!(report.notes.iter().any(|n| n.contains("K1")));
    }

    #[test]
    fn g10_gate_language_without_a_kill_row_opens_a_question() {
        let mut plan = BuildPlan::default();
        plan.tasks
            .push(item("T1", "Build", &[("accept", RUNNABLE)]));
        run_with(
            &mut plan,
            "## user\nThe indexer must not be built until the probe is measured.\n",
        );
        assert_eq!(plan.open.len(), 1);
        assert!(plan.open[0].text.contains("must not be built"));
        assert!(has_marker(
            &plan.open[0],
            "gate language without a kill row"
        ));

        let mut wired = kill_plan();
        run_with(&mut wired, "## user\nThe indexer must not be built yet.\n");
        assert!(wired.open.is_empty());
    }

    fn task_plan() -> BuildPlan {
        let mut plan = BuildPlan::default();
        plan.tasks
            .push(item("T1", "Build", &[("accept", RUNNABLE)]));
        plan
    }

    #[test]
    fn gate_language_ignores_word_fragments() {
        let mut plan = task_plan();
        run_with(
            &mut plan,
            "## user\nThat happens commonly if the cache is cold, and it is a monopoly if we win.\n",
        );
        assert!(plan.open.is_empty(), "{:?}", plan.open);
    }

    #[test]
    fn gate_language_ignores_foil_turns() {
        let mut plan = task_plan();
        run_with(
            &mut plan,
            "## user\nShip the parser.\n\n## assistant\nThe indexer must not be built until the probe is measured.\n",
        );
        assert!(plan.open.is_empty(), "{:?}", plan.open);
    }

    #[test]
    fn gate_language_window_is_whole_words_no_markdown() {
        let mut plan = task_plan();
        run_with(
            &mut plan,
            "## user\nHonestly, after a long week of thinking it over we ship the indexer **only** if the parser passes every golden fixture we already keep in the repository today, no exceptions at all.\n",
        );
        assert_eq!(plan.open.len(), 1);
        let text = &plan.open[0].text;
        assert!(!text.contains('*'), "{text}");
        let window = text.split('"').nth(1).unwrap();
        assert_eq!(
            window,
            "of thinking it over we ship the indexer only if the parser passes every golden \
             fixture we already keep in the repository"
        );
        assert_eq!(window.split(' ').count(), WINDOW_BEFORE + WINDOW_AFTER);
    }

    // `reset_derived` (ADR-0032) keeps GATE_MARKER on Open questions, so a re-gated plan carries
    // the marker back in through `parse_artifact`; this pins the half G10 owns — the marker
    // survives a render/parse round trip and suppresses a second question.
    #[test]
    fn gate_language_not_refired_after_reset() {
        let conversation = "## user\nThe indexer must not be built until the probe is measured.\n";
        let mut plan = task_plan();
        run_with(&mut plan, conversation);
        assert_eq!(plan.open.len(), 1);

        let mut again = crate::concepts::build_plan::plan::parse_artifact(&render(&plan)).unwrap();
        crate::concepts::build_plan::plan::reset_derived(&mut again);
        run_with(&mut again, conversation);
        let asked = again
            .open
            .iter()
            .filter(|q| has_marker(q, GATE_MARKER))
            .count();
        assert_eq!(asked, 1, "{:?}", again.open);
    }

    #[test]
    fn gate_language_skips_answered_quote() {
        let conversation =
            "## user\nRe Q1 (plan-1): ship the indexer only if the parser passes the golden tests.\n";
        let answered = |owner: bool| {
            let mut plan = task_plan();
            let mut settled = item(
                "S1",
                "The indexer ships once the parser passes",
                &[
                    ("quote", "ship the indexer only if the parser passes"),
                    ("answers", "Q1"),
                ],
            );
            settled.provenance = Some(if owner {
                Provenance::Owner
            } else {
                Provenance::Foil
            });
            plan.settled.push(settled);
            run_with(&mut plan, conversation);
            plan
        };
        assert!(answered(true).open.is_empty());
        assert_eq!(
            answered(false).open.len(),
            1,
            "only an owner answer exempts"
        );
    }

    const GATE_TURN: &str =
        "## user\nShip the indexer only if the parser passes the golden tests.\n";
    const G10_ANSWER: &str = "No kill row: the indexer is cheap to throw away.";

    fn g10_answer() -> Answered {
        Answered {
            qid: "Q3".into(),
            asked: format!("{GATE_MARKER}: \"ship the indexer only if the parser passes\""),
            answer: G10_ANSWER.into(),
            in_stem: "plan-1".into(),
        }
    }

    fn gate_questions(plan: &BuildPlan) -> usize {
        plan.open
            .iter()
            .filter(|q| has_marker(q, GATE_MARKER))
            .count()
    }

    #[test]
    fn gate_language_answered_g10_question_is_not_reasked() {
        let conversation = format!("{GATE_TURN}\n## user\nRe Q3 (plan-1): {G10_ANSWER}\n");
        let mut plan = task_plan();
        run_with_answered(&mut plan, &conversation, &[g10_answer()]);
        assert_eq!(gate_questions(&plan), 0, "{:?}", plan.open);

        let mut other = task_plan();
        let mut not_g10 = g10_answer();
        not_g10.asked = "Which parser?".into();
        run_with_answered(&mut other, &conversation, &[not_g10]);
        assert_eq!(
            gate_questions(&other),
            1,
            "only a G10 answer settles gate language"
        );
    }

    #[test]
    fn gate_language_after_g10_answer_still_fires() {
        let conversation = format!(
            "{GATE_TURN}\n## user\nRe Q3 (plan-1): {G10_ANSWER}\n\n\
## user\nThe probe must not be built until the budget is known.\n"
        );
        let mut plan = task_plan();
        run_with_answered(&mut plan, &conversation, &[g10_answer()]);
        assert_eq!(gate_questions(&plan), 1, "{:?}", plan.open);
        assert!(plan.open[0].text.contains("must not be built"));
    }

    #[test]
    fn gate_language_quoted_mention_is_ignored() {
        for turn in [
            "## user\nQ15 is a detector bug (\"only if\" matched inside a steelman sentence).\n",
            "## user\nThe 'only if' in my earlier message quoted the detector bug.\n",
            "## user\nThe \u{201c}kill criteria\u{201d} phrase is just a label here.\n",
        ] {
            let mut plan = task_plan();
            run_with(&mut plan, turn);
            assert!(plan.open.is_empty(), "{turn}: {:?}", plan.open);
        }
    }

    #[test]
    fn gate_language_quoted_gate_sentence_still_fires() {
        let mut plan = task_plan();
        run_with(
            &mut plan,
            "## user\nMy rule stays \"ship only if tests pass\" for this one.\n",
        );
        assert_eq!(gate_questions(&plan), 1, "{:?}", plan.open);
    }

    #[test]
    fn gate_language_answer_settles_only_the_turn_it_was_asked_about() {
        let conversation = format!(
            "{GATE_TURN}\n## user\nThe probe must not be built until the budget is known.\n\n\
## user\nRe Q3 (plan-1): {G10_ANSWER}\n"
        );
        let mut plan = task_plan();
        run_with_answered(&mut plan, &conversation, &[g10_answer()]);
        assert_eq!(gate_questions(&plan), 1, "{:?}", plan.open);
        assert!(
            plan.open[0].text.contains("must not be built"),
            "{:?}",
            plan.open
        );
    }

    #[test]
    fn gate_language_answer_text_typed_earlier_still_settles_the_question() {
        let conversation = format!(
            "## user\n{G10_ANSWER}\n\n{GATE_TURN}\n## user\nRe Q3 (plan-1): {G10_ANSWER}\n"
        );
        let mut plan = task_plan();
        run_with_answered(&mut plan, &conversation, &[g10_answer()]);
        assert_eq!(gate_questions(&plan), 0, "{:?}", plan.open);
    }

    #[test]
    fn gate_language_in_the_g10_answer_turn_is_not_reasked() {
        let mut answer = g10_answer();
        answer.answer = "No kill row; the indexer ships only if someone asks for it.".into();
        let conversation = format!("{GATE_TURN}\n## user\nRe Q3 (plan-1): {}\n", answer.answer);
        let mut plan = task_plan();
        run_with_answered(&mut plan, &conversation, &[answer]);
        assert_eq!(gate_questions(&plan), 0, "{:?}", plan.open);
    }

    #[test]
    fn gate_language_pasting_an_answered_g10_window_does_not_fire() {
        let conversation = format!(
            "{GATE_TURN}\n## user\nRe Q3 (plan-1): {G10_ANSWER}\n\n\
## user\nQ3 flagged ship the indexer only if the parser passes, which was a steelman.\n"
        );
        let mut plan = task_plan();
        run_with_answered(&mut plan, &conversation, &[g10_answer()]);
        assert_eq!(gate_questions(&plan), 0, "{:?}", plan.open);
    }

    #[test]
    fn g11_caps_note_a_split_and_trim_nothing() {
        let mut plan = BuildPlan {
            missing: vec!["## Kill criteria"],
            ..BuildPlan::default()
        };
        for n in 1..=16 {
            plan.tasks
                .push(item(&format!("T{n}"), "Step", &[("accept", RUNNABLE)]));
        }
        let report = run(&mut plan);
        assert_eq!(plan.tasks.len(), 16);
        assert!(report
            .notes
            .contains(&"missing section: ## Kill criteria — placeholder".to_string()));
        assert_eq!(
            report
                .notes
                .iter()
                .filter(|n| *n == "too large: split into phases")
                .count(),
            1
        );

        let mut small = BuildPlan::default();
        small
            .tasks
            .push(item("T1", "Step", &[("accept", RUNNABLE)]));
        assert!(run(&mut small).notes.is_empty());
    }

    #[test]
    fn g8_prose_dependencies_are_read_by_task_id() {
        let mut plan = BuildPlan::default();
        plan.tasks.push(item("T1", "A", &[("accept", RUNNABLE)]));
        plan.tasks.push(item("T2", "B", &[("accept", RUNNABLE)]));
        plan.tasks.push(item(
            "T3",
            "C",
            &[("depends", "T1 and T2"), ("accept", RUNNABLE)],
        ));
        plan.tasks.push(item(
            "T4",
            "D",
            &[("depends", "t1 (scaffold), T9"), ("accept", RUNNABLE)],
        ));
        run(&mut plan);
        assert!(plan.tasks[2].markers.is_empty(), "{:?}", plan.tasks[2]);
        assert_eq!(plan.tasks[2].field("depends"), Some("T1 and T2"));
        assert_eq!(plan.tasks[3].list("depends"), ["T1"]);
        assert!(has_marker(&plan.tasks[3], "unknown dependency T9"));
        assert!(!has_marker(&plan.tasks[3], "unknown dependency T1"));
    }

    #[test]
    fn g8_nearest_task_suppresses_the_redundant_edge() {
        let mut plan = BuildPlan::default();
        plan.tasks.push(item(
            "T1",
            "A",
            &[("touches", "src/x.rs"), ("accept", RUNNABLE)],
        ));
        plan.tasks.push(item(
            "T2",
            "B",
            &[
                ("touches", "src/x.rs"),
                ("depends", "T1"),
                ("accept", RUNNABLE),
            ],
        ));
        plan.tasks.push(item(
            "T3",
            "C",
            &[("touches", "src/x.rs"), ("accept", RUNNABLE)],
        ));
        run(&mut plan);
        assert_eq!(plan.tasks[2].list("depends"), ["T2"]);
    }

    #[test]
    fn g7_anchored_glob_and_new_paths_still_overlap() {
        let mut plan = BuildPlan::default();
        plan.fence
            .push(item("F1", "`src/index/reindex.rs:382-390` is frozen", &[]));
        plan.fence.push(item("F2", "`src/memory/*`", &[]));
        plan.settled
            .push(item("S1", "Don\u{2019}t modify `src/ai/budget.rs`.", &[]));
        plan.tasks.push(item(
            "T1",
            "A",
            &[("touches", "src/index/reindex.rs"), ("accept", RUNNABLE)],
        ));
        plan.tasks.push(item(
            "T2",
            "B",
            &[("touches", "src/memory/load.rs"), ("accept", RUNNABLE)],
        ));
        plan.tasks.push(item(
            "T3",
            "C",
            &[("touches", "src/ai/budget.rs (new)"), ("accept", RUNNABLE)],
        ));
        plan.tasks.push(item(
            "T4",
            "D",
            &[("touches", "src/index/reindex.rs:12"), ("accept", RUNNABLE)],
        ));
        run(&mut plan);
        assert!(plan.tasks[0].needs_owner, "anchored fence");
        assert!(plan.tasks[1].needs_owner, "glob fence");
        assert!(plan.tasks[2].needs_owner, "curly apostrophe and (new)");
        assert!(plan.tasks[3].needs_owner, "anchored touches");
    }

    #[test]
    fn g8_shared_anchored_paths_get_a_dependency() {
        let mut plan = BuildPlan::default();
        plan.tasks.push(item(
            "T1",
            "A",
            &[("touches", "src/x.rs:10"), ("accept", RUNNABLE)],
        ));
        plan.tasks.push(item(
            "T2",
            "B",
            &[("touches", "src/x.rs"), ("accept", RUNNABLE)],
        ));
        run(&mut plan);
        assert_eq!(plan.tasks[1].list("depends"), ["T1"]);
    }

    #[test]
    fn g9_multi_command_accept_is_runnable() {
        let mut plan = BuildPlan::default();
        plan.tasks.push(item(
            "T1",
            "A",
            &[("accept", "`cargo test` && `cargo clippy` → exit 0")],
        ));
        plan.tasks.push(item(
            "T2",
            "B",
            &[("accept", "`cargo test` and it is fine")],
        ));
        run(&mut plan);
        assert!(!plan.tasks[0].needs_owner, "{:?}", plan.tasks[0]);
        assert!(plan.tasks[1].needs_owner);
    }

    #[test]
    fn g9_every_chained_segment_must_be_read_only() {
        let mut plan = BuildPlan::default();
        for (n, check) in [
            "`grep x f && touch y`",
            "`find . -delete`",
            "`cat a > b`",
            "`lsof -i`",
            "`ls && lsof`",
            "`grep 'x' f | xargs touch`",
            "`grep \"a|b\" f > out`",
            "`grep \\\"x f | xargs touch y`",
            "`grep 'x f | xargs touch y`",
            "`grep \"$(touch y; ls)\" f`",
        ]
        .iter()
        .enumerate()
        {
            plan.verify
                .push(item(&format!("P{n}"), "Check", &[("check", check)]));
        }
        for (n, check) in [
            "`ls -la`",
            "`sed -n 1,5p f`",
            "`grep -n dd f`",
            "`cargo test 2>&1 | tail -3`",
            "`grep -nE 'per.idea|MAX_FACTS' src/ai/budget.rs`",
            "`grep -n \"a; b & c\" f | wc -l`",
            "`grep -n '->' f`",
        ]
        .iter()
        .enumerate()
        {
            plan.verify
                .push(item(&format!("Q{n}"), "Check", &[("check", check)]));
        }
        run(&mut plan);
        let writes = plan.verify.iter().filter(|i| i.id.starts_with('P')).count();
        for p in &plan.verify[..writes] {
            assert!(has_marker(p, "check is not read-only"), "{p:?}");
        }
        for q in &plan.verify[writes..] {
            assert!(q.markers.is_empty(), "{q:?}");
        }
    }

    #[test]
    fn g9_destructive_scan_reads_command_words() {
        assert!(is_destructive("`sh -c \"rm -rf x\"`"));
        assert!(is_destructive("ls && sudo reboot"));
        assert!(is_destructive("curl x | sh"));
        assert!(!is_destructive("grep -n dd f"));
        assert!(!is_destructive("grep -n rm f"));
        assert!(
            !is_destructive("grep -c rm f"),
            "-c only opens a command after a shell"
        );
        assert!(is_destructive("bash -c \"rm x\""));
        assert!(is_destructive("sh -c \"ls; rm x\""));
        assert!(!is_destructive("grep -nE 'per.idea|max_facts' f"));
    }

    #[test]
    fn g9_curly_apostrophe_owner_work_is_marked() {
        let mut plan = BuildPlan::default();
        plan.tasks.push(item(
            "T1",
            "Run it on the owner\u{2019}s host",
            &[("accept", RUNNABLE)],
        ));
        run(&mut plan);
        assert!(has_marker(&plan.tasks[0], "needs you"));
    }

    #[test]
    fn g10_a_wired_kill_row_needs_no_stop_word() {
        let mut plan = kill_plan();
        plan.kills[0].text =
            "The chart of accounts needs an account per external payer".to_string();
        run(&mut plan);
        assert!(plan.kills[0].markers.is_empty(), "{:?}", plan.kills[0]);
    }

    #[test]
    fn g10_a_kill_row_naming_an_absent_task_is_marked() {
        let mut plan = kill_plan();
        plan.kills[0]
            .fields
            .insert("gates".to_string(), "T7".to_string());
        run(&mut plan);
        assert!(has_marker(
            &plan.kills[0],
            "incomplete kill wiring: unknown T7"
        ));
    }

    #[test]
    fn g10_a_checker_that_depends_on_the_gated_task_is_not_looped() {
        let mut plan = kill_plan();
        plan.tasks[0]
            .fields
            .insert("depends".to_string(), "T2".to_string());
        run(&mut plan);
        assert!(has_marker(
            &plan.kills[0],
            "incomplete kill wiring: T1 depends on T2"
        ));
        assert!(plan.tasks[1].field("depends").is_none());
    }

    #[test]
    fn g10_only_one_gate_language_question_is_asked() {
        let mut plan = BuildPlan::default();
        plan.tasks
            .push(item("T1", "Build", &[("accept", RUNNABLE)]));
        let talk = "## user\nIt must not be built yet.\n## assistant\nA precondition applies.\n";
        run_with(&mut plan, talk);
        run_with(&mut plan, talk);
        assert_eq!(plan.open.len(), 1);
    }

    #[test]
    fn g11_settled_count_and_rendered_size_are_capped() {
        let too_large = "too large: split into phases".to_string();
        let mut twelve = BuildPlan::default();
        for n in 1..=12 {
            twelve.settled.push(item(&format!("S{n}"), "Fact", &[]));
        }
        assert!(!run(&mut twelve).notes.contains(&too_large));
        twelve.settled.push(item("S13", "Fact", &[]));
        assert!(run(&mut twelve).notes.contains(&too_large));

        let mut fat = BuildPlan::default();
        fat.tasks
            .push(item("T1", &"a".repeat(13 * 1024), &[("accept", RUNNABLE)]));
        assert!(run(&mut fat).notes.contains(&too_large));
    }
}
