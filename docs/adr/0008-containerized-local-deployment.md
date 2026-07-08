# ADR-0008 — Containerized local deployment (app + Ollama), optional GPU

- **Status:** Accepted
- **Date:** 2026-07-07
- **Deciders:** owner

## Context

idea-vault is a localhost tool ([ADR-0001](./0001-server-rendered-htmx-over-spa.md)) that needs a
local Ollama server ([ADR-0003](./0003-ollama-local-only-ai.md)). The owner wants to **host the whole
thing locally in containers, with or without a GPU**, rather than installing a Rust toolchain and
Ollama on the host directly. Sibling repos establish house patterns: `mcp-server` (single Rust
service, multi-stage Docker build, rustls-not-OpenSSL, static-ish runtime), `cosmic-mmo` (Compose
topology, loopback-only publishing, profile-gated one-shot jobs, json-file log caps; notably its LLM
sidecar is **CPU-only**), and `zomboid-seasons` (SQLite persisted on a named volume, container
creates its `/data`). None of the four sibling repos contains an nvidia GPU passthrough example, so
that part is designed fresh.

## Decision

We will ship a **Docker Compose stack of two containers** — `idea-vault` (the Rust binary) and
`ollama` — plus a profile-gated `ollama-pull` one-shot. GPU support is an **optional override file**
(`docker-compose.gpu.yml`) that grants the nvidia device to the `ollama` service only. To make this
work, the app reads its bind address, vault dir, index path, and Ollama URL from **environment
variables** (`IDEA_VAULT_*`) instead of hardcoded `localhost`. The **`vault/` is a host bind mount**
(source of truth); the **SQLite index and Ollama models are named volumes** (derived / re-pullable).

## Consequences

- **App config becomes env-driven.** `config.rs` must read `IDEA_VAULT_BIND` (default `127.0.0.1:3000`,
  `0.0.0.0:3000` in-container), `IDEA_VAULT_VAULT_DIR`, `IDEA_VAULT_INDEX_PATH`, `IDEA_VAULT_OLLAMA_URL`
  (default `http://localhost:11434`, `http://ollama:11434` in-compose), `IDEA_VAULT_OLLAMA_MODEL`. **No
  code path may hardcode `localhost:11434` or a localhost bind.** This updates D25 (boot) and the
  Ollama client construction (D11).
- **GPU is isolated to Ollama.** The Rust service is byte-for-byte identical in CPU and GPU mode;
  switching is `up -d` with/without the extra `-f`. The shared `ollama-models` volume means no re-pull.
- **Ownership discipline.** The image runs non-root with `APP_UID`/`APP_GID` build args; the bind
  mount and named volume must line up with the host uid, or writes `EACCES`. `.env.example` documents
  this.
- **First-run degraded window.** `depends_on: service_healthy` gates only the Ollama daemon, not a
  pulled model; the stack starts clean and the UI shows the degraded state ([D20](../05-ai-integration.md))
  until `ollama-pull` completes. This is deliberate — it avoids blocking boot on a multi-GB download.
- **Truth stays on the host.** `vault/` never enters an image layer (`.dockerignore`) and lives as
  plain files the owner can back up and git independently — preserving [ADR-0002](./0002-markdown-source-of-truth-sqlite-index.md).
- **Host publish IP is env-driven.** The `idea-vault` service's host-side port publish is
  `${IDEA_VAULT_HOST_BIND_IP:-127.0.0.1}:${IDEA_VAULT_HOST_PORT:-3000}:3000`, so setting
  `IDEA_VAULT_HOST_BIND_IP=0.0.0.0` (or a specific LAN interface IP) opts the web UI into LAN
  exposure — the app has no auth of its own, so this is an explicit opt-in, documented in
  `.env.example` with that caveat. `ollama`'s publish (`127.0.0.1:11434:11434`) is deliberately
  **not** tied to this var and stays loopback-only always, since Ollama has no auth.

## Alternatives considered

- **Run app + Ollama directly on the host (no containers)** — simplest, but the owner explicitly wants
  everything containerized for a clean, reproducible local host. Rejected per the request.
- **One container running both the app and Ollama** — fewer moving parts, but couples GPU/no-GPU to
  the app image, bloats it, and breaks the "GPU touches only Ollama" isolation. Rejected.
- **CPU-cap the model like `cosmic-mmo`** (`cpus`/`mem_limit`, `-ngl 0`) instead of GPU — valid for a
  shared machine and documented as an option, but it forecloses the owner's explicit "with GPU" mode.
  Rejected as the *default*; kept as a noted alternative.
- **`vault/` in a named volume** — cleaner uid story, but buries the user's irreplaceable content in
  `/var/lib/docker/volumes` where a `docker volume rm` could destroy it. Rejected: truth must stay a
  user-owned host directory.
- **Static musl build** (as `mcp-server`/`zomboid-seasons` do) — smaller, fully static runtime, but
  more build friction with bundled SQLite; we chose glibc `debian-slim` + bundled SQLite for
  arch-agnostic simplicity. Revisit if image size matters.
