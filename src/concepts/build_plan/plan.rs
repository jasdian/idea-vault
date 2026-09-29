//! The build-plan grammar (docs/adr/0030): a tolerant parser from the model's markdown into a
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
    /// Who the grounding quote came from, set by the gates and rendered as `— you` / `— foil` /
    /// `— idea` after the text.
    pub provenance: Option<Provenance>,
}

/// Where a Settled claim's evidence was found: the owner's own turn, the idea statement, or only
/// a foil (assistant) turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    Owner,
    Idea,
    Foil,
}

impl Provenance {
    pub fn label(self) -> &'static str {
        match self {
            Provenance::Owner => "you",
            Provenance::Idea => "idea",
            Provenance::Foil => "foil",
        }
    }
}

/// Split a trailing `— you` / `— foil` / `— idea` label off an item's text.
fn split_provenance(text: &str) -> (String, Option<Provenance>) {
    for p in [Provenance::Owner, Provenance::Idea, Provenance::Foil] {
        for dash in ["—", "-", "–"] {
            if let Some(rest) = text.strip_suffix(&format!(" {dash} {}", p.label())) {
                return (rest.trim_end().to_string(), Some(p));
            }
        }
    }
    (text.to_string(), None)
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

/// The next free id for `letter` among `items` (one past the highest number in use).
fn next_id(items: &[Item], letter: char) -> String {
    let max = items
        .iter()
        .filter_map(|i| i.id.strip_prefix(letter)?.parse::<usize>().ok())
        .max()
        .unwrap_or(0);
    format!("{letter}{}", max + 1)
}

impl BuildPlan {
    /// Move `item` to Open questions as a proposal, with a marker saying why it was opened.
    pub fn open_from(&mut self, mut item: Item, marker: impl Into<String>) {
        item.id = next_id(&self.open, 'Q');
        if !item.text.starts_with("proposed:") {
            item.text = format!("proposed: {}", item.text);
        }
        item.markers.push(marker.into());
        self.open.push(item);
    }

    /// Move `item` to Quarantined with the reason it must not be built on.
    pub fn quarantine(&mut self, mut item: Item, reason: impl Into<String>) {
        item.id = next_id(&self.quarantined, 'X');
        item.fields.insert("reason".to_string(), reason.into());
        self.quarantined.push(item);
    }

    /// Move `item` to Verify first, with a generated read-only `check` (if any) and a marker.
    pub fn verify_first(
        &mut self,
        mut item: Item,
        check: Option<String>,
        marker: impl Into<String>,
    ) {
        item.id = next_id(&self.verify, 'P');
        if let Some(check) = check {
            item.fields.insert("check".to_string(), check);
        }
        item.markers.push(marker.into());
        self.verify.push(item);
    }
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
        let (text, provenance) = split_provenance(&text);
        let mut item = Item::new(&id, &text);
        item.needs_owner = owner;
        item.provenance = provenance;
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
    parse_inner(answer, false)
}

/// Parse a stored build-plan artifact body. Unlike [`parse`] it reads the code-owned
/// `## Quarantined` section back (each item with its `reason`); the two italic header lines and
/// any `> note` lines before `## Goal` are dropped. Never call it on a model answer.
pub fn parse_artifact(body: &str) -> Result<BuildPlan, Unusable> {
    parse_inner(body, true)
}

fn parse_inner(answer: &str, read_quarantine: bool) -> Result<BuildPlan, Unusable> {
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
            Section::Quarantined if read_quarantine => {
                plan.quarantined.extend(parse_items(body, *section));
            }
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
    if let Some(p) = item.provenance {
        out.push_str(&format!(" — {}", p.label()));
    }
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

fn push_item(out: &mut String, item: &Item, box_: &str) {
    out.push_str(&format!("- {box_}{}: {}", item.id, item.text));
    for m in &item.markers {
        out.push_str(&format!(" ⟨{m}⟩"));
    }
    out.push('\n');
}

fn push_fields(out: &mut String, item: &Item, keys: &[&str]) {
    for key in keys {
        if let Some(v) = item.field(key) {
            out.push_str(&format!("  {key}: {v}\n"));
        }
    }
}

fn prompt_section(out: &mut String, heading: &str, items: &[Item], keys: &[&str], none: bool) {
    if items.is_empty() && !none {
        return;
    }
    out.push_str(&format!("\n{heading}\n"));
    if items.is_empty() {
        out.push_str("- none\n");
    }
    for item in items {
        push_item(out, item, "");
        push_fields(out, item, keys);
    }
}

/// Project a plan to a `PROMPT.md` any coding agent can follow: owner pins, foil conclusions to
/// confirm, fence, bootstrap checks, owner questions, tasks, kill criteria and the quarantined
/// claims not to build on. Empty sections are omitted except the two settled ones.
pub fn render_prompt(plan: &BuildPlan, idea_title: &str, stem: &str) -> String {
    let goal = plan.goal.lines().next().unwrap_or("").trim();
    let mut out = format!("# Build: {goal}\n\n_idea: {idea_title} · plan: {stem}_\n");
    let (pinned, foil): (Vec<Item>, Vec<Item>) = plan
        .settled
        .iter()
        .cloned()
        .partition(|i| matches!(i.provenance, Some(Provenance::Owner | Provenance::Idea)));
    prompt_section(
        &mut out,
        "## PINNED — the owner said it",
        &pinned,
        &["quote"],
        true,
    );
    prompt_section(
        &mut out,
        "## Foil conclusions — confirm at bootstrap",
        &foil,
        &["quote"],
        true,
    );
    prompt_section(&mut out, "## Fence", &plan.fence, &[], false);
    prompt_section(
        &mut out,
        "## Bootstrap checks — a failed check stops the run",
        &plan.verify,
        &["check"],
        false,
    );
    prompt_section(
        &mut out,
        "## Ask the owner before building past these",
        &plan.open,
        &[],
        false,
    );
    if !plan.tasks.is_empty() {
        out.push_str("\n## Plan\n");
        for t in &plan.tasks {
            push_item(&mut out, t, if t.needs_owner { "[?] " } else { "[ ] " });
            push_fields(&mut out, t, &["depends", "touches", "accept"]);
        }
    }
    prompt_section(
        &mut out,
        "## Kill criteria",
        &plan.kills,
        &["checked by", "gates"],
        false,
    );
    prompt_section(
        &mut out,
        "## Do not build on",
        &plan.quarantined,
        &["reason"],
        false,
    );
    out
}

fn table_cell(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|")
}

/// Project a plan to an `/attack`-style `@plan.md`: one table row per task with `score` and
/// `model` left as `?`, `[?]` marking owner tasks a loop must never auto-select, and an empty
/// `## Log`.
pub fn render_attack_plan(plan: &BuildPlan) -> String {
    let mut out = String::from(
        "| [ ] | T | Task | Depends | score | model | accept |\n|---|---|---|---|---|---|---|\n",
    );
    for t in &plan.tasks {
        let depends = t.list("depends").join(", ");
        out.push_str(&format!(
            "| {} | {} | {} | {} | ? | ? | {} |\n",
            if t.needs_owner { "[?]" } else { "[ ]" },
            table_cell(&t.id),
            table_cell(&t.text),
            table_cell(&depends),
            table_cell(t.field("accept").unwrap_or("")),
        ));
    }
    out.push_str("\n## Log\n");
    out
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

    const TEMPLATE: &str = include_str!("../skills/build-prompt.md");

    const SECTION_NAMES: [&str; 6] = [
        "Goal",
        "Settled",
        "Verify first",
        "Open questions",
        "Plan",
        "Kill criteria",
    ];

    fn template_body() -> &'static str {
        TEMPLATE.splitn(3, "---\n").nth(2).unwrap_or(TEMPLATE)
    }

    fn template_example() -> &'static str {
        let after = template_body().split("\nExample:\n").nth(1).unwrap_or("");
        let end = after
            .find("\nDiscussion:")
            .or_else(|| after.find("{context}"))
            .unwrap_or(after.len());
        &after[..end]
    }

    #[test]
    fn template_example_round_trips_every_field() {
        let plan = parse(template_example()).expect("the template example must parse");
        assert_eq!(plan.missing, ["## Verify first", "## Kill criteria"]);
        assert!(!plan.goal.is_empty(), "the example's goal was lost");
        assert!(plan.settled.iter().all(|s| s.field("quote").is_some()));
        assert!(!plan.open.is_empty());
        assert!(plan.tasks.len() >= 2);
        for task in &plan.tasks {
            assert!(!task.list("touches").is_empty(), "{} lost touches", task.id);
            assert!(task.field("accept").is_some(), "{} lost accept", task.id);
            assert!(
                task.fields.contains_key("depends"),
                "{} lost depends",
                task.id
            );
        }
        assert_eq!(plan.tasks[1].list("depends"), ["T1"]);
    }

    #[test]
    fn template_headings_stand_alone() {
        for line in template_body().lines().filter(|l| l.starts_with("## ")) {
            let name = line.trim_start_matches("## ");
            assert!(
                SECTION_NAMES.contains(&name),
                "heading with extra text: {line:?}"
            );
        }
        assert!(template_body().contains("\n## Goal\n"));
    }

    #[test]
    fn template_stays_under_its_byte_cap() {
        let bytes = template_body().len();
        assert!(
            bytes <= 1800,
            "template body is {bytes} bytes; the cap is 1800"
        );
    }

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
    fn provenance_renders_as_a_label_and_parses_back() {
        let mut plan = parse(PLAN).unwrap();
        plan.settled[0].provenance = Some(Provenance::Owner);
        plan.settled[1].provenance = Some(Provenance::Foil);
        let rendered = render(&plan);
        assert!(
            rendered.contains("- S2: Close is never gated like open. — foil\n"),
            "{rendered}"
        );
        assert_eq!(parse(&rendered).unwrap(), plan);
    }

    #[test]
    fn moved_items_take_the_next_free_id_in_their_section() {
        let mut plan = parse(PLAN).unwrap();
        let s2 = plan.settled.remove(1);
        plan.open_from(s2, "opened: listed in the open-questions artifact");
        let s1 = plan.settled.remove(0);
        plan.quarantine(s1.clone(), "quote not in the discussion");
        plan.verify_first(s1, Some("`grep -rnF BOCPD .`".into()), "foil-coined");
        assert_eq!(plan.open[1].id, "Q2");
        assert_eq!(
            plan.open[1].text,
            "proposed: Close is never gated like open."
        );
        assert_eq!(
            plan.open[1].markers,
            ["opened: listed in the open-questions artifact"]
        );
        assert_eq!(plan.quarantined[0].id, "X1");
        assert_eq!(
            plan.quarantined[0].field("reason"),
            Some("quote not in the discussion")
        );
        assert_eq!(plan.verify[1].id, "P2");
        assert_eq!(plan.verify[1].field("check"), Some("`grep -rnF BOCPD .`"));
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

    fn projected() -> BuildPlan {
        let mut plan = parse(PLAN).unwrap();
        plan.settled[0].provenance = Some(Provenance::Owner);
        plan.settled[1].provenance = Some(Provenance::Foil);
        plan.quarantine(
            Item::new("", "The owner chose freeze at entry"),
            "quote not in the discussion",
        );
        plan
    }

    #[test]
    fn projection_splits_owner_pins_from_foil_conclusions() {
        let prompt = render_prompt(&projected(), "Trader", "20260928-build-plan");
        let pinned = prompt.find("## PINNED — the owner said it").unwrap();
        let foil = prompt
            .find("## Foil conclusions — confirm at bootstrap")
            .unwrap();
        let bocpd = prompt.find("S1: BOCPD").unwrap();
        let close = prompt.find("S2: Close is never").unwrap();
        assert!(pinned < bocpd && bocpd < foil && foil < close, "{prompt}");
        assert!(
            prompt.starts_with("# Build: Run the cheapest disproof"),
            "{prompt}"
        );
        assert!(prompt.contains("Trader") && prompt.contains("20260928-build-plan"));
        assert!(prompt.contains("quote: \"going forward with BOCPD"));
        assert!(prompt.contains("- [?] T1:") && prompt.contains("- [ ] T2:"));
        assert!(prompt.contains("  accept: `python backtest/run.py"));
        assert!(prompt.contains("## Bootstrap checks — a failed check stops the run"));
        assert!(!prompt.contains("## Fence"), "empty sections are omitted");
    }

    #[test]
    fn projection_lists_quarantined_items_as_do_not_build_on() {
        let prompt = render_prompt(&projected(), "Trader", "stem");
        let at = prompt.find("## Do not build on").unwrap();
        let tail = &prompt[at..];
        assert!(tail.contains("The owner chose freeze at entry"), "{tail}");
        assert!(
            tail.contains("reason: quote not in the discussion"),
            "{tail}"
        );
        let none = render_prompt(&parse(PLAN).unwrap(), "T", "s");
        assert!(!none.contains("## Do not build on"));
    }

    #[test]
    fn projection_attack_plan_marks_owner_tasks_and_leaves_score_unset() {
        let mut plan = parse(PLAN).unwrap();
        plan.tasks[1]
            .fields
            .insert("accept".into(), "`a | b` → ok".into());
        let out = render_attack_plan(&plan);
        assert!(out.starts_with("| [ ] | T | Task | Depends | score | model | accept |\n"));
        assert!(
            out.contains("| [?] | T1 | Write SPEC.md with a dated kill criterion |  | ? | ? |  |"),
            "{out}"
        );
        assert!(
            out.contains("| [ ] | T2 | Backtest SPEC.md at a pessimistic spread | T1 | ? | ? | `a \\| b` → ok |"),
            "{out}"
        );
        assert!(out.ends_with("\n## Log\n"));
    }

    #[test]
    fn projection_parse_artifact_reads_quarantine_back() {
        let plan = projected();
        let body = format!(
            "# Build plan — Trader\n_quick · m · 2026-09-28 21:40_\n_gates: settled 2_\n\n> a note\n\n{}\n",
            render(&plan)
        );
        let back = parse_artifact(&body).unwrap();
        assert_eq!(back.quarantined.len(), 1);
        assert_eq!(back.quarantined[0].text, "The owner chose freeze at entry");
        assert_eq!(
            back.quarantined[0].field("reason"),
            Some("quote not in the discussion")
        );
        assert_eq!(back.goal, plan.goal);
        assert!(parse(&body).unwrap().quarantined.is_empty());
    }
}
