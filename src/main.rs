//! Binary entry point: the D25 boot sequence (docs/01-architecture.md).
//!
//! Load config → ensure the vault dir → open the index (create if missing) → reindex if drifted →
//! spawn a non-blocking LLM-backend probe (absence is valid, D20) → build state + router → bind and
//! serve. Boot must never block on the model and must not crash on a reindex error. A `import
//! <dir>` subcommand runs the Obsidian importer instead of the server (docs/adr/0009).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use idea_vault::ai::claude_code::ClaudeCodeConfig;
use idea_vault::ai::{ClaudeCodeClient, LlmBackend, OllamaClient};
use idea_vault::app::{build_router, AppState};
use idea_vault::config::{ClaudeSettings, Config, LlmBackendKind};
use idea_vault::{import, index, vault};
use tokio::sync::Semaphore;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // 1. Config.
    let config = Config::from_env();

    // Subcommand: `idea-vault import <source-dir>` bulk-imports flat markdown notes as ideas and
    // exits, without starting the server (docs/adr/0009 Phase 3).
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("import") {
        let Some(source) = args.get(2) else {
            anyhow::bail!("usage: idea-vault import <source-dir>");
        };
        let summary = import::import_dir(
            &PathBuf::from(source),
            &config.vault_dir,
            &config.index_path,
        )
        .with_context(|| format!("importing markdown from {source}"))?;
        println!(
            "imported {} new idea(s), skipped {} existing, {} unreadable — vault: {}",
            summary.imported,
            summary.skipped,
            summary.errored,
            config.vault_dir.display()
        );
        return Ok(());
    }

    // 2. Vault dir (source of truth).
    vault::ensure_vault_dir(&config.vault_dir)
        .with_context(|| format!("ensuring vault dir {}", config.vault_dir.display()))?;

    // 3. Open (or create) the derived index.
    let mut conn = index::schema::open_or_create(&config.index_path)
        .with_context(|| format!("opening index {}", config.index_path.display()))?;

    // 4. Reindex if the index has drifted from the vault (scaffold check_drift returns false).
    match index::reindex::check_drift(&conn, &config.vault_dir) {
        Ok(true) => match index::reindex::reindex(&mut conn, &config.vault_dir) {
            Ok(counts) => tracing::info!(
                ideas = counts.ideas,
                facts = counts.facts,
                links = counts.links,
                "reindex complete"
            ),
            Err(e) => {
                tracing::warn!(error = %e, "reindex failed at boot; continuing with existing index")
            }
        },
        Ok(false) => tracing::debug!("index fresh; no reindex needed"),
        Err(e) => tracing::warn!(error = %e, "drift check failed; continuing without reindex"),
    }

    // 5. LLM backend + non-blocking health probe (boot must not wait on the model — D20/D25).
    let llm = build_llm(&config)?;
    tracing::info!(backend = %backend_label(&config), model = llm.model(), "llm backend selected");
    {
        let probe_backend = llm.clone();
        tokio::spawn(async move {
            let health = probe_backend.probe().await;
            tracing::info!(?health, "llm boot probe");
        });
    }

    // 6. Shared state.
    let ai_concurrency = config.ai_concurrency.max(1);
    let bind = config.bind.clone();
    let skills = Arc::new(idea_vault::concepts::skills::SkillRegistry::builtin());
    let state = AppState {
        config: Arc::new(config),
        db: Arc::new(Mutex::new(conn)),
        llm,
        ai_semaphore: Arc::new(Semaphore::new(ai_concurrency)),
        skills,
    };

    // 7. Router + serve.
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    tracing::info!(address = %listener.local_addr()?, "idea-vault serving");
    axum::serve(listener, app).await.context("axum serve")?;

    Ok(())
}

/// A short label for the active backend, for the boot log.
fn backend_label(config: &Config) -> &'static str {
    match config.llm_backend {
        LlmBackendKind::Ollama => "ollama",
        LlmBackendKind::ClaudeCode => "claude-code",
    }
}

/// Construct the LLM backend selected in config (docs/adr/0009).
fn build_llm(config: &Config) -> anyhow::Result<LlmBackend> {
    match config.llm_backend {
        LlmBackendKind::Ollama => {
            let client = OllamaClient::new(config.ollama_url.clone(), config.ollama_model.clone())
                .context("failed to build the Ollama HTTP client (check proxy/TLS)")?
                .with_token_timeout(config.ollama_timeout);
            Ok(LlmBackend::Ollama(client))
        }
        LlmBackendKind::ClaudeCode => {
            let ClaudeSettings {
                binary,
                cwd,
                add_dirs,
                allowed_tools,
                model,
                skip_permissions,
                timeout,
            } = config.claude.clone();
            let system_prompt = claude_system_prompt(&add_dirs);
            Ok(LlmBackend::ClaudeCode(ClaudeCodeClient::new(
                ClaudeCodeConfig {
                    binary,
                    cwd,
                    add_dirs,
                    allowed_tools,
                    model,
                    system_prompt,
                    skip_permissions,
                    token_timeout: timeout,
                },
            )))
        }
    }
}

/// A system-prompt addendum telling the agentic foil where the owner's reference material lives, so
/// it knows to grep/read those dirs when interrogating an idea (docs/adr/0009 Phase 2).
fn claude_system_prompt(add_dirs: &[PathBuf]) -> Option<String> {
    if add_dirs.is_empty() {
        return None;
    }
    let dirs = add_dirs
        .iter()
        .map(|d| d.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "You are a rigorous ideation foil. The owner's reference material (markdown notes, \
         Obsidian vault, prior Claude Code artifacts) lives at: {dirs}. When it helps interrogate \
         the idea, Grep/Read those directories for relevant prior thinking and cite what you find. \
         Do not modify the owner's files unless they explicitly ask."
    ))
}
