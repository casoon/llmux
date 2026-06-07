//! Konfiguration: laden aus YAML, Datenstrukturen für Server, Budgets, Privacy,
//! Provider, Modell-Katalog und Task-Regeln.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub budgets: BudgetConfig,
    #[serde(default)]
    pub privacy: PrivacyConfig,
    pub providers: HashMap<String, ProviderConfig>,
    /// Katalog aller verfügbaren Modelle (Tier, Kontext, Tool-Fähigkeit, Preise).
    pub models: Vec<ModelEntry>,
    /// task_type -> Regel (Qualitäts-Floor, Tool-/Local-Zwang).
    pub classification: HashMap<String, TaskRule>,
    #[serde(default)]
    pub retry: RetryConfig,
    #[serde(default)]
    pub cache: CacheConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RetryConfig {
    /// Wiederholungen pro Modell bei transienten Fehlern (5xx/429/Netzwerk).
    pub max_retries: u32,
    pub backoff_initial_ms: u64,
    pub backoff_max_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 2,
            backoff_initial_ms: 500,
            backoff_max_ms: 8000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    /// Exact-Match-Antwort-Cache (kein Embedding). Spart Kosten bei identischen Requests.
    pub enabled: bool,
    pub ttl_seconds: u64,
    /// Ab dieser History-Länge wird nicht mehr gecacht (lange Agent-Loops matchen kaum
    /// und produzieren falsche Treffer).
    pub max_conversation_messages: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ttl_seconds: 1800,
            max_conversation_messages: 3,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AuthConfig {
    #[serde(default)]
    pub llmux_key: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct BudgetConfig {
    #[serde(default)]
    pub daily_max_usd: Option<f64>,
    #[serde(default)]
    pub monthly_max_usd: Option<f64>,
    /// Mit steigendem Budgetdruck wird die erlaubte Tier-Obergrenze gesenkt.
    /// Liste von Schwellen; die restriktivste zutreffende Regel gewinnt.
    #[serde(default)]
    pub pressure_downgrade: Vec<PressureRule>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct PressureRule {
    /// Budgetauslastung (0.0–1.0+), ab der diese Obergrenze greift.
    pub at: f64,
    /// Maximal erlaubtes Tier ab dieser Auslastung.
    pub max_tier: u8,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PrivacyConfig {
    #[serde(default)]
    pub local_first: bool,
    #[serde(default)]
    pub block_cloud_patterns: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub base_url: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Läuft lokal (kein Cloud-Versand) — relevant für Privacy-Routing.
    #[serde(default)]
    pub local: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelEntry {
    pub provider: String,
    pub model: String,
    /// Qualitätsstufe: 1 = billig/lokal … 5 = Top-Reasoning.
    pub tier: u8,
    /// Kontextfenster in Tokens.
    pub context: u64,
    #[serde(default)]
    pub supports_tools: bool,
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
}

impl ModelEntry {
    pub fn target(&self) -> Target {
        Target {
            provider: self.provider.clone(),
            model: self.model.clone(),
        }
    }
    /// Geschätzte Kosten in USD für gegebene Input-/Output-Tokens.
    pub fn est_cost(&self, input_tokens: u64, output_tokens: u64) -> f64 {
        input_tokens as f64 / 1_000_000.0 * self.input_per_mtok
            + output_tokens as f64 / 1_000_000.0 * self.output_per_mtok
    }
}

#[derive(Debug, Clone)]
pub struct Target {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TaskRule {
    /// Mindest-Tier für diese Aufgabe (Qualitäts-Floor).
    pub min_tier: u8,
    /// Aufgabe braucht zwingend Tool-Calling.
    #[serde(default)]
    pub require_tools: bool,
    /// Aufgabe darf nur an lokale Provider (Privacy).
    #[serde(default)]
    pub local_only: bool,
    /// Erwartetes Verhältnis Output-/Input-Tokens (für Kostenschätzung & Ranking).
    #[serde(default = "default_output_ratio")]
    pub expected_output_ratio: f64,
}

fn default_true() -> bool {
    true
}

fn default_output_ratio() -> f64 {
    1.0
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path.as_ref())?;
        let cfg: Config = serde_yaml::from_str(&raw)?;
        Ok(cfg)
    }

    /// True, wenn der Provider nutzbar ist (aktiviert und ggf. Key gesetzt).
    pub fn provider_ready(&self, provider: &str) -> bool {
        let Some(p) = self.providers.get(provider) else {
            return false;
        };
        if !p.enabled {
            return false;
        }
        match &p.api_key_env {
            Some(env) => std::env::var(env).is_ok(),
            None => true,
        }
    }

    pub fn provider_is_local(&self, provider: &str) -> bool {
        self.providers.get(provider).map(|p| p.local).unwrap_or(false)
    }

    /// Prüft die Konfiguration auf Konsistenz, damit Fehlkonfiguration beim Start
    /// auffällt statt erst beim ersten Request. Sammelt alle Verstöße.
    pub fn validate(&self) -> anyhow::Result<()> {
        use crate::classifier::TaskType;

        let mut errors: Vec<String> = Vec::new();

        // 1. Jedes Modell verweist auf einen definierten Provider.
        for m in &self.models {
            if !self.providers.contains_key(&m.provider) {
                errors.push(format!(
                    "Modell '{}' verweist auf unbekannten Provider '{}'",
                    m.model, m.provider
                ));
            }
        }

        // 2. classification-Schlüssel und Klassifikator-task_types decken sich exakt.
        let known: Vec<&str> = TaskType::ALL.iter().map(|t| t.as_key()).collect();
        for key in self.classification.keys() {
            if !known.contains(&key.as_str()) {
                errors.push(format!(
                    "classification-Schlüssel '{key}' wird vom Klassifikator nie erzeugt"
                ));
            }
        }
        for key in &known {
            if !self.classification.contains_key(*key) {
                errors.push(format!(
                    "task_type '{key}' wird klassifiziert, hat aber keine classification-Regel"
                ));
            }
        }

        // 3. Jede Routing-Anforderung ist erfüllbar: mindestens ein Modell, das
        //    Tier-Floor, Tool- und Local-Zwang der Regel gleichzeitig erfüllt.
        for (key, rule) in &self.classification {
            let satisfiable = self.models.iter().any(|m| {
                m.tier >= rule.min_tier
                    && (!rule.require_tools || m.supports_tools)
                    && (!rule.local_only || self.provider_is_local(&m.provider))
            });
            if !satisfiable {
                errors.push(format!(
                    "task_type '{key}' ist nicht erfüllbar: kein Modell mit tier>={}{}{}",
                    rule.min_tier,
                    if rule.require_tools { " + Tool-Support" } else { "" },
                    if rule.local_only { " + lokalem Provider" } else { "" },
                ));
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "Konfiguration ungültig:\n  - {}",
                errors.join("\n  - ")
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Config {
        serde_yaml::from_str(yaml).expect("fixture parses")
    }

    // Vollständige, gültige Minimalkonfiguration (alle 5 task_types erfüllbar).
    const VALID: &str = r#"
server: { host: "127.0.0.1", port: 0 }
providers:
  local_p: { enabled: true, base_url: "http://localhost/v1", local: true }
  cloud_p: { enabled: true, base_url: "https://example.com/v1" }
models:
  - { provider: local_p, model: "local-small", tier: 1, context: 8000, supports_tools: true, input_per_mtok: 0.0, output_per_mtok: 0.0 }
  - { provider: cloud_p, model: "big",         tier: 5, context: 8000, supports_tools: true, input_per_mtok: 1.0, output_per_mtok: 1.0 }
classification:
  simple_text:       { min_tier: 1 }
  summarize:         { min_tier: 1 }
  code_review:       { min_tier: 3 }
  architecture:      { min_tier: 4 }
  private_sensitive: { min_tier: 1, local_only: true }
"#;

    #[test]
    fn valid_config_passes() {
        assert!(parse(VALID).validate().is_ok());
    }

    #[test]
    fn shipped_example_config_validates() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/config/llmux.example.yaml");
        let cfg = Config::load(path).expect("example config loads");
        cfg.validate().expect("example config must validate");
    }

    #[test]
    fn rejects_unknown_provider_reference() {
        let yaml = VALID.replace("provider: cloud_p, model: \"big\"", "provider: ghost, model: \"big\"");
        let err = parse(&yaml).validate().unwrap_err().to_string();
        assert!(err.contains("unbekannten Provider 'ghost'"), "got: {err}");
    }

    #[test]
    fn rejects_unknown_classification_key() {
        let yaml = VALID.replace("simple_text:       { min_tier: 1 }", "made_up:           { min_tier: 1 }");
        let err = parse(&yaml).validate().unwrap_err().to_string();
        assert!(err.contains("made_up"), "got: {err}");
        // simple_text fehlt nun -> auch das wird gemeldet.
        assert!(err.contains("simple_text"), "got: {err}");
    }

    #[test]
    fn rejects_unsatisfiable_tier_floor() {
        // architecture verlangt tier>=4, aber kein Modell erreicht das.
        let yaml = VALID.replace("tier: 5", "tier: 3");
        let err = parse(&yaml).validate().unwrap_err().to_string();
        assert!(err.contains("architecture") && err.contains("nicht erfüllbar"), "got: {err}");
    }
}
