//! Exact-Match-Cache: deterministischer Schlüssel über den normalisierten Request,
//! pro gewähltem Modell. Kein Embedding — nur identische Anfragen treffen.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use serde_json::{Map, Value};

use crate::config::ModelEntry;

/// Anzahl Messages in der History (für den Cache-Längen-Guard).
pub fn conversation_len(body: &Value) -> usize {
    body.get("messages")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0)
}

/// Cache-Schlüssel aus gewähltem Modell + outputrelevanten Request-Feldern.
/// Volatile Felder (stream, eingehender model-Name, user, metadata) bleiben außen vor.
pub fn cache_key(model: &ModelEntry, body: &Value) -> String {
    let norm = normalized(body).to_string();
    let mut h = DefaultHasher::new();
    norm.hash(&mut h);
    format!("{}/{}:{:016x}", model.provider, model.model, h.finish())
}

fn normalized(body: &Value) -> Value {
    const FIELDS: &[&str] = &[
        "messages",
        "tools",
        "tool_choice",
        "functions",
        "function_call",
        "temperature",
        "top_p",
        "max_tokens",
        "response_format",
        "stop",
        "frequency_penalty",
        "presence_penalty",
        "seed",
    ];
    // serde_json::Map ist standardmäßig sortiert -> to_string ist deterministisch.
    let mut obj = Map::new();
    for &k in FIELDS {
        if let Some(v) = body.get(k) {
            obj.insert(k.to_string(), v.clone());
        }
    }
    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn model(provider: &str, name: &str) -> ModelEntry {
        ModelEntry {
            provider: provider.into(),
            model: name.into(),
            tier: 1,
            context: 8000,
            supports_tools: false,
            input_per_mtok: 0.0,
            output_per_mtok: 0.0,
        }
    }

    #[test]
    fn stable_across_reordered_keys() {
        let m = model("openai", "gpt");
        let a = json!({ "temperature": 0.2, "messages": [{ "role": "user", "content": "hi" }] });
        let b = json!({ "messages": [{ "role": "user", "content": "hi" }], "temperature": 0.2 });
        assert_eq!(cache_key(&m, &a), cache_key(&m, &b));
    }

    #[test]
    fn volatile_fields_do_not_affect_key() {
        let m = model("openai", "gpt");
        let a = json!({ "messages": [{ "role": "user", "content": "hi" }], "stream": true, "user": "alice" });
        let b = json!({ "messages": [{ "role": "user", "content": "hi" }] });
        assert_eq!(cache_key(&m, &a), cache_key(&m, &b));
    }

    #[test]
    fn isolated_per_model() {
        let body = json!({ "messages": [{ "role": "user", "content": "hi" }] });
        assert_ne!(cache_key(&model("openai", "gpt"), &body), cache_key(&model("openai", "gpt-mini"), &body));
        assert_ne!(cache_key(&model("openai", "gpt"), &body), cache_key(&model("azure", "gpt"), &body));
    }

    #[test]
    fn different_content_differs() {
        let m = model("openai", "gpt");
        let a = json!({ "messages": [{ "role": "user", "content": "hi" }] });
        let b = json!({ "messages": [{ "role": "user", "content": "bye" }] });
        assert_ne!(cache_key(&m, &a), cache_key(&m, &b));
    }

    #[test]
    fn conversation_len_counts_messages() {
        assert_eq!(conversation_len(&json!({ "messages": [{}, {}, {}] })), 3);
        assert_eq!(conversation_len(&json!({})), 0);
    }
}
