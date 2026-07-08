//! The LLM backend seam (docs/adr/0009). Callers hold an [`LlmBackend`] and never care which
//! concrete client answers — the persist boundaries, the shared concurrency semaphore (ADR-0006),
//! and the SSE pump all sit *above* it, so they are identical for either backend.
//!
//! Live-switchable (2026-07): rather than one fixed backend chosen at boot, `LlmBackend` holds an
//! Ollama client, the claude-code config, and an `Arc<RwLock<LlmSettings>>`. Each call reads the
//! current settings to pick the backend and apply its params (Ollama temperature; claude-code
//! model + effort), so the Settings page can toggle backends and tune them with no restart.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::ai::budget::ContextBudget;
use crate::ai::claude_code::{ClaudeCodeClient, ClaudeCodeConfig};
use crate::ai::ollama::{ChatMessage, ChatOptions, OllamaClient, TokenStream};
use crate::ai::{AiError, AiHealth};
use crate::config::LlmBackendKind;

/// Assumed Ollama context window (tokens) until `/api/show` answers — equal to the crate's
/// pre-dynamic-budget fixed 16 KiB byte budget (`ContextBudget::for_model_tokens(8192)`), so a
/// cold cache behaves exactly like the old constant.
pub(crate) const FALLBACK_OLLAMA_CTX_TOKENS: usize = 8_192;

/// Cap on the *auto-derived* Ollama window (tokens). `num_ctx` allocates KV cache — with K
/// concurrent calls (ADR-0006) a 128k-native model would silently balloon VRAM. An explicit
/// per-backend override (`ollama_ctx_tokens`) bypasses the cap: the owner asked for it.
pub(crate) const DEFAULT_OLLAMA_CTX_CAP: usize = 32_768;

/// Web-tool loop bounds (ADR-0017): rounds of "model may call tools", and executed calls per
/// round. 4 rounds × 3 calls is plenty for search-then-read-two-pages; anything deeper burns
/// local-model context for diminishing returns.
const MAX_TOOL_ROUNDS: usize = 4;
const MAX_CALLS_PER_ROUND: usize = 3;

/// How long a failed `/api/show` probe is remembered before it is retried. Without a negative
/// cache, an Ollama that answers `/api/chat` but persistently fails `/api/show` (a proxy, a
/// version that lacks the route, a model-name mismatch) would pay the probe timeout on EVERY
/// dispatch, forever — D20 says degrade, not silently add latency. The budget still self-heals:
/// the first dispatch after the backoff re-probes.
pub(crate) const CTX_PROBE_RETRY_AFTER: Duration = Duration::from_secs(60);

/// One `/api/show` probe outcome per model: the learned native window, or a failed probe with
/// its timestamp so retries back off ([`CTX_PROBE_RETRY_AFTER`]) instead of re-paying the probe
/// timeout on every dispatch.
#[derive(Clone, Copy, Debug)]
enum CtxProbe {
    Known(usize),
    FailedAt(Instant),
}

/// Context window (tokens) implied by a claude-code model name: the `[1m]` long-context marker
/// means 1M, anything else (including the CLI-default empty string) means the standard 200k.
/// The `"1m"` substring match is deliberately loose — the `claude_ctx_tokens` override covers
/// any future collision. There is deliberately NO default cap on the claude budget: the CLI
/// manages its own context, and the assembled prompt is one-shot input.
pub(crate) fn claude_window_tokens(model: &str) -> usize {
    if model.trim().to_lowercase().contains("1m") {
        1_000_000
    } else {
        200_000
    }
}

