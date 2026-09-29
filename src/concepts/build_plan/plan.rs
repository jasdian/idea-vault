//! The build-plan grammar (docs/adr/0029): a tolerant parser from the model's markdown into a
//! typed [`BuildPlan`], and the canonical renderer back. Pure — no I/O, no model call.
//!
//! The model writes `## Goal`, `## Settled`, `## Verify first`, `## Open questions`, `## Plan` and
//! `## Kill criteria` (plus an optional `## Fence`); each item is a line opening with its id
//! (`S1:`, `P1:`, `Q1:`, `T1:`, `K1:`) followed by `key: value` field lines. Code owns the
//! `## Quarantined` section and the `⟨…⟩` gate markers: markers are stripped on parse and re-added
//! by the gates, so re-parsing a rendered plan never compounds them.

use std::collections::BTreeMap;

use crate::ai::contract::repair_build_plan;

/// One item of a plan section: its id, its one-line text, and its `key: value` fields (keys
/// lowercase).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Item {
    pub id: String,
    pub text: String,
    pub fields: BTreeMap<String, String>,
    /// Gate annotations, rendered as `⟨…⟩` after the text. Never parsed back.
    pub markers: Vec<String>,
    /// A task only its owner can do or unblock (`[?]`); never auto-selected by a build loop.
    pub needs_owner: bool,
}

impl Item {
    pub fn new(id: &str, text: &str) -> Self {
        Item {
            id: id.to_string(),
            text: text.to_string(),
            ..Item::default()
        }
    }

    /// A field's value, if present and not a "none" placeholder.
    pub fn field(&self, key: &str) -> Option<&str> {
        self.fields
            .get(key)
            .map(String::as_str)
            .filter(|v| !is_none(v))
    }

    /// A comma-separated list field (`depends`, `touches`, `gates`), each entry trimmed of
    /// whitespace and backticks.
    pub fn list(&self, key: &str) -> Vec<String> {
        self.field(key)
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim().trim_matches('`').trim().to_string())
                    .filter(|s| !s.is_empty() && !is_none(s))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// A parsed build plan. Section vectors keep the model's order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BuildPlan {
    pub goal: String,
    pub settled: Vec<Item>,
    pub verify: Vec<Item>,
    pub open: Vec<Item>,
    pub tasks: Vec<Item>,
    pub kills: Vec<Item>,
    pub fence: Vec<Item>,
    /// Claims the gates removed, with a `reason` field. Code-owned.
    pub quarantined: Vec<Item>,
    /// Required sections the answer lacked; they parse as empty and are reported by the gates.
    pub missing: Vec<&'static str>,
}

/// The answer names neither a goal nor a single task, so there is nothing to gate or persist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unusable;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Goal,
    Settled,
    Verify,
    Open,
    Plan,
    Kill,
    Fence,
    Quarantined,
    Other,
}

const REQUIRED: [(Section, &str); 6] = [
    (Section::Goal, "## Goal"),
    (Section::Settled, "## Settled"),
    (Section::Verify, "## Verify first"),
    (Section::Open, "## Open questions"),
    (Section::Plan, "## Plan"),
    (Section::Kill, "## Kill criteria"),
];

fn section_of(heading: &str) -> Section {
    match heading
        .trim_start_matches('#')
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "goal" => Section::Goal,
        "settled" => Section::Settled,
        "verify first" => Section::Verify,
        "open questions" => Section::Open,
        "plan" => Section::Plan,
        "kill criteria" => Section::Kill,
        "fence" => Section::Fence,
        h if h.starts_with("quarantined") => Section::Quarantined,
        _ => Section::Other,
    }
}

fn id_letter(section: Section) -> char {
    match section {
        Section::Settled => 'S',
        Section::Verify => 'P',
        Section::Open => 'Q',
        Section::Plan => 'T',
        Section::Kill => 'K',
        Section::Fence => 'F',
        _ => 'X',
    }
}

const FIELD_KEYS: &[&str] = &[
    "quote",
    "check",
    "count",
    "depends",
    "touches",
    "accept",
    "checked by",
    "gates",
    "reason",
];

fn is_none(v: &str) -> bool {
    matches!(
        v.trim().trim_matches('`').to_ascii_lowercase().as_str(),
        "" | "none" | "-" | "—" | "n/a"
    )
}

