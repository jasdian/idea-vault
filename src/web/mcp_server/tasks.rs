//! The Task↔Job bridge (docs/adr/0024): maps the long-running tools' (`chat`, `store_idea`,
//! `run_skill`, `run_swarm`, `build_plan`) MCP-task lifecycle
//! (`tasks/get`/`tasks/result`/`tasks/cancel`, SEP-1686) onto idea-vault's existing
//! slug-keyed background-job machinery (`web::jobs`, ADR-0010), without any change to `jobs.rs`
//! itself — every job-state read/write below goes through its existing public functions.
//!
//! `web::jobs::peek` is a **one-shot, consuming** read of a terminal (`Failed`/`Notice`) slot —
//! correct for its one HTTP poll endpoint, but the MCP Task lifecycle asks about the same task
//! through two separate RPC methods (`tasks/get` then `tasks/result`), and a client MAY poll
//! `tasks/get` more than once. Reading `peek` from more than one of those calls would consume the
//! terminal slot on the first read and roll every later read over to `Idle`, silently reporting a
//! failed or cancelled task as a false success. [`TaskEntry::terminal`] is the fix: the first time
//! a terminal state is observed (by *either* `tasks/get` or `tasks/result`), it is cached on the
//! entry, and every later call reads the cache instead of `web::jobs` again.
//!
//! The plain-call fallback for a Task-unaware client (docs/adr/0028, [`TaskRegistry::call_sync_bounded`])
//! is the third reader of the same slot, and it goes through the very same [`TaskRegistry::observe`]
//! cache on a real task entry — never through a second, independent `peek` — so a task-mode poll
//! and a plain-call retry racing on one idea can never consume each other's terminal read.
//!
//! `web::jobs::Job` also carries no return payload (only a status), so a `Completed`/`Notice`
//! result is derived from the vault — the vault is truth anyway (markdown-is-truth). It is
//! derived **once**, at the first terminal observation, and stored as [`TaskEntry::rendered`]
//! (docs/adr/0033): re-deriving at read time would hand a late `tasks/result` or a replay whatever
//! turn happens to be newest *then*, not this task's own reply.
//!
//! A served result is also recorded in the [`ReplayCache`] (`idempotency`), so an identical call
//! after the result was served replays it instead of starting a second model run.
//!
//! [`TaskRegistry`] itself lives on [`super::handler::IdeaVaultMcpServer`] behind an `Arc`, one
//! instance for the whole mounted route (not per rmcp session) — a task minted on one session
//! must be pollable from another, since a client may reconnect between `enqueue_task` and
//! `tasks/get`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use chrono::Utc;
use rmcp::model::{
    CallToolResult, CancelTaskResult, Content, CreateTaskResult, GetTaskPayloadResult,
    GetTaskResult, JsonObject, RawContent, Task, TaskStatus,
};
use rmcp::ErrorData as McpError;
use serde_json::Value;

use crate::concepts::build_plan::workbench;
use crate::domain::IdeaState;
use crate::vault::store;
use crate::web::jobs::{self, Pending};
use crate::web::routes::chat::spawn_chat_turn;
use crate::web::routes::memory::{
    guard_can_store, guard_skill, guard_swarm, guard_workflow, run_store_work, spawn_skill_job,
    spawn_swarm_job, spawn_workflow_job,
};
use crate::web::state::AppState;

use super::idempotency::{
    args_hash, IdeaStamp, Replay, ReplayCache, ReplayId, ReplayKey, IDEMPOTENCY_KEY, REPLAY_TTL,
};
use super::tools::required_str;

/// How long a plain (non-task) long-running tool call waits for its job before answering
/// "still running". Deliberately a short does-it-finish-fast grace period, not a whole model turn:
/// a fast local model or a cached claude-code reply lands inside it, while anything slower is
/// handed back to the caller to retry — this is the scoped exception to ADR-0010 that ADR-0028
/// carves out, and it must stay far below any HTTP client's request timeout. Unrelated to
/// `IDEA_VAULT_OLLAMA_TIMEOUT_SECS`, which bounds model *inactivity* inside the detached job.
const SYNC_WAIT_BUDGET: Duration = Duration::from_secs(3);

/// Poll interval inside [`SYNC_WAIT_BUDGET`]: coarse enough that the wait loop is a handful of
/// cheap in-memory `peek`s, fine enough that a fast reply is returned within a fraction of a
/// second of landing.
const SYNC_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// What a turn-appending task reports when its job ended `Idle` but no turn landed: something
/// else (the web `/pending` poll) consumed the job's `Failed`/`Notice` slot first, so the newest
/// turn is not this task's reply and must not be served — or replayed — as one (docs/adr/0033).
const CONSUMED_ELSEWHERE: &str = "finished but its result was consumed elsewhere — check get_idea";

#[derive(Clone, Copy, PartialEq, Eq)]
enum TaskKind {
    Chat,
    Store,
    Skill,
    Swarm,
    /// `build_plan` (docs/adr/0033): the build-prompt capstone skill, or the ready-to-build
    /// workflow when audited — either way one pointer turn and one new plan version.
    Plan,
}

