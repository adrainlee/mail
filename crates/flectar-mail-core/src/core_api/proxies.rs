//! `Core` methods: shared proxies and per-account proxy choice (Siftmail).
//!
//! Proxy passwords are kept in the platform credential store (Windows
//! Credential Manager through `keyring`), never in SQLite; an in-memory cache
//! in [`crate::net::proxy`] mirrors them for the connection paths.

use crate::accounts::credentials::{self, Slot};
use crate::net::proxy::{self, ProxyConfig, ProxyMode};
use crate::*;

/// Legacy plaintext proxy passwords found in SQLite by [`find_legacy_secrets`].
pub(crate) struct LegacySecrets {
    /// (proxy id, password) from a `proxies.password` column.
    pub column: Vec<(i64, String)>,
    /// (account id, proxy id, password, cleaned settings json).
    pub settings: Vec<(i64, i64, String, String)>,
}

/// Find plaintext proxy passwords left by development builds: a `password`
/// column on `proxies`, or a `password`/`proxyPassword` inside an account's
/// `settings_json.connection`.
pub(crate) fn find_legacy_secrets(conn: &rusqlite::Connection) -> Result<LegacySecrets> {
    let has_column = conn
        .prepare("SELECT 1 FROM pragma_table_info('proxies') WHERE name = 'password'")?
        .exists([])?;
    let mut column = Vec::new();
    if has_column {
        let mut stmt = conn.prepare(
            "SELECT id, password FROM proxies WHERE password IS NOT NULL AND password <> ''",
        )?;
        column = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
    }
    let mut settings = Vec::new();
    let mut stmt = conn.prepare("SELECT id, settings_json FROM accounts")?;
    let rows: Vec<(i64, String)> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (account_id, json) in rows {
        if let Some((proxy_id, pw, cleaned)) = strip_legacy_settings(&json) {
            settings.push((account_id, proxy_id.unwrap_or(0), pw, cleaned));
        }
    }
    Ok(LegacySecrets { column, settings })
}

/// Remove a plaintext proxy password from an account's settings JSON,
/// returning (proxy id, password, cleaned json).
pub(crate) fn strip_legacy_settings(json: &str) -> Option<(Option<i64>, String, String)> {
    let mut v = serde_json::from_str::<serde_json::Value>(json).ok()?;
    let conn_obj = v.get_mut("connection")?.as_object_mut()?;
    let proxy_id = conn_obj.get("proxyId").and_then(|x| x.as_i64());
    let mut found = conn_obj
        .remove("proxyPassword")
        .and_then(|p| p.as_str().map(str::to_owned));
    if let Some(obj) = conn_obj.get_mut("proxy").and_then(|p| p.as_object_mut())
        && let Some(p) = obj.remove("password")
    {
        found = found.or(p.as_str().map(str::to_owned));
    }
    let pw = found?;
    Some((proxy_id, pw, v.to_string()))
}

impl Core {
    /// Load proxy passwords from the credential store into memory and move
    /// any legacy plaintext copies out of SQLite. Called at startup and
    /// safe to call again.
    pub async fn load_proxy_secrets(&self) -> Result<()> {
        let legacy = self.db.read(|conn| find_legacy_secrets(conn)).await?;
        for (id, pw) in legacy.column.iter() {
            self.store_proxy_password(*id, Some(pw.clone())).await?;
        }
        for (_, proxy_id, pw, _) in legacy.settings.iter().filter(|s| s.1 > 0) {
            self.store_proxy_password(*proxy_id, Some(pw.clone()))
                .await?;
        }
        if !legacy.column.is_empty() || !legacy.settings.is_empty() {
            self.db
                .write(move |conn| {
                    if !legacy.column.is_empty() {
                        conn.execute("UPDATE proxies SET password = NULL", [])?;
                    }
                    for (account_id, _, _, json) in &legacy.settings {
                        conn.execute(
                            "UPDATE accounts SET settings_json = ?2 WHERE id = ?1",
                            rusqlite::params![account_id, json],
                        )?;
                    }
                    Ok(())
                })
                .await?;
            tracing::info!("moved legacy proxy passwords into the credential store");
        }
        for p in self.db.read(|conn| proxy::list(conn)).await? {
            if p.username.is_empty() {
                continue;
            }
            let pw = credentials::load_async(
                self.credentials.clone(),
                proxy::credential_key(p.id),
                Slot::ProxyPassword,
            )
            .await
            .ok();
            proxy::cache_password(p.id, pw);
        }
        self.refresh_global_proxy().await
    }

