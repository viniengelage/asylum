use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, LazyLock};

use anyhow::{Context as _, Result};
use credentials_provider::CredentialsProvider;
use futures::FutureExt as _;
use gpui::{App, AsyncApp, Global};
use release_channel::ReleaseChannel;

/// An environment variable whose presence indicates that the system keychain
/// should be used in development.
///
/// By default, running Zed in development uses the development credentials
/// provider. Setting this environment variable allows you to interact with the
/// system keychain (for instance, if you need to test something).
///
/// Only works in development. Setting this environment variable in other
/// release channels is a no-op.
static ZED_DEVELOPMENT_USE_KEYCHAIN: LazyLock<bool> = LazyLock::new(|| {
    std::env::var("ZED_DEVELOPMENT_USE_KEYCHAIN").is_ok_and(|value| !value.is_empty())
});

pub struct ZedCredentialsProvider(pub Arc<dyn CredentialsProvider>);

impl Global for ZedCredentialsProvider {}

/// Returns the global [`CredentialsProvider`].
pub fn init_global(cx: &mut App) {
    // The `CredentialsProvider` trait has `Send + Sync` bounds on it, so it
    // seems like this is a false positive from Clippy.
    #[allow(clippy::arc_with_non_send_sync)]
    let provider = new(cx);
    cx.set_global(ZedCredentialsProvider(provider));
}

pub fn global(cx: &App) -> Arc<dyn CredentialsProvider> {
    cx.try_global::<ZedCredentialsProvider>()
        .map(|provider| provider.0.clone())
        .unwrap_or_else(|| new(cx))
}

fn new(cx: &App) -> Arc<dyn CredentialsProvider> {
    let use_development_provider = match ReleaseChannel::try_global(cx) {
        Some(ReleaseChannel::Dev) => {
            // In development we default to using the development
            // credentials provider to avoid getting spammed by relentless
            // keychain access prompts.
            //
            // However, if the `ZED_DEVELOPMENT_USE_KEYCHAIN` environment
            // variable is set, we will use the actual keychain.
            !*ZED_DEVELOPMENT_USE_KEYCHAIN
        }
        Some(ReleaseChannel::Nightly | ReleaseChannel::Preview | ReleaseChannel::Stable) | None => {
            false
        }
    };

    if use_development_provider {
        Arc::new(DevelopmentCredentialsProvider::new())
    } else {
        Arc::new(KeychainCredentialsProvider)
    }
}

/// A credentials provider that stores credentials in the system keychain.
///
/// Keychain entries are keyed by URL alone, so a non-default profile prefixes
/// every URL with its id to keep its credentials apart from the other profiles'.
/// The default profile keeps the bare URLs an installation already has.
struct KeychainCredentialsProvider;

fn keychain_url(url: &str) -> Cow<'_, str> {
    match paths::active_profile_id() {
        Some(profile_id) => Cow::Owned(format!("asylum-profile://{profile_id}/{url}")),
        None => Cow::Borrowed(url),
    }
}

// The keychain can't list entries by prefix, so a non-default profile keeps
// the URLs it wrote to be able to remove them when the profile is deleted.
fn stored_urls_file() -> Option<PathBuf> {
    paths::active_profile_id().map(|_| paths::data_dir().join("credential_urls.json"))
}

fn update_stored_urls(url: &str, stored: bool) -> Result<()> {
    let Some(path) = stored_urls_file() else {
        return Ok(());
    };
    let mut urls: BTreeSet<String> = match std::fs::read(&path) {
        Ok(json) => serde_json::from_slice(&json)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
        Err(error) => return Err(error.into()),
    };
    let changed = if stored {
        urls.insert(url.to_string())
    } else {
        urls.remove(url)
    };
    if changed {
        std::fs::write(&path, serde_json::to_vec_pretty(&urls)?)?;
    }
    Ok(())
}

impl CredentialsProvider for KeychainCredentialsProvider {
    fn read_credentials<'a>(
        &'a self,
        url: &'a str,
        cx: &'a AsyncApp,
    ) -> Pin<Box<dyn Future<Output = Result<Option<(String, Vec<u8>)>>> + 'a>> {
        async move {
            let url = keychain_url(url);
            cx.update(|cx| cx.read_credentials(&url)).await
        }
        .boxed_local()
    }

    fn write_credentials<'a>(
        &'a self,
        url: &'a str,
        username: &'a str,
        password: &'a [u8],
        cx: &'a AsyncApp,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
        async move {
            let url = keychain_url(url);
            cx.update(|cx| cx.write_credentials(&url, username, password))
                .await?;
            update_stored_urls(&url, true).context("failed to record the profile's credential URL")
        }
        .boxed_local()
    }

    fn delete_credentials<'a>(
        &'a self,
        url: &'a str,
        cx: &'a AsyncApp,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
        async move {
            let url = keychain_url(url);
            cx.update(|cx| cx.delete_credentials(&url)).await?;
            update_stored_urls(&url, false).context("failed to forget the profile's credential URL")
        }
        .boxed_local()
    }
}

/// A credentials provider that stores credentials in a local file.
///
/// This MUST only be used in development, as this is not a secure way of storing
/// credentials on user machines.
///
/// Its existence is purely to work around the annoyance of having to constantly
/// re-allow access to the system keychain when developing Zed.
struct DevelopmentCredentialsProvider {
    path: PathBuf,
}

impl DevelopmentCredentialsProvider {
    fn new() -> Self {
        let path = paths::config_dir().join("development_credentials");

        Self { path }
    }

    fn load_credentials(&self) -> Result<HashMap<String, (String, Vec<u8>)>> {
        let json = std::fs::read(&self.path)?;
        let credentials: HashMap<String, (String, Vec<u8>)> = serde_json::from_slice(&json)?;

        Ok(credentials)
    }

    fn save_credentials(&self, credentials: &HashMap<String, (String, Vec<u8>)>) -> Result<()> {
        let json = serde_json::to_string(credentials)?;
        std::fs::write(&self.path, json)?;

        Ok(())
    }
}

impl CredentialsProvider for DevelopmentCredentialsProvider {
    fn read_credentials<'a>(
        &'a self,
        url: &'a str,
        _cx: &'a AsyncApp,
    ) -> Pin<Box<dyn Future<Output = Result<Option<(String, Vec<u8>)>>> + 'a>> {
        async move {
            Ok(self
                .load_credentials()
                .unwrap_or_default()
                .get(url)
                .cloned())
        }
        .boxed_local()
    }

    fn write_credentials<'a>(
        &'a self,
        url: &'a str,
        username: &'a str,
        password: &'a [u8],
        _cx: &'a AsyncApp,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
        async move {
            let mut credentials = self.load_credentials().unwrap_or_default();
            credentials.insert(url.to_string(), (username.to_string(), password.to_vec()));

            self.save_credentials(&credentials)
        }
        .boxed_local()
    }

    fn delete_credentials<'a>(
        &'a self,
        url: &'a str,
        _cx: &'a AsyncApp,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
        async move {
            let mut credentials = self.load_credentials()?;
            credentials.remove(url);

            self.save_credentials(&credentials)
        }
        .boxed_local()
    }
}
