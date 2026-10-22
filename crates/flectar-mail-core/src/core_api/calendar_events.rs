//! `Core` methods: Calendar events and calendar connections.
//! Moved verbatim from `lib.rs`; see docs/fork.md.

use crate::*;

impl Core {
    pub async fn list_events(&self, start_ms: i64, end_ms: i64) -> Result<Vec<CalendarEvent>> {
        let (mut events, masters) = self
            .calendar_db
            .read(move |conn| {
                Ok((
                    repo::calendar::list_range(conn, start_ms, end_ms)?,
                    repo::calendar::recurring_masters(conn, end_ms)?,
                ))
            })
            .await?;

        // Expand recurring series into concrete occurrences. Occurrences keep
        // the master's row id (edits/deletes address the whole series in v1);
        // unsupported rules fall back to the master row alone.
        for m in masters {
            let Some(rrule) = m.event.rrule.clone() else {
                continue;
            };
            let duration = m
                .event
                .ends_at
                .map(|e| e - m.event.starts_at)
                .unwrap_or(1_800_000);
            let Some(occs) = caldav::rrule::expand(
                &rrule,
                m.event.starts_at,
                duration,
                m.event.all_day,
                m.ical_raw.as_deref(),
                start_ms,
                end_ms,
            ) else {
                continue; // unsupported rule: the master entry stands alone
            };
            events.retain(|e| e.id != m.event.id);
            for occ in occs {
                let mut e = m.event.clone();
                e.starts_at = occ.start;
                e.ends_at = Some(occ.end);
                events.push(e);
            }
        }
        events.sort_by_key(|e| e.starts_at);
        Ok(events)
    }

    /// Invite events carried by one message (the thread invite card).
    pub async fn events_for_message(&self, message_id: i64) -> Result<Vec<CalendarEvent>> {
        self.calendar_db
            .read(move |conn| repo::calendar::for_message(conn, message_id))
            .await
    }

    /// Create a meeting. The event lands on the local calendar immediately;
    /// if it has attendees, an invite email with an ICS (METHOD:REQUEST) is
    /// drafted and queued through the normal send pipeline (undo window,
    /// offline queueing, sent-folder append all apply).
    pub async fn create_event(&self, args: CreateEventArgs) -> Result<CalendarEvent> {
        if args.summary.trim().is_empty() {
            return Err(CoreError::Other("event needs a title".into()));
        }
        if args.ends_at <= args.starts_at {
            return Err(CoreError::Other("event must end after it starts".into()));
        }
        let account_id = args.account_id;
        let account = if account_id == 0 {
            None
        } else {
            Some(
                self.db
                    .read(move |conn| repo::accounts::get(conn, account_id))
                    .await?
                    .ok_or_else(|| CoreError::NotFound("account".into()))?,
            )
        };
        if account.is_none() && !args.attendees.is_empty() {
            return Err(CoreError::Other(
                "connect a mail account before inviting attendees".into(),
            ));
        }

        if let Some(calendar_id) = args.calendar_id {
            let cal = self
                .calendar_db
                .read(move |conn| repo::caldav::get_calendar(conn, calendar_id))
                .await?
                .ok_or_else(|| CoreError::NotFound("calendar".into()))?;
            if cal.account_id != account_id {
                return Err(CoreError::Other(
                    "calendar belongs to another account".into(),
                ));
            }
            if cal.read_only {
                return Err(CoreError::Other("calendar is read-only".into()));
            }
            if !cal.enabled {
                return Err(CoreError::Other("calendar is disabled".into()));
            }
        }

        let uid = format!("{}-{}@flectar-mail", now_ms(), crate::mime::rand_token());
        let attendees: Vec<EventAttendee> = args
            .attendees
            .iter()
            .map(|a| EventAttendee {
                email: a.email.clone(),
                name: a.name.clone(),
                partstat: Some("NEEDS-ACTION".into()),
            })
            .collect();

        let ev = args.clone();
        let organizer_email = account
            .as_ref()
            .map(|a| a.email.clone())
            .unwrap_or_default();
        let uid_for_db = uid.clone();
        let event_id = self
            .calendar_db
            .write(move |conn| {
                repo::calendar::insert_local(
                    conn,
                    account_id,
                    ev.calendar_id,
                    &uid_for_db,
                    ev.summary.trim(),
                    ev.location.as_deref(),
                    ev.description.as_deref(),
                    ev.join_url.as_deref(),
                    &organizer_email,
                    &attendees,
                    ev.starts_at,
                    ev.ends_at,
                    ev.all_day,
                )
            })
            .await?;

        if !args.attendees.is_empty() {
            let Some(account) = account.as_ref() else {
                return Err(CoreError::Other(
                    "connect a mail account before inviting attendees".into(),
                ));
            };
            let organizer = Address {
                name: account.display_name.clone(),
                email: account.email.clone(),
            };
            let ics = calendar::build_request_ics(&calendar::InviteSpec {
                uid: &uid,
                sequence: 0,
                summary: args.summary.trim(),
                description: args.description.as_deref(),
                location: args.location.as_deref(),
                join_url: args.join_url.as_deref(),
                organizer: &organizer,
                attendees: &args.attendees,
                starts_at_ms: args.starts_at,
                ends_at_ms: args.ends_at,
                dtstamp_ms: now_ms(),
            });
            let body_text = invite_body_text(&args);
            self.send_calendar_mail(
                account_id,
                args.attendees.clone(),
                format!("Invitation: {}", args.summary.trim()),
                body_text,
                &ics,
            )
            .await?;
        }

        self.enqueue_cal_push(event_id, account_id, "cal_put")
            .await?;

        // Microsoft accounts have no CalDAV endpoint, so also write the event
        // into their Outlook / Microsoft 365 calendar via Graph - that is what
        // Teams and Outlook show. Best-effort: a Graph failure (e.g. the
        // account predates the calendar consent) must not fail event creation,
        // which already succeeded locally. Skipped when the account has a
        // connected calendar: the sync task's push already writes it (and a
        // second direct POST would duplicate the event in Outlook).
        if account
            .as_ref()
            .is_some_and(|account| account.provider == Provider::Microsoft)
        {
            let has_cal_config = self
                .calendar_db
                .read(move |conn| repo::caldav::get_config(conn, account_id))
                .await?
                .is_some();
            if !has_cal_config && let Err(e) = self.push_event_to_graph(account_id, &args).await {
                tracing::warn!(error = %e, "graph: could not sync event to Outlook calendar");
            }
        }

        self.calendar_db
            .read(move |conn| repo::calendar::get(conn, event_id))
            .await?
            .ok_or_else(|| CoreError::NotFound("event".into()))
    }

