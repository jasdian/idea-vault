# Intent — Obsidian and Graphify as derived layers: findings and setup (ADR-0043)

The owner wants Obsidian (graph browsing over markdown) and Graphify (knowledge-graph builder)
working with idea-vault, to cut model context and improve output. This change records what was
found and how to set it up today, and changes no code:

- `docs/15-obsidian-and-graphify.md` holds research findings on Obsidian 1.12+ and Graphify
  0.9.85, checked against their sources. It also records what idea-vault tolerates (tested with
  `validate`), and a four-layer setup guide:
  - Obsidian over the vault, view-only by default
  - your notes as a reference source with a measured, opt-in Graphify map
  - Graphify `--code-only` on this repo
  - Obsidian MCP in your own Claude Code session, never the foil
- `docs/adr/0043` is **Proposed**. It lists options A–G that need code or a format change, each
  for the owner to accept or reject.

The design was stress-tested through idea-vault's own `interrogate` workflow, and the guide carries
its corrections. The ADR range in CLAUDE.md and docs/README moves to 0043, and docs/12 links the
guide.

## Acceptance criteria

- `docs/15` states every external fact with its source. It marks the Graphify token claim as
  self-measured, and lists as tested only what was run: the `validate` tolerance table, the
  journal measurement snippet, and the wrapper's refusal with a cloud key set.
- Every setup step works with the code on this branch. No step needs an ADR-0043 option.
- `docs/adr/0043` has status Proposed and lists each option with its cost and gate.
- CLAUDE.md and docs/README name ADRs 0001–0043, and `scripts/check-invariants.sh --strict` is
  clean.
- `bash scripts/gate.sh` is green.

## Expectation changes

None: this change touches no fixture, snapshot, floor or invariant rule.
