//! Import PostgreSQL connection profiles from a local DBeaver installation.
//!
//! DBeaver keeps connections per project in
//! `<workspace6>/<project>/.dbeaver/data-sources*.json`, and the user/password
//! pairs next to it in `credentials-config.json` — AES-128-CBC encrypted with a
//! key hard-coded in DBeaver itself (first 16 bytes of the file are the IV).
//! That is obfuscation, not protection, so decrypting it here is no different
//! from DBeaver reading it back; the passwords end up in the OS keychain once
//! `AppConfig::save()` runs. If the user enabled DBeaver's Eclipse "secure
//! storage" the passwords aren't in that file and the profiles come in without one.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::config::{ConnectionProfile, SshAuthMethod, SshTunnelConfig, SslMode};

/// DBeaver's built-in key for `credentials-config.json`.
const CREDENTIALS_KEY: [u8; 16] = [
    0xba, 0xbb, 0x4a, 0x9f, 0x77, 0x4a, 0xb8, 0x53, 0xc9, 0x6c, 0x2d, 0x65, 0x3d, 0xfe, 0x54, 0x4a,
];

/// Result of scanning one or more `data-sources*.json` files.
#[derive(Debug, Default)]
pub struct DbeaverScan {
    pub profiles: Vec<ConnectionProfile>,
    /// Connections of other database types (MySQL, Oracle, …) that were ignored.
    pub skipped_other: usize,
    /// Files that were read.
    pub sources: Vec<PathBuf>,
}

/// Candidate DBeaver `workspace6` directories for this OS (native, Flatpak, Snap).
fn workspace_dirs() -> Vec<PathBuf> {
    let mut dirs_out = Vec::new();
    if let Some(data) = dirs::data_dir() {
        // Windows: %APPDATA%\DBeaverData, Linux: ~/.local/share/DBeaverData
        dirs_out.push(data.join("DBeaverData").join("workspace6"));
    }
    if let Some(home) = dirs::home_dir() {
        dirs_out.push(home.join("Library").join("DBeaverData").join("workspace6")); // macOS
        dirs_out.push(home.join(".var/app/io.dbeaver.DBeaverCommunity/data/DBeaverData/workspace6"));
        dirs_out.push(home.join("snap/dbeaver-ce/current/.local/share/DBeaverData/workspace6"));
    }
    dirs_out
}

/// All `data-sources*.json` files across every project of every known workspace.
pub fn find_data_source_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    for ws in workspace_dirs() {
        let Ok(projects) = std::fs::read_dir(&ws) else { continue };
        for project in projects.flatten() {
            let meta_dir = project.path().join(".dbeaver");
            let Ok(entries) = std::fs::read_dir(&meta_dir) else { continue };
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with("data-sources") && name.ends_with(".json") {
                    files.push(e.path());
                }
            }
        }
    }
    files.sort();
    files
}

/// Read every given `data-sources*.json` (plus the `credentials-config.json` in
/// the same folder) and collect the PostgreSQL connections.
pub fn scan(files: &[PathBuf]) -> Result<DbeaverScan> {
    let mut out = DbeaverScan::default();
    for file in files {
        let text = std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        let sources: Value = serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
        let creds = file
            .parent()
            .map(|dir| dir.join("credentials-config.json"))
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| decrypt_credentials(&bytes).ok())
            .unwrap_or(Value::Null);
        let (profiles, skipped) = parse_data_sources(&sources, &creds);
        out.profiles.extend(profiles);
        out.skipped_other += skipped;
        out.sources.push(file.clone());
    }
    Ok(out)
}

/// Decrypt DBeaver's `credentials-config.json` into its JSON document:
/// `{ "<connection id>": { "#connection": {"user", "password"}, "network/ssh_tunnel": {...} } }`.
pub fn decrypt_credentials(bytes: &[u8]) -> Result<Value> {
    use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};

    if bytes.len() < 32 || bytes.len() % 16 != 0 {
        anyhow::bail!("credentials file has unexpected length {}", bytes.len());
    }
    let (iv, cipher_text) = bytes.split_at(16);
    let mut buf = cipher_text.to_vec();
    let plain = cbc::Decryptor::<aes::Aes128>::new(&CREDENTIALS_KEY.into(), iv.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|_| anyhow::anyhow!("credentials file could not be decrypted"))?;
    Ok(serde_json::from_slice(plain)?)
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Ports are strings in some DBeaver versions and numbers in others.
fn port_field(v: &Value, key: &str) -> Option<u16> {
    match v.get(key)? {
        Value::Number(n) => n.as_u64().and_then(|n| u16::try_from(n).ok()),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `jdbc:postgresql://host:port/db?...` → (host, port, db) — used when DBeaver
/// stored only a URL (connections created in "URL" mode).
fn parse_jdbc_url(url: &str) -> Option<(String, Option<u16>, String)> {
    let rest = url.strip_prefix("jdbc:postgresql://")?;
    let rest = rest.split('?').next().unwrap_or(rest);
    let (hostport, db) = rest.split_once('/').unwrap_or((rest, ""));
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_owned(), p.parse().ok()),
        None => (hostport.to_owned(), None),
    };
    Some((host, port, db.to_owned()))
}

