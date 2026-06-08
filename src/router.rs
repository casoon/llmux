//! Dynamischer Modell-Selektor.
//!
//! Entscheidet pro Request anhand von Aufgabe (Qualitäts-Floor), agentischen
//! Anforderungen (Tool-Use), Kontextgröße, Privacy und aktuellem Budgetdruck,
//! welches Modell genutzt wird — und wählt aus den gültigen Kandidaten das
//! günstigste. Bei Budgetdruck wird die Tier-Obergrenze dynamisch gesenkt
//! (graceful downgrade), statt sofort abzulehnen.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::config::{Config, ModelEntry, Profile, ProjectProfile};
use crate::cost;
use crate::logging::Store;

/// In-Memory-Gedächtnis pro Agent-Session (Tool-Call-Konsistenz im Loop).
#[derive(Default)]
pub struct SessionStore {
    inner: Mutex<HashMap<String, StickyChoice>>,
}

#[derive(Clone)]
struct StickyChoice {
    provider: String,
    model: String,
    tier: u8,
}

impl SessionStore {
    pub fn get(&self, id: &str) -> Option<(String, String, u8)> {
        self.inner
            .lock()
            .unwrap()
            .get(id)
            .map(|c| (c.provider.clone(), c.model.clone(), c.tier))
    }
    pub fn set(&self, id: &str, provider: &str, model: &str, tier: u8) {
        self.inner.lock().unwrap().insert(
            id.to_string(),
            StickyChoice {
                provider: provider.into(),
                model: model.into(),
                tier,
            },
        );
    }
}

pub struct SelectInput<'a> {
    pub task_key: &'a str,
    pub requires_tools: bool,
    pub input_tokens: u64,
    /// Cachebarer Prompt-Prefix (Teilmenge von `input_tokens`). Wird bei Providern
    /// mit Prompt-Caching für die Kostenschätzung rabattiert (#24).
    pub cached_prefix_tokens: u64,
    pub session: Option<&'a str>,
    /// Projekt-Scope-Regeln, die mit der Task-Policy verschmolzen werden (#33).
    pub project: Option<&'a ProjectProfile>,
    /// Aus dem Request abgeleitete Pflicht-Capabilities jenseits von Tools (#31),
    /// z. B. `json_schema`/`vision`. Werden mit `require_capabilities` der Task-Regel
    /// vereinigt; nur Modelle mit allen geforderten Fähigkeiten bleiben Kandidaten.
    pub req_capabilities: &'a [String],
    /// Routing-Profil (#30): `Interactive` ordnet nach erwarteter Latenz statt
    /// nach Kosten; alle harten Filter und die Budgetschranke bleiben unberührt.
    pub profile: Profile,
}

#[derive(Debug)]
pub struct Selection {
    /// Geordnete Kette: erstes Element = primär, danach Fallbacks (alle gültig).
    pub chain: Vec<ModelEntry>,
    pub tier: u8,
    /// Budgetdruck erzwang ein Tier unter dem Aufgaben-Floor.
    pub degraded: bool,
    pub budget_pressure: f64,
    pub expected_output_tokens: u64,
}

#[derive(Debug)]
pub enum SelectError {
    /// Keine Regel für diesen task_type.
    UnknownTask(String),
    /// Kein Modell erfüllt die harten Bedingungen (Tools/Local/Kontext/Provider).
    NoCandidate(String),
    /// Selbst das günstigste Modell sprengt das Restbudget.
    BudgetExceeded,
}

