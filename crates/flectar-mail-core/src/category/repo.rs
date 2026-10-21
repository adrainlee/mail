//! SQLite persistence for categories and rules. All SQL is parameterized.

use super::Category;
use super::rules::{
    Condition, MatchField, MatchMode, MatchOp, MessageFacts, Rule, validate_condition,
};
use crate::error::{CoreError, Result};
use rusqlite::{Connection, OptionalExtension, params};

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn list_categories(conn: &Connection) -> Result<Vec<Category>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, description, color, show_as_tab, priority, enabled, label_id
         FROM categories ORDER BY priority, id",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Category {
                id: r.get(0)?,
                name: r.get(1)?,
                description: r.get(2)?,
                color: r.get(3)?,
                show_as_tab: r.get(4)?,
                priority: r.get(5)?,
                enabled: r.get(6)?,
                label_id: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn valid_category_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

pub fn save_category(conn: &Connection, c: &Category) -> Result<()> {
    if !valid_category_id(&c.id) {
        return Err(CoreError::Other(format!("invalid category id: {}", c.id)));
    }
    if c.name.trim().is_empty() {
        return Err(CoreError::Other("category name is empty".into()));
    }
    let now = now_secs();
    conn.execute(
        "INSERT INTO categories (id, name, description, color, show_as_tab, priority, enabled,
                                 label_id, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)
         ON CONFLICT(id) DO UPDATE SET name = excluded.name,
           description = excluded.description, color = excluded.color,
           show_as_tab = excluded.show_as_tab, priority = excluded.priority,
           enabled = excluded.enabled, label_id = excluded.label_id,
           updated_at = excluded.updated_at",
        params![
            c.id,
            c.name.trim(),
            c.description,
            c.color,
            c.show_as_tab,
            c.priority,
            c.enabled,
            c.label_id,
            now
        ],
    )?;
    Ok(())
}

pub fn delete_category(conn: &Connection, id: &str) -> Result<bool> {
    Ok(conn.execute("DELETE FROM categories WHERE id = ?1", params![id])? > 0)
}