fn kind_for(name: &str) -> Result<TaskKind, McpError> {
    match name {
        "chat" => Ok(TaskKind::Chat),
        "store_idea" => Ok(TaskKind::Store),
        "run_skill" => Ok(TaskKind::Skill),
        "run_swarm" => Ok(TaskKind::Swarm),
        "build_plan" => Ok(TaskKind::Plan),
        _ => Err(McpError::invalid_params(
            format!("'{name}' does not support task-based invocation"),
            None,
        )),
    }
}

/// The tool name a kind was called as — the `tool` half of a [`ReplayKey`], so a `chat` and a
/// `run_skill` with coincidentally equal arguments can never replay each other.
fn tool_name(kind: TaskKind) -> &'static str {
    match kind {
        TaskKind::Chat => "chat",
        TaskKind::Store => "store_idea",
        TaskKind::Skill => "run_skill",
        TaskKind::Swarm => "run_swarm",
        TaskKind::Plan => "build_plan",
    }
}

/// A terminal outcome for an MCP task, cached the first time it's observed — see the module doc.
#[derive(Clone)]
enum Terminal {
    Completed,
    Failed(String),
    Notice(String),
    Cancelled,
}

fn translate_terminal(terminal: &Terminal) -> (TaskStatus, Option<String>) {
    match terminal {
        Terminal::Completed => (TaskStatus::Completed, None),
        Terminal::Failed(msg) => (TaskStatus::Failed, Some(msg.clone())),
        Terminal::Notice(msg) => (TaskStatus::Completed, Some(msg.clone())),
        Terminal::Cancelled => (TaskStatus::Cancelled, None),
    }
}

/// How a call identifies itself for replay (docs/adr/0033): the explicit `idempotency_key` the
/// client passed, if any, and the hash of its arguments.
struct CallId {
    explicit: Option<String>,
    args_hash: String,
}

impl CallId {
    fn of(args: &Value) -> Result<Self, McpError> {
        let explicit = match args.get(IDEMPOTENCY_KEY) {
            None | Some(Value::Null) => None,
            Some(Value::String(k)) if !k.trim().is_empty() => Some(k.trim().to_string()),
            Some(_) => {
                return Err(McpError::invalid_params(
                    "'idempotency_key' must be a non-empty string",
                    None,
                ))
            }
        };
        Ok(Self {
            explicit,
            args_hash: args_hash(args),
        })
    }
}

struct TaskEntry {
    slug: String,
    kind: TaskKind,
    /// What distinguishes this operation from another of the same kind ([`op_key`]) — what a
    /// plain retry is matched on, so a genuinely new message never reattaches to an older turn.
    key: Option<String>,
    call: CallId,
    terminal: Option<Terminal>,
    /// The tool result, rendered once at the first terminal observation (module doc) and served
    /// verbatim by `tasks/result`, the plain path and any replay.
    rendered: Option<CallToolResult>,
    /// The idea's turn count once the claim had persisted everything it writes synchronously —
    /// the baseline `turns_at_finish` must exceed for a turn-appending task to have actually
    /// landed its turn.
    turns_at_claim: usize,
    turns_at_finish: Option<usize>,
    /// The idea's stamp at render time, recorded with a replay entry (docs/adr/0033).
    idea_at_finish: Option<IdeaStamp>,
    /// Whether `rendered` may be recorded for replay: set at render time, only for a
    /// Completed/Notice outcome whose effect really reached the vault (docs/adr/0033).
    is_replayable: bool,
}

/// What one `observe()` call resolved, carrying everything a caller needs without re-locking.
struct Observed {
    status: TaskStatus,
    message: Option<String>,
    rendered: Option<CallToolResult>,
}

/// The maps behind one lock: the task_id → entry map every Task RPC reads, a slug → task_id
/// reverse index so a plain-call retry can find the in-flight task for its idea
/// ([`TaskRegistry::call_sync_bounded`]), and the replay cache of served results. `by_slug` only
/// ever points at the *newest* task minted for a slug; older entries stay reachable by task id
/// for a Task-capable client's polls.
#[derive(Default)]
struct Registry {
    tasks: HashMap<String, TaskEntry>,
    by_slug: HashMap<String, String>,
    replay: ReplayCache,
}

/// In-memory task registry. Never persisted: a task id is meaningless across a process
/// restart, exactly like `web::jobs`' own in-memory job map. A poisoned lock (a panic while an
/// entry was being mutated, which none of the mutations here can actually trigger) is recovered
/// from rather than propagated — see [`TaskRegistry::lock`] — so one bad task can't take down
/// `tasks/get`/`tasks/result`/`tasks/cancel` for every other idea for the rest of the process.
#[derive(Default)]
pub(super) struct TaskRegistry(Mutex<Registry>);

