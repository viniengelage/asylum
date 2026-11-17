//! Where the store accounts live and which app a project ships. Secrets (the `.p8`, the service
//! account JSON, the Expo token) only ever go to the keychain; the ids beside them go to the
//! profile's key-value store.
//!
//! Accounts are read at profile scope for now. [`accounts_source`] is the one place that picks
//! the scope, so accounts per project can be plugged in there.

use crate::{app_store_connect, google_play::ServiceAccount};
use anyhow::{Context as _, Result};
use chrono::NaiveDate;
use credentials_provider::CredentialsProvider;
use db::kvp::KeyValueStore;
use futures::future::LocalBoxFuture;
use gpui::{App, AsyncApp, Entity};
use project::Project;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const APP_STORE_CREDENTIALS_URL: &str = "https://appstoreconnect.apple.com/asylum-releases";
const GOOGLE_PLAY_CREDENTIALS_URL: &str = "https://play.google.com/asylum-releases";
const EXPO_CREDENTIALS_URL: &str = "https://expo.dev/asylum-releases";
const ACCOUNT_IDS_KEY: &str = "app_releases_account_ids";
const APP_LINK_KEY_PREFIX: &str = "app_releases_app_link:";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
pub enum AccountKind {
    AppStore,
    GooglePlay,
    Expo,
}

impl AccountKind {
    pub const ALL: [Self; 3] = [Self::Expo, Self::AppStore, Self::GooglePlay];

    pub fn label(self) -> &'static str {
        match self {
            Self::AppStore => "App Store Connect",
            Self::GooglePlay => "Google Play",
            Self::Expo => "EAS",
        }
    }

    fn credentials_url(self) -> &'static str {
        match self {
            Self::AppStore => APP_STORE_CREDENTIALS_URL,
            Self::GooglePlay => GOOGLE_PLAY_CREDENTIALS_URL,
            Self::Expo => EXPO_CREDENTIALS_URL,
        }
    }
}

#[derive(Clone)]
pub struct AppStoreAccount {
    pub issuer_id: String,
    pub key_id: String,
    pub private_key: Arc<str>,
}

impl AppStoreAccount {
    pub fn credentials(&self) -> app_store_connect::Credentials {
        app_store_connect::Credentials {
            issuer_id: self.issuer_id.clone(),
            key_id: self.key_id.clone(),
            private_key: self.private_key.clone(),
        }
    }
}

#[derive(Clone)]
pub struct GooglePlayAccount {
    pub service_account: ServiceAccount,
    pub json: Arc<str>,
}

#[derive(Clone)]
pub struct ExpoAccount {
    pub token: Arc<str>,
    pub expires_on: Option<NaiveDate>,
}

impl ExpoAccount {
    /// Expo access tokens that were given an expiry warn two weeks ahead.
    pub fn expires_soon(&self, today: NaiveDate) -> Option<i64> {
        let expires_on = self.expires_on?;
        let days = (expires_on - today).num_days();
        (days <= 14).then_some(days)
    }
}

#[derive(Clone, Default)]
pub struct StoredAccounts {
    pub app_store: Option<AppStoreAccount>,
    pub google_play: Option<GooglePlayAccount>,
    pub expo: Option<ExpoAccount>,
}

impl StoredAccounts {
    pub fn is_empty(&self) -> bool {
        self.app_store.is_none() && self.google_play.is_none() && self.expo.is_none()
    }

    pub fn has(&self, kind: AccountKind) -> bool {
        match kind {
            AccountKind::AppStore => self.app_store.is_some(),
            AccountKind::GooglePlay => self.google_play.is_some(),
            AccountKind::Expo => self.expo.is_some(),
        }
    }
}

/// The non-secret half of the accounts.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct AccountIds {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    app_store: Option<AppStoreIds>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expo_expires_on: Option<NaiveDate>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct AppStoreIds {
    issuer_id: String,
    key_id: String,
}

pub enum AccountToSave {
    AppStore(AppStoreAccount),
    GooglePlay(GooglePlayAccount),
    Expo(ExpoAccount),
}

impl AccountToSave {
    pub fn kind(&self) -> AccountKind {
        match self {
            Self::AppStore(_) => AccountKind::AppStore,
            Self::GooglePlay(_) => AccountKind::GooglePlay,
            Self::Expo(_) => AccountKind::Expo,
        }
    }
}

/// Reads and writes the accounts of one scope.
pub trait AccountsSource {
    fn load<'a>(&'a self, cx: &'a AsyncApp) -> LocalBoxFuture<'a, Result<StoredAccounts>>;
    fn save<'a>(
        &'a self,
        account: AccountToSave,
        cx: &'a AsyncApp,
    ) -> LocalBoxFuture<'a, Result<()>>;
    fn forget<'a>(&'a self, kind: AccountKind, cx: &'a AsyncApp) -> LocalBoxFuture<'a, Result<()>>;
}

pub struct ProfileAccounts {
    credentials_provider: Arc<dyn CredentialsProvider>,
    key_value_store: KeyValueStore,
}

