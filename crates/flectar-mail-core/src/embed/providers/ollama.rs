//! Ollama (`POST {base}/api/embed`), local or on the LAN, with an optional
//! bearer token for deployments behind an authenticating reverse proxy.

use super::{EmbedKind, EmbeddingConfig, EmbeddingProvider, ProviderKind, finish, http, model_id};
use crate::error::{CoreError, Result};

pub struct OllamaProvider {
    cfg: EmbeddingConfig,
    url: String,
    dim: usize,
    http: reqwest::Client,
}

impl OllamaProvider {
    /// Create and probe: detects the dimension or verifies the configured one.
    pub async fn connect(cfg: &EmbeddingConfig) -> Result<Self> {
        let mut cfg = cfg.clone();
        if cfg.base_url.trim().is_empty() {
            cfg.base_url = super::DEFAULT_OLLAMA_URL.into();
        }
        if cfg.model.trim().is_empty() {
            return Err(CoreError::Other("Ollama embedding model is not set".into()));
        }
        let mut p = Self {
            url: http::endpoint(&cfg.base_url, "/api/embed")?,
            http: http::client(&cfg)?,
            dim: cfg.dim,
            cfg,
        };
        let probe = p.request(&["dimension probe".to_string()]).await?;
        let got = probe.first().map(Vec::len).unwrap_or(0);
        if got == 0 {
            return Err(CoreError::Other(
                "Ollama returned an empty embedding".into(),
            ));
        }
        if p.dim != 0 && p.dim != got {
            return Err(CoreError::Other(format!(
                "embedding dimension mismatch: configured {}, model returns {got}; rebuild the vector index",
                p.dim
            )));
        }
        p.dim = got;
        Ok(p)
    }

    async fn request(&self, input: &[String]) -> Result<Vec<Vec<f32>>> {
        let body = serde_json::json!({ "model": self.cfg.model, "input": input });
        let v = http::post_json(
            &self.http,
            &self.url,
            self.cfg.api_key.as_deref(),
            &body,
            self.cfg.retries,
        )
        .await?;
        let vectors = v
            .get("embeddings")
            .and_then(http::parse_vectors)
            .ok_or_else(|| CoreError::Other("Ollama response has no embeddings".into()))?;
        finish(vectors, input.len(), self.dim)
    }

    /// List installed models (`GET /api/tags`) for the settings picker.
    pub async fn list_models(cfg: &EmbeddingConfig) -> Result<Vec<String>> {
        let base = if cfg.base_url.trim().is_empty() {
            super::DEFAULT_OLLAMA_URL
        } else {
            cfg.base_url.as_str()
        };
        let url = http::endpoint(base, "/api/tags")?;
        let mut req = http::client(cfg)?.get(url);
        if let Some(t) = cfg.api_key.as_deref().filter(|t| !t.is_empty()) {
            req = req.bearer_auth(t);
        }
        let v: serde_json::Value = req
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| CoreError::Network(format!("Ollama unreachable: {e}")))?
            .json()
            .await
            .map_err(|e| CoreError::Other(format!("Ollama tags: {e}")))?;
        Ok(v.get("models")
            .and_then(|m| m.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for OllamaProvider {
    fn id(&self) -> String {
        model_id(ProviderKind::Ollama, &self.cfg.model, self.dim)
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn max_batch(&self) -> usize {
        self.cfg.batch()
    }
    async fn embed(&self, texts: &[String], kind: EmbedKind) -> Result<Vec<Vec<f32>>> {
        let prefix = self.cfg.prefix(kind);
        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(self.max_batch()) {
            let input: Vec<String> = chunk.iter().map(|t| format!("{prefix}{t}")).collect();
            out.extend(self.request(&input).await?);
        }
        Ok(out)
    }
}
