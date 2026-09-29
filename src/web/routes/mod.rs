pub mod admin;
pub mod artifacts;
pub mod chat;
pub mod compact;
pub mod ideas;
pub mod mcp;
pub mod memory;
pub mod settings;
pub mod skills;
pub mod sources;

use crate::web::state::AppState;

// The byte budget for one AI prompt (D21) is no longer a constant here: every route reads the
// live, backend/model-derived `state.llm.context_budget()` (ADR-0014), so chat, store, reopen,
// the meter, and `memory::compact`'s fold targets all derive from the same single source.

/// The LLM view for one idea's turn: the shared backend scoped to the idea's attached reference
/// sources (frontmatter `sources:`, resolved through the registry — ADR-0021), so the model call,
/// the context budget, and the meter all agree on what rides the window.
///
/// A turn must never fail because a sources lookup did: on a read error, or when the idea has no
/// attach list, this degrades to the unscoped shared backend — the same warn-and-drop discipline
/// `SourceRegistry::resolve_attached` applies to a stale name. Worst case the foil answers
/// without its reference material, never not at all.
pub(crate) fn scoped_llm(state: &AppState, slug: &str) -> crate::ai::LlmBackend {
    match crate::vault::store::read_idea(&state.config.vault_dir, slug) {
        Ok(idea) if !idea.frontmatter.sources.is_empty() => state
            .llm
            .with_turn_sources(state.sources.resolve_attached(&idea.frontmatter.sources)),
        Ok(_) => state.llm.clone(),
        Err(e) => {
            tracing::warn!(slug, error = %e, "sources lookup failed; running the turn unscoped");
            state.llm.clone()
        }
    }
}

/// The related-ideas block for `slug` in at most `allowance` bytes (`memory::related`), or `""`.
///
/// Best-effort context: an index failure or a poisoned lock yields no block, never a failed
/// turn. Sync on purpose, so the index guard cannot live across an `.await`; this is also the
/// body of every [`crate::concepts::skills::RelatedProvider`] the routes hand to `concepts`.
pub(crate) fn related_block_logged(state: &AppState, slug: &str, allowance: usize) -> String {
    if allowance == 0 {
        return String::new();
    }
    let conn = match state.db.lock() {
        Ok(conn) => conn,
        Err(e) => {
            tracing::warn!(slug = %slug, error = %e, "db mutex poisoned; no related-ideas block");
            return String::new();
        }
    };
    match crate::memory::related::related_block(&conn, slug, allowance) {
        Ok(block) => block,
        Err(e) => {
            tracing::warn!(slug = %slug, error = %e, "related-ideas block skipped");
            String::new()
        }
    }
}

/// Rebuild the index, logging instead of failing the request — markdown truth already landed
/// and the next reindex reconciles (docs/03 "Consistency & failure model").
pub(crate) fn reindex_logged(state: &AppState) {
    match state.db.lock() {
        Ok(mut conn) => {
            if let Err(e) = crate::index::reindex::reindex(&mut conn, &state.config.vault_dir) {
                tracing::warn!(error = %e, "reindex after vault write failed; truth intact");
            }
        }
        Err(e) => tracing::warn!(error = %e, "db mutex poisoned; skipping reindex"),
    }
}

/// [`reindex_logged`] without the empty-vault guard (ADR-0019).
///
/// Only for a caller that has already proven the vault is real *by successfully mutating it*.
/// Today that is exactly `ideas::delete_idea`: it 404s unless `store::delete_idea` returned true,
/// so reaching the rebuild proves the idea folder existed and the vault was writable — neither of
/// which a ghost mount can fake. Deleting the last idea is the one legitimate way to reach
/// "vault empty, index populated", and the guard must not strand the deleted idea in the list.
pub(crate) fn reindex_logged_forced(state: &AppState) {
    match state.db.lock() {
        Ok(mut conn) => {
            if let Err(e) =
                crate::index::reindex::reindex_forced(&mut conn, &state.config.vault_dir)
            {
                tracing::warn!(error = %e, "reindex after delete failed; truth intact");
            }
        }
        Err(e) => tracing::warn!(error = %e, "db mutex poisoned; skipping reindex"),
    }
}

#[cfg(test)]
mod tests {
    use super::scoped_llm;
    use crate::sources::SourceConfig;
    use crate::web::state::AppState;
    use std::sync::{Arc, Mutex};

