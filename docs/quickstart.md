---
title: Quickstart
description: Run llmux in demo mode without any keys, then switch to real providers.
order: 10
---

## Build

llmux is not published to a package registry. Build it from the repository. The
dashboard is an Astro project that gets embedded into the binary at compile time, so
build it first:

```bash
git clone https://github.com/casoon/llmux
cd llmux
npm ci && npm run build     # dashboard -> dist/dashboard/
cargo install --path .      # or: cargo build --release
```

`cargo build` also works without the dashboard build. The binary then serves a
"dashboard not built" placeholder at `/`, while the proxy and the Stats API work as
usual. The same applies to `cargo install --git https://github.com/casoon/llmux`.

## Demo mode: no keys, no network

```bash
llmux --demo          # in a checkout: cargo run -- --demo
```

Demo mode uses the built-in echo provider and an in-memory database. Every request gets
a synthetic response that names the model and tier it was routed to. Open
`http://localhost:3456/` for the dashboard and send a request:

```bash
curl -s http://localhost:3456/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"messages":[{"role":"user","content":"explain the architecture trade-offs"}]}'
```

The demo catalog has one echo model per tier (`echo-nano` at tier 1 up to `echo-ultra`
at tier 5), so you can watch simple prompts land on tier 1, code tasks on tier 3 and
architecture questions on tier 4. The [showcase](../../showcase/) lists captured
requests and responses.

## Real providers

```bash
cp config/llmux.example.yaml config/llmux.yaml   # adjust providers and catalog
export OPENROUTER_API_KEY=sk-or-...              # only the keys you use
export OPENAI_API_KEY=sk-...
llmux                                             # or: cargo run
```

The example config enables OpenRouter, OpenAI and a local Ollama and sets
`auth.llmux_key: "mp_dev_changeme"`. Clients must send that value as
`Authorization: Bearer mp_dev_changeme`; an empty key disables auth (local use only).

## Run from anywhere

```bash
llmux init            # writes the example config to ~/.config/llmux/llmux.yaml
llmux init --demo     # ...or the echo demo config
llmux init --force    # overwrite an existing config
```

Config resolution, first match wins: `LLMUX_CONFIG`, then `./config/llmux.yaml`, then
`~/.config/llmux/llmux.yaml` (`$XDG_CONFIG_HOME` is honoured). If none exists, startup
fails with instructions.

The SQLite database goes to `LLMUX_DB` if set, otherwise to
`~/.local/share/llmux/llmux.sqlite` when the config came from the user directory, and
to `./data/llmux.sqlite` otherwise. The parent directory is created automatically.

## Docker

```bash
cp config/llmux.example.yaml config/llmux.yaml
cp .env.example .env                  # provider keys
docker compose up -d                  # builds the image locally
curl -fsS http://localhost:3456/healthz   # -> ok
```

There is no prebuilt image; Compose builds it from the `Dockerfile`, including the
dashboard stage. See [Deployment](../deployment/) for TLS, keys and backups.
