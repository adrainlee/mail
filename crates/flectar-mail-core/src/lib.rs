//! Email, calendar, account, and persistence logic shared by the native hosts.
//! The application embeds [`Core`], calls its async methods, and forwards
//! [`CoreEvent`]s to the Slint UI.

pub mod accounts;
pub mod ai;
pub mod autolabel;
pub mod caldav;
pub mod calendar;
pub mod carddav;
pub mod category;
pub mod config;
pub mod db;
pub mod embed;
pub mod error;
pub mod events;
mod file_io;
pub mod files;
pub mod googlecal;
pub mod graph;
pub mod graphcal;
mod http_body;
pub mod imap;
pub mod jmap;
pub mod mail_security;
pub mod migration;
pub mod mime;
pub mod models;
pub mod oauth;
pub mod queue;
pub mod route;
pub mod scheduler;
pub mod search;
pub mod sender_identities;
pub mod signatures;
pub mod smtp;
pub mod suggest;
pub mod sync;
pub mod unsubscribe;

use crate::accounts::credentials::{self, CredentialStoreHandle, Slot};
use crate::config::Paths;
use crate::db::Db;
use crate::db::repo;
#[cfg(feature = "local-embeddings")]
use crate::embed::Embedder;
use crate::error::{CoreError, Result};
use crate::events::{CoreEvent, EventBus};
use crate::models::*;
use crate::oauth::redirect::{LoopbackRedirectBroker, OAuthRedirectBrokerHandle};
use crate::oauth::tokens::TokenProvider;
use crate::sync::engine::{AccountHandle, SyncCmd, SyncCtx, spawn_account};
use rusqlite::OptionalExtension;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;

const MAX_INLINE_CID_BYTES_PER_IMAGE: usize = 8 * 1024 * 1024;
const MAX_INLINE_CID_BYTES_PER_MESSAGE: usize = 16 * 1024 * 1024;
const MAX_INLINE_CID_BYTES_PER_THREAD: usize = 32 * 1024 * 1024;
const MAX_INLINE_CID_IMAGES_PER_MESSAGE: usize = 32;
const MAX_DRAFT_ATTACHMENTS: usize = 32;
const MAX_DRAFT_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;
const MAX_DRAFT_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_CACHED_MESSAGE_BYTES: usize = 128 * 1024 * 1024;
const MAX_CACHED_HEADER_BYTES: usize = 256 * 1024;
const SEND_START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const SEND_STATUS_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

#[cfg(test)]
mod inline_cid_rewrite_tests {
    use crate::mime::rewrite_cid_src_with_images;
    use base64::Engine;
    use std::collections::HashMap;

    #[test]
    fn direct_encoding_preserves_existing_data_uri_bytes() {
        for len in [0, 1, 2, 3, 4, 7, 256, 1024] {
            let bytes: Vec<u8> = (0..len).map(|index| index as u8).collect();
            let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
            let map = HashMap::from([("image".to_owned(), ("image/png".to_owned(), bytes))]);
            assert_eq!(
                rewrite_cid_src_with_images("<img src='cid:image'>", &map),
                format!("<img src=\"data:image/png;base64,{encoded}\">")
            );
        }
    }
}

fn send_start_expired(now: i64, observed_at: i64, not_before: Option<i64>) -> bool {
    let timeout_ms = i64::try_from(SEND_START_TIMEOUT.as_millis()).unwrap_or(i64::MAX);
    now >= not_before.unwrap_or(observed_at).saturating_add(timeout_ms)
}

fn normalize_mail_profile_name(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(CoreError::Other("profile name cannot be empty".into()));
    }
    if value.chars().count() > 48 {
        return Err(CoreError::Other(
            "profile name cannot be longer than 48 characters".into(),
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(CoreError::Other(
            "profile name cannot contain control characters".into(),
        ));
    }
    Ok(value.to_owned())
}

fn normalize_mail_profile_color(value: &str) -> Result<String> {
    let value = value.trim();
    let valid = value.len() == 7
        && value.starts_with('#')
        && value[1..].bytes().all(|byte| byte.is_ascii_hexdigit());
    if !valid {
        return Err(CoreError::Other(
            "profile color must use #RRGGBB format".into(),
        ));
    }
    Ok(value.to_ascii_uppercase())
}

fn validate_folder_leaf(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(CoreError::Other("folder name cannot be empty".into()));
    }
    if value.chars().count() > 255 {
        return Err(CoreError::Other(
            "folder name cannot be longer than 255 characters".into(),
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(CoreError::Other(
            "folder name cannot contain control characters".into(),
        ));
    }
    Ok(value.to_owned())
}

pub use crate::db::repo::notifications::{NotificationOutboxItem, RoutedTab};
pub use crate::db::snapshot::DatabaseSnapshotManifest;

/// Read the presentation state needed to lay out the first application frame.
///
/// This is a best-effort presentation bootstrap. Normal core startup remains
/// authoritative for loading and validating the complete settings model.
pub fn startup_presentation(paths: &Paths) -> models::StartupPresentation {
    db::startup_presentation(&paths.db_file())
}

fn normalize_account_email(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 320 || value.chars().any(char::is_whitespace) {
        return None;
    }
    let (local, domain) = value.rsplit_once('@')?;
    if local.is_empty()
        || domain.is_empty()
        || local.contains('@')
        || domain.contains('@')
        || domain.starts_with('.')
        || domain.ends_with('.')
    {
        return None;
    }
    Some(format!("{local}@{}", domain.to_lowercase()))
}

/// Rules every account created from portable or migrated settings must
/// satisfy. Interactive setup checks the same fields as the user types them.
pub(crate) fn validate_portable_account(config: &PortableAccountConfig) -> Result<()> {
    imap::trusted_certificates(&config.settings.connection.trusted_certificate_pem)?;
    let email = config.email.trim();
    if email.len() > 320 || !email.contains('@') {
        return Err(CoreError::Other(format!(
            "invalid account email address: {}",
            config.email
        )));
    }
    if config.username.len() > 512
        || config.jmap_url.len() > 2048
        || config.imap_host.len() > 253
        || config.smtp_host.len() > 253
    {
        return Err(CoreError::Other("an account field is too long".into()));
    }
    let valid_pair = matches!(
        (config.provider, config.auth_kind),
        (Provider::Imap, AuthKind::Password)
            | (Provider::Gmail, AuthKind::Oauth2)
            | (Provider::Microsoft, AuthKind::Oauth2)
    );
    if !valid_pair {
        return Err(CoreError::Other(format!(
            "unsupported sign-in type for {email}"
        )));
    }
    if config.provider == Provider::Imap
        && config.mail_protocol == MailProtocol::Imap
        && (config.username.trim().is_empty()
            || config.imap_host.trim().is_empty()
            || config.smtp_host.trim().is_empty()
            || config.imap_port == 0
            || config.smtp_port == 0)
    {
        return Err(CoreError::Other(format!(
            "incomplete IMAP settings for {email}"
        )));
    }
    if config.provider == Provider::Imap
        && config.mail_protocol == MailProtocol::Jmap
        && config.username.trim().is_empty()
    {
        return Err(CoreError::Other(format!(
            "incomplete JMAP settings for {email}"
        )));
    }
    Ok(())
}

fn normalize_account_password(value: String) -> String {
    // Password controls are one-line inputs. Apply that invariant at the core
    // boundary too so clipboard line endings cannot reach protocol encoders.
    // Preserve spaces and every other character because they may be valid.
    value
        .chars()
        .filter(|character| !matches!(character, '\r' | '\n'))
        .collect()
}

#[cfg(test)]
mod account_input_tests {
    use super::{normalize_account_email, normalize_account_password};

    #[test]
    fn account_email_preserves_local_part_and_normalizes_domain() {
        assert_eq!(
            normalize_account_email(" User.Name@EXAMPLE.TEST ").as_deref(),
            Some("User.Name@example.test")
        );
        assert!(normalize_account_email("missing-domain@").is_none());
        assert!(normalize_account_email("two@@example.test").is_none());
        assert!(normalize_account_email("space @example.test").is_none());
    }

    #[test]
    fn pasted_password_line_breaks_are_removed_without_trimming_spaces() {
        assert_eq!(
            normalize_account_password("\r\n secret\nvalue \r\n".into()),
            " secretvalue "
        );
    }
}

/// An AI feature, each routed to a configurable model tier.
#[derive(Clone, Copy)]
enum Scenario {
    /// Ask-your-inbox agentic Q&A.
    Ask,
    /// Drafting / rewriting replies.
    Draft,
    /// Thread summaries.
    Summarize,
    /// Learning the user's writing voice.
    Voice,
    /// Palette natural-language commands (tiny prompt, latency-sensitive).
    Command,
    /// One-tap quick-reply chips (tiny output, latency-sensitive).
    QuickReply,
    /// Sorting incoming mail into categories at sync (short prompt, per-email).
    Categorize,
}

impl Scenario {
    fn as_str(self) -> &'static str {
        match self {
            Scenario::Ask => "ask",
            Scenario::Draft => "draft",
            Scenario::Summarize => "summarize",
            Scenario::Voice => "voice",
            Scenario::Command => "command",
            Scenario::QuickReply => "quick_reply",
            Scenario::Categorize => "automation",
        }
    }
}

/// Resolve the canonical model id for a scenario's configured tier.
fn resolve_ai_model(settings: &Settings, scenario: Scenario) -> String {
    let tier = match scenario {
        Scenario::Ask => settings.ai_tier_ask.as_str(),
        Scenario::Draft => settings.ai_tier_draft.as_str(),
        Scenario::Summarize => settings.ai_tier_summarize.as_str(),
        Scenario::Voice => settings.ai_tier_voice.as_str(),
        Scenario::Categorize => settings.ai_tier_categorize.as_str(),
        // Palette parsing and reply chips want the fastest model available.
        Scenario::Command | Scenario::QuickReply => "instant",
    };
    match tier {
        "instant" => &settings.ai_model_instant,
        "cheap" => &settings.ai_model_cheap,
        "intelligent" => &settings.ai_model_intelligent,
        _ => &settings.ai_model_intelligent,
    }
    .clone()
}

fn oauth_calendar_scopes(provider: Provider, enabled: bool) -> &'static [&'static str] {
    if !enabled {
        return &[];
    }
    match provider {
        Provider::Gmail => &[oauth::providers::GOOGLE_CALENDAR_SCOPE],
        Provider::Microsoft => &[
            oauth::providers::MS_ONLINE_MEETINGS_SCOPE,
            oauth::providers::MS_CALENDARS_SCOPE,
        ],
        Provider::Imap => &[],
    }
}

#[cfg(test)]
mod calendar_oauth_scope_tests {
    use super::*;

    #[test]
    fn mail_only_sign_in_never_requests_calendar_permissions() {
        assert!(oauth_calendar_scopes(Provider::Gmail, false).is_empty());
        assert!(oauth_calendar_scopes(Provider::Microsoft, false).is_empty());
    }

