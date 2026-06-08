//! SQLite-Logging jedes Requests + Budget-Abfragen (Tages-/Monatssumme).

use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;

pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(Debug, Default)]
pub struct RequestLog {
    pub tool: String,
    pub session: Option<String>,
    pub task_type: String,
    pub model: String,
    pub provider: String,
    pub tier: u8,
    pub used_fallback: bool,
    pub degraded: bool,
    pub budget_pressure: f64,
    pub estimated_tokens: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub estimated_cost_usd: f64,
    pub real_cost_usd: f64,
    pub latency_ms: u64,
    pub status: u16,
    pub cache_hit: bool,
    pub attempts: u32,
    pub attempt_trail: Option<String>,
    pub stop_reason: Option<String>,
    pub error: Option<String>,
    /// Modell wurde per Override (`x-llmux-model`/Alias) erzwungen — die einzige
    /// Policy-Dimension, die sich nicht aus den übrigen Feldern ableiten lässt (#28).
    pub forced: bool,
    /// Projekt-Scope aus `x-llmux-project` (None = kein Scope). (#33)
    pub project: Option<String>,
    /// Der Request erwartete Tool-Calling (Tools/Tool-History im Request). (#29)
    pub tools_expected: bool,
    /// Die Antwort enthielt tatsächlich einen Tool-Call. Zusammen mit
    /// `tools_expected` ergibt das das Qualitätssignal „Tool erwartet, keiner
    /// gekommen" — ein beobachtbarer Proxy, keine semantische Bewertung. (#29)
    pub tool_call_present: bool,
}

impl Store {
    pub fn open(path: &str) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS requests (
                id                 INTEGER PRIMARY KEY AUTOINCREMENT,
                ts                 TEXT NOT NULL,
                tool               TEXT,
                session            TEXT,
                task_type          TEXT,
                model              TEXT,
                provider           TEXT,
                tier               INTEGER,
                used_fallback      INTEGER,
                degraded           INTEGER,
                budget_pressure    REAL,
                estimated_tokens   INTEGER,
                prompt_tokens      INTEGER,
                completion_tokens  INTEGER,
                estimated_cost_usd REAL,
                real_cost_usd      REAL,
                latency_ms         INTEGER,
                status             INTEGER,
                cache_hit          INTEGER,
                attempts           INTEGER,
                attempt_trail      TEXT,
                stop_reason        TEXT,
                error              TEXT,
                forced             INTEGER,
                policy_result      TEXT,
                project            TEXT,
                tools_expected     INTEGER,
                tool_call_present  INTEGER
            );

