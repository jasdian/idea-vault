//! MCP prompts (docs/adr/0024): a small canned catalog surfaced in an MCP client's "/" picker,
//! adapted from the sibling `mcp-server` repo's declarative `PromptSpec` pattern. Each expands to
//! a single User message that names the idea-vault tools (`tools.rs`) to drive toward a concrete
//! goal — prompts are pure text templates, they perform no vault I/O themselves.

use rmcp::model::{
    GetPromptResult, JsonObject, ListPromptsResult, Prompt, PromptArgument, PromptMessage,
    PromptMessageRole,
};
use rmcp::ErrorData as McpError;

/// One templated argument for a prompt.
struct ArgSpec {
    name: &'static str,
    description: &'static str,
    required: bool,
}

/// A declarative prompt: name, description, args, and a body template whose `{argName}`
/// placeholders are filled at `get` time.
struct PromptSpec {
    name: &'static str,
    description: &'static str,
    arguments: &'static [ArgSpec],
    template: &'static str,
}

static PROMPTS: &[PromptSpec] = &[
    PromptSpec {
        name: "continue-discussion",
        description: "Resume an idea (reopening it first if it's stored) and push it further with one foil turn.",
        arguments: &[ArgSpec {
            name: "slug",
            description: "the idea's slug (see the list_ideas tool)",
            required: true,
        }],
        template: "Use get_idea to read idea '{slug}' in full (frontmatter, body, conversation, \
memory). If its state is 'stored', call reopen_idea first. Then act as a rigorous ideation foil: \
steelman the owner's latest point, then stress-test it from an angle the discussion has not yet \
covered, and send that as one chat turn to the idea.",
    },
    PromptSpec {
        name: "new-idea",
        description: "Start a brand-new Draft idea in the vault and open discussion on it.",
        arguments: &[ArgSpec {
            name: "title",
            description: "a short working title for the idea",
            required: true,
        }],
        template: "Call create_idea with title '{title}'. Then send an opening chat turn to the \
new idea that steelmans it in the owner's likely framing before probing its weakest assumption.",
    },
];

/// The advertised prompt catalog.
fn all_prompts() -> Vec<Prompt> {
    PROMPTS
        .iter()
        .map(|p| {
            let args: Vec<PromptArgument> = p
                .arguments
                .iter()
                .map(|a| {
                    PromptArgument::new(a.name)
                        .with_description(a.description)
                        .with_required(a.required)
                })
                .collect();
            Prompt::new(
                p.name,
                Some(p.description),
                if args.is_empty() { None } else { Some(args) },
            )
        })
        .collect()
}

pub(super) fn list_prompts() -> ListPromptsResult {
    ListPromptsResult::with_all_items(all_prompts())
}

/// Render one prompt by name, filling `{arg}` placeholders from `arguments`. Every argument in
/// this MVP catalog is required, so a missing one is always an error — there is no optional-arg
/// default phrase to fall back to (contrast the sibling `mcp-server`'s catalog).
pub(super) fn get_prompt(
    name: &str,
    arguments: Option<&JsonObject>,
) -> Result<GetPromptResult, McpError> {
    let spec = PROMPTS
        .iter()
        .find(|p| p.name == name)
        .ok_or_else(|| McpError::invalid_params(format!("unknown prompt '{name}'"), None))?;

    let mut body = spec.template.to_string();
    for arg in spec.arguments {
        let provided = arguments
            .and_then(|m| m.get(arg.name))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let value = provided.ok_or_else(|| {
            McpError::invalid_params(
                format!("prompt '{name}' requires argument '{}'", arg.name),
                None,
            )
        })?;
        body = body.replace(&format!("{{{}}}", arg.name), value);
    }

    let message = PromptMessage::new_text(PromptMessageRole::User, body);
    Ok(GetPromptResult::new(vec![message]).with_description(spec.description))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn catalog_has_unique_named_prompts_with_descriptions() {
        let prompts = all_prompts();
        assert_eq!(prompts.len(), 2);
        let mut seen = std::collections::HashSet::new();
        for p in &prompts {
            assert!(
                seen.insert(p.name.clone()),
                "duplicate prompt name {}",
                p.name
            );
            assert!(p.description.as_ref().is_some_and(|d| !d.is_empty()));
        }
    }

    #[test]
    fn required_arg_fills_placeholder() {
        let result = get_prompt(
            "continue-discussion",
            Some(json!({ "slug": "my-idea" }).as_object().unwrap()),
        )
        .unwrap();
        let text = render_text(&result);
        assert!(text.contains("my-idea"));
        assert!(!text.contains('{'), "placeholder must be filled: {text}");
    }

    #[test]
    fn missing_required_arg_is_invalid_params() {
        assert!(get_prompt("new-idea", None).is_err());
    }

    #[test]
    fn unknown_prompt_is_invalid_params() {
        assert!(get_prompt("nope", None).is_err());
    }

    fn render_text(r: &GetPromptResult) -> String {
        match &r.messages[0].content {
            rmcp::model::PromptMessageContent::Text { text } => text.clone(),
            _ => panic!("expected text content"),
        }
    }
}
