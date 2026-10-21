//! `Core` methods: Account listing, import, configuration and connection tests.
//! Moved verbatim from `lib.rs`; see docs/fork.md.

use crate::*;

impl Core {
    pub async fn list_accounts(&self) -> Result<Vec<Account>> {
        self.db.read(|conn| repo::accounts::list(conn)).await
    }

    /// Native settings surfaces need the editable server fields alongside the
    /// public account summary. Credentials remain in the OS keyring and are
    /// deliberately not included.
    pub async fn list_account_configs(&self) -> Result<Vec<AccountConfig>> {
        self.db
            .read(|conn| repo::accounts::list_configs(conn))
            .await
    }

    /// Import non-secret account metadata from a portable backup. Existing
    /// addresses are preserved, and imported accounts remain paused until the
    /// user authenticates on this device.
    pub async fn import_account_configs(
        &self,
        configs: Vec<PortableAccountConfig>,
    ) -> Result<usize> {
        for config in &configs {
            validate_portable_account(config)?;
        }

        self.db
            .write(move |conn| {
                let transaction = conn.transaction()?;
                let mut imported = 0usize;
                for config in &configs {
                    if repo::accounts::find_by_email(&transaction, config.email.trim())?.is_some() {
                        continue;
                    }
                    let account_id = repo::accounts::insert(
                        &transaction,
                        &repo::accounts::NewAccount {
                            email: config.email.trim(),
                            display_name: config.display_name.as_deref(),
                            avatar_url: None,
                            provider: config.provider,
                            auth_kind: config.auth_kind,
                            mail_protocol: config.mail_protocol,
                            username: config.username.trim(),
                            jmap_url: config.jmap_url.trim(),
                            jmap_account_id: None,
                            imap_host: config.imap_host.trim(),
                            imap_port: config.imap_port,
                            smtp_host: config.smtp_host.trim(),
                            smtp_port: config.smtp_port,
                        },
                    )?;
                    repo::accounts::set_settings(&transaction, account_id, &config.settings)?;
                    repo::accounts::set_sync_state(&transaction, account_id, "needs_reauth")?;
                    imported += 1;
                }
                transaction.commit()?;
                Ok(imported)
            })
            .await
    }

    pub async fn reorder_account(&self, source_id: i64, target_id: i64, after: bool) -> Result<()> {
        self.db
            .write(move |conn| repo::accounts::reorder(conn, source_id, target_id, after))
            .await?;
        // Account order also determines the account/folder sidebar order.
        // An empty MailUpdated is the core's existing global-view invalidation
        // signal and lets every host refresh that structure immediately.
        self.bus.emit(CoreEvent::MailUpdated { thread_ids: vec![] });
        Ok(())
    }

    /// Change one account's background history download boundary. Provider
    /// checkpoints are reopened, but existing local messages are retained.
    pub async fn set_account_mail_history(
        &self,
        account_id: i64,
        mail_history: MailHistory,
    ) -> Result<()> {
        let updated = self
            .db
            .write(move |conn| {
                let transaction = conn.transaction()?;
                let mut config = repo::accounts::get_config(&transaction, account_id)?
                    .ok_or_else(|| CoreError::NotFound(format!("account {account_id}")))?;
                if config.settings.mail_history == mail_history {
                    transaction.commit()?;
                    return Ok(None);
                }

                config.settings.mail_history = mail_history;
                repo::accounts::set_settings(&transaction, account_id, &config.settings)?;
                match (config.provider, config.mail_protocol) {
                    (Provider::Gmail, _) => repo::gmail::expire_history(&transaction, account_id)?,
                    (_, MailProtocol::Jmap) => {
                        transaction.execute(
                            "UPDATE jmap_sync_state SET email_state = NULL WHERE account_id = ?1",
                            rusqlite::params![account_id],
                        )?;
                    }
                    (Provider::Imap | Provider::Microsoft, MailProtocol::Imap) => {
                        repo::folders::reopen_backfill_for_account(&transaction, account_id)?;
                    }
                }
                transaction.commit()?;
                Ok(Some(config.settings))
            })
            .await?;

        if let Some(settings) = updated {
            if let Some(handle) = self.handles.read().await.get(&account_id).cloned() {
                handle.update_settings(settings);
            }
            self.bus.emit(CoreEvent::MailUpdated { thread_ids: vec![] });
        }
        Ok(())
    }

