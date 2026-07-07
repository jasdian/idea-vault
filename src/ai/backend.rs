//! The LLM backend seam (docs/adr/0009). Callers hold an [`LlmBackend`] and never care which
//! concrete client answers — the persist boundaries, the shared concurrency semaphore (ADR-0006),
//! and the SSE pump all sit *above* it, so they are identical for either backend.
//!
//! Live-switchable (2026-07): rather than one fixed backend chosen at boot, `LlmBackend` holds an
//! Ollama client, the claude-code config, and an `Arc<RwLock<LlmSettings>>`. Each call reads the
//! current settings to pick the backend and apply its params (Ollama temperature; claude-code
//! model + effort), so the Settings page can toggle backends and tune them with no restart.

use std::sync::{Arc, RwLock};

use crate::ai::claude_code::{ClaudeCodeClient, ClaudeCodeConfig};
use crate::ai::ollama::{ChatMessage, OllamaClient, TokenStream};
use crate::ai::{AiError, AiHealth};
use crate::config::LlmBackendKind;

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
}

/// The live LLM router: both backends available, dispatch chosen per-call from [`LlmSettings`].
#[derive(Clone)]
pub struct LlmBackend {
    ollama: OllamaClient,
    /// Base claude-code config (dirs/tools/cwd/system-prompt); model+effort are applied per call.
    claude_base: ClaudeCodeConfig,
    settings: Arc<RwLock<LlmSettings>>,
}

impl LlmBackend {
    pub fn new(ollama: OllamaClient, claude_base: ClaudeCodeConfig, settings: LlmSettings) -> Self {
        Self {
            ollama,
            claude_base,
            settings: Arc::new(RwLock::new(settings)),
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

    /// Build a claude-code client for the current settings: apply the model override and append the
    /// effort hint to the system prompt (the CLI has no effort flag).
    fn claude(&self) -> ClaudeCodeClient {
        let s = self.settings();
        let mut cfg = self.claude_base.clone();
        if !s.claude_model.trim().is_empty() {
            cfg.model = Some(s.claude_model.trim().to_string());
        }
        if !s.claude_effort.trim().is_empty() {
            let hint = format!(
                "Reasoning effort: {}. Match the depth of your analysis to it.",
                s.claude_effort.trim()
            );
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

    /// Non-streaming completion (extraction, skills, agents).
    pub async fn chat(&self, messages: Vec<ChatMessage>) -> Result<String, AiError> {
        let s = self.settings();
        match s.backend {
            LlmBackendKind::Ollama => self.ollama.chat_with(Some(s.temperature), messages).await,
            LlmBackendKind::ClaudeCode => self.claude().chat(messages).await,
        }
    }

    /// Streaming completion (D11). Terminal on error; aborts its backend when dropped, so a partial
    /// reply is never persisted.
    pub async fn chat_stream(&self, messages: Vec<ChatMessage>) -> Result<TokenStream, AiError> {
        let s = self.settings();
        match s.backend {
            LlmBackendKind::Ollama => {
                self.ollama
                    .chat_stream_with(Some(s.temperature), messages)
                    .await
            }
            LlmBackendKind::ClaudeCode => self.claude().chat_stream(messages).await,
        }
    }
}
