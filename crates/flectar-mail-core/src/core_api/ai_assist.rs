//! `Core` methods: AI configuration and assistant commands.
//! Moved verbatim from `lib.rs`; see docs/fork.md.

use crate::*;

impl Core {
    pub(crate) async fn ai_config(&self, scenario: Scenario) -> Result<ai::AiConfig> {
        let settings = self.db.read(|conn| repo::settings::get(conn)).await?;
        self.ai_config_from(&settings, scenario).await
    }

    /// Build an [`ai::AiConfig`] for `scenario` from already-loaded settings,
    /// picking the model for the scenario's tier (all tiers share the base URL
    /// and stored API key).
    pub(crate) async fn ai_config_from(
        &self,
        settings: &Settings,
        scenario: Scenario,
    ) -> Result<ai::AiConfig> {
        let api_key =
            match credentials::load_async(self.credentials.clone(), 0, Slot::AiApiKey).await {
                Ok(k) => k,
                // Local endpoints (LM Studio, Ollama over http://) need no key;
                // hosted ones do, so fail early with a pointer to Settings.
                Err(_) if settings.ai_base_url.starts_with("http://") => String::new(),
                Err(_) => return Err(CoreError::AiNotConfigured),
            };
        Ok(ai::AiConfig {
            base_url: settings.ai_base_url.clone(),
            model: resolve_ai_model(settings, scenario),
            api_key,
            language: ai::ui_language_name(&settings.language).map(str::to_string),
            usage_sink: {
                let db = self.db.clone();
                let scenario = scenario.as_str().to_string();
                Some(Arc::new(move |usage: ai::AiUsage| {
                    let db = db.clone();
                    let scenario = scenario.clone();
                    if let Ok(handle) = tokio::runtime::Handle::try_current() {
                        handle.spawn(async move {
                            let result = db
                                .write(move |conn| {
                                    repo::ai_usage::record(
                                        conn,
                                        &repo::ai_usage::AiUsageRecord {
                                            occurred_at: now_ms(),
                                            model: &usage.model,
                                            scenario: &scenario,
                                            prompt_tokens: usage.prompt_tokens,
                                            completion_tokens: usage.completion_tokens,
                                            total_tokens: usage.total_tokens,
                                            exact: usage.exact,
                                        },
                                    )
                                })
                                .await;
                            if let Err(e) = result {
                                tracing::warn!("recording ai usage: {e}");
                            }
                        });
                    }
                }))
            },
        })
    }

    pub async fn ai_usage_stats(&self) -> Result<AiUsageStats> {
        self.db.read(|conn| repo::ai_usage::stats(conn)).await
    }

    pub async fn email_stats(&self) -> Result<EmailStats> {
        self.db.read(|conn| repo::email_stats::stats(conn)).await
    }

