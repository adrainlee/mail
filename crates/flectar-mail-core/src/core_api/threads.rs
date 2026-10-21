//! `Core` methods: Unsubscribe, inline images, folders and mailbox actions.
//! Moved verbatim from `lib.rs`; see docs/fork.md.

use crate::*;

impl Core {
    /// Newest message of `thread_id` that can be unsubscribed from, or None
    /// when the thread genuinely offers no list unsubscribe.
    ///
    /// The stored column alone is not enough to answer this: mail synced before
    /// List-Unsubscribe joined the IMAP header fetch has NULL there even when
    /// the cached raw message carries the header. So on a miss, re-read the
    /// raws we already have on disk (newest first) and persist the first header
    /// found, rather than telling the user there is no unsubscribe link.
    pub async fn thread_unsubscribe_message(&self, thread_id: i64) -> Result<Option<i64>> {
        if let Some(id) = self
            .db
            .read(move |conn| repo::messages::thread_unsubscribe_message(conn, thread_id))
            .await?
        {
            return Ok(Some(id));
        }
        let candidates = self
            .db
            .read(move |conn| repo::messages::thread_unsubscribe_candidates(conn, thread_id))
            .await?;
        for (id, raw_path) in candidates {
            let Ok(raw) =
                crate::file_io::read_headers(&raw_path, MAX_CACHED_HEADER_BYTES, "cached headers")
                    .await
            else {
                continue;
            };
            let Ok(headers) = crate::mime::parse_header_block(&raw) else {
                continue;
            };
            let Some(list_unsubscribe) = headers.list_unsubscribe else {
                continue;
            };
            let post = headers.list_unsubscribe_post;
            self.db
                .write(move |conn| {
                    repo::messages::set_list_unsubscribe(
                        conn,
                        id,
                        &list_unsubscribe,
                        post.as_deref(),
                    )
                })
                .await?;
            tracing::info!(
                message_id = id,
                thread_id,
                "recovered List-Unsubscribe from raw"
            );
            return Ok(Some(id));
        }
        Ok(None)
    }

    /// Unsubscribe from the list a message came from, preferring the RFC 8058
    /// one-click HTTPS POST, then the RFC 2369 mailto: (sent immediately over
    /// SMTP), and finally handing the URL back for the browser. The returned
    /// outcome states what actually happened so the UI can toast honestly.
    pub async fn unsubscribe_message(&self, message_id: i64) -> Result<UnsubscribeOutcome> {
        let detail = self
            .db
            .read(move |conn| repo::messages::detail(conn, message_id))
            .await?;
        let raw = detail
            .list_unsubscribe
            .ok_or_else(|| CoreError::NotFound("message has no List-Unsubscribe header".into()))?;
        let plan = unsubscribe::plan(&raw, detail.list_unsubscribe_post.as_deref())
            .ok_or_else(|| CoreError::Other("List-Unsubscribe header has no usable URI".into()))?;
        match plan {
            unsubscribe::UnsubscribePlan::OneClick { url } => {
                match unsubscribe::post_one_click(&url).await {
                    Ok(()) => {
                        tracing::info!(message_id, %url, "one-click unsubscribe accepted");
                        Ok(UnsubscribeOutcome::OneClick)
                    }
                    // The endpoint exists but didn't take the POST (blocked
                    // bots, expired token, outage): let the user finish there.
                    Err(e) => {
                        tracing::warn!(message_id, %url, error = %e, "one-click unsubscribe failed; deferring to browser");
                        Ok(UnsubscribeOutcome::NeedsBrowser { url })
                    }
                }
            }
            unsubscribe::UnsubscribePlan::Mailto { to, subject, body } => {
                self.send_unsubscribe_mail(detail.account_id, &to, &subject, &body)
                    .await?;
                tracing::info!(message_id, %to, "mailto unsubscribe sent");
                Ok(UnsubscribeOutcome::MailtoSent)
            }
            unsubscribe::UnsubscribePlan::Browser { url } => {
                Ok(UnsubscribeOutcome::NeedsBrowser { url })
            }
        }
    }

