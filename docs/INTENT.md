# Intent — per-role call profiles (role tuning)

A living per-gated-change file (td-bot convention): rewritten before each gated change to state,
in the owner's words, what the change must do. `scripts/gate.sh` step 1 requires it to exist and
be non-empty; the rest of the gate proves the tree still honors the ADRs.

Swarms, workflows and skills run six agent roles, but every call uses the same temperature, claude
model and effort. Harvesters and the auditor should run cold and faithful, critics hot and
diverse, the synthesizer in between; on claude-code the auditor and synthesizer may deserve a
stronger model. I want each role to carry its own call profile.

## Acceptance criteria

- Each role (critic, researcher, advocate, harvester, synthesizer, auditor) has a temperature, and
  optionally a claude model and a claude effort; blank model/effort inherit the global value.
- Role tuning is on by default with sensible defaults (harvester/auditor cold, critic/advocate
  hot), and a Settings checkbox turns it off, which restores the single global setting.
- The Settings page shows and edits the six role rows live, with no restart.
- Role profiles apply to swarm, workflow and skill calls. Free chat with the foil, compaction and
  store-time extraction keep the global settings.
- There is one Ollama model for every role; only its temperature varies.
- A per-role claude model can never push a prompt over its model's context window.
- `ai` stays role-agnostic: it never imports `concepts`.
- ADR-0026 records the decision; ADR-0011 is not rewritten.
- Every commit ships through this gate: `bash scripts/gate.sh` green.

# Intent — cross-idea retrieval (in flight alongside per-role call profiles)

When I interrogate one idea, the foil sees nothing from the other ideas in the vault. I want the
vault's own links, tags and shared vocabulary pushed into each idea's context as a small
"Related ideas" block, without a new store, a vector DB or a model call at reindex/boot. I also
want an honest experiment that decides whether embeddings are worth building at all.

## Acceptance criteria

- `[[fact]]` and `[[idea#fact]]` links resolve into a queryable fact-to-fact table in `index.db`.
- Idea-to-idea `edges` (explicit links, exact shared tags, lexical word overlap) are derived from
  `vault/**` by reindex alone; deleting `index.db` and reindexing reproduces them exactly.
- Chat context carries an auto-injected "Related ideas" block that never includes the idea itself
  and only uses budget left over after the idea's own context, which stays byte-identical.
- Tag near-duplicates (`system-design`/`systems-design`) are surfaced, never silently merged.
- `vault_search` exists for the offline experiment only and is never exposed to the model.
- The phase-2 embeddings verdict follows the pre-registered kill criterion and lands in ADR-0027.
- No `unsafe`; `scripts/check-invariants.sh` stays green.