    #[test]
    fn opt_in_requests_the_provider_specific_calendar_permissions() {
        assert_eq!(
            oauth_calendar_scopes(Provider::Gmail, true),
            &[oauth::providers::GOOGLE_CALENDAR_SCOPE]
        );
        let microsoft = oauth_calendar_scopes(Provider::Microsoft, true);
        assert!(microsoft.contains(&oauth::providers::MS_CALENDARS_SCOPE));
        assert!(microsoft.contains(&oauth::providers::MS_ONLINE_MEETINGS_SCOPE));
    }
}

#[derive(Clone)]
pub struct ThreadPageRequest {
    pub cursor: Option<ThreadCursor>,
    pub limit: i64,
}

impl From<(Option<ThreadCursor>, i64)> for ThreadPageRequest {
    fn from((cursor, limit): (Option<ThreadCursor>, i64)) -> Self {
        Self { cursor, limit }
    }
}

#[derive(Clone)]
pub struct Core {
    /// Mail, contacts, folders, and mail-side durable actions.
    pub db: Db,
    /// Events and provider calendar sync state. Kept physically separate from
    /// the mail store so mailbox backfills cannot block the calendar UI.
    pub calendar_db: Db,
    /// Remote files, sync cursors and durable transfer operations.
    pub files_db: Db,
    pub(crate) file_work_lock: Arc<tokio::sync::Mutex<()>>,
    pub bus: EventBus,
    paths: Arc<Paths>,
    tokens: TokenProvider,
    credentials: CredentialStoreHandle,
    oauth_redirects: OAuthRedirectBrokerHandle,
    handles: Arc<RwLock<HashMap<i64, AccountHandle>>>,
    cal_handles: Arc<RwLock<HashMap<i64, caldav::task::CalTaskHandle>>>,
    card_handles: Arc<RwLock<HashMap<i64, carddav::task::CardDavTaskHandle>>>,
    /// Per-attachment single-flight locks. Concurrent preview/open/save calls
    /// share one remote fetch and cannot race the same `.download.part` file.
    attachment_locks:
        Arc<tokio::sync::Mutex<HashMap<i64, std::sync::Weak<tokio::sync::Mutex<()>>>>>,
    #[cfg(feature = "local-embeddings")]
    embed: Arc<embed::EmbedState>,
    /// Fired by `cancel_oauth` to abort a pending browser sign-in (the
    /// loopback wait otherwise blocks the UI until its 15-minute timeout).
    oauth_cancel: Arc<tokio::sync::Notify>,
    /// Fired by `notify_ui_ready` once the Slint UI has finished its startup
    /// show (the first-run intro). Account actors wait for this before
    /// touching OAuth tokens, so the OS keyring prompt never lands on top of
    /// the intro. A timeout fallback keeps tray-only/headless syncing alive.
    ui_ready: Arc<tokio::sync::Notify>,
    /// Serializes AI routing passes so the background router and an inline
    /// re-sort never classify the same 'pending' threads twice.
    ai_router_lock: Arc<tokio::sync::Mutex<()>>,
}

impl Core {
    #[cfg(desktop)]
    pub async fn start(paths: Paths) -> Result<Core> {
        Self::start_with_platform(
            paths,
            Arc::new(credentials::SystemCredentialStore),
            Arc::new(LoopbackRedirectBroker::default()),
            true,
        )
        .await
    }

    /// Start the core for a mail-only native shell. Mail synchronization,
    /// folder discovery, and keyword search stay fully enabled, while the
    /// optional local embedding model is left unloaded until a host that
    /// exposes semantic search asks for it.
    #[cfg(desktop)]
    pub async fn start_mail_ui(paths: Paths) -> Result<Core> {
        Self::start_mail_ui_with_credentials(paths, Arc::new(credentials::SystemCredentialStore))
            .await
    }

    /// Start a mail-only core with a host-provided credential backend. Mobile
    /// hosts use this entry point so no unsupported keyring fallback can run.
    pub async fn start_mail_ui_with_credentials(
        paths: Paths,
        credentials: CredentialStoreHandle,
    ) -> Result<Core> {
        Self::start_mail_ui_with_platform(
            paths,
            credentials,
            Arc::new(LoopbackRedirectBroker::default()),
        )
        .await
    }

    pub async fn start_mail_ui_with_platform(
        paths: Paths,
        credentials: CredentialStoreHandle,
        oauth_redirects: OAuthRedirectBrokerHandle,
    ) -> Result<Core> {
        Self::start_with_platform(paths, credentials, oauth_redirects, false).await
    }

    async fn start_with_platform(
        paths: Paths,
        credentials: CredentialStoreHandle,
        oauth_redirects: OAuthRedirectBrokerHandle,
        enable_embeddings: bool,
    ) -> Result<Core> {
        tracing::info!(
            data_dir = %paths.data_dir.display(),
            cache_dir = %paths.cache_dir.display(),
            "core startup: resolved application storage"
        );
        paths.ensure().map_err(|error| {
            CoreError::Other(format!(
                "creating application directories {} and {}: {error}",
                paths.data_dir.display(),
                paths.cache_dir.display()
            ))
        })?;
        let mail_db_path = paths.db_file();
        tracing::info!(database = %mail_db_path.display(), "core startup: opening mail database");
        let db = Db::open(&mail_db_path).map_err(|error| {
            CoreError::Other(format!(
                "opening mail database {}: {error}",
                mail_db_path.display()
            ))
        })?;
        // Product databases are opened by their first operation, not by Core construction.
        let calendar_db = Db::deferred_calendar(paths.calendar_db_file());
        let files_db = Db::deferred_files(paths.files_db_file());
        let bus = EventBus::new();
        let core = Core {
            db,
            calendar_db,
            files_db,
            file_work_lock: Arc::new(tokio::sync::Mutex::new(())),
            bus,
            paths: Arc::new(paths),
            tokens: TokenProvider::new(credentials.clone(), oauth_redirects.clone()),
            credentials,
            oauth_redirects,
            handles: Arc::new(RwLock::new(HashMap::new())),
            cal_handles: Arc::new(RwLock::new(HashMap::new())),
            card_handles: Arc::new(RwLock::new(HashMap::new())),
            attachment_locks: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            #[cfg(feature = "local-embeddings")]
            embed: Arc::new(embed::EmbedState::new()),
            oauth_cancel: Arc::new(tokio::sync::Notify::new()),
            ui_ready: Arc::new(tokio::sync::Notify::new()),
            ai_router_lock: Arc::new(tokio::sync::Mutex::new(())),
        };

        // Finish any destructive operation interrupted between the mail and
        // calendar stores, then remove calendar rows that predate the durable
        // operation journal and no longer have a mail account owner.
        core.recover_cross_store_state().await?;
        if core.paths.files_db_file().exists()
            || core
                .db
                .read(|conn| {
                    Ok(conn.query_row(
                        "SELECT EXISTS(SELECT 1 FROM app_settings WHERE key LIKE 'files:%')",
                        [],
                        |r| r.get::<_, bool>(0),
                    )?)
                })
                .await?
        {
            core.recover_files().await?;
            core.files_db.release_idle_connections().await?;
        }
        let removed_staged_files = core.cleanup_orphaned_draft_files().await?;
        if removed_staged_files > 0 {
            tracing::info!(removed_staged_files, "removed orphaned draft staging files");
        }

        // Make saved OAuth app registrations available before any actor
        // needs a token refresh.
        let settings = core.db.read(|conn| repo::settings::get(conn)).await?;
        apply_oauth_settings(&settings);

        // Recover any actions orphaned mid-flight by a previous crash/kill, so a
        // send that was executing when the app died retries instead of sticking
        // on "Sending…" forever.
        match core
            .db
            .write(|conn| repo::actions::recover_inflight(conn))
            .await
        {
            Ok(n) if n > 0 => tracing::info!("recovered {n} orphaned in-flight action(s)"),
            _ => {}
        }
        if core.calendar_db.is_open() {
            match core
                .calendar_db
                .write(|conn| repo::actions::recover_inflight(conn))
                .await
            {
                Ok(n) if n > 0 => tracing::info!("recovered {n} calendar action(s)"),
                _ => {}
            }
            core.calendar_db.release_idle_connections().await?;
        }
        match core
            .db
            .write(|conn| {
                Ok(conn.execute(
                    "UPDATE messages SET body_state = 'none' WHERE body_state = 'fetching'",
                    [],
                )?)
            })
            .await
        {
            Ok(n) if n > 0 => tracing::info!("recovered {n} orphaned content fetch(es)"),
            _ => {}
        }
        // A fresh profile has no snoozes, queued actions, mail actors or
        // calendar store to poll. Preserve startup work for existing profiles,
        // and let the scheduler activate after account setup or the first
        // calendar operation on a new profile.
        let scheduler_has_existing_work = core.paths.calendar_db_file().exists()
            || core
                .db
                .read(|conn| {
                    Ok(conn.query_row(
                        "SELECT EXISTS(SELECT 1 FROM accounts)
                             OR EXISTS(SELECT 1 FROM snoozes)
                             OR EXISTS(SELECT 1 FROM pending_actions)",
                        [],
                        |row| row.get::<_, bool>(0),
                    )?)
                })
                .await?;

        // Recovery and migration can leave both SQLite page caches warm after
        // scanning a large mailbox. Foreground startup needs only a small page
        // and badges; close these idle connections before spawning workers so
        // their recovery working set is not retained throughout the session.
        // The next read/write reopens its connection with the normal profile.
        core.db.release_idle_connections().await?;
        // Spawn actors for existing accounts - but only after the Slint UI
        // reports ready (notify_ui_ready). Actors immediately load OAuth
        // tokens from the OS keyring, and on a launch that plays the intro
        // that prompt must land after the show, not on top of it. The timeout
        // fallback keeps sync alive if no window ever reports in (tray-only
        // or a stalled startup).
        {
            let core = core.clone();
            tokio::spawn(async move {
                events::sync_diagnostic("startup_wait", None, "waiting_for_ui");
                tokio::select! {
                    _ = core.ui_ready.notified() => {}
                    _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {}
                }
                events::sync_diagnostic("startup_accounts", None, "loading");
                let configs = match core
                    .db
                    .read(|conn| repo::accounts::list_configs(conn))
                    .await
                {
                    Ok(c) => c,
                    Err(e) => {
                        events::sync_diagnostic("startup_accounts", None, e.code());
                        tracing::error!("listing accounts for startup sync: {e}");
                        return;
                    }
                };
                // Siftmail M1.5: stagger startup only for larger account sets;
                // one or two accounts connect immediately as before.
                let stagger = configs.len() >= sync::scheduler::STAGGER_MIN_ACCOUNTS;
                for cfg in configs {
                    if stagger {
                        core.spawn_actor_staggered(cfg).await;
                    } else {
                        core.spawn_actor(cfg).await;
                    }
                }
                events::sync_diagnostic("startup_workers", None, "ready");
                // Calendar sync tasks for accounts with a connected CalDAV server.
                if core.paths.calendar_db_file().exists()
                    && let Ok(cal_accounts) = core
                        .calendar_db
                        .read(|conn| repo::caldav::all_configs(conn))
                        .await
                {
                    for cfg in cal_accounts {
                        core.spawn_cal_task(cfg.account_id).await;
                    }
                }
                if let Ok(card_accounts) = core
                    .db
                    .read(|conn| repo::carddav::enabled_configs(conn))
                    .await
                {
                    for cfg in card_accounts {
                        core.spawn_card_task(cfg.account_id).await;
                    }
                    core.nudge_card(None).await;
                }
            });
        }

        scheduler::spawn(
            core.db.clone(),
            core.calendar_db.clone(),
            core.bus.clone(),
            core.handles.clone(),
            core.cal_handles.clone(),
            scheduler_has_existing_work,
        );

        #[cfg(feature = "local-embeddings")]
        if enable_embeddings {
            // Make the bundled default model available for offline first run,
            // then start the background embedding worker. Hosts that only
            // expose mailbox sync should not pay the memory cost of a local
            // BERT model they cannot use.
            core.provision_bundled_model().await;
            embed::worker::spawn(core.db.clone(), core.embed.clone(), core.paths.clone());
        }
        #[cfg(not(feature = "local-embeddings"))]
        let _ = enable_embeddings;

        // One-shot routing backfill: on a fresh 007 install or an upgrade to the
        // 014 routing column, resolve every existing thread's tab once. Guarded
        // by a marker row so it never repeats (preserved across reroute_all).
        {
            let c = core.clone();
            tokio::spawn(async move {
                let (marker, has_threads, auto) =
                    c.db.read(|conn| {
                        let s = repo::settings::get(conn)?;
                        let marker: i64 = conn.query_row(
                            "SELECT COUNT(*) FROM route_cache
                             WHERE sender_domain = '__routing_backfill__'",
                            [],
                            |r| r.get(0),
                        )?;
                        let has_threads = if marker == 0 && s.auto_labels_enabled {
                            conn.query_row("SELECT EXISTS(SELECT 1 FROM threads)", [], |r| {
                                r.get::<_, bool>(0)
                            })?
                        } else {
                            false
                        };
                        Ok((marker, has_threads, s.auto_labels_enabled))
                    })
                    .await
                    .unwrap_or((1, false, false));
                if marker == 0 {
                    if auto && has_threads {
                        match c.reroute_all().await {
                            Ok(n) => tracing::info!("routing backfill resolved {n} threads"),
                            Err(e) => tracing::warn!("routing backfill failed: {e}"),
                        }
                    }
                    let _ =
                        c.db.write(|conn| {
                            conn.execute(
                                "INSERT OR REPLACE INTO route_cache (sender_domain, route_key)
                                 VALUES ('__routing_backfill__', '1')",
                                [],
                            )?;
                            Ok(())
                        })
                        .await;
                }
            });
        }

        // Background AI router: drains threads marked 'pending' into a category
        // whenever mail changes. Decoupled from sync so LLM latency never blocks
        // the sync loop.
        {
            let c = core.clone();
            tokio::spawn(async move { c.ai_router_loop().await });
        }

        Ok(core)
    }

