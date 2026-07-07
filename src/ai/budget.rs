//! Assembles a prompt for a local model within its context budget
//! (docs/06-concepts/swarm.md D21).
//!
//! Strict priority order so small local models are never handed more than they can use:
//! (1) the idea body — always included in full, never dropped or truncated;
//! (2) memory facts, most relevant first (the caller ranks them), while they fit;
//! (3) conversation turns, trimmed from the oldest — the kept window is the contiguous most
//!     recent run, rendered in chronological order.
//!
//! `ai` does not read the vault itself (D4) — callers pass in already-loaded idea body, memory
//! and conversation text; this module only trims/orders/joins, purely and deterministically.
//! The budget is counted in bytes as a cheap proxy for tokens (~4 bytes/token for English).

/// Byte budget for one assembled context. Section headers count toward the budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    pub max_bytes: usize,
}

impl ContextBudget {
    pub fn new(max_bytes: usize) -> Self {
        Self { max_bytes }
    }

    /// Rough conversion from a model's context-window size in tokens, reserving half for the
    /// model's own output and the chat scaffolding.
    pub fn for_model_tokens(tokens: usize) -> Self {
        Self {
            max_bytes: tokens.saturating_mul(4) / 2,
        }
    }
}

/// Pre-loaded inputs, ready-ranked by the caller: `memory` most-relevant-first,
/// `turns` oldest-first (chronological transcript order).
#[derive(Debug, Clone, Copy)]
pub struct ContextInput<'a> {
    pub idea_body: &'a str,
    pub memory: &'a [String],
    /// Rolling summary of the folded conversation head `turns[0..k]` (auto-compact,
    /// docs/adr/0012). `None` when no compaction applies — the assembled output is then
    /// byte-identical to a plain transcript budgeting.
    pub summary: Option<&'a str>,
    /// The conversation turns to render verbatim, oldest-first. When `summary` is `Some`, this
    /// is the verbatim tail `turns[k..n]`; otherwise the whole transcript.
    pub turns: &'a [String],
}

/// The assembled context plus what made it in — callers (and tests) can see exactly what was
/// trimmed without re-parsing the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledContext {
    pub text: String,
    /// How many leading entries of `memory` were included.
    pub included_memory: usize,
    /// How many trailing (most recent) entries of `turns` were included.
    pub included_turns: usize,
    /// True if a rolling summary was provided and fit (rendered as its own atomic block).
    pub included_summary: bool,
    /// True if the assembly did not fully fit the budget: a memory fact, the summary block, or a
    /// turn was dropped, or the always-included idea body alone already exceeds it.
    pub truncated: bool,
}

const IDEA_HEADER: &str = "## Idea\n";
const MEMORY_HEADER: &str = "## Memory\n";
const SUMMARY_HEADER: &str = "## Earlier in this discussion (summarized)\n";
const CONVERSATION_HEADER: &str = "## Conversation\n";

