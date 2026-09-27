#!/usr/bin/env bash
# Regenerates the *.out files from the *.json requests against `llmux --demo`
# (built-in echo provider: no keys, no network, in-memory database).
#
#   cargo build --release && examples/capture.sh
#
# Each request runs in file order against one fresh demo instance, so
# cache-hit.json (a repeat of simple-question.json) is answered from the cache.
set -euo pipefail
cd "$(dirname "$0")"
BIN=${LLMUX_BIN:-../target/release/llmux}
PORT=3456

"$BIN" --demo >/dev/null 2>&1 &
PID=$!
trap 'kill $PID' EXIT
until curl -fsS "http://localhost:$PORT/healthz" >/dev/null 2>&1; do sleep 0.2; done

for slug in simple-question code-review architecture private-key cache-hit forced-model; do
  args=(-si "http://localhost:$PORT/v1/chat/completions" -H 'content-type: application/json' --data @"$slug.json")
  if [[ -f "$slug.headers" ]]; then
    while IFS= read -r header; do args+=(-H "$header"); done < "$slug.headers"
  fi
  response=$(curl "${args[@]}" | tr -d '\r')
  {
    printf '%s\n' "$response" | sed -n '1p'
    printf '%s\n' "$response" | sed -n '2,/^$/p' | grep -i '^x-llmux' || true
    echo
    printf '%s\n' "$response" | sed '1,/^$/d' | jq 'del(.created)'
  } > "$slug.out"
done

# What the request log recorded for the six requests above.
curl -fsS "http://localhost:$PORT/api/stats/policy" | jq . > stats-policy.out

# The transcript on the start page. The command shown is the command that runs.
HERO_CMD="curl -s localhost:$PORT/v1/chat/completions -H 'content-type: application/json' -d @architecture.json | jq '{model, usage}'"
{
  echo "\$ $HERO_CMD"
  bash -c "$HERO_CMD"
} > hero.out
