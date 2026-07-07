//! The `IdeaState` lifecycle enum and the in-memory `Idea` aggregate (frontmatter + body).
//! See docs/04-state-machine.md D9 for the transition table and
//! docs/adr/0007-state-in-frontmatter-not-db.md for why state lives in frontmatter, not SQLite.

use std::fmt;
use std::str::FromStr;

use crate::domain::frontmatter::IdeaFrontmatter;
use crate::domain::DomainError;

/// The four states an idea moves through. Serialized verbatim (snake_case) in `idea.md`
/// frontmatter — these strings are a data contract, not an implementation detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdeaState {
    Draft,
    InDiscussion,
    Stored,
    Reopened,
}

impl IdeaState {
    /// The exact frontmatter `state:` string for this variant (docs/03-data-model.md D8).
    pub fn as_str(&self) -> &'static str {
        match self {
            IdeaState::Draft => "draft",
            IdeaState::InDiscussion => "in_discussion",
            IdeaState::Stored => "stored",
            IdeaState::Reopened => "reopened",
        }
    }
}

impl FromStr for IdeaState {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "draft" => Ok(IdeaState::Draft),
            "in_discussion" => Ok(IdeaState::InDiscussion),
            "stored" => Ok(IdeaState::Stored),
            "reopened" => Ok(IdeaState::Reopened),
            other => Err(DomainError::InvalidState(other.to_string())),
        }
    }
}

impl fmt::Display for IdeaState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An idea's full in-memory representation: parsed frontmatter plus the markdown body
/// (the "current best statement", per docs/03-data-model.md).
#[derive(Debug, Clone, PartialEq)]
pub struct Idea {
    pub frontmatter: IdeaFrontmatter,
    pub body: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_as_str_matches_frontmatter_contract() {
        assert_eq!(IdeaState::Draft.as_str(), "draft");
        assert_eq!(IdeaState::InDiscussion.as_str(), "in_discussion");
        assert_eq!(IdeaState::Stored.as_str(), "stored");
        assert_eq!(IdeaState::Reopened.as_str(), "reopened");
    }

    #[test]
    fn state_display_delegates_to_as_str() {
        assert_eq!(IdeaState::Draft.to_string(), "draft");
        assert_eq!(IdeaState::InDiscussion.to_string(), "in_discussion");
        assert_eq!(IdeaState::Stored.to_string(), "stored");
        assert_eq!(IdeaState::Reopened.to_string(), "reopened");
    }

    #[test]
    fn state_from_str_all_variants() {
        assert_eq!("draft".parse::<IdeaState>().unwrap(), IdeaState::Draft);
        assert_eq!(
            "in_discussion".parse::<IdeaState>().unwrap(),
            IdeaState::InDiscussion
        );
        assert_eq!("stored".parse::<IdeaState>().unwrap(), IdeaState::Stored);
        assert_eq!(
            "reopened".parse::<IdeaState>().unwrap(),
            IdeaState::Reopened
        );
    }

    #[test]
    fn state_from_str_invalid_errors() {
        let err = "bogus".parse::<IdeaState>().unwrap_err();
        match err {
            DomainError::InvalidState(s) => assert_eq!(s, "bogus"),
            other => panic!("expected InvalidState, got {other:?}"),
        }
    }

    #[test]
    fn state_roundtrip_all_variants() {
        for state in [
            IdeaState::Draft,
            IdeaState::InDiscussion,
            IdeaState::Stored,
            IdeaState::Reopened,
        ] {
            let s = state.as_str();
            let parsed: IdeaState = s.parse().unwrap();
            assert_eq!(parsed, state);
        }
    }
}
