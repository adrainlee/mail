//! Siftmail M1.5 sync scheduler: spreads many accounts over the day and backs
//! off per mail provider instead of reconnecting every account at once.
//!
//! Rules (see docs/fork.md):
//! 1. Startup stagger: each account waits a random 0–10 minutes at startup.
//! 2. Interval jitter: next cycle = base ± jitter (default 60 ± 20 minutes).
//! 3. Provider backoff: a throttle-like failure on any account of a provider
//!    backs off the whole provider 1h → 2h → 4h → 8h … capped at 24h, each
//!    ±20 %. A successful cycle resets it. Auth failures stay per-account.
//! 4. At most [`PROVIDER_CONCURRENCY`] accounts of one provider connect at a
//!    time (in polling mode).
//! 5. Backoff state is persisted in `provider_backoff`.
//!
//! All decision functions take `now_ms` and an RNG so they are deterministic
//! under test.

use crate::error::{CoreError, Result};
use rand::Rng;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Maximum simultaneous polling connections per provider.
pub const PROVIDER_CONCURRENCY: usize = 2;
/// Startup stagger applies only when at least this many accounts start.
pub const STAGGER_MIN_ACCOUNTS: usize = 4;
/// Upper bound of the random startup delay.
pub const STARTUP_STAGGER_MAX: Duration = Duration::from_secs(10 * 60);
const BACKOFF_BASE: Duration = Duration::from_secs(60 * 60);
const BACKOFF_CAP: Duration = Duration::from_secs(24 * 60 * 60);
const BACKOFF_JITTER: f64 = 0.20;

/// Second-level labels treated as part of a public suffix (`example.co.uk`).
const COMPOUND_SLDS: &[&str] = &["co", "com", "net", "org", "ac", "gov", "edu", "ne", "or"];

/// Identify the provider of an IMAP host by its registrable domain, e.g.
/// `imap.gmail.com` → `gmail.com`, `mail.example.co.uk` → `example.co.uk`.
/// IP literals and single-label hosts are returned unchanged.
pub fn provider_key(host: &str) -> String {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.parse::<std::net::IpAddr>().is_ok() || host.starts_with('[') {
        return host;
    }
    let labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
    if labels.len() <= 2 {
        return labels.join(".");
    }
    let n = labels.len();
    let take = if labels[n - 1].len() == 2 && COMPOUND_SLDS.contains(&labels[n - 2]) {
        3
    } else {
        2
    };
    labels[n - take..].join(".")
}

/// Provider key for any account type, so IMAP, Gmail API and JMAP accounts
/// on one provider share a single backoff and permit pool.
pub fn account_provider_key(config: &crate::models::AccountConfig) -> String {
    use crate::models::{MailProtocol, Provider};
    if config.provider == Provider::Gmail {
        return "gmail.com".into();
    }
    if config.mail_protocol == MailProtocol::Jmap {
        let host = reqwest::Url::parse(&config.jmap_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_default();
        if !host.is_empty() {
            return provider_key(&host);
        }
    }
    provider_key(&config.imap_host)
}

/// Delay until the next regular cycle: the account's jittered poll schedule
/// when polling, otherwise `push_fallback` (the app-wide interval used as a
/// safety net while a push channel is open).
pub fn next_cycle_delay(
    schedule: &crate::models::SyncSchedule,
    push_fallback: Duration,
    rng: &mut impl Rng,
) -> Duration {
    if schedule.idle_push {
        push_fallback
    } else {
        jittered_interval(schedule.base(), schedule.jitter(), rng)
    }
}

/// Remaining provider backoff, if any (read from the persisted table).
pub async fn backoff_remaining(
    db: &crate::db::Db,
    provider: &str,
) -> Option<(Duration, BackoffState)> {
    let p = provider.to_string();
    let state = db
        .read(move |conn| get_backoff(conn, &p))
        .await
        .ok()
        .flatten()?;
    state.remaining(crate::models::now_ms()).map(|d| (d, state))
}

/// Record a cycle's outcome for the provider. Returns `true` when the error
/// put the provider into backoff.
pub async fn record_cycle(
    db: &crate::db::Db,
    provider: &str,
    outcome: std::result::Result<(), &CoreError>,
) -> bool {
    let p = provider.to_string();
    match outcome {
        Ok(()) => {
            let _ = db
                .write(move |conn| record_success(conn, &p, crate::models::now_ms()))
                .await;
            false
        }
        Err(e) if classify_failure(e) == FailureKind::ProviderThrottle => {
            let message = e.to_string();
            db.write(move |conn| {
                record_throttle(
                    conn,
                    &p,
                    &message,
                    crate::models::now_ms(),
                    &mut rand::rng(),
                )
            })
            .await
            .is_ok()
        }
        Err(_) => false,
    }
}

/// How a connect/cycle failure should be scheduled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Provider-wide: connection refused, rate limited, too many connections.
    ProviderThrottle,
    /// Only this account (bad credentials); never backs off the provider.
    AccountAuth,
    /// Anything else: the actor's existing short reconnect backoff applies.
    Transient,
}

