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

    // Kostenoptimierung: günstigstes Modell zuerst. Bei Gleichstand ggf. lokale
    // Modelle bevorzugen (local_first), dann höheres Tier.
    let local_first = cfg.privacy.local_first;
    candidates.sort_by(|a, b| {
        let ca = a.est_cost(input.input_tokens, expected_output);
        let cb = b.est_cost(input.input_tokens, expected_output);
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
        .filter(|m| m.est_cost(input.input_tokens, expected_output) <= remaining)
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