/// Pure parser — returns (PostgreSQL profiles, number of non-PostgreSQL connections).
pub fn parse_data_sources(sources: &Value, creds: &Value) -> (Vec<ConnectionProfile>, usize) {
    let mut profiles = Vec::new();
    let mut skipped = 0;
    let Some(conns) = sources.get("connections").and_then(Value::as_object) else {
        return (profiles, 0);
    };
    for (id, conn) in conns {
        if conn.get("provider").and_then(Value::as_str) != Some("postgresql") {
            skipped += 1;
            continue;
        }
        let cfg = conn.get("configuration").unwrap_or(&Value::Null);
        let cred = creds.get(id);
        let conn_cred = cred.and_then(|c| c.get("#connection"));

        let url_parts = str_field(cfg, "url").and_then(parse_jdbc_url);
        let host = str_field(cfg, "host")
            .map(str::to_owned)
            .or_else(|| url_parts.as_ref().map(|u| u.0.clone()))
            .unwrap_or_else(|| "localhost".to_owned());
        let port = port_field(cfg, "port")
            .or_else(|| url_parts.as_ref().and_then(|u| u.1))
            .unwrap_or(5432);
        let database = str_field(cfg, "database")
            .map(str::to_owned)
            .or_else(|| url_parts.as_ref().map(|u| u.2.clone()).filter(|d| !d.is_empty()))
            .unwrap_or_else(|| "postgres".to_owned());
        let user = conn_cred
            .and_then(|c| str_field(c, "user"))
            .or_else(|| str_field(cfg, "user"))
            .unwrap_or("postgres")
            .to_owned();
        let password = conn_cred
            .and_then(|c| str_field(c, "password"))
            .or_else(|| str_field(cfg, "password"))
            .unwrap_or("")
            .to_owned();

        let handlers = cfg.get("handlers");
        let enabled_handler = |key: &str| {
            handlers
                .and_then(|h| h.get(key))
                .filter(|h| h.get("enabled").and_then(Value::as_bool).unwrap_or(false))
        };

        // PostgreSQL SSL handler — key differs between DBeaver versions.
        let ssl = match enabled_handler("postgre_ssl").or_else(|| enabled_handler("ssl")) {
            None => SslMode::Prefer,
            Some(h) => {
                let props = h.get("properties").unwrap_or(&Value::Null);
                match str_field(props, "sslMode").or_else(|| str_field(props, "ssl.mode")) {
                    Some("disable") => SslMode::Disable,
                    Some("allow") | Some("prefer") => SslMode::Prefer,
                    // require, verify-ca, verify-full — or SSL enabled without a mode
                    _ => SslMode::Require,
                }
            }
        };

        let ssh_tunnel = enabled_handler("ssh_tunnel").map(|h| {
            let props = h.get("properties").unwrap_or(&Value::Null);
            let ssh_cred = cred.and_then(|c| c.get("network/ssh_tunnel"));
            let ssh_user = ssh_cred
                .and_then(|c| str_field(c, "user"))
                .or_else(|| str_field(h, "user"))
                .unwrap_or("")
                .to_owned();
            let auth = match str_field(props, "authType") {
                Some("PUBLIC_KEY") => SshAuthMethod::PrivateKey {
                    path: str_field(props, "keyPath").unwrap_or("").to_owned(),
                },
                _ => SshAuthMethod::Password(
                    ssh_cred
                        .and_then(|c| str_field(c, "password"))
                        .or_else(|| str_field(h, "password"))
                        .unwrap_or("")
                        .to_owned(),
                ),
            };
            SshTunnelConfig {
                enabled: true,
                host: str_field(props, "host").unwrap_or("").to_owned(),
                port: port_field(props, "port").unwrap_or(22),
                user: ssh_user,
                auth,
            }
        });

        // DBeaver's "connection type". Every connection is "dev" unless changed,
        // so only the deliberate prod/test choices become a danger tag.
        let color = match cfg.get("type").and_then(Value::as_str) {
            Some("prod") => Some([200, 60, 60]),
            Some("test") => Some([220, 180, 50]),
            _ => None,
        };

        profiles.push(ConnectionProfile {
            name: str_field(conn, "name").unwrap_or(id).to_owned(),
            host,
            port,
            user,
            password,
            database,
            ssl,
            ssh_tunnel,
            group: str_field(conn, "folder").map(str::to_owned),
            color,
        });
    }
    profiles.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    (profiles, skipped)
}