const THROTTLE_MARKERS: &[&str] = &[
    "too many",
    "rate limit",
    "ratelimit",
    "throttl",
    "try again later",
    "connection refused",
    "[unavailable]",
    "[limit]",
    "overquota",
    "temporarily unavailable",
    "server busy",
];

pub fn classify_failure(err: &CoreError) -> FailureKind {
    match err {
        CoreError::Auth(_) | CoreError::NeedsReauth => FailureKind::AccountAuth,
        // A broken proxy only affects the accounts using it, never the
        // whole mail provider.
        e if crate::net::proxy::is_proxy_error(e) => FailureKind::Transient,
        _ => {
            let text = err.to_string().to_ascii_lowercase();
            if THROTTLE_MARKERS.iter().any(|m| text.contains(m)) {
                FailureKind::ProviderThrottle
            } else {
                FailureKind::Transient
            }
        }
    }
}

fn jitter_fraction(d: Duration, fraction: f64, rng: &mut impl Rng) -> Duration {
    let f = rng.random_range(-fraction..=fraction);
    Duration::from_secs_f64((d.as_secs_f64() * (1.0 + f)).max(0.0))
}

/// Backoff for the given level (1-based): 1h·2^(level−1), capped at 24h, ±20 %.
pub fn backoff_delay(level: u32, rng: &mut impl Rng) -> Duration {
    let exp = level.saturating_sub(1).min(10);
    let raw = BACKOFF_BASE.saturating_mul(1u32 << exp).min(BACKOFF_CAP);
    jitter_fraction(raw, BACKOFF_JITTER, rng)
}

/// `base ± jitter`, never below one minute.
pub fn jittered_interval(base: Duration, jitter: Duration, rng: &mut impl Rng) -> Duration {
    let j = jitter.min(base).as_secs_f64();
    let offset = if j > 0.0 {
        rng.random_range(-j..=j)
    } else {
        0.0
    };
    Duration::from_secs_f64((base.as_secs_f64() + offset).max(60.0))
}

pub fn startup_delay(rng: &mut impl Rng) -> Duration {
    Duration::from_secs(rng.random_range(0..=STARTUP_STAGGER_MAX.as_secs()))
}

/// Persisted provider backoff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackoffState {
    pub provider: String,
    pub level: u32,
    pub next_allowed_at: i64,
    pub last_error: String,
}

impl BackoffState {
    /// Time still to wait at `now_ms`, if any.
    pub fn remaining(&self, now_ms: i64) -> Option<Duration> {
        (self.next_allowed_at > now_ms)
            .then(|| Duration::from_millis((self.next_allowed_at - now_ms) as u64))
    }
}

pub fn get_backoff(conn: &Connection, provider: &str) -> Result<Option<BackoffState>> {
    Ok(conn
        .query_row(
            "SELECT provider, level, next_allowed_at, last_error
             FROM provider_backoff WHERE provider = ?1",
            params![provider],
            |r| {
                Ok(BackoffState {
                    provider: r.get(0)?,
                    level: r.get::<_, i64>(1)?.max(0) as u32,
                    next_allowed_at: r.get(2)?,
                    last_error: r.get(3)?,
                })
            },
        )
        .optional()?)
}