    /// The smallest real `AppState` [`scoped_llm`] can run against: temp vault + registry, an
    /// in-memory index, and an Ollama backend pointed at a dead loopback port (never dialed —
    /// `scoped_llm` is sync). The tempdir is leaked for the process lifetime, like the
    /// integration harness does.
    fn state_with_vault() -> (AppState, std::path::PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let vault_dir = tmp.path().join("vault");
        std::fs::create_dir_all(&vault_dir).expect("vault dir");
        let config = crate::config::Config {
            bind: "127.0.0.1:0".to_string(),
            vault_dir: vault_dir.clone(),
            index_path: tmp.path().join("index.db"),
            ollama_url: "http://127.0.0.1:9".to_string(),
            ollama_model: "llama3.2".to_string(),
            ai_concurrency: 1,
            ollama_timeout: std::time::Duration::from_secs(5),
            ollama_temperature: 0.7,
            llm_backend: crate::ai::LlmBackendKind::Ollama,
            claude: crate::config::ClaudeSettings {
                binary: "claude".to_string(),
                cwd: vault_dir.clone(),
                add_dirs: Vec::new(),
                allowed_tools: Vec::new(),
                model: None,
                skip_permissions: true,
                timeout: std::time::Duration::from_secs(5),
                effort: "high".to_string(),
            },
            auto_compact: true,
            compact_threshold: 0.80,
            ollama_ctx_tokens: 0,
            claude_ctx_tokens: 0,
            web_access: false,
            audit_findings: true,
            mcp_config_path: tmp.path().join(".mcp-servers.json"),
            sources_config_path: tmp.path().join(".sources.json"),
            sources_dir: None,
            sources_applied: None,
            skills_dir: vault_dir.join(".skills"),
            mcp_server_token: None,
        };
        let ollama =
            crate::ai::OllamaClient::new(config.ollama_url.clone(), config.ollama_model.clone())
                .expect("build ollama client");
        let sources = Arc::new(crate::sources::SourceRegistry::load(
            config.sources_config_path.clone(),
            tmp.path().join(crate::sources::OVERRIDE_FILENAME),
            None,
            None,
        ));
        let skills = Arc::new(crate::concepts::skills::LiveSkills::load(
            config.skills_dir.clone(),
        ));
        let state = AppState {
            config: Arc::new(config),
            db: Arc::new(Mutex::new(
                rusqlite::Connection::open_in_memory().expect("in-memory db"),
            )),
            llm: crate::ai::LlmBackend::ollama_only(ollama),
            ai_semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
            skills,
            jobs: crate::web::jobs::new_registry(),
            queues: crate::web::jobs::new_queues(),
            mcp: Arc::new(crate::mcp::McpRegistry::load(
                tmp.path().join(".mcp-servers.json"),
            )),
            sources,
        };
        std::mem::forget(tmp);
        (state, vault_dir)
    }

    fn seed_idea(vault_dir: &std::path::Path, slug: &str, sources: Vec<String>) {
        use chrono::{TimeZone as _, Utc};
        crate::vault::store::write_idea(
            vault_dir,
            &crate::domain::Idea {
                frontmatter: crate::domain::IdeaFrontmatter {
                    title: "Scoped".into(),
                    slug: slug.into(),
                    state: crate::domain::IdeaState::InDiscussion,
                    tags: vec![],
                    sources,
                    created: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                    updated: Utc.with_ymd_and_hms(2026, 7, 7, 10, 0, 0).unwrap(),
                },
                body: "body\n".into(),
            },
        )
        .unwrap();
    }

    #[test]
    fn scoped_llm_attaches_resolved_sources_for_an_idea_with_an_attach_list() {
        let (state, vault_dir) = state_with_vault();
        let src_dir = tempfile::tempdir().unwrap();
        state
            .sources
            .add(SourceConfig {
                name: crate::domain::Name::try_from("refs").unwrap(),
                host_path: src_dir.path().to_path_buf(),
            })
            .unwrap();
        seed_idea(&vault_dir, "sourced", vec!["refs".into()]);

        // The scoped clone carries the source tool schemas on the Ollama path; the shared
        // instance stays source-free (its meter term is the observable difference).
        let scoped = scoped_llm(&state, "sourced");
        assert!(
            scoped.tool_context_bytes() > state.llm.tool_context_bytes(),
            "the scoped view must count the source-schema bytes"
        );
    }

    #[test]
    fn scoped_llm_degrades_to_unscoped_for_a_missing_idea() {
        // The Err branch (idea deleted under a live job, unparsable idea.md): the turn runs
        // unscoped, it does not fail — even with sources sitting in the registry.
        let (state, _vault_dir) = state_with_vault();
        let src_dir = tempfile::tempdir().unwrap();
        state
            .sources
            .add(SourceConfig {
                name: crate::domain::Name::try_from("refs").unwrap(),
                host_path: src_dir.path().to_path_buf(),
            })
            .unwrap();

        let llm = scoped_llm(&state, "ghost");
        assert_eq!(llm.tool_context_bytes(), state.llm.tool_context_bytes());
        assert_eq!(
            llm.tool_context_bytes(),
            0,
            "no source schemas ride the turn"
        );
    }

    #[test]
    fn scoped_llm_with_an_empty_attach_list_is_unscoped() {
        let (state, vault_dir) = state_with_vault();
        seed_idea(&vault_dir, "plain", vec![]);
        let llm = scoped_llm(&state, "plain");
        assert_eq!(llm.tool_context_bytes(), state.llm.tool_context_bytes());
    }
}
