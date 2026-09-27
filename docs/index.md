---
title: Overview
description: What llmux does with a request between your tool and your model provider, and where to read on.
order: 0
---

llmux is a single Rust binary that speaks the OpenAI chat completions API. Tools such as
Aider, Continue, Claude Code or your own agents send their requests to llmux instead of
to a provider. llmux decides, per request, which model, provider and cost tier to use,
forwards the request and returns the provider's response unchanged in shape.

The project is a prototype (v0.1.0). The automated suite runs against mock providers and
the built-in echo provider; checking against a live provider is a manual step, described
in [End-to-end test](./end-to-end/).

## What happens to a request

1. **Token estimate** over the whole request: message history and tool schemas.
2. **Privacy scan.** If user or tool content (or a tool schema) matches one of
   `privacy.block_cloud_patterns`, the task becomes `private_sensitive` and only
   providers marked `local: true` remain eligible.
3. **Classification** into a `task_type`: `simple_text`, `summarize`, `code_review` or
   `architecture` (keyword rules, optionally a small local LLM).
4. **Selection.** The task's `min_tier` sets the quality floor. Models without the
   required capabilities, without enough context, or on a disabled provider drop out.
   Budget pressure can lower the tier ceiling. The cheapest remaining candidate wins.
5. **Forwarding** with retry and fallback: transient errors retry the same model,
   exhausted providers fall through to the next candidate, other 4xx abort.
6. **Logging.** Model, tier, tokens, real cost, cache hit, fallback and the policy
   result go into SQLite. The Stats API and the embedded dashboard read from there.

The details are in [Routing](./reference/routing/).

## Where to go next

- [Quickstart](./quickstart/): demo mode, real providers, standalone install.
- [Connecting tools](./connecting-tools/): base URL, key and the `x-llmux-*` headers.
- [Deployment](./deployment/): Docker Compose, TLS, keys, backups.
- [Configuration](./reference/configuration/): every section of `llmux.yaml`.
- [Stats API](./stats-api/): the JSON endpoints behind the dashboard.
