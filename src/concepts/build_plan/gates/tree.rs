//! G14 tree lint and derivation: the task graph is checked as a whole — unique ids, resolving
//! references, no self-edge, named cycles, `[?]` inheritance, no dependency on a Quarantined
//! claim, every kill row gating a real task — and each task gets its code-owned `score` (DBVKC
//! digits), `model` and `wave` fields. Pure: it reads only the plan.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::leaf::{is_test_path, roots, COMPOUND_ACCEPT, NO_COUNT};
use super::GateReport;
use crate::concepts::build_plan::plan::{refs_of, BuildPlan, Item};
use crate::domain::evidence::content_overlap;

pub const BLOCKED_BY: &str = "blocked by";
pub const CYCLE: &str = "cycle:";
pub const QUARANTINED_DEP: &str = "depends on quarantined";
pub const GATES_NO_TASK: &str = "gates no task";
pub const SELF_EDGE: &str = "self dependency dropped";
pub const SPLIT_SCORE: &str = "split before building";
pub const REVIEW: &str = "review@opus";

const UNKNOWN_DEP: &str = "unknown dependency ";
const NO_RUNNABLE: &str = "no runnable accept";
const TOUCHES_FENCED: &str = "touches fenced ";
const REFUTED_UPSTREAM: &str = "refuted upstream";

const WAVE_CAP: usize = 3;
const WAVE_CAP_REVIEWED: usize = 2;
const MAX_READS_AND_TOUCHES: usize = 6;
const QUARANTINE_RATIO: f64 = 0.6;
const QUARANTINE_SHARED: usize = 3;

/// The DBVKC score of one task: open decision, breadth, no verifiable accept, gate surface,
/// context size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Score {
    pub d: bool,
    pub b: bool,
    pub v: bool,
    pub k: bool,
    pub c: bool,
}

impl Score {
    pub fn digits(self) -> String {
        [self.d, self.b, self.v, self.k, self.c]
            .iter()
            .map(|on| if *on { '1' } else { '0' })
            .collect()
    }

    pub fn sum(self) -> usize {
        [self.d, self.b, self.v, self.k, self.c]
            .iter()
            .filter(|on| **on)
            .count()
    }

    /// `sonnet` for 0–1, `opus` for 2–5, plus `review@opus` when the gate surface is touched.
    pub fn model(self) -> String {
        let tier = if self.sum() <= 1 { "sonnet" } else { "opus" };
        if self.k {
            format!("{tier} + {REVIEW}")
        } else {
            tier.to_string()
        }
    }
}