impl TaskRegistry {
    pub(super) fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Mint a task id for a just-spawned job and record it under both maps.
    fn register(&self, slug: String, kind: TaskKind, claimed: Claimed, call: CallId) -> String {
        let task_id = next_task_id();
        let mut map = self.lock();
        map.by_slug.insert(slug.clone(), task_id.clone());
        map.tasks.insert(
            task_id.clone(),
            TaskEntry {
                slug,
                kind,
                key: claimed.key,
                call,
                terminal: None,
                rendered: None,
                turns_at_claim: claimed.turns_at_claim,
                turns_at_finish: None,
                idea_at_finish: None,
                is_replayable: false,
            },
        );
        task_id
    }

    /// Mint a task that is terminal from birth, carrying a replayed result — what a task-mode
    /// replay hit hands back, so the client's `tasks/get` → `tasks/result` sequence works
    /// unchanged. It never enters `by_slug`: there is no job behind it to reattach to, and it is
    /// never itself recorded for replay (the original entry already is).
    fn register_replayed(&self, slug: String, kind: TaskKind, result: CallToolResult) -> String {
        let task_id = next_task_id();
        self.lock().tasks.insert(
            task_id.clone(),
            TaskEntry {
                slug,
                kind,
                key: None,
                call: CallId {
                    explicit: None,
                    args_hash: String::new(),
                },
                terminal: Some(Terminal::Completed),
                rendered: Some(result),
                turns_at_claim: 0,
                turns_at_finish: None,
                idea_at_finish: None,
                is_replayable: false,
            },
        );
        task_id
    }

    /// `enqueue_task`: validate + claim + spawn, then mint a task id. Fails fast (a protocol
    /// error, not a minted task) on a bad slug/state/busy-idea — matching the HTTP routes' own
    /// synchronous guards, so a doomed call never produces a task the client has to poll to learn
    /// it was doomed. (This is a deliberate asymmetry with the synchronous tools, which
    /// surface the same class of business error as a `CallToolResult`-level tool error instead —
    /// see docs/adr/0024's Consequences.) A replay hit (docs/adr/0033) claims nothing and mints
    /// an already-completed task.
    pub(super) async fn enqueue(
        &self,
        state: &AppState,
        name: &str,
        args: Option<JsonObject>,
    ) -> Result<CreateTaskResult, McpError> {
        let args = args.map(Value::Object).unwrap_or(Value::Null);
        let kind = kind_for(name)?;
        let slug = required_str(&args, "slug")?.to_string();
        let call = CallId::of(&args)?;
        let now = Utc::now().to_rfc3339();
        if let Some(result) = self.replay_hit(state, kind, &slug, &call)? {
            let task_id = self.register_replayed(slug, kind, result);
            let task = Task::new(task_id, TaskStatus::Completed, now.clone(), now);
            return Ok(CreateTaskResult::new(task));
        }
        let claimed = claim_counted(state, &slug, kind, &args)?;
        let task_id = self.register(slug, kind, claimed, call);
        let task =
            Task::new(task_id, TaskStatus::Working, now.clone(), now).with_poll_interval(1_500);
        Ok(CreateTaskResult::new(task))
    }

