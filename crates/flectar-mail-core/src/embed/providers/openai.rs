//! OpenAI-compatible embeddings (`POST {base}/v1/embeddings`).

use super::{EmbedKind, EmbeddingConfig, EmbeddingProvider, ProviderKind, finish, http, model_id};
use crate::error::{CoreError, Result};

pub struct OpenAiEmbeddingProvider {
    cfg: EmbeddingConfig,
    url: String,
    dim: usize,
    http: reqwest::Client,
}

impl OpenAiEmbeddingProvider {
    pub async fn connect(cfg: &EmbeddingConfig) -> Result<Self> {
        if cfg.model.trim().is_empty() {
            return Err(CoreError::Other("embedding model is not set".into()));
        }
        let base = cfg.base_url.trim().trim_end_matches('/');
        let base = if base.is_empty() {
            "https://api.openai.com"
        } else {
            base
        };
        // Accept both `https://host` and `https://host/v1`.
        let path = if base.ends_with("/v1") {
            "/embeddings"
        } else {
            "/v1/embeddings"
        };
        let mut p = Self {
            url: http::endpoint(base, path)?,
            http: http::client(cfg)?,
            dim: cfg.dim,
            cfg: cfg.clone(),
        };
        let probe = p.request(&["dimension probe".to_string()]).await?;
        let got = probe.first().map(Vec::len).unwrap_or(0);
        if got == 0 {
            return Err(CoreError::Other(
                "embedding service returned an empty vector".into(),
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
        let mut body = serde_json::json!({ "model": self.cfg.model, "input": input });
        if self.cfg.dim != 0 {
            body["dimensions"] = serde_json::json!(self.cfg.dim);
        }
        let v = http::post_json(
            &self.http,
            &self.url,
            self.cfg.api_key.as_deref(),
            &body,
            self.cfg.retries,
        )
        .await?;
        let mut rows: Vec<(u64, serde_json::Value)> = v
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| CoreError::Other("embedding response has no data".into()))?
            .iter()
            .enumerate()
            .map(|(i, row)| {
                (
                    row.get("index")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(i as u64),
                    row.get("embedding").cloned().unwrap_or_default(),
                )
            })
            .collect();
        // Responses may arrive out of order; `index` is authoritative.
        rows.sort_by_key(|(i, _)| *i);
        let arr = serde_json::Value::Array(rows.into_iter().map(|(_, e)| e).collect());
        let vectors = http::parse_vectors(&arr)
            .ok_or_else(|| CoreError::Other("embedding response is malformed".into()))?;
        finish(vectors, input.len(), self.dim)
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for OpenAiEmbeddingProvider {
    fn id(&self) -> String {
        model_id(ProviderKind::OpenAi, &self.cfg.model, self.dim)
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
