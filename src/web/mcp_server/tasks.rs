//! The Task↔Job bridge (docs/adr/0024): maps `chat`/`store_idea`'s MCP-task lifecycle
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
//! `web::jobs::Job` also carries no return payload (only a status), so a `Completed`/`Notice`
//! result is re-derived from the vault once cached — the vault is truth anyway (markdown-is-truth),
//! so re-reading it after completion is the correct source, not a workaround.
//!
//! [`TaskRegistry`] itself lives on [`super::handler::IdeaVaultMcpServer`] behind an `Arc`, one
//! instance for the whole mounted route (not per rmcp session) — a task minted on one session
//! must be pollable from another, since a client may reconnect between `enqueue_task` and
//! `tasks/get`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use chrono::Utc;
use rmcp::model::{
    CallToolResult, CancelTaskResult, Content, CreateTaskResult, GetTaskPayloadResult,
    GetTaskResult, JsonObject, Task, TaskStatus,
};
use rmcp::ErrorData as McpError;
use serde_json::Value;

use crate::app::AppState;
use crate::domain::IdeaState;
use crate::vault::store;
use crate::web::jobs::{self, Pending};
use crate::web::routes::chat::spawn_chat_turn;
use crate::web::routes::memory::{guard_can_store, run_store_work};

use super::tools::required_str;