    /// Write a just-created event into the Microsoft 365 calendar via Graph.
    pub(crate) async fn push_event_to_graph(
        &self,
        account_id: i64,
        args: &CreateEventArgs,
    ) -> Result<()> {
        let token = self
            .tokens
            .access_token_for_scope(
                account_id,
                Provider::Microsoft,
                oauth::providers::MS_CALENDARS_SCOPE,
            )
            .await?;
        // Fold the join link into the body so it rides along in Outlook/Teams.
        let body_html = match (&args.description, &args.join_url) {
            (d, Some(url)) => Some(format!(
                "{}<p><a href=\"{}\">Join the meeting</a></p>",
                d.as_deref().unwrap_or(""),
                url
            )),
            (Some(d), None) if !d.trim().is_empty() => Some(d.clone()),
            _ => None,
        };
        let attendees = args
            .attendees
            .iter()
            .map(|a| graph::GraphAttendee {
                email: a.email.clone(),
                name: a.name.clone(),
            })
            .collect();
        let event = graph::GraphEvent {
            subject: args.summary.trim(),
            body_html,
            location: args.location.as_deref(),
            start_ms: args.starts_at,
            end_ms: args.ends_at,
            all_day: args.all_day,
            attendees,
        };
        let create = graph::create_calendar_event(&token, None, &event);
        crate::net::proxy::ACCOUNT_SCOPE
            .scope(account_id, create)
            .await
            .map(|_| ())
    }

