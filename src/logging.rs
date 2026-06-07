//! SQLite-Logging jedes Requests + Budget-Abfragen (Tages-/Monatssumme).

use rusqlite::{Connection, OptionalExtension};
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
                error              TEXT
            );

            CREATE TABLE IF NOT EXISTS cache (
                key        TEXT PRIMARY KEY,
                model      TEXT,
                response   TEXT NOT NULL,
                created_ts TEXT NOT NULL,
                expires_ts TEXT NOT NULL,
                hits       INTEGER NOT NULL DEFAULT 0
            );
            "#,
        )?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    pub fn insert(&self, log: &RequestLog) -> anyhow::Result<()> {
        let ts = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"INSERT INTO requests
               (ts, tool, session, task_type, model, provider, tier, used_fallback,
                degraded, budget_pressure, estimated_tokens, prompt_tokens,
                completion_tokens, estimated_cost_usd, real_cost_usd, latency_ms,
                status, cache_hit, attempts, attempt_trail, stop_reason, error)
               VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)"#,
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
}

use chrono::Datelike;
