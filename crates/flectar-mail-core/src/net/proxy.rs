//! Siftmail M5: per-account outbound proxy (SOCKS5 or HTTP CONNECT).
//!
//! [`connect_tcp`] is the single TCP factory for mail connections: TLS is
//! negotiated by the caller *after* the tunnel is established, so the proxy
//! never sees plaintext. SOCKS5 supports username/password auth (RFC 1929)
//! and remote DNS (the proxy resolves the host name).

use crate::error::{CoreError, Result};
use base64::Engine as _;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyKind {
    Socks5,
    HttpConnect,
}

impl ProxyKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Socks5 => "socks5",
            Self::HttpConnect => "http",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        match s {
            "socks5" => Some(Self::Socks5),
            "http" => Some(Self::HttpConnect),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyConfig {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: String,
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub username: String,
    /// Never persisted in SQLite; supplied from the credential store.
    #[serde(skip)]
    pub password: Option<String>,
    /// SOCKS5: let the proxy resolve the target host name (default on).
    #[serde(default = "yes")]
    pub remote_dns: bool,
}

fn yes() -> bool {
    true
}

fn io_err(stage: &str, e: impl std::fmt::Display) -> CoreError {
    CoreError::Network(format!("proxy {stage}: {e}"))
}

/// Open a TCP stream to `host:port`, directly or through `proxy`.
pub async fn connect_tcp(host: &str, port: u16, proxy: Option<&ProxyConfig>) -> Result<TcpStream> {
    let Some(p) = proxy else {
        return TcpStream::connect((host, port))
            .await
            .map_err(|e| CoreError::Network(format!("connect {host}:{port}: {e}")));
    };
    let mut s = TcpStream::connect((p.host.as_str(), p.port))
        .await
        .map_err(|e| io_err("unreachable", format!("{}:{}: {e}", p.host, p.port)))?;
    s.set_nodelay(true).ok();
    match p.kind {
        ProxyKind::Socks5 => socks5_handshake(&mut s, host, port, p).await?,
        ProxyKind::HttpConnect => s = http_connect(s, host, port, p).await?,
    }
    Ok(s)
}

async fn socks5_handshake(s: &mut TcpStream, host: &str, port: u16, p: &ProxyConfig) -> Result<()> {
    let with_auth = !p.username.is_empty();
    let greeting: &[u8] = if with_auth { &[5, 2, 0, 2] } else { &[5, 1, 0] };
    s.write_all(greeting)
        .await
        .map_err(|e| io_err("socks greeting", e))?;
    let mut reply = [0u8; 2];
    s.read_exact(&mut reply)
        .await
        .map_err(|e| io_err("socks greeting", e))?;
    if reply[0] != 5 {
        return Err(io_err("socks", "not a SOCKS5 proxy"));
    }
    match reply[1] {
        0 => {}
        2 if with_auth => {
            let pass = p.password.as_deref().unwrap_or("");
            if p.username.len() > 255 || pass.len() > 255 {
                return Err(io_err("socks auth", "credentials too long"));
            }
            let mut msg = vec![1, p.username.len() as u8];
            msg.extend_from_slice(p.username.as_bytes());
            msg.push(pass.len() as u8);
            msg.extend_from_slice(pass.as_bytes());
            s.write_all(&msg)
                .await
                .map_err(|e| io_err("socks auth", e))?;
            let mut r = [0u8; 2];
            s.read_exact(&mut r)
                .await
                .map_err(|e| io_err("socks auth", e))?;
            if r[1] != 0 {
                return Err(io_err("socks auth", "proxy rejected username/password"));
            }
        }
        0xff => return Err(io_err("socks", "no acceptable authentication method")),
        m => return Err(io_err("socks", format!("unsupported method {m}"))),
    }
    let mut req = vec![5, 1, 0];
    let ip = host.parse::<std::net::IpAddr>().ok();
    match (ip, p.remote_dns) {
        (Some(std::net::IpAddr::V4(v4)), _) => {
            req.push(1);
            req.extend_from_slice(&v4.octets());
        }
        (Some(std::net::IpAddr::V6(v6)), _) => {
            req.push(4);
            req.extend_from_slice(&v6.octets());
        }
        (None, true) => {
            if host.len() > 255 {
                return Err(io_err("socks", "host name too long"));
            }
            req.push(3);
            req.push(host.len() as u8);
            req.extend_from_slice(host.as_bytes());
        }
        (None, false) => {
            let addr = tokio::net::lookup_host((host, port))
                .await
                .map_err(|e| io_err("dns", e))?
                .next()
                .ok_or_else(|| io_err("dns", format!("{host} has no address")))?;
            match addr.ip() {
                std::net::IpAddr::V4(v4) => {
                    req.push(1);
                    req.extend_from_slice(&v4.octets());
                }
                std::net::IpAddr::V6(v6) => {
                    req.push(4);
                    req.extend_from_slice(&v6.octets());
                }
            }
        }
    }
    req.extend_from_slice(&port.to_be_bytes());
    s.write_all(&req)
        .await
        .map_err(|e| io_err("socks connect", e))?;
    let mut head = [0u8; 4];
    s.read_exact(&mut head)
        .await
        .map_err(|e| io_err("socks connect", e))?;
    if head[1] != 0 {
        let why = match head[1] {
            2 => "connection not allowed by ruleset",
            3 => "network unreachable",
            4 => "host unreachable",
            5 => "connection refused",
            6 => "TTL expired",
            _ => "general failure",
        };
        return Err(io_err("socks connect", format!("{host}:{port}: {why}")));
    }
    let skip = match head[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut l = [0u8; 1];
            s.read_exact(&mut l)
                .await
                .map_err(|e| io_err("socks connect", e))?;
            l[0] as usize
        }
        _ => return Err(io_err("socks connect", "bad address type")),
    };
    let mut rest = vec![0u8; skip + 2];
    s.read_exact(&mut rest)
        .await
        .map_err(|e| io_err("socks connect", e))?;
    Ok(())
}

