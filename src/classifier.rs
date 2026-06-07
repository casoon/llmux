//! Regelbasierte Analyse des Requests: task_type + agentische Merkmale (Tool-Use).
//! Später ersetzbar durch ein kleines lokales Klassifikationsmodell.

use crate::privacy;
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

/// Klassifiziert anhand von Schlüsselwörtern. Privacy hat Vorrang.
pub fn classify(text: &str, patterns: &[String]) -> TaskType {
    if privacy::contains_sensitive(text, patterns) {
        return TaskType::PrivateSensitive;
    }

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

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}