/// Lint the task graph and write each task's derived fields; tally `blocked_by_owner` per task
/// that inherits `[?]` and `waves` as the number of waves.
pub fn apply(plan: &mut BuildPlan, report: &mut GateReport) {
    for items in [
        &mut plan.settled,
        &mut plan.verify,
        &mut plan.open,
        &mut plan.tasks,
        &mut plan.kills,
        &mut plan.fence,
    ] {
        unique_ids(items, report);
    }
    resolve_refs(plan, report);
    kill_targets(plan, report);
    name_cycles(plan, report);
    quarantine_deps(plan, report);
    inherit_owner(plan, report);
    let scores = derive_scores(plan);
    derive_waves(plan, &scores, report);
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

fn note_once(report: &mut GateReport, note: String) {
    if !report.notes.contains(&note) {
        report.note(note);
    }
}

fn unique_ids(items: &mut [Item], report: &mut GateReport) {
    let mut seen = BTreeSet::new();
    for i in 0..items.len() {
        if seen.insert(items[i].id.clone()) {
            continue;
        }
        let old = items[i].id.clone();
        let letter = old.chars().next().unwrap_or('X');
        let max = items
            .iter()
            .filter_map(|it| it.id.strip_prefix(letter)?.parse::<usize>().ok())
            .max()
            .unwrap_or(0);
        let new = format!("{letter}{}", max + 1);
        items[i].id = new.clone();
        seen.insert(new.clone());
        report.note(format!("duplicate id {old}: the later one is now {new}"));
        report.count("repaired");
    }
}

fn ids(items: &[Item]) -> BTreeSet<String> {
    items.iter().map(|i| i.id.clone()).collect()
}

fn resolve_refs(plan: &mut BuildPlan, report: &mut GateReport) {
    let (tasks, premises, questions) = (ids(&plan.tasks), ids(&plan.verify), ids(&plan.open));
    for task in &mut plan.tasks {
        let own = task.id.clone();
        let deps = task.depends_tasks();
        let self_edge = deps.contains(&own);
        let keep_t: Vec<String> = deps
            .iter()
            .filter(|d| **d != own && tasks.contains(*d))
            .cloned()
            .collect();
        let keep_p: Vec<String> = task
            .depends_premises()
            .into_iter()
            .filter(|p| premises.contains(p))
            .collect();
        let keep_q: Vec<String> = task
            .depends_questions()
            .into_iter()
            .filter(|q| questions.contains(q))
            .collect();
        let dangling: Vec<String> = deps
            .iter()
            .filter(|d| **d != own && !tasks.contains(*d))
            .cloned()
            .chain(
                task.depends_premises()
                    .into_iter()
                    .filter(|p| !premises.contains(p)),
            )
            .chain(
                task.depends_questions()
                    .into_iter()
                    .filter(|q| !questions.contains(q)),
            )
            .collect();
        if self_edge || !dangling.is_empty() {
            let mut refs = [keep_t, keep_p, keep_q].concat();
            refs.extend(task.depends_free());
            if refs.is_empty() {
                task.fields.remove("depends");
            } else {
                task.fields.insert("depends".to_string(), refs.join(", "));
            }
        }
        if self_edge {
            mark(task, SELF_EDGE);
            note_once(report, format!("{own}: dropped its dependency on itself"));
            report.count("repaired");
        }
        for d in &dangling {
            mark(task, format!("{UNKNOWN_DEP}{d}"));
            report.count("repaired");
        }
        let dropped: Vec<String> = task
            .markers
            .iter()
            .filter_map(|m| m.strip_prefix(UNKNOWN_DEP))
            .map(str::to_string)
            .collect();
        for d in dropped {
            note_once(report, format!("{own}: dropped dangling reference {d}"));
        }
    }
}

fn kill_targets(plan: &mut BuildPlan, report: &mut GateReport) {
    let tasks = ids(&plan.tasks);
    for kill in &mut plan.kills {
        for key in ["gates", "checked by"] {
            let Some(value) = kill.field(key) else {
                continue;
            };
            let refs = refs_of(value, 'T');
            let (kept, dangling): (Vec<String>, Vec<String>) =
                refs.into_iter().partition(|t| tasks.contains(t));
            if dangling.is_empty() {
                continue;
            }
            if kept.is_empty() {
                kill.fields.remove(key);
            } else {
                kill.fields.insert(key.to_string(), kept.join(", "));
            }
            for d in dangling {
                note_once(
                    report,
                    format!("{}: dropped dangling reference {d}", kill.id),
                );
                report.count("repaired");
            }
        }
        if kill
            .field("gates")
            .is_none_or(|g| refs_of(g, 'T').is_empty())
        {
            mark(kill, GATES_NO_TASK);
            note_once(report, format!("{} gates no task", kill.id));
        }
    }
}

/// Each task's `depends` edges as indexes into `plan.tasks`.
fn edges(plan: &BuildPlan) -> Vec<Vec<usize>> {
    let index: BTreeMap<&str, usize> = plan
        .tasks
        .iter()
        .enumerate()
        .map(|(i, t)| (t.id.as_str(), i))
        .collect();
    plan.tasks
        .iter()
        .map(|t| {
            t.depends_tasks()
                .iter()
                .filter_map(|d| index.get(d.as_str()).copied())
                .collect()
        })
        .collect()
}

fn reachable(edges: &[Vec<usize>], from: usize) -> BTreeSet<usize> {
    let mut seen = BTreeSet::new();
    let mut stack = edges[from].clone();
    while let Some(n) = stack.pop() {
        if seen.insert(n) {
            stack.extend(edges[n].iter().copied());
        }
    }
    seen
}

/// A path through `members` from `start` back to it, as indexes, start repeated at the end.
fn cycle_path(edges: &[Vec<usize>], members: &BTreeSet<usize>, start: usize) -> Vec<usize> {
    let mut parent: BTreeMap<usize, usize> = BTreeMap::new();
    let mut queue = VecDeque::from([start]);
    while let Some(u) = queue.pop_front() {
        for &v in &edges[u] {
            if !members.contains(&v) {
                continue;
            }
            if v == start {
                let mut path = vec![start];
                let mut at = u;
                while at != start {
                    path.push(at);
                    at = parent[&at];
                }
                path.push(start);
                path.reverse();
                return path;
            }
            if let std::collections::btree_map::Entry::Vacant(e) = parent.entry(v) {
                e.insert(u);
                queue.push_back(v);
            }
        }
    }
    vec![start, start]
}

fn name_cycles(plan: &mut BuildPlan, report: &mut GateReport) {
    let edges = edges(plan);
    let reach: Vec<BTreeSet<usize>> = (0..edges.len()).map(|i| reachable(&edges, i)).collect();
    let mut named: BTreeSet<BTreeSet<usize>> = BTreeSet::new();
    for i in 0..edges.len() {
        if !reach[i].contains(&i) {
            continue;
        }
        let members: BTreeSet<usize> = reach[i]
            .iter()
            .copied()
            .filter(|j| reach[*j].contains(&i))
            .collect();
        if !named.insert(members.clone()) {
            continue;
        }
        let name = cycle_path(&edges, &members, i)
            .iter()
            .map(|n| plan.tasks[*n].id.clone())
            .collect::<Vec<_>>()
            .join(" → ");
        for &m in &members {
            mark(&mut plan.tasks[m], format!("{CYCLE} {name}"));
            need_owner(&mut plan.tasks[m], report);
        }
        note_once(report, format!("dependency cycle: {name}"));
    }
}

fn quarantine_deps(plan: &mut BuildPlan, report: &mut GateReport) {
    for task in &mut plan.tasks {
        let entries: Vec<String> = task
            .field("depends")
            .unwrap_or_default()
            .split(',')
            .map(|e| e.trim().to_string())
            .filter(|e| !e.is_empty())
            .collect();
        for q in &plan.quarantined {
            let was = q.field("was");
            let cited = entries.iter().any(|e| {
                e.split(|c: char| !c.is_alphanumeric()).any(|w| {
                    w.eq_ignore_ascii_case(&q.id) || was.is_some_and(|x| w.eq_ignore_ascii_case(x))
                }) || content_overlap(e, &q.text).at_least(QUARANTINE_RATIO, QUARANTINE_SHARED)
            });
            if cited {
                mark(task, format!("{QUARANTINED_DEP} {}", q.id));
                need_owner(task, report);
                note_once(
                    report,
                    format!("{}: depends on quarantined {}", task.id, q.id),
                );
            }
        }
    }
}

fn inherit_owner(plan: &mut BuildPlan, report: &mut GateReport) {
    let edges = edges(plan);
    loop {
        let mut changed = false;
        for (i, deps) in edges.iter().enumerate() {
            if plan.tasks[i].needs_owner {
                continue;
            }
            let Some(&blocker) = deps.iter().find(|&&d| plan.tasks[d].needs_owner) else {
                continue;
            };
            let by = plan.tasks[blocker].id.clone();
            mark(&mut plan.tasks[i], format!("{BLOCKED_BY} {by}"));
            need_owner(&mut plan.tasks[i], report);
            report.count("blocked_by_owner");
            changed = true;
        }
        if !changed {
            break;
        }
    }
}

fn runnable_accept(accept: &str) -> bool {
    let Some(rest) = accept.trim().strip_prefix('`') else {
        return false;
    };
    let Some((cmd, rest)) = rest.split_once('`') else {
        return false;
    };
    let condition = rest
        .split_once('→')
        .or_else(|| rest.split_once("->"))
        .map(|(_, c)| c);
    !cmd.trim().is_empty() && condition.is_some_and(|c| !c.trim().is_empty())
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

fn fence_paths(plan: &BuildPlan) -> Vec<String> {
    let mut out = Vec::new();
    for item in &plan.fence {
        let mut texts = vec![item.text.as_str()];
        texts.extend(item.fields.values().map(String::as_str));
        let spans: Vec<String> = texts
            .iter()
            .flat_map(|t| t.split('`').skip(1).step_by(2))
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.contains(char::is_whitespace))
            .map(str::to_string)
            .collect();
        if spans.is_empty() && !item.text.trim().contains(char::is_whitespace) {
            out.push(item.text.trim().to_string());
        }
        out.extend(spans);
    }
    out
}

/// A path on the generic gate surface: migrations, schemas, lockfiles, auth, env files, generated
/// code, container and manifest files.
fn gate_surface(path: &str) -> bool {
    let lower = norm_path(path).to_lowercase();
    let name = lower.rsplit('/').next().unwrap_or_default();
    lower
        .split('/')
        .any(|seg| matches!(seg, "migrations" | "generated"))
        || lower.contains("schema")
        || name.ends_with(".lock")
        || lower
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|w| matches!(w, "auth" | "oauth" | "authn" | "authz"))
        || name.starts_with(".env")
        || name.starts_with("dockerfile")
        || name.starts_with("docker-compose")
        || matches!(name, "cargo.toml" | "package.json")
}

