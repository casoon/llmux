# llmux

Intent-based local LLM router. llmux sits as an OpenAI-compatible proxy between your tools (Aider, Continue, Claude Code, custom agents) and AI providers. It evaluates every prompt **before** sending and decides which model, provider, and cost tier makes sense.

```
┌─────────────────────────────────────────────────────┐
│      Aider  ·  Continue  ·  Claude Code  ·  Agent   │
└──────────────────────┬──────────────────────────────┘
                       │  OpenAI-compatible API
                       ▼
┌─────────────────────────────────────────────────────┐
│                      llmux                          │
├─────────────────────────────────────────────────────┤
│  token estimation  ·  intent classification         │
│  privacy filter    ·  budget pressure               │
│  dynamic model selection  ·  tool-aware routing     │
│  session stickiness  ·  exact-match cache           │
│  smart retry + fallback  ·  request logging         │
└───────┬──────────────────────┬──────────┬───────────┘
        │                      │          │
        ▼                      ▼          ▼
   OpenRouter               OpenAI     Ollama
        │                      │          │
        └──────────────────────┴──────────┘
                               │
                               ▼
               cheapest viable model for the task
```

## Status: Prototype (v0.1)

- **OpenAI-compatible endpoint** `POST /v1/chat/completions` (+ `GET /healthz`)
- **Token estimation** across the **entire** request (message history **+ tool schemas**)
- **Privacy filter**: detects secrets/keys by pattern → forces local-only providers
- **Rule-based classification** into `task_type` (`simple_text`, `summarize`, `code_review`, `architecture`, `private_sensitive`)
- **Dynamic, tier-based selector** (see below) instead of a fixed routing table
- **Provider forwarding** to OpenAI-compatible backends (OpenRouter, OpenAI, Ollama)
- **Smart retry + error classification**: transient errors (5xx/429/network) → same model with jittered backoff; 401/402/403 → next model; other 4xx → abort without fallback
- **Automatic fallback** along the valid candidate chain
- **Exact-match cache** (SQLite, no embeddings): identical requests to the same model cost $0
- **Per-request overrides** via `x-llmux-*` headers (force model, disable cache/fallback, cost cap)
- **Streaming passthrough** (`stream: true`)
- **SQLite logging** of every request (model, tier, tokens, cost, budget pressure, `degraded`, fallback, `attempts`, `attempt_trail`, `cache_hit`, `stop_reason`, session, errors)

Not yet included (by design): semantic cache, LLM-based classification, native Anthropic adapter (currently via OpenRouter), dashboard, multi-user.

> **Note:** Verified against mock providers only so far. Real end-to-end tests are on the roadmap.

## Agentic Workflows

llmux is designed for agent loops with tool calling:

- **Tool-aware routing**: requests with `tools`/`tool_choice`/`functions` or `tool` roles in history are routed only to models with `supports_tools: true`. The full OpenAI tool schema is passed through unchanged.
- **Context size matters**: token estimation accounts for the growing message history **and** tool definitions; models whose context window is insufficient are automatically excluded.
- **Session stickiness**: with header `x-llmux-session: <id>`, a loop stays on the same model (tool-call/tool-result consistency) as long as the budget allows.

## Dynamic Token/Cost Optimization

Instead of a fixed "task → model" mapping, the selector dynamically picks per request:

1. **Quality floor**: each task has a `min_tier` (1 = cheap/local … 5 = top reasoning).
2. **Hard filters**: tool capability, local provider (privacy), context window, provider available (enabled + key set).
3. **Budget pressure**: from the real cost sum (daily/monthly), utilization is calculated. Above configurable thresholds (`pressure_downgrade`), the allowed **tier ceiling** drops — graceful downgrade instead of hard cutoff.
4. **Cheapest-viable**: from valid candidates, the **cheapest** is selected (estimated cost = input tokens + expected output via `expected_output_ratio`).
5. Only when even the cheapest model exceeds the **remaining budget** → `402`.

Example: at low utilization, `architecture` goes to a tier-4 model; above 50% daily budget, the same task is downgraded to a cheaper tier (logged as `degraded = 1`).

## Architecture