    /// Answer an invite: store our response and email an ICS METHOD:REPLY to
    /// the organizer (the standard-compliant path every calendar understands).
    pub async fn rsvp_event(&self, args: RsvpEventArgs) -> Result<CalendarEvent> {
        let partstat = match args.response.as_str() {
            "accepted" => "ACCEPTED",
            "tentative" => "TENTATIVE",
            "declined" => "DECLINED",
            _ => return Err(CoreError::Other("invalid RSVP response".into())),
        };
        let event_id = args.event_id;
        let (ev, uid_seq) = self
            .calendar_db
            .read(move |conn| {
                Ok((
                    repo::calendar::get(conn, event_id)?,
                    repo::calendar::uid_and_sequence(conn, event_id)?,
                ))
            })
            .await?;
        let ev = ev.ok_or_else(|| CoreError::NotFound("event".into()))?;
        let (uid, sequence) = uid_seq.ok_or_else(|| CoreError::NotFound("event".into()))?;

        let account_id = ev.account_id;
        let account = self
            .db
            .read(move |conn| repo::accounts::get(conn, account_id))
            .await?
            .ok_or_else(|| CoreError::NotFound("account".into()))?;

        // Reply goes to the organizer; without one there is nobody to notify,
        // but we still record the response locally.
        if let Some(organizer) = ev.organizer.clone().filter(|o| !o.is_empty()) {
            let me = Address {
                name: account.display_name.clone(),
                email: account.email.clone(),
            };
            let ics = calendar::build_reply_ics(&calendar::ReplySpec {
                uid: &uid,
                sequence,
                summary: ev.summary.as_deref(),
                partstat,
                organizer_email: &organizer,
                attendee: &me,
                starts_at_ms: ev.starts_at,
                ends_at_ms: ev.ends_at,
                dtstamp_ms: now_ms(),
            });
            let verb = match partstat {
                "ACCEPTED" => "Accepted",
                "TENTATIVE" => "Tentative",
                _ => "Declined",
            };
            let title = ev.summary.clone().unwrap_or_else(|| "(no title)".into());
            self.send_calendar_mail(
                account_id,
                vec![Address {
                    name: None,
                    email: organizer,
                }],
                format!("{verb}: {title}"),
                format!(
                    "{} has responded {} to: {title}",
                    account.email,
                    verb.to_lowercase()
                ),
                &ics,
            )
            .await?;
        }

        self.calendar_db
            .write(move |conn| repo::calendar::set_rsvp(conn, event_id, partstat))
            .await?;
        // CalDAV-backed invites also sync the PARTSTAT change to the server.
        self.enqueue_cal_push(event_id, ev.account_id, "cal_put")
            .await?;
        self.calendar_db
            .read(move |conn| repo::calendar::get(conn, event_id))
            .await?
            .ok_or_else(|| CoreError::NotFound("event".into()))
    }

    /// Edit an event we organize. The change is applied locally at once,
    /// attendees get an updated REQUEST ICS (bumped SEQUENCE) when `notify`,
    /// and CalDAV-backed events are flagged for the next push.
    pub async fn update_event(&self, args: UpdateEventArgs) -> Result<CalendarEvent> {
        if args.summary.trim().is_empty() {
            return Err(CoreError::Other("event needs a title".into()));
        }
        if args.ends_at <= args.starts_at {
            return Err(CoreError::Other("event must end after it starts".into()));
        }
        let event_id = args.event_id;
        let existing = self
            .calendar_db
            .read(move |conn| repo::calendar::get(conn, event_id))
            .await?
            .ok_or_else(|| CoreError::NotFound("event".into()))?;
        if !existing.is_local {
            return Err(CoreError::Other(
                "only events you organize can be edited".into(),
            ));
        }

        // Preserve responses attendees already gave; new addresses start out
        // NEEDS-ACTION.
        let attendees: Vec<EventAttendee> = args
            .attendees
            .iter()
            .map(|a| EventAttendee {
                email: a.email.clone(),
                name: a.name.clone(),
                partstat: existing
                    .attendees
                    .iter()
                    .find(|old| old.email.eq_ignore_ascii_case(&a.email))
                    .and_then(|old| old.partstat.clone())
                    .or_else(|| Some("NEEDS-ACTION".into())),
            })
            .collect();

        let args_for_db = args.clone();
        let atts_for_db = attendees.clone();
        self.calendar_db
            .write(move |conn| {
                repo::calendar::update_local_fields(conn, &args_for_db, &atts_for_db)
            })
            .await?;

        if args.notify && !args.attendees.is_empty() {
            let account_id = existing.account_id;
            let account = self
                .db
                .read(move |conn| repo::accounts::get(conn, account_id))
                .await?
                .ok_or_else(|| CoreError::NotFound("account".into()))?;
            let (uid, sequence) = self
                .calendar_db
                .read(move |conn| repo::calendar::uid_and_sequence(conn, event_id))
                .await?
                .ok_or_else(|| CoreError::NotFound("event".into()))?;
            let organizer = Address {
                name: account.display_name.clone(),
                email: account.email.clone(),
            };
            let ics = calendar::build_request_ics(&calendar::InviteSpec {
                uid: &uid,
                sequence,
                summary: args.summary.trim(),
                description: args.description.as_deref(),
                location: args.location.as_deref(),
                join_url: args.join_url.as_deref(),
                organizer: &organizer,
                attendees: &args.attendees,
                starts_at_ms: args.starts_at,
                ends_at_ms: args.ends_at,
                dtstamp_ms: now_ms(),
            });
            let body = invite_body_text(&CreateEventArgs {
                account_id,
                calendar_id: None,
                summary: args.summary.clone(),
                description: args.description.clone(),
                location: args.location.clone(),
                join_url: args.join_url.clone(),
                starts_at: args.starts_at,
                ends_at: args.ends_at,
                all_day: args.all_day,
                attendees: args.attendees.clone(),
            });
            self.send_calendar_mail(
                account_id,
                args.attendees.clone(),
                format!("Updated invitation: {}", args.summary.trim()),
                body,
                &ics,
            )
            .await?;
        }

        self.enqueue_cal_push(event_id, existing.account_id, "cal_put")
            .await?;
        self.calendar_db
            .read(move |conn| repo::calendar::get(conn, event_id))
            .await?
            .ok_or_else(|| CoreError::NotFound("event".into()))
    }

