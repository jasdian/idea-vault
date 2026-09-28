//! The Task↔Job bridge (docs/adr/0024): maps `chat`/`store_idea`'s MCP-task lifecycle
//! (`tasks/get`/`tasks/result`/`tasks/cancel`, SEP-1686) onto idea-vault's existing
//! slug-keyed background-job machinery (`web::jobs`, ADR-0010), without any change to `jobs.rs`
//! itself — every job-state read/write below goes through its existing public functions.
//!
//! `web::jobs::Job` carries no return payload (only a status), so [`TaskRegistry::result`]
//! re-derives the tool's result from the vault once the job goes idle — the vault is truth
//! anyway (markdown-is-truth), so re-reading it after completion is the correct source, not a
//! workaround.
//!
//! [`TaskRegistry`] itself lives on [`super::handler::IdeaVaultMcpServer`] behind an `Arc`, one
//! instance for the whole mounted route (not per rmcp session) — a task minted on one session
//! must be pollable from another, since a client may reconnect between `enqueue_task` and
//! `tasks/get`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

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

struct TaskEntry {
    slug: String,
    kind: TaskKind,
}

/// In-memory task_id → (idea slug, tool) map. Never persisted: a task id is meaningless across a
/// process restart, exactly like `web::jobs`' own in-memory job map.
#[derive(Default)]
pub(super) struct TaskRegistry(Mutex<HashMap<String, TaskEntry>>);

impl TaskRegistry {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// `enqueue_task`: validate + claim + spawn, then mint a task id. Fails fast (a protocol
    /// error, not a minted task) on a bad slug/state/busy-idea — matching the HTTP routes' own
    /// synchronous guards, so a doomed call never produces a task the client has to poll to learn
    /// it was doomed.
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
        self.0
            .lock()
            .unwrap()
            .insert(task_id.clone(), TaskEntry { slug, kind });
        let task =
            Task::new(task_id, TaskStatus::Working, now.clone(), now).with_poll_interval(1_500);
        Ok(CreateTaskResult::new(task))
    }

    /// `tasks/get`.
    pub(super) fn info(&self, state: &AppState, task_id: &str) -> Result<GetTaskResult, McpError> {
        let slug = self.slug_of(task_id)?;
        let (status, message) = translate(jobs::peek(&state.jobs, &slug));
        let now = Utc::now().to_rfc3339();
        let mut task = Task::new(task_id.to_string(), status, now.clone(), now);
        if let Some(m) = message {
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
        let (slug, kind) = {
            let map = self.0.lock().unwrap();
            let entry = map.get(task_id).ok_or_else(|| {
                McpError::invalid_params(format!("unknown task '{task_id}'"), None)
            })?;
            (entry.slug.clone(), entry.kind)
        };
        match jobs::peek(&state.jobs, &slug) {
            Pending::Running { .. } => Err(McpError::invalid_request(
                "task is still running — poll tasks/get first",
                None,
            )),
            Pending::Failed(msg) => Ok(as_payload(CallToolResult::error(vec![Content::text(msg)]))),
            Pending::Notice(msg) => Ok(as_payload(finish_result(state, &slug, kind, Some(msg)))),
            Pending::Idle => Ok(as_payload(finish_result(state, &slug, kind, None))),
        }
    }

    /// `tasks/cancel`.
    pub(super) fn cancel(
        &self,
        state: &AppState,
        task_id: &str,
    ) -> Result<CancelTaskResult, McpError> {
        let slug = self.slug_of(task_id)?;
        jobs::cancel(&state.jobs, &slug);
        let now = Utc::now().to_rfc3339();
        Ok(CancelTaskResult {
            meta: None,
            task: Task::new(task_id.to_string(), TaskStatus::Cancelled, now.clone(), now),
        })
    }

    fn slug_of(&self, task_id: &str) -> Result<String, McpError> {
        self.0
            .lock()
            .unwrap()
            .get(task_id)
            .map(|e| e.slug.clone())
            .ok_or_else(|| McpError::invalid_params(format!("unknown task '{task_id}'"), None))
    }
}

fn translate(pending: Pending) -> (TaskStatus, Option<String>) {
    match pending {
        Pending::Running { note, .. } => (
            TaskStatus::Working,
            if note.is_empty() { None } else { Some(note) },
        ),
        Pending::Idle => (TaskStatus::Completed, None),
        Pending::Failed(msg) => (TaskStatus::Failed, Some(msg)),
        Pending::Notice(msg) => (TaskStatus::Completed, Some(msg)),
    }
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
