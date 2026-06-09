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

/// Eingebettete Vorlagen (zur Compile-Zeit), damit `llmux init` ohne Repo funktioniert.
const EXAMPLE_CONFIG: &str = include_str!("../config/llmux.example.yaml");
const DEMO_CONFIG: &str = include_str!("../config/llmux.demo.yaml");

fn print_help() {
    println!(
        "llmux — local intent-based LLM router\n\n\
         USAGE:\n  \
         llmux                 Run the proxy + dashboard (resolves your config)\n  \
         llmux --demo          Run with the built-in echo provider (no key/cloud, in-memory)\n  \
         llmux init            Write an example config to ~/.config/llmux/llmux.yaml\n  \
         llmux init --demo     Write the echo demo config there instead\n  \
         llmux init --force    Overwrite an existing config\n  \
         llmux --help          Show this help\n\n\
         Config resolution: LLMUX_CONFIG -> ./config/llmux.yaml -> ~/.config/llmux/llmux.yaml\n\
         Dashboard + Stats API are served at /. Env: LLMUX_CONFIG, LLMUX_DB, RUST_LOG."
    );
}

/// `llmux init [--demo] [--force]`: schreibt eine Start-Config ins User-Verzeichnis
/// (oder nach `LLMUX_CONFIG`), ohne Bestehendes zu überschreiben (außer `--force`).
fn cmd_init(args: &[String]) -> anyhow::Result<()> {
    let demo = args.iter().any(|a| a == "--demo");
    let force = args.iter().any(|a| a == "--force");

    let target = match std::env::var_os("LLMUX_CONFIG") {
        Some(p) => PathBuf::from(p),
        None => user_config_dir()
            .map(|d| d.join("llmux.yaml"))
            .ok_or_else(|| anyhow::anyhow!("Kein HOME/XDG_CONFIG_HOME — setze LLMUX_CONFIG"))?,
    };

    if target.exists() && !force {
        println!(
            "Config existiert bereits: {}\nMit --force überschreiben.",
            target.display()
        );
        return Ok(());
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target, if demo { DEMO_CONFIG } else { EXAMPLE_CONFIG })?;

    println!("Config geschrieben: {}", target.display());
    if demo {
        println!("Demo-Provider (echo) aktiv — starte: llmux");
    } else {
        println!("Provider-Keys in der Config/.env eintragen, dann starten: llmux");
        println!("Oder ohne Setup ausprobieren: llmux --demo");
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "llmux=info,tower_http=info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--help") | Some("-h") | Some("help") => {
            print_help();
            return Ok(());
        }
        Some("init") => return cmd_init(&args),
        _ => {}
    }
    let demo = args.iter().any(|a| a == "--demo");

    // Config laden: Demo nutzt die eingebettete Echo-Config (keine Datei), sonst Discovery.
    let (cfg, source, db_path) = if demo {
        let cfg: Config = serde_yaml::from_str(DEMO_CONFIG)?;
        let db = std::env::var("LLMUX_DB").unwrap_or_else(|_| ":memory:".into());
        (cfg, "demo (eingebauter echo-Provider)".to_string(), db)
    } else {
        let (path, from_user_dir) = resolve_config_path()?;
        let cfg = Config::load(&path)?;
        let db = resolve_db_path(from_user_dir);
        if let Some(parent) = db.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        (cfg, path.display().to_string(), db.to_string_lossy().into_owned())
    };
    cfg.validate()?;
    tracing::info!(source = %source, "Konfiguration geladen und validiert");

    let store = Store::open(&db_path)?;
    tracing::info!(path = %db_path, "SQLite-Log geöffnet");

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

#[cfg(test)]
mod tests {
    use super::*;

    // Die eingebetteten Vorlagen (`init`, `--demo`) müssen jederzeit parsen und die
    // Konsistenzprüfung bestehen.
    #[test]
    fn embedded_configs_parse_and_validate() {
        for (name, raw) in [("example", EXAMPLE_CONFIG), ("demo", DEMO_CONFIG)] {
            let cfg: Config =
                serde_yaml::from_str(raw).unwrap_or_else(|e| panic!("{name} parst nicht: {e}"));
            cfg.validate()
                .unwrap_or_else(|e| panic!("{name} ist inkonsistent: {e}"));
        }
    }
}
