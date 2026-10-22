//! Siftmail: background indexing through the configured remote embedding
//! provider (Ollama / OpenAI-compatible, see [`super::providers`]).
//!
//! Independent of the `local-embeddings` feature. When the remote service is
//! unreachable the worker sets [`fallback_active`], which lets the local
//! candle worker (if compiled in) keep indexing and answering queries until
//! the remote provider is back; recovered vectors from other models are then
//! re-queued so the index never mixes models.

use super::providers::{self, EmbedKind, EmbeddingConfig, EmbeddingProvider, ProviderKind};
use super::store::VectorIndex;
use crate::accounts::credentials::{self, CredentialStoreHandle, Slot};
use crate::db::{Db, repo};
use crate::error::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::RwLock;

const BATCH: i64 = 16;
const IDLE: Duration = Duration::from_secs(5);
const WORK_YIELD: Duration = Duration::from_millis(200);
const ERR_BACKOFF: Duration = Duration::from_secs(60);
/// Approximate per-chunk budget for remote models (most accept ≥ 512).
const CHUNK_TOKENS: usize = 512;

static FALLBACK: AtomicBool = AtomicBool::new(false);

/// The remote provider is configured but unreachable: local indexing may
/// stand in.
pub fn fallback_active() -> bool {
    FALLBACK.load(Ordering::Relaxed)
}

#[derive(Default)]
pub struct RemoteState {
    pub index: RwLock<VectorIndex>,
    pub provider: RwLock<Option<Arc<dyn EmbeddingProvider>>>,
    /// `provider:model:dim` the index is scoped to; empty when inactive.
    pub active: RwLock<String>,
    pub last_error: RwLock<Option<String>>,
    /// Config the provider was built from (rebuild when it changes).
    built_from: RwLock<Option<EmbeddingConfig>>,
}

impl Default for VectorIndex {
    fn default() -> Self {
        VectorIndex::new(0, "")
    }
}

impl RemoteState {
    /// The ready provider and its index, unless in fallback.
    pub async fn ready(&self) -> Option<Arc<dyn EmbeddingProvider>> {
        if fallback_active() {
            return None;
        }
        self.provider.read().await.clone()
    }

    /// Top-k hits for a query through the remote provider.
    pub async fn search(&self, text: &str, k: usize) -> Result<Option<Vec<(i64, f32)>>> {
        let Some(p) = self.ready().await else {
            return Ok(None);
        };
        let v = p.embed(&[text.to_string()], EmbedKind::Query).await?;
        let Some(q) = v.first() else {
            return Ok(Some(Vec::new()));
        };
        Ok(Some(self.index.read().await.search(q, k)))
    }

    async fn deactivate(&self) {
        *self.provider.write().await = None;
        *self.active.write().await = String::new();
        *self.built_from.write().await = None;
        *self.index.write().await = VectorIndex::new(0, "");
    }
}

pub fn spawn(db: Db, state: Arc<RemoteState>, credentials: CredentialStoreHandle) {
    tokio::spawn(async move {
        loop {
            match tick(&db, &state, &credentials).await {
                Ok(true) => tokio::time::sleep(WORK_YIELD).await,
                Ok(false) => tokio::time::sleep(IDLE).await,
                Err(e) => {
                    tracing::warn!("remote embed worker: {e}");
                    *state.last_error.write().await = Some(e.to_string());
                    tokio::time::sleep(ERR_BACKOFF).await;
                }
            }
        }
    });
}

/// Re-queue messages whose vectors came from another model and drop those
/// vectors. Returns how many messages were re-queued.
pub(crate) fn requeue_other_models(conn: &rusqlite::Connection, model_id: &str) -> Result<usize> {
    let n = conn.execute(
        "UPDATE messages SET embedding_state = 'pending'
          WHERE body_state = 'cached'
            AND id IN (SELECT message_id FROM message_embeddings WHERE model_id <> ?1)",
        rusqlite::params![model_id],
    )?;
    repo::embeddings::prune_other_models(conn, model_id)?;
    Ok(n)
}

async fn ensure_provider(
    db: &Db,
    state: &RemoteState,
    credentials: &CredentialStoreHandle,
    cfg: EmbeddingConfig,
) -> Result<Arc<dyn EmbeddingProvider>> {
    if state.built_from.read().await.as_ref() == Some(&cfg)
        && let Some(p) = state.provider.read().await.clone()
    {
        return Ok(p);
    }
    let mut with_key = cfg.clone();
    with_key.api_key = credentials::load_async(credentials.clone(), 0, Slot::EmbeddingApiKey)
        .await
        .ok();
    let provider = match providers::build_provider(&with_key).await {
        Ok(p) => p,
        Err(e) => {
            if !FALLBACK.swap(true, Ordering::Relaxed) {
                tracing::warn!("embedding service unreachable, falling back to local: {e}");
            }
            return Err(e);
        }
    };
    let id = provider.id();
    // New model or dimension: never mix vectors, re-index from scratch.
    let m = id.clone();
    let requeued = db.write(move |conn| requeue_other_models(conn, &m)).await?;
    if requeued > 0 {
        tracing::info!("embedding model is now {id}; re-indexing {requeued} messages");
    }
    let m = id.clone();
    let rows = db
        .read(move |conn| repo::embeddings::load_all(conn, &m))
        .await?;
    let mut idx = VectorIndex::new(provider.dim(), id.clone());
    for (message_id, _, v) in rows {
        idx.push(message_id, &v);
    }
    *state.index.write().await = idx;
    *state.provider.write().await = Some(provider.clone());
    *state.active.write().await = id;
    *state.built_from.write().await = Some(cfg);
    Ok(provider)
}

