# idea-vault - fully vibe-coded project

This tool exists to allow moving of all "in mind" ideas, into usable database.

## Description

A single-user, **localhost, offline** web tool where you bring a raw idea, **run it into the ground**
in conversation with a local AI, and store it in a markdown vault for later resumption. It mirrors an
LLM agent harness — memory, skills, agents, workflows, subagent swarming — applied to interrogating
one idea.

- **Backend/UI:** Rust — axum + Askama + HTMX + SSE, single binary, no JS build step.
- **Storage:** markdown files are the **source of truth**; SQLite is a **rebuildable index**.
- **AI:** a **local Ollama** server — nothing leaves your machine.
- **Hosting:** everything runs **in containers**, locally, **with or without a GPU**.

> **Status: scaffolded.** The design is complete in [`docs/`](docs/README.md) and the crate
> skeleton is built against it: `cargo run` boots the server (idea list + health + embedded
> assets), `domain/` is fully implemented and tested, and the remaining modules are typed stubs
> that answer **501 Not Implemented**. Start with [`CLAUDE.md`](CLAUDE.md) and
> [`docs/README.md`](docs/README.md).

## Run it locally, in containers

Requires Docker + Compose v2. Everything (the app **and** Ollama) runs in containers; published to
`127.0.0.1` only — it's a personal tool, not a network service.

```bash
cp .env.example .env          # set IDEA_VAULT_UID/GID to your `id -u` / `id -g`
mkdir -p vault                # the source-of-truth directory you own & back up

# --- CPU mode (default, portable, no host GPU tooling) ---
docker compose up -d --build

# --- GPU mode (nvidia) instead — see prerequisites below ---
docker compose -f docker-compose.yml -f docker-compose.gpu.yml up -d --build

# First run: pull a model into the shared ollama volume (multi-GB, minutes)
docker compose --profile tools run --rm ollama-pull

# open the app
xdg-open http://localhost:3000     # or just browse to it

docker compose down            # stop; named volumes (index, models) persist
```

Until the model is pulled, the app runs fine but shows a **degraded AI** banner — by design.

## Choosing a model

The model is config, not code: set `IDEA_VAULT_OLLAMA_MODEL` in `.env` and re-run the
`ollama-pull` one-shot. The workload here is rigorous multi-turn critique, strict-format memory
extraction, and a K-bounded parallel swarm — reasoning quality and per-token speed matter more
than raw parameter count, which is why small-active-parameter MoE models fit unusually well.
Recommendations as of July 2026:

| Hardware | Model | Why |
|---|---|---|
| 8 GB, CPU-only (**default**) | `qwen3.5:4b` | Mar 2026; thinking mode, 256K context, ~3 GB — the modern successor to the old `llama3.2` 3B slot |
| 16 GB (sweet spot) | `gpt-oss:20b` | MoE, 21B total / **3.6B active**: ~o3-mini-level reasoning, native structured output + tool calling, 14 GB; snappy streaming and cheap parallel swarm calls |
| 24 GB+ GPU | `qwen3.6:27b` or `gemma4:26b` | Near-frontier critique quality on consumer hardware |
| Tiny / experiments | `qwen3.5:2b`, `phi-4-mini` | Fast smoke-level chat; noticeably weaker as a critical foil |

Caveats: thinking-mode models emit 3–5× more tokens (fine for streamed chat, costly inside the
swarm's shared concurrency budget), and slow CPU inference can brush against the app's hard
token-timeout. `llama3.2` still works — it's just 2024-era; nothing in the app hardcodes any
model name.

### GPU mode prerequisites (nvidia)

GPU acceleration benefits **only** Ollama; the Rust app is identical either way. On the host:

```bash
sudo nvidia-ctk runtime configure --runtime=docker
sudo systemctl restart docker
docker run --rm --gpus all ubuntu nvidia-smi     # verify before using the gpu compose file
```

Linux/amd64 (and Jetson) only. See [`docs/12-deployment.md`](docs/12-deployment.md) for the full
topology, build pipeline, volume strategy, and pitfalls.

## What's in this repo

| Path | What |
|------|------|
| [`CLAUDE.md`](CLAUDE.md) | North-star spec + guidance for working in the repo |
| [`docs/`](docs/README.md) | The full design foundation: architecture, data model, state machine, AI integration, the five harness concepts, web UI, deployment, a 25+ diagram Mermaid catalog, and ADRs |
| [`Dockerfile`](Dockerfile) | Multi-stage Rust build (cargo-chef, bundled SQLite, rustls, non-root) |
| [`docker-compose.yml`](docker-compose.yml) | Base stack: `idea-vault` + `ollama` + `ollama-pull` (CPU) |
| [`docker-compose.gpu.yml`](docker-compose.gpu.yml) | nvidia GPU override for the `ollama` service |
| [`.env.example`](.env.example) | uid/gid, model, log level |

## Data & privacy

- `./vault/` — your ideas as plain markdown (**bind-mounted, you own it**). Back it up / `git` it.
- `idea-index` volume — the rebuildable SQLite index; delete it any time and it regenerates from the
  vault via reindex.
- `ollama-models` volume — downloaded models; re-pullable.
- Nothing is published beyond `127.0.0.1`, and no data leaves the machine.

## Developing without Docker

Once the crate is scaffolded, a bare `cargo run` works too — the app reads the same settings from
env with localhost-friendly defaults (`IDEA_VAULT_BIND=127.0.0.1:3000`,
`IDEA_VAULT_OLLAMA_URL=http://localhost:11434`, …). See the configuration contract in
[`docs/12-deployment.md`](docs/12-deployment.md#configuration-contract-env-driven).

## License

TBD.
