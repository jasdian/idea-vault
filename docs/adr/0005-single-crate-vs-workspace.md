# ADR-0005 — Single binary crate with strict module boundaries (not a workspace yet)

- **Status:** Accepted
- **Date:** 2026-07-07
- **Deciders:** owner

## Context

We must choose the Cargo project shape. The codebase has clear internal seams (domain, vault storage,
SQLite index, AI/Ollama, memory, harness concepts, web). A cargo **workspace** would enforce those
seams at the crate level; a **single crate** with disciplined modules is lighter. This is a
greenfield, single-owner, single-binary tool.

## Decision

We will build a **single binary crate** named `idea-vault` with strict internal modules and an
enforced **one-way dependency direction** (nothing depends on `web`). The layout is designed so it
can be **promoted to a workspace later** with mechanical effort if the need arises. See
[02-module-reference](../02-module-reference.md) for the module graph (D4) and rules.

## Consequences

- Lower overhead: one `Cargo.toml`, one compile graph, one version — fast to iterate on early.
- Module boundaries are a **convention enforced by discipline and review** (and the D4 dependency
  rules), not by the compiler. We accept this risk in exchange for speed.
- The chosen module names map 1:1 to candidate future crates (`core` = domain+vault+index, `ai` =
  ai+concepts+memory, `web` = binary), so extraction is later mechanical.
- If the module graph is violated (e.g. `domain` importing `web`), it is a design smell to be caught
  in review, not a build error.

## Alternatives considered

- **Cargo workspace from day one** (`idea-vault-core`, `-ai`, `-web`) — compiler-enforced boundaries
  and cleaner separation, but versioning/compile-graph overhead before there is any payoff for a solo
  greenfield tool. Rejected as premature; revisit if the crate grows or gains a second binary.
- **One flat module namespace (no submodules)** — least structure, but the seams that make the
  concepts legible would blur. Rejected: the harness-concept boundaries are worth keeping explicit.
