# Intent — Rust handbook Tier 1: compiler-enforced lints, truth-write fix, graceful shutdown, SQLite busy_timeout, new invariants (ADR-0041)

The owner's Engineering Standards Handbook, mapped to idea-vault, found rules the gate held only by
review: a bare `unwrap()` or a `println!` in shipping code, unsafe code without a stated reason, a
lint exception without a reason, SQL built with `format!`, `anyhow` leaking into library modules.
It also found one truth write whose `Result` was discarded (the chat route's Draft→InDiscussion
frontmatter write, against ARCH-4 and ADR-0007), a server that drops every job when it is stopped,
and an index opened without an explicit busy timeout. Owner decision of 2026-09-30 ("Tier 1"), with
the owner's correction that unsafe is denied, not forbidden: a justified site opts in locally.
Amends the ADR-0041 catalog; no new ADR.

## Acceptance criteria

- `Cargo.toml` `[lints]` denies `unsafe_code`, `clippy::unwrap_used`, `todo`, `unimplemented`,
  `print_stdout`, `print_stderr`, `undocumented_unsafe_blocks` and `missing_safety_doc`;
  `clippy.toml` exempts test code from unwrap and print and sets `upper-case-acronyms-aggressive`;
  `src/main.rs` prints CLI output only under a reasoned `#[expect(clippy::print_stdout)]`.
- A failed Draft→InDiscussion frontmatter write fails the chat send (503 for a read-only idea dir),
  releases the job slot and starts no model call; no truth write's `Result` is discarded in `src/`.
- SIGINT or SIGTERM stops accepting connections, drains in-flight requests and aborts running jobs
  (each run journal ends `Cancelled`), bounded by `SHUTDOWN_GRACE`.
- The index connection sets `busy_timeout` to the named `BUSY_TIMEOUT` (5s).
- `scripts/check-invariants.sh` gains `discard-truth-write`, `graceful-shutdown`, `sql-literal`,
  `anyhow-edge`, `no-deep-super`, `busy-timeout` and `allow-reason`, each with a seeded violation
  per detection arm in `tests/gate_invariants.rs`; every clippy lint attribute in `src/` is a
  reasoned `#[expect]`.
- docs/14, docs/10 and an ADR-0041 amendment name the new rules.
- Every behaviour change is observed failing first, and `bash scripts/gate.sh` is green.

## Expectation changes

- CLIPPY_ALLOW_FLOOR: 5 → 7. The ratchet now counts every clippy lint attribute (`allow` or
  `expect`); the five `too_many_arguments` allows became reasoned `#[expect]`s and `src/main.rs`
  adds two reasoned `#[expect(clippy::print_stdout)]` for the `import` and `regrade --export` CLI
  output, which HTC-8 allows in main.rs only.
- ratchet: the zero-state text reads "clippy lint attributes (allow or expect)"; the rule is neither
  removed nor downgraded.
- check-invariants catalog: seven new error rules, `discard-truth-write`, `graceful-shutdown`,
  `sql-literal`, `anyhow-edge`, `no-deep-super`, `busy-timeout` and `allow-reason`; nothing removed
  or downgraded.
- tests/gate_invariants.rs: `SEEDS` gains thirteen rows, one per detection arm of the new rules
  (store write; serve without it, main.rs missing; literal on the format! line, on the next line;
  library module; super::super, #[path]; no busy_timeout call, schema.rs missing; allow instead of
  expect, multi-line allow, expect without reason).
- tests/support/gate.rs: the clean tree gains `src/main.rs` (with `with_graceful_shutdown`) and
  `src/index/schema.rs` (with `busy_timeout`), and `allows_rs` writes reasoned `#[expect]`s instead
  of bare `#[allow]`s so the clean tree passes `allow-reason`.
- tests/*.rs and examples/*.rs: each crate root gains a reasoned crate-level
  `#![allow(clippy::unwrap_used)]` (examples also print) because clippy's test exemption does not
  reach helpers outside `#[test]` functions; no assertion changes.
- index::queries: `turn_fact_hits` binds the snippet token count as `?3` and `refresh_lexical_fts`
  spells the eligible kinds in its literal, so no SQL is built with `format!`; results unchanged.
