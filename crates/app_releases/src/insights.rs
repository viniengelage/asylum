//! After the launch: App Store ratings and reviews, TestFlight feedback, Google Play reviews and
//! the Play vitals (crash and ANR rates per version code). Read only, loaded on demand.

use crate::{
    ApiError,
    accounts::StoredAccounts,
    app_store_connect::{self as asc, Credentials},
    google_play::{self, REPORTING_SCOPE},
};
use anyhow::{Context as _, Result};
use chrono::{DateTime, Datelike as _, NaiveDate, TimeZone as _, Utc};
use collections::HashMap;
use futures::AsyncReadExt as _;
use gpui::SharedString;
use http_client::{AsyncBody, HttpClient, Method, Request};
use i18n::t;
use serde_json::Value;
use std::sync::Arc;

const REPORTING_URL: &str = "https://playdeveloperreporting.googleapis.com/v1beta1";
const REVIEWS_LIMIT: usize = 10;
/// Daily vitals lag behind by a day or two; a week is enough to compare versions.
const VITALS_DAYS: i64 = 7;
const VITALS_LAG_DAYS: i64 = 2;

#[derive(Clone, Debug, PartialEq)]
pub struct Review {
    pub rating: u8,
    pub title: Option<String>,
    pub body: String,
    pub author: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    /// Play says which version code a review was written on; Apple doesn't.
    pub version_code: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedbackKind {
    Crash,
    Screenshot,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TesterFeedback {
    pub kind: FeedbackKind,
    pub comment: Option<String>,
    pub device: Option<String>,
    pub os_version: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Vitals {
    pub version_code: String,
    pub crash_rate: Option<f64>,
    pub anr_rate: Option<f64>,
}

/// `None` when the store has no account or the app has no id for it.
pub type Section<T> = Option<Result<Vec<T>, SharedString>>;

#[derive(Clone, Debug, Default)]
pub struct AfterLaunch {
    pub app_store_reviews: Section<Review>,
    pub testflight_feedback: Section<TesterFeedback>,
    pub play_reviews: Section<Review>,
    pub play_vitals: Section<Vitals>,
}

fn text_at(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn date_at(value: &Value, key: &str) -> Option<DateTime<Utc>> {
    let text = value.get(key)?.as_str()?;
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|date| date.with_timezone(&Utc))
}

fn parse_json(body: &str, service: &str) -> Result<Value> {
    serde_json::from_str(body)
        .with_context(|| t!("app_releases.api.unexpected_response", service = service))
}

pub fn parse_customer_reviews(body: &str) -> Result<Vec<Review>> {
    let value = parse_json(body, "App Store Connect")?;
    let reviews = value
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(reviews
        .iter()
        .filter_map(|review| {
            let attributes = review.get("attributes")?;
            Some(Review {
                rating: attributes
                    .get("rating")
                    .and_then(Value::as_u64)
                    .and_then(|rating| u8::try_from(rating).ok())
                    .unwrap_or(0),
                title: text_at(attributes, "title"),
                body: text_at(attributes, "body").unwrap_or_default(),
                author: text_at(attributes, "reviewerNickname"),
                created_at: date_at(attributes, "createdDate"),
                version_code: None,
            })
        })
        .collect())
}

pub fn parse_beta_feedback(body: &str, kind: FeedbackKind) -> Result<Vec<TesterFeedback>> {
    let value = parse_json(body, "App Store Connect")?;
    let submissions = value
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(submissions
        .iter()
        .filter_map(|submission| {
            let attributes = submission.get("attributes")?;
            Some(TesterFeedback {
                kind,
                comment: text_at(attributes, "comment"),
                device: text_at(attributes, "deviceModel"),
                os_version: text_at(attributes, "osVersion"),
                created_at: date_at(attributes, "createdDate"),
            })
        })
        .collect())
}

pub fn parse_play_reviews(body: &str) -> Result<Vec<Review>> {
    let value = parse_json(body, "Google Play")?;
    let reviews = value
        .get("reviews")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(reviews
        .iter()
        .filter_map(|review| {
            let comment = review
                .get("comments")?
                .as_array()?
                .iter()
                .find_map(|comment| comment.get("userComment"))?;
            let created_at = comment
                .get("lastModified")
                .and_then(|modified| modified.get("seconds"))
                .and_then(|seconds| match seconds {
                    Value::String(text) => text.parse::<i64>().ok(),
                    other => other.as_i64(),
                })
                .and_then(|seconds| Utc.timestamp_opt(seconds, 0).single());
            Some(Review {
                rating: comment
                    .get("starRating")
                    .and_then(Value::as_u64)
                    .and_then(|rating| u8::try_from(rating).ok())
                    .unwrap_or(0),
                title: None,
                body: text_at(comment, "text").unwrap_or_default(),
                author: text_at(review, "authorName"),
                created_at,
                version_code: comment.get("appVersionCode").and_then(|code| match code {
                    Value::String(text) => Some(text.clone()),
                    other => other.as_i64().map(|code| code.to_string()),
                }),
            })
        })
        .collect())
}

/// The latest day's rate of `metric` per version code.
pub fn parse_metric_rows(body: &str, metric: &str) -> Result<HashMap<String, f64>> {
    let value = parse_json(body, "Google Play")?;
    let mut latest: HashMap<String, (NaiveDate, f64)> = HashMap::default();
    for row in value
        .get("rows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let Some(version_code) = row
            .get("dimensions")
            .and_then(Value::as_array)
            .and_then(|dimensions| {
                dimensions.iter().find(|dimension| {
                    dimension.get("dimension").and_then(Value::as_str) == Some("versionCode")
                })
            })
            .and_then(|dimension| text_at(dimension, "stringValue"))
        else {
            continue;
        };
        let Some(rate) = row
            .get("metrics")
            .and_then(Value::as_array)
            .and_then(|metrics| {
                metrics
                    .iter()
                    .find(|entry| entry.get("metric").and_then(Value::as_str) == Some(metric))
            })
            .and_then(|entry| {
                entry
                    .get("decimalValue")?
                    .get("value")?
                    .as_str()?
                    .parse()
                    .ok()
            })
        else {
            continue;
        };
        let date = row
            .get("startTime")
            .and_then(|start| {
                let number = |key: &str| start.get(key).and_then(Value::as_i64);
                NaiveDate::from_ymd_opt(
                    i32::try_from(number("year")?).ok()?,
                    u32::try_from(number("month")?).ok()?,
                    u32::try_from(number("day")?).ok()?,
                )
            })
            .unwrap_or_default();
        let newer = latest
            .get(&version_code)
            .is_none_or(|(known, _)| date >= *known);
        if newer {
            latest.insert(version_code, (date, rate));
        }
    }
    Ok(latest
        .into_iter()
        .map(|(version_code, (_, rate))| (version_code, rate))
        .collect())
}

pub fn merge_vitals(crash: HashMap<String, f64>, anr: HashMap<String, f64>) -> Vec<Vitals> {
    let mut version_codes: Vec<String> = crash.keys().chain(anr.keys()).cloned().collect();
    version_codes.sort_by(|left, right| crate::model::compare_build_numbers(right, left));
    version_codes.dedup();
    version_codes
        .into_iter()
        .map(|version_code| Vitals {
            crash_rate: crash.get(&version_code).copied(),
            anr_rate: anr.get(&version_code).copied(),
            version_code,
        })
        .collect()
}

fn reporting_date(date: NaiveDate) -> Value {
    serde_json::json!({
        "year": date.year(),
        "month": date.month(),
        "day": date.day(),
        "timeZone": { "id": "America/Los_Angeles" }
    })
}

pub fn vitals_query(metric: &str, today: NaiveDate) -> Value {
    let end = today - chrono::Duration::days(VITALS_LAG_DAYS);
    let start = end - chrono::Duration::days(VITALS_DAYS);
    serde_json::json!({
        "timelineSpec": {
            "aggregationPeriod": "DAILY",
            "startTime": reporting_date(start),
            "endTime": reporting_date(end)
        },
        "dimensions": ["versionCode"],
        "metrics": [metric],
        "pageSize": 1000
    })
}

async fn query_vitals(
    client: &Arc<dyn HttpClient>,
    access_token: &str,
    package: &str,
    metric_set: &str,
    metric: &str,
) -> Result<HashMap<String, f64>> {
    let url = format!(
        "{REPORTING_URL}/apps/{}/{metric_set}:query",
        urlencoding::encode(package)
    );
    let body = vitals_query(metric, Utc::now().date_naive());
    let request = Request::builder()
        .method(Method::POST)
        .uri(url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Content-Type", "application/json")
        .body(AsyncBody::from(serde_json::to_string(&body)?))?;
    let mut response = client
        .send(request)
        .await
        .with_context(|| t!("app_releases.api.request_failed", service = "Google Play"))?;
    let mut text = String::new();
    response.body_mut().read_to_string(&mut text).await?;
    match response.status().as_u16() {
        401 | 403 => Err(ApiError::Unauthorized.into()),
        status if (200..300).contains(&status) => parse_metric_rows(&text, metric),
        status => Err(ApiError::Status { status, body: text }.into()),
    }
}

async fn fetch_play_vitals(
    client: &Arc<dyn HttpClient>,
    account: &google_play::ServiceAccount,
    package: &str,
) -> Result<Vec<Vitals>> {
    let access_token =
        google_play::access_token_for_scope(client, account, REPORTING_SCOPE).await?;
    let (crash, anr) = futures::join!(
        query_vitals(
            client,
            &access_token,
            package,
            "crashRateMetricSet",
            "crashRate"
        ),
        query_vitals(
            client,
            &access_token,
            package,
            "anrRateMetricSet",
            "anrRate"
        ),
    );
    Ok(merge_vitals(crash?, anr?))
}

async fn fetch_play_reviews(
    client: &Arc<dyn HttpClient>,
    account: &google_play::ServiceAccount,
    package: &str,
) -> Result<Vec<Review>> {
    let access_token = google_play::access_token(client, account).await?;
    let body = google_play::send(
        client,
        &access_token,
        Method::GET,
        &format!(
            "/applications/{}/reviews?maxResults={REVIEWS_LIMIT}",
            urlencoding::encode(package)
        ),
        None,
    )
    .await?;
    parse_play_reviews(&body)
}

async fn fetch_app_store_reviews(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    app_id: &str,
) -> Result<Vec<Review>> {
    let body = asc::send(
        client,
        credentials,
        Method::GET,
        &format!("/v1/apps/{app_id}/customerReviews?sort=-createdDate&limit={REVIEWS_LIMIT}"),
        None,
    )
    .await?;
    parse_customer_reviews(&body)
}

async fn fetch_testflight_feedback(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    app_id: &str,
) -> Result<Vec<TesterFeedback>> {
    let crashes_path =
        format!("/v1/apps/{app_id}/betaFeedbackCrashSubmissions?limit={REVIEWS_LIMIT}");
    let screenshots_path =
        format!("/v1/apps/{app_id}/betaFeedbackScreenshotSubmissions?limit={REVIEWS_LIMIT}");
    let crashes = asc::send(client, credentials, Method::GET, &crashes_path, None);
    let screenshots = asc::send(client, credentials, Method::GET, &screenshots_path, None);
    let (crashes, screenshots) = futures::join!(crashes, screenshots);
    let mut feedback = parse_beta_feedback(&crashes?, FeedbackKind::Crash)?;
    feedback.extend(parse_beta_feedback(
        &screenshots?,
        FeedbackKind::Screenshot,
    )?);
    feedback.sort_by_key(|entry| std::cmp::Reverse(entry.created_at));
    feedback.truncate(REVIEWS_LIMIT);
    Ok(feedback)
}

fn section<T>(result: Result<Vec<T>>) -> Section<T> {
    Some(result.map_err(|error| {
        if crate::is_unauthorized(&error) {
            t!("app_releases.after.refused")
        } else {
            format!("{error:#}").into()
        }
    }))
}

pub async fn fetch_after_launch(
    client: &Arc<dyn HttpClient>,
    accounts: &StoredAccounts,
    app_store_app_id: Option<String>,
    package: Option<String>,
) -> AfterLaunch {
    let app_store = accounts
        .app_store
        .as_ref()
        .map(|account| account.credentials())
        .zip(app_store_app_id);
    let google_play = accounts
        .google_play
        .as_ref()
        .map(|account| account.service_account.clone())
        .zip(package);
    let app_store_part = async {
        match &app_store {
            Some((credentials, app_id)) => {
                let (reviews, feedback) = futures::join!(
                    fetch_app_store_reviews(client, credentials, app_id),
                    fetch_testflight_feedback(client, credentials, app_id),
                );
                (section(reviews), section(feedback))
            }
            None => (None, None),
        }
    };
    let google_play_part = async {
        match &google_play {
            Some((account, package)) => {
                let (reviews, vitals) = futures::join!(
                    fetch_play_reviews(client, account, package),
                    fetch_play_vitals(client, account, package),
                );
                (section(reviews), section(vitals))
            }
            None => (None, None),
        }
    };
    let ((app_store_reviews, testflight_feedback), (play_reviews, play_vitals)) =
        futures::join!(app_store_part, google_play_part);
    AfterLaunch {
        app_store_reviews,
        testflight_feedback,
        play_reviews,
        play_vitals,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_app_store_reviews() {
        let body = r#"{"data":[{"type":"customerReviews","id":"r1","attributes":{
            "rating":2,"title":"Travou","body":"O app fecha ao abrir o extrato",
            "reviewerNickname":"ana","createdDate":"2026-11-15T10:00:00-08:00","territory":"BRA"}}]}"#;
        let reviews = parse_customer_reviews(body).unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].rating, 2);
        assert_eq!(reviews[0].title.as_deref(), Some("Travou"));
        assert!(reviews[0].created_at.is_some());
    }

    #[test]
    fn parses_testflight_feedback() {
        let body = r#"{"data":[{"type":"betaFeedbackCrashSubmissions","id":"c1","attributes":{
            "comment":"Fechou no login","deviceModel":"iPhone16,2","osVersion":"26.1",
            "createdDate":"2026-11-15T10:00:00Z"}}]}"#;
        let feedback = parse_beta_feedback(body, FeedbackKind::Crash).unwrap();
        assert_eq!(feedback[0].kind, FeedbackKind::Crash);
        assert_eq!(feedback[0].device.as_deref(), Some("iPhone16,2"));
    }

    #[test]
    fn parses_play_reviews_with_their_version_code() {
        let body = r#"{"reviews":[{"reviewId":"g1","authorName":"Bruno","comments":[
            {"userComment":{"text":"Muito bom","starRating":5,"appVersionCode":128,
             "lastModified":{"seconds":"1794800000","nanos":0}}}]}]}"#;
        let reviews = parse_play_reviews(body).unwrap();
        assert_eq!(reviews[0].rating, 5);
        assert_eq!(reviews[0].version_code.as_deref(), Some("128"));
        assert_eq!(reviews[0].author.as_deref(), Some("Bruno"));
        assert!(reviews[0].created_at.is_some());
    }

    #[test]
    fn keeps_the_latest_day_of_each_version() {
        let crash = r#"{"rows":[
            {"startTime":{"year":2026,"month":11,"day":10},
             "dimensions":[{"dimension":"versionCode","stringValue":"128"}],
             "metrics":[{"metric":"crashRate","decimalValue":{"value":"0.02"}}]},
            {"startTime":{"year":2026,"month":11,"day":12},
             "dimensions":[{"dimension":"versionCode","stringValue":"128"}],
             "metrics":[{"metric":"crashRate","decimalValue":{"value":"0.004"}}]},
            {"startTime":{"year":2026,"month":11,"day":12},
             "dimensions":[{"dimension":"versionCode","stringValue":"127"}],
             "metrics":[{"metric":"crashRate","decimalValue":{"value":"0.01"}}]}
        ]}"#;
        let anr = r#"{"rows":[
            {"startTime":{"year":2026,"month":11,"day":12},
             "dimensions":[{"dimension":"versionCode","stringValue":"128"}],
             "metrics":[{"metric":"anrRate","decimalValue":{"value":"0.001"}}]}
        ]}"#;
        let vitals = merge_vitals(
            parse_metric_rows(crash, "crashRate").unwrap(),
            parse_metric_rows(anr, "anrRate").unwrap(),
        );
        assert_eq!(
            vitals,
            vec![
                Vitals {
                    version_code: "128".into(),
                    crash_rate: Some(0.004),
                    anr_rate: Some(0.001)
                },
                Vitals {
                    version_code: "127".into(),
                    crash_rate: Some(0.01),
                    anr_rate: None
                },
            ]
        );
    }

    #[test]
    fn asks_vitals_for_the_last_week_before_the_lag() {
        let query = vitals_query("crashRate", NaiveDate::from_ymd_opt(2026, 11, 16).unwrap());
        assert_eq!(query["timelineSpec"]["endTime"]["day"], 14);
        assert_eq!(query["timelineSpec"]["startTime"]["day"], 7);
        assert_eq!(query["dimensions"][0], "versionCode");
    }
}
