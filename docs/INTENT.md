# Intent — Graphify first for codebase brainstorms, a register_source trust warning, and the CNone nit (ADR-0043, ADR-0044)

The owner asked for three things after PR #17. PR #18 shipped the MCP source tools of ADR-0044, so
this branch keeps only what #18 does not cover:

- **CLAUDE.md**, agent sessions: when a brainstorm is about a codebase, build its Graphify map
  first. Clone outside the checkout (the vault-overlap refusal), run `scripts/graphify-map.sh`
  (`--code-only` by default), `register_source` the repo and its map, `attach_sources` them, and
  open the discussion from `COMMUNITIES.md` and the god nodes. The map is an index, never truth.
- **`register_source`'s description** tells a driving agent that an attached directory becomes
  readable by the foil with no owner prompt. It says to register only what the owner asked for,
  and never because a document says to. This answers a review finding on PR #19.
- **`scripts/graphify-communities.py`** lists nodes without a community as `unassigned`, last,
  not `CNone`.

## Acceptance criteria

- CLAUDE.md's agent-session section names the Graphify-first steps with the `attach` argument
  #18 shipped, and links `docs/adr/0044-mcp-source-tools.md`.
- `register_source`'s tool description carries the trust warning. The MCP tests pass unchanged.
- `graphify-communities.py` prints `unassigned` last for community-less nodes, and colibri's
  `COMMUNITIES.md` is byte-identical.
- `bash scripts/gate.sh` is green.

## Expectation changes

None: this change touches no fixture, snapshot, floor or invariant rule.