    /// A plain (non-task) `tools/call` for a long-running tool (docs/adr/0028): the same
    /// validate → claim → spawn as [`Self::enqueue`], then a bounded wait on the minted task.
    /// A terminal outcome inside [`SYNC_WAIT_BUDGET`] is returned as the tool result — the same
    /// rendered payload `tasks/result` serves; otherwise the call returns a non-error "still
    /// running" note and the job keeps running detached (ADR-0010). A retry with the same
    /// arguments reattaches to that task — waiting if it is still `Working`, or serving its
    /// cached terminal result if it finished in the meantime — instead of claiming a second job.
    /// Once the outcome has been served it is recorded for replay and the slug's reverse-index
    /// entry is dropped, so an identical call after that replays the served result instead of
    /// starting a second model run, under the key and turn-count rules of docs/adr/0033 (which
    /// amends ADR-0028's forget-after-serve). A *different* `chat` message is a new operation: it
    /// goes through the normal claim and fails "already busy" while the previous turn is still
    /// running, exactly as task mode does.
    pub(super) async fn call_sync_bounded(
        &self,
        state: &AppState,
        name: &str,
        args: Option<JsonObject>,
    ) -> Result<CallToolResult, McpError> {
        let args = args.map(Value::Object).unwrap_or(Value::Null);
        let kind = kind_for(name)?;
        let slug = required_str(&args, "slug")?.to_string();
        let key = op_key(kind, &args)?;
        let call = CallId::of(&args)?;

        // An in-flight task keeps the ADR-0028 reattach; only a finished-and-served run replays.
        let task_id = match self.reattachable(&slug, kind, key.as_deref()) {
            Some(task_id) => task_id,
            None => {
                if let Some(result) = self.replay_hit(state, kind, &slug, &call)? {
                    return Ok(result);
                }
                self.settle_newest(state, &slug)?;
                match claim_counted(state, &slug, kind, &args) {
                    Ok(claimed) => self.register(slug.clone(), kind, claimed, call),
                    // A twin call may have claimed and registered between our lookup and our
                    // claim (three separate critical sections); if so, join it rather than fail.
                    Err(e) => match self.reattachable(&slug, kind, key.as_deref()) {
                        Some(task_id) => task_id,
                        None => return Err(e),
                    },
                }
            }
        };

        let deadline = Instant::now() + SYNC_WAIT_BUDGET;
        loop {
            let observed = self.observe(state, &task_id)?;
            if let Some(rendered) = observed.rendered {
                self.serve(&task_id);
                return Ok(rendered);
            }
            if Instant::now() >= deadline {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "'{name}' is still running for idea '{slug}' (task {task_id}). Call the tool \
                     again with the same arguments to check for the result, or poll tasks/get \
                     then tasks/result with that task id."
                ))]));
            }
            tokio::time::sleep(SYNC_POLL_INTERVAL).await;
        }
    }

    /// The replay lookup (docs/adr/0033, D34). An explicit key replays whenever its arguments
    /// match and is an `invalid_params` error when they don't; a key never seen is a miss even
    /// if an unkeyed entry matches, because a fresh key is how a client asks for a deliberate
    /// re-run. Without a key, the args hash replays only while the idea's turn count and its
    /// `(state, updated)` stamp are still what they were when the run finished — an identical
    /// message after an intervening turn is a new question, and store/reopen change the idea
    /// without appending a turn, so a stale replay would otherwise skip the state guards.
    fn replay_hit(
        &self,
        state: &AppState,
        kind: TaskKind,
        slug: &str,
        call: &CallId,
    ) -> Result<Option<CallToolResult>, McpError> {
        let id = match &call.explicit {
            Some(k) => ReplayId::Explicit(k.clone()),
            None => ReplayId::Hash(call.args_hash.clone()),
        };
        let key = ReplayKey {
            tool: tool_name(kind),
            slug: slug.to_string(),
            id,
        };
        let Some(hit) = self.lock().replay.lookup(&key, Instant::now()).cloned() else {
            return Ok(None);
        };
        match key.id {
            ReplayId::Explicit(_) if hit.args_hash != call.args_hash => Err(
                McpError::invalid_params("idempotency_key reused with different arguments", None),
            ),
            ReplayId::Explicit(_) => Ok(Some(replayed(&hit))),
            ReplayId::Hash(_) => {
                let unchanged = turn_count(state, slug) == hit.turns_at_finish
                    && hit.idea_at_finish.is_some()
                    && idea_stamp(state, slug) == hit.idea_at_finish;
                Ok(unchanged.then(|| replayed(&hit)))
            }
        }
    }

    /// A result was just handed to a client: record it for replay if it may be replayed, then
    /// drop the slug → task_id link so the next call is judged by the replay rules instead of
    /// reattaching to a finished task.
    fn serve(&self, task_id: &str) {
        let mut map = self.lock();
        let Some(entry) = map.tasks.get(task_id) else {
            return;
        };
        let slug = entry.slug.clone();
        let replay = match (entry.is_replayable, &entry.rendered, entry.turns_at_finish) {
            (true, Some(result), Some(turns_at_finish)) => Some((
                tool_name(entry.kind),
                entry.call.explicit.clone(),
                Replay {
                    task_id: task_id.to_string(),
                    args_hash: entry.call.args_hash.clone(),
                    result: result.clone(),
                    turns_at_finish,
                    idea_at_finish: entry.idea_at_finish,
                    expires: Instant::now() + REPLAY_TTL,
                },
            )),
            _ => None,
        };
        if let Some((tool, explicit, replay)) = replay {
            let now = Instant::now();
            // Always under the args hash, so a keyless retry of a keyed call replays too (still
            // subject to the turn-count check), and under the explicit key when one was sent.
            if let Some(k) = explicit {
                let key = ReplayKey {
                    tool,
                    slug: slug.clone(),
                    id: ReplayId::Explicit(k),
                };
                map.replay.insert(key, replay.clone(), now);
            }
            let key = ReplayKey {
                tool,
                slug: slug.clone(),
                id: ReplayId::Hash(replay.args_hash.clone()),
            };
            map.replay.insert(key, replay, now);
        }
        if map.by_slug.get(&slug).is_some_and(|id| id == task_id) {
            map.by_slug.remove(&slug);
        }
    }

    /// The newest task for `slug`, if it was claimed for the same operation (same kind and the
    /// same [`op_key`]). `by_slug` is only cleared once the outcome has been served, so a match
    /// here is reattached to whether it is still `Working` or already terminal — the latter is
    /// the common retry after the "still running" note.
    fn reattachable(&self, slug: &str, kind: TaskKind, key: Option<&str>) -> Option<String> {
        let map = self.lock();
        let task_id = map.by_slug.get(slug)?;
        let entry = map.tasks.get(task_id)?;
        (entry.kind == kind && entry.key.as_deref() == key).then(|| task_id.clone())
    }

    /// Observe the slug's newest task once before a fresh claim, so a job that finished without
    /// anyone reading it has its one-shot `Failed`/`Notice` slot cached on *its* entry (and
    /// released) rather than left in `web::jobs` to block `try_claim_idle` for a new turn.
    fn settle_newest(&self, state: &AppState, slug: &str) -> Result<(), McpError> {
        let newest = self.lock().by_slug.get(slug).cloned();
        if let Some(task_id) = newest {
            self.observe(state, &task_id)?;
        }
        Ok(())
    }

    /// Resolve a task's current status, caching a terminal outcome — and rendering its result —
    /// the first time it's seen. See the module doc for why this cache exists instead of reading
    /// `web::jobs::peek` directly from both `info` and `result`.
    fn observe(&self, state: &AppState, task_id: &str) -> Result<Observed, McpError> {
        let mut map = self.lock();
        let entry = map
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| McpError::invalid_params(format!("unknown task '{task_id}'"), None))?;

        let terminal = match entry.terminal.clone() {
            Some(terminal) => terminal,
            None => match jobs::peek(&state.jobs, &entry.slug) {
                Pending::Running { note, .. } => {
                    return Ok(Observed {
                        status: TaskStatus::Working,
                        message: non_empty(note),
                        rendered: None,
                    })
                }
                Pending::Idle => Terminal::Completed,
                Pending::Failed(msg) => Terminal::Failed(msg),
                Pending::Notice(msg) => Terminal::Notice(msg),
            },
        };
        // A cancelled entry gets its terminal from `cancel`, not here, so it too is rendered on
        // its first observation rather than only when the terminal is first peeked.
        let terminal = match &entry.rendered {
            Some(_) => terminal,
            None => render(state, entry, terminal),
        };
        let (status, message) = translate_terminal(&terminal);
        entry.terminal = Some(terminal);
        Ok(Observed {
            status,
            message,
            rendered: entry.rendered.clone(),
        })
    }

    /// `tasks/get`.
    pub(super) fn info(&self, state: &AppState, task_id: &str) -> Result<GetTaskResult, McpError> {
        let observed = self.observe(state, task_id)?;
        let now = Utc::now().to_rfc3339();
        let mut task = Task::new(task_id.to_string(), observed.status, now.clone(), now);
        if let Some(m) = observed.message {
            task = task.with_status_message(m);
        }
        Ok(GetTaskResult { meta: None, task })
    }

    /// `tasks/result`. A protocol error while the job is still `Working` — the client is expected
    /// to poll `tasks/get` first and only call this once status is terminal, per SEP-1686. Serving
    /// it records the result for replay exactly as the plain path does (docs/adr/0033).
    pub(super) fn result(
        &self,
        state: &AppState,
        task_id: &str,
    ) -> Result<GetTaskPayloadResult, McpError> {
        let observed = self.observe(state, task_id)?;
        let Some(rendered) = observed.rendered else {
            return Err(McpError::invalid_request(
                "task is still running — poll tasks/get first",
                None,
            ));
        };
        self.serve(task_id);
        Ok(as_payload(rendered))
    }

    /// `tasks/cancel`. If the task already reached a terminal outcome before this call, that
    /// outcome is reported as-is (cancel cannot retroactively relabel a task that already
    /// completed, failed, or was cancelled) rather than always claiming `Cancelled`.
    pub(super) fn cancel(
        &self,
        state: &AppState,
        task_id: &str,
    ) -> Result<CancelTaskResult, McpError> {
        let (slug, reported_status) = {
            let mut map = self.lock();
            let entry = map.tasks.get_mut(task_id).ok_or_else(|| {
                McpError::invalid_params(format!("unknown task '{task_id}'"), None)
            })?;
            let status = match &entry.terminal {
                Some(terminal) => translate_terminal(terminal).0,
                None => {
                    entry.terminal = Some(Terminal::Cancelled);
                    TaskStatus::Cancelled
                }
            };
            (entry.slug.clone(), status)
        };
        // Best-effort abort of the underlying job; a no-op if it already finished (mark_done
        // already removed the slot, in which case the terminal state recorded above stands).
        jobs::cancel(&state.jobs, &slug);
        let now = Utc::now().to_rfc3339();
        Ok(CancelTaskResult {
            meta: None,
            task: Task::new(task_id.to_string(), reported_status, now.clone(), now),
        })
    }
}

