# Intent — idea-vault runs and is driven from a Claude Code cloud session (ADR-0024, ADR-0039)

A Claude Code on the web session could not use the app it was cloned into. Its network policy
blocked `index.crates.io`, so `cargo build` failed before compiling anything. Nothing booted
the server or turned on its inbound MCP endpoint. And a session's agent had no handle on the app
except the HTMX forms, so it was left re-creating the app's prompts by hand. This change adds three
scripts and no app code: a build that falls back to the git index on GitHub when the sparse index
is unreachable, a launcher that starts the app on the claude-code backend with the MCP server on,
and a stdlib MCP client that waits out long-running tools by repeating the identical call
(ADR-0028, ADR-0033). No route, config key or vault format changes, and `.claude/` stays
unversioned (ADR-0041). docs/12 and CLAUDE.md say how a session uses them.

## Acceptance criteria

- Where egress blocks `index.crates.io` but allows GitHub and `static.crates.io` (as a Claude Code
  cloud environment's network policy can), `bash scripts/session-build.sh` leaves `target/release/idea-vault` built.
  Where the sparse index is reachable, it uses it unchanged.
- `bash scripts/session-up.sh` boots the app on `127.0.0.1:3000` on the claude-code backend with
  `/api/mcp` gated by a generated token kept only under the ignored `target/session/`. Run again,
  it reuses the live instance. `--status` reports it and `--down` stops it.
- `python3 scripts/session-mcp.py call <long-running tool>` returns the job's rendered result, not
  the "still running" note, with exactly one job and one exchange in `conversation.md` per call.
  An identical repeat replays that result.
- No path under `.claude/` or `vault/` is versioned.
- `bash scripts/gate.sh` is green.

## Expectation changes

None: this change touches no fixture, snapshot, floor or invariant rule.
