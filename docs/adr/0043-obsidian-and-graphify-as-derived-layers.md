# ADR-0043 — Obsidian and Graphify as derived layers, not truth

- **Status:** Proposed
- **Date:** 2026-10-11
- **Deciders:** owner (pending)

## Context

The owner wants to use **Obsidian** (knowledge-graph browsing and editing over markdown) and
**Graphify** (Graphify-Labs, a knowledge-graph builder with tree-sitter for code and an LLM pass
for docs) with idea-vault, to reduce model context and improve output quality. The findings and a
no-code setup are in [docs/15](../15-obsidian-and-graphify.md). The forces:

- **Markdown is truth, and SQLite is a rebuildable index.** Nothing canonical lives anywhere else (CLAUDE.md).
- **No networked AI provider beyond the local `claude` CLI.** Graphify's headless extract
  auto-selects a cloud backend from whichever API key is in the environment (Gemini first,
  then Kimi/Moonshot, …). Only `--backend ollama`, `--backend claude-cli` and `--code-only`
  stay inside the rule.
- **The foil runs locked down** ([ADR-0039](./0039-foil-hygiene-and-lockdown.md)): no MCP and
  `--strict-mcp-config`.
- **Query-driven fact retrieval was tested and killed** ([ADR-0031](./0031-query-driven-fact-retrieval-killed.md)).
  The memory eval is the bar any selective-context scheme must clear.
- **Memory facts need a verbatim quote** from the discussion ([ADR-0023](./0023-verification-layer.md)).
  Graphify's INFERRED and AMBIGUOUS edges are model judgements with no such grounding.
- **Graphify's own token numbers** show about 1x on small corpora and large gains only on large
  ones queried repeatedly. A single idea is a small corpus.
- **Obsidian's two-writer behaviour:**
  - Its Properties UI re-serialises frontmatter, and a *Date & time* property holds a value
    with no timezone, which idea-vault's parser rejects (tested: the idea drops out of the index).
  - It rewrites links on rename.
  - A bare `[[slug]]` is ambiguous vault-wide when two ideas share a fact slug.
- **docs/00-vision:** backlinks connect ideas and their memories, "not to be Obsidian".

## Decision

We will treat Obsidian as an optional, view-only-by-default second viewer over the unchanged vault, and
Graphify as an optional, out-of-process derived artifact, used only on the owner's reference
sources (ADR-0021) and on this codebase. Neither becomes truth, memory, or a prompt input outside
the existing source tools. The no-code setup in docs/15 is the supported integration today. The
options below are **proposed for the owner to accept or reject one by one**. None is implemented by
this ADR.

- **A — tolerate a timezone-less `created`/`updated`**, reading it as UTC, and rewrite it in RFC
  3339 on the next app write. This turns the one Obsidian edit that breaks an idea into a no-op.
  The cost is a parser change in `domain::frontmatter`, plus a golden.
- **B — make fact links resolve in Obsidian.** This is a *safety* fix for the viewer layer, not a
  nicety. Today `[[idea#fact]]` and idea-to-idea `[[slug]]` don't resolve in Obsidian, and
  clicking one creates a stray note. Write each fact's slug into `aliases`, or
  write cross-fact links as `[[<idea>/memory/<fact>|<fact>]]` and teach `domain::links` to accept
  that form. This changes the vault format (a new `format_version`), so it needs its own ADR if
  chosen. A cheaper partial step is a `validate` rule that flags stray root-level `.md` notes.
- **C — deterministic canvas export.** `idea-vault export canvas [--idea <slug>]` writes a JSON
  Canvas file from the index's `edges` graph (link/tag/lexical), with no model call. It gives
  Obsidian the app's own graph rather than an LLM-inferred one. It is derived and deletable, and
  written outside idea folders.
- **D — graph-neighbourhood memory selection.** When an idea's memory overflows the budget, it
  would prefer facts linked (`fact_links`) to facts the recent turns mention, over strict
  newest-first. It ships only if it beats the current loader on the ADR-0031 eval. Otherwise it
  stays killed, like its query-driven predecessor.
- **E — Graphify's backend is Ollama unless the owner decides otherwise, per corpus.**
  `--backend claude-cli` is within CLAUDE.md's AI rule (the local `claude` CLI) but sends the
  whole corpus to Anthropic. The docs/15 wrapper hard-codes Ollama and refuses to run with a cloud
  key present. The owner records any `claude-cli` exception here, so it is never a silent fallback
  for a slow local build.
- **F — measure before the map counts as a saving.** The Layer 2 map is kept only after a
  journal-measured baseline (`prompt_tokens`, `source_*` result sizes from `.runs/`) and a recall
  check on known-answer questions, as in docs/15. If the foil already reads less than a map costs,
  the map stays a browsing aid.
- **G — Graphify as an import lens, never a writer.** If Graphify output is ever read by the app,
  it is as a reference source the foil may open, never as edges in `index.db` and never as memory.
  INFERRED and AMBIGUOUS edges stay leads.

The draft of this ADR and docs/15 was stress-tested in idea-vault itself (the `interrogate`
workflow: 7 confirmed, 12 uncertain, 1 refuted). The view-only stance, measure-first, the sibling
map folder and the hard-coded backend come from that audit. It also claimed that project hooks
could reach the foil. Code refutes that: the foil runs `--restricted`, which ignores user,
project and local settings.

## Consequences

- **Easier:**
  - The owner gets Obsidian's graph, Bases and local graph over their ideas today, at no cost.
  - Large personal note collections reach the foil through a one-page Graphify map instead of
    blind greps.
  - Coding agents get an offline map of the crate.
- **Harder:**
  - The owner has to keep two Obsidian rules: `created`/`updated` typed as Text, and no renames
    in Obsidian. A wrong date type silently drops the idea out of search until it is fixed,
    unless option A is taken.
  - The Graphify map goes stale unless the owner re-runs it.
- **Invariants kept:**
  - The vault contract, reindex and `validate` are unchanged.
  - The foil's tool set and MCP config are unchanged.
  - No new networked AI call.
  - Graphify never writes into an idea folder.

## Alternatives considered

- **Make Obsidian the UI** (drop or thin the web app). Rejected: it breaks the single-binary,
  localhost-web product and ties the loop to a closed-source desktop app. Its CLI also needs the
  app running, so it can't back a headless server.
- **Run Graphify over the idea-vault vault and feed its graph into prompts.** Rejected: a single
  idea is too small for gains by Graphify's own numbers, and its inferred edges would enter the
  model's context with no evidence gate. The index already has a deterministic graph.
- **Give the foil the Obsidian Local REST API MCP server.** Rejected: it breaks ADR-0039
  (`--strict-mcp-config`, a minimal tool surface). Reference sources already give read-only,
  path-contained access.
- **Embedding search over the vault (Smart Connections or similar) as context selection.**
  Deferred into option D's bar: it has to beat the ADR-0031 eval, which query-driven retrieval
  failed.

---

> ADRs are immutable once **Accepted**. To change a decision, write a new ADR that supersedes this
> one and update the Status line above.