/// Render a just-terminal entry's result once (module doc), applying the false-success guard and
/// deciding whether the result may be replayed (docs/adr/0033). Returns the terminal to record,
/// which the guard may have turned from `Completed` into `Failed`.
fn render(state: &AppState, entry: &mut TaskEntry, terminal: Terminal) -> Terminal {
    let turns = turn_count(state, &entry.slug);
    entry.turns_at_finish = Some(turns);
    entry.idea_at_finish = idea_stamp(state, &entry.slug);
    let turn_landed = turns > entry.turns_at_claim;
    let terminal = match (terminal, entry.kind) {
        // `Idle` reads as success, but the job appended nothing: its real outcome went to another
        // reader, and the newest turn belongs to someone else.
        (
            Terminal::Completed,
            TaskKind::Chat | TaskKind::Skill | TaskKind::Swarm | TaskKind::Plan,
        ) if !turn_landed => Terminal::Failed(CONSUMED_ELSEWHERE.to_string()),
        // A store appends no turn; its effect is the Stored state. `Idle` without it means the
        // job failed and another reader took its `Failed` slot — never report "stored".
        (Terminal::Completed, TaskKind::Store) if !is_stored(state, &entry.slug) => {
            Terminal::Failed(CONSUMED_ELSEWHERE.to_string())
        }
        (terminal, _) => terminal,
    };
    entry.is_replayable = match (&terminal, entry.kind) {
        (Terminal::Failed(_) | Terminal::Cancelled, _) => false,
        (Terminal::Completed | Terminal::Notice(_), TaskKind::Store) => {
            is_stored(state, &entry.slug)
        }
        (
            Terminal::Completed | Terminal::Notice(_),
            TaskKind::Chat | TaskKind::Skill | TaskKind::Swarm | TaskKind::Plan,
        ) => turn_landed,
    };
    entry.rendered = Some(terminal_result(
        state,
        &entry.slug,
        entry.kind,
        entry.turns_at_claim,
        terminal.clone(),
    ));
    terminal
}

