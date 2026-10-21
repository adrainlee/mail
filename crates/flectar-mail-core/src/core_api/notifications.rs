//! `Core` methods: Notification delivery.
//! Moved verbatim from `lib.rs`; see docs/fork.md.

use crate::*;

impl Core {
    /// Return notifications that are ready for the native host to dispatch.
    /// Eligibility is decided by sync before enqueueing; this API only exposes
    /// durable delivery state to the host process.
    pub async fn due_notifications(&self, limit: i64) -> Result<Vec<NotificationOutboxItem>> {
        let now = now_ms();
        let limit = limit.clamp(1, 100);
        self.db
            .read(move |conn| repo::notifications::list_due(conn, now, limit))
            .await
    }

    /// Resolve which inbox tab a thread lands in, for notification-scope
    /// filtering. `RoutedTab::Pending` means AI classification is still in
    /// flight and the host should wait before deciding.
    pub async fn notification_thread_tab(&self, thread_id: i64) -> Result<Option<RoutedTab>> {
        self.db
            .read(move |conn| repo::notifications::resolve_tab(conn, thread_id))
            .await
    }

    /// Defer a pending notification's next dispatch without consuming a delivery
    /// attempt, used while waiting for its thread's tab to resolve.
    pub async fn defer_notification_delivery(&self, id: i64, not_before: i64) -> Result<bool> {
        self.db
            .write(move |conn| repo::notifications::defer(conn, id, not_before))
            .await
    }

    /// Atomically claim one due notification. A false result means another
    /// dispatcher or a state transition won the race.
    pub async fn claim_notification_delivery(&self, id: i64) -> Result<bool> {
        let now = now_ms();
        self.db
            .write(move |conn| repo::notifications::try_claim(conn, id, now))
            .await
    }

    pub async fn mark_notification_delivered(&self, id: i64) -> Result<bool> {
        let now = now_ms();
        self.db
            .write(move |conn| repo::notifications::mark_delivered(conn, id, now))
            .await
    }

    pub async fn suppress_notification_delivery(
        &self,
        id: i64,
        reason: impl Into<String>,
    ) -> Result<bool> {
        let now = now_ms();
        let reason = reason.into();
        self.db
            .write(move |conn| repo::notifications::mark_suppressed(conn, id, now, &reason))
            .await
    }

    /// Return a claimed notification to the pending queue after a bounded
    /// delay. The cap prevents a bad caller from making a row disappear for an
    /// unreasonable amount of time.
    pub async fn retry_notification_delivery(
        &self,
        id: i64,
        delay_ms: i64,
        error: impl Into<String>,
    ) -> Result<bool> {
        const MAX_DELAY_MS: i64 = 24 * 60 * 60 * 1_000;
        let retry_at = now_ms().saturating_add(delay_ms.clamp(0, MAX_DELAY_MS));
        let error = error.into();
        self.db
            .write(move |conn| repo::notifications::retry(conn, id, retry_at, &error))
            .await
    }

    /// Recover rows left in `delivering` by a process crash. Native delivery is
    /// necessarily at-least-once because the OS send and SQLite commit cannot
    /// share a transaction.
    pub async fn recover_notification_deliveries(&self) -> Result<usize> {
        let now = now_ms();
        self.db
            .write(move |conn| repo::notifications::recover_delivering(conn, now))
            .await
    }
}
