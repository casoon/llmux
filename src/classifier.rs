//! Regelbasierte Analyse des Requests: task_type + agentische Merkmale (Tool-Use).
//! Später ersetzbar durch ein kleines lokales Klassifikationsmodell.

use crate::config::LlmClassifierConfig;
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskType {
    PrivateSensitive,
    Architecture,
    CodeReview,
    Summarize,
    SimpleText,
}

impl TaskType {
    /// Alle vom Klassifikator erzeugbaren task_types (für Config-Validierung).
    pub const ALL: [TaskType; 5] = [
        TaskType::PrivateSensitive,
        TaskType::Architecture,
        TaskType::CodeReview,
        TaskType::Summarize,
        TaskType::SimpleText,
    ];

    /// Schlüssel in der classification-Tabelle der Config.
    pub fn as_key(&self) -> &'static str {
        match self {
            TaskType::PrivateSensitive => "private_sensitive",
            TaskType::Architecture => "architecture",
            TaskType::CodeReview => "code_review",
            TaskType::Summarize => "summarize",
            TaskType::SimpleText => "simple_text",
        }
    }

    /// Inverse zu [`TaskType::as_key`] für die regelbasiert erzeugbaren Typen.
    /// `private_sensitive` ist bewusst ausgeschlossen — der wird allein über den
    /// Privacy-Scan gesetzt, nicht vom (LLM-)Klassifikator.
    fn from_key(key: &str) -> Option<TaskType> {
        match key {
            "architecture" => Some(TaskType::Architecture),
            "code_review" => Some(TaskType::CodeReview),
            "summarize" => Some(TaskType::Summarize),
            "simple_text" => Some(TaskType::SimpleText),
            _ => None,
        }
    }
}

/// Klassifiziert anhand von Schlüsselwörtern. Der `PrivateSensitive`-Fall wird
/// nicht hier, sondern aufrufseitig anhand des Privacy-Scans gesetzt (siehe
/// `privacy::request_is_sensitive`), da Privacy eine breitere Request-Oberfläche
/// prüft als die hier verwendete (zuletzt eingegangene User-Nachricht).
pub fn classify(text: &str) -> TaskType {
    let t = text.to_lowercase();

    if contains_any(
        &t,
        &[
            "architektur", "architecture", "konzept", "design pattern",
            "datenbank", "database schema", "security", "sicherheit",
            "skalier", "scalab", "trade-off", "tradeoff",
        ],
    ) {
        return TaskType::Architecture;
    }

    if contains_any(
        &t,
        &[
            "refactor", "bug", "fix", "test", "typescript", "rust", "astro",
            "code review", "review this", "optimiere", "optimize", "accessibility",
            "function", "klasse", "class ", "implementier", "implement",
        ],
    ) {
        return TaskType::CodeReview;
    }

    if contains_any(
        &t,
        &[
            "fasse zusammen", "zusammenfass", "summarize", "summary", "tl;dr",
            "erkläre", "erklär", "explain", "übersetze", "translate",
        ],
    ) {
        return TaskType::Summarize;
    }

    TaskType::SimpleText
}

/// System-Prompt des optionalen LLM-Klassifikators (#13): zwingt das Modell auf genau
/// einen der regelbasierten task_type-Schlüssel.
const CLASSIFY_SYSTEM_PROMPT: &str = "You are a request classifier for a code-assistant \
router. Read the user's request and reply with exactly one of these labels and nothing \
else: architecture, code_review, summarize, simple_text. Use 'architecture' for system \
design, data modeling, scalability or security trade-offs; 'code_review' for writing, \
fixing, refactoring, reviewing or testing code; 'summarize' for summarizing, explaining \
or translating; 'simple_text' for anything else. Answer with the label only.";

/// Klassifiziert `text`. Ist der LLM-Klassifikator konfiguriert und aktiv, entscheidet
/// das Modell; bei Fehler, Timeout oder unklarer Antwort übernimmt nahtlos die
/// regelbasierte [`classify`]. (#13)
pub async fn classify_with_llm(
    http: &reqwest::Client,
    llm: Option<&LlmClassifierConfig>,
    text: &str,
) -> TaskType {
    if let Some(cfg) = llm {
        if cfg.enabled {
            if let Some(t) = classify_llm(http, cfg, text).await {
                return t;
            }
        }
    }
    classify(text)
}

