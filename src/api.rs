//! HTTP-Layer: OpenAI-kompatibler Endpoint + State + dynamische Request-Pipeline
//! mit Cache, Retry/Fehlerklassifikation und Per-Request-Overrides.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use futures_util::StreamExt;
use serde_json::{json, Value};

use crate::config::{Config, ModelEntry, Profile, ProviderKind, RetryConfig};
use crate::logging::{RequestLog, Store};
use crate::router::{self, SelectError, SelectInput, SessionStore};
use crate::{cache, classifier, cost, privacy, providers};

pub struct AppState {
    pub cfg: Config,
    pub http: reqwest::Client,
    pub store: Store,
    pub sessions: SessionStore,
}

/// Routing-Metadaten, unabhängig davon ob dynamisch gewählt oder per Header erzwungen.
#[derive(Debug)]
struct Plan {
    chain: Vec<ModelEntry>,
    tier: u8,
    degraded: bool,
    pressure: f64,
    expected_output: u64,
    /// Modell wurde per Override erzwungen (Policy-Dimension, #28).
    forced: bool,
    /// Aufgelöster Projekt-Scope-Name aus `x-llmux-project` (für Logging, #33).
    project: Option<String>,
    /// Der Request erwartete Tool-Calling (Qualitätssignal, #29).
    tools_expected: bool,
}

/// Daten für den Semantic-Cache-Store (#14): unter welchem Modell-Scope und mit welchem
/// Query-Embedding eine erfolgreiche Antwort abgelegt wird. `None` = Semantic-Cache aus.
struct SemanticCache {
    scope: String,
    embedding: Vec<f32>,
}

/// Klassifikation eines Provider-Fehlers für die Retry-/Fallback-Strategie.
enum Outcome {
    Transient,    // 5xx/408 -> gleicher Key erneut (mit Backoff), dann nächstes Modell
    KeyExhausted, // 401/402/403/429 -> nächster Key, dann nächstes Modell
    BadRequest,   // sonstige 4xx -> Abbruch, kein Fallback (Request ist fehlerhaft)
}

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/chat/completions", post(chat_completions))
        // Read-only Stats-API (#18) — gleicher llmux_key wie der Proxy.
        .route("/api/stats/overview", get(stats_overview))
        .route("/api/stats/requests", get(stats_requests))
        .route("/api/stats/models", get(stats_models))
        .route("/api/stats/policy", get(stats_policy))
        .route("/api/stats/projects", get(stats_projects))
        .route("/api/stats/quality", get(stats_quality))
        .route("/api/stats/latency", get(stats_latency))
        .route("/api/stats/budget-series", get(stats_budget_series))
        .with_state(state)
        // Fallback: das eingebettete Dashboard. Registriert NACH /healthz, /v1/* und
        // /api/* — diese behalten Vorrang, nur unbelegte Pfade landen hier. (#20)
        .fallback(serve_dashboard)
}

/// Statisch eingebettete Dashboard-Build-Ausgabe (`dist/dashboard`), zur Compile-Zeit
/// ins Binary aufgenommen — kein Node zur Laufzeit nötig (#20).
#[derive(rust_embed::RustEmbed)]
#[folder = "dist/dashboard"]
struct DashboardAssets;

const DASHBOARD_NOT_BUILT: &str = "<!doctype html><meta charset=utf-8><title>llmux</title>\
<body style=\"font-family:system-ui;background:#111;color:#ddd;padding:2rem\">\
<h1>llmux dashboard not built</h1><p>Run <code>npm install &amp;&amp; npm run build</code> \
before building the binary to embed the dashboard. The proxy and Stats API are unaffected.</p>";

/// Content-Type aus der Dateiendung (die Astro-Ausgabe ist html/css/js plus ggf. Assets).
fn asset_mime(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("json") | Some("map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        _ => "application/octet-stream",
    }
}

fn dashboard_index() -> Response {
    match DashboardAssets::get("index.html") {
        Some(f) => (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            f.data.into_owned(),
        )
            .into_response(),
        None => (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            DASHBOARD_NOT_BUILT,
        )
            .into_response(),
    }
}

/// Liefert das eingebettete Dashboard: `/` und seitenartige Pfade → `index.html`,
/// vorhandene Asset-Dateien mit passendem MIME, fehlende Asset-Dateien → 404.
async fn serve_dashboard(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.is_empty() {
        return dashboard_index();
    }
    match DashboardAssets::get(path) {
        Some(f) => ([(header::CONTENT_TYPE, asset_mime(path))], f.data.into_owned()).into_response(),
        // Fehlende Datei mit Endung -> 404; seitenartiger Pfad -> Index (Single-Page).
        None if path.contains('.') => StatusCode::NOT_FOUND.into_response(),
        None => dashboard_index(),
    }
}

/// Query-Parameter für `/api/stats/requests`.
#[derive(Debug, Deserialize)]
struct RequestsQuery {
    limit: Option<usize>,
}

// Hinweis: Die read-only Stats-API (`/api/stats/*`) ist bewusst NICHT auth-pflichtig.
// llmux ist eine lokale Instanz; das eingebettete Dashboard (#19/#20) ruft diese
// Endpunkte vom Browser aus same-origin auf. Der Proxy (`/v1/...`) bleibt auth-pflichtig.
async fn stats_overview(State(state): State<Arc<AppState>>) -> Response {
    let mut v = state.store.stats_overview();
    if let Some(o) = v.as_object_mut() {
        o.insert("cost_today".into(), json!(state.store.spent_today()));
        o.insert("cost_month".into(), json!(state.store.spent_this_month()));
        o.insert(
            "budget_pressure".into(),
            json!(router::current_pressure(&state.cfg, &state.store)),
        );
        o.insert("daily_max_usd".into(), json!(state.cfg.budgets.daily_max_usd));
        o.insert("monthly_max_usd".into(), json!(state.cfg.budgets.monthly_max_usd));
    }
    Json(v).into_response()
}

async fn stats_requests(
    State(state): State<Arc<AppState>>,
    Query(q): Query<RequestsQuery>,
) -> Response {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    Json(json!({ "requests": state.store.recent_requests(limit) })).into_response()
}

async fn stats_models(State(state): State<Arc<AppState>>) -> Response {
    Json(json!({ "models": state.store.model_stats() })).into_response()
}

async fn stats_policy(State(state): State<Arc<AppState>>) -> Response {
    Json(state.store.policy_stats()).into_response()
}

async fn stats_projects(State(state): State<Arc<AppState>>) -> Response {
    Json(json!({ "projects": state.store.project_stats() })).into_response()
}

async fn stats_quality(State(state): State<Arc<AppState>>) -> Response {
    Json(state.store.quality_stats()).into_response()
}

async fn stats_latency(State(state): State<Arc<AppState>>) -> Response {
    Json(state.store.latency_stats()).into_response()
}

/// Budget-Zeitreihe (#19): Kosten je Stunde der letzten 24 h plus die Cap-Schwellen,
/// damit das Dashboard den Budgetdruck über die Zeit statt nur als Skalar zeigt.
async fn stats_budget_series(State(state): State<Arc<AppState>>) -> Response {
    Json(json!({
        "buckets": state.store.budget_series(24),
        "daily_max_usd": state.cfg.budgets.daily_max_usd,
        "monthly_max_usd": state.cfg.budgets.monthly_max_usd,
        "spent_today": state.store.spent_today(),
    }))
    .into_response()
}

