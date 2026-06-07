# llmux

Lokaler, intent-basierter LLM-Router. llmux sitzt als OpenAI-kompatibler Proxy
zwischen deinen Tools (Aider, Continue, Claude Code, eigene Agenten) und den
KI-Anbietern. Er bewertet jeden Prompt **vor** dem Senden und entscheidet, welches
Modell, welcher Provider und welche Kostenstufe sinnvoll sind.

```
Aider / Continue / Claude Code / Agent
        ↓
   llmux   ← Tokens · Tool-Use · Privacy · Klassifikation · Budgetdruck · dyn. Modellwahl · Logging
        ↓
OpenRouter / OpenAI / Ollama
        ↓
      Modell
```

## Status: Prototyp (v0.1)

- **OpenAI-kompatibler Endpoint** `POST /v1/chat/completions` (+ `GET /healthz`)
- **Token-Schätzung** über den **gesamten** Request (Message-History **+ Tool-Schemas**)
- **Privacy-Filter**: erkennt Secrets/Keys per Pattern → erzwingt nur lokale Provider
- **Regelbasierte Klassifikation** in `task_type` (`simple_text`, `summarize`, `code_review`, `architecture`, `private_sensitive`)
- **Dynamischer, tier-basierter Selektor** (siehe unten) statt fixer Routing-Tabelle
- **Provider-Weiterleitung** an OpenAI-kompatible Backends (OpenRouter, OpenAI, Ollama)
- **Smart Retry + Fehlerklassifikation**: transiente Fehler (5xx/429/Netzwerk) → gleiches Modell mit jittered Backoff; 401/402/403 → nächstes Modell; sonstige 4xx → Abbruch ohne Fallback
- **Automatischer Fallback** entlang der gültigen Kandidatenkette
- **Exact-Match-Cache** (SQLite, kein Embedding): identische Requests an dasselbe Modell kosten $0
- **Per-Request-Overrides** via `x-llmux-*`-Header (Modell erzwingen, Cache/Fallback aus, Kostendeckel)
- **Streaming-Passthrough** (`stream: true`)
- **SQLite-Logging** jedes Requests (Modell, Tier, Tokens, Kosten, Budgetdruck, `degraded`, Fallback, `attempts`, `attempt_trail`, `cache_hit`, `stop_reason`, Session, Fehler)

Noch **nicht** dabei (bewusst): semantischer Cache, LLM-basierte Klassifikation,
nativer Anthropic-Adapter (läuft aktuell über OpenRouter), Dashboard, Multi-User.

## Agentisches Arbeiten

llmux ist für Agent-Loops mit Tool-Calling ausgelegt:

- **Tool-aware Routing**: Requests mit `tools`/`tool_choice`/`functions` oder `tool`-Rollen
  in der History werden nur an Modelle mit `supports_tools: true` geroutet. Das
  vollständige OpenAI-Tool-Schema wird unverändert durchgereicht.
- **Kontextgröße zählt**: die Token-Schätzung berücksichtigt die wachsende
  Message-History **und** die Tool-Definitionen; Modelle, deren Kontextfenster nicht
  reicht, fallen automatisch raus.
- **Session-Stickiness**: mit Header `x-llmux-session: <id>` bleibt ein Loop auf
  demselben Modell (Tool-Call-/Tool-Result-Konsistenz), solange das Budget es zulässt.

## Dynamische Token-/Kosten-Optimierung

Statt fixer „task → Modell"-Zuordnung wählt der Selektor pro Request dynamisch:

1. **Qualitäts-Floor**: jede Aufgabe hat ein `min_tier` (1 = billig/lokal … 5 = Top).
2. **Harte Filter**: Tool-Fähigkeit, lokaler Provider (Privacy), Kontextfenster,
   Provider verfügbar (aktiviert + Key gesetzt).
3. **Budgetdruck**: aus der realen Kostensumme (Tag/Monat) wird die Auslastung
   berechnet. Über konfigurierbare Schwellen (`pressure_downgrade`) sinkt die
   erlaubte **Tier-Obergrenze** — also *graceful downgrade* statt hartem Abbruch.
