//! `Core` methods: OAuth flows, re-auth, account removal and recovery.
//! Moved verbatim from `lib.rs`; see docs/fork.md.

use crate::*;

impl Core {
    /// The Slint host calls this once its startup show (the first-run intro) is
    /// out of the way - or immediately when there is no show. Releases the
    /// deferred account-actor spawn in [`Core::start`], which is what first
    /// touches the OS keyring. `notify_one` stores a permit, so the order of
    /// caller vs. waiter never matters.
    pub fn notify_ui_ready(&self) {
        events::sync_diagnostic("ui_ready", None, "notified");
        self.ui_ready.notify_one();
    }

    /// Finish the rollback for an account whose process stopped after
    /// reserving its database id but before committing credentials. These rows
    /// are excluded from account lists and actor startup.
    pub(crate) async fn recover_incomplete_account_credentials(&self) -> Result<()> {
        let account_ids = self
            .db
            .read(|conn| repo::accounts::credential_setup_ids(conn))
            .await?;
        for account_id in account_ids {
            self.tokens.forget_account(account_id).await;
            credentials::delete_all_async(self.credentials.clone(), account_id).await?;
            self.db
                .write(move |conn| repo::accounts::delete(conn, account_id))
                .await?;
            tracing::info!(
                account_id,
                "rolled back incomplete account credential setup"
            );
        }
        Ok(())
    }

    /// Compensate for a failed cross-store credential commit. Credentials are
    /// deleted before the database id is released so a later account cannot
    /// inherit an orphaned token. A cleanup failure keeps the row in its hidden
    /// setup state for the next recovery attempt.
    pub(crate) async fn rollback_incomplete_account_credentials(&self, account_id: i64) {
        self.tokens.forget_account(account_id).await;
        if let Err(error) =
            credentials::delete_all_async(self.credentials.clone(), account_id).await
        {
            tracing::error!(
                account_id,
                %error,
                "could not remove credentials after account setup failed"
            );
            return;
        }
        if let Err(error) = self
            .db
            .write(move |conn| repo::accounts::delete(conn, account_id))
            .await
        {
            tracing::error!(
                account_id,
                %error,
                "could not remove incomplete account row"
            );
        }
    }

    pub async fn start_oauth(
        &self,
        provider: Provider,
        open_url: impl FnOnce(String) -> std::result::Result<(), String> + Send,
    ) -> Result<Account> {
        self.start_oauth_with_scopes(provider, &[], open_url).await
    }

    /// Connect mail and, when explicitly requested in account settings,
    /// include calendar consent in the same browser flow. Mail-only setup does
    /// not ask for Google Calendar or Microsoft Graph calendar permissions.
    pub async fn start_oauth_with_calendar<F>(
        &self,
        provider: Provider,
        connect_calendar: bool,
        open_url: F,
    ) -> Result<Account>
    where
        F: Fn(String) -> std::result::Result<(), String> + Clone + Send + 'static,
    {
        let consent_extra = oauth_calendar_scopes(provider, connect_calendar);
        let account = self
            .start_oauth_with_scopes(provider, consent_extra, open_url.clone())
            .await?;
        if connect_calendar {
            match provider {
                Provider::Gmail => {
                    self.connect_calendar(ConnectCalendarArgs {
                        account_id: account.id,
                        kind: "google".into(),
                        url: None,
                        username: None,
                        password: None,
                    })
                    .await?;
                }
                Provider::Microsoft => {
                    // The initial consent already included Calendars.ReadWrite;
                    // this retrieves its Graph-audience token without showing a
                    // second prompt, then discovers Outlook calendars.
                    self.connect_microsoft_calendar(account.id, open_url)
                        .await?;
                }
                Provider::Imap => {}
            }
        }
        Ok(account)
    }

