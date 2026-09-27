---
title: Configuration
description: The sections of llmux.yaml, with the values from config/llmux.example.yaml.
order: 20
---

The template is
[`config/llmux.example.yaml`](https://github.com/casoon/llmux/blob/main/config/llmux.example.yaml);
the demo config is
[`config/llmux.demo.yaml`](https://github.com/casoon/llmux/blob/main/config/llmux.demo.yaml).
Both are embedded in the binary and written by `llmux init` and `llmux init --demo`. The
config is validated at startup: a model on an unknown provider, an unknown capability
name or an alias pointing to an unknown model stops the server before it binds. The snippets below are taken from the example config, with
its comments left out.

## `server` and `auth`

```yaml
server:
  host: "0.0.0.0"
  port: 3456

auth:
  llmux_key: "mp_dev_changeme"
```

Clients send `Authorization: Bearer <llmux_key>` to `/v1/...`. An empty key disables
auth; only do that on a machine nobody else can reach.

## `providers`

```yaml
providers:
  openrouter:
    enabled: true
    base_url: "https://openrouter.ai/api/v1"
    api_key_env: "OPENROUTER_API_KEY"
  openai:
    enabled: true
    base_url: "https://api.openai.com/v1"
    api_key_env: "OPENAI_API_KEY"
  ollama:
    enabled: true
    base_url: "http://localhost:11434/v1"
    api_key_env: null
    local: true
    strip_params: ["frequency_penalty", "presence_penalty", "logit_bias"]
  anthropic:
    enabled: false
    kind: anthropic
    base_url: "https://api.anthropic.com/v1"
    api_key_env: "ANTHROPIC_API_KEY"
    prompt_caching: true
    cache_billed_fraction: 0.1
```

| Key                      | Meaning                                                                                              |
| ------------------------ | ---------------------------------------------------------------------------------------------------- |
| `kind`                   | default OpenAI-compatible; `anthropic` translates to `/v1/messages` (non-streaming); `echo` is the demo provider |
| `local`                  | the provider stays eligible for `local_only` requests                                                 |
| `api_key_env`            | environment variable holding the key; a provider whose key is missing is skipped                     |
| `keys`                   | instead of `api_key_env`: several keys with `env`, `weight` and optional `allow` list; weighted-random choice, rotation on 401, 402, 403 and 429 |
| `strip_params`           | request fields removed before sending (also per model)                                               |
| `prompt_caching`         | discount the repeated prompt prefix in the routing estimate by `cache_billed_fraction`; billing is unchanged |

## `models`

The catalog the selector chooses from. Prices are USD per million tokens.

```yaml
models:
  - { provider: ollama, model: "qwen2.5", tier: 1, context: 32000, supports_tools: false, input_per_mtok: 0.0, output_per_mtok: 0.0 }
  - { provider: openrouter, model: "google/gemini-flash-1.5", tier: 2, context: 1000000, supports_tools: true, capabilities: ["json_schema", "vision", "large_context"], input_per_mtok: 0.075, output_per_mtok: 0.30 }
  - { provider: openai, model: "gpt-4.1-mini", tier: 3, context: 128000, supports_tools: true, capabilities: ["json_schema", "vision"], input_per_mtok: 0.40, output_per_mtok: 1.60 }
  - { provider: openrouter, model: "anthropic/claude-3.5-sonnet", tier: 4, context: 200000, supports_tools: true, capabilities: ["json_schema", "vision"], input_per_mtok: 3.00, output_per_mtok: 15.00 }
  - { provider: openrouter, model: "anthropic/claude-3-opus", tier: 5, context: 200000, supports_tools: true, capabilities: ["vision"], input_per_mtok: 15.00, output_per_mtok: 75.00 }
```

Tier 1 means cheap or local, tier 5 top reasoning. Known capabilities: `tools`,
`json_schema`, `vision`, `streaming`, `streaming_usage`, `large_context`, `reasoning`,
`strict_tool_schema`. `supports_tools: true` is shorthand for `tools`. The example
config lists eight models; five are shown here.

## `classification`

```yaml
classification:
  simple_text:       { min_tier: 1, expected_output_ratio: 0.5 }
  summarize:         { min_tier: 1, expected_output_ratio: 0.3 }
  code_review:       { min_tier: 3, expected_output_ratio: 1.0 }
  architecture:      { min_tier: 4, expected_output_ratio: 1.5 }
  private_sensitive: { min_tier: 1, local_only: true, expected_output_ratio: 1.0 }
```

Optional per task: `require_tools`, `require_capabilities`, `local_only`.

## `aliases` and `projects`

```yaml
aliases:
  fast: "google/gemini-flash-1.5"
  best: "anthropic/claude-3-opus"
  cheap: "meta-llama/llama-3.1-8b-instruct:free"

projects:
  client-api:
    local_only: true
  checkout:
    min_tier: 3
  docs:
    forbid_providers: ["openai"]
```

An alias in `x-llmux-model` or in the request's `model` field forces its target. A
project scope applies when the request sends `x-llmux-project: <name>`; it can set
`local_only`, raise `min_tier`, and restrict providers with `require_providers` or
`forbid_providers`.

## `budgets` and `routing`

```yaml
budgets:
  daily_max_usd: 2.00
  monthly_max_usd: 30.00
  pressure_downgrade:
    - { at: 0.50, max_tier: 4 }
    - { at: 0.80, max_tier: 2 }
    - { at: 0.95, max_tier: 1 }

routing:
  default_profile: balanced
```

Profiles: `balanced` (cheapest viable), `interactive` (lowest expected latency first),
`batch` (currently the same as `balanced`). See [Routing](../routing/).

## `retry` and `cache`

```yaml
retry:
  max_retries: 2
  backoff_initial_ms: 500
  backoff_max_ms: 8000

cache:
  enabled: true
  ttl_seconds: 1800
  max_conversation_messages: 3
  eviction_interval_seconds: 300
  max_entries: 10000
```

The optional `cache.semantic` block (`enabled`, `base_url`, `model`, `api_key_env`,
`timeout_ms`, `threshold`, default `0.85`) adds an embedding-based second stage.

## `privacy` and `classifier`

```yaml
privacy:
  local_first: true
  block_cloud_patterns: [".env", "PRIVATE_KEY", "API_KEY", "BEGIN RSA", "password", "secret"]
  scan_system: false

classifier:
  user_messages: 1
```

With `local_first: true`, local models win ties on estimated cost. The privacy scan covers user and tool message content and tool or function schemas.
`scan_system: true` also scans `system` and `assistant` content. The optional
`classifier.llm` block (`enabled`, `base_url`, `model`, `api_key_env`, `timeout_ms`,
default `1500`) lets a small local model choose the task type.
