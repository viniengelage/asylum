use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::{borrow::Cow, cell::RefCell};

use agent_client_protocol::schema::v2 as acp;
use anyhow::{Context as _, Result, bail};
use futures::{AsyncReadExt as _, FutureExt as _};
use gpui::{App, AppContext as _, Task};
use html_to_markdown::{TagHandler, convert_html_to_markdown, markdown};
use http_client::{
    AsyncBody, HttpClient as _, HttpClientWithUrl, HttpRequestExt as _, RedirectPolicy, http,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ui::SharedString;
use util::markdown::{MarkdownEscaped, MarkdownInlineCode};

use super::json_compaction;
use crate::sandboxing::{NetworkRequest, SandboxRequest};
use crate::{AgentTool, ToolCallEventStream, ToolInput};

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Copy)]
enum ContentType {
    Html,
    Plaintext,
    Json,
    Binary,
}

/// Headers that carry credentials for the host they were written for, so a redirect to another
/// host must not forward them.
const CREDENTIAL_HEADERS: [&str; 3] = ["authorization", "cookie", "proxy-authorization"];

/// Response headers worth showing for an API request; the rest are mostly transport noise.
const SHOWN_RESPONSE_HEADERS: [&str; 8] = [
    "content-type",
    "location",
    "set-cookie",
    "www-authenticate",
    "retry-after",
    "etag",
    "x-request-id",
    "x-ratelimit-remaining",
];

/// The maximum number of HTTP redirects the fetch tool will follow. Each hop is
/// re-authorized against the shared network grants before being followed.
const MAX_REDIRECTS: usize = 20;

/// The outcome of a single (non-redirect-following) HTTP request.
enum FetchStep {
    /// The server responded with a redirect to this absolute URL. Its host must
    /// be authorized before the redirect is followed.
    Redirect(String),
    /// A terminal response was received and converted to Markdown.
    /// `saved_bytes` is how much compacting a JSON response removed.
    Complete { text: String, saved_bytes: usize },
}

/// One hop of a fetch: what is requested from `url`.
#[derive(Clone, Debug, PartialEq)]
struct FetchRequest {
    url: String,
    method: http::Method,
    headers: BTreeMap<String, String>,
    body: Option<String>,
}

impl FetchRequest {
    fn from_input(input: &FetchToolInput) -> Result<Self> {
        let method = match input.method.as_deref().map(str::trim) {
            None | Some("") => http::Method::GET,
            Some(method) => http::Method::from_bytes(method.to_ascii_uppercase().as_bytes())
                .with_context(|| format!("invalid HTTP method {method:?}"))?,
        };
        Ok(Self {
            url: input.url.clone(),
            method,
            headers: input.headers.clone(),
            body: input.body.clone(),
        })
    }

    /// A request beyond a plain GET of a page: its response shows the status and headers, and
    /// error statuses are returned as content, since an API's error body is what the caller
    /// needs to see.
    fn is_api_request(&self) -> bool {
        self.method != http::Method::GET || !self.headers.is_empty() || self.body.is_some()
    }

    /// The request to make after `status` redirected this one to `target`, following what
    /// browsers and curl do: 303 (and 301/302 after a POST) turn into a GET without a body,
    /// 307/308 repeat the request, and credentials never cross to another host.
    fn redirected(&self, status: http::StatusCode, target: String) -> Self {
        let becomes_get = status == http::StatusCode::SEE_OTHER
            || (self.method == http::Method::POST
                && matches!(
                    status,
                    http::StatusCode::MOVED_PERMANENTLY | http::StatusCode::FOUND
                ));
        let same_host = host_of(&self.url).is_some() && host_of(&self.url) == host_of(&target);
        let mut headers = self.headers.clone();
        if !same_host {
            headers.retain(|name, _| {
                !CREDENTIAL_HEADERS.contains(&name.to_ascii_lowercase().as_str())
            });
        }
        if becomes_get {
            headers.retain(|name, _| !name.eq_ignore_ascii_case("content-type"));
        }
        Self {
            url: target,
            method: if becomes_get {
                http::Method::GET
            } else {
                self.method.clone()
            },
            headers,
            body: if becomes_get { None } else { self.body.clone() },
        }
    }
}

