//! `Core` methods: Undo, send queue and drafts.
//! Moved verbatim from `lib.rs`; see docs/fork.md.

use crate::*;

impl Core {
    pub async fn undo_last(&self) -> Result<bool> {
        let cutoff = now_ms() - 30_000;
        let last = self
            .db
            .read(move |conn| repo::actions::last_undoable(conn, cutoff))
            .await?;
        let Some(last) = last else { return Ok(false) };

        // Undo the whole gesture: same kind, created within 150ms of it.
        let (kind, created) = (last.kind.clone(), last.created_at);
        let undone_threads = self
            .db
            .write(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id FROM pending_actions
                     WHERE kind = ?1 AND ABS(created_at - ?2) <= 150
                       AND state IN ('pending','inflight','done')",
                )?;
                let ids = stmt
                    .query_map(rusqlite::params![kind, created], |r| r.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                drop(stmt);
                let mut threads = Vec::new();
                for id in ids {
                    if let Some(action) = repo::actions::get(conn, id)?
                        && let Some(tid) = revert_action(conn, &action)?
                        && !threads.contains(&tid)
                    {
                        threads.push(tid);
                    }
                }
                Ok(threads)
            })
            .await?;

        if !undone_threads.is_empty() {
            self.bus.emit(CoreEvent::MailUpdated {
                thread_ids: undone_threads,
            });
        }
        self.nudge(None, || SyncCmd::RunActions).await;
        Ok(true)
    }

    pub async fn cancel_send(&self, action_id: i64) -> Result<bool> {
        self.db
            .write(move |conn| repo::actions::try_cancel(conn, action_id))
            .await
    }

    /// "Send now": make a queued send due immediately (skip the remaining undo
    /// window) and nudge its actor. Returns false if it was already sent or
    /// cancelled.
    pub async fn send_now(&self, action_id: i64) -> Result<bool> {
        let account_id = self
            .db
            .write(move |conn| repo::actions::expedite(conn, action_id))
            .await?;
        match account_id {
            Some(account_id) => {
                self.nudge(Some(account_id), || SyncCmd::RunActions).await;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Wait for one explicitly requested send to finish.
    ///
    /// The pending-action queue remains the durable owner of delivery, but an
    /// interactive composer needs a stronger answer than "the action was
    /// queued". If the first delivery attempt fails, cancel its scheduled
    /// retry before returning the error so an editable preserved draft cannot
    /// also be delivered later behind the user's back.
    pub async fn wait_for_send(&self, action_id: i64) -> Result<()> {
        let observed_at = now_ms();
        loop {
            let status = self
                .db
                .read(move |conn| {
                    Ok(conn
                        .query_row(
                            "SELECT kind, state, last_error, not_before
                             FROM pending_actions WHERE id = ?1",
                            rusqlite::params![action_id],
                            |row| {
                                Ok((
                                    row.get::<_, String>(0)?,
                                    row.get::<_, String>(1)?,
                                    row.get::<_, Option<String>>(2)?,
                                    row.get::<_, Option<i64>>(3)?,
                                ))
                            },
                        )
                        .optional()?)
                })
                .await?;
            let Some((kind, state, last_error, not_before)) = status else {
                return Err(CoreError::NotFound("send action".into()));
            };
            if kind != "send" {
                return Err(CoreError::Other(format!(
                    "action {action_id} is not a send action"
                )));
            }

            match state.as_str() {
                "done" => return Ok(()),
                "failed" => {
                    return Err(CoreError::Other(
                        last_error.unwrap_or_else(|| "message delivery failed".into()),
                    ));
                }
                "cancelled" => {
                    return Err(CoreError::Other("message delivery was cancelled".into()));
                }
                // A retryable failure has already been written back to the
                // pending row. There is a backoff before another claim, so the
                // cancellation normally wins immediately; if another worker
                // did claim it, keep observing that definitive attempt.
                "pending" if last_error.is_some() => {
                    if self.cancel_send(action_id).await? {
                        return Err(CoreError::Other(last_error.unwrap()));
                    }
                }
                "pending" if send_start_expired(now_ms(), observed_at, not_before) => {
                    if self.cancel_send(action_id).await? {
                        return Err(CoreError::Smtp(format!(
                            "message delivery did not start within {} seconds of its scheduled time",
                            SEND_START_TIMEOUT.as_secs()
                        )));
                    }
                }
                "pending" | "inflight" => {}
                other => {
                    return Err(CoreError::Other(format!(
                        "send action entered an unknown state: {other}"
                    )));
                }
            }

            tokio::time::sleep(SEND_STATUS_POLL_INTERVAL).await;
        }
    }

    pub async fn save_draft(&self, args: SaveDraftArgs) -> Result<i64> {
        // Resolve the visible From address independently of the authenticated
        // account identity. Only provider-discovered, verified identities can
        // cross this boundary; the canonical provider display name is stored.
        let sender = self
            .resolve_sender_identity(
                args.account_id,
                args.from.as_ref().map(|address| address.email.as_str()),
            )
            .await?
            .address();
        // Stage every attachment into an app-managed dir up front, so the paths
        // persisted to `draft_attachments` (and later read at dispatch) are
        // always files the app itself copied - never an arbitrary path handed
        // in by the composer. Snapshotting here also fixes the sent bytes at
        // compose time rather than whatever the source file becomes later.
        if args.attachments.len() > MAX_DRAFT_ATTACHMENTS {
            return Err(CoreError::Other(format!(
                "a draft can contain at most {MAX_DRAFT_ATTACHMENTS} attachments"
            )));
        }
        let body_bytes = args
            .body_text
            .len()
            .saturating_add(args.body_html.as_ref().map_or(0, String::len));
        if body_bytes > MAX_DRAFT_BODY_BYTES {
            return Err(CoreError::Other(format!(
                "draft body exceeds the {} MiB safety limit",
                MAX_DRAFT_BODY_BYTES / (1024 * 1024)
            )));
        }
        let staging_root = self.paths.draft_attachments_dir();
        let reusable_paths = if let Some(draft_id) = args.draft_id {
            self.db
                .read(move |conn| {
                    let mut statement = conn
                        .prepare("SELECT file_path FROM draft_attachments WHERE draft_id = ?1")?;
                    Ok(statement
                        .query_map(rusqlite::params![draft_id], |row| row.get::<_, String>(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?)
                })
                .await?
        } else {
            Vec::new()
        };
        let mut reusable_canonical_paths = HashSet::new();
        for path in reusable_paths {
            if let Ok(path) = tokio::fs::canonicalize(path).await {
                reusable_canonical_paths.insert(path);
            }
        }
        let mut staged: Vec<crate::models::DraftAttachmentIn> =
            Vec::with_capacity(args.attachments.len());
        let mut created_paths: Vec<String> = Vec::new();
        let mut staged_bytes = 0usize;
        for att in &args.attachments {
            let remaining = MAX_DRAFT_ATTACHMENT_BYTES.saturating_sub(staged_bytes);
            let staged_attachment = stage_draft_attachment(
                &staging_root,
                &att.file_path,
                &att.filename,
                remaining,
                &reusable_canonical_paths,
            )
            .await;
            let (file_path, file_size, created) = match staged_attachment {
                Ok(value) => value,
                Err(error) => {
                    for path in &created_paths {
                        remove_staged_attachment(&staging_root, path).await;
                    }
                    return Err(error);
                }
            };
            if created {
                created_paths.push(file_path.clone());
            }
            staged_bytes += file_size;
            staged.push(crate::models::DraftAttachmentIn {
                file_path,
                filename: att.filename.clone(),
            });
        }
        let retained_paths: HashSet<String> = staged
            .iter()
            .map(|attachment| attachment.file_path.clone())
            .collect();
        let save_result = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;

                let (provider, mail_protocol): (String, String) = tx.query_row(
                    "SELECT provider,mail_protocol FROM accounts WHERE id = ?1",
                    rusqlite::params![args.account_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                // A user can compose immediately after OAuth, before the first
                // provider page has created folders. Give drafts a valid local
                // home up front; Gmail's names match the native label/synthetic
                // folders that label reconciliation will reuse.
                let drafts_folder = match repo::folders::by_role(
                    &tx,
                    args.account_id,
                    roles::DRAFTS,
                )? {
                    Some(folder) => folder.id,
                    None => repo::folders::upsert(
                        &tx,
                        args.account_id,
                        if provider == "gmail" { "DRAFT" } else { "Drafts" },
                        Some("/"),
                        Some(roles::DRAFTS),
                    )?,
                };
                let gmail_all_folder = if provider == "gmail" {
                    Some(match repo::folders::by_role(&tx, args.account_id, roles::ALL)? {
                        Some(folder) => folder.id,
                        None => repo::folders::upsert(
                            &tx,
                            args.account_id,
                            "[Gmail]/All Mail",
                            Some("/"),
                            Some(roles::ALL),
                        )?,
                    })
                } else {
                    None
                };

                // Thread: replies join the parent's thread.
                let thread_id = if let Some(parent_id) = args.in_reply_to_message_id {
                    repo::messages::get_row(&tx, parent_id)?.and_then(|r| r.thread_id)
                } else {
                    None
                };

                let draft_id = match args.draft_id {
                    Some(id) => {
                        let updated = tx.execute(
                            "UPDATE messages SET subject = ?2, to_json = ?3, cc_json = ?4,
                                    bcc_json = ?5, date = ?6, from_name = ?7, from_addr = ?8
                              WHERE id = ?1 AND account_id = ?9 AND is_draft = 1",
                            rusqlite::params![
                                id,
                                args.subject,
                                serde_json::to_string(&args.to)?,
                                serde_json::to_string(&args.cc)?,
                                serde_json::to_string(&args.bcc)?,
                                now_ms(),
                                sender.name,
                                sender.email,
                                args.account_id,
                            ],
                        )?;
                        if updated != 1 {
                            return Err(CoreError::NotFound("draft".into()));
                        }
                        id
                    }
                    None => {
                        let tid = match thread_id {
                            Some(t) => t,
                            None => repo::threads::create(
                                &tx,
                                args.account_id,
                                None,
                                &crate::mime::normalize_subject(&args.subject),
                            )?,
                        };
                        let nm = repo::messages::NewMessage {
                            account_id: args.account_id,
                            folder_id: drafts_folder,
                            uid: None,
                            message_id: None,
                            gm_msgid: None,
                            gm_thrid: None,
                            subject: args.subject.clone(),
                            from: Some(sender.clone()),
                            to: args.to.clone(),
                            cc: args.cc.clone(),
                            bcc: args.bcc.clone(),
                            date: now_ms(),
                            internal_date: None,
                            is_read: true,
                            is_starred: false,
                            is_draft: true,
                            is_outgoing: true,
                            is_automated: false,
                            has_attachments: false,
                            size: None,
                            snippet: crate::mime::make_snippet(&args.body_text),
                            references: Vec::new(),
                            list_unsubscribe: None,
                            list_unsubscribe_post: None,
                            sender_addr: None,
                            sender_verification: Default::default(),
                        };
                        let id = repo::messages::insert(&tx, &nm, tid)?;
                        tx.execute(
                            "INSERT INTO drafts_meta (message_id, mode, in_reply_to_message_id)
                             VALUES (?1, ?2, ?3)",
                            rusqlite::params![id, args.mode, args.in_reply_to_message_id],
                        )?;
                        id
                    }
                };

                tx.execute(
                    "INSERT INTO message_bodies (message_id, text_body, html_body) VALUES (?1, ?2, ?3)
                     ON CONFLICT(message_id) DO UPDATE SET text_body = excluded.text_body,
                                                           html_body = excluded.html_body",
                    rusqlite::params![draft_id, args.body_text, args.body_html],
                )?;
                tx.execute(
                    "UPDATE messages SET body_state = 'cached', snippet = ?2 WHERE id = ?1",
                    rusqlite::params![draft_id, crate::mime::make_snippet(&args.body_text)],
                )?;
                // Staged outgoing attachments: replace on every save.
                let old_paths = {
                    let mut statement = tx.prepare(
                        "SELECT file_path FROM draft_attachments WHERE draft_id = ?1",
                    )?;
                    statement
                        .query_map(rusqlite::params![draft_id], |row| row.get::<_, String>(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?
                };
                tx.execute(
                    "DELETE FROM draft_attachments WHERE draft_id = ?1",
                    rusqlite::params![draft_id],
                )?;
                for att in &staged {
                    tx.execute(
                        "INSERT INTO draft_attachments (draft_id, file_path, filename) VALUES (?1,?2,?3)",
                        rusqlite::params![draft_id, att.file_path, att.filename],
                    )?;
                }
                tx.execute(
                    "UPDATE messages SET has_attachments = ?2 WHERE id = ?1",
                    rusqlite::params![draft_id, (!staged.is_empty()) as i64],
                )?;
                if let Some(all_folder) = gmail_all_folder {
                    repo::gmail::set_message_folders(
                        &tx,
                        draft_id,
                        &[drafts_folder, all_folder],
                    )?;
                }
                let thread_id =
                    repo::messages::get_row(&tx, draft_id)?.and_then(|r| r.thread_id);
                if let Some(tid) = thread_id {
                    repo::threads::recompute(&tx, tid)?;
                }
                repo::search::index_message(&tx, draft_id)?;
                let security = repo::accounts::get_config(&tx, args.account_id)?.ok_or_else(|| CoreError::NotFound("account".into()))?.settings.security;
                // Protected drafts stay local until explicit Send. Even incomplete
                // recipient lists must never cause a plaintext server upload.
                let sync_remote_account = if !security.enabled() && (provider == "gmail" || mail_protocol == "jmap") {
                    tx.execute(
                        "UPDATE pending_actions SET state='cancelled',finished_at=?2
                         WHERE message_id=?1 AND kind='save_draft' AND state='pending'",
                        rusqlite::params![draft_id, now_ms()],
                    )?;
                    repo::actions::enqueue(
                        &tx,
                        args.account_id,
                        "save_draft",
                        Some(draft_id),
                        thread_id,
                        &serde_json::json!({ "draftId": draft_id }),
                        None,
                    )?;
                    Some(args.account_id)
                } else {
                    None
                };
                tx.commit()?;
                Ok((draft_id, thread_id, sync_remote_account, old_paths))
            })
            .await;
        let (draft_id, thread_id, sync_remote_account, old_paths) = match save_result {
            Ok(value) => value,
            Err(error) => {
                for path in &created_paths {
                    remove_staged_attachment(&staging_root, path).await;
                }
                return Err(error);
            }
        };
        for path in old_paths {
            if !retained_paths.contains(&path) {
                remove_staged_attachment(&staging_root, &path).await;
            }
        }
        if let Some(thread_id) = thread_id {
            self.bus.emit(CoreEvent::MailUpdated {
                thread_ids: vec![thread_id],
            });
        }
        if let Some(account_id) = sync_remote_account {
            self.nudge(Some(account_id), || SyncCmd::RunActions).await;
        }
        Ok(draft_id)
    }

    /// Load the complete editable state for a saved draft. Message detail omits
    /// Bcc and app-managed attachment paths, so reopening from the rendered
    /// message alone would silently lose both on the next save.
    pub async fn get_draft(&self, draft_id: i64) -> Result<SaveDraftArgs> {
        let (mut draft, remote_attachments) = self
            .db
            .read(move |conn| {
                let mut draft = conn
                    .query_row(
                        "SELECT m.account_id, m.from_name, m.from_addr,
                                m.to_json, m.cc_json, m.bcc_json, m.subject,
                                COALESCE(b.text_body, ''), b.html_body,
                                COALESCE(dm.mode, 'new'), dm.in_reply_to_message_id,
                                m.is_draft
                         FROM messages m
                         LEFT JOIN message_bodies b ON b.message_id = m.id
                         LEFT JOIN drafts_meta dm ON dm.message_id = m.id
                         WHERE m.id = ?1",
                        rusqlite::params![draft_id],
                        |row| {
                            let is_draft = row.get::<_, i64>(11)? != 0;
                            let from_name = row.get::<_, Option<String>>(1)?;
                            let from_email = row.get::<_, Option<String>>(2)?;
                            Ok((
                                SaveDraftArgs {
                                    draft_id: Some(draft_id),
                                    account_id: row.get(0)?,
                                    from: from_email.map(|email| Address {
                                        name: from_name,
                                        email,
                                    }),
                                    to: repo::parse_json_column(&row.get::<_, String>(3)?, 3)?,
                                    cc: repo::parse_json_column(&row.get::<_, String>(4)?, 4)?,
                                    bcc: repo::parse_json_column(&row.get::<_, String>(5)?, 5)?,
                                    subject: row.get(6)?,
                                    body_text: row.get(7)?,
                                    body_html: row.get(8)?,
                                    mode: row.get(9)?,
                                    in_reply_to_message_id: row.get(10)?,
                                    attachments: Vec::new(),
                                },
                                is_draft,
                            ))
                        },
                    )
                    .optional()?
                    .ok_or_else(|| CoreError::NotFound("draft".into()))?;
                if !draft.1 {
                    return Err(CoreError::NotFound("draft".into()));
                }

                let mut stmt = conn.prepare(
                    "SELECT file_path, filename
                     FROM draft_attachments WHERE draft_id = ?1 ORDER BY id",
                )?;
                draft.0.attachments = stmt
                    .query_map(rusqlite::params![draft_id], |row| {
                        Ok(crate::models::DraftAttachmentIn {
                            file_path: row.get(0)?,
                            filename: row.get(1)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let remote_attachments = if draft.0.attachments.is_empty() {
                    let mut stmt = conn.prepare(
                        "SELECT id, COALESCE(filename, '')
                         FROM attachments
                         WHERE message_id = ?1 AND is_inline = 0
                         ORDER BY id",
                    )?;
                    stmt.query_map(rusqlite::params![draft_id], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?
                } else {
                    Vec::new()
                };
                Ok((draft.0, remote_attachments))
            })
            .await?;

        // Synced provider drafts have attachment descriptors, not composer
        // staging rows. Materialize those parts through the account's priority
        // reader so the composer shows them and the next save snapshots the
        // exact bytes into its managed staging directory.
        for (attachment_id, filename) in remote_attachments {
            let file_path = self.get_attachment(attachment_id).await?;
            draft.attachments.push(crate::models::DraftAttachmentIn {
                file_path,
                filename: if filename.is_empty() {
                    format!("attachment-{attachment_id}")
                } else {
                    crate::mime::decode_encoded_words(&filename)
                },
            });
        }
        Ok(draft)
    }

    pub async fn delete_draft(&self, draft_id: i64) -> Result<()> {
        let (thread_id, nudge_account, staged_paths) = self
            .db
            .write(move |conn| {
                let row = conn
                    .query_row(
                        "SELECT m.account_id, m.thread_id, m.folder_id, m.uid, m.is_draft,
                                COALESCE(f.role, ''), a.provider,
                                m.gm_msgid, m.gmail_draft_id, a.mail_protocol, m.jmap_id
                         FROM messages m LEFT JOIN folders f ON f.id = m.folder_id
                         JOIN accounts a ON a.id = m.account_id
                         WHERE m.id = ?1",
                        rusqlite::params![draft_id],
                        |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, Option<i64>>(1)?,
                                row.get::<_, Option<i64>>(2)?,
                                row.get::<_, Option<i64>>(3)?,
                                row.get::<_, i64>(4)? != 0,
                                row.get::<_, String>(5)?,
                                row.get::<_, String>(6)?,
                                row.get::<_, Option<String>>(7)?,
                                row.get::<_, Option<String>>(8)?,
                                row.get::<_, String>(9)?,
                                row.get::<_, Option<String>>(10)?,
                            ))
                        },
                    )
                    .optional()?
                    .ok_or_else(|| CoreError::NotFound("draft".into()))?;
                if !row.4 {
                    return Err(CoreError::NotFound("draft".into()));
                }

                let send_inflight = conn.query_row(
                    "SELECT EXISTS(
                       SELECT 1 FROM pending_actions
                       WHERE message_id=?1 AND kind='send' AND state='inflight'
                     )",
                    rusqlite::params![draft_id],
                    |query_row| query_row.get::<_, bool>(0),
                )?;
                if send_inflight {
                    return Err(CoreError::Other(
                        "the draft is currently being submitted and cannot be deleted".into(),
                    ));
                }
                conn.execute(
                    "UPDATE pending_actions SET state='cancelled',finished_at=?2
                     WHERE message_id=?1 AND kind='send' AND state='pending'",
                    rusqlite::params![draft_id, now_ms()],
                )?;

                let mut nudge_account = None;
                let mut staged_paths = Vec::new();
                if row.9 == "jmap" {
                    conn.execute(
                        "UPDATE pending_actions SET state='cancelled',finished_at=?2
                         WHERE message_id=?1 AND kind='save_draft'
                           AND state IN ('pending','inflight')",
                        rusqlite::params![draft_id, now_ms()],
                    )?;
                    if let Some(remote_id) = row.10.as_deref() {
                        repo::actions::enqueue(
                            conn,
                            row.0,
                            "delete_draft",
                            None,
                            row.1,
                            &serde_json::json!({ "jmapEmailId": remote_id }),
                            None,
                        )?;
                        nudge_account = Some(row.0);
                    }
                    staged_paths = repo::messages::take_draft_attachment_paths(conn, draft_id)?;
                    repo::messages::delete(conn, draft_id)?;
                } else if row.6 == "gmail" {
                    // Cancel any queued autosaves before deleting the local
                    // row. If a remote resource already exists, retain its
                    // opaque ids in a message-independent delete action.
                    conn.execute(
                        "UPDATE pending_actions SET state = 'cancelled', finished_at = ?2
                         WHERE message_id = ?1 AND kind = 'save_draft'
                           AND state = 'pending'",
                        rusqlite::params![draft_id, now_ms()],
                    )?;
                    if row.7.is_some() || row.8.is_some() {
                        repo::actions::enqueue(
                            conn,
                            row.0,
                            "delete_draft",
                            None,
                            row.1,
                            &serde_json::json!({
                                "gmailMessageId": row.7,
                                "gmailDraftId": row.8,
                            }),
                            None,
                        )?;
                        nudge_account = Some(row.0);
                    }
                    staged_paths = repo::messages::take_draft_attachment_paths(conn, draft_id)?;
                    repo::messages::delete(conn, draft_id)?;
                } else if row.3.is_some() && row.5 != roles::TRASH {
                    // This draft exists on IMAP. Move it optimistically and
                    // queue the matching remote move so the next sync cannot
                    // resurrect the local row in Drafts.
                    let trash = repo::folders::by_role(conn, row.0, roles::TRASH)?
                        .ok_or_else(|| CoreError::NotFound("no trash folder".into()))?;
                    let payload = serde_json::json!({
                        "srcFolderId": row.2,
                        "srcUid": row.3,
                        "targetFolderId": trash.id,
                    });
                    repo::messages::set_uid_and_folder(conn, draft_id, trash.id, None)?;
                    repo::actions::enqueue(
                        conn,
                        row.0,
                        "trash",
                        Some(draft_id),
                        row.1,
                        &payload,
                        None,
                    )?;
                    nudge_account = Some(row.0);
                } else {
                    // Purely local drafts have no remote copy to preserve.
                    staged_paths = repo::messages::take_draft_attachment_paths(conn, draft_id)?;
                    repo::messages::delete(conn, draft_id)?;
                }
                if let Some(tid) = row.1 {
                    repo::threads::recompute(conn, tid)?;
                }
                Ok((row.1, nudge_account, staged_paths))
            })
            .await?;
        for path in staged_paths {
            remove_staged_attachment(&self.paths.draft_attachments_dir(), &path).await;
        }
        if let Some(thread_id) = thread_id {
            self.bus.emit(CoreEvent::MailUpdated {
                thread_ids: vec![thread_id],
            });
        }
        if let Some(account_id) = nudge_account {
            self.nudge(Some(account_id), || SyncCmd::RunActions).await;
        }
        Ok(())
    }

    pub async fn queue_send(&self, args: QueueSendArgs) -> Result<QueueSendResult> {
        let draft_id = args.draft_id;
        let (policy, sender, recipients) = self
            .db
            .read(move |conn| {
                let detail = repo::messages::detail(conn, draft_id)?;
                let config = repo::accounts::get_config(conn, detail.account_id)?
                    .ok_or_else(|| CoreError::NotFound("account".into()))?;
                let bcc: String = conn.query_row(
                    "SELECT bcc_json FROM messages WHERE id=?1 AND is_draft=1",
                    [draft_id],
                    |r| r.get(0),
                )?;
                let mut recipients = detail.to;
                recipients.extend(detail.cc);
                recipients.extend(serde_json::from_str::<Vec<Address>>(&bcc)?);
                let sender = detail.from.email;
                if repo::sender_identities::get_verified(conn, detail.account_id, &sender)?
                    .is_none()
                {
                    return Err(CoreError::Other(format!(
                        "{sender} is not an authorized sender identity for this account"
                    )));
                }
                Ok((config.settings.security, sender, recipients))
            })
            .await?;
        crate::mail_security::Gpg::default()
            .preflight(&policy, &sender, &recipients)
            .await?;
        let settings = self.db.read(|conn| repo::settings::get(conn)).await?;
        let dispatch_at = args
            .send_at
            .unwrap_or_else(|| now_ms() + settings.undo_send_seconds * 1000);

        let draft_id = args.draft_id;
        let (action_id, account_id) = self
            .db
            .write(move |conn| {
                let row = repo::messages::get_row(conn, draft_id)?
                    .ok_or_else(|| CoreError::NotFound("draft".into()))?;
                conn.execute(
                    "UPDATE pending_actions SET state = 'cancelled', finished_at = ?2
                     WHERE message_id = ?1 AND kind = 'save_draft' AND state = 'pending'",
                    rusqlite::params![draft_id, now_ms()],
                )?;
                let payload = serde_json::json!({ "draftId": draft_id, "mailSecurity": policy });
                let aid = repo::actions::enqueue(
                    conn,
                    row.account_id,
                    "send",
                    Some(draft_id),
                    row.thread_id,
                    &payload,
                    Some(dispatch_at),
                )?;
                Ok((aid, row.account_id))
            })
            .await?;

        // Fire exactly when the send comes due instead of waiting for the
        // scheduler's next tick (up to TICK_SECS of extra slop, which made even
        // an immediate send feel sluggish). The scheduler still covers restarts
        // and any timer this task misses. Only armed for near-term sends; far
        // future "send later" relies on the scheduler so we don't hold a task
        // sleeping for hours.
        let delay_ms = (dispatch_at - now_ms()).max(0);
        if delay_ms <= 15 * 60 * 1000 {
            let core = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms as u64)).await;
                core.nudge(Some(account_id), || SyncCmd::RunActions).await;
            });
        }
        Ok(QueueSendResult {
            action_id,
            draft_id,
            dispatch_at,
        })
    }
}
