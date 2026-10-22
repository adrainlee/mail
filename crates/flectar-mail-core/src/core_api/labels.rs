//! `Core` methods: Split tabs, labels and routing.
//! Moved verbatim from `lib.rs`; see docs/fork.md.

use crate::*;

impl Core {
    pub async fn list_splits(&self) -> Result<Vec<SplitRule>> {
        self.db.read(|conn| repo::splits::list(conn)).await
    }

    /// First split rule (in position order) that matches a thread, if any.
    /// `threads.routed_tab` can't answer this for rules with a `target` (they
    /// write the target's route key, not `split:<id>`), so re-run the matcher.
    pub async fn find_matching_split(&self, thread_id: i64) -> Result<Option<SplitRule>> {
        self.db
            .read(move |conn| {
                for rule in repo::splits::list(conn)? {
                    if route::split_matches(conn, thread_id, &rule.query)? {
                        return Ok(Some(rule));
                    }
                }
                Ok(None)
            })
            .await
    }

    pub async fn save_split(
        &self,
        id: Option<i64>,
        name: String,
        position: i64,
        query: SplitRuleQuery,
        target: Option<String>,
    ) -> Result<SplitRule> {
        let saved = self
            .db
            .write(move |conn| {
                repo::splits::save(conn, id, &name, position, &query, target.as_deref())
            })
            .await?;
        // Rules changed: re-resolve every thread and drop the AI cache so edited
        // routing takes effect. AI (if any) runs in the background afterwards.
        self.reroute_all_sync().await?;
        Ok(saved)
    }

    pub async fn delete_split(&self, id: i64) -> Result<()> {
        self.db
            .write(move |conn| repo::splits::delete(conn, id))
            .await?;
        // A deleted rule's threads must fall back to the AI/heuristic/defaults.
        self.reroute_all_sync().await?;
        Ok(())
    }