/// Runtime-tunable LLM settings (the Settings page writes these; every call reads them).
#[derive(Clone, Debug)]
pub struct LlmSettings {
    /// Which backend answers right now.
    pub backend: LlmBackendKind,
    /// Ollama sampling temperature.
    pub temperature: f32,
    /// claude-code `--model` (empty = the CLI's default model).
    pub claude_model: String,
    /// claude-code reasoning effort (`low`/`medium`/`high`) — injected as a system-prompt hint,
    /// since the CLI has no per-call effort flag.
    pub claude_effort: String,
    /// Auto-compact (docs/adr/0012): fold the conversation head into a rolling summary before a
    /// chat turn once the context gets large. Live-tunable on the Settings page.
    pub auto_compact: bool,
    /// The effective-size fraction of the AI budget at which auto-compact fires (clamped
    /// 0.5..=0.95).
    pub compact_threshold: f32,
    /// Ollama context-window override in tokens; `0` = auto (the model's native window from
    /// `/api/show`, capped at [`DEFAULT_OLLAMA_CTX_CAP`], falling back to
    /// [`FALLBACK_OLLAMA_CTX_TOKENS`]). Per-backend because the two windows differ 10–100×.
    pub ollama_ctx_tokens: usize,
    /// claude-code context-window override in tokens; `0` = auto
    /// ([`claude_window_tokens`] of the model name — no default cap).
    pub claude_ctx_tokens: usize,
    /// Web access (ADR-0017): let the foil crawl the internet. On Ollama this runs the bounded
    /// [`ai::web`](crate::ai::web) tool loop (web_search + fetch_url); on claude-code it allows
    /// the CLI's own WebSearch/WebFetch tools. Off ⇒ both backends stay fully offline (the
    /// claude tools are explicitly disallowed, not merely unrequested).
    pub web_access: bool,
}

/// The live LLM router: both backends available, dispatch chosen per-call from [`LlmSettings`].
#[derive(Clone)]
pub struct LlmBackend {
    ollama: OllamaClient,
    /// Base claude-code config (dirs/tools/cwd/system-prompt); model+effort are applied per call.
    claude_base: ClaudeCodeConfig,
    settings: Arc<RwLock<LlmSettings>>,
    /// `/api/show` probe outcomes keyed by model name: learned native context windows, plus
    /// failed probes remembered for [`CTX_PROBE_RETRY_AFTER`] so a persistently failing
    /// `/api/show` is not re-paid on every dispatch. Self-heals: a success replaces a failure,
    /// and an expired failure re-probes.
    ollama_ctx_cache: Arc<RwLock<HashMap<String, CtxProbe>>>,
}

