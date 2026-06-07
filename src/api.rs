//! HTTP-Layer: OpenAI-kompatibler Endpoint + State + dynamische Request-Pipeline
//! mit Cache, Retry/Fehlerklassifikation und Per-Request-Overrides.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};

use crate::config::{Config, ModelEntry, RetryConfig};
use crate::logging::{RequestLog, Store};
use crate::router::{self, SelectError, SelectInput, SessionStore};
use crate::{cache, classifier, cost, providers};

pub struct AppState {
    pub cfg: Config,
    pub http: reqwest::Client,
    pub store: Store,
    pub sessions: SessionStore,
}

/// Routing-Metadaten, unabhängig davon ob dynamisch gewählt oder per Header erzwungen.
struct Plan {
    chain: Vec<ModelEntry>,
    tier: u8,
    degraded: bool,
    pressure: f64,
    expected_output: u64,
}

/// Klassifikation eines Provider-Fehlers für die Retry-/Fallback-Strategie.
enum Outcome {
    Transient,         // 5xx/429/Netzwerk -> gleiches Modell erneut (mit Backoff)
    ProviderExhausted, // 401/402/403 -> nächstes Modell, kein Retry
    BadRequest,        // sonstige 4xx -> Abbruch, kein Fallback (Request ist fehlerhaft)
}

pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(state)
}

async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !check_auth(&state.cfg, &headers) {
        return error_response(StatusCode::UNAUTHORIZED, "ungültiger oder fehlender API-Key");
    }

    let started = Instant::now();
    let tool = header(&headers, "x-llmux-tool").unwrap_or_else(|| "unknown".into());
    let session = header(&headers, "x-llmux-session");
    let is_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);

    // Per-Request-Overrides
    let force_model = header(&headers, "x-llmux-model");
    let no_cache = header_bool(&headers, "x-llmux-no-cache");
    let no_fallback = header_bool(&headers, "x-llmux-no-fallback");
    let max_cost = header(&headers, "x-llmux-max-cost").and_then(|s| s.parse::<f64>().ok());

    // Request verstehen
    let input_tokens = cost::estimate_request_tokens(&body);
    let requires_tools = classifier::requires_tools(&body);
    let text = extract_text(&body);
    let task = classifier::classify(&text, &state.cfg.privacy.block_cloud_patterns);
    let task_key = task.as_key();

    // Plan bestimmen: erzwungenes Modell oder dynamische Auswahl
    let mut plan = match build_plan(&state, &force_model, task_key, requires_tools, input_tokens, &session) {
        Ok(p) => p,
        Err(resp) => {
            log_failure(&state.store, &tool, &session, task_key, input_tokens, "plan", resp.0.as_u16(), &resp.1);
            return error_response(resp.0, &resp.1);
        }
    };

    if no_fallback {
        plan.chain.truncate(1);
    }

    let primary = plan.chain[0].clone();

    // Per-Request-Kostendeckel
    if let Some(mc) = max_cost {
        let est = primary.est_cost(input_tokens, plan.expected_output);
        if est > mc {
            let msg = format!("Schätzkosten {est:.5} USD über max-cost {mc:.5}");
            log_failure(&state.store, &tool, &session, task_key, input_tokens, "max_cost", 402, &msg);
            return error_response(StatusCode::PAYMENT_REQUIRED, &msg);
        }
    }

    // Cache-Lookup (Exact-Match, opt-out per Header, kein Stream, History-Guard)
    let cacheable = state.cfg.cache.enabled
        && !no_cache
        && !is_stream
        && cache::conversation_len(&body) <= state.cfg.cache.max_conversation_messages;
    let cache_key = cacheable.then(|| cache::cache_key(&primary, &body));

    if let Some(key) = &cache_key {
        if let Some(cached) = state.store.cache_lookup(key) {
            return cache_hit_response(&state, &primary, &plan, task_key, &tool, &session, cached, started);
        }
    }

    tracing::info!(
        task = task_key, tier = plan.tier, degraded = plan.degraded,
        tools = requires_tools, input_tokens,
        pressure = format!("{:.2}", plan.pressure),
        candidates = plan.chain.len(), forced = force_model.is_some(),
        "routing"
    );

    forward_with_retries(&state, body, plan, task_key, &tool, &session, is_stream, input_tokens, cache_key, started).await
}