/// The DBVKC score of `task` within `plan`.
pub fn score(plan: &BuildPlan, task: &Item) -> Score {
    let open = ids(&plan.open);
    let fence = fence_paths(plan);
    let touches = task.list("touches");
    let non_test = touches.iter().filter(|p| !is_test_path(p)).count();
    let marked = |prefix: &str| task.markers.iter().any(|m| m.starts_with(prefix));
    Score {
        d: task.depends_questions().iter().any(|q| open.contains(q))
            || marked(REFUTED_UPSTREAM)
            || marked(QUARANTINED_DEP),
        b: touches.is_empty() || non_test > 1 || roots(&touches).len() > 1,
        v: !task.field("accept").is_some_and(runnable_accept)
            || marked(NO_RUNNABLE)
            || marked(COMPOUND_ACCEPT)
            || marked(NO_COUNT),
        k: marked(TOUCHES_FENCED)
            || touches
                .iter()
                .any(|p| gate_surface(p) || fence.iter().any(|f| paths_overlap(p, f))),
        c: task.list("reads").len() + touches.len() > MAX_READS_AND_TOUCHES,
    }
}

fn derive_scores(plan: &mut BuildPlan) -> Vec<Score> {
    let scores: Vec<Score> = plan.tasks.iter().map(|t| score(plan, t)).collect();
    for (task, s) in plan.tasks.iter_mut().zip(&scores) {
        task.fields.insert("score".to_string(), s.digits());
        task.fields.insert("model".to_string(), s.model());
        if s.sum() >= 4 {
            mark(task, format!("score {} of 5: {SPLIT_SCORE}", s.sum()));
        }
    }
    scores
}

