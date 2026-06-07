//! Token-Schätzung für agentische Requests (Messages + Tool-Schemas).

use serde_json::Value;

/// Schätzt die Input-Tokens des gesamten Requests: alle Messages plus die
/// Tool-/Function-Schemas (die bei Agenten oft den Großteil des Kontexts ausmachen).
pub fn estimate_request_tokens(body: &Value) -> u64 {
    let mut chars = 0usize;

    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for msg in messages {
            chars += content_len(msg.get("content"));
            // tool_calls (Assistant) und tool-Ergebnisse zählen mit.
            if let Some(tc) = msg.get("tool_calls") {
                chars += tc.to_string().len();
            }
        }
    }

    // Tool-/Function-Definitionen — bei Agent-Loops kostenrelevant.
    for key in ["tools", "functions"] {
        if let Some(v) = body.get(key) {
            chars += v.to_string().len();
        }
    }

    (chars as u64).div_ceil(4)
}

/// Schätzt den cachebaren Prompt-Prefix: alle Messages **außer der letzten** plus
/// die Tool-/Function-Schemas. Bei Agent-Loops ist genau das der große, nahezu
/// statische Anteil (System-Prompt, Tool-Schemas, bisherige History), den Provider
/// mit Prompt-Caching stark vergünstigt abrechnen. Die letzte Message ist der
/// marginale Anteil des aktuellen Turns. (#24)
pub fn estimate_cached_prefix_tokens(body: &Value) -> u64 {
    let mut chars = 0usize;

    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        let prefix_len = messages.len().saturating_sub(1);
        for msg in &messages[..prefix_len] {
            chars += content_len(msg.get("content"));
            if let Some(tc) = msg.get("tool_calls") {
                chars += tc.to_string().len();
            }
        }
    }

    for key in ["tools", "functions"] {
        if let Some(v) = body.get(key) {
            chars += v.to_string().len();
        }
    }

    (chars as u64).div_ceil(4)
}

/// Effektive, kostenrelevante Input-Tokens nach Anwendung des Prefix-Rabatts:
/// der marginale Anteil voll, der gecachte Prefix nur zum `billed_fraction`-Anteil.
/// Nur für die Routing-/Auswahl-Schätzung — nicht für reale Kostenabrechnung. (#24)
pub fn effective_input_tokens(input_tokens: u64, prefix_tokens: u64, billed_fraction: f64) -> u64 {
    let prefix = prefix_tokens.min(input_tokens);
    let marginal = input_tokens - prefix;
    (marginal as f64 + prefix as f64 * billed_fraction).round() as u64
}

fn content_len(content: Option<&Value>) -> usize {
    match content {
        Some(Value::String(s)) => s.len(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .map(str::len)
            .sum(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn counts_message_content() {
        // 8 Zeichen "abcdefgh" -> div_ceil(4) = 2 Tokens.
        let body = json!({ "messages": [{ "role": "user", "content": "abcdefgh" }] });
        assert_eq!(estimate_request_tokens(&body), 2);
    }

    #[test]
    fn includes_tool_schemas() {
        let no_tools = json!({ "messages": [{ "role": "user", "content": "hi" }] });
        let with_tools = json!({
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [{ "type": "function", "function": { "name": "search", "description": "find things" } }]
        });
        assert!(estimate_request_tokens(&with_tools) > estimate_request_tokens(&no_tools));
    }

    #[test]
    fn handles_content_parts_array() {
        let body = json!({
            "messages": [{ "role": "user", "content": [{ "type": "text", "text": "abcd" }] }]
        });
        assert_eq!(estimate_request_tokens(&body), 1);
    }

    #[test]
    fn empty_body_is_zero() {
        assert_eq!(estimate_request_tokens(&json!({})), 0);
    }

    #[test]
    fn prefix_excludes_last_message_but_includes_schemas() {
        let body = json!({
            "messages": [
                { "role": "system", "content": "abcdefgh" }, // 8 -> 2 tokens (prefix)
                { "role": "user", "content": "wxyz" }        // letzte Message = marginal
            ],
            "tools": [{ "type": "function", "function": { "name": "x" } }]
        });
        let total = estimate_request_tokens(&body);
        let prefix = estimate_cached_prefix_tokens(&body);
        // Prefix enthält die System-Message + Schemas, aber nicht die letzte User-Message.
        assert!(prefix > 0 && prefix < total, "prefix={prefix} total={total}");
    }

    #[test]
    fn single_message_prefix_is_only_schemas() {
        let body = json!({
            "messages": [{ "role": "user", "content": "the actual task here" }],
            "tools": [{ "type": "function", "function": { "name": "search", "description": "find things" } }]
        });
        // Einzige Message ist marginal -> Prefix = nur die Schemas.
        let schemas_only = estimate_cached_prefix_tokens(&body);
        let no_tools = json!({ "messages": [{ "role": "user", "content": "x" }] });
        assert!(schemas_only > 0);
        assert_eq!(estimate_cached_prefix_tokens(&no_tools), 0);
    }

    #[test]
    fn effective_tokens_discounts_prefix() {
        // 1000 Tokens, davon 800 gecachter Prefix zu 10 % -> 200 + 80 = 280.
        assert_eq!(effective_input_tokens(1000, 800, 0.1), 280);
        // Voller Preis (fraction 1.0) -> unverändert.
        assert_eq!(effective_input_tokens(1000, 800, 1.0), 1000);
        // Prefix wird auf das Gesamttoken-Budget gedeckelt.
        assert_eq!(effective_input_tokens(100, 500, 0.0), 0);
    }
}
