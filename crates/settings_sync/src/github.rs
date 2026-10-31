use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, Method, Request};
use serde::Deserialize;

/// The "Asylum" GitHub App. Device Flow needs only the client id, which is public by design,
/// so no secret is shipped with the app.
pub const CLIENT_ID: &str = "Iv23lisjo7ZLRUpyI04H";

const API_URL: &str = "https://api.github.com";
const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

pub enum TokenPoll {
    Pending,
    SlowDown,
    Authorized(String),
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitHubUser {
    pub login: String,
    pub name: Option<String>,
    pub avatar_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Gist {
    pub id: String,
    pub description: Option<String>,
    #[serde(default)]
    pub files: HashMap<String, GistFile>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GistFile {
    pub content: Option<String>,
    #[serde(default)]
    pub truncated: bool,
    pub raw_url: Option<String>,
}

/// Lets callers tell an expired or revoked token apart from other failures.
#[derive(Debug)]
pub struct Unauthorized;

impl std::fmt::Display for Unauthorized {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("GitHub rejected the saved sign-in")
    }
}

impl std::error::Error for Unauthorized {}

/// The gist is gone, usually because the user deleted it on GitHub.
#[derive(Debug)]
pub struct NotFound;

impl std::fmt::Display for NotFound {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("not found on GitHub")
    }
}

impl std::error::Error for NotFound {}

fn form_body(fields: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields)
        .finish()
}