/// The accounts a project's releases are read with. Today every project uses the profile's.
pub fn accounts_source(_project: Option<&Entity<Project>>, cx: &App) -> Arc<dyn AccountsSource> {
    Arc::new(ProfileAccounts {
        credentials_provider: zed_credentials_provider::global(cx),
        key_value_store: KeyValueStore::global(cx),
    })
}

impl ProfileAccounts {
    fn read_ids(&self) -> Result<AccountIds> {
        match self.key_value_store.read_kvp(ACCOUNT_IDS_KEY)? {
            Some(text) => Ok(serde_json::from_str(&text).unwrap_or_else(|error| {
                log::error!("Lançamentos: ids de conta ilegíveis: {error}");
                AccountIds::default()
            })),
            None => Ok(AccountIds::default()),
        }
    }

    async fn write_ids(&self, ids: &AccountIds) -> Result<()> {
        self.key_value_store
            .write_kvp(ACCOUNT_IDS_KEY.to_string(), serde_json::to_string(ids)?)
            .await
    }

    async fn read_secret(&self, kind: AccountKind, cx: &AsyncApp) -> Result<Option<String>> {
        let Some((_username, bytes)) = self
            .credentials_provider
            .read_credentials(kind.credentials_url(), cx)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(
            String::from_utf8(bytes).context("Lançamentos: segredo salvo não é UTF-8")?,
        ))
    }
}

impl AccountsSource for ProfileAccounts {
    fn load<'a>(&'a self, cx: &'a AsyncApp) -> LocalBoxFuture<'a, Result<StoredAccounts>> {
        Box::pin(async move {
            let ids = self.read_ids()?;
            let mut accounts = StoredAccounts::default();
            if let (Some(app_store_ids), Some(private_key)) = (
                ids.app_store.clone(),
                self.read_secret(AccountKind::AppStore, cx).await?,
            ) {
                accounts.app_store = Some(AppStoreAccount {
                    issuer_id: app_store_ids.issuer_id,
                    key_id: app_store_ids.key_id,
                    private_key: private_key.into(),
                });
            }
            if let Some(json) = self.read_secret(AccountKind::GooglePlay, cx).await? {
                match ServiceAccount::parse(&json) {
                    Ok(service_account) => {
                        accounts.google_play = Some(GooglePlayAccount {
                            service_account,
                            json: json.into(),
                        })
                    }
                    Err(error) => {
                        log::error!("Lançamentos: conta de serviço salva inválida: {error}")
                    }
                }
            }
            if let Some(token) = self.read_secret(AccountKind::Expo, cx).await? {
                accounts.expo = Some(ExpoAccount {
                    token: token.into(),
                    expires_on: ids.expo_expires_on,
                });
            }
            Ok(accounts)
        })
    }

    fn save<'a>(
        &'a self,
        account: AccountToSave,
        cx: &'a AsyncApp,
    ) -> LocalBoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut ids = self.read_ids()?;
            let kind = account.kind();
            let (username, secret) = match account {
                AccountToSave::AppStore(account) => {
                    ids.app_store = Some(AppStoreIds {
                        issuer_id: account.issuer_id.clone(),
                        key_id: account.key_id.clone(),
                    });
                    (account.key_id, account.private_key)
                }
                AccountToSave::GooglePlay(account) => {
                    (account.service_account.client_email, account.json)
                }
                AccountToSave::Expo(account) => {
                    ids.expo_expires_on = account.expires_on;
                    ("expo-token".to_string(), account.token)
                }
            };
            self.credentials_provider
                .write_credentials(kind.credentials_url(), &username, secret.as_bytes(), cx)
                .await?;
            self.write_ids(&ids).await
        })
    }

    fn forget<'a>(&'a self, kind: AccountKind, cx: &'a AsyncApp) -> LocalBoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.credentials_provider
                .delete_credentials(kind.credentials_url(), cx)
                .await?;
            let mut ids = self.read_ids()?;
            match kind {
                AccountKind::AppStore => ids.app_store = None,
                AccountKind::Expo => ids.expo_expires_on = None,
                AccountKind::GooglePlay => {}
            }
            self.write_ids(&ids).await
        })
    }
}

/// Which app a folder ships, from its Expo config.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppLink {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eas_project_id: Option<String>,
}

impl AppLink {
    pub fn is_linked(&self) -> bool {
        self.bundle_id.is_some() || self.package.is_some() || self.eas_project_id.is_some()
    }

    /// Fields set in `overrides` win; the rest come from the config file.
    pub fn merged_with(&self, overrides: &AppLink) -> AppLink {
        AppLink {
            name: overrides.name.clone().or_else(|| self.name.clone()),
            bundle_id: overrides
                .bundle_id
                .clone()
                .or_else(|| self.bundle_id.clone()),
            package: overrides.package.clone().or_else(|| self.package.clone()),
            eas_project_id: overrides
                .eas_project_id
                .clone()
                .or_else(|| self.eas_project_id.clone()),
        }
    }
}

