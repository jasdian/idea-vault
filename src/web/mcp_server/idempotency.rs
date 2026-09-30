//! Idempotent replay for the long-running MCP tools (docs/adr/0033, amending ADR-0028).
//!
//! A plain-call client that retries after its result was already served (a dropped response, an
//! agent that lost track of the "still running" note) must not start a second model run — for
//! `run_skill build-prompt` that would write a second, unrelated build plan (ADR-0030). A served
//! result is therefore recorded here and replayed to an identical call, keyed either on an
//! explicit `idempotency_key` the client chose or on a hash of the call's arguments.
//!
//! The two keys have different staleness rules: an explicit key is the client saying "this is
//! the same operation", so it replays for as long as the entry lives; an args hash is only a
//! guess, so it replays only while the idea's turn count is unchanged since the run finished —
//! an identical `chat` message after an intervening turn is a new question, not a retry.
//!
//! In memory only, like the task registry itself: a replay entry is meaningless across a
//! restart, and a lost entry degrades to today's behaviour (a fresh run), never to a wrong one.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use rmcp::model::CallToolResult;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// How long a served result stays replayable — long enough to cover an agent session that
/// resumes the next morning, short enough that the in-memory map cannot grow without bound.
pub(super) const REPLAY_TTL: Duration = Duration::from_secs(24 * 3600);

/// The argument the client may set to name an operation; excluded from [`args_hash`] so a keyed
/// and an unkeyed call with otherwise equal arguments hash alike.
pub(super) const IDEMPOTENCY_KEY: &str = "idempotency_key";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum ReplayId {
    Explicit(String),
    Hash(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct ReplayKey {
    pub tool: &'static str,
    pub slug: String,
    pub id: ReplayId,
}

#[derive(Clone)]
pub(super) struct Replay {
    pub task_id: String,
    pub args_hash: String,
    pub result: CallToolResult,
    pub turns_at_finish: usize,
    pub expires: Instant,
}

#[derive(Default)]
pub(super) struct ReplayCache {
    entries: HashMap<ReplayKey, Replay>,
}

impl ReplayCache {
    /// Record a served result, sweeping expired entries first so the map only ever holds the
    /// last [`REPLAY_TTL`] of results.
    pub(super) fn insert(&mut self, key: ReplayKey, replay: Replay, now: Instant) {
        self.entries.retain(|_, r| r.expires > now);
        self.entries.insert(key, replay);
    }

    /// A live entry for `key`; an expired one counts as a miss.
    pub(super) fn lookup(&self, key: &ReplayKey, now: Instant) -> Option<&Replay> {
        self.entries.get(key).filter(|r| r.expires > now)
    }
}

/// SHA-256 (hex) of the call's arguments minus [`IDEMPOTENCY_KEY`], with object keys sorted so
/// two clients serialising the same arguments in a different order hash alike.
pub(super) fn args_hash(args: &Value) -> String {
    let mut canonical = canonical(args);
    if let Value::Object(map) = &mut canonical {
        map.remove(IDEMPOTENCY_KEY);
    }
    let text = serde_json::to_string(&canonical).unwrap_or_default();
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn canonical(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|k| (k.clone(), canonical(&map[k])))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::Content;
    use serde_json::json;

    fn replay(expires: Instant) -> Replay {
        Replay {
            task_id: "t".into(),
            args_hash: "h".into(),
            result: CallToolResult::success(vec![Content::text("r")]),
            turns_at_finish: 1,
            expires,
        }
    }

    fn key(id: &str) -> ReplayKey {
        ReplayKey {
            tool: "chat",
            slug: "s".into(),
            id: ReplayId::Explicit(id.into()),
        }
    }

    #[test]
    fn args_hash_ignores_key_order_and_the_idempotency_key() {
        let a = json!({ "slug": "s", "message": "m" });
        let b = json!({ "message": "m", "slug": "s", "idempotency_key": "k" });
        assert_eq!(args_hash(&a), args_hash(&b));
        assert_ne!(
            args_hash(&a),
            args_hash(&json!({ "slug": "s", "message": "n" }))
        );
    }

    #[test]
    fn expired_entries_miss_and_are_swept_on_insert() {
        let now = Instant::now();
        let mut cache = ReplayCache::default();
        cache.insert(key("old"), replay(now), now);
        assert!(
            cache.lookup(&key("old"), now).is_none(),
            "expiry is exclusive"
        );
        cache.insert(key("new"), replay(now + REPLAY_TTL), now);
        assert!(!cache.entries.contains_key(&key("old")), "swept");
        assert!(cache.lookup(&key("new"), now).is_some());
    }
}
