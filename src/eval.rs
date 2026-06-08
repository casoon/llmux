//! Deterministische Routing-Eval-Fixtures (#32).
//!
//! Beantwortet die zwei Kernfragen der Routing-Logik gegen einen festen,
//! repräsentativen Modellkatalog: Werden Prompts korrekt klassifiziert, und
//! routen sie in die erwartete Tier-/Fähigkeitsklasse? Die Fixtures laufen durch
//! dieselbe Pipeline wie der HTTP-Handler (Privacy hat Vorrang vor der
//! Keyword-Klassifikation → Selektor) und brechen, sobald Klassifikator, Task-
//! Regeln, Capabilities oder Katalog unbeabsichtigt driften.
//!
//! Dies ist **kein** Live-Provider-Benchmark — es läuft offline und ohne Keys.
//!
//! ## Eine neue Routing-Fixture hinzufügen
//!
//! 1. Einen Eintrag zu [`fixtures`] ergänzen: `name`, ein OpenAI-Request-`body`
//!    (per `json!`), und die Erwartungen `expect_task`, `expect_tools`,
//!    `expect_min_tier`, `expect_model`.
//! 2. Das erwartete Modell ergibt sich aus [`EVAL_CONFIG`] (günstigstes gültiges
//!    Modell nach den Selektor-Invarianten). Reicht der Katalog nicht, lieber den
//!    Katalog erweitern, als auf fragile Provider-Namen zu setzen.
//! 3. Budget-/Druck-abhängige Fälle gehören in einen eigenen Test mit eigener
//!    Config und vorbelegtem `Store` (siehe `budget_pressure_downgrades_selection`).

use serde_json::{json, Value};

use crate::classifier::{self, TaskType};
use crate::config::{Config, Profile};
use crate::cost;
use crate::logging::{RequestLog, Store};
use crate::privacy;
use crate::router::{self, SelectInput, SessionStore};

/// Repräsentativer Katalog für die Eval. Die Preise sind so gestaffelt, dass das
/// günstigste gültige Modell je Aufgabe eindeutig ist:
/// - `local-small` (tier1, lokal, keine Tools, gratis) — billigster Allrounder
/// - `cheap` (tier1, Tools) — billigstes tool-fähiges Modell
/// - `mid` (tier3, Tools) — billigstes Modell ab tier3
/// - `big` (tier5, Tools) — einziges Modell ab tier4
/// - `long` (tier3, 2M Kontext) — einziges Modell für sehr große Kontexte
const EVAL_CONFIG: &str = r#"
server: { host: "127.0.0.1", port: 0 }
privacy:
  block_cloud_patterns: ["sk-", "private_key"]
providers:
  local_p: { enabled: true, base_url: "http://localhost/v1", local: true }
  cloud_p: { enabled: true, base_url: "https://example.com/v1" }
models:
  - { provider: local_p, model: "local-small", tier: 1, context: 8000,    supports_tools: false, input_per_mtok: 0.0,  output_per_mtok: 0.0 }
  - { provider: cloud_p, model: "cheap",       tier: 1, context: 32000,   supports_tools: true,  input_per_mtok: 0.1,  output_per_mtok: 0.1 }
  - { provider: cloud_p, model: "mid",         tier: 3, context: 128000,  supports_tools: true,  input_per_mtok: 1.0,  output_per_mtok: 1.0 }
  - { provider: cloud_p, model: "big",         tier: 5, context: 200000,  supports_tools: true,  input_per_mtok: 10.0, output_per_mtok: 10.0 }
  - { provider: cloud_p, model: "long",        tier: 3, context: 2000000, supports_tools: true,  input_per_mtok: 2.0,  output_per_mtok: 2.0 }
classification:
  simple_text:       { min_tier: 1, expected_output_ratio: 1.0 }
  summarize:         { min_tier: 1, expected_output_ratio: 1.0 }
  code_review:       { min_tier: 3, expected_output_ratio: 1.0 }
  architecture:      { min_tier: 4, expected_output_ratio: 1.0 }
  private_sensitive: { min_tier: 1, local_only: true, expected_output_ratio: 1.0 }
"#;

struct Fixture {
    name: &'static str,
    body: Value,
    expect_task: TaskType,
    expect_tools: bool,
    expect_min_tier: u8,
    expect_model: &'static str,
}

