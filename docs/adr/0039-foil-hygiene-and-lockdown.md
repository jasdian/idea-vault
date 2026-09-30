# ADR-0039 — Foil hygiene and lockdown: tool allowlist, env pass-list, turn deadline, tool-output fence

- **Status:** Accepted
- **Date:** 2026-09-30
- **Deciders:** owner (decisions recorded 2026-09-30)
- **Amends:** [ADR-0009](./0009-pluggable-llm-backend-claude-code.md) (the full-agentic foil), [ADR-0013](./0013-containerized-claude-code.md) (what the child inherits), [ADR-0017](./0017-web-access-tools.md) (web tools on the claude path)

## Context

The claude-code backend ([ADR-0009](./0009-pluggable-llm-backend-claude-code.md)) spawned the local
`claude` CLI as a **full-agentic foil**: `--dangerously-skip-permissions`, a deny-list for the web
tools, the server's whole environment, and only a per-line inactivity timeout. Measured with claude
CLI **2.1.285** on a scratch vault (cwd `idea-a`, a canary file in the sibling `idea-b`):

- Under `--dangerously-skip-permissions --disallowedTools WebSearch,WebFetch` the session exposed
  about 27 built-ins, including Bash, Write, Edit, NotebookEdit, Task, Workflow, CronCreate and
  RemoteTrigger.
- Without `--strict-mcp-config` the foil inherits the owner's **user-level MCP servers**: on a host
  run that includes idea-vault's own MCP (`chat`, `store_idea`, `run_swarm`, …) and claude.ai
  connectors. The code passed `--strict-mcp-config` only when idea-vault had registered servers.
- `--tools "Read,Grep,Glob"` is a real allowlist: the `init` event's `tools` list is exactly those
  three (plus MCP unless strict), and a call to any other tool returns "No such tool available".
- **`--tools` plus `--dangerously-skip-permissions` is not isolation**: `Read ../idea-b/idea.md`
  returned the canary.
- With `--tools` and no skip-permissions, headless `-p` works; a Read or Grep outside cwd is denied
  with a clear `is_error` result ("requested permissions … haven't granted"), not a hang.
- **`--restricted` plus `--tools`** confines file tools to cwd and the `--add-dir` roots with an
  explicit error, refuses bypassPermissions, and ignores user and project settings, so a user hook or
  permission allow cannot widen it. It is the strongest of the options measured.
- `--strict-mcp-config` with an empty `{"mcpServers":{}}` strips every inherited MCP tool.

Two further gaps: the child inherited the server's environment, including `IDEA_VAULT_MCP_TOKEN`
(the inbound MCP bearer token, [ADR-0024](./0024-mcp-server-inbound.md)); and a foil busy with tool
calls never trips a per-line timeout, so a turn had no wall-clock bound. On the Ollama side, tool
results (fetched web text, reference-source files, MCP answers) reached the model raw, as
`role: "tool"` messages, where injected instructions read like any other text.

## Decision

We will run the claude-code foil **locked down**, scrub its environment, bound its turn, and fence
every Ollama tool result as untrusted data.

1. **Tool allowlist.** The child gets `--restricted --tools Read,Grep,Glob`, plus `WebSearch,WebFetch`
   only while the live `web_access` setting is on ([ADR-0017](./0017-web-access-tools.md)); with web
   access off the two web tools are also passed as `--disallowedTools`, so the off state stays honest
   if a CLI version ever widened `--tools`. `--allowedTools` pre-approves the foil tools and only those
   owner or router entries that name one of them or a registered `mcp__<server>` (anything else could
   not run anyway). The foil reads, and never writes or executes.
2. **Always `--strict-mcp-config`.** The MCP config file is written for every turn: the registered
   servers, or `{"mcpServers":{}}` when there are none (`EMPTY_MCP_CONFIG`). No user-level or cwd
   `.mcp.json` server reaches the foil. The file is owner-only (0600) and removed when the stream
   state drops.
3. **cwd is the idea's own folder** (`--restricted` confines file tools to it), with `--add-dir` for
   the idea's attached reference sources and the owner's reference directories
   ([ADR-0021](./0021-reference-sources.md)). Store-time extraction and compaction, which run
   source-free, still run inside their idea's folder. The idea's run journal
   (`.runs/`, [ADR-0037](./0037-run-journal-diagnostics-only-call-record.md)) sits inside that cwd
   and holds every earlier call's verbatim answer, so `--disallowedTools` always carries
   `Read(./.runs/**)` (`RUN_JOURNAL_DENY`), which the CLI also applies to Grep and Glob: the foil
   never reads rejected or fetched material back as if it were the discussion.
4. **No `--dangerously-skip-permissions`, anywhere.** The `ClaudeSettings` field is gone; a leftover
   `IDEA_VAULT_CLAUDE_SKIP_PERMISSIONS` is logged as ignored, neither honoured nor silently dropped.
   The `no-skip-permissions` invariant rule ([ADR-0041](./0041-no-mistakes-gate.md)) greps `src/`.
5. **The `init` event is checked.** Before any output is accepted, the stream-json `system/init` event's
   `tools` must all be in the allowlist (or a registered `mcp__<server>__…` tool) and its `mcp_servers`
   must be exactly the registered ones; a missing `tools` list, or output before `init`, is refused
   (`claude foil lockdown refused: …`). Allowlisted tools the session lacks are only logged. A
   deterministic test drives `tests/fixtures/fake-claude.sh`, which lists its `--tools` in `init`
   (modes `leakytools` and `noinit` exercise the refusals).
