# Siftmail fork notes

Siftmail is a fork of [flectar/mail](https://github.com/flectar/mail)
(AGPL-3.0-only). This file records where the fork diverges so upstream merges
stay manageable.

## Scope
- **Target platform: Windows only.** Non-Windows CI jobs are gated off with
  `if: false` in `.github/workflows/native-platforms.yml` rather than deleted;
  `android-production.yml` was removed. Platform code under `platform/` and
  `#[cfg(target_os = ...)]` branches is left in place for now (deleting it is
  risky for upstream merges) and will be trimmed once unreferenced.
- The Linux CI job remains for repository checks and the platform-neutral core
  library. Linux builds need `pkg-config libdbus-1-dev`.

## AI may only suggest
- `Core::classify_pending` no longer executes the actions configured on AI
  automation rules (`route_to`, `add_label`, `remove_label`, `mark_read`,
  `star`, `archive`, `trash`, `subject_prefix`, `body_note`). A model match is
  stored as a pending row in `ai_suggestions` (migration 013) via
  `crate::suggest`, whose `Suggestion` type carries no action.
- The AI category tab (Marketing/News/Social/Pitch) is still applied; it only
  changes a local view tab and does not mutate the mailbox.
- `scripts/check-suggest-boundary.sh` (run in CI) fails if `ai/`, `embed/` or
  `suggest/` reference mailbox action types.

## Sync scheduler (M1.5)
`crates/flectar-mail-core/src/sync/scheduler.rs`, wired into the IMAP actor
(`sync/engine.rs::run_actor`):
- Per-account `AccountSettings.sync_schedule` (`baseMinutes` 60,
  `jitterMinutes` 20, `idlePush` false). In polling mode (default) the actor
  logs out after each cycle and waits `base ± jitter`.
- Startup stagger 0–10 min when ≥ 4 accounts start together; newly added
  accounts sync immediately.
- Provider = registrable domain of the IMAP host. Throttle-like failures
  (too many connections, rate limit, refused, `[UNAVAILABLE]`, …) back off the
  whole provider 1h→2h→4h→…24h (±20 %), persisted in `provider_backoff`
  (migration 014); a successful cycle resets. Auth failures stay per-account.
- At most 2 polling connections per provider at a time.
- Not yet covered: Gmail API and JMAP actors, the backfill pool / dedicated
  body-reader connections, and the settings/status UI.

## Categories and rules (M2)
- `crate::category`: `categories`, `category_rules`, `rule_conditions`,
  `thread_categories` (migration 015, seeds 账单/订阅/通知/VPS警告/应用运行状态
  plus two disabled example rules).
- Conditions: from_address / from_domain / from_name / to / subject / body
  (body = stored snippet) × equals / contains / wildcard (`*`,`?`) / regex
  (size-limited) / exists, with case-sensitivity and negate; all/any mode;
  first match by priority wins. Domains compare case- and IDN-insensitively.
- Rules run first in `classify_pending`, independent of AI. Actions are only
  `RuleAction::{ApplyCategory, Star, MarkRead}` (star/read only when ticked).
- AI fallback: `Core::suggest_categories` batches ≤15 unmatched threads with
  nonce-fenced content; output validated against enabled category ids and
  stored as pending `ai_suggestions` rows (`rule_id = 'category'`). Accepting
  is an explicit user command (`accept_category_suggestion`).
