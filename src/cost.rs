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