    /// Long-lived loop: on every mail change, drain the AI routing queue in
    /// bounded batches. Serial (one broadcast receiver), so batches never
    /// overlap; the self-emitted refresh settles after one empty pass.
    async fn ai_router_loop(&self) {
        use tokio::sync::broadcast::error::RecvError;
        let mut rx = self.bus.subscribe();
        loop {
            match rx.recv().await {
                Ok(CoreEvent::MailUpdated { .. }) => loop {
                    match self.classify_pending(50).await {
                        Ok(0) => break,
                        Ok(_) => continue,
                        Err(e) => {
                            tracing::warn!("ai routing pass failed: {e}");
                            break;
                        }
                    }
                },
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    }

    /// Copy the installer-bundled default model into the data dir if it isn't
    /// there yet, so semantic search works with no network on first launch.
    /// Best-effort: if the resource is missing (e.g. dev builds), the worker
    /// falls back to downloading on demand.
    #[cfg(feature = "local-embeddings")]
    async fn provision_bundled_model(&self) {
        let models_dir = self.paths.models_dir();
        let spec = embed::spec_or_default(embed::DEFAULT_MODEL);
        if embed::model_present(&models_dir, spec.key) {
            return;
        }
        if let Some(src) = bundled_model_dir(spec.key) {
            let dst = embed::model_dir(&models_dir, spec.key);
            if let Err(e) = copy_model_files(&src, &dst, spec).await {
                tracing::warn!("bundled model copy failed: {e}");
            }
        }
    }

    fn sync_ctx(&self) -> SyncCtx {
        SyncCtx {
            db: self.db.clone(),
            calendar_db: self.calendar_db.clone(),
            bus: self.bus.clone(),
            paths: self.paths.clone(),
            tokens: self.tokens.clone(),
            credentials: self.credentials.clone(),
            startup_stagger: false,
        }
    }

    async fn spawn_actor(&self, cfg: AccountConfig) {
        events::sync_diagnostic("spawn_worker", Some(cfg.id), "starting");
        let handle = spawn_account(self.sync_ctx(), cfg);
        self.handles.write().await.insert(handle.account_id, handle);
    }

    /// Startup spawn (Siftmail M1.5): the first connection is delayed by a
    /// random 0–10 minutes so many accounts never connect at once.
    async fn spawn_actor_staggered(&self, cfg: AccountConfig) {
        events::sync_diagnostic("spawn_worker", Some(cfg.id), "starting (staggered)");
        let ctx = SyncCtx {
            startup_stagger: true,
            ..self.sync_ctx()
        };
        let handle = spawn_account(ctx, cfg);
        self.handles.write().await.insert(handle.account_id, handle);
    }

    async fn spawn_cal_task(&self, account_id: i64) {
        let handle = caldav::task::spawn(
            self.calendar_db.clone(),
            self.db.clone(),
            self.bus.clone(),
            self.tokens.clone(),
            self.credentials.clone(),
            account_id,
        );
        self.cal_handles.write().await.insert(account_id, handle);
    }

    async fn spawn_card_task(&self, account_id: i64) {
        let handle = carddav::task::spawn(
            self.db.clone(),
            self.bus.clone(),
            self.credentials.clone(),
            account_id,
        );
        if let Some(previous) = self.card_handles.write().await.insert(account_id, handle) {
            previous.abort();
        }
    }

    async fn nudge_card(&self, account_id: Option<i64>) {
        let handles = self.card_handles.read().await;
        for handle in handles
            .values()
            .filter(|handle| account_id.is_none_or(|id| id == handle.account_id))
        {
            handle.nudge();
        }
    }

    async fn nudge_cal(&self, account_id: Option<i64>) {
        let handles = self.cal_handles.read().await;
        match account_id {
            Some(id) => {
                if let Some(h) = handles.get(&id) {
                    h.nudge();
                }
            }
            None => {
                for h in handles.values() {
                    h.nudge();
                }
            }
        }
    }

    async fn nudge(&self, account_id: Option<i64>, cmd_for: impl Fn() -> SyncCmd) {
        let handles = self.handles.read().await;
        match account_id {
            Some(id) => {
                if let Some(h) = handles.get(&id) {
                    h.send(cmd_for());
                }
            }
            None => {
                for h in handles.values() {
                    h.send(cmd_for());
                }
            }
        }
    }
}

/// Build a (incoming → the user's reply) example from one of their sent
/// messages: the reply is its body, the incoming side is the message it
/// replied to (the prior message in its thread), or a synthetic prompt if it
/// started the thread. Returns None if the sent body is empty.
fn build_example_pair(
    conn: &rusqlite::Connection,
    sent_id: i64,
) -> Result<Option<(String, String)>> {
    let sent = repo::messages::detail(conn, sent_id)?;
    let reply = sent.text_body.clone().unwrap_or_default();
    if reply.trim().is_empty() {
        return Ok(None);
    }
    let msgs = repo::messages::list_for_thread(conn, sent.thread_id)?;
    let mut incoming: Option<&MessageDetail> = None;
    for m in &msgs {
        if m.id == sent_id {
            break;
        }
        incoming = Some(m);
    }
    let incoming_text = match incoming {
        Some(m) => {
            let body = m.text_body.clone().unwrap_or_else(|| m.snippet.clone());
            format!("Subject: {}\nFrom: {}\n{}", m.subject, m.from.email, body)
        }
        None => format!("(Compose a new email. Subject: {})", sent.subject),
    };
    Ok(Some((incoming_text, reply)))
}

/// Locate model files bundled by a native host. The host sets
/// `FLECTAR_MAIL_RESOURCE_DIR` to the platform-specific resource directory.
#[cfg(feature = "local-embeddings")]
fn bundled_model_dir(key: &str) -> Option<std::path::PathBuf> {
    let base = std::env::var_os("FLECTAR_MAIL_RESOURCE_DIR")?;
    let dir = std::path::PathBuf::from(base).join("models").join(key);
    dir.join("model.safetensors").exists().then_some(dir)
}

#[cfg(feature = "local-embeddings")]
async fn copy_model_files(
    src: &std::path::Path,
    dst: &std::path::Path,
    spec: &'static embed::ModelSpec,
) -> Result<()> {
    let verified_source = src.to_path_buf();
    tokio::task::spawn_blocking(move || embed::verify_model_files(&verified_source, spec))
        .await
        .map_err(|error| CoreError::Other(format!("bundled model verification task: {error}")))??;

    let parent = dst
        .parent()
        .ok_or_else(|| CoreError::Other("bundled model destination has no parent".into()))?;
    tokio::fs::create_dir_all(parent).await?;
    let nonce = rand::random::<u64>();
    let staging = parent.join(format!(".{}-{nonce:016x}.bundled", spec.key));
    let backup = parent.join(format!(".{}-{nonce:016x}.replaced", spec.key));
    tokio::fs::create_dir(&staging).await?;
    for artifact in spec.artifacts {
        if let Err(error) = crate::file_io::copy(
            src.join(artifact.filename),
            staging.join(artifact.filename),
            usize::try_from(artifact.bytes).unwrap_or(usize::MAX),
            "bundled model artifact",
        )
        .await
        {
            let _ = tokio::fs::remove_dir_all(&staging).await;
            return Err(error);
        }
    }
    let verified_staging = staging.clone();
    let verification = match tokio::task::spawn_blocking(move || {
        embed::verify_model_files(&verified_staging, spec)
    })
    .await
    {
        Ok(result) => result,
        Err(error) => Err(CoreError::Other(format!(
            "bundled model verification task: {error}"
        ))),
    };
    if let Err(error) = verification {
        let _ = tokio::fs::remove_dir_all(&staging).await;
        return Err(error);
    }

    let had_existing = tokio::fs::symlink_metadata(dst).await.is_ok();
    if had_existing && let Err(error) = tokio::fs::rename(dst, &backup).await {
        let _ = tokio::fs::remove_dir_all(&staging).await;
        return Err(error.into());
    }
    if let Err(error) = tokio::fs::rename(&staging, dst).await {
        if had_existing {
            let _ = tokio::fs::rename(&backup, dst).await;
        }
        let _ = tokio::fs::remove_dir_all(&staging).await;
        return Err(error.into());
    }
    if had_existing {
        let _ = tokio::fs::remove_dir_all(backup).await;
    }
    Ok(())
}

/// Reduce an untrusted filename to a single, benign path component: strip
/// separators/NUL/control chars, leading dots and spaces, and cap the length.
/// A file extension for a MIME type, used to name an on-disk attachment when
/// the message gave it no filename. Without a recognizable extension the OS
/// can't route "Open in app" to the right handler (e.g. a `text/calendar`
/// invite written as a bare `attachment-3` never reaches the calendar app).
fn ext_for_mime(mime: &str) -> Option<&'static str> {
    Some(
        match mime
            .split(';')
            .next()
            .unwrap_or(mime)
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "text/calendar" => "ics",
            "application/pdf" => "pdf",
            "text/plain" => "txt",
            "text/html" => "html",
            "text/csv" => "csv",
            "application/json" => "json",
            "application/zip" => "zip",
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/gif" => "gif",
            "image/webp" => "webp",
            "image/svg+xml" => "svg",
            "message/rfc822" => "eml",
            _ => return None,
        },
    )
}