fn fixtures() -> Vec<Fixture> {
    vec![
        Fixture {
            name: "simple short chat",
            body: json!({ "messages": [{ "role": "user", "content": "Wie ist das Wetter heute?" }] }),
            expect_task: TaskType::SimpleText,
            expect_tools: false,
            expect_min_tier: 1,
            expect_model: "local-small",
        },
        Fixture {
            name: "summarization",
            body: json!({ "messages": [{ "role": "user", "content": "Fasse zusammen, worum es in diesem Text geht." }] }),
            expect_task: TaskType::Summarize,
            expect_tools: false,
            expect_min_tier: 1,
            expect_model: "local-small",
        },
        Fixture {
            name: "code review",
            body: json!({ "messages": [{ "role": "user", "content": "Bitte review this function und fixe den bug." }] }),
            expect_task: TaskType::CodeReview,
            expect_tools: false,
            expect_min_tier: 3,
            expect_model: "mid",
        },
        Fixture {
            name: "architecture/design",
            body: json!({ "messages": [{ "role": "user", "content": "Erkläre die Architektur und die Trade-offs dieses Systems." }] }),
            expect_task: TaskType::Architecture,
            expect_tools: false,
            expect_min_tier: 4,
            expect_model: "big",
        },
        Fixture {
            name: "private/sensitive",
            body: json!({ "messages": [{ "role": "user", "content": "Hier ist mein Token sk-abc123, bitte hilf mir." }] }),
            expect_task: TaskType::PrivateSensitive,
            expect_tools: false,
            expect_min_tier: 1,
            expect_model: "local-small",
        },
        Fixture {
            name: "tool-using agent",
            body: json!({
                "messages": [{ "role": "user", "content": "Erledige die nächste Aufgabe." }],
                "tools": [{ "type": "function", "function": { "name": "search", "description": "find things", "parameters": { "type": "object" } } }]
            }),
            expect_task: TaskType::SimpleText,
            expect_tools: true,
            expect_min_tier: 1,
            expect_model: "cheap",
        },
        Fixture {
            name: "long-context",
            // ~180k Input-Tokens (+ erwarteter Output) passen nur in das 2M-Modell.
            body: json!({ "messages": [{ "role": "user", "content": "lorem ipsum dolor ".repeat(40_000) }] }),
            expect_task: TaskType::SimpleText,
            expect_tools: false,
            expect_min_tier: 1,
            expect_model: "long",
        },
    ]
}

