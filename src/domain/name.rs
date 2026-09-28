//! Registry names: the validated key of the `mcp` server and `sources` source registries.
//! A [`Name`] is spliced verbatim into model-facing tool names (`mcp__<name>__<tool>`) and
//! container mount targets (`/mnt/sources/<name>`), so it is restricted to the slug alphabet
//! ([`super::slug::is_valid`]: non-empty `[a-z0-9-]`) and an invalid one cannot be constructed.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::slug;

/// A registry name, valid by construction: non-empty `[a-z0-9-]`. Serializes as the plain
/// string; deserializing an invalid string fails.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Name(String);

impl Name {
    /// The name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A string rejected by [`Name`]'s validation; carries the rejected input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidName(pub String);

impl fmt::Display for InvalidName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid name '{}': use lowercase letters, digits and '-' only",
            self.0
        )
    }
}

impl std::error::Error for InvalidName {}

impl TryFrom<String> for Name {
    type Error = InvalidName;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if slug::is_valid(&value) {
            Ok(Self(value))
        } else {
            Err(InvalidName(value))
        }
    }
}

impl TryFrom<&str> for Name {
    type Error = InvalidName;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::try_from(value.to_string())
    }
}

impl From<Name> for String {
    fn from(name: Name) -> Self {
        name.0
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Name {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for Name {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for Name {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for Name {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<String> for Name {
    fn eq(&self, other: &String) -> bool {
        &self.0 == other
    }
}

impl PartialEq<Name> for String {
    fn eq(&self, other: &Name) -> bool {
        self == &other.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_try_from_rejects_invalid() {
        for bad in [
            "",
            "Has Caps",
            "under_score",
            "dots.too",
            "a/b",
            "a b",
            "mcp__x",
        ] {
            let err = Name::try_from(bad).unwrap_err();
            assert!(err.to_string().contains(bad), "{err}");
            assert!(Name::try_from(bad.to_string()).is_err());
        }
        let ok = Name::try_from("my-tracker-2").unwrap();
        assert_eq!(ok.as_str(), "my-tracker-2");
        assert_eq!(ok.to_string(), "my-tracker-2");
    }

    #[test]
    fn name_serde_rejects_invalid_on_deserialize() {
        assert!(serde_json::from_str::<Name>("\"Bad Name\"").is_err());
        assert!(serde_json::from_str::<Name>("\"\"").is_err());
        assert_eq!(
            serde_json::from_str::<Name>("\"docs\"").unwrap().as_str(),
            "docs"
        );
    }

    #[test]
    fn name_round_trips_as_plain_string() {
        let name = Name::try_from("rf-docs").unwrap();
        let json = serde_json::to_string(&name).unwrap();
        assert_eq!(json, "\"rf-docs\"");
        assert_eq!(serde_json::from_str::<Name>(&json).unwrap(), name);
    }
}