fn safe_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == '\0' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches(['.', ' ']);
    let base = if trimmed.is_empty() {
        "attachment"
    } else {
        trimmed
    };
    base.chars().take(200).collect()
}

/// Copy a composer-picked file into the app-managed draft-attachment staging
/// area and return the staged absolute path. Only a path already owned by this
/// same saved draft is reused; another draft's staged file is copied so later
/// replacement cannot delete a shared path. This guarantees dispatch only ever
/// reads files the app itself wrote, closing an arbitrary local-file
/// read/exfiltration path through the `save_draft` API.
async fn stage_draft_attachment(
    root: &std::path::Path,
    src: &str,
    filename: &str,
    max_bytes: usize,
    reusable_paths: &HashSet<std::path::PathBuf>,
) -> Result<(String, usize, bool)> {
    tokio::fs::create_dir_all(root).await?;
    let root = tokio::fs::canonicalize(root).await?;
    let canon_src = tokio::fs::canonicalize(src)
        .await
        .map_err(|e| CoreError::Other(format!("attachment {filename}: {e}")))?;
    if canon_src.starts_with(&root) && reusable_paths.contains(&canon_src) {
        // Already staged (e.g. re-saving a draft reloaded from the DB).
        let size = tokio::fs::metadata(&canon_src).await?.len();
        let size = usize::try_from(size).map_err(|_| {
            CoreError::Other(format!(
                "attachment {filename} is too large for this platform"
            ))
        })?;
        if size > max_bytes {
            return Err(CoreError::Other(format!(
                "draft attachments exceed the {} MiB safety limit",
                MAX_DRAFT_ATTACHMENT_BYTES / (1024 * 1024)
            )));
        }
        return Ok((canon_src.to_string_lossy().into_owned(), size, false));
    }
    let sub = root.join(crate::mime::rand_token());
    tokio::fs::create_dir_all(&sub).await?;
    let dst = sub.join(safe_filename(filename));
    let size = match crate::file_io::copy(&canon_src, &dst, max_bytes, "draft attachment").await {
        Ok(size) => size,
        Err(error) => {
            let _ = tokio::fs::remove_dir(&sub).await;
            return Err(error);
        }
    };
    Ok((dst.to_string_lossy().into_owned(), size, true))
}

pub(crate) async fn remove_staged_attachment(root: &std::path::Path, path: &str) {
    let (Ok(root), Ok(path)) = (
        tokio::fs::canonicalize(root).await,
        tokio::fs::canonicalize(path).await,
    ) else {
        return;
    };
    if !path.starts_with(&root) || path == root {
        return;
    }
    let parent = path.parent().map(std::path::Path::to_path_buf);
    let _ = tokio::fs::remove_file(&path).await;
    if let Some(parent) = parent.filter(|parent| parent.starts_with(&root) && parent != &root) {
        let _ = tokio::fs::remove_dir(parent).await;
    }
}

/// Plain-text body for an outgoing invite email (the ICS carries the real
/// event; this is what non-calendar clients show).
fn invite_body_text(args: &CreateEventArgs) -> String {
    use chrono::TimeZone;
    let fmt = |ms: i64| {
        chrono::Local
            .timestamp_millis_opt(ms)
            .earliest()
            .map(|dt| {
                if args.all_day {
                    dt.format("%a, %b %e, %Y").to_string()
                } else {
                    dt.format("%a, %b %e, %Y at %H:%M").to_string()
                }
            })
            .unwrap_or_default()
    };
    let mut out = format!(
        "You are invited: {}\n\nWhen: {} - {}\n",
        args.summary.trim(),
        fmt(args.starts_at),
        fmt(args.ends_at)
    );
    if let Some(loc) = args.location.as_deref().filter(|l| !l.is_empty()) {
        out.push_str(&format!("Where: {loc}\n"));
    }
    if let Some(url) = args.join_url.as_deref().filter(|u| !u.is_empty()) {
        out.push_str(&format!("Join: {url}\n"));
    }
    if let Some(desc) = args.description.as_deref().filter(|d| !d.is_empty()) {
        out.push_str(&format!("\n{desc}\n"));
    }
    out
}

/// Push user-entered OAuth app registrations into the resolver.
fn apply_oauth_settings(settings: &Settings) {
    oauth::providers::set_configured(
        Provider::Gmail,
        &settings.google_client_id,
        &settings.google_client_secret,
    );
    oauth::providers::set_configured(
        Provider::Microsoft,
        &settings.ms_client_id,
        &settings.ms_client_secret,
    );
}