/// Fragt das konfigurierte Modell nach dem task_type. `None` bei jedem Fehler
/// (Netzwerk, Timeout, Nicht-2xx, unparsbare oder unbekannte Antwort) — der Aufrufer
/// fällt dann auf die Regeln zurück.
async fn classify_llm(
    http: &reqwest::Client,
    cfg: &LlmClassifierConfig,
    text: &str,
) -> Option<TaskType> {
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let payload = json!({
        "model": cfg.model,
        "temperature": 0,
        "max_tokens": 16,
        "messages": [
            { "role": "system", "content": CLASSIFY_SYSTEM_PROMPT },
            { "role": "user", "content": text },
        ],
    });

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
    let answer = body
        .get("choices")?
        .get(0)?
        .get("message")?
        .get("content")?
        .as_str()?;
    parse_task_type(answer)
}

/// Extrahiert einen task_type aus der (ggf. geschwätzigen) Modellantwort: erster
/// bekannter Schlüssel, der als Teilstring vorkommt (case-insensitive).
fn parse_task_type(answer: &str) -> Option<TaskType> {
    let a = answer.to_lowercase();
    ["architecture", "code_review", "summarize", "simple_text"]
        .into_iter()
        .find(|key| a.contains(key))
        .and_then(TaskType::from_key)
}

/// Erkennt agentische Tool-Use-Requests: Tool-/Function-Definitionen, erzwungene
/// Tool-Wahl oder Tool-Ergebnisse in der History. Solche Requests müssen an ein
/// Modell mit Tool-Calling-Unterstützung gehen.
pub fn requires_tools(body: &Value) -> bool {
    let has_tool_defs = body
        .get("tools")
        .or_else(|| body.get("functions"))
        .and_then(Value::as_array)
        .map(|a| !a.is_empty())
        .unwrap_or(false);

    let forces_tool = matches!(
        body.get("tool_choice").or_else(|| body.get("function_call")),
        Some(Value::String(s)) if s != "none"
    ) || body.get("tool_choice").map(Value::is_object).unwrap_or(false);

    let history_has_tools = body
        .get("messages")
        .and_then(Value::as_array)
        .map(|msgs| {
            msgs.iter().any(|m| {
                m.get("role").and_then(Value::as_str) == Some("tool")
                    || m.get("tool_calls").is_some()
            })
        })
        .unwrap_or(false);

    has_tool_defs || forces_tool || history_has_tools
}

/// Aus dem Request abgeleitete Modell-Fähigkeiten jenseits von Tools (#31):
/// `json_schema`, wenn ein `response_format` (json_schema/json_object) gesetzt ist,
/// und `vision`, wenn eine Message Bild-Content-Parts enthält. Tools werden separat
/// über [`requires_tools`] erkannt. Das Ergebnis wird im Selektor zur Pflicht-
/// Capability-Anforderung; Modelle ohne die Fähigkeit fallen aus.
pub fn request_capabilities(body: &Value) -> Vec<String> {
    let mut caps = Vec::new();

    let needs_json = body
        .get("response_format")
        .and_then(|rf| rf.get("type"))
        .and_then(Value::as_str)
        .map(|t| t == "json_schema" || t == "json_object")
        .unwrap_or(false);
    if needs_json {
        caps.push("json_schema".to_string());
    }

    let has_image = body
        .get("messages")
        .and_then(Value::as_array)
        .map(|msgs| msgs.iter().any(message_has_image))
        .unwrap_or(false);
    if has_image {
        caps.push("vision".to_string());
    }

    caps
}

