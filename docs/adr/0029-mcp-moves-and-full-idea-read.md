# ADR-0029 — MCP moves and a full-idea read: driving ideation from an MCP client

- **Status:** Accepted — amended by [ADR-0036](./0036-mcp-list-workflows-and-run-workflow.md) (workflows are no longer web-only: `list_workflows` and `run_workflow`)
- **Date:** 2026-09-29
- **Deciders:** Owner
- **Amends:** [ADR-0024](./0024-mcp-server-inbound.md). The inbound tool catalog grows from 7 to
  11 tools: `list_skills`, `run_skill`, `run_swarm`, `get_artifact`. `get_idea` now returns the
  whole idea, and the two canned prompts make the client a relay rather than a foil.

## Context

The owner wants to run an idea's discussion from a Claude Code session, with idea-vault connected
as an MCP server, instead of from the web UI. ADR-0024's first tool set covered only the minimal
loop (create, chat, store, reopen). Three problems made that loop too thin to use for real ideation:

- **No moves.** The skill book and the swarm are how an idea gets "run into the ground" (the
  CLAUDE.md product loop). They existed only as web routes R6 and R7.
- **A partial read.** `get_idea` returned the MEMORY.md one-liners but not the fact bodies, and it
  listed no artifacts. So the quarantine a store writes ([ADR-0023](./0023-verification-layer.md))
  and every swarm/extract report were invisible over MCP.
- **Two foils.** The `continue-discussion` prompt told the client to "act as a rigorous ideation
  foil" and send that as a chat turn. `chat` saves its message as the owner's `## user` turn and
  idea-vault's own model answers it. The result was a client critique in the owner's voice,
  followed by the real foil's reply.

The owner chose to keep idea-vault's own model as the foil (Ollama or the claude CLI, per the
role/profile settings of [ADR-0026](./0026-per-role-call-profiles.md)). The MCP client relays
messages and picks moves.

## Decision

We will expose the moves the owner needs to finish an idea over MCP.

- **`list_skills`** (synchronous). The visible skill book: name, description, stage, role,
  use/avoid guidance and source.
- **`run_skill(slug, name)`** and **`run_swarm(slug, angles?)`**. These are
  `TaskSupport::Optional` long-running tools with the same task and bounded-wait paths as `chat`
  ([ADR-0028](./0028-optional-task-support-bounded-wait.md)). Their sync guards and job spawns are
  the web routes' own `memory::{guard_skill, spawn_skill_job, guard_swarm, spawn_swarm_job}`, split
  out of R6 and R7 so both surfaces run one copy of the rules (HND-10). Like R6 and R7, they claim
  with `try_claim` (an owner action, not the chat queue). A lost claim is an `invalid_params` busy
  error. A plain retry reattaches on the skill name, or on the comma-joined angle list. The result
  is the newest turn, i.e. the move's or swarm synthesis's assistant turn.
- **`get_idea`** returns everything the idea page shows:
  - memory facts with their bodies, plus the MEMORY.md summary
  - the compacted-context summary
  - the markdown artifact list
  - the names of the derived `.html` reports
- **`get_artifact(slug, artifact)`** (synchronous) reads one markdown artifact.
- **Prompts relay.** `continue-discussion` and `new-idea` tell the client to send the owner's words
  verbatim with `chat`, to offer moves via `list_skills`/`run_skill`/`run_swarm`, never to write
  foil turns itself, and to call `store_idea` when the owner says they're done. The server
  `instructions` string says the same.

## Consequences

- An MCP client can run the whole ideation loop:
  - create an idea and chat
  - run spine moves and swarms
  - store it (verified memory plus quarantine) and read what was stored
  - reopen it
- Still web-only:
  - workflows (R22), compact (R21), extract (R18)
  - tags, sources, fork, rename, and delete-*
  - Settings
  - the chat queue: a `chat` on a busy idea is still refused, not queued
  - MCP `resources`

  Adding any of these still requires amending ADR-0024's scope, as this ADR does.
- The business-error asymmetry from ADR-0024 now covers four long-running tools. Their guard
  failures (unknown skill or angle, Draft/Stored idea, busy idea, too many angles, a capstone or
  converge angle) are JSON-RPC `invalid_params` errors. The synchronous tools' errors stay
  tool-result errors.
- `get_idea`'s payload is larger (fact bodies and artifact metadata, not artifact bodies). A client
  reads an artifact body only on demand with `get_artifact`.

## Alternatives considered

- **Make the MCP client the foil** (an `append_turn` tool with no model call, and a store that takes
  client-authored facts). Rejected for now by the owner's choice: the app's own foil, its role
  profiles and the verification gate stay the single source of the idea's reasoning. It stays
  possible later as an additive mode.
- **One generic `run_move(kind, …)` tool** covering skill, swarm and workflow. Rejected: the three
  take different arguments, and one union schema would hide which fields apply. Separate tools keep
  each schema `additionalProperties: false` and self-describing.
- **Expose artifacts and memory as MCP `resources`.** Deferred: Claude Code's tool path is the one
  the owner uses, and `get_artifact` is a smaller change than adding the resources capability plus
  subscription semantics.
- **Full parity with the web UI in one pass.** Rejected as scope. The tools that are only needed
  occasionally (tags, fork, compact, extract, workflows) wait until using the loop over MCP shows
  they are missed.

---

> ADRs are immutable once **Accepted**. To change a decision, write a new ADR that supersedes this
> one and update the Status line above.
