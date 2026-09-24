//! Reads an OpenAPI 3.x or Swagger 2.0 document into one model the rest of the crate uses.
//! Schemas keep their internal `$ref`s, since validation resolves them against the document;
//! everything else (parameters, bodies, responses) is resolved while reading.

use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// The folder operations without a tag go to.
pub const UNTAGGED: &str = "Sem tag";
const MAX_REF_DEPTH: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpecFormat {
    OpenApi30,
    OpenApi31,
    Swagger2,
}

impl SpecFormat {
    pub fn label(self, version: &str) -> String {
        match self {
            SpecFormat::OpenApi30 | SpecFormat::OpenApi31 => format!("OpenAPI {version}"),
            SpecFormat::Swagger2 => "Swagger 2.0".to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Server {
    pub url: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApiKeyLocation {
    Header,
    Query,
    Cookie,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SchemeKind {
    Bearer {
        format: Option<String>,
    },
    Basic,
    ApiKey {
        location: ApiKeyLocation,
        name: String,
    },
    /// OAuth2 and OpenID Connect: a token obtained elsewhere, sent as a bearer token.
    OAuth2,
    Unsupported(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct SecurityScheme {
    pub name: String,
    pub kind: SchemeKind,
    pub description: Option<String>,
}

impl SecurityScheme {
    pub fn summary(&self) -> String {
        match &self.kind {
            SchemeKind::Bearer { format } => match format {
                Some(format) => format!("http · bearer · {format}"),
                None => "http · bearer".to_string(),
            },
            SchemeKind::Basic => "http · basic".to_string(),
            SchemeKind::ApiKey { location, name } => {
                let location = match location {
                    ApiKeyLocation::Header => "header",
                    ApiKeyLocation::Query => "query",
                    ApiKeyLocation::Cookie => "cookie",
                };
                format!("apiKey · {location} {name}")
            }
            SchemeKind::OAuth2 => "oauth2 · token bearer".to_string(),
            SchemeKind::Unsupported(kind) => format!("{kind} · não suportado"),
        }
    }
}

/// The alternatives an operation accepts; each one lists the schemes it needs together.
/// An empty list means the operation is public.
pub type SecurityRequirements = Vec<Vec<String>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ParameterLocation {
    Path,
    Query,
    Header,
    Cookie,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Parameter {
    pub name: String,
    pub location: ParameterLocation,
    pub required: bool,
    pub description: Option<String>,
    pub schema: Option<Value>,
    pub example: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RequestBody {
    pub content_type: String,
    pub other_content_types: Vec<String>,
    pub required: bool,
    pub schema: Option<Value>,
    pub example: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResponseSpec {
    pub status: String,
    pub description: Option<String>,
    pub content_type: Option<String>,
    pub schema: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Operation {
    /// `METHOD /path`: what saved requests and settings point at.
    pub key: String,
    pub method: String,
    pub path: String,
    pub operation_id: Option<String>,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub deprecated: bool,
    pub parameters: Vec<Parameter>,
    pub request_body: Option<RequestBody>,
    pub responses: Vec<ResponseSpec>,
    /// `None` inherits the document's default.
    pub security: Option<SecurityRequirements>,
}

impl Operation {
    pub fn title(&self) -> String {
        self.summary
            .clone()
            .filter(|summary| !summary.trim().is_empty())
            .or_else(|| self.operation_id.clone())
            .unwrap_or_else(|| self.path.clone())
    }

    /// The response a successful call should look like: the first 2xx, else `default`.
    pub fn success_response(&self) -> Option<&ResponseSpec> {
        self.responses
            .iter()
            .find(|response| response.status.starts_with('2'))
            .or_else(|| {
                self.responses
                    .iter()
                    .find(|response| response.status == "default")
            })
    }

    pub fn response_for_status(&self, status: u16) -> Option<&ResponseSpec> {
        let exact = status.to_string();
        let range = format!("{}XX", status / 100);
        self.responses
            .iter()
            .find(|response| response.status == exact)
            .or_else(|| {
                self.responses
                    .iter()
                    .find(|response| response.status.eq_ignore_ascii_case(&range))
            })
            .or_else(|| {
                self.responses
                    .iter()
                    .find(|response| response.status == "default")
            })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Tag {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug)]
pub struct Spec {
    pub format: SpecFormat,
    pub version_string: String,
    pub title: String,
    pub api_version: String,
    pub servers: Vec<Server>,
    pub security_schemes: Vec<SecurityScheme>,
    pub default_security: SecurityRequirements,
    pub tags: Vec<Tag>,
    pub operations: Vec<Operation>,
    /// References that could not be followed, and similar problems that don't stop reading.
    pub warnings: Vec<String>,
    /// The document with external references inlined, for resolving schema `$ref`s.
    pub document: Value,
}

impl Spec {
    pub fn operation(&self, key: &str) -> Option<&Operation> {
        self.operations
            .iter()
            .find(|operation| operation.key == key)
    }

    pub fn scheme(&self, name: &str) -> Option<&SecurityScheme> {
        self.security_schemes
            .iter()
            .find(|scheme| scheme.name == name)
    }

    pub fn effective_security<'a>(&'a self, operation: &'a Operation) -> &'a SecurityRequirements {
        operation
            .security
            .as_ref()
            .unwrap_or(&self.default_security)
    }

    /// Every scheme any of the operation's alternatives mentions. Specs often list schemes
    /// as alternatives when the server actually wants all of them, so all configured ones go.
    pub fn schemes_for(&self, operation: &Operation) -> Vec<&SecurityScheme> {
        let mut names: Vec<&str> = Vec::new();
        for requirement in self.effective_security(operation) {
            for name in requirement {
                if !names.contains(&name.as_str()) {
                    names.push(name);
                }
            }
        }
        names
            .into_iter()
            .filter_map(|name| self.scheme(name))
            .collect()
    }

    /// Operations grouped by their first tag, in the document's tag order.
    pub fn folders(&self) -> Vec<(String, Vec<&Operation>)> {
        let mut order: Vec<String> = self.tags.iter().map(|tag| tag.name.clone()).collect();
        let mut groups: HashMap<String, Vec<&Operation>> = HashMap::default();
        for operation in &self.operations {
            let tag = operation
                .tags
                .first()
                .cloned()
                .unwrap_or_else(|| UNTAGGED.to_string());
            if !order.contains(&tag) {
                order.push(tag.clone());
            }
            groups.entry(tag).or_default().push(operation);
        }
        order
            .into_iter()
            .filter_map(|tag| groups.remove(&tag).map(|operations| (tag, operations)))
            .collect()
    }

    /// Follows internal `$ref`s until reaching a value that isn't one.
    pub fn deref<'a>(&'a self, value: &'a Value) -> &'a Value {
        deref(&self.document, value)
    }
}

pub fn deref<'a>(document: &'a Value, mut value: &'a Value) -> &'a Value {
    for _ in 0..MAX_REF_DEPTH {
        let Some(reference) = value.get("$ref").and_then(Value::as_str) else {
            return value;
        };
        let Some(target) = reference
            .strip_prefix('#')
            .and_then(|pointer| document.pointer(pointer))
        else {
            return value;
        };
        value = target;
    }
    value
}

/// Reads a spec file, inlining references to other files next to it.
pub fn load(path: &Path, text: &str, read_file: &dyn Fn(&Path) -> Result<String>) -> Result<Spec> {
    let (document, repaired_lines) =
        parse_document_with_repairs(text).with_context(|| format!("lendo {}", path.display()))?;
    let mut warnings: Vec<String> = repaired_lines
        .into_iter()
        .map(|line| format!("linha {line}: valor com \": \" sem aspas, lido como texto"))
        .collect();
    let base_dir = path.parent().map(Path::to_path_buf);
    let mut cache = HashMap::default();
    let document = inline_external_refs(
        document,
        base_dir.as_deref(),
        read_file,
        &mut cache,
        &mut warnings,
        0,
    );
    from_document(document, warnings)
}

/// Parses YAML or JSON (YAML is a superset) into JSON values. Non-string map keys such as
/// unquoted status codes become strings, and a key repeated in the same map keeps its last
/// value, as JavaScript tooling does; generated specs have both.
pub fn parse_document(text: &str) -> Result<Value> {
    parse_document_with_repairs(text).map(|(document, _)| document)
}

/// How many lines with an unquoted `: ` inside a value get read as text before giving up.
const MAX_REPAIRS: usize = 64;

/// Like [`parse_document`], but a value with `: ` inside it (`description: nega (x: true)`),
/// which YAML rejects and JavaScript tooling reads anyway, is read as text. Returns the
/// lines read that way.
pub fn parse_document_with_repairs(text: &str) -> Result<(Value, Vec<usize>)> {
    let mut text = std::borrow::Cow::Borrowed(text);
    let mut repaired_lines = Vec::new();
    loop {
        let error = match serde_yaml::from_str::<LenientValue>(&text) {
            Ok(document) => return Ok((document.0, repaired_lines)),
            Err(error) => error,
        };
        let line = error.location().map(|location| location.line());
        let repaired = match line {
            Some(line)
                if repaired_lines.len() < MAX_REPAIRS
                    && error
                        .to_string()
                        .starts_with("mapping values are not allowed") =>
            {
                quote_line_value(&text, line)
            }
            _ => None,
        };
        match (repaired, line) {
            (Some(repaired), Some(line)) => {
                repaired_lines.push(line);
                text = std::borrow::Cow::Owned(repaired);
            }
            _ => return Err(error.into()),
        }
    }
}

/// Rewrites `key: some value: more` on the given line (1-based) as `key: "some value: more"`.
fn quote_line_value(text: &str, line_number: usize) -> Option<String> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    let line = *lines.get(line_number.checked_sub(1)?)?;
    let content_start = line.len() - line.trim_start().len();
    let content = &line[content_start..];
    let list_marker = if content.starts_with("- ") { 2 } else { 0 };
    let separator = content[list_marker..].find(": ")? + list_marker;
    let key = &content[..separator];
    let value = content[separator + 2..].trim();
    if value.is_empty() || value.starts_with(['"', '\'', '|', '>', '{', '[']) {
        return None;
    }
    let quoted = serde_json::to_string(value).ok()?;
    let repaired = format!("{}{key}: {quoted}", &line[..content_start]);
    let index = line_number - 1;
    let mut owned: Vec<String> = lines.drain(..).map(str::to_string).collect();
    owned[index] = repaired;
    Some(owned.join("\n"))
}

struct LenientValue(Value);

impl<'de> serde::Deserialize<'de> for LenientValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(LenientVisitor)
    }
}

struct LenientVisitor;

impl<'de> serde::de::Visitor<'de> for LenientVisitor {
    type Value = LenientValue;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a YAML or JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<LenientValue, E> {
        Ok(LenientValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<LenientValue, E> {
        Ok(LenientValue(Value::from(value)))
    }

    fn visit_u64<E>(self, value: u64) -> Result<LenientValue, E> {
        Ok(LenientValue(Value::from(value)))
    }

    fn visit_f64<E>(self, value: f64) -> Result<LenientValue, E> {
        Ok(LenientValue(
            serde_json::Number::from_f64(value)
                .map(Value::Number)
                .unwrap_or(Value::Null),
        ))
    }

    fn visit_str<E>(self, value: &str) -> Result<LenientValue, E> {
        Ok(LenientValue(Value::String(value.to_string())))
    }

    fn visit_string<E>(self, value: String) -> Result<LenientValue, E> {
        Ok(LenientValue(Value::String(value)))
    }

    fn visit_unit<E>(self) -> Result<LenientValue, E> {
        Ok(LenientValue(Value::Null))
    }

    fn visit_none<E>(self) -> Result<LenientValue, E> {
        Ok(LenientValue(Value::Null))
    }

    fn visit_some<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<LenientValue, D::Error> {
        serde::Deserialize::deserialize(deserializer)
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<LenientValue, A::Error> {
        let mut items = Vec::new();
        while let Some(LenientValue(item)) = seq.next_element()? {
            items.push(item);
        }
        Ok(LenientValue(Value::Array(items)))
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<LenientValue, A::Error> {
        let mut object = Map::new();
        while let Some((LenientValue(key), LenientValue(value))) = map.next_entry()? {
            let key = match key {
                Value::String(key) => key,
                Value::Null => "null".to_string(),
                other => other.to_string(),
            };
            object.insert(key, value);
        }
        Ok(LenientValue(Value::Object(object)))
    }

    fn visit_enum<A: serde::de::EnumAccess<'de>>(self, data: A) -> Result<LenientValue, A::Error> {
        // A YAML tag (`!Foo value`): the tag is dropped and the value kept.
        use serde::de::VariantAccess as _;
        let (_tag, variant): (LenientValue, _) = data.variant()?;
        variant.newtype_variant()
    }
}

fn inline_external_refs(
    value: Value,
    base_dir: Option<&Path>,
    read_file: &dyn Fn(&Path) -> Result<String>,
    cache: &mut HashMap<PathBuf, Value>,
    warnings: &mut Vec<String>,
    depth: usize,
) -> Value {
    match value {
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str)
                && !reference.starts_with('#')
            {
                return resolve_external(reference, base_dir, read_file, cache, warnings, depth);
            }
            Value::Object(
                object
                    .into_iter()
                    .map(|(key, value)| {
                        let value = inline_external_refs(
                            value, base_dir, read_file, cache, warnings, depth,
                        );
                        (key, value)
                    })
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| inline_external_refs(item, base_dir, read_file, cache, warnings, depth))
                .collect(),
        ),
        other => other,
    }
}

fn resolve_external(
    reference: &str,
    base_dir: Option<&Path>,
    read_file: &dyn Fn(&Path) -> Result<String>,
    cache: &mut HashMap<PathBuf, Value>,
    warnings: &mut Vec<String>,
    depth: usize,
) -> Value {
    let unresolved = || serde_json::json!({ "$ref": reference });
    if depth >= MAX_REF_DEPTH {
        warnings.push(format!("$ref aninhado demais: {reference}"));
        return unresolved();
    }
    if reference.starts_with("http://") || reference.starts_with("https://") {
        warnings.push(format!("$ref remoto não é seguido: {reference}"));
        return unresolved();
    }
    let (file, pointer) = reference.split_once('#').unwrap_or((reference, ""));
    let Some(base_dir) = base_dir else {
        warnings.push(format!("$ref sem pasta base: {reference}"));
        return unresolved();
    };
    let path = base_dir.join(file);
    let document = match cache.get(&path) {
        Some(document) => document.clone(),
        None => {
            let loaded = read_file(&path).and_then(|text| parse_document(&text));
            match loaded {
                Ok(document) => {
                    cache.insert(path.clone(), document.clone());
                    document
                }
                Err(error) => {
                    warnings.push(format!("$ref não encontrado: {reference} ({error})"));
                    return unresolved();
                }
            }
        }
    };
    let Some(target) = document.pointer(pointer).cloned() else {
        warnings.push(format!("$ref não encontrado: {reference}"));
        return unresolved();
    };
    // Internal references inside the other file point into that file, so they are
    // resolved now, before its content lands in this document.
    let target = inline_internal_refs(target, &document, warnings, depth + 1);
    inline_external_refs(target, path.parent(), read_file, cache, warnings, depth + 1)
}

fn inline_internal_refs(
    value: Value,
    document: &Value,
    warnings: &mut Vec<String>,
    depth: usize,
) -> Value {
    if depth >= MAX_REF_DEPTH {
        return value;
    }
    match value {
        Value::Object(object) => {
            if let Some(pointer) = object
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|reference| reference.strip_prefix('#'))
            {
                return match document.pointer(pointer) {
                    Some(target) => {
                        inline_internal_refs(target.clone(), document, warnings, depth + 1)
                    }
                    None => {
                        warnings.push(format!("$ref não encontrado: #{pointer}"));
                        Value::Object(object)
                    }
                };
            }
            Value::Object(
                object
                    .into_iter()
                    .map(|(key, value)| {
                        (key, inline_internal_refs(value, document, warnings, depth))
                    })
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| inline_internal_refs(item, document, warnings, depth))
                .collect(),
        ),
        other => other,
    }
}

pub fn from_document(document: Value, mut warnings: Vec<String>) -> Result<Spec> {
    let (format, version_string) =
        if let Some(version) = document.get("openapi").and_then(Value::as_str) {
            if version.starts_with("3.1") {
                (SpecFormat::OpenApi31, version.to_string())
            } else if version.starts_with('3') {
                (SpecFormat::OpenApi30, version.to_string())
            } else {
                return Err(anyhow!("versão do OpenAPI não suportada: {version}"));
            }
        } else if document.get("swagger").and_then(Value::as_str) == Some("2.0") {
            (SpecFormat::Swagger2, "2.0".to_string())
        } else {
            return Err(anyhow!(
                "o arquivo não parece um spec OpenAPI (faltam os campos `openapi` ou `swagger`)"
            ));
        };

    let info = document.get("info");
    let title = info
        .and_then(|info| info.get("title"))
        .and_then(Value::as_str)
        .unwrap_or("API")
        .to_string();
    let api_version = info
        .and_then(|info| info.get("version"))
        .map(value_to_string)
        .unwrap_or_default();

    let servers = match format {
        SpecFormat::Swagger2 => swagger_servers(&document),
        _ => openapi_servers(&document),
    };
    let security_schemes = read_security_schemes(&document, format);
    let default_security = document
        .get("security")
        .map(read_security)
        .unwrap_or_default();

    let tags = document
        .get("tags")
        .and_then(Value::as_array)
        .map(|tags| {
            tags.iter()
                .filter_map(|tag| {
                    Some(Tag {
                        name: tag.get("name")?.as_str()?.to_string(),
                        description: string_field(tag, "description"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let mut operations = Vec::new();
    if let Some(paths) = document.get("paths").and_then(Value::as_object) {
        for (path, item) in paths {
            let item = deref(&document, item);
            let shared_parameters = item
                .get("parameters")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for method in [
                "get", "put", "post", "delete", "options", "head", "patch", "trace",
            ] {
                let Some(operation) = item.get(method) else {
                    continue;
                };
                operations.push(read_operation(
                    &document,
                    format,
                    path,
                    method,
                    operation,
                    &shared_parameters,
                    &mut warnings,
                ));
            }
        }
    }

    Ok(Spec {
        format,
        version_string,
        title,
        api_version,
        servers,
        security_schemes,
        default_security,
        tags,
        operations,
        warnings,
        document,
    })
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        other => other.to_string(),
    }
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn openapi_servers(document: &Value) -> Vec<Server> {
    let Some(servers) = document.get("servers").and_then(Value::as_array) else {
        return Vec::new();
    };
    servers
        .iter()
        .filter_map(|server| {
            let mut url = server.get("url")?.as_str()?.to_string();
            // Server variables get their default values; the environment can change them.
            if let Some(variables) = server.get("variables").and_then(Value::as_object) {
                for (name, variable) in variables {
                    if let Some(default) = variable.get("default") {
                        url = url.replace(&format!("{{{name}}}"), &value_to_string(default));
                    }
                }
            }
            Some(Server {
                url: url.trim_end_matches('/').to_string(),
                description: string_field(server, "description"),
            })
        })
        .collect()
}

fn swagger_servers(document: &Value) -> Vec<Server> {
    let Some(host) = document.get("host").and_then(Value::as_str) else {
        return Vec::new();
    };
    let base_path = document
        .get("basePath")
        .and_then(Value::as_str)
        .unwrap_or("");
    let schemes: Vec<&str> = document
        .get("schemes")
        .and_then(Value::as_array)
        .map(|schemes| schemes.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let schemes = if schemes.is_empty() {
        vec!["https"]
    } else {
        schemes
    };
    schemes
        .into_iter()
        .map(|scheme| Server {
            url: format!("{scheme}://{host}{base_path}")
                .trim_end_matches('/')
                .to_string(),
            description: None,
        })
        .collect()
}

fn read_security_schemes(document: &Value, format: SpecFormat) -> Vec<SecurityScheme> {
    let schemes = match format {
        SpecFormat::Swagger2 => document.get("securityDefinitions"),
        _ => document
            .get("components")
            .and_then(|components| components.get("securitySchemes")),
    };
    let Some(schemes) = schemes.and_then(Value::as_object) else {
        return Vec::new();
    };
    schemes
        .iter()
        .map(|(name, scheme)| {
            let scheme = deref(document, scheme);
            let kind_name = scheme.get("type").and_then(Value::as_str).unwrap_or("");
            let kind = match kind_name {
                "http" => {
                    let http_scheme = scheme
                        .get("scheme")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    match http_scheme.as_str() {
                        "bearer" => SchemeKind::Bearer {
                            format: string_field(scheme, "bearerFormat"),
                        },
                        "basic" => SchemeKind::Basic,
                        other => SchemeKind::Unsupported(format!("http {other}")),
                    }
                }
                "basic" => SchemeKind::Basic,
                "apiKey" => {
                    let location = match scheme.get("in").and_then(Value::as_str) {
                        Some("query") => ApiKeyLocation::Query,
                        Some("cookie") => ApiKeyLocation::Cookie,
                        _ => ApiKeyLocation::Header,
                    };
                    SchemeKind::ApiKey {
                        location,
                        name: string_field(scheme, "name").unwrap_or_else(|| name.clone()),
                    }
                }
                "oauth2" | "openIdConnect" => SchemeKind::OAuth2,
                other => SchemeKind::Unsupported(other.to_string()),
            };
            SecurityScheme {
                name: name.clone(),
                kind,
                description: string_field(scheme, "description"),
            }
        })
        .collect()
}

fn read_security(value: &Value) -> SecurityRequirements {
    let Some(requirements) = value.as_array() else {
        return Vec::new();
    };
    requirements
        .iter()
        .filter_map(Value::as_object)
        .map(|requirement| requirement.keys().cloned().collect())
        .collect()
}

fn read_operation(
    document: &Value,
    format: SpecFormat,
    path: &str,
    method: &str,
    operation: &Value,
    shared_parameters: &[Value],
    warnings: &mut Vec<String>,
) -> Operation {
    let method = method.to_ascii_uppercase();
    let key = format!("{method} {path}");

    let mut parameters: Vec<Parameter> = Vec::new();
    let mut swagger_body: Option<RequestBody> = None;
    let mut swagger_form_fields: Vec<(String, Option<Value>)> = Vec::new();
    let own_parameters = operation
        .get("parameters")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for raw in shared_parameters.iter().chain(own_parameters.iter()) {
        let raw = deref(document, raw);
        if raw.get("$ref").is_some() {
            warnings.push(format!("{key}: parâmetro com $ref não encontrado"));
            continue;
        }
        let Some(name) = raw.get("name").and_then(Value::as_str) else {
            continue;
        };
        let location = match raw.get("in").and_then(Value::as_str) {
            Some("path") => ParameterLocation::Path,
            Some("query") => ParameterLocation::Query,
            Some("header") => ParameterLocation::Header,
            Some("cookie") => ParameterLocation::Cookie,
            Some("body") => {
                swagger_body = Some(RequestBody {
                    content_type: swagger_consumes(document, operation)
                        .into_iter()
                        .next()
                        .unwrap_or_else(|| "application/json".to_string()),
                    other_content_types: Vec::new(),
                    required: raw
                        .get("required")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    schema: raw.get("schema").cloned(),
                    example: None,
                });
                continue;
            }
            Some("formData") => {
                swagger_form_fields.push((name.to_string(), raw.get("type").cloned()));
                continue;
            }
            _ => continue,
        };
        let schema = match format {
            SpecFormat::Swagger2 => {
                let mut schema = Map::new();
                for field in ["type", "format", "enum", "default", "items"] {
                    if let Some(value) = raw.get(field) {
                        schema.insert(field.to_string(), value.clone());
                    }
                }
                Some(Value::Object(schema))
            }
            _ => raw.get("schema").cloned(),
        };
        let parameter = Parameter {
            name: name.to_string(),
            location,
            required: location == ParameterLocation::Path
                || raw
                    .get("required")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            description: string_field(raw, "description"),
            schema,
            example: raw.get("example").cloned(),
        };
        // An operation's own parameter replaces the path-level one with the same name.
        parameters.retain(|existing| {
            !(existing.name == parameter.name && existing.location == parameter.location)
        });
        parameters.push(parameter);
    }

    let request_body = match format {
        SpecFormat::Swagger2 => swagger_body.or_else(|| {
            if swagger_form_fields.is_empty() {
                return None;
            }
            let properties: Map<String, Value> = swagger_form_fields
                .into_iter()
                .map(|(name, kind)| {
                    let schema =
                        serde_json::json!({ "type": kind.unwrap_or(Value::from("string")) });
                    (name, schema)
                })
                .collect();
            Some(RequestBody {
                content_type: swagger_consumes(document, operation)
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| "application/x-www-form-urlencoded".to_string()),
                other_content_types: Vec::new(),
                required: false,
                schema: Some(serde_json::json!({ "type": "object", "properties": properties })),
                example: None,
            })
        }),
        _ => operation
            .get("requestBody")
            .map(|body| deref(document, body))
            .and_then(read_request_body),
    };

    let responses = operation
        .get("responses")
        .and_then(Value::as_object)
        .map(|responses| {
            responses
                .iter()
                .map(|(status, response)| {
                    let response = deref(document, response);
                    let (content_type, schema) = match format {
                        SpecFormat::Swagger2 => (
                            swagger_produces(document, operation).into_iter().next(),
                            response.get("schema").cloned(),
                        ),
                        _ => match response
                            .get("content")
                            .and_then(Value::as_object)
                            .and_then(|content| preferred_media(content))
                        {
                            Some((content_type, media)) => {
                                (Some(content_type), media.get("schema").cloned())
                            }
                            None => (None, None),
                        },
                    };
                    ResponseSpec {
                        status: status.clone(),
                        description: string_field(response, "description"),
                        content_type,
                        schema,
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    Operation {
        key,
        method,
        path: path.to_string(),
        operation_id: string_field(operation, "operationId"),
        summary: string_field(operation, "summary"),
        description: string_field(operation, "description"),
        tags: operation
            .get("tags")
            .and_then(Value::as_array)
            .map(|tags| {
                tags.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        deprecated: operation
            .get("deprecated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        parameters,
        request_body,
        responses,
        security: operation.get("security").map(read_security),
    }
}

/// JSON first, since it's what the body editor and validation understand best.
fn preferred_media(content: &Map<String, Value>) -> Option<(String, &Value)> {
    content
        .iter()
        .find(|(content_type, _)| is_json_content_type(content_type))
        .or_else(|| content.iter().next())
        .map(|(content_type, media)| (content_type.clone(), media))
}

pub fn is_json_content_type(content_type: &str) -> bool {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    essence == "application/json" || essence.ends_with("+json") || essence == "*/*"
}

fn read_request_body(body: &Value) -> Option<RequestBody> {
    let content = body.get("content").and_then(Value::as_object)?;
    let (content_type, media) = preferred_media(content)?;
    let example = media.get("example").cloned().or_else(|| {
        media
            .get("examples")
            .and_then(Value::as_object)
            .and_then(|examples| examples.values().next())
            .and_then(|example| example.get("value"))
            .cloned()
    });
    Some(RequestBody {
        other_content_types: content
            .keys()
            .filter(|other| **other != content_type)
            .cloned()
            .collect(),
        content_type,
        required: body
            .get("required")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        schema: media.get("schema").cloned(),
        example,
    })
}

fn swagger_consumes(document: &Value, operation: &Value) -> Vec<String> {
    string_list(
        operation
            .get("consumes")
            .or_else(|| document.get("consumes")),
    )
}

fn swagger_produces(document: &Value, operation: &Value) -> Vec<String> {
    string_list(
        operation
            .get("produces")
            .or_else(|| document.get("produces")),
    )
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a file name looks like an API description worth offering to link.
pub fn looks_like_spec_file(file_name: &str) -> bool {
    let lower = file_name.to_ascii_lowercase();
    let is_document = [".yaml", ".yml", ".json"]
        .iter()
        .any(|extension| lower.ends_with(extension));
    is_document && (lower.contains("openapi") || lower.contains("swagger"))
}

/// Cheap check on a file's beginning, to skip files that only have a matching name.
pub fn declares_spec_version(text: &str) -> bool {
    text.lines().take(40).any(|line| {
        let line = line.trim_start_matches(['{', ' ', '"', '\'']);
        line.starts_with("openapi") || line.starts_with("swagger")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn no_files(path: &Path) -> Result<String> {
        Err(anyhow!("sem arquivo {}", path.display()))
    }

    const OPENAPI: &str = r##"
openapi: 3.0.3
info: { title: Trix API, version: 2.4.0 }
servers:
  - url: https://api-dev.trix.com.br/{version}/
    description: dev
    variables:
      version: { default: v2 }
security:
  - bearerAuth: []
components:
  securitySchemes:
    bearerAuth: { type: http, scheme: bearer, bearerFormat: JWT }
    apiKeyAuth: { type: apiKey, in: header, name: x-api-key }
  parameters:
    UserId: { name: id, in: path, required: true, schema: { type: string } }
  schemas:
    Login: { type: object, properties: { email: { type: string } } }
tags:
  - name: users
  - name: auth
paths:
  /auth/login:
    post:
      tags: [auth]
      summary: Fazer login
      security: []
      requestBody:
        content:
          application/json:
            schema: { $ref: '#/components/schemas/Login' }
      responses:
        200: { description: ok }
  /users/{id}:
    parameters:
      - $ref: '#/components/parameters/UserId'
    get:
      tags: [users]
      parameters:
        - { name: fields, in: query, schema: { type: string } }
      responses:
        '200':
          description: ok
          content:
            application/json:
              schema: { type: object }
  /health:
    get:
      responses: { '204': { description: vazio } }
"##;

    #[test]
    fn reads_openapi_documents() {
        let spec = load(Path::new("/api/openapi.yml"), OPENAPI, &no_files).unwrap();
        assert_eq!(spec.format, SpecFormat::OpenApi30);
        assert_eq!(spec.title, "Trix API");
        assert_eq!(spec.api_version, "2.4.0");
        assert_eq!(spec.servers[0].url, "https://api-dev.trix.com.br/v2");
        assert_eq!(spec.security_schemes.len(), 2);

        let login = spec.operation("POST /auth/login").unwrap();
        assert_eq!(login.title(), "Fazer login");
        assert_eq!(login.security, Some(vec![]));
        assert!(spec.schemes_for(login).is_empty());
        let body = login.request_body.as_ref().unwrap();
        assert_eq!(body.content_type, "application/json");
        assert_eq!(login.responses[0].status, "200");

        let user = spec.operation("GET /users/{id}").unwrap();
        let names: Vec<_> = user.parameters.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["id", "fields"]);
        assert!(user.parameters[0].required);
        assert_eq!(spec.schemes_for(user)[0].name, "bearerAuth");

        let folders: Vec<_> = spec
            .folders()
            .into_iter()
            .map(|(tag, operations)| (tag, operations.len()))
            .collect();
        assert_eq!(
            folders,
            vec![
                ("users".to_string(), 1),
                ("auth".to_string(), 1),
                (UNTAGGED.to_string(), 1)
            ]
        );
    }

    #[test]
    fn reads_swagger_documents() {
        let text = r#"
swagger: "2.0"
info: { title: Legacy, version: "1" }
host: legacy.trix.com.br
basePath: /api
schemes: [https]
consumes: [application/json]
securityDefinitions:
  key: { type: apiKey, in: query, name: token }
paths:
  /pix:
    post:
      parameters:
        - { name: body, in: body, schema: { type: object } }
        - { name: page, in: query, type: integer }
      responses:
        200: { description: ok, schema: { type: object } }
"#;
        let spec = load(Path::new("/swagger.yaml"), text, &no_files).unwrap();
        assert_eq!(spec.format, SpecFormat::Swagger2);
        assert_eq!(spec.servers[0].url, "https://legacy.trix.com.br/api");
        let pix = spec.operation("POST /pix").unwrap();
        assert!(pix.request_body.is_some());
        assert_eq!(pix.parameters.len(), 1);
        assert_eq!(
            spec.security_schemes[0].kind,
            SchemeKind::ApiKey {
                location: ApiKeyLocation::Query,
                name: "token".to_string()
            }
        );
    }

    #[test]
    fn inlines_references_to_other_files() {
        let main = r##"
openapi: 3.1.0
info: { title: Split, version: "1" }
paths:
  /users:
    get:
      responses:
        '200':
          description: ok
          content:
            application/json:
              schema: { $ref: './schemas.yml#/User' }
"##;
        let schemas = r##"
User: { type: object, properties: { address: { $ref: '#/Address' } } }
Address: { type: string }
"##;
        let read = |path: &Path| -> Result<String> {
            assert_eq!(path, Path::new("/api/./schemas.yml"));
            Ok(schemas.to_string())
        };
        let spec = load(Path::new("/api/openapi.yml"), main, &read).unwrap();
        let schema = spec.operations[0].responses[0].schema.clone().unwrap();
        assert_eq!(
            schema,
            serde_json::json!({ "type": "object", "properties": { "address": { "type": "string" } } })
        );
        assert!(spec.warnings.is_empty());
    }

    #[test]
    fn tolerates_repeated_keys_and_numeric_keys() {
        let document = parse_document("a: 1\nb: { 200: ok }\na: 2\n").unwrap();
        assert_eq!(
            document,
            serde_json::json!({ "a": 2, "b": { "200": "ok" } })
        );

        let (document, repaired) =
            parse_document_with_repairs("x:\n  description: nega (`denial: true`)\n").unwrap();
        assert_eq!(
            document,
            serde_json::json!({ "x": { "description": "nega (`denial: true`)" } })
        );
        assert_eq!(repaired, vec![2]);
    }

    #[test]
    fn rejects_files_that_are_not_specs() {
        assert!(load(Path::new("/x.yml"), "name: app\n", &no_files).is_err());
        assert!(looks_like_spec_file("swagger-investors.yaml"));
        assert!(looks_like_spec_file("auth.openapi.yaml"));
        assert!(!looks_like_spec_file("docker-compose.yml"));
    }
}