    async fn refresh_global_proxy(&self) -> Result<()> {
        self.db.read(|conn| proxy::refresh_global_proxy(conn)).await
    }

    async fn store_proxy_password(&self, proxy_id: i64, password: Option<String>) -> Result<()> {
        let key = proxy::credential_key(proxy_id);
        match password.clone().filter(|p| !p.is_empty()) {
            Some(pw) => {
                credentials::store_async(self.credentials.clone(), key, Slot::ProxyPassword, pw)
                    .await?
            }
            None => {
                credentials::delete_async(self.credentials.clone(), key, Slot::ProxyPassword)
                    .await?
            }
        }
        proxy::cache_password(proxy_id, password);
        Ok(())
    }

    /// Shared proxy list (passwords omitted; `has_password` tells the UI).
    pub async fn list_proxies(&self) -> Result<Vec<(ProxyConfig, bool)>> {
        let list = self.db.read(|conn| proxy::list(conn)).await?;
        Ok(list
            .into_iter()
            .map(|p| {
                let has = proxy::cached_password(p.id).is_some();
                (p, has)
            })
            .collect())
    }

    /// Create/update a proxy. `password`: `None` keeps the stored one,
    /// `Some("")` clears it, `Some(x)` replaces it.
    pub async fn save_proxy(&self, p: ProxyConfig, password: Option<String>) -> Result<i64> {
        let id = self.db.write(move |conn| proxy::save(conn, &p)).await?;
        if let Some(pw) = password {
            self.store_proxy_password(id, Some(pw)).await?;
        }
        self.refresh_global_proxy().await?;
        Ok(id)
    }

    pub async fn delete_proxy(&self, id: i64) -> Result<bool> {
        let gone = self
            .db
            .write(move |conn| {
                if proxy::global_proxy_id(conn)? == Some(id) {
                    proxy::set_global_proxy_id(conn, None)?;
                }
                proxy::delete(conn, id)
            })
            .await?;
        self.store_proxy_password(id, None).await?;
        self.refresh_global_proxy().await?;
        Ok(gone)
    }

    pub async fn global_proxy_id(&self) -> Result<Option<i64>> {
        self.db.read(|conn| proxy::global_proxy_id(conn)).await
    }

    pub async fn set_global_proxy_id(&self, id: Option<i64>) -> Result<()> {
        self.db
            .write(move |conn| proxy::set_global_proxy_id(conn, id))
            .await?;
        self.refresh_global_proxy().await
    }

    /// Choose direct / global / custom for one account.
    pub async fn set_account_proxy(
        &self,
        account_id: i64,
        mode: ProxyMode,
        proxy_id: Option<i64>,
    ) -> Result<()> {
        if mode == ProxyMode::Custom && proxy_id.is_none() {
            return Err(CoreError::Other("choose a proxy for custom mode".into()));
        }
        self.db
            .write(move |conn| {
                let mut cfg = repo::accounts::get_config(conn, account_id)?
                    .ok_or_else(|| CoreError::NotFound(format!("account {account_id}")))?;
                cfg.settings.connection.proxy_mode = Some(mode);
                cfg.settings.connection.proxy_id = proxy_id.filter(|_| mode == ProxyMode::Custom);
                repo::accounts::set_settings(conn, account_id, &cfg.settings)
            })
            .await
    }

