//! Privacy-Filter: erkennt sensible Inhalte (Secrets, Keys, Kundendaten),
//! damit diese nicht in die Cloud geschickt werden.

use serde_json::Value;

/// True, wenn eines der konfigurierten Patterns (case-insensitive) im Text vorkommt.
pub fn contains_sensitive(text: &str, patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let lower = text.to_lowercase();
    patterns
        .iter()
        .any(|p| lower.contains(&p.to_lowercase()))
}

/// Prüft die Privacy-Oberfläche eines Requests gegen die Patterns.
///
/// Gescannt wird das tatsächlich vom Aufrufer beigesteuerte Nutzmaterial:
/// - der Content aller `user`- und `tool`-Messages, und
/// - die Top-Level-`tools`/`functions`-Schemas (Beschreibungen, Parameter),
///   die sonst ungeprüft an die Cloud gingen (Coverage-Lücke, siehe #23).
///
/// Statisch von Agent-Clients injizierter Kontext (`system`-/`assistant`-Content)
/// wird **standardmäßig ausgeschlossen**: er ist Client-Boilerplate, kein
/// User-Payload, und würde sonst (z. B. ein im System-Prompt eingebackener Pfad
/// oder eine Adresse) fälschlich `local_only` erzwingen. Über `scan_system` lässt
/// sich dieser Kontext bei Bedarf doch einbeziehen.
pub fn request_is_sensitive(body: &Value, patterns: &[String], scan_system: bool) -> bool {
    if patterns.is_empty() {
        return false;
    }

    let mut surface = String::new();

    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for msg in messages {
            let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
            let include = match role {
                "system" | "assistant" => scan_system,
                _ => true, // user, tool und unbekannte Rollen: einbeziehen.
            };
            if include {
                push_content(&mut surface, msg.get("content"));
            }
        }
    }

    // Tool-/Function-Schemas: Beschreibungen und Parameter können Secrets tragen.
    for key in ["tools", "functions"] {
        if let Some(v) = body.get(key) {
            surface.push_str(&v.to_string());
            surface.push('\n');
        }
    }

    contains_sensitive(&surface, patterns)
}

/// Hängt den Text-Content einer Message an `out` an (String oder OpenAI-Content-Parts).
fn push_content(out: &mut String, content: Option<&Value>) {
    match content {
        Some(Value::String(s)) => {
            out.push_str(s);
            out.push('\n');
        }
        Some(Value::Array(parts)) => {
            for part in parts {
                if let Some(t) = part.get("text").and_then(Value::as_str) {
                    out.push_str(t);
                    out.push('\n');
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn patterns() -> Vec<String> {
        vec!["PRIVATE_KEY".to_string(), "secret".to_string()]
    }

    #[test]
    fn matches_user_content() {
        let body = json!({ "messages": [{ "role": "user", "content": "hier ist mein PRIVATE_KEY" }] });
        assert!(request_is_sensitive(&body, &patterns(), false));
    }

    #[test]
    fn matches_tool_role_content() {
        let body = json!({ "messages": [{ "role": "tool", "content": "secret leaked" }] });
        assert!(request_is_sensitive(&body, &patterns(), false));
    }

    // #23: Secret im Tool-Schema wurde bisher nie gescannt (Coverage-Lücke).
    #[test]
    fn matches_tool_schema() {
        let body = json!({
            "messages": [{ "role": "user", "content": "harmlos" }],
            "tools": [{
                "type": "function",
                "function": { "name": "deploy", "description": "uses secret token" }
            }]
        });
        assert!(request_is_sensitive(&body, &patterns(), false));
    }

    // #23: System-/Assistant-Content ist standardmäßig nicht in Scope.
    #[test]
    fn ignores_system_content_by_default() {
        let body = json!({ "messages": [{ "role": "system", "content": "interner PRIVATE_KEY im Prompt" }] });
        assert!(!request_is_sensitive(&body, &patterns(), false));
        // ... lässt sich aber explizit aktivieren.
        assert!(request_is_sensitive(&body, &patterns(), true));
    }

    #[test]
    fn no_patterns_never_matches() {
        let body = json!({ "messages": [{ "role": "user", "content": "PRIVATE_KEY" }] });
        assert!(!request_is_sensitive(&body, &[], false));
    }
}
