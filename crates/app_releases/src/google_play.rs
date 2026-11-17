//! Google Play's Android Publisher API (v3) with a service account. Reading the tracks needs an
//! edit; read-only callers always delete it instead of committing.

use crate::ApiError;
use anyhow::{Context as _, Result, anyhow};
use chrono::Utc;
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, Method, Request};
use i18n::t;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use util::ResultExt as _;

pub const API_URL: &str = "https://androidpublisher.googleapis.com/androidpublisher/v3";
const SCOPE: &str = "https://www.googleapis.com/auth/androidpublisher";
const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const ASSERTION_LIFETIME_SECONDS: i64 = 60 * 60;
pub const PRODUCTION_TRACK: &str = "production";

/// The parts of the service account JSON the token exchange needs.
#[derive(Clone, Deserialize)]
pub struct ServiceAccount {
    pub client_email: String,
    private_key: String,
    #[serde(default)]
    token_uri: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
}

impl std::fmt::Debug for ServiceAccount {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServiceAccount")
            .field("client_email", &self.client_email)
            .finish_non_exhaustive()
    }
}

impl ServiceAccount {
    /// Checks the JSON and its key before anything is saved or sent.
    pub fn parse(json: &str) -> Result<Self> {
        let account: Self = serde_json::from_str(json.trim())
            .map_err(|_| anyhow!("{}", t!("app_releases.accounts.invalid_service_account")))?;
        EncodingKey::from_rsa_pem(account.private_key.as_bytes())
            .map_err(|_| anyhow!("{}", t!("app_releases.accounts.invalid_service_account")))?;
        Ok(account)
    }

