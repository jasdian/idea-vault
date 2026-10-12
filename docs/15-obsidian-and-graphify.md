# 15 — Obsidian and Graphify alongside idea-vault: findings and setup

> How to use **Obsidian** as a knowledge-graph viewer over the vault (view-only by default), and
> **Graphify** as an optional, derived knowledge graph over large reference material and over
> this codebase. The aims are a smaller model context, a better foil and a faster development
> loop, with no change to the vault contract. Everything in this guide works with today's code.
> The changes it does *not* make are in [ADR-0043](./adr/0043-obsidian-and-graphify-as-derived-layers.md),
> **Proposed** and awaiting an owner decision. Research date: 2026-10-11.

## The rule this guide keeps

The markdown vault is the source of truth, and SQLite is a rebuildable index (CLAUDE.md,
[03-data-model](./03-data-model.md)). Obsidian and Graphify slot in on either side of that line:

| Tool | What it is to idea-vault | Never |
|---|---|---|
| **Obsidian** | A second **viewer** over the same files, view-only by default with one narrow tolerated edit (Layer 1). The graph view and Bases replace nothing the app does. | The app never depends on Obsidian running, never reads `.obsidian/`, and never writes Obsidian-only syntax. |
| **Graphify** | A **derived artifact**, like `index.db`: rebuildable, deletable, never canonical. | Graphify output is never vault truth or memory, and never reaches a prompt except through the owner's own reference sources (Layer 2). It never runs on an auto-selected cloud backend. |

The app does not become Obsidian. [00-vision](./00-vision.md) keeps backlinks "to connect ideas and
their memories, not to be Obsidian". The foil's lockdown ([ADR-0039](./adr/0039-foil-hygiene-and-lockdown.md))
is untouched: no MCP server is added to the foil, and `--strict-mcp-config` stays.

## Findings

### What idea-vault already does to keep context small

These are the baseline any integration has to beat (`src/ai/budget.rs`, `src/memory/`):

- **The context is assembled in a fixed order and budgeted in bytes:** the idea body in full, then
  memory (every `MEMORY.md` line as `[[slug]] — summary`, then fact bodies newest first, with the
  tail cut at the budget), then the compacted summary (whole or nothing), then recent turns from
  the newest back.
- **Rolling compaction** folds the old head of `conversation.md` into `compacted.md`
  ([ADR-0012](./adr/0012-auto-compact.md), [ADR-0016](./adr/0016-forced-compact-folds-fully.md)).
- **Related ideas** come from the index's own graph: `link`/`tag`/`lexical` edges, 2 hops, capped
  at 2 KB ([ADR-0027](./adr/0027-cross-idea-retrieval-and-the-phase-2-verdict.md)).
- **Query-driven fact retrieval was tested and rejected** ([ADR-0031](./adr/0031-query-driven-fact-retrieval-killed.md)).
  Any "pick the relevant facts" scheme, graph-based or not, has to beat that eval before it ships.
- **Reference sources** ([ADR-0021](./adr/0021-reference-sources.md)) already let the foil search
  a read-only folder, such as the owner's own Obsidian vault, *on demand* instead of preloading it:
  `source_list`/`source_grep`/`source_read` on Ollama, and `--add-dir` on claude-code. Dot-paths
  are pruned. `IDEA_VAULT_CLAUDE_ADD_DIRS` is the global form.
- **`idea-vault import <dir>`** turns an Obsidian folder into Draft ideas. It skips `.obsidian` and
  `.trash`, and slugifies `[[Target|alias]]` and `[[Target#heading]]` to `[[target]]`.

### Obsidian (checked against the official help-docs source, Oct 2026)

- **Official CLI.** Shipped in 1.12 (release note 2026-02-10), enabled under Settings → General →
  Command line interface. It drives the *running* desktop app: if the app is closed, the first
  command starts it, so it is not a headless reader. Useful commands are `search`, `read`,
  `backlinks`/`links` (`format=json`), `properties`, `base:query` and `eval`. A separate
  *Obsidian Headless* npm package is for Sync and Publish only.
- **Bases.** A core plugin since 1.9. A `.base` file is YAML with `filters`, `formulas`,
  `properties` and `views` (table, list, cards, kanban, map). It is a cheap way to see every
  idea's `state`, `tags` and `updated` as a table.
