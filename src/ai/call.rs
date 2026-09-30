//! What one model call cost and how it stopped (docs/adr/0037): the record every backend fills
//! from its own wire protocol, so a truncation or a token count is data rather than a guess.
//!
//! Integer-only on purpose: the run journal serializes these verbatim, and a float in a journal
//! line would make two identical runs serialize differently across platforms (ADR-0037).

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// Token and request counts of one call. `None` means the backend did not report the number.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallUsage {
    /// Ollama `prompt_eval_count`; claude `usage.input_tokens` plus both cache counts.
    pub prompt_tokens: Option<u64>,
    /// Ollama `eval_count`; claude `usage.output_tokens`.
    pub output_tokens: Option<u64>,
    /// Billed requests: every Ollama tool round plus the final answer, one per claude process.
    /// This is what a workflow's call budget is charged (docs/adr/0034).
    pub api_calls: u32,
}

/// Sum two optional counts: known plus unknown is the known part, so a tool loop whose one round
/// lacked a count still reports the rest.
fn add_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (a, b) => a.or(b),
    }
}

impl std::ops::AddAssign for CallUsage {
    fn add_assign(&mut self, rhs: Self) {
        self.prompt_tokens = add_opt(self.prompt_tokens, rhs.prompt_tokens);
        self.output_tokens = add_opt(self.output_tokens, rhs.output_tokens);
        self.api_calls = self.api_calls.saturating_add(rhs.api_calls);
    }
}

/// How one call went, as far as the backend said.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallMeta {
    pub usage: CallUsage,
    /// Ollama `done_reason` (`stop`, `length`, …) or the claude `result` subtype; `None` when the
    /// backend did not say.
    pub stop_reason: Option<String>,
    /// The context window actually sent (Ollama `num_ctx`); `None` for claude-code.
    pub num_ctx: Option<u32>,
    /// The largest single request's prompt of a multi-round tool loop. `usage.prompt_tokens` sums
    /// every round (each re-sends the whole conversation), so only this is comparable with one
    /// `num_ctx` window. `None` for a single request, whose `usage.prompt_tokens` is that number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_prompt_tokens: Option<u64>,
    /// Wall-clock milliseconds of the whole call, tool rounds included.
    pub ms: u64,
    /// The journal sequence number this call was recorded under, so a later `Contract` entry can
    /// point at it. Not serialized: inside the journal the enclosing entry already carries it.
    #[serde(skip)]
    pub journal_seq: Option<u32>,
}

/// The stop reason Ollama reports when generation hit its output limit.
const LENGTH_STOP: &str = "length";

impl CallMeta {
    /// The answer was cut off by the output limit, so its tail is missing.
    pub fn output_truncated(&self) -> bool {
        self.stop_reason.as_deref() == Some(LENGTH_STOP)
    }

    /// The prompt filled at least 98% of the window, so Ollama very likely dropped its head
    /// (ADR-0014). Unknown counts are never a truncation.
    pub fn input_truncated(&self) -> bool {
        let prompt = self.peak_prompt_tokens.or(self.usage.prompt_tokens);
        match (prompt, self.num_ctx) {
            (Some(prompt), Some(ctx)) if ctx > 0 => prompt >= u64::from(ctx) * 98 / 100,
            _ => false,
        }
    }
}

/// The running totals of a multi-request call (the Ollama tool loop): usage summed across rounds,
/// and the largest single prompt, which is what [`CallMeta::input_truncated`] must compare with
/// the window.
#[derive(Debug, Clone, Default)]
pub struct RoundTotals {
    usage: CallUsage,
    peak_prompt: Option<u64>,
}

impl RoundTotals {
    /// Count one request.
    pub fn add(&mut self, usage: CallUsage) {
        self.peak_prompt = match (self.peak_prompt, usage.prompt_tokens) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        self.usage += usage;
    }

    /// Count a request that reported nothing (a refused round).
    pub fn add_request(&mut self) {
        self.usage.api_calls = self.usage.api_calls.saturating_add(1);
    }

    /// The whole call's meta: `last` (the final round, which owns the stop reason) with the
    /// totals in place of its own usage.
    pub fn finish(self, last: CallMeta) -> CallMeta {
        CallMeta {
            usage: self.usage,
            peak_prompt_tokens: self.peak_prompt,
            ..last
        }
    }
}

