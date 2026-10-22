//! `Core` methods: embedding provider settings (Siftmail). The config is a
//! JSON value in `app_settings`; the bearer token / API key lives in the
//! credential store.

use crate::accounts::credentials::{self, Slot};
use crate::embed::providers::{self, EmbeddingConfig, ProviderKind};
use crate::*;

const KEY: &str = "siftmail.embedding";

pub(crate) fn load(conn: &rusqlite::Connection) -> Result<(EmbeddingConfig, Option<String>)> {
    use rusqlite::OptionalExtension;
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key = ?1",
            rusqlite::params![KEY],
            |r| r.get(0),
        )
        .optional()?;
    let Some(raw) = raw else {
        return Ok((EmbeddingConfig::default(), None));
    };
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
    let cfg =
        serde_json::from_value(v.get("config").cloned().unwrap_or_default()).unwrap_or_default();
    let model_id = v.get("modelId").and_then(|m| m.as_str()).map(str::to_owned);
    Ok((cfg, model_id))
}

pub(crate) fn store(
    conn: &rusqlite::Connection,
    cfg: &EmbeddingConfig,
    model_id: &str,
) -> Result<()> {
    let v = serde_json::json!({ "config": cfg, "modelId": model_id });
    conn.execute(
        "INSERT INTO app_settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![KEY, v.to_string()],
    )?;
    Ok(())
}

/// Vectors stored under other model ids (they cannot be mixed with the new
/// ones and need a rebuild).
pub(crate) fn stale_vectors(conn: &rusqlite::Connection, model_id: &str) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM message_embeddings WHERE model_id <> ?1",
        rusqlite::params![model_id],
        |r| r.get(0),
    )?)
}

/// Result of saving or testing a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingCheck {
    pub model_id: String,
    pub dim: usize,
    pub latency_ms: u64,
    /// Previous model id, when the new one differs (dimension/model change).
    pub previous_model_id: Option<String>,
    /// Stored vectors from other models; the UI offers a rebuild when > 0.
    pub stale_vectors: i64,
}

impl Core {
    pub async fn embedding_config(&self) -> Result<(EmbeddingConfig, bool)> {
        let (cfg, _) = self.db.read(|conn| load(conn)).await?;
        let has_key = credentials::load_async(self.credentials.clone(), 0, Slot::EmbeddingApiKey)
            .await
            .is_ok();
        Ok((cfg, has_key))
    }

    /// User-facing notice when the configured remote embedding service is
    /// unavailable. Search keeps working: semantic hits come back empty and
    /// keyword (FTS) results are shown on their own, or the bundled local
    /// model takes over in builds that ship it.
    pub async fn embedding_notice(&self) -> Option<String> {
        let err = self.remote_embed.last_error.read().await.clone()?;
        let fallback = if cfg!(feature = "local-embeddings") {
            "the local model is indexing instead"
        } else {
            "search uses keywords only until it is reachable again"
        };
        Some(format!(
            "Embedding service unavailable ({err}); {fallback}."
        ))
    }

    async fn with_key(&self, mut cfg: EmbeddingConfig, key: Option<String>) -> EmbeddingConfig {
        cfg.api_key = match key {
            Some(k) if !k.is_empty() => Some(k),
            _ => credentials::load_async(self.credentials.clone(), 0, Slot::EmbeddingApiKey)
                .await
                .ok(),
        };
        cfg
    }

    /// Connect and embed one probe text; nothing is saved.
    pub async fn test_embedding(
        &self,
        cfg: EmbeddingConfig,
        api_key: Option<String>,
    ) -> Result<EmbeddingCheck> {
        if cfg.kind == ProviderKind::Local {
            return Err(CoreError::Other(
                "the local model is tested by the embedding worker".into(),
            ));
        }
        let cfg = self.with_key(cfg, api_key).await;
        let start = std::time::Instant::now();
        let provider = providers::build_provider(&cfg).await?;
        provider
            .embed(
                &["Siftmail connection test".into()],
                providers::EmbedKind::Query,
            )
            .await?;
        let model_id = provider.id();
        let (_, previous) = self.db.read(|conn| load(conn)).await?;
        let mid = model_id.clone();
        let stale = self.db.read(move |conn| stale_vectors(conn, &mid)).await?;
        Ok(EmbeddingCheck {
            dim: provider.dim(),
            latency_ms: start.elapsed().as_millis() as u64,
            previous_model_id: previous.filter(|p| *p != model_id),
            model_id,
            stale_vectors: stale,
        })
    }

    /// Validate (probe) and save. `api_key`: `None` keeps the stored key,
    /// `Some("")` clears it.
    pub async fn save_embedding_config(
        &self,
        mut cfg: EmbeddingConfig,
        api_key: Option<String>,
    ) -> Result<EmbeddingCheck> {
        match &api_key {
            Some(k) if k.is_empty() => {
                credentials::delete_async(self.credentials.clone(), 0, Slot::EmbeddingApiKey)
                    .await?
            }
            Some(k) => {
                credentials::store_async(
                    self.credentials.clone(),
                    0,
                    Slot::EmbeddingApiKey,
                    k.clone(),
                )
                .await?
            }
            None => {}
        }
        let check = if cfg.kind == ProviderKind::Local {
            let model_id = providers::model_id(cfg.kind, &cfg.model, cfg.dim);
            let (_, previous) = self.db.read(|conn| load(conn)).await?;
            let mid = model_id.clone();
            let stale = self.db.read(move |conn| stale_vectors(conn, &mid)).await?;
            EmbeddingCheck {
                previous_model_id: previous.filter(|p| *p != model_id),
                model_id,
                dim: cfg.dim,
                latency_ms: 0,
                stale_vectors: stale,
            }
        } else {
            self.test_embedding(cfg.clone(), None).await?
        };
        cfg.dim = check.dim;
        cfg.api_key = None;
        let mid = check.model_id.clone();
        self.db.write(move |conn| store(conn, &cfg, &mid)).await?;
        Ok(check)
    }

    /// Drop vectors from other models so the index is rebuilt with the
    /// configured one. Returns the number of removed vectors.
    pub async fn rebuild_embedding_index(&self) -> Result<i64> {
        self.db
            .write(|conn| {
                let (_, current) = load(conn)?;
                let current = current.unwrap_or_default();
                Ok(conn.execute(
                    "DELETE FROM message_embeddings WHERE model_id <> ?1",
                    rusqlite::params![current],
                )? as i64)
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_without_secret_and_counts_stale_vectors() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrations::run(&mut conn).unwrap();
        assert_eq!(load(&conn).unwrap().1, None);
        let cfg = EmbeddingConfig {
            kind: ProviderKind::Ollama,
            base_url: "http://192.168.1.20:11434".into(),
            model: "bge-m3".into(),
            dim: 1024,
            api_key: Some("secret".into()),
            ..Default::default()
        };
        store(&conn, &cfg, "ollama:bge-m3:1024").unwrap();
        let raw: String = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key = ?1",
                [KEY],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!raw.contains("secret"));
        let (back, id) = load(&conn).unwrap();
        assert_eq!(back.base_url, cfg.base_url);
        assert_eq!(back.api_key, None);
        assert_eq!(id.as_deref(), Some("ollama:bge-m3:1024"));
        assert_eq!(stale_vectors(&conn, "ollama:bge-m3:1024").unwrap(), 0);
    }
}