/// Optimistic local mutation + enqueue, in one transaction.
/// Returns (action_id, account_id) pairs.
fn apply_thread_action(
    conn: &mut rusqlite::Connection,
    thread_id: i64,
    kind: ActionKind,
    params: Option<&ActionParams>,
) -> Result<Vec<(i64, i64)>> {
    let tx = conn.transaction()?;
    let mut out: Vec<(i64, i64)> = Vec::new();

    let (account_id, account_provider, mail_protocol): (i64, String, String) = tx.query_row(
        "SELECT t.account_id, a.provider, a.mail_protocol
         FROM threads t JOIN accounts a ON a.id = t.account_id
         WHERE t.id = ?1",
        rusqlite::params![thread_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let gmail = account_provider == "gmail";
    let multi_mailbox = gmail || mail_protocol == "jmap";

    // Most thread actions intentionally ignore drafts. Archive and Trash are
    // exceptions: users can file or discard a draft from the Drafts view, and
    // excluding it here made the optimistic removal snap back on reconciliation.
    let include_drafts = matches!(
        kind,
        ActionKind::Archive | ActionKind::Trash | ActionKind::DeletePermanently
    );
    let mut stmt = tx.prepare(
        "SELECT m.id, m.folder_id, m.uid, m.is_read, m.is_starred, COALESCE(f.role,'')
         FROM messages m LEFT JOIN folders f ON f.id = m.folder_id
         WHERE m.thread_id = ?1 AND (m.is_draft = 0 OR ?2 = 1)",
    )?;
    #[allow(clippy::type_complexity)]
    let msgs: Vec<(i64, Option<i64>, Option<i64>, bool, bool, String)> = stmt
        .query_map(rusqlite::params![thread_id, include_drafts as i64], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get::<_, i64>(3)? != 0,
                r.get::<_, i64>(4)? != 0,
                r.get(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    fn folder_of(tx: &rusqlite::Transaction, account_id: i64, role: &str) -> Result<Option<i64>> {
        Ok(repo::folders::by_role(tx, account_id, role)?.map(|f| f.id))
    }

    #[allow(clippy::too_many_arguments)]
    fn enqueue_move(
        tx: &rusqlite::Transaction,
        account_id: i64,
        thread_id: i64,
        msg_id: i64,
        src_folder: Option<i64>,
        src_uid: Option<i64>,
        target: i64,
        kind_str: &str,
        multi_mailbox: bool,
    ) -> Result<i64> {
        let payload = serde_json::json!({
            "srcFolderId": src_folder,
            "srcUid": src_uid,
            "targetFolderId": target,
        });
        repo::messages::set_uid_and_folder(tx, msg_id, target, None)?;
        if multi_mailbox {
            if let Some(src_folder) = src_folder {
                tx.execute(
                    "DELETE FROM message_folders WHERE message_id = ?1 AND folder_id = ?2",
                    rusqlite::params![msg_id, src_folder],
                )?;
            }
            tx.execute(
                "INSERT OR IGNORE INTO message_folders (message_id, folder_id)
                 VALUES (?1, ?2)",
                rusqlite::params![msg_id, target],
            )?;
        }
        let aid = repo::actions::enqueue(
            tx,
            account_id,
            kind_str,
            Some(msg_id),
            Some(thread_id),
            &payload,
            None,
        )?;
        Ok(aid)
    }

    match kind {
        ActionKind::DeletePermanently => {
            for (id, f, u, _r, _s, role) in &msgs {
                let in_trash = role == roles::TRASH
                    || tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM message_folders mf
                     JOIN folders folder ON folder.id = mf.folder_id
                     WHERE mf.message_id = ?1 AND folder.role = 'trash')",
                        rusqlite::params![id],
                        |row| row.get::<_, bool>(0),
                    )?;
                if !in_trash {
                    continue;
                }
                let already_queued: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM pending_actions
                     WHERE message_id = ?1 AND kind = 'delete_permanently'
                       AND state IN ('pending','inflight'))",
                    rusqlite::params![id],
                    |row| row.get(0),
                )?;
                if already_queued {
                    continue;
                }
                let aid = repo::actions::enqueue(
                    &tx,
                    account_id,
                    kind.as_str(),
                    Some(*id),
                    Some(thread_id),
                    &serde_json::json!({"folderId": f, "uid": u}),
                    None,
                )?;
                out.push((aid, account_id));
            }
        }
        ActionKind::MarkRead | ActionKind::MarkUnread => {
            let target_read = kind == ActionKind::MarkRead;
            for (id, _f, _u, is_read, _s, _role) in &msgs {
                if *is_read != target_read {
                    repo::messages::set_read(&tx, *id, target_read)?;
                    let payload = serde_json::json!({});
                    let aid = repo::actions::enqueue(
                        &tx,
                        account_id,
                        kind.as_str(),
                        Some(*id),
                        Some(thread_id),
                        &payload,
                        None,
                    )?;
                    out.push((aid, account_id));
                }
            }
        }
        ActionKind::Star => {
            // Star the latest message only (thread-level star).
            if let Some((id, _f, _u, _r, is_starred, _role)) = msgs.iter().max_by_key(|m| m.0) {
                if *is_starred {
                    repo::threads::recompute(&tx, thread_id)?;
                    tx.commit()?;
                    return Ok(out);
                }
                repo::messages::set_starred(&tx, *id, true)?;
                let aid = repo::actions::enqueue(
                    &tx,
                    account_id,
                    "star",
                    Some(*id),
                    Some(thread_id),
                    &serde_json::json!({}),
                    None,
                )?;
                out.push((aid, account_id));
            }
        }
        ActionKind::Unstar => {
            for (id, _f, _u, _r, is_starred, _role) in &msgs {
                if *is_starred {
                    repo::messages::set_starred(&tx, *id, false)?;
                    let aid = repo::actions::enqueue(
                        &tx,
                        account_id,
                        "unstar",
                        Some(*id),
                        Some(thread_id),
                        &serde_json::json!({}),
                        None,
                    )?;
                    out.push((aid, account_id));
                }
            }
        }
        ActionKind::Archive | ActionKind::Trash | ActionKind::Spam => {
            let (target_role, kind_str) = match kind {
                ActionKind::Archive => (roles::ARCHIVE, "archive"),
                ActionKind::Trash => (roles::TRASH, "trash"),
                _ => (roles::SPAM, "spam"),
            };
            // Gmail-style fallback: archiving with no Archive folder moves to All Mail.
            let target = folder_of(&tx, account_id, target_role)?
                .or(if kind == ActionKind::Archive {
                    folder_of(&tx, account_id, roles::ALL)?
                } else {
                    None
                })
                .ok_or_else(|| CoreError::NotFound(format!("no {target_role} folder")))?;
            for (id, f, u, _r, _s, role) in &msgs {
                let movable = match kind {
                    ActionKind::Archive => role == roles::INBOX || role == roles::DRAFTS,
                    _ => role != target_role && !role.is_empty(),
                };
                if movable && f.is_some() {
                    let aid = enqueue_move(
                        &tx,
                        account_id,
                        thread_id,
                        *id,
                        *f,
                        *u,
                        target,
                        kind_str,
                        multi_mailbox,
                    )?;
                    out.push((aid, account_id));
                }
            }
            // Archiving also clears snooze.
            repo::snoozes::clear(&tx, thread_id)?;
        }
        ActionKind::Unarchive | ActionKind::NotSpam => {
            let target = folder_of(&tx, account_id, roles::INBOX)?
                .ok_or_else(|| CoreError::NotFound("no inbox folder".into()))?;
            let from_role = if kind == ActionKind::Unarchive {
                roles::ARCHIVE
            } else {
                roles::SPAM
            };
            for (id, f, u, _r, _s, role) in &msgs {
                if (role == from_role || (kind == ActionKind::Unarchive && role == roles::ALL))
                    && f.is_some()
                {
                    let aid = enqueue_move(
                        &tx,
                        account_id,
                        thread_id,
                        *id,
                        *f,
                        *u,
                        target,
                        kind.as_str(),
                        multi_mailbox,
                    )?;
                    out.push((aid, account_id));
                }
            }
        }
        ActionKind::Move => {
            let target = params
                .and_then(|p| p.target_folder_id)
                .ok_or_else(|| CoreError::Other("move requires targetFolderId".into()))?;
            let target_folder = repo::folders::get(&tx, target)?
                .ok_or_else(|| CoreError::NotFound(format!("folder {target}")))?;
            if target_folder.account_id != account_id {
                return Err(CoreError::Other(
                    "messages cannot be moved between accounts".into(),
                ));
            }
            for (id, f, u, _r, _s, _role) in &msgs {
                if f.is_some() && *f != Some(target) {
                    let aid = enqueue_move(
                        &tx,
                        account_id,
                        thread_id,
                        *id,
                        *f,
                        *u,
                        target,
                        "move",
                        multi_mailbox,
                    )?;
                    out.push((aid, account_id));
                }
            }
        }
        ActionKind::Snooze => {
            let wake_at = params
                .and_then(|p| p.wake_at)
                .ok_or_else(|| CoreError::Other("snooze requires wakeAt".into()))?;
            let orig = msgs.iter().find_map(|(_, f, ..)| *f);
            repo::snoozes::set(&tx, thread_id, account_id, wake_at, orig)?;
            let aid = repo::actions::enqueue(
                &tx,
                account_id,
                "snooze",
                None,
                Some(thread_id),
                &serde_json::json!({ "wakeAt": wake_at }),
                None,
            )?;
            out.push((aid, account_id));
        }
        ActionKind::Unsnooze => {
            repo::snoozes::clear(&tx, thread_id)?;
            let aid = repo::actions::enqueue(
                &tx,
                account_id,
                "unsnooze",
                None,
                Some(thread_id),
                &serde_json::json!({}),
                None,
            )?;
            out.push((aid, account_id));
        }
        ActionKind::AddLabel | ActionKind::RemoveLabel => {
            let label_id = params
                .and_then(|p| p.label_id)
                .ok_or_else(|| CoreError::Other("label action requires labelId".into()))?;
            let label = repo::labels::get(&tx, label_id)?
                .ok_or_else(|| CoreError::NotFound(format!("label {label_id}")))?;
            if let Some(owner_account_id) = label.owner_account_id
                && owner_account_id != account_id
            {
                return Err(CoreError::Other(format!(
                    "label '{}' belongs to another account",
                    label.name
                )));
            }
            let add = kind == ActionKind::AddLabel;
            let payload = serde_json::json!({ "labelId": label_id, "keyword": label.keyword });
            for (id, ..) in &msgs {
                if add {
                    repo::labels::add_to_message(&tx, *id, label_id)?;
                } else {
                    repo::labels::remove_from_message(&tx, *id, label_id)?;
                }
                // Auto labels are local-only: mutate membership but never push
                // their keyword to IMAP (server reconcile also skips them).
                if label.is_auto || label.owner_account_id.is_none() {
                    continue;
                }
                let aid = repo::actions::enqueue(
                    &tx,
                    account_id,
                    kind.as_str(),
                    Some(*id),
                    Some(thread_id),
                    &payload,
                    None,
                )?;
                out.push((aid, account_id));
            }
        }
        ActionKind::Send => {
            return Err(CoreError::Other("use queue_send for sending".into()));
        }
    }

    repo::threads::recompute(&tx, thread_id)?;
    tx.commit()?;
    Ok(out)
}

#[cfg(test)]
mod move_action_validation_tests {
    use super::*;

    #[tokio::test]
    async fn empty_trash_does_not_remove_mail_added_during_provider_purge() {
        let tmp = tempfile::tempdir().unwrap();
        let core = Core::start(Paths::for_tests(tmp.path())).await.unwrap();
        let old_id = core
            .db
            .write(|conn| {
                db::testutil::seed_account(conn);
                conn.execute(
                    "INSERT INTO folders (id, account_id, imap_name, role)
                     VALUES (2, 1, 'Trash', 'trash')",
                    [],
                )?;
                let (_, id) = db::testutil::seed_message(conn, "sender@test.dev", "old", false);
                conn.execute(
                    "UPDATE messages SET folder_id = 2 WHERE id = ?1",
                    rusqlite::params![id],
                )?;
                Ok(id)
            })
            .await
            .unwrap();
        let ctx = core.sync_ctx();
        let snapshot = sync::snapshot_trash_ids(&ctx, 1).await.unwrap();
        let new_id = core
            .db
            .write(|conn| {
                let (_, id) = db::testutil::seed_message(conn, "sender@test.dev", "new", false);
                conn.execute(
                    "UPDATE messages SET folder_id = 2 WHERE id = ?1",
                    rusqlite::params![id],
                )?;
                Ok(id)
            })
            .await
            .unwrap();
        sync::finish_empty_trash(&ctx, 1, snapshot).await.unwrap();
        let (old, new) = core
            .db
            .read(move |conn| {
                Ok((
                    repo::messages::get_row(conn, old_id)?,
                    repo::messages::get_row(conn, new_id)?,
                ))
            })
            .await
            .unwrap();
        assert!(old.is_none());
        assert!(new.is_some());
    }

