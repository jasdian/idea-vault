# ADR-0025 — Registry leaves (`mcp`, `sources`) may depend on `domain`

- **Status:** Accepted
- **Date:** 2026-09-28
- **Deciders:** owner
- **Amends:** [ADR-0018](./0018-mcp-servers.md), [ADR-0021](./0021-reference-sources.md) — the
  "std/serde only" dependency wording for `crate::mcp` and `crate::sources`.

## Context

`crate::mcp` and `crate::sources` are both owner-managed name registries: `mcp` holds named MCP
server configs, `sources` holds named reference-source configs. Both registries validate the
owner-supplied name against the same crate-wide slug alphabet — `mcp::is_valid_name`
(`pub use crate::domain::slug::is_valid as is_valid_name;`) and `sources`'s direct
`use crate::domain::slug;` — because that name is later interpolated into a filesystem/protocol
surface: the `sources` name becomes the `/mnt/sources/<name>` bind path, and the `mcp` name becomes
part of the `mcp__<name>__<tool>` tool name exposed to the model. Both modules were described in
[ADR-0018](./0018-mcp-servers.md) and [ADR-0021](./0021-reference-sources.md) ("Consequences") as
depending on "std/serde only," and D4's rules table ([02-module-reference](../02-module-reference.md))
carried the same "(std/serde only)" line for `mcp` — but the code already imports `domain::slug` in
both modules, so the docs and the code have drifted. Duplicating the validation logic into each
registry instead would let the two alphabets silently diverge over time, with no single place that
states the rule.

## Decision

`mcp` and `sources` may depend on `domain`, and **only** on `domain`. They must still never import
`ai` or `web`, and they stay free of protocol knowledge (no MCP wire format in `mcp`, no compose/
mount knowledge beyond path templating in `sources`). `domain` remains the pure leaf with no
internal dependencies of its own, so an edge from `mcp`/`sources` into `domain` cannot create a
cycle — `domain` never depends back on either.

## Consequences

- D4's mermaid graph and rules table ([02-module-reference](../02-module-reference.md)) now show
  `mcp --> domain` and `sources --> domain`, and the `mcp` row's "may depend on" column reads
  `domain` instead of "(std/serde only)".
- The module doc comments in `src/mcp.rs` and `src/sources.rs` should be updated to match (tracked
  separately; not part of this ADR's edits).
- The crate-wide slug alphabet has exactly one source of truth, `domain::slug::is_valid`, used
  verbatim by both registries — no drift risk between "what a valid source name is" and "what a
  valid MCP server name is."
- [ADR-0005](./0005-single-crate-vs-workspace.md)'s future workspace mapping is unaffected:
  `domain` sits in the proposed `idea-vault-core` crate, and `mcp`/`sources` would depend on that
  core crate exactly as they depend on the `domain` module today — no inversion, no new cycle.

## Alternatives considered

- **Copy the validation check into each module instead of sharing `domain::slug`.** Rejected: two
  copies of one invariant is exactly the drift this ADR exists to prevent, and it gains nothing —
  `domain` is already the pure, dependency-free leaf built for this.
- **Introduce a validated `domain::Name` newtype now** so both registries parse into a type that is
  a valid name by construction, rather than calling a boolean check. Attractive ("parse, don't
  validate") but a larger, separate change touching both registries' constructors and error types.
  Noted as a natural follow-up, not required to close this drift.

---

> ADRs are immutable once **Accepted**. To change a decision, write a new ADR that supersedes this
> one and update the Status line above.