/// Reads `app.json`, with or without the `expo` wrapper.
pub fn parse_app_json(text: &str) -> Option<AppLink> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let config = value.get("expo").unwrap_or(&value);
    let text_at = |path: &[&str]| -> Option<String> {
        let mut current = config;
        for key in path {
            current = current.get(*key)?;
        }
        current
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let link = AppLink {
        name: text_at(&["name"]),
        bundle_id: text_at(&["ios", "bundleIdentifier"]),
        package: text_at(&["android", "package"]),
        eas_project_id: text_at(&["extra", "eas", "projectId"]),
    };
    link.is_linked().then_some(link)
}

/// `app.config.ts`/`.js` can't be evaluated here, but the ids are almost always literals.
pub fn scan_app_config(text: &str) -> Option<AppLink> {
    let literal_after = |key: &str| -> Option<String> {
        let mut rest = text;
        while let Some(index) = rest.find(key) {
            let after = &rest[index + key.len()..];
            rest = after;
            let after = after.trim_start();
            let after = after
                .strip_prefix('"')
                .or_else(|| after.strip_prefix('\''))
                .map(|after| after.trim_start())
                .unwrap_or(after);
            let Some(after) = after.strip_prefix(':') else {
                continue;
            };
            let after = after.trim_start();
            let quote = after.chars().next()?;
            if !matches!(quote, '"' | '\'' | '`') {
                continue;
            }
            let value: String = after[1..]
                .chars()
                .take_while(|character| *character != quote)
                .collect();
            if !value.is_empty() && !value.contains("${") {
                return Some(value);
            }
        }
        None
    };
    let link = AppLink {
        name: literal_after("name"),
        bundle_id: literal_after("bundleIdentifier"),
        package: literal_after("package"),
        eas_project_id: literal_after("projectId"),
    };
    link.is_linked().then_some(link)
}

fn app_link_key(root: &str) -> String {
    format!("{APP_LINK_KEY_PREFIX}{root}")
}

/// What was typed in "Lojas e EAS" for a project whose config doesn't say.
pub fn load_link_override(store: &KeyValueStore, root: &str) -> AppLink {
    store
        .read_kvp(&app_link_key(root))
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub async fn save_link_override(store: KeyValueStore, root: String, link: AppLink) -> Result<()> {
    if link.is_linked() || link.name.is_some() {
        store
            .write_kvp(app_link_key(&root), serde_json::to_string(&link)?)
            .await
    } else {
        store.delete_kvp(app_link_key(&root)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_app_json() {
        let link = parse_app_json(
            r#"{"expo":{"name":"Trix","ios":{"bundleIdentifier":"br.com.trix.app"},
                "android":{"package":"br.com.trix"},"extra":{"eas":{"projectId":"f3c1"}}}}"#,
        )
        .unwrap();
        assert_eq!(link.name.as_deref(), Some("Trix"));
        assert_eq!(link.bundle_id.as_deref(), Some("br.com.trix.app"));
        assert_eq!(link.package.as_deref(), Some("br.com.trix"));
        assert_eq!(link.eas_project_id.as_deref(), Some("f3c1"));
        assert!(parse_app_json(r#"{"expo":{"name":"Sem ids"}}"#).is_none());
        assert!(parse_app_json("not json").is_none());
    }

    #[test]
    fn scans_app_config_literals() {
        let link = scan_app_config(
            r#"export default {
                name: "Trix",
                ios: { bundleIdentifier: 'br.com.trix.app' },
                android: { package: `br.com.trix` },
                extra: { eas: { "projectId": "f3c1" } },
                version: `${base}.1`,
            };"#,
        )
        .unwrap();
        assert_eq!(link.bundle_id.as_deref(), Some("br.com.trix.app"));
        assert_eq!(link.package.as_deref(), Some("br.com.trix"));
        assert_eq!(link.eas_project_id.as_deref(), Some("f3c1"));
        assert_eq!(link.name.as_deref(), Some("Trix"));
    }

    #[test]
    fn overrides_fill_what_the_config_lacks() {
        let detected = AppLink {
            name: Some("Trix".into()),
            bundle_id: Some("br.com.trix.app".into()),
            package: None,
            eas_project_id: None,
        };
        let overrides = AppLink {
            eas_project_id: Some("f3c1".into()),
            ..AppLink::default()
        };
        let merged = detected.merged_with(&overrides);
        assert_eq!(merged.bundle_id.as_deref(), Some("br.com.trix.app"));
        assert_eq!(merged.eas_project_id.as_deref(), Some("f3c1"));
    }

    #[test]
    fn warns_two_weeks_before_the_expo_token_expires() {
        let account = |expires_on| ExpoAccount {
            token: "t".into(),
            expires_on,
        };
        let today = NaiveDate::from_ymd_opt(2026, 11, 16).unwrap();
        assert_eq!(
            account(NaiveDate::from_ymd_opt(2026, 11, 30)).expires_soon(today),
            Some(14)
        );
        assert_eq!(
            account(NaiveDate::from_ymd_opt(2026, 12, 1)).expires_soon(today),
            None
        );
        assert_eq!(account(None).expires_soon(today), None);
    }
}
