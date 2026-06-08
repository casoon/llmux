//! Regelbasierte Analyse des Requests: task_type + agentische Merkmale (Tool-Use).
//! Später ersetzbar durch ein kleines lokales Klassifikationsmodell.

use serde_json::Value;

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
    use serde_json::json;

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
}