/// True if `existing` already has a profile for the same server/database/user.
pub fn is_duplicate(profile: &ConnectionProfile, existing: &[ConnectionProfile]) -> bool {
    existing.iter().any(|e| {
        e.host.eq_ignore_ascii_case(&profile.host)
            && e.port == profile.port
            && e.database == profile.database
            && e.user == profile.user
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn encrypt(plain: &[u8], iv: [u8; 16]) -> Vec<u8> {
        use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
        let mut buf = vec![0u8; plain.len() + 16];
        buf[..plain.len()].copy_from_slice(plain);
        let ct = cbc::Encryptor::<aes::Aes128>::new(&CREDENTIALS_KEY.into(), &iv.into())
            .encrypt_padded_mut::<Pkcs7>(&mut buf, plain.len())
            .unwrap()
            .to_vec();
        [iv.to_vec(), ct].concat()
    }

    #[test]
    fn decrypts_credentials_file() {
        let doc = json!({"postgres-jdbc-1": {"#connection": {"user": "app", "password": "s3cr3t"}}});
        let bytes = encrypt(doc.to_string().as_bytes(), [7u8; 16]);
        assert_eq!(decrypt_credentials(&bytes).unwrap(), doc);
        assert!(decrypt_credentials(&bytes[..20]).is_err());
    }

    #[test]
    fn parses_postgres_connections_and_skips_others() {
        let sources = json!({
            "folders": {},
            "connections": {
                "postgres-jdbc-1": {
                    "provider": "postgresql",
                    "name": "Prod DB",
                    "folder": "Work",
                    "configuration": {
                        "host": "db.example.com", "port": "6432", "database": "app",
                        "type": "prod",
                        "handlers": {
                            "postgre_ssl": {"enabled": true, "properties": {"sslMode": "verify-full"}},
                            "ssh_tunnel": {
                                "enabled": true,
                                "properties": {"host": "bastion", "port": 2222, "authType": "PUBLIC_KEY", "keyPath": "/k/id_ed25519"}
                            }
                        }
                    }
                },
                "postgres-jdbc-2": {
                    "provider": "postgresql",
                    "name": "local url",
                    "configuration": {"url": "jdbc:postgresql://127.0.0.1:5433/dev?ssl=false", "user": "me", "type": "dev"}
                },
                "mysql8-1": {"provider": "mysql", "name": "mysql", "configuration": {}}
            }
        });
        let creds = json!({
            "postgres-jdbc-1": {
                "#connection": {"user": "app", "password": "pw"},
                "network/ssh_tunnel": {"user": "jump"}
            }
        });
        let (profiles, skipped) = parse_data_sources(&sources, &creds);
        assert_eq!(skipped, 1);
        assert_eq!(profiles.len(), 2);

        let local = &profiles[0];
        assert_eq!(local.name, "local url");
        assert_eq!((local.host.as_str(), local.port, local.database.as_str()), ("127.0.0.1", 5433, "dev"));
        assert_eq!(local.user, "me");
        assert!(local.password.is_empty());
        assert!(local.ssh_tunnel.is_none());
        assert_eq!(local.color, None);

        let prod = &profiles[1];
        assert_eq!((prod.host.as_str(), prod.port, prod.database.as_str()), ("db.example.com", 6432, "app"));
        assert_eq!((prod.user.as_str(), prod.password.as_str()), ("app", "pw"));
        assert_eq!(prod.ssl, SslMode::Require);
        assert_eq!(prod.group.as_deref(), Some("Work"));
        assert_eq!(prod.color, Some([200, 60, 60]));
        let ssh = prod.ssh_tunnel.as_ref().unwrap();
        assert_eq!((ssh.host.as_str(), ssh.port, ssh.user.as_str()), ("bastion", 2222, "jump"));
        assert!(matches!(&ssh.auth, SshAuthMethod::PrivateKey { path } if path == "/k/id_ed25519"));
    }

    #[test]
    fn duplicate_detection() {
        let mut a = ConnectionProfile::default();
        a.host = "DB.local".into();
        let mut b = a.clone();
        b.host = "db.local".into();
        b.name = "other name".into();
        assert!(is_duplicate(&b, &[a.clone()]));
        b.database = "other".into();
        assert!(!is_duplicate(&b, &[a]));
    }
}