/// Where a token stream leaves its [`CallMeta`] once the terminal chunk or `result` line has been
/// decoded; empty until then, and empty for good when the stream ended in an error.
pub type MetaSlot = Arc<Mutex<Option<CallMeta>>>;

/// A fresh, empty [`MetaSlot`].
pub fn meta_slot() -> MetaSlot {
    Arc::new(Mutex::new(None))
}

/// Fill `slot`. A poisoned lock only loses diagnostics, never the reply, so it is skipped.
pub(crate) fn fill_slot(slot: &MetaSlot, meta: CallMeta) {
    if let Ok(mut s) = slot.lock() {
        *s = Some(meta);
    }
}

/// The meta in `slot`, if the stream finished cleanly.
pub fn read_slot(slot: &MetaSlot) -> Option<CallMeta> {
    slot.lock().ok().and_then(|s| s.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(prompt: Option<u64>, num_ctx: Option<u32>) -> CallMeta {
        CallMeta {
            usage: CallUsage {
                prompt_tokens: prompt,
                ..CallUsage::default()
            },
            num_ctx,
            ..CallMeta::default()
        }
    }

    #[test]
    fn input_truncated_at_98_percent_of_num_ctx_and_unknown_is_false() {
        assert!(meta(Some(980), Some(1000)).input_truncated());
        assert!(meta(Some(1000), Some(1000)).input_truncated());
        assert!(!meta(Some(979), Some(1000)).input_truncated());
        assert!(!meta(None, Some(1000)).input_truncated());
        assert!(!meta(Some(5000), None).input_truncated());
        assert!(!meta(Some(1), Some(0)).input_truncated());
    }

    #[test]
    fn tool_loop_rounds_sum_usage_but_truncation_reads_the_peak_prompt() {
        // Four healthy rounds of ~3k prompt tokens against an 8192 window: 12k summed, never full.
        let mut totals = RoundTotals::default();
        for prompt in [2_800, 3_000, 3_100, 3_200] {
            totals.add(CallUsage {
                prompt_tokens: Some(prompt),
                output_tokens: Some(100),
                api_calls: 1,
            });
        }
        let mut meta = totals.finish(CallMeta {
            stop_reason: Some("stop".into()),
            ..CallMeta::default()
        });
        meta.num_ctx = Some(8192);
        assert_eq!(meta.usage.prompt_tokens, Some(12_100));
        assert_eq!(meta.usage.output_tokens, Some(400));
        assert_eq!(meta.usage.api_calls, 4);
        assert_eq!(meta.peak_prompt_tokens, Some(3_200));
        assert_eq!(meta.stop_reason.as_deref(), Some("stop"));
        assert!(!meta.input_truncated(), "no single round filled the window");

        // One round that did fill the window is still a truncation.
        let mut totals = RoundTotals::default();
        totals.add_request();
        totals.add(CallUsage {
            prompt_tokens: Some(8_100),
            output_tokens: None,
            api_calls: 1,
        });
        let mut meta = totals.finish(CallMeta::default());
        meta.num_ctx = Some(8192);
        assert_eq!(meta.usage.api_calls, 2);
        assert!(meta.input_truncated());
    }

    #[test]
    fn output_truncated_only_on_a_length_stop() {
        let mut m = CallMeta::default();
        assert!(!m.output_truncated());
        m.stop_reason = Some("stop".into());
        assert!(!m.output_truncated());
        m.stop_reason = Some("length".into());
        assert!(m.output_truncated());
    }

    #[test]
    fn usage_sums_keep_known_counts_and_add_api_calls() {
        let mut a = CallUsage {
            prompt_tokens: Some(10),
            output_tokens: None,
            api_calls: 1,
        };
        a += CallUsage {
            prompt_tokens: Some(5),
            output_tokens: Some(7),
            api_calls: 2,
        };
        assert_eq!(
            a,
            CallUsage {
                prompt_tokens: Some(15),
                output_tokens: Some(7),
                api_calls: 3
            }
        );
        let mut none = CallUsage::default();
        none += CallUsage::default();
        assert_eq!(none.prompt_tokens, None);
    }
}
