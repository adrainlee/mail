//! `Core` methods: Sync triggers, thread listing and message bodies.
//! Moved verbatim from `lib.rs`; see docs/fork.md.

use crate::*;

impl Core {
    pub async fn sync_now(&self, account_id: Option<i64>) -> Result<()> {
        events::sync_diagnostic("refresh", account_id, "requested");
        let receivers = {
            let handles = self.handles.read().await;
            match account_id {
                Some(id) => handles
                    .get(&id)
                    .map(|handle| vec![handle.sync_now()])
                    .ok_or_else(|| CoreError::NotFound(format!("account {id}")))?,
                None => handles.values().map(AccountHandle::sync_now).collect(),
            }
        };
        if receivers.is_empty() {
            events::sync_diagnostic("refresh", account_id, "no_workers");
        }
        for receiver in receivers {
            let result =
                match tokio::time::timeout(std::time::Duration::from_secs(45), receiver).await {
                    Err(_) => {
                        events::sync_diagnostic("refresh", account_id, "timeout");
                        return Err(CoreError::Other("Inbox sync timed out".into()));
                    }
                    Ok(Err(_)) => {
                        events::sync_diagnostic("refresh", account_id, "worker_stopped");
                        return Err(CoreError::Other("sync actor stopped".into()));
                    }
                    Ok(Ok(result)) => result,
                };
            if let Err(error) = result {
                events::sync_diagnostic("refresh", account_id, "worker_error");
                return Err(CoreError::Other(error));
            }
        }
        events::sync_diagnostic("refresh", account_id, "complete");
        Ok(())
    }

    pub async fn get_sync_status(&self) -> Result<Vec<SyncStatus>> {
        let accounts = self.db.read(|conn| repo::accounts::list(conn)).await?;
        let mut statuses = Vec::with_capacity(accounts.len());
        for account in accounts {
            statuses.push(sync::engine::status_for_account(&self.db, account.id).await?);
        }
        Ok(statuses)
    }

    pub async fn list_threads(
        &self,
        view: View,
        split_id: Option<i64>,
        account_id: Option<i64>,
        label_id: Option<i64>,
        folder_id: Option<i64>,
        page: impl Into<ThreadPageRequest>,
    ) -> Result<ThreadPage> {
        use repo::threads::TabFilter;
        let ThreadPageRequest { cursor, limit } = page.into();
        // A category label filters to its single resolved tab; a manual label is
        // a cross-cutting filter. Split conventions: -1 = Important, -2 = Other,
        // positive ids = custom split tabs.
        let tab = if let Some(lid) = label_id {
            let is_auto = self
                .db
                .read(move |conn| Ok(repo::labels::get(conn, lid)?.map(|l| l.is_auto)))
                .await?
                .unwrap_or(false);
            Some(if is_auto {
                TabFilter::AutoLabel(lid)
            } else {
                TabFilter::ManualLabel(lid)
            })
        } else {
            match split_id {
                Some(-1) => Some(TabFilter::Important),
                Some(-2) => Some(TabFilter::Other),
                Some(id) if id > 0 => Some(TabFilter::Split(id)),
                _ => None,
            }
        };
        self.db
            .read(move |conn| {
                repo::threads::list(
                    conn,
                    &repo::threads::ListArgs {
                        view,
                        tab,
                        account_id,
                        folder_id,
                        cursor,
                        limit: limit.clamp(1, 200),
                    },
                )
            })
            .await
    }

    /// Exact count for the standard mailbox/folder scopes used by native
    /// shells. This shares the list query's predicates but executes one scalar
    /// SQL query instead of fetching every summary through cursor pagination.
    pub async fn count_threads(
        &self,
        view: View,
        account_id: Option<i64>,
        folder_id: Option<i64>,
    ) -> Result<usize> {
        self.count_threads_filtered(view, None, account_id, None, folder_id)
            .await
    }

    /// Exact count for any native sidebar scope, including Important/Other
    /// and manual or automatic labels.
    pub async fn count_threads_filtered(
        &self,
        view: View,
        split_id: Option<i64>,
        account_id: Option<i64>,
        label_id: Option<i64>,
        folder_id: Option<i64>,
    ) -> Result<usize> {
        use repo::threads::TabFilter;
        let tab = if let Some(label_id) = label_id {
            let is_auto = self
                .db
                .read(move |conn| Ok(repo::labels::get(conn, label_id)?.map(|l| l.is_auto)))
                .await?
                .unwrap_or(false);
            Some(if is_auto {
                TabFilter::AutoLabel(label_id)
            } else {
                TabFilter::ManualLabel(label_id)
            })
        } else {
            match split_id {
                Some(-1) => Some(TabFilter::Important),
                Some(-2) => Some(TabFilter::Other),
                Some(id) if id > 0 => Some(TabFilter::Split(id)),
                _ => None,
            }
        };
        self.db
            .read(move |conn| {
                repo::threads::count(
                    conn,
                    &repo::threads::ListArgs {
                        view,
                        tab,
                        account_id,
                        folder_id,
                        cursor: None,
                        limit: 1,
                    },
                )
            })
            .await
    }