async fn send(
    client: &Arc<dyn HttpClient>,
    method: Method,
    url: &str,
    token: Option<&str>,
    body: Option<(&str, String)>,
) -> Result<(u16, String)> {
    let mut builder = Request::builder()
        .method(method)
        .uri(url)
        .header("Accept", "application/json")
        .header("User-Agent", "Asylum")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(token) = token {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    let body = match body {
        Some((content_type, body)) => {
            builder = builder.header("Content-Type", content_type);
            AsyncBody::from(body)
        }
        None => AsyncBody::default(),
    };
    let mut response = client
        .send(builder.body(body)?)
        .await
        .with_context(|| format!("failed to reach GitHub at {url}"))?;
    let mut text = String::new();
    response.body_mut().read_to_string(&mut text).await?;
    let status = response.status().as_u16();
    if status == 401 {
        return Err(Unauthorized.into());
    }
    Ok((status, text))
}

async fn api<T: for<'de> Deserialize<'de>>(
    client: &Arc<dyn HttpClient>,
    method: Method,
    path: &str,
    token: &str,
    body: Option<serde_json::Value>,
) -> Result<T> {
    let url = format!("{API_URL}{path}");
    let body = body
        .map(|body| serde_json::to_string(&body).map(|body| ("application/json", body)))
        .transpose()?;
    let (status, text) = send(client, method, &url, Some(token), body).await?;
    if status == 404 {
        return Err(NotFound.into());
    }
    if !(200..300).contains(&status) {
        bail!("GitHub answered {status} for {path}: {text}");
    }
    serde_json::from_str(&text).with_context(|| format!("unexpected GitHub response for {path}"))
}

pub async fn request_device_code(client: &Arc<dyn HttpClient>) -> Result<DeviceCode> {
    let body = form_body(&[("client_id", CLIENT_ID)]);
    let (status, text) = send(
        client,
        Method::POST,
        DEVICE_CODE_URL,
        None,
        Some(("application/x-www-form-urlencoded", body)),
    )
    .await?;
    if !(200..300).contains(&status) {
        bail!("GitHub refused to start the sign-in ({status}): {text}");
    }
    serde_json::from_str(&text).context("unexpected response when starting the GitHub sign-in")
}

pub async fn poll_for_token(client: &Arc<dyn HttpClient>, device_code: &str) -> Result<TokenPoll> {
    #[derive(Deserialize)]
    struct Response {
        access_token: Option<String>,
        error: Option<String>,
        error_description: Option<String>,
    }

    let body = form_body(&[
        ("client_id", CLIENT_ID),
        ("device_code", device_code),
        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
    ]);
    let (_, text) = send(
        client,
        Method::POST,
        ACCESS_TOKEN_URL,
        None,
        Some(("application/x-www-form-urlencoded", body)),
    )
    .await?;
    let response: Response =
        serde_json::from_str(&text).context("unexpected response while waiting for GitHub")?;
    if let Some(token) = response.access_token {
        return Ok(TokenPoll::Authorized(token));
    }
    match response.error.as_deref() {
        Some("authorization_pending") => Ok(TokenPoll::Pending),
        Some("slow_down") => Ok(TokenPoll::SlowDown),
        Some("expired_token") => Err(anyhow!("the sign-in code expired; start again")),
        Some("access_denied") => Err(anyhow!("the sign-in was cancelled on GitHub")),
        _ => Err(anyhow!(
            "GitHub sign-in failed: {}",
            response
                .error_description
                .or(response.error)
                .unwrap_or(text)
        )),
    }
}

pub async fn current_user(client: &Arc<dyn HttpClient>, token: &str) -> Result<GitHubUser> {
    api(client, Method::GET, "/user", token, None).await
}

/// Finds the gist whose description is exactly `description` among the user's gists.
pub async fn find_gist(
    client: &Arc<dyn HttpClient>,
    token: &str,
    description: &str,
) -> Result<Option<String>> {
    const MAX_PAGES: usize = 20;
    for page in 1..=MAX_PAGES {
        let gists: Vec<Gist> = api(
            client,
            Method::GET,
            &format!("/gists?per_page=100&page={page}"),
            token,
            None,
        )
        .await?;
        if let Some(gist) = gists
            .iter()
            .find(|gist| gist.description.as_deref() == Some(description))
        {
            return Ok(Some(gist.id.clone()));
        }
        if gists.len() < 100 {
            break;
        }
    }
    Ok(None)
}

/// Returns the gist's files with their full contents.
pub async fn gist_files(
    client: &Arc<dyn HttpClient>,
    token: &str,
    gist_id: &str,
) -> Result<BTreeMap<String, String>> {
    let gist: Gist = api(
        client,
        Method::GET,
        &format!("/gists/{gist_id}"),
        token,
        None,
    )
    .await?;
    let mut files = BTreeMap::new();
    for (name, file) in gist.files {
        // Files above 1 MB come back cut short; their raw URL has the whole content.
        let content = match (file.truncated, file.raw_url, file.content) {
            (true, Some(raw_url), _) => {
                let (status, text) = send(client, Method::GET, &raw_url, Some(token), None).await?;
                if !(200..300).contains(&status) {
                    bail!("GitHub answered {status} when downloading {name}");
                }
                text
            }
            (_, _, Some(content)) => content,
            (_, _, None) => continue,
        };
        files.insert(name, content);
    }
    Ok(files)
}

pub async fn create_gist(
    client: &Arc<dyn HttpClient>,
    token: &str,
    description: &str,
    files: &BTreeMap<String, String>,
) -> Result<String> {
    let files: serde_json::Map<String, serde_json::Value> = files
        .iter()
        .map(|(name, content)| (name.clone(), serde_json::json!({ "content": content })))
        .collect();
    let gist: Gist = api(
        client,
        Method::POST,
        "/gists",
        token,
        Some(serde_json::json!({
            "description": description,
            "public": false,
            "files": files,
        })),
    )
    .await?;
    Ok(gist.id)
}

/// Writes the given files to the gist; a `None` content deletes the file.
pub async fn update_gist(
    client: &Arc<dyn HttpClient>,
    token: &str,
    gist_id: &str,
    files: &BTreeMap<String, Option<String>>,
) -> Result<()> {
    let files: serde_json::Map<String, serde_json::Value> = files
        .iter()
        .map(|(name, content)| {
            let value = match content {
                Some(content) => serde_json::json!({ "content": content }),
                None => serde_json::Value::Null,
            };
            (name.clone(), value)
        })
        .collect();
    let _: Gist = api(
        client,
        Method::PATCH,
        &format!("/gists/{gist_id}"),
        token,
        Some(serde_json::json!({ "files": files })),
    )
    .await?;
    Ok(())
}
