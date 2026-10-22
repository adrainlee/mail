//! Advisory AI output (Siftmail fork).
//!
//! Everything an AI model decides about a thread is represented here as a
//! [`Suggestion`]. By design this type carries **no executable action**: it
//! names a thread and the user-configured rule the model believes matches,
//! nothing else. There is intentionally no conversion from `Suggestion` into
//! a mailbox action type; executing anything requires an explicit
//! user command through the normal action path.
//!
//! The `ai`, `embed` and `suggest` modules must not reference mailbox action
//! types; `scripts/check-suggest-boundary.sh` asserts this in CI.

use crate::error::Result;
use rusqlite::{Connection, params};

/// A non-executable AI suggestion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub thread_id: i64,
    /// Id of the user-configured automation rule the model selected.
    pub rule_id: String,
    /// Display-only rule name at the time of the suggestion.
    pub rule_name: String,
    /// Model that produced the suggestion.
    pub model: String,
}

/// A stored suggestion row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSuggestion {
    pub id: i64,
    pub suggestion: Suggestion,
    pub state: String,
    pub created_at: i64,
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Record suggestions as `pending`. An existing suggestion for the same
/// (thread, rule) keeps its state, so a rejected suggestion is not revived.
pub fn record(conn: &Connection, suggestions: &[Suggestion]) -> Result<usize> {
    let now = now_secs();
    let mut stmt = conn.prepare_cached(
        "INSERT INTO ai_suggestions (thread_id, rule_id, rule_name, model, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(thread_id, rule_id) DO NOTHING",
    )?;
    let mut n = 0;
    for s in suggestions {
        n += stmt.execute(params![s.thread_id, s.rule_id, s.rule_name, s.model, now])?;
    }
    Ok(n)
}

/// Pending suggestions, newest first.
pub fn list_pending(conn: &Connection, limit: i64) -> Result<Vec<StoredSuggestion>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, thread_id, rule_id, rule_name, model, state, created_at
         FROM ai_suggestions WHERE state = 'pending'
         ORDER BY created_at DESC, id DESC LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(params![limit], |r| {
            Ok(StoredSuggestion {
                id: r.get(0)?,
                suggestion: Suggestion {
                    thread_id: r.get(1)?,
                    rule_id: r.get(2)?,
                    rule_name: r.get(3)?,
                    model: r.get(4)?,
                },
                state: r.get(5)?,
                created_at: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Mark a suggestion rejected (user decision). Accepting is left to the
/// caller's explicit user command path.
pub fn set_state(conn: &Connection, id: i64, state: &str) -> Result<bool> {
    if !matches!(state, "accepted" | "rejected" | "expired") {
        return Ok(false);
    }
    let n = conn.execute(
        "UPDATE ai_suggestions SET state = ?2, decided_at = ?3 WHERE id = ?1",
        params![id, state, now_secs()],
    )?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::db::migrations::run(&mut conn).unwrap();
        conn.execute_batch(
            "INSERT INTO accounts (id, email, provider, auth_kind, username, imap_host, imap_port,
               smtp_host, smtp_port, created_at)
             VALUES (1, 'me@test.dev', 'imap', 'password', 'me', 'h', 993, 'h', 587, 0);
             INSERT INTO threads (id, account_id, subject_norm) VALUES (1, 1, 'x');",
        )
        .unwrap();
        conn
    }

    fn s(rule: &str) -> Suggestion {
        Suggestion {
            thread_id: 1,
            rule_id: rule.into(),
            rule_name: "Invoices".into(),
            model: "m".into(),
        }
    }

    #[test]
    fn record_is_idempotent_and_keeps_user_decision() {
        let conn = db();
        assert_eq!(record(&conn, &[s("a"), s("b")]).unwrap(), 2);
        let pending = list_pending(&conn, 10).unwrap();
        assert_eq!(pending.len(), 2);
        assert!(set_state(&conn, pending[0].id, "rejected").unwrap());
        assert_eq!(record(&conn, &[s("a"), s("b")]).unwrap(), 0);
        assert_eq!(list_pending(&conn, 10).unwrap().len(), 1);
        assert!(!set_state(&conn, pending[1].id, "executed").unwrap());
    }
}