Single binary with modules (matching planned crates, separable later):

| Module         | Responsibility                                          |
|----------------|---------------------------------------------------------|
| `api`          | HTTP layer, request pipeline                            |
| `classifier`   | Prompt → `task_type` + tool-use detection               |
| `privacy`      | Sensitive content detection                             |
| `router`       | Dynamic selector (tier/tools/budget) + sessions         |
| `providers`    | Forwarding to OpenAI-compatible providers               |
| `cost`         | Token estimation (messages + tools)                     |
| `cache`        | Exact-match cache key + normalization                   |
| `logging`      | SQLite persistence, budget sums, response cache         |
| `config`       | YAML configuration (model catalog, task rules)          |

## Getting Started

```bash
# Copy the example config and adjust as needed
cp config/llmux.example.yaml config/llmux.yaml

# Set provider keys (only the ones you use)
export OPENROUTER_API_KEY=sk-or-...
export OPENAI_API_KEY=sk-...

cargo run
# -> llmux running on http://0.0.0.0:3456
```

Environment variables:

- `LLMUX_CONFIG` – path to config (default: `config/llmux.yaml`)
- `LLMUX_DB` – path to SQLite DB (default: `data/llmux.sqlite`)
- `RUST_LOG` – e.g. `llmux=debug`

### Docker

```bash
cp config/llmux.example.yaml config/llmux.yaml   # edit providers/catalog
cp .env.example .env                             # add provider keys
docker compose up -d
curl -fsS http://localhost:3456/healthz          # -> ok
```

The SQLite DB persists in the `llmux-data` volume; the config is mounted read-only
and keys come from `.env`. For TLS (reverse proxy), key management, volume backup,
and pointing your tools at the gateway, see **[docs/deployment.md](docs/deployment.md)**.

## Connecting Tools

```
Base URL: http://localhost:3456/v1
API Key:  <auth.llmux_key from config>
Model:    anything — llmux overrides the model based on classification
```

Optional headers:

- `x-llmux-tool: aider` — identifies the tool in logs
- `x-llmux-project: <name>` — applies that project's routing scope (see Configuration) and tags it in logs
- `x-llmux-profile: interactive|balanced|batch` — routing profile; `interactive` prefers lowest expected latency over cost (see Routing Policy)
- `x-llmux-session: <id>` — keeps an agent loop on the same model (stickiness)
- `x-llmux-model: <model|provider/model>` — forces a catalog model (bypasses selection)
- `x-llmux-no-cache: true` — skip cache for this request
- `x-llmux-no-fallback: true` — try only the primary model
- `x-llmux-max-cost: 0.05` — reject (`402`) if estimated cost exceeds this value

Response header `x-llmux-cache: hit` marks a cached response.

## Configuration

See `config/llmux.example.yaml`. Key sections:

- `models` — catalog with `tier`, `context`, prices (USD/1M tokens), and `capabilities` (e.g. `tools`, `json_schema`, `vision`, …); `supports_tools: true` is sugar for the `tools` capability. Unknown capability names are rejected at startup
- `classification` — per `task_type`: `min_tier`, optional `require_tools` / `require_capabilities` / `local_only` / `expected_output_ratio`
- `aliases` — logical names (`fast`/`best`/`cheap`) → catalog model; resolved from `x-llmux-model` or the request `model` field before selection
- `projects` — per-project routing scopes (`projects.<name>`), resolved from `x-llmux-project`: `local_only`, `min_tier` (raises the quality floor), `require_providers` / `forbid_providers`. Merged into the policy before selection; forced overrides are validated against them too. No project header → unchanged default behavior
- `budgets` — `daily_max_usd`, `monthly_max_usd` and `pressure_downgrade` (tier throttling)
- `routing.default_profile` — `balanced` (default, cheapest-viable), `interactive` (prefer lowest expected latency), or `batch`; overridden per request by `x-llmux-profile`
- `retry` — `max_retries`, `backoff_initial_ms`, `backoff_max_ms`
- `cache` — `enabled`, `ttl_seconds`, `max_conversation_messages` (history guard), `eviction_interval_seconds`, optional `max_entries` (row cap)
- `classifier.user_messages` — number of latest `user` messages the rule-based classifier derives `task_type` from (default `1`); the large static agent-client prefix (system prompt, tool schemas, history) is excluded so it doesn't skew the quality floor
- `privacy.block_cloud_patterns` — triggers for local-only routing. Scan surface: user/tool message content **and** tool/function schemas. `privacy.scan_system` (default `false`) additionally scans injected `system`/`assistant` content — off by default so client boilerplate doesn't spuriously force `local_only`
- `providers` — backends including `local: true` for local providers (Ollama), `kind: anthropic` for the native Anthropic adapter (translates to `/v1/messages`; non-streaming), `strip_params` to drop request fields the backend doesn't support (also per-model on `models[]`), `keys` for multiple weighted API keys (weighted-random selection; rotate on `401/402/403/429` before model fallback), and `prompt_caching` + `cache_billed_fraction` (default `0.1`) to discount the repeated prompt prefix in the **routing** cost estimate (real billing is unchanged)

