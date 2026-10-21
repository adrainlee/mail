//! Shared HTTP plumbing: client construction, bearer auth, retries.

use super::EmbeddingConfig;
use crate::error::{CoreError, Result};
use std::time::Duration;

pub(super) fn client(cfg: &EmbeddingConfig) -> Result<reqwest::Client> {
    let mut b = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(cfg.timeout())
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("Siftmail/", env!("CARGO_PKG_VERSION")));
    if !cfg.use_system_proxy {
        b = b.no_proxy();
    }
    b.build()
        .map_err(|e| CoreError::Network(format!("embedding HTTP client: {e}")))
}

pub(super) fn endpoint(base: &str, path: &str) -> Result<String> {
    let base = base.trim().trim_end_matches('/');
    let url = url::Url::parse(base)
        .map_err(|e| CoreError::Other(format!("invalid embedding base URL {base:?}: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(CoreError::Other(
            "embedding base URL must be http(s)".into(),
        ));
    }
    Ok(format!("{base}{path}"))
}

/// POST JSON with retries on timeouts, connect errors, 429 and 5xx.
pub(super) async fn post_json(
    http: &reqwest::Client,
    url: &str,
    bearer: Option<&str>,
    body: &serde_json::Value,
    retries: u32,
) -> Result<serde_json::Value> {
    let mut attempt = 0u32;
    loop {
        let mut req = http.post(url).json(body);
        if let Some(token) = bearer.filter(|t| !t.is_empty()) {
            req = req.bearer_auth(token);
        }
        let outcome = match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return resp.json::<serde_json::Value>().await.map_err(|e| {
                        CoreError::Other(format!("embedding response is not JSON: {e}"))
                    });
                }
                let text: String = resp
                    .text()
                    .await
                    .unwrap_or_default()
                    .chars()
                    .take(300)
                    .collect();
                let err = if status == reqwest::StatusCode::UNAUTHORIZED
                    || status == reqwest::StatusCode::FORBIDDEN
                {
                    return Err(CoreError::Auth(format!(
                        "embedding service rejected credentials ({status})"
                    )));
                } else {
                    CoreError::Network(format!("embedding service error {status}: {text}"))
                };
                let retryable =
                    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
                (err, retryable)
            }
            Err(e) => {
                let retryable = e.is_timeout() || e.is_connect();
                (
                    CoreError::Network(format!("embedding service unreachable: {e}")),
                    retryable,
                )
            }
        };
        let (err, retryable) = outcome;
        if !retryable || attempt >= retries {
            return Err(err);
        }
        attempt += 1;
        tokio::time::sleep(Duration::from_millis(200 * (1u64 << attempt.min(6)))).await;
    }
}

pub(super) fn parse_vectors(v: &serde_json::Value) -> Option<Vec<Vec<f32>>> {
    v.as_array()?
        .iter()
        .map(|row| {
            row.as_array()?
                .iter()
                .map(|x| x.as_f64().map(|f| f as f32))
                .collect::<Option<Vec<f32>>>()
        })
        .collect()
}