fn host_of(url: &str) -> Option<String> {
    url::Url::parse(&normalize_url(url))
        .ok()?
        .host_str()
        .map(str::to_ascii_lowercase)
}

fn content_type_of(header: Option<&str>) -> ContentType {
    let Some(header) = header else {
        return ContentType::Plaintext;
    };
    let essence = header
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if essence == "application/json" || essence.ends_with("+json") {
        ContentType::Json
    } else if essence == "text/html" || essence == "application/xhtml+xml" {
        ContentType::Html
    } else if essence.starts_with("text/")
        || essence.ends_with("xml")
        || essence == "application/javascript"
        || essence == "application/x-www-form-urlencoded"
        || essence == "application/graphql"
    {
        ContentType::Plaintext
    } else if essence.is_empty() {
        ContentType::Html
    } else {
        ContentType::Binary
    }
}

/// Prepends `https://` when the URL has no explicit HTTP(S) scheme, matching the
/// behavior the fetch tool has always had for user/model-supplied URLs.
fn normalize_url(url: &str) -> Cow<'_, str> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        Cow::Owned(format!("https://{url}"))
    } else {
        Cow::Borrowed(url)
    }
}

/// Fetches a URL and returns the content as Markdown.
///
/// This tool is not run inside the terminal OS sandbox, but it still refuses to
/// reach any host that hasn't been granted network access. It shares the same
/// per-host grants as the `terminal` tool: approving a host for one authorizes
/// it for the other, whether the grant is for this thread or saved permanently.
/// HTTP redirects are followed one hop at a time, and each hop's host must be
/// granted the same way, so a granted host can't redirect the request to a host
/// that hasn't been approved.
/// When unsandboxed access has been granted, these restrictions are lifted
/// entirely, matching the terminal, which is also how loopback and IP-literal
/// hosts (which can't be granted individually) become reachable.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
///
/// Besides reading pages, it calls HTTP APIs: pass `method`, `headers` (for example
/// `Authorization` or an API key) and `body`. Such requests return the status line and the
/// relevant response headers before the body, including for error statuses. Binary responses
/// are described rather than shown.
pub struct FetchToolInput {
    /// The URL to fetch.
    url: String,
    /// The HTTP method, such as GET, POST, PUT, PATCH or DELETE. Defaults to GET.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    method: Option<String>,
    /// Request headers by name, such as `Authorization`, `Accept` or `Content-Type`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    headers: BTreeMap<String, String>,
    /// The request body, sent as given. Set a matching `Content-Type` header, such as
    /// `application/json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    body: Option<String>,
}

pub struct FetchTool {
    http_client: Arc<HttpClientWithUrl>,
}

impl FetchTool {
    pub fn new(http_client: Arc<HttpClientWithUrl>) -> Self {
        Self { http_client }
    }