- **Properties** are typed vault-wide by name: Text, List, Number, Checkbox, Date, Date & time,
  Tags. Links inside properties must be quoted, nested properties are not supported, and
  `aliases` must be a list.
- **Wikilinks** resolve vault-wide. A bare `[[pricing]]` is **ambiguous** in Obsidian when two
  ideas both have `memory/pricing.md`, even though idea-vault resolves it within the idea.
- **The Local REST API plugin** (v5.4) serves `https://127.0.0.1:27124` with a key, and now
  includes an **MCP endpoint**. That is useful for the owner's own Claude Code sessions, never for
  the foil. Smart Connections writes `.smart-env/` with a local embedder. Copilot can use Ollama.
- **Two-writer gotchas:**
  - Obsidian picks up external changes automatically.
  - On rename it rewrites internal links vault-wide (on by default).
  - Its Properties UI re-serialises the whole frontmatter block, changing quoting and list style.
  - `.obsidian/workspace.json` changes on every file open.
  - Dot-folders (`.runs/`, `.skills/`, `.workflows/`) are hidden from Obsidian, which is the
    right outcome for `.runs/`.

### The foil cannot pick up Graphify or Obsidian config

The claude-code foil runs with `--restricted`. The `claude` CLI's help says this "ignores user,
project and local settings files", so a Graphify PreToolUse hook or skill installed in user or
project scope never loads into the foil. Only admin-managed settings still apply. `--strict-mcp-config`
keeps out every MCP server, the Obsidian one included (`src/ai/claude_code.rs`, [ADR-0039](./adr/0039-foil-hygiene-and-lockdown.md)).

### What idea-vault tolerates (tested with the release binary and `idea-vault validate`)

| Change Obsidian or a plugin may make | Result |
|---|---|
| `.obsidian/`, `.smart-env/`, a root `*.base` or `*.canvas` file, a `graphify-out/` folder in the vault root | ✅ ignored (the walker only enters folders that contain `idea.md`) |
| Double-quoted instead of single-quoted `title` | ✅ |
| Added `aliases:` list, added `summary:` property | ✅ kept as unknown keys, which survive the app's rewrites |
| `created`/`updated` written **without a timezone** (`2026-10-11T19:44:04`), which is what a *Date & time* typed property holds, or as a bare *Date* (`2026-10-11`) | ❌ **YAML error: the idea drops out of the index** until it is fixed |

So `created` and `updated` must stay **Text** properties in Obsidian, and must not be edited
there (setup step 2.3).

### Graphify (checked against its source, v0.9.85, Apache-2.0)

- **What it is:** a Python CLI and Claude Code skill (PyPI `graphifyy`, repo
  `Graphify-Labs/graphify`). It builds `graph.json`, `GRAPH_REPORT.md` (god nodes, Leiden
  communities, surprising links) and `graph.html` from a folder.
- **Code is parsed locally with tree-sitter** (no LLM). **Docs, markdown, PDFs and images go
  through an LLM "semantic pass".**
  - Headless `graphify extract` **auto-selects a backend from whichever API key is set**, in the
    order Gemini → Kimi (Moonshot, servers in China) → Claude API → OpenAI → DeepSeek → … → Ollama.
  - Always pass `--backend ollama` (fully local) or `--backend claude-cli` (the local `claude`
    binary on your subscription). `--backend claude` is the Anthropic **API**, a networked call
    that is out of scope here.
  - `--code-only` skips the semantic pass entirely.
- **Edges carry confidence:** `EXTRACTED` / `INFERRED` / `AMBIGUOUS`. INFERRED and AMBIGUOUS are
  model judgements, not evidence.
- **How it is queried:** `/graphify query|path|explain` in Claude Code, an optional PreToolUse
  hook that nudges Read/Grep toward the graph, and an MCP stdio server (`query_graph`,
  `get_neighbors`, `shortest_path`, …). Queries are logged to `~/.cache/graphify-queries.log`
  unless `GRAPHIFY_QUERY_LOG_DISABLE=1`.
- **Obsidian export:** `graphify export obsidian --dir <folder>` writes one note per node plus
  `graph.canvas`.