    fn token_uri(&self) -> &str {
        self.token_uri.as_deref().unwrap_or(DEFAULT_TOKEN_URI)
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct AssertionClaims {
    iss: String,
    scope: String,
    aud: String,
    iat: i64,
    exp: i64,
}

fn assertion_claims(account: &ServiceAccount, now: i64) -> AssertionClaims {
    AssertionClaims {
        iss: account.client_email.clone(),
        scope: SCOPE.to_string(),
        aud: account.token_uri().to_string(),
        iat: now,
        exp: now + ASSERTION_LIFETIME_SECONDS,
    }
}

fn assertion(account: &ServiceAccount, now: i64) -> Result<String> {
    let key = EncodingKey::from_rsa_pem(account.private_key.as_bytes())
        .map_err(|_| anyhow!("{}", t!("app_releases.accounts.invalid_service_account")))?;
    jsonwebtoken::encode(
        &Header::new(Algorithm::RS256),
        &assertion_claims(account, now),
        &key,
    )
    .map_err(|_| anyhow!("{}", t!("app_releases.accounts.invalid_service_account")))
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

/// Exchanges a signed assertion for an access token (OAuth 2 JWT bearer grant).
pub async fn access_token(
    client: &Arc<dyn HttpClient>,
    account: &ServiceAccount,
) -> Result<String> {
    let assertion = assertion(account, Utc::now().timestamp())?;
    let form = format!(
        "grant_type={}&assertion={}",
        urlencoding::encode("urn:ietf:params:oauth:grant-type:jwt-bearer"),
        urlencoding::encode(&assertion)
    );
    let request = Request::builder()
        .method(Method::POST)
        .uri(account.token_uri())
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(AsyncBody::from(form))?;
    let mut response = client
        .send(request)
        .await
        .with_context(|| t!("app_releases.api.request_failed", service = "Google"))?;
    let mut text = String::new();
    response.body_mut().read_to_string(&mut text).await?;
    let status = response.status().as_u16();
    // A revoked or deleted service account key answers 400 `invalid_grant`.
    if status == 400 || status == 401 || status == 403 {
        return Err(ApiError::Unauthorized.into());
    }
    if !(200..300).contains(&status) {
        return Err(ApiError::Status { status, body: text }.into());
    }
    let token: TokenResponse = serde_json::from_str(&text)
        .with_context(|| t!("app_releases.api.unexpected_response", service = "Google"))?;
    Ok(token.access_token)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LocalizedText {
    pub language: String,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackRelease {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub version_codes: Vec<String>,
    /// `completed`, `inProgress`, `halted` or `draft`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_fraction: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub release_notes: Vec<LocalizedText>,
    /// Fields Asylum doesn't read (country targeting, update priority…), kept so writing the
    /// track back doesn't drop them.
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

impl TrackRelease {
    /// Play names releases `128 (2.4.0)` by default; the version name is what's in parentheses.
    pub fn version_name(&self) -> Option<String> {
        let name = self.name.as_deref()?.trim();
        if let (Some(start), Some(end)) = (name.rfind('('), name.rfind(')'))
            && start < end
        {
            let inside = name[start + 1..end].trim();
            if !inside.is_empty() {
                return Some(inside.to_string());
            }
        }
        (!name.is_empty()).then(|| name.to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub track: String,
    #[serde(default)]
    pub releases: Vec<TrackRelease>,
}

#[derive(Deserialize)]
struct TracksResponse {
    #[serde(default)]
    tracks: Vec<Track>,
}

#[derive(Deserialize)]
struct EditResponse {
    id: String,
}

pub fn parse_tracks(body: &str) -> Result<Vec<Track>> {
    let response: TracksResponse = serde_json::from_str(body).with_context(|| {
        t!(
            "app_releases.api.unexpected_response",
            service = "Google Play"
        )
    })?;
    Ok(response.tracks)
}

pub fn console_url(package: &str) -> String {
    format!(
        "https://play.google.com/console/developers/app/{}/tracks",
        urlencoding::encode(package)
    )
}

pub(crate) async fn send(
    client: &Arc<dyn HttpClient>,
    access_token: &str,
    method: Method,
    path: &str,
    body: Option<String>,
) -> Result<String> {
    let builder = Request::builder()
        .method(method)
        .uri(format!("{API_URL}{path}"))
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Accept", "application/json");
    let request = match body {
        Some(body) => builder
            .header("Content-Type", "application/json")
            .body(AsyncBody::from(body))?,
        None => builder.body(AsyncBody::empty())?,
    };
    let mut response = client
        .send(request)
        .await
        .with_context(|| t!("app_releases.api.request_failed", service = "Google Play"))?;
    let mut text = String::new();
    response.body_mut().read_to_string(&mut text).await?;
    match response.status().as_u16() {
        401 | 403 => Err(ApiError::Unauthorized.into()),
        429 => Err(ApiError::RateLimited.into()),
        status if (200..300).contains(&status) => Ok(text),
        status => Err(ApiError::Status {
            status,
            body: first_error_message(&text).unwrap_or(text),
        }
        .into()),
    }
}

fn first_error_message(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value
        .get("error")?
        .get("message")?
        .as_str()
        .map(str::to_string)
}

pub(crate) async fn insert_edit(
    client: &Arc<dyn HttpClient>,
    access_token: &str,
    package: &str,
) -> Result<String> {
    let body = send(
        client,
        access_token,
        Method::POST,
        &format!("/applications/{}/edits", urlencoding::encode(package)),
        Some("{}".to_string()),
    )
    .await?;
    let edit: EditResponse = serde_json::from_str(&body).with_context(|| {
        t!(
            "app_releases.api.unexpected_response",
            service = "Google Play"
        )
    })?;
    Ok(edit.id)
}

pub(crate) async fn delete_edit(
    client: &Arc<dyn HttpClient>,
    access_token: &str,
    package: &str,
    edit_id: &str,
) -> Result<()> {
    send(
        client,
        access_token,
        Method::DELETE,
        &format!(
            "/applications/{}/edits/{edit_id}",
            urlencoding::encode(package)
        ),
        None,
    )
    .await?;
    Ok(())
}

/// The releases of every track, through an edit that is deleted afterwards whatever happens.
pub async fn fetch_tracks(
    client: &Arc<dyn HttpClient>,
    account: &ServiceAccount,
    package: &str,
) -> Result<Vec<Track>> {
    let access_token = access_token(client, account).await?;
    let edit_id = insert_edit(client, &access_token, package).await?;
    let result = send(
        client,
        &access_token,
        Method::GET,
        &format!(
            "/applications/{}/edits/{edit_id}/tracks",
            urlencoding::encode(package)
        ),
        None,
    )
    .await
    .and_then(|body| parse_tracks(&body));
    delete_edit(client, &access_token, package, &edit_id)
        .await
        .context("Google Play: falha ao descartar a edição de leitura")
        .log_err();
    result
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const TRACKS_FIXTURE: &str = r#"{
      "kind": "androidpublisher#tracksListResponse",
      "tracks": [
        {"track": "production", "releases": [
          {"name": "127 (2.3.9)", "versionCodes": ["127"], "status": "inProgress", "userFraction": 0.2,
           "releaseNotes": [{"language": "pt-BR", "text": "Melhorias no extrato"}],
           "countryTargeting": {"countries": ["BR"]}}
        ]},
        {"track": "internal", "releases": [
          {"name": "128 (2.4.0)", "versionCodes": ["128"], "status": "completed",
           "releaseNotes": [{"language": "pt-BR", "text": "Pix agendado"}, {"language": "en-US", "text": "Scheduled Pix"}]}
        ]},
        {"track": "beta", "releases": []}
      ]
    }"#;

    #[test]
    fn parses_tracks_and_keeps_unknown_fields() {
        let tracks = parse_tracks(TRACKS_FIXTURE).unwrap();
        assert_eq!(tracks.len(), 3);
        let production = &tracks[0].releases[0];
        assert_eq!(production.user_fraction, Some(0.2));
        assert_eq!(production.version_name().as_deref(), Some("2.3.9"));
        let written = serde_json::to_value(production).unwrap();
        assert_eq!(written["countryTargeting"]["countries"][0], "BR");
        assert_eq!(tracks[1].releases[0].release_notes.len(), 2);
    }

    #[test]
    fn version_name_falls_back_to_the_release_name() {
        let release = |name: &str| TrackRelease {
            name: Some(name.to_string()),
            version_codes: vec!["1".into()],
            status: "completed".into(),
            user_fraction: None,
            release_notes: Vec::new(),
            other: Default::default(),
        };
        assert_eq!(release("2.4.0").version_name().as_deref(), Some("2.4.0"));
        assert_eq!(
            release("Release 128 (2.4.0-rc1)").version_name().as_deref(),
            Some("2.4.0-rc1")
        );
    }

    #[test]
    fn checks_the_service_account_json() {
        assert!(ServiceAccount::parse("{}").is_err());
        assert!(
            ServiceAccount::parse(
                r#"{"client_email":"ci@trix.iam.gserviceaccount.com","private_key":"nope"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn asks_for_the_publisher_scope() {
        let account: ServiceAccount = serde_json::from_str(
            r#"{"client_email":"ci@trix.iam.gserviceaccount.com","private_key":"x","token_uri":"https://oauth2.googleapis.com/token"}"#,
        )
        .unwrap();
        let claims = assertion_claims(&account, 1_800_000_000);
        assert_eq!(claims.iss, "ci@trix.iam.gserviceaccount.com");
        assert_eq!(claims.scope, SCOPE);
        assert_eq!(claims.aud, "https://oauth2.googleapis.com/token");
        assert_eq!(claims.exp - claims.iat, 3600);
    }
}
