# Intent — reference sources layer

A living per-gated-change file (td-bot convention): rewritten before each gated change to state,
in the owner's words, what the change must do. `scripts/gate.sh` step 1 requires it to exist and
be non-empty; the rest of the gate proves the tree still honors the ADRs.

## Acceptance criteria

- I can register named reference sources — a name mapped to an absolute host path — via the
  web UI.
- Registering sources generates a compose override that bind-mounts each source read-only at
  `/mnt/sources/<name>`; **I** apply it with `docker compose up -d` — the app never runs
  docker (ADR-0020). In bare `cargo run` mode (`IDEA_VAULT_SOURCES_DIR` unset) the host paths
  are read directly, no override needed.
- I attach sources to an idea via `sources: [name]` in the idea.md frontmatter — the vault
  stays the self-describing source of truth.
- Both backends can read an idea's attached sources: claude-code gets the directories via
  `--add-dir`; Ollama gets deterministic `source_list` / `source_grep` / `source_read` tool
  leaves. The model picks WHICH source and what query — never HOW: code resolves every path
  (DRT).
- The whole change ships through this gate: `bash scripts/gate.sh` green.