async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> Response {
    if !check_auth(&state.cfg, &headers) {
        return error_response(StatusCode::UNAUTHORIZED, "ungültiger oder fehlender API-Key");
    }

    let started = Instant::now();
    let tool = header(&headers, "x-llmux-tool").unwrap_or_else(|| "unknown".into());
    let session = header(&headers, "x-llmux-session");
    let project = header(&headers, "x-llmux-project");
    // Routing-Profil: Header vor Config-Default; unbekannte Werte fallen auf Default (#30).
    let profile = header(&headers, "x-llmux-profile")
        .and_then(|p| Profile::parse(&p))
        .unwrap_or(state.cfg.routing.default_profile);
    let is_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);

    // Beim Streaming reale Usage anfordern (finaler Chunk trägt prompt/completion_tokens).
    if is_stream {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream_options".into(), json!({ "include_usage": true }));
        }
    }

    // Per-Request-Overrides. x-llmux-model erzwingt direkt (Alias wird aufgelöst);
    // das `model`-Feld des Requests erzwingt nur, wenn es ein definierter Alias ist
    // (sonst greift wie bisher die dynamische Auswahl).
    let force_model = match header(&headers, "x-llmux-model") {
        Some(m) => Some(state.cfg.resolve_alias(&m).unwrap_or(&m).to_string()),
        None => body
            .get("model")
            .and_then(Value::as_str)
            .and_then(|m| state.cfg.resolve_alias(m))
            .map(String::from),
    };
    let no_cache = header_bool(&headers, "x-llmux-no-cache");
    let no_fallback = header_bool(&headers, "x-llmux-no-fallback");
    let max_cost = header(&headers, "x-llmux-max-cost").and_then(|s| s.parse::<f64>().ok());

    // Request verstehen
    let input_tokens = cost::estimate_request_tokens(&body);
    let cached_prefix_tokens = cost::estimate_cached_prefix_tokens(&body);
    let requires_tools = classifier::requires_tools(&body);
    // Aus dem Request abgeleitete Pflicht-Capabilities jenseits von Tools (#31).
    let req_capabilities = classifier::request_capabilities(&body);
    // Privacy prüft eine breitere Oberfläche (User-/Tool-Content + Tool-Schemas) als
    // die Keyword-Klassifikation, die nur die letzte(n) User-Message(s) betrachtet
    // (#22/#23). Privacy hat Vorrang und erzwingt private_sensitive (-> local_only).
    let task = if privacy::request_is_sensitive(
        &body,
        &state.cfg.privacy.block_cloud_patterns,
        state.cfg.privacy.scan_system,
    ) {
        classifier::TaskType::PrivateSensitive
    } else {
        let user_text = extract_user_text(&body, state.cfg.classifier.user_messages);
        // Optionaler LLM-Klassifikator (#13); fällt bei Fehler/Timeout auf die Regeln zurück.
        classifier::classify_with_llm(&state.http, state.cfg.classifier.llm.as_ref(), &user_text)
            .await
    };
    let task_key = task.as_key();

    // Plan bestimmen: erzwungenes Modell oder dynamische Auswahl
    let mut plan = match build_plan(&state, &force_model, task_key, requires_tools, &req_capabilities, input_tokens, cached_prefix_tokens, &session, &project, profile) {
        Ok(p) => p,
        Err(resp) => {
            log_failure(&state.store, &tool, &session, &project, task_key, input_tokens, "plan", resp.0.as_u16(), &resp.1, force_model.is_some());
            return error_response(resp.0, &resp.1);
        }
    };

    if no_fallback {
        plan.chain.truncate(1);
    }

    let primary = plan.chain[0].clone();

    // Streaming für native Anthropic-Provider ist noch nicht implementiert (nur Nicht-Stream).
    if is_stream && state.cfg.provider_kind(&primary.provider) == ProviderKind::Anthropic {
        let msg = "Streaming wird für native Anthropic-Provider noch nicht unterstützt";
        log_failure(&state.store, &tool, &session, &project, task_key, input_tokens, "anthropic_stream", 400, msg, force_model.is_some());
        return error_response(StatusCode::BAD_REQUEST, msg);
    }

    // Cache-Voraussetzungen (opt-out per Header, History-Guard). Streaming-Antworten
    // werden separat gecacht (eigener Key-Suffix, SSE-Replay).
    let cacheable = state.cfg.cache.enabled
        && !no_cache
        && cache::conversation_len(&body) <= state.cfg.cache.max_conversation_messages;
    let cache_key = cacheable.then(|| {
        let k = cache::cache_key(&primary, &body);
        if is_stream {
            format!("{k}#stream")
        } else {
            k
        }
    });

    // Cross-cutting Pre-Hooks: Budget-Gate, dann Exact-Cache-Lookup (erster Halt gewinnt).
    // `ctx.semantic` ist hier None — der Semantic-Schritt folgt unten, weil er erst nach
    // einem Exact-Miss einen async Embedding-Fetch auslösen soll (Effizienz wie #14). (#15)
    let pipeline = PluginPipeline::default();
    let ctx = PluginCtx {
        state: &state,
        entry: &primary,
        task_key,
        tool: &tool,
        session: &session,
        project: &project,
        tier: plan.tier,
        pressure: plan.pressure,
        forced: force_model.is_some(),
        tools_expected: plan.tools_expected,
        expected_output: plan.expected_output,
        input_tokens,
        cached_prefix_tokens,
        max_cost,
        cache_key: cache_key.as_ref(),
        semantic: None,
        is_stream,
        started,
    };
    if let Some(resp) = pipeline.run_pre(&ctx) {
        return resp;
    }

    // Semantic-Cache (zweite Stufe, #14): erst nach bestandenem Budget-Gate und Exact-Miss
    // das Embedding holen (einziger async-Schritt), damit Exact-Treffer und Budget-
    // Ablehnungen keinen Embedding-Call kosten. Treffer schließt kurz; sonst wird das
    // Embedding für den CachePlugin-`post`-Store gemerkt.
    let semantic = if cacheable && !is_stream {
        match state.cfg.cache.semantic.as_ref().filter(|s| s.enabled) {
            Some(scfg) => {
                let text = extract_user_text(&body, usize::MAX);
                match cache::fetch_embedding(&state.http, scfg, &text).await {
                    Some(emb) => {
                        let scope = cache::semantic_scope(&primary);
                        if let Some(cached) =
                            state.store.semantic_cache_lookup(&scope, &emb, scfg.threshold)
                        {
                            return cache_hit_response(&ctx, cached);
                        }
                        Some(SemanticCache { scope, embedding: emb })
                    }
                    None => None,
                }
            }
            None => None,
        }
    } else {
        None
    };

    // `ctx` wird ab hier nicht mehr verwendet; seine Borrows (u. a. cache_key) enden, sodass
    // body/plan/cache_key/semantic ins Forwarding moven können.

    tracing::info!(
        task = task_key, tier = plan.tier, degraded = plan.degraded,
        tools = requires_tools, input_tokens,
        pressure = format!("{:.2}", plan.pressure),
        candidates = plan.chain.len(), forced = force_model.is_some(),
        project = project.as_deref().unwrap_or("-"),
        "routing"
    );

    forward_with_retries(&state, body, plan, task_key, &tool, &session, is_stream, input_tokens, cache_key, semantic, started).await
}

/// Baut den Plan: entweder erzwungenes Modell (Header) oder dynamische Auswahl.
#[allow(clippy::too_many_arguments)]
fn build_plan(
    state: &AppState,
    force_model: &Option<String>,
    task_key: &str,
    requires_tools: bool,
    req_capabilities: &[String],
    input_tokens: u64,
    cached_prefix_tokens: u64,
    session: &Option<String>,
    project: &Option<String>,
    profile: Profile,
) -> Result<Plan, (StatusCode, String)> {
    // Projekt-Scope auflösen (#33). Unbekannte Projektnamen ändern nichts.
    let profile_rules = project
        .as_deref()
        .and_then(|p| state.cfg.project_profile(p));

    if let Some(name) = force_model {
        let entry = state
            .cfg
            .models
            .iter()
            .find(|m| m.model == *name || format!("{}/{}", m.provider, m.model) == *name)
            .cloned()
            .ok_or((
                StatusCode::BAD_REQUEST,
                format!("erzwungenes Modell '{name}' nicht im Katalog"),
            ))?;

        // Ein erzwungenes Modell darf die günstigste-gültige Auswahl überspringen,
        // nicht aber die harten Sicherheits-/Fähigkeitsbedingungen des Selektors (#25).
        let rule = state.cfg.classification.get(task_key);
        let need_tools = requires_tools || rule.map(|r| r.require_tools).unwrap_or(false);
        // Projekt-Scope verschärft auch erzwungene Overrides (#33, konsistent zu #25/#28).
        let local_only =
            rule.map(|r| r.local_only).unwrap_or(false) || profile_rules.map(|p| p.local_only).unwrap_or(false);
        let expected_output = match rule {
            Some(r) => ((input_tokens as f64) * r.expected_output_ratio).ceil() as u64,
            None => input_tokens,
        };

        if !state
            .cfg
            .provider_ready_for_model(&entry.provider, &entry.model)
        {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("erzwungenes Modell '{name}': Provider '{}' nicht bereit oder kein nutzbarer Key", entry.provider),
            ));
        }
        // Geforderte Capabilities (Tools + Task-Regel + Request) auch bei Overrides erzwingen (#31).
        let mut required_caps: Vec<&str> = Vec::new();
        if need_tools {
            required_caps.push("tools");
        }
        if let Some(r) = rule {
            required_caps.extend(r.require_capabilities.iter().map(String::as_str));
        }
        required_caps.extend(req_capabilities.iter().map(String::as_str));
        if let Some(missing) = required_caps.iter().find(|c| !entry.has_capability(c)) {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("erzwungenes Modell '{name}' bietet die geforderte Capability '{missing}' nicht"),
            ));
        }
        if local_only && !state.cfg.provider_is_local(&entry.provider) {
            return Err((
                StatusCode::FORBIDDEN,
                format!("erzwungenes Modell '{name}' ist kein lokaler Provider — '{task_key}' (local_only) abgelehnt"),
            ));
        }
        if let Some(p) = profile_rules {
            if !p.allows_provider(&entry.provider) {
                return Err((
                    StatusCode::FORBIDDEN,
                    format!("erzwungenes Modell '{name}': Provider '{}' durch Projekt-Scope gesperrt", entry.provider),
                ));
            }
        }
        let needed_context = input_tokens + expected_output;
        if entry.context < needed_context {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("erzwungenes Modell '{name}': Kontextfenster {} < benötigt {needed_context}", entry.context),
            ));
        }
        // Budget: auch ein erzwungenes Modell darf das Restbudget nicht sprengen
        // (Prefix-Rabatt konsistent zur Routing-Schätzung).
        let billed = state.cfg.prompt_cache_billed_fraction(&entry.provider);
        let eff_input = cost::effective_input_tokens(input_tokens, cached_prefix_tokens, billed);
        let est = entry.est_cost(eff_input, expected_output);
        let remaining = router::remaining_budget(&state.cfg, &state.store);
        if est > remaining {
            return Err((
                StatusCode::PAYMENT_REQUIRED,
                format!("erzwungenes Modell '{name}': Schätzkosten {est:.5} USD über Restbudget {remaining:.5}"),
            ));
        }

        return Ok(Plan {
            tier: entry.tier,
            chain: vec![entry],
            degraded: false,
            pressure: router::current_pressure(&state.cfg, &state.store),
            expected_output,
            forced: true,
            project: project.clone(),
            tools_expected: requires_tools,
        });
    }

    let select_input = SelectInput {
        task_key,
        requires_tools,
        input_tokens,
        cached_prefix_tokens,
        session: session.as_deref(),
        project: profile_rules,
        req_capabilities,
        profile,
    };
    match router::select(&state.cfg, &state.store, &state.sessions, &select_input) {
        Ok(s) => Ok(Plan {
            chain: s.chain,
            tier: s.tier,
            degraded: s.degraded,
            pressure: s.budget_pressure,
            expected_output: s.expected_output_tokens,
            forced: false,
            project: project.clone(),
            tools_expected: requires_tools,
        }),
        Err(e) => Err(match e {
            SelectError::BudgetExceeded => (
                StatusCode::PAYMENT_REQUIRED,
                "Budgetlimit erreicht — kein Modell im Restbudget".into(),
            ),
            SelectError::NoCandidate(d) => (
                StatusCode::BAD_GATEWAY,
                format!("kein passendes Modell verfügbar ({d})"),
            ),
            SelectError::UnknownTask(t) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("keine Regel für task_type '{t}'"),
            ),
        }),
    }
}

