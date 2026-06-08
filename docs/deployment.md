# Deployment

A reproducible server setup for llmux: container startup, TLS via reverse proxy,
key management, volume backup, and pointing your tools at the gateway.

llmux is a single binary with SQLite persistence and no external services. The
recommended deployment is the provided Docker image behind a TLS-terminating
reverse proxy.

## 1. Container startup (Docker Compose)

```bash
# 1. Create your config from the template
cp config/llmux.example.yaml config/llmux.yaml
#    edit config/llmux.yaml — enable the providers you use, set the model catalog

# 2. Create your .env with provider keys
cp .env.example .env
#    edit .env — OPENROUTER_API_KEY / OPENAI_API_KEY / ANTHROPIC_API_KEY ...

# 3. Build and start
docker compose up -d

# 4. Verify
curl -fsS http://localhost:3456/healthz   # -> ok
docker compose logs -f llmux
```

What the compose file wires up:

- **Config**: `./config/llmux.yaml` mounted read-only at `/app/config/llmux.yaml`.
- **Keys**: `.env` injected as environment variables (referenced by `api_key_env`
  / `keys[].env` in the config).
- **Persistence**: named volume `llmux-data` mounted at `/app/data` — the SQLite
  DB (request logs, budget sums, response cache) survives restarts and rebuilds.
- **Health**: `docker compose ps` shows `healthy` once `/healthz` responds.

Relevant environment variables (defaults set in the image):

- `LLMUX_CONFIG` — config path (`/app/config/llmux.yaml`)
- `LLMUX_DB` — SQLite path (`/app/data/llmux.sqlite`)
- `RUST_LOG` — e.g. `llmux=debug`

> Without Docker: build with `cargo build --release` and run `./target/release/llmux`
> under a process supervisor (systemd, etc.) with the same environment variables.

## 2. Reverse proxy with TLS

llmux speaks plain HTTP and has no built-in TLS. Terminate TLS in a reverse proxy
and forward to llmux on the loopback/compose network. Bind llmux to localhost (or
keep it on the internal Docker network) so it is never exposed directly.

### Caddy (automatic certificates)

```caddyfile
llmux.example.com {
    reverse_proxy 127.0.0.1:3456
}
```

### nginx (with your own / certbot certificate)

```nginx
server {
    listen 443 ssl;
    server_name llmux.example.com;

    ssl_certificate     /etc/letsencrypt/live/llmux.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/llmux.example.com/privkey.pem;

    location / {
        proxy_pass         http://127.0.0.1:3456;
        proxy_http_version 1.1;
        # Streaming (stream: true) must not be buffered.
        proxy_buffering    off;
        proxy_read_timeout 300s;
    }
}
```

Notes:

- **Disable response buffering** for streaming (`stream: true`) responses, as shown.
- Raise `proxy_read_timeout` for long agent/tool turns.
- If you publish only behind the proxy, change the compose `ports:` mapping to
  `"127.0.0.1:3456:3456"` so the port is not reachable from outside the host.

## 3. Key management

- **Provider keys** live in `.env` (gitignored). Never commit real keys; commit
  only `.env.example`. The config references them by env-variable name via
  `api_key_env` or `keys[].env`, so keys never appear in the YAML.
- **Rotation**: update `.env` and `docker compose up -d` (recreates the container
  with the new environment). Multi-key providers (`keys:`) allow weighted
  rotation and per-model allow/deny without downtime.
- **Gateway key**: set `auth.llmux_key` in the config to require clients to send
  `Authorization: Bearer <key>`. Requests without a valid key get `401`. The
  comparison is constant-time. Keep this key out of the repo as well (e.g. set it
  from `.env` and reference it in your config, or template the config at deploy).
- **Local-only routing**: prompts matching `privacy.block_cloud_patterns` are
  forced to local providers and never reach the cloud — useful when sensitive
  repositories share the same gateway.

## 4. Volume backup

All state is in the SQLite database inside the `llmux-data` volume. Back it up
with a consistent online copy (do not just `cp` a live DB).

```bash
# Consistent backup using sqlite's online backup into a host file
docker compose exec llmux \
  sh -c 'apt-get install -y sqlite3 >/dev/null 2>&1; \
         sqlite3 "$LLMUX_DB" ".backup /app/data/backup.sqlite"'
docker compose cp llmux:/app/data/backup.sqlite ./llmux-backup-$(date +%F).sqlite
```

Simpler alternative — archive the whole volume while the container is stopped:

```bash
docker compose stop llmux
docker run --rm -v llmux_llmux-data:/data -v "$PWD":/backup debian:bookworm-slim \
  tar czf /backup/llmux-data-$(date +%F).tar.gz -C /data .
docker compose start llmux
```

Restore by extracting the archive back into the volume (or copying the backup
file to `$LLMUX_DB`). Schedule backups (cron) and keep them off-host.

## 5. Point your tools at the gateway

Configure any OpenAI-compatible client to use the gateway as its base URL and the
gateway key (if set) as the API key. llmux overrides the model per request based
on classification, so the client's model field is mostly irrelevant.

```
Base URL: https://llmux.example.com/v1
API Key:  <auth.llmux_key from config, or any value if auth is disabled>
Model:    anything
```

Examples:

- **Aider**: `OPENAI_API_BASE=https://llmux.example.com/v1 OPENAI_API_KEY=<key> aider`
- **Continue / Cline**: set the provider to "OpenAI-compatible", base URL
  `https://llmux.example.com/v1`, API key `<key>`.
- **OpenAI SDK**: `OpenAI(base_url="https://llmux.example.com/v1", api_key="<key>")`

Optional per-request headers (`x-llmux-tool`, `x-llmux-session`, `x-llmux-model`,
`x-llmux-no-cache`, `x-llmux-no-fallback`, `x-llmux-max-cost`) are documented in
the [README](../README.md#connecting-tools).
