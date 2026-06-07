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
