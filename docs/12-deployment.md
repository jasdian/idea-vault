# 12 — Deployment (Containers)

> How idea-vault is hosted **locally, entirely in containers**, with or without a GPU. Home of
> **D26** (deployment topology), **D27** (multi-stage image build), **D28** (CPU vs GPU composition),
> **D29** (claude-code container topology), **D31** (reference-source topology).
> Decisions: [ADR-0008](./adr/0008-containerized-local-deployment.md),
> [ADR-0013](./adr/0013-containerized-claude-code.md) (claude-code in containers),
> [ADR-0017](./adr/0017-web-access-tools.md) (`IDEA_VAULT_WEB_ACCESS`, `IDEA_VAULT_SEARCH_URL` —
> outbound internet needed when web access is on),
> [ADR-0018](./adr/0018-mcp-servers.md) (`IDEA_VAULT_MCP_CONFIG`, the owner's MCP server registry),
> [ADR-0021](./adr/0021-reference-sources.md) (named reference sources + the generated compose
> override the owner applies),
> [ADR-0024](./adr/0024-mcp-server-inbound.md) (`IDEA_VAULT_MCP_TOKEN`, the **inbound** MCP server
> at `/api/mcp` — the mirror image of ADR-0018's outbound registry).
> Patterns adapted
> from sibling repos: `mcp-server` (single-Rust-service multi-stage build), `cosmic-mmo` (compose
> topology, loopback publishing, profile-gated one-shot, json-file logging), `zomboid-seasons`
> (SQLite on a named volume, container-created `/data`).

## Topology in one paragraph

Two long-lived containers on one Compose network: **`idea-vault`** (the Rust axum binary) and
**`ollama`** (the local model server). The app reaches Ollama by **service DNS** (`http://ollama:11434`),
not `localhost`. The owner's **`vault/` is a host bind mount** (source of truth they own and back up);
the **SQLite index and Ollama models are named volumes** (rebuildable / re-pullable). A GPU changes
**only** the `ollama` service. The web UI's host-side publish is **loopback by default** but can opt
into LAN exposure via `IDEA_VAULT_HOST_BIND_IP` (the app has no built-in auth — only do this on a
trusted network); **Ollama's publish stays loopback-only always**, since Ollama has no auth of its
own and isn't meant to be reachable off-host.

## D26 — Deployment topology

```mermaid
flowchart TB
    subgraph host["Host machine"]
        BROWSER["Browser → http://localhost:3000"]
        VAULTDIR[("./vault  (bind mount — user owns, git/back up)")]
        SRCDIRS[("reference source dirs\n(host, ADR-0021)")]
        SRCOVR["vault/.docker-compose.sources.yml\n(generated override — see D31)"]
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
    APP -->|"regenerates on every /sources edit"| SRCOVR
    SRCDIRS -.->|"ro binds /mnt/sources/&lt;name&gt;\n(owner applies the override: up -d)"| APP
    APP <--> IDXVOL
    OLLAMA <--> MODELVOL
    PULL -->|"pull model"| OLLAMA
    APP -. "depends_on: ollama healthy" .-> OLLAMA
```

Why these choices (see [03-data-model](./03-data-model.md) truth/derived split):

| Data | Mount | Why |
|------|-------|-----|
| `vault/` (markdown, **truth**) | **host bind mount** `./vault` → `/vault`, long syntax + `create_host_path: false` | user-owned, irreplaceable, git-versioned; must survive `docker volume rm` and app removal. The source **must pre-exist**: the short syntax lets the daemon auto-create it `root:root`, which silently binds a ghost when the vault's filesystem mounts later than Docker ([ADR-0019](./adr/0019-vault-mount-verified-not-created.md)) |
| `.mcp-servers.json` (app config, **not** vault truth) | rides the same **host bind mount** as `vault/` by default | `IDEA_VAULT_MCP_CONFIG` defaults to `<vault>/.mcp-servers.json` purely because the vault bind mount is the one host-persistent path available; it is invisible to reindex ([03-data-model](./03-data-model.md), [ADR-0018](./adr/0018-mcp-servers.md)) |
| `.sources.json` + `.docker-compose.sources.yml` (app config, **not** vault truth) | ride the same **host bind mount** as `vault/` | the reference-source registry and its **generated** compose override ([ADR-0021](./adr/0021-reference-sources.md)) — same rationale as `.mcp-servers.json`, invisible to reindex; losing them costs a re-add of source paths, never ideas. Both are gitignored via `vault/.gitignore`, **which the app maintains** (host paths are machine-identifying and must not leak into a published ideas repo) |
| reference source dirs (owner's own material, read-only) | **host bind mounts** `<host_path>` → `/mnt/sources/<name>`, long syntax + `create_host_path: false`, declared by the generated override | never-invent, same as the vault bind ([ADR-0019](./adr/0019-vault-mount-verified-not-created.md)); `read_only: true` because they are reference, not workspace — the foil may grep/read, never write ([ADR-0021](./adr/0021-reference-sources.md), D31) |
| `index.db` (**derived**) | named volume `idea-index:/data` | rebuildable via reindex ([ADR-0002](./adr/0002-markdown-source-of-truth-sqlite-index.md)); app-managed, keep out of the user's tree; WAL sidecars live here too |
| Ollama models | named volume `ollama-models:/root/.ollama` | multi-GB, re-pullable; pull once, persist across restarts |
| `claude` CLI binary (claude-code override only) | **host bind mount** `${IDEA_VAULT_CLAUDE_HOST_BIN:-~/.local/bin/claude}:/opt/claude/claude:ro` | host-owned, host-managed version; ro so the container never rewrites it; dereferenced at container **start** — restart to pick up a host update ([ADR-0013](./adr/0013-containerized-claude-code.md)) |
| claude CLI state (claude-code override only) | named volume `claude-state:/claude` (via `HOME=/claude`) | rebuildable-adjacent but the owner wants it to **persist** — `.claude/` (projects, history, settings) + `.claude.json`, so project history survives container recreation and re-auth isn't needed every `up` |

## Configuration contract (env-driven)

Containerization requires the app to stop assuming `localhost`. `config.rs`
([02-module-reference](./02-module-reference.md)) reads these, each with a bare-`cargo run` default:

| Env var | Default (bare run) | In compose | Purpose |
|---------|--------------------|------------|---------|
| `IDEA_VAULT_BIND` | `127.0.0.1:3000` | `0.0.0.0:3000` | axum bind. **Must be `0.0.0.0` in a container** or the host port publish can't connect. |
| `IDEA_VAULT_HOST_BIND_IP` | `127.0.0.1` | `0.0.0.0` for LAN opt-in | compose-interpolation var, not read by `config.rs`: the **host-side** IP the `idea-vault` service's port is published on (`${IDEA_VAULT_HOST_BIND_IP:-127.0.0.1}:${IDEA_VAULT_HOST_PORT:-3000}:3000`). Distinct from `IDEA_VAULT_BIND` (the in-container axum bind, unchanged at `0.0.0.0:3000`) — this only controls who on the host/LAN can reach that published port. No built-in auth, so only set to `0.0.0.0`/a LAN IP on a trusted network. Ollama's own publish stays loopback-only always, independent of this var. |
| `IDEA_VAULT_VAULT_DIR` | `./vault` | `/vault` | vault root ([03-data-model](./03-data-model.md)). |
| `IDEA_VAULT_INDEX_PATH` | `./index.db` | `/data/index.db` | SQLite index path. |
| `IDEA_VAULT_OLLAMA_URL` | `http://localhost:11434` | `http://ollama:11434` | Ollama base URL ([05-ai-integration](./05-ai-integration.md)). **No code path hardcodes `localhost:11434`.** |
| `IDEA_VAULT_OLLAMA_MODEL` | `qwen3.5:4b` | `${IDEA_VAULT_OLLAMA_MODEL}` | default model, shared with the `ollama-pull` one-shot. |
| `IDEA_VAULT_AI_CONCURRENCY` | `2` | not set — falls back to `2` | process-wide bound K on concurrent Ollama calls — chat, skills, and swarm all share one semaphore ([ADR-0006](./adr/0006-bounded-concurrency-swarm.md)). |
| `IDEA_VAULT_OLLAMA_TIMEOUT_SECS` | `120` | not set — falls back to `120` | hard inactivity timeout for Ollama calls — the initial response and every token gap must arrive within this window or the call aborts ([05-ai-integration](./05-ai-integration.md), D20 degrade-not-hang). |
| `IDEA_VAULT_AUTO_COMPACT` | `true` | not set — falls back to `true` | initial auto-compact toggle: fold the conversation head into a rolling `compacted.md` summary before a chat turn once the context gets large ([ADR-0012](./adr/0012-auto-compact.md)); off only if set to `false`/`0`. Retunable live via `/settings`. |
| `IDEA_VAULT_COMPACT_THRESHOLD` | `0.80` | not set — falls back to `0.80` | initial effective-size fraction of the AI budget at which auto-compact fires, clamped to `0.5..=0.95` (unparsable/out-of-range falls back to the default); retunable live via `/settings`. |
| `IDEA_VAULT_LLM_BACKEND` | `ollama` | `ollama` (base file); the **claude override** changes the default to `${IDEA_VAULT_LLM_BACKEND:-claude-code}` — see [claude-code in containers](#claude-code-in-containers) | which LLM backend answers chat/skills/swarm **at boot** (the initial value): `ollama` or `claude-code` ([ADR-0009](./adr/0009-pluggable-llm-backend-claude-code.md)). Retunable live via the Settings page (`GET`/`POST /settings`) with no restart — see [ADR-0011](./adr/0011-live-switchable-llm-backend.md). |
| `IDEA_VAULT_OLLAMA_TEMPERATURE` | `0.7` | not set — falls back to `0.7` | initial Ollama sampling temperature, clamped to `0.0..=2.0` (unparsable/out-of-range falls back to the default); retunable live via `/settings`. |
| `IDEA_VAULT_OLLAMA_CTX_TOKENS` | `0` (auto) | `${IDEA_VAULT_OLLAMA_CTX_TOKENS:-0}` from `.env` | initial Ollama context-window override in **tokens**; `0` = derive from the model via `/api/show`, capped at 32,768 (VRAM guard), falling back to 8,192 until the cache warms; nonzero clamped `1024..=2_000_000`. Retunable live via `/settings` ([ADR-0014](./adr/0014-dynamic-context-budget.md)). |
| `IDEA_VAULT_CLAUDE_CTX_TOKENS` | `0` (auto) | `${IDEA_VAULT_CLAUDE_CTX_TOKENS:-0}` from `.env` (claude override file only) | initial claude-code context-window override in **tokens**; `0` = derive from the model name (`1m` marker → 1,000,000, else 200,000 — no default cap); nonzero clamped `1024..=2_000_000`. Retunable live via `/settings` ([ADR-0014](./adr/0014-dynamic-context-budget.md)). |
| `IDEA_VAULT_WEB_ACCESS` | `true` | not set — falls back to `true` | initial web-access toggle ([ADR-0017](./adr/0017-web-access-tools.md)): lets either backend crawl the internet — Ollama via the `ai::web` tool-calling loop, claude-code via its own WebSearch/WebFetch tools; off (`false`/`0`) disallows them on both. Retunable live via `/settings`. **The container needs outbound internet reachability when this is on** — a previously-unneeded posture, since the app otherwise only reaches the `ollama` service on the compose network. |
| `IDEA_VAULT_SEARCH_URL` | `https://html.duckduckgo.com/html/` | not set — falls back to the default | Ollama-path search endpoint used by `ai::web::web_search` ([ADR-0017](./adr/0017-web-access-tools.md)); override to point at a self-hosted SearXNG instance (or any HTML search endpoint accepting `?q=`) instead of DuckDuckGo. Read per call, no restart needed. Not used on the claude-code path (the CLI's own WebSearch is unaffected by it). |
| `IDEA_VAULT_SKILLS_DIR` | `<vault_dir>/.skills` | `${IDEA_VAULT_SKILLS_DIR}` (unset ⇒ same vault-relative default, so it rides the `vault/` bind mount) | where owner-authored/overriding skill markdown files live ([ADR-0022](./adr/0022-skills-as-markdown-and-the-skill-book.md)) — **app config, not vault truth**, invisible to the idea walker (no `idea.md`); loaded by `concepts::skills::LiveSkills`, reloadable live from `/skills` with no restart. |
| `IDEA_VAULT_AUDIT_FINDINGS` | `true` | not set — falls back to `true` | initial toggle for the factored audit layer ([ADR-0023](./adr/0023-verification-layer.md)): whether the swarm's and workflows' converge step runs an Auditor call over every finding (CONFIRMED/UNCERTAIN/REFUTED) before synthesis; off (`false`/`0`) skips the audit call. Knowledge extraction is never audited — its findings are per-lens harvests kept as artifacts. Retunable live via `/settings`. |
| `IDEA_VAULT_MCP_CONFIG` | `<vault>/.mcp-servers.json` | `${IDEA_VAULT_MCP_CONFIG}` (unset ⇒ same vault-relative default, so it rides the `vault/` bind mount) | path to the owner's **outbound** MCP server registry file (`crate::mcp::McpRegistry`, [ADR-0018](./adr/0018-mcp-servers.md)) — **app config, not vault truth**, but defaulted inside the vault dir purely because that's the one host-persistent bind mount; managed live from `/mcp` with no restart. Only override this if you want the registry to live outside the vault bind mount (e.g. on its own volume). |
| `IDEA_VAULT_MCP_TOKEN` | *(unset — feature off)* | `${IDEA_VAULT_MCP_TOKEN:-}` (unset ⇒ empty, which stays disabled) — set it in `.env` to turn the feature on | the Bearer token gating the **inbound** MCP server at `POST /api/mcp` ([ADR-0024](./adr/0024-mcp-server-inbound.md)) — the mirror image of `IDEA_VAULT_MCP_CONFIG` above (that one is the registry of servers idea-vault *calls*; this one gates the server idea-vault *is*). Unset or blank: `/api/mcp` is not mounted at all, not mounted-but-open. No live retuning — changing it needs a restart, since the route is only mounted once at boot. |
| `IDEA_VAULT_SOURCES_CONFIG` | `<vault>/.sources.json` | unset ⇒ same vault-relative default, so it rides the `vault/` bind mount | path to the reference-source registry JSON (`sources::SourceRegistry`, [ADR-0021](./adr/0021-reference-sources.md)) — **app config, not vault truth**, defaulted inside the vault dir for the same reason as `IDEA_VAULT_MCP_CONFIG`; managed live from `/sources` with no restart. Only override to move it (and the generated override beside it) elsewhere. |
| `IDEA_VAULT_SOURCES_DIR` | *(unset — bare mode)* | **fixed to `/mnt/sources` by the base file** — do not set it yourself | the in-container mount root the generated sources override binds each source under (`/mnt/sources/<name>`). Unset (or blank) is **bare `cargo run` mode**: registry host paths are read directly off the filesystem and the override, though still generated, is inert ([ADR-0021](./adr/0021-reference-sources.md)). Being set is also the app's container-mode signal for source status probing. |
| `IDEA_VAULT_SOURCES_APPLIED` | *(never owner-set)* | **baked into the container env by the generated override at `up` time** — like the compose-interpolation vars, never something you write yourself | the sources fingerprint (`name=path` pairs, sorted, `;`-joined) the running container was started with; the app compares it against the live registry to render the **NeedsReup** pill. Set-but-empty means "override layered, zero sources"; absent means "override never layered" — the two are deliberately distinguishable ([ADR-0021](./adr/0021-reference-sources.md)). |
| `IDEA_VAULT_CLAUDE_BIN` | `claude` | **fixed to `/opt/claude/claude`** by the claude override — do not set it yourself in a containerized run | path to the `claude` CLI. Native-only otherwise. |
| `IDEA_VAULT_CLAUDE_HOST_BIN` | *(native: unused)* | `~/.local/bin/claude` (default) — host path the claude override bind-mounts ro into the container | claude-code-in-containers only ([ADR-0013](./adr/0013-containerized-claude-code.md)); compose-interpolation var, not read by `config.rs`. |
| `CLAUDE_CODE_OAUTH_TOKEN` | *(native: unused — the CLI's own login state applies)* | **required** by the claude override (`:?` guard — `up`/`config` fails fast when unset) | long-lived token from a one-time host `claude setup-token`; inherited by the spawned CLI from the app's env ([ADR-0013](./adr/0013-containerized-claude-code.md)). |
| `IDEA_VAULT_CLAUDE_MODEL` | *(CLI default)* | `${IDEA_VAULT_CLAUDE_MODEL:-}` (blank = CLI default) | optional `--model` for the claude-code backend; retunable live via `/settings`. |
| `IDEA_VAULT_CLAUDE_CWD` | *(the vault dir)* | — | the foil's working dir. Defaults to the vault, **never the app source**, so a full-agentic foil cannot rewrite idea-vault. |
| `IDEA_VAULT_CLAUDE_ADD_DIRS` | *(none)* | — | colon-separated dirs the foil may read (Obsidian vault, Claude Code artifacts) → `--add-dir`. |
| `IDEA_VAULT_CLAUDE_ALLOWED_TOOLS` | *(all)* | — | comma-separated allow-list (only applied when permissions are **not** skipped). |
| `IDEA_VAULT_CLAUDE_SKIP_PERMISSIONS` | `true` | — | `--dangerously-skip-permissions` for unattended runs (the full-agentic default); set `false` to lock down. |
| `IDEA_VAULT_CLAUDE_TIMEOUT_SECS` | `300` | — | hard inactivity timeout for claude-code turns (agentic turns run longer than a hot local model). |
| `IDEA_VAULT_CLAUDE_EFFORT` | `high` | `${IDEA_VAULT_CLAUDE_EFFORT:-high}` | initial claude-code reasoning effort (`low`/`medium`/`high`), injected as a system-prompt hint since the CLI has no per-call effort flag; retunable live via `/settings`. |

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

> Pinning the file list with `COMPOSE_FILE` in `.env` instead of `-f` flags? Keep the generated
> sources override **last** — `docker-compose.yml:docker-compose.gpu.yml:docker-compose.claude.yml:vault/.docker-compose.sources.yml`
> (base : gpu : claude : sources) — later files win merges, and nothing may shadow the sources
> override's `IDEA_VAULT_SOURCES_APPLIED` env ([ADR-0021](./adr/0021-reference-sources.md), D31).

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

## claude-code in containers

An override, `docker-compose.claude.yml`, brings the agentic claude-code backend
([ADR-0009](./adr/0009-pluggable-llm-backend-claude-code.md)) into the containerized stack by
bind-mounting the **host's own** `claude` CLI rather than baking a copy into the image
([ADR-0013](./adr/0013-containerized-claude-code.md)).

### D29 — claude-code container topology

```mermaid
flowchart TB
    subgraph host["Host machine"]
        HOSTCLI["~/.local/bin/claude\n(symlink → versioned install, host-managed)"]
        ENVFILE[(".env\nCLAUDE_CODE_OAUTH_TOKEN\n(from one-time `claude setup-token`)")]

        subgraph net["Compose network: idea-vault"]
            APP["idea-vault container\nHOME=/claude, DISABLE_AUTOUPDATER=1\nIDEA_VAULT_CLAUDE_BIN=/opt/claude/claude"]
            OLLAMA["ollama container\n(depends_on unchanged — still starts)"]
        end

        CLAUDESTATE[("claude-state  (named volume → /claude\n.claude/ projects+history+settings, .claude.json)")]
    end

    HOSTCLI -.->|"bind mount, ro\n(dereferenced at container start)\nspawned as /opt/claude/claude"| APP
    ENVFILE -.->|"CLAUDE_CODE_OAUTH_TOKEN\n(:? fails `up` fast if unset,\ninherited by the spawned CLI)"| APP
    APP <--> CLAUDESTATE
    APP -. "live-switchable, ADR-0011" .-> OLLAMA
```

Run commands:

```bash
# one-time on the host
claude setup-token                                                    # paste the token into .env as CLAUDE_CODE_OAUTH_TOKEN

# claude-code backend, CPU
docker compose -f docker-compose.yml -f docker-compose.claude.yml up -d --build

# composable with the GPU override (disjoint services)
docker compose -f docker-compose.yml -f docker-compose.gpu.yml -f docker-compose.claude.yml up -d

# after a host `claude` CLI update — the bind mount is dereferenced at container start
docker compose -f docker-compose.yml -f docker-compose.claude.yml restart idea-vault
```

Pitfalls specific to this override (beyond the general pitfalls list below):

- **Rebuild before the volume is first created.** The image `chown`s the `/claude` mountpoint
  (D27) so a *freshly created* `claude-state` volume inherits app-uid ownership; a volume created
  from an older image is root-owned and the non-root `user:` can't write CLI state. Recovery:
  `down`, `docker volume rm idea-vault_claude-state`, rebuild, `up`.
- **Probe-green-but-auth-broken is possible.** The health probe is `claude --version`, which needs
  no authentication, so it stays green with a missing/expired token — only the first chat turn
  fails. `src/ai/claude_code.rs::classify_line` surfaces a bad-token error `result` as
  `AiError::Backend("claude error: <text>")` (e.g. "claude error: Invalid API key · Please run
  /login") so the failure is diagnosable in the UI instead of reading as an empty reply.
  `CLAUDE_CODE_OAUTH_TOKEN`'s `:?` guard on `up`/`config` catches the common "forgot to set it"
  case before the container even starts.
- **Restart to pick up a host CLI update.** The bind-mounted `~/.local/bin/claude` symlink is
  dereferenced once, at container start — `docker compose … restart idea-vault` (not a full
  rebuild) is enough after updating the CLI on the host.
- **Ollama still starts.** The override changes only the `idea-vault` service's environment/mounts;
  `ollama` keeps running so the Settings page can live-switch back to the local model with no
  restart ([ADR-0011](./adr/0011-live-switchable-llm-backend.md)).

## Reference sources in containers

The owner registers **named reference sources** — a `name` mapped to an absolute host directory —
on the `/sources` page ([ADR-0021](./adr/0021-reference-sources.md)); an idea opts in via
frontmatter `sources: [name]`, and attached sources reach the model as deterministic
`source_list`/`source_grep`/`source_read` tool leaves (Ollama) or `--add-dir` roots (claude-code).
In a bare `cargo run` the host paths are read directly and nothing below applies
(`IDEA_VAULT_SOURCES_DIR` unset = bare mode). In containers a host path means nothing until a bind
mount exists, so every registry mutation regenerates a compose override —
`vault/.docker-compose.sources.yml` — that **the owner applies**; the app never runs docker
(ADR-0020) and the `restart: "no"` manual bring-up posture is unchanged.

### D31 — Reference-source topology

```mermaid
flowchart TB
    PAGE["/sources page\n(add / edit path / remove — name immutable)"]
    REG[("vault/.sources.json\n(registry — app config riding the vault mount)")]
    OVR["vault/.docker-compose.sources.yml\nGENERATED override: one ro bind per source\n+ IDEA_VAULT_SOURCES_APPLIED = fingerprint"]
    UP(["owner: docker compose up -d\n(the app never runs docker — ADR-0020)"])
    DIRS[("host reference dirs\n(absolute paths, owner's notes/repos)")]
    MOUNTS["/mnt/sources/&lt;name&gt;\nread_only: true, create_host_path: false"]
    RESOLVE["app: resolve + probe\nMounted{entries} / NeedsReup / Missing\n(entries=0 = ghost-bind warning)"]
    OLLAMA["Ollama tool loop:\nsource_list / source_grep / source_read\n(deterministic leaves — DRT)"]
    CLAUDE["claude-code:\n--add-dir per resolved root\n+ system-prompt note"]

    PAGE --> REG
    REG -->|"every mutation regenerates"| OVR
    OVR --> UP
    DIRS --> UP
    UP -->|"binds"| MOUNTS
    MOUNTS --> RESOLVE
    RESOLVE -->|"idea frontmatter sources: [name]\n→ per-turn scoped backend"| OLLAMA
    RESOLVE --> CLAUDE
```

One-time setup: point `COMPOSE_FILE` at the generated override in `.env` (colon-separated, base
first, sources **last** — see the ordering note under D28), then apply each registry change by
re-running the plain up:

```bash
# .env — one-time
COMPOSE_FILE=docker-compose.yml:vault/.docker-compose.sources.yml

# after every add / edit / remove on /sources
docker compose up -d
```

Until the re-`up`, the affected source shows a **NeedsReup** pill: the registry's fingerprint
differs from the `IDEA_VAULT_SOURCES_APPLIED` value the override baked into the container env at
`up` time (or the override was never layered at all). `Mounted {entries: 0}` renders as a warning,
not a green light — an empty-but-listable mount is the ADR-0020 ghost-bind signature. And because
every generated bind carries `create_host_path: false`, a vanished host dir fails the **whole**
`up` (see the pitfalls below) — the **Missing** pill is the pre-warning to fix or remove the
source before re-upping.

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

**Local `.gguf` import (alternative to `ollama-pull`).** To run a local or fine-tuned weight file
instead of a registry pull, the `ollama-import` one-shot (`--profile tools`, same profile as
`ollama-pull`) reads the blob once into the `ollama-models` volume:

```bash
IDEA_VAULT_MODELS_DIR=/abs/path/to/gguf-dir IDEA_VAULT_GGUF=Your-Model-Q4_K_M.gguf \
  IDEA_VAULT_OLLAMA_MODEL=my-local docker compose --profile tools run --rm ollama-import
```

then set `IDEA_VAULT_OLLAMA_MODEL=my-local` in `.env` and `docker compose up -d`. See
`.env.example` for the `IDEA_VAULT_MODELS_DIR`/`IDEA_VAULT_GGUF` variables and a GPU-fit note for
7-8B Q4_K_M models on an 11 GB card.

## Pitfalls (carry into scaffolding & ops)

- **`localhost` in a container is the container.** Leaving `http://localhost:11434` makes every AI
  call hit the app itself. Read `IDEA_VAULT_OLLAMA_URL`. *(Most likely wiring mistake.)*
- **App must bind `0.0.0.0`** inside the container or the loopback publish can't reach it.
- **uid mismatch** on `./vault`: if `id -u` ≠ 1000, set `IDEA_VAULT_UID`/`GID` in `.env` **and**
  rebuild (so the build args match) — else `EACCES` on vault and index writes.
- **The vault bind source must exist before Docker starts — or Docker invents it.** With the short
  `./vault:/vault` syntax the daemon **auto-creates a missing source as `root:root`**. If `vault/`
  lives on a filesystem that mounts *later* than `docker.service` (network/NFS/iSCSI/LVM, or any
  `_netdev` mount), a boot race binds an empty root-owned **ghost** directory, and the real
  filesystem then mounts *over* it — hiding the ghost while the container keeps talking to it.
  Symptom: the UI lists **zero ideas** while every file is intact on disk, the log says
  `reindex complete ideas=0 facts=0 links=0`, writes fail with
  `vault error: io error: Permission denied (os error 13)`, and the healthcheck stays **green**.
  Confirm with `docker exec <c> stat /vault` vs `stat vault` on the host — a **different device or
  inode** is the ghost. Compare against `stat -c %D` on the real path — do **not** hardcode a device
  number in a sweep script; it changes across reboots (`8:34` on 2026-07-15 was `8:2` on 2026-07-16).
  This is why the base file binds `vault/` with long syntax + `create_host_path: false`
  ([ADR-0019](./adr/0019-vault-mount-verified-not-created.md)) — but **that guard alone does not stop
  this**, see the next two bullets. **Recovery:** `docker compose up -d --force-recreate idea-vault`
  once the filesystem is up (`restart` is *not* enough — it reuses the existing mount namespace and
  keeps the ghost). The index rebuilds itself from markdown.
  *(Silent, survives reboots, and looks like data loss when it is not.)*
- **`create_host_path: false` refuses to *create* a ghost — it will happily *bind* an existing one**
  ([ADR-0020](./adr/0020-boot-order-and-ghost-binds.md)). An empty ghost directory answers "does the
  source exist?" with **yes**, so once one exists on the underlay it defeats the guard on **every
  subsequent boot**, permanently, until deleted. A leftover ghost is not harmless debris; it is the
  trap re-arming itself. **Removing them is what makes the guard work.** Reveal the shadowed underlay
  with a *non-recursive* bind — an ordinary `mount --bind` would carry the real filesystem along and
  show you the wrong thing:
  ```bash
  sudo mount --bind /home /mnt/underlay          # non-recursive: does NOT carry submounts
  find /mnt/underlay/john/dump -maxdepth 7       # inspect first — expect empty dirs only
  rmdir /mnt/underlay/john/dump/git-repos/git-moje/idea-vault/vault
  rmdir -p --ignore-fail-on-non-empty /mnt/underlay/john/dump/git-repos/git-moje/idea-vault
  sudo umount /mnt/underlay
  ```
  **`rmdir`, never `rm -rf`.** `rmdir` refuses a non-empty directory, and that refusal *is* the
  safety property: it makes the procedure physically incapable of destroying real data that was
  misdirected onto the underlay by a past ghosted run (this host had 1.8 GB of exactly that). If a
  `rmdir` fails with "Directory not empty", **stop and look** — you found data, not a ghost.
- **Docker does not retry a failed mount, so there is no self-heal** — the container goes `exited`
  with `RestartCount=0` and stays there even after the filesystem appears; a `restart` policy covers
  container *exits*, not *start failures* ([ADR-0020](./adr/0020-boot-order-and-ghost-binds.md),
  measured). **The fix is not to win the race but to skip it: `restart: "no"` on every service**, so
  the daemon never auto-starts a container into the pre-mount window. Bring each stack up by hand
  from its folder once the volume is confirmed mounted (`findmnt <vaultDir>`). Check your exposure
  with `systemctl show docker.service home-john-dump.mount -p ActiveEnterTimestamp`: if the mount
  timestamp is later than docker's, an auto-starting container *would* race on every boot — which is
  exactly why nothing here auto-starts. *(If you ever need boot-time auto-start on a host that must
  come back unattended, ADR-0020's alternatives describe the one-service systemd unit that does it
  safely — it was written and removed here because a dev host has no such requirement.)*
- **Deleting `.idea-vault-root`** from the vault root makes an otherwise-empty vault look
  indistinguishable from a wrong path, so the app stops trusting it (`Suspect` — logged, and health
  reports it). Keep it; if you version your vault with git, **commit it** — a fresh clone of an
  intentionally-empty vault is otherwise flagged. A vault that still has idea folders re-adopts and
  re-writes the marker automatically.
- **`POST /admin/reindex` answering 409** is the empty-vault guard, not a bug: the vault has no
  ideas but the index does — almost always an unmounted vault. Check `GET /admin/health` first; if
  the vault really is empty, `POST /admin/reindex?force=1`.
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
- **claude-code override — rebuild before first volume creation**, **restart to pick up a host CLI
  update**, and **probe-green-but-auth-broken**: see [claude-code in containers](#claude-code-in-containers)
  above for the full detail and recovery steps ([ADR-0013](./adr/0013-containerized-claude-code.md)).
- **`COMPOSE_FILE` points at `vault/.docker-compose.sources.yml` before the app has ever written
  it** — every compose command (`up`, `down`, `logs`, …) fails with file-not-found, a
  chicken-and-egg: the app writes that file, but the pinned `up` can no longer run. Boot the stack
  once **without** the sources entry (or add your first source on a bare run) before pinning: the
  app regenerates the override at every boot, **even with zero sources**, precisely so a
  `COMPOSE_FILE` that lists it keeps working from then on
  ([ADR-0021](./adr/0021-reference-sources.md)).
- **A deleted/renamed source host dir fails the *whole* `up`** — every generated bind carries
  `create_host_path: false` (never-invent, [ADR-0019](./adr/0019-vault-mount-verified-not-created.md)/[ADR-0020](./adr/0020-boot-order-and-ghost-binds.md)
  lineage), so one missing source is a hard start failure for the app container, not a degraded
  source. Deliberate: a silently-invented empty dir is exactly the ghost class ADR-0019 exists to
  prevent, and the Sources page's **Missing** pill pre-warns before you ever re-`up`. Recovery:
  fix the path (or remove the source on `/sources`), then `docker compose up -d` again.

## Files

| File | Purpose |
|------|---------|
| [`Dockerfile`](../Dockerfile) | multi-stage build (D27) |
| [`.dockerignore`](../.dockerignore) | trims context; never bakes `vault/`/`*.db`/secrets |
| [`docker-compose.yml`](../docker-compose.yml) | base stack (app + ollama + ollama-pull), CPU |
| [`docker-compose.gpu.yml`](../docker-compose.gpu.yml) | nvidia override for `ollama` (D28) |
| [`docker-compose.claude.yml`](../docker-compose.claude.yml) | claude-code backend override for `idea-vault` — bind-mounts the host CLI + `claude-state` volume (D29, [ADR-0013](./adr/0013-containerized-claude-code.md)) |
| `vault/.docker-compose.sources.yml` (generated, not in the repo) | reference-source override — ro binds + applied fingerprint, regenerated by the app on every `/sources` edit, applied by the owner (D31, [ADR-0021](./adr/0021-reference-sources.md)) |
| [`.env.example`](../.env.example) | uid/gid, model, log level |

> These build once the crate is scaffolded ([02-module-reference](./02-module-reference.md)); today
> they are the deployment contract. Scaffolding is out of scope for the docs phase.

## Related

- [ADR-0008](./adr/0008-containerized-local-deployment.md) — the containerization decision + alternatives.
- [ADR-0003](./adr/0003-ollama-local-only-ai.md) — why Ollama; the URL is now env-driven.
- [ADR-0013](./adr/0013-containerized-claude-code.md) — claude-code in containers + rejected alternatives.
- [ADR-0014](./adr/0014-dynamic-context-budget.md) — dynamic context budget (`/api/show`, `num_ctx`, per-backend overrides).
- [ADR-0018](./adr/0018-mcp-servers.md) — the MCP server registry, its config-only vs. wire-client module split, and why `.mcp-servers.json` rides the vault bind mount without being vault truth.
- [ADR-0021](./adr/0021-reference-sources.md) — reference sources: the registry, the generated override the owner applies, and the deterministic tool leaves (D31).
- [05-ai-integration](./05-ai-integration.md) — D20 degradation the first-run relies on; the Ollama client contract (`/api/show`, `num_ctx`).
