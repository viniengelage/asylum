//! App Store Connect's REST API, authenticated with a short-lived ES256 token signed by the
//! team's `.p8` API key.

use crate::ApiError;
use anyhow::{Context as _, Result, anyhow};
use chrono::{DateTime, Utc};
use collections::HashMap;
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, Method, Request};
use i18n::t;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const API_URL: &str = "https://api.appstoreconnect.apple.com";
const AUDIENCE: &str = "appstoreconnect-v1";
/// Apple refuses tokens that live longer than 20 minutes.
const TOKEN_LIFETIME_SECONDS: i64 = 15 * 60;
const BUILDS_LIMIT: usize = 20;
const VERSIONS_LIMIT: usize = 10;

#[derive(Clone)]
pub struct Credentials {
    pub issuer_id: String,
    pub key_id: String,
    pub private_key: Arc<str>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("issuer_id", &self.issuer_id)
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct Claims {
    iss: String,
    iat: i64,
    exp: i64,
    aud: String,
}

/// Fails when the key is not an EC private key, so a wrong file is caught before any request.
pub fn signing_key(private_key: &str) -> Result<EncodingKey> {
    EncodingKey::from_ec_pem(private_key.trim().as_bytes())
        .map_err(|_| anyhow!("{}", t!("app_releases.accounts.invalid_p8")))
}

pub fn token(credentials: &Credentials, now: i64) -> Result<String> {
    let key = signing_key(&credentials.private_key)?;
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some(credentials.key_id.trim().to_string());
    header.typ = Some("JWT".to_string());
    let claims = Claims {
        iss: credentials.issuer_id.trim().to_string(),
        iat: now,
        exp: now + TOKEN_LIFETIME_SECONDS,
        aud: AUDIENCE.to_string(),
    };
    jsonwebtoken::encode(&header, &claims, &key)
        .map_err(|_| anyhow!("{}", t!("app_releases.accounts.invalid_p8")))
}

#[derive(Clone, Debug, PartialEq)]
pub struct App {
    pub id: String,
    pub name: String,
    pub bundle_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessingState {
    Processing,
    Failed,
    Invalid,
    Valid,
    Unknown,
}

impl ProcessingState {
    fn parse(value: &str) -> Self {
        match value {
            "PROCESSING" => Self::Processing,
            "FAILED" => Self::Failed,
            "INVALID" => Self::Invalid,
            "VALID" => Self::Valid,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BetaGroup {
    pub id: String,
    pub name: String,
    pub is_internal: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Build {
    pub id: String,
    /// The build number (`CFBundleVersion`).
    pub build_number: String,
    /// The marketing version (`CFBundleShortVersionString`), from the pre-release version.
    pub version: Option<String>,
    pub processing_state: ProcessingState,
    pub uploaded_at: Option<DateTime<Utc>>,
    pub expired: bool,
    /// e.g. `IN_BETA_TESTING`, `WAITING_FOR_BETA_REVIEW`, `BETA_REJECTED`.
    pub external_state: Option<String>,
    pub internal_state: Option<String>,
    pub groups: Vec<BetaGroup>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum PhasedReleaseState {
    Inactive,
    Active,
    Paused,
    Complete,
    Unknown,
}

impl PhasedReleaseState {
    fn parse(value: &str) -> Self {
        match value {
            "INACTIVE" => Self::Inactive,
            "ACTIVE" => Self::Active,
            "PAUSED" => Self::Paused,
            "COMPLETE" => Self::Complete,
            _ => Self::Unknown,
        }
    }

    pub fn as_api(self) -> &'static str {
        match self {
            Self::Inactive => "INACTIVE",
            Self::Active => "ACTIVE",
            Self::Paused => "PAUSED",
            Self::Complete | Self::Unknown => "COMPLETE",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PhasedRelease {
    pub id: String,
    pub state: PhasedReleaseState,
    pub current_day: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Version {
    pub id: String,
    pub version: String,
    /// `appStoreState`, e.g. `WAITING_FOR_REVIEW`, `REJECTED`, `READY_FOR_SALE`.
    pub state: String,
    pub build_id: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub phased_release: Option<PhasedRelease>,
}

#[derive(Deserialize)]
struct Document {
    data: DocumentData,
    #[serde(default)]
    included: Vec<Resource>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum DocumentData {
    Many(Vec<Resource>),
    One(Resource),
}

impl DocumentData {
    fn into_vec(self) -> Vec<Resource> {
        match self {
            Self::Many(resources) => resources,
            Self::One(resource) => vec![resource],
        }
    }
}

#[derive(Deserialize)]
struct Resource {
    #[serde(rename = "type")]
    kind: String,
    id: String,
    #[serde(default)]
    attributes: serde_json::Value,
    #[serde(default)]
    relationships: HashMap<String, Relationship>,
}

impl Resource {
    fn text(&self, name: &str) -> Option<String> {
        self.attributes
            .get(name)
            .and_then(|value| value.as_str())
            .map(str::to_string)
    }

    fn related(&self, name: &str) -> Vec<&Linkage> {
        match self
            .relationships
            .get(name)
            .and_then(|relationship| relationship.data.as_ref())
        {
            Some(RelationshipData::One(linkage)) => vec![linkage],
            Some(RelationshipData::Many(linkages)) => linkages.iter().collect(),
            None => Vec::new(),
        }
    }
}

#[derive(Deserialize)]
struct Relationship {
    #[serde(default)]
    data: Option<RelationshipData>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RelationshipData {
    Many(Vec<Linkage>),
    One(Linkage),
}

#[derive(Deserialize)]
struct Linkage {
    #[serde(rename = "type")]
    kind: String,
    id: String,
}

fn parse_document(body: &str) -> Result<Document> {
    serde_json::from_str(body).with_context(|| {
        t!(
            "app_releases.api.unexpected_response",
            service = "App Store Connect"
        )
    })
}

fn find_included<'a>(document: &'a Document, linkage: &Linkage) -> Option<&'a Resource> {
    document
        .included
        .iter()
        .find(|resource| resource.kind == linkage.kind && resource.id == linkage.id)
}

pub fn parse_apps(body: &str) -> Result<Vec<App>> {
    let document = parse_document(body)?;
    Ok(document
        .data
        .into_vec()
        .into_iter()
        .map(|resource| App {
            name: resource.text("name").unwrap_or_default(),
            bundle_id: resource.text("bundleId").unwrap_or_default(),
            id: resource.id,
        })
        .collect())
}

fn parse_group(resource: &Resource) -> BetaGroup {
    BetaGroup {
        id: resource.id.clone(),
        name: resource.text("name").unwrap_or_default(),
        is_internal: resource
            .attributes
            .get("isInternalGroup")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
    }
}

pub fn parse_beta_groups(body: &str) -> Result<Vec<BetaGroup>> {
    let document = parse_document(body)?;
    Ok(document.data.into_vec().iter().map(parse_group).collect())
}

pub fn parse_builds(body: &str) -> Result<Vec<Build>> {
    let document = parse_document(body)?;
    let resources = match &document.data {
        DocumentData::Many(resources) => resources.iter().collect::<Vec<_>>(),
        DocumentData::One(resource) => vec![resource],
    };
    Ok(resources
        .into_iter()
        .map(|resource| {
            let version = resource
                .related("preReleaseVersion")
                .first()
                .and_then(|linkage| find_included(&document, linkage))
                .and_then(|pre_release| pre_release.text("version"));
            let beta_detail = resource
                .related("buildBetaDetail")
                .first()
                .and_then(|linkage| find_included(&document, linkage));
            let groups = resource
                .related("betaGroups")
                .into_iter()
                .filter_map(|linkage| find_included(&document, linkage))
                .map(parse_group)
                .collect();
            Build {
                id: resource.id.clone(),
                build_number: resource.text("version").unwrap_or_default(),
                version,
                processing_state: ProcessingState::parse(
                    &resource.text("processingState").unwrap_or_default(),
                ),
                uploaded_at: resource
                    .text("uploadedDate")
                    .and_then(|date| DateTime::parse_from_rfc3339(&date).ok())
                    .map(|date| date.with_timezone(&Utc)),
                expired: resource
                    .attributes
                    .get("expired")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false),
                external_state: beta_detail.and_then(|detail| detail.text("externalBuildState")),
                internal_state: beta_detail.and_then(|detail| detail.text("internalBuildState")),
                groups,
            }
        })
        .collect())
}

pub fn parse_versions(body: &str) -> Result<Vec<Version>> {
    let document = parse_document(body)?;
    let resources = match &document.data {
        DocumentData::Many(resources) => resources.iter().collect::<Vec<_>>(),
        DocumentData::One(resource) => vec![resource],
    };
    Ok(resources
        .into_iter()
        .map(|resource| {
            let phased_release = resource
                .related("appStoreVersionPhasedRelease")
                .first()
                .and_then(|linkage| find_included(&document, linkage))
                .map(|phased| PhasedRelease {
                    id: phased.id.clone(),
                    state: PhasedReleaseState::parse(
                        &phased.text("phasedReleaseState").unwrap_or_default(),
                    ),
                    current_day: phased
                        .attributes
                        .get("currentDayNumber")
                        .and_then(|value| value.as_u64())
                        .and_then(|day| u32::try_from(day).ok()),
                });
            Version {
                id: resource.id.clone(),
                version: resource.text("versionString").unwrap_or_default(),
                state: resource
                    .text("appStoreState")
                    .or_else(|| resource.text("appVersionState"))
                    .unwrap_or_default(),
                build_id: resource
                    .related("build")
                    .first()
                    .map(|linkage| linkage.id.clone()),
                created_at: resource
                    .text("createdDate")
                    .and_then(|date| DateTime::parse_from_rfc3339(&date).ok())
                    .map(|date| date.with_timezone(&Utc)),
                phased_release,
            }
        })
        .collect())
}

/// Where Apple shows the review messages: the API has no text for a rejection.
pub fn resolution_center_url(app_id: &str) -> String {
    format!("https://appstoreconnect.apple.com/apps/{app_id}/distribution/reviewsubmissions")
}

pub fn testflight_url(app_id: &str) -> String {
    format!("https://appstoreconnect.apple.com/apps/{app_id}/testflight/ios")
}

pub(crate) async fn send(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    method: Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<String> {
    let token = token(credentials, Utc::now().timestamp())?;
    let builder = Request::builder()
        .method(method)
        .uri(format!("{API_URL}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json");
    let request = match body {
        Some(body) => builder
            .header("Content-Type", "application/json")
            .body(AsyncBody::from(serde_json::to_string(&body)?))?,
        None => builder.body(AsyncBody::empty())?,
    };
    let mut response = client.send(request).await.with_context(|| {
        t!(
            "app_releases.api.request_failed",
            service = "App Store Connect"
        )
    })?;
    let mut text = String::new();
    response.body_mut().read_to_string(&mut text).await?;
    match response.status().as_u16() {
        401 | 403 => Err(ApiError::Unauthorized.into()),
        429 => Err(ApiError::RateLimited.into()),
        status if (200..300).contains(&status) => Ok(text),
        status => Err(ApiError::Status {
            status,
            body: first_error_detail(&text).unwrap_or(text),
        }
        .into()),
    }
}

/// Apple's errors come as `{"errors": [{"title", "detail"}]}`; the detail is what explains it.
fn first_error_detail(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let error = value.get("errors")?.as_array()?.first()?;
    error
        .get("detail")
        .or_else(|| error.get("title"))
        .and_then(|text| text.as_str())
        .map(str::to_string)
}

async fn get(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    path: &str,
) -> Result<String> {
    send(client, credentials, Method::GET, path, None).await
}

pub async fn find_app(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    bundle_id: &str,
) -> Result<Option<App>> {
    let path = format!(
        "/v1/apps?filter[bundleId]={}&fields[apps]=name,bundleId&limit=1",
        urlencoding::encode(bundle_id)
    );
    let body = get(client, credentials, &path).await?;
    Ok(parse_apps(&body)?
        .into_iter()
        .find(|app| app.bundle_id == bundle_id))
}

/// Any request that only a valid key can make, for the connection test.
pub async fn check(client: &Arc<dyn HttpClient>, credentials: &Credentials) -> Result<usize> {
    let body = get(client, credentials, "/v1/apps?fields[apps]=name&limit=200").await?;
    Ok(parse_apps(&body)?.len())
}

pub async fn fetch_builds(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    app_id: &str,
) -> Result<Vec<Build>> {
    let path = format!(
        "/v1/builds?filter[app]={app_id}&sort=-uploadedDate&limit={BUILDS_LIMIT}\
         &include=preReleaseVersion,buildBetaDetail,betaGroups\
         &fields[betaGroups]=name,isInternalGroup&limit[betaGroups]=20"
    );
    parse_builds(&get(client, credentials, &path).await?)
}

pub async fn fetch_versions(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    app_id: &str,
) -> Result<Vec<Version>> {
    let path = format!(
        "/v1/apps/{app_id}/appStoreVersions?filter[platform]=IOS&limit={VERSIONS_LIMIT}\
         &include=build,appStoreVersionPhasedRelease"
    );
    parse_versions(&get(client, credentials, &path).await?)
}

pub async fn fetch_beta_groups(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    app_id: &str,
) -> Result<Vec<BetaGroup>> {
    let path = format!("/v1/apps/{app_id}/betaGroups?fields[betaGroups]=name,isInternalGroup");
    parse_beta_groups(&get(client, credentials, &path).await?)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A throwaway P-256 key made for these tests; it signs nothing real.
    pub(crate) const TEST_P8: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgX4nYxx1No08sUiN4
zcowjIKUsMuowsSaig3s4G46KxKhRANCAARqe9krh+agzSyYzU8BdP0fSRXYuC5Y
HOZlagzGSeI4Y2n41B0xXZzBiRsZxQ/caiTZS8mS9GsZwPYBmZC3ccuU
-----END PRIVATE KEY-----";
    const TEST_PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEanvZK4fmoM0smM1PAXT9H0kV2Lgu
WBzmZWoMxkniOGNp+NQdMV2cwYkbGcUP3Gok2UvJkvRrGcD2AZmQt3HLlA==
-----END PUBLIC KEY-----";

    pub(crate) const BUILDS_FIXTURE: &str = r#"{
      "data": [
        {
          "type": "builds", "id": "asc-128",
          "attributes": {"version": "128", "processingState": "VALID", "uploadedDate": "2026-11-14T12:30:00-08:00", "expired": false},
          "relationships": {
            "preReleaseVersion": {"data": {"type": "preReleaseVersions", "id": "pre-240"}},
            "buildBetaDetail": {"data": {"type": "buildBetaDetails", "id": "detail-128"}},
            "betaGroups": {"data": [{"type": "betaGroups", "id": "g-internal"}, {"type": "betaGroups", "id": "g-qa"}]}
          }
        },
        {
          "type": "builds", "id": "asc-126",
          "attributes": {"version": "126", "processingState": "VALID", "uploadedDate": "2026-11-01T10:00:00Z", "expired": true},
          "relationships": {
            "preReleaseVersion": {"data": {"type": "preReleaseVersions", "id": "pre-239"}},
            "buildBetaDetail": {"data": {"type": "buildBetaDetails", "id": "detail-126"}},
            "betaGroups": {"data": []}
          }
        }
      ],
      "included": [
        {"type": "preReleaseVersions", "id": "pre-240", "attributes": {"version": "2.4.0", "platform": "IOS"}},
        {"type": "preReleaseVersions", "id": "pre-239", "attributes": {"version": "2.3.9", "platform": "IOS"}},
        {"type": "buildBetaDetails", "id": "detail-128", "attributes": {"internalBuildState": "IN_BETA_TESTING", "externalBuildState": "WAITING_FOR_BETA_REVIEW"}},
        {"type": "buildBetaDetails", "id": "detail-126", "attributes": {"internalBuildState": "EXPIRED", "externalBuildState": "EXPIRED"}},
        {"type": "betaGroups", "id": "g-internal", "attributes": {"name": "Time Trix", "isInternalGroup": true}},
        {"type": "betaGroups", "id": "g-qa", "attributes": {"name": "QA externo", "isInternalGroup": false}}
      ]
    }"#;

    pub(crate) const VERSIONS_FIXTURE: &str = r#"{
      "data": [
        {
          "type": "appStoreVersions", "id": "v-240",
          "attributes": {"versionString": "2.4.0", "appStoreState": "WAITING_FOR_REVIEW", "createdDate": "2026-11-14T13:00:00Z"},
          "relationships": {
            "build": {"data": {"type": "builds", "id": "asc-128"}},
            "appStoreVersionPhasedRelease": {"data": null}
          }
        },
        {
          "type": "appStoreVersions", "id": "v-239",
          "attributes": {"versionString": "2.3.9", "appStoreState": "READY_FOR_SALE", "createdDate": "2026-11-01T13:00:00Z"},
          "relationships": {
            "build": {"data": {"type": "builds", "id": "asc-126"}},
            "appStoreVersionPhasedRelease": {"data": {"type": "appStoreVersionPhasedReleases", "id": "ph-239"}}
          }
        }
      ],
      "included": [
        {"type": "appStoreVersionPhasedReleases", "id": "ph-239", "attributes": {"phasedReleaseState": "ACTIVE", "currentDayNumber": 3}}
      ]
    }"#;

    #[test]
    fn signs_a_token_apple_accepts() {
        let credentials = Credentials {
            issuer_id: " 57246542-96fe-1a63-e053-0824d011072a ".into(),
            key_id: "2X9R4HXF34".into(),
            private_key: TEST_P8.into(),
        };
        let token = token(&credentials, 1_800_000_000).unwrap();
        let header = jsonwebtoken::decode_header(&token).unwrap();
        assert_eq!(header.alg, Algorithm::ES256);
        assert_eq!(header.kid.as_deref(), Some("2X9R4HXF34"));

        let mut validation = jsonwebtoken::Validation::new(Algorithm::ES256);
        validation.set_audience(&[AUDIENCE]);
        validation.validate_exp = false;
        let decoded = jsonwebtoken::decode::<Claims>(
            &token,
            &jsonwebtoken::DecodingKey::from_ec_pem(TEST_PUBLIC_KEY.as_bytes()).unwrap(),
            &validation,
        )
        .unwrap();
        assert_eq!(decoded.claims.iss, "57246542-96fe-1a63-e053-0824d011072a");
        assert!(decoded.claims.exp - decoded.claims.iat <= 20 * 60);
    }

    #[test]
    fn rejects_a_key_that_is_not_a_p8() {
        assert!(signing_key("not a key").is_err());
    }

    #[test]
    fn parses_builds_with_their_version_and_groups() {
        let builds = parse_builds(BUILDS_FIXTURE).unwrap();
        assert_eq!(builds.len(), 2);
        let build = &builds[0];
        assert_eq!(build.build_number, "128");
        assert_eq!(build.version.as_deref(), Some("2.4.0"));
        assert_eq!(build.processing_state, ProcessingState::Valid);
        assert_eq!(
            build.external_state.as_deref(),
            Some("WAITING_FOR_BETA_REVIEW")
        );
        let names: Vec<&str> = build
            .groups
            .iter()
            .map(|group| group.name.as_str())
            .collect();
        assert_eq!(names, ["Time Trix", "QA externo"]);
        assert!(build.groups[0].is_internal);
        assert!(builds[1].expired);
    }

    #[test]
    fn parses_versions_with_phased_release() {
        let versions = parse_versions(VERSIONS_FIXTURE).unwrap();
        assert_eq!(versions[0].state, "WAITING_FOR_REVIEW");
        assert_eq!(versions[0].build_id.as_deref(), Some("asc-128"));
        assert!(versions[0].phased_release.is_none());
        let phased = versions[1].phased_release.as_ref().unwrap();
        assert_eq!(phased.state, PhasedReleaseState::Active);
        assert_eq!(phased.current_day, Some(3));
    }

    #[test]
    fn reads_the_error_detail() {
        let body = r#"{"errors":[{"status":"409","title":"Conflict","detail":"The build is not in a valid state"}]}"#;
        assert_eq!(
            first_error_detail(body).as_deref(),
            Some("The build is not in a valid state")
        );
    }
}