/// The number of turns in the idea's conversation — the landed-a-turn signal and the
/// has-anything-happened-since check for replay. An unreadable conversation counts as zero,
/// which can only make a result non-replayable or a replay miss, never the reverse.
fn turn_count(state: &AppState, slug: &str) -> usize {
    store::read_conversation(&state.config.vault_dir, slug)
        .map(|c| store::split_turns(&c).len())
        .unwrap_or(0)
}

fn idea_stamp(state: &AppState, slug: &str) -> Option<IdeaStamp> {
    store::read_idea(&state.config.vault_dir, slug)
        .ok()
        .map(|idea| (idea.frontmatter.state, idea.frontmatter.updated))
}

fn is_stored(state: &AppState, slug: &str) -> bool {
    store::read_idea(&state.config.vault_dir, slug)
        .is_ok_and(|idea| idea.frontmatter.state == IdeaState::Stored)
}

/// A cached result handed back to a repeat call, marked so the caller can tell it is not a new
/// run's output.
fn replayed(hit: &Replay) -> CallToolResult {
    let mut result = hit.result.clone();
    let prefix = format!("(replayed result of task {}) ", hit.task_id);
    let first_text = result.content.iter_mut().find_map(|c| match &mut c.raw {
        RawContent::Text(t) => Some(t),
        _ => None,
    });
    match first_text {
        Some(t) => t.text.insert_str(0, &prefix),
        None => result.content.insert(0, Content::text(prefix)),
    }
    result
}

/// What a successful claim recorded: the job's [`op_key`] and the turn-count baseline.
struct Claimed {
    key: Option<String>,
    turns_at_claim: usize,
}

/// [`claim_and_spawn`] plus the turn-count baseline for the false-success guard and replay.
/// Counted *before* the claim so a fast job cannot land its reply first; a `chat` claim
/// persists the owner's turn synchronously, so that one turn is added to the baseline — only
/// the foil's reply counts as the task's landed turn.
fn claim_counted(
    state: &AppState,
    slug: &str,
    kind: TaskKind,
    args: &Value,
) -> Result<Claimed, McpError> {
    let before = turn_count(state, slug);
    let key = claim_and_spawn(state, slug, kind, args)?;
    let turns_at_claim = match kind {
        TaskKind::Chat => before + 1,
        TaskKind::Store | TaskKind::Skill | TaskKind::Swarm | TaskKind::Plan => before,
    };
    Ok(Claimed {
        key,
        turns_at_claim,
    })
}

/// The operation key a plain retry reattaches on: the `chat` message, the skill name, the
/// swarm's comma-joined angle list (empty for the default set), the constant `"store"` — one
/// slug has only one store, and a named key keeps every kind on the same `Some` shape — or the
/// plan's mode, `"quick"` or `"audited"` (docs/adr/0033).
fn op_key(kind: TaskKind, args: &Value) -> Result<Option<String>, McpError> {
    match kind {
        TaskKind::Chat => Ok(Some(required_str(args, "message")?.to_string())),
        TaskKind::Skill => Ok(Some(required_str(args, "name")?.to_string())),
        TaskKind::Swarm => Ok(Some(swarm_angles(args)?.join(","))),
        TaskKind::Store => Ok(Some("store".to_string())),
        TaskKind::Plan => Ok(Some(
            if plan_audited(args)? {
                "audited"
            } else {
                "quick"
            }
            .to_string(),
        )),
    }
}

/// `build_plan`'s optional `audited` flag: absent means the quick capstone.
fn plan_audited(args: &Value) -> Result<bool, McpError> {
    match args.get("audited") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(McpError::invalid_params(
            "'audited' must be a boolean",
            None,
        )),
    }
}