async fn http_connect(s: TcpStream, host: &str, port: u16, p: &ProxyConfig) -> Result<TcpStream> {
    let target = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let mut req = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
    if !p.username.is_empty() {
        let token = base64::engine::general_purpose::STANDARD.encode(format!(
            "{}:{}",
            p.username,
            p.password.as_deref().unwrap_or("")
        ));
        req.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    req.push_str("\r\n");
    let mut reader = BufReader::new(s);
    reader
        .get_mut()
        .write_all(req.as_bytes())
        .await
        .map_err(|e| io_err("http connect", e))?;
    let mut status = String::new();
    reader
        .read_line(&mut status)
        .await
        .map_err(|e| io_err("http connect", e))?;
    let code = status.split_whitespace().nth(1).unwrap_or("");
    if code != "200" {
        return Err(io_err(
            "http connect",
            format!("{target}: {}", status.trim()),
        ));
    }
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| io_err("http connect", e))?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }
    if !reader.buffer().is_empty() {
        return Err(io_err(
            "http connect",
            "proxy sent data before the tunnel opened",
        ));
    }
    Ok(reader.into_inner())
}

/// Measure a tunnel to `host:port` (settings "test connection").
pub async fn test_proxy(p: &ProxyConfig, host: &str, port: u16) -> Result<std::time::Duration> {
    let start = std::time::Instant::now();
    let s = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        connect_tcp(host, port, Some(p)),
    )
    .await
    .map_err(|_| io_err("test", "timed out"))??;
    drop(s);
    Ok(start.elapsed())
}

// ---- persistence (`proxies` table, shared by accounts via proxy_id) ----

