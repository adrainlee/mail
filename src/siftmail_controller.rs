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
}

#[cfg(test)]
mod tests {
    #[test]
    fn remaining_is_rounded_up_to_minutes() {
        assert_eq!(super::format_remaining(1), "1 min");
        assert_eq!(super::format_remaining(90 * 60_000), "1 h 30 min");
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
