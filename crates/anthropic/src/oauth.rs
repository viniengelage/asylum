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
/// Scopes requested at sign-in and echoed back on every refresh. The token endpoint grants what
/// it is asked for, so omitting them on refresh can hand back a narrower token than the session
/// started with.
pub const SCOPES: &[&str] = &["user:profile", "user:inference", "user:sessions:claude_code"];
/// How long before the stated expiry a token is treated as spent. The access token outlives
/// sign-in by hours, so a session that is still running when it lapses would otherwise fail its
/// next request; refreshing early also keeps a request that starts just under the wire valid for
/// its whole round trip. Claude Code uses the same five minutes.
const EXPIRY_LEEWAY_MS: u64 = 300_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnthropicOAuth {
    pub refresh_token: String,
    pub access_token: String,
    /// Expiration as milliseconds since UNIX epoch.
    pub expires_ms: u64,
    /// Scopes this token was granted. Empty for credentials stored before scopes were tracked,
    /// in which case [`SCOPES`] is sent instead.
    #[serde(default)]
    pub scopes: Vec<String>,
}

impl AnthropicOAuth {
    pub fn is_expired(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.expires_ms <= now.saturating_add(EXPIRY_LEEWAY_MS)
    }

    fn scopes_to_request(&self) -> String {
        if self.scopes.is_empty() {
            SCOPES.join(" ")
        } else {
            self.scopes.join(" ")
        }
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
        scope = urlencoding(&SCOPES.join(" ")),
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

    token_response_to_auth(token, None)
}

pub async fn refresh_token(
    client: &dyn HttpClient,
    current: &AnthropicOAuth,
) -> Result<AnthropicOAuth> {
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "refresh_token": current.refresh_token,
        "client_id": CLIENT_ID,
        "scope": current.scopes_to_request(),
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

    token_response_to_auth(token, Some(current))
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    /// Absent whenever the server keeps the current refresh token alive across a refresh, which
    /// it is free to do. Treating it as required would throw away a working credential.
    #[serde(default)]
    refresh_token: Option<String>,
    access_token: String,
    expires_in: u64,
    #[serde(default)]
    scope: Option<String>,
}

fn token_response_to_auth(
    token: TokenResponse,
    current: Option<&AnthropicOAuth>,
) -> Result<AnthropicOAuth> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let refresh_token = token
        .refresh_token
        .or_else(|| current.map(|current| current.refresh_token.clone()))
        .context("OAuth response carried no refresh token and none was already stored")?;

    let scopes = match token.scope {
        Some(scope) => scope.split_whitespace().map(str::to_string).collect(),
        None => current.map_or_else(Vec::new, |current| current.scopes.clone()),
    };

    Ok(AnthropicOAuth {
        refresh_token,
        access_token: token.access_token,
        expires_ms: now + (token.expires_in * 1000),
        scopes,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn stored_token() -> AnthropicOAuth {
        AnthropicOAuth {
            refresh_token: "old-refresh".into(),
            access_token: "old-access".into(),
            expires_ms: 0,
            scopes: vec!["user:profile".into(), "user:inference".into()],
        }
    }

    #[test]
    fn refresh_response_without_a_refresh_token_keeps_the_stored_one() {
        let response: TokenResponse =
            serde_json::from_str(r#"{"access_token":"new-access","expires_in":3600}"#).unwrap();

        let refreshed = token_response_to_auth(response, Some(&stored_token())).unwrap();

        assert_eq!(refreshed.refresh_token, "old-refresh");
        assert_eq!(refreshed.access_token, "new-access");
        assert_eq!(refreshed.scopes, stored_token().scopes);
    }

    #[test]
    fn refresh_response_rotating_the_refresh_token_replaces_it() {
        let response: TokenResponse = serde_json::from_str(
            r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600,"scope":"user:profile user:inference"}"#,
        )
        .unwrap();

        let refreshed = token_response_to_auth(response, Some(&stored_token())).unwrap();

        assert_eq!(refreshed.refresh_token, "new-refresh");
        assert_eq!(refreshed.scopes, ["user:profile", "user:inference"]);
    }

    #[test]
    fn sign_in_without_a_refresh_token_is_rejected() {
        let response: TokenResponse =
            serde_json::from_str(r#"{"access_token":"new-access","expires_in":3600}"#).unwrap();

        assert!(token_response_to_auth(response, None).is_err());
    }

    #[test]
    fn a_token_inside_the_refresh_leeway_counts_as_expired() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let mut auth = stored_token();
        auth.expires_ms = now + EXPIRY_LEEWAY_MS / 2;
        assert!(auth.is_expired());

        auth.expires_ms = now + EXPIRY_LEEWAY_MS * 2;
        assert!(!auth.is_expired());
    }

    #[test]
    fn credentials_stored_before_scopes_were_tracked_fall_back_to_the_defaults() {
        let auth: AnthropicOAuth = serde_json::from_str(
            r#"{"refresh_token":"r","access_token":"a","expires_ms":1}"#,
        )
        .unwrap();

        assert_eq!(auth.scopes_to_request(), SCOPES.join(" "));
    }
}
