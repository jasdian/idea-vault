# ADR-0004 — SSE for AI token streaming

- **Status:** Superseded by [ADR-0010](./0010-ai-turns-as-background-jobs.md)
- **Date:** 2026-07-07
- **Deciders:** owner

> **Superseded by ADR-0010.** AI turns (chat/skill/swarm) now run as detached background jobs
> polled via `GET /idea/{slug}/pending`, not SSE — see
> [ADR-0010](./0010-ai-turns-as-background-jobs.md) for why (a model call tied to the request future
> died on client navigation/disconnect; the htmx SSE extension referenced below was never vendored,
> so browser-side SSE consumption never actually worked) and for the mechanics that replaced it.
> The body below is preserved unchanged as the historical record of the original decision.

## Context

Local models generate tokens over seconds; a blocking request that returns only when generation
finishes feels dead. We need the browser to show tokens as they arrive, within the constraints of
[ADR-0001](./0001-server-rendered-htmx-over-spa.md) (server-rendered + HTMX, no SPA).

## Decision

We will stream AI output to the browser using **Server-Sent Events (SSE)**. The axum handler holds
the request open and emits SSE events as tokens arrive from Ollama's streaming response; HTMX's SSE
support swaps each chunk into the transcript. The stream→SSE adaptation lives in `ai::stream`, shared
plumbing in `web::sse`.

## Consequences

- Communication is **one-way server→client**, which is exactly the shape of token streaming — no need
  for the complexity of WebSockets.
- SSE works over plain HTTP and through HTMX's `sse` extension with no custom client JS.
- Each streaming endpoint must handle client disconnect (abort the Ollama call) and completion
  (append the finished assistant turn to `conversation.md`).
- Long-lived connections mean handlers must be async and not hold blocking resources; the concurrency
  limiter still applies to the underlying Ollama calls.
- Bidirectional interactions (which we don't need) would require a different transport.

## Alternatives considered

- **WebSockets** — bidirectional and capable, but heavier and unnecessary for one-way streaming, and
  less ergonomic with HTMX. Rejected: overkill.
- **Long-polling / chunked fetch parsed in JS** — reintroduces client-side JavaScript we're avoiding.
  Rejected: conflicts with the no-SPA principle.
- **Blocking request, render on completion** — trivial, but the UX for slow local models is poor.
  Rejected: streaming is a core UX requirement.