    /// Delete an event. Organized events with attendees email a METHOD:CANCEL
    /// when `notify`; CalDAV-backed rows become tombstones deleted at the next
    /// push, purely local rows disappear immediately.
    pub async fn delete_event(&self, event_id: i64, notify: bool) -> Result<()> {
        let ev = self
            .calendar_db
            .read(move |conn| repo::calendar::get(conn, event_id))
            .await?
            .ok_or_else(|| CoreError::NotFound("event".into()))?;

        if notify && ev.is_local && !ev.attendees.is_empty() {
            let account_id = ev.account_id;
            let account = self
                .db
                .read(move |conn| repo::accounts::get(conn, account_id))
                .await?
                .ok_or_else(|| CoreError::NotFound("account".into()))?;
            let (uid, sequence) = self
                .calendar_db
                .read(move |conn| repo::calendar::uid_and_sequence(conn, event_id))
                .await?
                .ok_or_else(|| CoreError::NotFound("event".into()))?;
            let organizer = Address {
                name: account.display_name.clone(),
                email: account.email.clone(),
            };
            let to: Vec<Address> = ev
                .attendees
                .iter()
                .map(|a| Address {
                    name: a.name.clone(),
                    email: a.email.clone(),
                })
                .collect();
            let title = ev.summary.clone().unwrap_or_else(|| "(no title)".into());
            let ics = calendar::build_cancel_ics(&calendar::InviteSpec {
                uid: &uid,
                sequence: sequence + 1,
                summary: &title,
                description: None,
                location: ev.location.as_deref(),
                join_url: None,
                organizer: &organizer,
                attendees: &to,
                starts_at_ms: ev.starts_at,
                ends_at_ms: ev.ends_at.unwrap_or(ev.starts_at + 1_800_000),
                dtstamp_ms: now_ms(),
            });
            self.send_calendar_mail(
                account_id,
                to,
                format!("Cancelled: {title}"),
                format!("{title} has been cancelled."),
                &ics,
            )
            .await?;
        }

        // CalDAV rows need the server-side DELETE; keep a tombstone and let
        // the push path finish the job. Mail-invite rows keep a terminal
        // tombstone - purging them would let the next re-parse of the
        // archived invitation email resurrect the event. Purely local rows
        // go right away.
        if ev.calendar_id.is_some() {
            self.calendar_db
                .write(move |conn| repo::calendar::mark_deleted(conn, event_id))
                .await?;
            self.enqueue_cal_push(event_id, ev.account_id, "cal_delete")
                .await?;
        } else if ev.message_id.is_some() {
            self.calendar_db
                .write(move |conn| repo::calendar::mark_deleted(conn, event_id))
                .await?;
        } else {
            self.calendar_db
                .write(move |conn| repo::calendar::hard_delete(conn, event_id))
                .await?;
        }
        Ok(())
    }

