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

/// A change to one track, applied through an edit that is committed only if every step works.
#[derive(Clone, Debug, PartialEq)]
pub enum TrackChange {
    /// Puts a version code from one track on another, keeping its notes.
    Promote {
        from_track: String,
        to_track: String,
        version_code: String,
        /// Below 1.0 this is a staged rollout.
        user_fraction: Option<f64>,
    },
    SetRollout {
        track: String,
        version_code: String,
        user_fraction: f64,
    },
    Halt {
        track: String,
        version_code: String,
    },
    Resume {
        track: String,
        version_code: String,
    },
    SetNotes {
        track: String,
        version_code: String,
        notes: Vec<LocalizedText>,
    },
}

impl TrackChange {
    pub fn track(&self) -> &str {
        match self {
            Self::Promote { to_track, .. } => to_track,
            Self::SetRollout { track, .. }
            | Self::Halt { track, .. }
            | Self::Resume { track, .. }
            | Self::SetNotes { track, .. } => track,
        }
    }
}

fn find_release<'a>(track: &'a Track, version_code: &str) -> Option<&'a TrackRelease> {
    track.releases.iter().find(|release| {
        release
            .version_codes
            .iter()
            .any(|code| code == version_code)
    })
}

fn release_mut<'a>(track: &'a mut Track, version_code: &str) -> Result<&'a mut TrackRelease> {
    let track_name = track.track.clone();
    track
        .releases
        .iter_mut()
        .find(|release| {
            release
                .version_codes
                .iter()
                .any(|code| code == version_code)
        })
        .ok_or_else(|| {
            anyhow!(
                "{}",
                t!(
                    "app_releases.play.not_in_track",
                    version_code = version_code,
                    track = track_name
                )
            )
        })
}

/// Releases that stay on the track next to a staged rollout: users outside the rollout keep
/// getting the last completed version.
fn completed_except(track: &Track, version_code: &str) -> Vec<TrackRelease> {
    track
        .releases
        .iter()
        .filter(|release| {
            release.status == "completed"
                && !release
                    .version_codes
                    .iter()
                    .any(|code| code == version_code)
        })
        .cloned()
        .collect()
}

/// The track as it should be written back after `change`. Pure, so every rule is tested.
pub fn apply_change(tracks: &[Track], change: &TrackChange) -> Result<Track> {
    let find_track = |name: &str| -> Track {
        tracks
            .iter()
            .find(|track| track.track == name)
            .cloned()
            .unwrap_or_else(|| Track {
                track: name.to_string(),
                releases: Vec::new(),
            })
    };
    match change {
        TrackChange::Promote {
            from_track,
            to_track,
            version_code,
            user_fraction,
        } => {
            let source = find_track(from_track);
            let source_release = find_release(&source, version_code).ok_or_else(|| {
                anyhow!(
                    "{}",
                    t!(
                        "app_releases.play.not_in_track",
                        version_code = version_code,
                        track = from_track
                    )
                )
            })?;
            let mut target = find_track(to_track);
            let staged = user_fraction.is_some_and(|fraction| fraction < 1.0);
            let mut releases = if staged {
                completed_except(&target, version_code)
            } else {
                Vec::new()
            };
            releases.push(TrackRelease {
                name: source_release.name.clone(),
                version_codes: vec![version_code.clone()],
                status: if staged { "inProgress" } else { "completed" }.to_string(),
                user_fraction: if staged { *user_fraction } else { None },
                release_notes: source_release.release_notes.clone(),
                other: Default::default(),
            });
            target.releases = releases;
            Ok(target)
        }
        TrackChange::SetRollout {
            track,
            version_code,
            user_fraction,
        } => {
            let mut target = find_track(track);
            if *user_fraction >= 1.0 {
                target.releases.retain(|release| {
                    release
                        .version_codes
                        .iter()
                        .any(|code| code == version_code)
                        || release.status != "completed"
                });
                let release = release_mut(&mut target, version_code)?;
                release.status = "completed".to_string();
                release.user_fraction = None;
            } else if *user_fraction <= 0.0 {
                return Err(anyhow!("{}", t!("app_releases.play.invalid_fraction")));
            } else {
                let release = release_mut(&mut target, version_code)?;
                release.status = "inProgress".to_string();
                release.user_fraction = Some(*user_fraction);
            }
            Ok(target)
        }
        TrackChange::Halt {
            track,
            version_code,
        } => {
            let mut target = find_track(track);
            release_mut(&mut target, version_code)?.status = "halted".to_string();
            Ok(target)
        }
        TrackChange::Resume {
            track,
            version_code,
        } => {
            let mut target = find_track(track);
            let release = release_mut(&mut target, version_code)?;
            release.status = if release.user_fraction.is_some() {
                "inProgress"
            } else {
                "completed"
            }
            .to_string();
            Ok(target)
        }
        TrackChange::SetNotes {
            track,
            version_code,
            notes,
        } => {
            let mut target = find_track(track);
            release_mut(&mut target, version_code)?.release_notes = notes.clone();
            Ok(target)
        }
    }
}