    pub(crate) async fn start_oauth_with_scopes(
        &self,
        provider: Provider,
        consent_extra: &[&str],
        open_url: impl FnOnce(String) -> std::result::Result<(), String> + Send,
    ) -> Result<Account> {
        let servers = match provider {
            Provider::Gmail => &accounts::providers::GMAIL,
            Provider::Microsoft => &accounts::providers::MICROSOFT,
            Provider::Imap => return Err(CoreError::Auth("not an oauth provider".into())),
        };
        credentials::check_available_async(self.credentials.clone()).await?;
        self.recover_incomplete_account_credentials().await?;

        let outcome = tokio::select! {
            r = oauth::flow::authorize_with_broker(
                provider,
                consent_extra,
                None,
                self.oauth_redirects.clone(),
                open_url,
            ) => r?,
            _ = self.oauth_cancel.notified() => {
                return Err(CoreError::Auth("sign-in cancelled".into()));
            }
        };
        let email = outcome.email.clone();
        let existing = self
            .db
            .read({
                let email = email.clone();
                move |conn| repo::accounts::find_by_email(conn, &email)
            })
            .await?;
        if let Some(existing) = existing {
            if existing.provider != provider {
                return Err(CoreError::Auth(format!(
                    "{} is already connected through {}; use that provider to reconnect it",
                    existing.email,
                    existing.provider.as_str()
                )));
            }

            let id = existing.id;
            let display_name = outcome.display_name.clone();
            let avatar_url = outcome.avatar_url.clone();
            self.tokens
                .store_initial(
                    id,
                    outcome.access_token,
                    outcome.expires_in,
                    outcome.refresh_token,
                    outcome.client_id,
                    outcome.client_secret,
                )
                .await?;
            self.db
                .write(move |conn| {
                    repo::accounts::update_oauth_identity(
                        conn,
                        id,
                        display_name.as_deref(),
                        avatar_url.as_deref(),
                    )
                })
                .await?;
            self.db
                .write(move |conn| repo::accounts::set_sync_state(conn, id, "idle"))
                .await?;

            if let Some(handle) = self.handles.read().await.get(&id).cloned() {
                handle.send(SyncCmd::SyncNow { complete: None });
            } else {
                let cfg = self
                    .db
                    .read(move |conn| repo::accounts::get_config(conn, id))
                    .await?
                    .ok_or_else(|| CoreError::NotFound("account".into()))?;
                self.spawn_actor(cfg).await;
            }

            return self
                .db
                .read(move |conn| repo::accounts::get(conn, id))
                .await?
                .ok_or_else(|| CoreError::NotFound("account".into()));
        }

        let display_name = outcome.display_name.clone();
        let avatar_url = outcome.avatar_url.clone();
        let id = self
            .db
            .write(move |conn| {
                repo::accounts::insert_with_sync_state(
                    conn,
                    &repo::accounts::NewAccount {
                        email: &email,
                        display_name: display_name.as_deref(),
                        avatar_url: avatar_url.as_deref(),
                        provider,
                        auth_kind: AuthKind::Oauth2,
                        mail_protocol: MailProtocol::Imap,
                        username: &email,
                        jmap_url: "",
                        jmap_account_id: None,
                        imap_host: servers.imap_host,
                        imap_port: servers.imap_port,
                        smtp_host: servers.smtp_host,
                        smtp_port: servers.smtp_port,
                    },
                    repo::accounts::CREDENTIAL_SETUP_STATE,
                )
            })
            .await?;

        if let Err(error) = self
            .tokens
            .store_initial(
                id,
                outcome.access_token,
                outcome.expires_in,
                outcome.refresh_token,
                outcome.client_id,
                outcome.client_secret,
            )
            .await
        {
            self.rollback_incomplete_account_credentials(id).await;
            return Err(error);
        }

        if let Err(error) = self
            .db
            .write(move |conn| repo::accounts::set_sync_state(conn, id, "idle"))
            .await
        {
            self.rollback_incomplete_account_credentials(id).await;
            return Err(error);
        }

        let cfg = self
            .db
            .read(move |conn| repo::accounts::get_config(conn, id))
            .await?
            .ok_or_else(|| CoreError::NotFound("account".into()))?;
        self.spawn_actor(cfg).await;

        self.db
            .read(move |conn| repo::accounts::get(conn, id))
            .await?
            .ok_or_else(|| CoreError::NotFound("account".into()))
    }