pub fn list_rules(conn: &Connection) -> Result<Vec<Rule>> {
    let mut stmt = conn.prepare(
        "SELECT id, category_id, name, priority, enabled, match_mode, also_star, also_mark_read
         FROM category_rules ORDER BY priority, id",
    )?;
    let mut rules = stmt
        .query_map([], |r| {
            let mode: String = r.get(5)?;
            Ok(Rule {
                id: r.get(0)?,
                category_id: r.get(1)?,
                name: r.get(2)?,
                priority: r.get(3)?,
                enabled: r.get(4)?,
                mode: if mode == "any" {
                    MatchMode::Any
                } else {
                    MatchMode::All
                },
                conditions: Vec::new(),
                also_star: r.get(6)?,
                also_mark_read: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut cstmt = conn.prepare(
        "SELECT field, op, value, case_sensitive, negate FROM rule_conditions
         WHERE rule_id = ?1 ORDER BY id",
    )?;
    for rule in &mut rules {
        rule.conditions = cstmt
            .query_map(params![rule.id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, bool>(3)?,
                    r.get::<_, bool>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter_map(|(f, o, value, case_sensitive, negate)| {
                Some(Condition {
                    field: MatchField::parse(&f)?,
                    op: MatchOp::parse(&o)?,
                    value,
                    case_sensitive,
                    negate,
                })
            })
            .collect();
    }
    Ok(rules)
}

/// Insert (id = 0) or replace a rule and its conditions. Returns the rule id.
pub fn save_rule(conn: &mut Connection, rule: &Rule) -> Result<i64> {
    if rule.name.trim().is_empty() {
        return Err(CoreError::Other("rule name is empty".into()));
    }
    if rule.conditions.is_empty() {
        return Err(CoreError::Other(
            "a rule needs at least one condition".into(),
        ));
    }
    for c in &rule.conditions {
        validate_condition(c).map_err(CoreError::Other)?;
    }
    let tx = conn.transaction()?;
    let mode = match rule.mode {
        MatchMode::All => "all",
        MatchMode::Any => "any",
    };
    let id = if rule.id > 0 {
        let n = tx.execute(
            "UPDATE category_rules SET category_id = ?2, name = ?3, priority = ?4, enabled = ?5,
               match_mode = ?6, also_star = ?7, also_mark_read = ?8 WHERE id = ?1",
            params![
                rule.id,
                rule.category_id,
                rule.name.trim(),
                rule.priority,
                rule.enabled,
                mode,
                rule.also_star,
                rule.also_mark_read
            ],
        )?;
        if n == 0 {
            return Err(CoreError::NotFound(format!("rule {}", rule.id)));
        }
        tx.execute(
            "DELETE FROM rule_conditions WHERE rule_id = ?1",
            params![rule.id],
        )?;
        rule.id
    } else {
        tx.execute(
            "INSERT INTO category_rules (category_id, name, priority, enabled, match_mode,
               also_star, also_mark_read, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                rule.category_id,
                rule.name.trim(),
                rule.priority,
                rule.enabled,
                mode,
                rule.also_star,
                rule.also_mark_read,
                now_secs()
            ],
        )?;
        tx.last_insert_rowid()
    };
    for c in &rule.conditions {
        tx.execute(
            "INSERT INTO rule_conditions (rule_id, field, op, value, case_sensitive, negate)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id,
                c.field.as_str(),
                c.op.as_str(),
                c.value,
                c.case_sensitive,
                c.negate
            ],
        )?;
    }
    tx.commit()?;
    Ok(id)
}

pub fn delete_rule(conn: &Connection, id: i64) -> Result<bool> {
    Ok(conn.execute("DELETE FROM category_rules WHERE id = ?1", params![id])? > 0)
}

/// Only rules whose category exists and is enabled.
pub fn active_rules(conn: &Connection) -> Result<Vec<Rule>> {
    let enabled: std::collections::HashSet<String> = list_categories(conn)?
        .into_iter()
        .filter(|c| c.enabled)
        .map(|c| c.id)
        .collect();
    Ok(list_rules(conn)?
        .into_iter()
        .filter(|r| r.enabled && enabled.contains(&r.category_id))
        .collect())
}

fn addresses_in_json(json: &str) -> Vec<String> {
    fn walk(v: &serde_json::Value, out: &mut Vec<String>) {
        match v {
            serde_json::Value::String(s) if s.contains('@') => out.push(s.clone()),
            serde_json::Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            serde_json::Value::Object(o) => o.values().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(json) {
        walk(&v, &mut out);
    }
    out
}

/// Facts from the newest inbound message of a thread. Body is the stored
/// snippet (bodies may not be downloaded yet).
pub fn thread_facts(conn: &Connection, thread_id: i64) -> Result<Option<MessageFacts>> {
    Ok(conn
        .query_row(
            "SELECT COALESCE(from_addr,''), COALESCE(from_name,''), COALESCE(to_json,'[]'),
                    COALESCE(subject,''), COALESCE(snippet,'')
             FROM messages WHERE thread_id = ?1 AND is_outgoing = 0 AND is_draft = 0
             ORDER BY date DESC, id DESC LIMIT 1",
            params![thread_id],
            |r| {
                Ok(MessageFacts {
                    from_address: r.get(0)?,
                    from_name: r.get(1)?,
                    to: addresses_in_json(&r.get::<_, String>(2)?),
                    subject: r.get(3)?,
                    body: r.get(4)?,
                })
            },
        )
        .optional()?)
}

pub fn set_thread_category(
    conn: &Connection,
    thread_id: i64,
    category_id: &str,
    source: &str,
    rule_id: Option<i64>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO thread_categories (thread_id, category_id, source, rule_id, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(thread_id) DO UPDATE SET category_id = excluded.category_id,
           source = excluded.source, rule_id = excluded.rule_id, updated_at = excluded.updated_at",
        params![thread_id, category_id, source, rule_id, now_secs()],
    )?;
    Ok(())
}

pub fn thread_category(conn: &Connection, thread_id: i64) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT category_id FROM thread_categories WHERE thread_id = ?1",
            params![thread_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

pub fn category_label(conn: &Connection, category_id: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT label_id FROM categories WHERE id = ?1",
            params![category_id],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::category::rules::evaluate;

    fn db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        crate::db::migrations::run(&mut conn).unwrap();
        conn.execute_batch(
            "INSERT INTO accounts (id, email, provider, auth_kind, username, imap_host, imap_port,
               smtp_host, smtp_port, created_at)
             VALUES (1, 'me@test.dev', 'imap', 'password', 'me', 'h', 993, 'h', 587, 0);
             INSERT INTO folders (id, account_id, imap_name, role) VALUES (1, 1, 'INBOX', 'inbox');
             INSERT INTO threads (id, account_id, subject_norm) VALUES (1, 1, 'x');
             INSERT INTO messages (id, account_id, thread_id, folder_id, uid, subject, from_addr,
               from_name, to_json, cc_json, date, snippet)
             VALUES (1, 1, 1, 1, 7, 'Invoice #42', 'billing@vendor.test', 'Vendor',
               '[{\"name\":\"Me\",\"email\":\"me@test.dev\"}]', '[]', 1, 'Amount due');",
        )
        .unwrap();
        conn
    }

    #[test]
    fn seeds_categories_and_disabled_example_rules() {
        let conn = db();
        let ids: Vec<String> = list_categories(&conn)
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        for id in [
            "bills",
            "subscriptions",
            "notifications",
            "vps_alerts",
            "app_status",
        ] {
            assert!(ids.contains(&id.to_string()), "{id}");
        }
        let rules = list_rules(&conn).unwrap();
        assert_eq!(rules.len(), 2);
        assert!(rules.iter().all(|r| !r.enabled));
        assert!(active_rules(&conn).unwrap().is_empty());
    }

    #[test]
    fn rule_round_trip_and_thread_evaluation() {
        let mut conn = db();
        let rule = Rule {
            id: 0,
            category_id: "bills".into(),
            name: "Vendor invoices".into(),
            priority: 1,
            enabled: true,
            mode: MatchMode::All,
            conditions: vec![
                Condition {
                    field: MatchField::FromDomain,
                    op: MatchOp::Equals,
                    value: "VENDOR.test".into(),
                    case_sensitive: false,
                    negate: false,
                },
                Condition {
                    field: MatchField::To,
                    op: MatchOp::Wildcard,
                    value: "*@test.dev".into(),
                    case_sensitive: false,
                    negate: false,
                },
            ],
            also_star: false,
            also_mark_read: false,
        };
        let id = save_rule(&mut conn, &rule).unwrap();
        let stored = list_rules(&conn)
            .unwrap()
            .into_iter()
            .find(|r| r.id == id)
            .unwrap();
        assert_eq!(stored.conditions, rule.conditions);
        let facts = thread_facts(&conn, 1).unwrap().unwrap();
        assert_eq!(facts.to, vec!["me@test.dev".to_string()]);
        let m = evaluate(&active_rules(&conn).unwrap(), &facts).unwrap();
        assert_eq!(m.rule_id, id);
        set_thread_category(&conn, 1, &m.category_id, "rule", Some(id)).unwrap();
        assert_eq!(thread_category(&conn, 1).unwrap().as_deref(), Some("bills"));
        // Disabling the category disables its rules.
        let mut bills = list_categories(&conn)
            .unwrap()
            .into_iter()
            .find(|c| c.id == "bills")
            .unwrap();
        bills.enabled = false;
        save_category(&conn, &bills).unwrap();
        assert!(active_rules(&conn).unwrap().is_empty());
        // Deleting the category cascades rules and clears the thread link.
        assert!(delete_category(&conn, "bills").unwrap());
        assert!(list_rules(&conn).unwrap().iter().all(|r| r.id != id));
        assert_eq!(thread_category(&conn, 1).unwrap(), None);
    }

    #[test]
    fn invalid_input_is_rejected() {
        let mut conn = db();
        let bad = Rule {
            id: 0,
            category_id: "bills".into(),
            name: "bad".into(),
            priority: 1,
            enabled: true,
            mode: MatchMode::Any,
            conditions: vec![Condition {
                field: MatchField::Subject,
                op: MatchOp::Regex,
                value: "(".into(),
                case_sensitive: false,
                negate: false,
            }],
            also_star: false,
            also_mark_read: false,
        };
        assert!(save_rule(&mut conn, &bad).is_err());
        let empty = Rule {
            conditions: vec![],
            ..bad
        };
        assert!(save_rule(&mut conn, &empty).is_err());
        let cat = Category {
            id: "Bad Id".into(),
            name: "x".into(),
            description: String::new(),
            color: None,
            show_as_tab: true,
            priority: 1,
            enabled: true,
            label_id: None,
        };
        assert!(save_category(&conn, &cat).is_err());
    }
}
