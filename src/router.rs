//! Dynamischer Modell-Selektor.
//!
//! Entscheidet pro Request anhand von Aufgabe (Qualitäts-Floor), agentischen
//! Anforderungen (Tool-Use), Kontextgröße, Privacy und aktuellem Budgetdruck,
//! welches Modell genutzt wird — und wählt aus den gültigen Kandidaten das
//! günstigste. Bei Budgetdruck wird die Tier-Obergrenze dynamisch gesenkt
//! (graceful downgrade), statt sofort abzulehnen.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::config::{Config, ModelEntry};
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
    let local_only = rule.local_only;
    let expected_output =
        ((input.input_tokens as f64) * rule.expected_output_ratio).ceil() as u64;
    let needed_context = input.input_tokens + expected_output;

    // Budgetdruck -> erlaubte Tier-Obergrenze.
    let pressure = budget_pressure(cfg, store);
    let tier_cap = tier_cap_for(cfg, pressure);
    let degraded = tier_cap < rule.min_tier;
    let tier_floor = if degraded { 1 } else { rule.min_tier };

    // Harte Filter: Tools, Local, Kontext, Provider-Verfügbarkeit, Tier-Band.
    let mut candidates: Vec<&ModelEntry> = cfg
        .models
        .iter()
        .filter(|m| !need_tools || m.supports_tools)
        .filter(|m| !local_only || cfg.provider_is_local(&m.provider))
        .filter(|m| m.context >= needed_context)
        .filter(|m| cfg.provider_ready(&m.provider))
        .filter(|m| m.tier >= tier_floor && m.tier <= tier_cap)
        .collect();

    if candidates.is_empty() {
        return Err(SelectError::NoCandidate(format!(
            "task={} tools={} local_only={} ctx>={} tier∈[{}..={}]",
            input.task_key, need_tools, local_only, needed_context, tier_floor, tier_cap
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

    // Kostenoptimierung: günstigstes Modell zuerst. Bei Gleichstand ggf. lokale
    // Modelle bevorzugen (local_first), dann höheres Tier.
    let local_first = cfg.privacy.local_first;
    candidates.sort_by(|a, b| {
        let ca = est_cost(a);
        let cb = est_cost(b);
        ca.partial_cmp(&cb)
            .unwrap_or(std::cmp::Ordering::Equal)
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
fn remaining_budget(cfg: &Config, store: &Store) -> f64 {
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
        SelectInput { task_key, requires_tools, input_tokens, cached_prefix_tokens: 0, session: None }
    }

    fn spend(store: &Store, usd: f64) {
        store
            .insert(&RequestLog { real_cost_usd: usd, ..Default::default() })
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
            input_tokens: 10_000, cached_prefix_tokens: 9_000, session: None,
        };
        let sel = select(&c, &s, &SessionStore::default(), &cached_in).unwrap();
        assert_eq!(sel.chain[0].model, "cached-m");

        // Ohne gecachten Prefix gewinnt wieder das pro Token günstigere Modell.
        let no_prefix = SelectInput { cached_prefix_tokens: 0, ..cached_in };
        let sel = select(&c, &s, &SessionStore::default(), &no_prefix).unwrap();
        assert_eq!(sel.chain[0].model, "plain-m");
    }

    #[test]
    fn session_stickiness_prefers_prior_choice() {
        let c = cfg("");
        let s = store();
        let sessions = SessionStore::default();
        sessions.set("s1", "cloud_p", "mid", 3);
        let inp = SelectInput { task_key: "simple_text", requires_tools: false, input_tokens: 100, cached_prefix_tokens: 0, session: Some("s1") };
        let sel = select(&c, &s, &sessions, &inp).unwrap();
        // Trotz günstigerem local-small bleibt der Loop bei mid.
        assert_eq!(sel.chain[0].model, "mid");
    }
}