/// Remove every `⟨…⟩` marker (a gate annotation) from a line.
fn strip_markers(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut depth = 0usize;
    for ch in line.chars() {
        match ch {
            '⟨' => depth += 1,
            '⟩' if depth > 0 => depth -= 1,
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.trim_end().to_string()
}

/// Split a line on `·` or `|` separators that sit outside backticks.
fn split_inline(line: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut in_tick = false;
    for ch in line.chars() {
        match ch {
            '`' => {
                in_tick = !in_tick;
                cur.push(ch);
            }
            '·' | '|' if !in_tick => parts.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    parts.push(cur);
    parts
        .into_iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// `key: value` with a known key (any case, optionally bold), else `None`.
fn as_field(part: &str) -> Option<(String, String)> {
    let t = part.trim().trim_start_matches(['-', '*', '+']).trim();
    let t = t.trim_start_matches("**");
    let (key, value) = t.split_once(':')?;
    let key = key
        .trim()
        .trim_end_matches("**")
        .trim()
        .to_ascii_lowercase();
    let key = if key == "checked_by" {
        "checked by".to_string()
    } else {
        key
    };
    FIELD_KEYS.contains(&key.as_str()).then(|| {
        let value = value.trim().trim_start_matches("**").trim();
        (key, value.to_string())
    })
}

/// Strip a list marker (bullet, number, `[ ]`/`[x]`/`[?]` box); report whether the box was `[?]`.
fn strip_list_marker(line: &str) -> (&str, bool, bool) {
    let t = line.trim_start();
    let mut listed = false;
    let mut rest = t;
    if let Some(r) = ["- ", "* ", "+ "].iter().find_map(|m| t.strip_prefix(m)) {
        rest = r;
        listed = true;
    } else {
        let digits = t.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 {
            if let Some(r) = t[digits..]
                .strip_prefix(". ")
                .or_else(|| t[digits..].strip_prefix(") "))
            {
                rest = r;
                listed = true;
            }
        }
    }
    let rest = rest.trim_start();
    for (box_, owner) in [
        ("[?]", true),
        ("[ ]", false),
        ("[x]", false),
        ("[X]", false),
    ] {
        if let Some(r) = rest.strip_prefix(box_) {
            return (r.trim_start(), true, owner);
        }
    }
    (rest, listed, false)
}

/// An item id at the start of `text` (`S1:`, `S1.`, `**S1**`, `S1 —`), returning (id, rest).
fn leading_id(text: &str) -> Option<(String, &str)> {
    let t = text.trim_start_matches("**");
    let mut chars = t.char_indices();
    let (_, letter) = chars.next()?;
    if !letter.is_ascii_alphabetic() {
        return None;
    }
    let digits: String = t[1..].chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let after = &t[1 + digits.len()..];
    let after = after.trim_start_matches("**");
    let rest = if let Some(r) = after.strip_prefix(':').or_else(|| after.strip_prefix('.')) {
        r
    } else if let Some(r) = after
        .trim_start()
        .strip_prefix('—')
        .or_else(|| after.trim_start().strip_prefix("- "))
    {
        r
    } else if after.is_empty() || (text.starts_with("**") && after.starts_with(' ')) {
        after
    } else {
        return None;
    };
    let rest = rest.trim_start_matches("**").trim();
    Some((format!("{}{digits}", letter.to_ascii_uppercase()), rest))
}

fn cells(row: &str) -> Vec<String> {
    let t = row.trim().trim_start_matches('|').trim_end_matches('|');
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_tick = false;
    let mut prev = '\0';
    for ch in t.chars() {
        match ch {
            '`' => {
                in_tick = !in_tick;
                cur.push(ch);
            }
            '|' if !in_tick && prev != '\\' => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
        prev = ch;
    }
    out.push(cur);
    out.into_iter()
        .map(|c| c.trim().replace("\\|", "|"))
        .collect()
}

fn is_rule_row(row: &str) -> bool {
    cells(row)
        .iter()
        .all(|c| !c.is_empty() && c.chars().all(|ch| matches!(ch, '-' | ':' | ' ')))
}

/// A markdown table in `## Plan`: the header names the columns (id, task, depends, touches,
/// accept), any other column is ignored.
fn parse_table(rows: &[&str]) -> Vec<Item> {
    let Some((header, body)) = rows.split_first() else {
        return Vec::new();
    };
    let columns: Vec<String> = cells(header)
        .iter()
        .map(|c| c.to_ascii_lowercase())
        .collect();
    let col = |names: &[&str]| columns.iter().position(|c| names.iter().any(|n| c == n));
    let id_col = col(&["t", "#", "id"]);
    let task_col = col(&["task", "subject", "what"]);
    let mut items = Vec::new();
    for row in body.iter().filter(|r| !is_rule_row(r)) {
        let c = cells(row);
        let get = |i: Option<usize>| i.and_then(|i| c.get(i)).cloned().unwrap_or_default();
        let raw_id = get(id_col);
        let (_, _, owner) = strip_list_marker(&raw_id);
        let id = leading_id(strip_list_marker(&raw_id).0)
            .map(|(id, _)| id)
            .unwrap_or_default();
        let mut item = Item::new(&id, &strip_markers(&get(task_col)));
        item.needs_owner = owner;
        for (key, names) in [
            ("depends", &["depends", "depends on", "deps"][..]),
            ("touches", &["touches", "files"][..]),
            ("accept", &["accept", "acceptance", "accept:"][..]),
        ] {
            let v = strip_markers(&get(col(names)));
            if !is_none(&v) {
                item.fields.insert(key.to_string(), v);
            }
        }
        items.push(item);
    }
    items
}

/// Parse one list-shaped section into items. Item lines open with an id or a list marker;
/// `key: value` lines (or `·`/`|`-separated fields on the item line) attach to the last item;
/// other continuation lines extend its text.
fn parse_items(lines: &[&str], section: Section) -> Vec<Item> {
    let table: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| l.trim_start().starts_with('|'))
        .collect();
    if section == Section::Plan && table.len() >= 2 {
        return parse_table(&table);
    }
    let mut items: Vec<Item> = Vec::new();
    let mut in_fence = false;
    for raw in lines {
        if raw.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        let line = strip_markers(raw);
        if in_fence || line.trim().is_empty() {
            continue;
        }
        if let (Some(last), Some((key, value))) = (items.last_mut(), as_field(&line)) {
            last.fields.insert(key, value);
            continue;
        }
        let (rest, listed, owner) = strip_list_marker(&line);
        let starts_item = listed || leading_id(rest).is_some() || items.is_empty();
        if !starts_item {
            if let Some(last) = items.last_mut() {
                last.text = format!("{} {}", last.text, line.trim()).trim().to_string();
            }
            continue;
        }
        let parts = split_inline(rest);
        let Some((head, tail)) = parts.split_first() else {
            continue;
        };
        let (id, text) = match leading_id(head) {
            Some((id, text)) => (id, text.to_string()),
            None => (String::new(), head.trim().to_string()),
        };
        if is_none(&text) && tail.is_empty() && id.is_empty() {
            continue;
        }
        let mut item = Item::new(&id, &text);
        item.needs_owner = owner;
        for part in tail {
            match as_field(part) {
                Some((key, value)) => {
                    item.fields.insert(key, value);
                }
                None => item.text = format!("{} · {}", item.text, part),
            }
        }
        items.push(item);
    }
    let letter = id_letter(section);
    let mut n = 0;
    for item in &mut items {
        n += 1;
        if item.id.is_empty() {
            item.id = format!("{letter}{n}");
        }
    }
    items
}

/// Parse a model's build-plan answer. The answer is first repaired (preamble, wrapping fence,
/// heading aliases — [`repair_build_plan`]); required sections it still lacks are recorded in
/// [`BuildPlan::missing`] and parse as empty. Fails only when there is neither a goal nor a task.
pub fn parse(answer: &str) -> Result<BuildPlan, Unusable> {
    let repaired = repair_build_plan(answer);
    let mut plan = BuildPlan::default();
    let mut blocks: Vec<(Section, Vec<&str>)> = Vec::new();
    let mut in_fence = false;
    for line in repaired.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if !in_fence && line.starts_with("## ") {
            blocks.push((section_of(line), Vec::new()));
        } else if let Some((_, body)) = blocks.last_mut() {
            body.push(line);
        }
    }
    let mut seen: Vec<Section> = Vec::new();
    for (section, body) in &blocks {
        seen.push(*section);
        match section {
            Section::Goal => {
                let text = body
                    .iter()
                    .map(|l| strip_markers(l))
                    .filter(|l| !l.trim().is_empty() && !l.starts_with('_'))
                    .collect::<Vec<_>>()
                    .join("\n");
                if !plan.goal.is_empty() && !text.is_empty() {
                    plan.goal.push('\n');
                }
                plan.goal.push_str(text.trim());
            }
            Section::Settled => plan.settled.extend(parse_items(body, *section)),
            Section::Verify => plan.verify.extend(parse_items(body, *section)),
            Section::Open => plan.open.extend(parse_items(body, *section)),
            Section::Plan => plan.tasks.extend(parse_items(body, *section)),
            Section::Kill => plan.kills.extend(parse_items(body, *section)),
            Section::Fence => plan.fence.extend(parse_items(body, *section)),
            Section::Quarantined | Section::Other => {}
        }
    }
    plan.missing = REQUIRED
        .iter()
        .filter(|(s, _)| !seen.contains(s))
        .map(|(_, h)| *h)
        .collect();
    if plan.goal.trim().is_empty() && plan.tasks.is_empty() {
        return Err(Unusable);
    }
    Ok(plan)
}

fn render_item(out: &mut String, item: &Item, task: bool) {
    let boxed = match (task, item.needs_owner) {
        (true, true) => "- [?] ",
        (true, false) => "- [ ] ",
        _ => "- ",
    };
    out.push_str(&format!("{boxed}{}: {}", item.id, item.text));
    for m in &item.markers {
        out.push_str(&format!(" ⟨{m}⟩"));
    }
    out.push('\n');
    for (key, value) in &item.fields {
        out.push_str(&format!("  {key}: {value}\n"));
    }
}

fn render_section(out: &mut String, heading: &str, items: &[Item], task: bool) {
    out.push_str(heading);
    out.push('\n');
    if items.is_empty() {
        out.push_str("- none\n");
    }
    for item in items {
        render_item(out, item, task);
    }
    out.push('\n');
}

/// Render a plan in the canonical grammar, gate markers included. `parse(render(p))` gives `p`
/// back with its markers and `missing` cleared.
pub fn render(plan: &BuildPlan) -> String {
    let mut out = String::new();
    out.push_str("## Goal\n");
    out.push_str(if plan.goal.trim().is_empty() {
        "- none"
    } else {
        plan.goal.trim()
    });
    out.push_str("\n\n");
    render_section(&mut out, "## Settled", &plan.settled, false);
    render_section(&mut out, "## Verify first", &plan.verify, false);
    render_section(&mut out, "## Open questions", &plan.open, false);
    if !plan.fence.is_empty() {
        render_section(&mut out, "## Fence", &plan.fence, false);
    }
    render_section(&mut out, "## Plan", &plan.tasks, true);
    render_section(&mut out, "## Kill criteria", &plan.kills, false);
    if !plan.quarantined.is_empty() {
        render_section(
            &mut out,
            "## Quarantined — do not build on these",
            &plan.quarantined,
            false,
        );
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: &str = "## Goal
Run the cheapest disproof before any Rust exists.

## Settled
- S1: BOCPD is the first regime detector, behind a trait
  quote: \"going forward with BOCPD seems probable for our usecase\"
- S2: Close is never gated like open.
  quote: \"Close must not be gated symmetrically with open\"

## Verify first
- P1: The sizing scaler is `calculate_regime_factor` at `risk/src/calculator.rs:385`
  check: `sed -n 385p risk/src/calculator.rs | grep -nF calculate_regime_factor`

## Open questions
- Q1: Freeze the zone snapshot at entry, or dwell hysteresis?

## Plan
- [?] T1: Write SPEC.md with a dated kill criterion
- [ ] T2: Backtest SPEC.md at a pessimistic spread
  depends: T1
  touches: `backtest/`
  accept: `python backtest/run.py --spec SPEC.md` → last line is KILL or SURVIVES

## Kill criteria
- K1: The backtest prints KILL → stop and report
  checked by: T2
  gates: T3";

    #[test]
    fn parse_reads_the_line_grammar() {
        let plan = parse(PLAN).unwrap();
        assert_eq!(
            plan.goal,
            "Run the cheapest disproof before any Rust exists."
        );
        assert_eq!(plan.settled.len(), 2);
        assert_eq!(
            plan.settled[0].field("quote"),
            Some("\"going forward with BOCPD seems probable for our usecase\"")
        );
        assert_eq!(plan.verify[0].id, "P1");
        assert!(plan.verify[0]
            .field("check")
            .unwrap()
            .starts_with("`sed -n 385p"));
        assert_eq!(
            plan.open[0].text,
            "Freeze the zone snapshot at entry, or dwell hysteresis?"
        );
        assert!(plan.tasks[0].needs_owner && !plan.tasks[1].needs_owner);
        assert_eq!(plan.tasks[1].list("depends"), ["T1"]);
        assert_eq!(plan.tasks[1].list("touches"), ["backtest/"]);
        assert_eq!(plan.kills[0].field("checked by"), Some("T2"));
        assert_eq!(plan.kills[0].list("gates"), ["T3"]);
        assert!(plan.missing.is_empty(), "{:?}", plan.missing);
    }

    #[test]
    fn parse_accepts_a_markdown_task_table() {
        let answer = "## Goal\nShip.\n## Plan\n| T | Task | Depends | Touches | Accept |\n|---|---|---|---|---|\n| T1 | Build the parser | — | `src/p.rs` | `cargo test p_` → exit 0 |\n| [?] T2 | Owner signs off | T1 | none | none |";
        let plan = parse(answer).unwrap();
        assert_eq!(plan.tasks.len(), 2);
        assert_eq!(plan.tasks[0].text, "Build the parser");
        assert_eq!(plan.tasks[0].list("touches"), ["src/p.rs"]);
        assert_eq!(
            plan.tasks[0].field("accept"),
            Some("`cargo test p_` → exit 0")
        );
        assert!(plan.tasks[0].list("depends").is_empty());
        assert!(plan.tasks[1].needs_owner);
        assert_eq!(plan.tasks[1].list("depends"), ["T1"]);
    }

    #[test]
    fn parse_reads_inline_fields_split_by_dots_or_bars() {
        let answer = "## Goal\nShip.\n## Plan\n1. **T1.** Add the probe · depends: none · touches: `src/ai/sources.rs` | accept: `cargo test probe_ | tail -1` → ok\n2. T2 — Wire it · Depends: T1";
        let plan = parse(answer).unwrap();
        let t1 = &plan.tasks[0];
        assert_eq!((t1.id.as_str(), t1.text.as_str()), ("T1", "Add the probe"));
        assert_eq!(t1.list("touches"), ["src/ai/sources.rs"]);
        assert_eq!(
            t1.field("accept"),
            Some("`cargo test probe_ | tail -1` → ok"),
            "a bar inside backticks is not a separator"
        );
        assert_eq!(plan.tasks[1].id, "T2");
        assert_eq!(plan.tasks[1].list("depends"), ["T1"]);
    }

    #[test]
    fn parse_numbers_items_without_ids_and_records_missing_sections() {
        let plan =
            parse("## Goal\nShip.\n## Settled\n- we ship solo\n- S3 buckets hold exports").unwrap();
        assert_eq!(
            plan.settled[1].text, "S3 buckets hold exports",
            "a word shaped like an id but with no separator stays text"
        );
        assert_eq!(
            plan.settled
                .iter()
                .map(|i| i.id.as_str())
                .collect::<Vec<_>>(),
            ["S1", "S2"]
        );
        assert_eq!(
            plan.missing,
            [
                "## Verify first",
                "## Open questions",
                "## Plan",
                "## Kill criteria"
            ]
        );
    }

    #[test]
    fn render_round_trips_and_strips_old_gate_markers() {
        let mut plan = parse(PLAN).unwrap();
        plan.settled[1].markers.push("opened from Settled".into());
        plan.quarantined.push(Item {
            fields: [(
                "reason".to_string(),
                "quote not in the discussion".to_string(),
            )]
            .into(),
            ..Item::new("X1", "The owner chose freeze at entry")
        });
        let rendered = render(&plan);
        assert!(rendered.contains("⟨opened from Settled⟩"), "{rendered}");
        let mut back = parse(&rendered).unwrap();
        assert!(back.settled[1].markers.is_empty());
        assert!(
            back.quarantined.is_empty(),
            "quarantine is code-owned, never parsed back"
        );
        back.quarantined = plan.quarantined.clone();
        plan.settled[1].markers.clear();
        assert_eq!(back, plan);
        let mut unquarantined = back.clone();
        unquarantined.quarantined.clear();
        assert_eq!(
            render(&parse(&render(&unquarantined)).unwrap()),
            render(&unquarantined),
            "render is a fixed point of parse"
        );
    }

    #[test]
    fn no_goal_and_no_task_is_unusable() {
        assert_eq!(parse("I could not make a plan."), Err(Unusable));
        assert_eq!(parse("## Settled\n- S1: x"), Err(Unusable));
        assert!(parse("## Plan\n- T1: something").is_ok());
    }

    #[test]
    fn fenced_code_in_a_task_is_not_parsed_as_items() {
        let answer = "## Goal\nShip.\n## Plan\n- T1: Run it\n  ```sh\n  - not an item\n  accept: not a field\n  ```\n  accept: `make` → exit 0";
        let plan = parse(answer).unwrap();
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].field("accept"), Some("`make` → exit 0"));
    }
}
