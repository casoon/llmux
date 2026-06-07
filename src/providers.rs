//! Weiterleitung an OpenAI-kompatible Provider (OpenRouter, OpenAI, Ollama).

use crate::config::{Config, Target};
use reqwest::Response;
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("provider '{0}' nicht konfiguriert oder deaktiviert")]
    Unavailable(String),
    #[error("API-Key env '{0}' nicht gesetzt")]
    MissingKey(String),
    #[error("HTTP-Fehler: {0}")]
    Http(#[from] reqwest::Error),
}

/// Sendet den (umgeschriebenen) Request an das Target. Setzt `model` auf den
/// Provider-Modellnamen und hängt Auth-Header an. Gibt die rohe Response zurück,
/// damit der Caller JSON parsen oder den Stream durchreichen kann.
pub async fn forward(
    cfg: &Config,
    client: &reqwest::Client,
    target: &Target,
    mut body: Value,
) -> Result<Response, ProviderError> {
    let provider = cfg
        .providers
        .get(&target.provider)
        .filter(|p| p.enabled)
        .ok_or_else(|| ProviderError::Unavailable(target.provider.clone()))?;

    // Modellfeld auf den providerspezifischen Namen umschreiben.
    if let Value::Object(map) = &mut body {
        map.insert("model".into(), Value::String(target.model.clone()));
    }

    let url = format!(
        "{}/chat/completions",
        provider.base_url.trim_end_matches('/')
    );

    let mut req = client.post(&url).json(&body);

    if let Some(env_name) = &provider.api_key_env {
        let key = std::env::var(env_name)
            .map_err(|_| ProviderError::MissingKey(env_name.clone()))?;
        req = req.bearer_auth(key);
    }

    let resp = req.send().await?;
    Ok(resp)
}