    #[tokio::test]
    async fn local_draft_purge_cleans_attachment_but_remote_move_waits_for_trash_uid() {
        let tmp = tempfile::tempdir().unwrap();
        let core = Core::start(Paths::for_tests(tmp.path())).await.unwrap();
        let staged_path = core
            .paths
            .draft_attachments_dir()
            .join("test")
            .join("file.txt");
        std::fs::create_dir_all(staged_path.parent().unwrap()).unwrap();
        std::fs::write(&staged_path, b"attachment").unwrap();
        let staged_path_string = staged_path.to_string_lossy().into_owned();
        let draft_id = core
            .db
            .write(move |conn| {
                db::testutil::seed_account(conn);
                conn.execute(
                    "INSERT INTO folders (id, account_id, imap_name, role)
                     VALUES (2, 1, 'Trash', 'trash')",
                    [],
                )?;
                let (_, draft_id) =
                    db::testutil::seed_message(conn, "sender@test.dev", "draft", false);
                conn.execute(
                    "UPDATE messages SET is_draft = 1, uid = NULL, folder_id = 2 WHERE id = ?1",
                    rusqlite::params![draft_id],
                )?;
                conn.execute(
                    "INSERT INTO drafts_meta (message_id) VALUES (?1)",
                    rusqlite::params![draft_id],
                )?;
                conn.execute(
                    "INSERT INTO draft_attachments (draft_id, file_path, filename)
                     VALUES (?1, ?2, 'file.txt')",
                    rusqlite::params![draft_id, staged_path_string],
                )?;
                Ok(draft_id)
            })
            .await
            .unwrap();
        let ctx = core.sync_ctx();
        assert!(sync::is_local_only_draft(&ctx, draft_id).await.unwrap());
        let unlinked_remote_draft = core
            .db
            .write(|conn| {
                let (_, id) = db::testutil::seed_message(conn, "sender@test.dev", "remote", false);
                conn.execute(
                    "UPDATE messages SET is_draft = 1, uid = NULL, folder_id = 2 WHERE id = ?1",
                    rusqlite::params![id],
                )?;
                Ok(id)
            })
            .await
            .unwrap();
        assert!(
            !sync::is_local_only_draft(&ctx, unlinked_remote_draft)
                .await
                .unwrap()
        );
        core.db
            .write(move |conn| {
                repo::actions::enqueue(
                    conn,
                    1,
                    "trash",
                    Some(draft_id),
                    None,
                    &serde_json::json!({"srcUid": 42}),
                    None,
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(!sync::is_local_only_draft(&ctx, draft_id).await.unwrap());
        sync::finish_permanent_delete(&ctx, Some(draft_id))
            .await
            .unwrap();
        assert!(!staged_path.exists());
        assert!(
            core.db
                .read(move |conn| repo::messages::get_row(conn, draft_id))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn permanent_delete_queues_only_trash_messages_and_keeps_them_until_confirmed() {
        let mut conn = db::testutil::conn();
        db::testutil::seed_account(&conn);
        conn.execute(
            "INSERT INTO folders (id, account_id, imap_name, role)
                      VALUES (2, 1, 'Trash', 'trash')",
            [],
        )
        .unwrap();
        let (thread_id, inbox_id) =
            db::testutil::seed_message(&conn, "sender@test.dev", "conversation", false);
        let (_, trash_id) =
            db::testutil::seed_message(&conn, "sender@test.dev", "old reply", false);
        conn.execute(
            "UPDATE messages SET thread_id = ?1, folder_id = 2, uid = 42 WHERE id = ?2",
            rusqlite::params![thread_id, trash_id],
        )
        .unwrap();

        let actions =
            apply_thread_action(&mut conn, thread_id, ActionKind::DeletePermanently, None).unwrap();
        assert_eq!(actions.len(), 1);
        let action = repo::actions::get(&conn, actions[0].0).unwrap().unwrap();
        assert_eq!(action.message_id, Some(trash_id));
        assert_eq!(action.kind, "delete_permanently");
        assert!(repo::messages::get_row(&conn, trash_id).unwrap().is_some());
        assert!(repo::messages::get_row(&conn, inbox_id).unwrap().is_some());
        assert!(
            apply_thread_action(&mut conn, thread_id, ActionKind::DeletePermanently, None)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn move_rejects_a_folder_owned_by_another_account_without_local_mutation() {
        let mut conn = db::testutil::conn();
        db::testutil::seed_account(&conn);
        conn.execute(
            "INSERT INTO accounts (id, email, provider, auth_kind, username,
             imap_host, imap_port, smtp_host, smtp_port, created_at)
             VALUES (2,'other@test.dev','imap','password','other','h',993,'h',587,0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folders (id, account_id, imap_name, role)
             VALUES (2, 2, 'Archive', 'archive')",
            [],
        )
        .unwrap();
        let (thread_id, message_id) =
            db::testutil::seed_message(&conn, "sender@test.dev", "cross account", false);
        let before = repo::messages::get_row(&conn, message_id).unwrap().unwrap();

        let error = apply_thread_action(
            &mut conn,
            thread_id,
            ActionKind::Move,
            Some(&ActionParams {
                wake_at: None,
                target_folder_id: Some(2),
                label_id: None,
            }),
        )
        .unwrap_err();

        assert!(error.to_string().contains("between accounts"));
        let after = repo::messages::get_row(&conn, message_id).unwrap().unwrap();
        assert_eq!(after.folder_id, before.folder_id);
        assert_eq!(after.uid, before.uid);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM pending_actions", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn label_action_rejects_a_label_owned_by_another_account() {
        let mut conn = db::testutil::conn();
        db::testutil::seed_account(&conn);
        conn.execute(
            "INSERT INTO accounts (id, email, provider, auth_kind, username,
             imap_host, imap_port, smtp_host, smtp_port, created_at)
             VALUES (2,'other@test.dev','imap','password','other','h',993,'h',587,0)",
            [],
        )
        .unwrap();
        let (thread_id, message_id) =
            db::testutil::seed_message(&conn, "sender@test.dev", "cross account label", false);
        let label = repo::labels::save(&conn, None, "Travel", "#123456", 0, Some(2)).unwrap();

        let error = apply_thread_action(
            &mut conn,
            thread_id,
            ActionKind::AddLabel,
            Some(&ActionParams {
                wake_at: None,
                target_folder_id: None,
                label_id: Some(label.id),
            }),
        )
        .unwrap_err();

        assert!(error.to_string().contains("another account"));
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM message_labels WHERE message_id = ?1",
                [message_id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM pending_actions", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn global_label_action_stays_local() {
        let mut conn = db::testutil::conn();
        db::testutil::seed_account(&conn);
        let (thread_id, message_id) =
            db::testutil::seed_message(&conn, "sender@test.dev", "global label", false);
        let label = repo::labels::save(&conn, None, "Follow up", "#123456", 0, None).unwrap();

        let actions = apply_thread_action(
            &mut conn,
            thread_id,
            ActionKind::AddLabel,
            Some(&ActionParams {
                wake_at: None,
                target_folder_id: None,
                label_id: Some(label.id),
            }),
        )
        .unwrap();

        assert!(actions.is_empty());
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM message_labels
                  WHERE message_id = ?1 AND label_id = ?2",
                rusqlite::params![message_id, label.id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM pending_actions", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}

/// Inverse of an action: cancel if pending, revert the local mutation, and
/// enqueue a compensating remote action when the original already ran.
/// Returns the affected thread id.
fn revert_action(
    conn: &mut rusqlite::Connection,
    action: &repo::actions::PendingAction,
) -> Result<Option<i64>> {
    let was_pending = repo::actions::try_cancel(conn, action.id)?;
    let tx = conn.transaction()?;
    let thread_id = action.thread_id;

    match action.kind.as_str() {
        "mark_read" | "mark_unread" => {
            if let Some(mid) = action.message_id {
                repo::messages::set_read(&tx, mid, action.kind == "mark_unread")?;
                if !was_pending {
                    let inverse = if action.kind == "mark_read" {
                        "mark_unread"
                    } else {
                        "mark_read"
                    };
                    repo::actions::enqueue(
                        &tx,
                        action.account_id,
                        inverse,
                        Some(mid),
                        thread_id,
                        &serde_json::json!({}),
                        None,
                    )?;
                }
            }
        }
        "star" | "unstar" => {
            if let Some(mid) = action.message_id {
                repo::messages::set_starred(&tx, mid, action.kind == "unstar")?;
                if !was_pending {
                    let inverse = if action.kind == "star" {
                        "unstar"
                    } else {
                        "star"
                    };
                    repo::actions::enqueue(
                        &tx,
                        action.account_id,
                        inverse,
                        Some(mid),
                        thread_id,
                        &serde_json::json!({}),
                        None,
                    )?;
                }
            }
        }
        "archive" | "trash" | "spam" | "move" | "unarchive" | "not_spam" => {
            if let Some(mid) = action.message_id {
                let src_folder = action.payload["srcFolderId"].as_i64();
                let src_uid = action.payload["srcUid"].as_i64();
                let cur_folder = action.payload["targetFolderId"].as_i64();
                if let Some(src) = src_folder {
                    let gmail: bool = tx.query_row(
                        "SELECT a.provider = 'gmail'
                         FROM messages m JOIN accounts a ON a.id = m.account_id
                         WHERE m.id = ?1",
                        rusqlite::params![mid],
                        |row| row.get(0),
                    )?;
                    if was_pending {
                        // Remote never changed; restore the original mapping.
                        repo::messages::set_uid_and_folder(&tx, mid, src, src_uid)?;
                    } else {
                        // Remote moved; move it back.
                        repo::messages::set_uid_and_folder(&tx, mid, src, None)?;
                        let payload = serde_json::json!({
                            "srcFolderId": cur_folder,
                            "srcUid": serde_json::Value::Null,
                            "targetFolderId": src,
                        });
                        repo::actions::enqueue(
                            &tx,
                            action.account_id,
                            "move",
                            Some(mid),
                            thread_id,
                            &payload,
                            None,
                        )?;
                    }
                    if gmail {
                        if let Some(current) = cur_folder {
                            tx.execute(
                                "DELETE FROM message_folders
                                 WHERE message_id = ?1 AND folder_id = ?2",
                                rusqlite::params![mid, current],
                            )?;
                        }
                        tx.execute(
                            "INSERT OR IGNORE INTO message_folders (message_id, folder_id)
                             VALUES (?1, ?2)",
                            rusqlite::params![mid, src],
                        )?;
                    }
                }
            }
        }
        "add_label" | "remove_label" => {
            if let (Some(mid), Some(label_id)) =
                (action.message_id, action.payload["labelId"].as_i64())
            {
                let was_add = action.kind == "add_label";
                // Restore the local membership to its pre-action state.
                if was_add {
                    repo::labels::remove_from_message(&tx, mid, label_id)?;
                } else {
                    repo::labels::add_to_message(&tx, mid, label_id)?;
                }
                if !was_pending {
                    // Remote already got the keyword; enqueue the inverse push.
                    let inverse = if was_add { "remove_label" } else { "add_label" };
                    repo::actions::enqueue(
                        &tx,
                        action.account_id,
                        inverse,
                        Some(mid),
                        thread_id,
                        &action.payload,
                        None,
                    )?;
                }
            }
        }
        "snooze" => {
            if let Some(tid) = thread_id {
                repo::snoozes::clear(&tx, tid)?;
            }
        }
        "unsnooze" => { /* nothing sensible to restore */ }
        "send" => { /* cancel already handled if pending; sent mail can't be unsent */ }
        _ => {}
    }

    if let Some(tid) = thread_id {
        repo::threads::recompute(&tx, tid)?;
    }
    tx.commit()?;
    Ok(thread_id)
}

#[cfg(test)]
mod ai_model_routing_tests {
    use super::*;

    #[test]
    fn fresh_settings_have_explicit_models_for_every_tier() {
        let settings = Settings::default();
        assert!(!settings.ai_model_instant.trim().is_empty());
        assert!(!settings.ai_model_cheap.trim().is_empty());
        assert!(!settings.ai_model_intelligent.trim().is_empty());
        assert_eq!(
            resolve_ai_model(&settings, Scenario::Ask),
            settings.ai_model_intelligent
        );
        assert_eq!(
            resolve_ai_model(&settings, Scenario::Summarize),
            settings.ai_model_instant
        );
    }

    #[test]
    fn each_scenario_uses_its_tier_model() {
        let s = Settings {
            ai_model_instant: "fast".into(),
            ai_model_cheap: "mid".into(),
            ai_model_intelligent: "smart".into(),
            ..Settings::default()
        };
        // Defaults: ask/draft -> intelligent, summarize -> instant, voice -> cheap.
        assert_eq!(resolve_ai_model(&s, Scenario::Ask), "smart");
        assert_eq!(resolve_ai_model(&s, Scenario::Draft), "smart");
        assert_eq!(resolve_ai_model(&s, Scenario::Summarize), "fast");
        assert_eq!(resolve_ai_model(&s, Scenario::Voice), "mid");
    }

    #[test]
    fn scenario_can_be_repointed_to_another_tier() {
        let s = Settings {
            ai_model_instant: "fast".into(),
            ai_tier_ask: "instant".into(), // route Ask to the instant tier
            ..Settings::default()
        };
        assert_eq!(resolve_ai_model(&s, Scenario::Ask), "fast");
    }

    #[test]
    fn unknown_tier_uses_the_intelligent_model() {
        let s = Settings {
            ai_model_intelligent: "safe-default".into(),
            ai_tier_ask: "unknown".into(),
            ..Settings::default()
        };
        assert_eq!(resolve_ai_model(&s, Scenario::Ask), "safe-default");
    }
}

#[cfg(test)]
mod attachment_ext_tests {
    use super::*;

    #[test]
    fn calendar_mime_maps_to_ics() {
        assert_eq!(ext_for_mime("text/calendar"), Some("ics"));
        // MIME parameters (e.g. method=REQUEST) and casing are tolerated.
        assert_eq!(ext_for_mime("text/calendar; method=REQUEST"), Some("ics"));
        assert_eq!(ext_for_mime("TEXT/CALENDAR"), Some("ics"));
    }

    #[test]
    fn unknown_mime_has_no_extension() {
        assert_eq!(ext_for_mime("application/x-unknown"), None);
    }
}

#[cfg(test)]
mod startup_connection_tests {
    use super::*;

    #[tokio::test]
    async fn startup_releases_recovery_connections_before_foreground_reads() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::for_tests(temp.path());
        let db = Db::open(&paths.db_file()).unwrap();
        db.write(|conn| {
            conn.execute(
                "INSERT INTO route_cache(sender_domain, route_key) VALUES('__routing_backfill__', '1')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        db.release_idle_connections().await.unwrap();
        drop(db);
        let core = Core::start_mail_ui(paths).await.unwrap();

        assert!(
            core.db
                .cache_usage()
                .await
                .unwrap()
                .iter()
                .all(|connection| connection.role != "writer")
        );
        core.get_settings().await.unwrap();
        let usage = core.db.cache_usage().await.unwrap();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].role, "reader");
    }
}

#[cfg(test)]
mod draft_staging_tests {
    use super::*;

    #[tokio::test]
    async fn cleanup_scans_staged_attachments_and_keeps_live_draft_references() {
        let temp = tempfile::tempdir().unwrap();
        let core = Core::start_mail_ui(Paths::for_tests(temp.path()))
            .await
            .unwrap();
        let (sent_id, draft_id) = core
            .db
            .write(|conn| {
                db::testutil::seed_account(conn);
                let (_, sent_id) = db::testutil::seed_message(conn, "me@test.dev", "sent", false);
                let (_, draft_id) = db::testutil::seed_message(conn, "me@test.dev", "draft", false);
                conn.execute("UPDATE messages SET is_draft = 1 WHERE id = ?1", [draft_id])?;
                for (message_id, name) in [(sent_id, "sent"), (draft_id, "draft")] {
                    conn.execute(
                        "INSERT INTO draft_attachments (draft_id, file_path, filename)
                         VALUES (?1, ?2, ?2)",
                        rusqlite::params![message_id, name],
                    )?;
                }
                Ok((sent_id, draft_id))
            })
            .await
            .unwrap();

        core.cleanup_orphaned_draft_files().await.unwrap();
        let retained = core
            .db
            .read(|conn| {
                let mut statement =
                    conn.prepare("SELECT draft_id FROM draft_attachments ORDER BY draft_id")?;
                Ok(statement
                    .query_map([], |row| row.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await
            .unwrap();
        assert_ne!(sent_id, draft_id);
        assert_eq!(retained, vec![draft_id]);
    }

    fn draft_args(
        draft_id: Option<i64>,
        source: &std::path::Path,
        filename: &str,
    ) -> SaveDraftArgs {
        SaveDraftArgs {
            draft_id,
            account_id: 1,
            from: None,
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "bounded draft".into(),
            body_text: "body".into(),
            body_html: None,
            mode: "new".into(),
            in_reply_to_message_id: None,
            attachments: vec![DraftAttachmentIn {
                file_path: source.to_string_lossy().into_owned(),
                filename: filename.into(),
            }],
        }
    }

    #[tokio::test]
    async fn failed_and_replaced_drafts_do_not_leak_staged_files() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::for_tests(temp.path());
        let core = Core::start_mail_ui(paths.clone()).await.unwrap();
        core.db
            .write(|conn| {
                db::testutil::seed_account(conn);
                Ok(())
            })
            .await
            .unwrap();
        let first_source = temp.path().join("first.txt");
        let second_source = temp.path().join("second.txt");
        tokio::fs::write(&first_source, b"first").await.unwrap();
        tokio::fs::write(&second_source, b"second").await.unwrap();

        assert!(
            core.save_draft(draft_args(Some(999_999), &first_source, "first.txt"))
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read_dir(paths.draft_attachments_dir())
                .unwrap()
                .count(),
            0
        );

        let draft_id = core
            .save_draft(draft_args(None, &first_source, "first.txt"))
            .await
            .unwrap();
        let first_staged = core
            .db
            .read(move |conn| {
                Ok(conn.query_row(
                    "SELECT file_path FROM draft_attachments WHERE draft_id = ?1",
                    rusqlite::params![draft_id],
                    |row| row.get::<_, String>(0),
                )?)
            })
            .await
            .unwrap();
        assert!(std::path::Path::new(&first_staged).exists());

        core.save_draft(draft_args(Some(draft_id), &second_source, "second.txt"))
            .await
            .unwrap();
        let second_staged = core
            .db
            .read(move |conn| {
                Ok(conn.query_row(
                    "SELECT file_path FROM draft_attachments WHERE draft_id = ?1",
                    rusqlite::params![draft_id],
                    |row| row.get::<_, String>(0),
                )?)
            })
            .await
            .unwrap();
        assert_ne!(first_staged, second_staged);
        assert!(!std::path::Path::new(&first_staged).exists());
        assert_eq!(std::fs::read(second_staged).unwrap(), b"second");
    }
}

#[cfg(test)]
mod draft_action_race_tests {
    use super::*;

    async fn jmap_draft_with_send(state: &str) -> (Core, tempfile::TempDir, i64, i64) {
        let temp = tempfile::tempdir().unwrap();
        let core = Core::start_mail_ui(Paths::for_tests(temp.path()))
            .await
            .unwrap();
        let state = state.to_owned();
        let (draft_id, action_id) = core
            .db
            .write(move |conn| {
                db::testutil::seed_account(conn);
                conn.execute(
                    "UPDATE accounts SET mail_protocol='jmap',jmap_url='https://mail.test.dev' WHERE id=1",
                    [],
                )?;
                conn.execute("UPDATE folders SET role='drafts' WHERE id=1", [])?;
                let (thread_id, draft_id) =
                    db::testutil::seed_message(conn, "me@test.dev", "Draft", false);
                conn.execute(
                    "UPDATE messages SET is_draft=1,is_outgoing=1,uid=NULL WHERE id=?1",
                    rusqlite::params![draft_id],
                )?;
                let action_id = repo::actions::enqueue(
                    conn,
                    1,
                    "send",
                    Some(draft_id),
                    Some(thread_id),
                    &serde_json::json!({ "draftId": draft_id }),
                    Some(now_ms() + 60_000),
                )?;
                repo::actions::set_state(conn, action_id, &state, None)?;
                Ok((draft_id, action_id))
            })
            .await
            .unwrap();
        (core, temp, draft_id, action_id)
    }

    #[tokio::test]
    async fn deleting_a_jmap_draft_cancels_a_queued_send() {
        let (core, _temp, draft_id, action_id) = jmap_draft_with_send("pending").await;
        let staged_path = core
            .paths
            .draft_attachments_dir()
            .join("cancelled-send")
            .join("file.txt");
        std::fs::create_dir_all(staged_path.parent().unwrap()).unwrap();
        std::fs::write(&staged_path, b"attachment").unwrap();
        let stored_path = staged_path.to_string_lossy().into_owned();
        core.db
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO draft_attachments (draft_id, file_path, filename)
                     VALUES (?1, ?2, 'file.txt')",
                    rusqlite::params![draft_id, stored_path],
                )?;
                Ok(())
            })
            .await
            .unwrap();

        core.delete_draft(draft_id).await.unwrap();
        assert!(!staged_path.exists());

        core.db
            .read(move |conn| {
                assert!(repo::messages::get_row(conn, draft_id)?.is_none());
                assert_eq!(
                    repo::actions::get(conn, action_id)?.map(|action| action.state),
                    Some("cancelled".into())
                );
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn deleting_a_draft_during_submission_is_rejected_without_data_loss() {
        let (core, _temp, draft_id, action_id) = jmap_draft_with_send("inflight").await;

        let error = core.delete_draft(draft_id).await.unwrap_err();
        assert!(error.to_string().contains("currently being submitted"));

        core.db
            .read(move |conn| {
                assert!(repo::messages::get_row(conn, draft_id)?.is_some());
                assert_eq!(
                    repo::actions::get(conn, action_id)?.map(|action| action.state),
                    Some("inflight".into())
                );
                Ok(())
            })
            .await
            .unwrap();
    }
}

#[cfg(test)]
mod send_confirmation_tests {
    use super::*;

    #[test]
    fn send_start_timeout_begins_at_scheduled_delivery() {
        let observed_at = 1_000;
        let scheduled_at = 121_000;
        assert!(!send_start_expired(61_000, observed_at, Some(scheduled_at)));
        assert!(!send_start_expired(
            180_999,
            observed_at,
            Some(scheduled_at)
        ));
        assert!(send_start_expired(181_000, observed_at, Some(scheduled_at)));
        assert!(send_start_expired(61_000, observed_at, None));
    }

    async fn send_action() -> (Core, tempfile::TempDir, i64) {
        let temp = tempfile::tempdir().unwrap();
        let core = Core::start_mail_ui(Paths::for_tests(temp.path()))
            .await
            .unwrap();
        let action_id = core
            .db
            .write(|conn| {
                db::testutil::seed_account(conn);
                repo::actions::enqueue(
                    conn,
                    1,
                    "send",
                    None,
                    None,
                    &serde_json::json!({ "draftId": 1 }),
                    None,
                )
            })
            .await
            .unwrap();
        (core, temp, action_id)
    }

    #[tokio::test]
    async fn confirmed_send_returns_only_after_done_state() {
        let (core, _temp, action_id) = send_action().await;
        let waiting_core = core.clone();
        let waiter = tokio::spawn(async move { waiting_core.wait_for_send(action_id).await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());

        core.db
            .write(move |conn| repo::actions::set_state(conn, action_id, "done", None))
            .await
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn failed_attempt_is_cancelled_before_returning_the_error() {
        let (core, _temp, action_id) = send_action().await;
        core.db
            .write(move |conn| {
                repo::actions::bump_attempt(
                    conn,
                    action_id,
                    now_ms() + 60_000,
                    "SMTP relay rejected the message",
                )
            })
            .await
            .unwrap();

        let error = core.wait_for_send(action_id).await.unwrap_err();
        assert!(error.to_string().contains("SMTP relay rejected"));
        core.db
            .read(move |conn| {
                assert_eq!(
                    repo::actions::get(conn, action_id)?.map(|action| action.state),
                    Some("cancelled".into())
                );
                Ok(())
            })
            .await
            .unwrap();
    }
}

#[cfg(test)]
mod thread_read_tests {
    use super::*;

    #[tokio::test]
    async fn fresh_profile_keeps_mail_contacts_and_calendar_independent() {
        let temp = tempfile::tempdir().unwrap();
        let core = Core::start_mail_ui(Paths::for_tests(temp.path()))
            .await
            .unwrap();
        assert!(!core.paths.calendar_db_file().exists());
        assert!(!core.paths.files_db_file().exists());
        assert!(!core.calendar_db.is_open());
        assert!(!core.files_db.is_open());
        let (thread_id, message_id) = core
            .db
            .write(|conn| {
                db::testutil::seed_account(conn);
                let (thread_id, message_id) =
                    db::testutil::seed_message(conn, "sender@example.com", "Rich message", false);
                repo::messages::store_body(
                    conn,
                    message_id,
                    Some("Plain alternative"),
                    Some("<html><body><strong>Rich HTML body</strong></body></html>"),
                    None,
                    false,
                    Some("Rich HTML body"),
                )?;
                repo::threads::recompute(conn, thread_id)?;
                conn.execute(
                    "INSERT INTO contacts (email, name, is_managed)
                     VALUES ('person@example.com', 'Person', 1)",
                    [],
                )?;
                Ok((thread_id, message_id))
            })
            .await
            .unwrap();

        let detail = core.get_thread(thread_id).await.unwrap();

        assert_eq!(detail.messages.len(), 1);
        assert_eq!(detail.messages[0].id, message_id);
        assert_eq!(
            detail.messages[0].html_body.as_deref(),
            Some("<html><body><strong>Rich HTML body</strong></body></html>")
        );

        core.calendar_db
            .write(|conn| {
                conn.execute(
                    "INSERT INTO calendars (id, account_id, url, display_name)
                     VALUES (1, 1, 'local://primary', 'Primary')",
                    [],
                )?;
                conn.execute(
                    "INSERT INTO calendar_events (
                       account_id, ical_uid, summary, starts_at, calendar_id, is_local
                     ) VALUES (1, 'fresh-event', 'Calendar event', 1000, 1, 1)",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();

        let mail_contract = core
            .db
            .read(|conn| {
                let version: i64 =
                    conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
                let contacts: i64 =
                    conn.query_row("SELECT COUNT(*) FROM contacts", [], |row| row.get(0))?;
                let has_calendar: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'calendar_events')",
                    [],
                    |row| row.get(0),
                )?;
                let integrity: String =
                    conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
                Ok((version, contacts, has_calendar, integrity))
            })
            .await
            .unwrap();
        assert_eq!(
            mail_contract,
            (
                crate::db::migrations::LATEST_VERSION,
                1,
                false,
                "ok".to_owned()
            )
        );

        let calendar_contract = core
            .calendar_db
            .read(|conn| {
                let version: i64 =
                    conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
                let events: i64 =
                    conn.query_row("SELECT COUNT(*) FROM calendar_events", [], |row| row.get(0))?;
                let has_messages: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'messages')",
                    [],
                    |row| row.get(0),
                )?;
                let integrity: String =
                    conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
                Ok((version, events, has_messages, integrity))
            })
            .await
            .unwrap();
        assert_eq!(calendar_contract, (1, 1, false, "ok".to_owned()));
    }
}

#[cfg(test)]
mod account_backup_import_tests {
    use super::*;

    fn portable_imap(email: &str) -> PortableAccountConfig {
        PortableAccountConfig {
            email: email.into(),
            display_name: Some("Portable account".into()),
            provider: Provider::Imap,
            auth_kind: AuthKind::Password,
            mail_protocol: MailProtocol::Imap,
            username: email.into(),
            jmap_url: String::new(),
            imap_host: "imap.example.com".into(),
            imap_port: 993,
            smtp_host: "smtp.example.com".into(),
            smtp_port: 465,
            settings: AccountSettings::default(),
        }
    }

    #[tokio::test]
    async fn imported_configuration_requires_reauthentication_and_merges_by_email() {
        let temp = tempfile::tempdir().unwrap();
        let core = Core::start_mail_ui(Paths::for_tests(temp.path()))
            .await
            .unwrap();

        assert_eq!(
            core.import_account_configs(vec![portable_imap("person@example.com")])
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            core.import_account_configs(vec![portable_imap("PERSON@example.com")])
                .await
                .unwrap(),
            0
        );

        let accounts = core.list_accounts().await.unwrap();
        core.set_account_mail_history(accounts[0].id, MailHistory::OneYear)
            .await
            .unwrap();
        assert_eq!(
            core.list_account_configs().await.unwrap()[0]
                .settings
                .mail_history,
            MailHistory::OneYear
        );
        let configs = core.list_account_configs().await.unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].sync_state, "needs_reauth");
        assert_eq!(configs[0].imap_host, "imap.example.com");
    }
}

#[cfg(test)]
mod local_data_reset_tests {
    use super::*;

    async fn start_test_core(paths: &Paths) -> Core {
        Core::start_mail_ui_with_credentials(
            paths.clone(),
            Arc::new(credentials::DevelopmentFileCredentialStore::new(
                paths.data_dir.join("test-credentials.json"),
            )),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn delete_all_local_data_keeps_databases_open_and_empties_profile() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::for_tests(temp.path());
        let core = start_test_core(&paths).await;
        core.db
            .write(|conn| {
                db::testutil::seed_account(conn);
                db::testutil::seed_message(conn, "sender@example.com", "Reset me", false);
                conn.execute(
                    "INSERT INTO contacts (email, name) VALUES ('friend@example.com', 'Friend')",
                    [],
                )?;
                conn.execute(
                    "INSERT INTO snippets (name, body_text) VALUES ('Hello', 'Hi')",
                    [],
                )?;
                repo::settings::set(
                    conn,
                    &Settings {
                        theme: "carbon".into(),
                        ..Settings::default()
                    },
                )?;
                Ok(())
            })
            .await
            .unwrap();
        core.calendar_db
            .write(|conn| {
                conn.execute(
                    "INSERT INTO calendar_events
                     (account_id, ical_uid, starts_at, is_local)
                     VALUES (1, 'event-1', 1000, 1)",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        std::fs::create_dir_all(paths.models_dir()).unwrap();
        std::fs::write(paths.models_dir().join("model.bin"), b"model").unwrap();

        core.delete_all_local_data().await.unwrap();

        let counts = core
            .db
            .read(|conn| {
                Ok((
                    conn.query_row("SELECT COUNT(*) FROM accounts", [], |row| {
                        row.get::<_, i64>(0)
                    })?,
                    conn.query_row("SELECT COUNT(*) FROM contacts", [], |row| {
                        row.get::<_, i64>(0)
                    })?,
                    conn.query_row("SELECT COUNT(*) FROM snippets", [], |row| {
                        row.get::<_, i64>(0)
                    })?,
                    conn.query_row("SELECT COUNT(*) FROM app_settings", [], |row| {
                        row.get::<_, i64>(0)
                    })?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(counts, (0, 0, 0, 0));
        let calendar_count = core
            .calendar_db
            .read(|conn| {
                Ok(
                    conn.query_row("SELECT COUNT(*) FROM calendar_events", [], |row| {
                        row.get::<_, i64>(0)
                    })?,
                )
            })
            .await
            .unwrap();
        assert_eq!(calendar_count, 0);
        assert!(!paths.models_dir().exists());
        assert!(core.list_accounts().await.unwrap().is_empty());
        assert_eq!(core.get_settings().await.unwrap().theme, "system");
    }

    #[tokio::test]
    async fn startup_recovery_finishes_interrupted_removal_and_cleans_orphans() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::for_tests(temp.path());
        let core = start_test_core(&paths).await;
        core.db
            .write(|conn| {
                db::testutil::seed_account(conn);
                conn.execute(
                    "INSERT INTO cross_store_operations (kind, account_id, created_at)
                     VALUES ('remove_account', 1, 1000)",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        core.calendar_db
            .write(|conn| {
                conn.execute(
                    "INSERT INTO calendar_events
                     (account_id, ical_uid, starts_at, is_local)
                     VALUES (1, 'pending-delete', 1000, 1),
                            (99, 'legacy-orphan', 1000, 1)",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        std::fs::create_dir_all(paths.mail_dir(1)).unwrap();
        std::fs::create_dir_all(paths.attachments_dir(1)).unwrap();

        core.recover_cross_store_state().await.unwrap();

        let (accounts, operations) = core
            .db
            .read(|conn| {
                Ok((
                    conn.query_row("SELECT COUNT(*) FROM accounts", [], |row| {
                        row.get::<_, i64>(0)
                    })?,
                    conn.query_row("SELECT COUNT(*) FROM cross_store_operations", [], |row| {
                        row.get::<_, i64>(0)
                    })?,
                ))
            })
            .await
            .unwrap();
        let calendar_events = core
            .calendar_db
            .read(|conn| {
                Ok(
                    conn.query_row("SELECT COUNT(*) FROM calendar_events", [], |row| {
                        row.get::<_, i64>(0)
                    })?,
                )
            })
            .await
            .unwrap();
        assert_eq!((accounts, operations, calendar_events), (0, 0, 0));
        assert!(!paths.mail_dir(1).exists());
        assert!(!paths.attachments_dir(1).exists());
    }

    #[tokio::test]
    async fn startup_recovery_detaches_only_missing_mail_message_links() {
        let temp = tempfile::tempdir().unwrap();
        let core = Core::start_mail_ui(Paths::for_tests(temp.path()))
            .await
            .unwrap();
        let existing_message_id =
            core.db
                .write(|conn| {
                    db::testutil::seed_account(conn);
                    Ok(db::testutil::seed_message(
                        conn,
                        "sender@example.com",
                        "Calendar invite",
                        false,
                    )
                    .1)
                })
                .await
                .unwrap();
        let missing_message_id = existing_message_id + 10_000;
        core.calendar_db
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO calendar_events
                     (account_id, message_id, ical_uid, starts_at, is_local)
                     VALUES (1, ?1, 'linked-existing', 1000, 1),
                            (1, ?2, 'linked-missing', 2000, 1)",
                    rusqlite::params![existing_message_id, missing_message_id],
                )?;
                Ok(())
            })
            .await
            .unwrap();

        assert_eq!(
            core.detach_orphaned_calendar_message_links().await.unwrap(),
            1
        );

        let links = core
            .calendar_db
            .read(|conn| {
                let mut statement = conn.prepare(
                    "SELECT ical_uid, message_id FROM calendar_events ORDER BY ical_uid",
                )?;
                Ok(statement
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await
            .unwrap();
        assert_eq!(
            links,
            vec![
                ("linked-existing".to_owned(), Some(existing_message_id)),
                ("linked-missing".to_owned(), None),
            ]
        );
    }
}

// Siftmail: `impl Core` is split across `core_api/*.rs` (pure move).
mod core_api;
