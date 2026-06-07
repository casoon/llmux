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
use futures_util::StreamExt;
use serde_json::{json, Value};

use crate::config::{Config, ModelEntry, ProviderKind, RetryConfig};
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
struct Plan {
    chain: Vec<ModelEntry>,
    tier: u8,
    degraded: bool,
    pressure: f64,
    expected_output: u64,
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
        .with_state(state)
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
        classifier::classify(&user_text)
    };
    let task_key = task.as_key();

    // Plan bestimmen: erzwungenes Modell oder dynamische Auswahl
    let mut plan = match build_plan(&state, &force_model, task_key, requires_tools, input_tokens, cached_prefix_tokens, &session) {
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

    // Streaming für native Anthropic-Provider ist noch nicht implementiert (nur Nicht-Stream).
    if is_stream && state.cfg.provider_kind(&primary.provider) == ProviderKind::Anthropic {
        let msg = "Streaming wird für native Anthropic-Provider noch nicht unterstützt";
        log_failure(&state.store, &tool, &session, task_key, input_tokens, "anthropic_stream", 400, msg);
        return error_response(StatusCode::BAD_REQUEST, msg);
    }

    // Per-Request-Kostendeckel (mit Prefix-Rabatt, konsistent zur Routing-Schätzung).
    if let Some(mc) = max_cost {
        let billed = state.cfg.prompt_cache_billed_fraction(&primary.provider);
        let eff_input = cost::effective_input_tokens(input_tokens, cached_prefix_tokens, billed);
        let est = primary.est_cost(eff_input, plan.expected_output);
        if est > mc {
            let msg = format!("Schätzkosten {est:.5} USD über max-cost {mc:.5}");
            log_failure(&state.store, &tool, &session, task_key, input_tokens, "max_cost", 402, &msg);
            return error_response(StatusCode::PAYMENT_REQUIRED, &msg);
        }
    }

    // Cache-Lookup (Exact-Match, opt-out per Header, History-Guard). Streaming-
    // Antworten werden separat gecacht (eigener Key-Suffix, SSE-Replay).
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

    if let Some(key) = &cache_key {
        if let Some(cached) = state.store.cache_lookup(key) {
            return if is_stream {
                cache_hit_stream_response(&state, &primary, &plan, task_key, &tool, &session, cached, started)
            } else {
                cache_hit_response(&state, &primary, &plan, task_key, &tool, &session, cached, started)
            };
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
#[allow(clippy::too_many_arguments)]
fn build_plan(
    state: &AppState,
    force_model: &Option<String>,
    task_key: &str,
    requires_tools: bool,
    input_tokens: u64,
    cached_prefix_tokens: u64,
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
        cached_prefix_tokens,
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
    state: &Arc<AppState>,
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
                            is_stream, input_tokens, attempts, trail, cache_key, started,
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
    let real_cost = ctx.entry.est_cost(prompt_tokens, completion_tokens);

    if stream_err.is_none() {
        if let Some(key) = &ctx.cache_key {
            let body = String::from_utf8_lossy(acc).into_owned();
            app.store
                .cache_store(key, &ctx.entry.model, &body, app.cfg.cache.ttl_seconds);
        }
    }

    let _ = app.store.insert(&RequestLog {
        tool: ctx.tool.clone(),
        session: ctx.session.clone(),
        task_type: ctx.task_key.clone(),
        model: ctx.entry.model.clone(),
        provider: ctx.entry.provider.clone(),
        tier: ctx.tier,
        used_fallback: ctx.used_fallback,
        degraded: ctx.degraded,
        budget_pressure: ctx.pressure,
        estimated_tokens: ctx.input_tokens,
        prompt_tokens,
        completion_tokens,
        estimated_cost_usd: ctx.entry.est_cost(ctx.input_tokens, ctx.expected_output),
        real_cost_usd: real_cost,
        latency_ms: ctx.started.elapsed().as_millis() as u64,
        status: if stream_err.is_some() { StatusCode::BAD_GATEWAY.as_u16() } else { 200 },
        attempts: ctx.attempts,
        attempt_trail: ctx.trail_json.clone(),
        stop_reason: finish,
        error: stream_err,
        ..Default::default()
    });
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
#[allow(clippy::too_many_arguments)]
fn cache_hit_stream_response(
    state: &AppState,
    entry: &ModelEntry,
    plan: &Plan,
    task_key: &str,
    tool: &str,
    session: &Option<String>,
    cached: String,
    started: Instant,
) -> Response {
    let (pt, ct, _) = parse_stream_usage(cached.as_bytes());
    let _ = state.store.insert(&RequestLog {
        tool: tool.into(),
        session: session.clone(),
        task_type: task_key.into(),
        model: entry.model.clone(),
        provider: entry.provider.clone(),
        tier: plan.tier,
        budget_pressure: plan.pressure,
        prompt_tokens: pt.unwrap_or(0),
        completion_tokens: ct.unwrap_or(0),
        latency_ms: started.elapsed().as_millis() as u64,
        status: 200,
        cache_hit: true,
        ..Default::default()
    });

    tracing::info!(task = task_key, model = %entry.model, "cache hit (stream)");
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("x-llmux-cache", "hit")
        .body(Body::from(cached))
        .unwrap()
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

#[cfg(test)]
mod tests {
    use super::*;

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