impl LlmBackend {
    pub fn new(ollama: OllamaClient, claude_base: ClaudeCodeConfig, settings: LlmSettings) -> Self {
        Self {
            ollama,
            claude_base,
            settings: Arc::new(RwLock::new(settings)),
            ollama_ctx_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// One-backend constructor for tests and Ollama-only runs: Ollama active, with a placeholder
    /// claude config that is never invoked unless the settings toggle to claude-code.
    pub fn ollama_only(ollama: OllamaClient) -> Self {
        let claude_base = ClaudeCodeConfig {
            binary: "claude".to_string(),
            cwd: std::path::PathBuf::from("."),
            add_dirs: Vec::new(),
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
            model: None,
            system_prompt: None,
            skip_permissions: true,
            token_timeout: std::time::Duration::from_secs(300),
        };
        Self::new(
            ollama,
            claude_base,
            LlmSettings {
                backend: LlmBackendKind::Ollama,
                temperature: 0.7,
                claude_model: String::new(),
                claude_effort: "high".to_string(),
                auto_compact: true,
                compact_threshold: 0.80,
                ollama_ctx_tokens: 0,
                claude_ctx_tokens: 0,
                // Off in the test/Ollama-only constructor: the mock server speaks the streaming
                // protocol only, and unit tests must never touch the real network. Production
                // boots from `Config::web_access` (default on) in main.rs.
                web_access: false,
            },
        )
    }

    /// A snapshot of the current settings (for the Settings page + health).
    pub fn settings(&self) -> LlmSettings {
        self.settings
            .read()
            .expect("llm settings lock poisoned")
            .clone()
    }

    /// Replace the settings (the Settings page save) — effective on the next call.
    pub fn set_settings(&self, next: LlmSettings) {
        *self.settings.write().expect("llm settings lock poisoned") = next;
    }

    /// Build a claude-code client for the current settings: apply the model override, append the
    /// effort hint to the system prompt (the CLI has no effort flag), and apply the web-access
    /// toggle (ADR-0017) — allow the CLI's own WebSearch/WebFetch when on, explicitly disallow
    /// them when off (an allowlist omission would not survive `--dangerously-skip-permissions`).
    fn claude(&self) -> ClaudeCodeClient {
        let s = self.settings();
        let mut cfg = self.claude_base.clone();
        if !s.claude_model.trim().is_empty() {
            cfg.model = Some(s.claude_model.trim().to_string());
        }
        let mut hints: Vec<String> = Vec::new();
        if !s.claude_effort.trim().is_empty() {
            hints.push(format!(
                "Reasoning effort: {}. Match the depth of your analysis to it.",
                s.claude_effort.trim()
            ));
        }
        if s.web_access {
            for tool in ["WebSearch", "WebFetch"] {
                if !cfg.allowed_tools.iter().any(|t| t == tool) {
                    cfg.allowed_tools.push(tool.to_string());
                }
            }
            hints.push(
                "Web access is enabled: use WebSearch/WebFetch when live external facts \
                 (market numbers, prior art, competitors, current events) would sharpen the \
                 interrogation, and cite the URLs you used."
                    .to_string(),
            );
        } else {
            cfg.disallowed_tools = vec!["WebSearch".to_string(), "WebFetch".to_string()];
        }
        if !hints.is_empty() {
            let hint = hints.join("\n\n");
            cfg.system_prompt = Some(match cfg.system_prompt {
                Some(p) => format!("{p}\n\n{hint}"),
                None => hint,
            });
        }
        ClaudeCodeClient::new(cfg)
    }

    /// Health probe for the degraded-AI UI (D20) — probes whichever backend is active.
    pub async fn probe(&self) -> AiHealth {
        match self.settings().backend {
            LlmBackendKind::Ollama => self.ollama.probe().await,
            LlmBackendKind::ClaudeCode => self.claude().probe().await,
        }
    }

    /// A human-facing model label for the active backend (degraded hint, meter, logs).
    pub fn model(&self) -> String {
        let s = self.settings();
        match s.backend {
            LlmBackendKind::Ollama => self.ollama.model().to_string(),
            LlmBackendKind::ClaudeCode => {
                if s.claude_model.trim().is_empty() {
                    "claude-code".to_string()
                } else {
                    s.claude_model.trim().to_string()
                }
            }
        }
    }

    /// The active backend's context window in tokens, resolved from the live settings:
    /// a nonzero per-backend override wins; otherwise Ollama uses the cached native window
    /// (fallback until `/api/show` has answered) capped at [`DEFAULT_OLLAMA_CTX_CAP`], and
    /// claude-code derives from the model name ([`claude_window_tokens`] — no default cap).
    ///
    /// Sync (one lock read + one map read, no I/O) so the meter and `over_threshold` can call it
    /// on the request path.
    pub fn context_window_tokens(&self) -> usize {
        self.window_tokens(&self.settings())
    }

    /// [`Self::context_window_tokens`] resolved from a caller-held settings snapshot, so a
    /// dispatch can size `num_ctx` from the SAME snapshot it picked the backend and temperature
    /// from rather than taking a second, later lock read.
    fn window_tokens(&self, s: &LlmSettings) -> usize {
        match s.backend {
            LlmBackendKind::Ollama => {
                if s.ollama_ctx_tokens > 0 {
                    return s.ollama_ctx_tokens;
                }
                let native = match self
                    .ollama_ctx_cache
                    .read()
                    .expect("ollama ctx cache lock poisoned")
                    .get(self.ollama.model())
                {
                    Some(CtxProbe::Known(tokens)) => *tokens,
                    Some(CtxProbe::FailedAt(_)) | None => FALLBACK_OLLAMA_CTX_TOKENS,
                };
                native.min(DEFAULT_OLLAMA_CTX_CAP)
            }
            LlmBackendKind::ClaudeCode => {
                if s.claude_ctx_tokens > 0 {
                    return s.claude_ctx_tokens;
                }
                claude_window_tokens(&s.claude_model)
            }
        }
    }

    /// The live byte budget for one assembled prompt (D21) — the single source every consumer
    /// (context assembly, compaction targets, the meter) derives from.
    pub fn context_budget(&self) -> ContextBudget {
        ContextBudget::for_model_tokens(self.context_window_tokens())
    }

    /// Learn the configured Ollama model's native context window (`/api/show`), once: a known
    /// window returns immediately; a failure within the last [`CTX_PROBE_RETRY_AFTER`] returns
    /// immediately too (negative cache — a persistently failing `/api/show` must not tax every
    /// dispatch with the probe timeout); otherwise probe, caching either outcome. Called from
    /// boot (cache warm) and from every Ollama chat dispatch.
    pub async fn refresh_ollama_ctx(&self) {
        let model = self.ollama.model().to_string();
        match self
            .ollama_ctx_cache
            .read()
            .expect("ollama ctx cache lock poisoned")
            .get(&model)
        {
            Some(CtxProbe::Known(_)) => return,
            Some(CtxProbe::FailedAt(at)) if at.elapsed() < CTX_PROBE_RETRY_AFTER => return,
            _ => {}
        }
        let probe = match self.ollama.show_context_length().await {
            Some(tokens) => {
                tracing::debug!(model = %model, tokens, "learned ollama native context window");
                CtxProbe::Known(tokens)
            }
            None => {
                tracing::debug!(
                    model = %model,
                    retry_after_secs = CTX_PROBE_RETRY_AFTER.as_secs(),
                    "ollama /api/show probe failed; using the fallback window, backing off"
                );
                CtxProbe::FailedAt(Instant::now())
            }
        };
        self.ollama_ctx_cache
            .write()
            .expect("ollama ctx cache lock poisoned")
            .insert(model, probe);
    }

    /// Per-call Ollama options from the dispatch's own settings snapshot. `num_ctx` is ALWAYS
    /// sent — even the fallback 8192 beats Ollama's ~4k server default (which silently truncated
    /// our 16 KiB prompts) — and is floored at the window the already-assembled `messages` imply
    /// ([`ContextBudget::min_window_tokens`]): the prompt was sized against a budget snapshot
    /// taken BEFORE the shared semaphore (ADR-0006), so a Settings edit while the job was queued
    /// must never shrink the window under it — Ollama would silently truncate, the exact failure
    /// ADR-0014 exists to prevent. The floor can never exceed the assemble-time window, so it
    /// re-introduces no VRAM surprise.
    fn ollama_options(&self, settings: &LlmSettings, messages: &[ChatMessage]) -> ChatOptions {
        let prompt_bytes: usize = messages.iter().map(|m| m.content.len()).sum();
        let num_ctx = self
            .window_tokens(settings)
            .max(ContextBudget::min_window_tokens(prompt_bytes));
        ChatOptions {
            temperature: Some(settings.temperature),
            num_ctx: Some(num_ctx),
        }
    }

    /// Non-streaming completion (extraction, skills, agents). With web access on (ADR-0017) the
    /// Ollama path runs the bounded tool loop instead of a plain one-shot call.
    pub async fn chat(&self, messages: Vec<ChatMessage>) -> Result<String, AiError> {
        let s = self.settings();
        match s.backend {
            LlmBackendKind::Ollama => {
                // Cold cache: this very call refreshes the window while the prompt was assembled
                // at the fallback budget; the next turn assembles at the real window. Accepted —
                // one conservative turn, never an over-budget one.
                self.refresh_ollama_ctx().await;
                let options = self.ollama_options(&s, &messages);
                if s.web_access {
                    self.ollama_chat_with_web(options, messages).await
                } else {
                    self.ollama.chat_with(options, messages).await
                }
            }
            LlmBackendKind::ClaudeCode => self.claude().chat(messages).await,
        }
    }

    /// The Ollama web-tool loop (ADR-0017): offer `web_search`/`fetch_url` on a non-streaming
    /// `/api/chat`, execute whatever the model calls, feed results back as `role: "tool"`
    /// messages, and repeat — bounded by [`MAX_TOOL_ROUNDS`] rounds and [`MAX_CALLS_PER_ROUND`]
    /// executions per round, then one forced tool-free call so the turn always ends in prose.
    ///
    /// Degrades, never dies (D20): a model without tool support ("does not support tools") falls
    /// back to the plain offline call, and a failed search/fetch returns as readable tool-result
    /// text the model can route around ([`crate::ai::web::execute_tool`] is infallible).
    async fn ollama_chat_with_web(
        &self,
        options: ChatOptions,
        messages: Vec<ChatMessage>,
    ) -> Result<String, AiError> {
        let tools = crate::ai::web::tool_definitions();
        let mut convo: Vec<serde_json::Value> = messages
            .iter()
            .map(|m| serde_json::json!({ "role": m.role, "content": m.content }))
            .collect();

        for round in 0..MAX_TOOL_ROUNDS {
            let msg = match self.ollama.chat_tools(options, &convo, Some(&tools)).await {
                Ok(msg) => msg,
                // First round only: a model without tool support answers 400 — run the turn as
                // a plain offline call instead of failing it.
                Err(AiError::Backend(detail))
                    if round == 0 && detail.contains("does not support tools") =>
                {
                    tracing::warn!(
                        model = self.ollama.model(),
                        "web access is on but the model does not support tools; \
                         falling back to a plain offline call"
                    );
                    return self.ollama.chat_with(options, messages).await;
                }
                Err(e) => return Err(e),
            };

            let calls = msg
                .get("tool_calls")
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default();
            if calls.is_empty() {
                // No (more) tool use — the content is the reply.
                return Ok(msg
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or_default()
                    .to_string());
            }

            convo.push(msg.clone());
            for call in calls.iter().take(MAX_CALLS_PER_ROUND) {
                let name = call
                    .pointer("/function/name")
                    .and_then(|n| n.as_str())
                    .unwrap_or_default();
                let args = call
                    .pointer("/function/arguments")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                tracing::info!(tool = name, args = %args, "web tool call (ollama loop)");
                let result = crate::ai::web::execute_tool(name, &args).await;
                convo.push(serde_json::json!({
                    "role": "tool",
                    "tool_name": name,
                    "content": result,
                }));
            }
        }

        // Rounds exhausted: one final call WITHOUT tools so the model must answer in prose off
        // everything it gathered.
        let msg = self.ollama.chat_tools(options, &convo, None).await?;
        Ok(msg
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string())
    }

    /// Streaming completion (D11). Terminal on error; aborts its backend when dropped, so a partial
    /// reply is never persisted.
    pub async fn chat_stream(&self, messages: Vec<ChatMessage>) -> Result<TokenStream, AiError> {
        let s = self.settings();
        match s.backend {
            LlmBackendKind::Ollama => {
                self.refresh_ollama_ctx().await;
                let options = self.ollama_options(&s, &messages);
                self.ollama.chat_stream_with(options, messages).await
            }
            LlmBackendKind::ClaudeCode => self.claude().chat_stream(messages).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_backend() -> LlmBackend {
        // Never dialled in these tests — resolution is lock/map reads only.
        let ollama = OllamaClient::new("http://127.0.0.1:9", "llama3.2").unwrap();
        LlmBackend::ollama_only(ollama)
    }

    #[test]
    fn claude_window_tokens_maps_the_1m_marker() {
        assert_eq!(claude_window_tokens("opus"), 200_000);
        assert_eq!(claude_window_tokens(""), 200_000);
        assert_eq!(claude_window_tokens("opus[1m]"), 1_000_000);
        assert_eq!(claude_window_tokens("Sonnet[1M]"), 1_000_000);
        assert_eq!(claude_window_tokens("opus-4-1"), 200_000);
    }

    #[test]
    fn cold_cache_ollama_budget_equals_the_old_16k_constant() {
        let b = test_backend();
        assert_eq!(b.context_window_tokens(), FALLBACK_OLLAMA_CTX_TOKENS);
        assert_eq!(
            b.context_budget().max_bytes,
            16 * 1024,
            "fallback must be byte-identical to the pre-dynamic budget"
        );
    }

    #[test]
    fn cached_native_window_is_used_but_capped() {
        let b = test_backend();
        // A modest native window is taken as-is…
        b.ollama_ctx_cache
            .write()
            .unwrap()
            .insert("llama3.2".to_string(), CtxProbe::Known(16_384));
        assert_eq!(b.context_window_tokens(), 16_384);
        // …an enormous one is clamped to the VRAM-guard cap.
        b.ollama_ctx_cache
            .write()
            .unwrap()
            .insert("llama3.2".to_string(), CtxProbe::Known(131_072));
        assert_eq!(b.context_window_tokens(), DEFAULT_OLLAMA_CTX_CAP);
    }

    #[test]
    fn ollama_override_beats_cache_and_cap() {
        let b = test_backend();
        b.ollama_ctx_cache
            .write()
            .unwrap()
            .insert("llama3.2".to_string(), CtxProbe::Known(131_072));
        let mut s = b.settings();
        s.ollama_ctx_tokens = 65_536; // over the auto cap — the owner asked for it
        b.set_settings(s);
        assert_eq!(b.context_window_tokens(), 65_536);
        assert_eq!(b.context_budget().max_bytes, 65_536 * 4 / 2);
    }

    #[test]
    fn num_ctx_is_floored_at_the_assembled_prompt() {
        // A prompt assembled against an earlier, larger budget snapshot (e.g. Settings shrank the
        // window while the job was queued on the semaphore) must not be truncated: num_ctx is
        // floored at the window the prompt's byte size implies.
        let b = test_backend();
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "x".repeat(40_000), // sized against a 20k-token window's 40 KiB budget
        }];
        let s = b.settings(); // window resolves to the 8192 fallback — smaller than the prompt
        let opts = b.ollama_options(&s, &messages);
        assert_eq!(opts.num_ctx, Some(20_000), "floored at prompt_bytes / 2");

        // A prompt comfortably inside the live window leaves num_ctx at the window itself.
        let small = vec![ChatMessage {
            role: "user".to_string(),
            content: "hi".to_string(),
        }];
        let opts = b.ollama_options(&s, &small);
        assert_eq!(opts.num_ctx, Some(FALLBACK_OLLAMA_CTX_TOKENS));
    }

    #[tokio::test]
    async fn a_failed_show_probe_is_cached_and_backed_off() {
        // Port 9 refuses instantly, so every probe fails fast.
        let b = test_backend();
        b.refresh_ollama_ctx().await;
        let first = match b.ollama_ctx_cache.read().unwrap().get("llama3.2") {
            Some(CtxProbe::FailedAt(at)) => *at,
            other => panic!("failure must be cached, got {other:?}"),
        };
        // The window still degrades to the fallback…
        assert_eq!(b.context_window_tokens(), FALLBACK_OLLAMA_CTX_TOKENS);
        // …and a dispatch within the backoff does NOT re-probe (the timestamp is untouched).
        b.refresh_ollama_ctx().await;
        match b.ollama_ctx_cache.read().unwrap().get("llama3.2") {
            Some(CtxProbe::FailedAt(at)) => assert_eq!(*at, first, "no re-probe inside backoff"),
            other => panic!("failure entry must survive the backoff window, got {other:?}"),
        }
        // An expired failure re-probes (fails again here → a fresh timestamp): self-healing.
        if let Some(past) = Instant::now().checked_sub(CTX_PROBE_RETRY_AFTER) {
            b.ollama_ctx_cache
                .write()
                .unwrap()
                .insert("llama3.2".to_string(), CtxProbe::FailedAt(past));
            b.refresh_ollama_ctx().await;
            match b.ollama_ctx_cache.read().unwrap().get("llama3.2") {
                Some(CtxProbe::FailedAt(at)) => {
                    assert!(*at > past, "an expired failure entry is re-probed")
                }
                other => panic!("re-probe against a dead server must fail again, got {other:?}"),
            }
        }
    }

    #[test]
    fn claude_budget_derives_from_model_and_honors_override() {
        let b = test_backend();
        let mut s = b.settings();
        s.backend = LlmBackendKind::ClaudeCode;
        s.claude_model = "opus".to_string();
        b.set_settings(s.clone());
        assert_eq!(b.context_window_tokens(), 200_000);

        s.claude_model = "sonnet[1m]".to_string();
        b.set_settings(s.clone());
        assert_eq!(
            b.context_window_tokens(),
            1_000_000,
            "no default claude cap"
        );

        s.claude_ctx_tokens = 64_000;
        b.set_settings(s);
        assert_eq!(b.context_window_tokens(), 64_000, "override wins");
    }
}
