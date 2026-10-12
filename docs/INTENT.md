# Intent — reference sources over MCP, and the CNone nit (ADR-0044)

The first real run of the docs/15 Graphify flow attached a codebase and its map to an idea from an
agent session. The inbound MCP server had no source tools, so the session had to `curl` the HTML
form routes. PR #17 named this gap as a follow-up, and the owner asked for it to be fixed, along
with a review nit on `scripts/graphify-communities.py`.

- Three synchronous MCP tools (ADR-0044, amending ADR-0024):
  - `list_sources`;
  - `register_source`, through `SourceRegistry::add`, the Sources page's own checks;
  - `attach_sources`, through `set_sources_core`, which `POST /idea/{slug}/sources` now calls too.

  Editing and removing a source stay owner-only on the Sources page.
- `graphify-communities.py` lists nodes without a community as `unassigned`, last, not `CNone`.
- docs/13, docs/15 and the server instructions name the new tools. docs/15's curl recipe becomes
  MCP calls. CLAUDE.md and docs/README name ADRs 0001–0044.

## Acceptance criteria

- Over `/api/mcp`, `register_source` refuses each of: a bad name, a relative path, a duplicate name,
  and a duplicate path. `list_sources` shows a registered source as `mounted`.
- `attach_sources` refuses an unknown name and leaves the frontmatter unchanged. It collapses
  duplicates in order, writes `sources:` to `idea.md`, `get_idea` reflects it, and `[]` detaches
  all.
- The web attach route behaves as before: its existing tests pass unchanged through the shared
  core.
- `graphify-communities.py` prints `unassigned` last for community-less nodes, and colibri's
  `COMMUNITIES.md` is byte-identical.
- `bash scripts/gate.sh` is green.

## Expectation changes

- `tools::tests::catalog_is_stable_and_marks_long_running_tools_as_task_optional`: the catalog size
  assertion goes from 17 to 20, and the three new tools are asserted synchronous (ADR-0044).
- `tests/mcp_server.rs` `tools_list_includes_the_catalog`: the expected names gain the three tools.
- The unknown-source error on attach now reads "register it first (the sources page, or MCP
  register_source)" in place of "register it on the sources page first".