## Routing Policy

llmux is a local **governance layer**, not just a cheapest-model router. Every
request is decided along explicit policy dimensions, and the decision is recorded
so it can be explained and reported:

| Dimension    | Enforced by                                              | Logged as                          |
|--------------|----------------------------------------------------------|------------------------------------|
| `privacy`    | secret patterns → `local_only` (cloud excluded)          | `task_type=private_sensitive`      |
| `capability` | required capabilities (`tools`/`json_schema`/`vision`) + context fit | `tier`, `model`, `provider`        |
| `quality`    | task `min_tier` floor → selected tier                    | `tier`                             |
| `cost`       | budget-pressure tier downgrade, remaining-budget gate    | `budget_pressure`, `degraded`      |
| `override`   | `x-llmux-model` / alias forces a model                   | `forced`                           |

Each request stores a single **policy result** label — one of `allowed`, `forced`,
`cached`, `fallback`, `degraded`, `rejected` — alongside a `forced` flag and, for
rejections, the reason (`error`). These are surfaced via `GET /api/stats/policy`
(see [docs/stats-api.md](docs/stats-api.md)).

**Forced overrides do not bypass safety/capability constraints.** A forced model is
still validated against provider/key readiness, required tool support, context fit,
privacy `local_only`, and remaining budget — it only bypasses cheapest-viable
selection. An override that violates a hard constraint is rejected (`forced: true`,
`result: rejected`).

**Latency** is both reported and (optionally) used for routing. It is always
aggregated for reporting (`GET /api/stats/latency`, p50/p95 by provider/task). It
affects *selection* only under the `interactive` profile, which orders viable
candidates by lowest expected p50 latency (from logs) before cost — all hard filters
and the budget gate still apply. The default `balanced` profile is cheapest-viable.

## Querying Logs

```bash
sqlite3 data/llmux.sqlite \
  "SELECT task_type, model, tier, COUNT(*),
          ROUND(SUM(real_cost_usd),4) AS cost, SUM(degraded) AS downgrades
   FROM requests GROUP BY task_type, model, tier;"
```

## Stats API

Read-only JSON endpoints over the request log, authenticated with the same
`auth.llmux_key` as the proxy:

- `GET /api/stats/overview` — requests/min, cost today/month, budget pressure, cache hit rate, p95 latency
- `GET /api/stats/requests?limit=50` — recent route decisions (live feed / inspector)
- `GET /api/stats/models` — per-model cost, latency p50/p95, success/error/fallback/cache rates
- `GET /api/stats/policy` — allowed/rejected/degraded/fallback/cached/forced/local-only counts + top rejection reasons
- `GET /api/stats/projects` — per-project requests, cost, rejects, forced, local-only
- `GET /api/stats/quality` — per-model/task reliability proxies: success/error/fallback rates, stop-reason distribution, "tools expected but no tool call", error clusters (operational signals, not semantic evaluation)
- `GET /api/stats/latency` — p50/p95 latency by provider and task type, plus cache-hit vs provider latency

Response shapes are documented in **[docs/stats-api.md](docs/stats-api.md)**.

## License

Apache License 2.0 — see [LICENSE](LICENSE).