    /// Connect a calendar server to an account. Generic servers store the app
    /// password in the keyring; Google reuses the account's OAuth tokens (the
    /// caller must have completed the calendar-scope re-consent first).
    /// Discovery doubles as the connection test - nothing persists on failure.
    pub async fn connect_calendar(&self, args: ConnectCalendarArgs) -> Result<Vec<Calendar>> {
        let account_id = args.account_id;
        let kind = if args.kind == "google" {
            "google"
        } else {
            "generic"
        };
        let base_url = match kind {
            "google" => {
                let calendar_id = self
                    .db
                    .read(move |conn| {
                        Ok(repo::accounts::get(conn, account_id)?.map(|account| account.email))
                    })
                    .await?
                    .ok_or_else(|| CoreError::NotFound("account".into()))?;
                caldav::google_calendar_url(&calendar_id)?
            }
            _ => {
                let url = args
                    .url
                    .clone()
                    .filter(|u| !u.trim().is_empty())
                    .ok_or_else(|| CoreError::CalDav("server URL is required".into()))?;
                let mut url = url.trim().to_string();
                if !url.contains("://") {
                    url = format!("https://{url}");
                }
                url
            }
        };

        let submitted_password = args.password.clone().filter(|p| !p.is_empty());

        // Build auth without persisting anything yet. An empty password on an
        // existing generic connection means "keep the current app password",
        // so settings can be edited without exposing or re-entering it.
        let auth = match kind {
            "google" => caldav::DavAuth::Bearer(
                self.tokens
                    .access_token(account_id, Provider::Gmail)
                    .await?,
            ),
            _ => {
                let user = args.username.clone().unwrap_or_default();
                credentials::check_available_async(self.credentials.clone()).await?;
                let pass = match submitted_password.clone() {
                    Some(password) => password,
                    None => {
                        let has_existing_generic_config = self
                            .calendar_db
                            .read(move |conn| {
                                Ok(repo::caldav::get_config(conn, account_id)?
                                    .is_some_and(|config| config.kind == "generic"))
                            })
                            .await?;
                        if !has_existing_generic_config {
                            return Err(CoreError::CalDav("password is required".into()));
                        }
                        credentials::load_async(
                            self.credentials.clone(),
                            account_id,
                            Slot::CaldavPassword,
                        )
                        .await?
                    }
                };
                caldav::DavAuth::Basic(user, pass)
            }
        };
        let google_access_token = match &auth {
            caldav::DavAuth::Bearer(token) if kind == "google" => Some(token.clone()),
            _ => None,
        };
        let transport = caldav::HttpTransport::new(auth, &base_url)?;
        let (discovery, google_calendars) = if kind == "google" {
            // Validate the primary DAV collection so a disabled CalDAV API is
            // reported immediately. Calendar API metadata then supplies every
            // secondary/shared/special calendar id; CalDAV itself only knows
            // about the one id embedded in its collection URL.
            let primary =
                caldav::discovery::discover_known_collection(&transport, &base_url).await?;
            let access_token = google_access_token.as_deref().ok_or_else(|| {
                CoreError::CalDav("Google calendar authentication was not initialized".into())
            })?;
            let listed = crate::net::proxy::ACCOUNT_SCOPE
                .scope(account_id, googlecal::list_calendars(access_token))
                .await?;
            if listed.is_empty() {
                return Err(CoreError::CalDav("no Google calendars found".into()));
            }
            (primary, Some(listed))
        } else {
            (
                caldav::discovery::discover(&transport, &base_url).await?,
                None,
            )
        };

        // Persist: keyring first, then config + collections.
        if kind == "generic"
            && let Some(pass) = submitted_password
        {
            credentials::store_async(
                self.credentials.clone(),
                account_id,
                Slot::CaldavPassword,
                pass,
            )
            .await?;
        }
        let cfg = repo::caldav::CaldavConfig {
            account_id,
            kind: kind.to_string(),
            base_url,
            username: args.username.clone(),
            principal_url: discovery.principal_url.clone(),
            home_set_url: Some(discovery.home_set_url.clone()),
            enabled: true,
            last_error: None,
        };
        let calendars = discovery.calendars.clone();
        let out = self
            .calendar_db
            .write(move |conn| {
                let tx = conn.transaction()?;
                repo::caldav::upsert_config(&tx, &cfg)?;
                if let Some(google_calendars) = &google_calendars {
                    googlecal::reconcile_calendars(&tx, account_id, google_calendars)?;
                } else {
                    let calendar_urls = calendars
                        .iter()
                        .map(|calendar| calendar.url.clone())
                        .collect::<HashSet<_>>();
                    let mut first_id = None;
                    for c in &calendars {
                        let id = repo::caldav::upsert_calendar(
                            &tx,
                            account_id,
                            &c.url,
                            c.display_name.as_deref(),
                            c.color.as_deref(),
                            false,
                        )?;
                        first_id.get_or_insert(id);
                    }
                    repo::caldav::retain_calendars(&tx, account_id, &calendar_urls)?;
                    if let Some(id) = first_id {
                        // Keep an existing default if one is set; else first wins.
                        let has_default: i64 = tx.query_row(
                            "SELECT COUNT(*) FROM calendars WHERE account_id = ?1 AND is_default = 1",
                            rusqlite::params![account_id],
                            |r| r.get(0),
                        )?;
                        if has_default == 0 {
                            repo::caldav::set_default_calendar(&tx, account_id, id)?;
                        }
                    }
                }
                let list = repo::caldav::list_calendars(&tx, Some(account_id))?;
                tx.commit()?;
                Ok(list)
            })
            .await?;

        self.spawn_cal_task(account_id).await;
        self.nudge_cal(Some(account_id)).await;
        Ok(out)
    }

