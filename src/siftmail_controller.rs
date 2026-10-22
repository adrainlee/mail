//! Siftmail settings pages (scheduler status, categories, proxies,
//! embeddings) and the translation bar. UI state lives in the
//! `SiftmailBridge` Slint global; every core call runs on the tokio runtime
//! and applies its result back on the UI thread.

use super::*;
use flectar_mail_core::Core;
use std::sync::Arc;

pub(super) struct Ctx {
    pub app: slint::Weak<AppWindow>,
    pub state: Rc<RefCell<InboxState>>,
    pub runtime: Rc<tokio::runtime::Runtime>,
}

impl Clone for Ctx {
    fn clone(&self) -> Self {
        Self {
            app: self.app.clone(),
            state: Rc::clone(&self.state),
            runtime: Rc::clone(&self.runtime),
        }
    }
}

impl Ctx {
    pub fn core(&self) -> Option<Arc<Core>> {
        self.state
            .borrow()
            .core
            .as_ref()
            .map(|c| c.account_preferences_core())
    }

    /// Run `work(core)` off the UI thread, then `apply` on it.
    pub fn spawn<T, F, Fut>(&self, work: F, apply: impl FnOnce(AppWindow, T) + 'static)
    where
        T: Send + 'static,
        F: FnOnce(Arc<Core>) -> Fut,
        Fut: std::future::Future<Output = Result<T, String>> + Send + 'static,
    {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let Some(core) = self.core() else {
            app.global::<SiftmailBridge>()
                .set_status("The mail core is not running yet.".into());
            return;
        };
        let task = self.runtime.spawn(work(core));
        let weak = self.app.clone();
        let _ = slint::spawn_local(async move {
            let result = task.await.unwrap_or_else(|e| Err(e.to_string()));
            let Some(app) = weak.upgrade() else { return };
            match result {
                Ok(v) => apply(app, v),
                Err(e) => app.global::<SiftmailBridge>().set_status(e.into()),
            }
        });
    }
}

pub(super) fn format_remaining(ms: i64) -> String {
    let mins = (ms + 59_999) / 60_000;
    if mins >= 60 {
        format!("{} h {} min", mins / 60, mins % 60)
    } else {
        format!("{mins} min")
    }
}

fn refresh_scheduler(ctx: &Ctx) {
    ctx.spawn(
        |core| async move { core.sync_scheduler_status().await.map_err(|e| e.to_string()) },
        |app, rows| {
            let rows: Vec<SchedulerRow> = rows
                .into_iter()
                .map(|r| SchedulerRow {
                    provider: r.provider.into(),
                    accounts: r.account_emails.join(", ").into(),
                    level: r.backoff.as_ref().map_or(0, |b| b.level as i32),
                    remaining: format_remaining(r.remaining_ms).into(),
                    last_error: r
                        .backoff
                        .map(|b| b.last_error)
                        .unwrap_or_default()
                        .into(),
                    backing_off: r.remaining_ms > 0,
                })
                .collect();
            app.global::<SiftmailBridge>()
                .set_scheduler_rows(Rc::new(slint::VecModel::from(rows)).into());
        },
    );
}

// ---- categories -------------------------------------------------------

use flectar_mail_core::category::{Category, Condition, MatchField, MatchMode, MatchOp, Rule};

pub(super) const RULE_FIELDS: [MatchField; 6] = [
    MatchField::Subject,
    MatchField::Body,
    MatchField::FromAddress,
    MatchField::FromDomain,
    MatchField::FromName,
    MatchField::To,
];
pub(super) const RULE_OPS: [MatchOp; 4] = [
    MatchOp::Contains,
    MatchOp::Equals,
    MatchOp::Wildcard,
    MatchOp::Regex,
];

/// Stable id for a new category: an ASCII slug of the name, or a time-based
/// id for names without ASCII letters (e.g. Chinese).
pub(super) fn category_id_for(name: &str, now_ms: i64) -> String {
    let mut slug = String::new();
    for c in name.trim().to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-').chars().take(40).collect::<String>();
    if slug.is_empty() {
        format!("c-{now_ms}")
    } else {
        slug
    }
}

/// A rule the simple editor can represent: one condition, a known field/op.
pub(super) fn simple_rule_parts(rule: &Rule) -> Option<(usize, usize, String)> {
    let [c] = rule.conditions.as_slice() else {
        return None;
    };
    if c.negate {
        return None;
    }
    let f = RULE_FIELDS.iter().position(|x| *x == c.field)?;
    let o = RULE_OPS.iter().position(|x| *x == c.op)?;
    Some((f, o, c.value.clone()))
}

pub(super) fn build_rule(
    id: i64,
    category_id: String,
    name: String,
    field: usize,
    op: usize,
    value: String,
    enabled: bool,
) -> Option<Rule> {
    let value = value.trim().to_string();
    if value.is_empty() {
        return None;
    }
    Some(Rule {
        id,
        name: if name.trim().is_empty() {
            value.clone()
        } else {
            name.trim().to_string()
        },
        category_id,
        priority: 100,
        enabled,
        mode: MatchMode::All,
        conditions: vec![Condition {
            field: *RULE_FIELDS.get(field)?,
            op: *RULE_OPS.get(op)?,
            value,
            case_sensitive: false,
            negate: false,
        }],
        also_star: false,
        also_mark_read: false,
    })
}

struct CategoryData {
    categories: Vec<Category>,
    rules: Vec<Rule>,
    suggestions: Vec<flectar_mail_core::suggest::CategorySuggestionRow>,
    titles: std::collections::HashMap<i64, String>,
}

fn refresh_categories(ctx: &Ctx) {
    ctx.spawn(
        |core| async move {
            let e = |e: flectar_mail_core::error::CoreError| e.to_string();
            let categories = core.list_categories().await.map_err(e)?;
            let rules = core.list_category_rules().await.map_err(e)?;
            let suggestions = core.list_category_suggestions(100).await.map_err(e)?;
            let titles = core
                .thread_titles(suggestions.iter().map(|s| s.thread_id).collect())
                .await
                .map_err(e)?
                .into_iter()
                .collect();
            Ok(CategoryData {
                categories,
                rules,
                suggestions,
                titles,
            })
        },
        |app, d| {
            let b = app.global::<SiftmailBridge>();
            let index = |id: &str| {
                d.categories
                    .iter()
                    .position(|c| c.id == id)
                    .map_or(0, |i| i as i32)
            };
            let name = |id: &str| {
                d.categories
                    .iter()
                    .find(|c| c.id == id)
                    .map_or(id.to_string(), |c| c.name.clone())
            };
            let cats: Vec<CategoryItem> = d
                .categories
                .iter()
                .map(|c| CategoryItem {
                    id: c.id.clone().into(),
                    name: c.name.clone().into(),
                    description: c.description.clone().into(),
                })
                .collect();
            let names: Vec<slint::SharedString> =
                d.categories.iter().map(|c| c.name.clone().into()).collect();
            let rules: Vec<RuleItem> = d
                .rules
                .iter()
                .map(|r| {
                    let simple = simple_rule_parts(r);
                    let summary = r
                        .conditions
                        .iter()
                        .map(|c| format!("{} {} \"{}\"", c.field.as_str(), c.op.as_str(), c.value))
                        .collect::<Vec<_>>()
                        .join(if r.mode == MatchMode::All { " AND " } else { " OR " });
                    let (f, o, v) = simple.clone().unwrap_or((0, 0, String::new()));
                    RuleItem {
                        id: r.id as i32,
                        category_index: index(&r.category_id),
                        category_name: name(&r.category_id).into(),
                        name: r.name.clone().into(),
                        field_index: f as i32,
                        op_index: o as i32,
                        value: v.into(),
                        summary: summary.into(),
                        editable: simple.is_some(),
                        enabled: r.enabled,
                    }
                })
                .collect();
            let sugg: Vec<SuggestionItem> = d
                .suggestions
                .iter()
                .map(|s| SuggestionItem {
                    id: s.id as i32,
                    title: d
                        .titles
                        .get(&s.thread_id)
                        .cloned()
                        .unwrap_or_else(|| format!("#{}", s.thread_id))
                        .into(),
                    category_index: index(&s.category_id),
                    category_name: name(&s.category_id).into(),
                    confidence: format!("{:.0}%", s.confidence * 100.0).into(),
                    rationale: s.rationale.clone().into(),
                })
                .collect();
            b.set_categories(Rc::new(slint::VecModel::from(cats)).into());
            b.set_category_names(Rc::new(slint::VecModel::from(names)).into());
            b.set_rules(Rc::new(slint::VecModel::from(rules)).into());
            b.set_suggestions(Rc::new(slint::VecModel::from(sugg)).into());
        },
    );
}

/// Run a core mutation, report `done` and reload the categories page.
fn mutate_categories<F, Fut>(ctx: &Ctx, done: &'static str, work: F)
where
    F: FnOnce(Arc<Core>) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
{
    let again = ctx.clone();
    ctx.spawn(work, move |app, ()| {
        app.global::<SiftmailBridge>().set_status(done.into());
        refresh_categories(&again);
    });
}

fn category_id_at(ctx: &Ctx, index: i32) -> Option<String> {
    let app = ctx.app.upgrade()?;
    let cats = app.global::<SiftmailBridge>().get_categories();
    slint::Model::row_data(&cats, usize::try_from(index).ok()?).map(|c| c.id.to_string())
}

fn register_categories(ctx: &Ctx, bridge: &SiftmailBridge) {
    let c = ctx.clone();
    bridge.on_refresh_categories(move || refresh_categories(&c));

    let c = ctx.clone();
    bridge.on_save_category(move |id, name, description| {
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        let description = description.to_string();
        let id = id.to_string();
        mutate_categories(&c, "Category saved.", move |core| async move {
            let e = |e: flectar_mail_core::error::CoreError| e.to_string();
            let existing = core.list_categories().await.map_err(e)?;
            let category = match existing.into_iter().find(|x| x.id == id) {
                Some(mut x) => {
                    x.name = name;
                    x.description = description;
                    x
                }
                None => {
                    let json = serde_json::json!({
                        "id": category_id_for(&name, chrono::Utc::now().timestamp_millis()),
                        "name": name,
                        "description": description,
                    });
                    serde_json::from_value(json).map_err(|e| e.to_string())?
                }
            };
            core.save_category(category).await.map_err(e)
        });
    });

    let c = ctx.clone();
    bridge.on_delete_category(move |id| {
        let id = id.to_string();
        mutate_categories(&c, "Category deleted.", move |core| async move {
            core.delete_category(id).await.map(|_| ()).map_err(|e| e.to_string())
        });
    });

    let c = ctx.clone();
    bridge.on_save_rule(move |id, cat, name, field, op, value, enabled| {
        let Some(category_id) = category_id_at(&c, cat) else {
            return;
        };
        let Some(rule) = build_rule(
            i64::from(id),
            category_id,
            name.to_string(),
            field.max(0) as usize,
            op.max(0) as usize,
            value.to_string(),
            enabled,
        ) else {
            return;
        };
        mutate_categories(&c, "Rule saved.", move |core| async move {
            core.save_category_rule(rule)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
    });

    let c = ctx.clone();
    bridge.on_delete_rule(move |id| {
        mutate_categories(&c, "Rule deleted.", move |core| async move {
            core.delete_category_rule(i64::from(id))
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
    });

    let c = ctx.clone();
    bridge.on_apply_rules_now(move || {
        mutate_categories(&c, "Rules applied to recent mail.", |core| async move {
            core.apply_category_rules(500)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
    });

    let c = ctx.clone();
    bridge.on_run_ai_suggestions(move || {
        mutate_categories(&c, "AI suggestions updated.", |core| async move {
            core.suggest_categories(50)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
    });

    let c = ctx.clone();
    bridge.on_accept_suggestion(move |id, cat| {
        let chosen = category_id_at(&c, cat);
        mutate_categories(&c, "Category applied.", move |core| async move {
            core.accept_category_suggestion_as(i64::from(id), chosen)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
    });

    let c = ctx.clone();
    bridge.on_reject_suggestion(move |id| {
        mutate_categories(&c, "Suggestion rejected.", move |core| async move {
            core.reject_suggestion(i64::from(id))
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
    });
}

// ---- proxies ----------------------------------------------------------

use flectar_mail_core::net::proxy::{ProxyConfig, ProxyKind, ProxyMode};

pub(super) fn mode_index(mode: ProxyMode) -> i32 {
    match mode {
        ProxyMode::Direct => 0,
        ProxyMode::Global => 1,
        ProxyMode::Custom => 2,
    }
}

pub(super) fn mode_at(index: i32) -> ProxyMode {
    match index {
        0 => ProxyMode::Direct,
        2 => ProxyMode::Custom,
        _ => ProxyMode::Global,
    }
}

struct ProxyData {
    proxies: Vec<(ProxyConfig, bool)>,
    global: Option<i64>,
    accounts: Vec<flectar_mail_core::models::AccountConfig>,
}

fn refresh_proxies(ctx: &Ctx) {
    ctx.spawn(
        |core| async move {
            let e = |e: flectar_mail_core::error::CoreError| e.to_string();
            Ok(ProxyData {
                proxies: core.list_proxies().await.map_err(e)?,
                global: core.global_proxy_id().await.map_err(e)?,
                accounts: core.list_account_configs().await.map_err(e)?,
            })
        },
        |app, d| {
            let b = app.global::<SiftmailBridge>();
            // proxy-names: index 0 = none, n = proxies[n - 1].
            let pos = |id: Option<i64>| {
                id.and_then(|id| d.proxies.iter().position(|(p, _)| p.id == id))
                    .map_or(0, |i| i as i32 + 1)
            };
            let mut names: Vec<slint::SharedString> = vec!["—".into()];
            names.extend(d.proxies.iter().map(|(p, _)| {
                let label = if p.name.is_empty() {
                    format!("{}:{}", p.host, p.port)
                } else {
                    p.name.clone()
                };
                slint::SharedString::from(label)
            }));
            let items: Vec<ProxyItem> = d
                .proxies
                .iter()
                .map(|(p, has)| ProxyItem {
                    id: p.id as i32,
                    name: p.name.clone().into(),
                    kind_index: i32::from(p.kind == ProxyKind::HttpConnect),
                    host: p.host.clone().into(),
                    port: p.port.to_string().into(),
                    username: p.username.clone().into(),
                    has_password: *has,
                })
                .collect();
            let accounts: Vec<AccountProxyItem> = d
                .accounts
                .iter()
                .map(|a| AccountProxyItem {
                    account_id: a.id as i32,
                    email: a.email.clone().into(),
                    mode_index: mode_index(flectar_mail_core::net::proxy::effective_mode(
                        &a.settings.connection,
                    )),
                    proxy_index: pos(a.settings.connection.proxy_id),
                })
                .collect();
            b.set_proxies(Rc::new(slint::VecModel::from(items)).into());
            b.set_proxy_names(Rc::new(slint::VecModel::from(names)).into());
            b.set_global_proxy_index(pos(d.global));
            b.set_account_proxies(Rc::new(slint::VecModel::from(accounts)).into());
        },
    );
}

/// Proxy id for a `proxy-names` index (0 = none).
fn proxy_id_at(ctx: &Ctx, index: i32) -> Option<i64> {
    let app = ctx.app.upgrade()?;
    let list = app.global::<SiftmailBridge>().get_proxies();
    let i = usize::try_from(index).ok()?.checked_sub(1)?;
    slint::Model::row_data(&list, i).map(|p| i64::from(p.id))
}

fn mutate_proxies<F, Fut>(ctx: &Ctx, done: &'static str, work: F)
where
    F: FnOnce(Arc<Core>) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
{
    let again = ctx.clone();
    ctx.spawn(work, move |app, ()| {
        app.global::<SiftmailBridge>().set_status(done.into());
        refresh_proxies(&again);
    });
}

fn register_proxies(ctx: &Ctx, bridge: &SiftmailBridge) {
    let c = ctx.clone();
    bridge.on_refresh_proxies(move || refresh_proxies(&c));

    let c = ctx.clone();
    bridge.on_save_proxy(
        move |id, name, kind, host, port, username, password, changed| {
            let Ok(port) = port.trim().parse::<u16>() else {
                if let Some(app) = c.app.upgrade() {
                    app.global::<SiftmailBridge>()
                        .set_status("Port must be a number between 1 and 65535.".into());
                }
                return;
            };
            let cfg = ProxyConfig {
                id: i64::from(id),
                name: name.trim().to_string(),
                kind: if kind == 1 {
                    ProxyKind::HttpConnect
                } else {
                    ProxyKind::Socks5
                },
                host: host.trim().to_string(),
                port,
                username: username.trim().to_string(),
                password: None,
                remote_dns: true,
            };
            let password = changed.then(|| password.to_string());
            mutate_proxies(&c, "Proxy saved.", move |core| async move {
                core.save_proxy(cfg, password)
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            });
        },
    );

    let c = ctx.clone();
    bridge.on_delete_proxy(move |id| {
        mutate_proxies(&c, "Proxy deleted.", move |core| async move {
            core.delete_proxy(i64::from(id))
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
    });

    let c = ctx.clone();
    bridge.on_set_global_proxy(move |index| {
        let id = proxy_id_at(&c, index);
        mutate_proxies(&c, "Global proxy updated.", move |core| async move {
            core.set_global_proxy_id(id).await.map_err(|e| e.to_string())
        });
    });

    let c = ctx.clone();
    bridge.on_set_account_proxy(move |account_id, mode, index| {
        let mode = mode_at(mode);
        let id = proxy_id_at(&c, index);
        if mode == ProxyMode::Custom && id.is_none() {
            return;
        }
        mutate_proxies(&c, "Account proxy updated.", move |core| async move {
            core.set_account_proxy(i64::from(account_id), mode, id)
                .await
                .map_err(|e| e.to_string())
        });
    });

    let c = ctx.clone();
    bridge.on_test_proxy(move |id, host, port| {
        let host = host.to_string();
        let port = port.trim().parse::<u16>().unwrap_or(993);
        let target = format!("{host}:{port}");
        if let Some(app) = c.app.upgrade() {
            app.global::<SiftmailBridge>()
                .set_proxy_test_result(format!("Testing via proxy → {target}…").into());
        }
        let weak = c.app.clone();
        let Some(core) = c.core() else { return };
        let task = c.runtime.spawn(async move { core.test_proxy(i64::from(id), host, port).await });
        let _ = slint::spawn_local(async move {
            let text = match task.await {
                Ok(Ok(ms)) => format!("OK: reached {target} through the proxy in {ms} ms."),
                Ok(Err(e)) => format!("Failed: {e}"),
                Err(e) => format!("Failed: {e}"),
            };
            if let Some(app) = weak.upgrade() {
                app.global::<SiftmailBridge>()
                    .set_proxy_test_result(text.into());
            }
        });
    });
}

// ---- embeddings -------------------------------------------------------

use flectar_mail_core::embed::providers::{EmbeddingConfig, ProviderKind};

pub(super) fn embedding_kind_at(index: i32) -> ProviderKind {
    match index {
        1 => ProviderKind::OpenAi,
        2 => ProviderKind::Local,
        _ => ProviderKind::Ollama,
    }
}

pub(super) fn embedding_kind_index(kind: ProviderKind) -> i32 {
    match kind {
        ProviderKind::Ollama => 0,
        ProviderKind::OpenAi => 1,
        ProviderKind::Local => 2,
    }
}

pub(super) fn embedding_form(kind: i32, url: &str, model: &str) -> EmbeddingConfig {
    let kind = embedding_kind_at(kind);
    let url = url.trim();
    EmbeddingConfig {
        kind,
        base_url: match (kind, url.is_empty()) {
            (ProviderKind::Ollama, true) => {
                flectar_mail_core::embed::providers::DEFAULT_OLLAMA_URL.into()
            }
            _ => url.trim_end_matches('/').to_string(),
        },
        model: model.trim().to_string(),
        // 0 = probe the dimension on save/test.
        dim: 0,
        ..EmbeddingConfig::default()
    }
}

pub(super) fn rebuild_text(check: &flectar_mail_core::EmbeddingCheck) -> Option<String> {
    (check.stale_vectors > 0).then(|| {
        format!(
            "The embedding model is now {} ({} dimensions){}. {} stored vectors were made with another model and cannot be mixed with the new ones. Rebuild removes them so mail is re-indexed with the new model.",
            check.model_id,
            check.dim,
            check
                .previous_model_id
                .as_ref()
                .map(|p| format!(", previously {p}"))
                .unwrap_or_default(),
            check.stale_vectors
        )
    })
}

fn refresh_embedding(ctx: &Ctx) {
    ctx.spawn(
        |core| async move { core.embedding_config().await.map_err(|e| e.to_string()) },
        |app, (cfg, has_key)| {
            let b = app.global::<SiftmailBridge>();
            b.set_embedding_kind_index(embedding_kind_index(cfg.kind));
            b.set_embedding_url(cfg.base_url.into());
            b.set_embedding_model(cfg.model.into());
            b.set_embedding_has_key(has_key);
        },
    );
}

fn set_embedding_result(ctx: &Ctx, text: String) {
    if let Some(app) = ctx.app.upgrade() {
        app.global::<SiftmailBridge>()
            .set_embedding_result(text.into());
    }
}

fn register_embedding(ctx: &Ctx, bridge: &SiftmailBridge) {
    let c = ctx.clone();
    bridge.on_refresh_embedding(move || refresh_embedding(&c));

    let c = ctx.clone();
    bridge.on_test_embedding(move |kind, url, model, key| {
        let cfg = embedding_form(kind, &url, &model);
        let key = Some(key.to_string()).filter(|k| !k.is_empty());
        set_embedding_result(&c, "Testing…".into());
        let report = c.clone();
        c.spawn(
            move |core| async move {
                Ok(core.test_embedding(cfg, key).await.map_err(|e| e.to_string()))
            },
            move |_, r| {
                set_embedding_result(
                    &report,
                    match r {
                        Ok(ch) => format!(
                            "OK: {} · {} dimensions · {} ms",
                            ch.model_id, ch.dim, ch.latency_ms
                        ),
                        Err(e) => format!("Failed: {e}"),
                    },
                )
            },
        );
    });

    let c = ctx.clone();
    bridge.on_save_embedding(move |kind, url, model, key, changed| {
        let cfg = embedding_form(kind, &url, &model);
        let key = changed.then(|| key.to_string());
        set_embedding_result(&c, "Checking and saving…".into());
        let report = c.clone();
        c.spawn(
            move |core| async move {
                Ok(core
                    .save_embedding_config(cfg, key)
                    .await
                    .map_err(|e| e.to_string()))
            },
            move |app, r| {
                let b = app.global::<SiftmailBridge>();
                match r {
                    Ok(ch) => {
                        set_embedding_result(
                            &report,
                            format!("Saved: {} · {} dimensions", ch.model_id, ch.dim),
                        );
                        if let Some(text) = rebuild_text(&ch) {
                            b.set_embedding_rebuild_text(text.into());
                            b.set_embedding_rebuild_prompt(true);
                        }
                        refresh_embedding(&report);
                    }
                    Err(e) => set_embedding_result(&report, format!("Not saved: {e}")),
                }
            },
        );
    });

    let c = ctx.clone();
    bridge.on_rebuild_embedding_index(move || {
        let report = c.clone();
        c.spawn(
            |core| async move { core.rebuild_embedding_index().await.map_err(|e| e.to_string()) },
            move |app, n| {
                app.global::<SiftmailBridge>()
                    .set_embedding_rebuild_prompt(false);
                set_embedding_result(
                    &report,
                    format!("Removed {n} old vectors; mail will be re-indexed."),
                );
            },
        );
    });
}

// ---- translation bar ---------------------------------------------------

use flectar_mail_core::translate::{TranslateConfig, TranslateEvent};
use std::sync::atomic::{AtomicBool, Ordering};

/// Target names sent to the model, matching `TranslationBridge.languages`.
pub(super) const TARGET_LANGUAGES: [&str; 7] = [
    "Chinese (Simplified)",
    "English",
    "Japanese",
    "Korean",
    "French",
    "German",
    "Spanish",
];

/// Readable plain text from translated HTML (or plain) output. Also drops
/// any placeholder markers left in streaming previews.
pub(super) fn display_text(s: &str, is_html: bool) -> String {
    let mut out = String::with_capacity(s.len());
    if is_html {
        let mut in_tag = false;
        let mut tag = String::new();
        for c in s.chars() {
            match (in_tag, c) {
                (false, '<') => {
                    in_tag = true;
                    tag.clear();
                }
                (true, '>') => {
                    in_tag = false;
                    let name = tag
                        .trim_start_matches('/')
                        .split(|c: char| c.is_whitespace() || c == '/')
                        .next()
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    if matches!(
                        name.as_str(),
                        "p" | "br" | "div" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
                            | "blockquote" | "pre" | "table"
                    ) && !out.ends_with('\n')
                    {
                        out.push('\n');
                    }
                }
                (true, c) => tag.push(c),
                (false, c) => out.push(c),
            }
        }
        out = out
            .replace("&nbsp;", " ")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&amp;", "&");
    } else {
        out.push_str(s);
    }
    // ⟦t1⟧ / ⟦/t1⟧ / ⟦c2⟧ markers in previews.
    let mut clean = String::with_capacity(out.len());
    let mut skip = false;
    for c in out.chars() {
        match c {
            '⟦' => skip = true,
            '⟧' => skip = false,
            _ if !skip => clean.push(c),
            _ => {}
        }
    }
    let mut lines: Vec<&str> = clean.lines().map(str::trim_end).collect();
    lines.dedup_by(|a, b| a.is_empty() && b.is_empty());
    lines.join("\n").trim().to_string()
}

fn current_message_id(state: &Rc<RefCell<InboxState>>) -> Option<i64> {
    let s = state.borrow();
    if let Some(m) = s.conversation_messages.get(s.conversation_selected_index) {
        return Some(i64::from(m.id));
    }
    s.selected_id.map(i64::from)
}

#[derive(Default)]
struct TranslationRun {
    cancel: Option<Arc<AtomicBool>>,
    message_id: Option<i64>,
    generation: u64,
}

fn tr_bridge(ctx: &Ctx) -> Option<(AppWindow, ())> {
    ctx.app.upgrade().map(|a| (a, ()))
}

fn start_translation(ctx: &Ctx, run: &Rc<RefCell<TranslationRun>>, lang: i32, confirmed: bool) {
    let Some((app, ())) = tr_bridge(ctx) else { return };
    let b = app.global::<TranslationBridge>();
    let Some(message_id) = current_message_id(&ctx.state) else {
        b.set_message("Select a message first.".into());
        return;
    };
    let target = TARGET_LANGUAGES
        .get(usize::try_from(lang).unwrap_or(0))
        .copied()
        .unwrap_or(TARGET_LANGUAGES[0])
        .to_string();
    let cfg = TranslateConfig {
        target_lang: target.clone(),
        ..TranslateConfig::default()
    };
    if !confirmed {
        b.set_phase("estimating".into());
        b.set_message("Estimating size…".into());
        let c = ctx.clone();
        let r = Rc::clone(run);
        let est_cfg = cfg.clone();
        ctx.spawn(
            move |core| async move {
                core.estimate_translation(message_id, est_cfg)
                    .await
                    .map_err(|e| e.to_string())
            },
            move |app, est| {
                let b = app.global::<TranslationBridge>();
                if est.already_in_target {
                    b.set_phase("idle".into());
                    b.set_message(format!("This message is already in {target}.").into());
                } else if est.needs_confirmation {
                    let cost = est
                        .est_cost
                        .map(|c| format!(", about ${c:.2}"))
                        .unwrap_or_default();
                    b.set_confirm_text(
                        format!(
                            "Translating it needs {} requests and roughly {} input tokens{cost}. Continue?",
                            est.chunks, est.input_tokens
                        )
                        .into(),
                    );
                    b.set_phase("confirm".into());
                    b.set_message("".into());
                } else {
                    start_translation(&c, &r, lang, true);
                }
            },
        );
        return;
    }

    // Run.
    let cancel = Arc::new(AtomicBool::new(false));
    let generation = {
        let mut r = run.borrow_mut();
        if let Some(old) = r.cancel.take() {
            old.store(true, Ordering::Relaxed);
        }
        r.cancel = Some(cancel.clone());
        r.message_id = Some(message_id);
        r.generation += 1;
        r.generation
    };
    b.set_phase("running".into());
    b.set_done(0);
    b.set_total(0);
    b.set_message("".into());
    b.set_translated_text("".into());
    b.set_show_translated(true);

    // Streaming: previews and finished blocks are shown as they arrive.
    let weak = ctx.app.clone();
    let parts: Arc<std::sync::Mutex<std::collections::BTreeMap<u32, String>>> = Default::default();
    let on_event: Arc<dyn Fn(TranslateEvent) + Send + Sync> = Arc::new(move |e| {
        let weak = weak.clone();
        let parts = parts.clone();
        let update = move |app: AppWindow| {
            let b = app.global::<TranslationBridge>();
            match e {
                TranslateEvent::Progress { done, total } => {
                    b.set_done(done as i32);
                    b.set_total(total as i32);
                }
                TranslateEvent::Block { idx, text } | TranslateEvent::Preview { idx, text } => {
                    let joined = {
                        let mut p = parts.lock().unwrap_or_else(|e| e.into_inner());
                        p.insert(idx, text);
                        p.values().cloned().collect::<Vec<_>>().join("\n\n")
                    };
                    b.set_translated_text(display_text(&joined, true).into());
                }
            }
        };
        let _ = weak.upgrade_in_event_loop(update);
    });
    let r = Rc::clone(run);
    ctx.spawn(
        move |core| async move {
            Ok(core
                .translate_message(message_id, cfg, cancel, Some(on_event))
                .await
                .map_err(|e| e.to_string()))
        },
        move |app, result| {
            if r.borrow().generation != generation {
                return;
            }
            r.borrow_mut().cancel = None;
            let b = app.global::<TranslationBridge>();
            match result {
                Ok(o) if o.status == "same_language" => {
                    b.set_phase("idle".into());
                    b.set_message("Already in the target language.".into());
                }
                Ok(o) => {
                    b.set_translated_text(display_text(&o.output, o.is_html).into());
                    b.set_phase("done".into());
                    b.set_message(
                        match o.status.as_str() {
                            "done" => "Translated (view only; the message is unchanged).".to_string(),
                            _ => format!(
                                "Partly translated: {} of {} parts; {} blocks kept in the original. Translate again to continue.",
                                o.chunks_done,
                                o.chunks_total,
                                o.failed_blocks.len()
                            ),
                        }
                        .into(),
                    );
                }
                Err(e) => {
                    b.set_phase("error".into());
                    b.set_message(format!("Translation failed: {e}").into());
                }
            }
        },
    );
}

fn register_translation(ctx: &Ctx, app: &AppWindow) {
    let run: Rc<RefCell<TranslationRun>> = Rc::default();
    let b = app.global::<TranslationBridge>();

    let (c, r) = (ctx.clone(), Rc::clone(&run));
    b.on_start(move |lang| start_translation(&c, &r, lang, false));
    let (c, r) = (ctx.clone(), Rc::clone(&run));
    b.on_confirm(move || {
        let Some(app) = c.app.upgrade() else { return };
        let lang = app.global::<TranslationBridge>().get_language_index();
        start_translation(&c, &r, lang, true);
    });
    let (c, r) = (ctx.clone(), Rc::clone(&run));
    b.on_cancel(move || {
        if let Some(flag) = r.borrow_mut().cancel.take() {
            flag.store(true, Ordering::Relaxed);
        }
        if let Some(app) = c.app.upgrade() {
            let b = app.global::<TranslationBridge>();
            b.set_phase("idle".into());
            b.set_message("Cancelled; finished parts are kept.".into());
        }
    });
    let (c, r) = (ctx.clone(), Rc::clone(&run));
    b.on_close(move || {
        let mut run = r.borrow_mut();
        if let Some(flag) = run.cancel.take() {
            flag.store(true, Ordering::Relaxed);
        }
        run.generation += 1;
        if let Some(app) = c.app.upgrade() {
            let b = app.global::<TranslationBridge>();
            b.set_open(false);
            b.set_phase("idle".into());
            b.set_translated_text("".into());
            b.set_message("".into());
        }
    });
}

pub(super) fn register(
    app: &AppWindow,
    state: &Rc<RefCell<InboxState>>,
    runtime: &Rc<tokio::runtime::Runtime>,
) {
    let ctx = Ctx {
        app: app.as_weak(),
        state: Rc::clone(state),
        runtime: Rc::clone(runtime),
    };
    let bridge = app.global::<SiftmailBridge>();

    let c = ctx.clone();
    bridge.on_refresh_scheduler(move || refresh_scheduler(&c));
    let c = ctx.clone();
    bridge.on_reset_backoff(move |provider| {
        let provider = provider.to_string();
        let again = c.clone();
        c.spawn(
            move |core| async move {
                core.reset_provider_backoff(provider)
                    .await
                    .map_err(|e| e.to_string())
            },
            move |_, ()| refresh_scheduler(&again),
        );
    });
    register_categories(&ctx, &bridge);
    register_proxies(&ctx, &bridge);
    register_embedding(&ctx, &bridge);
    register_translation(&ctx, app);
}

#[cfg(test)]
mod tests {
    #[test]
    fn remaining_is_rounded_up_to_minutes() {
        assert_eq!(super::format_remaining(1), "1 min");
        assert_eq!(super::format_remaining(90 * 60_000), "1 h 30 min");
    }

    #[test]
    fn display_text_strips_markup_and_placeholders() {
        let t = super::display_text(
            "<h1>标题</h1><p>见 <a href=\"x\">报告</a> &amp; 附件</p><p>⟦t1⟧预览⟦/t1⟧</p>",
            true,
        );
        assert_eq!(t, "标题\n见 报告 & 附件\n预览");
        assert_eq!(super::display_text("plain ⟦c1⟧ text", false), "plain  text");
        assert_eq!(super::TARGET_LANGUAGES.len(), 7);
    }

    #[test]
    fn embedding_form_defaults_and_rebuild_prompt() {
        use flectar_mail_core::embed::providers::ProviderKind;
        let c = super::embedding_form(0, " ", "bge-m3");
        assert_eq!(c.kind, ProviderKind::Ollama);
        assert_eq!(c.base_url, "http://127.0.0.1:11434");
        let c = super::embedding_form(1, "https://x.test/v1/", "m");
        assert_eq!((c.kind, c.base_url.as_str()), (ProviderKind::OpenAi, "https://x.test/v1"));
        for i in 0..3 {
            assert_eq!(super::embedding_kind_index(super::embedding_kind_at(i)), i);
        }
        let mut check = flectar_mail_core::EmbeddingCheck {
            model_id: "ollama:bge-m3:1024".into(),
            dim: 1024,
            latency_ms: 3,
            previous_model_id: Some("local:x:384".into()),
            stale_vectors: 0,
        };
        assert!(super::rebuild_text(&check).is_none());
        check.stale_vectors = 12;
        let t = super::rebuild_text(&check).unwrap();
        assert!(t.contains("12") && t.contains("local:x:384"));
    }

    #[test]
    fn proxy_modes_map_both_ways() {
        use flectar_mail_core::net::proxy::ProxyMode;
        for m in [ProxyMode::Direct, ProxyMode::Global, ProxyMode::Custom] {
            assert_eq!(super::mode_at(super::mode_index(m)), m);
        }
        assert_eq!(super::mode_at(7), ProxyMode::Global);
    }

    #[test]
    fn category_ids_are_valid_slugs() {
        assert_eq!(super::category_id_for("Work & Projects", 5), "work-projects");
        assert_eq!(super::category_id_for("发票", 5), "c-5");
    }

    #[test]
    fn simple_rules_round_trip_through_the_editor() {
        let r = super::build_rule(3, "work".into(), "".into(), 2, 1, " boss@x.com ".into(), true)
            .unwrap();
        assert_eq!(r.name, "boss@x.com");
        assert_eq!(super::simple_rule_parts(&r), Some((2, 1, "boss@x.com".into())));
        assert!(super::build_rule(0, "w".into(), "".into(), 0, 0, "  ".into(), true).is_none());
        assert!(super::build_rule(0, "w".into(), "".into(), 99, 0, "x".into(), true).is_none());
    }
}