pub fn list_backoffs(conn: &Connection) -> Result<Vec<BackoffState>> {
    let mut stmt = conn.prepare(
        "SELECT provider, level, next_allowed_at, last_error
         FROM provider_backoff WHERE level > 0 ORDER BY provider",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(BackoffState {
                provider: r.get(0)?,
                level: r.get::<_, i64>(1)?.max(0) as u32,
                next_allowed_at: r.get(2)?,
                last_error: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Record a provider-wide throttle. Failures reported while the provider is
/// already backing off (other accounts racing) do not escalate the level again.
pub fn record_throttle(
    conn: &Connection,
    provider: &str,
    error: &str,
    now_ms: i64,
    rng: &mut impl Rng,
) -> Result<BackoffState> {
    let current = get_backoff(conn, provider)?;
    if let Some(cur) = &current
        && cur.level > 0
        && cur.next_allowed_at > now_ms
    {
        return Ok(cur.clone());
    }
    let level = current.map(|c| c.level).unwrap_or(0).saturating_add(1);
    let next = now_ms + backoff_delay(level, rng).as_millis() as i64;
    let error: String = error.chars().take(500).collect();
    conn.execute(
        "INSERT INTO provider_backoff (provider, level, next_allowed_at, last_error, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(provider) DO UPDATE SET level = excluded.level,
           next_allowed_at = excluded.next_allowed_at,
           last_error = excluded.last_error, updated_at = excluded.updated_at",
        params![provider, level as i64, next, error, now_ms],
    )?;
    Ok(BackoffState {
        provider: provider.to_string(),
        level,
        next_allowed_at: next,
        last_error: error,
    })
}

/// A successful cycle restores the provider's normal cadence.
pub fn record_success(conn: &Connection, provider: &str, now_ms: i64) -> Result<()> {
    conn.execute(
        "UPDATE provider_backoff SET level = 0, next_allowed_at = 0, updated_at = ?2
         WHERE provider = ?1 AND level > 0",
        params![provider, now_ms],
    )?;
    Ok(())
}

/// Process-wide per-provider connection permits.
pub async fn acquire_provider_permit(provider: &str) -> OwnedSemaphorePermit {
    static GATES: OnceLock<Mutex<HashMap<String, Arc<Semaphore>>>> = OnceLock::new();
    let sem = {
        let mut gates = GATES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        gates
            .entry(provider.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(PROVIDER_CONCURRENCY)))
            .clone()
    };
    sem.acquire_owned()
        .await
        .expect("provider semaphores are never closed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    fn db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::migrations::run(&mut conn).unwrap();
        conn
    }

    #[test]
    fn provider_key_groups_hosts_by_registrable_domain() {
        assert_eq!(provider_key("imap.gmail.com"), "gmail.com");
        assert_eq!(provider_key("IMAP.Mail.Yahoo.com."), "yahoo.com");
        assert_eq!(provider_key("outlook.office365.com"), "office365.com");
        assert_eq!(provider_key("mail.example.co.uk"), "example.co.uk");
        assert_eq!(provider_key("imap.exmail.qq.com"), "qq.com");
        assert_eq!(provider_key("192.168.1.5"), "192.168.1.5");
        assert_eq!(provider_key("localhost"), "localhost");
    }

    #[test]
    fn failures_are_classified() {
        assert_eq!(
            classify_failure(&CoreError::Imap("NO [LIMIT] Too many connections".into())),
            FailureKind::ProviderThrottle
        );
        assert_eq!(
            classify_failure(&CoreError::Network(
                "Connection refused (os error 111)".into()
            )),
            FailureKind::ProviderThrottle
        );
        assert_eq!(
            classify_failure(&CoreError::Auth("bad password".into())),
            FailureKind::AccountAuth
        );
        assert_eq!(
            classify_failure(&CoreError::NeedsReauth),
            FailureKind::AccountAuth
        );
        assert_eq!(
            classify_failure(&CoreError::Network("dns failure".into())),
            FailureKind::Transient
        );
    }

    #[test]
    fn proxy_failures_do_not_back_off_the_provider() {
        assert_eq!(
            classify_failure(&CoreError::Network(
                "proxy unreachable: 10.0.0.1:1080: Connection refused".into()
            )),
            FailureKind::Transient
        );
    }

    #[test]
    fn backoff_doubles_hourly_with_jitter_and_caps_at_a_day() {
        let mut rng = StdRng::seed_from_u64(7);
        for (level, hours) in [
            (1u32, 1.0),
            (2, 2.0),
            (3, 4.0),
            (4, 8.0),
            (5, 16.0),
            (6, 24.0),
            (30, 24.0),
        ] {
            for _ in 0..50 {
                let d = backoff_delay(level, &mut rng).as_secs_f64() / 3600.0;
                assert!(
                    d >= hours * 0.8 - 1e-9 && d <= hours * 1.2 + 1e-9,
                    "{level}: {d}"
                );
            }
        }
    }

    #[test]
    fn interval_and_startup_jitter_stay_in_bounds() {
        let mut rng = StdRng::seed_from_u64(1);
        let base = Duration::from_secs(3600);
        let jitter = Duration::from_secs(1200);
        let mut min = u64::MAX;
        let mut max = 0;
        for _ in 0..500 {
            let d = jittered_interval(base, jitter, &mut rng).as_secs();
            min = min.min(d);
            max = max.max(d);
            assert!((2400..=4800).contains(&d));
            assert!(startup_delay(&mut rng) <= STARTUP_STAGGER_MAX);
        }
        assert!(max - min > 1800, "jitter should actually spread cycles");
        assert_eq!(
            jittered_interval(Duration::from_secs(10), Duration::ZERO, &mut rng),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn throttle_escalates_persists_and_resets_on_success() {
        let conn = db();
        let mut rng = StdRng::seed_from_u64(3);
        let h = 3_600_000i64;
        let s1 = record_throttle(&conn, "qq.com", "too many", 0, &mut rng).unwrap();
        assert_eq!(s1.level, 1);
        assert!(s1.remaining(0).is_some());
        // Another account failing during the window does not escalate.
        let again = record_throttle(&conn, "qq.com", "too many", 1000, &mut rng).unwrap();
        assert_eq!(again, s1);
        // After the window, a new failure escalates to level 2 (~2h).
        let t = s1.next_allowed_at + 1;
        let s2 = record_throttle(&conn, "qq.com", "refused", t, &mut rng).unwrap();
        assert_eq!(s2.level, 2);
        let wait = s2.next_allowed_at - t;
        assert!(wait >= (1.6 * h as f64) as i64 && wait <= (2.4 * h as f64) as i64);
        // Persisted ("survives restart") and isolated per provider.
        assert_eq!(get_backoff(&conn, "qq.com").unwrap().unwrap(), s2);
        assert!(get_backoff(&conn, "gmail.com").unwrap().is_none());
        assert_eq!(list_backoffs(&conn).unwrap().len(), 1);
        record_success(&conn, "qq.com", t + 5).unwrap();
        let reset = get_backoff(&conn, "qq.com").unwrap().unwrap();
        assert_eq!(reset.level, 0);
        assert!(reset.remaining(t + 5).is_none());
        assert!(list_backoffs(&conn).unwrap().is_empty());
        // After a reset the ladder restarts at 1h.
        let s3 = record_throttle(&conn, "qq.com", "x", t + 10, &mut rng).unwrap();
        assert_eq!(s3.level, 1);
    }

    #[tokio::test]
    async fn provider_permits_cap_concurrency() {
        let a = acquire_provider_permit("permit-test.example").await;
        let _b = acquire_provider_permit("permit-test.example").await;
        let third = tokio::time::timeout(
            Duration::from_millis(50),
            acquire_provider_permit("permit-test.example"),
        )
        .await;
        assert!(third.is_err(), "third connection must wait");
        let _other = acquire_provider_permit("other.example").await;
        drop(a);
        let _c = tokio::time::timeout(
            Duration::from_millis(500),
            acquire_provider_permit("permit-test.example"),
        )
        .await
        .expect("permit released");
    }

    #[test]
    fn every_account_type_has_a_provider_key() {
        use crate::models::{AccountConfig, AuthKind, MailProtocol, Provider};
        let mut c = AccountConfig {
            id: 1,
            email: "a@x".into(),
            display_name: None,
            avatar_url: None,
            provider: Provider::Gmail,
            auth_kind: AuthKind::Oauth2,
            mail_protocol: MailProtocol::Imap,
            username: "a".into(),
            jmap_url: String::new(),
            jmap_account_id: None,
            imap_host: String::new(),
            imap_port: 993,
            smtp_host: String::new(),
            smtp_port: 465,
            settings: Default::default(),
        };
        assert_eq!(account_provider_key(&c), "gmail.com");
        c.provider = Provider::Imap;
        c.mail_protocol = MailProtocol::Jmap;
        c.jmap_url = "https://mail.fastmail.com/jmap".into();
        assert_eq!(account_provider_key(&c), "fastmail.com");
        c.mail_protocol = MailProtocol::Imap;
        c.imap_host = "imap.example.co.uk".into();
        assert_eq!(account_provider_key(&c), "example.co.uk");
    }

    #[test]
    fn next_cycle_delay_polls_on_schedule_or_uses_push_fallback() {
        let mut rng = StdRng::seed_from_u64(7);
        let mut s = crate::models::SyncSchedule::default();
        let d = next_cycle_delay(&s, Duration::from_secs(5), &mut rng);
        assert!(d >= Duration::from_secs(40 * 60) && d <= Duration::from_secs(80 * 60));
        s.idle_push = true;
        assert_eq!(
            next_cycle_delay(&s, Duration::from_secs(5), &mut rng),
            Duration::from_secs(5)
        );
    }
}
