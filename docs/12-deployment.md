# 12 — Deployment (Containers)

> How idea-vault is hosted **locally, entirely in containers**, with or without a GPU. Home of
> **D26** (deployment topology), **D27** (multi-stage image build), **D28** (CPU vs GPU composition).
> Decision: [ADR-0008](./adr/0008-containerized-local-deployment.md). Patterns adapted from sibling
> repos: `mcp-server` (single-Rust-service multi-stage build), `cosmic-mmo` (compose topology,
> loopback publishing, profile-gated one-shot, json-file logging), `zomboid-seasons` (SQLite on a
> named volume, container-created `/data`).

## Topology in one paragraph

Two long-lived containers on one Compose network: **`idea-vault`** (the Rust axum binary) and
**`ollama`** (the local model server). The app reaches Ollama by **service DNS** (`http://ollama:11434`),
not `localhost`. The owner's **`vault/` is a host bind mount** (source of truth they own and back up);
the **SQLite index and Ollama models are named volumes** (rebuildable / re-pullable). A GPU changes
**only** the `ollama` service. Everything is published to **loopback** — a local tool, not a network
service.

## D26 — Deployment topology

```mermaid
flowchart TB
    subgraph host["Host machine"]
        BROWSER["Browser → http://localhost:3000"]
        VAULTDIR[("./vault  (bind mount — user owns, git/back up)")]
        CLI["host `ollama` CLI (optional)"]

        subgraph net["Compose network: idea-vault"]
            APP["idea-vault container\naxum :3000, non-root uid 1000\nreads IDEA_VAULT_* env"]
            OLLAMA["ollama container\n:11434"]
            PULL["ollama-pull (profile: tools)\none-shot model bootstrap"]
        end

        IDXVOL[("idea-index  (named volume → /data/index.db)")]
        MODELVOL[("ollama-models  (named volume → /root/.ollama)")]
    end

    BROWSER -->|"127.0.0.1:3000"| APP
    CLI -.->|"127.0.0.1:11434 (manage only)"| OLLAMA
    APP -->|"http://ollama:11434 (service DNS)"| OLLAMA
    APP <--> VAULTDIR
    APP <--> IDXVOL
    OLLAMA <--> MODELVOL
    PULL -->|"pull model"| OLLAMA
    APP -. "depends_on: ollama healthy" .-> OLLAMA
```

Why these choices (see [03-data-model](./03-data-model.md) truth/derived split):

| Data | Mount | Why |
|------|-------|-----|
| `vault/` (markdown, **truth**) | **host bind mount** `./vault:/vault` | user-owned, irreplaceable, git-versioned; must survive `docker volume rm` and app removal |
| `index.db` (**derived**) | named volume `idea-index:/data` | rebuildable via reindex ([ADR-0002](./adr/0002-markdown-source-of-truth-sqlite-index.md)); app-managed, keep out of the user's tree; WAL sidecars live here too |
| Ollama models | named volume `ollama-models:/root/.ollama` | multi-GB, re-pullable; pull once, persist across restarts |

## Configuration contract (env-driven)

Containerization requires the app to stop assuming `localhost`. `config.rs`
([02-module-reference](./02-module-reference.md)) reads these, each with a bare-`cargo run` default:

| Env var | Default (bare run) | In compose | Purpose |
|---------|--------------------|------------|---------|
| `IDEA_VAULT_BIND` | `127.0.0.1:3000` | `0.0.0.0:3000` | axum bind. **Must be `0.0.0.0` in a container** or the host port publish can't connect. |
| `IDEA_VAULT_VAULT_DIR` | `./vault` | `/vault` | vault root ([03-data-model](./03-data-model.md)). |
| `IDEA_VAULT_INDEX_PATH` | `./index.db` | `/data/index.db` | SQLite index path. |
| `IDEA_VAULT_OLLAMA_URL` | `http://localhost:11434` | `http://ollama:11434` | Ollama base URL ([05-ai-integration](./05-ai-integration.md)). **No code path hardcodes `localhost:11434`.** |
| `IDEA_VAULT_OLLAMA_MODEL` | `qwen3.5:4b` | `${IDEA_VAULT_OLLAMA_MODEL}` | default model, shared with the `ollama-pull` one-shot. |
| `IDEA_VAULT_AI_CONCURRENCY` | `2` | not set — falls back to `2` | process-wide bound K on concurrent Ollama calls — chat, skills, and swarm all share one semaphore ([ADR-0006](./adr/0006-bounded-concurrency-swarm.md)). |
| `IDEA_VAULT_OLLAMA_TIMEOUT_SECS` | `120` | not set — falls back to `120` | hard inactivity timeout for Ollama calls — the initial response and every token gap must arrive within this window or the call aborts ([05-ai-integration](./05-ai-integration.md), D20 degrade-not-hang). |
| `IDEA_VAULT_LLM_BACKEND` | `ollama` | `ollama` | which LLM backend answers chat/skills/swarm: `ollama` or `claude-code` ([ADR-0009](./adr/0009-pluggable-llm-backend-claude-code.md)). |
| `IDEA_VAULT_CLAUDE_BIN` | `claude` | — | path to the `claude` CLI (claude-code backend only). |
| `IDEA_VAULT_CLAUDE_MODEL` | *(CLI default)* | — | optional `--model` for the claude-code backend. |
| `IDEA_VAULT_CLAUDE_CWD` | *(the vault dir)* | — | the foil's working dir. Defaults to the vault, **never the app source**, so a full-agentic foil cannot rewrite idea-vault. |
| `IDEA_VAULT_CLAUDE_ADD_DIRS` | *(none)* | — | colon-separated dirs the foil may read (Obsidian vault, Claude Code artifacts) → `--add-dir`. |
| `IDEA_VAULT_CLAUDE_ALLOWED_TOOLS` | *(all)* | — | comma-separated allow-list (only applied when permissions are **not** skipped). |
| `IDEA_VAULT_CLAUDE_SKIP_PERMISSIONS` | `true` | — | `--dangerously-skip-permissions` for unattended runs (the full-agentic default); set `false` to lock down. |
| `IDEA_VAULT_CLAUDE_TIMEOUT_SECS` | `300` | — | hard inactivity timeout for claude-code turns (agentic turns run longer than a hot local model). |

