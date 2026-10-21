//! `Core` methods: Contacts and CardDAV.
//! Moved verbatim from `lib.rs`; see docs/fork.md.

use crate::*;

impl Core {
    /// Compose "To" autocomplete. `account_id` Some scopes suggestions to the
    /// sending account's own contacts; None returns contacts across all accounts
    /// (the "show contacts from all accounts" setting).
    pub async fn list_contacts(
        &self,
        prefix: String,
        account_id: Option<i64>,
        limit: i64,
    ) -> Result<Vec<Address>> {
        self.db
            .read(move |conn| {
                let settings = repo::settings::get(conn)?;
                let account_id = if settings.contact_suggest_all_accounts {
                    None
                } else {
                    account_id
                };
                repo::contacts::autocomplete(
                    conn,
                    &prefix,
                    account_id,
                    settings.suggest_learned_contacts,
                    limit.clamp(1, 50),
                )
            })
            .await
    }

    /// Contact suggestions for the search screen: every query token must match
    /// (accent-insensitive), ranked by how much the user emails that contact.
    /// Search operators are stripped first so "from:x be" still suggests on "be".
    pub async fn suggest_contacts(
        &self,
        query: String,
        limit: i64,
    ) -> Result<Vec<ContactSuggestion>> {
        let text = search::parse(&query).text;
        self.db
            .read(move |conn| {
                let settings = repo::settings::get(conn)?;
                repo::contacts::suggest(
                    conn,
                    &text,
                    None,
                    settings.suggest_learned_contacts,
                    limit.clamp(1, 20),
                )
            })
            .await
    }

    /// Full records for the contacts workspace (including an empty-query list).
    pub async fn list_contact_records(
        &self,
        query: String,
        limit: i64,
    ) -> Result<Vec<ContactRecord>> {
        self.db
            .read(move |conn| repo::contacts::list_records(conn, &query, limit))
            .await
    }

    /// One strict page for the dedicated contacts workspace. Filtering lives
    /// in SQLite so infinite scrolling never needs to preload the directory.
    pub async fn list_contact_record_page(
        &self,
        query: String,
        account_id: Option<i64>,
        favorites_only: bool,
        suggestions_only: bool,
        cursor: Option<ContactRecordCursor>,
        limit: i64,
    ) -> Result<ContactRecordPage> {
        self.db
            .read(move |conn| {
                repo::contacts::list_record_page(
                    conn,
                    &query,
                    account_id,
                    favorites_only,
                    suggestions_only,
                    cursor.as_ref(),
                    limit.clamp(1, 100),
                )
            })
            .await
    }

