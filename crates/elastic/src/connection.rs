//! Saved connections. A connection lives either in the project (`.asylum/elastic/connections.json`,
//! which may be committed) or only in this profile (the key-value store); the password or API key
//! is in the profile's keychain either way.

use crate::client::{Auth, Endpoint};
use anyhow::Context as _;
use db::kvp::KeyValueStore;
use fs::Fs;
use http_client::Url;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const PROJECT_FILE: &str = ".asylum/elastic/connections.json";
pub const STREAMS_FILE: &str = ".asylum/elastic/streams.json";
pub const QUERIES_DIR: &str = ".asylum/elastic/queries";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    Dev,
    Staging,
    #[default]
    Prod,
}

impl Environment {
    pub const ALL: [Self; 3] = [Self::Dev, Self::Staging, Self::Prod];

    pub fn label(self) -> &'static str {
        match self {
            Self::Dev => "dev",
            Self::Staging => "staging",
            Self::Prod => "prod",
        }
    }
}

/// How requests reach the cluster.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    /// Kibana's console proxy, with the login used in the browser.
    #[default]
    Kibana,
    /// Elasticsearch's own port.
    Direct,
}

impl Via {
    pub fn label(self) -> &'static str {
        match self {
            Via::Kibana => "Kibana",
            Via::Direct => "direto",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    #[default]
    Password,
    ApiKey,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Only this profile sees it.
    Profile,
    /// Written to the project, without the secret.
    Project,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedConnection {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub environment: Environment,
    #[serde(default)]
    pub via: Via,
    pub url: String,
    #[serde(default)]
    pub auth: AuthKind,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub username: String,
}

impl SavedConnection {
    /// Same URL and user share one keychain entry across projects.
    pub fn keychain_url(&self) -> String {
        match self.auth {
            AuthKind::Password => {
                format!("elastic://{}@{}", self.username, self.url_without_slash())
            }
            AuthKind::ApiKey => format!("elastic-api-key://{}", self.url_without_slash()),
            AuthKind::None => String::new(),
        }
    }

    fn url_without_slash(&self) -> &str {
        self.url.trim_end_matches('/')
    }

    /// `kibana.example.com`, as the dock shows it under the name.
    pub fn host(&self) -> String {
        Url::parse(&self.url)
            .ok()
            .and_then(|url| {
                let host = url.host_str()?.to_string();
                Some(match url.port() {
                    Some(port) => format!("{host}:{port}"),
                    None => host,
                })
            })
            .unwrap_or_else(|| self.url.clone())
    }

    pub fn endpoint(&self) -> anyhow::Result<Endpoint> {
        let url =
            Url::parse(self.url.trim()).with_context(|| format!("URL inválida: {}", self.url))?;
        anyhow::ensure!(
            matches!(url.scheme(), "http" | "https"),
            "a URL precisa começar com http:// ou https://"
        );
        Ok(match self.via {
            Via::Kibana => Endpoint::Kibana(url),
            Via::Direct => Endpoint::Direct(url),
        })
    }

    pub fn auth_with(&self, secret: Option<String>) -> Auth {
        match self.auth {
            AuthKind::Password => Auth::Basic {
                username: self.username.clone(),
                password: secret.unwrap_or_default(),
            },
            AuthKind::ApiKey => Auth::ApiKey(secret.unwrap_or_default()),
            AuthKind::None => Auth::None,
        }
    }
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
    let file: ConnectionsFile =
        serde_json::from_str(&text).with_context(|| format!("{} ilegível", path.display()))?;
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
    format!("elastic-connections-{}", root.display())
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

/// The data streams the project cares about, shown first in the dock. Committed, so the team
/// shares them.
#[derive(Default, Serialize, Deserialize)]
struct StreamsFile {
    #[serde(default)]
    streams: Vec<String>,
}

pub async fn load_project_streams(fs: &dyn Fs, root: &Path) -> anyhow::Result<Vec<String>> {
    let path = root.join(STREAMS_FILE);
    if !fs.is_file(&path).await {
        return Ok(Vec::new());
    }
    let text = fs.load(&path).await?;
    let file: StreamsFile =
        serde_json::from_str(&text).with_context(|| format!("{} ilegível", path.display()))?;
    Ok(file.streams)
}

pub async fn save_project_streams(
    fs: &dyn Fs,
    root: &Path,
    streams: Vec<String>,
) -> anyhow::Result<()> {
    let path = root.join(STREAMS_FILE);
    if let Some(parent) = path.parent() {
        fs.create_dir(parent).await?;
    }
    let mut text = serde_json::to_string_pretty(&StreamsFile { streams })?;
    text.push('\n');
    fs.atomic_write(path, text).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_file_keeps_defaults_short() -> anyhow::Result<()> {
        let file: ConnectionsFile = serde_json::from_str(
            r#"{"connections": [{"id": "a", "name": "trix-logs",
                "url": "https://kibana.example.com/", "username": "vinicios"}]}"#,
        )?;
        let connection = &file.connections[0];
        assert_eq!(connection.via, Via::Kibana);
        assert_eq!(connection.auth, AuthKind::Password);
        assert_eq!(connection.environment, Environment::Prod);
        assert_eq!(
            connection.keychain_url(),
            "elastic://vinicios@https://kibana.example.com"
        );
        assert_eq!(connection.host(), "kibana.example.com");
        assert!(matches!(connection.endpoint()?, Endpoint::Kibana(_)));
        Ok(())
    }

    #[test]
    fn endpoint_rejects_urls_without_scheme() {
        let connection = SavedConnection {
            id: "a".into(),
            name: "x".into(),
            environment: Environment::Prod,
            via: Via::Direct,
            url: "10.0.0.5:9200".into(),
            auth: AuthKind::None,
            username: String::new(),
        };
        assert!(connection.endpoint().is_err());
    }
}