    /// Build and send the tiny unsubscribe-request message a mailto: List-
    /// Unsubscribe asks for. Sent directly (not through the pending-action
    /// queue): the user expects an immediate, definite answer, and there is no
    /// draft to reconcile.
    pub(crate) async fn send_unsubscribe_mail(
        &self,
        account_id: i64,
        to: &str,
        subject: &str,
        body: &str,
    ) -> Result<()> {
        let cfg = self
            .db
            .read(move |conn| repo::accounts::get_config(conn, account_id))
            .await?
            .ok_or_else(|| CoreError::NotFound(format!("account {account_id}")))?;
        let from = Address {
            name: cfg.display_name.clone(),
            email: cfg.email.clone(),
        };
        let domain = cfg
            .email
            .split('@')
            .nth(1)
            .unwrap_or("localhost")
            .to_string();
        let recipients = [Address {
            name: None,
            email: to.to_string(),
        }];
        let out = crate::mime::OutgoingMessage {
            from,
            reply_to: None,
            to: &recipients,
            cc: &[],
            bcc: &[],
            subject,
            body_text: body,
            body_html: None,
            in_reply_to: None,
            references: &[],
            message_id: None,
            message_id_domain: &domain,
            attachments: Vec::new(),
        };
        let (_msg_id, raw) = crate::mime::build_message(&out)?;
        let raw = crate::mail_security::Gpg::default()
            .protect(&raw, &cfg.settings.security, &cfg.email, out.to)
            .await?;
        let auth = match cfg.auth_kind {
            AuthKind::Password => crate::smtp::SmtpAuth::Password(
                credentials::load_async(self.credentials.clone(), cfg.id, Slot::Password).await?,
            ),
            AuthKind::Oauth2 => crate::smtp::SmtpAuth::XOAuth2(
                self.tokens.access_token(cfg.id, cfg.provider).await?,
            ),
        };
        crate::smtp::send_raw(&cfg, &auth, &cfg.email, &[to.to_string()], &raw).await
    }

    /// Rewrite `src="cid:…"` references in a message body to `data:` URIs built
    /// from its inline attachments so embedded images render in the native
    /// HTML renderer. No-op when the body has no cid: references. On any
    /// failure the original HTML is returned unchanged.
    pub(crate) async fn inline_cid_images(&self, message_id: i64, html: String) -> String {
        if !html.contains("cid:") {
            return html;
        }
        let referenced = crate::mime::referenced_cids(&html);
        if referenced.is_empty() {
            return html;
        }
        let atts = self
            .db
            .read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, content_id, mime_type, size, file_path FROM attachments
                     WHERE message_id = ?1 AND content_id IS NOT NULL
                       AND (part_id IS NOT NULL OR imap_section IS NOT NULL)",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![message_id], |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, Option<i64>>(3)?,
                            r.get::<_, Option<String>>(4)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await;
        let atts = match atts {
            Ok(a) if !a.is_empty() => a,
            _ => return html,
        };
        let mut fetched = Vec::new();
        let mut remaining = MAX_INLINE_CID_BYTES_PER_MESSAGE;
        for (attachment_id, content_id, mime, declared_size, file_path) in atts {
            if fetched.len() == MAX_INLINE_CID_IMAGES_PER_MESSAGE || remaining == 0 {
                break;
            }
            let normalized = crate::mime::normalize_cid(&content_id);
            if !referenced.contains(&normalized)
                || mime
                    .as_deref()
                    .is_some_and(|value| !value.starts_with("image/"))
            {
                continue;
            }
            let max_bytes = MAX_INLINE_CID_BYTES_PER_IMAGE.min(remaining);
            let declared_size = declared_size.and_then(|size| usize::try_from(size).ok());
            if declared_size.is_some_and(|size| size > max_bytes)
                || (file_path.is_none() && declared_size.is_none())
            {
                continue;
            }
            let Ok(path) = self.get_attachment(attachment_id).await else {
                continue;
            };
            let Ok(bytes) = crate::file_io::read(path, max_bytes, "inline image").await else {
                continue;
            };
            remaining -= bytes.len();
            fetched.push((content_id, mime, bytes));
        }
        if fetched.is_empty() {
            return html;
        }
        // Extraction + base64 of (possibly large) image parts is CPU-bound.
        let fallback = Arc::new(html);
        let worker_html = Arc::clone(&fallback);
        let rewritten = tokio::task::spawn_blocking(move || {
            let mut map = std::collections::HashMap::new();
            for (content_id, mime, bytes) in fetched {
                map.insert(
                    crate::mime::normalize_cid(&content_id),
                    (
                        mime.unwrap_or_else(|| "application/octet-stream".to_string()),
                        bytes,
                    ),
                );
            }
            crate::mime::rewrite_cid_src_with_images(&worker_html, &map)
        })
        .await;
        rewritten
            .unwrap_or_else(|_| Arc::try_unwrap(fallback).unwrap_or_else(|html| (*html).clone()))
    }