#[allow(clippy::too_many_arguments)]
async fn forward_with_retries(
    state: &Arc<AppState>,
    body: Value,
    plan: Plan,
    task_key: &str,
    tool: &str,
    session: &Option<String>,
    is_stream: bool,
    input_tokens: u64,
    cache_key: Option<String>,
    semantic: Option<SemanticCache>,
    started: Instant,
) -> Response {
    let mut trail: Vec<Value> = Vec::new();
    let mut attempts: u32 = 0;
    let mut last_err = String::from("kein Versuch ausgeführt");
    let mut last_status = StatusCode::BAD_GATEWAY;

    for entry in plan.chain.iter() {
        let target = entry.target();

        // Einsatzbereite Keys für dieses Modell auflösen und gewichtet ordnen.
        let resolved = match state.cfg.providers.get(&target.provider) {
            Some(p) => providers::resolve_keys(p, &target.model),
            None => Vec::new(),
        };
        if resolved.is_empty() {
            last_err = format!("kein nutzbarer API-Key für Provider '{}'", target.provider);
            trail.push(json!({ "provider": target.provider, "model": target.model, "error": last_err.clone() }));
            continue; // nächstes Modell
        }
        let order = providers::order_keys_weighted(resolved, random_unit());

        'keys: for (key_idx, key) in order.into_iter().enumerate() {
            let mut attempt: u32 = 0;
            loop {
                attempts += 1;
                match providers::forward(&state.cfg, &state.http, &target, body.clone(), key.auth.as_deref()).await {
                    Ok(resp) if resp.status().is_success() => {
                        trail.push(json!({ "provider": target.provider, "model": target.model, "key": key_idx, "status": resp.status().as_u16() }));
                        return handle_success(
                            state, resp, entry, &plan, task_key, tool, session,
                            entry.provider != plan.chain[0].provider || entry.model != plan.chain[0].model,
                            is_stream, input_tokens, attempts, trail, cache_key, semantic, started,
                        )
                        .await;
                    }
                    Ok(resp) => {
                        let status = resp.status();
                        last_status = status;
                        let detail = resp.text().await.unwrap_or_default();
                        last_err = format!("{status}: {}", truncate(&detail, 300));
                        trail.push(json!({ "provider": target.provider, "model": target.model, "key": key_idx, "status": status.as_u16(), "error": truncate(&detail, 200) }));

                        match classify_status(status) {
                            Outcome::Transient => {
                                if attempt < state.cfg.retry.max_retries {
                                    let ms = backoff_ms(&state.cfg.retry, attempt);
                                    tracing::warn!(provider = %target.provider, model = %target.model, "transient {status}, retry #{} in {ms}ms", attempt + 1);
                                    tokio::time::sleep(Duration::from_millis(ms)).await;
                                    attempt += 1;
                                    continue;
                                }
                                break 'keys; // transiente Fehler erschöpft -> nächstes Modell
                            }
                            Outcome::KeyExhausted => {
                                tracing::warn!(provider = %target.provider, model = %target.model, key = key_idx, "key-fehler {status}, rotiere Key");
                                break; // nächster Key
                            }
                            Outcome::BadRequest => {
                                // Request ist fehlerhaft -> kein Fallback, Status durchreichen.
                                let _ = state.store.insert(&RequestLog {
                                    tool: tool.into(),
                                    session: session.clone(),
                                    task_type: task_key.into(),
                                    model: entry.model.clone(),
                                    provider: entry.provider.clone(),
                                    tier: plan.tier,
                                    degraded: plan.degraded,
                                    budget_pressure: plan.pressure,
                                    estimated_tokens: input_tokens,
                                    status: status.as_u16(),
                                    attempts,
                                    attempt_trail: serde_json::to_string(&trail).ok(),
                                    error: Some(last_err.clone()),
                                    forced: plan.forced,
                                    project: plan.project.clone(),
                                    ..Default::default()
                                });
                                return error_response(status, &last_err);
                            }
                        }
                    }
                    Err(e) => {
                        last_err = e.to_string();
                        trail.push(json!({ "provider": target.provider, "model": target.model, "key": key_idx, "error": last_err.clone() }));
                        if attempt < state.cfg.retry.max_retries {
                            let ms = backoff_ms(&state.cfg.retry, attempt);
                            tracing::warn!(provider = %target.provider, model = %target.model, "netzwerkfehler, retry #{} in {ms}ms", attempt + 1);
                            tokio::time::sleep(Duration::from_millis(ms)).await;
                            attempt += 1;
                            continue;
                        }
                        break 'keys; // Netzwerkfehler erschöpft -> nächstes Modell
                    }
                }
            }
        }
    }

    let _ = last_status;
    let trail_json = serde_json::to_string(&trail).ok();
    let _ = state.store.insert(&RequestLog {
        tool: tool.into(),
        session: session.clone(),
        task_type: task_key.into(),
        tier: plan.tier,
        degraded: plan.degraded,
        budget_pressure: plan.pressure,
        estimated_tokens: input_tokens,
        attempts,
        attempt_trail: trail_json,
        status: StatusCode::BAD_GATEWAY.as_u16(),
        error: Some(last_err.clone()),
        forced: plan.forced,
        project: plan.project.clone(),
        ..Default::default()
    });
    error_response(
        StatusCode::BAD_GATEWAY,
        &format!("alle Modelle fehlgeschlagen: {last_err}"),
    )
}

