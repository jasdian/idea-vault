# ADR-0001 — Server-rendered HTML + HTMX over a JS SPA

- **Status:** Accepted
- **Date:** 2026-07-07
- **Deciders:** owner

## Context

idea-vault is a single-user localhost tool written in Rust. The UI needs to render markdown, show a
live-streaming AI conversation, and offer light interactivity (create idea, submit turn, store,
reopen). We must decide how the frontend is built and served. The project values a single
self-contained binary and no external toolchains.

## Decision

We will render HTML on the server with **Askama** templates and drive interactivity with **HTMX**,
returning HTML **partials** for dynamic updates. AI output streams over **SSE** (see
[ADR-0004](./0004-sse-token-streaming.md)). There is **no JavaScript SPA** and **no JS build step**.
>
> **Note (2026-07-07):** the "AI output streams over SSE" clause was superseded by
> [ADR-0010](./0010-ai-turns-as-background-jobs.md) — AI turns now run as detached background jobs
> polled via HTMX, not SSE. Everything else in this decision (Askama + HTMX partials, no SPA, no JS
> build step) is unchanged and still authoritative.

## Consequences

- The app ships as one binary; the only client asset is a vendored HTMX file (and optional small CSS).
- Handlers return either full pages or HTML fragments; there is no JSON API contract to maintain for
  the UI. (An internal API may still exist for admin/reindex.)
- Frontend state lives in the DOM and on the server, not in a client framework — simpler to reason
  about for a solo tool, at the cost of rich client-side interactions we don't need.
- Rich, highly-interactive UI patterns (drag-drop canvases, complex client state) would be awkward;
  we accept this because the product is conversation- and document-centric.

## Alternatives considered

- **TypeScript SPA (React/Svelte) + JSON API** — more UI power, but adds a second toolchain, a build
  step, and a client/server contract to keep in sync. Rejected: overhead with no payoff for a
  single-user document/chat tool.
- **Server-rendered with no HTMX (full-page reloads)** — simplest, but a token-streaming chat with
  full-page reloads is a poor experience. Rejected: streaming UX matters here.
- **WASM frontend (Leptos/Yew)** — all-Rust and capable, but reintroduces a build/bundle step and
  heavier client runtime. Rejected: HTMX gets us streaming interactivity with far less machinery.
