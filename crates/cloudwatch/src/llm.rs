use std::sync::LazyLock;

use anyhow::{Context as _, Result, anyhow};
use futures::StreamExt as _;
use gpui::{App, SharedString, Task};
use language_model::{
    CompletionIntent, LanguageModelRegistry, LanguageModelRequest, LanguageModelRequestMessage,
    Role,
};
use regex::Regex;
use serde::de::DeserializeOwned;
use util::ResultExt as _;

/// One request to the user's default model, outside any thread, returning the whole answer.
pub(crate) fn complete(system: String, prompt: String, cx: &mut App) -> Task<Result<String>> {
    let registry = LanguageModelRegistry::read_global(cx);
    let Some(model) = registry.default_model() else {
        return Task::ready(Err(anyhow!(i18n::t!("cloudwatch_query.llm.no_model"))));
    };
    let provider = match registry.provider_for_model(&model) {
        Ok(provider) => provider,
        Err(error) => return Task::ready(Err(error.into())),
    };
    let request = LanguageModelRequest {
        intent: Some(CompletionIntent::InlineAssist),
        messages: vec![
            LanguageModelRequestMessage {
                role: Role::System,
                content: vec![system.into()],
                cache: false,
                reasoning_details: None,
            },
            LanguageModelRequestMessage {
                role: Role::User,
                content: vec![prompt.into()],
                cache: false,
                reasoning_details: None,
            },
        ],
        ..Default::default()
    };
    cx.spawn(async move |cx| {
        let mut response = provider.stream_completion_text(&model, request, cx).await?;
        let mut text = String::new();
        while let Some(chunk) = response.stream.next().await {
            text.push_str(&chunk?);
        }
        Ok(text)
    })
}

/// The name shown on the chip of the ⌘K prompt.
pub(crate) fn default_model_name(cx: &App) -> Option<SharedString> {
    LanguageModelRegistry::read_global(cx)
        .default_model()
        .map(|model| SharedString::from(model.name().0.to_string()))
}

/// Models wrap JSON in prose or fences now and then; this takes the outermost object.
pub(crate) fn parse_json<T: DeserializeOwned>(answer: &str) -> Result<T> {
    let start = answer.find('{').context("the answer has no JSON object")?;
    let end = answer.rfind('}').context("the answer has no JSON object")?;
    let json = answer
        .get(start..=end)
        .context("the answer has no JSON object")?;
    serde_json::from_str(json).with_context(|| format!("unexpected answer: {answer}"))
}

struct Rule {
    pattern: Regex,
    replacement: &'static str,
}

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    let rule = |pattern: &str, replacement: &'static str| {
        Regex::new(pattern).log_err().map(|pattern| Rule {
            pattern,
            replacement,
        })
    };
    [
        // Tokens first, before their pieces look like anything else.
        rule(
            r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
            "«token»",
        ),
        rule(
            r"(?i)(bearer|basic|apikey|token)(\s*[:=]?\s*)[A-Za-z0-9._~+/=-]{16,}",
            "$1$2«token»",
        ),
        rule(r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b", "«aws-key»"),
        rule(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}", "«email»"),
        rule(r"\b\d{3}\.\d{3}\.\d{3}-\d{2}\b", "«cpf»"),
        rule(r"\b\d(?:[ -]?\d){12,18}\b", "«número»"),
        rule(r"\b\d{11,}\b", "«número»"),
    ]
    .into_iter()
    .flatten()
    .collect()
});

/// Masks personal data and secrets in log text before it goes to the model; the shape stays
/// readable (`«email»`), so the model can still group messages.
pub(crate) fn mask(text: &str) -> String {
    let mut masked = text.to_string();
    for rule in RULES.iter() {
        if rule.pattern.is_match(&masked) {
            masked = rule
                .pattern
                .replace_all(&masked, rule.replacement)
                .into_owned();
        }
    }
    masked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_personal_data_and_keeps_the_rest() {
        assert_eq!(
            mask("login falhou para ana.silva@trix.com.br (cpf 123.456.789-09)"),
            "login falhou para «email» (cpf «cpf»)"
        );
        assert_eq!(
            mask("Authorization: Bearer abcdefghijklmnopqrstuvwxyz123456"),
            "Authorization: Bearer «token»"
        );
        assert_eq!(mask("chave AKIAABCDEFGHIJKLMNOP"), "chave «aws-key»");
        assert_eq!(mask("telefone 11987654321"), "telefone «número»");
        assert_eq!(
            mask("POST /v1/checkout/pay 200 812ms userId=48213"),
            "POST /v1/checkout/pay 200 812ms userId=48213"
        );
    }

    #[test]
    fn reads_json_wrapped_in_prose() {
        #[derive(serde::Deserialize)]
        struct Answer {
            query: String,
        }
        let answer: Answer =
            parse_json("Aqui está:\n```json\n{\"query\": \"filter level = 1\"}\n```").unwrap();
        assert_eq!(answer.query, "filter level = 1");
        assert!(parse_json::<Answer>("sem json").is_err());
    }
}