/// Assemble a context within `budget` from `input`, honouring the D21 priority order.
///
/// The idea body is always included in full even when it alone exceeds the budget ("always
/// included" is unconditional — the current best statement is the one thing a foil must see).
/// Memory facts are then taken in the caller's ranking order until one no longer fits (strict
/// order — no cherry-picking smaller lower-ranked facts past a bigger higher-ranked one).
/// Conversation turns are taken newest-backwards while they fit, keeping a contiguous recent
/// window, and rendered oldest-first.
pub fn assemble_context(budget: ContextBudget, input: ContextInput<'_>) -> AssembledContext {
    // 1. Idea body — unconditional.
    let mut text = String::from(IDEA_HEADER);
    text.push_str(input.idea_body);
    if !input.idea_body.ends_with('\n') {
        text.push('\n');
    }
    let mut truncated = text.len() > budget.max_bytes;

    // 2. Memory facts, ranked order, while they fit. Facts are flattened to one bullet line
    // (a multi-line fact must not break the list; byte accounting matches what is rendered).
    let mut included_memory = 0;
    if !input.memory.is_empty() {
        let mut section = String::from(MEMORY_HEADER);
        for fact in input.memory {
            let entry = format!("- {}\n", fact.trim().replace('\n', " "));
            if text.len() + section.len() + entry.len() > budget.max_bytes {
                truncated = true;
                break;
            }
            section.push_str(&entry);
            included_memory += 1;
        }
        if included_memory > 0 {
            text.push_str(&section);
        }
    }

    // 3. Rolling summary (auto-compact, docs/adr/0012): one atomic block between Memory and
    // Conversation — included whole if it fits after Idea+Memory, else dropped whole. It stands
    // in for the folded head `turns[0..k]`, so partial inclusion would be incoherent.
    let mut included_summary = false;
    if let Some(summary) = input.summary.filter(|s| !s.is_empty()) {
        let mut block = String::from(SUMMARY_HEADER);
        block.push_str(summary);
        if !summary.ends_with('\n') {
            block.push('\n');
        }
        if text.len() + block.len() <= budget.max_bytes {
            text.push_str(&block);
            included_summary = true;
        } else {
            truncated = true;
        }
    }

    // 4. Conversation turns: newest-backwards selection, chronological rendering.
    let mut included_turns = 0;
    if !input.turns.is_empty() {
        let mut used = 0;
        for turn in input.turns.iter().rev() {
            let entry_len = turn.trim_end().len() + 1; // + newline
            if text.len() + CONVERSATION_HEADER.len() + used + entry_len > budget.max_bytes {
                truncated = true;
                break;
            }
            used += entry_len;
            included_turns += 1;
        }
        if included_turns > 0 {
            text.push_str(CONVERSATION_HEADER);
            for turn in &input.turns[input.turns.len() - included_turns..] {
                text.push_str(turn.trim_end());
                text.push('\n');
            }
        }
    }

    AssembledContext {
        text,
        included_memory,
        included_turns,
        included_summary,
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn everything_fits_in_priority_order() {
        let memory = strings(&["fact one", "fact two"]);
        let turns = strings(&["## user\nfirst", "## assistant\nsecond"]);
        let out = assemble_context(
            ContextBudget::new(10_000),
            ContextInput {
                idea_body: "The idea.\n",
                memory: &memory,
                summary: None,
                turns: &turns,
            },
        );

        assert!(!out.truncated);
        assert_eq!(out.included_memory, 2);
        assert_eq!(out.included_turns, 2);
        let idea_pos = out.text.find("## Idea").unwrap();
        let mem_pos = out.text.find("## Memory").unwrap();
        let conv_pos = out.text.find("## Conversation").unwrap();
        assert!(idea_pos < mem_pos && mem_pos < conv_pos);
    }

    #[test]
    fn idea_body_is_never_dropped_even_when_it_alone_exceeds_budget() {
        let body = "x".repeat(500);
        let memory = strings(&["fact"]);
        let out = assemble_context(
            ContextBudget::new(100),
            ContextInput {
                idea_body: &body,
                memory: &memory,
                summary: None,
                turns: &[],
            },
        );

        assert!(out.text.contains(&body), "body included in full");
        assert_eq!(out.included_memory, 0);
        assert!(out.truncated);
    }

    #[test]
    fn memory_facts_take_ranking_order_and_stop_at_first_non_fit() {
        // Rank order: big fact first. It fits; the next big one doesn't; nothing after the
        // first non-fit is cherry-picked even though "tiny" would fit.
        let big_a = "a".repeat(40);
        let big_b = "b".repeat(40);
        let memory = strings(&[&big_a, &big_b, "tiny"]);
        let out = assemble_context(
            ContextBudget::new(IDEA_HEADER.len() + 5 + MEMORY_HEADER.len() + 43 + 20),
            ContextInput {
                idea_body: "idea\n",
                memory: &memory,
                summary: None,
                turns: &[],
            },
        );

        assert_eq!(out.included_memory, 1);
        assert!(out.text.contains(&big_a));
        assert!(
            !out.text.contains("tiny"),
            "no cherry-picking past a non-fit"
        );
        assert!(out.truncated);
    }

    #[test]
    fn turns_trim_from_the_oldest_and_render_chronologically() {
        let turns = strings(&["turn-one is long enough to drop", "turn-two", "turn-three"]);
        // Budget sized so only the two most recent turns fit.
        let out = assemble_context(
            ContextBudget::new(
                IDEA_HEADER.len()
                    + 5
                    + CONVERSATION_HEADER.len()
                    + "turn-two\n".len()
                    + "turn-three\n".len()
                    + 2,
            ),
            ContextInput {
                idea_body: "idea\n",
                memory: &[],
                summary: None,
                turns: &turns,
            },
        );

        assert_eq!(out.included_turns, 2);
        assert!(out.truncated);
        assert!(!out.text.contains("turn-one"), "oldest trimmed first");
        let two = out.text.find("turn-two").unwrap();
        let three = out.text.find("turn-three").unwrap();
        assert!(two < three, "kept window renders oldest-first");
    }

    #[test]
    fn zero_budget_still_yields_the_idea_body_only() {
        let memory = strings(&["fact"]);
        let turns = strings(&["turn"]);
        let out = assemble_context(
            ContextBudget::new(0),
            ContextInput {
                idea_body: "idea\n",
                memory: &memory,
                summary: None,
                turns: &turns,
            },
        );

        assert!(out.text.contains("idea"));
        assert_eq!(out.included_memory, 0);
        assert_eq!(out.included_turns, 0);
        assert!(out.truncated);
    }

    #[test]
    fn exact_budget_boundary_is_inclusive() {
        // A prospective total exactly equal to max_bytes must be accepted (`>` not `>=`).
        let memory = strings(&["ff"]);
        let exact = IDEA_HEADER.len() + "idea\n".len() + MEMORY_HEADER.len() + "- ff\n".len();
        let out = assemble_context(
            ContextBudget::new(exact),
            ContextInput {
                idea_body: "idea\n",
                memory: &memory,
                summary: None,
                turns: &[],
            },
        );
        assert_eq!(out.included_memory, 1);
        assert!(!out.truncated);
        assert_eq!(out.text.len(), exact);

        // One byte less and the fact no longer fits.
        let out = assemble_context(
            ContextBudget::new(exact - 1),
            ContextInput {
                idea_body: "idea\n",
                memory: &memory,
                summary: None,
                turns: &[],
            },
        );
        assert_eq!(out.included_memory, 0);
        assert!(out.truncated);
    }

    #[test]
    fn body_overflow_alone_sets_truncated() {
        let out = assemble_context(
            ContextBudget::new(3),
            ContextInput {
                idea_body: "idea body far over budget\n",
                memory: &[],
                summary: None,
                turns: &[],
            },
        );
        assert!(out.truncated, "body overflow must be visible to callers");
    }

    #[test]
    fn multi_line_memory_fact_is_flattened_to_one_bullet() {
        let memory = strings(&["line one\nline two"]);
        let out = assemble_context(
            ContextBudget::new(1_000),
            ContextInput {
                idea_body: "idea\n",
                memory: &memory,
                summary: None,
                turns: &[],
            },
        );
        assert!(out.text.contains("- line one line two\n"));
    }

    #[test]
    fn assembly_is_deterministic() {
        let memory = strings(&["m1", "m2"]);
        let turns = strings(&["t1", "t2", "t3"]);
        let input = ContextInput {
            idea_body: "idea\n",
            memory: &memory,
            summary: None,
            turns: &turns,
        };
        let a = assemble_context(ContextBudget::new(64), input);
        let b = assemble_context(ContextBudget::new(64), input);
        assert_eq!(a, b);
    }

    #[test]
    fn empty_sections_emit_no_headers() {
        let out = assemble_context(
            ContextBudget::new(1_000),
            ContextInput {
                idea_body: "idea\n",
                memory: &[],
                summary: None,
                turns: &[],
            },
        );
        assert!(!out.text.contains("## Memory"));
        assert!(!out.text.contains("## Conversation"));
        assert!(!out.truncated);
    }

    #[test]
    fn summary_section_renders_between_memory_and_conversation() {
        let memory = strings(&["a fact"]);
        let turns = strings(&["## user\ntail turn"]);
        let out = assemble_context(
            ContextBudget::new(10_000),
            ContextInput {
                idea_body: "idea\n",
                memory: &memory,
                summary: Some("## Decisions\n- something folded"),
                turns: &turns,
            },
        );
        assert!(out.included_summary);
        assert!(!out.truncated);
        let mem = out.text.find("## Memory").unwrap();
        let sum = out.text.find(SUMMARY_HEADER.trim()).unwrap();
        let conv = out.text.find("## Conversation").unwrap();
        assert!(
            mem < sum && sum < conv,
            "summary sits between memory and tail"
        );
        assert!(out.text.contains("- something folded"));
    }

    #[test]
    fn summary_is_dropped_whole_when_it_does_not_fit() {
        let big_summary = "s".repeat(400);
        let out = assemble_context(
            ContextBudget::new(IDEA_HEADER.len() + "idea\n".len() + 50),
            ContextInput {
                idea_body: "idea\n",
                memory: &[],
                summary: Some(&big_summary),
                turns: &[],
            },
        );
        // Atomic: none of it is spliced in, and the drop is visible.
        assert!(!out.included_summary);
        assert!(out.truncated);
        assert!(!out.text.contains(SUMMARY_HEADER.trim()));
        assert!(!out.text.contains(&big_summary));
    }

    #[test]
    fn output_is_byte_identical_when_summary_is_none() {
        // Strict-superset guarantee: with `summary: None` the assembly is exactly what it was
        // before auto-compact, so every pre-existing budgeting/test stays green.
        let memory = strings(&["m1", "m2"]);
        let turns = strings(&["## user\nfirst", "## assistant\nsecond"]);
        let without_field = |budget: usize| {
            // Re-derive the expected text the "old" way: Idea + Memory + Conversation, no summary.
            assemble_context(
                ContextBudget::new(budget),
                ContextInput {
                    idea_body: "idea\n",
                    memory: &memory,
                    summary: None,
                    turns: &turns,
                },
            )
        };
        let a = without_field(10_000);
        // A summary of Some("") must also be a no-op (empty block never rendered).
        let b = assemble_context(
            ContextBudget::new(10_000),
            ContextInput {
                idea_body: "idea\n",
                memory: &memory,
                summary: Some(""),
                turns: &turns,
            },
        );
        assert!(!a.text.contains(SUMMARY_HEADER.trim()));
        assert_eq!(a.text, b.text);
        assert!(!b.included_summary);
    }
}
