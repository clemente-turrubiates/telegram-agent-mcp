//! Bot API transport: one bot token, plus retry/error handling.
//!
//! Holds no conversation state — that lives in [`super::hub::Hub`], which is
//! shared across every agent in the process.

use anyhow::{Context, Result, bail};
use std::time::Duration;

use super::model::ApiResponse;

const API_BASE: &str = "https://api.telegram.org";
const MAX_API_ATTEMPTS: u32 = 5;

/// A bot token and the HTTP client used to call it. Cheap to clone; the
/// underlying `reqwest::Client` is a shared connection pool.
#[derive(Clone)]
pub struct TelegramApi {
    http: reqwest::Client,
    token: String,
}

impl TelegramApi {
    pub fn new(http: reqwest::Client, token: String) -> Self {
        Self { http, token }
    }

    fn url(&self, method: &str) -> String {
        format!("{API_BASE}/bot{}/{method}", self.token)
    }

    /// POSTs a Telegram Bot API method and decodes the JSON envelope,
    /// automatically retrying on HTTP 429 (flood control) using the
    /// `retry_after` hint Telegram provides, and raising a clear error on 409
    /// (another process already long-polling this token).
    pub async fn call<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        body: &serde_json::Value,
        request_timeout: Option<Duration>,
    ) -> Result<T> {
        for attempt in 1..=MAX_API_ATTEMPTS {
            let mut req = self.http.post(self.url(method)).json(body);
            if let Some(t) = request_timeout {
                req = req.timeout(t);
            }
            let resp: ApiResponse<T> = req
                .send()
                .await
                .with_context(|| format!("sending request to Telegram {method}"))?
                .json()
                .await
                .with_context(|| format!("parsing Telegram {method} response"))?;

            if resp.ok {
                return resp
                    .result
                    .with_context(|| format!("missing result in {method} response"));
            }

            match resp.error_code {
                Some(429) if attempt < MAX_API_ATTEMPTS => {
                    let wait = resp
                        .parameters
                        .and_then(|p| p.retry_after)
                        .unwrap_or(1)
                        .max(1);
                    tracing::warn!(
                        "Telegram rate limit hit on {method}, retrying after {wait}s (attempt {attempt}/{MAX_API_ATTEMPTS})"
                    );
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                }
                Some(409) => bail!(
                    "Telegram getUpdates conflict (409): another process is already long-polling \
                     this bot token. Only one poller may run per token — check for a stale server \
                     instance, and give each agent its own bot rather than sharing one token."
                ),
                _ => bail!(
                    "Telegram API error on {method}: {}",
                    resp.description.unwrap_or_else(|| "unknown error".into())
                ),
            }
        }
        bail!("Telegram API {method} failed after {MAX_API_ATTEMPTS} attempts (rate limited)")
    }
}
