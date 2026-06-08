//! Weiterleitung an OpenAI-kompatible Provider (OpenRouter, OpenAI, Ollama)
//! sowie nativ an Anthropic (`/messages`) mit Format-Übersetzung in beide Richtungen.

use crate::config::{Config, ProviderKind, Target};
use reqwest::Response;
use serde_json::{json, Value};

const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Fallback für `max_tokens` (von Anthropic zwingend verlangt), falls der Client keins setzt.
const ANTHROPIC_DEFAULT_MAX_TOKENS: u64 = 4096;

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("provider '{0}' nicht konfiguriert oder deaktiviert")]
    Unavailable(String),
    #[error("API-Key env '{0}' nicht gesetzt")]
    MissingKey(String),
    #[error("HTTP-Fehler: {0}")]
    Http(#[from] reqwest::Error),
}

/// Ein einsatzbereiter Key: der zu verwendende Auth-Wert (None = kein Auth) + Gewicht.
#[derive(Debug, Clone)]
pub struct ResolvedKey {
    pub auth: Option<String>,
    pub weight: f64,
}

/// Ermittelt die für `model` einsetzbaren Keys eines Providers (Env gesetzt,
/// Allow/Deny erfüllt). Fällt auf `api_key_env` zurück; kein Auth -> ein leerer Slot.
pub fn resolve_keys(provider: &crate::config::ProviderConfig, model: &str) -> Vec<ResolvedKey> {
    if !provider.keys.is_empty() {
        return provider
            .keys
            .iter()
            .filter(|k| k.allow.is_empty() || k.allow.iter().any(|m| m == model))
            .filter(|k| !k.deny.iter().any(|m| m == model))
            .filter_map(|k| {
                std::env::var(&k.env).ok().map(|v| ResolvedKey {
                    auth: Some(v),
                    weight: k.weight.max(0.0),
                })
            })
            .collect();
    }
    match &provider.api_key_env {
        Some(env) => std::env::var(env)
            .ok()
            .map(|v| vec![ResolvedKey { auth: Some(v), weight: 1.0 }])
            .unwrap_or_default(),
        None => vec![ResolvedKey { auth: None, weight: 1.0 }],
    }
}

/// Wählt per `r` ∈ [0,1) gewichtet einen Index. Bei Gesamtgewicht 0 -> 0.
pub fn weighted_index(keys: &[ResolvedKey], r: f64) -> usize {
    let total: f64 = keys.iter().map(|k| k.weight.max(0.0)).sum();
    if total <= 0.0 {
        return 0;
    }
    let target = r.clamp(0.0, 1.0) * total;
    let mut acc = 0.0;
    for (i, k) in keys.iter().enumerate() {
        acc += k.weight.max(0.0);
        if target < acc {
            return i;
        }
    }
    keys.len() - 1
}

/// Ordnet die Keys für die Rotation: der gewichtet gewählte zuerst, Rest danach.
pub fn order_keys_weighted(mut keys: Vec<ResolvedKey>, r: f64) -> Vec<ResolvedKey> {
    if keys.len() > 1 {
        let idx = weighted_index(&keys, r);
        keys.swap(0, idx);
    }
    keys
}

