# Stats API

Read-only JSON endpoints over the SQLite request log, for the dashboard (#19) and
the future embedded UI (#20). All values are derived from real log data.

## Authentication

**None.** The Stats API is read-only and llmux is a local instance, so these endpoints
are open — the browser dashboard reads them same-origin without a token. The proxy
(`/v1/...`) stays authenticated with `auth.llmux_key`.

```bash
curl -fsS http://localhost:3456/api/stats/overview
```

## Endpoints

### `GET /api/stats/overview`

Top-line counters for the header panel.

```json
{
  "status": "online",
  "total_requests": 1542,
  "requests_per_minute": 4.2,
  "cache_hit_rate": 0.31,
  "error_count": 17,
  "p95_latency_ms": 1840,
  "cost_today": 1.27,
  "cost_month": 18.44,
  "budget_pressure": 0.64,
  "daily_max_usd": 2.0,
  "monthly_max_usd": null
}
```

- `requests_per_minute` — averaged over the last 5 minutes.
- `cache_hit_rate` — fraction `[0,1]` over all logged requests.
- `p95_latency_ms` — 95th percentile over successful (`2xx`) requests.
- `budget_pressure` — highest of daily/monthly utilization `[0,1+]`; `0` if no
  budget limits are set. `daily_max_usd` / `monthly_max_usd` are `null` when unset.

### `GET /api/stats/requests?limit=50`

Most recent route decisions, newest first. `limit` defaults to `50`, clamped to
`[1, 500]`. Returns `{ "requests": [ … ] }`.

```json
{
  "requests": [
    {
      "id": 1542,
      "time": "2026-06-07T17:42:18.123456+00:00",
      "tool": "aider",
      "session": "loop-7",
      "project": "client-api",
      "task_type": "code_review",
      "model": "gpt-4.1-mini",
      "provider": "openai",
      "tier": 3,
      "used_fallback": false,
      "degraded": false,
      "forced": false,
      "estimated_cost_usd": 0.0021,
      "real_cost_usd": 0.0019,
      "prompt_tokens": 820,
      "completion_tokens": 140,
      "latency_ms": 1310,
      "status": 200,
      "cache_hit": false,
      "attempts": 1,
      "attempt_trail": "[{\"provider\":\"openai\",\"model\":\"gpt-4.1-mini\",\"status\":200}]",
      "stop_reason": "stop",
      "error": null,
      "result": "allowed"
    }
  ]
}
```

`result` is the stored policy label per row (priority `rejected > forced > cached
> fallback > degraded > allowed`):

- `rejected` — status outside `2xx`
- `forced` — model overridden via `x-llmux-model` / alias (honored)
- `cached` — served from the response cache
- `fallback` — a fallback model in the chain answered
- `degraded` — budget pressure forced a tier below the task floor
- `allowed` — normal primary-model success

`forced` (boolean) marks an override regardless of outcome, so a forced request
that was rejected by the hard constraints (#25) still has `forced: true` with
`result: "rejected"`.

`attempt_trail` is the raw JSON string stored per request (provider/model/status
per attempt) for the request inspector.

### `GET /api/stats/models`

Per `(provider, model)` aggregates, busiest first. Returns `{ "models": [ … ] }`.

```json
{
  "models": [
    {
      "provider": "openai",
      "model": "gpt-4.1-mini",
      "requests": 384,
      "real_cost_usd": 0.42,
      "avg_tier": 3.0,
      "p50_latency_ms": 1200,
      "p95_latency_ms": 2800,
      "success_rate": 0.98,
      "error_rate": 0.02,
      "fallback_rate": 0.04,
      "cache_hit_rate": 0.10
    }
  ]
}
```

Rates are fractions `[0,1]` of that model's request count. Latency percentiles are
over the model's successful (`2xx`) requests.

### `GET /api/stats/policy`

Routing-outcome counters and the top rejection reasons. The counters are
**independent** (a request can be both `degraded` and `fallback`), not a partition.

```json
{
  "allowed": 1248,
  "rejected": 17,
  "degraded": 86,
  "fallback": 41,
  "cached": 132,
  "forced": 41,
  "forced_rejected": 3,
  "local_only": 132,
  "top_rejection_reasons": [
    { "reason": "Budgetlimit erreicht — kein Modell im Restbudget", "count": 9 },
    { "reason": "status 502", "count": 5 }
  ]
}
```

- `allowed` excludes forced overrides so the primary result categories don't overlap.
- `forced` — requests with a model override (`x-llmux-model` / alias), including
  those rejected by the hard constraints; `forced_rejected` is that rejected subset.
- `local_only` — requests classified as `private_sensitive` (privacy-forced local).
- `top_rejection_reasons` — grouped by error text (or `status <code>` when no error
  message), top 5.

### `GET /api/stats/projects`

Aggregates by project scope (`x-llmux-project`). Requests without a project run
under `(none)`. Returns `{ "projects": [ … ] }`, busiest first.

```json
{
  "projects": [
    {
      "project": "client-api",
      "requests": 129,
      "real_cost_usd": 0.18,
      "rejected": 1,
      "forced": 0,
      "local_only": 92
    }
  ]
}
```

- `local_only` — requests classified as `private_sensitive` within the project.
- `forced` — model overrides within the project.

> **Follow-up:** per-project × model mix and a local-vs-cloud split need provider
> locality joined into the log; tracked for a later iteration.

### `GET /api/stats/quality`

Post-response reliability signals. These are **operational, observable proxies —
not a semantic evaluation** of answer quality (the `note` field restates this).

```json
{
  "note": "operational quality signals (observable proxies), not semantic evaluation",
  "by_model_task": [
    {
      "model": "gpt-4.1-mini",
      "task_type": "code_review",
      "requests": 211,
      "success_rate": 0.98,
      "error_rate": 0.02,
      "fallback_rate": 0.05,
      "cache_hit_rate": 0.10,
      "avg_attempts": 1.1,
      "tool_call_missing": 3
    }
  ],
  "stop_reasons": [
    { "stop_reason": "stop", "count": 1402 },
    { "stop_reason": "tool_calls", "count": 188 },
    { "stop_reason": "length", "count": 12 }
  ],
  "tool_call_missing": 3,
  "error_clusters": [
    { "reason": "upstream 502", "count": 5 }
  ]
}
```

- `tool_call_missing` — successful (`2xx`) requests that expected tool calling
  (`tools`/`tool_choice`/tool history) but whose response contained no tool call.
  Per `(model, task_type)` and as a global total.
- `stop_reasons` — distribution over successful requests.
- `error_clusters` — non-`2xx` rows grouped by error text (or `status <code>`), top 10.

### `GET /api/stats/latency`

Latency aggregates over successful (`2xx`) requests. Provider-call latency excludes
cache hits (which are near-instant); the cache-vs-provider split reports both.
Per-model p50/p95 is available in `/api/stats/models`.

```json
{
  "by_provider": [
    { "provider": "openai", "p50_ms": 1200, "p95_ms": 2800, "samples": 384 }
  ],
  "by_task": [
    { "task_type": "code_review", "p50_ms": 1400, "p95_ms": 3100, "samples": 211 }
  ],
  "cache_hit_p50_ms": 4,
  "provider_p50_ms": 1180
}
```

Percentiles are nearest-rank. `samples` is the number of latency observations in
that group. Latency affects *routing* only under the `interactive` profile
(`x-llmux-profile` / `routing.default_profile`); otherwise it is reporting-only.

### `GET /api/stats/budget-series`

Hourly real cost over the last 24 hours (oldest → newest), gaps filled with `0`, plus
the configured cap thresholds. Drives the dashboard's budget-pressure chart.

```json
{
  "buckets": [
    { "hour": "2026-06-08T12:00", "cost": 0.0, "requests": 0 },
    { "hour": "2026-06-08T13:00", "cost": 0.21, "requests": 12 }
  ],
  "daily_max_usd": 2.0,
  "monthly_max_usd": 50.0,
  "spent_today": 1.27
}
```

`hour` is a UTC hour bucket (`%Y-%m-%dT%H:00`). The series always has exactly 24
buckets. `daily_max_usd` / `monthly_max_usd` are `null` when unset.
