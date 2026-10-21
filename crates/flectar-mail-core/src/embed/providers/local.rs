//! Wraps the synchronous local candle embedder as an async provider.

use super::{EmbedKind, EmbeddingProvider, ProviderKind, model_id};
use crate::embed::Embedder;
use crate::error::{CoreError, Result};
use std::sync::Arc;

pub struct LocalCandleProvider(pub Arc<dyn Embedder>);

#[async_trait::async_trait]
impl EmbeddingProvider for LocalCandleProvider {
    fn id(&self) -> String {
        model_id(ProviderKind::Local, self.0.model_id(), self.0.dim())
    }
    fn dim(&self) -> usize {
        self.0.dim()
    }
    fn max_batch(&self) -> usize {
        32
    }
    async fn embed(&self, texts: &[String], kind: EmbedKind) -> Result<Vec<Vec<f32>>> {
        let inner = self.0.clone();
        let texts = texts.to_vec();
        tokio::task::spawn_blocking(move || match kind {
            EmbedKind::Passage => inner.embed(&texts),
            EmbedKind::Query => texts.iter().map(|t| inner.embed_query(t)).collect(),
        })
        .await
        .map_err(|e| CoreError::Other(format!("embedding task failed: {e}")))?
    }
}