pub fn list(conn: &Connection) -> Result<Vec<ProxyConfig>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, kind, host, port, username, remote_dns FROM proxies ORDER BY id",
    )?;
    let rows = stmt
        .query_map([], row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows.into_iter().flatten().collect())
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<Option<ProxyConfig>> {
    let kind: String = r.get(2)?;
    Ok(ProxyKind::parse(&kind).map(|kind| ProxyConfig {
        id: r.get(0).unwrap_or(0),
        name: r.get(1).unwrap_or_default(),
        kind,
        host: r.get(3).unwrap_or_default(),
        port: r.get::<_, i64>(4).unwrap_or(0).clamp(0, 65535) as u16,
        username: r.get(5).unwrap_or_default(),
        password: None,
        remote_dns: r.get(6).unwrap_or(true),
    }))
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<ProxyConfig>> {
    Ok(conn
        .query_row(
            "SELECT id, name, kind, host, port, username, remote_dns FROM proxies WHERE id = ?1",
            params![id],
            row,
        )
        .optional()?
        .flatten())
}

pub fn save(conn: &Connection, p: &ProxyConfig) -> Result<i64> {
    if p.host.trim().is_empty() || p.port == 0 {
        return Err(CoreError::Other("proxy host and port are required".into()));
    }
    if p.id > 0 {
        conn.execute(
            "UPDATE proxies SET name = ?2, kind = ?3, host = ?4, port = ?5, username = ?6, remote_dns = ?7 WHERE id = ?1",
            params![p.id, p.name, p.kind.as_str(), p.host.trim(), p.port, p.username, p.remote_dns],
        )?;
        Ok(p.id)
    } else {
        conn.execute(
            "INSERT INTO proxies (name, kind, host, port, username, remote_dns) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![p.name, p.kind.as_str(), p.host.trim(), p.port, p.username, p.remote_dns],
        )?;
        Ok(conn.last_insert_rowid())
    }
}

pub fn delete(conn: &Connection, id: i64) -> Result<bool> {
    Ok(conn.execute("DELETE FROM proxies WHERE id = ?1", params![id])? > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// Echo server standing in for the IMAP host.
    async fn echo() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                tokio::spawn(async move {
                    let (mut r, mut w) = s.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        port
    }

    async fn pipe(mut a: TcpStream, mut b: TcpStream) {
        let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
    }

    /// Minimal SOCKS5 server: optional user/pass, domain or IPv4 targets.
    async fn socks5(
        require: Option<(&'static str, &'static str)>,
    ) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let seen2 = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let seen = seen2.clone();
                tokio::spawn(async move {
                    let mut h = [0u8; 2];
                    s.read_exact(&mut h).await.unwrap();
                    let mut methods = vec![0u8; h[1] as usize];
                    s.read_exact(&mut methods).await.unwrap();
                    if let Some((u, p)) = require {
                        if !methods.contains(&2) {
                            s.write_all(&[5, 0xff]).await.unwrap();
                            return;
                        }
                        s.write_all(&[5, 2]).await.unwrap();
                        let mut v = [0u8; 2];
                        s.read_exact(&mut v).await.unwrap();
                        let mut user = vec![0u8; v[1] as usize];
                        s.read_exact(&mut user).await.unwrap();
                        let mut pl = [0u8; 1];
                        s.read_exact(&mut pl).await.unwrap();
                        let mut pass = vec![0u8; pl[0] as usize];
                        s.read_exact(&mut pass).await.unwrap();
                        let ok = user == u.as_bytes() && pass == p.as_bytes();
                        s.write_all(&[1, if ok { 0 } else { 1 }]).await.unwrap();
                        if !ok {
                            return;
                        }
                    } else {
                        s.write_all(&[5, 0]).await.unwrap();
                    }
                    let mut r = [0u8; 4];
                    s.read_exact(&mut r).await.unwrap();
                    let host = match r[3] {
                        3 => {
                            let mut l = [0u8; 1];
                            s.read_exact(&mut l).await.unwrap();
                            let mut name = vec![0u8; l[0] as usize];
                            s.read_exact(&mut name).await.unwrap();
                            String::from_utf8(name).unwrap()
                        }
                        1 => {
                            let mut ip = [0u8; 4];
                            s.read_exact(&mut ip).await.unwrap();
                            std::net::Ipv4Addr::from(ip).to_string()
                        }
                        _ => unreachable!(),
                    };
                    let mut pb = [0u8; 2];
                    s.read_exact(&mut pb).await.unwrap();
                    let port = u16::from_be_bytes(pb);
                    seen.lock().unwrap().push(format!("{host}:{port}"));
                    let target = if host == "imap.example.test" {
                        "127.0.0.1".to_string()
                    } else {
                        host
                    };
                    match TcpStream::connect((target.as_str(), port)).await {
                        Ok(t) => {
                            s.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await.unwrap();
                            pipe(s, t).await;
                        }
                        Err(_) => {
                            s.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).await.unwrap();
                        }
                    }
                });
            }
        });
        (port, seen)
    }

    async fn http_proxy(auth: Option<&'static str>) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((s, _)) = l.accept().await {
                tokio::spawn(async move {
                    let mut r = BufReader::new(s);
                    let mut first = String::new();
                    r.read_line(&mut first).await.unwrap();
                    let mut authorized = auth.is_none();
                    loop {
                        let mut line = String::new();
                        r.read_line(&mut line).await.unwrap();
                        if let Some(a) = auth
                            && line.trim() == format!("Proxy-Authorization: Basic {a}")
                        {
                            authorized = true;
                        }
                        if line == "\r\n" {
                            break;
                        }
                    }
                    let mut s = r.into_inner();
                    if !authorized {
                        s.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n")
                            .await
                            .unwrap();
                        return;
                    }
                    let target = first.split_whitespace().nth(1).unwrap().to_string();
                    let t = TcpStream::connect(target).await.unwrap();
                    s.write_all(b"HTTP/1.1 200 Connection established\r\nVia: test\r\n\r\n")
                        .await
                        .unwrap();
                    pipe(s, t).await;
                });
            }
        });
        port
    }

    async fn roundtrip(mut s: TcpStream) {
        s.write_all(b"* OK hello").await.unwrap();
        let mut buf = [0u8; 10];
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"* OK hello");
    }

    fn cfg(kind: ProxyKind, port: u16) -> ProxyConfig {
        ProxyConfig {
            id: 0,
            name: "p".into(),
            kind,
            host: "127.0.0.1".into(),
            port,
            username: String::new(),
            password: None,
            remote_dns: true,
        }
    }

    #[tokio::test]
    async fn socks5_remote_dns_with_auth_tunnels_bytes() {
        let target = echo().await;
        let (port, seen) = socks5(Some(("alice", "s3cret"))).await;
        let mut p = cfg(ProxyKind::Socks5, port);
        p.username = "alice".into();
        p.password = Some("s3cret".into());
        roundtrip(
            connect_tcp("imap.example.test", target, Some(&p))
                .await
                .unwrap(),
        )
        .await;
        // Remote DNS: the proxy saw the host name, not a resolved address.
        assert_eq!(
            seen.lock().unwrap()[0],
            format!("imap.example.test:{target}")
        );
        p.password = Some("wrong".into());
        let err = connect_tcp("imap.example.test", target, Some(&p))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("rejected"), "{err}");
    }

    #[tokio::test]
    async fn socks5_reports_target_failures() {
        let (port, _) = socks5(None).await;
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let err = connect_tcp("127.0.0.1", closed, Some(&cfg(ProxyKind::Socks5, port)))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("connection refused"), "{err}");
        let target = echo().await;
        roundtrip(
            connect_tcp("127.0.0.1", target, Some(&cfg(ProxyKind::Socks5, port)))
                .await
                .unwrap(),
        )
        .await;
    }

    #[tokio::test]
    async fn http_connect_with_basic_auth() {
        let target = echo().await;
        let token = base64::engine::general_purpose::STANDARD.encode("bob:pw");
        let port = http_proxy(Some(Box::leak(token.into_boxed_str()))).await;
        let mut p = cfg(ProxyKind::HttpConnect, port);
        let err = connect_tcp("127.0.0.1", target, Some(&p))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("407"), "{err}");
        p.username = "bob".into();
        p.password = Some("pw".into());
        roundtrip(connect_tcp("127.0.0.1", target, Some(&p)).await.unwrap()).await;
        assert!(test_proxy(&p, "127.0.0.1", target).await.is_ok());
    }

    #[tokio::test]
    async fn unreachable_proxy_is_a_proxy_error() {
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let err = connect_tcp(
            "imap.example.test",
            993,
            Some(&cfg(ProxyKind::Socks5, closed)),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().starts_with("proxy unreachable")
                || err.to_string().contains("proxy unreachable"),
            "{err}"
        );
        assert!(is_proxy_error(&err));
    }

    #[test]
    fn persistence_round_trip_never_stores_password() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::db::migrations::run(&mut conn).unwrap();
        let mut p = cfg(ProxyKind::Socks5, 1080);
        p.username = "u".into();
        p.password = Some("secret".into());
        let id = save(&conn, &p).unwrap();
        let got = get(&conn, id).unwrap().unwrap();
        assert_eq!(got.password, None);
        assert_eq!(got.username, "u");
        assert_eq!(list(&conn).unwrap().len(), 1);
        let cols: String = conn
            .query_row(
                "SELECT group_concat(name) FROM pragma_table_info('proxies')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!cols.contains("pass"));
        assert!(save(&conn, &cfg(ProxyKind::HttpConnect, 0)).is_err());
        assert!(delete(&conn, id).unwrap());
    }
}

/// Whether an error came from the proxy itself (unreachable/auth/handshake),
/// so the scheduler can back off only accounts using that proxy.
pub fn is_proxy_error(err: &CoreError) -> bool {
    matches!(err, CoreError::Network(m) if m.starts_with("proxy "))
}
