//! `Core` methods: Siftmail category management, deterministic rules and
//! advisory AI category suggestions.

use crate::category::{self, Category, Rule, RuleAction, RuleMatch};
use crate::*;

/// User-originated acceptance of a suggestion. Only constructible here, from
/// an explicit UI command (`Core::accept_category_suggestion`).
struct UserAcceptance {
    suggestion_id: i64,
}

impl Core {
    pub async fn list_categories(&self) -> Result<Vec<Category>> {
        self.db
            .read(|conn| category::repo::list_categories(conn))
            .await
    }

    pub async fn save_category(&self, c: Category) -> Result<()> {
        self.db
            .write(move |conn| category::repo::save_category(conn, &c))
            .await
    }

    pub async fn delete_category(&self, id: String) -> Result<bool> {
        self.db
            .write(move |conn| category::repo::delete_category(conn, &id))
            .await
    }

    pub async fn list_category_rules(&self) -> Result<Vec<Rule>> {
        self.db.read(|conn| category::repo::list_rules(conn)).await
    }

    /// Save a rule; clears previous "no match" marks so the new rule set is
    /// evaluated against recent threads again.
    pub async fn save_category_rule(&self, rule: Rule) -> Result<i64> {
        let id = self
            .db
            .write(move |conn| {
                let id = category::repo::save_rule(conn, &rule)?;
                conn.execute(
                    "DELETE FROM thread_categories WHERE source = 'rule' AND category_id IS NULL",
                    [],
                )?;
                Ok(id)
            })
            .await?;
        Ok(id)
    }

    pub async fn delete_category_rule(&self, id: i64) -> Result<bool> {
        self.db
            .write(move |conn| category::repo::delete_rule(conn, id))
            .await
    }

    /// Rule test panel: which rule (if any) matches this thread right now.
    pub async fn test_category_rules(&self, thread_id: i64) -> Result<Option<RuleMatch>> {
        self.db
            .read(move |conn| {
                let rules = category::repo::active_rules(conn)?;
                Ok(category::repo::thread_facts(conn, thread_id)?
                    .and_then(|f| category::evaluate(&rules, &f)))
            })
            .await
    }