/// `run_swarm`'s optional `angles` argument: absent means the default D14 set (an empty list).
fn swarm_angles(args: &Value) -> Result<Vec<String>, McpError> {
    let Some(raw) = args.get("angles") else {
        return Ok(Vec::new());
    };
    raw.as_array()
        .ok_or_else(|| McpError::invalid_params("'angles' must be an array of strings", None))?
        .iter()
        .map(|a| {
            a.as_str().map(|s| s.trim().to_string()).ok_or_else(|| {
                McpError::invalid_params("'angles' must be an array of strings", None)
            })
        })
        .filter(|a| !matches!(a, Ok(s) if s.is_empty()))
        .collect()
}

fn busy_error(slug: &str) -> McpError {
    McpError::invalid_params(
        format!("idea '{slug}' is already busy with another job"),
        None,
    )
}

/// The shared validate → claim → spawn sequence behind both `enqueue_task` and the plain-call
/// fallback — one copy of the business rules, so the two entry points can never drift. Returns
/// the job's [`op_key`] for the caller to record on the task entry.
fn claim_and_spawn(
    state: &AppState,
    slug: &str,
    kind: TaskKind,
    args: &Value,
) -> Result<Option<String>, McpError> {
    let idea = store::read_idea(&state.config.vault_dir, slug)
        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

    match kind {
        TaskKind::Chat => {
            let message = required_str(args, "message")?.to_string();
            if idea.frontmatter.state == IdeaState::Stored {
                return Err(McpError::invalid_params(
                    "idea is stored — reopen it before chatting",
                    None,
                ));
            }
            if !jobs::try_claim_idle(&state.jobs, slug) {
                return Err(busy_error(slug));
            }
            if let Err(e) = spawn_chat_turn(state, slug, idea, &message) {
                return Err(McpError::internal_error(e.to_string(), None));
            }
            Ok(Some(message))
        }
        TaskKind::Store => {
            let key = op_key(kind, args)?;
            if let Err(e) = guard_can_store(&state.config.vault_dir, slug, &idea) {
                return Err(McpError::invalid_params(e.to_string(), None));
            }
            if !jobs::try_claim(&state.jobs, slug) {
                return Err(busy_error(slug));
            }
            let task_state = state.clone();
            let task_slug = slug.to_string();
            let abort = jobs::spawn_job(&state.jobs, slug, async move {
                match run_store_work(&task_state, &task_slug).await {
                    Ok(None) => jobs::mark_done(&task_state.jobs, &task_slug),
                    Ok(Some(notice)) => jobs::mark_notice(&task_state.jobs, &task_slug, notice),
                    Err(m) => jobs::mark_failed(&task_state.jobs, &task_slug, m),
                }
            });
            jobs::set_abort(&state.jobs, slug, abort);
            Ok(key)
        }
        // Owner actions like their web routes (R6/R7): `try_claim`, not the chat queue's
        // `try_claim_idle` — the same guards via the shared `guard_*` fns (HND-10).
        TaskKind::Skill => {
            let name = required_str(args, "name")?.to_string();
            let skill = guard_skill(state, &idea, &name)
                .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
            if !jobs::try_claim(&state.jobs, slug) {
                return Err(busy_error(slug));
            }
            spawn_skill_job(state, slug, skill);
            Ok(Some(name))
        }
        TaskKind::Swarm => {
            let requested = swarm_angles(args)?;
            let key = requested.join(",");
            let (skills, angles) = guard_swarm(state, &idea, requested)
                .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
            if !jobs::try_claim(&state.jobs, slug) {
                return Err(busy_error(slug));
            }
            spawn_swarm_job(state, slug, skills, angles);
            Ok(Some(key))
        }
        // The same two web seams the idea page's plan buttons use (HND-10): the quick plan is the
        // build-prompt capstone skill, the audited one the ready-to-build workflow. Lineage
        // (revises the head, carries owner answers) is `finish`'s job either way (ADR-0032).
        TaskKind::Plan => {
            let key = op_key(kind, args)?;
            if plan_audited(args)? {
                guard_workflow(&idea, PLAN_WORKFLOW)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                if !jobs::try_claim(&state.jobs, slug) {
                    return Err(busy_error(slug));
                }
                spawn_workflow_job(state, slug, PLAN_WORKFLOW.to_string());
            } else {
                let skill = guard_skill(state, &idea, PLAN_SKILL)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                if !jobs::try_claim(&state.jobs, slug) {
                    return Err(busy_error(slug));
                }
                spawn_skill_job(state, slug, skill);
            }
            Ok(key)
        }
    }
}

/// The capstone skill behind a quick `build_plan`.
const PLAN_SKILL: &str = "build-prompt";
/// The workflow behind an audited `build_plan`.
const PLAN_WORKFLOW: &str = crate::concepts::workflows::READY_TO_BUILD;

