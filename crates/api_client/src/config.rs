//! `.asylum/api/<id>.json`: what the spec doesn't say and a team wants to share. Credentials
//! and tokens never go here; they live in the keychain of the active profile.

use crate::{
    jsonpath,
    schema::{self, property_names, property_paths},
    spec::{Operation, SchemeKind, SecurityScheme, Spec},
    vars::SECRET_PREFIX,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const COLLECTIONS_DIR: &str = ".asylum/api";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CollectionFile {
    /// The spec, relative to the folder holding `.asylum`. Empty for a collection imported
    /// without one.
    #[serde(default)]
    pub spec: String,
    /// The Postman collection it came from, so exporting it back replaces that collection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postman_id: Option<String>,
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
                secret: false,
            }),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Variable {
    pub name: String,
    pub value: String,
    /// The value lives in the keychain; `value` stays empty in the file.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub secret: bool,
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
    /// How `body` and `refresh_body` go out. Without it, the operation's request body in the
    /// spec decides, and JSON when there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<LoginBodyFormat>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoginBodyFormat {
    Json,
    /// `application/x-www-form-urlencoded`, built from the fields of the JSON object in `body`;
    /// what OAuth token endpoints like Keycloak's accept.
    Form,
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

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SavedRequest {
    pub id: String,
    pub name: String,
    pub operation: String,
    pub draft: RequestDraft,
    /// Set when `operation` isn't one of the spec's, e.g. a request imported from Postman.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postman_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scripts: Vec<ImportedScript>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptKind {
    Test,
    #[default]
    Unsupported,
}

/// A Postman script kept as written. Nothing runs JavaScript here, so it stays off.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ImportedScript {
    pub kind: ScriptKind,
    /// `prerequest` or `test`, as Postman runs them.
    pub listen: String,
    pub source: String,
    #[serde(default)]
    pub enabled: bool,
}

pub fn parse(text: &str) -> anyhow::Result<CollectionFile> {
    Ok(serde_json::from_str(text)?)
}

