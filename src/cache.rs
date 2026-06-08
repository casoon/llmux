//! Exact-Match-Cache: deterministischer Schlüssel über den normalisierten Request,
//! pro gewähltem Modell. Kein Embedding — nur identische Anfragen treffen.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::config::{ModelEntry, SemanticCacheConfig};

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

// ---------------------------------------------------------------------------
// Semantic-Cache (zweite Stufe, #14): Embeddings + Cosine-Ähnlichkeit.
// ---------------------------------------------------------------------------

/// Cosine-Ähnlichkeit zweier gleich langer Vektoren. 0.0 bei Längen-Mismatch oder
/// Null-Vektor (kein Treffer).
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Serialisiert ein Embedding als Little-Endian-f32-Bytes für den SQLite-BLOB-Store.
pub fn embedding_to_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// Liest ein Embedding aus dem BLOB zurück (unvollständige Reste werden ignoriert).
pub fn embedding_from_bytes(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Holt ein Embedding vom OpenAI-kompatiblen `/embeddings`-Endpoint. `None` bei jedem
/// Fehler (Netzwerk, Timeout, Nicht-2xx, unparsbare Antwort) — der Semantic-Schritt
/// wird dann übersprungen.
pub async fn fetch_embedding(
    http: &reqwest::Client,
    cfg: &SemanticCacheConfig,
    text: &str,
) -> Option<Vec<f32>> {
    let url = format!("{}/embeddings", cfg.base_url.trim_end_matches('/'));
    let payload = json!({ "model": cfg.model, "input": text });

    let mut req = http
        .post(&url)
        .timeout(Duration::from_millis(cfg.timeout_ms))
        .json(&payload);
    if let Some(env) = &cfg.api_key_env {
        if let Ok(key) = std::env::var(env) {
            req = req.bearer_auth(key);
        }
    }

    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: Value = resp.json().await.ok()?;
    let arr = body.pointer("/data/0/embedding")?.as_array()?;
    let v: Vec<f32> = arr.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect();
    (!v.is_empty()).then_some(v)
}

/// Scope-Schlüssel für den Semantic-Store: gleiche Pro-Modell-Isolation wie der
/// Exact-Match-Cache.
pub fn semantic_scope(model: &ModelEntry) -> String {
    format!("{}/{}", model.provider, model.model)
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
            capabilities: Vec::new(),
            input_per_mtok: 0.0,
            output_per_mtok: 0.0,
            strip_params: Vec::new(),
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

    // #14: Cosine-Grundverhalten.
    #[test]
    fn cosine_basics() {
        assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert!(cosine_similarity(&[1.0, 1.0], &[2.0, 2.0]) > 0.99); // gleiche Richtung
        assert_eq!(cosine_similarity(&[1.0], &[1.0, 0.0]), 0.0); // Längen-Mismatch
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]), 0.0); // Null-Vektor
    }

    // #14: BLOB-Serialisierung ist verlustfrei.
    #[test]
    fn embedding_roundtrip() {
        let v = vec![0.5f32, -1.25, 3.0, 0.0];
        assert_eq!(embedding_from_bytes(&embedding_to_bytes(&v)), v);
    }

    // #14: Embedding wird vom Mock-Endpoint korrekt geparst.
    #[tokio::test]
    async fn fetch_embedding_parses_response() {
        use axum::{routing::post, Json, Router};
        let app = Router::new().route(
            "/embeddings",
            post(|| async { Json(json!({ "data": [{ "embedding": [0.1, 0.2, 0.3] }] })) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let cfg = SemanticCacheConfig {
            enabled: true,
            base_url: format!("http://{addr}"),
            model: "e".into(),
            api_key_env: None,
            timeout_ms: 2000,
            threshold: 0.85,
        };
        let got = fetch_embedding(&reqwest::Client::new(), &cfg, "hallo").await;
        assert_eq!(got, Some(vec![0.1, 0.2, 0.3]));
    }
}
