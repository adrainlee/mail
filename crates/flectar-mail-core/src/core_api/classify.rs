//! `Core` methods: AI categorization (suggest-only).
//! Moved verbatim from `lib.rs`; see docs/fork.md.

use crate::*;

impl Core {
    /// Classify up to `limit` pending threads. With no custom automations this
    /// preserves the low-cost sender-domain category cache. Compound rules are
    /// evaluated per message (their conditions may depend on subject/body).
    /// Siftmail: matched rules are recorded as pending suggestions only; their
    /// configured actions are never executed from AI output.
    pub async fn classify_pending(&self, limit: i64) -> Result<i64> {
        let _guard = self.ai_router_lock.lock().await;
        let settings = self.db.read(|conn| repo::settings::get(conn)).await?;
        if !settings.ai_categorize {
            return Ok(0);
        }
        let cfg = match self.ai_config_from(&settings, Scenario::Categorize).await {
            Ok(c) => c,
            // No key: leave threads 'pending' (still shown in Important/Other).
            Err(_) => return Ok(0),
        };

        let (jobs, cache) = self
            .db
            .read(move |conn| {
                Ok((
                    route::pending_threads(conn, limit)?,
                    route::load_cache(conn)?,
                ))
            })
            .await?;
        if jobs.is_empty() {
            return Ok(0);
        }

        let prompt = settings.ai_category_prompt.clone();
        let rules: Vec<AiAutomationRule> = settings
            .ai_automation_rules
            .iter()
            .filter(|rule| rule.enabled)
            .cloned()
            .collect();
        let compound = !rules.is_empty();
        let model = cfg.model.clone();
        // Resolved (thread id, category keyword). The category only selects a
        // local view tab; it never mutates the mailbox.
        let mut resolved: Vec<(i64, Option<String>)> = Vec::new();
        // Siftmail: AI may only suggest. Rules the model selects become
        // pending, non-executable suggestions (see `crate::suggest`).
        let mut suggestions: Vec<suggest::Suggestion> = Vec::new();
        let mut decided: std::collections::HashMap<String, Option<autolabel::Category>> =
            std::collections::HashMap::new();
        let mut cache_writes: Vec<(String, String)> = Vec::new();

        for (tid, facts) in jobs {
            let domain = route::sender_domain(&facts.from_addr);
            if compound {
                let msgs = route::automation_prompt(
                    &prompt,
                    &rules,
                    &facts.from_addr,
                    &facts.subject,
                    &facts.snippet,
                );
                match ai::chat(&cfg, msgs).await {
                    Ok(out) => {
                        let decision = route::parse_automation_decision(&out);
                        suggestions.extend(route::suggestions_from_decision(
                            tid, &decision, &rules, &model,
                        ));
                        resolved.push((tid, decision.category.map(|c| c.keyword().to_string())));
                    }
                    Err(e) => {
                        tracing::warn!("ai automation failed for thread {tid}: {e}");
                    }
                }
                continue;
            }

            let category: Option<autolabel::Category> = if let Some(hit) = cache.get(&domain) {
                // Cache stores the category keyword, "" = no category.
                autolabel::Category::from_keyword(hit)
            } else if let Some(d) = decided.get(&domain) {
                *d
            } else {
                let msgs = route::category_prompt(
                    &prompt,
                    &facts.from_addr,
                    &facts.subject,
                    &facts.snippet,
                );
                match ai::chat(&cfg, msgs).await {
                    Ok(out) => {
                        let cat = route::parse_category(&out);
                        decided.insert(domain.clone(), cat);
                        // Persist the decision keyed by domain ("" = no category).
                        let key = cat.map(|c| c.keyword().to_string()).unwrap_or_default();
                        cache_writes.push((domain.clone(), key));
                        cat
                    }
                    // Transient failure: leave this thread 'pending' so it retries
                    // on the next pass, and don't poison the cache for the sender.
                    Err(e) => {
                        tracing::warn!("ai categorize failed for thread {tid}: {e}");
                        continue;
                    }
                }
            };
            resolved.push((tid, category.map(|c| c.keyword().to_string())));
        }

        // One write pass: category tab routing + pending suggestions. No AI
        // output reaches perform_action, labels, flags or local annotations.
        let count = self
            .db
            .write(move |conn| {
                use rusqlite::OptionalExtension;
                for (domain, key) in &cache_writes {
                    route::cache_put(conn, domain, key)?;
                }
                let mut count = 0i64;
                for (tid, kw) in &resolved {
                    let key = match kw {
                        Some(k) => {
                            let id: Option<i64> = conn
                                .query_row(
                                    "SELECT id FROM labels WHERE keyword = ?1 AND is_auto = 1",
                                    rusqlite::params![k],
                                    |r| r.get(0),
                                )
                                .optional()?;
                            id.map(|id| format!("label:{id}"))
                        }
                        None => None,
                    };
                    route::apply_tab(conn, *tid, key.as_deref())?;
                    count += 1;
                }
                suggest::record(conn, &suggestions)?;
                Ok(count)
            })
            .await?;

        self.bus.emit(CoreEvent::MailUpdated { thread_ids: vec![] });
        Ok(count)
    }
}
