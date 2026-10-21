//! `Core` methods: Attachment retrieval.
//! Moved verbatim from `lib.rs`; see docs/fork.md.

use crate::*;

impl Core {
    /// Return a stable cached attachment path. Legacy messages extract from
    /// their raw MIME; selectively-cached messages download only the requested
    /// IMAP section through the priority reader.
    pub async fn get_attachment(&self, attachment_id: i64) -> Result<String> {
        let fetch_lock = {
            let mut locks = self.attachment_locks.lock().await;
            match locks.get(&attachment_id).and_then(std::sync::Weak::upgrade) {
                Some(lock) => lock,
                None => {
                    if locks.len() >= 64 {
                        locks.retain(|_, lock| lock.strong_count() > 0);
                    }
                    let lock = Arc::new(tokio::sync::Mutex::new(()));
                    locks.insert(attachment_id, Arc::downgrade(&lock));
                    lock
                }
            }
        };
        let _fetch_guard = fetch_lock.lock().await;

        // This check deliberately happens after acquiring the single-flight
        // lock: a concurrent caller may just have populated `file_path`.
        let (message_id, part_id, imap_section, filename, mime_type, file_path, jmap_blob_id) = self
            .db
            .read(move |conn| {
                conn.query_row(
                    "SELECT message_id, part_id, imap_section, filename, mime_type, file_path, jmap_blob_id
                     FROM attachments WHERE id = ?1",
                    rusqlite::params![attachment_id],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, Option<String>>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, Option<String>>(3)?,
                            r.get::<_, Option<String>>(4)?,
                            r.get::<_, Option<String>>(5)?,
                            r.get::<_, Option<String>>(6)?,
                        ))
                    },
                )
                .map_err(Into::into)
            })
            .await?;

        if let Some(path) = file_path
            && tokio::fs::metadata(&path).await.is_ok()
        {
            if let Err(error) = self
                .index_attachment_text(attachment_id, std::path::Path::new(&path))
                .await
            {
                tracing::debug!(%error,"attachment text indexing deferred");
            }
            return Ok(path);
        }

        let row = self
            .db
            .read(move |conn| repo::messages::get_row(conn, message_id))
            .await?
            .ok_or_else(|| CoreError::NotFound("message".into()))?;

        // Prefer the legacy raw cache when it is healthy. If its file was
        // removed/corrupted but this row also has an IMAP section, safely fall
        // through to the remote section instead of making the attachment
        // permanently inaccessible because of a stale `raw_path`.
        let legacy = match (
            row.raw_path.as_ref().filter(|_| jmap_blob_id.is_none()),
            part_id.as_deref(),
        ) {
            (Some(raw_path), Some(part_id)) => {
                match crate::file_io::read(raw_path, MAX_CACHED_MESSAGE_BYTES, "cached message")
                    .await
                {
                    Ok(raw) => match crate::mime::extract_attachment(&raw, part_id) {
                        Ok(value) => Some(value),
                        Err(error) if imap_section.is_none() => return Err(error),
                        Err(_) => None,
                    },
                    Err(error) if imap_section.is_none() => return Err(error),
                    Err(_) => None,
                }
            }
            (Some(_), None) if imap_section.is_none() => {
                return Err(CoreError::NotFound("attachment part".into()));
            }
            _ => None,
        };
        let (bytes, parsed_name) = if let Some(value) = legacy {
            value
        } else {
            let handle = self
                .handles
                .read()
                .await
                .get(&row.account_id)
                .cloned()
                .ok_or_else(|| CoreError::NotFound(format!("account {}", row.account_id)))?;
            (handle.fetch_attachment(attachment_id).await?, None)
        };

        let mut safe_name = safe_filename(
            &filename
                .map(|name| crate::mime::decode_encoded_words(&name))
                .or(parsed_name)
                .unwrap_or_else(|| format!("attachment-{attachment_id}")),
        );
        // Invites and other body parts often carry no filename; give the temp
        // file an extension from its MIME type so opening it hands off to the
        // right app (a `text/calendar` part becomes `*.ics`, not a bare name
        // the OS opens in a text editor or refuses to open at all).
        if std::path::Path::new(&safe_name).extension().is_none()
            && let Some(ext) = mime_type.as_deref().and_then(ext_for_mime)
        {
            safe_name.push('.');
            safe_name.push_str(ext);
        }
        let dir = self
            .paths
            .attachments_dir(row.account_id)
            .join(attachment_id.to_string());
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(&safe_name);
        crate::file_io::write_atomic(&path, &bytes, "attachment cache").await?;

        let path_str = path.to_string_lossy().to_string();
        let p = path_str.clone();
        self.db
            .write(move |conn| {
                conn.execute(
                    "UPDATE attachments SET file_path = ?2 WHERE id = ?1",
                    rusqlite::params![attachment_id, p],
                )?;
                Ok(())
            })
            .await?;
        if let Err(error) = self.index_attachment_text(attachment_id, &path).await {
            tracing::debug!(%error,"attachment text indexing deferred");
        }
        Ok(path_str)
    }

    /// Extract an attachment and write it to a caller-chosen destination (the
    /// "download" / save-as path). Reuses `get_attachment` so extraction and
    /// filename handling stay in one place.
    pub async fn save_attachment(&self, attachment_id: i64, dest: String) -> Result<()> {
        let src = self.get_attachment(attachment_id).await?;
        tokio::fs::copy(&src, &dest).await?;
        Ok(())
    }
}
