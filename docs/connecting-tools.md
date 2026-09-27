---
title: Connecting tools
description: Point any OpenAI-compatible client at llmux and steer routing per request with headers.
order: 20
---

Any client that lets you set an OpenAI base URL works:

| Setting  | Value                                                           |
| -------- | --------------------------------------------------------------- |
| Base URL | `http://localhost:3456/v1`                                      |
| API key  | the value of `auth.llmux_key` in your config                    |
| Model    | anything; llmux replaces it with the model it selects           |

The `model` field is only honoured when it names an alias from the `aliases` section;
otherwise selection is dynamic. To pin a model, use `x-llmux-model`.

## Request headers

| Header                  | Effect                                                                                   |
| ----------------------- | ---------------------------------------------------------------------------------------- |
| `x-llmux-tool`          | Names the calling tool in the log (`unknown` if absent).                                 |
| `x-llmux-project`       | Applies that project's scope from `projects.<name>` and tags the log entry.              |
| `x-llmux-profile`       | `interactive`, `balanced` or `batch`; overrides `routing.default_profile`.                |
| `x-llmux-session`       | Keeps an agent loop on the same model while the budget allows.                           |
| `x-llmux-model`         | Forces a catalog model (`model` or `provider/model`) or an alias.                         |
| `x-llmux-no-cache`      | `true`, `1` or `yes`: skip cache lookup and store.                                       |
| `x-llmux-no-fallback`   | `true`, `1` or `yes`: try only the primary model.                                        |
| `x-llmux-max-cost`      | Reject with `402` if the estimated cost in USD exceeds this value, e.g. `0.05`.          |

A forced model does not bypass hard constraints. It is still checked for provider and key
readiness, tool support, context fit, privacy `local_only` and remaining budget; a
violation is rejected and logged with `forced: true`, `result: rejected`.

## Response headers

| Header             | Meaning                                         |
| ------------------ | ----------------------------------------------- |
| `x-llmux-cache`    | `hit` when the response came from the cache.    |
| `x-llmux-echo`     | `1` when the built-in echo provider answered.   |

## Agent loops and tools

Requests with `tools`, `tool_choice`, `functions` or `tool` messages in the history are
routed only to models with the `tools` capability (`supports_tools: true`). The tool
schema is passed through unchanged and counted in the token estimate, so models whose
context window is too small drop out as the history grows. Send `x-llmux-session: <id>`
to keep tool calls and tool results on one model.

## Example

```bash
curl -s http://localhost:3456/v1/chat/completions \
  -H 'Authorization: Bearer mp_dev_changeme' \
  -H 'content-type: application/json' \
  -H 'x-llmux-tool: curl' \
  -H 'x-llmux-model: echo-ultra' \
  -d '{"messages":[{"role":"user","content":"Hello"}]}'
```

The [forced model example](../../showcase/forced-model/) shows the response.
