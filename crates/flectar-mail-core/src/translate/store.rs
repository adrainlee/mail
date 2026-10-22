//! Persistence for translations and the block cache.

use crate::error::Result;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;
use std::sync::Mutex;

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredTranslation {
    pub status: String,
    pub output: String,
    pub is_html: bool,
    pub source_hash: String,
    pub chunks_done: i64,
    pub chunks_total: i64,
    pub failed_blocks: i64,
}

#[allow(clippy::too_many_arguments)]
pub fn save(
    conn: &Connection,
    message_id: i64,
    lang: &str,
    model: &str,
    source_hash: &str,
    status: &str,
    output: &str,
    is_html: bool,
    chunks_done: usize,
    chunks_total: usize,
    failed_blocks: usize,
) -> Result<()> {
    let t = now();
    let (html, text) = if is_html {
        (Some(output), None)
    } else {
        (None, Some(output))
    };
    conn.execute(
        "INSERT INTO translations (message_id, target_lang, model, source_hash, status, html, text,
           chunks_done, chunks_total, failed_blocks, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)
         ON CONFLICT(message_id, target_lang, model) DO UPDATE SET source_hash = excluded.source_hash,
           status = excluded.status, html = excluded.html, text = excluded.text,
           chunks_done = excluded.chunks_done, chunks_total = excluded.chunks_total,
           failed_blocks = excluded.failed_blocks, updated_at = excluded.updated_at",
        params![message_id, lang, model, source_hash, status, html, text, chunks_done as i64,
                chunks_total as i64, failed_blocks as i64, t],
    )?;
    Ok(())
}

/// A stored translation, only if it was made from the same source body.
pub fn get(
    conn: &Connection,
    message_id: i64,
    lang: &str,
    model: &str,
    source_hash: &str,
) -> Result<Option<StoredTranslation>> {
    Ok(conn
        .query_row(
            "SELECT status, html, text, source_hash, chunks_done, chunks_total, failed_blocks
             FROM translations WHERE message_id = ?1 AND target_lang = ?2 AND model = ?3",
            params![message_id, lang, model],
            |r| {
                let html: Option<String> = r.get(1)?;
                let text: Option<String> = r.get(2)?;
                Ok(StoredTranslation {
                    status: r.get(0)?,
                    is_html: html.is_some(),
                    output: html.or(text).unwrap_or_default(),
                    source_hash: r.get(3)?,
                    chunks_done: r.get(4)?,
                    chunks_total: r.get(5)?,
                    failed_blocks: r.get(6)?,
                })
            },
        )
        .optional()?
        .filter(|t| t.source_hash == source_hash))
}

pub fn load_chunks(conn: &Connection, hashes: &[String]) -> Result<HashMap<String, String>> {
    let mut stmt = conn.prepare_cached("SELECT output FROM translation_chunks WHERE hash = ?1")?;
    let mut out = HashMap::new();
    for h in hashes {
        if let Some(v) = stmt
            .query_row(params![h], |r| r.get::<_, String>(0))
            .optional()?
        {
            out.insert(h.clone(), v);
        }
    }
    Ok(out)
}

pub fn put_chunks(conn: &Connection, rows: &HashMap<String, String>) -> Result<()> {
    let t = now();
    let mut stmt = conn.prepare_cached(
        "INSERT INTO translation_chunks (hash, output, used_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(hash) DO UPDATE SET output = excluded.output, used_at = excluded.used_at",
    )?;
    for (h, o) in rows {
        stmt.execute(params![h, o, t])?;
    }
    Ok(())
}

/// LRU trim of the block cache to roughly `max_bytes` of output text.
pub fn trim_cache(conn: &Connection, max_bytes: i64) -> Result<usize> {
    let total: i64 = conn.query_row(
        "SELECT COALESCE(SUM(length(output)), 0) FROM translation_chunks",
        [],
        |r| r.get(0),
    )?;
    if total <= max_bytes {
        return Ok(0);
    }
    let mut excess = total - max_bytes;
    let rows: Vec<(String, i64)> = conn
        .prepare("SELECT hash, length(output) FROM translation_chunks ORDER BY used_at ASC")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut n = 0;
    for (h, len) in rows {
        if excess <= 0 {
            break;
        }
        conn.execute("DELETE FROM translation_chunks WHERE hash = ?1", params![h])?;
        excess -= len;
        n += 1;
    }
    Ok(n)
}

/// In-memory cache preloaded from SQLite; new entries are flushed after a run.
#[derive(Default)]
pub struct MemCache {
    pub known: Mutex<HashMap<String, String>>,
    pub new: Mutex<HashMap<String, String>>,
}

impl super::BlockCache for MemCache {
    fn get(&self, hash: &str) -> Option<String> {
        self.known.lock().ok()?.get(hash).cloned()
    }
    fn put(&self, hash: &str, output: &str) {
        if let Ok(mut n) = self.new.lock() {
            n.insert(hash.to_string(), output.to_string());
        }
    }
}