> This is the one behavioral change containers impose on the app design. It updates the boot
> ([D25](./01-architecture.md)) "bind localhost" step and the Ollama client construction
> ([D11](./05-ai-integration.md)).

## D27 — Multi-stage image build

Adapted from `mcp-server`, plus `cargo-chef` dependency caching (which the reference Dockerfiles
lacked). Bundled SQLite (no system `libsqlite3`) and `rustls` (no OpenSSL) keep the runtime minimal.

```mermaid
flowchart LR
    subgraph build["Build stages (rust:1.91-slim)"]
        CHEF["chef\ninstall cargo-chef"] --> PLAN["planner\nchef prepare → recipe.json\n(hashes Cargo manifests)"]
        PLAN --> COOK["builder\nchef cook --release\n(deps cached until Cargo.* change)"]
        COOK --> COMPILE["cargo build --release\n--bin idea-vault"]
    end
    subgraph run["Runtime (debian:bookworm-slim)"]
        USERSTAGE["non-root app user (uid 1000)\nmkdir+chown /data /vault BEFORE USER"]
        BIN["COPY /usr/local/bin/idea-vault\n+ ca-certificates, curl"]
        HC["HEALTHCHECK curl /admin/health\nENTRYPOINT idea-vault"]
    end
    COMPILE -->|"copy the one binary"| BIN
    USERSTAGE --> BIN --> HC
```

Key runtime details:

- **Non-root**, uid/gid via `APP_UID`/`APP_GID` build args so the same uid owns the bind-mounted
  `vault/` and the named index volume.
- `/data` and `/vault` are `mkdir`+`chown`ed **before** `USER` so a freshly-created named volume
  inherits the app uid (the cosmic/zomboid volume-ownership gotcha — Docker copies mountpoint
  ownership onto empty volumes only).
- `curl` + `ca-certificates` are installed **for the healthcheck** (which hits `/admin/health`, the
  route that itself probes Ollama — [D20](./05-ai-integration.md)).

## D28 — CPU vs GPU (compose composition)

GPU acceleration matters only to Ollama; the app is byte-for-byte identical in both modes. The
difference is a single override file merged on top of the base compose.

```mermaid
flowchart TB
    BASE["docker-compose.yml\n(app + ollama + ollama-pull, CPU)"]
    GPU["docker-compose.gpu.yml\nollama: deploy.resources.reservations.devices\n[driver: cdi, device_ids: [nvidia.com/gpu=all], capabilities: [gpu]]\n+ NVIDIA_VISIBLE_DEVICES / DRIVER_CAPABILITIES"]

    BASE -->|"docker compose up -d"| CPU(["CPU mode — portable, no host GPU tooling"])
    BASE --> MERGE
    GPU --> MERGE
    MERGE["compose merges override onto ollama only"] -->|"-f docker-compose.yml -f docker-compose.gpu.yml up -d"| GPUM(["GPU mode — Ollama offloads layers to nvidia"])
```

Switching modes is just re-running `up -d` with or without the second `-f`. The `ollama-models`
volume is shared, so **no re-pull and no app rebuild** when moving between CPU and GPU.

### With GPU — host prerequisites

Modern Docker (25+) exposes NVIDIA GPUs through **CDI** (Container Device Interface), and the
override requests the CDI device `nvidia.com/gpu=all` — not the legacy `driver: nvidia` runtime.

1. NVIDIA driver installed (`nvidia-smi` works).
2. NVIDIA Container Toolkit installed and exposing GPUs over CDI:
   - **NixOS**: `hardware.nvidia-container-toolkit.enable = true;` — regenerates the CDI spec
     under `/run/cdi` on rebuild and registers **no** docker runtime hook.
   - **Debian/RHEL**: install `nvidia-container-toolkit`, then generate the spec:
     ```bash
     sudo nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml
     ```
