//! Output contracts (docs/adr/0023): deterministic checks that a model's answer has the shape its
//! skill promised, plus the repair that strips the chatter local models wrap answers in.
//!
//! Pure functions, no I/O and no model calls. The evaluator-optimizer loop lives with the callers:
//! a single interactive skill call validates, and on a [`Violation`] asks the model ONCE more with
//! [`retry_note`] appended; a fan-out agent only repairs (a retry per agent would double the
//! fan-out's cost). Compaction uses the heading helpers warn-only.

use crate::domain::OutputContract;

/// Why an answer failed its contract — phrased so it can be read back to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    Empty,
    NotBullets,
    NoNumberedList,
    NoFencedBlock,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Violation::Empty => "the answer was empty",
            Violation::NotBullets => {
                "the answer must be markdown bullet lines (\"- ...\"), or nothing at all if there is nothing to list"
            }
            Violation::NoNumberedList => {
                "the answer must be a numbered list (\"1. ...\", \"2. ...\"), most important first"
            }
            Violation::NoFencedBlock => {
                "the answer must be exactly one fenced ```markdown code block"
            }
        })
    }
}

/// The instruction appended to the original prompt for the one retry. The failed answer is not
/// resent — it would only spend the context budget on the mistake.
pub fn retry_note(violation: &Violation) -> String {
    format!(
        "\n\nIMPORTANT — a previous answer to this request was rejected because {violation}. \
         Answer again, following the required format exactly, with no preamble."
    )
}

fn is_bullet(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("- ") || t.starts_with("* ") || t.starts_with("+ ")
}

fn is_numbered(line: &str) -> bool {
    let t = line.trim_start();
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && {
        let rest = &t[digits..];
        rest.starts_with(". ") || rest.starts_with(") ")
    }
}

/// The lines from the first line matching `is_item` through the last one, trimmed. Everything
/// before (a "Here are the causes:" preamble) and after (a "Let me know if…" sign-off) goes;
/// continuation lines between items stay.
fn item_block(text: &str, is_item: fn(&str) -> bool) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let first = lines.iter().position(|l| is_item(l))?;
    let last = lines.iter().rposition(|l| is_item(l))?;
    Some(lines[first..=last].join("\n").trim().to_string())
}

/// The fenced block: the first opening fence line, closed by the LAST bare ```` ``` ```` line
/// after it — a build prompt routinely contains nested code fences, so the first closing fence
/// is usually the wrong one. The opening is normalized to ```` ```markdown ````.
fn fenced_block(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let open = lines.iter().position(|l| {
        let t = l.trim().to_ascii_lowercase();
        t == "```markdown" || t == "```md" || t == "```"
    })?;
    let close = lines.iter().rposition(|l| l.trim() == "```")?;
    if close <= open {
        return None;
    }
    let inner = lines[open + 1..close].join("\n");
    if inner.trim().is_empty() {
        return None;
    }
    Some(format!("```markdown\n{inner}\n```"))
}

/// Check `raw` against `contract`, returning the repaired answer (chatter stripped, shape
/// normalized) or why it cannot be repaired. `BulletsOrEmpty` accepts an empty answer.
pub fn validate(contract: OutputContract, raw: &str) -> Result<String, Violation> {
    let text = raw.trim();
    match contract {
        OutputContract::Free => {
            if text.is_empty() {
                Err(Violation::Empty)
            } else {
                Ok(text.to_string())
            }
        }
        OutputContract::BulletsOrEmpty => {
            if text.is_empty() {
                Ok(String::new())
            } else {
                item_block(text, is_bullet).ok_or(Violation::NotBullets)
            }
        }
        OutputContract::RankedList => {
            if text.is_empty() {
                return Err(Violation::Empty);
            }
            // Only the preamble goes: a ranked list may close with a verdict line on purpose.
            let lines: Vec<&str> = text.lines().collect();
            let first = lines
                .iter()
                .position(|l| is_numbered(l))
                .ok_or(Violation::NoNumberedList)?;
            Ok(lines[first..].join("\n").trim().to_string())
        }
        OutputContract::FencedMarkdown => {
            if text.is_empty() {
                Err(Violation::Empty)
            } else {
                fenced_block(text).ok_or(Violation::NoFencedBlock)
            }
        }
    }
}

