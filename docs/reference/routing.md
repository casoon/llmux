---
title: Routing
description: How llmux classifies a request, filters the model catalog and picks one model, and what it logs about the decision.
order: 10
---

## Classification

The rule-based classifier looks at the latest `user` message only
(`classifier.user_messages`, default `1`), so the large static prefix of agent clients
(system prompt, tool schemas, history) does not skew it. It checks keyword groups in this
order and stops at the first match:

| `task_type`    | Examples of keywords                                                         |
| -------------- | ---------------------------------------------------------------------------- |
| `architecture` | architecture, design pattern, database schema, security, scalab, trade-off   |
| `code_review`  | refactor, bug, fix, test, rust, typescript, review this, function, implement |
| `summarize`    | summarize, summary, tl;dr, explain, translate                                |
| `simple_text`  | everything else                                                              |

The keyword lists also contain German terms. `private_sensitive` is never set by the
classifier; only the privacy scan sets it. With `classifier.llm` enabled, a small local
model returns the label instead, and any error or timeout falls back to the rules.

## Selection

The selector applies its steps in this order:

1. **Quality floor.** The task's `min_tier`, raised by a project's `min_tier` if set.
2. **Hard filters.** Required capabilities (`tools` when the request uses tools, plus
   `json_schema` or `vision` when the request needs them, plus the task's
   `require_capabilities`), `local_only` from privacy or project, context window
   (estimated input plus expected output), provider enabled and key present, project
   allow and deny lists.
3. **Budget pressure.** Utilisation is today's or this month's real cost divided by
   `daily_max_usd` or `monthly_max_usd`, whichever is higher. Every
   `pressure_downgrade` rule with `at` at or below that value caps the tier; the lowest
   cap wins. If the cap falls below the quality floor, the request is marked
   `degraded`.
4. **Cheapest viable.** Among the remaining models, the lowest estimated cost wins:
   input tokens plus `expected_output_ratio` times input tokens, at catalog prices.
   Under the `interactive` profile, lowest expected p50 latency from the log comes
   first and cost breaks ties. On equal rank, local models come first when
   `privacy.local_first` is set, then the higher tier. With `x-llmux-session`, the
   model chosen earlier in the session is kept while it stays valid.
5. **Budget gate.** Only if even the cheapest candidate exceeds the remaining budget
   does llmux answer `402`.

With the example config's budget rules, the allowed ceiling drops to tier 4 at 50 %
utilisation, tier 2 at 80 % and tier 1 at 95 %.

## Retry and fallback

| Provider answer         | Reaction                                                      |
| ----------------------- | ------------------------------------------------------------- |
| 5xx, 429, network error | retry the same model with jittered exponential backoff        |
| 401, 402, 403           | rotate to the next key if configured, then the next model     |
| other 4xx               | abort, no fallback                                            |

`retry.max_retries`, `backoff_initial_ms` and `backoff_max_ms` tune the backoff.
`x-llmux-no-fallback: true` limits a request to its primary model.

## Cache

The exact-match cache keys on the selected model and the normalised request. A hit
costs nothing and carries `x-llmux-cache: hit`. Conversations longer than
`cache.max_conversation_messages` are not cached. Expired rows are removed every
`eviction_interval_seconds`, and `max_entries` caps the table. The optional semantic
stage (`cache.semantic`) compares embeddings of the user text against earlier requests
to the same model after an exact miss (non-streaming requests only).

## Policy result

Every request is logged with one policy label:

| Result     | Meaning                                                        |
| ---------- | -------------------------------------------------------------- |
| `allowed`  | selected dynamically and answered                              |
| `forced`   | pinned by `x-llmux-model` or an alias                          |
| `cached`   | answered from the cache                                        |
| `fallback` | answered by a later candidate after the first one failed       |
| `degraded` | answered below the task's quality floor because of budget      |
| `rejected` | refused, with the reason in `error`                            |

`GET /api/stats/policy` aggregates these labels; see the [Stats API](../../stats-api/).
