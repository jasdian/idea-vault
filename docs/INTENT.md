# Intent — MCP source tools: register and attach reference sources without the forms (ADR-0044)

An agent session drives idea-vault over MCP, but grounding an idea in a checkout or its Graphify
map meant leaving MCP and posting the `/sources` HTMX forms with curl. Worse, R43 replaces the
whole attached set, so a client that posted one name detached the rest. The owner took the
follow-up left on PR #17 as an API change. This change adds three synchronous MCP tools.
`list_sources` reads the registry with fixed status words. `register_source` reuses R37's
`SourceRegistry::add` and refuses a path in or containing the vault. `attach_sources` merges attach and detach
lists through R43's own write path, now shared as `ideas::update_idea_sources`. Editing and
removing a source stay on `/sources`.

## Acceptance criteria

- The MCP catalog has 20 tools. `list_sources`, `register_source` and `attach_sources` are task
  support `Forbidden` and make no model call.
- `register_source` refuses a bad name, a duplicate name or path, a relative, `..` or root path,
  and a path that is the vault, under it, or contains it (relative vault dirs and
  not-yet-created paths included). Each refusal is `invalid_params` and writes nothing.
- `attach_sources` merges in order. It refuses an unregistered new name, an empty call and a busy
  idea, writing nothing. An identical repeat writes nothing.
- R43 and `attach_sources` share one write path. An unchanged R43 submission no longer bumps
  `updated`.
- `tests/mcp_server.rs` covers register → list → attach → `get_idea` → detach and every refusal.
  Unit tests cover the merge, the argument parsing, the status words and the vault check.
- ADR-0044 is Accepted. docs/13, docs/12, docs/15 (its curl calls replaced), docs/README, CLAUDE.md
  and the server instructions name the tools. CLAUDE.md and docs/README name ADRs 0001–0044.
- `bash scripts/gate.sh` is green.

## Expectation changes

None: this change touches no fixture, snapshot, floor or invariant rule.