    /// Performs a single HTTP request *without* following redirects, so the tool can
    /// re-authorize each hop against the shared network grants before following
    /// it. Returns the redirect target when the server responds with a 3xx, or
    /// the final content converted to Markdown otherwise.
    async fn fetch_step(
        http_client: Arc<HttpClientWithUrl>,
        request: &FetchRequest,
    ) -> Result<(http::StatusCode, FetchStep)> {
        let normalized = normalize_url(&request.url);

        let mut builder = http::Request::builder()
            .method(request.method.clone())
            .uri(normalized.as_ref())
            .follow_redirects(RedirectPolicy::NoFollow);
        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let body = match &request.body {
            Some(body) => AsyncBody::from(body.clone()),
            None => AsyncBody::default(),
        };
        let http_request = builder
            .body(body)
            .with_context(|| format!("invalid request to {normalized:?}"))?;
        let mut response = http_client.send(http_request).await?;

        let status = response.status();
        // 304 Not Modified is a 3xx without a Location to follow.
        if status.is_redirection() && status != http::StatusCode::NOT_MODIFIED {
            let location = response
                .headers()
                .get("location")
                .context("redirect response is missing a Location header")?
                .to_str()
                .context("redirect response has an invalid Location header")?;
            let target = url::Url::parse(&normalized)
                .with_context(|| format!("could not parse URL {normalized:?}"))?
                .join(location)
                .with_context(|| format!("invalid redirect target {location:?}"))?;
            anyhow::ensure!(
                matches!(target.scheme(), "http" | "https"),
                "refusing to follow redirect to non-HTTP(S) URL {target}"
            );
            return Ok((status, FetchStep::Redirect(target.to_string())));
        }

        let mut body = Vec::new();
        response
            .body_mut()
            .read_to_end(&mut body)
            .await
            .context("error reading response body")?;

        let api_request = request.is_api_request();
        if status.is_client_error() && !api_request {
            let text = String::from_utf8_lossy(body.as_slice());
            bail!("status error {}, response: {text:?}", status.as_u16());
        }

        let content_type_header = match response.headers().get("content-type") {
            Some(value) => Some(value.to_str().context("invalid Content-Type header")?),
            None => None,
        };
        if content_type_header.is_none() && !api_request && !body.is_empty() {
            bail!("missing Content-Type header");
        }
        let content_type = content_type_of(content_type_header);

        let (text, saved_bytes) =
            Self::body_text(content_type, content_type_header, &body, &normalized)?;
        let text = if api_request {
            let mut head = format!(
                "HTTP {} {}",
                status.as_u16(),
                status.canonical_reason().unwrap_or_default()
            );
            for (name, value) in response.headers() {
                if SHOWN_RESPONSE_HEADERS.contains(&name.as_str()) {
                    head.push_str(&format!(
                        "\n{name}: {}",
                        String::from_utf8_lossy(value.as_bytes())
                    ));
                }
            }
            if text.trim().is_empty() {
                format!("{head}\n\n(empty body)")
            } else {
                format!("{head}\n\n{text}")
            }
        } else {
            text
        };

        Ok((status, FetchStep::Complete { text, saved_bytes }))
    }

    fn body_text(
        content_type: ContentType,
        content_type_header: Option<&str>,
        body: &[u8],
        normalized: &str,
    ) -> Result<(String, usize)> {
        if body.is_empty() {
            return Ok((String::new(), 0));
        }
        let text = match content_type {
            ContentType::Html => {
                let mut handlers: Vec<TagHandler> = vec![
                    Rc::new(RefCell::new(markdown::WebpageChromeRemover)),
                    Rc::new(RefCell::new(markdown::ParagraphHandler)),
                    Rc::new(RefCell::new(markdown::HeadingHandler)),
                    Rc::new(RefCell::new(markdown::ListHandler)),
                    Rc::new(RefCell::new(markdown::TableHandler::new())),
                    Rc::new(RefCell::new(markdown::StyledTextHandler)),
                ];
                if normalized.contains("wikipedia.org") {
                    use html_to_markdown::structure::wikipedia;

                    handlers.push(Rc::new(RefCell::new(wikipedia::WikipediaChromeRemover)));
                    handlers.push(Rc::new(RefCell::new(wikipedia::WikipediaInfoboxHandler)));
                    handlers.push(Rc::new(
                        RefCell::new(wikipedia::WikipediaCodeHandler::new()),
                    ));
                } else {
                    handlers.push(Rc::new(RefCell::new(markdown::CodeHandler)));
                }

                convert_html_to_markdown(body, &mut handlers)?
            }
            ContentType::Plaintext => String::from_utf8_lossy(body).into_owned(),
            ContentType::Binary => format!(
                "Binary response not shown: {} bytes of {}.",
                body.len(),
                content_type_header.unwrap_or("unknown type")
            ),
            ContentType::Json => {
                let json: serde_json::Value = serde_json::from_slice(body)?;
                let pretty = serde_json::to_string_pretty(&json)?;

                match json_compaction::compact_json(&pretty) {
                    Some(compacted) => {
                        let text = format!("```json\n{}\n```", compacted.text);
                        let saved_bytes = pretty.len().saturating_sub(text.len());
                        let text = match compacted.note() {
                            Some(note) => format!("{text}\n\n{note}"),
                            None => text,
                        };
                        return Ok((text, saved_bytes));
                    }
                    None => format!("```json\n{pretty}\n```"),
                }
            }
        };
        Ok((text, 0))
    }
}

