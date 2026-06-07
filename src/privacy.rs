//! Privacy-Filter: erkennt sensible Inhalte (Secrets, Keys, Kundendaten),
//! damit diese nicht in die Cloud geschickt werden.

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