/// Baut den Plan: entweder erzwungenes Modell (Header) oder dynamische Auswahl.
fn build_plan(
    state: &AppState,
    force_model: &Option<String>,
    task_key: &str,
    requires_tools: bool,
    input_tokens: u64,
    session: &Option<String>,
) -> Result<Plan, (StatusCode, String)> {
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
        let expected_output = input_tokens;
        return Ok(Plan {
            tier: entry.tier,
            chain: vec![entry],
            degraded: false,
            pressure: router::current_pressure(&state.cfg, &state.store),
            expected_output,
        });
    }

    let select_input = SelectInput {
        task_key,
        requires_tools,
        input_tokens,
        session: session.as_deref(),
    };
    match router::select(&state.cfg, &state.store, &state.sessions, &select_input) {
        Ok(s) => Ok(Plan {
            chain: s.chain,
            tier: s.tier,
            degraded: s.degraded,
            pressure: s.budget_pressure,
            expected_output: s.expected_output_tokens,
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
    state: &AppState,
    body: Value,
    plan: Plan,
    task_key: &str,
    tool: &str,
    session: &Option<String>,
    is_stream: bool,
    input_tokens: u64,
    cache_key: Option<String>,
    started: Instant,
) -> Response {
    let mut trail: Vec<Value> = Vec::new();
    let mut attempts: u32 = 0;
    let mut last_err = String::from("kein Versuch ausgeführt");
    let mut last_status = StatusCode::BAD_GATEWAY;

    for entry in plan.chain.iter() {
        let target = entry.target();
        let mut attempt: u32 = 0;
        loop {
            attempts += 1;
            match providers::forward(&state.cfg, &state.http, &target, body.clone()).await {
                Ok(resp) if resp.status().is_success() => {
                    trail.push(json!({ "provider": target.provider, "model": target.model, "status": resp.status().as_u16() }));
                    return handle_success(
                        state, resp, entry, &plan, task_key, tool, session,
                        entry.provider != plan.chain[0].provider || entry.model != plan.chain[0].model,
                        is_stream, input_tokens, attempts, trail, cache_key, started,
                    )
                    .await;
                }
                Ok(resp) => {
                    let status = resp.status();
                    last_status = status;
                    let detail = resp.text().await.unwrap_or_default();
                    last_err = format!("{status}: {}", truncate(&detail, 300));
                    trail.push(json!({ "provider": target.provider, "model": target.model, "status": status.as_u16(), "error": truncate(&detail, 200) }));

                    match classify_status(status) {
                        Outcome::Transient if attempt < state.cfg.retry.max_retries => {
                            let ms = backoff_ms(&state.cfg.retry, attempt);
                            tracing::warn!(provider = %target.provider, model = %target.model, "transient {status}, retry #{} in {ms}ms", attempt + 1);
                            tokio::time::sleep(Duration::from_millis(ms)).await;
                            attempt += 1;
                            continue;
                        }
                        Outcome::BadRequest => {
                            // Request ist fehlerhaft -> kein Fallback, Status durchreichen.
                            log_failure(&state.store, tool, session, task_key, input_tokens, "bad_request", status.as_u16(), &last_err);
                            return error_response(status, &last_err);
                        }
                        _ => break, // Retries erschöpft oder Provider erschöpft -> nächstes Modell
                    }
                }
                Err(e) => {
                    last_err = e.to_string();
                    trail.push(json!({ "provider": target.provider, "model": target.model, "error": last_err.clone() }));
                    if attempt < state.cfg.retry.max_retries {
                        let ms = backoff_ms(&state.cfg.retry, attempt);
                        tracing::warn!(provider = %target.provider, model = %target.model, "netzwerkfehler, retry #{} in {ms}ms", attempt + 1);
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                        attempt += 1;
                        continue;
                    }
                    break;
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
        ..Default::default()
    });
    error_response(
        StatusCode::BAD_GATEWAY,
        &format!("alle Modelle fehlgeschlagen: {last_err}"),
    )
}

#[allow(clippy::too_many_arguments)]
async fn handle_success(
    state: &AppState,
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
    started: Instant,
) -> Response {
    let status = resp.status();
    let trail_json = serde_json::to_string(&trail).ok();

    if is_stream {
        let est_cost = entry.est_cost(input_tokens, plan.expected_output);
        let _ = state.store.insert(&RequestLog {
            tool: tool.into(),
            session: session.clone(),
            task_type: task_key.into(),
            model: entry.model.clone(),
            provider: entry.provider.clone(),
            tier: plan.tier,
            used_fallback,
            degraded: plan.degraded,
            budget_pressure: plan.pressure,
            estimated_tokens: input_tokens,
            prompt_tokens: input_tokens,
            completion_tokens: plan.expected_output,
            estimated_cost_usd: est_cost,
            real_cost_usd: est_cost,
            latency_ms: started.elapsed().as_millis() as u64,
            status: status.as_u16(),
            attempts,
            attempt_trail: trail_json,
            ..Default::default()
        });
        return Response::builder()
            .status(status)
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(resp.bytes_stream()))
            .unwrap();
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
    let real_cost = entry.est_cost(prompt_tokens, completion_tokens);

    // Erfolgreiche, nicht-streamende Antwort cachen.
    if let Some(key) = &cache_key {
        if let Ok(s) = serde_json::to_string(&payload) {
            state
                .store
                .cache_store(key, &entry.model, &s, state.cfg.cache.ttl_seconds);
        }
    }

    let _ = state.store.insert(&RequestLog {
        tool: tool.into(),
        session: session.clone(),
        task_type: task_key.into(),
        model: entry.model.clone(),
        provider: entry.provider.clone(),
        tier: plan.tier,
        used_fallback,
        degraded: plan.degraded,
        budget_pressure: plan.pressure,
        estimated_tokens: input_tokens,
        prompt_tokens,
        completion_tokens,
        estimated_cost_usd: entry.est_cost(input_tokens, plan.expected_output),
        real_cost_usd: real_cost,
        latency_ms: started.elapsed().as_millis() as u64,
        status: status.as_u16(),
        attempts,
        attempt_trail: trail_json,
        stop_reason,
        ..Default::default()
    });

    tracing::info!(
        task = task_key, provider = %entry.provider, model = %entry.model,
        tier = plan.tier, fallback = used_fallback, attempts,
        prompt_tokens, completion_tokens, cost_usd = real_cost, "request ok"
    );

    (status, Json(payload)).into_response()
}

#[allow(clippy::too_many_arguments)]
fn cache_hit_response(
    state: &AppState,
    entry: &ModelEntry,
    plan: &Plan,
    task_key: &str,
    tool: &str,
    session: &Option<String>,
    cached: String,
    started: Instant,
) -> Response {
    let payload: Value = serde_json::from_str(&cached).unwrap_or_else(|_| json!({}));
    let prompt_tokens = payload.pointer("/usage/prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
    let completion_tokens = payload.pointer("/usage/completion_tokens").and_then(Value::as_u64).unwrap_or(0);

    let _ = state.store.insert(&RequestLog {
        tool: tool.into(),
        session: session.clone(),
        task_type: task_key.into(),
        model: entry.model.clone(),
        provider: entry.provider.clone(),
        tier: plan.tier,
        budget_pressure: plan.pressure,
        prompt_tokens,
        completion_tokens,
        latency_ms: started.elapsed().as_millis() as u64,
        status: 200,
        cache_hit: true,
        ..Default::default()
    });

    tracing::info!(task = task_key, model = %entry.model, "cache hit");
    (
        StatusCode::OK,
        [("x-llmux-cache", "hit")],
        Json(payload),
    )
        .into_response()
}

fn classify_status(status: StatusCode) -> Outcome {
    let c = status.as_u16();
    if c == 408 || c == 429 || status.is_server_error() {
        Outcome::Transient
    } else if c == 401 || c == 402 || c == 403 {
        Outcome::ProviderExhausted
    } else {
        Outcome::BadRequest
    }
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
        .map(|token| token == expected)
        .unwrap_or(false)
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

/// Extrahiert allen Text aus messages[].content (String oder OpenAI-Content-Parts).
fn extract_text(body: &Value) -> String {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return String::new();
    };
    let mut out = String::new();
    for msg in messages {
        match msg.get("content") {
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
    out
}

#[allow(clippy::too_many_arguments)]
fn log_failure(
    store: &Store,
    tool: &str,
    session: &Option<String>,
    task: &str,
    est: u64,
    _stage: &str,
    status: u16,
    err: &str,
) {
    let _ = store.insert(&RequestLog {
        tool: tool.into(),
        session: session.clone(),
        task_type: task.into(),
        estimated_tokens: est,
        status,
        error: Some(err.into()),
        ..Default::default()
    });
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