    /// Siftmail: dial the server this account really uses (IMAP host, JMAP
    /// origin or the Gmail API) through `proxy_id`, or through whatever the
    /// account is currently set to use when `proxy_id` is `None`.
    pub async fn test_account_proxy(
        &self,
        account_id: i64,
        proxy_id: Option<i64>,
    ) -> Result<(String, u64)> {
        let cfg = self
            .db
            .read(move |conn| repo::accounts::get_config(conn, account_id))
            .await?
            .ok_or_else(|| CoreError::NotFound(format!("account {account_id}")))?;
        let (host, port) = proxy::account_endpoint(&cfg);
        let p = match proxy_id {
            Some(id) => {
                let mut p = self
                    .db
                    .read(move |conn| proxy::get(conn, id))
                    .await?
                    .ok_or_else(|| CoreError::NotFound(format!("proxy {id}")))?;
                p.password = proxy::cached_password(id);
                Some(p)
            }
            None => cfg.settings.connection.proxy.clone(),
        };
        let target = format!("{host}:{port}");
        let ms = match p {
            Some(p) => proxy::test_proxy(&p, &host, port).await?.as_millis() as u64,
            None => {
                let start = std::time::Instant::now();
                tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    tokio::net::TcpStream::connect((host.as_str(), port)),
                )
                .await
                .map_err(|_| CoreError::Network(format!("{target}: timed out")))?
                .map_err(|e| CoreError::Network(format!("{target}: {e}")))?;
                start.elapsed().as_millis() as u64
            }
        };
        Ok((target, ms))
    }

    /// Siftmail: test a shared proxy against the servers of the accounts
    /// that use it (or, when none does yet, the first account's server), so
    /// the result reflects real traffic rather than a fixed public host.
    /// Returns the dialed target and round-trip time.
    pub async fn test_proxy_for_accounts(&self, id: i64) -> Result<Vec<(String, Result<u64>)>> {
        let (users, first) = self
            .db
            .read(move |conn| {
                let all = repo::accounts::list_configs(conn)?;
                let users: Vec<(String, u16)> = all
                    .iter()
                    .filter(|c| c.settings.connection.proxy.as_ref().map(|p| p.id) == Some(id))
                    .map(proxy::account_endpoint)
                    .collect();
                Ok((users, all.first().map(proxy::account_endpoint)))
            })
            .await?;
        let mut targets = users;
        targets.dedup();
        if targets.is_empty() {
            targets.extend(first);
        }
        if targets.is_empty() {
            return Err(CoreError::Other(
                "add an account first: the test dials the account's own mail server".into(),
            ));
        }
        let mut out = Vec::new();
        for (host, port) in targets.into_iter().take(5) {
            let r = self.test_proxy(id, host.clone(), port).await;
            out.push((format!("{host}:{port}"), r));
        }
        Ok(out)
    }

    /// Dial `host:port` through a saved proxy (with its stored password).
    pub async fn test_proxy(&self, id: i64, host: String, port: u16) -> Result<u64> {
        let mut p = self
            .db
            .read(move |conn| proxy::get(conn, id))
            .await?
            .ok_or_else(|| CoreError::NotFound(format!("proxy {id}")))?;
        p.password = proxy::cached_password(id);
        Ok(proxy::test_proxy(&p, &host, port).await?.as_millis() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_and_strips_legacy_plaintext_passwords() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrations::run(&mut conn).unwrap();
        conn.execute("ALTER TABLE proxies ADD COLUMN password TEXT", [])
            .unwrap();
        conn.execute(
            "INSERT INTO proxies (id, kind, host, port, username, password) VALUES (7, 'socks5', 'h', 1080, 'u', 'colpw')",
            [],
        )
        .unwrap();
        let legacy = find_legacy_secrets(&conn).unwrap();
        assert_eq!(legacy.column, vec![(7, "colpw".to_string())]);

        let (id, pw, cleaned) = strip_legacy_settings(
            r#"{"connection":{"proxyId":7,"proxyPassword":"jsonpw","proxy":{"password":"x"}}}"#,
        )
        .unwrap();
        assert_eq!((id, pw.as_str()), (Some(7), "jsonpw"));
        assert!(!cleaned.contains("jsonpw") && !cleaned.contains("password"));
        assert!(strip_legacy_settings(r#"{"connection":{"proxyId":7}}"#).is_none());
    }
}
