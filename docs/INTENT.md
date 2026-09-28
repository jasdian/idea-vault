# Intent — doc-sync campaign: bring docs/ back in line with the code at 7b755a7

A living per-gated-change file (td-bot convention): rewritten before each gated change to state,
in the owner's words, what the change must do. `scripts/gate.sh` step 1 requires it to exist and
be non-empty; the rest of the gate proves the tree still honors the ADRs.

This campaign works the twelve leaves the 2026-09-28 `/doc-sync full` run found for
908a8ad..7b755a7. The docs are the spec, so a leaf changes a doc only where the code already
shipped the behaviour and the change breaks no confirmed decision. Everything else is either
routed as a code fix or raised to me.

## Acceptance criteria

- Every route the app serves is in D17, every template is in the 09-web-ui template tree, every
  source file is placed in the D5 layout, and every ADR is linked from docs/README.md. The
  doc-sync mechanical check reports no drift in those categories.
- 03-data-model, 05-ai-integration, 06-concepts/swarm and 06-concepts/memory say what the code
  does now: the `sources` frontmatter field, the empty-vault reindex veto, the vault-root
  dotfiles, the chat queue, the merged tool loop and per-turn source scoping, the angle cap, and
  compacted-summary reopen. Each new sentence cites the code by `module::symbol`.
- The swarm angle picker never offers a selection the server will reject.
- `.env.example` shows how to turn on the inbound MCP server token, and no code comment still cites
  the old `restart: unless-stopped` policy.
- No ADR is rewritten. Anything that contradicts an Accepted ADR comes to me as a question.
- Every commit ships through this gate: `bash scripts/gate.sh` green.