    /// Google calendar connection: re-run the OAuth consent with the
    /// calendar scope added (scopes are fixed at consent time, so the account
    /// must re-consent) and swap in the widened tokens, then discover.
    pub async fn connect_google_calendar(
        &self,
        account_id: i64,
        open_url: impl FnOnce(String) -> std::result::Result<(), String> + Send,
    ) -> Result<Vec<Calendar>> {
        let account = self
            .db
            .read(move |conn| repo::accounts::get(conn, account_id))
            .await?
            .ok_or_else(|| CoreError::NotFound("account".into()))?;
        if account.provider != Provider::Gmail {
            return Err(CoreError::CalDav(
                "google calendar needs a Gmail account".into(),
            ));
        }

        credentials::check_available_async(self.credentials.clone()).await?;

        let outcome = tokio::select! {
            r = oauth::flow::authorize_with_broker(
                Provider::Gmail,
                &[oauth::providers::GOOGLE_CALENDAR_SCOPE],
                Some(&account.email),
                self.oauth_redirects.clone(),
                open_url,
            ) => r?,
            _ = self.oauth_cancel.notified() => {
                return Err(CoreError::Auth("sign-in cancelled".into()));
            }
        };
        if !outcome.email.eq_ignore_ascii_case(&account.email) {
            return Err(CoreError::Auth(format!(
                "consent was granted for {} - expected {}",
                outcome.email, account.email
            )));
        }
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

        self.connect_calendar(ConnectCalendarArgs {
            account_id,
            kind: "google".into(),
            url: None,
            username: None,
            password: None,
        })
        .await
    }

    /// Mint a Graph token for one extra scope, widening consent in the
    /// browser when the scope was never granted (incremental consent, like
    /// `connect_google_calendar`). `open_url` opens the consent page.
    pub(crate) async fn graph_token_with_consent(
        &self,
        account_id: i64,
        scope: &str,
        open_url: impl FnOnce(String) -> std::result::Result<(), String> + Send,
    ) -> Result<String> {
        let account = self
            .db
            .read(move |conn| repo::accounts::get(conn, account_id))
            .await?
            .ok_or_else(|| CoreError::NotFound("account".into()))?;
        if account.provider != Provider::Microsoft {
            return Err(CoreError::Auth("needs a Microsoft account".into()));
        }

        let extra_scopes = [scope];
        match self
            .tokens
            .access_token_for_scope(account_id, Provider::Microsoft, scope)
            .await
        {
            Ok(t) => Ok(t),
            // Scope not yet consented (or refresh token stale): widen consent
            // in the browser, then mint the Graph token again.
            Err(CoreError::NeedsReauth) => {
                credentials::check_available_async(self.credentials.clone()).await?;
                let outcome = tokio::select! {
                    r = oauth::flow::authorize_with_broker(
                        Provider::Microsoft,
                        &extra_scopes,
                        Some(&account.email),
                        self.oauth_redirects.clone(),
                        open_url,
                    ) => r?,
                    _ = self.oauth_cancel.notified() => {
                        return Err(CoreError::Auth("sign-in cancelled".into()));
                    }
                };
                if !outcome.email.eq_ignore_ascii_case(&account.email) {
                    return Err(CoreError::Auth(format!(
                        "consent was granted for {} - expected {}",
                        outcome.email, account.email
                    )));
                }
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
                self.tokens
                    .access_token_for_scope(account_id, Provider::Microsoft, scope)
                    .await
            }
            Err(e) => Err(e),
        }
    }

    /// Create a Microsoft Teams online meeting for a Microsoft account and
    /// return its join URL (for insertion into a compose draft).
    ///
    /// Needs a Graph-scoped token; the first attempt may trigger an
    /// incremental re-consent in the browser.
    pub async fn create_teams_meeting(
        &self,
        account_id: i64,
        subject: &str,
        start_ms: i64,
        end_ms: i64,
        open_url: impl FnOnce(String) -> std::result::Result<(), String> + Send,
    ) -> Result<graph::OnlineMeeting> {
        let token = self
            .graph_token_with_consent(
                account_id,
                oauth::providers::MS_ONLINE_MEETINGS_SCOPE,
                open_url,
            )
            .await?;
        crate::net::proxy::ACCOUNT_SCOPE
            .scope(
                account_id,
                graph::create_online_meeting(&token, subject, start_ms, end_ms),
            )
            .await
    }

