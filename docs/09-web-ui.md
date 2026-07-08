# 09 — Web UI

> The HTTP surface: the route map, the request/middleware pipeline, the Askama template hierarchy,
> and the HTMX interaction patterns (background-job polling, not SSE). Home of **D16** (request
> lifecycle) and **D17** (route map). Module: `web`. Decisions:
> [ADR-0001](./adr/0001-server-rendered-htmx-over-spa.md),
> [ADR-0010](./adr/0010-ai-turns-as-background-jobs.md) (supersedes the earlier SSE decision,
> [ADR-0004](./adr/0004-sse-token-streaming.md)),
> [ADR-0011](./adr/0011-live-switchable-llm-backend.md),
> [ADR-0016](./adr/0016-forced-compact-folds-fully.md) (the compact route's `Notice` pending state),
> [ADR-0017](./adr/0017-web-access-tools.md) (the Settings page's `web_access` checkbox).

## Interaction model

Server-rendered HTML + HTMX, no SPA. Two response shapes — there is no long-lived streaming
response anywhere in the app:

- **Full page** — initial navigations (list, idea view, history, settings). Rendered from a base
  Askama layout.
- **Partial** — an HTML fragment swapped into the DOM by HTMX (e.g. a new idea row, an appended
  turn, a re-rendered transcript/memory panel). AI-driven routes (chat/skill/swarm/store) return a
  partial immediately — a transcript plus a "thinking" indicator — and the indicator self-repolls
  `GET /idea/:slug/pending` until the background job finishes
  ([ADR-0010](./adr/0010-ai-turns-as-background-jobs.md), [D11](./05-ai-integration.md)); store's
  poll response widens to the stored view once its job lands (D12).

## D17 — Route map

Every route, its method, response shape, and the template it renders.

```mermaid
flowchart LR
    subgraph pages["Full pages"]
        R1["GET / — idea list + search"]
        R2["GET /idea/:slug — idea view (body, convo, memory)"]
        R12["GET /idea/:slug/history — read-only full thread + Fork control"]
        R13["GET /settings — live LLM backend + params form (incl. web_access checkbox, ADR-0017)"]
        R19["GET /idea/:slug/artifact/:name — view one artifact (.md full page | .html served raw)"]
    end
    subgraph partials["HTMX partials"]
        R3["POST /ideas — create (D10) → idea row / redirect"]
        R4["POST /idea/:slug/store — Store (D12, job) → transcript + indicator"]
        R5["POST /idea/:slug/reopen — Reopen (D13) → discussion view"]
        R6["POST /idea/:slug/skill/:name — run skill (D18, job) → transcript + indicator"]
        R7["POST /idea/:slug/swarm — run swarm (D14, job) → transcript + indicator"]
        R8["GET /search?q= — results fragment (FTS)"]
        R9["POST /idea/:slug/chat — chat turn (D11, job) → transcript + indicator"]
        R9b["GET /idea/:slug/pending — poll target → transcript (indicator | error | final)"]
        R14["POST /idea/:slug/fork — branch to a new InDiscussion idea → HX-Redirect"]
        R15["POST /idea/:slug/turn/:index/delete — remove one turn → transcript"]
        R16["POST /idea/:slug/memory/:fact/delete — remove one memory fact → memory panel"]
        R13b["POST /settings — apply live settings → settings form"]
        R18["POST /idea/:slug/extract — run knowledge extraction (D30, job) → transcript + indicator"]
        R20["POST /idea/:slug/artifact/:name/delete — remove one artifact file → artifacts panel"]
        R21["POST /idea/:slug/compact — fold now (ADR-0012/0016, job) → transcript + indicator | notice"]
        R22["POST /idea/:slug/workflow/:name — run workflow (D19, job) → transcript + indicator"]
    end
    subgraph admin["Admin"]
        R10["POST /admin/reindex — rebuild index (D15)"]
        R11["GET /admin/health — LLM backend probe (D20)"]
        R17["GET /static/{*path} — static assets"]
    end

    R1 --> T_LIST["templates/list.html"]
    R2 --> T_IDEA["templates/idea.html"]
    R3 --> T_ROW["templates/_idea_row.html"]
    R4 --> T_TURN["templates/_turn.html (via transcript partial)"]
    R5 --> T_DISC["templates/_discussion.html"]
    R6 --> T_TURN
    R7 --> T_TURN
    R8 --> T_RESULTS["templates/_search_results.html"]
    R9 --> T_TURN
    R9b --> T_TURN
    R9b -.->|"store job lands"| T_STORED["templates/_stored.html (HX-Retarget #discussion)"]
    R12 --> T_HIST["templates/history.html"]
    R13 --> T_SET["templates/settings.html"]
    R13b --> T_SETF["templates/_settings.html"]
    R15 --> T_TURN
    R16 --> T_MEM["templates/_memory.html"]
    R18 --> T_TURN
    R19 --> T_ART["templates/artifact.html (.md) | raw .html export"]
    R20 --> T_ARTS["templates/_artifacts.html"]
    R21 --> T_TURN
    R22 --> T_TURN
```

Route groups map to `web::routes` submodules: `ideas` (R1, R2, R3, R8, R9b, R12, R14), `chat` (R9),
`memory`/idea-actions (R4–R7, R15, R16, R22 — the module name predates the delete/workflow routes
but still owns them; R22 (`run_workflow`) runs the D19 deterministic workflow DAG behind the same
claim → spawn → poll job shape as R6/R7), `settings` (R13, R13b), `admin` (R10, R11, R17),
`artifacts` (R18, R19, R20 — knowledge extraction and its per-idea artifact files,
[ADR-0015](./adr/0015-knowledge-extraction-artifacts.md)), `compact` (R21 — the manual "compact
now" fold, [ADR-0012](./adr/0012-auto-compact.md)/[ADR-0016](./adr/0016-forced-compact-folds-fully.md)).

## D16 — HTTP request / middleware pipeline

How a request traverses tower middleware to a handler and back, and where the two response shapes
diverge. Error mapping here implements the taxonomy [D24](./05-ai-integration.md).

```mermaid
flowchart TD
    REQ["incoming request"] --> TRACE["tower: tracing / request log"]
    TRACE --> STATE["inject AppState (config, index, LlmBackend, semaphore, jobs registry)"]
    STATE --> ROUTE["axum router match (D17)"]
    ROUTE --> HANDLER["handler"]
    HANDLER --> BRANCH{"AI-driven route?"}
    BRANCH -- "no (page / partial)" --> RENDER["Askama render → HTML"]
    BRANCH -- "yes (chat/skill/swarm/workflow/store/extract/compact)" --> JOBBR["try_claim + persist up front + tokio::spawn detached task (D11, ADR-0010)"]
    JOBBR --> RENDER2["render transcript + thinking indicator → HTML"]
    RENDER --> ERRMAP
    RENDER2 --> ERRMAP
    ERRMAP["error → response mapping (D24)"] --> RESP["response"]
    RESP -.->|"browser polls"| POLL["GET /idea/:slug/pending re-enters this pipeline"]
```

## Template hierarchy (Askama)

Compile-time templates under `templates/`, backed by `web::templates` structs.

```
templates/
  base.html              # layout: head, vendored htmx.min.js, nav, {% block content %}
  list.html              # extends base — idea list + search box
  idea.html              # extends base — one idea: body (rendered md), conversation, memory panel
  history.html            # extends base — the "btw" read-only full thread + Fork control
  settings.html           # extends base — live LLM backend + params page
  _idea_row.html         # partial — a single idea in the list
  _turn.html             # partial — one conversation turn (user/assistant); also the poll-target shape
  _discussion.html       # partial — the discussion pane (compose box + transcript/poll target)
  _actions.html          # partial — the #idea-actions block (moves/swarm/store); also sent OOB
  _stored.html           # partial — stored view (consolidated body + memory facts); delivered by
                          #   the R9b poll once a store job (R4) lands truth as Stored, via
                          #   HX-Retarget #discussion (respond_discussion_or_stored)
  _search_results.html   # partial — FTS results
  _memory.html            # partial — the memory panel (re-rendered after a fact delete)
  _settings.html          # partial — the settings form (re-rendered after a save)
  artifact.html           # extends base — one .md artifact rendered as a full page (R19)
  _artifacts.html         # partial — the artifacts panel (re-rendered after an artifact delete)
  artifact_export.html    # standalone (no base) — the opt-in .html knowledge report, written to
                          #   disk by R18, not served directly by any route
```

Convention: files prefixed `_` are HTMX partials (never a full page); everything else `extends
base.html`.

## HTMX / polling patterns

- **Create / actions:** `hx-post` on forms/buttons; server returns a partial that `hx-swap` inserts.
- **Chat / skill / swarm / workflow / store / extract / compact (background job + poll):** the
  compose form (or a skill/swarm/workflow/store/compact button) posts to its route; the handler
  claims the per-idea job slot, persists what it can up front, spawns a detached task, and
  immediately returns a transcript partial ending in a "thinking…" indicator block. That block is
  itself an HTMX fragment
  (`hx-get="/idea/:slug/pending" hx-trigger="load delay:1500ms" hx-target="#transcript"`) that
  re-fires ~1.5s after it lands; each poll response either re-emits the same self-triggering
  indicator (job still running, with an updated elapsed-seconds count), an error block (job
  failed — consumed on read), a neutral notice block (job completed as a genuine no-op — consumed
  on read the same way, currently only emitted by the manual compact route's `NothingToFold`
  outcome, [ADR-0016](./adr/0016-forced-compact-folds-fully.md)), or the finished transcript with
  no further trigger (job done). This survives navigation because the underlying model call runs in
  a task detached from any one request ([ADR-0010](./adr/0010-ai-turns-as-background-jobs.md)). The
  workflow route (R22, D19) only persists its converged synthesis as one
  `## assistant (workflow: {name})` turn — intermediate fan-out/judge steps are not written to
  `conversation.md`, mirroring the swarm's discard-intermediates rule — and the finished turn's
  label keeps the workflow kind (`foil · workflow {name}`, distinguishing it from a same-named
  skill turn's `foil · {name}`). The store route (R4, D12) is the one exception to "finished
  transcript with no further trigger": when its job lands, truth has already flipped to `Stored`,
  so the poll response instead widens to the stored view — see the next bullet.
- **Store's finish path — poll widens to the stored view:** because only the store job can leave an
  idea `Stored` (every other job route guards on the discussion states), the shared poll handler
  (`web::routes::ideas::respond_discussion_or_stored`, serving both R9b and cancel) checks the
  on-disk state on every poll: while `InDiscussion`/`Reopened` it returns the normal transcript
  poll response, but once state is `Stored` it instead renders `_stored.html` (consolidated body +
  memory facts) plus the OOB `state--stored` badge, and sends `HX-Retarget: #discussion` /
  `HX-Reswap: innerHTML` response headers so HTMX swaps the *whole* discussion panel (composer and
  actions included) instead of just `#transcript` — the same swap the old synchronous store
  response used to perform directly.
- **Out-of-band state refresh:** transcript responses (chat, poll, cancel, skill, swarm, workflow,
  store, extract, compact, delete-turn) append two top-level `hx-swap-oob="true"` fragments after the
  `#transcript` inner HTML: the `#idea-state` subhead badge and the `#idea-actions` block
  (`_actions.html`, an always-present container so a Draft page still has the OOB target). This is
  how the first chat turn's Draft → InDiscussion flip becomes visible — badge and moves/store
  controls update without a reload, while the composer (outside `#transcript`) survives a poll
  completing mid-typing. Store's own immediate response follows this same shape (transcript +
  indicator + OOB badge/actions, `hx-target="#transcript"`); it's only the poll response once the
  store job *lands* that swaps all of `#discussion` (previous bullet). Reopen still swaps all of
  `#discussion` synchronously, so it carries only the OOB badge.
- **Markdown rendering:** idea bodies and memory facts are rendered server-side (markdown → sanitized
  HTML) before templating; the browser only receives HTML.
- **Degraded AI:** when `/admin/health` (or the boot probe) reports the active LLM backend absent,
  the compose box is rendered disabled with the banner from [D20](./05-ai-integration.md);
  read-only browsing is unaffected. Which backend counts as "active" follows the live Settings
  toggle ([ADR-0011](./adr/0011-live-switchable-llm-backend.md)).

## Mapping to code

| Piece | Location |
|-------|----------|
| Router + AppState + middleware | `app.rs` |
| Route handlers | `web::routes::{ideas,chat,memory,settings,admin,artifacts,compact}` |
| Background job registry + poll | `web::jobs` (shared by chat R9, skill R6, swarm R7, workflow R22, store R4, extract R18, compact R21, and the R9b poll endpoint) |
| Template structs | `web::templates` |
| Template sources | `templates/*.html` |

## Related

- [05-ai-integration](./05-ai-integration.md) — D11 background-job flow, D20 degradation, D24 errors.
- [06-concepts/swarm](./06-concepts/swarm.md) — D30, the extraction flow R18/R19/R20 drive.
- [06-concepts/workflows](./06-concepts/workflows.md) — D19, the deterministic DAG R22 runs.
- [07-flows](./07-flows.md) — the flows that enter through these routes.
- [ADR-0010](./adr/0010-ai-turns-as-background-jobs.md), [ADR-0011](./adr/0011-live-switchable-llm-backend.md),
  [ADR-0012](./adr/0012-auto-compact.md), [ADR-0015](./adr/0015-knowledge-extraction-artifacts.md),
  [ADR-0016](./adr/0016-forced-compact-folds-fully.md), [ADR-0017](./adr/0017-web-access-tools.md).
