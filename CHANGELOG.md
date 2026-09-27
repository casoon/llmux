# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
No version has been tagged yet; `Cargo.toml` is at 0.1.0.

## [Unreleased]

### Added

- OpenAI-compatible proxy: `POST /v1/chat/completions` with streaming passthrough, and
  `GET /healthz`.
- Token estimation over message history and tool schemas, rule-based classification into
  five task types, and a privacy scan that forces local-only routing.
- Tier-based selector: quality floor, capability, context and provider filters,
  budget-pressure downgrade, cheapest-viable choice and a remaining-budget gate.
- Retry with jittered backoff, error classification and fallback along the candidate chain.
- Exact-match SQLite cache with TTL, history guard and eviction; optional semantic cache.
- Optional LLM classification by a small local model, with fallback to the rules.
- Native Anthropic adapter, weighted multi-key providers, model aliases and parameter
  sanitisation.
- Routing governance: project scopes, routing profiles, latency as a routing and reporting
  dimension, and a capability catalog.
- Request pipeline as an ordered plugin chain for budget, cache and logging.
- SQLite request log with policy result per request, and a read-only Stats API
  (`/api/stats/*`, eight endpoints).
- Astro dashboard, embedded in the binary and served at `/`.
- `Dockerfile` and `docker-compose.yml`.
- Config lookup in the user directory, `llmux init`, and `llmux --demo` with a built-in echo
  provider.
- Per-request override headers `x-llmux-*`.

### Security

- Constant-time comparison of the gateway key.
