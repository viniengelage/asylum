use std::sync::LazyLock;

use regex::Regex;
use task_agents::CloudWatchFinding;

const MAX_SUMMARY_CHARS: usize = 80;

/// The commands of a Logs Insights query, without comments. A `|` inside quotes or a regex
/// literal (`msg like /a|b/`) doesn't split.
pub(crate) fn commands(query: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut in_regex = false;
    let mut chars = query.chars().peekable();
    while let Some(character) = chars.next() {
        if let Some(open) = quote {
            current.push(character);
            if character == '\\' {
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
            } else if character == open {
                quote = None;
            }
            continue;
        }
        if in_regex {
            current.push(character);
            if character == '\\' {
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
            } else if character == '/' {
                in_regex = false;
            }
            continue;
        }
        match character {
            '"' | '\'' | '`' => {
                quote = Some(character);
                current.push(character);
            }
            '/' if regex_can_start(&current) => {
                in_regex = true;
                current.push(character);
            }
            '#' => {
                while chars.peek().is_some_and(|next| *next != '\n') {
                    chars.next();
                }
            }
            '|' => push_command(&mut commands, &mut current),
            '\n' | '\r' | '\t' => current.push(' '),
            _ => current.push(character),
        }
    }
    push_command(&mut commands, &mut current);
    commands
}

fn regex_can_start(before: &str) -> bool {
    let before = before.trim_end();
    before.ends_with('~')
        || before.ends_with('(')
        || before.ends_with(',')
        || before
            .rsplit(|character: char| character.is_whitespace())
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("like"))
}

fn push_command(commands: &mut Vec<String>, current: &mut String) {
    let command = current.split_whitespace().collect::<Vec<_>>().join(" ");
    if !command.is_empty() {
        commands.push(command);
    }
    current.clear();
}

fn split_command(command: &str) -> (String, &str) {
    match command.split_once(' ') {
        Some((name, rest)) => (name.to_ascii_lowercase(), rest.trim()),
        None => (command.to_ascii_lowercase(), ""),
    }
}

/// The filters and stats of a query, which say what it looks for better than its `fields`:
/// `level = "error" · count(*) by msg`.
pub(crate) fn summary(query: &str) -> String {
    let commands = commands(query);
    let parts = commands
        .iter()
        .filter_map(|command| {
            let (name, rest) = split_command(command);
            matches!(name.as_str(), "filter" | "stats").then_some(rest)
        })
        .filter(|rest| !rest.is_empty())
        .collect::<Vec<_>>();
    let text = if parts.is_empty() {
        commands.first().cloned().unwrap_or_default()
    } else {
        parts.join(" · ")
    };
    truncate(&text, MAX_SUMMARY_CHARS)
}

/// The single field the last `stats … by` groups by, if there is one.
pub(crate) fn grouped_by(query: &str) -> Option<String> {
    let stats = commands(query).into_iter().rev().find_map(|command| {
        let (name, rest) = split_command(&command);
        (name == "stats").then(|| rest.to_string())
    })?;
    let lowercase = stats.to_ascii_lowercase();
    let by = lowercase.rfind(" by ")?;
    let field = stats.get(by + 4..)?.trim();
    (!field.is_empty() && !field.contains(',')).then(|| field.to_string())
}

/// For `stats count(*) by <field>`: the most frequent value, its count and the total, so the card
/// can say "22 of the 37 are …". Time bins aren't a finding.
pub(crate) fn finding(
    rows: &[Vec<(String, String)>],
    grouped_by: Option<&str>,
) -> Option<CloudWatchFinding> {
    let field = grouped_by?;
    if field.to_ascii_lowercase().starts_with("bin(") || rows.len() < 2 {
        return None;
    }
    let count_of = |row: &[(String, String)]| {
        row.iter()
            .filter(|(name, _)| name != field)
            .find_map(|(_, value)| value.trim().parse::<f64>().ok())
            .filter(|count| *count >= 0.0)
            .map(|count| count.round() as u64)
    };
    let mut total = 0;
    let mut top: Option<(String, u64)> = None;
    for row in rows {
        let count = count_of(row)?;
        let value = row
            .iter()
            .find(|(name, _)| name == field)
            .map(|(_, value)| value.clone())?;
        total += count;
        if top.as_ref().is_none_or(|(_, top_count)| count > *top_count) {
            top = Some((value, count));
        }
    }
    let (value, count) = top?;
    (count > 0).then_some(CloudWatchFinding {
        value: truncate(&value, MAX_SUMMARY_CHARS),
        count,
        total,
    })
}

