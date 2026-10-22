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
}

#[cfg(test)]
mod tests {
    #[test]
    fn remaining_is_rounded_up_to_minutes() {
        assert_eq!(super::format_remaining(1), "1 min");
        assert_eq!(super::format_remaining(90 * 60_000), "1 h 30 min");
    }
}