- **Token claims, read honestly:**
  - "71.5x fewer tokens per query" is the project's own measurement on one 52-file corpus. The
    same table shows **~1x on 6 files** and 5.4x on 4, and the build-pass cost is excluded.
  - BENCHMARKS.md reports an *accuracy* gain (key-fact coverage 70.8% → 82.0% on ERPNext at
    ~140K tokens per query), not a saving.
  - Expect gains only on **large** corpora queried **many** times.
- **Known failure modes:**
  - Graphs go stale after `git pull` until `graphify update .`.
  - The LLM can invent nodes (now flagged as unverified, not dropped).
  - Recent releases fixed same-basename merges and the graph shrinking after extractor failures.

#### Re-checked by running it (v0.9.87, 2026-10-12)

Installed from PyPI (`uv tool install graphifyy`) and run on a 33 MB C/Python/TypeScript repo
(JustVugg/colibri). Five things differ from a reading of the source, and the setup below follows
the run:

- **`extract` stops at `graph.json`.** It writes no `GRAPH_REPORT.md` in any mode, and prints
  "next: run `graphify cluster-only <dir>`". The report comes from `cluster-only`.
- **`--out DIR` writes `DIR/graphify-out/`**, not `DIR` itself. The report is at
  `DIR/graphify-out/GRAPH_REPORT.md`.