/// The list items of a bullets or numbered answer, markers stripped — how an orchestrator splits
/// one agent's answer into separately auditable findings. Text with no list becomes one item.
pub fn items(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut in_list = false;
    for line in text.lines() {
        let t = line.trim();
        if is_bullet(line) || is_numbered(line) {
            let body = if is_bullet(line) {
                t[2..].trim()
            } else {
                let digits = t.chars().take_while(char::is_ascii_digit).count();
                t[digits + 2..].trim()
            };
            out.push(body.to_string());
            in_list = true;
        } else if in_list && !t.is_empty() && line.starts_with([' ', '\t']) {
            if let Some(last) = out.last_mut() {
                last.push(' ');
                last.push_str(t);
            }
        } else if !t.is_empty() {
            in_list = false;
        }
    }
    if out.is_empty() && !text.trim().is_empty() {
        out.push(text.trim().to_string());
    }
    out.retain(|i| !i.is_empty());
    out
}

/// The `## ` headings from `required` that `text` does not carry as a line of its own.
pub fn missing_headings<'a>(text: &str, required: &[&'a str]) -> Vec<&'a str> {
    required
        .iter()
        .filter(|h| !text.lines().any(|l| l.trim() == **h))
        .copied()
        .collect()
}

/// Shrink `text` to at most `max` bytes without ever cutting a `## ` heading: body lines are
/// dropped from the end of whichever section is currently largest until it fits, and a section's
/// last remaining line is shortened (on a char boundary) rather than dropped. Only if the headings
/// alone exceed `max` does it fall back to a plain char-boundary cut.
pub fn trim_sections(text: &str, max: usize) -> String {
    fn cut(s: &str, max: usize) -> &str {
        let mut end = max.min(s.len());
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].trim_end()
    }
    if text.len() <= max {
        return text.to_string();
    }
    let mut sections: Vec<(Option<String>, Vec<String>)> = vec![(None, Vec::new())];
    for line in text.lines() {
        if line.starts_with("## ") {
            sections.push((Some(line.to_string()), Vec::new()));
        } else if let Some(last) = sections.last_mut() {
            last.1.push(line.to_string());
        }
    }
    let render = |sections: &[(Option<String>, Vec<String>)]| -> String {
        let mut out: Vec<&str> = Vec::new();
        for (heading, body) in sections {
            if let Some(h) = heading {
                out.push(h);
            }
            out.extend(body.iter().map(String::as_str));
        }
        out.join("\n").trim().to_string()
    };
    loop {
        let rendered = render(&sections);
        if rendered.len() <= max {
            return rendered;
        }
        let overflow = rendered.len() - max;
        let largest = sections
            .iter_mut()
            .filter(|(_, body)| body.iter().any(|l| !l.is_empty()))
            .max_by_key(|(_, body)| body.iter().map(|l| l.len() + 1).sum::<usize>());
        let Some((_, body)) = largest else {
            return cut(&rendered, max).to_string();
        };
        let non_empty = body.iter().filter(|l| !l.is_empty()).count();
        if non_empty > 1 {
            body.pop();
        } else if let Some(line) = body.iter_mut().rev().find(|l| !l.is_empty()) {
            let keep = line.len().saturating_sub(overflow);
            *line = cut(line, keep).to_string();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_accepts_any_text_and_rejects_empty() {
        assert_eq!(validate(OutputContract::Free, "  hi \n").unwrap(), "hi");
        assert_eq!(validate(OutputContract::Free, " \n"), Err(Violation::Empty));
    }

    #[test]
    fn bullets_or_empty_strips_preamble_and_signoff_and_accepts_nothing() {
        let raw = "Here are the decisions:\n\n- Ship v1 solo\n  because time\n- Drop B2B\n\nHope this helps!";
        assert_eq!(
            validate(OutputContract::BulletsOrEmpty, raw).unwrap(),
            "- Ship v1 solo\n  because time\n- Drop B2B"
        );
        assert_eq!(validate(OutputContract::BulletsOrEmpty, "").unwrap(), "");
        assert_eq!(
            validate(OutputContract::BulletsOrEmpty, "No decisions were settled."),
            Err(Violation::NotBullets)
        );
    }

    #[test]
    fn ranked_list_strips_only_the_preamble() {
        let raw = "Sure! Causes:\n1. **Nobody pays** — x\n2) Churn — y\nMost dangerous: 1.";
        assert_eq!(
            validate(OutputContract::RankedList, raw).unwrap(),
            "1. **Nobody pays** — x\n2) Churn — y\nMost dangerous: 1."
        );
        assert_eq!(
            validate(OutputContract::RankedList, "- a bullet, not a rank"),
            Err(Violation::NoNumberedList)
        );
        assert_eq!(
            validate(OutputContract::RankedList, "2026 was a year."),
            Err(Violation::NoNumberedList),
            "a leading number without a list marker is not an item"
        );
    }

    #[test]
    fn fenced_markdown_pairs_the_opening_with_the_last_bare_fence() {
        let raw =
            "Here you go:\n```markdown\n# Build it\n```bash\ncargo test\n```\nDone.\n```\nThanks!";
        assert_eq!(
            validate(OutputContract::FencedMarkdown, raw).unwrap(),
            "```markdown\n# Build it\n```bash\ncargo test\n```\nDone.\n```"
        );
        assert_eq!(
            validate(OutputContract::FencedMarkdown, "```\nplain fence\n```").unwrap(),
            "```markdown\nplain fence\n```"
        );
        assert_eq!(
            validate(OutputContract::FencedMarkdown, "no fence at all"),
            Err(Violation::NoFencedBlock)
        );
        assert_eq!(
            validate(OutputContract::FencedMarkdown, "```markdown\n```"),
            Err(Violation::NoFencedBlock)
        );
    }

    #[test]
    fn retry_note_names_the_violation() {
        assert!(retry_note(&Violation::NoNumberedList).contains("numbered list"));
    }

    #[test]
    fn items_splits_bullets_and_numbers_and_joins_continuations() {
        let text = "Intro line\n1. First\n   more of first\n2. Second\n- third\n\nOutro";
        assert_eq!(
            items(text),
            ["First more of first", "Second", "third"].map(String::from)
        );
        assert_eq!(items("just prose"), ["just prose".to_string()]);
        assert!(items("  ").is_empty());
    }

    #[test]
    fn missing_headings_lists_only_absent_ones() {
        let text = "## Decisions\n- a\n## Open threads\n- b";
        assert_eq!(
            missing_headings(text, &["## Decisions", "## Rejected forks"]),
            ["## Rejected forks"]
        );
    }

    #[test]
    fn trim_sections_keeps_every_heading() {
        let text = "## Decisions\n- d1\n- d2 long long long long\n## Open threads\n- o1\n## Rejected forks\n- r1\n## Key facts & constraints\n- k1";
        let trimmed = trim_sections(text, 90);
        assert!(trimmed.len() <= 90, "{} bytes", trimmed.len());
        for h in [
            "## Decisions",
            "## Open threads",
            "## Rejected forks",
            "## Key facts & constraints",
        ] {
            assert!(trimmed.contains(h), "lost {h}: {trimmed}");
        }
        assert_eq!(trim_sections("short", 90), "short");
    }

    #[test]
    fn trim_sections_shortens_a_lone_line_on_a_char_boundary() {
        let out = trim_sections("héllo wörld", 3);
        assert!("héllo wörld".starts_with(&out) && !out.is_empty() && out.len() <= 3);
        let out = trim_sections("## Decisions\n- héllo wörld", 16);
        assert!(out.starts_with("## Decisions") && out.len() <= 16, "{out}");
    }
}
