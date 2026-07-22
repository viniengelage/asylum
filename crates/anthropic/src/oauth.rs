use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::AsyncReadExt;
use http_client::{AsyncBody, HttpClient, Method, Request as HttpRequest};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const DEFAULT_REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnthropicOAuth {
    pub refresh_token: String,
    pub access_token: String,
    /// Expiration as milliseconds since UNIX epoch.
    pub expires_ms: u64,
}

impl AnthropicOAuth {
    pub fn is_expired(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.expires_ms <= now
    }

    pub fn access_token(&self) -> &str {
        &self.access_token
    }
}

pub struct AuthorizeParams {
    pub url: String,
    pub verifier: String,
    pub state: Option<String>,
}

pub fn build_authorize_url() -> Result<AuthorizeParams> {
    build_authorize_url_with_redirect(DEFAULT_REDIRECT_URI)
}

pub fn build_authorize_url_with_redirect(redirect_uri: &str) -> Result<AuthorizeParams> {
    let (challenge, verifier) = generate_pkce_pair();

    // Generate a random CSRF state token (separate from the verifier).
    let mut state_bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut state_bytes);
    let state: String = state_bytes.iter().map(|b| format!("{b:02x}")).collect();

    let url = format!(
        "{AUTHORIZE_URL}?\
         code=true\
         &client_id={CLIENT_ID}\
         &response_type=code\
         &redirect_uri={redirect}\
         &scope={scope}\
         &code_challenge={challenge}\
         &code_challenge_method=S256\
         &state={state}",
        redirect = urlencoding(redirect_uri),
        scope = urlencoding("user:profile user:inference user:sessions:claude_code"),
    );

    Ok(AuthorizeParams {
        url,
        verifier,
        state: Some(state),
    })
}

pub async fn exchange_code(
    client: &dyn HttpClient,
    code: &str,
    verifier: &str,
) -> Result<AnthropicOAuth> {
    exchange_code_with_redirect(client, code, verifier, DEFAULT_REDIRECT_URI).await
}

pub async fn exchange_code_with_redirect(
    client: &dyn HttpClient,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<AnthropicOAuth> {
    exchange_code_with_redirect_and_state(client, code, verifier, redirect_uri, None).await
}

pub async fn exchange_code_with_redirect_and_state(
    client: &dyn HttpClient,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    state: Option<&str>,
) -> Result<AnthropicOAuth> {
    // When code comes from clipboard as "CODE#STATE", split them apart.
    // When code comes from the callback server, state is passed separately.
    let (auth_code, inline_state) = code.split_once('#').unwrap_or((code, ""));
    let effective_state = state.unwrap_or(inline_state);

    let mut body = serde_json::json!({
        "grant_type": "authorization_code",
        "code": auth_code,
        "redirect_uri": redirect_uri,
        "client_id": CLIENT_ID,
        "code_verifier": verifier,
    });
    if !effective_state.is_empty() {
        body["state"] = serde_json::Value::String(effective_state.to_string());
    }

    log::info!(
        "Claude OAuth exchange: code_len={}, state_len={}, verifier_len={}",
        auth_code.len(),
        effective_state.len(),
        verifier.len()
    );

    let request = HttpRequest::builder()
        .method(Method::POST)
        .uri(TOKEN_URL)
        .header("Content-Type", "application/json")
        .body(AsyncBody::from(serde_json::to_string(&body)?))
        .context("failed to build OAuth token exchange request")?;

    let mut response = client
        .send(request)
        .await
        .context("failed to send OAuth token exchange request")?;

    let status = response.status();
    let mut response_body = String::new();
    response
        .body_mut()
        .read_to_string(&mut response_body)
        .await
        .context("failed to read OAuth token exchange response")?;

    if !status.is_success() {
        return Err(anyhow!(
            "OAuth token exchange failed with status {status}: {response_body}"
        ));
    }

    let token: TokenResponse = serde_json::from_str(&response_body)
        .context("failed to parse OAuth token response")?;

    Ok(token_response_to_auth(token))
}

pub async fn refresh_token(
    client: &dyn HttpClient,
    current: &AnthropicOAuth,
) -> Result<AnthropicOAuth> {
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": current.refresh_token,
        "client_id": CLIENT_ID,
    });

    let request = HttpRequest::builder()
        .method(Method::POST)
        .uri(TOKEN_URL)
        .header("Content-Type", "application/json")
        .body(AsyncBody::from(serde_json::to_string(&body)?))
        .context("failed to build OAuth token refresh request")?;

    let mut response = client
        .send(request)
        .await
        .context("failed to send OAuth token refresh request")?;

    if !response.status().is_success() {
        let mut error_body = String::new();
        response
            .body_mut()
            .read_to_string(&mut error_body)
            .await
            .ok();
        return Err(anyhow!(
            "OAuth token refresh failed with status {}: {}",
            response.status(),
            error_body
        ));
    }

    let mut response_body = String::new();
    response
        .body_mut()
        .read_to_string(&mut response_body)
        .await
        .context("failed to read OAuth token refresh response")?;

    let token: TokenResponse = serde_json::from_str(&response_body)
        .context("failed to parse OAuth token refresh response")?;

    Ok(token_response_to_auth(token))
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    refresh_token: String,
    access_token: String,
    expires_in: u64,
}

fn token_response_to_auth(token: TokenResponse) -> AnthropicOAuth {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    AnthropicOAuth {
        refresh_token: token.refresh_token,
        access_token: token.access_token,
        expires_ms: now + (token.expires_in * 1000),
    }
}

fn generate_pkce_pair() -> (String, String) {
    let mut verifier_bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut verifier_bytes);
    let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);

    let challenge_bytes = Sha256::digest(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(challenge_bytes);

    (challenge, verifier)
}

fn urlencoding(input: &str) -> String {
    let mut output = String::new();
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                output.push(byte as char);
            }
            _ => {
                output.push('%');
                output.push_str(&format!("{:02X}", byte));
            }
        }
    }
    output
}