    /// Load only the message displayed by a single-message thread preview.
    /// Conversation consumers can still request the complete thread separately.
    pub async fn get_latest_thread_body(&self, thread_id: i64) -> Result<MessageDetail> {
        let id = self
            .db
            .read(move |conn| repo::messages::latest_in_thread(conn, thread_id))
            .await?
            .ok_or_else(|| CoreError::NotFound(format!("thread {thread_id}")))?;
        self.load_body(id, Some(thread_id)).await
    }

    pub async fn get_thread(&self, thread_id: i64) -> Result<ThreadDetail> {
        let t0 = std::time::Instant::now();
        let mut detail = self
            .db
            .read(move |conn| {
                let thread = repo::threads::get_summary(conn, thread_id)?
                    .ok_or_else(|| CoreError::NotFound(format!("thread {thread_id}")))?;
                let messages = repo::messages::list_for_thread(conn, thread_id)?;
                Ok(ThreadDetail { thread, messages })
            })
            .await?;
        let t_db = t0.elapsed();
        // Resolve embedded cid: images to data: URIs for the native HTML
        // renderer, which cannot fetch cid: URLs. Batched: one DB read and one
        // encode pass for the whole thread.
        let pairs: Vec<(i64, String)> = detail
            .messages
            .iter_mut()
            .filter_map(|m| m.html_body.take().map(|h| (m.id, h)))
            .collect();
        let mut resolved = self.inline_cid_images_batch(thread_id, pairs).await;
        for m in &mut detail.messages {
            if let Some(html) = resolved.remove(&m.id) {
                m.html_body = Some(html);
            }
        }
        let t_cid = t0.elapsed() - t_db;
        // Kick off priority fetches for unfetched bodies, newest first. The
        // conditional database claim in `request_body` deduplicates repeated
        // thread opens while the request is queued or in flight.
        for m in detail.messages.iter().rev() {
            if m.body_state == "none" {
                self.request_body(m.account_id, m.id).await;
            }
        }
        let html_bytes: usize = detail
            .messages
            .iter()
            .filter_map(|m| m.html_body.as_ref().map(String::len))
            .sum();
        tracing::debug!(
            "get_thread {thread_id}: {} msgs, {html_bytes}B html, db {t_db:?}, cid {t_cid:?}, total {:?}",
            detail.messages.len(),
            t0.elapsed()
        );
        Ok(detail)
    }

    /// Load the lightweight message sequence for a conversation. Bodies,
    /// inline images, and attachment rows are deliberately omitted so a long
    /// thread can be opened without materializing every rich message.
    pub async fn get_thread_outline(&self, thread_id: i64) -> Result<ThreadDetail> {
        self.db
            .read(move |conn| {
                let thread = repo::threads::get_summary(conn, thread_id)?
                    .ok_or_else(|| CoreError::NotFound(format!("thread {thread_id}")))?;
                let messages = repo::messages::list_outline_for_thread(conn, thread_id)?;
                Ok(ThreadDetail { thread, messages })
            })
            .await
    }

    pub(crate) async fn request_body(&self, account_id: i64, message_id: i64) {
        let claimed = self
            .db
            .write(move |conn| repo::messages::begin_body_fetch(conn, message_id))
            .await
            .unwrap_or(false);
        if !claimed {
            return;
        }
        let sent = self
            .handles
            .read()
            .await
            .get(&account_id)
            .is_some_and(|handle| handle.send(SyncCmd::FetchBody { message_id }));
        if !sent {
            let _ = self
                .db
                .write(move |conn| repo::messages::cancel_body_fetch(conn, message_id))
                .await;
        }
    }

    pub async fn get_body(&self, message_id: i64) -> Result<MessageDetail> {
        self.load_body(message_id, None).await
    }

    pub(crate) async fn load_body(
        &self,
        message_id: i64,
        preview_thread: Option<i64>,
    ) -> Result<MessageDetail> {
        let (mut detail, cached_raw_path) = self
            .db
            .read(move |conn| {
                let detail = repo::messages::detail(conn, message_id)?;
                let cached_raw_path = if detail.sender_verification.is_verified() {
                    None
                } else {
                    repo::messages::get_row(conn, message_id)?.and_then(|row| row.raw_path)
                };
                Ok((detail, cached_raw_path))
            })
            .await?;
        // Previously downloaded messages may already have raw headers on disk.
        // Recover receiver-stamped sender authentication lazily when opened
        // instead of forcing a mailbox-wide header re-download.
        if let Some(raw_path) = cached_raw_path
            && let Ok(raw) =
                crate::file_io::read_headers(raw_path, MAX_CACHED_HEADER_BYTES, "cached headers")
                    .await
            && let Ok(headers) = crate::mime::parse_header_block(&raw)
            && headers.sender_verification.is_verified()
        {
            let verification = headers.sender_verification;
            self.db
                .write(move |conn| {
                    repo::messages::set_sender_verification(conn, message_id, verification)
                })
                .await?;
            detail.sender_verification = verification;
        }
        if detail.body_state == "none" {
            self.request_body(detail.account_id, message_id).await;
        }
        if let Some(html) = detail.html_body.take() {
            detail.html_body = if let Some(thread_id) = preview_thread {
                // Render cached content immediately; missing inline attachments
                // arrive through MailUpdated instead of delaying text on network I/O.
                self.inline_cid_images_batch(thread_id, vec![(message_id, html)])
                    .await
                    .remove(&message_id)
            } else {
                Some(self.inline_cid_images(message_id, html).await)
            };
        }
        Ok(detail)
    }
}
