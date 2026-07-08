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
//!
//! Two extras ride on the same slot: the spawned task's [`tokio::task::AbortHandle`], so the owner
//! can **cancel** a running job (aborting the task drops the in-flight model future — nothing
//! partial is persisted, because every write happens only after the model call fully completes),
//! and a live **note** the orchestrators advance ("swarm · attacking 2/4: constraints") so the
//! indicator shows real per-step progress rather than a bare spinner.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::task::AbortHandle;

/// One idea's job slot. `Failed` and `Notice` are read once (by the next poll) then cleared.
pub enum JobStatus {
    Running,
    Failed(String),
    /// The job finished fine but changed nothing on disk (e.g. a forced compaction with nothing
    /// to fold) — a one-shot neutral message, so an honest no-op is never a silent one.
    Notice(String),
}

pub struct Job {
    pub status: JobStatus,
    pub started: Instant,
    /// The detached task's abort handle, registered post-spawn (the handle only exists once
    /// `tokio::spawn` has returned). `None` until [`set_abort`] runs, or for a `Failed` slot.
    pub abort: Option<AbortHandle>,
    /// A live one-line progress note the orchestrators advance (see [`set_note`]); surfaced by
    /// [`peek`] and rendered in the "thinking" indicator. Empty means "no specific step yet".
    pub note: String,
}

/// Shared registry keyed by idea slug. At most one entry per idea.
pub type Jobs = Arc<Mutex<HashMap<String, Job>>>;

pub fn new_registry() -> Jobs {
    Arc::new(Mutex::new(HashMap::new()))
}

/// What a page render / poll should show for an idea.
pub enum Pending {
    /// A job is running; carries elapsed whole seconds since it started and the live progress note.
    Running { secs: u64, note: String },
    /// The last job failed; carries the message. Consumed here (cleared from the map).
    Failed(String),
    /// The last job completed as an honest no-op; carries a neutral one-shot message. Consumed
    /// here (cleared from the map), exactly like `Failed`.
    Notice(String),
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
            abort: None,
            note: String::new(),
        },
    );
    true
}

/// Register the spawned task's abort handle on the (already claimed) slot, so [`cancel`] can stop
/// it. A no-op if the slot is gone (the job already finished) or not running — never resurrects a
/// completed job.
pub fn set_abort(jobs: &Jobs, slug: &str, abort: AbortHandle) {
    if let Ok(mut map) = jobs.lock() {
        if let Some(job) = map.get_mut(slug) {
            if matches!(job.status, JobStatus::Running) {
                job.abort = Some(abort);
            }
        }
    }
}

/// Advance the running job's live progress note (e.g. "swarm · converging 4 findings"). A no-op if
/// the slot is gone, so an orchestrator reporting after cancellation/completion writes nothing.
pub fn set_note(jobs: &Jobs, slug: &str, note: &str) {
    if let Ok(mut map) = jobs.lock() {
        if let Some(job) = map.get_mut(slug) {
            job.note = note.to_string();
        }
    }
}

/// Cancel a running job: abort its detached task (dropping the in-flight model future — nothing
/// partial is persisted) and clear the slot so the next action on this idea is accepted again.
/// Returns `true` if a slot was cleared, `false` if there was nothing to cancel.
pub fn cancel(jobs: &Jobs, slug: &str) -> bool {
    let Ok(mut map) = jobs.lock() else {
        return false;
    };
    match map.remove(slug) {
        Some(job) => {
            if let Some(handle) = job.abort {
                handle.abort();
            }
            true
        }
        None => false,
    }
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
                abort: None,
                note: String::new(),
            },
        );
    }
}

/// The job finished as an honest no-op — keep a neutral message so the next poll can show it,
/// then it's cleared (same one-shot lifecycle as [`mark_failed`]).
pub fn mark_notice(jobs: &Jobs, slug: &str, message: String) {
    if let Ok(mut map) = jobs.lock() {
        map.insert(
            slug.to_string(),
            Job {
                status: JobStatus::Notice(message),
                started: Instant::now(),
                abort: None,
                note: String::new(),
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
            note,
            ..
        }) => Pending::Running {
            secs: started.elapsed().as_secs(),
            note: note.clone(),
        },
        Some(Job {
            status: JobStatus::Failed(_) | JobStatus::Notice(_),
            ..
        }) => match map.remove(slug) {
            Some(Job {
                status: JobStatus::Failed(msg),
                ..
            }) => Pending::Failed(msg),
            Some(Job {
                status: JobStatus::Notice(msg),
                ..
            }) => Pending::Notice(msg),
            _ => Pending::Idle,
        },
        None => Pending::Idle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_note_round_trips_through_peek() {
        let jobs = new_registry();
        assert!(try_claim(&jobs, "i"));
        // A fresh claim has an empty note.
        match peek(&jobs, "i") {
            Pending::Running { note, .. } => assert_eq!(note, ""),
            _ => panic!("expected Running"),
        }
        set_note(&jobs, "i", "swarm · attacking 2/4: constraints");
        match peek(&jobs, "i") {
            Pending::Running { note, .. } => {
                assert_eq!(note, "swarm · attacking 2/4: constraints")
            }
            _ => panic!("expected Running"),
        }
    }

    #[test]
    fn set_note_is_a_no_op_without_a_running_slot() {
        let jobs = new_registry();
        set_note(&jobs, "i", "ignored");
        assert!(matches!(peek(&jobs, "i"), Pending::Idle));
    }

    #[test]
    fn cancel_clears_the_slot_so_it_can_be_reclaimed() {
        let jobs = new_registry();
        assert!(try_claim(&jobs, "i"));
        assert!(!try_claim(&jobs, "i"), "busy while running");
        assert!(cancel(&jobs, "i"), "cancel clears a running slot");
        assert!(matches!(peek(&jobs, "i"), Pending::Idle));
        assert!(try_claim(&jobs, "i"), "slot is free again after cancel");
    }

    #[test]
    fn cancel_of_an_idle_slot_is_false() {
        let jobs = new_registry();
        assert!(!cancel(&jobs, "i"));
    }

    #[test]
    fn mark_notice_is_consumed_exactly_once() {
        let jobs = new_registry();
        assert!(try_claim(&jobs, "i"));
        mark_notice(&jobs, "i", "nothing to fold".into());
        match peek(&jobs, "i") {
            Pending::Notice(msg) => assert_eq!(msg, "nothing to fold"),
            _ => panic!("expected Notice"),
        }
        // One-shot: the next poll is back to Idle, and the slot is claimable again.
        assert!(matches!(peek(&jobs, "i"), Pending::Idle));
        assert!(try_claim(&jobs, "i"));
    }
}
