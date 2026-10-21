//! `Core` methods: Search, embeddings and Ask.
//! Moved verbatim from `lib.rs`; see docs/fork.md.

use crate::*;

impl Core {
    pub async fn search(
        &self,
        query: String,
        account_id: Option<i64>,
        limit: i64,
    ) -> Result<Vec<ThreadSummary>> {
        let mut parsed = search::parse(&query);
        // Scope every branch (lexical, semantic, operator filters) to the UI's
        // active account when one is selected; None searches all accounts.
        parsed.account_id = account_id;
        let limit = limit.clamp(1, 100);
        let t0 = std::time::Instant::now();

        // The lexical DB read and the semantic branch (a CPU-bound model
        // forward pass + KNN) are independent - run them concurrently so
        // latency is max(lexical, semantic), not their sum. Semantic is
        // best-effort and skipped for queries too short to carry meaning.
        let lex_fut = {
            let q = parsed.clone();
            self.db.read(move |conn| {
                repo::search::lexical_thread_ids(conn, &q, repo::search::candidate_cap(limit))
            })
        };
        let vec_fut = async {
            if parsed.text.chars().count() < 3 {
                Vec::new()
            } else {
                self.vector_hits(&parsed.text, 200)
                    .await
                    .unwrap_or_default()
            }
        };
        let (lexical, vec_hits) = tokio::join!(lex_fut, vec_fut);
        let lexical = lexical?;
        let t_branches = t0.elapsed();

        let parsed2 = parsed.clone();
        let out = self
            .db
            .read(move |conn| repo::search::fuse(conn, &parsed2, lexical, &vec_hits, limit))
            .await;
        tracing::debug!(
            "search '{}': branches {:?}, fuse+hydrate {:?}",
            parsed.text,
            t_branches,
            t0.elapsed() - t_branches
        );
        out
    }

    /// Search for a chronological mail timeline. This keeps relevance-ranked
    /// search available to assistants and retrieval callers while giving the
    /// interactive mail list the newest matching threads first.
    pub async fn search_chronological(
        &self,
        query: String,
        account_id: Option<i64>,
        limit: i64,
    ) -> Result<Vec<ThreadSummary>> {
        Ok(self
            .search_chronological_page(query, account_id, None, limit)
            .await?
            .threads)
    }

    /// Cursor-paginated chronological search for the native mail list.
    pub async fn search_chronological_page(
        &self,
        query: String,
        account_id: Option<i64>,
        cursor: Option<models::SearchCursor>,
        limit: i64,
    ) -> Result<models::SearchPage> {
        let mut parsed = search::parse(&query);
        parsed.account_id = account_id;
        let limit = limit.clamp(1, 100);
        self.db
            .read(move |conn| repo::search::chronological_page(conn, &parsed, cursor, limit))
            .await
    }

    /// Embed `text` as a query and return the top-`k` (message_id, score) hits
    /// from the in-memory index. Empty when no local model is loaded. Query
    /// embeddings are cached, so repeated or backspaced-over queries skip the
    /// model forward pass entirely.
    #[cfg(feature = "local-embeddings")]
    pub(crate) async fn vector_hits(&self, text: &str, k: usize) -> Result<Vec<(i64, f32)>> {
        let Some(embedder) = self.embed.embedder().await else {
            return Ok(Vec::new());
        };
        let qv = match self.embed.cached_query(text).await {
            Some(v) => v,
            None => {
                let t = text.to_string();
                let v = tokio::task::spawn_blocking(move || embedder.embed_query(&t))
                    .await
                    .map_err(|e| CoreError::Other(format!("embed query join: {e}")))??;
                self.embed.cache_query(text.to_string(), v.clone()).await;
                v
            }
        };
        let idx = self.embed.index.read().await;
        Ok(idx.search(&qv, k))
    }

    #[cfg(not(feature = "local-embeddings"))]
    pub(crate) async fn vector_hits(&self, _text: &str, _k: usize) -> Result<Vec<(i64, f32)>> {
        Ok(Vec::new())
    }

    /// Pre-compute and cache the query embedding for `query` while the user is
    /// still typing, so the search that fires when they pause skips the model
    /// forward pass. Best-effort: no-ops when the model isn't loaded, the
    /// query is too short for the semantic branch, or it's already cached.
    #[cfg(feature = "local-embeddings")]
    pub async fn warm_query_embedding(&self, query: String) {
        let parsed = search::parse(&query);
        if parsed.text.chars().count() < 3 {
            return;
        }
        let Some(embedder) = self.embed.embedder().await else {
            return;
        };
        if self.embed.cached_query(&parsed.text).await.is_some() {
            return;
        }
        let t = parsed.text.clone();
        if let Ok(Ok(v)) = tokio::task::spawn_blocking(move || embedder.embed_query(&t)).await {
            self.embed.cache_query(parsed.text, v).await;
        }
    }