    /// Re-run the OAuth consent for an existing account whose refresh token was
    /// revoked/expired (state `needs_reauth`) and swap in fresh tokens in place.
    /// Unlike `start_oauth` this never inserts a row: it updates the existing
    /// account's credentials and nudges its (paused) actor to reconnect.
    pub async fn reauth_account(
        &self,
        account_id: i64,
        open_url: impl FnOnce(String) -> std::result::Result<(), String> + Send,
    ) -> Result<Account> {
        let account = self
            .db
            .read(move |conn| repo::accounts::get(conn, account_id))
            .await?
            .ok_or_else(|| CoreError::NotFound("account".into()))?;
        // Only OAuth mailboxes reauth through the browser; password/IMAP
        // accounts recover by re-entering credentials in the account editor.
        let consent_extra: &[&str] = match account.provider {
            Provider::Microsoft => &[
                oauth::providers::MS_ONLINE_MEETINGS_SCOPE,
                oauth::providers::MS_CALENDARS_SCOPE,
            ],
            Provider::Gmail => &[],
            Provider::Imap => return Err(CoreError::Auth("not an oauth provider".into())),
        };
        credentials::check_available_async(self.credentials.clone()).await?;
        let outcome = tokio::select! {
            // Hint the provider at the account being repaired so the browser
            // preselects it instead of silently reusing whatever session is
            // active (which then fails the email match below).
            r = oauth::flow::authorize_with_broker(
                account.provider,
                consent_extra,
                Some(&account.email),
                self.oauth_redirects.clone(),
                open_url,
            ) => r?,
            _ = self.oauth_cancel.notified() => {
                return Err(CoreError::Auth("sign-in cancelled".into()));
            }
        };
        // Refuse to graft another mailbox's tokens onto this account. Signing in
        // as a different address is "add account", not "reauth".
        if !outcome.email.eq_ignore_ascii_case(&account.email) {
            return Err(CoreError::Auth(format!(
                "signed in as {} but this account is {}",
                outcome.email, account.email
            )));
        }

        let display_name = outcome.display_name.clone();
        let avatar_url = outcome.avatar_url.clone();
        self.tokens
            .store_initial(
                account_id,
                outcome.access_token,
                outcome.expires_in,
                outcome.refresh_token,
                outcome.client_id,
                outcome.client_secret,
            )
            .await?;
        self.db
            .write(move |conn| {
                repo::accounts::update_oauth_identity(
                    conn,
                    account_id,
                    display_name.as_deref(),
                    avatar_url.as_deref(),
                )
            })
            .await?;

        // Clear needs_reauth and wake the actor. The reauth pause loop retries
        // its connect on the next SyncNow; if no actor is live (e.g. reauth
        // right after launch), spawn one so the fresh tokens take effect.
        self.db
            .write(move |conn| repo::accounts::set_sync_state(conn, account_id, "idle"))
            .await?;
        let has_handle = self.handles.read().await.contains_key(&account_id);
        if has_handle {
            if let Some(handle) = self.handles.read().await.get(&account_id) {
                handle.send(SyncCmd::SyncNow { complete: None });
            }
        } else {
            let cfg = self
                .db
                .read(move |conn| repo::accounts::get_config(conn, account_id))
                .await?
                .ok_or_else(|| CoreError::NotFound("account".into()))?;
            self.spawn_actor(cfg).await;
        }

        self.db
            .read(move |conn| repo::accounts::get(conn, account_id))
            .await?
            .ok_or_else(|| CoreError::NotFound("account".into()))
    }

    pub(crate) async fn purge_calendar_account(&self, account_id: i64) -> Result<()> {
        self.calendar_db
            .write(move |conn| {
                let tx = conn.transaction()?;
                tx.execute(
                    "DELETE FROM pending_actions WHERE account_id = ?1",
                    rusqlite::params![account_id],
                )?;
                tx.execute(
                    "DELETE FROM calendar_events WHERE account_id = ?1",
                    rusqlite::params![account_id],
                )?;
                tx.execute(
                    "DELETE FROM calendars WHERE account_id = ?1",
                    rusqlite::params![account_id],
                )?;
                tx.execute(
                    "DELETE FROM caldav_config WHERE account_id = ?1",
                    rusqlite::params![account_id],
                )?;
                tx.commit()?;
                Ok(())
            })
            .await
    }

