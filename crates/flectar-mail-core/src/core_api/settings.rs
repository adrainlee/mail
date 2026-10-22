//! `Core` methods: Counts, settings and mail profiles.
//! Moved verbatim from `lib.rs`; see docs/fork.md.

use crate::*;

impl Core {
    /// Exact unread counts for every split tab and sidebar row in one call.
    pub async fn unread_counts(&self, account_id: Option<i64>) -> Result<UnreadCounts> {
        self.db
            .read(move |conn| {
                let splits = repo::splits::list(conn)?;
                let labels = repo::labels::list(conn)?;
                repo::counts::unread_counts(conn, account_id, &splits, &labels)
            })
            .await
    }

    /// Native sidebar badges for all accounts, computed in one grouped query.
    pub async fn mailbox_badge_counts(&self) -> Result<Vec<MailboxBadgeCounts>> {
        self.db
            .read(|conn| repo::counts::mailbox_badge_counts(conn))
            .await
    }

    pub async fn get_settings(&self) -> Result<Settings> {
        self.db.read(|conn| repo::settings::get(conn)).await
    }

    pub async fn set_settings(&self, settings: Settings) -> Result<()> {
        let oauth_settings = settings.clone();
        let now_ms = chrono::Utc::now().timestamp_millis();
        self.db
            .write(move |conn| {
                let tx = conn.transaction()?;
                let previous = repo::settings::get(&tx)?;
                // Allow unrelated settings to be saved even if an older build
                // persisted invalid OAuth keys; validate any newly supplied ID.
                if settings.google_client_id != previous.google_client_id {
                    oauth::providers::validate_custom_client_id(
                        Provider::Gmail,
                        &settings.google_client_id,
                    )?;
                }
                if settings.ms_client_id != previous.ms_client_id {
                    oauth::providers::validate_custom_client_id(
                        Provider::Microsoft,
                        &settings.ms_client_id,
                    )?;
                }
                repo::contacts::advance_learning_boundaries(
                    &tx,
                    !previous.collect_outgoing_contacts && settings.collect_outgoing_contacts,
                    !previous.collect_incoming_contacts && settings.collect_incoming_contacts,
                    now_ms,
                )?;
                repo::settings::set(&tx, &settings)?;
                tx.commit()?;
                Ok(())
            })
            .await?;
        // Keep the resolver in the same order as persisted writes.
        apply_oauth_settings(&oauth_settings);
        Ok(())
    }

