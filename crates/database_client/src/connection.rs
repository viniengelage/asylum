//! Saved connections. A connection lives either in the project (`.asylum/db/connections.json`,
//! which may be committed) or only in this profile (the key-value store); the password is in the
//! profile's keychain either way.

use crate::session::ConnectTarget;
use crate::tls::SslMode;
use anyhow::Context as _;
use db::kvp::KeyValueStore;
use fs::Fs;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const PROJECT_FILE: &str = ".asylum/db/connections.json";
pub const DEFAULT_PORT: u16 = 5432;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    #[default]
    Local,
    Dev,
    Staging,
    Prod,
}

impl Environment {
    pub const ALL: [Self; 4] = [Self::Local, Self::Dev, Self::Staging, Self::Prod];

    pub fn label(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Dev => "dev",
            Self::Staging => "staging",
            Self::Prod => "prod",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Only this profile sees it.
    Profile,
    /// Written to the project, without the password.
    Project,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedConnection {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub environment: Environment,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub database: String,
    pub user: String,
    #[serde(default)]
    pub ssl_mode: SslMode,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default = "default_true")]
    pub confirm_writes: bool,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn default_true() -> bool {
    true
}

impl SavedConnection {
    /// Same server, database and user share one keychain entry across projects.
    pub fn keychain_url(&self) -> String {
        format!(
            "postgres://{}@{}:{}/{}",
            self.user, self.host, self.port, self.database
        )
    }

    /// `user@host:port`, as the dock shows it under the name.
    pub fn address(&self) -> String {
        format!("{}@{}:{}", self.user, self.host, self.port)
    }

    pub fn target(&self, password: Option<&str>) -> ConnectTarget {
        let mut config = tokio_postgres::Config::new();
        config
            .host(&self.host)
            .port(self.port)
            .dbname(&self.database)
            .user(&self.user)
            .application_name("Asylum")
            .connect_timeout(Duration::from_secs(10))
            .ssl_mode(self.ssl_mode.negotiation());
        if let Some(password) = password.filter(|password| !password.is_empty()) {
            config.password(password);
        }
        if self.read_only {
            config.options("-c default_transaction_read_only=on");
        }
        ConnectTarget {
            config,
            ssl_mode: self.ssl_mode,
        }
    }
}

/// The fields a connection URL fills in the form.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UrlFields {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub database: Option<String>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub ssl_mode: Option<SslMode>,
}

impl UrlFields {
    pub fn count(&self) -> usize {
        [
            self.host.is_some(),
            self.port.is_some(),
            self.database.is_some(),
            self.user.is_some(),
            self.password.is_some(),
            self.ssl_mode.is_some(),
        ]
        .into_iter()
        .filter(|filled| *filled)
        .count()
    }
}

/// Reads a `postgres://` URL or a `key=value` string into form fields. Unix socket hosts are
/// left out: the form only dials TCP.
pub fn parse_url(input: &str) -> anyhow::Result<UrlFields> {
    let target = ConnectTarget::parse(input)?;
    let config = &target.config;
    let host = config.get_hosts().iter().find_map(|host| match host {
        tokio_postgres::config::Host::Tcp(host) => Some(host.clone()),
        #[cfg(unix)]
        tokio_postgres::config::Host::Unix(_) => None,
    });
    let has_ssl_mode = input.contains("sslmode");
    Ok(UrlFields {
        host,
        port: config.get_ports().first().copied(),
        database: config.get_dbname().map(str::to_owned),
        user: config.get_user().map(str::to_owned),
        password: config
            .get_password()
            .map(|password| String::from_utf8_lossy(password).into_owned()),
        ssl_mode: has_ssl_mode.then_some(target.ssl_mode),
    })
}

#[derive(Default, Serialize, Deserialize)]
struct ConnectionsFile {
    #[serde(default)]
    connections: Vec<SavedConnection>,
}

pub fn project_file(root: &Path) -> PathBuf {
    root.join(PROJECT_FILE)
}

pub async fn load_project(fs: &dyn Fs, root: &Path) -> anyhow::Result<Vec<SavedConnection>> {
    let path = project_file(root);
    if !fs.is_file(&path).await {
        return Ok(Vec::new());
    }
    let text = fs.load(&path).await?;
    let file: ConnectionsFile = serde_json::from_str(&text)
        .with_context(|| format!("{} ilegível", path.display()))?;
    Ok(file.connections)
}

pub async fn save_project(
    fs: &dyn Fs,
    root: &Path,
    connections: Vec<SavedConnection>,
) -> anyhow::Result<()> {
    let path = project_file(root);
    if let Some(parent) = path.parent() {
        fs.create_dir(parent).await?;
    }
    let mut text = serde_json::to_string_pretty(&ConnectionsFile { connections })?;
    text.push('\n');
    fs.atomic_write(path, text).await
}

fn profile_key(root: &Path) -> String {
    format!("database_client-connections-{}", root.display())
}

pub fn load_profile(store: &KeyValueStore, root: &Path) -> anyhow::Result<Vec<SavedConnection>> {
    match store.read_kvp(&profile_key(root))? {
        Some(text) => Ok(serde_json::from_str::<ConnectionsFile>(&text)?.connections),
        None => Ok(Vec::new()),
    }
}

pub async fn save_profile(
    store: KeyValueStore,
    root: &Path,
    connections: Vec<SavedConnection>,
) -> anyhow::Result<()> {
    let text = serde_json::to_string(&ConnectionsFile { connections })?;
    store.write_kvp(profile_key(root), text).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_fills_the_form() {
        let fields =
            parse_url("postgres://app:s3cr%40t@localhost:5433/trix_core?sslmode=require").unwrap();
        assert_eq!(
            fields,
            UrlFields {
                host: Some("localhost".into()),
                port: Some(5433),
                database: Some("trix_core".into()),
                user: Some("app".into()),
                password: Some("s3cr@t".into()),
                ssl_mode: Some(SslMode::Require),
            }
        );
        assert_eq!(fields.count(), 6);

        // tokio-postgres fills in the default port, which is what the form wants anyway.
        let fields = parse_url("postgres://db.internal/app").unwrap();
        assert_eq!(fields.port, Some(DEFAULT_PORT));
        assert_eq!(fields.ssl_mode, None);
        assert_eq!(fields.count(), 3);
    }

    #[test]
    fn project_file_keeps_defaults_short() {
        let file: ConnectionsFile = serde_json::from_str(
            r#"{"connections": [{"id": "a", "name": "trix", "host": "localhost",
                "database": "trix", "user": "app", "ssl_mode": "verify-full"}]}"#,
        )
        .unwrap();
        let connection = &file.connections[0];
        assert_eq!(connection.port, DEFAULT_PORT);
        assert_eq!(connection.environment, Environment::Local);
        assert_eq!(connection.ssl_mode, SslMode::VerifyFull);
        assert!(connection.confirm_writes);
        assert_eq!(connection.keychain_url(), "postgres://app@localhost:5432/trix");
    }
}