    /// Microsoft calendar connection: Outlook / Microsoft 365 has no CalDAV
    /// endpoint, so the calendar syncs via Graph. Widens consent to
    /// `Calendars.ReadWrite` when needed, lists the account's calendars and
    /// persists them under a "microsoft"-kind config; the shared calendar
    /// task then pulls via calendarView delta.
    pub async fn connect_microsoft_calendar(
        &self,
        account_id: i64,
        open_url: impl FnOnce(String) -> std::result::Result<(), String> + Send,
    ) -> Result<Vec<Calendar>> {
        let token = self
            .graph_token_with_consent(account_id, oauth::providers::MS_CALENDARS_SCOPE, open_url)
            .await?;
        let discovered = crate::net::proxy::ACCOUNT_SCOPE
            .scope(account_id, graph::list_calendars(&token))
            .await?;
        if discovered.is_empty() {
            return Err(CoreError::CalDav("no calendars found".into()));
        }

        let cfg = repo::caldav::CaldavConfig {
            account_id,
            kind: "microsoft".to_string(),
            base_url: graph::GRAPH_BASE.to_string(),
            username: None,
            principal_url: None,
            home_set_url: None,
            enabled: true,
            last_error: None,
        };
        let out = self
            .calendar_db
            .write(move |conn| {
                let tx = conn.transaction()?;
                repo::caldav::upsert_config(&tx, &cfg)?;
                let mut default_id = None;
                let mut first_id = None;
                for c in &discovered {
                    let id = repo::caldav::upsert_calendar(
                        &tx,
                        account_id,
                        &c.id,
                        c.name.as_deref(),
                        c.hex_color.as_deref(),
                        !c.can_edit,
                    )?;
                    first_id.get_or_insert(id);
                    if c.is_default {
                        default_id.get_or_insert(id);
                    }
                }
                if let Some(id) = default_id.or(first_id) {
                    // Keep an existing default if one is set; else Outlook's
                    // default calendar (or the first) wins.
                    let has_default: i64 = tx.query_row(
                        "SELECT COUNT(*) FROM calendars WHERE account_id = ?1 AND is_default = 1",
                        rusqlite::params![account_id],
                        |r| r.get(0),
                    )?;
                    if has_default == 0 {
                        repo::caldav::set_default_calendar(&tx, account_id, id)?;
                    }
                }
                let list = repo::caldav::list_calendars(&tx, Some(account_id))?;
                tx.commit()?;
                Ok(list)
            })
            .await?;

        self.spawn_cal_task(account_id).await;
        self.nudge_cal(Some(account_id)).await;
        Ok(out)
    }

    /// Disconnect the calendar server: events stay locally, sync bookkeeping
    /// is cleared, credentials removed.
    pub async fn disconnect_calendar(&self, account_id: i64) -> Result<()> {
        self.cal_handles.write().await.remove(&account_id);
        self.calendar_db
            .write(move |conn| repo::caldav::delete_config(conn, account_id))
            .await?;
        let _ = credentials::store_async(
            self.credentials.clone(),
            account_id,
            Slot::CaldavPassword,
            String::new(),
        )
        .await;
        self.bus.emit(CoreEvent::CalendarUpdated { account_id });
        Ok(())
    }

    pub async fn list_calendars(&self, account_id: Option<i64>) -> Result<Vec<Calendar>> {
        self.calendar_db
            .read(move |conn| repo::caldav::list_calendars(conn, account_id))
            .await
    }

    pub async fn list_calendar_connections(&self) -> Result<Vec<CalendarConnection>> {
        self.calendar_db
            .read(|conn| {
                Ok(repo::caldav::list_configs(conn)?
                    .into_iter()
                    .map(|cfg| CalendarConnection {
                        account_id: cfg.account_id,
                        kind: cfg.kind,
                        base_url: cfg.base_url,
                        username: cfg.username.unwrap_or_default(),
                        enabled: cfg.enabled,
                        last_error: cfg.last_error,
                    })
                    .collect())
            })
            .await
    }

    /// Pause/resume a connected calendar without deleting local events,
    /// discovered collections, sync tokens, or credentials.
    pub async fn set_account_calendar_enabled(&self, account_id: i64, enabled: bool) -> Result<()> {
        let found = self
            .calendar_db
            .write(move |conn| repo::caldav::set_config_enabled(conn, account_id, enabled))
            .await?;
        if !found {
            return Err(CoreError::NotFound("calendar connection".into()));
        }
        if enabled {
            if !self.cal_handles.read().await.contains_key(&account_id) {
                self.spawn_cal_task(account_id).await;
            }
            self.nudge_cal(Some(account_id)).await;
        } else {
            self.cal_handles.write().await.remove(&account_id);
        }
        self.bus.emit(CoreEvent::CalendarUpdated { account_id });
        Ok(())
    }