    /// Create or update a local mail profile without touching provider state.
    /// The settings read and write share the database writer so an unrelated
    /// preference cannot be lost to a stale read/modify/write cycle.
    pub async fn save_mail_profile(
        &self,
        profile_id: Option<String>,
        name: String,
        color: String,
    ) -> Result<(Settings, MailProfile)> {
        let name = normalize_mail_profile_name(&name)?;
        let color = normalize_mail_profile_color(&color)?;
        self.db
            .write(move |conn| {
                let mut settings = repo::settings::get(conn)?;
                let requested_id = profile_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|id| !id.is_empty());
                let duplicate = settings.mail_profiles.iter().any(|profile| {
                    Some(profile.id.as_str()) != requested_id
                        && profile.name.eq_ignore_ascii_case(&name)
                });
                if duplicate {
                    return Err(CoreError::Other(format!(
                        "a profile named {name:?} already exists"
                    )));
                }

                let profile = if let Some(id) = requested_id {
                    let profile = settings
                        .mail_profiles
                        .iter_mut()
                        .find(|profile| profile.id == id)
                        .ok_or_else(|| CoreError::NotFound(format!("mail profile {id}")))?;
                    profile.name = name;
                    profile.color = color;
                    profile.clone()
                } else {
                    if settings.mail_profiles.len() >= 50 {
                        return Err(CoreError::Other(
                            "no more than 50 mail profiles can be created".into(),
                        ));
                    }
                    let base_id = format!(
                        "profile-{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_micros()
                    );
                    let mut id = base_id.clone();
                    let mut suffix = 2u32;
                    while settings
                        .mail_profiles
                        .iter()
                        .any(|profile| profile.id == id)
                    {
                        id = format!("{base_id}-{suffix}");
                        suffix += 1;
                    }
                    let profile = MailProfile {
                        id,
                        name,
                        color,
                        account_ids: Vec::new(),
                    };
                    settings.mail_profiles.push(profile.clone());
                    profile
                };
                repo::settings::set(conn, &settings)?;
                Ok((settings, profile))
            })
            .await
    }

    /// Assign an account to one profile, or remove its profile assignment.
    pub async fn assign_account_profile(
        &self,
        account_id: i64,
        profile_id: Option<String>,
    ) -> Result<Settings> {
        self.db
            .write(move |conn| {
                if repo::accounts::get(conn, account_id)?.is_none() {
                    return Err(CoreError::NotFound(format!("account {account_id}")));
                }
                let mut settings = repo::settings::get(conn)?;
                let profile_id = profile_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|id| !id.is_empty());
                if let Some(id) = profile_id
                    && !settings
                        .mail_profiles
                        .iter()
                        .any(|profile| profile.id == id)
                {
                    return Err(CoreError::NotFound(format!("mail profile {id}")));
                }
                for profile in &mut settings.mail_profiles {
                    profile.account_ids.retain(|id| *id != account_id);
                }
                if let Some(id) = profile_id {
                    let profile = settings
                        .mail_profiles
                        .iter_mut()
                        .find(|profile| profile.id == id)
                        .expect("profile existence checked above");
                    profile.account_ids.push(account_id);
                    profile.account_ids.sort_unstable();
                }
                repo::settings::set(conn, &settings)?;
                Ok(settings)
            })
            .await
    }

    /// Set an account-specific marker color, or restore its generated color.
    /// Choosing an individual color also leaves any profile in the same
    /// settings write so the visible color has one unambiguous owner.
    pub async fn set_account_color(
        &self,
        account_id: i64,
        color: Option<String>,
    ) -> Result<Settings> {
        let color = color
            .map(|color| normalize_mail_profile_color(&color))
            .transpose()?;
        self.db
            .write(move |conn| {
                if repo::accounts::get(conn, account_id)?.is_none() {
                    return Err(CoreError::NotFound(format!("account {account_id}")));
                }
                let mut settings = repo::settings::get(conn)?;
                for profile in &mut settings.mail_profiles {
                    profile.account_ids.retain(|id| *id != account_id);
                }
                let key = account_id.to_string();
                if let Some(color) = color {
                    settings.account_colors.insert(key, color);
                } else {
                    settings.account_colors.remove(&key);
                }
                repo::settings::set(conn, &settings)?;
                Ok(settings)
            })
            .await
    }

    /// Delete one local profile. Connected accounts and provider data remain.
    pub async fn delete_mail_profile(&self, profile_id: String) -> Result<Settings> {
        let profile_id = profile_id.trim().to_owned();
        self.db
            .write(move |conn| {
                let mut settings = repo::settings::get(conn)?;
                let before = settings.mail_profiles.len();
                settings
                    .mail_profiles
                    .retain(|profile| profile.id != profile_id);
                if settings.mail_profiles.len() == before {
                    return Err(CoreError::NotFound(format!("mail profile {profile_id}")));
                }
                repo::settings::set(conn, &settings)?;
                Ok(settings)
            })
            .await
    }

    pub async fn set_show_account_badges(&self, enabled: bool) -> Result<Settings> {
        self.db
            .write(move |conn| {
                let mut settings = repo::settings::get(conn)?;
                settings.show_account_badges = enabled;
                repo::settings::set(conn, &settings)?;
                Ok(settings)
            })
            .await
    }

    pub async fn set_group_mail_by_date(&self, enabled: bool) -> Result<()> {
        self.db
            .write(move |conn| {
                let mut settings = repo::settings::get(conn)?;
                settings.group_mail_by_date = enabled;
                repo::settings::set(conn, &settings)
            })
            .await
    }

    /// Persist one or both OAuth registrations in a single settings write.
    /// Read on the writer thread so unrelated preferences aren't overwritten
    /// by a stale settings snapshot while this operation waits for the DB.
    pub async fn set_oauth_apps(
        &self,
        google: Option<(String, String)>,
        microsoft: Option<String>,
    ) -> Result<Settings> {
        // Reject a bad registration before either provider is written. Empty
        // IDs remain the explicit way to clear a custom registration.
        if let Some((id, _)) = &google {
            oauth::providers::validate_custom_client_id(Provider::Gmail, id)?;
        }
        if let Some(id) = &microsoft {
            oauth::providers::validate_custom_client_id(Provider::Microsoft, id)?;
        }
        self.db
            .write(move |conn| {
                let mut settings = repo::settings::get(conn)?;
                if let Some((id, secret)) = google {
                    settings.google_client_id = id.trim().to_owned();
                    settings.google_client_secret = if settings.google_client_id.is_empty() {
                        String::new()
                    } else {
                        secret.trim().to_owned()
                    };
                }
                if let Some(id) = microsoft {
                    settings.ms_client_id = id.trim().to_owned();
                    // Public desktop clients use PKCE, without a secret.
                    settings.ms_client_secret.clear();
                }
                repo::settings::set(conn, &settings)?;
                apply_oauth_settings(&settings);
                Ok(settings)
            })
            .await
    }

    pub async fn set_sync_interval_minutes(&self, minutes: i64) -> Result<()> {
        if !matches!(minutes, 1 | 5 | 15) {
            return Err(CoreError::Other(format!(
                "unsupported sync interval: {minutes} minutes"
            )));
        }
        let mut settings = self.get_settings().await?;
        settings.sync_interval_minutes = minutes;
        self.set_settings(settings).await?;

        // Wake every live provider so the newly persisted cadence takes effect
        // now instead of after its previous timeout expires.
        for handle in self.handles.read().await.values() {
            handle.send(SyncCmd::SyncNow { complete: None });
        }
        for handle in self.cal_handles.read().await.values() {
            handle.nudge();
        }
        for handle in self.card_handles.read().await.values() {
            handle.nudge();
        }
        Ok(())
    }
}
