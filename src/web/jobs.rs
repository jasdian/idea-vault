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

use std::collections::{HashMap, VecDeque};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use futures::FutureExt as _;
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

/// Claim the slot only if it is entirely empty (no `Running`, `Failed`, or `Notice` entry). Unlike
/// [`try_claim`] — which overwrites a consumed-but-present `Failed`/`Notice` slot so a fresh owner
/// action can reclaim it — this never clobbers an unshown outcome, so it is the safe gate for the
/// queue drainer: the drainer must not eat a pending error the owner has not seen yet. Atomic (one
/// lock), so racing pollers can't both win.
pub fn try_claim_idle(jobs: &Jobs, slug: &str) -> bool {
    let Ok(mut map) = jobs.lock() else {
        return false;
    };
    if map.contains_key(slug) {
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

/// Spawn a claimed job's detached task with a panic backstop. Every call site already converts
/// `work`'s own `Result` to [`mark_done`]/[`mark_failed`] internally — but if `work` itself
/// *panics* partway through (a template render, an unexpected slice index, ...), a bare
/// `tokio::spawn` never runs either arm: the slot stays `Running` forever, the "thinking… Ns"
/// poll (ADR-0010) keeps counting with no error ever surfacing, and the owner's only way out is
/// restarting the process. `catch_unwind`ing the future here converts that into an honest
/// `mark_failed`, matching what every other failure mode in this job already does. Returns the
/// [`AbortHandle`] the caller passes to [`set_abort`], same as calling `tokio::spawn` directly.
pub fn spawn_job<F>(jobs: &Jobs, slug: &str, work: F) -> AbortHandle
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let jobs = jobs.clone();
    let slug = slug.to_string();
    let handle = tokio::spawn(async move {
        if AssertUnwindSafe(work).catch_unwind().await.is_err() {
            mark_failed(
                &jobs,
                &slug,
                "internal error: the background job panicked".to_string(),
            );
        }
    });
    handle.abort_handle()
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

// ---- pending-message queue ------------------------------------------------
//
// A discussion idea holds at most one in-flight job (above), so a message sent while a job runs
// used to be dropped. Instead each idea has a small FIFO of pending chat messages: a busy send is
// enqueued (not forced to wait), the poll loop drains the next one whenever the idea goes idle,
// and the owner can drop any queued message before it runs. In-memory like the job slots — a
// process restart loses an un-started queue, exactly as it loses an in-flight job.

/// One message waiting its turn. `id` is unique per process so the remove control can name it.
#[derive(Clone)]
pub struct QueuedMessage {
    pub id: u64,
    pub text: String,
}

/// Per-idea pending-message FIFOs plus the monotonic id source, behind one lock.
pub struct QueueRegistry {
    map: Mutex<HashMap<String, VecDeque<QueuedMessage>>>,
    next_id: AtomicU64,
}

pub type Queues = Arc<QueueRegistry>;

pub fn new_queues() -> Queues {
    Arc::new(QueueRegistry {
        map: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
    })
}

/// Per-idea cap so a stuck backend can't let the queue grow without bound.
pub const MAX_QUEUED: usize = 20;

/// Append a pending message; returns its id, or `None` if the idea is already at [`MAX_QUEUED`].
pub fn enqueue(queues: &Queues, slug: &str, text: &str) -> Option<u64> {
    let mut map = queues.map.lock().ok()?;
    let q = map.entry(slug.to_string()).or_default();
    if q.len() >= MAX_QUEUED {
        return None;
    }
    let id = queues.next_id.fetch_add(1, Ordering::Relaxed);
    q.push_back(QueuedMessage {
        id,
        text: text.to_string(),
    });
    Some(id)
}

/// Pop the oldest pending message (FIFO) for the drainer to start. Removes the idea's empty FIFO.
pub fn dequeue(queues: &Queues, slug: &str) -> Option<QueuedMessage> {
    let mut map = queues.map.lock().ok()?;
    let q = map.get_mut(slug)?;
    let msg = q.pop_front();
    if q.is_empty() {
        map.remove(slug);
    }
    msg
}

/// Drop one queued message by id (the owner's per-item remove control). `true` if it was present.
pub fn remove_queued(queues: &Queues, slug: &str, id: u64) -> bool {
    let Ok(mut map) = queues.map.lock() else {
        return false;
    };
    let Some(q) = map.get_mut(slug) else {
        return false;
    };
    let before = q.len();
    q.retain(|m| m.id != id);
    let removed = q.len() != before;
    if q.is_empty() {
        map.remove(slug);
    }
    removed
}

/// The idea's pending messages in send order (for the queue panel).
pub fn list_queued(queues: &Queues, slug: &str) -> Vec<QueuedMessage> {
    let Ok(map) = queues.map.lock() else {
        return Vec::new();
    };
    map.get(slug)
        .map(|q| q.iter().cloned().collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawn_job_catches_a_panic_and_marks_failed_instead_of_leaving_the_slot_stuck() {
        let jobs = new_registry();
        assert!(try_claim(&jobs, "i"));
        let abort = spawn_job(&jobs, "i", async {
            panic!("boom");
        });
        set_abort(&jobs, "i", abort);

        // Poll until the spawned task has run (it's a separate tokio task, so give it a beat).
        let mut pending = peek(&jobs, "i");
        for _ in 0..100 {
            if !matches!(pending, Pending::Running { .. }) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            pending = peek(&jobs, "i");
        }
        match pending {
            Pending::Failed(msg) => assert!(msg.contains("panicked"), "message was: {msg}"),
            _ => panic!("expected the panic to surface as Failed"),
        }
        // The slot is free again — a panicked job must not wedge the idea forever.
        assert!(try_claim(&jobs, "i"));
    }

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
    fn try_claim_idle_refuses_a_failed_slot_so_the_drainer_never_eats_an_unshown_error() {
        let jobs = new_registry();
        mark_failed(&jobs, "i", "boom".into());
        // A Failed slot is "occupied" until a poll consumes it — the drainer must back off.
        assert!(!try_claim_idle(&jobs, "i"));
        // ...but the regular reclaim path still overwrites it (owner-initiated next action).
        assert!(try_claim(&jobs, "i"));
    }

    #[test]
    fn queue_is_fifo_and_removes_the_empty_slot() {
        let queues = new_queues();
        let a = enqueue(&queues, "i", "first").unwrap();
        let b = enqueue(&queues, "i", "second").unwrap();
        assert_ne!(a, b, "ids are unique");
        assert_eq!(list_queued(&queues, "i").len(), 2);
        let first = dequeue(&queues, "i").unwrap();
        assert_eq!(first.text, "first", "FIFO order");
        assert!(remove_queued(&queues, "i", b), "remove the remaining by id");
        assert!(dequeue(&queues, "i").is_none(), "empty now");
        assert!(list_queued(&queues, "i").is_empty());
    }

    #[test]
    fn enqueue_respects_the_per_idea_cap() {
        let queues = new_queues();
        for n in 0..MAX_QUEUED {
            assert!(enqueue(&queues, "i", &format!("m{n}")).is_some());
        }
        assert!(
            enqueue(&queues, "i", "one too many").is_none(),
            "the cap is enforced"
        );
    }

    #[test]
    fn remove_queued_of_a_missing_id_is_false() {
        let queues = new_queues();
        enqueue(&queues, "i", "only").unwrap();
        assert!(!remove_queued(&queues, "i", 9999));
        assert!(!remove_queued(&queues, "other", 1));
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
