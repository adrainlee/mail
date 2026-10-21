//! `Core` methods: AI translation of a message (Siftmail, plan §2).

use crate::translate::{self, TranslateConfig, TranslateEvent, TranslationOutcome};
use crate::*;
use std::sync::atomic::AtomicBool;

fn segment_body(
    html: Option<String>,
    text: Option<String>,
    opts: &translate::SegmentOpts,
) -> Option<(translate::Segmented, String)> {
    use sha2::Digest;
    let (seg, src) = match (html, text) {
        (Some(h), _) if !h.trim().is_empty() => (translate::segment_html(&h, opts), h),
        (_, Some(t)) if !t.trim().is_empty() => (translate::segment_text(&t), t),
        _ => return None,
    };
    let hash = sha2::Sha256::digest(src.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Some((seg, hash))
}

impl Core {
    /// Translation defaults from settings (target language).
    pub async fn translate_config(&self) -> Result<TranslateConfig> {
        let settings = self.db.read(|conn| repo::settings::get(conn)).await?;
        Ok(TranslateConfig {
            target_lang: settings.translate_target_lang,
            ..TranslateConfig::default()
        })
    }

    async fn load_translatable(&self, message_id: i64) -> Result<(translate::Segmented, String)> {
        let (html, text) = self
            .db
            .read(move |conn| {
                use rusqlite::OptionalExtension;
                Ok(conn
                    .query_row(
                        "SELECT html_body, text_body FROM message_bodies WHERE message_id = ?1",
                        rusqlite::params![message_id],
                        |r| {
                            Ok((
                                r.get::<_, Option<String>>(0)?,
                                r.get::<_, Option<String>>(1)?,
                            ))
                        },
                    )
                    .optional()?
                    .unwrap_or((None, None)))
            })
            .await?;
        // Bodies stored here are already sanitized by the MIME pipeline.
        segment_body(html, text, &translate::SegmentOpts::default())
            .ok_or_else(|| CoreError::NotFound(format!("body of message {message_id}")))
    }

    /// Cost/size estimate shown before translating large mail.
    pub async fn estimate_translation(
        &self,
        message_id: i64,
        cfg: TranslateConfig,
    ) -> Result<translate::Estimate> {
        let (seg, _) = self.load_translatable(message_id).await?;
        Ok(translate::estimate(&seg, &cfg))
    }

    /// A stored translation made from the current body, if any.
    pub async fn get_translation(
        &self,
        message_id: i64,
        lang: String,
    ) -> Result<Option<translate::store::StoredTranslation>> {
        let (_, hash) = self.load_translatable(message_id).await?;
        let model = self.ai_config(Scenario::Translate).await?.model;
        self.db
            .read(move |conn| translate::store::get(conn, message_id, &lang, &model, &hash))
            .await
    }

    /// Translate a message. Set `cancel` to stop; finished blocks are kept
    /// and cached, so calling again continues where it stopped. The caller
    /// is responsible for confirming large jobs (see `estimate_translation`).
    pub async fn translate_message(
        &self,
        message_id: i64,
        cfg: TranslateConfig,
        cancel: Arc<AtomicBool>,
        on_event: Option<Arc<dyn Fn(TranslateEvent) + Send + Sync>>,
    ) -> Result<TranslationOutcome> {
        let (seg, source_hash) = self.load_translatable(message_id).await?;
        let estimate = translate::estimate(&seg, &cfg);
        if cfg.skip_same_language && estimate.already_in_target {
            return Ok(TranslationOutcome {
                status: "same_language".into(),
                output: String::new(),
                is_html: !seg.plain,
                chunks_done: 0,
                chunks_total: 0,
                failed_blocks: Vec::new(),
            });
        }
        let ai = self.ai_config(Scenario::Translate).await?;
        let model = ai.model.clone();
        let lang = cfg.target_lang.clone();
        let hashes: Vec<String> = seg
            .blocks
            .iter()
            .filter(|b| b.translatable)
            .map(|b| translate::block_hash(&b.source, &lang, &model))
            .collect();
        let known = self
            .db
            .read(move |conn| translate::store::load_chunks(conn, &hashes))
            .await?;
        let cache = Arc::new(translate::store::MemCache::default());
        *cache.known.lock().unwrap_or_else(|e| e.into_inner()) = known;
        let result = translate::run(
            &seg,
            &cfg,
            Arc::new(ai),
            Some(cache.clone()),
            cancel,
            on_event,
        )
        .await;
        let assembled = translate::reassemble(&seg, &result.outputs);
        let status = if result.outputs.is_empty() && !assembled.failed.is_empty() {
            "failed"
        } else if result.cancelled || !result.errors.is_empty() || !assembled.failed.is_empty() {
            "partial"
        } else {
            "done"
        };
        let fresh = std::mem::take(&mut *cache.new.lock().unwrap_or_else(|e| e.into_inner()));
        let outcome = TranslationOutcome {
            status: status.into(),
            output: assembled.output,
            is_html: !seg.plain,
            chunks_done: result.chunks_done,
            chunks_total: result.chunks_total,
            failed_blocks: assembled.failed,
        };
        let saved = outcome.clone();
        self.db
            .write(move |conn| {
                translate::store::put_chunks(conn, &fresh)?;
                translate::store::save(
                    conn,
                    message_id,
                    &lang,
                    &model,
                    &source_hash,
                    &saved.status,
                    &saved.output,
                    saved.is_html,
                    saved.chunks_done,
                    saved.chunks_total,
                    saved.failed_blocks.len(),
                )?;
                // Keep the block cache bounded (~32 MB of text).
                translate::store::trim_cache(conn, 32 * 1024 * 1024)?;
                Ok(())
            })
            .await?;
        Ok(outcome)
    }
}