/// True, wenn der Content einer Message Bild-Parts trägt (`image_url`/`image`).
fn message_has_image(msg: &Value) -> bool {
    msg.get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts.iter().any(|p| {
                matches!(
                    p.get("type").and_then(Value::as_str),
                    Some("image_url") | Some("image") | Some("input_image")
                )
            })
        })
        .unwrap_or(false)
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{http::StatusCode, routing::post, Json, Router};
    use serde_json::json;

    /// Startet einen OpenAI-kompatiblen Mock, der auf `/chat/completions` mit `responder`
    /// antwortet, und liefert die Basis-URL. (Memory: echte lokale Modelle zu schwer.)
    async fn spawn_mock<F, R>(responder: F) -> String
    where
        F: Fn() -> R + Clone + Send + Sync + 'static,
        R: IntoResponse,
    {
        let app = Router::new().route(
            "/chat/completions",
            post(move || {
                let r = responder.clone();
                async move { r() }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn llm_cfg(base_url: String, enabled: bool) -> LlmClassifierConfig {
        LlmClassifierConfig {
            enabled,
            base_url,
            model: "test".into(),
            api_key_env: None,
            timeout_ms: 2000,
        }
    }

    use axum::response::IntoResponse;

    #[test]
    fn classifies_each_task_type() {
        assert_eq!(classify("Erkläre mir die Architektur dieses Systems"), TaskType::Architecture);
        assert_eq!(classify("Bitte refactor diese function"), TaskType::CodeReview);
        assert_eq!(classify("Fasse zusammen, worum es geht"), TaskType::Summarize);
        assert_eq!(classify("Wie ist das Wetter heute?"), TaskType::SimpleText);
    }

    #[test]
    fn detects_tool_definitions() {
        let body = json!({ "tools": [{ "type": "function", "function": { "name": "x" } }] });
        assert!(requires_tools(&body));
    }

    #[test]
    fn detects_forced_tool_choice() {
        assert!(requires_tools(&json!({ "tool_choice": "auto" })));
        assert!(requires_tools(&json!({ "tool_choice": { "type": "function" } })));
        assert!(!requires_tools(&json!({ "tool_choice": "none" })));
    }

    #[test]
    fn detects_tool_role_in_history() {
        let body = json!({ "messages": [{ "role": "tool", "content": "result" }] });
        assert!(requires_tools(&body));
    }

    #[test]
    fn plain_chat_requires_no_tools() {
        let body = json!({ "messages": [{ "role": "user", "content": "hi" }] });
        assert!(!requires_tools(&body));
    }

    #[test]
    fn detects_json_schema_capability() {
        let body = json!({
            "messages": [{ "role": "user", "content": "hi" }],
            "response_format": { "type": "json_schema", "json_schema": { "name": "x" } }
        });
        assert_eq!(request_capabilities(&body), vec!["json_schema"]);
    }

    #[test]
    fn detects_vision_capability() {
        let body = json!({
            "messages": [{ "role": "user", "content": [
                { "type": "text", "text": "what is this" },
                { "type": "image_url", "image_url": { "url": "data:..." } }
            ] }]
        });
        assert_eq!(request_capabilities(&body), vec!["vision"]);
    }

    #[test]
    fn plain_request_has_no_extra_capabilities() {
        let body = json!({ "messages": [{ "role": "user", "content": "hi" }] });
        assert!(request_capabilities(&body).is_empty());
    }

    // #13: parst auch eine geschwätzige Modellantwort robust.
    #[test]
    fn parses_model_answer() {
        assert_eq!(parse_task_type("code_review"), Some(TaskType::CodeReview));
        assert_eq!(parse_task_type("Label: ARCHITECTURE"), Some(TaskType::Architecture));
        assert_eq!(parse_task_type("simple_text\n"), Some(TaskType::SimpleText));
        assert_eq!(parse_task_type("völliger Unsinn"), None);
    }

    // #13: bei aktivem LLM bestimmt das Modell den task_type — hier überstimmt es die
    // Regeln, die für denselben Text CodeReview ergäben.
    #[tokio::test]
    async fn llm_decides_task_type_when_enabled() {
        let base = spawn_mock(|| {
            Json(json!({ "choices": [{ "message": { "content": "architecture" } }] }))
        })
        .await;
        let got = classify_with_llm(
            &reqwest::Client::new(),
            Some(&llm_cfg(base, true)),
            "fix the bug in this function",
        )
        .await;
        assert_eq!(got, TaskType::Architecture);
        // Gegenprobe: regelbasiert ergäbe CodeReview.
        assert_eq!(classify("fix the bug in this function"), TaskType::CodeReview);
    }

    // #13: bei einem Fehler des LLM-Endpoints greift nahtlos die Regelklassifikation.
    #[tokio::test]
    async fn falls_back_to_rules_on_llm_error() {
        let base = spawn_mock(|| StatusCode::INTERNAL_SERVER_ERROR).await;
        let got = classify_with_llm(
            &reqwest::Client::new(),
            Some(&llm_cfg(base, true)),
            "fix the bug in this function",
        )
        .await;
        assert_eq!(got, TaskType::CodeReview);
    }

    // #13: deaktivierter LLM-Klassifikator ruft den Endpoint gar nicht erst auf.
    #[tokio::test]
    async fn disabled_llm_uses_rules() {
        // Nicht erreichbare URL — würde der Code sie kontaktieren, schlüge der Test fehl.
        let cfg = llm_cfg("http://127.0.0.1:1".into(), false);
        let got = classify_with_llm(&reqwest::Client::new(), Some(&cfg), "fix the bug").await;
        assert_eq!(got, TaskType::CodeReview);
    }
}
