//! Turns an operation and what the user typed into an HTTP request, and sends it.

use crate::{
    config::{HeaderEntry, RequestDraft},
    spec::{ApiKeyLocation, ParameterLocation, SchemeKind, Spec, is_json_content_type},
    vars::{self, SECRET_PREFIX},
};
use anyhow::{Context as _, Result};
use base64::Engine as _;
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, HttpRequestExt as _, RedirectPolicy, http};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

/// Bodies past this are cut, so a download doesn't fill the editor.
const MAX_RESPONSE_BYTES: usize = 20 * 1024 * 1024;
pub const TOKEN_VARIABLE: &str = "token";
pub const REFRESH_TOKEN_VARIABLE: &str = "refreshToken";

/// Where a header the request didn't define comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeaderOrigin {
    Spec,
    Collection,
    Scheme(String),
}

impl HeaderOrigin {
    pub fn label(&self) -> String {
        match self {
            HeaderOrigin::Spec => "spec".to_string(),
            HeaderOrigin::Collection => "coleção".to_string(),
            HeaderOrigin::Scheme(name) => format!("coleção · {name}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlannedHeader {
    pub name: String,
    /// Still with its `{{placeholders}}`.
    pub value: String,
    pub origin: HeaderOrigin,
    pub enabled: bool,
}

/// What authentication a request will carry, for the Auth tab.
#[derive(Clone, Debug, PartialEq)]
pub struct AuthPlan {
    pub scheme: String,
    pub summary: String,
    /// Why it won't be sent, when it won't.
    pub missing: Option<String>,
}

/// The values requests are filled with.
pub trait Variables {
    fn get(&self, name: &str) -> Option<String>;
}

impl<F: Fn(&str) -> Option<String>> Variables for F {
    fn get(&self, name: &str) -> Option<String> {
        self(name)
    }
}

pub fn api_key_secret(scheme: &str) -> String {
    format!("{SECRET_PREFIX}apiKey.{scheme}")
}

pub fn basic_secret(scheme: &str, part: &str) -> String {
    format!("{SECRET_PREFIX}basic.{scheme}.{part}")
}

/// Headers the request inherits, in the order they apply: the spec's, the collection's,
/// then the security schemes'. The request's own headers go on top of these.
pub fn inherited_headers(
    spec: &Spec,
    operation_key: &str,
    collection_headers: &[HeaderEntry],
    variables: &dyn Variables,
    has_body: bool,
) -> Vec<PlannedHeader> {
    let mut headers = Vec::new();
    let Some(operation) = spec.operation(operation_key) else {
        return headers;
    };
    if let Some(content_type) = operation
        .success_response()
        .and_then(|response| response.content_type.clone())
        .filter(|content_type| content_type != "*/*")
    {
        headers.push(PlannedHeader {
            name: "Accept".to_string(),
            value: content_type,
            origin: HeaderOrigin::Spec,
            enabled: true,
        });
    }
    if has_body && let Some(body) = &operation.request_body {
        headers.push(PlannedHeader {
            name: "Content-Type".to_string(),
            value: body.content_type.clone(),
            origin: HeaderOrigin::Spec,
            enabled: true,
        });
    }
    for header in collection_headers {
        headers.push(PlannedHeader {
            name: header.name.clone(),
            value: header.value.clone(),
            origin: HeaderOrigin::Collection,
            enabled: header.enabled,
        });
    }
    for scheme in spec.schemes_for(operation) {
        let origin = HeaderOrigin::Scheme(scheme.name.clone());
        match &scheme.kind {
            SchemeKind::Bearer { .. } | SchemeKind::OAuth2 => {
                if variables.get(TOKEN_VARIABLE).is_some() {
                    headers.push(PlannedHeader {
                        name: "Authorization".to_string(),
                        value: format!("Bearer {{{{{TOKEN_VARIABLE}}}}}"),
                        origin,
                        enabled: true,
                    });
                }
            }
            SchemeKind::Basic => {
                let username = variables.get(&basic_secret(&scheme.name, "username"));
                let password = variables.get(&basic_secret(&scheme.name, "password"));
                if let (Some(username), Some(password)) = (username, password) {
                    let encoded = base64::engine::general_purpose::STANDARD
                        .encode(format!("{username}:{password}"));
                    headers.push(PlannedHeader {
                        name: "Authorization".to_string(),
                        value: format!("Basic {encoded}"),
                        origin,
                        enabled: true,
                    });
                }
            }
            SchemeKind::ApiKey { location, name } => {
                let secret = api_key_secret(&scheme.name);
                if variables.get(&secret).is_none() {
                    continue;
                }
                match location {
                    ApiKeyLocation::Header => headers.push(PlannedHeader {
                        name: name.clone(),
                        value: format!("{{{{{secret}}}}}"),
                        origin,
                        enabled: true,
                    }),
                    ApiKeyLocation::Cookie => headers.push(PlannedHeader {
                        name: "Cookie".to_string(),
                        value: format!("{name}={{{{{secret}}}}}"),
                        origin,
                        enabled: true,
                    }),
                    ApiKeyLocation::Query => {}
                }
            }
            SchemeKind::Unsupported(_) => {}
        }
    }
    headers
}

pub fn auth_plan(spec: &Spec, operation_key: &str, variables: &dyn Variables) -> Vec<AuthPlan> {
    let Some(operation) = spec.operation(operation_key) else {
        return Vec::new();
    };
    spec.schemes_for(operation)
        .into_iter()
        .map(|scheme| {
            let missing = match &scheme.kind {
                SchemeKind::Bearer { .. } | SchemeKind::OAuth2 => variables
                    .get(TOKEN_VARIABLE)
                    .is_none()
                    .then(|| "sem token: faça login na aba API".to_string()),
                SchemeKind::Basic => (variables
                    .get(&basic_secret(&scheme.name, "username"))
                    .is_none()
                    || variables
                        .get(&basic_secret(&scheme.name, "password"))
                        .is_none())
                .then(|| "usuário e senha não configurados".to_string()),
                SchemeKind::ApiKey { .. } => variables
                    .get(&api_key_secret(&scheme.name))
                    .is_none()
                    .then(|| "chave não configurada".to_string()),
                SchemeKind::Unsupported(kind) => Some(format!("{kind} não é suportado")),
            };
            AuthPlan {
                scheme: scheme.name.clone(),
                summary: scheme.summary(),
                missing,
            }
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
pub struct PreparedRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    /// Placeholders nothing defines; they are sent as typed.
    pub missing: Vec<String>,
}

impl PreparedRequest {
    /// A copy with credentials hidden, for showing and copying.
    pub fn masked(&self) -> Self {
        let mut masked = self.clone();
        for (name, value) in &mut masked.headers {
            let lower = name.to_ascii_lowercase();
            if lower == "authorization" {
                let scheme = value.split(' ').next().unwrap_or("").to_string();
                *value = format!(
                    "{scheme} {}",
                    mask(value.split_once(' ').map_or("", |(_, rest)| rest))
                );
            } else if lower.contains("key") || lower.contains("token") || lower == "cookie" {
                *value = mask(value);
            }
        }
        masked
    }

    pub fn curl(&self) -> String {
        let quote = |text: &str| format!("'{}'", text.replace('\'', "'\\''"));
        let mut command = format!("curl -X {} {}", self.method, quote(&self.url));
        for (name, value) in &self.headers {
            command.push_str(&format!(" \\\n  -H {}", quote(&format!("{name}: {value}"))));
        }
        if let Some(body) = &self.body {
            command.push_str(&format!(" \\\n  --data-raw {}", quote(body)));
        }
        command
    }
}

pub(crate) fn mask(value: &str) -> String {
    let count = value.chars().count();
    if count <= 12 {
        return "••••".to_string();
    }
    let start: String = value.chars().take(6).collect();
    let end: String = value.chars().skip(count - 3).collect();
    format!("{start}…{end}")
}

pub fn prepare(
    spec: &Spec,
    operation_key: &str,
    draft: &RequestDraft,
    collection_headers: &[HeaderEntry],
    variables: &dyn Variables,
) -> Result<PreparedRequest> {
    let operation = spec
        .operation(operation_key)
        .with_context(|| format!("{operation_key} não existe mais no spec"))?;
    let mut missing: Vec<String> = Vec::new();
    let mut fill = |text: &str| -> String {
        let substitution = vars::substitute(text, &|name| {
            vars::dynamic_value(name).or_else(|| variables.get(name))
        });
        for name in substitution.missing {
            if !missing.contains(&name) {
                missing.push(name);
            }
        }
        substitution.text
    };

    let mut url = fill(&draft.url);
    for parameter in &draft.path_params {
        let value = fill(&parameter.value);
        url = url.replace(
            &format!("{{{}}}", parameter.name),
            &urlencoding::encode(&value),
        );
    }
    let mut query: Vec<(String, String)> = draft
        .query
        .iter()
        .filter(|parameter| parameter.enabled && !parameter.name.is_empty())
        .map(|parameter| (parameter.name.clone(), fill(&parameter.value)))
        .collect();
    for scheme in spec.schemes_for(operation) {
        if let SchemeKind::ApiKey {
            location: ApiKeyLocation::Query,
            name,
        } = &scheme.kind
            && let Some(value) = variables.get(&api_key_secret(&scheme.name))
        {
            query.push((name.clone(), value));
        }
    }
    if !query.is_empty() {
        let encoded: Vec<String> = query
            .iter()
            .map(|(name, value)| {
                format!(
                    "{}={}",
                    urlencoding::encode(name),
                    urlencoding::encode(value)
                )
            })
            .collect();
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str(&encoded.join("&"));
    }

    let body = draft
        .body
        .as_ref()
        .filter(|body| !body.trim().is_empty())
        .map(|body| fill(body));

    let mut headers: Vec<(String, String)> = Vec::new();
    let mut set = |name: &str, value: String| {
        headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
        headers.push((name.to_string(), value));
    };
    for header in inherited_headers(
        spec,
        operation_key,
        collection_headers,
        variables,
        body.is_some(),
    ) {
        let disabled = draft
            .disabled_inherited
            .iter()
            .any(|name| name.eq_ignore_ascii_case(&header.name));
        if header.enabled && !disabled && !header.name.is_empty() {
            set(&header.name, fill(&header.value));
        }
    }
    for header in &draft.headers {
        if header.enabled && !header.name.is_empty() {
            set(&header.name, fill(&header.value));
        }
    }
    for parameter in &operation.parameters {
        if parameter.location == ParameterLocation::Path
            && !draft
                .path_params
                .iter()
                .any(|entry| entry.name == parameter.name)
        {
            missing.push(format!("{{{}}}", parameter.name));
        }
    }

    Ok(PreparedRequest {
        method: operation.method.clone(),
        url,
        headers,
        body,
        missing,
    })
}

#[derive(Clone, Debug)]
pub struct ReceivedResponse {
    pub status: u16,
    pub reason: Option<String>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub truncated: bool,
    pub elapsed: Duration,
}

impl ReceivedResponse {
    pub fn content_type(&self) -> Option<&str> {
        self.headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.as_str())
    }

    pub fn json(&self) -> Option<serde_json::Value> {
        let looks_like_json = self.content_type().is_none_or(is_json_content_type)
            || self
                .body
                .first()
                .is_some_and(|byte| matches!(byte, b'{' | b'['));
        if !looks_like_json {
            return None;
        }
        serde_json::from_slice(&self.body).ok()
    }

    /// The body for the editor: JSON pretty-printed, anything else as text.
    pub fn display_body(&self) -> String {
        if let Some(json) = self.json()
            && let Ok(pretty) = serde_json::to_string_pretty(&json)
        {
            return pretty;
        }
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

pub async fn execute(
    http_client: Arc<dyn HttpClient>,
    prepared: &PreparedRequest,
) -> Result<ReceivedResponse> {
    let method = http::Method::from_bytes(prepared.method.as_bytes())?;
    let mut builder = http::Request::builder()
        .method(method)
        .uri(prepared.url.as_str())
        .follow_redirects(RedirectPolicy::FollowAll);
    for (name, value) in &prepared.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let body = match &prepared.body {
        Some(body) => AsyncBody::from(body.clone()),
        None => AsyncBody::empty(),
    };
    let request = builder
        .body(body)
        .with_context(|| format!("URL inválida: {}", prepared.url))?;

    let started = Instant::now();
    let mut response = http_client.send(request).await?;
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.to_string(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    let mut body = Vec::new();
    let mut truncated = false;
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let read = response.body_mut().read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        if body.len() + read > MAX_RESPONSE_BYTES {
            body.extend_from_slice(&chunk[..MAX_RESPONSE_BYTES - body.len()]);
            truncated = true;
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Ok(ReceivedResponse {
        status: status.as_u16(),
        reason: status.canonical_reason().map(str::to_string),
        headers,
        body,
        truncated,
        elapsed: started.elapsed(),
    })
}

/// The `exp` claim of a JWT, in seconds since the epoch.
pub fn jwt_expiry(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    claims.get("exp")?.as_i64()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::ParamEntry, spec};
    use pretty_assertions::assert_eq;
    use std::path::Path;

    const DOCUMENT: &str = r##"
openapi: 3.0.3
info: { title: T, version: "1" }
security: [{ bearerAuth: [] }, { apiKeyAuth: [] }]
components:
  securitySchemes:
    bearerAuth: { type: http, scheme: bearer }
    apiKeyAuth: { type: apiKey, in: header, name: x-api-key }
paths:
  /users/{id}:
    patch:
      parameters:
        - { name: id, in: path, required: true, schema: { type: string } }
        - { name: notify, in: query, schema: { type: boolean } }
      requestBody:
        content: { application/json: { schema: { type: object } } }
      responses:
        '200': { description: ok, content: { application/json: { schema: { type: object } } } }
  /health:
    get:
      security: []
      responses: { '204': { description: ok } }
"##;

    fn spec() -> Spec {
        spec::load(Path::new("/s.yml"), DOCUMENT, &|_| {
            Err(anyhow::anyhow!("x"))
        })
        .unwrap()
    }

    fn variables(name: &str) -> Option<String> {
        match name {
            "baseUrl" => Some("https://api.trix.com.br".to_string()),
            "token" => Some("eyJ.abc.def".to_string()),
            "secret.apiKey.apiKeyAuth" => Some("k-123".to_string()),
            "tenant" => Some("trix".to_string()),
            _ => None,
        }
    }

    #[test]
    fn prepares_requests_with_inherited_headers() {
        let spec = spec();
        let draft = RequestDraft {
            url: "{{baseUrl}}/users/{id}".to_string(),
            path_params: vec![ParamEntry {
                name: "id".to_string(),
                value: "a b".to_string(),
                enabled: true,
            }],
            query: vec![ParamEntry {
                name: "notify".to_string(),
                value: "true".to_string(),
                enabled: true,
            }],
            headers: vec![HeaderEntry {
                name: "accept".to_string(),
                value: "*/*".to_string(),
                enabled: true,
            }],
            disabled_inherited: vec!["X-Debug".to_string()],
            body: Some(r#"{"tenant":"{{tenant}}","missing":"{{nope}}"}"#.to_string()),
        };
        let collection_headers = vec![
            HeaderEntry {
                name: "X-Tenant-Id".to_string(),
                value: "{{tenant}}".to_string(),
                enabled: true,
            },
            HeaderEntry {
                name: "X-Debug".to_string(),
                value: "1".to_string(),
                enabled: true,
            },
        ];
        let prepared = prepare(
            &spec,
            "PATCH /users/{id}",
            &draft,
            &collection_headers,
            &variables,
        )
        .unwrap();
        assert_eq!(
            prepared.url,
            "https://api.trix.com.br/users/a%20b?notify=true"
        );
        assert_eq!(
            prepared.headers,
            vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("X-Tenant-Id".to_string(), "trix".to_string()),
                (
                    "Authorization".to_string(),
                    "Bearer eyJ.abc.def".to_string()
                ),
                ("x-api-key".to_string(), "k-123".to_string()),
                ("accept".to_string(), "*/*".to_string()),
            ]
        );
        assert_eq!(
            prepared.body.as_deref(),
            Some(r#"{"tenant":"trix","missing":"{{nope}}"}"#)
        );
        assert_eq!(prepared.missing, vec!["nope".to_string()]);
        let masked = prepared.masked();
        assert_eq!(masked.headers[2].1, "Bearer ••••");
        assert!(
            prepared
                .curl()
                .starts_with("curl -X PATCH 'https://api.trix.com.br/users/a%20b?notify=true'")
        );
    }

    #[test]
    fn public_operations_carry_no_credentials() {
        let spec = spec();
        let draft = RequestDraft {
            url: "{{baseUrl}}/health".to_string(),
            ..RequestDraft::default()
        };
        let prepared = prepare(&spec, "GET /health", &draft, &[], &variables).unwrap();
        assert!(prepared.headers.is_empty());
        assert!(auth_plan(&spec, "GET /health", &variables).is_empty());
        let plans = auth_plan(&spec, "PATCH /users/{id}", &|_: &str| None);
        assert_eq!(plans.len(), 2);
        assert!(plans.iter().all(|plan| plan.missing.is_some()));
    }

    #[test]
    fn reads_jwt_expiry() {
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"exp":1790000000}"#);
        assert_eq!(jwt_expiry(&format!("h.{payload}.s")), Some(1_790_000_000));
        assert_eq!(jwt_expiry("not-a-jwt"), None);
    }
}
