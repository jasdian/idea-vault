# Intent — skill system: skills as markdown, verification layer, spine

A living per-gated-change file (td-bot convention): rewritten before each gated change to state,
in the owner's words, what the change must do. `scripts/gate.sh` step 1 requires it to exist and
be non-empty; the rest of the gate proves the tree still honors the ADRs.

This change ports the mechanisms of the owner's agent-harness skill system (`the-unknown` /
`backend-mono`: SKILLBOOK, work-hard, verified-reporting, reflect, librarian, plan-lens) into the
product's own ideation skills ([ADR-0022](adr/0022-skills-as-markdown-and-the-skill-book.md),
[ADR-0023](adr/0023-verification-layer.md)).

## Acceptance criteria

- Every ideation skill is a markdown file with frontmatter (name, description, stage, role,
  output contract, use_when, avoid_when, hidden) and a prompt body. The built-ins ship compiled
  into the binary; I can add or override a skill by dropping `<name>.md` into `vault/.skills/`
  and pressing reload on the `/skills` skill book — no restart. A broken file is listed on the
  skill book and never stops boot or takes a built-in move away.
- The skill book shows every move grouped by spine stage (steelman → attack → consequence →
  converge → capstone) with when to use it and when not to; move chips carry the same guidance.
- The five placeholder prompts are real prompts, and `steelman` and `market-size` exist.
- A skill's answer is checked against its output contract; a wrong-shaped answer to a single
  interactive move gets exactly one retry, and a build prompt persists only its fenced block.
- Swarms and workflows audit their findings by default: one Auditor call labels each finding
  CONFIRMED / UNCERTAIN / REFUTED against the discussion; refuted findings stay visible under
  "Disproven objections"; a near-uniform pass is flagged; a failed audit degrades to "unverified".
  I can switch the audit off on the Settings page.
- Storing an idea distils facts from the consolidated statement, shows the model the facts already
  in memory (ADD / UPDATE-by-appending / NOOP), and only remembers a fact whose supporting quote
  really occurs in the discussion — the rest go to a quarantined-facts artifact I can read, and
  the stored view tells me so.
- The idea page shows which spine stages the discussion has covered, suggests the next move, and
  warns (never blocks) on wrong turns such as a build prompt before any attack.
- Workflows are staged: `interrogate`, `steelman-then-attack`, `ready-to-build`.
- The whole change ships through this gate: `bash scripts/gate.sh` green.
