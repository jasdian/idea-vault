//! Agent-role tests against the mock Ollama (docs/06-concepts/agents.md): each standard role's
//! persona reaches the model, the skill lens hydrates, failures surface as errors for the
//! orchestrator to null-out (D14), and nothing touches the vault. No live model.

mod support;

use std::sync::Arc;

use idea_vault::ai::{LlmBackend, OllamaClient};
use idea_vault::concepts::agents::{run_agent, AgentRole, AgentTask};
use idea_vault::concepts::skills::SkillRegistry;
use idea_vault::concepts::ConceptError;
use support::{refused_url, spawn, ChatScript};
use tokio::sync::Semaphore;

async fn run_role(role: AgentRole, skill: Option<&str>) -> (String, Vec<String>) {
    let mock = spawn(
        &["llama3.2"],
        ChatScript::Tokens(vec![format!("{} output", role.as_str())]),
    )
    .await;
    let client = LlmBackend::Ollama(OllamaClient::new(mock.url.clone(), "llama3.2").unwrap());
    let semaphore = Arc::new(Semaphore::new(2));
    let registry = SkillRegistry::builtin();

    let result = run_agent(
        &client,
        &semaphore,
        &registry,
        AgentTask {
            role,
            skill: skill.map(str::to_string),
            context: "BUDGETED-CONTEXT-BLOCK".to_string(),
        },
    )
    .await
    .unwrap();
    assert_eq!(result.role, role);
    (result.content, mock.chat_bodies())
}

#[tokio::test]
async fn each_standard_role_sends_its_persona_and_returns_its_result() {
    for (role, marker) in [
        (AgentRole::Critic, "You are the Critic"),
        (AgentRole::Researcher, "You are the Researcher"),
        (AgentRole::Synthesizer, "You are the Synthesizer"),
    ] {
        let (content, bodies) = run_role(role, None).await;
        assert_eq!(content, format!("{} output", role.as_str()));
        assert_eq!(bodies.len(), 1);
        assert!(bodies[0].contains(marker), "persona for {role:?} sent");
        assert!(
            bodies[0].contains("BUDGETED-CONTEXT-BLOCK"),
            "context for {role:?} sent"
        );
    }
}

#[tokio::test]
async fn skill_lens_hydrates_into_the_agent_prompt() {
    let (_, bodies) = run_role(AgentRole::Critic, Some("cheapest-disproof")).await;
    assert!(bodies[0].contains("You are the Critic"));
    assert!(
        bodies[0].contains("cheapest, fastest test"),
        "skill template present"
    );
    assert!(bodies[0].contains("BUDGETED-CONTEXT-BLOCK"));
}

#[tokio::test]
async fn failed_agent_surfaces_an_error_for_the_judge_to_skip() {
    let client = LlmBackend::Ollama(OllamaClient::new(refused_url().await, "llama3.2").unwrap());
    let semaphore = Arc::new(Semaphore::new(1));
    let registry = SkillRegistry::builtin();

    let result = run_agent(
        &client,
        &semaphore,
        &registry,
        AgentTask {
            role: AgentRole::Critic,
            skill: None,
            context: "ctx".to_string(),
        },
    )
    .await;
    assert!(matches!(result, Err(ConceptError::Ai(_))));
}
