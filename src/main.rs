//! llmux — lokaler intent-basierter LLM-Router.
//! Pipeline: Prompt -> Token-Schätzung -> Privacy -> Klassifikation
//!           -> Budget -> Routing -> Weiterleitung -> Logging.

mod api;
mod cache;
mod classifier;
mod config;
mod cost;
mod logging;
mod privacy;
mod providers;
mod router;

/// Deterministische Routing-Eval-Fixtures (Klassifikator + Selektor-Kalibrierung).
#[cfg(test)]
mod eval;

use std::path::PathBuf;
use std::sync::Arc;

use api::AppState;
use config::Config;
use logging::Store;

/// User-Konfigverzeichnis `~/.config/llmux` (bzw. `$XDG_CONFIG_HOME/llmux`).
fn user_config_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|d| d.join("llmux"))
}

/// User-Datenverzeichnis `~/.local/share/llmux` (bzw. `$XDG_DATA_HOME/llmux`).
fn user_data_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .map(|d| d.join("llmux"))
}

/// Findet die Config. Reihenfolge: `LLMUX_CONFIG` (explizit) → `./config/llmux.yaml`
/// (Repo/lokal) → `~/.config/llmux/llmux.yaml` (standalone). Liefert zusätzlich, ob die
/// Config aus dem User-Verzeichnis stammt (steuert die DB-Default-Ablage). Schlägt mit
/// einer Anleitung fehl, wenn nichts gefunden wird.
fn resolve_config_path() -> anyhow::Result<(PathBuf, bool)> {
    if let Some(p) = std::env::var_os("LLMUX_CONFIG") {
        let p = PathBuf::from(p);
        if !p.exists() {
            anyhow::bail!("LLMUX_CONFIG zeigt auf '{}', aber die Datei existiert nicht", p.display());
        }
        return Ok((p, false));
    }

    let local = PathBuf::from("config/llmux.yaml");
    if local.exists() {
        return Ok((local, false));
    }

    let user = user_config_dir().map(|d| d.join("llmux.yaml"));
    if let Some(p) = user.as_ref() {
        if p.exists() {
            return Ok((p.clone(), true));
        }
    }

    anyhow::bail!(
        "Keine Konfiguration gefunden. Gesucht: ./config/llmux.yaml{}.\n\
         Lege eine an, z. B.:\n  mkdir -p {dir}\n  cp config/llmux.example.yaml {dst}\n\
         oder setze LLMUX_CONFIG=/pfad/zur/llmux.yaml.",
        user.as_ref().map(|p| format!(" und {}", p.display())).unwrap_or_default(),
        dir = user_config_dir().map(|d| d.display().to_string()).unwrap_or_else(|| "~/.config/llmux".into()),
        dst = user.map(|p| p.display().to_string()).unwrap_or_else(|| "~/.config/llmux/llmux.yaml".into()),
    );
}

/// Wählt den DB-Pfad. `LLMUX_DB` gewinnt; sonst landet die DB neben der Config:
/// im User-Datenverzeichnis (standalone) oder unter `./data` (Repo/lokal).
fn resolve_db_path(config_from_user_dir: bool) -> PathBuf {
    if let Some(p) = std::env::var_os("LLMUX_DB") {
        return PathBuf::from(p);
    }
    if config_from_user_dir {
        if let Some(dir) = user_data_dir() {
            return dir.join("llmux.sqlite");
        }
    }
    PathBuf::from("data/llmux.sqlite")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "llmux=info,tower_http=info".into()),
        )
        .init();

    let (config_path, from_user_dir) = resolve_config_path()?;
    let cfg = Config::load(&config_path)?;
    cfg.validate()?;
    tracing::info!(path = %config_path.display(), "Konfiguration geladen und validiert");

    let db_path = resolve_db_path(from_user_dir);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let store = Store::open(db_path.to_str().unwrap_or("data/llmux.sqlite"))?;
    tracing::info!(path = %db_path.display(), "SQLite-Log geöffnet");

    let addr = format!("{}:{}", cfg.server.host, cfg.server.port);
    // Browserfähige Anzeige-URL: 0.0.0.0/:: ist eine Bind-Adresse, kein Ziel zum Öffnen.
    let display_host = match cfg.server.host.as_str() {
        "0.0.0.0" | "::" | "[::]" => "localhost",
        h => h,
    };
    let display_addr = format!("{display_host}:{}", cfg.server.port);
    let state = Arc::new(AppState {
        cfg,
        http: reqwest::Client::new(),
        store,
        sessions: router::SessionStore::default(),
    });

    // Hintergrund-Eviction: abgelaufene Cache-Einträge + optionales Zeilenlimit.
    if state.cfg.cache.enabled {
        let evict = state.clone();
        tokio::spawn(async move {
            let secs = evict.cfg.cache.eviction_interval_seconds.max(1);
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(secs));
            loop {
                tick.tick().await;
                let expired = evict.store.evict_expired_cache();
                let capped = evict
                    .cfg
                    .cache
                    .max_entries
                    .map(|m| evict.store.enforce_cache_cap(m))
                    .unwrap_or(0);
                if expired + capped > 0 {
                    tracing::debug!(expired, capped, "cache eviction");
                }
            }
        });
    }

    let app = api::build_router(state);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("llmux läuft auf http://{display_addr}  (Dashboard unter /)");
    axum::serve(listener, app).await?;
    Ok(())
}