#[derive(Clone, Copy)]
enum TaskKind {
    Chat,
    Store,
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

struct TaskEntry {
    slug: String,
    kind: TaskKind,
    terminal: Option<Terminal>,
}

/// What one `observe()` call resolved, carrying everything a caller needs without re-locking.
struct Observed {
    slug: String,
    kind: TaskKind,
    status: TaskStatus,
    message: Option<String>,
    terminal: Option<Terminal>,
}

/// In-memory task_id → task-entry map. Never persisted: a task id is meaningless across a process
/// restart, exactly like `web::jobs`' own in-memory job map. A poisoned lock (a panic while an
/// entry was being mutated, which none of the mutations here can actually trigger) is recovered
/// from rather than propagated — see [`TaskRegistry::lock`] — so one bad task can't take down
/// `tasks/get`/`tasks/result`/`tasks/cancel` for every other idea for the rest of the process.
#[derive(Default)]
pub(super) struct TaskRegistry(Mutex<HashMap<String, TaskEntry>>);

impl TaskRegistry {
    pub(super) fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, TaskEntry>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `enqueue_task`: validate + claim + spawn, then mint a task id. Fails fast (a protocol
    /// error, not a minted task) on a bad slug/state/busy-idea — matching the HTTP routes' own
    /// synchronous guards, so a doomed call never produces a task the client has to poll to learn
    /// it was doomed. (This is a deliberate asymmetry with the five synchronous tools, which
    /// surface the same class of business error as a `CallToolResult`-level tool error instead —
    /// see docs/adr/0024's Consequences.)
    pub(super) async fn enqueue(
        &self,
        state: &AppState,
        name: &str,
        args: Option<JsonObject>,
    ) -> Result<CreateTaskResult, McpError> {
        let args = args.map(Value::Object).unwrap_or(Value::Null);
        let kind = match name {
            "chat" => TaskKind::Chat,
            "store_idea" => TaskKind::Store,
            _ => {
                return Err(McpError::invalid_params(
                    format!("'{name}' does not support task-based invocation"),
                    None,
                ))
            }
        };
        let slug = required_str(&args, "slug")?.to_string();
        let idea = store::read_idea(&state.config.vault_dir, &slug)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;

        match kind {
            TaskKind::Chat => {
                let message = required_str(&args, "message")?.to_string();
                if idea.frontmatter.state == IdeaState::Stored {
                    return Err(McpError::invalid_params(
                        "idea is stored — reopen it before chatting",
                        None,
                    ));
                }
                if !jobs::try_claim_idle(&state.jobs, &slug) {
                    return Err(McpError::invalid_params(
                        format!("idea '{slug}' is already busy with another job"),
                        None,
                    ));
                }
                if let Err(e) = spawn_chat_turn(state, &slug, idea, &message) {
                    return Err(McpError::internal_error(e.to_string(), None));
                }
            }
            TaskKind::Store => {
                if let Err(e) = guard_can_store(&state.config.vault_dir, &slug, &idea) {
                    return Err(McpError::invalid_params(e.to_string(), None));
                }
                if !jobs::try_claim(&state.jobs, &slug) {
                    return Err(McpError::invalid_params(
                        format!("idea '{slug}' is already busy with another job"),
                        None,
                    ));
                }
                let task_state = state.clone();
                let task_slug = slug.clone();
                let abort = jobs::spawn_job(&state.jobs, &slug, async move {
                    match run_store_work(&task_state, &task_slug).await {
                        Ok(None) => jobs::mark_done(&task_state.jobs, &task_slug),
                        Ok(Some(notice)) => jobs::mark_notice(&task_state.jobs, &task_slug, notice),
                        Err(m) => jobs::mark_failed(&task_state.jobs, &task_slug, m),
                    }
                });
                jobs::set_abort(&state.jobs, &slug, abort);
            }
        }

        let task_id = next_task_id();
        let now = Utc::now().to_rfc3339();
        self.lock().insert(
            task_id.clone(),
            TaskEntry {
                slug,
                kind,
                terminal: None,
            },
        );
        let task =
            Task::new(task_id, TaskStatus::Working, now.clone(), now).with_poll_interval(1_500);
        Ok(CreateTaskResult::new(task))
    }

    /// Resolve a task's current status, caching a terminal outcome the first time it's seen — see
    /// the module doc for why this cache exists instead of reading `web::jobs::peek` directly from
    /// both `info` and `result`.
    fn observe(&self, state: &AppState, task_id: &str) -> Result<Observed, McpError> {
        let mut map = self.lock();
        let entry = map
            .get_mut(task_id)
            .ok_or_else(|| McpError::invalid_params(format!("unknown task '{task_id}'"), None))?;

        if let Some(terminal) = &entry.terminal {
            let (status, message) = translate_terminal(terminal);
            return Ok(Observed {
                slug: entry.slug.clone(),
                kind: entry.kind,
                status,
                message,
                terminal: Some(terminal.clone()),
            });
        }

        let (status, message, terminal) = match jobs::peek(&state.jobs, &entry.slug) {
            Pending::Running { note, .. } => (TaskStatus::Working, non_empty(note), None),
            Pending::Idle => (TaskStatus::Completed, None, Some(Terminal::Completed)),
            Pending::Failed(msg) => (
                TaskStatus::Failed,
                Some(msg.clone()),
                Some(Terminal::Failed(msg)),
            ),
            Pending::Notice(msg) => (
                TaskStatus::Completed,
                Some(msg.clone()),
                Some(Terminal::Notice(msg)),
            ),
        };
        entry.terminal = terminal.clone();
        Ok(Observed {
            slug: entry.slug.clone(),
            kind: entry.kind,
            status,
            message,
            terminal,
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
    /// to poll `tasks/get` first and only call this once status is terminal, per SEP-1686.
    pub(super) fn result(
        &self,
        state: &AppState,
        task_id: &str,
    ) -> Result<GetTaskPayloadResult, McpError> {
        let observed = self.observe(state, task_id)?;
        let Some(terminal) = observed.terminal else {
            return Err(McpError::invalid_request(
                "task is still running — poll tasks/get first",
                None,
            ));
        };
        let result = match terminal {
            Terminal::Failed(msg) => CallToolResult::error(vec![Content::text(msg)]),
            Terminal::Cancelled => CallToolResult::error(vec![Content::text("task was cancelled")]),
            Terminal::Notice(msg) => finish_result(state, &observed.slug, observed.kind, Some(msg)),
            Terminal::Completed => finish_result(state, &observed.slug, observed.kind, None),
        };
        Ok(as_payload(result))
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
            let entry = map.get_mut(task_id).ok_or_else(|| {
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

fn non_empty(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

fn as_payload(result: CallToolResult) -> GetTaskPayloadResult {
    GetTaskPayloadResult::new(serde_json::to_value(result).unwrap_or(Value::Null))
}

/// Re-derive the tool's `CallToolResult` from vault state — see the module doc for why.
fn finish_result(
    state: &AppState,
    slug: &str,
    kind: TaskKind,
    notice: Option<String>,
) -> CallToolResult {
    match kind {
        TaskKind::Chat => match store::read_conversation(&state.config.vault_dir, slug) {
            Ok(conversation) => {
                let reply = store::split_turns(&conversation)
                    .into_iter()
                    .last()
                    .unwrap_or_default();
                CallToolResult::success(vec![Content::text(reply)])
            }
            Err(e) => CallToolResult::error(vec![Content::text(e.to_string())]),
        },
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
    }
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
