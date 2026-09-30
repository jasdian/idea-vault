//! `rmcp::ServerHandler` for idea-vault's inbound MCP surface (docs/adr/0024).
//!
//! `IdeaVaultMcpServer` is cloned once per rmcp session by the `StreamableHttpService` factory
//! (`mod.rs`), like the sibling `mcp-server` repo's `CosmicMcpServer` — but unlike that
//! multi-tenant server, no per-caller identity is resolved here: the `AuthLayer` gate (`auth.rs`)
//! is a single-token boolean check, not per-user credentials, so `call_tool` never needs to pull
//! anything out of `context.extensions`. `tasks` is the one piece of real session-spanning state
//! (the task_id → idea-slug map), held behind an `Arc` so every clone shares the same registry.

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, CancelTaskParams, CancelTaskResult, CreateTaskResult,
    GetPromptRequestParams, GetPromptResult, GetTaskInfoParams, GetTaskPayloadResult,
    GetTaskResult, GetTaskResultParams, ListPromptsResult, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::ErrorData as McpError;
use rmcp::ServerHandler;

use crate::web::state::AppState;

use super::tasks::TaskRegistry;
use super::{prompts, tools};

#[derive(Clone)]
pub struct IdeaVaultMcpServer {
    state: AppState,
    tasks: Arc<TaskRegistry>,
}

impl IdeaVaultMcpServer {
    pub fn new(state: AppState) -> Self {
        Self {
            state,
            tasks: Arc::new(TaskRegistry::new()),
        }
    }
}

impl ServerHandler for IdeaVaultMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .build(),
        )
        .with_instructions(
            "idea-vault: a localhost ideation vault. list_ideas/get_idea/get_artifact/search \
             read the vault; list_skills reads the skill book; create_idea starts a new Draft. \
             The foil is idea-vault's own model: a chat message is saved as the owner's turn and \
             the foil answers it, so relay the owner's words rather than arguing in their place. \
             chat, run_skill, run_swarm, store_idea and build_plan run model turns and can take \
             a while — prefer calling them with task:{} and polling tasks/get, then \
             tasks/result once complete. Called plainly, they wait a few seconds and otherwise \
             answer with a 'still running' note: call again with the same arguments to collect \
             the result; a retry after the result was served replays it rather than running \
             again (pass a fresh idempotency_key to force a new run). build_plan writes a new \
             plan version; get_plan reads its open questions and owner-blocked tasks; \
             answer_plan records the owner's answers as a new version with no model call. \
             Relay the owner's own words to answer_plan; never compose them.",
        )
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tools::catalog()
            .into_iter()
            .find(|t| t.name.as_ref() == name)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(tools::catalog()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        tools::call_sync(&self.state, &self.tasks, &request.name, request.arguments).await
    }

    async fn enqueue_task(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CreateTaskResult, McpError> {
        self.tasks
            .enqueue(&self.state, &request.name, request.arguments)
            .await
    }

    async fn get_task_info(
        &self,
        request: GetTaskInfoParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, McpError> {
        self.tasks.info(&self.state, &request.task_id)
    }

    async fn get_task_result(
        &self,
        request: GetTaskResultParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskPayloadResult, McpError> {
        self.tasks.result(&self.state, &request.task_id)
    }

    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CancelTaskResult, McpError> {
        self.tasks.cancel(&self.state, &request.task_id)
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        Ok(prompts::list_prompts())
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResult, McpError> {
        prompts::get_prompt(&request.name, request.arguments.as_ref())
    }
}