#[allow(clippy::too_many_arguments)]
async fn handle_success(
    state: &Arc<AppState>,
    resp: reqwest::Response,
    entry: &ModelEntry,
    plan: &Plan,
    task_key: &str,
    tool: &str,
    session: &Option<String>,
    used_fallback: bool,
    is_stream: bool,
    input_tokens: u64,
    attempts: u32,
    trail: Vec<Value>,
    cache_key: Option<String>,
    semantic: Option<SemanticCache>,
    started: Instant,
) -> Response {
    let status = resp.status();
    let trail_json = serde_json::to_string(&trail).ok();

    if is_stream {
        // Bytes durchreichen, parallel akkumulieren; am Stream-Ende reale Usage
        // aus dem finalen Chunk loggen und (falls cachebar) die SSE-Antwort ablegen.
        return stream_response(
            Arc::clone(state),
            resp,
            status,
            StreamCtx {
                entry: entry.clone(),
                tier: plan.tier,
                degraded: plan.degraded,
                pressure: plan.pressure,
                expected_output: plan.expected_output,
                task_key: task_key.to_string(),
                tool: tool.to_string(),
                session: session.clone(),
                used_fallback,
                forced: plan.forced,
                project: plan.project.clone(),
                tools_expected: plan.tools_expected,
                input_tokens,
                attempts,
                trail_json,
                cache_key,
                started,
            },
        );
    }

    let payload: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                &format!("Antwort des Providers nicht lesbar: {e}"),
            )
        }
    };

    // Native Anthropic-Antwort ins OpenAI-Format übersetzen, bevor Usage/Cache/Logging greifen.
    let payload = if state.cfg.provider_kind(&entry.provider) == ProviderKind::Anthropic {
        let created = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        providers::to_openai_response(&payload, created)
    } else {
        payload
    };

    let prompt_tokens = payload
        .pointer("/usage/prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(input_tokens);
    let completion_tokens = payload
        .pointer("/usage/completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let stop_reason = payload
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
        .map(String::from);
    let tool_call_present = response_has_tool_call(&payload);
    let real_cost = entry.est_cost(prompt_tokens, completion_tokens);

    // Cache-Store + Logging laufen über die post-Hooks der Plugin-Pipeline (#15).
    let out = PluginOutcome {
        status: status.as_u16(),
        used_fallback,
        degraded: plan.degraded,
        estimated_tokens: input_tokens,
        prompt_tokens,
        completion_tokens,
        estimated_cost_usd: entry.est_cost(input_tokens, plan.expected_output),
        real_cost_usd: real_cost,
        attempts,
        attempt_trail: trail_json,
        stop_reason,
        error: None,
        tool_call_present,
        body: serde_json::to_string(&payload).ok(),
    };
    let ctx = PluginCtx {
        state,
        entry,
        task_key,
        tool,
        session,
        project: &plan.project,
        tier: plan.tier,
        pressure: plan.pressure,
        forced: plan.forced,
        tools_expected: plan.tools_expected,
        expected_output: plan.expected_output,
        input_tokens,
        cached_prefix_tokens: 0,
        max_cost: None,
        cache_key: cache_key.as_ref(),
        semantic: semantic.as_ref(),
        is_stream,
        started,
    };
    PluginPipeline::default().run_post(&ctx, &out);

    tracing::info!(
        task = task_key, provider = %entry.provider, model = %entry.model,
        tier = plan.tier, fallback = used_fallback, attempts,
        prompt_tokens, completion_tokens, cost_usd = real_cost, "request ok"
    );

    (status, Json(payload)).into_response()
}

/// Eigentümer-Kontext für das verzögerte Logging/Caching einer Streaming-Antwort,
/// die nach Rückkehr des Handlers weiter gepollt wird.
struct StreamCtx {
    entry: ModelEntry,
    tier: u8,
    degraded: bool,
    pressure: f64,
    expected_output: u64,
    task_key: String,
    tool: String,
    session: Option<String>,
    used_fallback: bool,
    forced: bool,
    project: Option<String>,
    tools_expected: bool,
    input_tokens: u64,
    attempts: u32,
    trail_json: Option<String>,
    cache_key: Option<String>,
    started: Instant,
}

/// Baut die SSE-Antwort: leitet jeden Chunk durch und akkumuliert ihn. Am Ende
/// des Upstream-Streams wird `finalize_stream` aufgerufen (reale Usage + Cache).
fn stream_response(
    app: Arc<AppState>,
    resp: reqwest::Response,
    status: StatusCode,
    ctx: StreamCtx,
) -> Response {
    let init = (Box::pin(resp.bytes_stream()), Vec::<u8>::new(), app, ctx, false);
    let stream = futures_util::stream::unfold(
        init,
        |(mut up, mut acc, app, ctx, finished)| async move {
            if finished {
                return None;
            }
            match up.next().await {
                Some(Ok(chunk)) => {
                    acc.extend_from_slice(&chunk);
                    Some((Ok::<_, reqwest::Error>(chunk), (up, acc, app, ctx, false)))
                }
                Some(Err(e)) => {
                    finalize_stream(&app, &acc, &ctx, Some(e.to_string()));
                    Some((Err(e), (up, acc, app, ctx, true)))
                }
                None => {
                    finalize_stream(&app, &acc, &ctx, None);
                    None
                }
            }
        },
    );

    Response::builder()
        .status(status)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Parst die akkumulierte SSE nach realer Usage und finish_reason, loggt den
/// Request und legt bei Erfolg die vollständige SSE-Antwort im Cache ab.
fn finalize_stream(app: &AppState, acc: &[u8], ctx: &StreamCtx, stream_err: Option<String>) {
    let (pt, ct, finish) = parse_stream_usage(acc);
    let prompt_tokens = pt.unwrap_or(ctx.input_tokens);
    let completion_tokens = ct.unwrap_or(0);
    let tool_call_present = finish.as_deref() == Some("tool_calls");
    let real_cost = ctx.entry.est_cost(prompt_tokens, completion_tokens);

    // Verzögertes Cache-Store + Logging laufen über dieselben post-Hooks wie der
    // non-stream-Pfad (#15). Bei Stream-Fehler verhindert `error` den Cache-Store.
    let out = PluginOutcome {
        status: if stream_err.is_some() { StatusCode::BAD_GATEWAY.as_u16() } else { 200 },
        used_fallback: ctx.used_fallback,
        degraded: ctx.degraded,
        estimated_tokens: ctx.input_tokens,
        prompt_tokens,
        completion_tokens,
        estimated_cost_usd: ctx.entry.est_cost(ctx.input_tokens, ctx.expected_output),
        real_cost_usd: real_cost,
        attempts: ctx.attempts,
        attempt_trail: ctx.trail_json.clone(),
        stop_reason: finish,
        error: stream_err,
        tool_call_present,
        body: Some(String::from_utf8_lossy(acc).into_owned()),
    };
    let pctx = PluginCtx {
        state: app,
        entry: &ctx.entry,
        task_key: &ctx.task_key,
        tool: &ctx.tool,
        session: &ctx.session,
        project: &ctx.project,
        tier: ctx.tier,
        pressure: ctx.pressure,
        forced: ctx.forced,
        tools_expected: ctx.tools_expected,
        expected_output: ctx.expected_output,
        input_tokens: ctx.input_tokens,
        cached_prefix_tokens: 0,
        max_cost: None,
        cache_key: ctx.cache_key.as_ref(),
        semantic: None,
        is_stream: true,
        started: ctx.started,
    };
    PluginPipeline::default().run_post(&pctx, &out);
}

/// Extrahiert prompt-/completion_tokens und finish_reason aus einer SSE-Antwort.
/// Der letzte `data:`-Chunk mit `usage` gewinnt (include_usage liefert ihn am Ende).
fn parse_stream_usage(acc: &[u8]) -> (Option<u64>, Option<u64>, Option<String>) {
    let text = String::from_utf8_lossy(acc);
    let mut prompt = None;
    let mut completion = None;
    let mut finish = None;
    for line in text.lines() {
        let Some(payload) = line.trim_start().strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        if let Some(p) = v.pointer("/usage/prompt_tokens").and_then(Value::as_u64) {
            prompt = Some(p);
        }
        if let Some(c) = v.pointer("/usage/completion_tokens").and_then(Value::as_u64) {
            completion = Some(c);
        }
        if let Some(f) = v.pointer("/choices/0/finish_reason").and_then(Value::as_str) {
            finish = Some(f.to_string());
        }
    }
    (prompt, completion, finish)
}

/// Cache-Treffer für eine Streaming-Anfrage: gespeicherte SSE als Stream zurückspielen.
fn cache_hit_stream_response(ctx: &PluginCtx, cached: String) -> Response {
    let (pt, ct, finish) = parse_stream_usage(cached.as_bytes());
    let _ = ctx.state.store.insert(&RequestLog {
        tool: ctx.tool.into(),
        session: ctx.session.clone(),
        task_type: ctx.task_key.into(),
        model: ctx.entry.model.clone(),
        provider: ctx.entry.provider.clone(),
        tier: ctx.tier,
        budget_pressure: ctx.pressure,
        prompt_tokens: pt.unwrap_or(0),
        completion_tokens: ct.unwrap_or(0),
        latency_ms: ctx.started.elapsed().as_millis() as u64,
        status: 200,
        cache_hit: true,
        forced: ctx.forced,
        project: ctx.project.clone(),
        tools_expected: ctx.tools_expected,
        tool_call_present: finish.as_deref() == Some("tool_calls"),
        ..Default::default()
    });

    tracing::info!(task = ctx.task_key, model = %ctx.entry.model, "cache hit (stream)");
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("x-llmux-cache", "hit")
        .body(Body::from(cached))
        .unwrap()
}

fn cache_hit_response(ctx: &PluginCtx, cached: String) -> Response {
    let payload: Value = serde_json::from_str(&cached).unwrap_or_else(|_| json!({}));
    let prompt_tokens = payload.pointer("/usage/prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
    let completion_tokens = payload.pointer("/usage/completion_tokens").and_then(Value::as_u64).unwrap_or(0);

    let _ = ctx.state.store.insert(&RequestLog {
        tool: ctx.tool.into(),
        session: ctx.session.clone(),
        task_type: ctx.task_key.into(),
        model: ctx.entry.model.clone(),
        provider: ctx.entry.provider.clone(),
        tier: ctx.tier,
        budget_pressure: ctx.pressure,
        prompt_tokens,
        completion_tokens,
        latency_ms: ctx.started.elapsed().as_millis() as u64,
        status: 200,
        cache_hit: true,
        forced: ctx.forced,
        project: ctx.project.clone(),
        tools_expected: ctx.tools_expected,
        tool_call_present: response_has_tool_call(&payload),
        ..Default::default()
    });

    tracing::info!(task = ctx.task_key, model = %ctx.entry.model, "cache hit");
    (
        StatusCode::OK,
        [("x-llmux-cache", "hit")],
        Json(payload),
    )
        .into_response()
}

// ===========================================================================
// Plugin-Pipeline für Cross-cutting Concerns (#15)
//
// Die request-übergreifenden Belange Budget, Cache und Logging sind als geordnete
// Plugin-Liste ausgedrückt. Jedes Plugin hat `pre()` (vor dem Forwarding; darf per
// `Some(Response)` kurzschließen — Cache-Treffer, Budget-Ablehnung) und `post()`
// (nach Vorliegen des Ergebnisses; läuft in umgekehrter Reihenfolge). Ein neues
// Plugin wird durch Eintrag in `PluginPipeline::default` eingehängt, ohne den
// Kern-Request-Pfad zu ändern.
//
// Privacy ist bewusst KEIN Pipeline-Plugin: der Privacy-Scan ist ein Routing-Input
// (erzwingt `private_sensitive`/`local_only` bei der Klassifikation, schließt nie
// kurz und verarbeitet kein Ergebnis nach) und bleibt im Klassifikations-Schritt.
// ===========================================================================

/// Request-invarianter Kontext, den alle Plugins teilen. `entry` ist phasenabhängig:
/// in `pre` das geplante Primärmodell, in `post` das Modell, das geantwortet hat.
struct PluginCtx<'a> {
    state: &'a AppState,
    entry: &'a ModelEntry,
    task_key: &'a str,
    tool: &'a str,
    session: &'a Option<String>,
    project: &'a Option<String>,
    tier: u8,
    pressure: f64,
    forced: bool,
    tools_expected: bool,
    expected_output: u64,
    input_tokens: u64,
    cached_prefix_tokens: u64,
    max_cost: Option<f64>,
    cache_key: Option<&'a String>,
    semantic: Option<&'a SemanticCache>,
    is_stream: bool,
    started: Instant,
}

/// Terminal-Ergebnis eines erfolgreich weitergeleiteten Requests, das die `post`-Hooks
/// (Cache-Store, Logging) verarbeiten. Cache-Treffer und Fehler-Gates loggen selbst in
/// `pre` und durchlaufen `run_post` nicht.
struct PluginOutcome {
    status: u16,
    used_fallback: bool,
    degraded: bool,
    estimated_tokens: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    estimated_cost_usd: f64,
    real_cost_usd: f64,
    attempts: u32,
    attempt_trail: Option<String>,
    stop_reason: Option<String>,
    error: Option<String>,
    tool_call_present: bool,
    /// Serialisierte Antwort für den Cache-Store. Gespeichert wird nur, wenn zusätzlich
    /// `error` None ist und ein `cache_key` vorliegt (siehe `CachePlugin::post`); `None`
    /// hier unterdrückt den Store ebenfalls.
    body: Option<String>,
}

trait Plugin: Send + Sync {
    fn pre(&self, _ctx: &PluginCtx) -> Option<Response> {
        None
    }
    fn post(&self, _ctx: &PluginCtx, _out: &PluginOutcome) {}
}

/// Geordnete Plugin-Kette. `run_pre` läuft vorwärts (erster Halt gewinnt), `run_post`
/// rückwärts.
struct PluginPipeline {
    plugins: Vec<Box<dyn Plugin>>,
}

impl Default for PluginPipeline {
    fn default() -> Self {
        Self {
            plugins: vec![
                Box::new(BudgetPlugin),
                Box::new(CachePlugin),
                Box::new(LoggingPlugin),
            ],
        }
    }
}

impl PluginPipeline {
    fn run_pre(&self, ctx: &PluginCtx) -> Option<Response> {
        for p in &self.plugins {
            if let Some(resp) = p.pre(ctx) {
                return Some(resp);
            }
        }
        None
    }

    fn run_post(&self, ctx: &PluginCtx, out: &PluginOutcome) {
        for p in self.plugins.iter().rev() {
            p.post(ctx, out);
        }
    }
}

/// Budget: lehnt den Request ab (402), wenn die geschätzten Kosten den per-Request-Cap
/// (`x-llmux-max-cost`) übersteigen. Konsistent zur Routing-Schätzung mit Prefix-Rabatt.
struct BudgetPlugin;
impl Plugin for BudgetPlugin {
    fn pre(&self, ctx: &PluginCtx) -> Option<Response> {
        let mc = ctx.max_cost?;
        let billed = ctx.state.cfg.prompt_cache_billed_fraction(&ctx.entry.provider);
        let eff_input = cost::effective_input_tokens(ctx.input_tokens, ctx.cached_prefix_tokens, billed);
        let est = ctx.entry.est_cost(eff_input, ctx.expected_output);
        if est > mc {
            let msg = format!("Schätzkosten {est:.5} USD über max-cost {mc:.5}");
            log_failure(
                &ctx.state.store, ctx.tool, ctx.session, ctx.project, ctx.task_key,
                ctx.input_tokens, "max_cost", 402, &msg, ctx.forced,
            );
            return Some(error_response(StatusCode::PAYMENT_REQUIRED, &msg));
        }
        None
    }
}

/// Cache: Exact-Match-Lookup als `pre`-Kurzschluss; Ablage der frischen Antwort
/// (Exact-Cache und optional Semantic-Cache) in `post`. Der Semantic-Lookup selbst läuft
/// inline im Handler, da er einen async Embedding-Fetch erfordert und erst nach einem
/// Exact-Miss greifen soll.
struct CachePlugin;
impl Plugin for CachePlugin {
    fn pre(&self, ctx: &PluginCtx) -> Option<Response> {
        let key = ctx.cache_key?;
        ctx.state.store.cache_lookup(key).map(|cached| {
            if ctx.is_stream {
                cache_hit_stream_response(ctx, cached)
            } else {
                cache_hit_response(ctx, cached)
            }
        })
    }

    fn post(&self, ctx: &PluginCtx, out: &PluginOutcome) {
        if out.error.is_some() {
            return;
        }
        let Some(key) = ctx.cache_key else { return };
        let Some(body) = out.body.as_deref() else { return };
        let ttl = ctx.state.cfg.cache.ttl_seconds;
        ctx.state.store.cache_store(key, &ctx.entry.model, body, ttl);
        if let Some(sem) = ctx.semantic {
            ctx.state
                .store
                .semantic_cache_store(&sem.scope, &sem.embedding, body, ttl);
        }
    }
}

/// Logging: schreibt die RequestLog-Zeile aus invariantem Kontext + Terminal-Ergebnis.
struct LoggingPlugin;
impl Plugin for LoggingPlugin {
    fn post(&self, ctx: &PluginCtx, out: &PluginOutcome) {
        let _ = ctx.state.store.insert(&RequestLog {
            tool: ctx.tool.into(),
            session: ctx.session.clone(),
            task_type: ctx.task_key.into(),
            model: ctx.entry.model.clone(),
            provider: ctx.entry.provider.clone(),
            tier: ctx.tier,
            used_fallback: out.used_fallback,
            degraded: out.degraded,
            budget_pressure: ctx.pressure,
            estimated_tokens: out.estimated_tokens,
            prompt_tokens: out.prompt_tokens,
            completion_tokens: out.completion_tokens,
            estimated_cost_usd: out.estimated_cost_usd,
            real_cost_usd: out.real_cost_usd,
            latency_ms: ctx.started.elapsed().as_millis() as u64,
            status: out.status,
            attempts: out.attempts,
            attempt_trail: out.attempt_trail.clone(),
            stop_reason: out.stop_reason.clone(),
            error: out.error.clone(),
            forced: ctx.forced,
            project: ctx.project.clone(),
            tools_expected: ctx.tools_expected,
            tool_call_present: out.tool_call_present,
            ..Default::default()
        });
    }
}

fn classify_status(status: StatusCode) -> Outcome {
    let c = status.as_u16();
    if c == 401 || c == 402 || c == 403 || c == 429 {
        Outcome::KeyExhausted
    } else if c == 408 || status.is_server_error() {
        Outcome::Transient
    } else {
        Outcome::BadRequest
    }
}

/// Pseudozufälliger Wert in [0,1) aus der Systemzeit — für die gewichtete Key-Wahl.
fn random_unit() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos % 1_000_000) as f64 / 1_000_000.0
}