3. Verify the daemon discovered the device and it reaches a container:
   ```bash
   docker info | grep -A4 'CDI spec'                 # lists nvidia.com/gpu=all
   docker run --rm --device nvidia.com/gpu=all --entrypoint nvidia-smi ollama/ollama:latest -L
   ```
   Note: `docker run --gpus all …` may fail on CDI-only hosts (e.g. `AMD CDI spec not found`);
   request the device by name instead, as the override does.

Then: `docker compose -f docker-compose.yml -f docker-compose.gpu.yml up -d`, and confirm with
`docker compose logs ollama` (look for an `inference compute … library=CUDA` line naming the GPU).

> **Legacy runtime hosts**: if your host uses the nvidia docker *runtime* (older setups configured
> via `sudo nvidia-ctk runtime configure --runtime=docker && sudo systemctl restart docker`) rather
> than CDI, swap the reservation for `driver: nvidia`, `count: all`.

> `cosmic-mmo` runs its LLM sidecar deliberately **CPU-only** (`llama.cpp` with `-ngl 0`, bounded by
> `cpus`/`mem_limit`), so the nvidia block here is written fresh against the Compose spec, not copied.
> A CPU-cap approach (`cpus`, `mem_limit`) is a valid alternative to protect a co-located machine.

## Operating the stack

```bash
docker compose build                                             # build the app image
docker compose up -d                                             # CPU mode (default)
docker compose -f docker-compose.yml -f docker-compose.gpu.yml up -d   # GPU mode
docker compose --profile tools run --rm ollama-pull              # first-run: pull the model
# open http://localhost:3000
docker compose down                                              # stop (volumes persist)
```

First-run note: a fresh `ollama-models` volume has no model, so AI is in the **degraded** state
([D20](./05-ai-integration.md)) until `ollama-pull` finishes (multi-GB, minutes). `depends_on:
service_healthy` gates only the **daemon**, not the model — the stack starts clean and the UI shows
the degraded banner until the model exists. This is intentional and matches the graceful-degradation
requirement.

## Pitfalls (carry into scaffolding & ops)

- **`localhost` in a container is the container.** Leaving `http://localhost:11434` makes every AI
  call hit the app itself. Read `IDEA_VAULT_OLLAMA_URL`. *(Most likely wiring mistake.)*
- **App must bind `0.0.0.0`** inside the container or the loopback publish can't reach it.
- **uid mismatch** on `./vault`: if `id -u` ≠ 1000, set `IDEA_VAULT_UID`/`GID` in `.env` **and**
  rebuild (so the build args match) — else `EACCES` on vault and index writes.
- **SQLite WAL**: `index.db-wal`/`-shm` live in the same volume; back up/reset all three together;
  never point two containers at one SQLite file. Losing the volume is recoverable via reindex.
- **GPU toolkit missing / wrong request mechanism** → `could not select device driver "nvidia"`
  means the host has no legacy nvidia runtime (expected on CDI hosts like NixOS) — the override uses
  `driver: cdi` + `device_ids: [nvidia.com/gpu=all]` for exactly this reason. Verify the device with
  `docker run --rm --device nvidia.com/gpu=all --entrypoint nvidia-smi ollama/ollama:latest -L`.
  Needs Compose v2 (the legacy `docker-compose` v1 ignores the reservation block).
- **arch**: nvidia passthrough is Linux/amd64 (and Jetson) only; on Apple Silicon the container is
  CPU-only. Build on the arch you deploy (or use `buildx`).
- **Bundled SQLite** compiles a C file in the builder — fine on `rust:slim` (ships `cc`); if a future
  base drops the toolchain, add `build-essential`.
- **Static assets**: templates compile into the binary (Askama), but htmx/CSS served from disk must
  be embedded (`rust-embed`) or `COPY`ed from the builder, or the UI ships without JS/CSS.

## Files

| File | Purpose |
|------|---------|
| [`Dockerfile`](../Dockerfile) | multi-stage build (D27) |
| [`.dockerignore`](../.dockerignore) | trims context; never bakes `vault/`/`*.db`/secrets |
| [`docker-compose.yml`](../docker-compose.yml) | base stack (app + ollama + ollama-pull), CPU |
| [`docker-compose.gpu.yml`](../docker-compose.gpu.yml) | nvidia override for `ollama` (D28) |
| [`.env.example`](../.env.example) | uid/gid, model, log level |

> These build once the crate is scaffolded ([02-module-reference](./02-module-reference.md)); today
> they are the deployment contract. Scaffolding is out of scope for the docs phase.

## Related

- [ADR-0008](./adr/0008-containerized-local-deployment.md) — the containerization decision + alternatives.
- [ADR-0003](./adr/0003-ollama-local-only-ai.md) — why Ollama; the URL is now env-driven.
- [05-ai-integration](./05-ai-integration.md) — D20 degradation the first-run relies on.