    #[cfg(not(feature = "local-embeddings"))]
    pub async fn warm_query_embedding(&self, _query: String) {}

    pub async fn embedding_status(&self) -> Result<EmbeddingStatus> {
        let settings = self.db.read(|conn| repo::settings::get(conn)).await?;
        let enabled = settings.embedding_backend == "local";
        let model = settings.embedding_model.clone();
        let model_id = embed::spec_or_default(&model).key.to_string();
        let m = model_id.clone();
        let (total, embedded, pending) = self
            .db
            .read(move |conn| repo::embeddings::counts(conn, &m))
            .await?;
        #[cfg(feature = "local-embeddings")]
        let ready = self.embed.embedder().await.is_some()
            && *self.embed.active_model.read().await == model_id;
        #[cfg(not(feature = "local-embeddings"))]
        let ready = false;
        Ok(EmbeddingStatus {
            enabled,
            model: model_id,
            total,
            embedded,
            pending,
            ready,
        })
    }

    /// Requeue the whole mailbox for (re-)embedding. The worker drains it.
    pub async fn semantic_reindex(&self) -> Result<i64> {
        let n = self
            .db
            .write(|conn| repo::embeddings::mark_all_pending(conn))
            .await?;
        Ok(n as i64)
    }

    /// RAG: answer a natural-language question grounded in the most relevant
    /// messages, returning the answer plus its source citations.
    /// Answer a question about the mailbox. Seeds with a semantic-search RAG
    /// pass, then hands the model a `search_inbox` tool so it can reformulate
    /// queries and dig deeper on its own before answering. Falls back to a plain
    /// one-shot RAG answer if the model/endpoint doesn't support tool calling.
    pub async fn ai_ask(&self, question: String, request_id: String) -> Result<AskResult> {
        const MAX_ROUNDS: usize = 4;
        let cfg = self.ai_config(Scenario::Ask).await?;

        // RAG seed: the model always starts from the best hybrid matches.
        let mut sources = self.retrieve_search(&question, 8).await?;
        if sources.is_empty() {
            return Ok(AskResult {
                answer: "I couldn't find anything relevant in your mailbox. \
                         Make sure semantic search is enabled and indexing has finished."
                    .into(),
                citations: Vec::new(),
            });
        }
        // Surface the seed sources immediately; more are emitted as the model searches.
        self.emit_ask_citations(&request_id, &sources);

        let mut initial_context = String::new();
        for (i, m) in sources.iter().enumerate() {
            initial_context.push_str(&ai::format_excerpt(i + 1, m));
        }
        let ask_system = format!("{}{}", ai::AGENTIC_ASK_SYSTEM, ai::language_directive(&cfg));
        let mut messages: Vec<serde_json::Value> = vec![
            serde_json::json!({ "role": "system", "content": ask_system }),
            serde_json::json!({
                "role": "user",
                "content": format!("Emails:\n\n{initial_context}\nQuestion: {question}"),
            }),
        ];
        let tools = ai::search_inbox_tool();

        // Agentic loop: let the model call search_inbox until it answers or we
        // hit the round cap. `answer = Some` means the model produced text.
        let mut answer: Option<String> = None;
        for round in 0..MAX_ROUNDS {
            match ai::chat_tools(&cfg, messages.clone(), tools.clone()).await {
                Ok(ai::ChatStep::Content(text)) => {
                    answer = Some(text);
                    break;
                }
                Ok(ai::ChatStep::Tools { assistant, calls }) => {
                    messages.push(assistant);
                    for call in calls {
                        let (result, added) = if call.name == "search_inbox" {
                            self.run_search_inbox(&call.arguments, &mut sources).await
                        } else {
                            (format!("Unknown tool: {}", call.name), 0)
                        };
                        messages.push(serde_json::json!({
                            "role": "tool",
                            "tool_call_id": call.id,
                            "content": result,
                        }));
                        if added > 0 {
                            self.emit_ask_citations(&request_id, &sources);
                        }
                    }
                }
                Err(e) => {
                    // Any tool-round failure (no tool support, a provider hiccup,
                    // a malformed tool reply) shouldn't sink the whole Ask - we
                    // already have grounded sources, so fall through to a plain
                    // streamed answer over them instead of erroring out.
                    tracing::warn!("ai_ask tool round {round} failed, using plain fallback: {e}");
                    break;
                }
            }
        }

        let answer = match answer {
            // The agentic path answered directly (chat_tools is non-streaming, so
            // emit its text as one delta). Empty answers fall through to the
            // streamed fallback below rather than settling on a blank result.
            Some(text) if !text.trim().is_empty() => {
                self.bus.emit(CoreEvent::AskDelta {
                    request_id: request_id.clone(),
                    delta: text.clone(),
                });
                text
            }
            _ => {
                // Cap reached while still searching, a tool-less model, or a
                // mid-loop failure: force a final streamed answer over everything
                // gathered, tools off. Reasoning is streamed on its own channel.
                messages.push(serde_json::json!({
                    "role": "system",
                    "content": "Now answer the user's question using ONLY the numbered excerpts \
                                above. Cite them like [1]. If the answer isn't there, say you \
                                couldn't find it. Answer in the user's language, concisely; light \
                                Markdown is fine, no preamble.",
                }));
                let (bus_a, rid_a) = (self.bus.clone(), request_id.clone());
                let (bus_r, rid_r) = (self.bus.clone(), request_id.clone());
                let (answer, _reasoning) = ai::chat_stream_json_split(
                    &cfg,
                    messages,
                    move |delta| {
                        bus_a.emit(CoreEvent::AskDelta {
                            request_id: rid_a.clone(),
                            delta: delta.to_string(),
                        });
                    },
                    move |delta| {
                        bus_r.emit(CoreEvent::AskReasoning {
                            request_id: rid_r.clone(),
                            delta: delta.to_string(),
                        });
                    },
                )
                .await?;
                answer
            }
        };
        // Never settle on a blank answer - the model thought but produced no
        // user-facing text.
        let answer = if answer.trim().is_empty() {
            "I couldn't find an answer to that in your mailbox.".to_string()
        } else {
            answer
        };
        self.bus.emit(CoreEvent::AskDone { request_id });

        Ok(AskResult {
            answer,
            citations: Self::ask_citations(&sources),
        })
    }

