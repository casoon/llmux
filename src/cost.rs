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
}
