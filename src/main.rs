//! Binary entry point: the D25 boot sequence (docs/01-architecture.md).
//!
//! Load config → ensure the vault dir → open the index (create if missing) → reindex if drifted →
//! spawn a non-blocking Ollama probe (absence is valid, D20) → build state + router → bind and
//! serve. Boot must never block on Ollama and must not crash on a reindex error.

use std::sync::{Arc, Mutex};

use anyhow::Context;
use idea_vault::ai::OllamaClient;
use idea_vault::app::{build_router, AppState};
use idea_vault::config::Config;
use idea_vault::{index, vault};
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

    // 5. AI client + non-blocking health probe (boot must not wait on Ollama — D20/D25).
    let ollama = OllamaClient::new(config.ollama_url.clone(), config.ollama_model.clone())
        .context("failed to build the Ollama HTTP client (check proxy/TLS environment)")?
        .with_token_timeout(config.ollama_timeout);
    {
        let probe_client = ollama.clone();
        tokio::spawn(async move {
            let health = probe_client.probe().await;
            tracing::info!(?health, "ollama boot probe");
        });
    }

    // 6. Shared state.
    let ai_concurrency = config.ai_concurrency.max(1);
    let bind = config.bind.clone();
    let skills = Arc::new(idea_vault::concepts::skills::SkillRegistry::builtin());
    let state = AppState {
        config: Arc::new(config),
        db: Arc::new(Mutex::new(conn)),
        ollama,
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
