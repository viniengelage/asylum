//! EAS (Expo Application Services) through its GraphQL API: the builds and submissions of one
//! EAS project, read with a personal or robot access token.

use crate::{ApiError, model::Platform};
use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, Method, Request};
use i18n::t;
use serde::Deserialize;
use std::sync::Arc;

pub const GRAPHQL_URL: &str = "https://api.expo.dev/graphql";

/// Enough to cover the builds of the last few versions of both platforms.
const ACTIVITY_LIMIT: usize = 40;

const ACTIVITY_QUERY: &str = "query AsylumAppActivity($appId: String!, $limit: Int!) {
  app {
    byId(appId: $appId) {
      id
      fullName
      builds(offset: 0, limit: $limit) {
        id
        status
        platform
        buildProfile
        appVersion
        appBuildVersion
        gitCommitHash
        gitCommitMessage
        createdAt
        completedAt
        queuePosition
        error { errorCode message }
      }
      submissions(filter: {}, offset: 0, limit: $limit) {
        id
        status
        platform
        createdAt
        completedAt
        submittedBuild { id appVersion appBuildVersion }
        error { errorCode message }
      }
    }
  }
}";

const VIEWER_QUERY: &str = "query AsylumViewer {
  meActor {
    __typename
    id
    ... on User { username }
    ... on Robot { firstName }
  }
}";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BuildStatus {
    New,
    InQueue,
    InProgress,
    PendingCancel,
    Finished,
    Errored,
    Canceled,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SubmissionStatus {
    AwaitingBuild,
    InQueue,
    InProgress,
    Finished,
    Errored,
    Canceled,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum EasPlatform {
    Ios,
    Android,
    #[serde(other)]
    Other,
}

impl EasPlatform {
    fn platform(self) -> Option<Platform> {
        match self {
            Self::Ios => Some(Platform::Ios),
            Self::Android => Some(Platform::Android),
            Self::Other => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Build {
    pub id: String,
    pub platform: Platform,
    pub status: BuildStatus,
    pub profile: Option<String>,
    pub app_version: Option<String>,
    /// `buildNumber` on iOS, `versionCode` on Android.
    pub build_version: Option<String>,
    pub commit: Option<String>,
    pub commit_message: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub queue_position: Option<u32>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Submission {
    pub id: String,
    pub platform: Platform,
    pub status: SubmissionStatus,
    pub build_id: Option<String>,
    pub app_version: Option<String>,
    pub build_version: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Activity {
    pub project_name: Option<String>,
    pub builds: Vec<Build>,
    pub submissions: Vec<Submission>,
}

#[derive(Deserialize)]
struct GraphQlResponse<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Deserialize)]
struct GraphQlError {
    message: String,
    #[serde(default)]
    extensions: Option<GraphQlErrorExtensions>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphQlErrorExtensions {
    #[serde(default)]
    error_code: Option<String>,
    #[serde(default)]
    code: Option<String>,
}

#[derive(Deserialize)]
struct ActivityData {
    app: AppQuery,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppQuery {
    by_id: RawApp,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawApp {
    #[serde(default)]
    full_name: Option<String>,
    #[serde(default)]
    builds: Vec<RawBuild>,
    #[serde(default)]
    submissions: Vec<RawSubmission>,
}

#[derive(Deserialize)]
struct RawError {
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBuild {
    id: String,
    status: BuildStatus,
    platform: EasPlatform,
    #[serde(default)]
    build_profile: Option<String>,
    #[serde(default)]
    app_version: Option<String>,
    #[serde(default)]
    app_build_version: Option<String>,
    #[serde(default)]
    git_commit_hash: Option<String>,
    #[serde(default)]
    git_commit_message: Option<String>,
    #[serde(default)]
    created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    completed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    queue_position: Option<u32>,
    #[serde(default)]
    error: Option<RawError>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSubmittedBuild {
    id: String,
    #[serde(default)]
    app_version: Option<String>,
    #[serde(default)]
    app_build_version: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSubmission {
    id: String,
    status: SubmissionStatus,
    platform: EasPlatform,
    #[serde(default)]
    created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    completed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    submitted_build: Option<RawSubmittedBuild>,
    #[serde(default)]
    error: Option<RawError>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ViewerData {
    me_actor: Option<Viewer>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Viewer {
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    first_name: Option<String>,
    #[serde(rename = "__typename", default)]
    kind: Option<String>,
}

fn unwrap_response<T>(response: GraphQlResponse<T>) -> Result<T> {
    if let Some(error) = response.errors.first() {
        let code = error
            .extensions
            .as_ref()
            .and_then(|extensions| extensions.error_code.clone().or(extensions.code.clone()))
            .unwrap_or_default();
        if code.contains("UNAUTHORIZED") || code.contains("UNAUTHENTICATED") {
            return Err(ApiError::Unauthorized.into());
        }
        if response.data.is_none() {
            return Err(ApiError::Status {
                status: 200,
                body: error.message.clone(),
            }
            .into());
        }
        log::warn!("EAS: resposta com erros parciais: {}", error.message);
    }
    response
        .data
        .with_context(|| t!("app_releases.api.unexpected_response", service = "EAS"))
}

pub fn parse_activity(body: &str) -> Result<Activity> {
    let response: GraphQlResponse<ActivityData> = serde_json::from_str(body)
        .with_context(|| t!("app_releases.api.unexpected_response", service = "EAS"))?;
    let app = unwrap_response(response)?.app.by_id;
    let builds = app
        .builds
        .into_iter()
        .filter_map(|build| {
            Some(Build {
                platform: build.platform.platform()?,
                id: build.id,
                status: build.status,
                profile: build.build_profile,
                app_version: build.app_version,
                build_version: build.app_build_version,
                commit: build.git_commit_hash,
                commit_message: build.git_commit_message,
                created_at: build.created_at,
                completed_at: build.completed_at,
                queue_position: build.queue_position,
                error: build.error.and_then(|error| error.message),
            })
        })
        .collect();
    let submissions = app
        .submissions
        .into_iter()
        .filter_map(|submission| {
            let submitted_build = submission.submitted_build;
            Some(Submission {
                platform: submission.platform.platform()?,
                id: submission.id,
                status: submission.status,
                build_id: submitted_build.as_ref().map(|build| build.id.clone()),
                app_version: submitted_build
                    .as_ref()
                    .and_then(|build| build.app_version.clone()),
                build_version: submitted_build.and_then(|build| build.app_build_version),
                created_at: submission.created_at,
                completed_at: submission.completed_at,
                error: submission.error.and_then(|error| error.message),
            })
        })
        .collect();
    Ok(Activity {
        project_name: app.full_name,
        builds,
        submissions,
    })
}

/// Who the token belongs to, for the connection test.
pub fn parse_viewer(body: &str) -> Result<String> {
    let response: GraphQlResponse<ViewerData> = serde_json::from_str(body)
        .with_context(|| t!("app_releases.api.unexpected_response", service = "EAS"))?;
    let viewer = unwrap_response(response)?
        .me_actor
        .ok_or(ApiError::Unauthorized)?;
    Ok(viewer
        .username
        .or(viewer.first_name)
        .or(viewer.kind)
        .unwrap_or_default())
}

async fn query<T>(
    client: &Arc<dyn HttpClient>,
    token: &str,
    query: &str,
    variables: serde_json::Value,
    parse: impl FnOnce(&str) -> Result<T>,
) -> Result<T> {
    let body = serde_json::json!({ "query": query, "variables": variables });
    let request = Request::builder()
        .method(Method::POST)
        .uri(GRAPHQL_URL)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .body(AsyncBody::from(serde_json::to_string(&body)?))?;
    let mut response = client
        .send(request)
        .await
        .with_context(|| t!("app_releases.api.request_failed", service = "EAS"))?;
    let mut text = String::new();
    response.body_mut().read_to_string(&mut text).await?;
    match response.status().as_u16() {
        401 | 403 => return Err(ApiError::Unauthorized.into()),
        429 => return Err(ApiError::RateLimited.into()),
        status if !(200..300).contains(&status) => {
            return Err(ApiError::Status { status, body: text }.into());
        }
        _ => {}
    }
    parse(&text)
}

pub async fn fetch_activity(
    client: &Arc<dyn HttpClient>,
    token: &str,
    project_id: &str,
) -> Result<Activity> {
    query(
        client,
        token,
        ACTIVITY_QUERY,
        serde_json::json!({ "appId": project_id, "limit": ACTIVITY_LIMIT }),
        parse_activity,
    )
    .await
}

pub async fn fetch_viewer(client: &Arc<dyn HttpClient>, token: &str) -> Result<String> {
    query(
        client,
        token,
        VIEWER_QUERY,
        serde_json::json!({}),
        parse_viewer,
    )
    .await
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::is_unauthorized;

    pub(crate) const ACTIVITY_FIXTURE: &str = r#"{
      "data": {
        "app": {
          "byId": {
            "id": "f3c1",
            "fullName": "@trix/app",
            "builds": [
              {
                "id": "b-ios-128", "status": "FINISHED", "platform": "IOS",
                "buildProfile": "production", "appVersion": "2.4.0", "appBuildVersion": "128",
                "gitCommitHash": "9f2c1ab77de", "gitCommitMessage": "Ajusta o extrato",
                "createdAt": "2026-11-14T12:00:00.000Z", "completedAt": "2026-11-14T12:18:00.000Z",
                "queuePosition": null, "error": null
              },
              {
                "id": "b-android-128", "status": "IN_PROGRESS", "platform": "ANDROID",
                "buildProfile": "production", "appVersion": "2.4.0", "appBuildVersion": "128",
                "gitCommitHash": "9f2c1ab77de", "gitCommitMessage": "Ajusta o extrato",
                "createdAt": "2026-11-14T12:00:00.000Z", "completedAt": null,
                "queuePosition": null, "error": null
              },
              {
                "id": "b-ios-127", "status": "ERRORED", "platform": "IOS",
                "buildProfile": "production", "appVersion": "2.4.0", "appBuildVersion": "127",
                "gitCommitHash": "11aa", "gitCommitMessage": null,
                "createdAt": "2026-11-13T10:00:00.000Z", "completedAt": "2026-11-13T10:09:00.000Z",
                "queuePosition": null, "error": {"errorCode": "XCODE_BUILD_ERROR", "message": "Falhou no Xcode"}
              },
              {
                "id": "b-web", "status": "FINISHED", "platform": "WEB",
                "createdAt": "2026-11-13T10:00:00.000Z"
              }
            ],
            "submissions": [
              {
                "id": "s-ios-128", "status": "FINISHED", "platform": "IOS",
                "createdAt": "2026-11-14T12:20:00.000Z", "completedAt": "2026-11-14T12:24:00.000Z",
                "submittedBuild": {"id": "b-ios-128", "appVersion": "2.4.0", "appBuildVersion": "128"},
                "error": null
              }
            ]
          }
        }
      }
    }"#;

    #[test]
    fn parses_builds_and_submissions() {
        let activity = parse_activity(ACTIVITY_FIXTURE).unwrap();
        assert_eq!(activity.project_name.as_deref(), Some("@trix/app"));
        assert_eq!(activity.builds.len(), 3, "web builds are left out");
        let ios = &activity.builds[0];
        assert_eq!(ios.platform, Platform::Ios);
        assert_eq!(ios.status, BuildStatus::Finished);
        assert_eq!(ios.build_version.as_deref(), Some("128"));
        assert_eq!(activity.builds[2].error.as_deref(), Some("Falhou no Xcode"));
        let submission = &activity.submissions[0];
        assert_eq!(submission.build_id.as_deref(), Some("b-ios-128"));
        assert_eq!(submission.status, SubmissionStatus::Finished);
    }

    #[test]
    fn unknown_statuses_do_not_break_parsing() {
        let body = r#"{"data":{"app":{"byId":{"builds":[{"id":"1","status":"SOMETHING_NEW","platform":"IOS"}],"submissions":[]}}}}"#;
        let activity = parse_activity(body).unwrap();
        assert_eq!(activity.builds[0].status, BuildStatus::Unknown);
    }

    #[test]
    fn graphql_auth_errors_are_unauthorized() {
        let body = r#"{"data":null,"errors":[{"message":"Unauthorized","extensions":{"errorCode":"UNAUTHORIZED_ERROR"}}]}"#;
        assert!(is_unauthorized(&parse_activity(body).unwrap_err()));
        let body = r#"{"data":null,"errors":[{"message":"App not found"}]}"#;
        assert!(!is_unauthorized(&parse_activity(body).unwrap_err()));
    }

    #[test]
    fn reads_the_viewer_name() {
        let body = r#"{"data":{"meActor":{"__typename":"User","id":"u1","username":"vini"}}}"#;
        assert_eq!(parse_viewer(body).unwrap(), "vini");
        let body = r#"{"data":{"meActor":{"__typename":"Robot","id":"r1","firstName":"ci"}}}"#;
        assert_eq!(parse_viewer(body).unwrap(), "ci");
        let body = r#"{"data":{"meActor":null}}"#;
        assert!(is_unauthorized(&parse_viewer(body).unwrap_err()));
    }
}
