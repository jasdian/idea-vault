//! The LLM backend seam (docs/adr/0009). Callers hold an [`LlmBackend`] and never care which
//! concrete client answers — the persist boundaries, the shared concurrency semaphore (ADR-0006),
//! and the SSE pump all sit *above* it, so they are identical for either backend.
//!
//! Live-switchable (2026-07): rather than one fixed backend chosen at boot, `LlmBackend` holds an
//! Ollama client, the claude-code config, and an `Arc<RwLock<LlmSettings>>`. Each call reads the
//! current settings to pick the backend and apply its params (Ollama temperature; claude-code
//! model + effort), so the Settings page can toggle backends and tune them with no restart.
//!
//! **MCP bridge lives here** — deliberately, to keep the dependency graph acyclic: the
//! [`mcp`](crate::mcp) module is pure config/persistence (which servers exist, std+serde only)
//! and [`ai::mcp`](crate::ai::mcp) is the pure wire client (how to call one server); neither
//! knows about the other. This module combines them per turn: the registry (injected via
//! [`LlmBackend::with_mcp`], `None` for Ollama-only test rigs) says which servers are enabled,
//! `ai::mcp` connects and lists/calls their tools, and the tool names are mangled
//! `mcp__<server>__<tool>` (the claude CLI's convention, equally valid as an Ollama function
//! name) so one flat definitions array can route back to the right server.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use futures::StreamExt as _;

use crate::ai::budget::ContextBudget;
use crate::ai::call::{fill_slot, read_slot, CallMeta, CallUsage, MetaSlot, RoundTotals};
use crate::ai::claude_code::{ClaudeCodeClient, ClaudeCodeConfig};
use crate::ai::contract::ContractOutcome;
use crate::ai::journal::{self, CallRecord, JournalHandle, ToolRecord};
use crate::ai::mcp::{McpClient, McpSession, McpTool};
use crate::ai::ollama::{ChatMessage, ChatOptions, OllamaClient, TokenStream};
use crate::ai::untrusted::{fence_untrusted, FENCE_NOTE};
use crate::ai::verdict::{HaystackRef, ParserKind};
use crate::ai::{AiError, AiHealth};
use crate::domain::OutputContract;
use crate::mcp::{McpRegistry, McpServerConfig};
use crate::sources::ResolvedSource;

/// The selectable LLM backend (docs/adr/0009). The boot default is Ollama, for an offline local
/// run (`config::Config::from_env`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmBackendKind {
    Ollama,
    ClaudeCode,
}

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
    /// Factored audit (docs/adr/0023): swarms and workflows run one Auditor call that labels each
    /// finding CONFIRMED / UNCERTAIN / REFUTED before synthesis. Costs one model call per run.
    pub audit_findings: bool,
    /// Per-role call profiles (docs/adr/0026): when on, a call scoped with
    /// [`LlmBackend::for_role`] overlays its role's [`RoleProfile`] on this snapshot.
    pub role_tuning: bool,
    /// Role name → profile. Plain string keys: `ai` never knows the role set (D4); `concepts`
    /// seeds the defaults.
    pub role_profiles: BTreeMap<String, RoleProfile>,
}

/// One agent role's call parameters (docs/adr/0026). A blank `claude_model` or `claude_effort`
/// inherits the global setting; `temperature` always applies (Ollama only — the claude CLI has
/// no temperature flag).
#[derive(Clone, Debug, PartialEq)]
pub struct RoleProfile {
    pub temperature: f32,
    pub claude_model: String,
    pub claude_effort: String,
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
    /// The MCP server registry, injected via [`with_mcp`](Self::with_mcp) rather than read from
    /// app state — `ai` must not depend on `web`/`app` (docs/02-module-reference.md D4). `None`
    /// (tests, `ollama_only`) means no MCP tools ever; an empty/all-disabled registry means the
    /// same for now but picks up a Settings edit on the very next turn.
    mcp: Option<Arc<McpRegistry>>,
    /// Per-server tool lists with a short TTL ([`MCP_TOOLS_TTL`]), keyed `name@url` so an edited
    /// URL is a natural miss. Kills the per-turn initialize+tools/list handshake under swarm
    /// fan-out; a 60s-stale tool list is harmless (calls still hit the live server).
    mcp_tools_cache: McpToolsCache,
    /// This turn's attached reference sources (ADR-0021) — always empty on the shared `AppState`
    /// instance. The web job layer resolves an idea's frontmatter attach list through the
    /// source registry and builds a per-job scoped clone via
    /// [`with_turn_sources`](Self::with_turn_sources), so `ai` never reads app state (D4).
    /// `Arc` keeps the clone cheap; the shared instance is immutable-by-construction.
    turn_sources: Arc<Vec<ResolvedSource>>,
    /// This turn's idea folder (ADR-0039): the claude-code foil's cwd, which `--restricted`
    /// makes the edge of what it may read. `None` on the shared instance, whose turns fall back
    /// to the configured base cwd.
    turn_dir: Option<Arc<std::path::PathBuf>>,
    /// The agent role this scoped clone calls as ([`for_role`](Self::for_role)); `None` on the
    /// shared instance, so role-less calls (free chat, compaction, extraction) read the global
    /// settings.
    call_role: Option<Arc<str>>,
    /// This clone's Ollama tool-loop bound as (rounds, calls per round), at most
    /// ([`MAX_TOOL_ROUNDS`], [`MAX_CALLS_PER_ROUND`]); narrowed per call site by
    /// [`with_tool_budget`](Self::with_tool_budget).
    tool_budget: (usize, usize),
    /// This job's run journal (docs/adr/0037), attached by the web job layer via
    /// [`with_journal`](Self::with_journal); `None` on the shared instance and in test rigs, whose
    /// calls go unjournaled.
    journal: Option<JournalHandle>,
    /// Billed requests made through this clone, for a workflow's call budget (docs/adr/0034):
    /// every Ollama request (each tool round included) and every claude process adds one, counted
    /// when the request is sent, so a failed request is charged too.
    api_meter: Option<Arc<AtomicU32>>,
}

/// Per-server cached tool list: fetch instant (TTL anchor) + the tools, keyed `name@url`.
type McpToolsCache = Arc<RwLock<HashMap<String, (Instant, Vec<McpTool>)>>>;

/// How long a cached MCP tool list stays fresh.
const MCP_TOOLS_TTL: Duration = Duration::from_secs(60);

impl LlmBackend {
    pub fn new(ollama: OllamaClient, claude_base: ClaudeCodeConfig, settings: LlmSettings) -> Self {
        Self {
            ollama,
            claude_base,
            settings: Arc::new(RwLock::new(settings)),
            ollama_ctx_cache: Arc::new(RwLock::new(HashMap::new())),
            mcp: None,
            mcp_tools_cache: Arc::new(RwLock::new(HashMap::new())),
            turn_sources: Arc::new(Vec::new()),
            turn_dir: None,
            call_role: None,
            tool_budget: (MAX_TOOL_ROUNDS, MAX_CALLS_PER_ROUND),
            journal: None,
            api_meter: None,
        }
    }

    /// A per-turn view whose calls are recorded in run journal `j` (docs/adr/0037). Settings,
    /// caches, registries and sources are shared with `self`.
    pub fn with_journal(&self, j: JournalHandle) -> Self {
        let mut scoped = self.clone();
        scoped.journal = Some(j);
        scoped
    }

    /// The run journal this view records into, if any.
    pub fn journal(&self) -> Option<&JournalHandle> {
        self.journal.as_ref()
    }