/// Exponentielles Backoff mit ±20 % Jitter, gedeckelt auf backoff_max_ms.
fn backoff_ms(cfg: &RetryConfig, attempt: u32) -> u64 {
    let base = cfg
        .backoff_initial_ms
        .saturating_mul(1u64 << attempt.min(16))
        .min(cfg.backoff_max_ms);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let factor = 0.8 + (nanos % 400) as f64 / 1000.0; // 0.8 .. 1.199
    (base as f64 * factor) as u64
}

fn check_auth(cfg: &Config, headers: &HeaderMap) -> bool {
    let expected = &cfg.auth.llmux_key;
    if expected.is_empty() {
        return true;
    }
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|token| constant_time_eq(token.as_bytes(), expected.as_bytes()))
        .unwrap_or(false)
}

/// Konstantzeit-Vergleich gegen Timing-Seitenkanäle beim API-Key-Vergleich.
/// Die Länge gilt nicht als geheim; bei gleicher Länge wird ohne Early-Exit
/// über alle Bytes akkumuliert.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

fn header_bool(headers: &HeaderMap, name: &str) -> bool {
    matches!(
        header(headers, name).as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

/// Text der letzten `n` `user`-Messages (mind. 1), für die regelbasierte
/// Klassifikation. System-/Assistant-/Tool-Rollen bleiben außen vor, damit der
/// große statische Prefix von Agent-Clients den `task_type` nicht verzerrt (#22).
fn extract_user_text(body: &Value, n: usize) -> String {
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
        push_content(&mut out, msg.get("content"));
    }
    out
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

#[allow(clippy::too_many_arguments)]
fn log_failure(
    store: &Store,
    tool: &str,
    session: &Option<String>,
    project: &Option<String>,
    task: &str,
    est: u64,
    _stage: &str,
    status: u16,
    err: &str,
    forced: bool,
) {
    let _ = store.insert(&RequestLog {
        tool: tool.into(),
        session: session.clone(),
        task_type: task.into(),
        estimated_tokens: est,
        status,
        error: Some(err.into()),
        forced,
        project: project.clone(),
        ..Default::default()
    });
}

/// True, wenn die OpenAI-Antwort einen Tool-Call enthält (Qualitätssignal #29).
fn response_has_tool_call(payload: &Value) -> bool {
    if payload.pointer("/choices/0/finish_reason").and_then(Value::as_str) == Some("tool_calls") {
        return true;
    }
    payload
        .pointer("/choices/0/message/tool_calls")
        .and_then(Value::as_array)
        .map(|a| !a.is_empty())
        .unwrap_or(false)
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({
            "error": { "message": message, "type": "llmux_error" }
        })),
    )
        .into_response()
}