    /// Recreate the old same-database `ON DELETE SET NULL` behavior for mail
    /// invite links now that calendar data has its own SQLite file. Calendar
    /// rows remain useful after their source message is deleted, so detach the
    /// optional reference rather than deleting the event.
    pub(crate) async fn detach_orphaned_calendar_message_links(&self) -> Result<usize> {
        let referenced_message_ids = self
            .calendar_db
            .read(|conn| {
                let mut statement = conn.prepare(
                    "SELECT DISTINCT message_id FROM calendar_events
                     WHERE message_id IS NOT NULL ORDER BY message_id",
                )?;
                Ok(statement
                    .query_map([], |row| row.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await?;
        if referenced_message_ids.is_empty() {
            return Ok(0);
        }

        // SQLite builds in the supported package set accept at least 999 bind
        // parameters. Stay well below that limit and reuse one mail-actor
        // roundtrip so even a large calendar does not block startup on N
        // cross-thread queries.
        let ids_for_lookup = referenced_message_ids.clone();
        let existing_message_ids = self
            .db
            .read(move |conn| {
                let mut existing = HashSet::new();
                for chunk in ids_for_lookup.chunks(500) {
                    let placeholders = std::iter::repeat_n("?", chunk.len())
                        .collect::<Vec<_>>()
                        .join(",");
                    let sql = format!("SELECT id FROM messages WHERE id IN ({placeholders})");
                    let mut statement = conn.prepare(&sql)?;
                    existing.extend(
                        statement
                            .query_map(rusqlite::params_from_iter(chunk), |row| {
                                row.get::<_, i64>(0)
                            })?
                            .collect::<rusqlite::Result<Vec<_>>>()?,
                    );
                }
                Ok(existing)
            })
            .await?;
        let orphaned = referenced_message_ids
            .into_iter()
            .filter(|message_id| !existing_message_ids.contains(message_id))
            .collect::<Vec<_>>();
        if orphaned.is_empty() {
            return Ok(0);
        }
        let detached = orphaned.len();
        self.calendar_db
            .write(move |conn| {
                let tx = conn.transaction()?;
                for message_id in orphaned {
                    tx.execute(
                        "UPDATE calendar_events SET message_id = NULL WHERE message_id = ?1",
                        rusqlite::params![message_id],
                    )?;
                }
                tx.commit()?;
                Ok(())
            })
            .await?;
        Ok(detached)
    }

    pub(crate) async fn complete_account_removal(&self, account_id: i64) -> Result<()> {
        let _files_guard = self.file_work_lock.lock().await;
        if let Some(h) = self.handles.write().await.remove(&account_id) {
            h.abort();
        }
        self.cal_handles.write().await.remove(&account_id);
        if let Some(handle) = self.card_handles.write().await.remove(&account_id) {
            handle.abort();
        }
        self.tokens.forget_account(account_id).await;
        self.purge_calendar_account(account_id).await?;
        self.files_db
            .write(move |conn| {
                conn.execute("DELETE FROM connections WHERE account_id=?1", [account_id])?;
                Ok(())
            })
            .await?;
        credentials::delete_all_async(self.credentials.clone(), account_id).await?;
        self.db
            .write(move |conn| {
                conn.execute(
                    "DELETE FROM app_settings WHERE key = ?1",
                    [format!("files:{account_id}")],
                )?;
                let mut settings = repo::settings::get(conn)?;
                for profile in &mut settings.mail_profiles {
                    profile.account_ids.retain(|id| *id != account_id);
                }
                let account_key = account_id.to_string();
                settings.account_colors.remove(&account_key);
                settings.account_short_names.remove(&account_key);
                settings.account_themes.remove(&account_key);
                settings.signature_defaults.remove(&account_key);
                settings
                    .signature_list
                    .retain(|signature| signature.account_id != account_id);
                repo::settings::set(conn, &settings)?;
                repo::accounts::delete(conn, account_id)
            })
            .await?;
        for directory in [
            self.paths.mail_dir(account_id),
            self.paths.attachments_dir(account_id),
            self.paths.files_cache_dir(account_id),
            self.paths.files_staging_dir(account_id),
        ] {
            match tokio::fs::remove_dir_all(directory).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(CoreError::Io(error)),
            }
        }
        self.db
            .write(move |conn| {
                conn.execute(
                    "DELETE FROM cross_store_operations
                     WHERE kind = 'remove_account' AND account_id = ?1",
                    rusqlite::params![account_id],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    pub(crate) async fn recover_cross_store_state(&self) -> Result<()> {
        let pending = self
            .db
            .read(|conn| {
                let mut statement = conn.prepare(
                    "SELECT account_id FROM cross_store_operations
                     WHERE kind = 'remove_account' ORDER BY created_at, account_id",
                )?;
                Ok(statement
                    .query_map([], |row| row.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await?;
        for account_id in pending {
            tracing::warn!(account_id, "finishing interrupted account removal");
            self.complete_account_removal(account_id).await?;
        }

        if !self.paths.calendar_db_file().exists() {
            return Ok(());
        }

        let valid_accounts = self
            .db
            .read(|conn| {
                let mut statement = conn.prepare("SELECT id FROM accounts")?;
                Ok(statement
                    .query_map([], |row| row.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<HashSet<_>>>()?)
            })
            .await?;
        let calendar_accounts = self
            .calendar_db
            .read(|conn| {
                let mut statement = conn.prepare(
                    "SELECT account_id FROM calendar_events WHERE account_id != 0
                     UNION SELECT account_id FROM calendars WHERE account_id != 0
                     UNION SELECT account_id FROM caldav_config WHERE account_id != 0
                     UNION SELECT account_id FROM pending_actions WHERE account_id != 0",
                )?;
                Ok(statement
                    .query_map([], |row| row.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await?;
        for account_id in calendar_accounts {
            if !valid_accounts.contains(&account_id) {
                tracing::warn!(account_id, "removing orphaned calendar account data");
                self.purge_calendar_account(account_id).await?;
                self.files_db
                    .write(move |conn| {
                        conn.execute("DELETE FROM connections WHERE account_id=?1", [account_id])?;
                        Ok(())
                    })
                    .await?;
            }
        }
        let detached = self.detach_orphaned_calendar_message_links().await?;
        if detached > 0 {
            tracing::info!(detached, "detached orphaned calendar message links");
        }
        Ok(())
    }

    pub(crate) async fn cleanup_orphaned_draft_files(&self) -> Result<usize> {
        let (stale_paths, referenced_paths) = self
            .db
            .write(|conn| {
                let stale_paths = {
                    let mut statement = conn.prepare(
                        "SELECT da.file_path FROM draft_attachments da
                         JOIN messages m ON m.id = da.draft_id
                         WHERE m.is_draft = 0",
                    )?;
                    statement
                        .query_map([], |row| row.get::<_, String>(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?
                };
                conn.execute(
                    "DELETE FROM draft_attachments
                     WHERE EXISTS (
                         SELECT 1 FROM messages m
                         WHERE m.id = draft_attachments.draft_id AND m.is_draft = 0
                     )",
                    [],
                )?;
                let referenced_paths = {
                    let mut statement = conn.prepare("SELECT file_path FROM draft_attachments")?;
                    statement
                        .query_map([], |row| row.get::<_, String>(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?
                };
                Ok((stale_paths, referenced_paths))
            })
            .await?;

        let root = self.paths.draft_attachments_dir();
        let mut removed = 0usize;
        for path in stale_paths {
            if std::path::Path::new(&path).exists() {
                remove_staged_attachment(&root, &path).await;
                removed += 1;
            }
        }
        let Ok(canonical_root) = tokio::fs::canonicalize(&root).await else {
            return Ok(removed);
        };
        let mut referenced = HashSet::new();
        for path in referenced_paths {
            if let Ok(path) = tokio::fs::canonicalize(path).await
                && path.starts_with(&canonical_root)
            {
                referenced.insert(path);
            }
        }

        let mut directories = tokio::fs::read_dir(&canonical_root).await?;
        while let Some(directory) = directories.next_entry().await? {
            let file_type = directory.file_type().await?;
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let mut entries = tokio::fs::read_dir(directory.path()).await?;
            while let Some(entry) = entries.next_entry().await? {
                let file_type = entry.file_type().await?;
                if !file_type.is_file() && !file_type.is_symlink() {
                    continue;
                }
                let path = entry.path();
                let is_referenced = tokio::fs::canonicalize(&path)
                    .await
                    .is_ok_and(|path| referenced.contains(&path));
                if !is_referenced && tokio::fs::remove_file(path).await.is_ok() {
                    removed += 1;
                }
            }
            let _ = tokio::fs::remove_dir(directory.path()).await;
        }
        Ok(removed)
    }

    pub async fn remove_account(&self, account_id: i64) -> Result<()> {
        let created_at = chrono::Utc::now().timestamp_millis();
        self.db
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO cross_store_operations (kind, account_id, created_at)
                     VALUES ('remove_account', ?1, ?2)
                     ON CONFLICT(kind, account_id) DO NOTHING",
                    rusqlite::params![account_id, created_at],
                )?;
                Ok(())
            })
            .await?;
        self.complete_account_removal(account_id).await
    }

    /// Remove every user-owned record and secret while keeping the live
    /// database connections usable. This is the in-app equivalent of a fresh
    /// profile; it deliberately does not touch anything on provider servers.
    pub async fn delete_all_local_data(&self) -> Result<()> {
        let account_ids = self
            .db
            .read(|conn| {
                let mut statement = conn.prepare("SELECT id FROM accounts ORDER BY id")?;
                Ok(statement
                    .query_map([], |row| row.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await?;

        for account_id in account_ids {
            self.remove_account(account_id).await?;
        }

        // Account cascades remove mail/folders/actions. These tables also hold
        // profile-level data, so clear them explicitly for a genuine reset.
        self.db
            .write(|conn| {
                conn.execute_batch(
                    "DELETE FROM contacts;
                     DELETE FROM snippets;
                     DELETE FROM split_rules;
                     DELETE FROM route_cache;
                     DELETE FROM ai_usage_events;
                     DELETE FROM app_settings;",
                )?;
                Ok(())
            })
            .await?;
        self.calendar_db
            .write(|conn| {
                conn.execute_batch(
                    "DELETE FROM pending_actions;
                     DELETE FROM calendar_events;
                     DELETE FROM calendars;
                     DELETE FROM caldav_config;",
                )?;
                Ok(())
            })
            .await?;

        self.files_db
            .write(|c| {
                c.execute("DELETE FROM connections", [])?;
                Ok(())
            })
            .await?;
        // Account id 0 owns app-level secrets such as the optional AI key.
        credentials::delete_all_async(self.credentials.clone(), 0).await?;
        let _ = tokio::fs::remove_dir_all(self.paths.draft_attachments_dir()).await;
        let _ = tokio::fs::remove_dir_all(self.paths.models_dir()).await;
        self.bus.emit(CoreEvent::MailUpdated { thread_ids: vec![] });
        self.bus.emit(CoreEvent::CalendarUpdated { account_id: 0 });
        Ok(())
    }

    /// Export coherent, integrity-checked copies of all three SQLite stores plus a
    /// versioned manifest. Credentials remain in the platform keyring and are
    /// intentionally outside this database snapshot.
    pub async fn create_database_snapshot(
        &self,
        destination: std::path::PathBuf,
    ) -> Result<DatabaseSnapshotManifest> {
        let parent = destination.parent().ok_or_else(|| {
            CoreError::Other("database snapshot destination has no parent directory".into())
        })?;
        let canonical_parent = tokio::fs::canonicalize(parent).await.map_err(|error| {
            CoreError::Other(format!(
                "resolving snapshot destination {}: {error}",
                parent.display()
            ))
        })?;
        let data_dir = tokio::fs::canonicalize(&self.paths.data_dir).await?;
        let cache_dir = tokio::fs::canonicalize(&self.paths.cache_dir).await?;
        if canonical_parent.starts_with(&data_dir) || canonical_parent.starts_with(&cache_dir) {
            return Err(CoreError::Other(
                "database snapshots must be exported outside the application data and cache directories"
                    .into(),
            ));
        }
        let _files_guard = self.file_work_lock.lock().await;
        db::snapshot::create(
            &self.db,
            &self.calendar_db,
            &self.files_db,
            Some(self.paths.data_dir.join("file_transfers")),
            &destination,
        )
        .await
    }
}
