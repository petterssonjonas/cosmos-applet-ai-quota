// SPDX-License-Identifier: MPL-2.0

pub mod claude;
pub mod openai;
pub mod opencode;
pub mod openrouter;

use crate::model::{Credential, Provider, ProviderError, Snapshot};
use reqwest::{Client, RequestBuilder};
use serde_json::Value;
use std::time::Duration;

pub fn client() -> Result<Client, ProviderError> {
    Client::builder()
        .timeout(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("cosmic-applet-ai-quota/0.1.0")
        .build()
        .map_err(|_| ProviderError("Could not initialize HTTPS client".into()))
}

pub async fn json(request: RequestBuilder) -> Result<Value, ProviderError> {
    let mut response = request.send().await.map_err(|error| {
        ProviderError(
            if error.is_timeout() {
                "Request timed out"
            } else {
                "Network request failed"
            }
            .into(),
        )
    })?;
    let status = response.status();
    if !status.is_success() {
        return Err(ProviderError(match status.as_u16() {
            401 => "Sign-in expired or credential rejected. Reconnect.".into(),
            403 => "Provider denied access to this data (HTTP 403).".into(),
            429 => "Provider rate limit reached. Try again later.".into(),
            _ => format!("Provider returned HTTP {}", status.as_u16()),
        }));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ProviderError("Could not read provider response".into()))?
    {
        if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
            return Err(ProviderError("Provider response is too large".into()));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| ProviderError("Provider returned invalid JSON".into()))
}

pub async fn fetch(provider: Provider, credential: &Credential) -> Result<Snapshot, ProviderError> {
    let client = client()?;
    match provider {
        Provider::Claude => claude::fetch(&client, credential).await,
        Provider::OpenAi => openai::fetch(&client, credential).await,
        Provider::OpenCodeGo => opencode::fetch_go(&client, credential).await,
        Provider::OpenCodeZen => opencode::fetch_zen(&client, credential).await,
        Provider::OpenRouter => openrouter::fetch(&client, credential).await,
    }
}

pub fn timestamp(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| {
        value
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|date| date.timestamp())
    })
}

pub fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|n| n.is_finite())
}

pub fn remaining(used: &Value) -> Option<f64> {
    number(used)
        .filter(|used| *used >= 0.0)
        .map(|used| (100.0 - used).clamp(0.0, 100.0))
}
