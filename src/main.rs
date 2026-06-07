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

use std::sync::Arc;

use api::AppState;
use config::Config;
use logging::Store;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "llmux=info,tower_http=info".into()),
        )
        .init();

    let config_path =
        std::env::var("LLMUX_CONFIG").unwrap_or_else(|_| "config/llmux.yaml".into());
    let cfg = Config::load(&config_path)?;
    cfg.validate()?;
    tracing::info!(path = %config_path, "Konfiguration geladen und validiert");

    let db_path = std::env::var("LLMUX_DB").unwrap_or_else(|_| "data/llmux.sqlite".into());
    if let Some(parent) = std::path::Path::new(&db_path).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let store = Store::open(&db_path)?;
    tracing::info!(path = %db_path, "SQLite-Log geöffnet");

    let addr = format!("{}:{}", cfg.server.host, cfg.server.port);
    let state = Arc::new(AppState {
        cfg,
        http: reqwest::Client::new(),
        store,
        sessions: router::SessionStore::default(),
    });

    let app = api::build_router(state);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("llmux läuft auf http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