/// Sendet den (umgeschriebenen) Request an das Target mit dem übergebenen Auth-Key.
/// Setzt `model` auf den Provider-Modellnamen und hängt den passenden Auth-Header an.
pub async fn forward(
    cfg: &Config,
    client: &reqwest::Client,
    target: &Target,
    mut body: Value,
    auth: Option<&str>,
) -> Result<Response, ProviderError> {
    let provider = cfg
        .providers
        .get(&target.provider)
        .filter(|p| p.enabled)
        .ok_or_else(|| ProviderError::Unavailable(target.provider.clone()))?;

    // Vom Ziel nicht unterstützte Felder entfernen (Provider-weit + modellspezifisch).
    if let Some(map) = body.as_object_mut() {
        for field in provider.strip_params.iter() {
            map.remove(field);
        }
        if let Some(model) = cfg
            .models
            .iter()
            .find(|m| m.provider == target.provider && m.model == target.model)
        {
            for field in model.strip_params.iter() {
                map.remove(field);
            }
        }
    }

    if provider.kind == ProviderKind::Anthropic {
        let url = format!("{}/messages", provider.base_url.trim_end_matches('/'));
        let payload = to_anthropic_request(&body, &target.model);
        let key = auth.ok_or_else(|| ProviderError::MissingKey("anthropic api_key".into()))?;
        let resp = client
            .post(&url)
            .header("x-api-key", key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&payload)
            .send()
            .await?;
        return Ok(resp);
    }

    // OpenAI-kompatibel: Modellfeld auf den providerspezifischen Namen umschreiben.
    if let Value::Object(map) = &mut body {
        map.insert("model".into(), Value::String(target.model.clone()));
    }

    let url = format!(
        "{}/chat/completions",
        provider.base_url.trim_end_matches('/')
    );

    let mut req = client.post(&url).json(&body);
    if let Some(key) = auth {
        req = req.bearer_auth(key);
    }

    let resp = req.send().await?;
    Ok(resp)
}

/// Übersetzt einen OpenAI-`chat/completions`-Body in einen Anthropic-`/messages`-Body.
///
/// system-Messages werden zum Top-Level-`system`. Der Tool-Zustand eines Agent-Loops
/// bleibt erhalten (#26): assistant-`tool_calls` werden zu `tool_use`-Blöcken,
/// `role: tool`-Messages zu `tool_result`-Blöcken mit passender `tool_use_id`.
/// Aufeinanderfolgende Messages gleicher Rolle (z. B. parallele Tool-Resultate)
/// werden zusammengeführt, da Anthropic strikt alternierende Rollen erwartet.
pub fn to_anthropic_request(body: &Value, model: &str) -> Value {
    let mut system = String::new();
    let mut messages: Vec<Value> = Vec::new();

    if let Some(arr) = body.get("messages").and_then(Value::as_array) {
        for m in arr {
            match m.get("role").and_then(Value::as_str).unwrap_or("user") {
                "system" => {
                    let text = message_text(m);
                    if !text.is_empty() {
                        if !system.is_empty() {
                            system.push('\n');
                        }
                        system.push_str(&text);
                    }
                }
                "assistant" => {
                    let mut blocks: Vec<Value> = Vec::new();
                    let text = message_text(m);
                    if !text.is_empty() {
                        blocks.push(json!({ "type": "text", "text": text }));
                    }
                    if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
                        blocks.extend(calls.iter().map(openai_tool_call_to_anthropic));
                    }
                    push_blocks(&mut messages, "assistant", blocks);
                }
                "tool" => {
                    let id = m.get("tool_call_id").and_then(Value::as_str).unwrap_or("");
                    let block = json!({
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": message_text(m)
                    });
                    push_blocks(&mut messages, "user", vec![block]);
                }
                _ => {
                    let text = message_text(m);
                    let blocks = if text.is_empty() {
                        Vec::new()
                    } else {
                        vec![json!({ "type": "text", "text": text })]
                    };
                    push_blocks(&mut messages, "user", blocks);
                }
            }
        }
    }

    // Eine reine Einzel-Text-Message zur kompakten String-Form kollabieren
    // (Anthropic akzeptiert beides; hält die Payload schlank und kompatibel).
    for msg in &mut messages {
        collapse_single_text(msg);
    }

    let max_tokens = body
        .get("max_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(ANTHROPIC_DEFAULT_MAX_TOKENS);

    let mut out = json!({ "model": model, "max_tokens": max_tokens, "messages": messages });
    let obj = out.as_object_mut().unwrap();
    if !system.is_empty() {
        obj.insert("system".into(), json!(system));
    }
    for k in ["temperature", "top_p"] {
        if let Some(v) = body.get(k) {
            obj.insert(k.into(), v.clone());
        }
    }
    if let Some(stop) = body.get("stop") {
        obj.insert("stop_sequences".into(), stop.clone());
    }
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let mapped: Vec<Value> = tools.iter().filter_map(openai_tool_to_anthropic).collect();
        if !mapped.is_empty() {
            obj.insert("tools".into(), json!(mapped));
        }
    }
    out
}

