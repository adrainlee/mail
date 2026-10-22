//! `Core` methods: sync scheduler status for the settings view (Siftmail).

use crate::sync::scheduler::{self, BackoffState};
use crate::*;

/// One provider row: its accounts and current backoff, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSchedule {
    pub provider: String,
    pub account_emails: Vec<String>,
    pub backoff: Option<BackoffState>,
    /// Milliseconds still to wait; 0 when the provider may connect.
    pub remaining_ms: i64,
    /// Accounts of this provider that keep a push channel open.
    pub push_emails: Vec<String>,
}

pub(crate) fn provider_schedules(
    configs: &[AccountConfig],
    backoffs: Vec<BackoffState>,
    now_ms: i64,
) -> Vec<ProviderSchedule> {
    let mut rows: Vec<ProviderSchedule> = Vec::new();
    for c in configs {
        let key = scheduler::account_provider_key(c);
        let push = c.settings.sync_schedule.push_for(c);
        let idx = match rows.iter().position(|r| r.provider == key) {
            Some(i) => i,
            None => {
                rows.push(ProviderSchedule {
                    provider: key,
                    account_emails: Vec::new(),
                    backoff: None,
                    remaining_ms: 0,
                    push_emails: Vec::new(),
                });
                rows.len() - 1
            }
        };
        rows[idx].account_emails.push(c.email.clone());
        if push {
            rows[idx].push_emails.push(c.email.clone());
        }
    }
    for b in backoffs {
        let remaining_ms = (b.next_allowed_at - now_ms).max(0);
        match rows.iter_mut().find(|r| r.provider == b.provider) {
            Some(r) => {
                r.remaining_ms = remaining_ms;
                r.backoff = Some(b);
            }
            None => rows.push(ProviderSchedule {
                provider: b.provider.clone(),
                account_emails: Vec::new(),
                remaining_ms,
                backoff: Some(b),
                push_emails: Vec::new(),
            }),
        }
    }
    rows.sort_by(|a, b| {
        b.remaining_ms
            .cmp(&a.remaining_ms)
            .then(a.provider.cmp(&b.provider))
    });
    rows
}

impl Core {
    /// Per-provider backoff state for every account type.
    pub async fn sync_scheduler_status(&self) -> Result<Vec<ProviderSchedule>> {
        let (configs, backoffs) = self
            .db
            .read(|conn| {
                Ok((
                    repo::accounts::list_configs(conn)?,
                    scheduler::list_backoffs(conn)?,
                ))
            })
            .await?;
        Ok(provider_schedules(&configs, backoffs, now_ms()))
    }

    /// Clear a provider's backoff (user override from the status view).
    pub async fn reset_provider_backoff(&self, provider: String) -> Result<()> {
        self.db
            .write(move |conn| scheduler::record_success(conn, &provider, now_ms()))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_accounts_by_provider_and_orders_backoffs_first() {
        let b = BackoffState {
            provider: "example.com".into(),
            level: 2,
            next_allowed_at: 5_000,
            last_error: "429".into(),
        };
        let rows = provider_schedules(&[], vec![b.clone()], 1_000);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].remaining_ms, 4_000);
        let rows = provider_schedules(&[], vec![b], 9_000);
        assert_eq!(rows[0].remaining_ms, 0);
    }
}
