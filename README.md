# llmux

Intent-based local LLM router. llmux sits as an OpenAI-compatible proxy between your tools (Aider, Continue, Claude Code, custom agents) and AI providers. It evaluates every prompt **before** sending and decides which model, provider, and cost tier makes sense.

```
Aider / Continue / Claude Code / Agent
        ↓
   llmux   ← Tokens · Tool-Use · Privacy · Classification · Budget Pressure · Dynamic Model Selection · Logging
        ↓
OpenRouter / OpenAI / Ollama
        ↓
      Model
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

## Connecting Tools

```
Base URL: http://localhost:3456/v1
API Key:  <auth.llmux_key from config>
Model:    anything — llmux overrides the model based on classification
```

Optional headers:

- `x-llmux-tool: aider` — identifies the tool in logs
- `x-llmux-session: <id>` — keeps an agent loop on the same model (stickiness)
- `x-llmux-model: <model|provider/model>` — forces a catalog model (bypasses selection)
- `x-llmux-no-cache: true` — skip cache for this request
- `x-llmux-no-fallback: true` — try only the primary model
- `x-llmux-max-cost: 0.05` — reject (`402`) if estimated cost exceeds this value

Response header `x-llmux-cache: hit` marks a cached response.

## Configuration

See `config/llmux.example.yaml`. Key sections:

- `models` — catalog with `tier`, `context`, `supports_tools`, prices (USD/1M tokens)
- `classification` — per `task_type`: `min_tier`, optional `require_tools` / `local_only` / `expected_output_ratio`
- `budgets` — `daily_max_usd`, `monthly_max_usd` and `pressure_downgrade` (tier throttling)
- `retry` — `max_retries`, `backoff_initial_ms`, `backoff_max_ms`
- `cache` — `enabled`, `ttl_seconds`, `max_conversation_messages` (history guard)
- `privacy.block_cloud_patterns` — triggers for local-only routing
- `providers` — backends including `local: true` for local providers (Ollama)

## Querying Logs

```bash
sqlite3 data/llmux.sqlite \
  "SELECT task_type, model, tier, COUNT(*),
          ROUND(SUM(real_cost_usd),4) AS cost, SUM(degraded) AS downgrades
   FROM requests GROUP BY task_type, model, tier;"
```

## License

Apache License 2.0 — see [LICENSE](LICENSE).