    /// Operator-aware hybrid retrieval (semantic RAG fused with `from:`/`to:`/
    /// `subject:`/`is:`/`has:` keyword filters) hydrated to message details for
    /// citation. Powers both the Ask RAG seed and the agentic `search_inbox`
    /// tool, so the model can search by meaning, sender, recipient, and more.
    pub(crate) async fn retrieve_search(
        &self,
        query: &str,
        k: usize,
    ) -> Result<Vec<MessageDetail>> {
        let parsed = crate::search::parse(query);
        // Semantic branch only carries meaning for queries of a few chars+.
        let vec_hits = if parsed.text.chars().count() >= 3 {
            self.vector_hits(&parsed.text, 200)
                .await
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let k = k as i64;
        self.db
            .read(move |conn| {
                let ids = repo::search::message_hits(conn, &parsed, &vec_hits, k)?;
                let mut out = Vec::new();
                for id in ids {
                    if let Ok(d) = repo::messages::detail(conn, id) {
                        out.push(d);
                    }
                }
                Ok::<_, CoreError>(out)
            })
            .await
    }

    /// Execute a `search_inbox` tool call: run the search, append any *new*
    /// messages to `sources` with stable citation numbers, and return the
    /// excerpt block for the model plus how many new sources were added.
    pub(crate) async fn run_search_inbox(
        &self,
        arguments: &str,
        sources: &mut Vec<MessageDetail>,
    ) -> (String, usize) {
        let args: serde_json::Value =
            serde_json::from_str(arguments).unwrap_or_else(|_| serde_json::json!({}));
        let query = args["query"].as_str().unwrap_or("").trim().to_string();
        if query.is_empty() {
            return ("(empty query - nothing searched)".into(), 0);
        }
        if sources.len() >= 24 {
            return (
                "Source limit reached; answer with what you already have.".into(),
                0,
            );
        }
        let limit = args["limit"].as_u64().unwrap_or(6).clamp(1, 8) as usize;
        let details = self
            .retrieve_search(&query, limit)
            .await
            .unwrap_or_default();

        let mut block = String::new();
        let mut added = 0;
        for d in details {
            if sources.iter().any(|s| s.id == d.id) {
                continue; // already cited under an earlier number
            }
            block.push_str(&ai::format_excerpt(sources.len() + 1, &d));
            sources.push(d);
            added += 1;
            if sources.len() >= 24 {
                break;
            }
        }
        let text = if added == 0 {
            format!("No new results for \"{query}\".")
        } else {
            format!("Results for \"{query}\":\n\n{block}")
        };
        (text, added)
    }

    pub(crate) fn emit_ask_citations(&self, request_id: &str, sources: &[MessageDetail]) {
        self.bus.emit(CoreEvent::AskCitations {
            request_id: request_id.to_string(),
            citations: Self::ask_citations(sources),
        });
    }

    pub(crate) fn ask_citations(sources: &[MessageDetail]) -> Vec<AskCitation> {
        sources
            .iter()
            .map(|d| AskCitation {
                message_id: d.id,
                thread_id: d.thread_id,
                subject: d.subject.clone(),
                from: d.from.name.clone().unwrap_or_else(|| d.from.email.clone()),
                date: d.date,
                snippet: d.snippet.clone(),
            })
            .collect()
    }
}