    /// Clear local mail-derived people data without touching manual or
    /// CardDAV-backed contacts.
    pub async fn clear_contact_suggestions(&self) -> Result<usize> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let removed = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                let removed = repo::contacts::clear_suggestions(&tx, now_ms)?;
                tx.commit()?;
                Ok(removed)
            })
            .await?;
        self.bus.emit(CoreEvent::ContactsUpdated { account_id: 0 });
        Ok(removed)
    }

    pub async fn save_contact(&self, record: ContactRecord) -> Result<ContactRecord> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let account_ids = record.account_ids.clone();
        let was_new = record.id <= 0;
        let saved = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                let was_saved = if was_new {
                    false
                } else {
                    repo::contacts::get_record(&tx, record.id)?
                        .is_some_and(|existing| existing.is_managed)
                };
                let saved = repo::contacts::save_record(&tx, &record, now_ms)?;
                if was_new || !was_saved {
                    if account_ids.len() == 1 {
                        repo::carddav::attach_new_contact(&tx, account_ids[0], saved.id)?;
                    }
                } else {
                    repo::carddav::mark_saved_contact_dirty(&tx, saved.id)?;
                }
                tx.commit()?;
                Ok(saved)
            })
            .await?;
        self.nudge_card(None).await;
        Ok(saved)
    }

    pub async fn delete_contact(&self, id: i64) -> Result<()> {
        self.db
            .write(move |conn| {
                let tx = conn.transaction()?;
                repo::carddav::mark_deleted_contact(&tx, id)?;
                repo::contacts::delete_record(&tx, id)?;
                tx.commit()?;
                Ok(())
            })
            .await?;
        self.nudge_card(None).await;
        Ok(())
    }

    /// Connect an RFC 6352 CardDAV service. Discovery is also the connection
    /// test, so neither configuration nor credentials survive a failed probe.
    pub async fn connect_carddav(&self, args: ConnectCardDavArgs) -> Result<Vec<AddressBook>> {
        let account_id = args.account_id;
        if self
            .db
            .read(move |conn| repo::accounts::get(conn, account_id))
            .await?
            .is_none()
        {
            return Err(CoreError::NotFound(format!("account {account_id}")));
        }
        let mut base_url = args.url.trim().to_owned();
        if base_url.is_empty() {
            return Err(CoreError::CardDav("server URL is required".into()));
        }
        if !base_url.contains("://") {
            base_url = format!("https://{base_url}");
        }
        if args.password.is_empty() {
            return Err(CoreError::CardDav("app password is required".into()));
        }
        credentials::check_available_async(self.credentials.clone()).await?;
        let transport = carddav::http::HttpTransport::new(
            args.username.clone(),
            args.password.clone(),
            &base_url,
        )?;
        let discovered = carddav::discovery::discover(&transport, &base_url).await?;

        let previous_password =
            credentials::load_async(self.credentials.clone(), account_id, Slot::CarddavPassword)
                .await
                .ok();
        credentials::store_async(
            self.credentials.clone(),
            account_id,
            Slot::CarddavPassword,
            args.password,
        )
        .await?;
        let config = repo::carddav::CardDavConfig {
            account_id,
            base_url,
            username: args.username,
            principal_url: discovered.principal_url,
            home_set_url: discovered.home_set_url,
            enabled: true,
            last_error: None,
        };
        let books = discovered.addressbooks;
        let persisted = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                repo::carddav::upsert_config(&tx, &config)?;
                let mut first = None;
                let urls = books
                    .iter()
                    .map(|book| book.url.clone())
                    .collect::<HashSet<_>>();
                for book in books {
                    let id = repo::carddav::upsert_addressbook(
                        &tx,
                        account_id,
                        &book.url,
                        book.display_name.as_deref(),
                        book.read_only,
                    )?;
                    first.get_or_insert(id);
                }
                repo::carddav::retain_addressbooks(&tx, account_id, &urls)?;
                if let Some(id) = first {
                    repo::carddav::ensure_default(&tx, account_id, id)?;
                }
                let result = repo::carddav::list_addressbooks(&tx, Some(account_id))?;
                tx.commit()?;
                Ok(result)
            })
            .await;
        let books = match persisted {
            Ok(books) => books,
            Err(error) => {
                if let Some(password) = previous_password {
                    let _ = credentials::store_async(
                        self.credentials.clone(),
                        account_id,
                        Slot::CarddavPassword,
                        password,
                    )
                    .await;
                } else {
                    let _ = credentials::delete_async(
                        self.credentials.clone(),
                        account_id,
                        Slot::CarddavPassword,
                    )
                    .await;
                }
                return Err(error);
            }
        };
        self.spawn_card_task(account_id).await;
        self.nudge_card(Some(account_id)).await;
        Ok(books)
    }

    pub async fn list_carddav_connections(&self) -> Result<Vec<CardDavConnection>> {
        self.db
            .read(|conn| repo::carddav::list_connections(conn))
            .await
    }

    pub async fn list_addressbooks(&self, account_id: Option<i64>) -> Result<Vec<AddressBook>> {
        self.db
            .read(move |conn| repo::carddav::list_addressbooks(conn, account_id))
            .await
    }

    pub async fn set_account_carddav_enabled(&self, account_id: i64, enabled: bool) -> Result<()> {
        let found = self
            .db
            .write(move |conn| repo::carddav::set_enabled(conn, account_id, enabled))
            .await?;
        if !found {
            return Err(CoreError::NotFound("CardDAV connection".into()));
        }
        if enabled {
            if !self.card_handles.read().await.contains_key(&account_id) {
                self.spawn_card_task(account_id).await;
            }
            self.nudge_card(Some(account_id)).await;
        } else {
            if let Some(handle) = self.card_handles.write().await.remove(&account_id) {
                handle.abort();
            }
        }
        Ok(())
    }

    pub async fn disconnect_carddav(&self, account_id: i64) -> Result<()> {
        if let Some(handle) = self.card_handles.write().await.remove(&account_id) {
            handle.abort();
        }
        self.db
            .write(move |conn| {
                let tx = conn.transaction()?;
                repo::carddav::disconnect(&tx, account_id)?;
                tx.commit()?;
                Ok(())
            })
            .await?;
        let _ =
            credentials::delete_async(self.credentials.clone(), account_id, Slot::CarddavPassword)
                .await;
        self.bus.emit(CoreEvent::ContactsUpdated { account_id });
        Ok(())
    }
}
