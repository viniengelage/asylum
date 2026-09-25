//! `.asylum/api/<id>.json`: what the spec doesn't say and a team wants to share. Credentials
//! and tokens never go here; they live in the keychain of the active profile.

use crate::{
    jsonpath,
    schema::{self, property_names, property_paths},
    spec::{SchemeKind, Spec},
    vars::SECRET_PREFIX,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const COLLECTIONS_DIR: &str = ".asylum/api";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CollectionFile {
    /// The spec, relative to the folder holding `.asylum`.
    pub spec: String,
    #[serde(default)]
    pub environments: Vec<Environment>,
    #[serde(default)]
    pub headers: Vec<HeaderEntry>,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub saved: Vec<SavedRequest>,
    /// Values taken from responses into variables, per operation.
    #[serde(default)]
    pub captures: Vec<CaptureRule>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CaptureRule {
    pub operation: String,
    /// JSONPath into the response body.
    pub path: String,
    pub variable: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Environment {
    pub name: String,
    #[serde(default)]
    pub variables: Vec<Variable>,
}

impl Environment {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.variables
            .iter()
            .find(|variable| variable.name == name)
            .map(|variable| variable.value.as_str())
    }

    pub fn set(&mut self, name: &str, value: String) {
        match self
            .variables
            .iter_mut()
            .find(|variable| variable.name == name)
        {
            Some(variable) => variable.value = value,
            None => self.variables.push(Variable {
                name: name.to_string(),
                value,
            }),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Variable {
    pub name: String,
    pub value: String,
}

fn enabled_by_default() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HeaderEntry {
    pub name: String,
    pub value: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuthConfig {
    #[serde(default)]
    pub login: Option<LoginConfig>,
    #[serde(default = "enabled_by_default")]
    pub renew_before_expiry: bool,
    #[serde(default = "enabled_by_default")]
    pub retry_on_unauthorized: bool,
    /// Keep the session in the keychain between launches.
    #[serde(default = "enabled_by_default")]
    pub remember_session: bool,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            login: None,
            renew_before_expiry: true,
            retry_on_unauthorized: true,
            remember_session: true,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LoginConfig {
    /// The operation that logs in (`POST /auth/login`).
    pub operation: String,
    /// The bearer scheme the token is sent as.
    pub scheme: String,
    /// The JSON sent to log in; credentials are `{{secret.*}}` placeholders.
    pub body: String,
    pub token_path: String,
    #[serde(default)]
    pub refresh_token_path: Option<String>,
    /// Seconds until expiry; without it, the token's JWT `exp` claim is used.
    #[serde(default)]
    pub expires_in_path: Option<String>,
    #[serde(default)]
    pub refresh_operation: Option<String>,
    #[serde(default)]
    pub refresh_body: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RequestDraft {
    pub url: String,
    #[serde(default)]
    pub path_params: Vec<ParamEntry>,
    #[serde(default)]
    pub query: Vec<ParamEntry>,
    /// Headers of this request only.
    #[serde(default)]
    pub headers: Vec<HeaderEntry>,
    /// Inherited headers (from the spec or the collection) turned off for this request.
    #[serde(default)]
    pub disabled_inherited: Vec<String>,
    #[serde(default)]
    pub body: Option<String>,
    /// Fields of a form or multipart body, used instead of `body` for those content types.
    #[serde(default)]
    pub form: Vec<FormField>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FormField {
    pub name: String,
    /// The text, or for a file field the path of the file to send.
    pub value: String,
    #[serde(default)]
    pub is_file: bool,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyKind {
    Json,
    Text,
    UrlEncoded,
    Multipart,
}

impl BodyKind {
    pub fn for_content_type(content_type: &str) -> Self {
        let essence = content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if essence == "multipart/form-data" || essence.starts_with("multipart/") {
            BodyKind::Multipart
        } else if essence == "application/x-www-form-urlencoded" {
            BodyKind::UrlEncoded
        } else if crate::spec::is_json_content_type(&essence) {
            BodyKind::Json
        } else {
            BodyKind::Text
        }
    }

    pub fn is_form(self) -> bool {
        matches!(self, BodyKind::UrlEncoded | BodyKind::Multipart)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ParamEntry {
    pub name: String,
    pub value: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedRequest {
    pub id: String,
    pub name: String,
    pub operation: String,
    pub draft: RequestDraft,
}

pub fn parse(text: &str) -> anyhow::Result<CollectionFile> {
    Ok(serde_json::from_str(text)?)
}

pub fn serialize(file: &CollectionFile) -> anyhow::Result<String> {
    let mut text = serde_json::to_string_pretty(file)?;
    text.push('\n');
    Ok(text)
}

/// A file name for the collection: the spec's title as a slug.
pub fn slug(title: &str) -> String {
    let mut slug = String::new();
    for character in title.chars() {
        let character = match character {
            'á' | 'à' | 'â' | 'ã' | 'ä' | 'Á' | 'À' | 'Â' | 'Ã' => 'a',
            'é' | 'ê' | 'É' | 'Ê' => 'e',
            'í' | 'Í' => 'i',
            'ó' | 'ô' | 'õ' | 'Ó' | 'Ô' | 'Õ' => 'o',
            'ú' | 'ü' | 'Ú' => 'u',
            'ç' | 'Ç' => 'c',
            other => other,
        };
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-').to_string();
    if slug.is_empty() {
        "api".to_string()
    } else {
        slug
    }
}

/// A new collection for a spec: one environment per server (or an empty `local` one) and
/// the login guessed from the operations.
pub fn initial(spec: &Spec, spec_relative_path: String) -> CollectionFile {
    let mut environments: Vec<Environment> = spec
        .servers
        .iter()
        .enumerate()
        .map(|(index, server)| Environment {
            name: server
                .description
                .clone()
                .filter(|description| description.len() <= 24)
                .unwrap_or_else(|| {
                    if index == 0 {
                        "dev".to_string()
                    } else {
                        format!("servidor {}", index + 1)
                    }
                }),
            variables: vec![Variable {
                name: "baseUrl".to_string(),
                value: server.url.clone(),
            }],
        })
        .collect();
    if environments.is_empty() {
        environments.push(Environment {
            name: "local".to_string(),
            variables: vec![Variable {
                name: "baseUrl".to_string(),
                value: String::new(),
            }],
        });
    }
    CollectionFile {
        spec: spec_relative_path,
        environments,
        headers: Vec::new(),
        auth: AuthConfig {
            login: guess_login(spec),
            ..AuthConfig::default()
        },
        saved: Vec::new(),
        captures: Vec::new(),
    }
}

fn is_password_like(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("password") || lower.contains("senha") || lower == "secret" || lower == "pin"
}

/// The operation that logs in: a POST whose body asks for a password, preferring paths that
/// sound like it.
pub fn guess_login(spec: &Spec) -> Option<LoginConfig> {
    let scheme = spec
        .security_schemes
        .iter()
        .find(|scheme| matches!(scheme.kind, SchemeKind::Bearer { .. } | SchemeKind::OAuth2))?;
    let score = |path: &str, title: &str| -> i32 {
        let text = format!("{path} {title}").to_ascii_lowercase();
        let mut score = 0;
        for word in ["login", "signin", "sign-in", "session", "entrar"] {
            if text.contains(word) {
                score += 3;
            }
        }
        for word in ["auth", "token"] {
            if text.contains(word) {
                score += 1;
            }
        }
        for word in [
            "reset", "recovery", "recuper", "forgot", "change", "register", "signup",
        ] {
            if text.contains(word) {
                score -= 5;
            }
        }
        score
    };
    let login = spec
        .operations
        .iter()
        .filter(|operation| operation.method == "POST")
        .filter_map(|operation| {
            let schema = operation.request_body.as_ref()?.schema.as_ref()?;
            let fields = property_names(&spec.document, schema);
            fields.iter().any(|field| is_password_like(field)).then(|| {
                (
                    score(&operation.path, &operation.title()),
                    operation,
                    fields,
                )
            })
        })
        .max_by_key(|(score, _, _)| *score)?;
    let (_, operation, fields) = login;

    let body: serde_json::Map<String, Value> = fields
        .iter()
        .map(|field| {
            (
                field.clone(),
                Value::String(format!("{{{{{SECRET_PREFIX}{field}}}}}")),
            )
        })
        .collect();
    let response_paths = operation
        .success_response()
        .and_then(|response| response.schema.as_ref())
        .map(|schema| property_paths(&spec.document, schema))
        .unwrap_or_default();
    let find = |candidates: &[&str]| -> Option<String> {
        response_paths
            .iter()
            .find(|path| {
                let last = path.rsplit('.').next().unwrap_or(path).to_ascii_lowercase();
                candidates.contains(&last.as_str())
            })
            .map(|path| format!("$.{path}"))
    };
    let token_path = find(&[
        "accesstoken",
        "access_token",
        "token",
        "jwt",
        "idtoken",
        "id_token",
    ])
    .unwrap_or_else(|| "$.accessToken".to_string());
    let refresh_token_path = find(&["refreshtoken", "refresh_token"]);
    let expires_in_path = find(&["expiresin", "expires_in"]);

    let refresh_operation = refresh_token_path.as_ref().and_then(|_| {
        spec.operations.iter().find(|candidate| {
            candidate.method == "POST"
                && candidate.key != operation.key
                && candidate.path.to_ascii_lowercase().contains("refresh")
        })
    });
    let refresh_body = refresh_operation.map(|refresh| {
        let field = refresh
            .request_body
            .as_ref()
            .and_then(|body| body.schema.as_ref())
            .and_then(|schema| {
                property_names(&spec.document, schema)
                    .into_iter()
                    .find(|name| name.to_ascii_lowercase().contains("refresh"))
            })
            .unwrap_or_else(|| "refreshToken".to_string());
        serde_json::json!({ field: "{{refreshToken}}" }).to_string()
    });

    Some(LoginConfig {
        operation: operation.key.clone(),
        scheme: scheme.name.clone(),
        body: serde_json::to_string_pretty(&Value::Object(body)).unwrap_or_default(),
        token_path,
        refresh_token_path,
        expires_in_path,
        refresh_operation: refresh_operation.map(|refresh| refresh.key.clone()),
        refresh_body,
    })
}

/// The body a new request starts with: the spec's example, else one built from the schema.
pub fn example_body(spec: &Spec, operation_key: &str) -> Option<String> {
    let operation = spec.operation(operation_key)?;
    let body = operation.request_body.as_ref()?;
    if BodyKind::for_content_type(&body.content_type).is_form() {
        return None;
    }
    let value = match (&body.example, &body.schema) {
        (Some(example), _) => example.clone(),
        (None, Some(schema)) => schema::example(&spec.document, schema),
        (None, None) => return None,
    };
    serde_json::to_string_pretty(&value).ok()
}

/// A starting draft for an operation: `{{baseUrl}}` plus the path, its parameters, and the
/// example body.
pub fn initial_draft(spec: &Spec, operation_key: &str) -> RequestDraft {
    use crate::spec::ParameterLocation;
    let Some(operation) = spec.operation(operation_key) else {
        return RequestDraft::default();
    };
    let value_for = |parameter: &crate::spec::Parameter| -> String {
        parameter
            .example
            .as_ref()
            .or_else(|| {
                parameter
                    .schema
                    .as_ref()
                    .and_then(|schema| schema.get("example"))
            })
            .or_else(|| {
                parameter
                    .schema
                    .as_ref()
                    .and_then(|schema| schema.get("default"))
            })
            .map(|value| match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default()
    };
    RequestDraft {
        url: format!("{{{{baseUrl}}}}{}", operation.path),
        path_params: operation
            .parameters
            .iter()
            .filter(|parameter| parameter.location == ParameterLocation::Path)
            .map(|parameter| ParamEntry {
                name: parameter.name.clone(),
                value: value_for(parameter),
                enabled: true,
            })
            .collect(),
        query: operation
            .parameters
            .iter()
            .filter(|parameter| parameter.location == ParameterLocation::Query)
            .map(|parameter| ParamEntry {
                name: parameter.name.clone(),
                value: value_for(parameter),
                enabled: parameter.required,
            })
            .collect(),
        headers: operation
            .parameters
            .iter()
            .filter(|parameter| parameter.location == ParameterLocation::Header)
            .map(|parameter| HeaderEntry {
                name: parameter.name.clone(),
                value: value_for(parameter),
                enabled: parameter.required,
            })
            .collect(),
        disabled_inherited: Vec::new(),
        body: example_body(spec, operation_key),
        form: form_fields(spec, operation_key),
    }
}

/// One field per property of a form or multipart body; `format: binary` ones are files.
pub fn form_fields(spec: &Spec, operation_key: &str) -> Vec<FormField> {
    let Some(body) = spec
        .operation(operation_key)
        .and_then(|operation| operation.request_body.as_ref())
    else {
        return Vec::new();
    };
    if !BodyKind::for_content_type(&body.content_type).is_form() {
        return Vec::new();
    }
    let Some(schema) = body.schema.as_ref() else {
        return Vec::new();
    };
    let resolved = crate::spec::deref(&spec.document, schema);
    let required: Vec<&str> = resolved
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    property_names(&spec.document, schema)
        .into_iter()
        .map(|name| {
            let property = resolved
                .get("properties")
                .and_then(|properties| properties.get(&name))
                .map(|property| crate::spec::deref(&spec.document, property));
            let is_file = property.is_some_and(|property| {
                let format = property.get("format").and_then(Value::as_str);
                let item_format = property
                    .get("items")
                    .and_then(|items| items.get("format"))
                    .and_then(Value::as_str);
                matches!(format, Some("binary" | "base64"))
                    || matches!(item_format, Some("binary" | "base64"))
            });
            FormField {
                enabled: required.contains(&name.as_str()) || required.is_empty(),
                name,
                value: String::new(),
                is_file,
            }
        })
        .collect()
}

pub fn login_paths_are_valid(login: &LoginConfig) -> bool {
    jsonpath::is_valid(&login.token_path)
        && login
            .refresh_token_path
            .as_deref()
            .is_none_or(jsonpath::is_valid)
        && login
            .expires_in_path
            .as_deref()
            .is_none_or(jsonpath::is_valid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec;
    use pretty_assertions::assert_eq;
    use std::path::Path;

    #[test]
    fn slugs_titles() {
        assert_eq!(slug("Documentação TRX 3.0"), "documentacao-trx-3-0");
        assert_eq!(slug("  "), "api");
    }

    #[test]
    fn guesses_the_login_operation() {
        let text = r##"
openapi: 3.0.3
info: { title: T, version: "1" }
components:
  securitySchemes:
    bearerAuth: { type: http, scheme: bearer }
  schemas:
    AccessToken:
      type: object
      properties: { accessToken: { type: string }, refreshToken: { type: string } }
paths:
  /auth/recovery-password:
    post:
      requestBody: { content: { application/json: { schema: { properties: { password: { type: string } } } } } }
      responses: { '200': { description: ok } }
  /auth/:
    post:
      summary: Fazer login
      requestBody:
        content:
          application/json:
            schema: { type: object, properties: { email: { type: string }, password: { type: string } } }
      responses:
        '200': { description: ok, content: { application/json: { schema: { $ref: '#/components/schemas/AccessToken' } } } }
  /auth/refresh-token:
    post:
      requestBody: { content: { application/json: { schema: { properties: { refreshToken: { type: string } } } } } }
      responses: { '200': { description: ok } }
"##;
        let spec = spec::load(Path::new("/s.yml"), text, &|_| Err(anyhow::anyhow!("x"))).unwrap();
        let login = guess_login(&spec).unwrap();
        assert_eq!(login.operation, "POST /auth/");
        assert_eq!(login.token_path, "$.accessToken");
        assert_eq!(login.refresh_token_path.as_deref(), Some("$.refreshToken"));
        assert_eq!(
            login.refresh_operation.as_deref(),
            Some("POST /auth/refresh-token")
        );
        assert_eq!(
            serde_json::from_str::<Value>(&login.body).unwrap(),
            serde_json::json!({ "email": "{{secret.email}}", "password": "{{secret.password}}" })
        );
        assert_eq!(
            login.refresh_body.as_deref(),
            Some(r#"{"refreshToken":"{{refreshToken}}"}"#)
        );

        let file = initial(&spec, "swagger.yaml".to_string());
        assert_eq!(file.environments[0].name, "local");
        let round_trip = parse(&serialize(&file).unwrap()).unwrap();
        assert_eq!(round_trip, file);
    }
}
