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
        description:
            "Resume an idea (reopening it first if it's stored) and push it further with the foil.",
        arguments: &[ArgSpec {
            name: "slug",
            description: "the idea's slug (see the list_ideas tool)",
            required: true,
        }],
        // The foil is idea-vault's own model (ADR-0024): a chat message is saved as the owner's
        // turn, so the client relays and picks moves — it must not argue as the foil itself, or
        // the transcript gets a second foil speaking in the owner's voice.
        template: "Use get_idea to read idea '{slug}' in full (body, conversation, memory, \
artifacts) and give me a short recap of where it stands. If its state is 'stored', call \
reopen_idea first. idea-vault's own model is the foil; you are my relay. Send my messages to it \
verbatim with the chat tool and show me its replies. When I ask for a move, call list_skills and \
run_skill with the one I pick; when I ask to attack it from many angles, call run_swarm. Do not \
write foil turns yourself. When I say I'm done, call store_idea.",
    },
    PromptSpec {
        name: "new-idea",
        description: "Start a brand-new Draft idea in the vault and open discussion on it.",
        arguments: &[ArgSpec {
            name: "title",
            description: "a short working title for the idea",
            required: true,
        }],
        template: "Ask me for the idea in my own words, then call create_idea with title \
'{title}' and that text as the body. Send my framing as the opening chat turn so \
idea-vault's foil can steelman it and probe its weakest assumption, and show me its reply. From \
then on relay my messages with chat (do not write foil turns yourself), offer moves from \
list_skills via run_skill or run_swarm when I ask, and call store_idea when I say I'm done.",
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
/// this catalog is required, so a missing one is always an error — there is no optional-arg
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

    /// The client relays; the foil is the app's own model. A prompt that told the client to act
    /// as the foil saved its critique as an owner turn and doubled the foil (docs/adr/0024).
    #[test]
    fn prompts_make_the_client_a_relay_not_a_second_foil() {
        for (name, arg) in [("continue-discussion", "slug"), ("new-idea", "title")] {
            let result = get_prompt(name, Some(json!({ arg: "x" }).as_object().unwrap())).unwrap();
            let text = render_text(&result);
            assert!(!text.contains("act as"), "{name}: {text}");
            for tool in ["chat", "list_skills", "run_skill", "store_idea"] {
                assert!(text.contains(tool), "{name} must name {tool}: {text}");
            }
        }
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
