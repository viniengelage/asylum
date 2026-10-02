use std::{fmt, sync::Arc};

use anyhow::Context as _;
use base64::Engine as _;
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, Method, Request, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Where the requests go. Most clusters only expose Kibana to people, so going through its
/// console proxy (the one Dev Tools uses) needs nothing beyond the login used in the browser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    Kibana(Url),
    Direct(Url),
}

impl Endpoint {
    pub fn url(&self) -> &Url {
        match self {
            Endpoint::Kibana(url) | Endpoint::Direct(url) => url,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum Auth {
    Basic { username: String, password: String },
    ApiKey(String),
    None,
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Auth::Basic { username, .. } => f
                .debug_struct("Basic")
                .field("username", username)
                .finish_non_exhaustive(),
            Auth::ApiKey(_) => f.write_str("ApiKey(..)"),
            Auth::None => f.write_str("None"),
        }
    }
}

impl Auth {
    fn header(&self) -> Option<String> {
        match self {
            Auth::Basic { username, password } => {
                let pair = format!("{username}:{password}");
                Some(format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD.encode(pair)
                ))
            }
            Auth::ApiKey(key) => Some(format!("ApiKey {key}")),
            Auth::None => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ElasticError {
    #[error("{via} recusou o login: {message}")]
    Unauthorized { via: &'static str, message: String },
    #[error("sem permissão: {message}")]
    Forbidden { message: String },
    #[error("não encontrado: {message}")]
    NotFound { message: String },
    #[error("{message}")]
    BadRequest { kind: String, message: String },
    #[error("{via} respondeu {status}: {message}")]
    Status {
        via: &'static str,
        status: u16,
        message: String,
    },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Clone)]
pub struct Elastic {
    http: Arc<dyn HttpClient>,
    endpoint: Endpoint,
    auth: Auth,
}

impl fmt::Debug for Elastic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Elastic")
            .field("endpoint", &self.endpoint)
            .field("auth", &self.auth)
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ClusterInfo {
    pub cluster_name: String,
    pub version: ClusterVersion,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ClusterVersion {
    pub number: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CurrentUser {
    pub username: String,
    #[serde(default)]
    pub roles: Vec<String>,
    pub authentication_realm: Realm,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Realm {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataStream {
    pub name: String,
    pub backing_indices: usize,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub kind: String,
}

/// Which of the requested privileges the current user lacks, per index pattern.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MissingPrivileges {
    pub cluster: Vec<String>,
    pub index: Vec<(String, Vec<String>)>,
}

impl MissingPrivileges {
    pub fn is_empty(&self) -> bool {
        self.cluster.is_empty() && self.index.is_empty()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct EsqlColumn {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct EsqlResult {
    pub columns: Vec<EsqlColumn>,
    pub values: Vec<Vec<Value>>,
    #[serde(default)]
    pub took: Option<u64>,
}

/// A window on `@timestamp`, in the date-math Elasticsearch accepts (`now-30m`, an ISO date).
/// It goes to `_query` as a filter, so the query text never has to mention time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TimeRange {
    pub from: String,
    pub to: String,
}

impl Elastic {
    pub fn new(http: Arc<dyn HttpClient>, endpoint: Endpoint, auth: Auth) -> Self {
        Self {
            http,
            endpoint,
            auth,
        }
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// The cluster's name and version. `GET /` needs the `monitor` cluster privilege, which
    /// read-only roles often lack; then this falls back to Kibana's version, which matches the
    /// cluster's on a stack installed together.
    pub async fn info_or_fallback(&self) -> Result<ClusterInfo, ElasticError> {
        match self.info().await {
            Ok(info) => Ok(info),
            Err(ElasticError::Forbidden { .. }) => {
                let user = self.current_user().await?;
                let version = self.kibana_version().await.ok().flatten();
                Ok(ClusterInfo {
                    cluster_name: format!("sem monitor · {}", user.username),
                    version: ClusterVersion {
                        number: version.unwrap_or_else(|| "?".to_string()),
                    },
                })
            }
            Err(error) => Err(error),
        }
    }

    /// Kibana's own version, when going through Kibana. The cluster may run another one.
    pub async fn kibana_version(&self) -> Result<Option<String>, ElasticError> {
        let Endpoint::Kibana(kibana) = &self.endpoint else {
            return Ok(None);
        };
        let url = join(kibana, "api/status")?;
        let status: Value = self.send_to_kibana(Method::GET, url, None).await?;
        Ok(status
            .pointer("/version/number")
            .and_then(Value::as_str)
            .map(str::to_string))
    }

    pub async fn info(&self) -> Result<ClusterInfo, ElasticError> {
        self.parse(self.request(Method::GET, "/", None).await?)
    }

    pub async fn current_user(&self) -> Result<CurrentUser, ElasticError> {
        self.parse(
            self.request(Method::GET, "/_security/_authenticate", None)
                .await?,
        )
    }

    pub async fn missing_privileges(
        &self,
        cluster: &[&str],
        index_patterns: &[&str],
        index_privileges: &[&str],
    ) -> Result<MissingPrivileges, ElasticError> {
        let body = json!({
            "cluster": cluster,
            "index": [{ "names": index_patterns, "privileges": index_privileges }],
        });
        let response = self
            .request(Method::POST, "/_security/user/_has_privileges", Some(body))
            .await?;
        Ok(parse_missing_privileges(&response))
    }

    pub async fn data_streams(&self) -> Result<Vec<DataStream>, ElasticError> {
        let response = self.request(Method::GET, "/_data_stream", None).await?;
        let streams = response
            .get("data_streams")
            .and_then(Value::as_array)
            .context("resposta sem data_streams")?;
        Ok(streams
            .iter()
            .filter_map(|stream| {
                Some(DataStream {
                    name: stream.get("name")?.as_str()?.to_string(),
                    backing_indices: stream
                        .get("indices")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len),
                    status: stream
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
            })
            .collect())
    }

    /// Data stream names through `_resolve/index`, which only needs `view_index_metadata` on
    /// what it lists: for users that can't read `_data_stream`.
    pub async fn resolved_data_streams(&self) -> Result<Vec<DataStream>, ElasticError> {
        let response = self
            .request(Method::GET, "/_resolve/index/*?expand_wildcards=open", None)
            .await?;
        Ok(response
            .get("data_streams")
            .and_then(Value::as_array)
            .map(|streams| {
                streams
                    .iter()
                    .filter_map(|stream| {
                        Some(DataStream {
                            name: stream.get("name")?.as_str()?.to_string(),
                            backing_indices: stream
                                .get("backing_indices")
                                .and_then(Value::as_array)
                                .map_or(0, Vec::len),
                            status: String::new(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Store size per data stream, in bytes. Needs `monitor`, so callers treat it as optional.
    pub async fn data_stream_sizes(
        &self,
    ) -> Result<std::collections::HashMap<String, u64>, ElasticError> {
        let response = self
            .request(Method::GET, "/_data_stream/_stats", None)
            .await?;
        Ok(response
            .get("data_streams")
            .and_then(Value::as_array)
            .map(|streams| {
                streams
                    .iter()
                    .filter_map(|stream| {
                        Some((
                            stream.get("data_stream")?.as_str()?.to_string(),
                            stream.get("store_size_bytes")?.as_u64()?,
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    pub async fn fields(&self, index_pattern: &str) -> Result<Vec<Field>, ElasticError> {
        let path = format!(
            "/{}/_field_caps?fields=*&include_unmapped=false",
            urlencoding::encode(index_pattern)
        );
        let response = self.request(Method::GET, &path, None).await?;
        Ok(parse_fields(&response))
    }

    pub async fn esql(
        &self,
        query: &str,
        range: Option<&TimeRange>,
    ) -> Result<EsqlResult, ElasticError> {
        let mut body = json!({ "query": query });
        if let Some(range) = range {
            body["filter"] = json!({
                "range": { "@timestamp": { "gte": range.from, "lte": range.to } }
            });
        }
        self.parse(self.request(Method::POST, "/_query", Some(body)).await?)
    }

    /// Sends an Elasticsearch API call, straight or through Kibana's console proxy.
    pub async fn request(
        &self,
        method: Method,
        path_and_query: &str,
        body: Option<Value>,
    ) -> Result<Value, ElasticError> {
        match &self.endpoint {
            Endpoint::Direct(base) => {
                let url = join(base, path_and_query.trim_start_matches('/'))?;
                self.send(method, url, body, "O Elasticsearch", &[]).await
            }
            Endpoint::Kibana(kibana) => {
                let url = kibana_proxy_url(kibana, &method, path_and_query)?;
                self.send_to_kibana(Method::POST, url, body).await
            }
        }
    }

    async fn send_to_kibana(
        &self,
        method: Method,
        url: Url,
        body: Option<Value>,
    ) -> Result<Value, ElasticError> {
        // Kibana refuses writes without `kbn-xsrf`, and 9.x answers its internal APIs (the
        // console proxy is one) only to requests that say they come from Kibana's own UI.
        let headers = [
            ("kbn-xsrf", "asylum"),
            ("x-elastic-internal-origin", "Kibana"),
        ];
        self.send(method, url, body, "O Kibana", &headers).await
    }

    async fn send(
        &self,
        method: Method,
        url: Url,
        body: Option<Value>,
        via: &'static str,
        headers: &[(&str, &str)],
    ) -> Result<Value, ElasticError> {
        let mut builder = Request::builder()
            .method(method)
            .uri(url.as_str())
            .header("Accept", "application/json");
        if let Some(authorization) = self.auth.header() {
            builder = builder.header("Authorization", authorization);
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let body = match body {
            Some(body) => {
                builder = builder.header("Content-Type", "application/json");
                AsyncBody::from(serde_json::to_string(&body).context("corpo da requisição")?)
            }
            None => AsyncBody::default(),
        };
        let request = builder.body(body).context("requisição inválida")?;

        let mut response = self
            .http
            .send(request)
            .await
            .with_context(|| format!("falha ao chamar {}", redacted(&url)))?;
        let mut text = String::new();
        response
            .body_mut()
            .read_to_string(&mut text)
            .await
            .context("falha ao ler a resposta")?;
        // Kibana's console proxy answers 200 and carries the cluster's status in a header.
        let status = response
            .headers()
            .get("x-console-proxy-status-code")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or_else(|| response.status().as_u16());
        if (200..300).contains(&status) {
            if text.trim().is_empty() {
                return Ok(Value::Null);
            }
            let value: Value = serde_json::from_str(&text).with_context(|| {
                format!(
                    "resposta que não é JSON de {}: {}",
                    redacted(&url),
                    snippet(&text)
                )
            })?;
            // An Elasticsearch error body that still came with a success status.
            if let Some(status) = value
                .get("status")
                .and_then(Value::as_u64)
                .filter(|_| value.get("error").is_some_and(Value::is_object))
            {
                return Err(error_from_response(
                    u16::try_from(status).unwrap_or(500),
                    &text,
                    via,
                ));
            }
            return Ok(value);
        }
        Err(error_from_response(status, &text, via))
    }

    fn parse<T: serde::de::DeserializeOwned>(&self, value: Value) -> Result<T, ElasticError> {
        let shown = snippet(&value.to_string());
        serde_json::from_value(value).map_err(|error| {
            ElasticError::Other(anyhow::anyhow!(
                "resposta inesperada do Elasticsearch ({error}): {shown}"
            ))
        })
    }
}

fn join(base: &Url, path: &str) -> Result<Url, ElasticError> {
    let mut base = base.clone();
    // Without the trailing slash `join` would replace a base path like `/kibana`.
    if !base.path().ends_with('/') {
        base.set_path(&format!("{}/", base.path()));
    }
    base.join(path)
        .with_context(|| format!("caminho inválido: {path}"))
        .map_err(ElasticError::from)
}

fn kibana_proxy_url(
    kibana: &Url,
    method: &Method,
    path_and_query: &str,
) -> Result<Url, ElasticError> {
    let mut url = join(kibana, "api/console/proxy")?;
    url.query_pairs_mut()
        .append_pair("path", path_and_query)
        .append_pair("method", method.as_str());
    Ok(url)
}

fn snippet(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() > 300 {
        format!("{}…", text.chars().take(300).collect::<String>())
    } else {
        text.to_string()
    }
}

fn redacted(url: &Url) -> String {
    let mut url = url.clone();
    url.set_query(None);
    url.set_password(None).ok();
    url.to_string()
}

/// Elasticsearch answers `{"error": {"type", "reason", "root_cause"}, "status"}`; Kibana
/// answers `{"statusCode", "error", "message"}`, and the console proxy passes the former on
/// with the cluster's status.
fn error_from_response(status: u16, text: &str, via: &'static str) -> ElasticError {
    let body: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let elastic_error = body.get("error").filter(|error| error.is_object());
    let kind = elastic_error
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let message = elastic_error
        .and_then(|error| error.get("reason"))
        .or_else(|| body.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| text.chars().take(300).collect());
    // A 401 relayed by the proxy comes from the cluster, with Elasticsearch's error shape.
    let via = if elastic_error.is_some() {
        "O Elasticsearch"
    } else {
        via
    };
    match status {
        401 => ElasticError::Unauthorized { via, message },
        403 => ElasticError::Forbidden { message },
        404 => ElasticError::NotFound { message },
        400 => ElasticError::BadRequest { kind, message },
        _ => ElasticError::Status {
            via,
            status,
            message,
        },
    }
}

fn parse_missing_privileges(response: &Value) -> MissingPrivileges {
    let denied = |privileges: &Value| -> Vec<String> {
        privileges
            .as_object()
            .map(|privileges| {
                privileges
                    .iter()
                    .filter(|(_, granted)| granted.as_bool() == Some(false))
                    .map(|(name, _)| name.clone())
                    .collect()
            })
            .unwrap_or_default()
    };
    let cluster = response.get("cluster").map(denied).unwrap_or_default();
    let index = response
        .get("index")
        .and_then(Value::as_object)
        .map(|indices| {
            indices
                .iter()
                .map(|(pattern, privileges)| (pattern.clone(), denied(privileges)))
                .filter(|(_, missing)| !missing.is_empty())
                .collect()
        })
        .unwrap_or_default();
    MissingPrivileges { cluster, index }
}

fn parse_fields(response: &Value) -> Vec<Field> {
    let Some(fields) = response.get("fields").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut fields: Vec<Field> = fields
        .iter()
        .filter(|(name, _)| !name.starts_with('_'))
        .filter_map(|(name, types)| {
            // A field mapped differently across indices lists every type; the first is enough
            // for completion.
            let kind = types.as_object()?.keys().next()?.clone();
            (kind != "object" && kind != "nested").then(|| Field {
                name: name.clone(),
                kind,
            })
        })
        .collect();
    fields.sort_by(|a, b| a.name.cmp(&b.name));
    fields
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_client::{FakeHttpClient, Response};
    use std::sync::Mutex;

    fn kibana() -> Endpoint {
        Endpoint::Kibana(Url::parse("https://kibana.example.com/base").expect("url"))
    }

    fn basic() -> Auth {
        Auth::Basic {
            username: "vinicios".into(),
            password: "segredo".into(),
        }
    }

    type Seen = Arc<Mutex<Vec<(String, String, Vec<(String, String)>, String)>>>;

    fn client(endpoint: Endpoint, status: u16, response: &'static str) -> (Elastic, Seen) {
        let seen: Seen = Arc::default();
        let http = FakeHttpClient::create({
            let seen = seen.clone();
            move |mut request| {
                let seen = seen.clone();
                async move {
                    let mut body = String::new();
                    request.body_mut().read_to_string(&mut body).await?;
                    let headers = request
                        .headers()
                        .iter()
                        .map(|(name, value)| {
                            (
                                name.to_string(),
                                value.to_str().unwrap_or_default().to_string(),
                            )
                        })
                        .collect();
                    seen.lock().map_err(|_| anyhow::anyhow!("poisoned"))?.push((
                        request.method().to_string(),
                        request.uri().to_string(),
                        headers,
                        body,
                    ));
                    Ok(Response::builder()
                        .status(status)
                        .body(AsyncBody::from(response))?)
                }
            }
        });
        (Elastic::new(http, endpoint, basic()), seen)
    }

    #[test]
    fn esql_through_kibana_uses_the_console_proxy() -> anyhow::Result<()> {
        let (elastic, seen) = client(
            kibana(),
            200,
            r#"{"took": 12, "columns": [{"name": "c", "type": "long"}], "values": [[37]]}"#,
        );
        let range = TimeRange {
            from: "now-30m".into(),
            to: "now".into(),
        };
        let result = futures::executor::block_on(
            elastic.esql("FROM logs-* | STATS c = COUNT(*)", Some(&range)),
        )?;
        assert_eq!(result.values, vec![vec![json!(37)]]);
        assert_eq!(result.took, Some(12));

        let seen = seen.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
        let (method, uri, headers, body) = &seen[0];
        assert_eq!(method, "POST");
        assert_eq!(
            uri,
            "https://kibana.example.com/base/api/console/proxy?path=%2F_query&method=POST"
        );
        let header = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(header("kbn-xsrf"), Some("asylum"));
        assert_eq!(
            header("authorization"),
            Some("Basic dmluaWNpb3M6c2VncmVkbw==")
        );
        let body: Value = serde_json::from_str(body)?;
        assert_eq!(body["filter"]["range"]["@timestamp"]["gte"], "now-30m");
        Ok(())
    }

    #[test]
    fn direct_requests_keep_method_and_path() -> anyhow::Result<()> {
        let endpoint = Endpoint::Direct(Url::parse("http://10.0.0.5:9200")?);
        let (elastic, seen) = client(
            endpoint,
            200,
            r#"{"cluster_name": "trix", "version": {"number": "9.2.4"}}"#,
        );
        let info = futures::executor::block_on(elastic.info())?;
        assert_eq!(info.version.number, "9.2.4");
        let seen = seen.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
        assert_eq!(seen[0].0, "GET");
        assert_eq!(seen[0].1, "http://10.0.0.5:9200/");
        Ok(())
    }

    #[test]
    fn query_errors_carry_the_elasticsearch_reason() {
        let (elastic, _) = client(
            kibana(),
            400,
            r#"{"error": {"type": "verification_exception", "reason": "Found 1 problem\nline 1:22: Unknown column [level], did you mean [log.level]?"}, "status": 400}"#,
        );
        let error =
            futures::executor::block_on(elastic.esql("FROM logs-* | WHERE level == 1", None))
                .expect_err("a consulta tem erro");
        match error {
            ElasticError::BadRequest { kind, message } => {
                assert_eq!(kind, "verification_exception");
                assert!(message.contains("did you mean [log.level]"));
            }
            other => panic!("erro inesperado: {other:?}"),
        }
    }

    #[test]
    fn console_proxy_status_header_wins() {
        let http = FakeHttpClient::create(|_| async move {
            Ok(Response::builder()
                .status(200)
                .header("x-console-proxy-status-code", "403")
                .body(AsyncBody::from(
                    r#"{"error": {"type": "security_exception", "reason": "action [cluster:monitor/main] is unauthorized for user [vinicios]"}, "status": 403}"#,
                ))?)
        });
        let elastic = Elastic::new(http, kibana(), basic());
        let error = futures::executor::block_on(elastic.info()).expect_err("sem monitor");
        match error {
            ElasticError::Forbidden { message } => {
                assert!(message.contains("cluster:monitor/main"))
            }
            other => panic!("erro inesperado: {other:?}"),
        }
    }

    #[test]
    fn error_bodies_with_success_status_are_errors() {
        let (elastic, _) = client(
            kibana(),
            200,
            r#"{"error": {"type": "security_exception", "reason": "unauthorized"}, "status": 403}"#,
        );
        let error = futures::executor::block_on(elastic.info()).expect_err("erro");
        assert!(matches!(error, ElasticError::Forbidden { .. }));
    }

    #[test]
    fn kibana_login_errors_name_kibana() {
        let (elastic, _) = client(
            kibana(),
            401,
            r#"{"statusCode": 401, "error": "Unauthorized", "message": "Unauthorized"}"#,
        );
        let error = futures::executor::block_on(elastic.info()).expect_err("login recusado");
        assert!(matches!(
            error,
            ElasticError::Unauthorized {
                via: "O Kibana",
                ..
            }
        ));
    }

    #[test]
    fn missing_privileges_lists_only_denied_ones() {
        let response = json!({
            "has_all_requested": false,
            "cluster": { "monitor": true },
            "index": {
                "logs-*": { "read": true, "view_index_metadata": true },
                "traces-apm*": { "read": false, "view_index_metadata": true }
            }
        });
        let missing = parse_missing_privileges(&response);
        assert!(missing.cluster.is_empty());
        assert_eq!(
            missing.index,
            vec![("traces-apm*".to_string(), vec!["read".to_string()])]
        );
    }

    #[test]
    fn fields_skip_metadata_and_objects() {
        let response = json!({
            "indices": ["a"],
            "fields": {
                "_id": { "_id": {} },
                "log": { "object": {} },
                "log.level": { "keyword": {} },
                "@timestamp": { "date": {} }
            }
        });
        let names: Vec<_> = parse_fields(&response)
            .into_iter()
            .map(|field| field.name)
            .collect();
        assert_eq!(names, vec!["@timestamp", "log.level"]);
    }
}