    /// A view that adds every billed request to `meter` (docs/adr/0034): how a workflow charges
    /// its call budget by what the backends actually sent, tool rounds included.
    pub fn with_call_meter(&self, meter: Arc<AtomicU32>) -> Self {
        let mut scoped = self.clone();
        scoped.api_meter = Some(meter);
        scoped
    }

    /// Charge one request to the call meter, if one is attached.
    fn count_request(&self) {
        if let Some(meter) = &self.api_meter {
            meter.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Record how call `meta`'s answer met `contract` in the run journal (docs/adr/0037). A no-op
    /// for an unjournaled call.
    pub fn record_contract(
        &self,
        meta: &CallMeta,
        contract: OutputContract,
        outcome: &ContractOutcome,
    ) {
        let (Some(j), Some(seq)) = (&self.journal, meta.journal_seq) else {
            return;
        };
        if let Ok(mut w) = j.lock() {
            w.record_contract(seq, &contract_name(contract), outcome.clone());
        }
    }

    /// Record a parser's verdict on call `meta`'s answer in the run journal (docs/adr/0038), for
    /// `regrade` to replay. A no-op for an unjournaled call.
    pub fn record_verdict(
        &self,
        meta: &CallMeta,
        parser: ParserKind,
        summary: String,
        haystack: Option<HaystackRef>,
    ) {
        let (Some(j), Some(seq)) = (&self.journal, meta.journal_seq) else {
            return;
        };
        if let Ok(mut w) = j.lock() {
            w.record_verdict(seq, parser, summary, haystack);
        }
    }

    /// Record the output-contract verdict on call `meta`'s raw answer (docs/adr/0038): every
    /// validated call gets one, the retry included, so `regrade --parser contract` sees each.
    pub fn record_contract_verdict(&self, meta: &CallMeta, contract: OutputContract, raw: &str) {
        if self.journal.is_none() {
            return;
        }
        self.record_verdict(
            meta,
            ParserKind::Contract {
                name: contract_name(contract),
            },
            crate::ai::contract::summarize_contract(contract, raw),
            None,
        );
    }

    /// [`Self::record_verdict`] on the call `contract` last settled on in this run: for a verdict
    /// computed downstream of the call, where its meta is no longer at hand (the build-plan gates
    /// run on the kept planner answer, and a run has one planner call). A no-op when unjournaled
    /// or when no call settled on `contract`.
    pub fn record_verdict_on_contract(
        &self,
        contract: OutputContract,
        parser: ParserKind,
        summary: String,
        haystack: Option<HaystackRef>,
    ) {
        let Some(j) = &self.journal else {
            return;
        };
        if let Ok(mut w) = j.lock() {
            if let Some(seq) = w.last_contract_call(&contract_name(contract)) {
                w.record_verdict(seq, parser, summary, haystack);
            }
        }
    }

    /// The journal fields of one call that are known before it runs; `None` when unjournaled.
    fn call_record(&self, s: &LlmSettings, messages: &[ChatMessage]) -> Option<CallRecord> {
        self.journal.as_ref()?;
        let request = serde_json::to_string(messages).unwrap_or_default();
        let (backend, temperature_milli) = match s.backend {
            LlmBackendKind::Ollama => (
                "ollama",
                // Thousandths, never a float in the journal (docs/adr/0037).
                Some((s.temperature.max(0.0) * 1000.0).round() as u32),
            ),
            LlmBackendKind::ClaudeCode => ("claude-code", None),
        };
        Some(CallRecord {
            role: self.call_role.as_deref().map(str::to_string),
            backend: backend.to_string(),
            model: self.model_label(s),
            temperature_milli,
            request_sha256: journal::sha256_hex(&request),
            response_text: String::new(),
            meta: CallMeta::default(),
        })
    }

    /// Write one finished call to the journal; returns its seq.
    fn journal_call(
        &self,
        record: Option<CallRecord>,
        text: &str,
        meta: &CallMeta,
        tools: Vec<ToolRecord>,
    ) -> Option<u32> {
        let record = record?;
        let mut w = self.journal.as_ref()?.lock().ok()?;
        Some(w.record_call(
            CallRecord {
                response_text: text.to_string(),
                meta: meta.clone(),
                ..record
            },
            tools,
        ))
    }

    /// A per-turn scoped view of the backend: same settings/caches/registries (shared `Arc`s),
    /// plus this idea's resolved reference sources (ADR-0021). Built once per background job by
    /// the web layer; attaching/detaching a source in the UI is live on the very next turn
    /// because nothing persists past the clone. The probe runs on the shared instance, and
    /// compaction and store-time extraction run on a [`with_turn_dir`](Self::with_turn_dir) view
    /// only, so they stay source-free by construction.
    pub fn with_turn_sources(&self, sources: Vec<ResolvedSource>) -> Self {
        let mut scoped = self.clone();
        scoped.turn_sources = Arc::new(sources);
        scoped
    }

    /// A per-turn view whose claude-code foil runs in `dir`, the idea's own folder (ADR-0039).
    /// Settings, caches, registries and sources are shared with `self`.
    pub fn with_turn_dir(&self, dir: std::path::PathBuf) -> Self {
        let mut scoped = self.clone();
        scoped.turn_dir = Some(Arc::new(dir));
        scoped
    }

    /// A bounded, read-only probe over this turn's attached sources (docs/adr/0030), used by the
    /// build-plan gates to check anchors and tokens without a model call. Empty on an unscoped
    /// backend, so every check reports `Unverified`.
    pub fn source_probe(&self) -> crate::ai::sources::SourceProbe {
        crate::ai::sources::SourceProbe::new(&self.turn_sources)
    }

    /// A scoped view that calls as agent role `role` (docs/adr/0026): same settings, caches and
    /// registries, with that role's [`RoleProfile`] overlaid per call while role tuning is on.
    pub fn for_role(&self, role: &str) -> Self {
        let mut scoped = self.clone();
        scoped.call_role = Some(Arc::from(role));
        scoped
    }

    /// A scoped view whose Ollama tool loop runs at most `rounds` rounds of at most `calls`
    /// executed tool calls each, both clamped to `1..=` the global bounds (AI-3). A Ground reader
    /// runs on 2×2 (docs/adr/0034): a small model reading code past that mostly overflows its
    /// context. The claude-code backend runs the CLI's own agent loop, which this cannot bound.
    pub fn with_tool_budget(&self, rounds: usize, calls: usize) -> Self {
        let mut scoped = self.clone();
        scoped.tool_budget = (
            rounds.clamp(1, MAX_TOOL_ROUNDS),
            calls.clamp(1, MAX_CALLS_PER_ROUND),
        );
        scoped
    }

    /// This clone's tool-loop bound as (rounds, calls per round).
    pub fn tool_budget(&self) -> (usize, usize) {
        self.tool_budget
    }

    /// Attach the MCP server registry (main.rs; shares the `AppState` `Arc` so registry edits are
    /// live). Builder-style because the registry is optional wiring, not a core constructor arg.
    pub fn with_mcp(mut self, mcp: Arc<McpRegistry>) -> Self {
        self.mcp = Some(mcp);
        self.mcp_tools_cache = Arc::new(RwLock::new(HashMap::new()));
        self
    }

    /// A fresh (within-TTL) cached tool list for `key` (`name@url`), if any.
    fn mcp_tools_cached(&self, key: &str) -> Option<Vec<McpTool>> {
        let cache = self
            .mcp_tools_cache
            .read()
            .expect("mcp tools cache lock poisoned");
        cache
            .get(key)
            .filter(|(at, _)| at.elapsed() < MCP_TOOLS_TTL)
            .map(|(_, tools)| tools.clone())
    }

    /// Record a freshly-listed tool set for `key`.
    fn mcp_tools_note(&self, key: &str, tools: Vec<McpTool>) {
        self.mcp_tools_cache
            .write()
            .expect("mcp tools cache lock poisoned")
            .insert(key.to_string(), (Instant::now(), tools));
    }

    /// Snapshot the enabled MCP servers for one turn (empty when no registry is attached).
    fn enabled_mcp_servers(&self) -> Vec<McpServerConfig> {
        self.mcp.as_ref().map(|r| r.enabled()).unwrap_or_default()
    }

    /// One-backend constructor for tests and Ollama-only runs: Ollama active, with a placeholder
    /// claude config that is never invoked unless the settings toggle to claude-code.
    pub fn ollama_only(ollama: OllamaClient) -> Self {
        let claude_base = ClaudeCodeConfig {
            binary: "claude".to_string(),
            cwd: std::path::PathBuf::from("."),
            add_dirs: Vec::new(),
            allowed_tools: Vec::new(),
            web_access: false,
            model: None,
            system_prompt: None,
            token_timeout: std::time::Duration::from_secs(300),
            turn_timeout: crate::ai::claude_code::DEFAULT_TURN_TIMEOUT,
            env_pass: Vec::new(),
            mcp_config_json: None,
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
                // On, as in production: tests see the same swarm/workflow call shape the owner
                // gets by default.
                audit_findings: true,
                // Off with no profiles: tests keep the single-temperature request shape unless
                // they opt in.
                role_tuning: false,
                role_profiles: BTreeMap::new(),
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

    /// The settings one call runs with: the live snapshot, overlaid by this clone's role profile
    /// when role tuning is on and the role has one (docs/adr/0026).
    fn effective_settings(&self) -> LlmSettings {
        let mut s = self.settings();
        let Some(role) = self.call_role.as_deref() else {
            return s;
        };
        if !s.role_tuning {
            return s;
        }
        if let Some(p) = s.role_profiles.get(role).cloned() {
            s.temperature = p.temperature;
            if !p.claude_model.trim().is_empty() {
                s.claude_model = p.claude_model;
            }
            if !p.claude_effort.trim().is_empty() {
                s.claude_effort = p.claude_effort;
            }
        }
        s
    }

    /// Replace the settings (the Settings page save) — effective on the next call.
    pub fn set_settings(&self, next: LlmSettings) {
        *self.settings.write().expect("llm settings lock poisoned") = next;
    }

    /// Build a claude-code client for the current settings: apply the model override, append the
    /// effort hint to the system prompt (the CLI has no effort flag), carry the web-access toggle
    /// (ADR-0017; the client adds or denies WebSearch/WebFetch), run in this turn's idea folder
    /// (ADR-0039), and hand the enabled MCP servers to the CLI as an `--mcp-config` JSON blob plus
    /// a `mcp__<name>` tool-prefix approval per server (the CLI expands a bare prefix to every
    /// tool the server offers).
    fn claude(&self, s: &LlmSettings) -> ClaudeCodeClient {
        ClaudeCodeClient::new(self.claude_config_from(s))
    }

    /// [`claude_config_from`](Self::claude_config_from) over a fresh effective snapshot.
    #[cfg(test)]
    fn claude_config(&self) -> ClaudeCodeConfig {
        self.claude_config_from(&self.effective_settings())
    }

    /// The per-call config [`claude`](Self::claude) wraps, composed from the caller's snapshot so
    /// the backend choice and its params come from one read — split out so the settings→config
    /// composition (model/effort/web/MCP) is assertable in unit tests without spawning a CLI.
    fn claude_config_from(&self, s: &LlmSettings) -> ClaudeCodeConfig {
        let mut cfg = self.claude_base.clone();
        if let Some(dir) = &self.turn_dir {
            cfg.cwd = dir.as_ref().clone();
        }
        cfg.web_access = s.web_access;
        if !s.claude_model.trim().is_empty() {
            cfg.model = Some(s.claude_model.trim().to_string());
        }
        let mcp_servers = self.enabled_mcp_servers();
        cfg.mcp_config_json = claude_mcp_config_json(&mcp_servers);
        for server in &mcp_servers {
            let prefix = format!("mcp__{}", server.name);
            if !cfg.allowed_tools.contains(&prefix) {
                cfg.allowed_tools.push(prefix);
            }
        }
        // Per-idea reference sources (ADR-0021): each resolved root becomes an `--add-dir` so
        // the CLI may read it. Only *resolved* sources reach here (the registry drops unmounted
        // roots at resolve time), so `--add-dir` never points at a nonexistent path. The
        // env-derived base `add_dirs` stay untouched underneath — the global fallback.
        for src in self.turn_sources.iter() {
            if !cfg.add_dirs.contains(&src.root) {
                cfg.add_dirs.push(src.root.clone());
            }
        }
        let mut hints: Vec<String> = Vec::new();
        if !s.claude_effort.trim().is_empty() {
            hints.push(format!(
                "Reasoning effort: {}. Match the depth of your analysis to it.",
                s.claude_effort.trim()
            ));
        }
        if s.web_access {
            hints.push(
                "Web access is enabled: use WebSearch/WebFetch when live external facts \
                 (market numbers, prior art, competitors, current events) would sharpen the \
                 interrogation, and cite the URLs you used."
                    .to_string(),
            );
        }
        // The Ollama path's equivalent is the `with_sources_note` prompt prefix — never both.
        if !self.turn_sources.is_empty() {
            let lines = self
                .turn_sources
                .iter()
                .map(|s| format!("- {} -> {}", s.name, s.root.display()))
                .collect::<Vec<_>>()
                .join("\n");
            hints.push(format!(
                "Attached reference sources for THIS idea (read-only reference material the \
                 owner registered):\n{lines}\nGrep/Read those directories when it helps \
                 interrogate the idea, and cite the source name and file path for anything you \
                 use. Never modify them — they are reference, not workspace."
            ));
        }
        if !hints.is_empty() {
            let hint = hints.join("\n\n");
            cfg.system_prompt = Some(match cfg.system_prompt {
                Some(p) => format!("{p}\n\n{hint}"),
                None => hint,
            });
        }
        cfg
    }

    /// Health probe for the degraded-AI UI (D20) — probes whichever backend is active.
    pub async fn probe(&self) -> AiHealth {
        let s = self.effective_settings();
        match s.backend {
            LlmBackendKind::Ollama => self.ollama.probe().await,
            LlmBackendKind::ClaudeCode => self.claude(&s).probe().await,
        }
    }

    /// A human-facing model label for the active backend (degraded hint, meter, logs).
    pub fn model(&self) -> String {
        self.model_label(&self.effective_settings())
    }

    /// [`model`](Self::model) from a caller-held settings snapshot.
    fn model_label(&self, s: &LlmSettings) -> String {
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
                // Prompts are sized from the global snapshot before any role overlay, so the
                // window must fit the smallest model a role call may land on (docs/adr/0026).
                let global = claude_window_tokens(&s.claude_model);
                if !s.role_tuning {
                    return global;
                }
                s.role_profiles
                    .values()
                    .map(|p| p.claude_model.trim())
                    .filter(|m| !m.is_empty())
                    .map(claude_window_tokens)
                    .fold(global, usize::min)
            }
        }
    }

    /// The live byte budget for one assembled prompt (D21) — the single source every consumer
    /// (context assembly, compaction targets, the meter) derives from.
    /// Byte size of the tool definitions that will ride the next turn's context, for the usage
    /// meter's "(+N KB tools)" term (ADR-0017): the built-in web tools when the Ollama loop would
    /// attach them, plus the last-known size of every enabled MCP server's schemas (both backends
    /// load those). Sync and network-free — MCP sizes come from the registry's display cache.
    pub fn tool_context_bytes(&self) -> usize {
        let s = self.settings();
        let mut bytes = 0;
        if s.backend == LlmBackendKind::Ollama && s.web_access {
            bytes += crate::ai::web::tool_definitions().to_string().len();
        }
        if s.backend == LlmBackendKind::Ollama && !self.turn_sources.is_empty() {
            bytes += crate::ai::sources::tool_definitions(&self.turn_sources)
                .to_string()
                .len();
        }
        if let Some(mcp) = &self.mcp {
            bytes += mcp.enabled_tools_bytes();
        }
        bytes
    }

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

    /// Prepend the deterministic sources note to the first user message on the Ollama path —
    /// the model cannot call tools it does not know exist (ADR-0021). claude-code gets the
    /// equivalent via the system-prompt hint in [`claude_config`](Self::claude_config), never
    /// both. ~200 bytes, the same un-budgeted class as the chat `FOIL_INSTRUCTION`; callers
    /// apply it BEFORE [`ollama_options`](Self::ollama_options) so the `num_ctx` floor covers
    /// it. A no-op on unscoped turns.
    fn with_sources_note(&self, mut messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
        if self.turn_sources.is_empty() {
            return messages;
        }
        let names = self
            .turn_sources
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let note = format!(
            "Attached reference sources for this idea: {names}. They are the owner's read-only \
             reference material — query them with the source_list, source_grep and source_read \
             tools when prior notes/docs would sharpen the interrogation, and cite the source \
             name and file path for anything you use."
        );
        if let Some(first) = messages.first_mut() {
            first.content = format!("{note}\n\n{}", first.content);
        }
        messages
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

    /// Non-streaming completion (extraction, skills, agents). With web access on (ADR-0017) or
    /// any MCP server enabled, the Ollama path runs the bounded tool loop instead of a plain
    /// one-shot call.
    pub async fn chat(&self, messages: Vec<ChatMessage>) -> Result<String, AiError> {
        self.chat_meta(messages).await.map(|(text, _)| text)
    }

    /// [`chat`](Self::chat) plus how the call stopped and what it cost (docs/adr/0037): the stop
    /// reason, token counts, the window sent and every billed request of a tool loop. A journaled
    /// view also records the call, its verbatim answer and its tool rounds; the returned meta then
    /// carries the journal seq.
    pub async fn chat_meta(
        &self,
        messages: Vec<ChatMessage>,
    ) -> Result<(String, CallMeta), AiError> {
        let started = Instant::now();
        let s = self.effective_settings();
        let record = self.call_record(&s, &messages);
        let mut tools: Vec<ToolRecord> = Vec::new();
        let (text, mut meta) = match s.backend {
            LlmBackendKind::Ollama => {
                // Cold cache: this very call refreshes the window while the prompt was assembled
                // at the fallback budget; the next turn assembles at the real window. Accepted —
                // one conservative turn, never an over-budget one.
                self.refresh_ollama_ctx().await;
                // Both notes BEFORE ollama_options, so the num_ctx floor counts them.
                let messages = self.with_sources_note(messages);
                let mcp_servers = self.enabled_mcp_servers();
                let (options, reply) =
                    if s.web_access || !mcp_servers.is_empty() || !self.turn_sources.is_empty() {
                        let messages = with_fence_note(messages);
                        let options = self.ollama_options(&s, &messages);
                        let reply = self
                            .ollama_chat_with_tools(
                                options,
                                messages,
                                s.web_access,
                                &mcp_servers,
                                &mut tools,
                            )
                            .await;
                        (options, reply)
                    } else {
                        let options = self.ollama_options(&s, &messages);
                        self.count_request();
                        (options, self.ollama.chat_with(options, messages).await)
                    };
                let (text, mut meta) = reply?;
                meta.num_ctx = options.num_ctx.and_then(|n| u32::try_from(n).ok());
                (text, meta)
            }
            LlmBackendKind::ClaudeCode => {
                self.count_request();
                self.claude(&s).chat_meta(messages).await?
            }
        };
        meta.ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        meta.journal_seq = self.journal_call(record, &text, &meta, tools);
        Ok((text, meta))
    }

    /// The Ollama tool loop (ADR-0017 web tools + MCP + ADR-0021 reference sources): offer
    /// `web_search`/`fetch_url` (when web access is on), the deterministic `source_*` leaves
    /// (when this turn has attached sources) and every enabled MCP server's tools (mangled
    /// `mcp__<server>__<tool>`) on a non-streaming `/api/chat`, execute whatever the model
    /// calls, feed results back as
    /// `role: "tool"` messages, and repeat — bounded by this clone's
    /// [`tool_budget`](Self::tool_budget) (at most [`MAX_TOOL_ROUNDS`] rounds and
    /// [`MAX_CALLS_PER_ROUND`] executions per round), then one forced tool-free call so the turn
    /// always ends in prose.
    ///
    /// MCP wiring is one connect + `tools/list` per enabled server per turn, and the session is
    /// kept for that turn's `tools/call`s. Degrades, never dies (D20): a server that fails to
    /// connect or list is skipped with a warning (the rest still serve); a model without tool
    /// support ("does not support tools") falls back to the plain offline call; and every failed
    /// tool execution — web or MCP — returns as readable tool-result text the model can route
    /// around, never a turn failure.
    ///
    /// The returned meta sums every round (docs/adr/0037): each request is one `api_call`, and the
    /// stop reason is the final round's. `peak_prompt_tokens` is the largest single round, which
    /// is what input truncation is judged by. Every executed tool call is appended to `tool_log`.
    async fn ollama_chat_with_tools(
        &self,
        options: ChatOptions,
        messages: Vec<ChatMessage>,
        web_access: bool,
        mcp_servers: &[McpServerConfig],
        tool_log: &mut Vec<ToolRecord>,
    ) -> Result<(String, CallMeta), AiError> {
        // Clients first, sessions second: an `McpSession` borrows its `McpClient`, so the client
        // list must be fully built (and never mutated again) before any session exists.
        let mut clients: Vec<(String, McpClient)> = Vec::new();
        for server in mcp_servers {
            match McpClient::new(server.url.clone(), server.bearer_token.clone()) {
                Ok(client) => clients.push((server.name.to_string(), client)),
                Err(e) => tracing::warn!(
                    server = %server.name,
                    error = %e,
                    "mcp client build failed; skipping server this turn"
                ),
            }
        }
        // Tool definitions come from the TTL cache when fresh — under swarm fan-out every agent
        // turn runs this path, and K concurrent × N step turns of initialize+tools/list handshakes
        // against the same servers is pure latency (the review's per-turn-handshake finding).
        // Sessions are NOT opened here: a turn that never calls an MCP tool never dials the
        // server. They open lazily at the first `mcp__…` execution below.
        let mut sessions: HashMap<String, McpSession<'_>> = HashMap::new();
        let mut mcp_tools: Vec<(String, Vec<McpTool>)> = Vec::new();
        for (name, client) in &clients {
            let cache_key = format!("{name}@{}", client.url());
            if let Some(tools) = self.mcp_tools_cached(&cache_key) {
                mcp_tools.push((name.clone(), tools));
                continue;
            }
            match client.connect().await {
                Ok(mut session) => match session.list_tools().await {
                    Ok(tools) => {
                        self.mcp_tools_note(&cache_key, tools.clone());
                        mcp_tools.push((name.clone(), tools));
                        // Keep the already-open session — the likeliest tool call target.
                        sessions.insert(name.clone(), session);
                    }
                    Err(e) => tracing::warn!(
                        server = %name,
                        error = %e,
                        "mcp tools/list failed; skipping server this turn"
                    ),
                },
                Err(e) => tracing::warn!(
                    server = %name,
                    error = %e,
                    "mcp connect failed; skipping server this turn"
                ),
            }
        }

        // Feed the meter's "(+N KB tools)" term: the schemas ride every round of this turn's
        // context, so record each server's serialized share (ADR-0017 honest-meter rule).
        if let Some(registry) = &self.mcp {
            for (name, tools) in &mcp_tools {
                let one = [(name.clone(), tools.clone())];
                let bytes = merged_tool_definitions(None, None, &one).to_string().len();
                registry.note_tools_bytes(name, bytes);
            }
        }

        let web_defs = web_access.then(crate::ai::web::tool_definitions);
        let source_defs = (!self.turn_sources.is_empty())
            .then(|| crate::ai::sources::tool_definitions(&self.turn_sources));
        let tools = merged_tool_definitions(web_defs.as_ref(), source_defs.as_ref(), &mcp_tools);
        if tools.as_array().is_none_or(Vec::is_empty) {
            // Web off, no sources attached, every MCP server degraded away: nothing to offer.
            self.count_request();
            return self.ollama.chat_with(options, messages).await;
        }

        let mut convo: Vec<serde_json::Value> = messages
            .iter()
            .map(|m| serde_json::json!({ "role": m.role, "content": m.content }))
            .collect();

        // Summed usage for the budget and the journal, the peak prompt for input truncation
        // (each round re-sends the whole conversation, docs/adr/0037).
        let mut totals = RoundTotals::default();
        let (max_rounds, max_calls) = self.tool_budget;
        for round in 0..max_rounds {
            self.count_request();
            let msg = match self.ollama.chat_tools(options, &convo, Some(&tools)).await {
                Ok((msg, meta)) => {
                    totals.add(meta.usage.clone());
                    if msg
                        .get("tool_calls")
                        .and_then(|c| c.as_array())
                        .is_none_or(Vec::is_empty)
                    {
                        // No (more) tool use — the content is the reply.
                        let text = msg
                            .get("content")
                            .and_then(|c| c.as_str())
                            .unwrap_or_default()
                            .to_string();
                        return Ok((text, totals.finish(meta)));
                    }
                    msg
                }
                // First round only: a model without tool support answers 400 — run the turn as
                // a plain offline call instead of failing it.
                Err(AiError::Backend(detail))
                    if round == 0 && detail.contains("does not support tools") =>
                {
                    tracing::warn!(
                        model = self.ollama.model(),
                        "tools are configured (web access and/or MCP servers) but the model \
                         does not support tool calling; falling back to a plain call"
                    );
                    // The refused round was a request too.
                    totals.add_request();
                    self.count_request();
                    let (text, meta) = self.ollama.chat_with(options, messages).await?;
                    totals.add(meta.usage.clone());
                    return Ok((text, totals.finish(meta)));
                }
                Err(e) => return Err(e),
            };

            let calls = msg
                .get("tool_calls")
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default();

            convo.push(msg.clone());
            for call in calls.iter().take(max_calls) {
                let name = call
                    .pointer("/function/name")
                    .and_then(|n| n.as_str())
                    .unwrap_or_default();
                let args = call
                    .pointer("/function/arguments")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                tracing::info!(tool = name, args = %args, "tool call (ollama loop)");
                // `mcp__<server>__<tool>` routes to that server's live session; `source_*` is a
                // deterministic reference-source leaf (ADR-0021); everything else is a built-in
                // web tool. Every failure path is content, never a turn failure.
                let mut is_error = false;
                let result = match split_mcp_tool_name(name) {
                    Some((server, tool)) => {
                        // Lazily open this server's session on its first call (cache-hit turns
                        // skipped the upfront handshake entirely).
                        if !sessions.contains_key(server) {
                            if let Some((sname, client)) = clients.iter().find(|(n, _)| n == server)
                            {
                                match client.connect().await {
                                    Ok(session) => {
                                        sessions.insert(sname.clone(), session);
                                    }
                                    Err(e) => tracing::warn!(
                                        server = %sname,
                                        error = %e,
                                        "mcp connect failed at call time"
                                    ),
                                }
                            }
                        }
                        match sessions.get_mut(server) {
                            Some(session) => {
                                session.call_tool(tool, &args).await.unwrap_or_else(|e| {
                                    is_error = true;
                                    format!("mcp tool error: {e}")
                                })
                            }
                            // The model invented a server, or that server is unreachable.
                            None => {
                                is_error = true;
                                format!("mcp server '{server}' is not available")
                            }
                        }
                    }
                    // A hallucinated `source_*` call on an unscoped turn still answers as
                    // content ("no reference sources are attached to this idea").
                    None if name.starts_with("source_") => {
                        crate::ai::sources::execute_tool(name, &args, &self.turn_sources).await
                    }
                    None => crate::ai::web::execute_tool(name, &args).await,
                };
                tool_log.push(ToolRecord {
                    round: u32::try_from(round).unwrap_or(u32::MAX),
                    name: name.to_string(),
                    args_sha256: journal::sha256_hex(&args.to_string()),
                    result_text: journal::cap_tool_result(&result),
                    is_error,
                });
                // Tool output is untrusted data (ADR-0039): fenced so an injected "ignore the
                // above" reads as quoted text, never as the owner or the system speaking.
                convo.push(serde_json::json!({
                    "role": "tool",
                    "tool_name": name,
                    "content": fence_untrusted(&format!("tool {name}"), &result),
                }));
            }
        }

        // Rounds exhausted: one final call WITHOUT tools so the model must answer in prose off
        // everything it gathered.
        self.count_request();
        let (msg, meta) = self.ollama.chat_tools(options, &convo, None).await?;
        totals.add(meta.usage.clone());
        let text = msg
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string();
        Ok((text, totals.finish(meta)))
    }

    /// Streaming completion (D11). Terminal on error; aborts its backend when dropped, so a partial
    /// reply is never persisted. On a journaled view the call is recorded once the stream ends
    /// cleanly (docs/adr/0037); an error or a drop records nothing.
    pub async fn chat_stream(&self, messages: Vec<ChatMessage>) -> Result<TokenStream, AiError> {
        let started = Instant::now();
        let s = self.effective_settings();
        let record = self.call_record(&s, &messages);
        let (stream, num_ctx) = match s.backend {
            LlmBackendKind::Ollama => {
                self.refresh_ollama_ctx().await;
                let options = self.ollama_options(&s, &messages);
                self.count_request();
                let stream = self.ollama.chat_stream_with(options, messages).await?;
                (stream, options.num_ctx.and_then(|n| u32::try_from(n).ok()))
            }
            LlmBackendKind::ClaudeCode => {
                self.count_request();
                (self.claude(&s).chat_stream(messages).await?, None)
            }
        };
        let Some(record) = record else {
            return Ok(stream);
        };
        let slot = stream.meta();
        let pending = PendingCall {
            backend: self.clone(),
            record,
            started,
            num_ctx,
            slot: slot.clone(),
        };
        let journaled = futures::stream::unfold(
            (stream, String::new(), Some(pending)),
            |(mut stream, mut text, mut pending)| async move {
                match stream.next().await {
                    Some(Ok(token)) => {
                        text.push_str(&token);
                        Some((Ok(token), (stream, text, pending)))
                    }
                    // Terminal: a failed call is not recorded.
                    Some(Err(e)) => Some((Err(e), (stream, text, None))),
                    None => {
                        if let Some(p) = pending.take() {
                            p.record(&text);
                        }
                        None
                    }
                }
            },
        )
        .boxed();
        Ok(TokenStream::new(journaled, slot))
    }
}

/// A streamed call waiting for its end to be journaled.
struct PendingCall {
    backend: LlmBackend,
    record: CallRecord,
    started: Instant,
    num_ctx: Option<u32>,
    slot: MetaSlot,
}

impl PendingCall {
    /// Journal the finished call and leave its seq in the stream's meta slot.
    fn record(self, text: &str) {
        let mut meta = read_slot(&self.slot).unwrap_or_else(|| CallMeta {
            usage: CallUsage {
                api_calls: 1,
                ..CallUsage::default()
            },
            ..CallMeta::default()
        });
        meta.num_ctx = self.num_ctx;
        meta.ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        meta.journal_seq = self
            .backend
            .journal_call(Some(self.record), text, &meta, Vec::new());
        fill_slot(&self.slot, meta);
    }
}

/// Prefix a tool-loop turn's first message with the fence note ([`FENCE_NOTE`], ADR-0039), so the
/// model knows the fenced tool results that follow are data. The tool loop has no separate system
/// prompt, so it rides where the sources note does. Only tool-loop turns carry it: a plain call
/// has no tool output to fence.
fn with_fence_note(mut messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    if let Some(first) = messages.first_mut() {
        first.content = format!("{FENCE_NOTE}\n\n{}", first.content);
    }
    messages
}

/// Merge the web tool definitions (already Ollama-shaped, or `None` when web access is off) and
/// the reference-source tool definitions (`None` on an unscoped turn, ADR-0021) with every
/// listed MCP server's tools, mangled `mcp__<server>__<tool>` so the executor can route a call
/// back to its server. Merge order: web, sources, MCP. Pure — the per-turn connect/list I/O
/// happens in the caller, so this merge (the part that must be exactly right for the model to
/// call anything) is unit-testable without a network. A tool with no `inputSchema` gets an empty
/// object schema: Ollama rejects a function definition whose `parameters` is `null`.
pub(crate) fn merged_tool_definitions(
    web_defs: Option<&serde_json::Value>,
    source_defs: Option<&serde_json::Value>,
    mcp_tools: &[(String, Vec<McpTool>)],
) -> serde_json::Value {
    let mut defs: Vec<serde_json::Value> = web_defs
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    defs.extend(
        source_defs
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default(),
    );
    for (server, tools) in mcp_tools {
        for tool in tools {
            let parameters = if tool.input_schema.is_null() {
                serde_json::json!({ "type": "object", "properties": {} })
            } else {
                tool.input_schema.clone()
            };
            defs.push(serde_json::json!({
                "type": "function",
                "function": {
                    "name": format!("mcp__{server}__{}", tool.name),
                    "description": tool.description,
                    "parameters": parameters,
                }
            }));
        }
    }
    serde_json::Value::Array(defs)
}

/// Split a mangled MCP tool name back into `(server, tool)`. `None` means "not an MCP name" —
/// the caller falls through to the built-in web tools. The tool part may itself contain `__`
/// (only the FIRST separator after the prefix splits), so arbitrary server-side tool names
/// survive the round trip as long as server names stay `[a-z0-9-]` (enforced by `mcp::add`).
pub(crate) fn split_mcp_tool_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    (!server.is_empty() && !tool.is_empty()).then_some((server, tool))
}

/// Render the enabled servers as the claude CLI's `--mcp-config` JSON (`mcpServers` keyed by
/// name, Streamable-HTTP type, bearer token as an `Authorization` header — omitted entirely when
/// no token is configured). `None` when no server is enabled, so the CLI is spawned without any
/// MCP flags at all. Pure and unit-tested; `claude_code::chat_stream` only writes the string to
/// a temp file.
pub(crate) fn claude_mcp_config_json(servers: &[McpServerConfig]) -> Option<String> {
    if servers.is_empty() {
        return None;
    }
    let mut map = serde_json::Map::new();
    for server in servers {
        let mut entry = serde_json::json!({ "type": "http", "url": server.url });
        if let Some(token) = &server.bearer_token {
            entry["headers"] = serde_json::json!({ "Authorization": format!("Bearer {token}") });
        }
        map.insert(server.name.to_string(), entry);
    }
    Some(serde_json::json!({ "mcpServers": map }).to_string())
}

/// A contract's frontmatter spelling (`ranked_list`, …), so the journal reads like a skill.
fn contract_name(contract: OutputContract) -> String {
    serde_json::to_value(contract)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Name;

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
    fn tool_defs_merge_mangles_names_and_respects_the_enabled_filter() {
        // A real registry on disk: one enabled, one disabled server — only the enabled one's
        // tools may reach the merged definitions (the loop lists `registry.enabled()` only).
        let tmp = tempfile::tempdir().unwrap();
        let registry = McpRegistry::load(tmp.path().join(".mcp-servers.json"));
        registry
            .add(McpServerConfig {
                name: Name::try_from("tracker").unwrap(),
                url: "http://mcp.example/rpc".to_string(),
                bearer_token: None,
                enabled: true,
            })
            .unwrap();
        registry
            .add(McpServerConfig {
                name: Name::try_from("dormant").unwrap(),
                url: "http://mcp.example/off".to_string(),
                bearer_token: None,
                enabled: false,
            })
            .unwrap();

        // Fake the per-server tools/list results for the enabled set (no network in unit tests).
        let listed: Vec<(String, Vec<McpTool>)> = registry
            .enabled()
            .into_iter()
            .map(|server| {
                (
                    server.name.into(),
                    vec![
                        McpTool {
                            name: "list_issues".to_string(),
                            description: "list open issues".to_string(),
                            input_schema: serde_json::json!({
                                "type": "object",
                                "properties": { "label": { "type": "string" } }
                            }),
                        },
                        McpTool {
                            name: "no_schema".to_string(),
                            description: String::new(),
                            input_schema: serde_json::Value::Null,
                        },
                    ],
                )
            })
            .collect();

        let web_defs = crate::ai::web::tool_definitions();
        let merged = merged_tool_definitions(Some(&web_defs), None, &listed);
        let names: Vec<&str> = merged
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d.pointer("/function/name").unwrap().as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "web_search",
                "fetch_url",
                "mcp__tracker__list_issues",
                "mcp__tracker__no_schema",
            ],
            "web defs first, then mangled MCP tools; the disabled server contributes nothing"
        );
        // Schemas pass through; a null schema degrades to an empty object schema (Ollama rejects
        // null `parameters`).
        assert_eq!(
            merged[2].pointer("/function/parameters/properties/label/type"),
            Some(&serde_json::json!("string"))
        );
        assert_eq!(
            merged[3].pointer("/function/parameters/type"),
            Some(&serde_json::json!("object"))
        );

        // With web access off, only the MCP tools remain.
        let mcp_only = merged_tool_definitions(None, None, &listed);
        assert_eq!(mcp_only.as_array().unwrap().len(), 2);
        // With none of the three, the merge is empty (the loop degrades to a plain call).
        assert_eq!(
            merged_tool_definitions(None, None, &[]),
            serde_json::json!([])
        );
    }