/// Übersetzt eine Anthropic-`/messages`-Antwort zurück in das OpenAI-`chat.completion`-Format.
pub fn to_openai_response(resp: &Value, created: u64) -> Value {
    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    if let Some(content) = resp.get("content").and_then(Value::as_array) {
        for block in content {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(Value::as_str) {
                        text.push_str(t);
                    }
                }
                Some("tool_use") => {
                    let id = block.get("id").and_then(Value::as_str).unwrap_or("");
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                    let args = block.get("input").map(Value::to_string).unwrap_or_else(|| "{}".into());
                    tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": { "name": name, "arguments": args }
                    }));
                }
                _ => {}
            }
        }
    }

    let finish = match resp.get("stop_reason").and_then(Value::as_str) {
        Some("max_tokens") => "length",
        Some("tool_use") => "tool_calls",
        Some("refusal") => "content_filter",
        _ => "stop", // end_turn, stop_sequence, sonstige
    };

    let prompt = resp.pointer("/usage/input_tokens").and_then(Value::as_u64).unwrap_or(0);
    let completion = resp.pointer("/usage/output_tokens").and_then(Value::as_u64).unwrap_or(0);

    let mut message = json!({
        "role": "assistant",
        "content": if text.is_empty() { Value::Null } else { json!(text) }
    });
    if !tool_calls.is_empty() {
        message.as_object_mut().unwrap().insert("tool_calls".into(), json!(tool_calls));
    }

    json!({
        "id": resp.get("id").cloned().unwrap_or_else(|| json!("")),
        "object": "chat.completion",
        "created": created,
        "model": resp.get("model").cloned().unwrap_or_else(|| json!("")),
        "choices": [{ "index": 0, "message": message, "finish_reason": finish }],
        "usage": {
            "prompt_tokens": prompt,
            "completion_tokens": completion,
            "total_tokens": prompt + completion
        }
    })
}

/// Extrahiert den Textinhalt einer OpenAI-Message (String oder Content-Parts).
fn message_text(msg: &Value) -> String {
    match msg.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Hängt eine Content-Block-Liste als Message der Rolle `role` an. Ist die letzte
/// Message bereits dieselbe Rolle, werden die Blöcke zusammengeführt — Anthropic
/// erlaubt nur alternierende user/assistant-Rollen (#26). Leere Blocklisten werden
/// übersprungen.
fn push_blocks(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some(last) = messages.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some(role) {
            if let Some(arr) = last.get_mut("content").and_then(Value::as_array_mut) {
                arr.extend(blocks);
                return;
            }
        }
    }
    messages.push(json!({ "role": role, "content": blocks }));
}

/// Kollabiert eine Message, deren Content genau ein Text-Block ist, zur String-Form.
fn collapse_single_text(msg: &mut Value) {
    let text = {
        let Some(arr) = msg.get("content").and_then(Value::as_array) else {
            return;
        };
        if arr.len() != 1 || arr[0].get("type").and_then(Value::as_str) != Some("text") {
            return;
        }
        match arr[0].get("text").and_then(Value::as_str) {
            Some(t) => t.to_string(),
            None => return,
        }
    };
    if let Some(o) = msg.as_object_mut() {
        o.insert("content".into(), Value::String(text));
    }
}

