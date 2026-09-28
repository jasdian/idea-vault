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