    pub async fn set_calendar_enabled(&self, calendar_id: i64, enabled: bool) -> Result<()> {
        self.calendar_db
            .write(move |conn| repo::caldav::set_calendar_enabled(conn, calendar_id, enabled))
            .await?;
        let cal = self
            .calendar_db
            .read(move |conn| repo::caldav::get_calendar(conn, calendar_id))
            .await?;
        if let Some(cal) = cal {
            if enabled {
                self.nudge_cal(Some(cal.account_id)).await;
            }
            self.bus.emit(CoreEvent::CalendarUpdated {
                account_id: cal.account_id,
            });
        }
        Ok(())
    }

    /// User override of the calendar's display color (`#RRGGBB`). None
    /// clears the override so the next discovery restores the server color.
    pub async fn set_calendar_color(&self, calendar_id: i64, color: Option<String>) -> Result<()> {
        if let Some(c) = &color {
            let valid = c.len() == 7
                && c.starts_with('#')
                && c[1..].chars().all(|ch| ch.is_ascii_hexdigit());
            if !valid {
                return Err(CoreError::Other("invalid color".into()));
            }
        }
        let cal = self
            .calendar_db
            .read(move |conn| repo::caldav::get_calendar(conn, calendar_id))
            .await?
            .ok_or_else(|| CoreError::NotFound("calendar".into()))?;
        self.calendar_db
            .write(move |conn| {
                repo::caldav::set_calendar_color(conn, calendar_id, color.as_deref())
            })
            .await?;
        self.bus.emit(CoreEvent::CalendarUpdated {
            account_id: cal.account_id,
        });
        Ok(())
    }

    /// Make this calendar the account's target for newly created events.
    pub async fn set_default_calendar(&self, calendar_id: i64) -> Result<()> {
        let cal = self
            .calendar_db
            .read(move |conn| repo::caldav::get_calendar(conn, calendar_id))
            .await?
            .ok_or_else(|| CoreError::NotFound("calendar".into()))?;
        if cal.read_only {
            return Err(CoreError::Other("calendar is read-only".into()));
        }
        let account_id = cal.account_id;
        self.calendar_db
            .write(move |conn| repo::caldav::set_default_calendar(conn, account_id, calendar_id))
            .await?;
        self.bus.emit(CoreEvent::CalendarUpdated { account_id });
        Ok(())
    }

    pub async fn calendar_sync_now(&self, account_id: Option<i64>) {
        self.nudge_cal(account_id).await;
    }

    /// Queue a CalDAV write for the account's calendar task, when the account
    /// has one configured. No-op otherwise (purely local calendars).
    pub(crate) async fn enqueue_cal_push(
        &self,
        event_id: i64,
        account_id: i64,
        kind: &str,
    ) -> Result<()> {
        let kind = kind.to_string();
        self.calendar_db
            .write(move |conn| {
                if repo::caldav::get_config(conn, account_id)?.is_none() {
                    return Ok(());
                }
                // Dirty guards the row against being clobbered by a pull that
                // runs between now and the push.
                if kind == "cal_put" {
                    repo::calendar::mark_dirty(conn, event_id)?;
                }
                let payload = serde_json::json!({ "eventId": event_id });
                repo::actions::enqueue(conn, account_id, &kind, None, None, &payload, None)?;
                Ok(())
            })
            .await?;
        self.nudge_cal(Some(account_id)).await;
        Ok(())
    }

    /// Draft + queue an email carrying an ICS part, through the normal send
    /// pipeline. The ICS is staged like an attachment.
    pub(crate) async fn send_calendar_mail(
        &self,
        account_id: i64,
        to: Vec<Address>,
        subject: String,
        body_text: String,
        ics: &str,
    ) -> Result<()> {
        let tmp_dir = self.paths.temp_dir();
        tokio::fs::create_dir_all(&tmp_dir).await?;
        let tmp_path = tmp_dir.join(format!("invite-{}.ics", crate::mime::rand_token()));
        tokio::fs::write(&tmp_path, ics.as_bytes()).await?;

        let draft_id = self
            .save_draft(SaveDraftArgs {
                draft_id: None,
                account_id,
                from: None,
                to,
                cc: Vec::new(),
                bcc: Vec::new(),
                subject,
                body_text,
                body_html: None,
                mode: "new".into(),
                in_reply_to_message_id: None,
                attachments: vec![DraftAttachmentIn {
                    file_path: tmp_path.to_string_lossy().into_owned(),
                    filename: "invite.ics".into(),
                }],
            })
            .await?;
        // The draft staged its own copy; the temp file can go.
        let _ = tokio::fs::remove_file(&tmp_path).await;
        self.queue_send(QueueSendArgs {
            draft_id,
            send_at: None,
        })
        .await?;
        Ok(())
    }
}