    fn turn_sources_fixture() -> Vec<ResolvedSource> {
        vec![ResolvedSource {
            name: Name::try_from("rf-docs").unwrap(),
            root: std::path::PathBuf::from("/mnt/sources/rf-docs"),
        }]
    }

    #[test]
    fn source_probe_follows_the_turn_scope() {
        let shared = test_backend();
        assert!(shared.source_probe().is_empty());
        assert!(!shared
            .with_turn_sources(turn_sources_fixture())
            .source_probe()
            .is_empty());
    }

    #[test]
    fn merge_order_is_web_then_sources_then_mcp() {
        let web_defs = crate::ai::web::tool_definitions();
        let source_defs = crate::ai::sources::tool_definitions(&turn_sources_fixture());
        let listed = vec![(
            "tracker".to_string(),
            vec![McpTool {
                name: "list_issues".to_string(),
                description: String::new(),
                input_schema: serde_json::Value::Null,
            }],
        )];
        let merged = merged_tool_definitions(Some(&web_defs), Some(&source_defs), &listed);
        let names: Vec<&str> = merged
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d.pointer("/function/name").unwrap().as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "web_search",
                "fetch_url",
                "source_list",
                "source_grep",
                "source_read",
                "mcp__tracker__list_issues",
            ]
        );
    }

    #[test]
    fn scoped_clone_carries_sources_and_leaves_the_shared_instance_untouched() {
        let shared = test_backend();
        assert!(shared.turn_sources.is_empty());

        let scoped = shared.with_turn_sources(turn_sources_fixture());
        assert_eq!(scoped.turn_sources.len(), 1);
        assert_eq!(scoped.turn_sources[0].name, "rf-docs");
        // The shared instance stays source-free — every unscoped turn is unchanged.
        assert!(shared.turn_sources.is_empty());

        // The settings lock is genuinely shared: a Settings edit through the scoped clone is
        // live on the shared instance too (the existing live-tuning contract survives scoping).
        let mut s = scoped.settings();
        s.temperature = 1.3;
        scoped.set_settings(s);
        assert_eq!(shared.settings().temperature, 1.3);
    }

    #[test]
    fn claude_foil_runs_in_the_turn_dir_with_the_live_web_toggle() {
        let b = test_backend();
        let mut s = b.settings();
        s.backend = LlmBackendKind::ClaudeCode;
        s.web_access = true;
        b.set_settings(s);

        // The shared instance keeps the configured base cwd; an idea turn runs in its folder.
        assert_eq!(b.claude_config().cwd, std::path::PathBuf::from("."));
        let idea = b.with_turn_dir(std::path::PathBuf::from("/vault/my-idea"));
        let cfg = idea.claude_config();
        assert_eq!(cfg.cwd, std::path::PathBuf::from("/vault/my-idea"));
        assert!(cfg.web_access);
        assert!(b.turn_dir.is_none(), "the shared instance is untouched");

        let mut s = b.settings();
        s.web_access = false;
        b.set_settings(s);
        assert!(!idea.claude_config().web_access, "the toggle is live");
    }

    #[test]
    fn claude_config_gains_add_dirs_and_hint_only_when_scoped() {
        let b = test_backend();
        let mut s = b.settings();
        s.backend = LlmBackendKind::ClaudeCode;
        b.set_settings(s);

        // Unscoped: no source dirs, no sources hint.
        let cfg = b.claude_config();
        assert!(cfg.add_dirs.is_empty());
        assert!(!cfg.system_prompt.unwrap_or_default().contains("rf-docs"));

        // Scoped: the resolved root rides --add-dir and the hint names the source. Duplicate
        // roots are not pushed twice.
        let scoped = b.with_turn_sources(turn_sources_fixture());
        let cfg = scoped.claude_config();
        assert_eq!(
            cfg.add_dirs,
            [std::path::PathBuf::from("/mnt/sources/rf-docs")]
        );
        let prompt = cfg.system_prompt.expect("sources hint rendered");
        assert!(prompt.contains("rf-docs -> /mnt/sources/rf-docs"));
        assert!(prompt.contains("Never modify them"));
    }

    #[test]
    fn sources_note_prepends_only_on_scoped_turns() {
        let messages = || {
            vec![ChatMessage {
                role: "user".to_string(),
                content: "the prompt".to_string(),
            }]
        };
        let b = test_backend();
        assert_eq!(
            b.with_sources_note(messages())[0].content,
            "the prompt",
            "unscoped turns are byte-identical to today"
        );

        let scoped = b.with_turn_sources(turn_sources_fixture());
        let noted = scoped.with_sources_note(messages());
        assert!(noted[0]
            .content
            .starts_with("Attached reference sources for this idea: rf-docs."));
        assert!(noted[0].content.ends_with("the prompt"));
    }

    #[test]
    fn tool_context_bytes_counts_source_schemas_on_scoped_ollama_turns() {
        let b = test_backend();
        let unscoped = b.tool_context_bytes();
        let scoped = b.with_turn_sources(turn_sources_fixture());
        let expected = crate::ai::sources::tool_definitions(&scoped.turn_sources)
            .to_string()
            .len();
        assert_eq!(scoped.tool_context_bytes(), unscoped + expected);
    }

    #[test]
    fn mcp_tool_names_split_back_to_server_and_tool() {
        assert_eq!(
            split_mcp_tool_name("mcp__tracker__list_issues"),
            Some(("tracker", "list_issues"))
        );
        // Only the first separator after the prefix splits — tool names may contain `__`.
        assert_eq!(
            split_mcp_tool_name("mcp__s__weird__tool"),
            Some(("s", "weird__tool"))
        );
        // Non-MCP and malformed names fall through to the web-tool executor.
        assert_eq!(split_mcp_tool_name("web_search"), None);
        assert_eq!(split_mcp_tool_name("mcp__noseparator"), None);
        assert_eq!(split_mcp_tool_name("mcp____tool"), None);
        assert_eq!(split_mcp_tool_name("mcp__server__"), None);
    }

    #[test]
    fn claude_mcp_config_serializes_servers_and_omits_empty_headers() {
        assert_eq!(claude_mcp_config_json(&[]), None, "no servers, no flags");

        let servers = vec![
            McpServerConfig {
                name: Name::try_from("open").unwrap(),
                url: "http://mcp.example/a".to_string(),
                bearer_token: None,
                enabled: true,
            },
            McpServerConfig {
                name: Name::try_from("locked").unwrap(),
                url: "https://mcp.example/b".to_string(),
                bearer_token: Some("tok".to_string()),
                enabled: true,
            },
        ];
        let json = claude_mcp_config_json(&servers).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["mcpServers"]["open"]["type"], "http");
        assert_eq!(v["mcpServers"]["open"]["url"], "http://mcp.example/a");
        assert!(
            v["mcpServers"]["open"].get("headers").is_none(),
            "no token ⇒ no Authorization header at all"
        );
        assert_eq!(
            v["mcpServers"]["locked"]["headers"]["Authorization"],
            "Bearer tok"
        );
    }

    #[test]
    fn claude_client_gains_mcp_config_and_tool_prefix_allows() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = McpRegistry::load(tmp.path().join(".mcp-servers.json"));
        registry
            .add(McpServerConfig {
                name: Name::try_from("tracker").unwrap(),
                url: "http://mcp.example/rpc".to_string(),
                bearer_token: None,
                enabled: true,
            })
            .unwrap();

        let b = test_backend().with_mcp(Arc::new(registry));
        let mut s = b.settings();
        s.backend = LlmBackendKind::ClaudeCode;
        b.set_settings(s);

        // The per-call claude config carries the rendered --mcp-config JSON and the tool-prefix
        // allow for every enabled server.
        let cfg = b.claude_config();
        let json = cfg.mcp_config_json.expect("mcp config rendered");
        assert!(json.contains("\"tracker\""));
        assert!(cfg.allowed_tools.iter().any(|t| t == "mcp__tracker"));

        // Toggling the server off is live: the next call composes a config with no MCP at all.
        b.mcp
            .as_ref()
            .unwrap()
            .set_enabled("tracker", false)
            .unwrap();
        let cfg = b.claude_config();
        assert_eq!(cfg.mcp_config_json, None);
        assert!(!cfg.allowed_tools.iter().any(|t| t == "mcp__tracker"));
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

    fn profile(temperature: f32, claude_model: &str, claude_effort: &str) -> RoleProfile {
        RoleProfile {
            temperature,
            claude_model: claude_model.to_string(),
            claude_effort: claude_effort.to_string(),
        }
    }

    fn role_tuned_backend() -> LlmBackend {
        let b = test_backend();
        let mut s = b.settings();
        s.temperature = 0.7;
        s.claude_model = "sonnet".to_string();
        s.claude_effort = "low".to_string();
        s.role_tuning = true;
        s.role_profiles = BTreeMap::from([
            ("harvester".to_string(), profile(0.2, "", "")),
            ("auditor".to_string(), profile(0.3, "opus", "high")),
        ]);
        b.set_settings(s);
        b
    }

    fn user_turn() -> Vec<ChatMessage> {
        vec![ChatMessage {
            role: "user".to_string(),
            content: "hi".to_string(),
        }]
    }

    #[test]
    fn a_role_scoped_call_samples_at_its_role_temperature() {
        let b = role_tuned_backend();
        let harvester = b.for_role("harvester");
        let opts = harvester.ollama_options(&harvester.effective_settings(), &user_turn());
        assert_eq!(opts.temperature, Some(0.2));
        assert_eq!(
            opts.num_ctx,
            Some(b.context_window_tokens()),
            "a role call's window equals the budget prompts were sized against"
        );

        let opts = b.ollama_options(&b.effective_settings(), &user_turn());
        assert_eq!(
            opts.temperature,
            Some(0.7),
            "the shared instance stays global"
        );
    }

    #[test]
    fn role_tuning_off_or_an_unprofiled_role_keeps_the_global_settings() {
        let b = role_tuned_backend();
        assert_eq!(b.for_role("critic").effective_settings().temperature, 0.7);

        let mut s = b.settings();
        s.role_tuning = false;
        b.set_settings(s);
        assert_eq!(
            b.for_role("harvester").effective_settings().temperature,
            0.7
        );
    }

    #[test]
    fn a_role_profile_sets_the_claude_model_and_effort_and_blank_inherits() {
        let b = role_tuned_backend();
        let mut s = b.settings();
        s.backend = LlmBackendKind::ClaudeCode;
        b.set_settings(s);

        let cfg = b.for_role("auditor").claude_config();
        assert_eq!(cfg.model.as_deref(), Some("opus"));
        assert!(cfg
            .system_prompt
            .as_deref()
            .is_some_and(|p| p.contains("Reasoning effort: high")));

        let cfg = b.for_role("harvester").claude_config();
        assert_eq!(cfg.model.as_deref(), Some("sonnet"));
        assert!(cfg
            .system_prompt
            .as_deref()
            .is_some_and(|p| p.contains("Reasoning effort: low")));
    }

    #[test]
    fn claude_window_is_the_smallest_across_role_model_overrides() {
        let b = role_tuned_backend();
        let mut s = b.settings();
        s.backend = LlmBackendKind::ClaudeCode;
        s.claude_model = "opus[1m]".to_string();
        b.set_settings(s.clone());
        assert_eq!(
            b.context_window_tokens(),
            200_000,
            "the auditor's plain opus bounds the budget"
        );

        s.role_tuning = false;
        b.set_settings(s.clone());
        assert_eq!(b.context_window_tokens(), 1_000_000);

        s.role_tuning = true;
        s.role_profiles
            .get_mut("auditor")
            .unwrap()
            .claude_model
            .clear();
        b.set_settings(s);
        assert_eq!(
            b.context_window_tokens(),
            1_000_000,
            "a blank-model profile inherits the global model, not the 200k default"
        );
    }

    /// A minimal Ollama stand-in whose every `/api/chat` answer asks for three `source_list`
    /// calls; returns its URL and the raw chat bodies it received. `/api/show` answers 404.
    async fn tool_hungry_ollama() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = bodies.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 4096];
                    let body_at = loop {
                        let n = sock.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        req.extend_from_slice(&buf[..n]);
                        if let Some(i) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&req[..body_at]).to_lowercase();
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    while req.len() < body_at + len {
                        let n = sock.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        req.extend_from_slice(&buf[..n]);
                    }
                    if !head.starts_with("post /api/chat") {
                        let _ = sock
                            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                            .await;
                        return;
                    }
                    seen.lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&req[body_at..]).into_owned());
                    let call =
                        serde_json::json!({"function": {"name": "source_list", "arguments": {}}});
                    let payload = serde_json::json!({
                        "message": {"role": "assistant", "content": "", "tool_calls": [call, call, call]},
                        "done": true,
                    })
                    .to_string();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        payload.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        (url, bodies)
    }

    #[tokio::test]
    async fn with_tool_budget_clamps_and_limits_rounds() {
        let shared = test_backend();
        assert_eq!(shared.tool_budget(), (MAX_TOOL_ROUNDS, MAX_CALLS_PER_ROUND));
        assert_eq!(shared.with_tool_budget(9, 9).tool_budget(), (4, 3));
        assert_eq!(shared.with_tool_budget(0, 0).tool_budget(), (1, 1));
        let narrowed = shared.with_tool_budget(2, 2);
        assert_eq!(narrowed.for_role("researcher").tool_budget(), (2, 2));
        assert_eq!(
            shared.tool_budget(),
            (4, 3),
            "the shared instance is untouched"
        );

        // Each round the model asks for three calls; the loop runs `rounds` tool rounds, executes
        // `calls` of them per round, then one forced tool-free call.
        let (url, bodies) = tool_hungry_ollama().await;
        let backend = LlmBackend::ollama_only(OllamaClient::new(&url, "llama3.2").unwrap())
            .with_turn_sources(turn_sources_fixture());
        let user = vec![ChatMessage {
            role: "user".into(),
            content: "map it".into(),
        }];
        backend
            .with_tool_budget(2, 2)
            .chat(user.clone())
            .await
            .unwrap();
        let sent = bodies.lock().unwrap().clone();
        assert_eq!(sent.len(), 3, "2 tool rounds + 1 forced answer");
        let executed = sent[2].matches("\"role\":\"tool\"").count();
        assert_eq!(executed, 4, "2 calls in each of 2 rounds");
        bodies.lock().unwrap().clear();
        backend.chat(user).await.unwrap();
        assert_eq!(
            bodies.lock().unwrap().len(),
            MAX_TOOL_ROUNDS + 1,
            "the default is the global bound"
        );
    }
}
