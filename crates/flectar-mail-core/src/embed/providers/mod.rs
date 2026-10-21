//! Siftmail M4: pluggable embedding providers.
//!
//! [`EmbeddingProvider`] is the async counterpart of the local [`super::Embedder`]
//! trait so remote services (a LAN Ollama, any OpenAI-compatible endpoint)
//! can be used. Every provider returns L2-normalized vectors and validates the
//! vector dimension. Embedding output is data only: providers cannot reference
//! mailbox actions (enforced by scripts/check-suggest-boundary.sh).

mod http;
#[cfg(feature = "local-embeddings")]
mod local;
mod ollama;
mod openai;

#[cfg(feature = "local-embeddings")]
pub use local::LocalCandleProvider;
pub use ollama::OllamaProvider;
pub use openai::OpenAiEmbeddingProvider;

use crate::error::{CoreError, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbedKind {
    Passage,
    Query,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    #[default]
    Local,
    Ollama,
    OpenAi,
}

/// Default Ollama endpoint; any `http(s)://host:port` (e.g. a LAN box) works.
pub const DEFAULT_OLLAMA_URL: &str = "http://127.0.0.1:11434";

/// Provider configuration. Secrets are not stored here: `api_key` is filled
/// in at runtime from the credential store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct EmbeddingConfig {
    pub kind: ProviderKind,
    pub base_url: String,
    pub model: String,
    /// Expected dimension; `0` = detect with a probe request.
    pub dim: usize,
    pub batch: usize,
    pub timeout_secs: u64,
    /// Retries for timeouts, 429 and 5xx (exponential backoff).
    pub retries: u32,
    pub query_prefix: String,
    pub passage_prefix: String,
    /// Route requests through the system proxy. Off by default so a LAN
    /// Ollama is reached directly.
    pub use_system_proxy: bool,
    #[serde(skip)]
    pub api_key: Option<String>,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            kind: ProviderKind::Local,
            base_url: String::new(),
            model: String::new(),
            dim: 0,
            batch: 32,
            timeout_secs: 30,
            retries: 2,
            query_prefix: String::new(),
            passage_prefix: String::new(),
            use_system_proxy: false,
            api_key: None,
        }
    }
}

impl EmbeddingConfig {
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs.clamp(1, 600))
    }
    pub fn batch(&self) -> usize {
        self.batch.clamp(1, 512)
    }
    fn prefix(&self, kind: EmbedKind) -> &str {
        match kind {
            EmbedKind::Passage => &self.passage_prefix,
            EmbedKind::Query => &self.query_prefix,
        }
    }
}

/// `provider:model:dim`, e.g. `ollama:bge-m3:1024`. Vectors from different
/// ids must never be mixed in one index.
pub fn model_id(kind: ProviderKind, model: &str, dim: usize) -> String {
    let k = match kind {
        ProviderKind::Local => "local",
        ProviderKind::Ollama => "ollama",
        ProviderKind::OpenAi => "openai",
    };
    format!("{k}:{model}:{dim}")
}

#[async_trait::async_trait]
pub trait EmbeddingProvider: Send + Sync {
    fn id(&self) -> String;
    fn dim(&self) -> usize;
    fn max_batch(&self) -> usize;
    /// Embed texts; returned vectors are L2-normalized and `dim()` long.
    async fn embed(&self, texts: &[String], kind: EmbedKind) -> Result<Vec<Vec<f32>>>;
}

pub fn l2_normalize(mut v: Vec<f32>) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 && norm.is_finite() {
        v.iter_mut().for_each(|x| *x /= norm);
    }
    v
}

/// Validate and normalize a provider response.
pub(crate) fn finish(
    vectors: Vec<Vec<f32>>,
    expected_len: usize,
    dim: usize,
) -> Result<Vec<Vec<f32>>> {
    if vectors.len() != expected_len {
        return Err(CoreError::Other(format!(
            "embedding service returned {} vectors for {expected_len} inputs",
            vectors.len()
        )));
    }
    vectors
        .into_iter()
        .map(|v| {
            if dim != 0 && v.len() != dim {
                return Err(CoreError::Other(format!(
                    "embedding dimension mismatch: expected {dim}, got {}; rebuild the vector index",
                    v.len()
                )));
            }
            if v.iter().any(|x| !x.is_finite()) {
                return Err(CoreError::Other("embedding contains non-finite values".into()));
            }
            Ok(l2_normalize(v))
        })
        .collect()
}

/// Build a remote provider, probing the dimension when `cfg.dim == 0` and
/// verifying it otherwise. `Local` must be built with the candle runtime.
pub async fn build_provider(cfg: &EmbeddingConfig) -> Result<Arc<dyn EmbeddingProvider>> {
    let provider: Arc<dyn EmbeddingProvider> = match cfg.kind {
        ProviderKind::Ollama => Arc::new(OllamaProvider::connect(cfg).await?),
        ProviderKind::OpenAi => Arc::new(OpenAiEmbeddingProvider::connect(cfg).await?),
        ProviderKind::Local => {
            return Err(CoreError::Other(
                "the local embedding provider is created by the embedding worker".into(),
            ));
        }
    };
    Ok(provider)
}

