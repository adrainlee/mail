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