6. **Env pass-list.** The child, and the `--version` probe, is spawned with `env_clear()` plus a fixed
   list: `HOME`, `PATH`, `USER`, `SHELL`, `LANG`, `LC_ALL`, `LC_CTYPE`, `TERM`, `TMPDIR`,
   `XDG_CONFIG_HOME`, `XDG_CACHE_HOME`, `XDG_DATA_HOME`, `CLAUDE_CODE_OAUTH_TOKEN`,
   `CLAUDE_CONFIG_DIR`, `ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL`, `HTTP_PROXY`, `HTTPS_PROXY`,
   `NO_PROXY` (upper and lower case) and `NODE_EXTRA_CA_CERTS`. The owner extends it with
   `IDEA_VAULT_CLAUDE_ENV_PASS` (comma-separated). **`IDEA_VAULT_*` keys are always removed, even if
   listed**, because the server's own secrets live under that prefix.
7. **Turn deadline.** A whole turn ends at `IDEA_VAULT_CLAUDE_TURN_TIMEOUT_SECS` (default 1800) after
   spawn. Each line waits for the smaller of the inactivity timeout and what is left of the deadline;
   past it the turn returns `AiError::Backend("claude turn exceeded {N}s wall clock")`. `kill_on_drop`
   reaps the process and the temp MCP file is removed, so a timed-out turn persists nothing (D11).
8. **Tool-output fence (Ollama tool loop).** Every tool result is wrapped by
   `ai::untrusted::fence_untrusted("tool <name>", result)` between `<<<untrusted-output` and
   `>>>end-untrusted-output`. Any data line that could be read as a marker (after optional leading
   backslashes, spaces or tabs) gets one more leading `\`, an injective escape, so a fenced block that
   contains a fenced block nests without ambiguity; data is split on CR as well as LF; a label is forced
   onto one line. One sentence (`FENCE_NOTE`) is prefixed to the tool-loop turn, telling the model that
   fenced text is data. Owner vault context is not fenced (it is the owner's own words), web truncation
   stays head-only, and fence markers never reach `conversation.md`. The `tool-fence` invariant rule
   requires every `"role": "tool"` message in `src/ai` to go through `fence_untrusted`.

**Hygiene, not containment (env scrub).** The env pass-list keeps secrets out of the foil's
environment. It does not by itself stop a foil that can execute code from reading
`/proc/$PPID/environ`. What makes the boundary real is the lockdown above: with no Bash, Write or
Edit and file tools confined by `--restricted` to the idea folder and its source roots, the foil has
no way to reach that file. Each half is defence in depth for the other, and this ADR claims the
combination, not either alone.

## Consequences

- **The foil is a reader.** It can Read, Grep and Glob the one idea it is interrogating and the
  attached sources, search and fetch the web while that is on, and call only the MCP servers idea-vault
  registered. It cannot edit the owner's files, run a shell, or see a sibling idea. The safety
  consequence in ADR-0009 ("a full-agentic foil can run …") is retired.
- **A tool the owner wants is now a decision, not a default.** `IDEA_VAULT_CLAUDE_ALLOWED_TOOLS` can only
  pre-approve tools inside the allowlist; widening the allowlist is a code change and a new ADR.
- **Containers keep working.** The [ADR-0013](./0013-containerized-claude-code.md) override sets
  `HOME=/claude` and `CLAUDE_CODE_OAUTH_TOKEN`; both are on the pass-list, so the token still reaches
  the CLI. `IDEA_VAULT_CLAUDE_ENV_PASS` is the escape hatch for a proxy or CA variable a site needs.
  Owners with a non-standard proxy or certificate setup should confirm their variable is passed.
- **A too-tight deadline drops work.** A timed-out turn persists nothing, so the 1800 s default is
  generous; the owner can raise it.
- **A CLI upgrade can change these semantics.** The flag behaviour above was measured on 2.1.285; the
  `init` check turns a widened session into a refused turn rather than a silent one. Re-measure after
  CLI upgrades.
- **The stretch goal, `check_args`** (validating required and unknown keys of `web_*` and `source_*`
  tool calls against their own definitions), is not part of this change.

## Alternatives considered

- **Keep `--dangerously-skip-permissions` and rely on the env scrub.** Rejected: it is not isolation
  (the canary read) and it leaves Bash and Write reachable.
- **A permissions allowlist without `--tools`.** Rejected: unapproved tools still exist in the
  session; `--tools` removes them.
- **Pin a canonical `claude` binary path at boot.** Rejected: it checks the configured path against
  itself, and pinning breaks the CLI's symlink self-update.
- **Reorder the prompt context-first for cache reuse.** Rejected: the CLI would not reuse the cache
  and it rewrites every persona and skill.
- **Fence owner vault context too.** Rejected: it is the owner's own words, not attacker-controlled.
- **Silently ignore `IDEA_VAULT_CLAUDE_SKIP_PERMISSIONS`.** Rejected: a warning costs nothing and
  names the removed knob.

---

> ADRs are immutable once **Accepted**. To change a decision, write a new ADR that supersedes this
> one and update the Status line above.