fn non_empty(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

fn as_payload(result: CallToolResult) -> GetTaskPayloadResult {
    GetTaskPayloadResult::new(serde_json::to_value(result).unwrap_or(Value::Null))
}

/// The tool result for a task that reached `terminal` — one implementation, rendered once per
/// task (module doc), behind `tasks/result`, the plain-call fallback and replay, so every
/// surface answers identically.
fn terminal_result(
    state: &AppState,
    slug: &str,
    kind: TaskKind,
    own_turn: usize,
    terminal: Terminal,
) -> CallToolResult {
    match terminal {
        Terminal::Failed(msg) => CallToolResult::error(vec![Content::text(msg)]),
        Terminal::Cancelled => CallToolResult::error(vec![Content::text("task was cancelled")]),
        Terminal::Notice(msg) => finish_result(state, slug, kind, own_turn, Some(msg)),
        Terminal::Completed => finish_result(state, slug, kind, own_turn, None),
    }
}

/// The task's own turn: the first one after its claim baseline (`own_turn` = `turns_at_claim`).
/// Not the newest turn — a task first observed after another job has since run on the idea
/// would otherwise serve (and cache for replay) that job's reply. Nothing else can append in
/// between: the claimed slot refuses other jobs and the workbench, and a web chat sent while it
/// runs waits in the in-memory queue until the slot frees.
fn own_turn_text(state: &AppState, slug: &str, own_turn: usize) -> Result<String, String> {
    let conversation =
        store::read_conversation(&state.config.vault_dir, slug).map_err(|e| e.to_string())?;
    Ok(store::split_turns(&conversation)
        .into_iter()
        .nth(own_turn)
        .unwrap_or_default())
}

/// Derive the tool's `CallToolResult` from vault state at render time — see the module doc for
/// why, and for why this runs once per task rather than at every read.
fn finish_result(
    state: &AppState,
    slug: &str,
    kind: TaskKind,
    own_turn: usize,
    notice: Option<String>,
) -> CallToolResult {
    match kind {
        // Each of these appends exactly one assistant turn — the reply, the skill's move, or the
        // swarm's converged synthesis — so the task's own turn is the result.
        TaskKind::Chat | TaskKind::Skill | TaskKind::Swarm => {
            match own_turn_text(state, slug, own_turn) {
                Ok(reply) => CallToolResult::success(vec![Content::text(reply)]),
                Err(e) => CallToolResult::error(vec![Content::text(e)]),
            }
        }
        TaskKind::Store => match store::read_idea(&state.config.vault_dir, slug) {
            Ok(idea) => {
                let mut msg = format!("stored — state is now {}", idea.frontmatter.state.as_str());
                if let Some(n) = notice {
                    msg.push_str(&format!("; {n}"));
                }
                CallToolResult::success(vec![Content::text(msg)])
            }
            Err(e) => CallToolResult::error(vec![Content::text(e.to_string())]),
        },
        TaskKind::Plan => plan_result(state, slug, own_turn),
    }
}

/// A finished `build_plan`: the lineage head the run just wrote, as the same view `get_plan`
/// returns, so the client can relay its open questions to the owner and answer them with
/// `answer_plan` (docs/adr/0033). The plan served is the one the run's own turn — its pointer —
/// links to: a run whose plan was unusable wrote an explanation but no plan, and an older head
/// must not pass for this run's output, so that turn is served instead. A later re-plan by
/// someone else does not replace it either: the version is read by the pointer's stem, not as
/// whatever the head is now.
fn plan_result(state: &AppState, slug: &str, own_turn: usize) -> CallToolResult {
    let vault_dir = &state.config.vault_dir;
    let own = match own_turn_text(state, slug, own_turn) {
        Ok(own) => own,
        Err(e) => return CallToolResult::error(vec![Content::text(e)]),
    };
    let Some(stem) = pointer_stem(&own) else {
        return CallToolResult::success(vec![Content::text(own)]);
    };
    match workbench::plan_view(vault_dir, slug, Some(stem)) {
        Ok(view) if view.stem == stem => CallToolResult::success(vec![Content::text(format!(
            "plan {}\n\n{}\n\nrelay the open questions and owner-blocked tasks to the owner; \
                 answer with answer_plan in the owner's own words",
            view.stem,
            view.to_json()
        ))]),
        Ok(_) | Err(_) => CallToolResult::success(vec![Content::text(own)]),
    }
}

/// The plan stem a build-plan pointer turn links to (`**Build plan** → [<stem>](…)`).
fn pointer_stem(turn: &str) -> Option<&str> {
    let body = turn.split_once('\n').map_or(turn, |(_, rest)| rest);
    let rest = body
        .trim_start()
        .strip_prefix(crate::domain::evidence::POINTER_PREFIX)?;
    rest.split_once(']')
        .map(|(stem, _)| stem)
        .filter(|s| !s.is_empty())
}

/// A process-local, monotonically increasing task id — never persisted or compared across a
/// restart, exactly like `web::jobs`' own in-memory job map, so no `uuid` dependency is needed.
fn next_task_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "task-{}-{n}",
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    )
}