    /// Translate a plain-language automation request into a validated action
    /// plan. This only previews behavior; it never executes mailbox actions.
    pub async fn ai_plan_automation(&self, prompt: String) -> Result<AiAutomationPlan> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Ok(AiAutomationPlan {
                issues: vec![
                    "Describe the emails to match and what Flectar Mail should do.".into(),
                ],
                ..AiAutomationPlan::default()
            });
        }
        let (settings, labels, splits) = self
            .db
            .read(|conn| {
                Ok((
                    repo::settings::get(conn)?,
                    repo::labels::list(conn)?,
                    repo::splits::list(conn)?,
                ))
            })
            .await?;
        let cfg = self.ai_config_from(&settings, Scenario::Categorize).await?;
        let messages = route::automation_planner_prompt(prompt, &labels, &splits);
        let output = ai::chat(&cfg, messages).await?;
        Ok(route::validate_automation_plan(
            route::parse_automation_plan(&output),
            &labels,
            &splits,
        ))
    }

    pub async fn set_ai_key(&self, api_key: String) -> Result<()> {
        if api_key.trim().is_empty() {
            credentials::delete_all_async(self.credentials.clone(), 0).await?;
            return Ok(());
        }
        credentials::store_async(
            self.credentials.clone(),
            0,
            Slot::AiApiKey,
            api_key.trim().to_string(),
        )
        .await
    }

    pub async fn ai_status(&self) -> Result<AiStatus> {
        let settings = self.db.read(|conn| repo::settings::get(conn)).await?;
        let configured = credentials::load_async(self.credentials.clone(), 0, Slot::AiApiKey)
            .await
            .is_ok()
            || settings.ai_base_url.starts_with("http://");
        Ok(AiStatus {
            configured,
            model_instant: settings.ai_model_instant,
            model_cheap: settings.ai_model_cheap,
            model_intelligent: settings.ai_model_intelligent,
            base_url: settings.ai_base_url,
        })
    }

    /// Model ids from the configured endpoint. Works keyless on OpenRouter,
    /// so this is available before an API key is saved.
    pub async fn ai_list_models(&self) -> Result<Vec<String>> {
        let settings = self.db.read(|conn| repo::settings::get(conn)).await?;
        let api_key = credentials::load_async(self.credentials.clone(), 0, Slot::AiApiKey)
            .await
            .unwrap_or_default();
        ai::list_models(&settings.ai_base_url, &api_key).await
    }

    /// Parse a natural-language palette query ("meeting tomorrow 8pm ...")
    /// into a structured intent the UI can execute.
    pub async fn ai_command(&self, query: String) -> Result<AiIntent> {
        let cfg = self.ai_config(Scenario::Command).await?;
        ai::intent(&cfg, &query).await
    }

    pub async fn ai_summarize(&self, thread_id: i64) -> Result<crate::models::AiThreadSummary> {
        let cfg = self.ai_config(Scenario::Summarize).await?;
        let detail = self.get_thread(thread_id).await?;
        let context = ai::thread_context(&detail.messages, 24_000);
        let bus = self.bus.clone();
        ai::summarize_thread_stream(&cfg, &detail.thread.subject, &context, move |delta| {
            bus.emit(CoreEvent::AiSummaryDelta {
                thread_id,
                delta: delta.to_owned(),
            });
        })
        .await
    }

    /// Up to 3 short one-tap reply suggestions grounded in the thread, shown
    /// as chips in an empty reply composer. Runs on the instant tier: the
    /// chips are only useful if they appear before the user starts typing.
    pub async fn ai_quick_replies(&self, thread_id: i64) -> Result<Vec<String>> {
        let settings = self.db.read(|conn| repo::settings::get(conn)).await?;
        let cfg = self.ai_config_from(&settings, Scenario::QuickReply).await?;
        let detail = self.get_thread(thread_id).await?;
        let context = ai::thread_context(&detail.messages, 12_000);
        // Match the user's learned voice when voice drafting is enabled, same as
        // full AI drafts; otherwise pass an empty profile for the neutral prompt.
        let profile = if settings.voice_drafting {
            settings.voice_profile.as_str()
        } else {
            ""
        };
        ai::quick_replies(&cfg, &detail.thread.subject, &context, profile).await
    }

    /// Draft or rewrite email body text. With a thread, the reply is grounded
    /// in its content; without, it's freeform writing from the instruction.
    /// When `voice` (or the persisted setting) is on, the draft imitates the
    /// user's learned writing style and their similar past sent emails.
    pub async fn ai_draft(
        &self,
        thread_id: Option<i64>,
        reply_to_message_id: Option<i64>,
        instruction: String,
        sender_name: String,
        voice: Option<bool>,
        has_signature: bool,
    ) -> Result<String> {
        let settings = self.db.read(|conn| repo::settings::get(conn)).await?;
        let cfg = self.ai_config_from(&settings, Scenario::Draft).await?;
        let use_voice = voice.unwrap_or(settings.voice_drafting);

        let (subject, context, reply_target) = match thread_id {
            Some(tid) => {
                let detail = self.get_thread(tid).await?;
                (
                    detail.thread.subject.clone(),
                    ai::thread_context(&detail.messages, 24_000),
                    ai::reply_target_line(&detail.messages, reply_to_message_id),
                )
            }
            None => (String::new(), String::new(), String::new()),
        };

        if use_voice {
            let query = format!("{subject}\n{instruction}");
            let examples = self.voice_examples(&query, 3).await.unwrap_or_default();
            return ai::chat(
                &cfg,
                ai::apply_language(
                    ai::draft_prompt_voiced(
                        &subject,
                        &context,
                        &reply_target,
                        &instruction,
                        ai::VoiceDraftContext {
                            sender_name: &sender_name,
                            profile: &settings.voice_profile,
                            examples: &examples,
                            has_signature,
                        },
                    ),
                    &cfg,
                ),
            )
            .await;
        }

        ai::chat(
            &cfg,
            ai::apply_language(
                ai::draft_prompt(
                    &subject,
                    &context,
                    &reply_target,
                    &instruction,
                    &sender_name,
                    has_signature,
                ),
                &cfg,
            ),
        )
        .await
    }

    /// Copy-edit a draft body (plain text or simple HTML) without changing
    /// meaning, tone, or language. Returns the corrected draft.
    pub async fn ai_proofread(&self, body: String) -> Result<String> {
        let cfg = self.ai_config(Scenario::Draft).await?;
        ai::chat(&cfg, ai::proofread_prompt(&body)).await
    }

    /// Generate a clean email signature for an account from its name and
    /// address. Returns plain text with line breaks for the caller to render.
    pub async fn ai_signature(&self, name: String, email: String) -> Result<String> {
        let cfg = self.ai_config(Scenario::Draft).await?;
        ai::chat(
            &cfg,
            ai::apply_language(ai::signature_prompt(&name, &email), &cfg),
        )
        .await
    }

    /// Distill the user's writing voice from their sent mail and persist it as
    /// a style profile. Returns the profile text.
    pub async fn ai_learn_voice(&self) -> Result<String> {
        let cfg = self.ai_config(Scenario::Voice).await?;
        let samples = self
            .db
            .read(|conn| {
                let rows = repo::messages::list_sent_bodies(conn, None, 30)?;
                Ok::<_, CoreError>(
                    rows.into_iter()
                        .map(|(_, subj, body)| format!("Subject: {subj}\n{body}"))
                        .collect::<Vec<_>>(),
                )
            })
            .await?;
        if samples.is_empty() {
            return Err(CoreError::Other(
                "No sent emails to learn from yet. Send or sync some mail first.".into(),
            ));
        }
        let profile = ai::chat(&cfg, ai::voice_profile_prompt(&samples)).await?;

        let p = profile.clone();
        let now = now_ms();
        self.db
            .write(move |conn| {
                let mut s = repo::settings::get(conn)?;
                s.voice_profile = p;
                s.voice_learned_at = now;
                repo::settings::set(conn, &s)
            })
            .await?;
        Ok(profile)
    }

    /// Up to `k` (incoming → the user's reply) exchanges from their sent mail
    /// most relevant to `query`, for few-shot voice imitation. Prefers semantic
    /// retrieval; falls back to recent sent mail when the index is empty.
    pub(crate) async fn voice_examples(
        &self,
        query: &str,
        k: usize,
    ) -> Result<Vec<(String, String)>> {
        let hits = self.vector_hits(query, 40).await.unwrap_or_default();
        let hit_ids: Vec<i64> = hits.iter().map(|(id, _)| *id).collect();
        self.db
            .read(move |conn| {
                let sent_ids = repo::messages::filter_sent(conn, &hit_ids)?;
                let mut out: Vec<(String, String)> = Vec::new();
                for mid in sent_ids {
                    if out.len() >= k {
                        break;
                    }
                    if let Some(pair) = build_example_pair(conn, mid)? {
                        out.push(pair);
                    }
                }
                if out.is_empty() {
                    // No index / no similar sent mail: use recent sent as exemplars.
                    for (_, subject, body) in
                        repo::messages::list_sent_bodies(conn, None, k as i64)?
                    {
                        out.push((format!("(Compose a new email. Subject: {subject})"), body));
                    }
                }
                Ok::<_, CoreError>(out)
            })
            .await
    }
}
