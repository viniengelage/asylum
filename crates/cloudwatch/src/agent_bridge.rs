use gpui::{App, SharedString, Window};

use crate::agent_toolkit;

/// What a CloudWatch view hands to the agent's chat.
#[derive(Clone, Debug)]
pub(crate) enum AgentContext {
    Query {
        path: Option<std::path::PathBuf>,
        query: String,
        log_groups: Vec<String>,
    },
    Event {
        log_group: Option<String>,
        message: String,
        fields: Vec<(String, String)>,
    },
    TailWindow {
        log_groups: Vec<String>,
        lines: Vec<String>,
    },
}

/// Mentions `context` in the agent panel's message editor, with `prompt` after it; `submit`
/// sends it right away.
pub(crate) fn send_to_agent(
    context: AgentContext,
    title: SharedString,
    prompt: Option<String>,
    submit: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let id = agent_toolkit::remember(describe(&context), cx);
    window.dispatch_action(
        Box::new(zed_actions::agent::MentionLogs {
            id,
            title: title.to_string(),
            prompt,
            submit,
        }),
        cx,
    );
}

/// The context as the model reads it; masking happens when the mention is resolved.
fn describe(context: &AgentContext) -> String {
    match context {
        AgentContext::Query {
            path,
            query,
            log_groups,
        } => {
            let name = path
                .as_ref()
                .and_then(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "query".to_string());
            agent_toolkit::query_context(&name, query, log_groups)
        }
        AgentContext::Event {
            log_group,
            message,
            fields,
        } => {
            let mut text = match log_group {
                Some(group) => format!("CloudWatch event from `{group}`:\n"),
                None => "CloudWatch event:\n".to_string(),
            };
            text.push_str(&format!("```\n{}\n```\n", message.trim_end()));
            if !fields.is_empty() {
                text.push_str("Fields:\n");
                for (name, value) in fields {
                    text.push_str(&format!("- {name}: {}\n", value.trim_end()));
                }
            }
            if let Some((_, request_id)) = fields
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("requestId"))
            {
                text.push_str(&format!(
                    "Other events of this request: filter requestId = \"{request_id}\".\n"
                ));
            }
            let mut stack_text = message.clone();
            for (_, value) in fields {
                stack_text.push('\n');
                stack_text.push_str(value);
            }
            let locations = agent_toolkit::stack_locations(&stack_text);
            if !locations.is_empty() {
                text.push_str(&format!(
                    "Files in the stack trace (read them in the project): {}\n",
                    locations.join(", ")
                ));
            }
            text
        }
        AgentContext::TailWindow { log_groups, lines } => format!(
            "Lines from a CloudWatch live tail of {}:\n```\n{}\n```",
            log_groups.join(", "),
            lines.join("\n")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_carry_the_request_and_the_stack_files() {
        let text = describe(&AgentContext::Event {
            log_group: Some("/ecs/trix-api-prod".into()),
            message: "Adquirente respondeu 502 ao capturar o pagamento".into(),
            fields: vec![
                ("requestId".into(), "req_9HX1c2".into()),
                (
                    "err.stack".into(),
                    "Error: Adquirente respondeu 502\n    at capturePayment (src/modules/payments/checkout.ts:142:11)".into(),
                ),
            ],
        });
        assert!(text.starts_with("CloudWatch event from `/ecs/trix-api-prod`:"));
        assert!(text.contains("filter requestId = \"req_9HX1c2\""));
        assert!(text.contains("src/modules/payments/checkout.ts:142"));
    }
}