    /// Evaluate deterministic rules on threads not evaluated yet. Matches set
    /// the thread category (and its label tab when linked) and may star /
    /// mark read when the user enabled that on the rule. Returns matches.
    pub async fn apply_category_rules(&self, limit: i64) -> Result<i64> {
        let matches: Vec<(i64, RuleMatch)> = self
            .db
            .write(move |conn| {
                let rules = category::repo::active_rules(conn)?;
                if rules.is_empty() {
                    return Ok(Vec::new());
                }
                let ids: Vec<i64> = conn
                    .prepare(
                        "SELECT t.id FROM threads t
                         LEFT JOIN thread_categories c ON c.thread_id = t.id
                         WHERE c.thread_id IS NULL
                         ORDER BY t.last_message_at DESC LIMIT ?1",
                    )?
                    .query_map(rusqlite::params![limit], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                let mut out = Vec::new();
                for tid in ids {
                    let Some(facts) = category::repo::thread_facts(conn, tid)? else {
                        // No inbound message yet; mark evaluated.
                        conn.execute(
                            "INSERT OR IGNORE INTO thread_categories (thread_id, category_id, source, updated_at)
                             VALUES (?1, NULL, 'rule', 0)",
                            rusqlite::params![tid],
                        )?;
                        continue;
                    };
                    match category::evaluate(&rules, &facts) {
                        Some(m) => {
                            category::repo::set_thread_category(
                                conn,
                                tid,
                                &m.category_id,
                                "rule",
                                Some(m.rule_id),
                            )?;
                            if let Some(label) = category::repo::category_label(conn, &m.category_id)? {
                                route::apply_tab(conn, tid, Some(&format!("label:{label}")))?;
                            }
                            out.push((tid, m));
                        }
                        None => {
                            conn.execute(
                                "INSERT OR IGNORE INTO thread_categories (thread_id, category_id, source, updated_at)
                                 VALUES (?1, NULL, 'rule', 0)",
                                rusqlite::params![tid],
                            )?;
                        }
                    }
                }
                Ok(out)
            })
            .await?;
        let count = matches.len() as i64;
        for (tid, m) in matches {
            for action in m.actions() {
                let kind = match action {
                    RuleAction::ApplyCategory(_) => continue,
                    RuleAction::Star => ActionKind::Star,
                    RuleAction::MarkRead => ActionKind::MarkRead,
                };
                if let Err(e) = self
                    .perform_action(PerformActionArgs {
                        kind,
                        thread_ids: vec![tid],
                        params: None,
                    })
                    .await
                {
                    tracing::warn!("category rule action failed for thread {tid}: {e}");
                }
            }
        }
        if count > 0 {
            self.bus.emit(CoreEvent::MailUpdated { thread_ids: vec![] });
        }
        Ok(count)
    }

    /// Ask the AI for category suggestions on threads no rule matched. Only
    /// writes pending rows to `ai_suggestions`; never changes a thread.
    pub async fn suggest_categories(&self, limit: i64) -> Result<usize> {
        let settings = self.db.read(|conn| repo::settings::get(conn)).await?;
        if !settings.ai_categorize {
            return Ok(0);
        }
        let Ok(cfg) = self.ai_config_from(&settings, Scenario::Categorize).await else {
            return Ok(0);
        };
        let limit = limit.clamp(1, category::suggest_llm::MAX_BATCH as i64);
        let (cats, items) = self
            .db
            .read(move |conn| {
                let cats: Vec<Category> = category::repo::list_categories(conn)?
                    .into_iter()
                    .filter(|c| c.enabled)
                    .collect();
                let ids: Vec<i64> = conn
                    .prepare(
                        "SELECT c.thread_id FROM thread_categories c
                         JOIN threads t ON t.id = c.thread_id
                         WHERE c.category_id IS NULL AND c.source = 'rule'
                           AND NOT EXISTS (SELECT 1 FROM ai_suggestions s
                                           WHERE s.thread_id = c.thread_id AND s.rule_id = ?1)
                         ORDER BY t.last_message_at DESC LIMIT ?2",
                    )?
                    .query_map(
                        rusqlite::params![crate::suggest::CATEGORY_SUGGESTION, limit],
                        |r| r.get::<_, i64>(0),
                    )?
                    .collect::<rusqlite::Result<_>>()?;
                let mut items = Vec::new();
                for tid in ids {
                    if let Some(f) = category::repo::thread_facts(conn, tid)? {
                        items.push(category::suggest_llm::SuggestInput {
                            thread_id: tid,
                            from: f.from_address,
                            subject: f.subject,
                            body: f.body,
                        });
                    }
                }
                Ok((cats, items))
            })
            .await?;
        if cats.is_empty() || items.is_empty() {
            return Ok(0);
        }
        let nonce = category::suggest_llm::new_nonce();
        let out = match ai::chat(&cfg, category::suggest_llm::prompt(&cats, &items, &nonce)).await {
            Ok(out) => out,
            Err(e) => {
                // Remote model unavailable: classification degrades to rules only.
                tracing::warn!("category suggestions unavailable: {e}");
                return Ok(0);
            }
        };
        let suggestions = category::suggest_llm::parse(&out, &cats, &items);
        let model = cfg.model.clone();
        let asked: Vec<i64> = items.iter().map(|i| i.thread_id).collect();
        self.db
            .write(move |conn| {
                let n = crate::suggest::record_category(conn, &suggestions, &model)?;
                // Threads the model had no category for are marked so they are
                // not re-sent on every pass (cost control).
                crate::suggest::mark_category_asked(conn, &asked, &model)?;
                Ok(n)
            })
            .await
    }

    pub async fn list_category_suggestions(
        &self,
        limit: i64,
    ) -> Result<Vec<crate::suggest::CategorySuggestionRow>> {
        self.db
            .read(move |conn| crate::suggest::list_pending_categories(conn, limit))
            .await
    }

    pub async fn reject_suggestion(&self, id: i64) -> Result<bool> {
        self.db
            .write(move |conn| crate::suggest::set_state(conn, id, "rejected"))
            .await
    }

    /// Explicit user command: apply a suggested category to its thread.
    pub async fn accept_category_suggestion(&self, id: i64) -> Result<bool> {
        self.accept_category_suggestion_as(id, None).await
    }

    /// Explicit user command from the review list: apply `chosen` (the
    /// category the user picked, which may differ from the AI's guess) or the
    /// suggested one. Only the category label is applied; nothing else runs.
    pub async fn accept_category_suggestion_as(
        &self,
        id: i64,
        chosen: Option<String>,
    ) -> Result<bool> {
        let acceptance = UserAcceptance { suggestion_id: id };
        let applied = self
            .db
            .write(move |conn| {
                let Some(row) = crate::suggest::get_category(conn, acceptance.suggestion_id)?
                else {
                    return Ok(false);
                };
                let category_id = chosen.unwrap_or_else(|| row.category_id.clone());
                if !category::repo::list_categories(conn)?
                    .iter()
                    .any(|c| c.id == category_id)
                {
                    return Err(CoreError::NotFound(format!("category {category_id}")));
                }
                category::repo::set_thread_category(
                    conn,
                    row.thread_id,
                    &category_id,
                    "user",
                    None,
                )?;
                if let Some(label) = category::repo::category_label(conn, &category_id)? {
                    route::apply_tab(conn, row.thread_id, Some(&format!("label:{label}")))?;
                }
                crate::suggest::set_state(conn, row.id, "accepted")
            })
            .await?;
        if applied {
            self.bus.emit(CoreEvent::MailUpdated { thread_ids: vec![] });
        }
        Ok(applied)
    }

    /// Display subject + snippet for suggestion review rows.
    pub async fn thread_titles(&self, thread_ids: Vec<i64>) -> Result<Vec<(i64, String)>> {
        self.db
            .read(move |conn| {
                let mut out = Vec::new();
                for id in thread_ids {
                    use rusqlite::OptionalExtension;
                    let t: Option<(Option<String>, String)> = conn
                        .query_row(
                            "SELECT subject_norm, snippet FROM threads WHERE id = ?1",
                            rusqlite::params![id],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .optional()?;
                    if let Some((subject, snippet)) = t {
                        let title = subject.filter(|s| !s.is_empty()).unwrap_or(snippet);
                        out.push((id, title));
                    }
                }
                Ok(out)
            })
            .await
    }
}
