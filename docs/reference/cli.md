---
title: Command line
description: The llmux commands, environment variables and HTTP endpoints.
order: 30
---

## Commands

Output of `llmux --help` (v0.1.0):

```text
llmux — local intent-based LLM router

USAGE:
  llmux                 Run the proxy + dashboard (resolves your config)
  llmux --demo          Run with the built-in echo provider (no key/cloud, in-memory)
  llmux init            Write an example config to ~/.config/llmux/llmux.yaml
  llmux init --demo     Write the echo demo config there instead
  llmux init --force    Overwrite an existing config
  llmux --help          Show this help

Config resolution: LLMUX_CONFIG -> ./config/llmux.yaml -> ~/.config/llmux/llmux.yaml
Dashboard + Stats API are served at /. Env: LLMUX_CONFIG, LLMUX_DB, RUST_LOG.
```

## Environment

| Variable        | Purpose                                                                      |
| --------------- | ---------------------------------------------------------------------------- |
| `LLMUX_CONFIG`  | Path to the config file; takes precedence over the lookup order.             |
| `LLMUX_DB`      | Path to the SQLite database.                                                 |
| `RUST_LOG`      | Log filter, e.g. `llmux=debug`.                                              |
| provider keys   | Whatever `api_key_env` or `keys[].env` name, e.g. `OPENROUTER_API_KEY`.      |

## HTTP endpoints

| Method and path                | Auth                     | Purpose                                     |
| ------------------------------ | ------------------------ | ------------------------------------------- |
| `POST /v1/chat/completions`    | `Bearer auth.llmux_key`  | OpenAI-compatible proxy, streaming included |
| `GET /healthz`                 | none                     | returns `ok`                                |
| `GET /api/stats/*`             | none                     | [Stats API](../../stats-api/), read-only    |
| `GET /`                        | none                     | embedded dashboard                          |

The Stats API and the dashboard are unauthenticated because llmux is meant to run
locally. When you expose llmux beyond localhost, restrict `/api/` and `/` at the
reverse proxy; see [Deployment](../../deployment/).