/// OpenAI-`tool_call` (`{id, function:{name, arguments(JSON-String)}}`)
/// -> Anthropic-`tool_use`-Block (`{type, id, name, input(JSON-Objekt)}`).
fn openai_tool_call_to_anthropic(tc: &Value) -> Value {
    let id = tc.get("id").and_then(Value::as_str).unwrap_or("");
    let f = tc.get("function");
    let name = f
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let input = f
        .and_then(|f| f.get("arguments"))
        .and_then(Value::as_str)
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .unwrap_or_else(|| json!({}));
    json!({ "type": "tool_use", "id": id, "name": name, "input": input })
}

/// OpenAI-Tool (`{type:"function", function:{name,description,parameters}}`)
/// -> Anthropic-Tool (`{name, description, input_schema}`).
fn openai_tool_to_anthropic(tool: &Value) -> Option<Value> {
    let f = tool.get("function")?;
    let name = f.get("name")?.as_str()?;
    let mut o = json!({
        "name": name,
        "input_schema": f.get("parameters").cloned().unwrap_or_else(|| json!({ "type": "object" }))
    });
    if let Some(d) = f.get("description") {
        o.as_object_mut().unwrap().insert("description".into(), d.clone());
    }
    Some(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_extracts_system_and_messages() {
        let body = json!({
            "model": "ignored",
            "max_tokens": 100,
            "messages": [
                { "role": "system", "content": "You are helpful." },
                { "role": "user", "content": "Hi" },
                { "role": "assistant", "content": "Hello" }
            ]
        });
        let out = to_anthropic_request(&body, "claude-x");
        assert_eq!(out["model"], "claude-x");
        assert_eq!(out["max_tokens"], 100);
        assert_eq!(out["system"], "You are helpful.");
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
    }

    #[test]
    fn request_defaults_max_tokens_and_maps_tools() {
        let body = json!({
            "messages": [{ "role": "user", "content": [{ "type": "text", "text": "hi" }] }],
            "stop": ["END"],
            "tools": [{ "type": "function", "function": { "name": "search", "description": "find", "parameters": { "type": "object" } } }]
        });
        let out = to_anthropic_request(&body, "claude-x");
        assert_eq!(out["max_tokens"], ANTHROPIC_DEFAULT_MAX_TOKENS);
        assert_eq!(out["stop_sequences"][0], "END");
        assert_eq!(out["tools"][0]["name"], "search");
        assert!(out["tools"][0].get("input_schema").is_some());
        // Content-Parts wurden zu Text zusammengezogen.
        assert_eq!(out["messages"][0]["content"], "hi");
    }

    #[test]
    fn response_maps_text_usage_and_finish() {
        let resp = json!({
            "id": "msg_123",
            "model": "claude-x",
            "stop_reason": "end_turn",
            "content": [{ "type": "text", "text": "Hello there" }],
            "usage": { "input_tokens": 12, "output_tokens": 7 }
        });
        let out = to_openai_response(&resp, 1700000000);
        assert_eq!(out["object"], "chat.completion");
        assert_eq!(out["id"], "msg_123");
        assert_eq!(out["choices"][0]["message"]["content"], "Hello there");
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        assert_eq!(out["usage"]["prompt_tokens"], 12);
        assert_eq!(out["usage"]["completion_tokens"], 7);
        assert_eq!(out["usage"]["total_tokens"], 19);
    }

    #[test]
    fn request_maps_assistant_tool_calls_to_tool_use() {
        let body = json!({
            "messages": [
                { "role": "user", "content": "search x" },
                { "role": "assistant", "content": "", "tool_calls": [
                    { "id": "call_1", "type": "function", "function": { "name": "search", "arguments": "{\"q\":\"x\"}" } }
                ] }
            ]
        });
        let out = to_anthropic_request(&body, "claude-x");
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[1]["role"], "assistant");
        let block = &msgs[1]["content"][0];
        assert_eq!(block["type"], "tool_use");
        assert_eq!(block["id"], "call_1");
        assert_eq!(block["name"], "search");
        // arguments-String wurde zu einem JSON-Objekt geparst.
        assert_eq!(block["input"]["q"], "x");
    }

    #[test]
    fn request_maps_tool_role_to_tool_result() {
        let body = json!({
            "messages": [
                { "role": "assistant", "content": "", "tool_calls": [
                    { "id": "call_1", "type": "function", "function": { "name": "search", "arguments": "{}" } }
                ] },
                { "role": "tool", "tool_call_id": "call_1", "content": "result text" }
            ]
        });
        let out = to_anthropic_request(&body, "claude-x");
        let msgs = out["messages"].as_array().unwrap();
        let last = msgs.last().unwrap();
        assert_eq!(last["role"], "user");
        assert_eq!(last["content"][0]["type"], "tool_result");
        assert_eq!(last["content"][0]["tool_use_id"], "call_1");
        assert_eq!(last["content"][0]["content"], "result text");
    }

    #[test]
    fn request_preserves_text_with_tool_use_and_merges_parallel_results() {
        let body = json!({
            "messages": [
                { "role": "assistant", "content": "let me check", "tool_calls": [
                    { "id": "a", "type": "function", "function": { "name": "f", "arguments": "{}" } },
                    { "id": "b", "type": "function", "function": { "name": "g", "arguments": "{}" } }
                ] },
                { "role": "tool", "tool_call_id": "a", "content": "ra" },
                { "role": "tool", "tool_call_id": "b", "content": "rb" }
            ]
        });
        let out = to_anthropic_request(&body, "claude-x");
        let msgs = out["messages"].as_array().unwrap();
        // assistant: Text-Block bleibt neben den beiden tool_use-Blöcken erhalten.
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["content"][0]["type"], "text");
        assert_eq!(msgs[0]["content"][0]["text"], "let me check");
        assert_eq!(msgs[0]["content"][1]["type"], "tool_use");
        assert_eq!(msgs[0]["content"][2]["type"], "tool_use");
        // Beide Tool-Resultate sind zu einer einzigen user-Message zusammengeführt.
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"].as_array().unwrap().len(), 2);
        assert_eq!(msgs[1]["content"][0]["tool_use_id"], "a");
        assert_eq!(msgs[1]["content"][1]["tool_use_id"], "b");
    }

    fn keys(weights: &[f64]) -> Vec<ResolvedKey> {
        weights
            .iter()
            .map(|w| ResolvedKey { auth: Some("k".into()), weight: *w })
            .collect()
    }

    #[test]
    fn weighted_index_respects_weights() {
        let ks = keys(&[3.0, 1.0]); // 75% / 25%
        assert_eq!(weighted_index(&ks, 0.0), 0);
        assert_eq!(weighted_index(&ks, 0.5), 0);
        assert_eq!(weighted_index(&ks, 0.74), 0);
        assert_eq!(weighted_index(&ks, 0.76), 1);
        assert_eq!(weighted_index(&ks, 0.99), 1);
    }

    #[test]
    fn weighted_index_zero_total_is_safe() {
        let ks = keys(&[0.0, 0.0]);
        assert_eq!(weighted_index(&ks, 0.5), 0);
    }

    #[test]
    fn order_puts_weighted_choice_first() {
        let ordered = order_keys_weighted(keys(&[3.0, 1.0]), 0.9);
        assert_eq!(ordered.len(), 2);
        // r=0.9 wählt Index 1 -> nach vorne getauscht.
        assert_eq!(ordered[0].weight, 1.0);
    }

    #[test]
    fn response_maps_tool_use() {
        let resp = json!({
            "id": "msg_1",
            "model": "claude-x",
            "stop_reason": "tool_use",
            "content": [
                { "type": "text", "text": "" },
                { "type": "tool_use", "id": "tu_1", "name": "search", "input": { "q": "x" } }
            ],
            "usage": { "input_tokens": 5, "output_tokens": 3 }
        });
        let out = to_openai_response(&resp, 0);
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
        let tc = &out["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(tc["function"]["name"], "search");
        assert_eq!(tc["function"]["arguments"], "{\"q\":\"x\"}");
    }
}