    /// Override SMTP Sent filing for providers with configurable auto-save.
    pub async fn set_account_sent_copy_policy(
        &self,
        account_id: i64,
        policy: SentCopyPolicy,
    ) -> Result<()> {
        let settings = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                let mut config = repo::accounts::get_config(&tx, account_id)?
                    .ok_or_else(|| CoreError::NotFound(format!("account {account_id}")))?;
                config.settings.sent_copy_policy = policy;
                repo::accounts::set_settings(&tx, account_id, &config.settings)?;
                tx.commit()?;
                Ok(config.settings)
            })
            .await?;
        if let Some(handle) = self.handles.read().await.get(&account_id).cloned() {
            handle.update_settings(settings);
        }
        self.bus.emit(CoreEvent::MailUpdated { thread_ids: vec![] });
        Ok(())
    }

    pub async fn test_connection(&self, args: &AddPasswordAccountArgs) -> ConnectionTestResult {
        let mut args = args.clone();
        args.password = normalize_account_password(args.password);
        if args.mail_protocol == MailProtocol::Jmap {
            return match crate::jmap::client::connect_with(
                &args.email,
                &args.username,
                &args.password,
                &args.jmap_url,
                None,
            )
            .await
            {
                Ok(_) => ConnectionTestResult {
                    ok: true,
                    error: None,
                },
                Err(error) => ConnectionTestResult {
                    ok: false,
                    error: Some(error.to_client_json()),
                },
            };
        }
        match self.check_imap_smtp(&args).await {
            Ok(()) => ConnectionTestResult {
                ok: true,
                error: None,
            },
            Err(e) => ConnectionTestResult {
                ok: false,
                error: Some(e.to_string()),
            },
        }
    }

    pub(crate) async fn check_imap_smtp(&self, args: &AddPasswordAccountArgs) -> Result<()> {
        let creds = imap::ImapCredentials::Password {
            user: args.username.clone(),
            password: args.password.clone(),
        };
        let mut session =
            imap::connect_with_settings(&args.imap_host, args.imap_port, creds, &args.connection)
                .await?;
        let mailbox_result = imap::select(&mut session, "INBOX").await.map(|_| ());
        imap::logout(session).await;
        mailbox_result?;
        let config = AccountConfig {
            id: 0,
            email: args.email.clone(),
            display_name: None,
            avatar_url: None,
            provider: Provider::Imap,
            auth_kind: AuthKind::Password,
            mail_protocol: MailProtocol::Imap,
            username: args.username.clone(),
            jmap_url: String::new(),
            jmap_account_id: None,
            imap_host: args.imap_host.clone(),
            imap_port: args.imap_port,
            smtp_host: args.smtp_host.clone(),
            smtp_port: args.smtp_port,
            settings: AccountSettings {
                connection: args.connection.clone(),
                ..Default::default()
            },
        };
        smtp::test_connection(&config, &smtp::SmtpAuth::Password(args.password.clone())).await
    }

    pub async fn add_account_password(&self, args: AddPasswordAccountArgs) -> Result<Account> {
        imap::trusted_certificates(&args.connection.trusted_certificate_pem)?;
        let email = normalize_account_email(&args.email)
            .ok_or_else(|| CoreError::Auth("enter a valid email address".into()))?;
        let mut args = AddPasswordAccountArgs {
            email,
            display_name: args
                .display_name
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty()),
            username: args.username.trim().to_owned(),
            password: normalize_account_password(args.password),
            mail_protocol: args.mail_protocol,
            jmap_url: args.jmap_url.trim().to_owned(),
            imap_host: args.imap_host.trim().to_owned(),
            imap_port: args.imap_port,
            smtp_host: args.smtp_host.trim().to_owned(),
            smtp_port: args.smtp_port,
            connection: args.connection,
        };
        if args.username.is_empty() {
            args.username = args.email.clone();
        }
        if args.password.is_empty() {
            return Err(CoreError::Auth("email and password are required".into()));
        }
        if args.username.len() > 512
            || args.jmap_url.len() > 2048
            || args.imap_host.len() > 253
            || args.smtp_host.len() > 253
        {
            return Err(CoreError::Auth("an account field is too long".into()));
        }
        if args.mail_protocol == MailProtocol::Imap
            && (args.imap_host.is_empty()
                || args.smtp_host.is_empty()
                || args.imap_port == 0
                || args.smtp_port == 0)
        {
            return Err(CoreError::Auth(
                "IMAP and SMTP server hosts and ports are required".into(),
            ));
        }
        let args = if args.mail_protocol == MailProtocol::Jmap {
            AddPasswordAccountArgs {
                jmap_url: crate::jmap::client::normalize_base_url(&args.jmap_url, &args.email)?,
                ..args
            }
        } else {
            args
        };

        credentials::check_available_async(self.credentials.clone()).await?;
        self.recover_incomplete_account_credentials().await?;

        // Verify credentials before storing anything. For JMAP, also pin the
        // exact Mail account selected from the authenticated Session so a
        // later reconnect can never drift to a different shared account.
        let jmap_account_id = if args.mail_protocol == MailProtocol::Jmap {
            Some(
                crate::jmap::client::connect_with(
                    &args.email,
                    &args.username,
                    &args.password,
                    &args.jmap_url,
                    None,
                )
                .await?
                .account_id,
            )
        } else {
            self.check_imap_smtp(&args).await?;
            None
        };

        let existing = self
            .db
            .read({
                let email = args.email.clone();
                move |conn| repo::accounts::find_by_email(conn, &email)
            })
            .await?;
        let is_new_account = existing.is_none();

        let id = if let Some(existing) = existing {
            if existing.provider != Provider::Imap || existing.auth_kind != AuthKind::Password {
                return Err(CoreError::Auth(format!(
                    "{} is already connected through {}; reconnect it through that provider",
                    existing.email,
                    existing.provider.as_str()
                )));
            }

            // Stop every worker before changing the persisted remote identity.
            // Otherwise a request already in flight against the old server
            // could commit its result after the reset transaction below.
            if let Some(handle) = self.handles.write().await.remove(&existing.id) {
                handle.abort();
            }

            let a = args.clone();
            let remote_account = jmap_account_id.clone();
            let id = existing.id;
            let update_result = self
                .db
                .write(move |conn| {
                    let transaction = conn.transaction()?;
                    repo::accounts::update_password(
                        &transaction,
                        id,
                        &repo::accounts::NewAccount {
                            email: &a.email,
                            display_name: a.display_name.as_deref(),
                            avatar_url: None,
                            provider: Provider::Imap,
                            auth_kind: AuthKind::Password,
                            mail_protocol: a.mail_protocol,
                            username: &a.username,
                            jmap_url: &a.jmap_url,
                            jmap_account_id: remote_account.as_deref(),
                            imap_host: &a.imap_host,
                            imap_port: a.imap_port,
                            smtp_host: &a.smtp_host,
                            smtp_port: a.smtp_port,
                        },
                    )?;
                    let mut settings = repo::accounts::get_config(&transaction, id)?
                        .ok_or_else(|| CoreError::NotFound("account".into()))?
                        .settings;
                    settings.connection = a.connection.clone();
                    repo::accounts::set_settings(&transaction, id, &settings)?;
                    transaction.commit()?;
                    Ok(())
                })
                .await;
            if let Err(error) = update_result {
                if let Ok(Some(cfg)) = self
                    .db
                    .read(move |conn| repo::accounts::get_config(conn, id))
                    .await
                {
                    self.spawn_actor(cfg).await;
                }
                return Err(error);
            }

            id
        } else {
            let a = args.clone();
            let remote_account = jmap_account_id.clone();
            self.db
                .write(move |conn| {
                    let transaction = conn.transaction()?;
                    let id = repo::accounts::insert_with_sync_state(
                        &transaction,
                        &repo::accounts::NewAccount {
                            email: &a.email,
                            display_name: a.display_name.as_deref(),
                            avatar_url: None,
                            provider: Provider::Imap,
                            auth_kind: AuthKind::Password,
                            mail_protocol: a.mail_protocol,
                            username: &a.username,
                            jmap_url: &a.jmap_url,
                            jmap_account_id: remote_account.as_deref(),
                            imap_host: &a.imap_host,
                            imap_port: a.imap_port,
                            smtp_host: &a.smtp_host,
                            smtp_port: a.smtp_port,
                        },
                        repo::accounts::CREDENTIAL_SETUP_STATE,
                    )?;
                    let settings = AccountSettings {
                        connection: a.connection.clone(),
                        ..Default::default()
                    };
                    repo::accounts::set_settings(&transaction, id, &settings)?;
                    transaction.commit()?;
                    Ok(id)
                })
                .await?
        };

        if let Err(error) = credentials::store_async(
            self.credentials.clone(),
            id,
            Slot::Password,
            args.password.clone(),
        )
        .await
        {
            // A newly inserted account without its password can never sync.
            // Roll it back atomically from the user's point of view.
            if is_new_account {
                self.rollback_incomplete_account_credentials(id).await;
            } else if let Ok(Some(cfg)) = self
                .db
                .read(move |conn| repo::accounts::get_config(conn, id))
                .await
            {
                // Reconnect editing stops the previous workers before the
                // identity transaction. If keyring persistence then fails,
                // restore an actor so the account visibly enters its normal
                // authentication error state instead of remaining inert until
                // the next app restart.
                self.spawn_actor(cfg).await;
            }
            return Err(error);
        }

        if is_new_account
            && let Err(error) = self
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

    /// Abort any sign-in currently waiting on the browser redirect.
    pub fn cancel_oauth(&self) {
        tracing::info!("oauth: sign-in cancelled by user");
        self.oauth_cancel.notify_waiters();
    }
}