            CREATE TABLE IF NOT EXISTS cache (
                key        TEXT PRIMARY KEY,
                model      TEXT,
                response   TEXT NOT NULL,
                created_ts TEXT NOT NULL,
                expires_ts TEXT NOT NULL,
                hits       INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS semantic_cache (
                id         INTEGER PRIMARY KEY AUTOINCREMENT,
                scope      TEXT NOT NULL,
                embedding  BLOB NOT NULL,
                response   TEXT NOT NULL,
                created_ts TEXT NOT NULL,
                expires_ts TEXT NOT NULL,
                hits       INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_semantic_scope ON semantic_cache(scope);
            "#,
        )?;

        // Idempotente Migration für bestehende DBs (CREATE TABLE IF NOT EXISTS
        // ergänzt keine Spalten an einer schon vorhandenen Tabelle). Fehler bei
        // bereits existierender Spalte werden bewusst ignoriert (#28).
        for stmt in [
            "ALTER TABLE requests ADD COLUMN forced INTEGER",
            "ALTER TABLE requests ADD COLUMN policy_result TEXT",
            "ALTER TABLE requests ADD COLUMN project TEXT",
            "ALTER TABLE requests ADD COLUMN tools_expected INTEGER",
            "ALTER TABLE requests ADD COLUMN tool_call_present INTEGER",
        ] {
            let _ = conn.execute(stmt, []);
        }

        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    pub fn insert(&self, log: &RequestLog) -> anyhow::Result<()> {
        let ts = chrono::Utc::now().to_rfc3339();
        // Policy-Ergebnis einmalig beim Logging festschreiben, damit jede Zeile
        // ihre Routing-Entscheidung in Policy-Begriffen erklärt (#28).
        let policy_result = route_result(
            log.status as i64,
            log.cache_hit,
            log.used_fallback,
            log.degraded,
            log.forced,
        );
        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"INSERT INTO requests
               (ts, tool, session, task_type, model, provider, tier, used_fallback,
                degraded, budget_pressure, estimated_tokens, prompt_tokens,
                completion_tokens, estimated_cost_usd, real_cost_usd, latency_ms,
                status, cache_hit, attempts, attempt_trail, stop_reason, error,
                forced, policy_result, project, tools_expected, tool_call_present)
               VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27)"#,
            rusqlite::params![
                ts,
                log.tool,
                log.session,
                log.task_type,
                log.model,
                log.provider,
                log.tier as i64,
                log.used_fallback as i64,
                log.degraded as i64,
                log.budget_pressure,
                log.estimated_tokens as i64,
                log.prompt_tokens as i64,
                log.completion_tokens as i64,
                log.estimated_cost_usd,
                log.real_cost_usd,
                log.latency_ms as i64,
                log.status as i64,
                log.cache_hit as i64,
                log.attempts as i64,
                log.attempt_trail,
                log.stop_reason,
                log.error,
                log.forced as i64,
                policy_result,
                log.project,
                log.tools_expected as i64,
                log.tool_call_present as i64,
            ],
        )?;
        Ok(())
    }

    /// Gültiger Cache-Eintrag (noch nicht abgelaufen). Erhöht den Trefferzähler.
    pub fn cache_lookup(&self, key: &str) -> Option<String> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        let res: Option<String> = conn
            .query_row(
                "SELECT response FROM cache WHERE key = ?1 AND expires_ts > ?2",
                rusqlite::params![key, now],
                |row| row.get(0),
            )
            .optional()
            .unwrap_or(None);
        if res.is_some() {
            let _ = conn.execute("UPDATE cache SET hits = hits + 1 WHERE key = ?1", [key]);
        }
        res
    }

    pub fn cache_store(&self, key: &str, model: &str, response: &str, ttl_seconds: u64) {
        let now = chrono::Utc::now();
        let created = now.to_rfc3339();
        let expires = (now + chrono::Duration::seconds(ttl_seconds as i64)).to_rfc3339();
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            r#"INSERT OR REPLACE INTO cache (key, model, response, created_ts, expires_ts, hits)
               VALUES (?1, ?2, ?3, ?4, ?5, 0)"#,
            rusqlite::params![key, model, response, created, expires],
        );
    }

    /// Semantic-Cache (#14): legt ein Embedding + Antwort unter einem Modell-`scope` ab.
    pub fn semantic_cache_store(&self, scope: &str, embedding: &[f32], response: &str, ttl_seconds: u64) {
        let now = chrono::Utc::now();
        let created = now.to_rfc3339();
        let expires = (now + chrono::Duration::seconds(ttl_seconds as i64)).to_rfc3339();
        let blob = crate::cache::embedding_to_bytes(embedding);
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            r#"INSERT INTO semantic_cache (scope, embedding, response, created_ts, expires_ts, hits)
               VALUES (?1, ?2, ?3, ?4, ?5, 0)"#,
            rusqlite::params![scope, blob, response, created, expires],
        );
    }

    /// Semantic-Cache-Lookup (#14): durchsucht die nicht abgelaufenen Einträge des
    /// `scope` per Brute-Force-Cosine und liefert die Antwort des ähnlichsten Eintrags,
    /// sofern dessen Ähnlichkeit `>= threshold` ist. Erhöht dessen Trefferzähler.
    pub fn semantic_cache_lookup(&self, scope: &str, query: &[f32], threshold: f32) -> Option<String> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, embedding, response FROM semantic_cache WHERE scope = ?1 AND expires_ts > ?2")
            .ok()?;
        let rows = stmt
            .query_map(rusqlite::params![scope, now], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_, String>(2)?))
            })
            .ok()?;

        let mut best: Option<(i64, f32, String)> = None;
        for row in rows.flatten() {
            let (id, blob, response) = row;
            let emb = crate::cache::embedding_from_bytes(&blob);
            let sim = crate::cache::cosine_similarity(query, &emb);
            if best.as_ref().map(|b| sim > b.1).unwrap_or(true) {
                best = Some((id, sim, response));
            }
        }

        match best {
            Some((id, sim, response)) if sim >= threshold => {
                let _ = conn.execute("UPDATE semantic_cache SET hits = hits + 1 WHERE id = ?1", [id]);
                Some(response)
            }
            _ => None,
        }
    }

    /// Löscht abgelaufene Cache-Einträge (Exact + Semantic). Gibt die Anzahl entfernter Zeilen zurück.
    pub fn evict_expired_cache(&self) -> usize {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        let exact = conn
            .execute("DELETE FROM cache WHERE expires_ts <= ?1", [now.clone()])
            .unwrap_or(0);
        let semantic = conn
            .execute("DELETE FROM semantic_cache WHERE expires_ts <= ?1", [now])
            .unwrap_or(0);
        exact + semantic
    }

    /// Hält die Cache-Tabelle unter `max` Zeilen, indem die ältesten (nach
    /// created_ts) entfernt werden. Gibt die Anzahl entfernter Zeilen zurück.
    pub fn enforce_cache_cap(&self, max: usize) -> usize {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"DELETE FROM cache WHERE key IN (
                   SELECT key FROM cache ORDER BY created_ts DESC LIMIT -1 OFFSET ?1
               )"#,
            [max as i64],
        )
        .unwrap_or(0)
    }

    /// Reale Kostensumme seit Tagesbeginn (UTC).
    pub fn spent_today(&self) -> f64 {
        let since = chrono::Utc::now()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .to_rfc3339();
        self.spent_since(&since)
    }

    /// Reale Kostensumme seit Monatsbeginn (UTC).
    pub fn spent_this_month(&self) -> f64 {
        let now = chrono::Utc::now().date_naive();
        let first = now.with_day(1).unwrap();
        let since = first.and_hms_opt(0, 0, 0).unwrap().and_utc().to_rfc3339();
        self.spent_since(&since)
    }

    fn spent_since(&self, since_rfc3339: &str) -> f64 {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COALESCE(SUM(real_cost_usd), 0.0) FROM requests WHERE ts >= ?1",
            [since_rfc3339],
            |row| row.get(0),
        )
        .unwrap_or(0.0)
    }

    // ----- Stats-API (#18): aus den Request-Logs abgeleitete Kennzahlen -----

    /// DB-abgeleitete Overview-Kennzahlen (ohne budget-/config-abhängige Werte —
    /// die ergänzt der Handler). Felder: `status`, `total_requests`,
    /// `requests_per_minute` (letzte 5 min), `cache_hit_rate`, `error_count`,
    /// `p95_latency_ms`.
    pub fn stats_overview(&self) -> Value {
        let conn = self.conn.lock().unwrap();
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM requests", [], |r| r.get(0))
            .unwrap_or(0);
        let cache_hits: i64 = conn
            .query_row("SELECT COUNT(*) FROM requests WHERE cache_hit=1", [], |r| r.get(0))
            .unwrap_or(0);
        let errors: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM requests WHERE status>=400 OR status=0",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let since = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
        let recent: i64 = conn
            .query_row("SELECT COUNT(*) FROM requests WHERE ts>=?1", [&since], |r| r.get(0))
            .unwrap_or(0);
        let lat = sorted_latencies(&conn, "status BETWEEN 200 AND 299 AND latency_ms>0");

        json!({
            "status": "online",
            "total_requests": total,
            "requests_per_minute": (recent as f64) / 5.0,
            "cache_hit_rate": if total > 0 { cache_hits as f64 / total as f64 } else { 0.0 },
            "error_count": errors,
            "p95_latency_ms": percentile(&lat, 0.95),
        })
    }

    /// Jüngste Route-Entscheidungen (neueste zuerst) für den Live-Feed und den
    /// Request-Inspector. Jede Zeile trägt ein abgeleitetes `result`-Label.
    pub fn recent_requests(&self, limit: usize) -> Vec<Value> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT id, ts, tool, session, task_type, model, provider, tier, used_fallback, \
             degraded, estimated_cost_usd, real_cost_usd, prompt_tokens, completion_tokens, \
             latency_ms, status, cache_hit, attempts, attempt_trail, stop_reason, error, \
             forced, policy_result, project \
             FROM requests ORDER BY id DESC LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map([limit as i64], |r| {
            let used_fallback: i64 = r.get(8)?;
            let degraded: i64 = r.get(9)?;
            let status: i64 = r.get(15)?;
            let cache_hit: i64 = r.get(16)?;
            let forced: Option<i64> = r.get(21)?;
            let forced = forced.unwrap_or(0) != 0;
            // Gespeichertes Policy-Label bevorzugen; für Altzeilen (vor #28) ableiten.
            let result = r
                .get::<_, Option<String>>(22)?
                .unwrap_or_else(|| {
                    route_result(status, cache_hit != 0, used_fallback != 0, degraded != 0, forced).to_string()
                });
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "time": r.get::<_, String>(1)?,
                "tool": r.get::<_, Option<String>>(2)?,
                "session": r.get::<_, Option<String>>(3)?,
                "project": r.get::<_, Option<String>>(23)?,
                "task_type": r.get::<_, Option<String>>(4)?,
                "model": r.get::<_, Option<String>>(5)?,
                "provider": r.get::<_, Option<String>>(6)?,
                "tier": r.get::<_, i64>(7)?,
                "used_fallback": used_fallback != 0,
                "degraded": degraded != 0,
                "estimated_cost_usd": r.get::<_, f64>(10)?,
                "real_cost_usd": r.get::<_, f64>(11)?,
                "prompt_tokens": r.get::<_, i64>(12)?,
                "completion_tokens": r.get::<_, i64>(13)?,
                "latency_ms": r.get::<_, i64>(14)?,
                "status": status,
                "cache_hit": cache_hit != 0,
                "forced": forced,
                "attempts": r.get::<_, i64>(17)?,
                "attempt_trail": r.get::<_, Option<String>>(18)?,
                "stop_reason": r.get::<_, Option<String>>(19)?,
                "error": r.get::<_, Option<String>>(20)?,
                "result": result,
            }))
        });
        match rows {
            Ok(it) => it.flatten().collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Aggregate je (Provider, Modell): Requests, reale Kosten, p50/p95-Latenz,
    /// Erfolgs-/Fehler-/Fallback-/Cache-Rate, Durchschnitts-Tier.
    pub fn model_stats(&self) -> Vec<Value> {
        let conn = self.conn.lock().unwrap();

        // Latenzen je Gruppe für Perzentile sammeln (separate Abfrage, dann in Rust).
        let mut lat_by_group: HashMap<(String, String), Vec<i64>> = HashMap::new();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT provider, model, latency_ms FROM requests \
             WHERE status BETWEEN 200 AND 299 AND latency_ms>0 AND model IS NOT NULL AND model<>''",
        ) {
            if let Ok(rows) = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            }) {
                for (p, m, l) in rows.flatten() {
                    lat_by_group.entry((p, m)).or_default().push(l);
                }
            }
        }
        for v in lat_by_group.values_mut() {
            v.sort_unstable();
        }

        let mut stmt = match conn.prepare(
            "SELECT provider, model, COUNT(*), \
             COALESCE(SUM(real_cost_usd),0.0), \
             SUM(CASE WHEN status BETWEEN 200 AND 299 THEN 1 ELSE 0 END), \
             SUM(CASE WHEN status>=400 OR status=0 THEN 1 ELSE 0 END), \
             SUM(used_fallback), SUM(cache_hit), AVG(tier) \
             FROM requests WHERE model IS NOT NULL AND model<>'' \
             GROUP BY provider, model ORDER BY COUNT(*) DESC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map([], |r| {
            let provider: String = r.get(0)?;
            let model: String = r.get(1)?;
            let count: i64 = r.get(2)?;
            let cost: f64 = r.get(3)?;
            let success: i64 = r.get(4)?;
            let errors: i64 = r.get(5)?;
            let fallback: i64 = r.get(6)?;
            let cache: i64 = r.get(7)?;
            let avg_tier: f64 = r.get(8)?;
            let lat = lat_by_group.get(&(provider.clone(), model.clone()));
            let (p50, p95) = match lat {
                Some(v) => (percentile(v, 0.50), percentile(v, 0.95)),
                None => (0, 0),
            };
            let n = count as f64;
            Ok(json!({
                "provider": provider,
                "model": model,
                "requests": count,
                "real_cost_usd": cost,
                "avg_tier": avg_tier,
                "p50_latency_ms": p50,
                "p95_latency_ms": p95,
                "success_rate": if n > 0.0 { success as f64 / n } else { 0.0 },
                "error_rate": if n > 0.0 { errors as f64 / n } else { 0.0 },
                "fallback_rate": if n > 0.0 { fallback as f64 / n } else { 0.0 },
                "cache_hit_rate": if n > 0.0 { cache as f64 / n } else { 0.0 },
            }))
        });
        match rows {
            Ok(it) => it.flatten().collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Policy-Zähler (unabhängige Zähler, keine Partition) plus Top-Ablehngründe.
    /// `local_only` = Requests, die als `private_sensitive` klassifiziert wurden.
    /// `forced` = per Override erzwungene Modelle (inkl. der durch die harten
    /// Constraints abgelehnten, separat als `forced_rejected`); siehe #25/#28.
    pub fn policy_stats(&self) -> Value {
        let conn = self.conn.lock().unwrap();
        let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap_or(0) };

        // `allowed` grenzt erzwungene Overrides aus, damit sich die primären
        // Ergebnis-Kategorien nicht überlappen.
        let allowed = count(
            "SELECT COUNT(*) FROM requests WHERE status BETWEEN 200 AND 299 \
             AND cache_hit=0 AND used_fallback=0 AND degraded=0 AND COALESCE(forced,0)=0",
        );
        let rejected = count("SELECT COUNT(*) FROM requests WHERE status>=400 OR status=0");
        let degraded = count("SELECT COUNT(*) FROM requests WHERE degraded=1");
        let fallback = count("SELECT COUNT(*) FROM requests WHERE used_fallback=1");
        let cached = count("SELECT COUNT(*) FROM requests WHERE cache_hit=1");
        let forced = count("SELECT COUNT(*) FROM requests WHERE forced=1");
        let forced_rejected =
            count("SELECT COUNT(*) FROM requests WHERE forced=1 AND (status>=400 OR status=0)");
        let local_only = count("SELECT COUNT(*) FROM requests WHERE task_type='private_sensitive'");

        let mut reasons: Vec<Value> = Vec::new();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT COALESCE(error, 'status ' || status) AS reason, COUNT(*) c \
             FROM requests WHERE status>=400 OR status=0 \
             GROUP BY reason ORDER BY c DESC LIMIT 5",
        ) {
            if let Ok(rows) = stmt.query_map([], |r| {
                Ok(json!({ "reason": r.get::<_, String>(0)?, "count": r.get::<_, i64>(1)? }))
            }) {
                reasons = rows.flatten().collect();
            }
        }

        json!({
            "allowed": allowed,
            "rejected": rejected,
            "degraded": degraded,
            "fallback": fallback,
            "cached": cached,
            "forced": forced,
            "forced_rejected": forced_rejected,
            "local_only": local_only,
            "top_rejection_reasons": reasons,
        })
    }

    /// Aggregate je Projekt-Scope (#33): Requests, reale Kosten, Ablehnungen,
    /// erzwungene Overrides und Privacy-(`local_only`)-Anteil. Requests ohne
    /// `x-llmux-project` laufen unter `(none)`.
    pub fn project_stats(&self) -> Vec<Value> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT COALESCE(project,'(none)') AS p, COUNT(*), \
             COALESCE(SUM(real_cost_usd),0.0), \
             SUM(CASE WHEN status>=400 OR status=0 THEN 1 ELSE 0 END), \
             SUM(COALESCE(forced,0)), \
             SUM(CASE WHEN task_type='private_sensitive' THEN 1 ELSE 0 END) \
             FROM requests GROUP BY p ORDER BY COUNT(*) DESC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map([], |r| {
            Ok(json!({
                "project": r.get::<_, String>(0)?,
                "requests": r.get::<_, i64>(1)?,
                "real_cost_usd": r.get::<_, f64>(2)?,
                "rejected": r.get::<_, i64>(3)?,
                "forced": r.get::<_, i64>(4)?,
                "local_only": r.get::<_, i64>(5)?,
            }))
        });
        match rows {
            Ok(it) => it.flatten().collect(),
            Err(_) => Vec::new(),
        }
    }

    /// p50-Latenz (ms) je (Provider, Modell) aus erfolgreichen Provider-Antworten
    /// (Cache-Treffer ausgenommen, da ~instant). Für das latenz-orientierte Routing
    /// im `interactive`-Profil (#30). Modelle ohne Historie fehlen in der Map.
    pub fn model_latency_p50(&self) -> HashMap<(String, String), u64> {
        let conn = self.conn.lock().unwrap();
        let mut by_group: HashMap<(String, String), Vec<i64>> = HashMap::new();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT provider, model, latency_ms FROM requests \
             WHERE status BETWEEN 200 AND 299 AND latency_ms>0 AND cache_hit=0 \
             AND model IS NOT NULL AND model<>''",
        ) {
            if let Ok(rows) = stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
            }) {
                for (p, m, l) in rows.flatten() {
                    by_group.entry((p, m)).or_default().push(l);
                }
            }
        }
        by_group
            .into_iter()
            .map(|(k, mut v)| {
                v.sort_unstable();
                (k, percentile(&v, 0.5).max(0) as u64)
            })
            .collect()
    }

    /// Latenz-Aggregate (#30): p50/p95 je Provider und task_type sowie Cache- vs.
    /// Provider-Latenz. Per-Modell-Perzentile liefert bereits `model_stats`.
    pub fn latency_stats(&self) -> Value {
        let conn = self.conn.lock().unwrap();

        // Gruppierte p50/p95 über erfolgreiche Provider-Antworten (ohne Cache).
        let grouped = |col: &str, label: &str| -> Vec<Value> {
            let sql = format!(
                "SELECT {col}, latency_ms FROM requests \
                 WHERE status BETWEEN 200 AND 299 AND latency_ms>0 AND cache_hit=0 \
                 AND {col} IS NOT NULL AND {col}<>''"
            );
            let mut map: HashMap<String, Vec<i64>> = HashMap::new();
            if let Ok(mut stmt) = conn.prepare(&sql) {
                if let Ok(rows) =
                    stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                {
                    for (k, l) in rows.flatten() {
                        map.entry(k).or_default().push(l);
                    }
                }
            }
            let mut out: Vec<Value> = map
                .into_iter()
                .map(|(k, mut v)| {
                    v.sort_unstable();
                    json!({ label: k, "p50_ms": percentile(&v, 0.5), "p95_ms": percentile(&v, 0.95), "samples": v.len() })
                })
                .collect();
            out.sort_by(|a, b| b["samples"].as_i64().cmp(&a["samples"].as_i64()));
            out
        };

        let cache_p50 = percentile(
            &sorted_latencies(&conn, "status BETWEEN 200 AND 299 AND latency_ms>0 AND cache_hit=1"),
            0.5,
        );
        let provider_p50 = percentile(
            &sorted_latencies(&conn, "status BETWEEN 200 AND 299 AND latency_ms>0 AND cache_hit=0"),
            0.5,
        );

        json!({
            "by_provider": grouped("provider", "provider"),
            "by_task": grouped("task_type", "task_type"),
            "cache_hit_p50_ms": cache_p50,
            "provider_p50_ms": provider_p50,
        })
    }

    /// Post-Response-Qualitätssignale (#29): beobachtbare Proxies, **keine**
    /// semantische Bewertung. Liefert je (Modell, task_type) Erfolgs-/Fehler-/
    /// Fallback-/Cache-Rate, Ø-Versuche und „Tool erwartet, keiner gekommen",
    /// dazu die stop_reason-Verteilung, die Gesamtzahl fehlender Tool-Calls und
    /// die häufigsten Fehlercluster.
    pub fn quality_stats(&self) -> Value {
        let conn = self.conn.lock().unwrap();

        let mut by_model_task: Vec<Value> = Vec::new();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT model, task_type, COUNT(*), \
             SUM(CASE WHEN status BETWEEN 200 AND 299 THEN 1 ELSE 0 END), \
             SUM(CASE WHEN status>=400 OR status=0 THEN 1 ELSE 0 END), \
             SUM(used_fallback), SUM(cache_hit), AVG(attempts), \
             SUM(CASE WHEN tools_expected=1 AND COALESCE(tool_call_present,0)=0 \
                      AND status BETWEEN 200 AND 299 THEN 1 ELSE 0 END) \
             FROM requests WHERE model IS NOT NULL AND model<>'' \
             GROUP BY model, task_type ORDER BY COUNT(*) DESC",
        ) {
            if let Ok(rows) = stmt.query_map([], |r| {
                let count: i64 = r.get(2)?;
                let n = count as f64;
                let rate = |v: i64| if n > 0.0 { v as f64 / n } else { 0.0 };
                Ok(json!({
                    "model": r.get::<_, String>(0)?,
                    "task_type": r.get::<_, Option<String>>(1)?,
                    "requests": count,
                    "success_rate": rate(r.get::<_, i64>(3)?),
                    "error_rate": rate(r.get::<_, i64>(4)?),
                    "fallback_rate": rate(r.get::<_, i64>(5)?),
                    "cache_hit_rate": rate(r.get::<_, i64>(6)?),
                    "avg_attempts": r.get::<_, Option<f64>>(7)?.unwrap_or(0.0),
                    "tool_call_missing": r.get::<_, i64>(8)?,
                }))
            }) {
                by_model_task = rows.flatten().collect();
            }
        }

        let mut stop_reasons: Vec<Value> = Vec::new();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT COALESCE(stop_reason,'(none)'), COUNT(*) FROM requests \
             WHERE status BETWEEN 200 AND 299 GROUP BY 1 ORDER BY 2 DESC",
        ) {
            if let Ok(rows) = stmt.query_map([], |r| {
                Ok(json!({ "stop_reason": r.get::<_, String>(0)?, "count": r.get::<_, i64>(1)? }))
            }) {
                stop_reasons = rows.flatten().collect();
            }
        }

        let tool_call_missing: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM requests WHERE tools_expected=1 \
                 AND COALESCE(tool_call_present,0)=0 AND status BETWEEN 200 AND 299",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);

        let mut error_clusters: Vec<Value> = Vec::new();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT COALESCE(error,'status '||status) AS reason, COUNT(*) c \
             FROM requests WHERE status>=400 OR status=0 GROUP BY reason ORDER BY c DESC LIMIT 10",
        ) {
            if let Ok(rows) = stmt.query_map([], |r| {
                Ok(json!({ "reason": r.get::<_, String>(0)?, "count": r.get::<_, i64>(1)? }))
            }) {
                error_clusters = rows.flatten().collect();
            }
        }

        json!({
            "note": "operational quality signals (observable proxies), not semantic evaluation",
            "by_model_task": by_model_task,
            "stop_reasons": stop_reasons,
            "tool_call_missing": tool_call_missing,
            "error_clusters": error_clusters,
        })
    }
}