#[cfg(test)]
pub(crate) mod test_server;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_and_validate() {
        let v = finish(vec![vec![3.0, 4.0]], 1, 2).unwrap();
        assert!((v[0][0] - 0.6).abs() < 1e-6 && (v[0][1] - 0.8).abs() < 1e-6);
        assert!(
            finish(vec![vec![1.0]], 1, 2)
                .unwrap_err()
                .to_string()
                .contains("dimension")
        );
        assert!(finish(vec![vec![1.0, 2.0]], 2, 2).is_err());
        assert!(finish(vec![vec![f32::NAN, 1.0]], 1, 2).is_err());
        assert_eq!(
            model_id(ProviderKind::Ollama, "bge-m3", 1024),
            "ollama:bge-m3:1024"
        );
    }
}

#[cfg(test)]
mod provider_tests {
    use super::test_server::{Seen, start};
    use super::*;

    fn input_len(s: &Seen) -> usize {
        s.body["input"].as_array().map(Vec::len).unwrap_or(0)
    }

    fn cfg(kind: ProviderKind, base: &str) -> EmbeddingConfig {
        EmbeddingConfig {
            kind,
            base_url: base.into(),
            model: "bge-m3".into(),
            batch: 2,
            timeout_secs: 5,
            retries: 2,
            query_prefix: "q: ".into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn ollama_lan_url_bearer_batching_and_dim_probe() {
        let server = start(Arc::new(|s: &Seen, _| {
            assert_eq!(s.path, "/api/embed");
            let rows: Vec<String> = (0..input_len(s)).map(|_| "[3.0,4.0,0.0]".into()).collect();
            (200, format!("{{\"embeddings\":[{}]}}", rows.join(",")))
        }))
        .await;
        let mut c = cfg(ProviderKind::Ollama, &server.base);
        c.api_key = Some("lan-token".into());
        let p = build_provider(&c).await.unwrap();
        assert_eq!(p.dim(), 3);
        assert_eq!(p.id(), "ollama:bge-m3:3");
        let texts: Vec<String> = (0..5).map(|i| format!("t{i}")).collect();
        let v = p.embed(&texts, EmbedKind::Query).await.unwrap();
        assert_eq!(v.len(), 5);
        assert!((v[0][0] - 0.6).abs() < 1e-6);
        let seen = server.seen.lock().unwrap().clone();
        // probe + ceil(5 / 2) batches
        assert_eq!(seen.len(), 4);
        assert!(
            seen.iter()
                .all(|s| s.authorization.as_deref() == Some("Bearer lan-token"))
        );
        assert_eq!(seen[1].body["input"][0], "q: t0");
        assert_eq!(seen[1].body["model"], "bge-m3");
    }

    #[tokio::test]
    async fn dimension_mismatch_is_reported() {
        let server = start(Arc::new(|_: &Seen, _| {
            (200, r#"{"embeddings":[[1.0,2.0]]}"#.into())
        }))
        .await;
        let mut c = cfg(ProviderKind::Ollama, &server.base);
        c.dim = 1024;
        let err = build_provider(&c).await.err().unwrap().to_string();
        assert!(err.contains("dimension mismatch"), "{err}");
    }

    #[tokio::test]
    async fn openai_format_out_of_order_and_dimensions_param() {
        let server = start(Arc::new(|s: &Seen, _| {
            assert_eq!(s.path, "/v1/embeddings");
            let n = input_len(s);
            let rows: Vec<String> = (0..n)
                .rev()
                .map(|i| format!("{{\"index\":{i},\"embedding\":[{}.0,1.0]}}", i + 1))
                .collect();
            (200, format!("{{\"data\":[{}]}}", rows.join(",")))
        }))
        .await;
        let mut c = cfg(ProviderKind::OpenAi, &format!("{}/v1", server.base));
        c.dim = 2;
        c.api_key = Some("sk-test".into());
        let p = build_provider(&c).await.unwrap();
        let v = p
            .embed(&["a".into(), "b".into()], EmbedKind::Passage)
            .await
            .unwrap();
        // index 0 → [1,1], index 1 → [2,1] after reordering
        assert!((v[0][0] - v[0][1]).abs() < 1e-6);
        assert!(v[1][0] > v[1][1]);
        let seen = server.seen.lock().unwrap().clone();
        assert_eq!(seen[0].body["dimensions"], 2);
        assert_eq!(seen[0].authorization.as_deref(), Some("Bearer sk-test"));
    }

    #[tokio::test]
    async fn unauthorized_is_not_retried_and_5xx_is() {
        let server = start(Arc::new(|_: &Seen, _| (401, "{}".into()))).await;
        let err = build_provider(&cfg(ProviderKind::OpenAi, &server.base))
            .await
            .err()
            .unwrap();
        assert!(matches!(err, CoreError::Auth(_)));
        assert_eq!(server.seen.lock().unwrap().len(), 1);

        let flaky = start(Arc::new(|_: &Seen, n| {
            if n < 3 {
                (503, "{}".into())
            } else {
                (200, r#"{"embeddings":[[1.0]]}"#.into())
            }
        }))
        .await;
        let p = build_provider(&cfg(ProviderKind::Ollama, &flaky.base))
            .await
            .unwrap();
        assert_eq!(p.dim(), 1);
        assert_eq!(flaky.seen.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn unreachable_lan_host_fails_fast_with_network_error() {
        // Bind then drop to get a closed local port.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut c = cfg(ProviderKind::Ollama, &format!("http://127.0.0.1:{port}"));
        c.retries = 0;
        let err = build_provider(&c).await.err().unwrap();
        assert!(matches!(err, CoreError::Network(_)), "{err}");
        assert!(
            build_provider(&cfg(ProviderKind::Ollama, "ftp://x"))
                .await
                .is_err()
        );
    }
}
