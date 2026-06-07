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
}