pub fn select(
    cfg: &Config,
    store: &Store,
    sessions: &SessionStore,
    input: &SelectInput,
) -> Result<Selection, SelectError> {
    let rule = cfg
        .classification
        .get(input.task_key)
        .ok_or_else(|| SelectError::UnknownTask(input.task_key.to_string()))?;

    let need_tools = input.requires_tools || rule.require_tools;
    // Pflicht-Capabilities zusammenführen: Tools (über need_tools), die Task-Regel
    // und die aus dem Request abgeleiteten Fähigkeiten (#31).
    let mut required_caps: Vec<&str> = Vec::new();
    if need_tools {
        required_caps.push("tools");
    }
    required_caps.extend(rule.require_capabilities.iter().map(String::as_str));
    required_caps.extend(input.req_capabilities.iter().map(String::as_str));
    // Projekt-Scope verschärft die Task-Policy (#33): erzwingt ggf. local_only und
    // hebt den Qualitäts-Floor an (nie darunter).
    let local_only = rule.local_only || input.project.map(|p| p.local_only).unwrap_or(false);
    let proj_min_tier = input.project.and_then(|p| p.min_tier).unwrap_or(0);
    let eff_min_tier = rule.min_tier.max(proj_min_tier);
    let expected_output =
        ((input.input_tokens as f64) * rule.expected_output_ratio).ceil() as u64;
    let needed_context = input.input_tokens + expected_output;

    // Budgetdruck -> erlaubte Tier-Obergrenze.
    let pressure = budget_pressure(cfg, store);
    let tier_cap = tier_cap_for(cfg, pressure);
    let degraded = tier_cap < eff_min_tier;
    let tier_floor = if degraded { 1 } else { eff_min_tier };

    // Harte Filter: Capabilities, Local, Kontext, Provider-Verfügbarkeit, Projekt-Allow/Deny, Tier-Band.
    let mut candidates: Vec<&ModelEntry> = cfg
        .models
        .iter()
        .filter(|m| required_caps.iter().all(|c| m.has_capability(c)))
        .filter(|m| !local_only || cfg.provider_is_local(&m.provider))
        .filter(|m| m.context >= needed_context)
        .filter(|m| cfg.provider_ready_for_model(&m.provider, &m.model))
        .filter(|m| input.project.map(|p| p.allows_provider(&m.provider)).unwrap_or(true))
        .filter(|m| m.tier >= tier_floor && m.tier <= tier_cap)
        .collect();

    if candidates.is_empty() {
        return Err(SelectError::NoCandidate(format!(
            "task={} caps={:?} local_only={} ctx>={} tier∈[{}..={}]",
            input.task_key, required_caps, local_only, needed_context, tier_floor, tier_cap
        )));
    }

    // Geschätzte Kosten eines Kandidaten. Bei Providern mit Prompt-Caching wird der
    // wiederholte Prefix rabattiert, damit der große statische Anteil das Ranking
    // und die Budget-Schranke nicht verzerrt (#24). Reale Abrechnung bleibt unberührt.
    let est_cost = |m: &ModelEntry| {
        let billed = cfg.prompt_cache_billed_fraction(&m.provider);
        let eff_input =
            cost::effective_input_tokens(input.input_tokens, input.cached_prefix_tokens, billed);
        m.est_cost(eff_input, expected_output)
    };

    // Erwartete Latenz je Modell — nur im `interactive`-Profil aus den Logs geladen.
    let latency_p50 = if input.profile == Profile::Interactive {
        store.model_latency_p50()
    } else {
        HashMap::new()
    };
    // Modelle ohne Latenz-Historie werden im interactive-Profil ans Ende gestellt
    // (keine Evidenz, dass sie schnell sind).
    let exp_latency = |m: &ModelEntry| {
        latency_p50
            .get(&(m.provider.clone(), m.model.clone()))
            .copied()
            .unwrap_or(u64::MAX)
    };

    // Ranking: `interactive` nach erwarteter Latenz (Kosten als Tiebreak), sonst
    // cheapest-viable. Danach ggf. lokale Modelle bevorzugen (local_first), dann
    // höheres Tier. Harte Filter und Budget sind bereits angewandt bzw. folgen.
    let local_first = cfg.privacy.local_first;
    candidates.sort_by(|a, b| {
        let ca = est_cost(a);
        let cb = est_cost(b);
        let cost_cmp = ca.partial_cmp(&cb).unwrap_or(std::cmp::Ordering::Equal);
        let primary = if input.profile == Profile::Interactive {
            exp_latency(a).cmp(&exp_latency(b)).then(cost_cmp)
        } else {
            cost_cmp
        };
        primary
            .then_with(|| {
                if local_first {
                    let la = cfg.provider_is_local(&a.provider);
                    let lb = cfg.provider_is_local(&b.provider);
                    lb.cmp(&la)
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .then(b.tier.cmp(&a.tier))
    });

    // Session-Stickiness: gewähltes Modell des Loops bevorzugen, falls weiterhin gültig.
    if let Some(sid) = input.session {
        if let Some((p, m, _)) = sessions.get(sid) {
            if let Some(pos) = candidates
                .iter()
                .position(|c| c.provider == p && c.model == m)
            {
                let sticky = candidates.remove(pos);
                candidates.insert(0, sticky);
            }
        }
    }

    // Budgetschranke: Kandidaten, deren Schätzkosten das Restbudget sprengen, raus.
    let remaining = remaining_budget(cfg, store);
    let chain: Vec<ModelEntry> = candidates
        .iter()
        .filter(|m| est_cost(m) <= remaining)
        .map(|m| (*m).clone())
        .collect();

    if chain.is_empty() {
        return Err(SelectError::BudgetExceeded);
    }

    let primary = &chain[0];
    let tier = primary.tier;

    if let Some(sid) = input.session {
        sessions.set(sid, &primary.provider, &primary.model, tier);
    }

    Ok(Selection {
        chain,
        tier,
        degraded,
        budget_pressure: pressure,
        expected_output_tokens: expected_output,
    })
}

/// Aktueller Budgetdruck (für Logging bei erzwungenem Modell).
pub fn current_pressure(cfg: &Config, store: &Store) -> f64 {
    budget_pressure(cfg, store)
}

/// Höchste Budgetauslastung (Tag vs. Monat), 0.0 wenn keine Limits gesetzt.
fn budget_pressure(cfg: &Config, store: &Store) -> f64 {
    let mut p: f64 = 0.0;
    if let Some(d) = cfg.budgets.daily_max_usd {
        if d > 0.0 {
            p = p.max(store.spent_today() / d);
        }
    }
    if let Some(m) = cfg.budgets.monthly_max_usd {
        if m > 0.0 {
            p = p.max(store.spent_this_month() / m);
        }
    }
    p
}

fn tier_cap_for(cfg: &Config, pressure: f64) -> u8 {
    let mut cap = u8::MAX;
    for rule in &cfg.budgets.pressure_downgrade {
        if pressure >= rule.at {
            cap = cap.min(rule.max_tier);
        }
    }
    cap
}

/// Verbleibendes Budget (kleineres aus Tages-/Monatsrest). f64::INFINITY ohne Limit.
pub fn remaining_budget(cfg: &Config, store: &Store) -> f64 {
    let mut rem = f64::INFINITY;
    if let Some(d) = cfg.budgets.daily_max_usd {
        rem = rem.min((d - store.spent_today()).max(0.0));
    }
    if let Some(m) = cfg.budgets.monthly_max_usd {
        rem = rem.min((m - store.spent_this_month()).max(0.0));
    }
    rem
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::RequestLog;

    // Provider ohne api_key_env sind immer ready; local_p ist lokal.
    const BASE: &str = r#"
server: { host: "127.0.0.1", port: 0 }
providers:
  local_p: { enabled: true, base_url: "http://localhost/v1", local: true }
  cloud_p: { enabled: true, base_url: "https://example.com/v1" }
models:
  - { provider: local_p, model: "local-small", tier: 1, context: 8000, supports_tools: false, input_per_mtok: 0.0, output_per_mtok: 0.0 }
  - { provider: cloud_p, model: "cheap",       tier: 1, context: 8000, supports_tools: true,  input_per_mtok: 0.1, output_per_mtok: 0.1 }
  - { provider: cloud_p, model: "mid",         tier: 3, context: 8000, supports_tools: true,  input_per_mtok: 1.0, output_per_mtok: 1.0 }
  - { provider: cloud_p, model: "big",         tier: 5, context: 200000, supports_tools: true, input_per_mtok: 10.0, output_per_mtok: 10.0 }
classification:
  simple_text:       { min_tier: 1, expected_output_ratio: 1.0 }
  summarize:         { min_tier: 1, expected_output_ratio: 1.0 }
  code_review:       { min_tier: 3, expected_output_ratio: 1.0 }
  architecture:      { min_tier: 4, expected_output_ratio: 1.0 }
  private_sensitive: { min_tier: 1, local_only: true, expected_output_ratio: 1.0 }
"#;

    fn cfg(extra: &str) -> Config {
        serde_yaml::from_str(&format!("{extra}{BASE}")).expect("fixture parses")
    }

    fn store() -> Store {
        Store::open(":memory:").expect("in-memory db")
    }

    fn input(task_key: &str, requires_tools: bool, input_tokens: u64) -> SelectInput<'_> {
        SelectInput { task_key, requires_tools, input_tokens, cached_prefix_tokens: 0, session: None, project: None, req_capabilities: &[], profile: Profile::Balanced }
    }

    fn spend(store: &Store, usd: f64) {
        store
            .insert(&RequestLog { real_cost_usd: usd, ..Default::default() })
            .unwrap();
    }

    // Erfolgreiche Antwort von `model` (Provider "p") mit gegebener Latenz loggen.
    fn spend_latency(store: &Store, model: &str, ms: u64) {
        store
            .insert(&RequestLog {
                provider: "p".into(),
                model: model.into(),
                status: 200,
                latency_ms: ms,
                ..Default::default()
            })
            .unwrap();
    }

    #[test]
    fn cheapest_viable_wins() {
        let c = cfg("");
        let s = store();
        let sel = select(&c, &s, &SessionStore::default(), &input("simple_text", false, 100)).unwrap();
        assert_eq!(sel.chain[0].model, "local-small");
        assert!(!sel.degraded);
    }

    #[test]
    fn tier_floor_excludes_low_tiers() {
        let c = cfg("");
        let s = store();
        let sel = select(&c, &s, &SessionStore::default(), &input("code_review", false, 100)).unwrap();
        // tier-1/2 ausgeschlossen, günstigstes der Tier>=3-Kandidaten = mid.
        assert_eq!(sel.chain[0].model, "mid");
        assert!(sel.chain.iter().all(|m| m.tier >= 3));
    }

    #[test]
    fn tool_filter_excludes_non_tool_models() {
        let c = cfg("");
        let s = store();
        let sel = select(&c, &s, &SessionStore::default(), &input("simple_text", true, 100)).unwrap();
        // local-small kann keine Tools -> günstigstes tool-fähiges Tier-1 = cheap.
        assert_eq!(sel.chain[0].model, "cheap");
        assert!(sel.chain.iter().all(|m| m.supports_tools));
    }

    #[test]
    fn local_only_restricts_to_local_provider() {
        let c = cfg("");
        let s = store();
        let sel = select(&c, &s, &SessionStore::default(), &input("private_sensitive", false, 100)).unwrap();
        assert!(sel.chain.iter().all(|m| c.provider_is_local(&m.provider)));
        assert_eq!(sel.chain[0].model, "local-small");
    }

    #[test]
    fn context_window_must_fit() {
        let c = cfg("");
        let s = store();
        // 100k Input + 100k erwarteter Output = 200k -> nur big passt.
        let sel = select(&c, &s, &SessionStore::default(), &input("simple_text", false, 100_000)).unwrap();
        assert_eq!(sel.chain[0].model, "big");
    }

    #[test]
    fn budget_pressure_downgrades_with_degraded_flag() {
        let c = cfg("budgets:\n  daily_max_usd: 1.0\n  pressure_downgrade:\n    - { at: 0.5, max_tier: 2 }\n");
        let s = store();
        spend(&s, 0.6); // 60 % Auslastung -> tier_cap = 2
        let sel = select(&c, &s, &SessionStore::default(), &input("code_review", false, 100)).unwrap();
        assert!(sel.degraded, "code_review (min_tier 3) muss unter Druck degradieren");
        assert!(sel.chain.iter().all(|m| m.tier <= 2));
        assert!(sel.budget_pressure >= 0.5);
    }

    #[test]
    fn budget_exceeded_when_cheapest_too_expensive() {
        let c = cfg("budgets:\n  daily_max_usd: 0.000001\n");
        let s = store();
        // code_review schließt das kostenlose lokale Modell aus; mid/big sprengen Restbudget.
        let err = select(&c, &s, &SessionStore::default(), &input("code_review", false, 1000)).unwrap_err();
        assert!(matches!(err, SelectError::BudgetExceeded));
    }

    #[test]
    fn unknown_task_errors() {
        let c = cfg("");
        let s = store();
        let err = select(&c, &s, &SessionStore::default(), &input("nope", false, 100)).unwrap_err();
        assert!(matches!(err, SelectError::UnknownTask(_)));
    }

    #[test]
    fn prompt_cache_discount_flips_ranking() {
        // Zwei tool-fähige Tier-3-Modelle: 'plain' ist pro Token günstiger, 'cached'
        // teurer — aber mit Prompt-Caching. Bei großem wiederholtem Prefix gewinnt
        // 'cached', weil der marginale Anteil rabattiert geschätzt wird (#24).
        let yaml = r#"
server: { host: "127.0.0.1", port: 0 }
providers:
  plain:  { enabled: true, base_url: "https://a/v1" }
  cached: { enabled: true, base_url: "https://b/v1", prompt_caching: true, cache_billed_fraction: 0.1 }
models:
  - { provider: plain,  model: "plain-m",  tier: 3, context: 200000, supports_tools: true, input_per_mtok: 1.0, output_per_mtok: 0.0 }
  - { provider: cached, model: "cached-m", tier: 3, context: 200000, supports_tools: true, input_per_mtok: 2.0, output_per_mtok: 0.0 }
classification:
  simple_text:       { min_tier: 3, expected_output_ratio: 0.0 }
  summarize:         { min_tier: 3 }
  code_review:       { min_tier: 3 }
  architecture:      { min_tier: 3 }
  private_sensitive: { min_tier: 3 }
"#;
        let c: Config = serde_yaml::from_str(yaml).expect("fixture parses");
        let s = store();
        // 10k Input, davon 9k gecachter Prefix.
        let cached_in = SelectInput {
            task_key: "simple_text", requires_tools: false,
            input_tokens: 10_000, cached_prefix_tokens: 9_000, session: None, project: None, req_capabilities: &[], profile: Profile::Balanced,
        };
        let sel = select(&c, &s, &SessionStore::default(), &cached_in).unwrap();
        assert_eq!(sel.chain[0].model, "cached-m");

        // Ohne gecachten Prefix gewinnt wieder das pro Token günstigere Modell.
        let no_prefix = SelectInput { cached_prefix_tokens: 0, ..cached_in };
        let sel = select(&c, &s, &SessionStore::default(), &no_prefix).unwrap();
        assert_eq!(sel.chain[0].model, "plain-m");
    }

    #[test]
    fn model_specific_key_denylist_excludes_candidate() {
        // Provider mit genau einem Key, dessen allow-Liste nur 'other' erlaubt.
        // Das günstigere 'gated' hat damit keinen nutzbaren Key und darf nicht
        // gewählt werden, obwohl der Provider global einen gesetzten Key hat (#27).
        std::env::set_var("LLMUX_TEST_GATED_KEY", "k");
        let yaml = r#"
server: { host: "127.0.0.1", port: 0 }
providers:
  local_p: { enabled: true, base_url: "http://localhost/v1", local: true }
  gated_p:
    enabled: true
    base_url: "https://example.com/v1"
    keys:
      - { env: "LLMUX_TEST_GATED_KEY", weight: 1.0, allow: ["other"] }
models:
  - { provider: gated_p, model: "gated", tier: 1, context: 8000, supports_tools: true, input_per_mtok: 0.1, output_per_mtok: 0.1 }
  - { provider: gated_p, model: "other", tier: 1, context: 8000, supports_tools: true, input_per_mtok: 9.0, output_per_mtok: 9.0 }
classification:
  simple_text:       { min_tier: 1, expected_output_ratio: 1.0 }
  summarize:         { min_tier: 1 }
  code_review:       { min_tier: 3 }
  architecture:      { min_tier: 4 }
  private_sensitive: { min_tier: 1, local_only: true, expected_output_ratio: 1.0 }
"#;
        let c: Config = serde_yaml::from_str(yaml).expect("fixture parses");
        let s = store();
        let sel = select(&c, &s, &SessionStore::default(), &input("simple_text", false, 100)).unwrap();
        // Trotz günstigerem 'gated' bleibt nur 'other' mit nutzbarem Key.
        assert!(sel.chain.iter().all(|m| m.model == "other"));
        std::env::remove_var("LLMUX_TEST_GATED_KEY");
    }

    #[test]
    fn project_min_tier_raises_quality_floor() {
        let c = cfg("");
        let s = store();
        let proj = ProjectProfile { min_tier: Some(3), ..Default::default() };
        let inp = SelectInput {
            task_key: "simple_text", requires_tools: false, input_tokens: 100,
            cached_prefix_tokens: 0, session: None, project: Some(&proj), req_capabilities: &[], profile: Profile::Balanced,
        };
        let sel = select(&c, &s, &SessionStore::default(), &inp).unwrap();
        // Ohne Projekt gewänne local-small (tier1); der Projekt-Floor erzwingt tier>=3 -> mid.
        assert_eq!(sel.chain[0].model, "mid");
        assert!(sel.chain.iter().all(|m| m.tier >= 3));
    }

    #[test]
    fn project_forbid_provider_excludes_candidates() {
        let c = cfg("");
        let s = store();
        let proj = ProjectProfile { forbid_providers: vec!["cloud_p".into()], ..Default::default() };
        // code_review (tier>=3) hat nur cloud-Modelle -> mit forbid cloud_p kein Kandidat.
        let inp = SelectInput {
            task_key: "code_review", requires_tools: false, input_tokens: 100,
            cached_prefix_tokens: 0, session: None, project: Some(&proj), req_capabilities: &[], profile: Profile::Balanced,
        };
        let err = select(&c, &s, &SessionStore::default(), &inp).unwrap_err();
        assert!(matches!(err, SelectError::NoCandidate(_)));
    }

    #[test]
    fn required_capability_filters_models() {
        // Zwei tier-1-Modelle; nur 'rich' kann json_schema. Verlangt der Request die
        // Capability, fällt das günstigere 'plain' raus (#31).
        let yaml = r#"
server: { host: "127.0.0.1", port: 0 }
providers:
  p: { enabled: true, base_url: "https://x/v1" }
models:
  - { provider: p, model: "plain", tier: 1, context: 8000, supports_tools: true, input_per_mtok: 0.1, output_per_mtok: 0.1 }
  - { provider: p, model: "rich",  tier: 1, context: 8000, supports_tools: true, capabilities: ["json_schema"], input_per_mtok: 9.0, output_per_mtok: 9.0 }
classification:
  simple_text:       { min_tier: 1 }
  summarize:         { min_tier: 1 }
  code_review:       { min_tier: 1 }
  architecture:      { min_tier: 1 }
  private_sensitive: { min_tier: 1 }
"#;
        let c: Config = serde_yaml::from_str(yaml).expect("fixture parses");
        let s = store();
        let caps = vec!["json_schema".to_string()];
        let inp = SelectInput {
            task_key: "simple_text", requires_tools: false, input_tokens: 100,
            cached_prefix_tokens: 0, session: None, project: None, req_capabilities: &caps, profile: Profile::Balanced,
        };
        let sel = select(&c, &s, &SessionStore::default(), &inp).unwrap();
        assert_eq!(sel.chain[0].model, "rich");
        assert!(sel.chain.iter().all(|m| m.has_capability("json_schema")));

        // Ohne die Anforderung gewinnt wieder das günstigere 'plain'.
        let inp2 = SelectInput {
            task_key: "simple_text", requires_tools: false, input_tokens: 100,
            cached_prefix_tokens: 0, session: None, project: None, req_capabilities: &[], profile: Profile::Balanced,
        };
        let sel2 = select(&c, &s, &SessionStore::default(), &inp2).unwrap();
        assert_eq!(sel2.chain[0].model, "plain");
    }

    #[test]
    fn interactive_profile_prefers_latency_but_respects_hard_filters() {
        // 'fast-pricey' ist schneller, aber teurer und kann kein json_schema;
        // 'slow-cheap' ist günstiger, langsamer und kann json_schema (#30/#31).
        let yaml = r#"
server: { host: "127.0.0.1", port: 0 }
providers:
  p: { enabled: true, base_url: "https://x/v1" }
models:
  - { provider: p, model: "slow-cheap",  tier: 1, context: 8000, supports_tools: true, capabilities: ["json_schema"], input_per_mtok: 0.1, output_per_mtok: 0.1 }
  - { provider: p, model: "fast-pricey", tier: 1, context: 8000, supports_tools: true, input_per_mtok: 5.0, output_per_mtok: 5.0 }
classification:
  simple_text:       { min_tier: 1 }
  summarize:         { min_tier: 1 }
  code_review:       { min_tier: 1 }
  architecture:      { min_tier: 1 }
  private_sensitive: { min_tier: 1 }
"#;
        let c: Config = serde_yaml::from_str(yaml).expect("fixture parses");
        let s = store();
        // Latenz-Historie: slow-cheap langsam (2000ms), fast-pricey schnell (200ms).
        for _ in 0..3 {
            spend_latency(&s, "slow-cheap", 2000);
            spend_latency(&s, "fast-pricey", 200);
        }

        let mk = |profile, caps: &'static [String]| SelectInput {
            task_key: "simple_text", requires_tools: false, input_tokens: 100,
            cached_prefix_tokens: 0, session: None, project: None, req_capabilities: caps, profile,
        };

        // Balanced (Default): günstigstes Modell.
        let bal = select(&c, &s, &SessionStore::default(), &mk(Profile::Balanced, &[])).unwrap();
        assert_eq!(bal.chain[0].model, "slow-cheap");

        // Interactive: niedrigste erwartete Latenz, trotz höherer Kosten.
        let inter = select(&c, &s, &SessionStore::default(), &mk(Profile::Interactive, &[])).unwrap();
        assert_eq!(inter.chain[0].model, "fast-pricey");

        // Harter Filter gewinnt: verlangt der Request json_schema, bleibt nur das
        // langsamere 'slow-cheap' — Latenzpräferenz darf das nicht aushebeln.
        let caps = vec!["json_schema".to_string()];
        let inter_caps = SelectInput {
            task_key: "simple_text", requires_tools: false, input_tokens: 100,
            cached_prefix_tokens: 0, session: None, project: None, req_capabilities: &caps, profile: Profile::Interactive,
        };
        let sel = select(&c, &s, &SessionStore::default(), &inter_caps).unwrap();
        assert_eq!(sel.chain[0].model, "slow-cheap");
    }

    #[test]
    fn session_stickiness_prefers_prior_choice() {
        let c = cfg("");
        let s = store();
        let sessions = SessionStore::default();
        sessions.set("s1", "cloud_p", "mid", 3);
        let inp = SelectInput { task_key: "simple_text", requires_tools: false, input_tokens: 100, cached_prefix_tokens: 0, session: Some("s1"), project: None, req_capabilities: &[], profile: Profile::Balanced };
        let sel = select(&c, &s, &sessions, &inp).unwrap();
        // Trotz günstigerem local-small bleibt der Loop bei mid.
        assert_eq!(sel.chain[0].model, "mid");
    }
}