    /// Persist a single shared tab order across custom splits and auto-label
    /// tabs. `order` lists every reorderable tab top-to-bottom as `(kind, id)`
    /// where `kind` is `"split"` or `"label"`; its index becomes the row's
    /// `position`. Built-in Important/Other are pinned and never included.
    ///
    /// Only re-resolves routing when the splits' relative priority actually
    /// changed (moving an auto-label around can't affect first-match-wins), so a
    /// pure label move stays cheap.
    pub async fn reorder_tabs(&self, order: Vec<(String, i64)>) -> Result<()> {
        let split_order_changed = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                let split_ids = |tx: &rusqlite::Transaction| -> rusqlite::Result<Vec<i64>> {
                    tx.prepare("SELECT id FROM split_rules ORDER BY position, id")?
                        .query_map([], |r| r.get(0))?
                        .collect()
                };
                let before = split_ids(&tx)?;
                for (i, (kind, id)) in order.iter().enumerate() {
                    let pos = i as i64;
                    match kind.as_str() {
                        "split" => tx.execute(
                            "UPDATE split_rules SET position = ?2 WHERE id = ?1",
                            rusqlite::params![id, pos],
                        )?,
                        "label" => tx.execute(
                            "UPDATE labels SET position = ?2 WHERE id = ?1",
                            rusqlite::params![id, pos],
                        )?,
                        _ => 0,
                    };
                }
                let after = split_ids(&tx)?;
                tx.commit()?;
                Ok(before != after)
            })
            .await?;
        if split_order_changed {
            self.reroute_all_sync().await?;
        } else {
            self.bus.emit(CoreEvent::MailUpdated { thread_ids: vec![] });
        }
        Ok(())
    }

    pub async fn list_labels(&self) -> Result<Vec<Label>> {
        self.db.read(|conn| repo::labels::list(conn)).await
    }

    /// Explicitly place one thread in a built-in or automatic category. This
    /// is used by native drag-and-drop targets; `apply_tab` also keeps the
    /// visible auto-label chip in sync with the exclusive route.
    pub async fn route_thread_to_tab(&self, thread_id: i64, target: String) -> Result<()> {
        self.db
            .write(move |conn| {
                let valid = match target.as_str() {
                    "important" | "other" => true,
                    value if value.starts_with("label:") => {
                        let Some(id) = value
                            .strip_prefix("label:")
                            .and_then(|id| id.parse::<i64>().ok())
                        else {
                            return Err(CoreError::Other(
                                "invalid mail category destination".into(),
                            ));
                        };
                        repo::labels::get(conn, id)?.is_some_and(|label| label.is_auto)
                    }
                    _ => false,
                };
                if !valid {
                    return Err(CoreError::Other("invalid mail category destination".into()));
                }
                route::apply_tab(conn, thread_id, Some(&target))
            })
            .await?;
        self.bus.emit(CoreEvent::MailUpdated {
            thread_ids: vec![thread_id],
        });
        Ok(())
    }

    pub async fn save_label(
        &self,
        id: Option<i64>,
        name: String,
        color: String,
        position: i64,
        owner_account_id: Option<i64>,
    ) -> Result<Label> {
        let (label, accounts) = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                if let Some(account_id) = owner_account_id
                    && repo::accounts::get(&tx, account_id)?.is_none()
                {
                    return Err(CoreError::NotFound(format!("account {account_id}")));
                }
                let previous = id
                    .map(|label_id| repo::labels::get(&tx, label_id))
                    .transpose()?
                    .flatten();
                let cascade_descendants = previous
                    .as_ref()
                    .and_then(|label| label.owner_account_id)
                    .map(|account_id| repo::accounts::get(&tx, account_id))
                    .transpose()?
                    .flatten()
                    .is_some_and(|account| account.provider == Provider::Gmail);
                let (label, mut changed_labels) = repo::labels::save_with_descendants(
                    &tx,
                    id,
                    &name,
                    &color,
                    position,
                    owner_account_id,
                    cascade_descendants,
                )?;
                // Descendants are already deepest-first; update them before
                // their root to avoid temporary Gmail name collisions.
                changed_labels.push(label.clone());
                let mut accounts = Vec::new();
                if id.is_none()
                    && let Some(account_id) = label.owner_account_id
                    && repo::accounts::get(&tx, account_id)?
                        .is_some_and(|account| account.provider == Provider::Gmail)
                {
                    repo::actions::enqueue(
                        &tx,
                        account_id,
                        "gmail_label_create",
                        None,
                        None,
                        &serde_json::json!({ "labelId": label.id }),
                        None,
                    )?;
                    accounts.push(account_id);
                }
                for changed in changed_labels {
                    let mappings = {
                        let mut stmt = tx.prepare(
                            "SELECT account_id, provider_id FROM gmail_labels
                             WHERE local_label_id = ?1",
                        )?;
                        stmt.query_map(rusqlite::params![changed.id], |row| {
                            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?
                    };
                    for (account_id, provider_id) in mappings {
                        repo::actions::enqueue(
                            &tx,
                            account_id,
                            "gmail_label_update",
                            None,
                            None,
                            &serde_json::json!({
                                "providerId": provider_id,
                                "name": changed.name,
                                "color": changed.color,
                            }),
                            None,
                        )?;
                        accounts.push(account_id);
                    }
                }
                accounts.sort_unstable();
                accounts.dedup();
                tx.commit()?;
                Ok((label, accounts))
            })
            .await?;
        for account_id in accounts {
            self.nudge(Some(account_id), || SyncCmd::RunActions).await;
        }
        Ok(label)
    }

    pub async fn delete_label(&self, id: i64) -> Result<()> {
        let accounts = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                let root = repo::labels::get(&tx, id)?;
                let delete_subtree = root
                    .as_ref()
                    .and_then(|label| label.owner_account_id)
                    .map(|account_id| repo::accounts::get(&tx, account_id))
                    .transpose()?
                    .flatten()
                    .is_some_and(|account| account.provider == Provider::Gmail);
                let labels = match root {
                    Some(root) if delete_subtree => repo::labels::subtree(&tx, &root)?,
                    Some(root) => vec![root],
                    None => Vec::new(),
                };
                let mut accounts = Vec::new();
                for label in &labels {
                    let mappings = {
                        let mut stmt = tx.prepare(
                            "SELECT account_id, provider_id FROM gmail_labels
                             WHERE local_label_id = ?1",
                        )?;
                        stmt.query_map(rusqlite::params![label.id], |row| {
                            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?
                    };
                    for (account_id, provider_id) in mappings {
                        repo::actions::enqueue(
                            &tx,
                            account_id,
                            "gmail_label_delete",
                            None,
                            None,
                            &serde_json::json!({ "providerId": provider_id }),
                            None,
                        )?;
                        accounts.push(account_id);
                    }
                    if delete_subtree && let Some(account_id) = label.owner_account_id {
                        // If creation has not reached Gmail yet, cancelling it
                        // is both faster and avoids manufacturing a remote
                        // label only to delete it immediately.
                        tx.execute(
                            "UPDATE pending_actions
                             SET state='cancelled', finished_at=?3
                             WHERE account_id=?1 AND kind='gmail_label_create'
                               AND state='pending'
                               AND json_extract(payload, '$.labelId')=?2",
                            rusqlite::params![account_id, label.id, now_ms()],
                        )?;
                        accounts.push(account_id);
                    }
                }
                for label in labels {
                    repo::labels::delete(&tx, label.id)?;
                }
                accounts.sort_unstable();
                accounts.dedup();
                tx.commit()?;
                Ok(accounts)
            })
            .await?;
        for account_id in accounts {
            self.nudge(Some(account_id), || SyncCmd::RunActions).await;
        }
        Ok(())
    }

    pub async fn restore_auto_labels(&self) -> Result<i64> {
        let restored = self
            .db
            .write(|conn| repo::labels::restore_auto_defaults(conn))
            .await?;
        if restored > 0 {
            self.reroute_all_sync().await?;
        }
        Ok(restored)
    }

    /// Re-run routing over all stored mail. Legacy alias kept for the existing
    /// "Relabel" action; delegates to [`Core::reroute_all`].
    /// Re-resolve every thread's tab from scratch and drain the AI queue, so the
    /// caller sees the final state. Used by the explicit "Re-sort" action and the
    /// one-shot startup backfill.
    pub async fn reroute_all(&self) -> Result<i64> {
        let n = self.reroute_all_sync().await?;
        // Drain the AI queue now so the caller sees the final state.
        while self.classify_pending(200).await? > 0 {}
        Ok(n)
    }

    /// Deterministic re-route only (rules + heuristic, or mark 'pending'); the
    /// background router handles the AI pass. Fast enough to run on every rule
    /// edit without blocking on model calls.
    pub(crate) async fn reroute_all_sync(&self) -> Result<i64> {
        let n = self
            .db
            .write(|conn| {
                let tx = conn.transaction()?;
                // Keep the one-shot backfill marker; drop real sender decisions
                // so an edited prompt/rules re-evaluates.
                tx.execute(
                    "DELETE FROM route_cache WHERE sender_domain <> '__routing_backfill__'",
                    [],
                )?;
                tx.execute(
                    "DELETE FROM message_labels WHERE label_id IN
                     (SELECT id FROM labels WHERE is_auto = 1)",
                    [],
                )?;
                tx.execute("UPDATE threads SET routed_tab = NULL", [])?;
                tx.execute(
                    "UPDATE messages SET local_subject_prefix = '', local_body_note = ''",
                    [],
                )?;
                let settings = repo::settings::get(&tx)?;
                let mut n = 0i64;
                if settings.auto_labels_enabled {
                    let splits = repo::splits::list(&tx)?;
                    let ids: Vec<i64> = {
                        let mut stmt = tx.prepare("SELECT id FROM threads")?;

                        stmt.query_map([], |r| r.get(0))?
                            .collect::<rusqlite::Result<Vec<_>>>()?
                    };
                    for id in ids {
                        route::route_thread_deterministic(
                            &tx,
                            &splits,
                            settings.ai_categorize,
                            id,
                        )?;
                        n += 1;
                    }
                }
                tx.commit()?;
                Ok(n)
            })
            .await?;
        // Routing changed across many threads; a blanket refresh is fine here.
        // The emit also wakes the background AI router to handle any 'pending'.
        self.bus.emit(CoreEvent::MailUpdated { thread_ids: vec![] });
        Ok(n)
    }
}