- **Community naming is a second LLM pass with its own backend choice.** `cluster-only` and
  `label` "auto-detect from API keys" unless given `--backend`, so a `--code-only` build can still
  reach a cloud model at this step. `--no-label` keeps it off any model, but leaves every
  community named "Community N" (the report's 299-line hub list on colibri says nothing).
  `scripts/graphify-communities.py` names them by their dominant source files instead, with no
  model call.
- **`--code-only` still yields INFERRED edges.** Cross-file call resolution is guessed: 4,010 of
  30,942 edges (13%, mean confidence 0.85) on colibri. Read them as leads, as for any INFERRED
  edge. They come from tree-sitter heuristics, not a model, so the build stays offline.
- **`--backend claude-cli` works on `extract`** although `extract --help` lists only
  `gemini|kimi|claude|openai|deepseek|ollama`. Its fixed prompt is large: one one-line markdown
  file cost 35,633 input tokens.

### What follows for idea-vault

1. **A single idea is small. That is where Graphify's own numbers show no gain**, and where
   idea-vault already loads a compact, curated memory (`MEMORY.md` plus gated facts). Pointing
   Graphify at one idea's folder buys little and adds ungated, model-inferred edges.
2. **The only foil-side lever is the owner's large reference corpus**, a personal Obsidian vault
   attached as a source, where a Graphify map can stand in for some blind greps. It is
   **unproven**. Source reads cost tokens only when the foil makes them, so a map can *add* tokens
   if the foil reads little today. A map that leaves notes out silently loses recall, which is the
   same failure that got query-driven retrieval killed ([ADR-0031](./adr/0031-query-driven-fact-retrieval-killed.md)).
   So Layer 2 starts by measuring, and the map is an extra index, never a filter.
3. **The second win is development sessions on this repo.** `graphify extract src --code-only` is
   offline and deterministic, and gives a coding agent a map of the 40-odd modules (Layer 3).
4. **Obsidian's value is the owner's view**: graph, Bases, backlinks, local graph by depth. It is a
   second writer to the truth, so it is **view-only by default**. The narrow edits that are safe
   come with two rules: frontmatter dates stay Text, and renames happen in the app, not in
   Obsidian.

## Setup

Four layers, each optional and independent. Layer 0 is the prerequisite.

### Layer 0 — idea-vault itself

| Where | Run |
|---|---|
| Laptop | `cargo run` (or `cargo build --release`), then open `http://localhost:3000`. Ollama must be running (`ollama serve`, model pulled), or set `IDEA_VAULT_LLM_BACKEND=claude-code` with an authenticated `claude` CLI. |
| Containers | `cp .env.example .env`, then `docker compose up -d --build`. See [12-deployment](./12-deployment.md) (CPU, GPU, claude-code override). |
| Claude Code on the web | `bash scripts/session-up.sh`, then `python3 scripts/session-mcp.py call …`. See [12-deployment "Agent sessions"](./12-deployment.md#agent-sessions-claude-code-on-the-web). |

Check it with `cargo run -- validate` (read-only; exit 0 = clean).

### Layer 1 — Obsidian over the vault (view-only by default)

1. **Open the vault folder as an Obsidian vault:** Open folder as vault → `vault/` (the folder
   holding `.idea-vault-root`). Obsidian creates `vault/.obsidian/`, which idea-vault ignores.
   Don't nest another vault inside it.
2. **Settings → Files and links:**
   1. **Automatically update internal links: off.** A rename in Obsidian would rewrite other
      ideas' `[[slug]]` links behind the app's back. Rename ideas in the app (R23 retitles in
      place; the slug stays).
   2. **Default location for new notes:** a folder outside idea folders, e.g. `inbox/`. A note
      created in the vault root or an idea folder is not an idea until the app creates one.
   3. **Excluded files:** `artifacts/` if the graph gets noisy (optional).
3. **Properties:** in Settings → Properties (or the Properties view), make sure **`created` and
   `updated` are typed _Text_**, never Date or Date & time, and never edit them by hand. The app
   writes RFC 3339 timestamps with a timezone, and a value without one makes the idea unreadable
   (findings table above). `state` is a Text property, and its values are the app's own
   (`draft`, `in_discussion`, `stored`, `reopened`). Change state with the app's Store and
   Reopen, not by editing.
4. **Treat it as view-only.** Never edit `conversation.md` (the app's append-only transcript),
   `compacted.md`, `MEMORY.md` (the app keeps it in step with `memory/`), frontmatter, or anything
   under `.runs/`. The one tolerated edit is prose in an idea's body or a fact's body, followed by
   step 5. Prefer making changes through the app (chat, Store, rename), which keeps the
   transcript and memory gates honest.
   - **Clicking an unresolved link creates a note.** In Obsidian, `[[idea#fact]]` reads as
     *file* `idea`, *heading* `fact`, which doesn't resolve. A bare `[[slug]]` to an idea folder
     doesn't resolve either. Clicking one creates an empty note in the default location, which
     is why step 2.2 points that location at `inbox/`. Delete stray notes. They aren't ideas, and
     `validate` ignores them.
   - **Sync tools** (Obsidian Sync, iCloud, Dropbox) can drop conflict copies such as
     `memory/pricing 1.md`. `validate` flags them, because `MEMORY.md` and `memory/` must match.
     They also move the vault off the machine, so keep a sensitive vault on local disk and git.
5. **After any edit in Obsidian**, run `idea-vault validate`. Restart the app or `POST
   /admin/reindex` so search and related ideas see the change.
6. **Useful views** (copy into the vault root):

   `ideas.base`, every idea as a table:

   ```yaml
   filters:
     and:
       - 'file.name == "idea"'
   properties:
     note.state:
       displayName: State
   views:
     - type: table
       name: All ideas
       groupBy:
         property: note.state
         direction: ASC
       order: [file.folder, note.title, note.state, note.tags, note.updated]
     - type: table
       name: In discussion
       filters:
         or:
           - 'state == "in_discussion"'
           - 'state == "reopened"'
   ```

   Graph view colour groups (Graph view → Groups):

   | Query | Colour meaning |
   |---|---|
   | `path:memory` | memory facts |
   | `file:idea` | idea statements |
   | `path:artifacts` | swarm, workflow and plan artifacts |

   Turn on **Local graph** with depth 2 on an idea for its neighbourhood: the same 2-hop reach the
   app's related-ideas block uses.
7. **If the vault is a git repo** (recommended for backup), add to the *vault's* `.gitignore`:

   ```gitignore
   .obsidian/workspace*.json
   .obsidian/cache
   .smart-env/
   graphify-out/
   ```

> Known limitations: Obsidian's graph shows idea-vault's links only partly. A bare `[[slug]]`
> between memory facts is ambiguous when two ideas have a fact with the same slug, and
> `[[idea#fact]]` and idea-to-idea `[[slug]]` don't resolve at all. The app resolves all three
> correctly. Fixing Obsidian's view needs a format change, which is ADR-0043 option B. It is
> listed there as a Layer 1 safety fix, not a nicety.

### Layer 2 — Your own Obsidian vault as a reference source, with a Graphify map (measure first)

This is the one place a graph might shrink the foil's context. Prove it before relying on it.

1. **Attach your notes as a plain source first.** Register the folder on `/sources` (e.g. `notes` →
   `~/notes`), then attach it to an idea (idea page → sources, frontmatter `sources: [notes]`). In
   containers, apply the generated override yourself ([12-deployment "Reference sources in
   containers"](./12-deployment.md#reference-sources-in-containers)). This alone is useful, with
   no Graphify at all.
2. **Measure the baseline** over a week of real turns on ideas with the source attached. Every AI
   job already journals its calls (`vault/<idea>/.runs/*.jsonl`, [ADR-0037](./adr/0037-run-journal-diagnostics-only-call-record.md)):

   ```bash
   python3 - vault <<'PY'
   import json, pathlib, statistics, sys
   prompt, tools = [], []
   for f in pathlib.Path(sys.argv[1]).glob("*/.runs/*.jsonl"):
       for line in f.open():
           try:
               r = json.loads(line)
           except json.JSONDecodeError:
               continue                              # a run killed mid-write leaves a torn last line
           if r.get("type") == "llm_call":
               prompt.append(r.get("meta", {}).get("usage", {}).get("prompt_tokens") or 0)
           elif r.get("type") == "tool_call" and r.get("name", "").startswith("source_"):
               tools.append(len(r.get("result_text", "")))   # Ollama path; capped at 12,000 chars per call
   print("median prompt tokens/call:", statistics.median(prompt or [0]))
   print("source_* calls:", len(tools), "median result chars:", statistics.median(tools or [0]))
   PY
   ```

   On claude-code, `prompt_tokens` includes whatever the foil read through `--add-dir`. If the
   foil reads less per turn than a `GRAPH_REPORT.md` of your notes would cost, **stop here**: the
   map adds tokens, so keep it as a browsing aid only.
3. **Build the map next to the notes, not inside them.** The source is read-only (ADR-0021), and a
   map written into it would be fed back into the next build. Build it with
   [`scripts/graphify-map.sh`](../scripts/graphify-map.sh), the one place where the backend is
   decided. It refuses to run with a cloud key in the environment, refuses a map dir inside the
   source or inside `vault/`, accepts only `--backend ollama` or `--backend claude-cli`
   (`--code-only` by default), clusters with `--no-label`, and writes `BUILT.txt` and a
   deterministic `COMMUNITIES.md` next to `graphify-out/`:

   ```bash
   uv tool install "graphifyy[ollama]"
   bash scripts/graphify-map.sh ~/notes ~/notes-map --backend ollama   # Ollama from OLLAMA_BASE_URL, else IDEA_VAULT_OLLAMA_URL
   ```

   `--backend claude-cli` (your local `claude` binary on your subscription) also stays within
   this repo's AI rule, but it sends the whole corpus to Anthropic. Choose it deliberately, never
   as a fallback for a slow Ollama build. For a sensitive vault, the answer is Ollama or no map.
4. **Attach the map as a second source** (`notes-map` → `~/notes-map`) on the same idea.
5. **Use it as an index, never a filter.** Put this once in the idea body, or say it in chat:
   *"`notes-map/COMMUNITIES.md` and `notes-map/graphify-out/GRAPH_REPORT.md` are a map of my notes built on the date in `BUILT.txt`. Use it to
   find where to look, then still search `notes` directly. Notes missing from the map still exist.
   If `BUILT.txt` is older than a week, say so."* Small Ollama models follow this unreliably, so
   judge by recall, not by obedience.
6. **Check recall, not just tokens.** Pick 5–10 questions whose answers you know are in your
   notes. Ask each on a fresh idea with `notes` only, then with `notes` and `notes-map`, and
   compare answers and step 2's numbers. Keep the map only if recall holds *and* tokens drop.
7. **Treat INFERRED and AMBIGUOUS edges as leads, not facts.** Anything the foil learns this way
   reaches memory only through the normal Store gate, which needs a verbatim quote from the
   discussion ([ADR-0023](./adr/0023-verification-layer.md)). Rebuild after large edits.

Without Graphify, Layer 2's steps 1–2 still apply. A hand-written index note (an MOC) in your
notes is the zero-dependency map.

### Layer 3 — Graphify on a large codebase (this one or any repository you work in)

This is where Graphify's numbers are strongest. Code is parsed locally with tree-sitter, so a
`--code-only` build is offline, deterministic and free. No model sees the code. Its INFERRED
edges are tree-sitter guesses at cross-file calls (13% on colibri, below), not model judgements,
and still only leads. A large repo does not fit in any context window. For scale, idea-vault's own `src/`
is 100 Rust files and 57k lines (~2.2 MB, roughly 550k tokens), so an agent can only ever read a
slice of it. A map tells it which slice.

**What a map of a real repo looks like** (Graphify's committed case study `worked/rsl-siege-manager`,
a Python and TypeScript monorepo, not re-run here):

- **Output:** 1,886 nodes, 3,876 edges and 141 communities, built in about 10 minutes at $0.
- **Size:** the entry map `GRAPH_REPORT.md` is **36 KB (~9k tokens)**. `graph.json` is **2 MB**
  and is queried, never read whole.
- **Its own review found:**
  - test fixtures dominate "core abstractions" when tests are included;
  - god nodes skew to entry points and utilities (a one-line `cn()` helper ranked 5th);
  - community cohesion was uniformly low;
  - that run still carried 10% INFERRED edges (average confidence 0.62).

So exclude tests and generated code, and use `--code-only`.

```bash
cd <repo>
printf 'tests/\n**/*_test.*\ntarget/\nnode_modules/\ndist/\n*.min.js\n' > .graphifyignore   # gitignore syntax
ex="$(git rev-parse --git-path info/exclude)"                 # works in worktrees and submodules too
grep -qxF .graphifyignore "$ex" 2>/dev/null || echo .graphifyignore >> "$ex"   # keep a clone you don't own clean
bash <idea-vault>/scripts/graphify-map.sh . ../<repo>-map    # --code-only by default; the map goes beside the repo
graphify hook install          # optional: rebuild on commit and checkout; run `graphify update .` after `git pull`
graphify install --project     # optional: the /graphify skill for this repo (in idea-vault, .claude/ stays unversioned, ADR-0041)
```

**How a coding session uses it:** read `COMMUNITIES.md` and the god nodes in
`graphify-out/GRAPH_REPORT.md` once (the module map), then `/graphify path A B` or `/graphify explain X` to find the few files a change
touches. Open only those, then grep around them. The map tells the agent where to look. The
compiler, tests and gate decide whether the change is right.

**What Obsidian adds for code:** a human view, not agent tokens.
`graphify export obsidian --dir ~/vaults/<repo>-map` (a *separate* vault, never `vault/`) gives one
browsable note per module or symbol and a `graph.canvas`. Keep your own architecture notes and
decisions in that Obsidian vault. Attach them to idea-vault ideas as a source (Layer 2) when you
brainstorm about that codebase.

**Measured here (2026-10-12, Graphify 0.9.87, `scripts/graphify-map.sh`)** on JustVugg/colibri
at `bf244291`, a pure-C MoE inference engine with CUDA, Vulkan and Python tooling (tests, `docs/`
and `site/` excluded):

| | |
|---|---|
| Input | 404 code files (~157k lines of C/headers, ~22k of Python); 21 files partly parsed (CUDA and some C trip the C grammar), 18 with no symbols |
| Build | 20 s wall on 4 workers, $0, no model call |
| Graph | 10,262 nodes, 30,942 edges (87% EXTRACTED, 13% INFERRED), 299 communities |
| `graph.json` | 10.4 MB: queried, never read |
| `GRAPH_REPORT.md` | 60.9 KB (~15k tokens), a third of it the unlabeled "Community N" hub list |
| `COMMUNITIES.md` | 15.7 KB (~4k tokens): the 60 largest communities by file, the top cross-file couplings and the largest files |

The god nodes were real hubs (`Dsv4CudaTensor`, `json_get()`, `st_find()`, `ColiVkTensor`), and
the file-named communities matched the repo's one-file-per-model-family layout. The map was then
attached to an idea as a second source next to the code itself (Layer 2 steps 4–5), and the
claude-code foil was pointed at `COMMUNITIES.md` first. To measure on your repo, compare
`wc -c <map>/COMMUNITIES.md` against what your agent reads per task today (a Claude Code session's
`/cost`, or idea-vault's journal snippet in Layer 2).

**From an agent session** (docs/12 "Agent sessions"), the inbound MCP server has no source tools.
Register the repo and its map, and attach both to an idea, through the app's own form routes on
loopback:

```bash
curl -s --noproxy '*' -X POST http://127.0.0.1:3000/sources/add -d name=<repo> --data-urlencode host_path=/abs/<repo>
curl -s --noproxy '*' -X POST http://127.0.0.1:3000/sources/add -d name=<repo>-map --data-urlencode host_path=/abs/<repo>-map
curl -s --noproxy '*' -X POST http://127.0.0.1:3000/idea/<slug>/sources -d 'sources=<repo>&sources=<repo>-map'
```

### Layer 4 — Obsidian in your own Claude Code session (optional)

To let *your* interactive Claude Code session search Obsidian (not the foil):

1. Install the Local REST API plugin in Obsidian and copy its API key.
2. `claude mcp add --transport http obsidian https://127.0.0.1:27124/mcp/ --header "Authorization: Bearer <key>"`
   (the plugin uses its own local CA. Trust it, or enable the plain-HTTP port 27123 on
   loopback only).
3. Use it for owner-side research. idea-vault's foil never gets this server: it runs
   `--strict-mcp-config` with its own empty MCP config by design.

The same session can drive idea-vault over `/api/mcp` (`IDEA_VAULT_MCP_TOKEN`,
[13-mcp-server-inbound](./13-mcp-server-inbound.md)): Obsidian for your notes, idea-vault for the
ideation loop.

## Checklist

- [ ] `idea-vault validate` exits 0 after Obsidian has been opened on the vault.
- [ ] In Obsidian, `created` and `updated` are Text properties.
- [ ] "Automatically update internal links" is off.
- [ ] Every Graphify build goes through `scripts/graphify-map.sh` (`--code-only` by default, else `--backend ollama` or a deliberate `claude-cli`), so clustering runs `--no-label`.
- [ ] Graphify output lives beside a source, never inside it or inside `vault/`.
- [ ] Layer 2's map is kept only after the step 2 baseline and the step 6 recall check.
- [ ] A code map (Layer 3) is `--code-only` with tests and generated code in `.graphifyignore`.
- [ ] `GRAPHIFY_QUERY_LOG_DISABLE=1` is set where queries could leak note titles into a log.
- [ ] The vault's own `.gitignore` covers `.obsidian/workspace*.json`, `.smart-env/` and `graphify-out/`.

## How this guide was stress-tested

The draft design went through idea-vault itself on the claude-code backend, as a chat turn and
then the `interrogate` workflow (premortem, cheapest-disproof, constraints and second-order
effects lenses, then audit and synthesis). The audit counted 7 confirmed, 12 uncertain and
1 refuted. The changes it forced are in this version:

- Obsidian is view-only by default.
- Layer 2 measures first, and the map is an index, not a filter.
- The map lives beside the source.
- The backend is hard-coded in a wrapper that refuses to run with cloud keys present.
- Fact-link ambiguity is a Layer 1 safety issue (ADR-0043 B).

One claim was settled in code rather than taken on trust: project or user hooks can't reach the
foil, because of `--restricted`. Still open and unmeasured:

- whether Ollama extracts a usable graph from your notes;
- whether Graphify's `--backend` holds on every sub-pass (the wrapper's key check is the backstop);
- how stable Graphify's formats are at 0.9.x.

## Related

- [ADR-0043](./adr/0043-obsidian-and-graphify-as-derived-layers.md) (Proposed): the code changes this guide does not make.
- [ADR-0021](./adr/0021-reference-sources.md) reference sources · [ADR-0027](./adr/0027-cross-idea-retrieval-and-the-phase-2-verdict.md) related ideas · [ADR-0031](./adr/0031-query-driven-fact-retrieval-killed.md) the retrieval eval · [ADR-0039](./adr/0039-foil-hygiene-and-lockdown.md) foil lockdown.
- [12-deployment](./12-deployment.md) for hosting, and [03-data-model](./03-data-model.md) for the vault contract.