static STACK_LOCATION: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"(?:^|[\s(])((?:[A-Za-z]:)?[A-Za-z0-9_@.\-/\\]+\.[A-Za-z]{1,5}):(\d+)").ok()
});

/// The `path:line` locations of a stack trace, without the runtime's own frames.
pub(crate) fn stack_locations(text: &str) -> Vec<String> {
    let Some(pattern) = STACK_LOCATION.as_ref() else {
        return Vec::new();
    };
    let mut locations = Vec::new();
    for captures in pattern.captures_iter(text) {
        let (Some(path), Some(line)) = (captures.get(1), captures.get(2)) else {
            continue;
        };
        let path = path.as_str();
        if path.starts_with("node:")
            || path.starts_with("internal/")
            || path.contains("node_modules")
        {
            continue;
        }
        let location = format!("{path}:{}", line.as_str());
        if !locations.contains(&location) {
            locations.push(location);
        }
    }
    locations
}

pub(crate) fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() > max_chars {
        format!(
            "{}…",
            text.chars()
                .take(max_chars.saturating_sub(1))
                .collect::<String>()
        )
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUERY: &str = "# erros de pagamento\nfields @timestamp, msg, requestId\n| filter level = \"error\" and msg like /pagamento|checkout/\n| sort @timestamp desc\n| limit 200\n\n# | stats count(*) by bin(1m)";

    #[test]
    fn splits_commands_outside_quotes_regexes_and_comments() {
        assert_eq!(
            commands(QUERY),
            vec![
                "fields @timestamp, msg, requestId",
                "filter level = \"error\" and msg like /pagamento|checkout/",
                "sort @timestamp desc",
                "limit 200",
            ]
        );
        assert_eq!(
            commands("filter msg = \"a | b\" | stats count(*) as n by bin(5m)"),
            vec!["filter msg = \"a | b\"", "stats count(*) as n by bin(5m)"]
        );
    }

    #[test]
    fn summarizes_filters_and_stats() {
        assert_eq!(
            summary("fields msg | filter level = \"error\" | stats count(*) by msg"),
            "level = \"error\" · count(*) by msg"
        );
        assert_eq!(
            summary("fields @timestamp, @message | sort @timestamp desc"),
            "fields @timestamp, @message"
        );
    }

    #[test]
    fn finds_the_most_frequent_message() {
        let field = grouped_by("filter level = \"error\" | stats count(*) by msg");
        assert_eq!(field.as_deref(), Some("msg"));
        let row = |message: &str, count: &str| {
            vec![
                ("msg".to_string(), message.to_string()),
                ("count(*)".to_string(), count.to_string()),
            ]
        };
        let rows = vec![
            row("Adquirente respondeu 502", "22"),
            row("timeout do gateway", "11"),
            row("carrinho sem método", "3"),
            row("assinatura inválida", "1"),
        ];
        assert_eq!(
            finding(&rows, field.as_deref()),
            Some(CloudWatchFinding {
                value: "Adquirente respondeu 502".into(),
                count: 22,
                total: 37,
            })
        );
        assert_eq!(finding(&rows, Some("bin(1m)")), None);
        assert_eq!(grouped_by("stats count(*) by msg, level"), None);
        assert_eq!(grouped_by("fields msg"), None);
    }

    #[test]
    fn reads_stack_locations() {
        let stack = "Error: Adquirente respondeu 502\n    at capturePayment (src/modules/payments/checkout.ts:142:11)\n    at async PaymentsController.pay (src/modules/payments/payments.controller.ts:58:5)\n    at async node:internal/process/task_queues:95:5";
        assert_eq!(
            stack_locations(stack),
            vec![
                "src/modules/payments/checkout.ts:142",
                "src/modules/payments/payments.controller.ts:58",
            ]
        );
    }
}
