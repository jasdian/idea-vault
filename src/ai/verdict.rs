//! What a journaled verdict names (docs/adr/0038, D40): which deterministic parser judged a model
//! answer, and the evidence it judged it against.
//!
//! These types live in `ai` because the run journal (docs/adr/0037) carries them, and `ai` sits
//! below every parser. The summaries themselves are written by each parser's own module and
//! dispatched by `crate::regrade::summarize`, so a journaled verdict and a regraded one come from
//! the same function (D4: a parser never reaches up into the bin-level `regrade`).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A deterministic judge of one model answer. Replaying it over a recorded answer is only
/// meaningful for code that needs no model call: parsing, detectors and gates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParserKind {
    /// The Auditor's `F<i>: LABEL — reason` lines over `n` findings (docs/adr/0023).
    Audit { n: usize },
    /// Store-time `FACT:` blocks and their quote evidence gate (D12, docs/adr/0023).
    Facts,
    /// A skill's output contract, by its frontmatter name (`ranked_list`, …).
    Contract { name: String },
    /// The build-plan parser and its gates G1–G14 (docs/adr/0030).
    PlanGates,
}

impl ParserKind {
    /// The spelling `regrade --parser` and the corpus directories use.
    pub fn family(&self) -> &'static str {
        match self {
            ParserKind::Audit { .. } => "audit",
            ParserKind::Facts => "facts",
            ParserKind::Contract { .. } => "contract",
            ParserKind::PlanGates => "plan-gates",
        }
    }

    /// Whether this parser reads the idea's evidence, and so needs a [`Haystack`].
    pub fn needs_haystack(&self) -> bool {
        matches!(self, ParserKind::Facts | ParserKind::PlanGates)
    }
}

/// The evidence a grounding parser checks quotes against: the idea statement and the raw
/// `conversation.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Haystack<'a> {
    pub idea_body: &'a str,
    pub conversation: &'a str,
}

/// Where a verdict's [`Haystack`] can be found again. `conversation.md` is append-only, so the
/// recorded-length prefix is the text the parser saw as long as its hash still matches; the idea
/// body is matched by hash against the live `idea.md`.
///
/// `idea_body` carries the text itself only when the same run rewrote the body (a store
/// consolidates it), since the live file can then never match again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HaystackRef {
    pub conversation_len: u64,
    pub conversation_sha256: String,
    pub idea_body_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idea_body: Option<String>,
}

impl HaystackRef {
    /// The reference to `haystack` as it stands now.
    pub fn of(haystack: Haystack) -> Self {
        HaystackRef {
            conversation_len: haystack.conversation.len() as u64,
            conversation_sha256: sha256_hex(haystack.conversation.as_bytes()),
            idea_body_sha256: sha256_hex(haystack.idea_body.as_bytes()),
            idea_body: None,
        }
    }

    /// [`Self::of`], keeping the body text because the caller is about to replace it on disk.
    pub fn of_rewritten(haystack: Haystack) -> Self {
        HaystackRef {
            idea_body: Some(haystack.idea_body.to_string()),
            ..Self::of(haystack)
        }
    }
}

/// SHA-256 of `bytes`, lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_kind_serializes_tagged_and_round_trips() {
        for kind in [
            ParserKind::Audit { n: 3 },
            ParserKind::Facts,
            ParserKind::Contract {
                name: "ranked_list".into(),
            },
            ParserKind::PlanGates,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            assert!(json.starts_with("{\"kind\":"), "{json}");
            assert_eq!(serde_json::from_str::<ParserKind>(&json).unwrap(), kind);
        }
    }

    #[test]
    fn rewritten_ref_keeps_the_body_and_the_same_hashes() {
        let h = Haystack {
            idea_body: "body",
            conversation: "## user\nhi\n",
        };
        let plain = HaystackRef::of(h);
        let kept = HaystackRef::of_rewritten(h);
        assert_eq!(plain.idea_body, None);
        assert_eq!(kept.idea_body.as_deref(), Some("body"));
        assert_eq!(plain.idea_body_sha256, kept.idea_body_sha256);
        assert_eq!(plain.conversation_len, 11);
        let json = serde_json::to_string(&plain).unwrap();
        assert!(!json.contains("idea_body\":"), "{json}");
    }
}