/// Resolve the host of `url` and confirm it doesn't point into loopback /
/// private / link-local space, applying the same forbidden-IP policy the
/// terminal sandbox's proxy uses. Returns an error (including "resolves only to
/// forbidden addresses") that aborts the fetch.
///
/// DNS resolution blocks, so callers should run this off the foreground thread.
/// See the caller for why this is a gate rather than a full resolve-to-connect
/// pin.
fn verify_host_not_forbidden(url: &str) -> Result<()> {
    let normalized = normalize_url(url);
    let parsed =
        url::Url::parse(&normalized).with_context(|| format!("could not parse URL {url:?}"))?;
    let host = parsed
        .host_str()
        .with_context(|| format!("URL {url:?} has no host to reach"))?;
    // Default to the scheme's port when the URL omits one; resolution needs a
    // port but the value doesn't affect which IPs a host resolves to.
    let port = parsed
        .port_or_known_default()
        .unwrap_or(if parsed.scheme() == "http" { 80 } else { 443 });

    http_proxy::PinnedHost::resolve(host, port).map(|_pinned| ())?;
    Ok(())
}

/// Extracts the host from a fetch URL as a [`http_proxy::HostPattern`] so it can
/// be matched against the shared network grants. Mirrors the scheme handling in
/// [`normalize_url`] (defaulting to `https://` when none is given).
fn host_pattern_for_url(url: &str) -> Result<http_proxy::HostPattern> {
    let normalized = normalize_url(url);
    let parsed =
        url::Url::parse(&normalized).with_context(|| format!("could not parse URL {url:?}"))?;
    let host = parsed
        .host_str()
        .with_context(|| format!("URL {url:?} has no host to authorize network access for"))?;
    http_proxy::HostPattern::parse(host).map_err(|error| match error {
        http_proxy::HostPatternError::IpLiteral(_) => anyhow::anyhow!(
            "cannot fetch {host:?}: loopback and IP-literal hosts can't be granted network \
             access individually. They are only reachable once unsandboxed access has been \
             granted (for example, via a terminal command that requests it)."
        ),
        error => anyhow::anyhow!("cannot authorize network access to {host:?}: {error}"),
    })
}

impl AgentTool for FetchTool {
    type Input = FetchToolInput;
    type Output = String;

