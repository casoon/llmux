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
    /// Logische Namen (`fast`, `best`, `cheap`) -> Katalogmodell (`model` oder `provider/model`).
    /// Wird vor der Auswahl auf `x-llmux-model` und das `model`-Feld des Requests angewandt.
    #[serde(default)]
    pub aliases: HashMap<String, String>,
    /// Projekt-Scopes (`projects.<name>`): pro Projekt zusätzliche Routing-Regeln,
    /// aufgelöst über den `x-llmux-project`-Header. Leer = kein Scope (Default). (#33)
    #[serde(default)]
    pub projects: HashMap<String, ProjectProfile>,
    #[serde(default)]
    pub retry: RetryConfig,
    /// Routing-Profil-Defaults (Latenz vs. Kosten, #30).
    #[serde(default)]
    pub routing: RoutingConfig,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub classifier: ClassifierConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ClassifierConfig {
    /// Anzahl der letzten `user`-Messages, über die der regelbasierte Klassifikator
    /// den `task_type` bestimmt. System-/Assistant-/Tool-Rollen bleiben außen vor,
    /// damit der große statische Prefix von Agent-Clients die Wahl nicht verzerrt (#22).
    pub user_messages: usize,
}

impl Default for ClassifierConfig {
    fn default() -> Self {
        Self { user_messages: 1 }
    }
}

/// Routing-Profil: priorisiert bei sonst gleichen harten Filtern Kosten oder
/// Latenz (#30). Nur `interactive` weicht von cheapest-viable ab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// Niedrigste erwartete Latenz zuerst (Kosten als Tiebreak).
    Interactive,
    /// Günstigstes gültiges Modell (Standard).
    #[default]
    Balanced,
    /// Wie Balanced (Kosten dominieren); Platzhalter für aggressivere Kostenwahl.
    Batch,
}

