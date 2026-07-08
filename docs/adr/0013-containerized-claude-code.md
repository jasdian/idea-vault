# ADR-0013 — claude-code backend in the container: bind-mount CLI + `setup-token` + `HOME=/claude` volume

- **Status:** Accepted
- **Date:** 2026-07-08
- **Deciders:** owner

## Context

[ADR-0009](./0009-pluggable-llm-backend-claude-code.md) added the claude-code backend and
[ADR-0011](./0011-live-switchable-llm-backend.md) made it live-switchable, but both were exercised
only on a **native** (`cargo run`) install. The containerized stack ([ADR-0008](./0008-containerized-local-deployment.md))
never wired it up: the runtime image (`debian:bookworm-slim`) ships no `claude` CLI, the base
`docker-compose.yml` passes zero claude env vars, and no auth path exists inside the container. The
UI surfaces this honestly ("the foil is offline — the `claude` CLI isn't runnable… set
`IDEA_VAULT_CLAUDE_BIN`…"), but the owner hosts the stack in containers (NixOS host) and wants the
agentic backend to work there without abandoning the containers-first deployment story.

Two things must be true of any solution:

1. **The owner's own `claude` CLI install stays authoritative** — no separate credential material
   baked into the image, no drift between the host CLI version and an in-image copy.
2. **Claude project history must persist** across container recreation — the same expectation the
   named `ollama-models` and `idea-index` volumes already meet for their own state.

## Decision

Add an **override compose file**, `docker-compose.claude.yml`, following the existing
`docker-compose.gpu.yml` house pattern (touches only what changes, heavy explanatory header,
composable with the base file):

- **Bind-mount the host `claude` binary read-only** — `${IDEA_VAULT_CLAUDE_HOST_BIN:-~/.local/bin/claude}:/opt/claude/claude:ro`
  — instead of installing/baking it into the image. `IDEA_VAULT_CLAUDE_BIN` is fixed to
  `/opt/claude/claude` by the override (the app has no PATH entry for it).
- **Auth via a long-lived token, not the interactive login flow.** One-time on the host:
  `claude setup-token`, then set the result as `CLAUDE_CODE_OAUTH_TOKEN` in `.env`. The spawned
  `claude` child already inherits the full service environment (`src/ai/claude_code.rs` never calls
  `.env()`/`env_clear()`), so the token reaches the CLI with **zero Rust changes**. The override
  guards it with compose's `:?` interpolation (`CLAUDE_CODE_OAUTH_TOKEN:?run 'claude setup-token'
  on the host and set it in .env`) so `up`/`config` fails fast with a clear message instead of
  booting a container that can authenticate nothing.
- **Persist all CLI state on a named volume via `HOME=/claude`.** Setting `HOME` (not
  `CLAUDE_CONFIG_DIR`) relocates *everything* the CLI writes — `.claude/` (projects, history,
  settings) and `.claude.json` — onto one `claude-state` named volume with one env var, matching
  the "vault is a bind mount, everything else is a rebuildable/re-pullable named volume" split
  from [12-deployment](../12-deployment.md). The `Dockerfile` pre-creates and `chown`s `/claude`
  alongside `/data`/`/vault` so a *freshly created* volume copies the app uid's ownership (the same
  cosmic/zomboid volume-ownership gotcha D27 already documents for `/data`/`/vault`) — otherwise the
  non-root `user:` cannot write CLI state to a root-owned volume.
- **`DISABLE_AUTOUPDATER=1`** — the binary is read-only and host-managed; the CLI must not try to
  rewrite itself inside a ro mount.
- Ollama's `depends_on` stays untouched — the override boots claude-code as the *default* active
  backend (`IDEA_VAULT_LLM_BACKEND` still overridable, and live-switchable back to Ollama on the
  Settings page with no restart, [ADR-0011](./0011-live-switchable-llm-backend.md)).

Run: `docker compose -f docker-compose.yml -f docker-compose.claude.yml up -d --build`, composable
with the GPU override since the two touch disjoint services: `-f docker-compose.yml -f
docker-compose.gpu.yml -f docker-compose.claude.yml up -d`.

Alongside the compose file, `src/ai/claude_code.rs::classify_line`'s `Some("result")` arm now
surfaces an error `result` (`is_error: true` — the shape a bad/expired token returns) as
`Line::ErrorResult(text)` → `AiError::Backend("claude error: {text}")`, instead of silently mapping
to `Line::Result(None)` → `Ok("")`. The container health probe is `claude --version`, which needs
no auth and stays green regardless of token validity, so without this hardening a bad token would
have failed silently as a misleading "the model returned an empty reply" instead of a diagnosable
auth error in the UI.

## Consequences

- **New files/vars.** New `docker-compose.claude.yml`; new env vars `IDEA_VAULT_CLAUDE_HOST_BIN`
  (host path of the CLI to mount, default `~/.local/bin/claude`) and `CLAUDE_CODE_OAUTH_TOKEN`
  (required by the override, `:?`-guarded). `.env.example`'s claude section is now container-first;
  `IDEA_VAULT_CLAUDE_BIN` is marked **native-only** — do not set it for a containerized run, the
  override fixes it.
- **Symlink note.** The host default `~/.local/bin/claude` is a symlink into a versioned install
  dir; Docker dereferences it at container **start**, not continuously, so a host CLI update
  requires `docker compose -f docker-compose.yml -f docker-compose.claude.yml restart idea-vault`
  to pick up.
- **Rebuild-before-first-volume-creation pitfall.** The ownership `chown` only lands on a *fresh*
  `claude-state` volume created from the updated image. If the volume already exists from an older
  image (root-owned `/claude`), the CLI cannot write. Recovery: `down`, `docker volume rm
  idea-vault_claude-state`, rebuild, `up`.
- **Probe-green-but-auth-broken is possible** (`claude --version` needs no token) — mitigated, not
  eliminated, by the `classify_line` hardening above: a bad/expired token now fails the first chat
  turn with a real error message instead of an empty reply.
- **Token visible in `docker inspect`.** Accepted for a loopback, single-owner local tool — the
  same trust boundary the rest of the stack already assumes (no auth on `localhost:3000`).
- **Ollama still starts** even with claude-code as the default backend, so the owner is always one
  Settings toggle away from the offline local model — the two backends are never mutually exclusive
  at the container level.

## Alternatives considered

- **Bake the CLI into the image** (`npm install -g @anthropic-ai/claude-code` or similar in the
  Dockerfile) — adds a ~245MB layer (npm + a node runtime the rest of the crate has zero use for)
  and pins the in-image CLI version independent of whatever the owner updates on the host, so the
  two silently drift. Rejected.
- **Mount the whole `~/.claude` host directory into the container** — exposes the owner's full host
  credential/session store (not just this one token) to every container process, weighs ~2.3GB
  (project history for every repo, not just idea-vault's), and creates refresh races between the
  host CLI and the in-container CLI writing the same files concurrently. Rejected.
- **`npm install` the CLI at container start (entrypoint script)** — same node-runtime cost as
  baking it in, plus a slow, network-dependent container start. Rejected.
- **`CLAUDE_CONFIG_DIR` instead of `HOME`** — narrower in principle, but the CLI still needs a
  writable `$HOME` for other state; the image's default `$HOME=/app` is root-owned, so this would
  still require a second variable/volume for the parts `CLAUDE_CONFIG_DIR` doesn't cover. `HOME`
  alone relocates everything with one knob. Rejected as unnecessary complexity.
- **Ephemeral (non-persisted) CLI state** — simplest override, but violates the owner's explicit
  "claude project history must persist" requirement and would force a fresh, empty CLI state (and
  likely re-auth friction) on every `docker compose up`. Rejected.
- **Docker secrets for the token** — the standard hardening for multi-tenant/production secrets,
  but overkill for a loopback single-owner tool where `docker inspect` access already implies host
  access; adds a swarm/secrets-file dependency this stack otherwise has no use for. Rejected.

---

> ADRs are immutable once **Accepted**. To change this decision, write a new ADR that supersedes it.