/// Einzelwort-Policy-Ergebnis einer Route-Entscheidung. Priorität: Fehler vor
/// erzwungenem Override vor Cache vor Fallback vor Downgrade vor regulär erlaubt —
/// so bleibt ein gehonorierter Override sichtbar, ein abgelehnter Request wird
/// aber stets als `rejected` geführt (#28).
fn route_result(status: i64, cache_hit: bool, fallback: bool, degraded: bool, forced: bool) -> &'static str {
    if !(200..300).contains(&status) {
        "rejected"
    } else if forced {
        "forced"
    } else if cache_hit {
        "cached"
    } else if fallback {
        "fallback"
    } else if degraded {
        "degraded"
    } else {
        "allowed"
    }
}

/// Aufsteigend sortierte Latenzen für Zeilen, die `where_clause` erfüllen.
fn sorted_latencies(conn: &Connection, where_clause: &str) -> Vec<i64> {
    let sql = format!("SELECT latency_ms FROM requests WHERE {where_clause} ORDER BY latency_ms");
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Vec::new();
    };
    let out = match stmt.query_map([], |r| r.get::<_, i64>(0)) {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => Vec::new(),
    };
    out
}

/// Perzentil (nearest-rank) aus einer aufsteigend sortierten Liste. 0 wenn leer.
fn percentile(sorted: &[i64], p: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = (((sorted.len() - 1) as f64) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

use chrono::Datelike;

#[cfg(test)]
impl Store {
    /// Letzte Log-Zeile als (status, prompt_tokens, completion_tokens, real_cost_usd, cache_hit).
    pub fn last_request(&self) -> Option<(i64, i64, i64, f64, i64)> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT status, prompt_tokens, completion_tokens, real_cost_usd, cache_hit \
             FROM requests ORDER BY id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()
        .unwrap_or(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open(":memory:").unwrap()
    }

    /// Schreibt einen Cache-Eintrag mit explizitem Ablaufzeitpunkt (für Tests).
    fn insert_cache(store: &Store, key: &str, expires_ts: &str) {
        let conn = store.conn.lock().unwrap();
        conn.execute(
            r#"INSERT INTO cache (key, model, response, created_ts, expires_ts, hits)
               VALUES (?1, 'm', '{}', ?2, ?3, 0)"#,
            rusqlite::params![key, expires_ts, expires_ts],
        )
        .unwrap();
    }

    fn cache_count(store: &Store) -> i64 {
        let conn = store.conn.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM cache", [], |r| r.get(0)).unwrap()
    }

    // #14: Ein semantisch ähnliches (anders gewichtetes) Query-Embedding trifft über
    // der Schwelle; ein unähnliches (orthogonales) verfehlt sie.
    #[test]
    fn semantic_cache_hits_above_threshold_misses_below() {
        let s = store();
        let future = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        // Eintrag direkt ablegen (mit künftigem Ablauf).
        let blob = crate::cache::embedding_to_bytes(&[1.0, 0.0, 0.0]);
        {
            let conn = s.conn.lock().unwrap();
            conn.execute(
                r#"INSERT INTO semantic_cache (scope, embedding, response, created_ts, expires_ts, hits)
                   VALUES ('p/m', ?1, '{"cached":true}', ?2, ?2, 0)"#,
                rusqlite::params![blob, future],
            )
            .unwrap();
        }
        // Fast gleiche Richtung -> hohe Cosine -> Treffer.
        assert_eq!(
            s.semantic_cache_lookup("p/m", &[0.99, 0.1, 0.0], 0.85).as_deref(),
            Some(r#"{"cached":true}"#)
        );
        // Orthogonal -> Cosine 0 -> Miss.
        assert!(s.semantic_cache_lookup("p/m", &[0.0, 1.0, 0.0], 0.85).is_none());
        // Anderer Scope -> Miss.
        assert!(s.semantic_cache_lookup("p/other", &[1.0, 0.0, 0.0], 0.85).is_none());
    }

    // #14: store -> lookup über die öffentliche API, inklusive TTL-Eviction.
    #[test]
    fn semantic_cache_store_and_evict() {
        let s = store();
        s.semantic_cache_store("p/m", &[0.0, 1.0], r#"{"a":1}"#, 3600);
        assert_eq!(
            s.semantic_cache_lookup("p/m", &[0.0, 1.0], 0.9).as_deref(),
            Some(r#"{"a":1}"#)
        );
        // Abgelaufenen Eintrag einfügen und Eviction prüfen.
        {
            let conn = s.conn.lock().unwrap();
            conn.execute(
                r#"INSERT INTO semantic_cache (scope, embedding, response, created_ts, expires_ts, hits)
                   VALUES ('p/m', ?1, '{}', '2000-01-01T00:00:00+00:00', '2000-01-01T00:00:00+00:00', 0)"#,
                rusqlite::params![crate::cache::embedding_to_bytes(&[1.0, 0.0])],
            )
            .unwrap();
        }
        assert_eq!(s.evict_expired_cache(), 1); // nur die abgelaufene semantische Zeile
    }

    #[test]
    fn evict_expired_removes_only_stale_rows() {
        let s = store();
        let past = "2000-01-01T00:00:00+00:00";
        let future = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        insert_cache(&s, "stale", past);
        insert_cache(&s, "fresh", &future);

        let removed = s.evict_expired_cache();
        assert_eq!(removed, 1);
        assert_eq!(cache_count(&s), 1);
        // Der frische Eintrag ist noch auffindbar.
        assert!(s.cache_lookup("fresh").is_some());
    }

    /// #28: Jede Logzeile trägt ein Policy-Label, und die Policy-Zähler bilden
    /// normales Routing, Budget-Downgrade, erzwungenen Override und Ablehnung ab.
    #[test]
    fn policy_result_and_counts_reflect_routing_decisions() {
        let s = store();
        s.insert(&RequestLog { model: "m".into(), provider: "p".into(), status: 200, ..Default::default() }).unwrap();
        s.insert(&RequestLog { model: "m".into(), provider: "p".into(), status: 200, degraded: true, ..Default::default() }).unwrap();
        s.insert(&RequestLog { model: "m".into(), provider: "p".into(), status: 200, forced: true, ..Default::default() }).unwrap();
        // Erzwungener Override, von den harten Constraints abgelehnt (#25).
        s.insert(&RequestLog { status: 403, forced: true, error: Some("local-only".into()), ..Default::default() }).unwrap();

        // Gespeichertes Policy-Label je Zeile (neueste zuerst).
        let rows = s.recent_requests(10);
        let results: Vec<&str> = rows.iter().map(|r| r["result"].as_str().unwrap()).collect();
        assert_eq!(results, vec!["rejected", "forced", "degraded", "allowed"]);

        let p = s.policy_stats();
        assert_eq!(p["allowed"], 1);
        assert_eq!(p["degraded"], 1);
        assert_eq!(p["forced"], 2, "gehonorierter + abgelehnter Override");
        assert_eq!(p["forced_rejected"], 1);
        assert_eq!(p["rejected"], 1);
    }

    /// #30: `latency_stats` aggregiert p50/p95 je Provider/Task und trennt Cache-
    /// von Provider-Latenz (Cache-Treffer fließen nicht in die Provider-Gruppen).
    #[test]
    fn latency_stats_aggregate_by_provider_and_task() {
        let s = store();
        s.insert(&RequestLog { provider: "p1".into(), model: "m".into(), task_type: "simple_text".into(), status: 200, latency_ms: 100, ..Default::default() }).unwrap();
        s.insert(&RequestLog { provider: "p1".into(), model: "m".into(), task_type: "simple_text".into(), status: 200, latency_ms: 300, ..Default::default() }).unwrap();
        // Cache-Treffer (code_review) zählt nur in die Cache-Latenz, nicht in die Gruppen.
        s.insert(&RequestLog { provider: "p1".into(), model: "m".into(), task_type: "code_review".into(), status: 200, latency_ms: 50, cache_hit: true, ..Default::default() }).unwrap();

        let l = s.latency_stats();
        let p1 = l["by_provider"].as_array().unwrap().iter().find(|r| r["provider"] == "p1").unwrap();
        assert_eq!(p1["samples"], 2, "Cache-Treffer ist ausgeschlossen");
        assert!(p1["p50_ms"].as_i64().unwrap() >= 100);

        let tasks = l["by_task"].as_array().unwrap();
        assert!(tasks.iter().any(|r| r["task_type"] == "simple_text"));
        assert!(!tasks.iter().any(|r| r["task_type"] == "code_review"), "Cache-Zeile zählt nicht");

        assert_eq!(l["cache_hit_p50_ms"], 50);
        assert!(l["provider_p50_ms"].as_i64().unwrap() >= 100);
    }

    /// #29: `quality_stats` fasst Erfolgs-, Fallback- und Fehler-Ausgänge zusammen
    /// und erkennt „Tool erwartet, aber kein Tool-Call".
    #[test]
    fn quality_stats_summarize_outcomes() {
        let s = store();
        // Erfolg mit Tool-Call.
        s.insert(&RequestLog { model: "m".into(), task_type: "code_review".into(), status: 200, stop_reason: Some("tool_calls".into()), tools_expected: true, tool_call_present: true, ..Default::default() }).unwrap();
        // Erfolg, Tools erwartet, aber KEIN Tool-Call -> tool_call_missing.
        s.insert(&RequestLog { model: "m".into(), task_type: "code_review".into(), status: 200, stop_reason: Some("stop".into()), tools_expected: true, tool_call_present: false, ..Default::default() }).unwrap();
        // Fallback-Erfolg.
        s.insert(&RequestLog { model: "m".into(), task_type: "simple_text".into(), status: 200, used_fallback: true, stop_reason: Some("stop".into()), ..Default::default() }).unwrap();
        // Provider-Fehler.
        s.insert(&RequestLog { model: "m".into(), task_type: "simple_text".into(), status: 502, error: Some("upstream 502".into()), ..Default::default() }).unwrap();

        let q = s.quality_stats();
        assert_eq!(q["tool_call_missing"], 1);

        let bmt = q["by_model_task"].as_array().unwrap();
        let cr = bmt.iter().find(|r| r["task_type"] == "code_review").unwrap();
        assert_eq!(cr["requests"], 2);
        assert_eq!(cr["tool_call_missing"], 1);
        let st = bmt.iter().find(|r| r["task_type"] == "simple_text").unwrap();
        assert!((st["fallback_rate"].as_f64().unwrap() - 0.5).abs() < 1e-9);
        assert!((st["error_rate"].as_f64().unwrap() - 0.5).abs() < 1e-9);

        // stop_reasons zählt nur 2xx: stop=2, tool_calls=1.
        let stop: std::collections::HashMap<String, i64> = q["stop_reasons"]
            .as_array().unwrap().iter()
            .map(|r| (r["stop_reason"].as_str().unwrap().to_string(), r["count"].as_i64().unwrap()))
            .collect();
        assert_eq!(stop.get("stop"), Some(&2));
        assert_eq!(stop.get("tool_calls"), Some(&1));

        assert!(q["error_clusters"].as_array().unwrap().iter().any(|r| r["reason"] == "upstream 502"));
    }

    /// #33: Logs tragen Projekt-Metadaten, und `project_stats` aggregiert je Scope.
    #[test]
    fn project_stats_aggregate_by_scope() {
        let s = store();
        s.insert(&RequestLog { project: Some("client-api".into()), real_cost_usd: 0.05, status: 200, ..Default::default() }).unwrap();
        s.insert(&RequestLog { project: Some("client-api".into()), task_type: "private_sensitive".into(), status: 200, ..Default::default() }).unwrap();
        s.insert(&RequestLog { project: Some("client-api".into()), status: 502, error: Some("x".into()), ..Default::default() }).unwrap();
        s.insert(&RequestLog { status: 200, ..Default::default() }).unwrap(); // ohne Projekt

        let stats = s.project_stats();
        let client = stats.iter().find(|p| p["project"] == "client-api").expect("client-api aggregiert");
        assert_eq!(client["requests"], 3);
        assert_eq!(client["rejected"], 1);
        assert_eq!(client["local_only"], 1);
        assert!((client["real_cost_usd"].as_f64().unwrap() - 0.05).abs() < 1e-9);
        // Requests ohne x-llmux-project laufen unter "(none)".
        assert!(stats.iter().any(|p| p["project"] == "(none)"));
    }

    #[test]
    fn enforce_cap_bounds_table_to_newest() {
        let s = store();
        let future = chrono::Utc::now() + chrono::Duration::hours(1);
        // Drei Einträge mit aufsteigendem created_ts.
        for (i, key) in ["a", "b", "c"].iter().enumerate() {
            let ts = (future + chrono::Duration::seconds(i as i64)).to_rfc3339();
            insert_cache(&s, key, &ts);
        }
        let removed = s.enforce_cache_cap(2);
        assert_eq!(removed, 1);
        assert_eq!(cache_count(&s), 2);
        // Der älteste ("a") wurde entfernt, die neuesten bleiben.
        assert!(s.cache_lookup("a").is_none());
        assert!(s.cache_lookup("c").is_some());
    }
}