4. **Cheapest-viable**: aus den gültigen Kandidaten wird das **günstigste** gewählt
   (geschätzte Kosten = Input-Tokens + erwarteter Output über `expected_output_ratio`).
5. Erst wenn selbst das billigste Modell das **Restbudget** sprengt → `402`.

Beispiel: bei niedriger Auslastung geht `architecture` an ein Tier-4-Modell; ab
50 % Tagesbudget wird dieselbe Aufgabe auf ein günstigeres Tier heruntergestuft
(im Log als `degraded = 1` markiert).

## Architektur

Ein Binary mit Modulen (entspricht den geplanten Crates, später trennbar):

| Modul          | Aufgabe                                        |
|----------------|------------------------------------------------|
| `api`          | HTTP-Layer, Request-Pipeline                            |
| `classifier`   | Prompt → `task_type` + Tool-Use-Erkennung               |
| `privacy`      | Erkennung sensibler Inhalte                             |
| `router`       | dynamischer Selektor (Tier/Tools/Budget) + Sessions     |
| `providers`    | Weiterleitung an OpenAI-kompatible Provider             |
| `cost`         | Token-Schätzung (Messages + Tools)                      |
| `cache`        | Exact-Match-Cache-Key + Normalisierung                  |
| `logging`      | SQLite-Persistenz, Budget-Summen, Antwort-Cache         |
| `config`       | YAML-Konfiguration (Modell-Katalog, Task-Regeln)        |

## Starten

```bash
# Provider-Keys setzen (nur die, die du nutzt)
export OPENROUTER_API_KEY=sk-or-...
export OPENAI_API_KEY=sk-...

cargo run
# -> llmux läuft auf http://0.0.0.0:3456
```

Umgebungsvariablen:

- `LLMUX_CONFIG` – Pfad zur Config (Default `config/llmux.yaml`)
- `LLMUX_DB` – Pfad zur SQLite-DB (Default `data/llmux.sqlite`)
- `RUST_LOG` – z.B. `llmux=debug`

## In Tools eintragen

```
Base URL: http://localhost:3456/v1
API Key:  <auth.llmux_key aus der Config>
Model:    egal — llmux überschreibt das Modell anhand der Klassifikation
```

Optionale Header:

- `x-llmux-tool: aider` — identifiziert das Tool im Log
- `x-llmux-session: <id>` — hält einen Agent-Loop auf demselben Modell (Stickiness)
- `x-llmux-model: <model|provider/model>` — erzwingt ein Katalog-Modell (umgeht die Auswahl)
- `x-llmux-no-cache: true` — Cache für diesen Request überspringen
- `x-llmux-no-fallback: true` — nur das primäre Modell versuchen
- `x-llmux-max-cost: 0.05` — Request ablehnen (`402`), wenn die Schätzkosten den Wert übersteigen

Antwort-Header `x-llmux-cache: hit` markiert eine Antwort aus dem Cache.

## Konfiguration

Siehe `config/llmux.yaml`. Kernstücke:

- `models` — Katalog mit `tier`, `context`, `supports_tools`, Preisen (USD/1 Mio Tokens)
- `classification` — pro `task_type`: `min_tier`, optional `require_tools` / `local_only` / `expected_output_ratio`
- `budgets` — `daily_max_usd`, `monthly_max_usd` und `pressure_downgrade` (Tier-Drosselung)
- `retry` — `max_retries`, `backoff_initial_ms`, `backoff_max_ms`
- `cache` — `enabled`, `ttl_seconds`, `max_conversation_messages` (History-Guard)
- `privacy.block_cloud_patterns` — Trigger für lokales Routing
- `providers` — Backends inkl. `local: true` für lokale Provider (Ollama)

## Logs auswerten

```bash
sqlite3 data/llmux.sqlite \
  "SELECT task_type, model, tier, COUNT(*),
          ROUND(SUM(real_cost_usd),4) AS cost, SUM(degraded) AS downgrades
   FROM requests GROUP BY task_type, model, tier;"
```

## Lizenz

Apache License 2.0 — siehe [LICENSE](LICENSE).