/// UTF-8-sichere Kürzung auf max. `max` Bytes (an Zeichengrenze).
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let end = s
        .char_indices()
        .take_while(|(i, _)| *i <= max)
        .last()
        .map(|(i, _)| i)
        .unwrap_or(0);
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixture für die forced-model-Validierung (#25): lokales + Cloud-Modelle mit
    // unterschiedlichen Fähigkeiten/Kontextgrößen, alle Provider ohne Auth (ready).
    const FORCE_YAML: &str = r#"
server: { host: "127.0.0.1", port: 0 }
providers:
  local_p: { enabled: true, base_url: "http://localhost/v1", local: true }
  cloud_p: { enabled: true, base_url: "https://example.com/v1" }
models:
  - { provider: local_p, model: "local-small", tier: 1, context: 8000, supports_tools: false, input_per_mtok: 0.0, output_per_mtok: 0.0 }
  - { provider: cloud_p, model: "no-tools",     tier: 2, context: 8000, supports_tools: false, input_per_mtok: 0.1, output_per_mtok: 0.1 }
  - { provider: cloud_p, model: "cloud-big",    tier: 5, context: 8000, supports_tools: true,  input_per_mtok: 1.0, output_per_mtok: 1.0 }
  - { provider: cloud_p, model: "small-ctx",    tier: 1, context: 1000, supports_tools: true,  input_per_mtok: 0.1, output_per_mtok: 0.1 }
classification:
  simple_text:       { min_tier: 1, expected_output_ratio: 1.0 }
  summarize:         { min_tier: 1 }
  code_review:       { min_tier: 3 }
  architecture:      { min_tier: 4 }
  private_sensitive: { min_tier: 1, local_only: true, expected_output_ratio: 1.0 }
projects:
  secure: { local_only: true }
"#;

    fn force_state() -> AppState {
        let cfg: Config = serde_yaml::from_str(FORCE_YAML).expect("fixture parses");
        AppState {
            cfg,
            http: reqwest::Client::new(),
            store: Store::open(":memory:").unwrap(),
            sessions: SessionStore::default(),
        }
    }

    // #20: Das eingebettete Dashboard wird als Fallback ausgeliefert, ohne die API-/
    // Proxy-Routen zu verdrängen (/healthz, /api/* behalten Vorrang).
    #[tokio::test]
    async fn embedded_dashboard_serves_without_shadowing_api_routes() {
        let app = build_router(Arc::new(force_state()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = reqwest::Client::new();
        let base = format!("http://{addr}");

        // Routen-Vorrang: /healthz und die offene Stats-API bleiben erreichbar.
        let health = client.get(format!("{base}/healthz")).send().await.unwrap();
        assert_eq!(health.status(), 200);
        assert_eq!(health.text().await.unwrap(), "ok");
        assert_eq!(
            client.get(format!("{base}/api/stats/overview")).send().await.unwrap().status(),
            200
        );

        // Fallback: `/` liefert HTML (echtes Dashboard oder "not built"-Platzhalter).
        let root = client.get(format!("{base}/")).send().await.unwrap();
        assert_eq!(root.status(), 200);
        assert!(root
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/html"));

        // Fehlende Asset-Datei -> 404 (kein Index-Fallback für Dateien mit Endung).
        assert_eq!(
            client.get(format!("{base}/_astro/does-not-exist.js")).send().await.unwrap().status(),
            404
        );
    }

    // #15: Die Pipeline ist eine geordnete Plugin-Liste — ein zusätzliches Plugin wird
    // allein durch Eintrag in die Liste eingehängt. `pre` läuft vorwärts (der erste Halt
    // stoppt die Kette), `post` rückwärts.
    #[test]
    fn plugin_pipeline_runs_pre_forward_post_reverse_and_halts() {
        use std::sync::Mutex as StdMutex;

        struct Rec {
            tag: char,
            halt: bool,
            log: Arc<StdMutex<Vec<String>>>,
        }
        impl Plugin for Rec {
            fn pre(&self, _ctx: &PluginCtx) -> Option<Response> {
                self.log.lock().unwrap().push(format!("pre:{}", self.tag));
                self.halt.then(|| StatusCode::OK.into_response())
            }
            fn post(&self, _ctx: &PluginCtx, _out: &PluginOutcome) {
                self.log.lock().unwrap().push(format!("post:{}", self.tag));
            }
        }

        let log = Arc::new(StdMutex::new(Vec::new()));
        let mk = |tag, halt| Box::new(Rec { tag, halt, log: log.clone() }) as Box<dyn Plugin>;
        let pipeline = PluginPipeline {
            plugins: vec![mk('a', false), mk('b', true), mk('c', false)],
        };

        let st = force_state();
        let entry = st.cfg.models[0].clone();
        let none: Option<String> = None;
        let ctx = PluginCtx {
            state: &st,
            entry: &entry,
            task_key: "simple_text",
            tool: "t",
            session: &none,
            project: &none,
            tier: 1,
            pressure: 0.0,
            forced: false,
            tools_expected: false,
            expected_output: 0,
            input_tokens: 0,
            cached_prefix_tokens: 0,
            max_cost: None,
            cache_key: None,
            semantic: None,
            is_stream: false,
            started: Instant::now(),
        };

        // pre stoppt bei 'b' (Halt) — 'c'.pre läuft nicht mehr.
        assert!(pipeline.run_pre(&ctx).is_some());

        let out = PluginOutcome {
            status: 200,
            used_fallback: false,
            degraded: false,
            estimated_tokens: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            estimated_cost_usd: 0.0,
            real_cost_usd: 0.0,
            attempts: 0,
            attempt_trail: None,
            stop_reason: None,
            error: None,
            tool_call_present: false,
            body: None,
        };
        pipeline.run_post(&ctx, &out);

        let seq = log.lock().unwrap().clone();
        assert_eq!(seq, vec!["pre:a", "pre:b", "post:c", "post:b", "post:a"]);
    }

    #[test]
    fn forced_cloud_model_rejected_for_private_sensitive() {
        let st = force_state();
        let err = build_plan(&st, &Some("cloud-big".into()), "private_sensitive", false, &[], 100, 0, &None, &None, Profile::Balanced)
            .unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN, "got: {err:?}");
    }

    #[test]
    fn forced_model_without_tool_support_rejected_when_tools_required() {
        let st = force_state();
        let err = build_plan(&st, &Some("no-tools".into()), "simple_text", true, &[], 100, 0, &None, &None, Profile::Balanced)
            .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST, "got: {err:?}");
    }

    #[test]
    fn forced_model_with_insufficient_context_rejected() {
        let st = force_state();
        // 100k Input + 100k erwarteter Output passt nicht in 1k-Kontext.
        let err = build_plan(&st, &Some("small-ctx".into()), "simple_text", false, &[], 100_000, 0, &None, &None, Profile::Balanced)
            .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST, "got: {err:?}");
    }

    #[test]
    fn forced_valid_model_passes_constraints() {
        let st = force_state();
        let plan = build_plan(&st, &Some("cloud-big".into()), "simple_text", false, &[], 100, 0, &None, &None, Profile::Balanced)
            .expect("gültiges erzwungenes Modell wird akzeptiert");
        assert_eq!(plan.chain.len(), 1);
        assert_eq!(plan.chain[0].model, "cloud-big");
        assert_eq!(plan.project, None);
    }

    #[test]
    fn project_local_only_routes_dynamic_selection_to_local() {
        let st = force_state();
        // Projekt 'secure' erzwingt local_only -> simple_text geht an local-small,
        // und der Projektname landet im Plan (Logging-Metadaten, #33).
        let plan = build_plan(&st, &None, "simple_text", false, &[], 100, 0, &None, &Some("secure".into()), Profile::Balanced)
            .expect("local_only-Projekt hat einen gültigen lokalen Kandidaten");
        assert!(st.cfg.provider_is_local(&plan.chain[0].provider));
        assert_eq!(plan.chain[0].model, "local-small");
        assert_eq!(plan.project.as_deref(), Some("secure"));
    }

    #[test]
    fn forced_cloud_model_rejected_by_project_local_only() {
        let st = force_state();
        // Erzwungenes Cloud-Modell unter local_only-Projekt -> abgelehnt (#33/#25).
        let err = build_plan(&st, &Some("cloud-big".into()), "simple_text", false, &[], 100, 0, &None, &Some("secure".into()), Profile::Balanced)
            .unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN, "got: {err:?}");
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        let cfg = RetryConfig { max_retries: 5, backoff_initial_ms: 500, backoff_max_ms: 8000 };
        // attempt 0: base 500ms, ±20 % Jitter.
        let b0 = backoff_ms(&cfg, 0);
        assert!((400..=600).contains(&b0), "attempt 0 außerhalb Jitter-Band: {b0}");
        // Hohe attempt: base auf backoff_max_ms gedeckelt, Jitter darum.
        let capped = backoff_ms(&cfg, 20);
        assert!(capped >= (8000.0 * 0.8) as u64, "untere Jitter-Grenze verletzt: {capped}");
        assert!(capped <= (8000.0 * 1.2) as u64, "Deckel verletzt: {capped}");
    }

    // #22: Ein großer statischer System-Prefix voller Architektur-Keywords darf den
    // task_type nicht verzerren — klassifiziert wird die kurze User-Aufgabe.
    #[test]
    fn classifies_by_latest_user_message_not_system_prefix() {
        let body = json!({
            "messages": [
                { "role": "system", "content": "You are an expert in software architecture, \
                  database schema design, scalability and security trade-offs. Tools available: ..." },
                { "role": "user", "content": "fix the bug in this function" }
            ]
        });
        let user_text = extract_user_text(&body, 1);
        assert_eq!(classifier::classify(&user_text), classifier::TaskType::CodeReview);
        // Über die gesamte Payload würde fälschlich Architecture gewinnen.
        assert_eq!(
            classifier::classify(&extract_user_text(&body, 99)),
            classifier::TaskType::CodeReview
        );
    }

    #[test]
    fn extract_user_text_respects_message_count() {
        let body = json!({
            "messages": [
                { "role": "user", "content": "first" },
                { "role": "assistant", "content": "reply" },
                { "role": "user", "content": "second" }
            ]
        });
        assert_eq!(extract_user_text(&body, 1).trim(), "second");
        let two = extract_user_text(&body, 2);
        assert!(two.contains("first") && two.contains("second"));
    }

    #[test]
    fn constant_time_eq_matches_semantics() {
        assert!(constant_time_eq(b"secret-key", b"secret-key"));
        assert!(!constant_time_eq(b"secret-key", b"secret-keX"));
        assert!(!constant_time_eq(b"short", b"longer-key"));
        assert!(constant_time_eq(b"", b""));
    }

    /// Stats-API (#18): Overview liefert DB-Kennzahlen + Budget-Felder, der
    /// Requests-Feed listet die jüngsten Zeilen (neueste zuerst). Auth wird wie
    /// beim Proxy über den llmux_key erzwungen.
    #[tokio::test]
    async fn stats_api_reports_overview_and_recent_requests_with_auth() {
        let yaml = r#"
server: { host: "127.0.0.1", port: 0 }
auth: { llmux_key: "secret" }
providers:
  p: { enabled: true, base_url: "http://localhost/v1" }
models:
  - { provider: p, model: "m1", tier: 1, context: 8000, supports_tools: true, input_per_mtok: 0.0, output_per_mtok: 0.0 }
classification:
  simple_text:       { min_tier: 1 }
  summarize:         { min_tier: 1 }
  code_review:       { min_tier: 1 }
  architecture:      { min_tier: 1 }
  private_sensitive: { min_tier: 1 }
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        let store = Store::open(":memory:").unwrap();
        // Drei Logzeilen: Erfolg, Cache-Treffer, Provider-Fehler.
        store.insert(&RequestLog { tool: "aider".into(), task_type: "simple_text".into(), model: "m1".into(), provider: "p".into(), tier: 1, prompt_tokens: 10, completion_tokens: 5, real_cost_usd: 0.01, latency_ms: 100, status: 200, ..Default::default() }).unwrap();
        store.insert(&RequestLog { tool: "aider".into(), task_type: "simple_text".into(), model: "m1".into(), provider: "p".into(), tier: 1, latency_ms: 5, status: 200, cache_hit: true, ..Default::default() }).unwrap();
        store.insert(&RequestLog { tool: "aider".into(), task_type: "simple_text".into(), status: 502, error: Some("boom".into()), ..Default::default() }).unwrap();

        let state = Arc::new(AppState { cfg, http: reqwest::Client::new(), store, sessions: SessionStore::default() });
        let app = build_router(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = reqwest::Client::new();
        let base = format!("http://{addr}");

        // Read-only Stats-API ist offen (lokale Instanz, #19) — auch ohne Key 200.
        let unauth = client.get(format!("{base}/api/stats/overview")).send().await.unwrap();
        assert_eq!(unauth.status(), 200);

        // Overview mit erwarteten Feldern (Key optional, wird ignoriert).
        let ov: Value = client
            .get(format!("{base}/api/stats/overview"))
            .send().await.unwrap()
            .json().await.unwrap();
        assert_eq!(ov["total_requests"], 3);
        assert_eq!(ov["error_count"], 1);
        assert!((ov["cache_hit_rate"].as_f64().unwrap() - 1.0 / 3.0).abs() < 1e-9);
        assert!(ov.get("cost_today").is_some() && ov.get("budget_pressure").is_some());
        // p95 über erfolgreiche Zeilen (100, 5) -> 100.
        assert_eq!(ov["p95_latency_ms"], 100);

        // Requests-Feed: limit greift, neueste zuerst, abgeleitetes result.
        let rq: Value = client
            .get(format!("{base}/api/stats/requests?limit=2"))
            .bearer_auth("secret")
            .send().await.unwrap()
            .json().await.unwrap();
        let arr = rq["requests"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        // Neueste Zeile ist der Fehler -> result "rejected".
        assert_eq!(arr[0]["status"], 502);
        assert_eq!(arr[0]["result"], "rejected");
        assert_eq!(arr[0]["error"], "boom");
        // Die zweite ist der Cache-Treffer.
        assert_eq!(arr[1]["result"], "cached");
        assert_eq!(arr[1]["cache_hit"], true);
    }

    #[test]
    fn detects_tool_call_in_response() {
        // finish_reason = tool_calls.
        assert!(response_has_tool_call(&json!({ "choices": [{ "finish_reason": "tool_calls", "message": {} }] })));
        // tool_calls-Array im message-Objekt.
        assert!(response_has_tool_call(&json!({ "choices": [{ "finish_reason": "stop", "message": { "tool_calls": [{ "id": "x" }] } }] })));
        // Reine Textantwort -> kein Tool-Call.
        assert!(!response_has_tool_call(&json!({ "choices": [{ "finish_reason": "stop", "message": { "content": "hi" } }] })));
    }

    #[test]
    fn parses_usage_from_sse_stream() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                   data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                   data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":7}}\n\n\
                   data: [DONE]\n\n";
        let (p, c, f) = parse_stream_usage(sse.as_bytes());
        assert_eq!(p, Some(12));
        assert_eq!(c, Some(7));
        assert_eq!(f.as_deref(), Some("stop"));
    }

    #[test]
    fn parse_usage_tolerates_missing_usage() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
        let (p, c, _) = parse_stream_usage(sse.as_bytes());
        assert_eq!(p, None);
        assert_eq!(c, None);
    }

    #[test]
    fn status_classification() {
        assert!(matches!(classify_status(StatusCode::INTERNAL_SERVER_ERROR), Outcome::Transient));
        assert!(matches!(classify_status(StatusCode::REQUEST_TIMEOUT), Outcome::Transient));
        assert!(matches!(classify_status(StatusCode::TOO_MANY_REQUESTS), Outcome::KeyExhausted));
        assert!(matches!(classify_status(StatusCode::UNAUTHORIZED), Outcome::KeyExhausted));
        assert!(matches!(classify_status(StatusCode::PAYMENT_REQUIRED), Outcome::KeyExhausted));
        assert!(matches!(classify_status(StatusCode::BAD_REQUEST), Outcome::BadRequest));
    }

    /// Ein toter Key (401) führt zur Rotation auf den nächsten Key statt zum
    /// Request-Fehler (T3.2). Der Mock antwortet beim ersten Aufruf mit 401, danach 200.
    #[tokio::test]
    async fn dead_key_rotates_instead_of_failing() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let mock = {
            let calls = calls.clone();
            Router::new().route(
                "/chat/completions",
                post(move || {
                    let calls = calls.clone();
                    async move {
                        let n = calls.fetch_add(1, Ordering::SeqCst);
                        if n == 0 {
                            (StatusCode::UNAUTHORIZED, Json(json!({ "error": "dead key" }))).into_response()
                        } else {
                            Json(json!({
                                "id": "x", "object": "chat.completion",
                                "choices": [{ "index": 0, "message": { "role": "assistant", "content": "ok" }, "finish_reason": "stop" }],
                                "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
                            }))
                            .into_response()
                        }
                    }
                }),
            )
        };
        let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mock_addr = mock_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(mock_listener, mock).await.unwrap() });

        std::env::set_var("LLMUX_TEST_KEY_A", "dead");
        std::env::set_var("LLMUX_TEST_KEY_B", "good");
        let yaml = format!(
            r#"
server: {{ host: "127.0.0.1", port: 0 }}
retry: {{ max_retries: 0, backoff_initial_ms: 1, backoff_max_ms: 1 }}
providers:
  p:
    enabled: true
    base_url: "http://{mock_addr}"
    keys:
      - {{ env: "LLMUX_TEST_KEY_A", weight: 1.0 }}
      - {{ env: "LLMUX_TEST_KEY_B", weight: 1.0 }}
models:
  - {{ provider: p, model: "m1", tier: 1, context: 8000, supports_tools: true, input_per_mtok: 0.0, output_per_mtok: 0.0 }}
classification:
  simple_text:       {{ min_tier: 1 }}
  summarize:         {{ min_tier: 1 }}
  code_review:       {{ min_tier: 1 }}
  architecture:      {{ min_tier: 1 }}
  private_sensitive: {{ min_tier: 1 }}
"#
        );
        let cfg: Config = serde_yaml::from_str(&yaml).unwrap();
        let state = Arc::new(AppState {
            cfg,
            http: reqwest::Client::new(),
            store: Store::open(":memory:").unwrap(),
            sessions: SessionStore::default(),
        });
        let app = build_router(state.clone());
        let llmux_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let llmux_addr = llmux_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(llmux_listener, app).await.unwrap() });

        let client = reqwest::Client::new();
        let url = format!("http://{llmux_addr}/v1/chat/completions");
        let body = json!({ "messages": [{ "role": "user", "content": "hi" }] });
        let resp = client.post(&url).json(&body).send().await.unwrap();

        assert_eq!(resp.status(), 200, "Rotation muss den toten Key überbrücken");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "erst 401, dann rotierter 200");

        std::env::remove_var("LLMUX_TEST_KEY_A");
        std::env::remove_var("LLMUX_TEST_KEY_B");
    }

    /// Nicht unterstützte Request-Felder werden vor dem Weiterleiten entfernt; der
    /// Request gelangt trotzdem erfolgreich zum Provider (T3.4).
    #[tokio::test]
    async fn unsupported_params_are_stripped() {
        use std::sync::Mutex;

        let seen: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
        let mock = {
            let seen = seen.clone();
            Router::new().route(
                "/chat/completions",
                post(move |Json(body): Json<Value>| {
                    let seen = seen.clone();
                    async move {
                        *seen.lock().unwrap() = Some(body);
                        Json(json!({
                            "id": "x", "object": "chat.completion",
                            "choices": [{ "index": 0, "message": { "role": "assistant", "content": "ok" }, "finish_reason": "stop" }],
                            "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
                        }))
                    }
                }),
            )
        };
        let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mock_addr = mock_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(mock_listener, mock).await.unwrap() });

        // Provider strippt logit_bias, das Modell zusätzlich frequency_penalty.
        let yaml = format!(
            r#"
server: {{ host: "127.0.0.1", port: 0 }}
providers:
  p: {{ enabled: true, base_url: "http://{mock_addr}", strip_params: ["logit_bias"] }}
models:
  - {{ provider: p, model: "m1", tier: 1, context: 8000, supports_tools: true, input_per_mtok: 0.0, output_per_mtok: 0.0, strip_params: ["frequency_penalty"] }}
classification:
  simple_text:       {{ min_tier: 1 }}
  summarize:         {{ min_tier: 1 }}
  code_review:       {{ min_tier: 1 }}
  architecture:      {{ min_tier: 1 }}
  private_sensitive: {{ min_tier: 1 }}
"#
        );
        let cfg: Config = serde_yaml::from_str(&yaml).unwrap();
        let state = Arc::new(AppState {
            cfg,
            http: reqwest::Client::new(),
            store: Store::open(":memory:").unwrap(),
            sessions: SessionStore::default(),
        });
        let app = build_router(state.clone());
        let llmux_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let llmux_addr = llmux_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(llmux_listener, app).await.unwrap() });

        let client = reqwest::Client::new();
        let url = format!("http://{llmux_addr}/v1/chat/completions");
        let body = json!({
            "messages": [{ "role": "user", "content": "hi" }],
            "frequency_penalty": 0.5,
            "logit_bias": { "50256": -100 },
            "temperature": 0.2
        });
        let resp = client.post(&url).json(&body).send().await.unwrap();
        assert_eq!(resp.status(), 200);

        let received = seen.lock().unwrap().clone().expect("mock erhielt einen Request");
        assert!(received.get("frequency_penalty").is_none(), "frequency_penalty muss entfernt sein");
        assert!(received.get("logit_bias").is_none(), "logit_bias muss entfernt sein");
        // Unbetroffene Felder bleiben erhalten.
        assert_eq!(received["temperature"], 0.2);
    }

    /// Ein Alias im `model`-Feld erzwingt das konfigurierte Zielmodell statt der
    /// (günstigeren) dynamischen Auswahl (T3.3).
    #[tokio::test]
    async fn model_alias_routes_to_configured_target() {
        use std::sync::Mutex;

        let seen: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
        let mock = {
            let seen = seen.clone();
            Router::new().route(
                "/chat/completions",
                post(move |Json(body): Json<Value>| {
                    let seen = seen.clone();
                    async move {
                        *seen.lock().unwrap() = Some(body);
                        Json(json!({
                            "id": "x", "object": "chat.completion",
                            "choices": [{ "index": 0, "message": { "role": "assistant", "content": "ok" }, "finish_reason": "stop" }],
                            "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
                        }))
                    }
                }),
            )
        };
        let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mock_addr = mock_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(mock_listener, mock).await.unwrap() });

        // basic ist günstiger und würde dynamisch gewählt; Alias zwingt zu premium.
        let yaml = format!(
            r#"
server: {{ host: "127.0.0.1", port: 0 }}
providers:
  p: {{ enabled: true, base_url: "http://{mock_addr}" }}
models:
  - {{ provider: p, model: "basic",   tier: 1, context: 8000, supports_tools: true, input_per_mtok: 0.1, output_per_mtok: 0.1 }}
  - {{ provider: p, model: "premium", tier: 1, context: 8000, supports_tools: true, input_per_mtok: 9.0, output_per_mtok: 9.0 }}
aliases:
  best: "premium"
classification:
  simple_text:       {{ min_tier: 1 }}
  summarize:         {{ min_tier: 1 }}
  code_review:       {{ min_tier: 1 }}
  architecture:      {{ min_tier: 1 }}
  private_sensitive: {{ min_tier: 1 }}
"#
        );
        let cfg: Config = serde_yaml::from_str(&yaml).unwrap();
        let state = Arc::new(AppState {
            cfg,
            http: reqwest::Client::new(),
            store: Store::open(":memory:").unwrap(),
            sessions: SessionStore::default(),
        });
        let app = build_router(state.clone());
        let llmux_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let llmux_addr = llmux_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(llmux_listener, app).await.unwrap() });

        let client = reqwest::Client::new();
        let url = format!("http://{llmux_addr}/v1/chat/completions");
        let body = json!({ "model": "best", "messages": [{ "role": "user", "content": "hi" }] });
        let resp = client.post(&url).json(&body).send().await.unwrap();
        assert_eq!(resp.status(), 200);

        let received = seen.lock().unwrap().clone().expect("mock erhielt einen Request");
        assert_eq!(received["model"], "premium", "Alias 'best' muss zu 'premium' routen");
    }

    /// End-to-End über einen Mock-Anthropic-Provider: der OpenAI-Request wird nach
    /// `/messages` übersetzt (system extrahiert, max_tokens gesetzt) und die Antwort
    /// zurück ins OpenAI-Format inkl. usage (T3.1).
    #[tokio::test]
    async fn anthropic_provider_translates_request_and_response() {
        use std::sync::Mutex;

        // Mock Anthropic: speichert den empfangenen Body, antwortet im Anthropic-Format.
        let seen: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
        let mock = {
            let seen = seen.clone();
            Router::new().route(
                "/messages",
                post(move |Json(body): Json<Value>| {
                    let seen = seen.clone();
                    async move {
                        *seen.lock().unwrap() = Some(body);
                        Json(json!({
                            "id": "msg_abc",
                            "type": "message",
                            "role": "assistant",
                            "model": "claude-mock",
                            "stop_reason": "end_turn",
                            "content": [{ "type": "text", "text": "Hallo!" }],
                            "usage": { "input_tokens": 9, "output_tokens": 4 }
                        }))
                    }
                }),
            )
        };
        let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mock_addr = mock_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(mock_listener, mock).await.unwrap() });

        std::env::set_var("LLMUX_TEST_ANTHROPIC_KEY", "sk-test");
        let yaml = format!(
            r#"
server: {{ host: "127.0.0.1", port: 0 }}
providers:
  anthropic: {{ enabled: true, kind: anthropic, base_url: "http://{mock_addr}", api_key_env: "LLMUX_TEST_ANTHROPIC_KEY" }}
models:
  - {{ provider: anthropic, model: "claude-mock", tier: 1, context: 200000, supports_tools: true, input_per_mtok: 3.0, output_per_mtok: 15.0 }}
classification:
  simple_text:       {{ min_tier: 1 }}
  summarize:         {{ min_tier: 1 }}
  code_review:       {{ min_tier: 1 }}
  architecture:      {{ min_tier: 1 }}
  private_sensitive: {{ min_tier: 1 }}
"#
        );
        let cfg: Config = serde_yaml::from_str(&yaml).unwrap();
        let state = Arc::new(AppState {
            cfg,
            http: reqwest::Client::new(),
            store: Store::open(":memory:").unwrap(),
            sessions: SessionStore::default(),
        });
        let app = build_router(state.clone());
        let llmux_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let llmux_addr = llmux_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(llmux_listener, app).await.unwrap() });

        let client = reqwest::Client::new();
        let url = format!("http://{llmux_addr}/v1/chat/completions");
        let body = json!({
            "messages": [
                { "role": "system", "content": "Du bist hilfreich." },
                { "role": "user", "content": "Hallo" }
            ]
        });
        let resp = client.post(&url).json(&body).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let out: Value = resp.json().await.unwrap();

        // Antwort ist OpenAI-geformt inkl. usage.
        assert_eq!(out["object"], "chat.completion");
        assert_eq!(out["choices"][0]["message"]["content"], "Hallo!");
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        assert_eq!(out["usage"]["prompt_tokens"], 9);
        assert_eq!(out["usage"]["completion_tokens"], 4);

        // Request wurde nach Anthropic übersetzt: system extrahiert, max_tokens gesetzt.
        let received = seen.lock().unwrap().clone().expect("mock erhielt einen Request");
        assert_eq!(received["system"], "Du bist hilfreich.");
        assert!(received["max_tokens"].as_u64().unwrap() > 0);
        assert_eq!(received["messages"][0]["role"], "user");

        std::env::remove_var("LLMUX_TEST_ANTHROPIC_KEY");
    }

    /// End-to-End über einen Mock-Provider: Streaming reicht SSE durch, loggt reale
    /// Usage (T2.1) und cached die Antwort; die zweite identische Anfrage kommt aus
    /// dem Cache, ohne den Provider erneut zu treffen (T2.2).
    #[tokio::test]
    async fn streaming_logs_real_usage_and_serves_from_cache() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let hits = Arc::new(AtomicUsize::new(0));
        const SSE: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
            data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
            data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":5}}\n\n\
            data: [DONE]\n\n";

        // Mock-Provider: zählt Aufrufe, antwortet mit fester SSE.
        let mock = {
            let hits = hits.clone();
            Router::new().route(
                "/chat/completions",
                post(move || {
                    let hits = hits.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        ([("content-type", "text/event-stream")], SSE)
                    }
                }),
            )
        };
        let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mock_addr = mock_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(mock_listener, mock).await.unwrap() });

        // llmux mit Cache an, ein lokales tier-1-Modell, das auf den Mock zeigt.
        let yaml = format!(
            r#"
server: {{ host: "127.0.0.1", port: 0 }}
cache: {{ enabled: true, ttl_seconds: 600, max_conversation_messages: 5, eviction_interval_seconds: 300 }}
providers:
  mock: {{ enabled: true, base_url: "http://{mock_addr}", local: true }}
models:
  - {{ provider: mock, model: "m1", tier: 1, context: 8000, supports_tools: true, input_per_mtok: 1.0, output_per_mtok: 1.0 }}
classification:
  simple_text:       {{ min_tier: 1 }}
  summarize:         {{ min_tier: 1 }}
  code_review:       {{ min_tier: 1 }}
  architecture:      {{ min_tier: 1 }}
  private_sensitive: {{ min_tier: 1, local_only: true }}
"#
        );
        let cfg: Config = serde_yaml::from_str(&yaml).unwrap();
        let state = Arc::new(AppState {
            cfg,
            http: reqwest::Client::new(),
            store: Store::open(":memory:").unwrap(),
            sessions: SessionStore::default(),
        });
        let app = build_router(state.clone());
        let llmux_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let llmux_addr = llmux_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(llmux_listener, app).await.unwrap() });

        let client = reqwest::Client::new();
        let url = format!("http://{llmux_addr}/v1/chat/completions");
        let body = json!({ "stream": true, "messages": [{ "role": "user", "content": "hello" }] });

        // 1. Anfrage: Provider-Aufruf, SSE durchgereicht, kein Cache-Header.
        let r1 = client.post(&url).json(&body).send().await.unwrap();
        assert_eq!(r1.status(), 200);
        assert!(r1.headers().get("x-llmux-cache").is_none());
        let chunk = r1.text().await.unwrap();
        assert!(chunk.contains("[DONE]"));
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // T2.1: am Stream-Ende wurde reale Usage geloggt (11/5), Status 200.
        // Kurzer Yield, damit der Insert sicher abgeschlossen ist.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let (status, pt, ct, cost, cache_hit) = state.store.last_request().unwrap();
        assert_eq!((status, pt, ct, cache_hit), (200, 11, 5, 0));
        assert!(cost > 0.0, "real_cost_usd muss > 0 sein, war {cost}");

        // 2. identische Anfrage: Cache-Treffer, Provider wird NICHT erneut aufgerufen.
        let r2 = client.post(&url).json(&body).send().await.unwrap();
        assert_eq!(r2.status(), 200);
        assert_eq!(
            r2.headers().get("x-llmux-cache").and_then(|v| v.to_str().ok()),
            Some("hit")
        );
        let replay = r2.text().await.unwrap();
        assert!(replay.contains("[DONE]"));
        assert_eq!(hits.load(Ordering::SeqCst), 1, "Cache-Treffer darf den Provider nicht erneut treffen");
    }
}
