use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use aws_types::os_shim_internal::{Env, Fs};
use base64::Engine as _;
use sha2::{Digest as _, Sha256};
use util::ResultExt as _;

/// The cache keeps no end for an `aws login` session, only the id token issued at sign-in (the
/// SDK carries it over on every refresh); AWS lets the session run for 12 h from there.
pub(crate) const LOGIN_SESSION_LENGTH: Duration = Duration::from_secs(12 * 60 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Validity {
    Until(SystemTime),
    NoExpiry,
}

/// Where `aws login` caches its tokens, as `aws-config` resolves it.
pub(crate) fn login_cache_dir() -> PathBuf {
    match std::env::var_os("AWS_LOGIN_CACHE_DIRECTORY") {
        Some(directory) => PathBuf::from(directory),
        None => util::paths::home_dir().join(".aws/login/cache"),
    }
}

fn sso_cache_dir() -> PathBuf {
    util::paths::home_dir().join(".aws/sso/cache")
}

/// How long each profile's credentials last, for the profiles whose cache says so. Profiles
/// missing from the map have no knowable end (a `credential_process`, an assumed role).
pub(crate) async fn load_validities() -> anyhow::Result<HashMap<String, Validity>> {
    let fs = Fs::real();
    let env = Env::real();
    let profiles = aws_config::profile::load(&fs, &env, &Default::default(), None).await?;
    let login_dir = login_cache_dir();
    let sso_tokens = read_sso_tokens(&sso_cache_dir());
    let mut validities = HashMap::new();
    for name in profiles.profiles() {
        let Some(profile) = profiles.get_profile(name) else {
            continue;
        };
        let validity = if let Some(session) = profile.get("login_session") {
            std::fs::read_to_string(login_dir.join(login_cache_file_name(session)))
                .ok()
                .and_then(|cache| login_session_expiry(&cache))
                .map(Validity::Until)
        } else if let Some(start_url) = profile.get("sso_start_url").or_else(|| {
            profile
                .get("sso_session")
                .and_then(|session| profiles.sso_session(session))
                .and_then(|session| session.get("sso_start_url"))
        }) {
            sso_tokens
                .iter()
                .find_map(|token| sso_token_expiry(token, start_url))
                .map(Validity::Until)
        } else if profile.get("aws_access_key_id").is_some()
            && profile.get("aws_session_token").is_none()
        {
            Some(Validity::NoExpiry)
        } else {
            None
        };
        if let Some(validity) = validity {
            validities.insert(name.to_string(), validity);
        }
    }
    Ok(validities)
}

fn read_sso_tokens(directory: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.log_err())
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .collect()
}

/// `aws-config` names the cache file after the SHA-256 of the profile's `login_session`.
pub(crate) fn login_cache_file_name(login_session: &str) -> String {
    let digest = Sha256::digest(login_session.trim().as_bytes());
    let mut name = String::with_capacity(digest.len() * 2 + 5);
    for byte in digest {
        name.push_str(&format!("{byte:02x}"));
    }
    name.push_str(".json");
    name
}

/// The end of the session behind an `aws login` cache file, from its id token's `iat`.
pub(crate) fn login_session_expiry(cache: &str) -> Option<SystemTime> {
    let cache: serde_json::Value = serde_json::from_str(cache).ok()?;
    let id_token = cache.get("idToken")?.as_str()?;
    let payload = id_token.split('.').nth(1)?;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    let issued_at = claims.get("iat")?.as_u64()?;
    Some(UNIX_EPOCH + Duration::from_secs(issued_at) + LOGIN_SESSION_LENGTH)
}

/// The expiry of an IAM Identity Center token cache file, when it belongs to `start_url`.
pub(crate) fn sso_token_expiry(cache: &str, start_url: &str) -> Option<SystemTime> {
    let cache: serde_json::Value = serde_json::from_str(cache).ok()?;
    if cache.get("startUrl")?.as_str()?.trim_end_matches('/') != start_url.trim_end_matches('/') {
        return None;
    }
    let expires_at = cache.get("expiresAt")?.as_str()?;
    let expires_at = chrono::DateTime::parse_from_rfc3339(expires_at).ok()?;
    let seconds = u64::try_from(expires_at.timestamp()).ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(seconds))
}

/// A fingerprint of the login cache directory that changes whenever `aws login` writes to it.
pub(crate) fn login_cache_fingerprint(directory: &Path) -> Vec<(PathBuf, Option<SystemTime>)> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut fingerprint: Vec<(PathBuf, Option<SystemTime>)> = entries
        .filter_map(|entry| entry.log_err())
        .map(|entry| {
            let modified = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok());
            (entry.path(), modified)
        })
        .collect();
    fingerprint.sort();
    fingerprint
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id_token(claims: &str) -> String {
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!(
            "{}.{}.signature",
            engine.encode(r#"{"alg":"ES256"}"#),
            engine.encode(claims)
        )
    }

    #[test]
    fn login_session_ends_twelve_hours_after_sign_in() {
        let cache = serde_json::json!({
            "accessToken": { "expiresAt": "2026-06-12T14:57:49Z" },
            "idToken": id_token(r#"{"iat":1781275369,"exp":1781276269}"#),
        })
        .to_string();
        assert_eq!(
            login_session_expiry(&cache),
            Some(UNIX_EPOCH + Duration::from_secs(1_781_275_369 + 12 * 60 * 60))
        );
        assert_eq!(login_session_expiry("{}"), None);
        assert_eq!(login_session_expiry("not json"), None);
    }

    #[test]
    fn login_cache_file_is_named_after_the_session_hash() {
        assert_eq!(
            login_cache_file_name(" abc\n"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad.json"
        );
    }

    #[test]
    fn sso_token_matches_its_start_url() {
        let cache = serde_json::json!({
            "startUrl": "https://company.awsapps.com/start/",
            "expiresAt": "2026-11-17T15:00:00Z",
        })
        .to_string();
        assert_eq!(
            sso_token_expiry(&cache, "https://company.awsapps.com/start"),
            Some(UNIX_EPOCH + Duration::from_secs(1_794_927_600))
        );
        assert_eq!(
            sso_token_expiry(&cache, "https://other.awsapps.com/start"),
            None
        );
    }
}
