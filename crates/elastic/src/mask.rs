//! Masks personal data in log text before it goes to a model: e-mails, CPF/CNPJ, card numbers,
//! phone numbers and bearer tokens. The shape stays readable (`«email»`), so patterns still group.

use std::sync::LazyLock;

use regex::Regex;

struct Rule {
    pattern: Regex,
    replacement: &'static str,
}

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    let rule = |pattern: &str, replacement: &'static str| {
        Regex::new(pattern).ok().map(|pattern| Rule {
            pattern,
            replacement,
        })
    };
    [
        // JWTs and bearer tokens first, before their pieces look like anything else.
        rule(
            r"eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
            "«token»",
        ),
        rule(
            r"(?i)(bearer|basic|apikey)\s+[A-Za-z0-9._~+/=-]{16,}",
            "$1 «token»",
        ),
        rule(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}", "«email»"),
        rule(r"\b\d{2}\.?\d{3}\.?\d{3}/?\d{4}-?\d{2}\b", "«cnpj»"),
        rule(r"\b\d{3}\.\d{3}\.\d{3}-\d{2}\b", "«cpf»"),
        rule(r"\b\d(?:[ -]?\d){12,18}\b", "«cartão»"),
        rule(
            r"(?:\+?55\s?)?\(?\b\d{2}\)?\s?9\d{4}[-\s]?\d{4}\b",
            "«telefone»",
        ),
        // A bare 11-digit number is most likely a CPF without punctuation.
        rule(r"\b\d{11}\b", "«cpf»"),
    ]
    .into_iter()
    .flatten()
    .collect()
});

pub fn mask(text: &str) -> String {
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
    use super::mask;

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
        assert_eq!(
            mask("cartão 4111 1111 1111 1111 recusado"),
            "cartão «cartão» recusado"
        );
        assert_eq!(mask("ligar para (11) 98765-4321"), "ligar para «telefone»");
        assert_eq!(
            mask("POST /v1/checkout/pay 200 812ms userId=51902"),
            "POST /v1/checkout/pay 200 812ms userId=51902"
        );
        assert_eq!(mask("cnpj 12.345.678/0001-90"), "cnpj «cnpj»");
    }
}