pub async fn update_track(
    client: &Arc<dyn HttpClient>,
    account: &ServiceAccount,
    package: &str,
    change: &TrackChange,
) -> Result<()> {
    let access_token = access_token(client, account).await?;
    update_track_with_token(client, &access_token, package, change).await
}

/// Applies `change` in a new edit and commits it. Any failure deletes the edit, so a half-made
/// change never stays pending in the Play Console.
async fn update_track_with_token(
    client: &Arc<dyn HttpClient>,
    access_token: &str,
    package: &str,
    change: &TrackChange,
) -> Result<()> {
    let edit_id = insert_edit(client, access_token, package).await?;
    let encoded_package = urlencoding::encode(package).to_string();
    let result = async {
        let tracks = parse_tracks(
            &send(
                client,
                access_token,
                Method::GET,
                &format!("/applications/{encoded_package}/edits/{edit_id}/tracks"),
                None,
            )
            .await?,
        )?;
        let track = apply_change(&tracks, change)?;
        send(
            client,
            access_token,
            Method::PUT,
            &format!(
                "/applications/{encoded_package}/edits/{edit_id}/tracks/{}",
                urlencoding::encode(change.track())
            ),
            Some(serde_json::to_string(&track)?),
        )
        .await?;
        send(
            client,
            access_token,
            Method::POST,
            &format!("/applications/{encoded_package}/edits/{edit_id}:commit"),
            None,
        )
        .await?;
        anyhow::Ok(())
    }
    .await;
    if result.is_err() {
        delete_edit(client, access_token, package, &edit_id)
            .await
            .context("Google Play: falha ao descartar a edição")
            .log_err();
    }
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

    #[test]
    fn promoting_to_a_staged_rollout_keeps_the_completed_release() {
        let mut tracks = parse_tracks(TRACKS_FIXTURE).unwrap();
        tracks[0].releases[0].status = "completed".into();
        tracks[0].releases[0].user_fraction = None;
        let track = apply_change(
            &tracks,
            &TrackChange::Promote {
                from_track: "internal".into(),
                to_track: "production".into(),
                version_code: "128".into(),
                user_fraction: Some(0.1),
            },
        )
        .unwrap();
        assert_eq!(track.track, "production");
        assert_eq!(track.releases.len(), 2);
        assert_eq!(track.releases[0].version_codes, ["127"]);
        let promoted = &track.releases[1];
        assert_eq!(promoted.status, "inProgress");
        assert_eq!(promoted.user_fraction, Some(0.1));
        assert_eq!(promoted.release_notes.len(), 2, "notes come along");
    }

    #[test]
    fn promoting_fully_replaces_the_track() {
        let tracks = parse_tracks(TRACKS_FIXTURE).unwrap();
        let track = apply_change(
            &tracks,
            &TrackChange::Promote {
                from_track: "internal".into(),
                to_track: "beta".into(),
                version_code: "128".into(),
                user_fraction: None,
            },
        )
        .unwrap();
        assert_eq!(track.releases.len(), 1);
        assert_eq!(track.releases[0].status, "completed");
        assert!(track.releases[0].user_fraction.is_none());
    }

    #[test]
    fn promoting_a_version_the_track_lacks_fails() {
        let tracks = parse_tracks(TRACKS_FIXTURE).unwrap();
        let result = apply_change(
            &tracks,
            &TrackChange::Promote {
                from_track: "beta".into(),
                to_track: "production".into(),
                version_code: "128".into(),
                user_fraction: None,
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn changes_halts_and_resumes_the_rollout() {
        let tracks = parse_tracks(TRACKS_FIXTURE).unwrap();
        let raised = apply_change(
            &tracks,
            &TrackChange::SetRollout {
                track: "production".into(),
                version_code: "127".into(),
                user_fraction: 0.5,
            },
        )
        .unwrap();
        assert_eq!(raised.releases[0].user_fraction, Some(0.5));
        assert_eq!(
            serde_json::to_value(&raised.releases[0]).unwrap()["countryTargeting"]["countries"][0],
            "BR",
            "fields Asylum doesn't know survive the write"
        );

        let full = apply_change(
            &tracks,
            &TrackChange::SetRollout {
                track: "production".into(),
                version_code: "127".into(),
                user_fraction: 1.0,
            },
        )
        .unwrap();
        assert_eq!(full.releases[0].status, "completed");
        assert!(full.releases[0].user_fraction.is_none());

        let halted = apply_change(
            &tracks,
            &TrackChange::Halt {
                track: "production".into(),
                version_code: "127".into(),
            },
        )
        .unwrap();
        assert_eq!(halted.releases[0].status, "halted");
        let resumed = apply_change(
            &[halted],
            &TrackChange::Resume {
                track: "production".into(),
                version_code: "127".into(),
            },
        )
        .unwrap();
        assert_eq!(resumed.releases[0].status, "inProgress");

        assert!(
            apply_change(
                &tracks,
                &TrackChange::SetRollout {
                    track: "production".into(),
                    version_code: "127".into(),
                    user_fraction: 0.0,
                },
            )
            .is_err()
        );
    }

    #[gpui::test]
    async fn a_failed_update_deletes_the_edit() {
        let deleted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let client: Arc<dyn HttpClient> = http_client::FakeHttpClient::create({
            let deleted = deleted.clone();
            move |request| {
                let deleted = deleted.clone();
                async move {
                    let path = request.uri().path().to_string();
                    let method = request.method().clone();
                    let body = if path.ends_with("/token") {
                        r#"{"access_token":"ya29"}"#.to_string()
                    } else if method == Method::POST && path.ends_with("/edits") {
                        r#"{"id":"edit-1"}"#.to_string()
                    } else if method == Method::GET && path.ends_with("/tracks") {
                        TRACKS_FIXTURE.to_string()
                    } else if method == Method::PUT {
                        return Ok(http_client::Response::builder()
                            .status(400)
                            .body(r#"{"error":{"message":"Track is locked"}}"#.into())?);
                    } else if method == Method::DELETE {
                        deleted.store(true, std::sync::atomic::Ordering::SeqCst);
                        String::new()
                    } else {
                        panic!("unexpected {method} {path}");
                    };
                    Ok(http_client::Response::builder()
                        .status(200)
                        .body(body.into())?)
                }
            }
        });
        let error = update_track_with_token(
            &client,
            "ya29",
            "br.com.trix",
            &TrackChange::Halt {
                track: "production".into(),
                version_code: "127".into(),
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<crate::ApiError>(),
            Some(crate::ApiError::Status { status: 400, body }) if body == "Track is locked"
        ));
        assert!(deleted.load(std::sync::atomic::Ordering::SeqCst));
    }
}
