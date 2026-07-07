//! Per-idea background AI jobs so a slow model call survives the browser.
//!
//! The problem: a chat/skill/swarm reply can take a minute on local hardware. If the model call is
//! tied to the HTTP request, navigating away (or a dropped connection) cancels the request future,
//! which kills the generation — and the "foil is thinking" state, being client-only, is lost too.
//!
//! The fix: each idea has at most one in-flight job. Starting a move spawns a **detached** task
//! (not the request future), records `Running`, and returns immediately. The task persists its
//! turn(s) to `conversation.md` on success regardless of who is watching. The idea page and a poll
//! endpoint read this map to render (and resume, after navigation) a server-driven "thinking… Ns"
//! indicator, then swap in the finished transcript. Failures surface as a visible error.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// One idea's job slot. `Failed` is read once (by the next poll) then cleared.
pub enum JobStatus {
    Running,
    Failed(String),
}

pub struct Job {
    pub status: JobStatus,
    pub started: Instant,
}

/// Shared registry keyed by idea slug. At most one entry per idea.
pub type Jobs = Arc<Mutex<HashMap<String, Job>>>;

pub fn new_registry() -> Jobs {
    Arc::new(Mutex::new(HashMap::new()))
}

/// What a page render / poll should show for an idea.
pub enum Pending {
    /// A job is running; carries elapsed whole seconds since it started.
    Running(u64),
    /// The last job failed; carries the message. Consumed here (cleared from the map).
    Failed(String),
    /// No job — the transcript on disk is final.
    Idle,
}

/// Try to claim the single job slot. Returns `false` if a job is already running for this idea
/// (so the caller must not start a second one).
pub fn try_claim(jobs: &Jobs, slug: &str) -> bool {
    let Ok(mut map) = jobs.lock() else {
        return false;
    };
    if matches!(
        map.get(slug),
        Some(Job {
            status: JobStatus::Running,
            ..
        })
    ) {
        return false;
    }
    map.insert(
        slug.to_string(),
        Job {
            status: JobStatus::Running,
            started: Instant::now(),
        },
    );
    true
}

/// The job finished successfully — clear the slot (the result is already on disk).
pub fn mark_done(jobs: &Jobs, slug: &str) {
    if let Ok(mut map) = jobs.lock() {
        map.remove(slug);
    }
}

/// The job failed — keep the message so the next poll can show it, then it's cleared.
pub fn mark_failed(jobs: &Jobs, slug: &str, message: String) {
    if let Ok(mut map) = jobs.lock() {
        map.insert(
            slug.to_string(),
            Job {
                status: JobStatus::Failed(message),
                started: Instant::now(),
            },
        );
    }
}

/// Read (and, for a failure, consume) the current job state for an idea.
pub fn peek(jobs: &Jobs, slug: &str) -> Pending {
    let Ok(mut map) = jobs.lock() else {
        return Pending::Idle;
    };
    match map.get(slug) {
        Some(Job {
            status: JobStatus::Running,
            started,
        }) => Pending::Running(started.elapsed().as_secs()),
        Some(Job {
            status: JobStatus::Failed(_),
            ..
        }) => match map.remove(slug) {
            Some(Job {
                status: JobStatus::Failed(msg),
                ..
            }) => Pending::Failed(msg),
            _ => Pending::Idle,
        },
        None => Pending::Idle,
    }
}