pub fn serialize(value: &impl Serialize) -> anyhow::Result<String> {
    let mut text = serde_json::to_string_pretty(value)?;
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
                secret: false,
            }],
        })
        .collect();
    if environments.is_empty() {
        environments.push(Environment {
            name: "local".to_string(),
            variables: vec![Variable {
                name: "baseUrl".to_string(),
                value: String::new(),
                secret: false,
            }],
        });
    }
    CollectionFile {
        spec: spec_relative_path,
        postman_id: None,
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
        format: None,
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
        url: operation.url_template(),
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

pub const POSTMAN_SCHEMA_V21: &str =
    "https://schema.getpostman.com/json/collection/v2.1.0/collection.json";

#[derive(Clone, Debug, PartialEq)]
pub enum PostmanError {
    Json {
        line: usize,
        column: usize,
        message: String,
    },
    NotPostman,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PostmanAuth {
    Bearer {
        token: String,
    },
    Basic {
        username: String,
        password: String,
    },
    ApiKey {
        key: String,
        value: String,
        in_query: bool,
    },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PostmanVariable {
    pub name: String,
    pub value: String,
    pub secret: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PostmanRequest {
    pub postman_id: Option<String>,
    pub name: String,
    pub folder: Option<String>,
    pub method: String,
    /// What comes before the path: `{{baseUrl}}` or `https://host`.
    pub origin: String,
    /// The path as written, with `:id`, `{{id}}` or `{id}` segments left as they are.
    pub path_segments: Vec<String>,
    pub path_variables: Vec<ParamEntry>,
    pub query: Vec<ParamEntry>,
    pub headers: Vec<HeaderEntry>,
    pub body: Option<String>,
    pub form: Vec<FormField>,
    /// `(variable, JSONPath)` taken from `pm.environment.set(…, pm.response.json()…)`.
    pub captures: Vec<(String, String)>,
    pub scripts: Vec<ImportedScript>,
    pub tests: usize,
    pub auth: Option<PostmanAuth>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PostmanCollection {
    pub postman_id: Option<String>,
    pub name: String,
    pub version: String,
    pub folder_count: usize,
    pub requests: Vec<PostmanRequest>,
    pub variables: Vec<PostmanVariable>,
    pub auth: Option<PostmanAuth>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PostmanEnvironment {
    pub name: String,
    pub variables: Vec<PostmanVariable>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PostmanFile {
    Collection(PostmanCollection),
    Environment(PostmanEnvironment),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PostmanImport {
    pub collections: Vec<PostmanCollection>,
    pub environments: Vec<PostmanEnvironment>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImportCounts {
    pub folders: usize,
    pub requests: usize,
    pub environments: usize,
    pub variables: usize,
    pub secrets: usize,
    pub auth: bool,
    pub captures: usize,
    pub tests: usize,
    pub unsupported: usize,
}

impl PostmanImport {
    pub fn add(&mut self, file: PostmanFile) {
        match file {
            PostmanFile::Collection(collection) => self.collections.push(collection),
            PostmanFile::Environment(environment) => self.environments.push(environment),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.collections.is_empty() && self.environments.is_empty()
    }

    fn all_variables(&self) -> impl Iterator<Item = &PostmanVariable> {
        self.environments
            .iter()
            .flat_map(|environment| environment.variables.iter())
            .chain(
                self.collections
                    .iter()
                    .flat_map(|collection| collection.variables.iter()),
            )
    }

    fn requests(&self) -> impl Iterator<Item = &PostmanRequest> {
        self.collections
            .iter()
            .flat_map(|collection| collection.requests.iter())
    }

    pub fn counts(&self, spec: Option<&Spec>) -> ImportCounts {
        ImportCounts {
            folders: self
                .collections
                .iter()
                .map(|collection| collection.folder_count)
                .sum(),
            requests: self.requests().count(),
            environments: self.environments.len(),
            variables: self.all_variables().count(),
            secrets: self
                .all_variables()
                .filter(|variable| variable.secret)
                .count(),
            auth: self
                .collections
                .iter()
                .filter_map(|collection| collection.auth.as_ref())
                .any(|auth| auth_is_kept(auth, spec)),
            captures: self.requests().map(|request| request.captures.len()).sum(),
            tests: self.requests().map(|request| request.tests).sum(),
            unsupported: self
                .requests()
                .flat_map(|request| request.scripts.iter())
                .filter(|script| script.kind == ScriptKind::Unsupported)
                .count(),
        }
    }

    /// For each request, in order, the spec operation it matches and how many leading path
    /// segments the spec's server already covers.
    pub fn matches<'a>(&self, spec: &'a Spec) -> Vec<Option<(&'a Operation, usize)>> {
        self.requests()
            .map(|request| match_operation(spec, request))
            .collect()
    }

    /// A placeholder's value as the imported files define it, for credentials that have to
    /// be stored as plain values.
    fn resolve(&self, text: &str) -> Option<String> {
        let trimmed = text.trim();
        if !trimmed.contains("{{") {
            return (!trimmed.is_empty()).then(|| trimmed.to_string());
        }
        let name = trimmed.strip_prefix("{{")?.strip_suffix("}}")?.trim();
        self.all_variables()
            .find(|variable| variable.name == name && !variable.value.is_empty())
            .map(|variable| variable.value.clone())
    }
}

pub fn parse_postman(text: &str) -> Result<PostmanFile, PostmanError> {
    let value: Value = serde_json::from_str(text).map_err(|error| {
        let suffix = format!(" at line {} column {}", error.line(), error.column());
        let message = error.to_string();
        PostmanError::Json {
            line: error.line(),
            column: error.column(),
            message: message.trim_end_matches(&suffix).to_string(),
        }
    })?;
    if let Some(schema) = value.pointer("/info/schema").and_then(Value::as_str) {
        let version = if schema.contains("v2.1") {
            "2.1"
        } else if schema.contains("v2.0") {
            "2.0"
        } else {
            return Err(PostmanError::NotPostman);
        };
        return Ok(PostmanFile::Collection(read_collection(&value, version)));
    }
    if value.get("info").is_none()
        && let Some(values) = value.get("values").and_then(Value::as_array)
    {
        return Ok(PostmanFile::Environment(PostmanEnvironment {
            name: text_of(value.get("name")),
            variables: values.iter().filter_map(read_variable).collect(),
        }));
    }
    Err(PostmanError::NotPostman)
}

fn text_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn is_disabled(value: &Value) -> bool {
    value
        .get("disabled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn read_variable(value: &Value) -> Option<PostmanVariable> {
    let name = text_of(value.get("key"));
    if name.is_empty() {
        return None;
    }
    Some(PostmanVariable {
        name,
        value: text_of(value.get("value")),
        secret: value.get("type").and_then(Value::as_str) == Some("secret"),
    })
}

fn read_collection(value: &Value, version: &str) -> PostmanCollection {
    let mut collection = PostmanCollection {
        postman_id: value
            .pointer("/info/_postman_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        name: text_of(value.pointer("/info/name")),
        version: version.to_string(),
        variables: value
            .get("variable")
            .and_then(Value::as_array)
            .map(|variables| variables.iter().filter_map(read_variable).collect())
            .unwrap_or_default(),
        auth: value.get("auth").and_then(read_auth),
        ..PostmanCollection::default()
    };
    if let Some(items) = value.get("item").and_then(Value::as_array) {
        read_items(items, None, &mut collection);
    }
    collection
}

fn read_items(items: &[Value], folder: Option<&str>, collection: &mut PostmanCollection) {
    for item in items {
        let name = text_of(item.get("name"));
        if let Some(children) = item.get("item").and_then(Value::as_array) {
            collection.folder_count += 1;
            let path = match folder {
                Some(parent) => format!("{parent} / {name}"),
                None => name,
            };
            read_items(children, Some(&path), collection);
        } else if let Some(request) = item.get("request") {
            collection
                .requests
                .push(read_request(item, request, name, folder));
        }
    }
}

fn read_request(
    item: &Value,
    request: &Value,
    name: String,
    folder: Option<&str>,
) -> PostmanRequest {
    // Items without an id (common in v2.0 exports) still need an identity to merge by.
    let postman_id = item
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("name:{}/{name}", folder.unwrap_or("")));
    let mut imported = PostmanRequest {
        postman_id: Some(postman_id),
        name,
        folder: folder.map(str::to_string),
        method: "GET".to_string(),
        ..PostmanRequest::default()
    };
    match request {
        Value::String(url) => read_url(&Value::String(url.clone()), &mut imported),
        Value::Object(_) => {
            if let Some(method) = request.get("method").and_then(Value::as_str) {
                imported.method = method.to_ascii_uppercase();
            }
            if let Some(url) = request.get("url") {
                read_url(url, &mut imported);
            }
            read_headers(request.get("header"), &mut imported);
            if let Some(body) = request.get("body") {
                read_body(body, &mut imported);
            }
            imported.auth = request.get("auth").and_then(read_auth);
        }
        _ => {}
    }
    if let Some(events) = item.get("event").and_then(Value::as_array) {
        for event in events {
            let listen = text_of(event.get("listen"));
            let source = match event.pointer("/script/exec") {
                Some(Value::Array(lines)) => lines
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("\n"),
                Some(Value::String(source)) => source.clone(),
                _ => String::new(),
            };
            if source.trim().is_empty() {
                continue;
            }
            classify_script(&listen, &source, is_disabled(event), &mut imported);
        }
    }
    imported
}

fn read_url(url: &Value, request: &mut PostmanRequest) {
    let raw = match url {
        Value::String(raw) => raw.clone(),
        _ => text_of(url.get("raw")),
    };
    let (origin, segments, query) = split_raw_url(&raw);
    request.origin = origin;
    request.path_segments = segments;
    request.query = query;
    if let Value::Object(_) = url {
        if let Some(host) = url.get("host") {
            let host = match host {
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("."),
                other => text_of(Some(other)),
            };
            if !host.is_empty() {
                let protocol = text_of(url.get("protocol"));
                let port = text_of(url.get("port"));
                request.origin = match (protocol.is_empty(), port.is_empty()) {
                    (true, true) => host,
                    (true, false) => format!("{host}:{port}"),
                    (false, true) => format!("{protocol}://{host}"),
                    (false, false) => format!("{protocol}://{host}:{port}"),
                };
            }
        }
        match url.get("path") {
            Some(Value::Array(parts)) => {
                request.path_segments = parts
                    .iter()
                    .map(|part| text_of(Some(part)))
                    .filter(|part| !part.is_empty())
                    .collect();
            }
            Some(Value::String(path)) => {
                request.path_segments = path
                    .split('/')
                    .filter(|part| !part.is_empty())
                    .map(str::to_string)
                    .collect();
            }
            _ => {}
        }
        if let Some(query) = url.get("query").and_then(Value::as_array) {
            request.query = query
                .iter()
                .map(|parameter| ParamEntry {
                    name: text_of(parameter.get("key")),
                    value: text_of(parameter.get("value")),
                    enabled: !is_disabled(parameter),
                })
                .filter(|parameter| !parameter.name.is_empty())
                .collect();
        }
        if let Some(variables) = url.get("variable").and_then(Value::as_array) {
            request.path_variables = variables
                .iter()
                .map(|variable| ParamEntry {
                    name: text_of(variable.get("key")),
                    value: text_of(variable.get("value")),
                    enabled: true,
                })
                .filter(|variable| !variable.name.is_empty())
                .collect();
        }
    }
    for segment in &request.path_segments {
        if let Some(name) = segment.strip_prefix(':')
            && !request
                .path_variables
                .iter()
                .any(|variable| variable.name == name)
        {
            request.path_variables.push(ParamEntry {
                name: name.to_string(),
                value: String::new(),
                enabled: true,
            });
        }
    }
}

/// Splits `{{baseUrl}}/users/:id?active=true` into the origin, the path segments and the query.
fn split_raw_url(raw: &str) -> (String, Vec<String>, Vec<ParamEntry>) {
    let raw = raw.trim();
    let (base, query) = raw.split_once('?').unwrap_or((raw, ""));
    let path_start = if let Some(scheme_end) = base.find("://") {
        base[scheme_end + 3..]
            .find('/')
            .map_or(base.len(), |index| scheme_end + 3 + index)
    } else if base.starts_with("{{") {
        base.find("}}").map_or(base.len(), |index| index + 2)
    } else if base.starts_with('/') {
        0
    } else {
        base.find('/').unwrap_or(base.len())
    };
    let segments = base[path_start..]
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect();
    let query = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            ParamEntry {
                name: name.to_string(),
                value: value.to_string(),
                enabled: true,
            }
        })
        .collect();
    (base[..path_start].to_string(), segments, query)
}

fn read_headers(headers: Option<&Value>, request: &mut PostmanRequest) {
    match headers {
        Some(Value::Array(headers)) => {
            request.headers = headers
                .iter()
                .map(|header| HeaderEntry {
                    name: text_of(header.get("key")),
                    value: text_of(header.get("value")),
                    enabled: !is_disabled(header),
                })
                .filter(|header| !header.name.is_empty())
                .collect();
        }
        Some(Value::String(headers)) => {
            request.headers = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .map(|(name, value)| HeaderEntry {
                    name: name.trim().to_string(),
                    value: value.trim().to_string(),
                    enabled: true,
                })
                .collect();
        }
        _ => {}
    }
}

fn read_body(body: &Value, request: &mut PostmanRequest) {
    if is_disabled(body) {
        return;
    }
    let fields = |key: &str| -> Vec<FormField> {
        body.get(key)
            .and_then(Value::as_array)
            .map(|fields| {
                fields
                    .iter()
                    .map(|field| {
                        let is_file = field.get("type").and_then(Value::as_str) == Some("file");
                        let value = if is_file {
                            match field.get("src") {
                                Some(Value::Array(sources)) => text_of(sources.first()),
                                other => text_of(other),
                            }
                        } else {
                            text_of(field.get("value"))
                        };
                        FormField {
                            name: text_of(field.get("key")),
                            value,
                            is_file,
                            enabled: !is_disabled(field),
                        }
                    })
                    .filter(|field| !field.name.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    };
    match body.get("mode").and_then(Value::as_str) {
        Some("raw") => {
            let raw = text_of(body.get("raw"));
            request.body = (!raw.trim().is_empty()).then_some(raw);
        }
        Some("urlencoded") => request.form = fields("urlencoded"),
        Some("formdata") => request.form = fields("formdata"),
        Some("graphql") => {
            let query = text_of(body.pointer("/graphql/query"));
            let variables = body
                .pointer("/graphql/variables")
                .and_then(Value::as_str)
                .and_then(|variables| serde_json::from_str::<Value>(variables).ok())
                .unwrap_or(Value::Null);
            request.body = serde_json::to_string_pretty(
                &serde_json::json!({ "query": query, "variables": variables }),
            )
            .ok();
        }
        _ => {}
    }
}

/// A collection has no query parameters of its own, so a query API key only lands somewhere
/// when the spec has a scheme for it.
fn auth_is_kept(auth: &PostmanAuth, spec: Option<&Spec>) -> bool {
    match auth {
        PostmanAuth::ApiKey { key, in_query, .. } => {
            (!*in_query && !key.is_empty()) || api_key_scheme(spec, key).is_some()
        }
        PostmanAuth::Bearer { .. } | PostmanAuth::Basic { .. } => true,
    }
}

fn api_key_scheme<'a>(spec: Option<&'a Spec>, key: &str) -> Option<&'a SecurityScheme> {
    spec?.security_schemes.iter().find(|scheme| {
        matches!(&scheme.kind, SchemeKind::ApiKey { name, .. } if name.eq_ignore_ascii_case(key))
    })
}

fn read_auth(auth: &Value) -> Option<PostmanAuth> {
    let kind = auth.get("type").and_then(Value::as_str)?;
    // v2.1 lists `[{ key, value }]`; v2.0 uses a plain object.
    let parameter = |key: &str| -> String {
        match auth.get(kind) {
            Some(Value::Array(parameters)) => parameters
                .iter()
                .find(|parameter| parameter.get("key").and_then(Value::as_str) == Some(key))
                .map(|parameter| text_of(parameter.get("value")))
                .unwrap_or_default(),
            Some(Value::Object(parameters)) => text_of(parameters.get(key)),
            _ => String::new(),
        }
    };
    match kind {
        "bearer" => Some(PostmanAuth::Bearer {
            token: parameter("token"),
        }),
        "basic" => Some(PostmanAuth::Basic {
            username: parameter("username"),
            password: parameter("password"),
        }),
        "apikey" => Some(PostmanAuth::ApiKey {
            key: parameter("key"),
            value: parameter("value"),
            in_query: parameter("in") == "query",
        }),
        _ => None,
    }
}

/// Literal credentials go to the keychain so neither the collection file nor an export holds them.
fn apply_request_auth(
    request: &PostmanRequest,
    draft: &mut RequestDraft,
    secret_names: &[String],
    secrets: &mut Vec<(String, String)>,
) {
    let Some(auth) = &request.auth else {
        return;
    };
    let secret_prefix = format!(
        "postman.{}",
        slug(request.postman_id.as_deref().unwrap_or(&request.name))
    );
    let mut keep = |part: &str, value: &str| -> String {
        if value.contains("{{") {
            mark_secrets(value, secret_names)
        } else if value.is_empty() {
            String::new()
        } else {
            let name = format!("{secret_prefix}.{part}");
            secrets.push((name.clone(), value.to_string()));
            format!("{{{{{SECRET_PREFIX}{name}}}}}")
        }
    };
    let header = |name: &str, value: String| HeaderEntry {
        name: name.to_string(),
        value,
        enabled: true,
    };
    match auth {
        PostmanAuth::Bearer { token } => {
            let token = keep("token", token);
            draft
                .headers
                .push(header("Authorization", format!("Bearer {token}")));
        }
        PostmanAuth::Basic { username, password } => {
            // A placeholder can't be base64-encoded ahead of time.
            if !username.contains("{{") && !password.contains("{{") {
                use base64::Engine as _;
                let encoded = base64::engine::general_purpose::STANDARD
                    .encode(format!("{username}:{password}"));
                let encoded = keep("basic", &encoded);
                draft
                    .headers
                    .push(header("Authorization", format!("Basic {encoded}")));
            }
        }
        PostmanAuth::ApiKey {
            key,
            value,
            in_query,
        } => {
            if key.is_empty() {
                return;
            }
            let value = keep("apiKey", value);
            if *in_query {
                draft.query.push(ParamEntry {
                    name: key.clone(),
                    value,
                    enabled: true,
                });
            } else {
                draft.headers.push(header(key, value));
            }
        }
    }
}

/// What a Postman script writes to mean the response body.
pub(crate) const RESPONSE_EXPRESSIONS: [&str; 2] =
    ["pm.response.json()", "JSON.parse(responseBody)"];

const SET_CALLS: [&str; 4] = [
    "pm.environment.set(",
    "pm.collectionVariables.set(",
    "pm.globals.set(",
    "postman.setEnvironmentVariable(",
];

fn classify_script(listen: &str, source: &str, disabled: bool, request: &mut PostmanRequest) {
    let keep = |kind: ScriptKind| ImportedScript {
        kind,
        listen: listen.to_string(),
        source: source.to_string(),
        enabled: false,
    };
    if disabled || source.contains("pm.sendRequest") {
        request.scripts.push(keep(ScriptKind::Unsupported));
        return;
    }
    let tests = source.matches("pm.test(").count();
    let mut aliases: Vec<String> = RESPONSE_EXPRESSIONS
        .iter()
        .map(|expression| expression.to_string())
        .collect();
    let mut captures = Vec::new();
    let mut leftover = false;
    for line in source.lines() {
        let line = line.trim().trim_end_matches(';').trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        if let Some(alias) = response_alias(line, &aliases) {
            aliases.push(alias);
        } else if let Some(capture) = capture_from_line(line, &aliases) {
            captures.push(capture);
        } else {
            leftover = true;
        }
    }
    request.tests += tests;
    for capture in captures {
        if !request.captures.contains(&capture) {
            request.captures.push(capture);
        }
    }
    if tests > 0 {
        request.scripts.push(keep(ScriptKind::Test));
    } else if leftover {
        request.scripts.push(keep(ScriptKind::Unsupported));
    }
}

/// `const data = pm.response.json()` makes `data` mean the response body.
pub(crate) fn response_alias(line: &str, aliases: &[String]) -> Option<String> {
    let rest = ["const ", "let ", "var "]
        .iter()
        .find_map(|keyword| line.strip_prefix(keyword))?;
    let (name, value) = rest.split_once('=')?;
    let name = name.trim();
    let is_identifier = !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '$'));
    (is_identifier && aliases.iter().any(|alias| alias == value.trim())).then(|| name.to_string())
}

pub(crate) fn capture_from_line(line: &str, aliases: &[String]) -> Option<(String, String)> {
    let rest = SET_CALLS
        .iter()
        .find_map(|call| line.strip_prefix(call))?
        .strip_suffix(')')?;
    let quote = rest
        .chars()
        .next()
        .filter(|quote| matches!(quote, '"' | '\'' | '`'))?;
    let after_quote = &rest[1..];
    let end = after_quote.find(quote)?;
    let variable = after_quote[..end].to_string();
    let expression = after_quote[end + 1..]
        .trim_start()
        .strip_prefix(',')?
        .trim();
    aliases.iter().find_map(|alias| {
        let accessor = expression.strip_prefix(alias.as_str())?;
        if !(accessor.starts_with('.') || accessor.starts_with('[')) {
            return None;
        }
        let path = format!("${accessor}");
        jsonpath::is_valid(&path).then(|| (variable.clone(), path))
    })
}

fn segment_is_parameter(segment: &str) -> bool {
    segment.starts_with(':')
        || (segment.starts_with('{') && segment.ends_with('}') && segment.len() > 2)
}

/// The operation with the same method whose path, with any parameter matching any other,
/// ends the request's path; the fewest leftover leading segments wins.
pub fn match_operation<'a>(
    spec: &'a Spec,
    request: &PostmanRequest,
) -> Option<(&'a Operation, usize)> {
    let segments = &request.path_segments;
    spec.operations
        .iter()
        .filter(|operation| operation.method.eq_ignore_ascii_case(&request.method))
        .filter_map(|operation| {
            let spec_segments: Vec<&str> = operation
                .path
                .split('/')
                .filter(|segment| !segment.is_empty())
                .collect();
            let prefix = segments.len().checked_sub(spec_segments.len())?;
            if spec_segments.is_empty() && !segments.is_empty() {
                return None;
            }
            let same = spec_segments.iter().zip(&segments[prefix..]).all(
                |(spec_segment, segment)| match (
                    segment_is_parameter(spec_segment),
                    segment_is_parameter(segment),
                ) {
                    (true, true) => true,
                    (false, false) => spec_segment == segment,
                    _ => false,
                },
            );
            same.then_some((operation, prefix))
        })
        .min_by_key(|(_, prefix)| *prefix)
}

impl PostmanRequest {
    /// Rebuilds a saved request's address so it can be matched again.
    fn from_saved(method: &str, draft: &RequestDraft) -> Self {
        let (origin, path_segments, _) = split_raw_url(&draft.url);
        Self {
            method: method.to_string(),
            origin,
            path_segments,
            path_variables: draft.path_params.clone(),
            query: draft.query.clone(),
            headers: draft.headers.clone(),
            body: draft.body.clone(),
            form: draft.form.clone(),
            ..Self::default()
        }
    }

    fn path_variable(&self, name: &str) -> String {
        self.path_variables
            .iter()
            .find(|variable| variable.name == name)
            .map(|variable| variable.value.clone())
            .unwrap_or_default()
    }

    /// `/users/{id}`: the path as written, with Postman's `:id` in the spec's notation.
    pub fn written_path(&self) -> String {
        let mut path = String::new();
        for segment in &self.path_segments {
            path.push('/');
            match segment.strip_prefix(':') {
                Some(name) => {
                    path.push('{');
                    path.push_str(name);
                    path.push('}');
                }
                None => path.push_str(segment),
            }
        }
        if path.is_empty() {
            path.push('/');
        }
        path
    }

    fn draft_with(&self, url: String, path_params: Vec<ParamEntry>) -> RequestDraft {
        RequestDraft {
            url,
            path_params,
            query: self.query.clone(),
            headers: self.headers.clone(),
            disabled_inherited: Vec::new(),
            body: self.body.clone(),
            form: self.form.clone(),
        }
    }

    pub fn draft_as_written(&self) -> RequestDraft {
        let path_params = self
            .path_segments
            .iter()
            .filter_map(|segment| segment.strip_prefix(':'))
            .map(|name| ParamEntry {
                name: name.to_string(),
                value: self.path_variable(name),
                enabled: true,
            })
            .collect();
        self.draft_with(
            format!("{}{}", self.origin, self.written_path()),
            path_params,
        )
    }

    /// The draft of a matched request, addressed by the spec's path and parameter names.
    pub fn draft_for(&self, operation: &Operation, prefix: usize) -> RequestDraft {
        let spec_segments = operation
            .path
            .split('/')
            .filter(|segment| !segment.is_empty());
        let path_params = spec_segments
            .zip(self.path_segments.iter().skip(prefix))
            .filter_map(|(spec_segment, segment)| {
                let name = spec_segment.strip_prefix('{')?.strip_suffix('}')?;
                let value = if let Some(variable) = segment.strip_prefix(':') {
                    self.path_variable(variable)
                } else if segment.starts_with("{{") {
                    segment.clone()
                } else if let Some(variable) = segment
                    .strip_prefix('{')
                    .and_then(|segment| segment.strip_suffix('}'))
                {
                    self.path_variable(variable)
                } else {
                    segment.clone()
                };
                Some(ParamEntry {
                    name: name.to_string(),
                    value,
                    enabled: true,
                })
            })
            .collect();
        let leading: String = self
            .path_segments
            .iter()
            .take(prefix)
            .map(|segment| format!("/{segment}"))
            .collect();
        self.draft_with(
            format!("{}{leading}{}", self.origin, operation.path),
            path_params,
        )
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MergeOutcome {
    /// Keychain entries to store: secret name (without `secret.`) and value.
    pub secrets: Vec<(String, String)>,
    pub matched: usize,
    pub unmatched: usize,
}

fn keep_variable(
    environment: &mut Environment,
    variable: &PostmanVariable,
    overwrite: bool,
    secrets: &mut Vec<(String, String)>,
) {
    if variable.secret {
        if !variable.value.is_empty() {
            secrets.push((variable.name.clone(), variable.value.clone()));
        }
        match environment
            .variables
            .iter_mut()
            .find(|existing| existing.name == variable.name)
        {
            Some(existing) => {
                existing.value.clear();
                existing.secret = true;
            }
            None => environment.variables.push(Variable {
                name: variable.name.clone(),
                value: String::new(),
                secret: true,
            }),
        }
        return;
    }
    let exists = environment.get(&variable.name).is_some();
    if !exists || (overwrite && !variable.value.is_empty()) {
        environment.set(&variable.name, variable.value.clone());
    }
}

/// Brings an import into a collection file. Items are merged by their Postman id; nothing
/// the user added is removed, and secret values come back in the outcome instead of the file.
pub fn merge_postman(
    file: &mut CollectionFile,
    import: &PostmanImport,
    spec: Option<&Spec>,
) -> MergeOutcome {
    let mut outcome = MergeOutcome::default();
    let secret_names: Vec<String> = import
        .all_variables()
        .filter(|variable| variable.secret)
        .map(|variable| variable.name.clone())
        .collect();
    for imported in &import.environments {
        let index = match file
            .environments
            .iter()
            .position(|environment| environment.name == imported.name)
        {
            Some(index) => index,
            None => {
                file.environments.push(Environment {
                    name: imported.name.clone(),
                    variables: Vec::new(),
                });
                file.environments.len() - 1
            }
        };
        if let Some(environment) = file.environments.get_mut(index) {
            for variable in &imported.variables {
                keep_variable(environment, variable, true, &mut outcome.secrets);
            }
        }
    }
    for collection in &import.collections {
        if file.postman_id.is_none() {
            file.postman_id = collection.postman_id.clone();
        }
        if !collection.variables.is_empty() && file.environments.is_empty() {
            file.environments.push(Environment {
                name: collection.name.clone(),
                variables: Vec::new(),
            });
        }
        // Collection variables are defaults; a value an environment already has wins.
        for environment in &mut file.environments {
            for variable in &collection.variables {
                keep_variable(environment, variable, false, &mut outcome.secrets);
            }
        }
        if let Some(auth) = &collection.auth {
            merge_auth(
                file,
                auth,
                import,
                spec,
                &secret_names,
                &mut outcome.secrets,
            );
        }
    }

    for request in import.requests() {
        let matched = spec.and_then(|spec| match_operation(spec, request));
        let (operation, method, mut draft) = match matched {
            Some((operation, prefix)) => {
                outcome.matched += 1;
                (
                    operation.key.clone(),
                    operation.method.clone(),
                    request.draft_for(operation, prefix),
                )
            }
            None => {
                outcome.unmatched += 1;
                (
                    format!("{} {}", request.method, request.written_path()),
                    request.method.clone(),
                    request.draft_as_written(),
                )
            }
        };
        mark_secrets_in_draft(&mut draft, &secret_names);
        apply_request_auth(request, &mut draft, &secret_names, &mut outcome.secrets);
        for (variable, path) in &request.captures {
            if !file
                .captures
                .iter()
                .any(|rule| rule.operation == operation && &rule.variable == variable)
            {
                file.captures.push(CaptureRule {
                    operation: operation.clone(),
                    path: path.clone(),
                    variable: variable.clone(),
                });
            }
        }
        let existing = request.postman_id.as_ref().and_then(|postman_id| {
            file.saved
                .iter_mut()
                .find(|saved| saved.postman_id.as_ref() == Some(postman_id))
        });
        match existing {
            Some(saved) => {
                saved.name = request.name.clone();
                saved.operation = operation;
                saved.draft = draft;
                saved.method = Some(method);
                saved.folder = request.folder.clone();
                saved.scripts = request.scripts.clone();
            }
            None => file.saved.push(SavedRequest {
                id: uuid::Uuid::new_v4().to_string(),
                name: request.name.clone(),
                operation,
                draft,
                method: Some(method),
                postman_id: request.postman_id.clone(),
                folder: request.folder.clone(),
                scripts: request.scripts.clone(),
            }),
        }
    }
    outcome
}

/// Points the saved requests that aren't on any spec operation at the operation with the same
/// method and path, if there is one now.
pub fn match_saved_with_spec(file: &mut CollectionFile, spec: &Spec) -> MergeOutcome {
    let mut outcome = MergeOutcome::default();
    for saved in &mut file.saved {
        if spec.operation(&saved.operation).is_some() {
            continue;
        }
        let Some(method) = saved.method.clone() else {
            continue;
        };
        let request = PostmanRequest::from_saved(&method, &saved.draft);
        match match_operation(spec, &request) {
            Some((operation, prefix)) => {
                let previous = std::mem::replace(&mut saved.operation, operation.key.clone());
                for rule in &mut file.captures {
                    if rule.operation == previous {
                        rule.operation = operation.key.clone();
                    }
                }
                let disabled_inherited = std::mem::take(&mut saved.draft.disabled_inherited);
                saved.draft = request.draft_for(operation, prefix);
                saved.draft.disabled_inherited = disabled_inherited;
                outcome.matched += 1;
            }
            None => outcome.unmatched += 1,
        }
    }
    outcome
}

fn mark_secrets(text: &str, secret_names: &[String]) -> String {
    if secret_names.is_empty() {
        return text.to_string();
    }
    let mut result = String::with_capacity(text.len());
    let mut last = 0;
    for range in crate::vars::placeholder_ranges(text) {
        let name = text[range.start + 2..range.end - 2].trim();
        if secret_names.iter().any(|secret| secret == name) {
            result.push_str(&text[last..range.start]);
            result.push_str(&format!("{{{{{SECRET_PREFIX}{name}}}}}"));
            last = range.end;
        }
    }
    result.push_str(&text[last..]);
    result
}

fn mark_secrets_in_draft(draft: &mut RequestDraft, secret_names: &[String]) {
    draft.url = mark_secrets(&draft.url, secret_names);
    for parameter in draft.path_params.iter_mut().chain(draft.query.iter_mut()) {
        parameter.value = mark_secrets(&parameter.value, secret_names);
    }
    for header in &mut draft.headers {
        header.value = mark_secrets(&header.value, secret_names);
    }
    for field in &mut draft.form {
        field.value = mark_secrets(&field.value, secret_names);
    }
    if let Some(body) = &mut draft.body {
        *body = mark_secrets(body, secret_names);
    }
}

/// The collection's own auth: the spec's schemes get its credentials; without a matching
/// scheme it becomes a collection header, its literal value kept in the keychain.
fn merge_auth(
    file: &mut CollectionFile,
    auth: &PostmanAuth,
    import: &PostmanImport,
    spec: Option<&Spec>,
    secret_names: &[String],
    secrets: &mut Vec<(String, String)>,
) {
    let schemes = spec
        .map(|spec| spec.security_schemes.as_slice())
        .unwrap_or_default();
    let header_value = |name: &str, value: &str, secrets: &mut Vec<(String, String)>| {
        if value.contains("{{") {
            mark_secrets(value, secret_names)
        } else {
            secrets.push((name.to_string(), value.to_string()));
            format!("{{{{{SECRET_PREFIX}{name}}}}}")
        }
    };
    let add_header = |file: &mut CollectionFile, name: &str, value: String| {
        if !file
            .headers
            .iter()
            .any(|header| header.name.eq_ignore_ascii_case(name))
        {
            file.headers.push(HeaderEntry {
                name: name.to_string(),
                value,
                enabled: true,
            });
        }
    };
    let without_prefix = |secret: String| secret.trim_start_matches(SECRET_PREFIX).to_string();
    match auth {
        PostmanAuth::Bearer { token } => {
            // The spec's login already fills the bearer token.
            if file.auth.login.is_some() || token.trim().is_empty() {
                return;
            }
            let value = header_value("postman.token", token, secrets);
            add_header(file, "Authorization", format!("Bearer {value}"));
        }
        PostmanAuth::Basic { username, password } => {
            let basic_schemes: Vec<_> = schemes
                .iter()
                .filter(|scheme| matches!(scheme.kind, SchemeKind::Basic))
                .collect();
            if basic_schemes.is_empty() {
                if let (Some(username), Some(password)) =
                    (import.resolve(username), import.resolve(password))
                {
                    use base64::Engine as _;
                    let encoded = base64::engine::general_purpose::STANDARD
                        .encode(format!("{username}:{password}"));
                    let value = header_value("postman.basic", &encoded, secrets);
                    add_header(file, "Authorization", format!("Basic {value}"));
                }
                return;
            }
            for scheme in basic_schemes {
                for (part, value) in [("username", username), ("password", password)] {
                    if let Some(value) = import.resolve(value) {
                        secrets.push((
                            without_prefix(crate::send::basic_secret(&scheme.name, part)),
                            value,
                        ));
                    }
                }
            }
        }
        PostmanAuth::ApiKey {
            key,
            value,
            in_query,
        } => match (api_key_scheme(spec, key), import.resolve(value)) {
            (Some(scheme), Some(value)) => secrets.push((
                without_prefix(crate::send::api_key_secret(&scheme.name)),
                value,
            )),
            (None, _) if !*in_query && !key.is_empty() => {
                let value = header_value("postman.apiKey", value, secrets);
                add_header(file, key, value);
            }
            _ => {}
        },
    }
}

/// The `secret.*` names the file mentions, which an exported environment lists empty so
/// whoever imports it knows what to fill in.
pub fn secret_names(file: &CollectionFile) -> Vec<String> {
    let text = serde_json::to_string(file).unwrap_or_default();
    let mut names: Vec<String> = Vec::new();
    for name in crate::vars::placeholder_names(&text) {
        if let Some(name) = name.strip_prefix(SECRET_PREFIX)
            && !names.iter().any(|existing| existing == name)
        {
            names.push(name.to_string());
        }
    }
    for environment in &file.environments {
        for variable in &environment.variables {
            if variable.secret && !names.contains(&variable.name) {
                names.push(variable.name.clone());
            }
        }
    }
    names
}

/// Postman says `{{name}}` where the collection says `{{secret.name}}`.
fn unmark_secrets(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(text.replace(&format!("{{{{{SECRET_PREFIX}"), "{{")),
        Value::Array(items) => Value::Array(items.into_iter().map(unmark_secrets).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, unmark_secrets(value)))
                .collect(),
        ),
        other => other,
    }
}

fn postman_url(draft: &RequestDraft) -> Value {
    let (origin, segments, mut query) = split_raw_url(&draft.url);
    query.extend(draft.query.iter().cloned());
    let segments: Vec<String> = segments
        .into_iter()
        .map(|segment| {
            match segment
                .strip_prefix('{')
                .and_then(|segment| segment.strip_suffix('}'))
            {
                Some(name) if !segment.starts_with("{{") => format!(":{name}"),
                _ => segment,
            }
        })
        .collect();
    let mut raw = format!("{origin}/{}", segments.join("/"));
    let enabled: Vec<String> = query
        .iter()
        .filter(|parameter| parameter.enabled)
        .map(|parameter| format!("{}={}", parameter.name, parameter.value))
        .collect();
    if !enabled.is_empty() {
        raw.push('?');
        raw.push_str(&enabled.join("&"));
    }
    let mut url = serde_json::Map::new();
    url.insert("raw".into(), Value::String(raw));
    if let Some((protocol, host)) = origin.split_once("://") {
        let (host, port) = host.split_once(':').unwrap_or((host, ""));
        url.insert("protocol".into(), Value::String(protocol.to_string()));
        url.insert(
            "host".into(),
            Value::Array(
                host.split('.')
                    .map(|part| Value::String(part.to_string()))
                    .collect(),
            ),
        );
        if !port.is_empty() {
            url.insert("port".into(), Value::String(port.to_string()));
        }
    } else if !origin.is_empty() {
        url.insert("host".into(), serde_json::json!([origin]));
    }
    url.insert("path".into(), serde_json::json!(segments));
    if !query.is_empty() {
        url.insert(
            "query".into(),
            Value::Array(
                query
                    .iter()
                    .map(|parameter| {
                        serde_json::json!({
                            "key": parameter.name,
                            "value": parameter.value,
                            "disabled": !parameter.enabled,
                        })
                    })
                    .collect(),
            ),
        );
    }
    if !draft.path_params.is_empty() {
        url.insert(
            "variable".into(),
            Value::Array(
                draft
                    .path_params
                    .iter()
                    .map(|parameter| serde_json::json!({ "key": parameter.name, "value": parameter.value }))
                    .collect(),
            ),
        );
    }
    Value::Object(url)
}

fn postman_body(draft: &RequestDraft, kind: Option<BodyKind>) -> Option<Value> {
    let form: Vec<&FormField> = draft.form.iter().collect();
    let as_form = match kind {
        Some(kind) => kind.is_form(),
        None => draft.body.is_none() && !form.is_empty(),
    };
    if as_form && !form.is_empty() {
        let multipart = match kind {
            Some(kind) => kind == BodyKind::Multipart,
            None => form.iter().any(|field| field.is_file),
        };
        let mode = if multipart { "formdata" } else { "urlencoded" };
        let fields: Vec<Value> = form
            .iter()
            .map(|field| {
                if field.is_file {
                    serde_json::json!({ "key": field.name, "type": "file", "src": field.value, "disabled": !field.enabled })
                } else {
                    serde_json::json!({ "key": field.name, "value": field.value, "type": "text", "disabled": !field.enabled })
                }
            })
            .collect();
        return Some(serde_json::json!({ "mode": mode, mode: fields }));
    }
    let body = draft.body.as_ref().filter(|body| !body.trim().is_empty())?;
    let mut value = serde_json::json!({ "mode": "raw", "raw": body });
    if serde_json::from_str::<Value>(body).is_ok() {
        value["options"] = serde_json::json!({ "raw": { "language": "json" } });
    }
    Some(value)
}

fn postman_events(scripts: &[ImportedScript], captures: &[&CaptureRule]) -> Vec<Value> {
    let mut by_listen: Vec<(String, Vec<String>)> = Vec::new();
    let mut push = |listen: &str, text: &str| {
        let lines = text.lines().map(str::to_string);
        match by_listen
            .iter_mut()
            .find(|(existing, _)| existing == listen)
        {
            Some((_, existing)) => existing.extend(lines),
            None => by_listen.push((listen.to_string(), lines.collect())),
        }
    };
    for script in scripts {
        let listen = if script.listen.is_empty() {
            "test"
        } else {
            script.listen.as_str()
        };
        push(listen, &script.source);
    }
    for rule in captures {
        let accessor = rule.path.trim().strip_prefix('$').unwrap_or(&rule.path);
        push(
            "test",
            &format!(
                "pm.environment.set({}, pm.response.json(){accessor});",
                Value::String(rule.variable.clone())
            ),
        );
    }
    by_listen
        .into_iter()
        .map(|(listen, exec)| {
            serde_json::json!({
                "listen": listen,
                "script": { "type": "text/javascript", "exec": exec },
            })
        })
        .collect()
}

fn postman_request_item(
    name: &str,
    postman_id: Option<&str>,
    method: &str,
    draft: &RequestDraft,
    kind: Option<BodyKind>,
    events: Vec<Value>,
) -> Value {
    let headers: Vec<Value> = draft
        .headers
        .iter()
        .filter(|header| !header.name.is_empty())
        .map(|header| {
            serde_json::json!({ "key": header.name, "value": header.value, "disabled": !header.enabled })
        })
        .collect();
    let mut request = serde_json::json!({
        "method": method,
        "header": headers,
        "url": postman_url(draft),
    });
    if let Some(body) = postman_body(draft, kind) {
        request["body"] = body;
    }
    let mut item = serde_json::json!({ "name": name, "request": request });
    // Ids made up from a name only identify the item here.
    if let Some(postman_id) = postman_id.filter(|id| !id.starts_with("name:")) {
        item["id"] = Value::String(postman_id.to_string());
    }
    if !events.is_empty() {
        item["event"] = Value::Array(events);
    }
    item
}

fn insert_in_folder(items: &mut Vec<Value>, folder: &[&str], item: Value) {
    let Some((first, rest)) = folder.split_first() else {
        items.push(item);
        return;
    };
    let index = match items.iter().position(|existing| {
        existing.get("item").is_some()
            && existing.get("name").and_then(Value::as_str) == Some(*first)
    }) {
        Some(index) => index,
        None => {
            items.push(serde_json::json!({ "name": first, "item": [] }));
            items.len() - 1
        }
    };
    if let Some(Value::Array(children)) = items
        .get_mut(index)
        .and_then(|folder| folder.get_mut("item"))
    {
        insert_in_folder(children, rest, item);
    }
}

/// A Postman v2.1 collection: the spec's operations by tag, then the saved requests in their
/// folders. Secret values never leave the keychain; placeholders stay for Postman to fill.
pub fn to_postman_collection(name: &str, file: &CollectionFile, spec: Option<&Spec>) -> Value {
    let mut items: Vec<Value> = Vec::new();
    let collection_headers: Vec<HeaderEntry> = file
        .headers
        .iter()
        .filter(|header| header.enabled)
        .cloned()
        .collect();
    let with_collection_headers = |draft: &RequestDraft| -> RequestDraft {
        let mut draft = draft.clone();
        for header in &collection_headers {
            if !draft
                .headers
                .iter()
                .any(|existing| existing.name.eq_ignore_ascii_case(&header.name))
            {
                draft.headers.push(header.clone());
            }
        }
        draft
    };
    let captures_for = |operation: &str| -> Vec<&CaptureRule> {
        file.captures
            .iter()
            .filter(|rule| rule.operation == operation)
            .collect()
    };
    let body_kind = |operation: &str| -> Option<BodyKind> {
        spec.and_then(|spec| spec.operation(operation))
            .and_then(|operation| operation.request_body.as_ref())
            .map(|body| BodyKind::for_content_type(&body.content_type))
    };
    if let Some(spec) = spec {
        for (tag, operations) in spec.folders() {
            let children: Vec<Value> = operations
                .iter()
                .map(|operation| {
                    let draft = with_collection_headers(&initial_draft(spec, &operation.key));
                    postman_request_item(
                        &operation.title(),
                        None,
                        &operation.method,
                        &draft,
                        body_kind(&operation.key),
                        postman_events(&[], &captures_for(&operation.key)),
                    )
                })
                .collect();
            items.push(serde_json::json!({ "name": tag, "item": children }));
        }
    }
    for saved in &file.saved {
        let method = saved
            .method
            .clone()
            .or_else(|| {
                spec.and_then(|spec| spec.operation(&saved.operation))
                    .map(|operation| operation.method.clone())
            })
            .or_else(|| {
                saved
                    .operation
                    .split_whitespace()
                    .next()
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "GET".to_string());
        let captures = captures_for(&saved.operation);
        let item = postman_request_item(
            &saved.name,
            saved.postman_id.as_deref(),
            &method,
            &with_collection_headers(&saved.draft),
            body_kind(&saved.operation),
            postman_events(&saved.scripts, &captures),
        );
        let folder: Vec<&str> = saved
            .folder
            .as_deref()
            .map(|folder| folder.split(" / ").collect())
            .unwrap_or_default();
        insert_in_folder(&mut items, &folder, item);
    }
    let mut collection = serde_json::json!({
        "info": {
            "_postman_id": file
                .postman_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            "name": name,
            "schema": POSTMAN_SCHEMA_V21,
        },
        "item": items,
    });
    if file.auth.login.is_some() {
        collection["auth"] = serde_json::json!({
            "type": "bearer",
            "bearer": [{ "key": "token", "value": format!("{{{{{}}}}}", crate::send::TOKEN_VARIABLE), "type": "string" }],
        });
    }
    unmark_secrets(collection)
}

/// A Postman environment. Secret variables, and the secrets the collection mentions, go with
/// an empty value.
pub fn to_postman_environment(environment: &Environment, secret_names: &[String]) -> Value {
    let mut values: Vec<Value> = environment
        .variables
        .iter()
        .map(|variable| {
            serde_json::json!({
                "key": variable.name,
                "value": if variable.secret { "" } else { variable.value.as_str() },
                "type": if variable.secret { "secret" } else { "default" },
                "enabled": true,
            })
        })
        .collect();
    for name in secret_names {
        if environment.get(name).is_none() {
            values.push(
                serde_json::json!({ "key": name, "value": "", "type": "secret", "enabled": true }),
            );
        }
    }
    serde_json::json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "name": environment.name,
        "values": values,
        "_postman_variable_scope": "environment",
    })
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
    const POSTMAN_SPEC: &str = r##"
openapi: 3.0.3
info: { title: Shop, version: "1" }
paths:
  /users/{userId}:
    get:
      parameters: [{ name: userId, in: path, required: true, schema: { type: string } }]
      responses: { '200': { description: ok } }
  /auth/login:
    post:
      responses: { '200': { description: ok } }
"##;

    const COLLECTION_V21: &str = r##"{
  "info": {
    "_postman_id": "c-1",
    "name": "Shop",
    "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"
  },
  "auth": { "type": "bearer", "bearer": [{ "key": "token", "value": "{{apiToken}}", "type": "string" }] },
  "item": [
    {
      "name": "Users",
      "item": [
        {
          "name": "Admin",
          "item": [
            {
              "id": "r-1",
              "name": "Get user",
              "request": {
                "method": "GET",
                "header": [{ "key": "X-Tenant", "value": "{{tenant}}" }],
                "url": {
                  "raw": "{{baseUrl}}/users/:id?full=true",
                  "host": ["{{baseUrl}}"],
                  "path": ["users", ":id"],
                  "query": [{ "key": "full", "value": "true" }],
                  "variable": [{ "key": "id", "value": "42" }]
                }
              },
              "event": [
                {
                  "listen": "test",
                  "script": {
                    "exec": [
                      "const data = pm.response.json();",
                      "pm.environment.set(\"userName\", data.profile.name);",
                      "pm.test(\"ok\", function () { pm.response.to.have.status(200); });"
                    ]
                  }
                }
              ]
            }
          ]
        }
      ]
    },
    {
      "id": "r-2",
      "name": "Upload avatar",
      "request": {
        "method": "POST",
        "url": "https://api.shop.test/avatar",
        "body": {
          "mode": "formdata",
          "formdata": [
            { "key": "file", "type": "file", "src": "/tmp/a.png" },
            { "key": "note", "type": "text", "value": "hi" }
          ]
        }
      },
      "event": [
        { "listen": "prerequest", "script": { "exec": ["pm.sendRequest('https://x', () => {});"] } }
      ]
    }
  ]
}"##;

    const COLLECTION_V20: &str = r##"{
  "info": {
    "name": "Old",
    "schema": "https://schema.getpostman.com/json/collection/v2.0.0/collection.json"
  },
  "item": [
    {
      "name": "Login",
      "request": {
        "method": "post",
        "url": "{{baseUrl}}/auth/login",
        "auth": { "type": "basic", "basic": { "username": "ana", "password": "pw" } },
        "body": { "mode": "raw", "raw": "{\"password\": \"{{password}}\"}" }
      }
    }
  ]
}"##;

    const ENVIRONMENT: &str = r##"{
  "name": "dev",
  "values": [
    { "key": "baseUrl", "value": "https://dev.shop.test", "type": "default", "enabled": true },
    { "key": "password", "value": "hunter2", "type": "secret", "enabled": true }
  ],
  "_postman_variable_scope": "environment"
}"##;

    fn postman_spec() -> Spec {
        spec::load(Path::new("/s.yml"), POSTMAN_SPEC, &|_| {
            Err(anyhow::anyhow!("x"))
        })
        .unwrap()
    }

    fn sample_import() -> PostmanImport {
        let mut import = PostmanImport::default();
        for text in [COLLECTION_V21, COLLECTION_V20, ENVIRONMENT] {
            import.add(parse_postman(text).unwrap());
        }
        import
    }

    #[test]
    fn reads_postman_collections_and_environments() {
        let import = sample_import();
        let shop = &import.collections[0];
        assert_eq!(shop.version, "2.1");
        assert_eq!(shop.folder_count, 2);
        let get_user = &shop.requests[0];
        assert_eq!(get_user.folder.as_deref(), Some("Users / Admin"));
        assert_eq!(get_user.origin, "{{baseUrl}}");
        assert_eq!(get_user.path_segments, vec!["users", ":id"]);
        assert_eq!(get_user.query[0].name, "full");
        assert_eq!(
            get_user.captures,
            vec![("userName".to_string(), "$.profile.name".to_string())]
        );
        assert_eq!(get_user.tests, 1);
        assert_eq!(get_user.scripts[0].kind, ScriptKind::Test);
        let upload = &shop.requests[1];
        assert_eq!(upload.origin, "https://api.shop.test");
        assert!(upload.form[0].is_file);
        assert_eq!(upload.form[0].value, "/tmp/a.png");
        assert_eq!(upload.scripts[0].kind, ScriptKind::Unsupported);
        assert!(!upload.scripts[0].enabled);

        let old = &import.collections[1];
        assert_eq!(old.version, "2.0");
        assert_eq!(old.requests[0].method, "POST");
        assert_eq!(old.requests[0].postman_id.as_deref(), Some("name:/Login"));
        assert_eq!(
            old.requests[0].auth,
            Some(PostmanAuth::Basic {
                username: "ana".to_string(),
                password: "pw".to_string(),
            })
        );

        assert_eq!(
            import.counts(None),
            ImportCounts {
                folders: 2,
                requests: 3,
                environments: 1,
                variables: 2,
                secrets: 1,
                auth: true,
                captures: 1,
                tests: 1,
                unsupported: 1,
            }
        );
    }

    #[test]
    fn reports_the_line_of_invalid_json() {
        match parse_postman("{\n  \"info\": {\n    \"name\": \"x\",,\n") {
            Err(PostmanError::Json { line, .. }) => assert_eq!(line, 3),
            other => panic!("expected a JSON error, got {other:?}"),
        }
        assert_eq!(parse_postman("{\"a\": 1}"), Err(PostmanError::NotPostman));
    }

    #[test]
    fn matches_postman_paths_to_spec_operations() {
        let spec = postman_spec();
        let mut request = PostmanRequest {
            method: "GET".to_string(),
            origin: "{{baseUrl}}".to_string(),
            path_segments: vec!["users".to_string(), "{{userId}}".to_string()],
            ..PostmanRequest::default()
        };
        let (operation, prefix) = match_operation(&spec, &request).unwrap();
        assert_eq!((operation.key.as_str(), prefix), ("GET /users/{userId}", 0));
        let draft = request.draft_for(operation, prefix);
        assert_eq!(draft.url, "{{baseUrl}}/users/{userId}");
        assert_eq!(draft.path_params[0].value, "{{userId}}");

        request.path_segments = vec!["v1".to_string(), "users".to_string(), ":id".to_string()];
        request.path_variables = vec![ParamEntry {
            name: "id".to_string(),
            value: "7".to_string(),
            enabled: true,
        }];
        let (operation, prefix) = match_operation(&spec, &request).unwrap();
        assert_eq!(prefix, 1);
        let draft = request.draft_for(operation, prefix);
        assert_eq!(draft.url, "{{baseUrl}}/v1/users/{userId}");
        assert_eq!(draft.path_params[0].value, "7");

        request.method = "DELETE".to_string();
        assert!(match_operation(&spec, &request).is_none());
    }

    #[test]
    fn reimporting_merges_by_postman_id_and_keeps_user_items() {
        let spec = postman_spec();
        let mut file = CollectionFile::default();
        file.saved.push(SavedRequest {
            id: "mine".to_string(),
            name: "My request".to_string(),
            operation: "GET /users/{userId}".to_string(),
            ..SavedRequest::default()
        });
        let import = sample_import();
        let outcome = merge_postman(&mut file, &import, Some(&spec));
        assert_eq!((outcome.matched, outcome.unmatched), (2, 1));
        assert_eq!(
            outcome.secrets,
            vec![
                ("password".to_string(), "hunter2".to_string()),
                (
                    "postman.name-login.basic".to_string(),
                    "YW5hOnB3".to_string()
                ),
            ]
        );
        assert_eq!(file.postman_id.as_deref(), Some("c-1"));
        assert_eq!(file.saved.len(), 4);
        let get_user = file
            .saved
            .iter()
            .find(|saved| saved.postman_id.as_deref() == Some("r-1"))
            .unwrap();
        assert_eq!(get_user.operation, "GET /users/{userId}");
        assert_eq!(get_user.draft.path_params[0].value, "42");
        let login = file
            .saved
            .iter()
            .find(|saved| saved.name == "Login")
            .unwrap();
        assert_eq!(
            login.draft.body.as_deref(),
            Some("{\"password\": \"{{secret.password}}\"}")
        );
        assert!(
            login
                .draft
                .headers
                .iter()
                .any(|header| header.name == "Authorization"
                    && header.value == "Basic {{secret.postman.name-login.basic}}")
        );
        let upload = file
            .saved
            .iter()
            .find(|saved| saved.name == "Upload avatar")
            .unwrap();
        assert_eq!(upload.operation, "POST /avatar");
        assert_eq!(upload.method.as_deref(), Some("POST"));
        assert_eq!(
            file.captures,
            vec![CaptureRule {
                operation: "GET /users/{userId}".to_string(),
                path: "$.profile.name".to_string(),
                variable: "userName".to_string(),
            }]
        );
        let dev = &file.environments[0];
        assert_eq!(dev.get("baseUrl"), Some("https://dev.shop.test"));
        assert!(
            dev.variables
                .iter()
                .any(|variable| variable.name == "password"
                    && variable.secret
                    && variable.value.is_empty())
        );
        let saved_text = serialize(&file).unwrap();
        assert!(!saved_text.contains("hunter2"));
        assert!(!saved_text.contains("YW5hOnB3"));

        merge_postman(&mut file, &import, Some(&spec));
        assert_eq!(file.saved.len(), 4, "reimporting updates instead of adding");
        assert!(file.saved.iter().any(|saved| saved.id == "mine"));
        assert_eq!(file.captures.len(), 1);
    }

    #[test]
    fn exports_without_secret_values() {
        let spec = postman_spec();
        let mut file = CollectionFile::default();
        merge_postman(&mut file, &sample_import(), Some(&spec));
        let exported = to_postman_collection("Shop", &file, Some(&spec));
        let text = exported.to_string();
        assert!(!text.contains("hunter2"));
        assert!(!text.contains("YW5hOnB3"));
        assert!(!text.contains("{{secret."));
        assert_eq!(exported["info"]["schema"], POSTMAN_SCHEMA_V21);
        assert_eq!(exported["info"]["_postman_id"], "c-1");

        let reread = match parse_postman(&serialize(&exported).unwrap()).unwrap() {
            PostmanFile::Collection(collection) => collection,
            PostmanFile::Environment(_) => panic!("exported a collection"),
        };
        let get_user = reread
            .requests
            .iter()
            .find(|request| request.postman_id.as_deref() == Some("r-1"))
            .unwrap();
        assert_eq!(get_user.folder.as_deref(), Some("Users / Admin"));
        assert_eq!(get_user.path_segments, vec!["users", ":userId"]);
        assert_eq!(
            get_user.captures,
            vec![("userName".to_string(), "$.profile.name".to_string())]
        );

        let environment = to_postman_environment(&file.environments[0], &secret_names(&file));
        let password = environment["values"]
            .as_array()
            .unwrap()
            .iter()
            .find(|value| value["key"] == "password")
            .unwrap();
        assert_eq!(password["type"], "secret");
        assert_eq!(password["value"], "");
    }
}
