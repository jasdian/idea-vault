# ADR-0010 — AI turns run as detached background jobs, not SSE streams

- **Status:** Accepted (**supersedes** [ADR-0004](./0004-sse-token-streaming.md))
- **Date:** 2026-07-07
- **Deciders:** owner

## Context

[ADR-0004](./0004-sse-token-streaming.md) chose Server-Sent Events so the browser could see AI
tokens as they arrived from Ollama. Building it out surfaced two problems that make SSE unworkable
as originally decided:

1. **A model call tied to the request future dies with the request.** Holding the axum handler
   open for the whole generation means the future driving the Ollama call *is* the request future.
   Navigating away, closing the tab, or a dropped connection cancels that future — which kills the
   in-flight generation, and the "the foil is thinking" state, being purely client-side (an open
   `EventSource`), is lost with it. A slow local model (tens of seconds to minutes) makes this a
   routine annoyance, not an edge case.
2. **The htmx SSE extension was never vendored.** ADR-0004's plumbing assumed HTMX's `sse`
   extension (`hx-ext="sse"`, `sse-connect`) would be available alongside the vendored `htmx.min.js`
   ([ADR-0001](./0001-server-rendered-htmx-over-spa.md)); it never shipped, so browser-side SSE
   consumption never actually worked end to end.

The underlying UX goal from ADR-0004 — don't let a slow local model feel dead, and don't block the
request thread — still stands; only the transport was wrong.

## Decision

We will run every AI-driven turn (chat, skill invocation, swarm) as a **detached background job**,
not a long-lived SSE response. `web::jobs` owns a shared registry (`Jobs = Arc<Mutex<HashMap<String,
Job>>>`) keyed by idea slug, **one job per idea**:

- `try_claim(jobs, slug) -> bool` — claim the single job slot for an idea; returns `false` if a job
  is already running (the caller must not start a second one — a second "Send" while busy just
  re-shows the in-flight state instead of queueing).
- `mark_done(jobs, slug)` — clear the slot on success (the result is already on disk).
- `mark_failed(jobs, slug, message)` — record a human-readable failure message, read once by the
  next poll then cleared.
- `spawn_job(jobs, slug, work) -> AbortHandle` — every route's `tokio::spawn` call site goes through
  this instead of `tokio::spawn` directly: it wraps `work` in `catch_unwind` so a panic partway
  through (not just a `Result::Err`) still reaches `mark_failed` instead of leaving the slot
  `Running` forever with nothing left to ever clear it — a bare `tokio::spawn` swallows a panicking
  task's result silently.
- `peek(jobs, slug) -> Pending` — `Pending::Running(u64)` (elapsed whole seconds since start),
  `Pending::Failed(String)` (consumed on read), or `Pending::Idle` (no job — the transcript on disk
  is final).

Mechanics of one turn (chat as the canonical case; skill and swarm follow the same shape):

1. `POST /idea/{slug}/chat` validates the form, claims the job slot, **persists the user turn to
   `conversation.md` up front** (so it survives navigation and renders under the indicator even if
   the tab is closed and reopened), makes the `Draft→InDiscussion` transition if this is the first
   turn, then `tokio::spawn`s a **detached** task carrying its own clone of `AppState` — not the
   request future — and returns immediately with the transcript-plus-"thinking"-indicator partial.
2. The detached task assembles the budgeted context, acquires the shared AI semaphore
   ([ADR-0006](./0006-bounded-concurrency-swarm.md)), calls the active `LlmBackend`
   ([ADR-0009](./0009-pluggable-llm-backend-claude-code.md)/[ADR-0011](./0011-live-switchable-llm-backend.md)),
   and **only on success** appends the assistant turn to `conversation.md` and reindexes
   (log-not-fail); on failure it calls `mark_failed` with a human-readable message instead of
   writing anything to the vault. A partial/empty reply is never persisted.
3. The idea page and `GET /idea/{slug}/pending` both read `jobs::peek` to render (or resume, after
   navigation) a **server-driven "thinking… Ns" indicator** — the elapsed-seconds count is computed
   server-side from `Job.started`, not tracked in the browser — and swap in the finished transcript
   once `peek` reports `Idle`. A `Failed` result surfaces as a visible error in the transcript pane,
   consumed exactly once. The indicator is a **self-repolling** `hx-trigger="load delay:1500ms"`
   element: each poll response must itself carry a fresh copy to keep the chain alive. htmx never
   swaps on a 4xx/5xx (or a dropped-connection) response, which would otherwise permanently kill the
   chain with the last-rendered "…Ns" frozen on screen; a `base.html` listener on
   `htmx:responseError`/`htmx:sendError` re-fires the same element's `load` trigger after a short
   delay so a transient failure self-heals instead of requiring a manual reload.

Skills (`POST /idea/{slug}/skill/{name}`) and swarm (`POST /idea/{slug}/swarm`) use the identical
claim → spawn → poll pattern; only the work inside the detached task differs (a single skill
invocation vs. a bounded fan-out/converge).

## Consequences

- **Navigation-safe generation.** A model call now outlives the request/connection that started it;
  the owner can navigate away and back (or lose the connection) without losing the reply.
- **No streaming UX.** The transcript updates in one swap when the job completes, not token by
  token; the loss is mitigated by the visible elapsed-seconds "thinking" indicator so the app never
  looks hung.
- **Simpler failure handling.** Because the assistant turn is only ever written after a complete,
  non-empty reply, "never persist a partial turn" — an invariant ADR-0004 also had to maintain
  under disconnect — now falls out of the design for free: there is no partial to accidentally
  persist.
- **One job per idea, not per request.** A second Send while a job is running does not queue or
  clobber; it just re-shows the in-flight state. This is a deliberate simplicity choice — no queue,
  no cancellation UI — acceptable for a single-owner localhost tool.
- **`ai::stream` and `web::sse` obligations are dropped.** The NDJSON→SSE token adapter and shared
  SSE plumbing described in ADR-0004 are no longer part of the chat path.
- **New endpoint.** `GET /idea/{slug}/pending` — the poll target; returns the same transcript
  partial the POST handlers return.

## Alternatives considered

- **Keep SSE, hold the connection across a client reconnect (`Last-Event-ID`)** — would fix the
  htmx-extension gap but not the request-future-cancellation problem, since the *server-side* Ollama
  call is still what needs to survive; rejected as solving only half the issue.
- **WebSockets** — same request-lifetime coupling problem as SSE unless paired with the same
  detached-task pattern anyway, at the cost of a heavier transport; rejected as ADR-0004 already
  rejected it for being unnecessary, and this ADR doesn't change that calculus.
- **Vendor the htmx SSE extension and keep streaming** — fixes problem 2 but not problem 1
  (navigation still kills the generation); rejected as incomplete.
- **Client-side polling with a JS timer** instead of a self-repolling HTMX fragment
  (`hx-get="/idea/{slug}/pending" hx-trigger="load delay:1500ms"` on the indicator itself, so each
  poll response either re-emits the same self-triggering indicator or swaps in the finished
  transcript) — would reintroduce hand-rolled client JS, conflicting with the no-SPA/no-custom-JS
  principle ([ADR-0001](./0001-server-rendered-htmx-over-spa.md)); rejected — HTMX's declarative
  triggers cover this without custom script.

---

> ADRs are immutable once **Accepted**. To change this decision, write a new ADR that supersedes it.