    const NAME: &'static str = "fetch";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Fetch
    }

    fn allow_in_restricted_mode() -> bool {
        false
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => match input.method.as_deref().map(str::trim) {
                Some(method) if !method.is_empty() && !method.eq_ignore_ascii_case("GET") => {
                    format!(
                        "Fetch {} {}",
                        method.to_ascii_uppercase(),
                        MarkdownEscaped(&input.url)
                    )
                    .into()
                }
                _ => format!("Fetch {}", MarkdownEscaped(&input.url)).into(),
            },
            Err(_) => "Fetch URL".into(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<Self::Output, Self::Output>> {
        let http_client = self.http_client.clone();
        cx.spawn(async move |cx| {
            let input: FetchToolInput = input.recv().await.map_err(|e| e.to_string())?;
            let mut request = FetchRequest::from_input(&input).map_err(|e| e.to_string())?;
            let api_request = request.is_api_request();

            // First, the standard tool-permission gate (honors the fetch tool's
            // allow/deny/confirm rules).
            let authorize = cx.update(|cx| {
                let context =
                    crate::ToolPermissionContext::new(Self::NAME, vec![input.url.clone()]);

                let title = if request.method == http::Method::GET {
                    format!("Fetch {}", MarkdownInlineCode(&input.url))
                } else {
                    format!(
                        "Fetch {} {}",
                        request.method,
                        MarkdownInlineCode(&input.url)
                    )
                };
                event_stream.authorize(title, context, cx)
            });
            futures::select! {
                result = authorize.fuse() => result.map_err(|e| e.to_string())?,
                _ = event_stream.cancelled_by_user().fuse() => {
                    return Err("Fetch cancelled by user".to_string());
                }
            };

            // Then, unless unsandboxed access is already in effect, the per-host
            // network grant shared with the terminal tool. If the host isn't
            // already granted (for this thread or in saved settings) the user is
            // shown the same escalation prompt the terminal uses; a denial
            // aborts the fetch. This tool never runs inside the OS sandbox, so
            // the grant is only consulted to decide whether the request may
            // proceed. When unsandboxed access has been granted the terminal
            // already runs without isolation, so we drop fetch's restrictions
            // too — including reaching hosts that can't be granted individually
            // (loopback and IP literals).
            //
            // Crucially, this authorization is applied to every redirect hop as
            // well as the initial URL, so a granted host can't 30x-redirect the
            // fetch to a host the user never approved. We disable the HTTP
            // client's own redirect following and re-run the grant for each hop
            // before requesting it.
            //
            // When the sandboxing feature flag is off the terminal isn't
            // sandboxed either, so gating fetch by host would provide no
            // isolation; skip it entirely (the pre-sandboxing behavior).
            let unsandboxed = cx.update(|cx| {
                !crate::sandboxing::sandboxing_enabled(cx)
                    || event_stream.unsandboxed_access_granted(cx)
            });

            let mut redirects = 0;
            let text = loop {
                if !unsandboxed {
                    let host = host_pattern_for_url(&request.url).map_err(|e| e.to_string())?;
                    let authorize_host = cx.update(|cx| {
                        let request = SandboxRequest {
                            network: NetworkRequest::Hosts(vec![host]),
                            ..Default::default()
                        };
                        event_stream.authorize_sandbox(request, String::new(), cx)
                    });
                    futures::select! {
                        result = authorize_host.fuse() => result.map_err(|e| e.to_string())?,
                        _ = event_stream.cancelled_by_user().fuse() => {
                            return Err("Fetch cancelled by user".to_string());
                        }
                    };

                    // Authorizing the *hostname* is not enough: a granted host
                    // (or a redirect to one) whose DNS points into loopback /
                    // private / link-local space would otherwise let the model
                    // reach the local machine or LAN (SSRF / DNS rebinding).
                    // Resolve and vet the host now, applying the same
                    // forbidden-IP policy the terminal sandbox's proxy enforces.
                    //
                    // NOTE: this is a gate, not a full pin. `HttpClientWithUrl`
                    // resolves the hostname again when it connects, so a DNS
                    // answer that flips between this check and that connect could
                    // still slip through. Closing that residual window would
                    // require the HTTP client to connect to a pre-vetted IP
                    // (`PinnedHost::socket_addrs`) rather than re-resolving;
                    // until then this blocks the realistic case of a stably
                    // resolving host that points at forbidden space.
                    let verify_task = cx.background_spawn({
                        let url = request.url.clone();
                        async move { verify_host_not_forbidden(&url) }
                    });
                    futures::select! {
                        result = verify_task.fuse() => result.map_err(|e| e.to_string())?,
                        _ = event_stream.cancelled_by_user().fuse() => {
                            return Err("Fetch cancelled by user".to_string());
                        }
                    };
                }

                let fetch_task = cx.background_spawn({
                    let http_client = http_client.clone();
                    let request = request.clone();
                    async move { Self::fetch_step(http_client, &request).await }
                });

                let (status, step) = futures::select! {
                    result = fetch_task.fuse() => result.map_err(|e| e.to_string())?,
                    _ = event_stream.cancelled_by_user().fuse() => {
                        return Err("Fetch cancelled by user".to_string());
                    }
                };

                match step {
                    FetchStep::Complete { text, saved_bytes } => {
                        event_stream.record_compacted_output(saved_bytes, cx);
                        break text;
                    }
                    FetchStep::Redirect(target) => {
                        redirects += 1;
                        if redirects > MAX_REDIRECTS {
                            return Err(format!(
                                "exceeded the maximum of {MAX_REDIRECTS} redirects"
                            ));
                        }
                        request = request.redirected(status, target);
                    }
                }
            };

            if text.trim().is_empty() && !api_request {
                return Err("no textual content found".to_string());
            }
            Ok(text)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These use IP-literal URLs, which "resolve" to themselves, so the SSRF gate
    // is exercised without depending on real DNS. IP literals can't be *granted*
    // network access (that's a separate, earlier check), but they can be the
    // target a granted hostname redirects to or resolves into — which is exactly
    // the case this gate defends.

    #[test]
    fn verify_host_rejects_loopback_literal() {
        let error = verify_host_not_forbidden("http://127.0.0.1/internal")
            .expect_err("loopback must be refused");
        assert!(
            error.to_string().contains("loopback"),
            "error should explain the forbidden range, got: {error}"
        );
    }

    #[test]
    fn verify_host_rejects_private_and_metadata_literals() {
        for url in [
            "http://10.0.0.5/",
            "https://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/", // cloud metadata
            "http://[::1]/",
        ] {
            assert!(
                verify_host_not_forbidden(url).is_err(),
                "expected {url} to be refused as a forbidden destination"
            );
        }
    }

    fn input(method: Option<&str>, headers: &[(&str, &str)], body: Option<&str>) -> FetchToolInput {
        FetchToolInput {
            url: "https://api.example.com/items".into(),
            method: method.map(str::to_string),
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            body: body.map(str::to_string),
        }
    }

    #[test]
    fn plain_gets_stay_page_reads() {
        let request = FetchRequest::from_input(&input(None, &[], None)).unwrap();
        assert_eq!(request.method, http::Method::GET);
        assert!(!request.is_api_request());
        let request = FetchRequest::from_input(&input(Some("patch"), &[], None)).unwrap();
        assert_eq!(request.method, http::Method::PATCH);
        assert!(request.is_api_request());
        assert!(FetchRequest::from_input(&input(Some("NOT A METHOD"), &[], None)).is_err());
    }

    #[test]
    fn redirects_follow_browser_rules_and_keep_credentials_on_their_host() {
        let request = FetchRequest::from_input(&input(
            Some("POST"),
            &[
                ("Authorization", "Bearer secret"),
                ("Content-Type", "application/json"),
                ("Accept", "application/json"),
            ],
            Some("{}"),
        ))
        .unwrap();

        let repeated = request.redirected(
            http::StatusCode::TEMPORARY_REDIRECT,
            "https://api.example.com/v2/items".into(),
        );
        assert_eq!(repeated.method, http::Method::POST);
        assert_eq!(repeated.body.as_deref(), Some("{}"));
        assert!(repeated.headers.contains_key("Authorization"));

        let moved = request.redirected(
            http::StatusCode::SEE_OTHER,
            "https://cdn.example.net/result".into(),
        );
        assert_eq!(moved.method, http::Method::GET);
        assert_eq!(moved.body, None);
        assert_eq!(
            moved.headers.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["Accept"]
        );
    }

    #[test]
    fn content_types_pick_how_the_body_is_shown() {
        assert_eq!(
            content_type_of(Some("application/problem+json")),
            ContentType::Json
        );
        assert_eq!(
            content_type_of(Some("text/html; charset=utf-8")),
            ContentType::Html
        );
        assert_eq!(
            content_type_of(Some("application/xml")),
            ContentType::Plaintext
        );
        assert_eq!(content_type_of(Some("text/csv")), ContentType::Plaintext);
        assert_eq!(content_type_of(Some("image/png")), ContentType::Binary);
        assert_eq!(content_type_of(None), ContentType::Plaintext);
    }

    #[test]
    fn api_requests_send_method_headers_and_body_and_return_errors_as_content() {
        let client = http_client::FakeHttpClient::create(|request| async move {
            assert_eq!(request.method(), http::Method::POST);
            assert_eq!(request.headers()["x-api-key"], "abc");
            let mut body = String::new();
            let mut incoming = request.into_body();
            incoming.read_to_string(&mut body).await?;
            assert_eq!(body, r#"{"name":"x"}"#);
            Ok(http::Response::builder()
                .status(422)
                .header("content-type", "application/json")
                .header("x-request-id", "req-1")
                .header("server", "noise")
                .body(AsyncBody::from(r#"{"error":"name taken"}"#.to_string()))?)
        });
        let request = FetchRequest::from_input(&input(
            Some("POST"),
            &[("x-api-key", "abc")],
            Some(r#"{"name":"x"}"#),
        ))
        .unwrap();
        let (status, step) =
            futures::executor::block_on(FetchTool::fetch_step(client, &request)).unwrap();
        assert_eq!(status.as_u16(), 422);
        let FetchStep::Complete { text, .. } = step else {
            panic!("expected a complete response");
        };
        assert!(
            text.starts_with("HTTP 422 Unprocessable Entity\n"),
            "{text}"
        );
        assert!(text.contains("x-request-id: req-1"));
        assert!(!text.contains("server: noise"));
        assert!(text.contains("name taken"));
    }

    #[test]
    fn verify_host_allows_public_literal() {
        verify_host_not_forbidden("https://93.184.215.14/")
            .expect("a public address must be allowed through the gate");
    }
}
