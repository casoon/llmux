# End-to-End Test Against a Live Provider (T1.1)

This proves the full pipeline — token estimation, classification, routing,
forwarding, real token/cost accounting, and SQLite logging — works against a
real cloud provider.

## Prerequisites

- A provider key in the environment. Either:
  - `OPENROUTER_API_KEY` (recommended — covers all catalog models), or
  - `OPENAI_API_KEY` (covers the `openai` provider models only).
- A config file at `config/llmux.yaml`. Copy the template:

  ```sh
  cp config/llmux.example.yaml config/llmux.yaml
  ```

  The example config has `auth.llmux_key: "mp_dev_changeme"` — used as the
  `Authorization: Bearer` token below.

## 1. Start the server

```sh
export OPENROUTER_API_KEY=sk-or-...      # your key
cargo run --release
```

Expected startup log:

```
INFO llmux: Konfiguration geladen und validiert source=config/llmux.yaml
INFO llmux: SQLite-Log geöffnet path=data/llmux.sqlite
INFO llmux: llmux läuft auf http://localhost:3456  (Dashboard unter /)
```

The read-only dashboard is now served at `http://localhost:3456/`; the requests
below show up in its live feed.

If the config is malformed the server exits **before** binding, e.g.:

```
Error: Konfiguration ungültig:
  - Modell 'x' verweist auf unbekannten Provider 'ghost'
```

## 2. Send a `simple_text` request

Routed to the cheapest tier-1 model (a free/cheap model in the example catalog).

```sh
curl -s http://localhost:3456/v1/chat/completions \
  -H "Authorization: Bearer mp_dev_changeme" \
  -H "Content-Type: application/json" \
  -H "x-llmux-tool: e2e-test" \
  -d '{"messages":[{"role":"user","content":"Say hello in one short sentence."}]}' | jq .
```

Expected: a normal OpenAI-shaped response with a `choices[0].message.content`
string and a `usage` block.

## 3. Send a `code_review` request

`code_review` has `min_tier: 3`, so the selector picks a higher-tier model
(e.g. `gpt-4.1-mini`).

```sh
curl -s http://localhost:3456/v1/chat/completions \
  -H "Authorization: Bearer mp_dev_changeme" \
  -H "Content-Type: application/json" \
  -H "x-llmux-tool: e2e-test" \
  -d '{"messages":[{"role":"user","content":"Review this Rust function for bugs: fn add(a:i32,b:i32)->i32{a-b}"}]}' | jq .
```

The server log line for each request shows the chosen model and real cost:

```
INFO llmux::api: request ok task=simple_text provider=openrouter model=... tier=1 prompt_tokens=23 completion_tokens=9 cost_usd=0.0000...
INFO llmux::api: request ok task=code_review provider=openai model=gpt-4.1-mini tier=3 prompt_tokens=41 completion_tokens=120 cost_usd=0.00021...
```

## 4. Verify the logged entries (Definition of Done)

The DoD requires a log entry with `status=200`, `real_cost_usd > 0`, and the
correct `model`. Query the SQLite log directly:

```sh
sqlite3 -header -column data/llmux.sqlite \
  "SELECT task_type, model, provider, tier, status, prompt_tokens,
          completion_tokens, real_cost_usd
   FROM requests
   WHERE tool = 'e2e-test'
   ORDER BY id DESC LIMIT 2;"
```

Expected (illustrative — exact tokens/cost depend on the model and prompt):

```
task_type    model          provider    tier  status  prompt_tokens  completion_tokens  real_cost_usd
-----------  -------------  ----------  ----  ------  -------------  -----------------  -------------
code_review  gpt-4.1-mini   openai      3     200     41             120                0.0002104
simple_text  ...            openrouter  1     200     23             9                  0.0000...
```

Pass criteria:

- `status = 200` for both rows.
- `real_cost_usd > 0` for the `code_review` row (a tier-3 paid model). A free
  tier-1 model legitimately logs `real_cost_usd = 0`; to assert a non-zero cost
  on the cheap request too, force a paid model:
  `-H "x-llmux-model: google/gemini-flash-1.5"`.
- `prompt_tokens` / `completion_tokens` come from the provider's `usage` block
  (not the local estimate), and `real_cost_usd` is derived from them.

> Note: this run requires a live provider key and therefore must be executed
> manually. Everything else in the pipeline is covered by `cargo test`
> (31 unit tests, no network).