impl Profile {
    /// Parst einen Profilnamen (case-insensitive); None bei unbekanntem Wert.
    pub fn parse(s: &str) -> Option<Profile> {
        match s.to_ascii_lowercase().as_str() {
            "interactive" => Some(Profile::Interactive),
            "balanced" => Some(Profile::Balanced),
            "batch" => Some(Profile::Batch),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RoutingConfig {
    /// Profil, wenn der Request keinen `x-llmux-profile`-Header setzt.
    pub default_profile: Profile,
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
    /// Obergrenze der Cache-Zeilen. None = unbegrenzt (nur TTL-basierte Eviction).
    #[serde(default)]
    pub max_entries: Option<usize>,
    /// Intervall des Hintergrund-Sweeps (abgelaufene Einträge + Zeilenlimit).
    pub eviction_interval_seconds: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ttl_seconds: 1800,
            max_conversation_messages: 3,
            max_entries: None,
            eviction_interval_seconds: 300,
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
    /// Auch statisch injizierten `system`-/`assistant`-Content gegen die Patterns
    /// scannen. Standard `false`: dieser Kontext ist Client-Boilerplate, kein
    /// User-Payload, und würde sonst spurious `local_only` erzwingen (#23).
    #[serde(default)]
    pub scan_system: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub base_url: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Mehrere Keys mit Gewicht und optionaler Modell-Allow/Deny-Liste. Bei Last-
    /// verteilung wird gewichtet-zufällig gewählt; bei Key-Fehlern rotiert der Router
    /// zum nächsten Key, bevor er auf ein anderes Modell ausweicht. Leer = `api_key_env`.
    #[serde(default)]
    pub keys: Vec<ProviderKey>,
    /// Läuft lokal (kein Cloud-Versand) — relevant für Privacy-Routing.
    #[serde(default)]
    pub local: bool,
    /// Protokoll des Providers: OpenAI-kompatibel (Standard) oder nativ Anthropic.
    #[serde(default)]
    pub kind: ProviderKind,
    /// Request-Felder, die dieser Provider nicht unterstützt und die vor dem
    /// Weiterleiten entfernt werden (z. B. `frequency_penalty` bei manchen Backends).
    #[serde(default)]
    pub strip_params: Vec<String>,
    /// Provider cached wiederholte Prompt-Prefixe (Anthropic `cache_control`,
    /// OpenAI automatisches Prefix-Caching). Aktiviert den Prefix-Rabatt in der
    /// Routing-Kostenschätzung (#24) — die reale Kostenabrechnung bleibt unberührt.
    #[serde(default)]
    pub prompt_caching: bool,
    /// Anteil, zu dem der gecachte Prefix bei der Routing-Schätzung berechnet wird
    /// (0.0–1.0). Default 0.1 ≈ Anthropic Cache-Read. Greift nur bei `prompt_caching`.
    #[serde(default = "default_cache_billed_fraction")]
    pub cache_billed_fraction: f64,
}

/// Ein API-Key-Slot eines Providers (Env-Variable + Gewicht + Modellfilter).
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderKey {
    /// Env-Variable, die den API-Key hält.
    pub env: String,
    /// Relatives Gewicht für die gewichtet-zufällige Auswahl.
    #[serde(default = "default_weight")]
    pub weight: f64,
    /// Nur für diese Modelle nutzbar (leer = alle).
    #[serde(default)]
    pub allow: Vec<String>,
    /// Für diese Modelle gesperrt.
    #[serde(default)]
    pub deny: Vec<String>,
}

fn default_weight() -> f64 {
    1.0
}

fn default_cache_billed_fraction() -> f64 {
    0.1
}

/// Backend-Protokoll eines Providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// OpenAI-kompatibel (`POST /chat/completions`, Bearer-Auth).
    #[default]
    Openai,
    /// Nativ Anthropic (`POST /messages`, `x-api-key` + `anthropic-version`).
    Anthropic,
}

/// Bekannte Modell-Fähigkeiten. `tools`/`json_schema`/`vision` werden aus dem
/// Request automatisch als Anforderung erkannt; alle bekannten Namen können von
/// Task-Regeln über `require_capabilities` verlangt werden. Unbekannte Namen in der
/// Config werden von [`Config::validate`] abgewiesen (#31).
pub const KNOWN_CAPABILITIES: &[&str] = &[
    "tools",
    "json_schema",
    "vision",
    "streaming",
    "streaming_usage",
    "large_context",
    "reasoning",
    "strict_tool_schema",
];

#[derive(Debug, Clone, Deserialize)]
pub struct ModelEntry {
    pub provider: String,
    pub model: String,
    /// Qualitätsstufe: 1 = billig/lokal … 5 = Top-Reasoning.
    pub tier: u8,
    /// Kontextfenster in Tokens.
    pub context: u64,
    /// Kompatibilitäts-Sugar: entspricht der Capability `tools` (#31).
    #[serde(default)]
    pub supports_tools: bool,
    /// Explizite Fähigkeiten dieses Modells (siehe [`KNOWN_CAPABILITIES`]). `tools`
    /// ist auch über `supports_tools` ausdrückbar.
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    /// Zusätzlich zu den Provider-weiten `strip_params` für dieses Modell zu entfernende Felder.
    #[serde(default)]
    pub strip_params: Vec<String>,
}

impl ModelEntry {
    pub fn target(&self) -> Target {
        Target {
            provider: self.provider.clone(),
            model: self.model.clone(),
        }
    }

    /// True, wenn das Modell die Fähigkeit `cap` bietet. `tools` schließt das
    /// Kompatibilitäts-Flag `supports_tools` ein (#31).
    pub fn has_capability(&self, cap: &str) -> bool {
        if cap == "tools" && self.supports_tools {
            return true;
        }
        self.capabilities.iter().any(|c| c == cap)
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

/// Projekt-skopierte Routing-Regeln (#33). Werden vor der Auswahl mit der Task-
/// Policy verschmolzen; alle Felder sind optional und ändern das Default-Verhalten
/// nur, wenn gesetzt. Erzwungene Overrides umgehen diese Regeln nicht.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ProjectProfile {
    /// Erzwingt lokale Provider (wie Privacy-`local_only`), zusätzlich zur Task-Regel.
    pub local_only: bool,
    /// Hebt den Qualitäts-Floor an: effektives `min_tier` = max(Task, Projekt).
    pub min_tier: Option<u8>,
    /// Nur diese Provider sind erlaubt (leer = keine Einschränkung).
    pub require_providers: Vec<String>,
    /// Diese Provider sind für das Projekt gesperrt.
    pub forbid_providers: Vec<String>,
}

impl ProjectProfile {
    /// True, wenn `provider` unter den Projektregeln zulässig ist.
    pub fn allows_provider(&self, provider: &str) -> bool {
        if self.forbid_providers.iter().any(|p| p == provider) {
            return false;
        }
        self.require_providers.is_empty() || self.require_providers.iter().any(|p| p == provider)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TaskRule {
    /// Mindest-Tier für diese Aufgabe (Qualitäts-Floor).
    pub min_tier: u8,
    /// Aufgabe braucht zwingend Tool-Calling.
    #[serde(default)]
    pub require_tools: bool,
    /// Aufgabe verlangt zusätzliche Modell-Fähigkeiten (siehe [`KNOWN_CAPABILITIES`]). (#31)
    #[serde(default)]
    pub require_capabilities: Vec<String>,
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

    /// True, wenn der Provider für genau dieses Modell nutzbar ist: aktiviert und
    /// mindestens ein für das Modell freigegebener Key gesetzt (oder Provider ohne
    /// Auth). Deckt sich mit `providers::resolve_keys`, sodass der Selektor kein
    /// Modell wählt, dessen Provider global „ready" ist, aber für dieses Modell
    /// keinen brauchbaren Key hat — der Fehler fiele sonst erst beim Forwarding auf (#27).
    pub fn provider_ready_for_model(&self, provider: &str, model: &str) -> bool {
        let Some(p) = self.providers.get(provider) else {
            return false;
        };
        if !p.enabled {
            return false;
        }
        !crate::providers::resolve_keys(p, model).is_empty()
    }

    pub fn provider_is_local(&self, provider: &str) -> bool {
        self.providers.get(provider).map(|p| p.local).unwrap_or(false)
    }

    /// Projekt-Profil für den `x-llmux-project`-Wert (None = kein Scope). (#33)
    pub fn project_profile(&self, name: &str) -> Option<&ProjectProfile> {
        self.projects.get(name)
    }

    pub fn provider_kind(&self, provider: &str) -> ProviderKind {
        self.providers.get(provider).map(|p| p.kind).unwrap_or_default()
    }

    /// Anteil, zu dem der gecachte Prompt-Prefix bei der Routing-Schätzung berechnet
    /// wird. `1.0` (voller Preis), wenn der Provider kein Prompt-Caching nutzt (#24).
    pub fn prompt_cache_billed_fraction(&self, provider: &str) -> f64 {
        self.providers
            .get(provider)
            .filter(|p| p.prompt_caching)
            .map(|p| p.cache_billed_fraction.clamp(0.0, 1.0))
            .unwrap_or(1.0)
    }

    /// Löst einen logischen Modell-Alias auf das Zielmodell auf (None = kein Alias).
    pub fn resolve_alias(&self, name: &str) -> Option<&str> {
        self.aliases.get(name).map(String::as_str)
    }

    /// True, wenn `name` ein Katalogmodell bezeichnet (`model` oder `provider/model`).
    fn is_catalog_model(&self, name: &str) -> bool {
        self.models
            .iter()
            .any(|m| m.model == name || format!("{}/{}", m.provider, m.model) == name)
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

        // 3. Unbekannte Capability-Namen in Katalog und Task-Regeln abweisen (#31).
        let known_cap = |c: &String| KNOWN_CAPABILITIES.contains(&c.as_str());
        for m in &self.models {
            for c in m.capabilities.iter().filter(|c| !known_cap(c)) {
                errors.push(format!(
                    "Modell '{}' deklariert unbekannte Capability '{c}'",
                    m.model
                ));
            }
        }
        for (key, rule) in &self.classification {
            for c in rule.require_capabilities.iter().filter(|c| !known_cap(c)) {
                errors.push(format!(
                    "task_type '{key}' verlangt unbekannte Capability '{c}'"
                ));
            }
        }

        // 4. Jede Routing-Anforderung ist erfüllbar: mindestens ein Modell, das
        //    Tier-Floor, Tool-/Capability- und Local-Zwang gleichzeitig erfüllt.
        for (key, rule) in &self.classification {
            let satisfiable = self.models.iter().any(|m| {
                m.tier >= rule.min_tier
                    && (!rule.require_tools || m.has_capability("tools"))
                    && rule.require_capabilities.iter().all(|c| m.has_capability(c))
                    && (!rule.local_only || self.provider_is_local(&m.provider))
            });
            if !satisfiable {
                errors.push(format!(
                    "task_type '{key}' ist nicht erfüllbar: kein Modell mit tier>={}{}{}{}",
                    rule.min_tier,
                    if rule.require_tools { " + Tool-Support" } else { "" },
                    if rule.require_capabilities.is_empty() {
                        String::new()
                    } else {
                        format!(" + Capabilities {:?}", rule.require_capabilities)
                    },
                    if rule.local_only { " + lokalem Provider" } else { "" },
                ));
            }
        }

        // 5. Jeder Alias zeigt auf ein existierendes Katalogmodell.
        for (alias, target) in &self.aliases {
            if !self.is_catalog_model(target) {
                errors.push(format!(
                    "Alias '{alias}' zeigt auf unbekanntes Modell '{target}'"
                ));
            }
        }

        // 6. Projekt-Scopes referenzieren nur definierte Provider.
        for (name, profile) in &self.projects {
            for p in profile.require_providers.iter().chain(&profile.forbid_providers) {
                if !self.providers.contains_key(p) {
                    errors.push(format!(
                        "Projekt '{name}' referenziert unbekannten Provider '{p}'"
                    ));
                }
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

    #[test]
    fn resolves_and_validates_aliases() {
        let yaml = format!("{VALID}aliases:\n  best: \"big\"\n  cheap: \"local-small\"\n");
        let cfg = parse(&yaml);
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.resolve_alias("best"), Some("big"));
        assert_eq!(cfg.resolve_alias("nope"), None);
    }

    #[test]
    fn rejects_unknown_capability_name() {
        let yaml = VALID.replace(
            r#"supports_tools: true, input_per_mtok: 0.0, output_per_mtok: 0.0 }"#,
            r#"supports_tools: true, capabilities: ["telepathy"], input_per_mtok: 0.0, output_per_mtok: 0.0 }"#,
        );
        let err = parse(&yaml).validate().unwrap_err().to_string();
        assert!(err.contains("telepathy") && err.contains("unbekannte Capability"), "got: {err}");
    }

    #[test]
    fn task_require_capability_satisfiable_and_unsatisfiable() {
        // 'big' (tier5) bekommt vision; code_review (tier>=3) darf vision verlangen.
        let ok = VALID
            .replace(
                r#"supports_tools: true, input_per_mtok: 1.0, output_per_mtok: 1.0 }"#,
                r#"supports_tools: true, capabilities: ["vision"], input_per_mtok: 1.0, output_per_mtok: 1.0 }"#,
            )
            .replace(
                "code_review:       { min_tier: 3 }",
                r#"code_review:       { min_tier: 3, require_capabilities: ["vision"] }"#,
            );
        assert!(parse(&ok).validate().is_ok());

        // simple_text verlangt vision, aber kein Modell kann es -> nicht erfüllbar.
        let bad = VALID.replace(
            "simple_text:       { min_tier: 1 }",
            r#"simple_text:       { min_tier: 1, require_capabilities: ["vision"] }"#,
        );
        let err = parse(&bad).validate().unwrap_err().to_string();
        assert!(err.contains("simple_text") && err.contains("nicht erfüllbar"), "got: {err}");
    }

    #[test]
    fn rejects_alias_to_unknown_model() {
        let yaml = format!("{VALID}aliases:\n  best: \"ghost-model\"\n");
        let err = parse(&yaml).validate().unwrap_err().to_string();
        assert!(err.contains("Alias 'best'") && err.contains("ghost-model"), "got: {err}");
    }
}
