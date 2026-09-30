# Intent — hardening: a Rust `validate` command, frontmatter round-trip and version, conversation fsync, validate in the gate (ADR-0002, ADR-0041)

The owner's hardening plan (build plan 20260930-141215, a small ticket set, not an architecture
idea). Markdown is the source of truth (ADR-0002), so the files must be trustworthy on their own:
nothing checks a vault's frontmatter, MEMORY.md coverage or duplicate memories; a rewrite of
`idea.md` drops any frontmatter key the app does not know; `idea.md` carries no format version;
and `conversation.md` is appended without an fsync. The chat route's swallowed Draft→InDiscussion
write (the plan's T4) is already fixed on main (BE-007). Turn ordering is deliberately not
validated: consecutive user turns are legitimate (ADR-0032).

## Acceptance criteria

- `idea-vault validate` (vault from `IDEA_VAULT_VAULT_DIR`) reports every unparseable `idea.md` or
  `memory/*.md` and every slug that does not match its folder or file name, every memory fact
  missing from `MEMORY.md` and every `MEMORY.md` line pointing at a missing fact, and every pair of
  facts in one idea sharing a title or a body (case and whitespace folded). One line per finding,
  exit 1 on any finding, exit 0 when clean; read-only. `cargo test validate` covers each check.
- Rewriting an `idea.md` keeps every frontmatter key the app does not know: known keys first in
  struct order, then unknown keys sorted (`cargo test frontmatter_roundtrip`).
- A newly written `idea.md` carries a frontmatter format version; an `idea.md` without one still
  loads (`cargo test frontmatter_version`).
- Every append to `conversation.md` is fsynced before it returns (`cargo test chat_fsync`).
- `scripts/gate.sh` runs `validate` on a temporary copy of the golden vault fixture and fails on any
  finding, and `bash scripts/gate.sh` is green.

## Expectation changes

None: this change touches no fixture, snapshot, floor or invariant rule.