/// Text der letzten `n` `user`-Messages — spiegelt `api::extract_user_text`, damit
/// die Eval exakt dieselbe Klassifikator-Oberfläche sieht wie der Handler.
fn user_text(body: &Value, n: usize) -> String {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return String::new();
    };
    let users: Vec<&Value> = messages
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        .collect();
    let start = users.len().saturating_sub(n.max(1));
    let mut out = String::new();
    for msg in &users[start..] {
        match msg.get("content") {
            Some(Value::String(s)) => {
                out.push_str(s);
                out.push('\n');
            }
            Some(Value::Array(parts)) => {
                for p in parts {
                    if let Some(t) = p.get("text").and_then(Value::as_str) {
                        out.push_str(t);
                        out.push('\n');
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Klassifiziert wie der Handler: Privacy hat Vorrang vor der Keyword-Klassifikation.
fn classify_request(cfg: &Config, body: &Value) -> TaskType {
    if privacy::request_is_sensitive(
        body,
        &cfg.privacy.block_cloud_patterns,
        cfg.privacy.scan_system,
    ) {
        TaskType::PrivateSensitive
    } else {
        classifier::classify(&user_text(body, cfg.classifier.user_messages))
    }
}

#[test]
fn routing_fixtures_classify_and_select_as_expected() {
    let cfg: Config = serde_yaml::from_str(EVAL_CONFIG).expect("eval config parses");
    cfg.validate().expect("eval config must validate");

    for f in fixtures() {
        let store = Store::open(":memory:").unwrap();
        let task = classify_request(&cfg, &f.body);
        let requires_tools = classifier::requires_tools(&f.body);

        assert_eq!(task, f.expect_task, "[{}] task_type", f.name);
        assert_eq!(requires_tools, f.expect_tools, "[{}] requires_tools", f.name);

        let rule = cfg
            .classification
            .get(task.as_key())
            .unwrap_or_else(|| panic!("[{}] keine Regel für {}", f.name, task.as_key()));
        assert_eq!(rule.min_tier, f.expect_min_tier, "[{}] min_tier", f.name);

        let sel = router::select(
            &cfg,
            &store,
            &SessionStore::default(),
            &SelectInput {
                task_key: task.as_key(),
                requires_tools,
                input_tokens: cost::estimate_request_tokens(&f.body),
                cached_prefix_tokens: cost::estimate_cached_prefix_tokens(&f.body),
                session: None,
                project: None,
                req_capabilities: &[],
                profile: Profile::Balanced,
            },
        )
        .unwrap_or_else(|e| panic!("[{}] select fehlgeschlagen: {e:?}", f.name));

        assert_eq!(sel.chain[0].model, f.expect_model, "[{}] gewähltes Modell", f.name);
        assert!(!sel.degraded, "[{}] unerwartetes Budget-Downgrade", f.name);
    }
}

/// Katalog wie [`EVAL_CONFIG`], aber mit Tagesbudget und Druck-Downgrade: ab 50 %
/// Auslastung sinkt die Tier-Obergrenze auf 2.
const EVAL_PRESSURE_CONFIG: &str = r#"
server: { host: "127.0.0.1", port: 0 }
budgets:
  daily_max_usd: 1.0
  pressure_downgrade:
    - { at: 0.5, max_tier: 2 }
providers:
  local_p: { enabled: true, base_url: "http://localhost/v1", local: true }
  cloud_p: { enabled: true, base_url: "https://example.com/v1" }
models:
  - { provider: local_p, model: "local-small", tier: 1, context: 8000,   supports_tools: false, input_per_mtok: 0.0, output_per_mtok: 0.0 }
  - { provider: cloud_p, model: "cheap",       tier: 1, context: 32000,  supports_tools: true,  input_per_mtok: 0.1, output_per_mtok: 0.1 }
  - { provider: cloud_p, model: "mid",         tier: 3, context: 128000, supports_tools: true,  input_per_mtok: 1.0, output_per_mtok: 1.0 }
  - { provider: cloud_p, model: "big",         tier: 5, context: 200000, supports_tools: true,  input_per_mtok: 10.0, output_per_mtok: 10.0 }
classification:
  simple_text:       { min_tier: 1, expected_output_ratio: 1.0 }
  summarize:         { min_tier: 1, expected_output_ratio: 1.0 }
  code_review:       { min_tier: 3, expected_output_ratio: 1.0 }
  architecture:      { min_tier: 4, expected_output_ratio: 1.0 }
  private_sensitive: { min_tier: 1, local_only: true, expected_output_ratio: 1.0 }
"#;

#[test]
fn budget_pressure_downgrades_selection() {
    let cfg: Config = serde_yaml::from_str(EVAL_PRESSURE_CONFIG).expect("config parses");
    let store = Store::open(":memory:").unwrap();
    // 80 % Tagesbudget verbraucht -> Druck 0.8 >= 0.5 -> Tier-Cap 2.
    store
        .insert(&RequestLog { real_cost_usd: 0.8, ..Default::default() })
        .unwrap();

    let body = json!({ "messages": [{ "role": "user", "content": "review this function, fix the bug" }] });
    let task = classify_request(&cfg, &body);
    assert_eq!(task, TaskType::CodeReview, "Vorbedingung: code_review");

    let sel = router::select(
        &cfg,
        &store,
        &SessionStore::default(),
        &SelectInput {
            task_key: task.as_key(),
            requires_tools: false,
            input_tokens: cost::estimate_request_tokens(&body),
            cached_prefix_tokens: 0,
            session: None,
            project: None,
            req_capabilities: &[],
            profile: Profile::Balanced,
        },
    )
    .expect("unter Druck bleibt ein gültiges Tier-<=2-Modell");

    // Harte Floor (min_tier 3) weicht dem Budgetdruck: degradiert auf Tier <= 2.
    assert!(sel.degraded, "code_review muss unter Budgetdruck degradieren");
    assert!(sel.chain.iter().all(|m| m.tier <= 2), "Tier-Cap 2 verletzt");
}