/// Topological layers over the ready tasks: a task lands in the first wave after all its
/// dependencies whose members share no touched path with it and that holds fewer than
/// [`WAVE_CAP`] tasks ([`WAVE_CAP_REVIEWED`] once any member touches the gate surface). Owner
/// tasks and tasks on a cycle get no wave.
fn derive_waves(plan: &mut BuildPlan, scores: &[Score], report: &mut GateReport) {
    let edges = edges(plan);
    let n = plan.tasks.len();
    let mut indegree: Vec<usize> = edges.iter().map(Vec::len).collect();
    let mut queue: VecDeque<usize> = (0..n).filter(|&i| indegree[i] == 0).collect();
    let mut order = Vec::new();
    while let Some(i) = queue.pop_front() {
        order.push(i);
        for j in 0..n {
            if edges[j].contains(&i) {
                indegree[j] -= 1;
                if indegree[j] == 0 {
                    queue.push_back(j);
                }
            }
        }
    }
    let touches: Vec<Vec<String>> = plan.tasks.iter().map(|t| t.list("touches")).collect();
    let mut wave_of: Vec<Option<usize>> = vec![None; n];
    let mut members: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in order {
        if plan.tasks[i].needs_owner {
            continue;
        }
        let Some(after) = edges[i]
            .iter()
            .map(|d| wave_of[*d])
            .try_fold(0, |acc, w| w.map(|w| acc.max(w)))
        else {
            continue;
        };
        let mut wave = after + 1;
        loop {
            let here = members.get(&wave).map(Vec::as_slice).unwrap_or_default();
            let reviewed = scores[i].k || here.iter().any(|m| scores[*m].k);
            let cap = if reviewed {
                WAVE_CAP_REVIEWED
            } else {
                WAVE_CAP
            };
            let clash = here.iter().any(|m| {
                touches[*m]
                    .iter()
                    .any(|a| touches[i].iter().any(|b| paths_overlap(a, b)))
            });
            if here.len() < cap && !clash {
                break;
            }
            wave += 1;
        }
        wave_of[i] = Some(wave);
        members.entry(wave).or_default().push(i);
    }
    for (task, wave) in plan.tasks.iter_mut().zip(&wave_of) {
        match wave {
            Some(w) => task.fields.insert("wave".to_string(), w.to_string()),
            None => task.fields.remove("wave"),
        };
    }
    if let Some(&last) = members.keys().next_back() {
        report.tally.insert("waves", last);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCEPT: &str = "`cargo test --quiet x` → exit 0, ≥3 passed";

    fn task(id: &str, fields: &[(&str, &str)]) -> Item {
        let mut it = Item::new(id, &format!("Add part {id}"));
        it.fields.insert("accept".into(), ACCEPT.into());
        for (k, v) in fields {
            it.fields.insert((*k).to_string(), (*v).to_string());
        }
        it
    }

    fn plan(tasks: Vec<Item>) -> BuildPlan {
        BuildPlan {
            goal: "Ship it".into(),
            tasks,
            ..BuildPlan::default()
        }
    }

    fn run(plan: &mut BuildPlan) -> GateReport {
        let mut report = GateReport::default();
        apply(plan, &mut report);
        report
    }

    fn get<'a>(plan: &'a BuildPlan, id: &str) -> &'a Item {
        plan.tasks.iter().find(|t| t.id == id).expect("task")
    }

    fn wave<'a>(plan: &'a BuildPlan, id: &str) -> Option<&'a str> {
        get(plan, id).field("wave")
    }

    #[test]
    fn a_dangling_reference_is_dropped_with_a_note() {
        let mut p = plan(vec![
            task("T1", &[("touches", "src/a.rs")]),
            task("T2", &[("touches", "src/b.rs"), ("depends", "T1, T9, Q4")]),
        ]);
        let report = run(&mut p);
        assert_eq!(get(&p, "T2").field("depends"), Some("T1"));
        assert!(
            report
                .notes
                .contains(&"T2: dropped dangling reference T9".to_string()),
            "{:?}",
            report.notes
        );
        assert!(report
            .notes
            .contains(&"T2: dropped dangling reference Q4".to_string()));
    }

    #[test]
    fn a_self_edge_and_a_duplicate_id_are_repaired() {
        let mut p = plan(vec![
            task("T1", &[("touches", "src/a.rs"), ("depends", "T1")]),
            task("T1", &[("touches", "src/b.rs")]),
        ]);
        let report = run(&mut p);
        assert_eq!(p.tasks[1].id, "T2");
        assert_eq!(p.tasks[0].field("depends"), None);
        assert!(p.tasks[0].markers.contains(&SELF_EDGE.to_string()));
        assert!(report.notes.iter().any(|n| n.contains("duplicate id T1")));
    }

    #[test]
    fn a_cycle_is_named_and_needs_the_owner() {
        let mut p = plan(vec![
            task("T1", &[("touches", "src/a.rs"), ("depends", "T3")]),
            task("T2", &[("touches", "src/b.rs"), ("depends", "T1")]),
            task("T3", &[("touches", "src/c.rs"), ("depends", "T2")]),
        ]);
        let report = run(&mut p);
        assert!(
            report
                .notes
                .contains(&"dependency cycle: T1 → T3 → T2 → T1".to_string()),
            "{:?}",
            report.notes
        );
        for id in ["T1", "T2", "T3"] {
            assert!(get(&p, id).needs_owner, "{id}");
            assert!(get(&p, id).markers.iter().any(|m| m.starts_with(CYCLE)));
            assert_eq!(wave(&p, id), None);
        }
    }

    #[test]
    fn a_task_on_an_owner_task_inherits_the_block_transitively() {
        let mut owner = task("T1", &[("touches", "src/a.rs")]);
        owner.needs_owner = true;
        let mut p = plan(vec![
            owner,
            task("T2", &[("touches", "src/b.rs"), ("depends", "T1")]),
            task("T3", &[("touches", "src/c.rs"), ("depends", "T2")]),
            task("T4", &[("touches", "src/d.rs")]),
        ]);
        let report = run(&mut p);
        assert!(get(&p, "T2").needs_owner);
        assert!(get(&p, "T2").markers.contains(&"blocked by T1".to_string()));
        assert!(get(&p, "T3").markers.contains(&"blocked by T2".to_string()));
        assert!(!get(&p, "T4").needs_owner);
        assert_eq!(report.tally.get("blocked_by_owner"), Some(&2));
    }

    #[test]
    fn a_task_depending_on_a_quarantined_claim_needs_the_owner() {
        let mut p = plan(vec![task(
            "T1",
            &[
                ("touches", "src/a.rs"),
                ("depends", "the owner chose freeze at entry"),
            ],
        )]);
        p.quarantine(
            Item::new("S1", "The owner chose freeze at entry"),
            "claimed quote is not in the discussion",
        );
        run(&mut p);
        let t1 = get(&p, "T1");
        assert!(t1.needs_owner);
        assert!(t1.markers.contains(&format!("{QUARANTINED_DEP} X1")));
        assert_eq!(t1.field("score").map(|s| &s[..1]), Some("1"));
    }

    #[test]
    fn a_task_citing_a_quarantined_items_original_id_needs_the_owner() {
        let mut p = plan(vec![task(
            "T1",
            &[("touches", "src/a.rs"), ("depends", "S1 (the entry rule)")],
        )]);
        p.quarantine(
            Item::new("S1", "Freeze applies before the first fill"),
            "claimed quote is not in the discussion",
        );
        run(&mut p);
        let t1 = get(&p, "T1");
        assert!(t1.needs_owner, "{:?}", t1.markers);
        assert!(t1.markers.contains(&format!("{QUARANTINED_DEP} X1")));
    }

    #[test]
    fn a_depends_entry_sharing_two_words_with_a_quarantined_claim_is_not_flagged() {
        let mut p = plan(vec![task(
            "T1",
            &[("touches", "src/a.rs"), ("depends", "freeze entry rule")],
        )]);
        p.quarantine(
            Item::new("S1", "The owner chose freeze at entry"),
            "claimed quote is not in the discussion",
        );
        run(&mut p);
        let t1 = get(&p, "T1");
        assert!(!t1.needs_owner, "{:?}", t1.markers);
        assert!(!t1.markers.iter().any(|m| m.starts_with(QUARANTINED_DEP)));
    }

    #[test]
    fn a_kill_row_must_gate_a_real_task() {
        let mut p = plan(vec![task("T1", &[("touches", "src/a.rs")])]);
        let mut k = Item::new("K1", "Stop if the probe is slow");
        k.fields.insert("gates".into(), "T7".into());
        k.fields.insert("checked by".into(), "T1".into());
        p.kills.push(k);
        let report = run(&mut p);
        assert!(p.kills[0].markers.contains(&GATES_NO_TASK.to_string()));
        assert_eq!(p.kills[0].field("gates"), None);
        assert!(report
            .notes
            .contains(&"K1: dropped dangling reference T7".to_string()));
    }

    #[test]
    fn disjoint_tasks_share_a_wave_and_dependents_follow() {
        let mut p = plan(vec![
            task("T1", &[("touches", "src/a.rs")]),
            task("T2", &[("touches", "src/b.rs")]),
            task("T3", &[("touches", "src/c.rs"), ("depends", "T1")]),
        ]);
        let report = run(&mut p);
        assert_eq!(wave(&p, "T1"), Some("1"));
        assert_eq!(wave(&p, "T2"), Some("1"));
        assert_eq!(wave(&p, "T3"), Some("2"));
        assert_eq!(report.tally.get("waves"), Some(&2));
    }

    #[test]
    fn overlapping_touches_push_a_task_to_the_next_wave() {
        let mut p = plan(vec![
            task("T1", &[("touches", "src/web")]),
            task("T2", &[("touches", "src/web/routes.rs")]),
            task("T3", &[("touches", "src/c.rs")]),
        ]);
        run(&mut p);
        assert_eq!(wave(&p, "T1"), Some("1"));
        assert_eq!(wave(&p, "T2"), Some("2"));
        assert_eq!(wave(&p, "T3"), Some("1"));
    }

    #[test]
    fn a_wave_holds_three_tasks_or_two_on_the_gate_surface() {
        let ids = ["T1", "T2", "T3", "T4"];
        let mut p = plan(
            ids.iter()
                .map(|id| task(id, &[("touches", &format!("src/{id}.rs"))]))
                .collect(),
        );
        run(&mut p);
        let waves: Vec<_> = ids.iter().map(|id| wave(&p, id)).collect();
        assert_eq!(waves, [Some("1"), Some("1"), Some("1"), Some("2")]);

        let mut p = plan(vec![
            task("T1", &[("touches", "migrations/001.sql")]),
            task("T2", &[("touches", "src/b.rs")]),
            task("T3", &[("touches", "src/c.rs")]),
        ]);
        run(&mut p);
        let waves: Vec<_> = ["T1", "T2", "T3"].iter().map(|id| wave(&p, id)).collect();
        assert_eq!(waves, [Some("1"), Some("1"), Some("2")]);
    }

    #[test]
    fn dbvkc_digits_are_derived_from_the_task() {
        let mut p = plan(vec![task(
            "T1",
            &[
                ("touches", "src/a.rs, docs/a.md, Cargo.toml"),
                ("reads", "a, b, c, d"),
                ("depends", "Q1"),
                ("accept", "run the tests"),
            ],
        )]);
        p.open.push(Item::new("Q1", "Freeze at entry or dwell?"));
        run(&mut p);
        assert_eq!(get(&p, "T1").field("score"), Some("11111"));

        let mut p = plan(vec![task("T1", &[("touches", "src/a.rs, tests/a.rs")])]);
        run(&mut p);
        assert_eq!(get(&p, "T1").field("score"), Some("00000"));

        let mut p = plan(vec![task("T1", &[("touches", "src/a.rs, tests/a.rs")])]);
        p.tasks[0].text = "Ship it, refuted upstream by nobody".into();
        run(&mut p);
        assert_eq!(
            get(&p, "T1").field("score"),
            Some("00000"),
            "model prose cannot raise its own score"
        );
    }

    #[test]
    fn the_model_follows_the_score() {
        let s = |d, b, v, k, c| Score { d, b, v, k, c };
        assert_eq!(s(false, false, false, false, false).model(), "sonnet");
        assert_eq!(s(true, false, false, false, false).model(), "sonnet");
        assert_eq!(s(true, true, false, false, false).model(), "opus");
        assert_eq!(
            s(false, false, false, true, false).model(),
            "sonnet + review@opus"
        );
        assert_eq!(
            s(true, true, true, true, false).model(),
            "opus + review@opus"
        );

        let mut p = plan(vec![task(
            "T1",
            &[
                ("touches", "src/auth/a.rs, docs/b.md"),
                ("depends", "Q1"),
                ("accept", "prose"),
            ],
        )]);
        p.open.push(Item::new("Q1", "Which spread?"));
        run(&mut p);
        let t1 = get(&p, "T1");
        assert_eq!(t1.field("score"), Some("11110"));
        assert_eq!(t1.field("model"), Some("opus + review@opus"));
        assert!(
            t1.markers.iter().any(|m| m.ends_with(SPLIT_SCORE)),
            "{t1:?}"
        );
    }
}
