# Intent — run Graphify the way docs/15 says, and say what it really does (ADR-0043)

The owner asked to use Graphify to map a large codebase (JustVugg/colibri) before brainstorming it
in idea-vault, and to fix whatever idea-vault was missing to run it. Running Graphify 0.9.87 for
real showed that docs/15's commands do not produce what the guide says:

- `graphify extract` writes no `GRAPH_REPORT.md`; `cluster-only` does.
- `--out DIR` writes `DIR/graphify-out/`.
- Community naming in `cluster-only` is a second LLM pass that auto-picks a backend from API keys,
  even after a `--code-only` extract.
- `--code-only` still yields INFERRED edges (13% on colibri).

This change adds the missing pieces as scripts and docs, with no app code and no ADR-0043 option:

- `scripts/graphify-map.sh` is the checked-in wrapper that docs/15 described as a `~/.bashrc`
  function. It refuses cloud keys, refuses a map inside the source or the vault, allows only
  `--code-only` (default), `ollama` or `claude-cli`, clusters with `--no-label`, and writes
  `BUILT.txt`.
- `scripts/graphify-communities.py` names communities by their dominant files (stdlib, no model
  call), since `--no-label` leaves them "Community N".
- docs/15 records the re-check, points Layers 2 and 3 at the wrapper, and replaces "Measured here:
  not yet" with the colibri numbers. It also shows how an agent session attaches a repo and its
  map as sources, which the MCP server has no tool for.

## Acceptance criteria

- `scripts/graphify-map.sh` refuses, with exit 1, each of: a cloud API key in the environment, a
  map dir inside the source, a map dir inside the vault, a source inside the vault, and a backend
  other than `ollama` or `claude-cli`.
- On a real repo it writes `graphify-out/graph.json`, `graphify-out/GRAPH_REPORT.md`,
  `COMMUNITIES.md` and `BUILT.txt` in the map dir and leaves the source's git status clean.
- docs/15 states only what was run for each 0.9.87 claim, and no step needs an ADR-0043 option.
- `bash scripts/gate.sh` is green.

## Expectation changes

None: this change touches no fixture, snapshot, floor or invariant rule.
