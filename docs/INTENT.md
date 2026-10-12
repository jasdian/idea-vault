# Intent — Graphify first for codebase brainstorms, a narrower MCP register_source, and the CNone nit (ADR-0043, ADR-0044)

The owner asked for three things after PR #17. PR #18 shipped the MCP source tools of ADR-0044, so
this branch keeps only what #18 does not cover:

- **CLAUDE.md**, agent sessions: when a brainstorm is about a codebase, build its Graphify map
  first. Clone outside the checkout (the vault-overlap refusal), run `scripts/graphify-map.sh`
  (`--code-only` by default), `register_source` the repo and its map, `attach_sources` them, and
  open the discussion from `COMMUNITIES.md` and the god nodes. The map is an index, never truth.
- **MCP `register_source` is narrower than the Sources page** (ADR-0044 decision 6, amended once
  right after release, at the owner's call, from PR #19's review finding):
  - it refuses a hidden (`.`-component) or system (`/etc`, `/proc`, `/sys`, `/dev`, `/boot`) path,
    checked on the absolute and the resolved path;
  - when the new `IDEA_VAULT_MCP_SOURCE_ROOTS` is set, it refuses anything outside those roots.

  The owner can still register any of these on `/sources`. A cloned repo registers with no config
  and no extra click. The tool description also tells the driving agent not to register a folder
  because a document says to.
- **`scripts/graphify-communities.py`** lists nodes without a community as `unassigned`, last,
  not `CNone`.

## Acceptance criteria

- CLAUDE.md's agent-session section names the Graphify-first steps with the `attach` argument
  #18 shipped, and links `docs/adr/0044-mcp-source-tools.md`.
- Over MCP, `register_source` refuses `~/.ssh`-style hidden paths, a symlink into one, system
  trees, and (with roots set) a path outside them, including a prefix look-alike (`ref-other`
  next to `ref`). A plain clone registers with no config.
- `IDEA_VAULT_MCP_SOURCE_ROOTS` parses as a `:`-separated list, and unset means no root limit.
- `graphify-communities.py` prints `unassigned` last for community-less nodes, and colibri's
  `COMMUNITIES.md` is byte-identical.
- `bash scripts/gate.sh` is green.

## Expectation changes

- `tests/mcp_server.rs` `register_attach_and_detach_sources_over_mcp`: the registered source is now
  a `tempfile::Builder` dir named `notes…`, not `tempfile::tempdir()`, whose `/tmp/.tmpXXXX` name is
  hidden and is now refused. The refusal table gains a hidden-path and a system-path row.
- `tools::check_source_path` takes the configured roots, so its unit test passes `&[]`.