    /// Batch variant of `inline_cid_images` for the thread-open path: one DB
    /// read and one encode pass for the whole thread. Only attachments already
    /// on disk are inlined; missing ones are downloaded in the background (a
    /// `MailUpdated` event refreshes the thread when they land) so opening a
    /// thread never waits on the network. Returns every input id mapped to its
    /// (possibly rewritten) HTML.
    pub(crate) async fn inline_cid_images_batch(
        &self,
        thread_id: i64,
        pairs: Vec<(i64, String)>,
    ) -> std::collections::HashMap<i64, String> {
        use std::collections::HashMap;
        let mut out: HashMap<i64, String> = HashMap::new();
        let mut need: Vec<(i64, String)> = Vec::new();
        let mut referenced: HashMap<i64, HashSet<String>> = HashMap::new();
        for (id, html) in pairs {
            if html.contains("cid:") {
                let cids = crate::mime::referenced_cids(&html);
                if cids.is_empty() {
                    out.insert(id, html);
                } else {
                    referenced.insert(id, cids);
                    need.push((id, html));
                }
            } else {
                out.insert(id, html);
            }
        }
        if need.is_empty() {
            return out;
        }

        let ids: Vec<i64> = need.iter().map(|(id, _)| *id).collect();
        let atts = self
            .db
            .read(move |conn| {
                let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let sql = format!(
                    "SELECT message_id, id, content_id, mime_type, file_path, size FROM attachments
                     WHERE message_id IN ({placeholders}) AND content_id IS NOT NULL
                       AND (part_id IS NOT NULL OR imap_section IS NOT NULL)"
                );
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt
                    .query_map(rusqlite::params_from_iter(ids.iter()), |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, Option<String>>(3)?,
                            r.get::<_, Option<String>>(4)?,
                            r.get::<_, Option<i64>>(5)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await
            .unwrap_or_default();
        if atts.is_empty() {
            out.extend(need);
            return out;
        }

        let mut fetched: Vec<(i64, String, Option<String>, Vec<u8>)> = Vec::new();
        let mut missing: Vec<i64> = Vec::new();
        let mut thread_remaining = MAX_INLINE_CID_BYTES_PER_THREAD;
        let mut message_remaining: HashMap<i64, usize> = HashMap::new();
        let mut message_counts: HashMap<i64, usize> = HashMap::new();
        for (message_id, attachment_id, content_id, mime, file_path, declared_size) in atts {
            let normalized = crate::mime::normalize_cid(&content_id);
            if !referenced
                .get(&message_id)
                .is_some_and(|cids| cids.contains(&normalized))
                || mime
                    .as_deref()
                    .is_some_and(|value| !value.starts_with("image/"))
            {
                continue;
            }
            let count = message_counts.entry(message_id).or_default();
            if *count == MAX_INLINE_CID_IMAGES_PER_MESSAGE || thread_remaining == 0 {
                continue;
            }
            let message_remaining = message_remaining
                .entry(message_id)
                .or_insert(MAX_INLINE_CID_BYTES_PER_MESSAGE);
            let max_bytes = MAX_INLINE_CID_BYTES_PER_IMAGE
                .min(*message_remaining)
                .min(thread_remaining);
            let declared_size = declared_size.and_then(|size| usize::try_from(size).ok());
            if max_bytes == 0 || declared_size.is_some_and(|size| size > max_bytes) {
                continue;
            }
            let bytes = match file_path {
                Some(path) => crate::file_io::read(path, max_bytes, "inline image")
                    .await
                    .ok(),
                None => None,
            };
            match bytes {
                Some(bytes) => {
                    *count += 1;
                    *message_remaining -= bytes.len();
                    thread_remaining -= bytes.len();
                    fetched.push((message_id, content_id, mime, bytes));
                }
                None => {
                    let Some(size) = declared_size else { continue };
                    *count += 1;
                    *message_remaining -= size;
                    thread_remaining -= size;
                    missing.push(attachment_id);
                }
            }
        }
        if !missing.is_empty() {
            let core = self.clone();
            tokio::spawn(async move {
                let mut any = false;
                for id in missing {
                    any |= core.get_attachment(id).await.is_ok();
                }
                if any {
                    core.bus.emit(CoreEvent::MailUpdated {
                        thread_ids: vec![thread_id],
                    });
                }
            });
        }
        if fetched.is_empty() {
            out.extend(need);
            return out;
        }

        // Only messages with fetched images need rewriting. Move the others
        // straight to the result so their HTML is never copied for this worker.
        let fetched_ids: HashSet<i64> = fetched.iter().map(|(id, _, _, _)| *id).collect();
        let mut to_rewrite = Vec::new();
        for (id, html) in need {
            if fetched_ids.contains(&id) {
                to_rewrite.push((id, html));
            } else {
                out.insert(id, html);
            }
        }

        // Extraction + base64 of (possibly large) image parts is CPU-bound.
        let fallback = Arc::new(to_rewrite);
        let worker_need = Arc::clone(&fallback);
        let rewritten = tokio::task::spawn_blocking(move || {
            let mut maps: HashMap<i64, HashMap<String, (String, Vec<u8>)>> = HashMap::new();
            for (message_id, content_id, mime, bytes) in fetched {
                maps.entry(message_id).or_default().insert(
                    crate::mime::normalize_cid(&content_id),
                    (
                        mime.unwrap_or_else(|| "application/octet-stream".to_string()),
                        bytes,
                    ),
                );
            }
            worker_need
                .iter()
                .map(|(id, html)| match maps.get(id) {
                    Some(map) => {
                        let html = crate::mime::rewrite_cid_src_with_images(html, map);
                        (*id, html)
                    }
                    None => (*id, html.clone()),
                })
                .collect::<Vec<_>>()
        })
        .await;
        let rewritten = rewritten
            .unwrap_or_else(|_| Arc::try_unwrap(fallback).unwrap_or_else(|html| (*html).clone()));
        out.extend(rewritten);
        out
    }

    pub async fn list_folders(&self, account_id: Option<i64>) -> Result<Vec<FolderInfo>> {
        self.db
            .read(move |conn| repo::folders::list_info(conn, account_id))
            .await
    }

    pub async fn create_folder(
        &self,
        account_id: i64,
        parent_folder_id: Option<i64>,
        name: String,
    ) -> Result<()> {
        let name = validate_folder_leaf(&name)?;
        let config = self.folder_account_config(account_id).await?;
        let parent = match parent_folder_id {
            Some(folder_id) => Some(self.folder_for_child(account_id, folder_id).await?),
            None => None,
        };
        if config.provider == Provider::Gmail {
            if name.contains('/') {
                return Err(CoreError::Other(
                    "folder names cannot contain the hierarchy separator \"/\"".into(),
                ));
            }
            let full_name = parent
                .as_ref()
                .map(|parent| format!("{}/{}", parent.imap_name, name))
                .unwrap_or(name);
            sync::gmail::create_user_folder(&self.sync_ctx(), &config, &full_name).await?;
        } else if config.mail_protocol == MailProtocol::Jmap {
            let secret =
                credentials::load_async(self.credentials.clone(), config.id, Slot::Password)
                    .await?;
            let connected = jmap::client::connect(&config, &secret).await?;
            if parent.is_none() && !connected.may_create_top_level_mailbox {
                return Err(CoreError::Other(
                    "the server does not allow creating top-level mailboxes".into(),
                ));
            }
            let parent_remote = parent.as_ref().and_then(|folder| folder.jmap_id.clone());
            let parent_local_id = parent.as_ref().map(|folder| folder.id);
            let mailbox = connected
                .mailbox_create(&name, parent_remote, jmap_client::mailbox::Role::None)
                .await
                .map_err(jmap::client::map_error)?;
            let rights = mailbox.my_rights();
            let permissions = repo::folders::FolderPermissions {
                can_create_children: rights.is_some_and(|rights| rights.may_create_child()),
                can_rename: rights.is_some_and(|rights| rights.may_rename()),
                can_delete: rights.is_some_and(|rights| rights.may_delete()),
            };
            let remote_id = mailbox
                .id()
                .ok_or_else(|| CoreError::Jmap("Mailbox/set returned no id".into()))?
                .to_owned();
            let display_path = parent
                .as_ref()
                .map(|parent| format!("{} / {name}", parent.imap_name))
                .unwrap_or(name);
            self.db
                .write(move |conn| {
                    let folder_id = repo::folders::upsert_jmap(
                        conn,
                        account_id,
                        &remote_id,
                        &display_path,
                        None,
                    )?;
                    repo::folders::set_parent(conn, folder_id, parent_local_id)?;
                    repo::folders::set_permissions(conn, folder_id, permissions)?;
                    Ok(())
                })
                .await?;
        } else {
            let delimiter = match parent.as_ref().and_then(|folder| folder.delimiter.clone()) {
                Some(delimiter) if !delimiter.is_empty() => delimiter,
                _ => {
                    self.db
                        .read(move |conn| {
                            Ok(repo::folders::list(conn, Some(account_id))?
                                .into_iter()
                                .find_map(|folder| {
                                    folder.delimiter.filter(|value| !value.is_empty())
                                })
                                .unwrap_or_else(|| "/".into()))
                        })
                        .await?
                }
            };
            if name.contains(&delimiter) {
                return Err(CoreError::Other(format!(
                    "folder names cannot contain the hierarchy separator {delimiter:?}"
                )));
            }
            let encoded_leaf = imap::encode_mailbox_name(&name);
            let parent_local_id = parent.as_ref().map(|folder| folder.id);
            let remote_name = parent
                .as_ref()
                .map(|parent| format!("{}{delimiter}{encoded_leaf}", parent.imap_name))
                .unwrap_or(encoded_leaf);
            let mut session = self.connect_folder_imap(&config).await?;
            imap::create_folder(&mut session, &remote_name).await?;
            imap::logout(session).await;
            self.db
                .write(move |conn| {
                    let folder_id = repo::folders::upsert(
                        conn,
                        account_id,
                        &remote_name,
                        Some(&delimiter),
                        None,
                    )?;
                    repo::folders::set_parent(conn, folder_id, parent_local_id)?;
                    Ok(())
                })
                .await?;
        }
        self.bus.emit(CoreEvent::MailUpdated { thread_ids: vec![] });
        Ok(())
    }

    pub async fn rename_folder(&self, folder_id: i64, name: String) -> Result<()> {
        let name = validate_folder_leaf(&name)?;
        let folder = self
            .db
            .read(move |conn| repo::folders::get(conn, folder_id))
            .await?
            .ok_or_else(|| CoreError::NotFound(format!("folder {folder_id}")))?;
        if folder.role.is_some() {
            return Err(CoreError::Other("system folders cannot be renamed".into()));
        }
        if !folder.can_rename {
            return Err(CoreError::Other(
                "the server does not allow renaming this folder".into(),
            ));
        }
        let account_id = folder.account_id;
        let config = self.folder_account_config(account_id).await?;
        let delimiter = if config.mail_protocol == MailProtocol::Jmap {
            " / ".to_owned()
        } else {
            folder.delimiter.clone().unwrap_or_else(|| "/".into())
        };
        if name.contains(&delimiter) {
            return Err(CoreError::Other(format!(
                "folder names cannot contain the hierarchy separator {delimiter:?}"
            )));
        }
        let old_name = folder.imap_name.clone();
        let parent_prefix = old_name.rsplit_once(&delimiter).map(|(parent, _)| parent);
        if config.provider == Provider::Gmail {
            let full_name = parent_prefix
                .map(|parent| format!("{parent}{delimiter}{name}"))
                .unwrap_or(name);
            sync::gmail::rename_user_folder(&self.sync_ctx(), &config, folder_id, &full_name)
                .await?;
        } else if config.mail_protocol == MailProtocol::Jmap {
            let secret =
                credentials::load_async(self.credentials.clone(), config.id, Slot::Password)
                    .await?;
            let connected = jmap::client::connect(&config, &secret).await?;
            let remote_id = folder
                .jmap_id
                .as_deref()
                .ok_or_else(|| CoreError::NotFound(format!("remote folder {folder_id}")))?;
            connected
                .mailbox_rename(remote_id, &name)
                .await
                .map_err(jmap::client::map_error)?;
            let new_name = parent_prefix
                .map(|parent| format!("{parent}{delimiter}{name}"))
                .unwrap_or(name);
            self.db
                .write(move |conn| {
                    repo::folders::rename_tree(conn, account_id, &old_name, &new_name, &delimiter)
                })
                .await?;
        } else {
            let encoded = imap::encode_mailbox_name(&name);
            let new_name = parent_prefix
                .map(|parent| format!("{parent}{delimiter}{encoded}"))
                .unwrap_or(encoded);
            let mut session = self.connect_folder_imap(&config).await?;
            imap::rename_folder(&mut session, &old_name, &new_name).await?;
            imap::logout(session).await;
            self.db
                .write(move |conn| {
                    repo::folders::rename_tree(conn, account_id, &old_name, &new_name, &delimiter)
                })
                .await?;
        }
        self.bus.emit(CoreEvent::MailUpdated { thread_ids: vec![] });
        Ok(())
    }

    pub async fn delete_folder(&self, folder_id: i64) -> Result<()> {
        let folder = self
            .db
            .read(move |conn| repo::folders::get(conn, folder_id))
            .await?
            .ok_or_else(|| CoreError::NotFound(format!("folder {folder_id}")))?;
        if folder.role.is_some() {
            return Err(CoreError::Other("system folders cannot be deleted".into()));
        }
        if !folder.can_delete {
            return Err(CoreError::Other(
                "the server does not allow deleting this folder".into(),
            ));
        }
        let account_id = folder.account_id;
        let config = self.folder_account_config(account_id).await?;
        let delimiter = if config.mail_protocol == MailProtocol::Jmap {
            " / ".to_owned()
        } else {
            folder.delimiter.clone().unwrap_or_else(|| "/".into())
        };
        let prefix = format!("{}{delimiter}", folder.imap_name);
        let mut tree = self
            .db
            .read(move |conn| {
                Ok(repo::folders::list(conn, Some(account_id))?
                    .into_iter()
                    .filter(|candidate| {
                        candidate.id == folder_id || candidate.imap_name.starts_with(&prefix)
                    })
                    .collect::<Vec<_>>())
            })
            .await?;
        tree.sort_by_key(|candidate| std::cmp::Reverse(candidate.imap_name.len()));
        if config.provider == Provider::Gmail {
            for candidate in &tree {
                sync::gmail::delete_user_folder(&self.sync_ctx(), &config, candidate.id).await?;
            }
        } else if config.mail_protocol == MailProtocol::Jmap {
            let secret =
                credentials::load_async(self.credentials.clone(), config.id, Slot::Password)
                    .await?;
            let connected = jmap::client::connect(&config, &secret).await?;
            for candidate in &tree {
                let remote_id = candidate.jmap_id.as_deref().ok_or_else(|| {
                    CoreError::NotFound(format!("remote folder {}", candidate.id))
                })?;
                connected
                    .mailbox_destroy(remote_id, true)
                    .await
                    .map_err(jmap::client::map_error)?;
            }
            let root_name = folder.imap_name;
            self.db
                .write(move |conn| {
                    repo::folders::delete_tree(conn, account_id, &root_name, &delimiter)
                })
                .await?;
        } else {
            let mut session = self.connect_folder_imap(&config).await?;
            for candidate in &tree {
                imap::delete_folder(&mut session, &candidate.imap_name).await?;
            }
            imap::logout(session).await;
            let root_name = folder.imap_name;
            self.db
                .write(move |conn| {
                    repo::folders::delete_tree(conn, account_id, &root_name, &delimiter)
                })
                .await?;
        }
        self.bus.emit(CoreEvent::MailUpdated { thread_ids: vec![] });
        Ok(())
    }

    pub(crate) async fn folder_account_config(&self, account_id: i64) -> Result<AccountConfig> {
        self.db
            .read(move |conn| repo::accounts::get_config(conn, account_id))
            .await?
            .ok_or_else(|| CoreError::NotFound(format!("account {account_id}")))
    }

    pub(crate) async fn folder_for_child(
        &self,
        account_id: i64,
        folder_id: i64,
    ) -> Result<repo::folders::Folder> {
        let folder = self
            .db
            .read(move |conn| repo::folders::get(conn, folder_id))
            .await?
            .ok_or_else(|| CoreError::NotFound(format!("folder {folder_id}")))?;
        if folder.account_id != account_id {
            return Err(CoreError::Other(
                "parent folder belongs to another account".into(),
            ));
        }
        if !folder.can_create_children {
            return Err(CoreError::Other(
                "the server does not allow subfolders inside this folder".into(),
            ));
        }
        Ok(folder)
    }

    pub(crate) async fn connect_folder_imap(
        &self,
        config: &AccountConfig,
    ) -> Result<imap::Session> {
        let credentials = match config.auth_kind {
            AuthKind::Password => imap::ImapCredentials::Password {
                user: config.username.clone(),
                password: credentials::load_async(
                    self.credentials.clone(),
                    config.id,
                    Slot::Password,
                )
                .await?,
            },
            AuthKind::Oauth2 => imap::ImapCredentials::XOAuth2 {
                user: config.username.clone(),
                access_token: self.tokens.access_token(config.id, config.provider).await?,
            },
        };
        imap::connect_with_settings(
            &config.imap_host,
            config.imap_port,
            credentials,
            &config.settings.connection,
        )
        .await
    }

    pub async fn perform_action(&self, args: PerformActionArgs) -> Result<ActionResult> {
        let mut action_ids: Vec<i64> = Vec::new();
        let mut touched_accounts: Vec<i64> = Vec::new();

        for thread_id in args.thread_ids.clone() {
            let kind = args.kind;
            let params = args.params.clone();
            let ids = self
                .db
                .write(move |conn| apply_thread_action(conn, thread_id, kind, params.as_ref()))
                .await?;
            for (aid, account_id) in ids {
                action_ids.push(aid);
                if !touched_accounts.contains(&account_id) {
                    touched_accounts.push(account_id);
                }
            }
        }

        self.bus.emit(CoreEvent::MailUpdated {
            thread_ids: args.thread_ids.clone(),
        });
        for acc in touched_accounts {
            self.nudge(Some(acc), || SyncCmd::RunActions).await;
        }
        Ok(ActionResult { action_ids })
    }

    /// Queue a provider-side purge of Trash for one account or all accounts.
    /// The provider enumerates the whole mailbox, including mail outside the
    /// local history window and messages not loaded in the UI.
    pub async fn empty_trash(&self, account_id: Option<i64>) -> Result<ActionResult> {
        let queued = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                let mut stmt = tx.prepare(
                    "SELECT account_id, id FROM folders WHERE role = 'trash'
                 AND (?1 IS NULL OR account_id = ?1)",
                )?;
                let folders = stmt
                    .query_map(rusqlite::params![account_id], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                drop(stmt);
                if folders.is_empty() {
                    return Err(CoreError::NotFound(
                        "Trash folder is not available yet".into(),
                    ));
                }
                let mut queued = Vec::new();
                for (account_id, folder_id) in folders {
                    let active: bool = tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM pending_actions
                     WHERE account_id = ?1 AND kind = 'empty_trash'
                       AND state IN ('pending','inflight'))",
                        rusqlite::params![account_id],
                        |row| row.get(0),
                    )?;
                    if !active {
                        let action_id = repo::actions::enqueue(
                            &tx,
                            account_id,
                            "empty_trash",
                            None,
                            None,
                            &serde_json::json!({"folderId": folder_id}),
                            None,
                        )?;
                        queued.push((action_id, account_id));
                    }
                }
                tx.commit()?;
                Ok(queued)
            })
            .await?;
        for &(_, account_id) in &queued {
            self.nudge(Some(account_id), || SyncCmd::RunActions).await;
        }
        Ok(ActionResult {
            action_ids: queued.into_iter().map(|(id, _)| id).collect(),
        })
    }
}