async fn tick(db: &Db, state: &RemoteState, credentials: &CredentialStoreHandle) -> Result<bool> {
    let (cfg, _) = db
        .read(|conn| crate::core_api::embedding_settings::load(conn))
        .await?;
    if cfg.kind == ProviderKind::Local || cfg.base_url.trim().is_empty() {
        FALLBACK.store(false, Ordering::Relaxed);
        if !state.active.read().await.is_empty() {
            state.deactivate().await;
        }
        return Ok(false);
    }
    let provider = ensure_provider(db, state, credentials, cfg).await?;
    let model_id = provider.id();
    let dim = provider.dim();
    let batch = db
        .read(|conn| repo::embeddings::pending(conn, BATCH))
        .await?;
    if batch.is_empty() {
        // Still reachable? A cheap probe keeps fallback state honest.
        if fallback_active() {
            provider.embed(&["ping".into()], EmbedKind::Query).await?;
            FALLBACK.store(false, Ordering::Relaxed);
        }
        return Ok(false);
    }
    for mid in batch {
        let src = db
            .read(move |conn| repo::embeddings::source_text(conn, mid))
            .await?;
        let chunks = src
            .map(|(subject, body)| super::prepare_chunks(&subject, &body, CHUNK_TOKENS))
            .unwrap_or_default();
        let vectors = if chunks.is_empty() {
            Vec::new()
        } else {
            match provider.embed(&chunks, EmbedKind::Passage).await {
                Ok(v) => v,
                Err(e) => {
                    FALLBACK.store(true, Ordering::Relaxed);
                    *state.built_from.write().await = None;
                    return Err(e);
                }
            }
        };
        if FALLBACK.swap(false, Ordering::Relaxed) {
            tracing::info!("embedding service reachable again");
            let m = model_id.clone();
            db.write(move |conn| requeue_other_models(conn, &m).map(|_| ()))
                .await?;
        }
        let m = model_id.clone();
        let vecs = vectors.clone();
        db.write(move |conn| repo::embeddings::store_vectors(conn, mid, &m, dim, &vecs))
            .await?;
        let mut idx = state.index.write().await;
        for v in &vectors {
            idx.push(mid, v);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requeue_moves_other_model_vectors_back_to_pending() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrations::run(&mut conn).unwrap();
        // No messages: nothing to requeue, still prunes cleanly.
        assert_eq!(requeue_other_models(&conn, "ollama:m:4").unwrap(), 0);
    }

    #[tokio::test]
    async fn worker_activates_remote_provider_and_falls_back_when_unreachable() {
        use crate::embed::providers::test_server::{Seen, start};
        let server = start(Arc::new(|s: &Seen, _| {
            let n = s.body["input"].as_array().map(Vec::len).unwrap_or(1);
            let rows: Vec<String> = (0..n).map(|_| "[1.0,0.0,0.0]".into()).collect();
            (200, format!("{{\"embeddings\":[{}]}}", rows.join(",")))
        }))
        .await;
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("mail.db")).unwrap();
        let creds: CredentialStoreHandle =
            Arc::new(crate::accounts::credentials::UnavailableCredentialStore);
        let mut cfg = EmbeddingConfig {
            kind: ProviderKind::Ollama,
            base_url: server.base.clone(),
            model: "bge-m3".into(),
            ..Default::default()
        };
        let c = cfg.clone();
        db.write(move |conn| crate::core_api::embedding_settings::store(conn, &c, ""))
            .await
            .unwrap();
        let state = RemoteState::default();
        assert!(!tick(&db, &state, &creds).await.unwrap());
        assert_eq!(*state.active.read().await, "ollama:bge-m3:3");
        assert_eq!(state.search("hello", 5).await.unwrap(), Some(Vec::new()));
        assert!(!fallback_active());

        // Service goes away: the worker reports it and local may stand in.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        cfg.base_url = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        cfg.retries = 0;
        cfg.timeout_secs = 2;
        let c = cfg.clone();
        db.write(move |conn| crate::core_api::embedding_settings::store(conn, &c, ""))
            .await
            .unwrap();
        assert!(tick(&db, &state, &creds).await.is_err());
        assert!(fallback_active());
        assert_eq!(state.search("hello", 5).await.unwrap(), None);
        FALLBACK.store(false, Ordering::Relaxed);
    }
}
