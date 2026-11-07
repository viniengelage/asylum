//! Loopback OAuth 2.0 callback server and the HTML page the browser shows after authorizing.

#[cfg(not(target_family = "wasm"))]
mod server {
    use anyhow::{Context as _, Result, anyhow};
    use i18n::t;
    use std::str::FromStr;
    use std::time::Duration;
    use url::Url;

    enum CallbackOutcome<'a> {
        Success,
        Failure { detail: &'a str },
    }

    // The browser renders this outside the app, so the Polar v2 values are CSS literals
    // rather than theme tokens.
    fn oauth_callback_page(outcome: CallbackOutcome) -> String {
        let lang = i18n::current_language().code();
        let (tile_class, tile_svg, title, body, detail) = match outcome {
            CallbackOutcome::Success => (
                "tile success",
                r#"<svg viewBox="0 0 24 24"><polyline points="20 6 9 17 4 12"/></svg>"#,
                t!("oauth_callback.success.title"),
                t!("oauth_callback.success.body").to_string(),
                String::new(),
            ),
            CallbackOutcome::Failure { detail } => (
                "tile failure",
                r#"<svg viewBox="0 0 24 24"><path d="M10.29 3.86 1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z"/><line x1="12" y1="9" x2="12" y2="13"/><line x1="12" y1="17" x2="12.01" y2="17"/></svg>"#,
                t!("oauth_callback.failure.title"),
                t!("oauth_callback.failure.body").to_string(),
                format!(r#"<div class="detail">{}</div>"#, html_escape(detail)),
            ),
        };
        let title = html_escape(&title);
        let body = html_escape(&body);
        format!(
            r#"<!DOCTYPE html>
<html lang="{lang}">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} — Asylum</title>
<style>
  * {{ margin: 0; padding: 0; box-sizing: border-box; }}
  body {{
    font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Ubuntu, sans-serif;
    background: #060509;
    color: #edeaf5;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 20px;
    min-height: 100vh;
    padding: 16px;
  }}
  .card {{
    background: #100e17;
    border: 1px solid #ffffff0f;
    border-radius: 16px;
    padding: 28px 32px;
    width: 340px;
    max-width: 100%;
    text-align: center;
    box-shadow: 0 12px 40px rgba(0, 0, 0, 0.45);
  }}
  .tile {{
    width: 44px;
    height: 44px;
    margin: 0 auto 18px;
    border-radius: 12px;
    display: flex;
    align-items: center;
    justify-content: center;
  }}
  .tile svg {{
    width: 22px;
    height: 22px;
    stroke-width: 2.5;
    stroke-linecap: round;
    stroke-linejoin: round;
    fill: none;
  }}
  .tile.success {{ background: rgba(190, 242, 100, 0.12); }}
  .tile.success svg {{ stroke: #bef264; }}
  .tile.failure {{ background: rgba(251, 113, 133, 0.12); }}
  .tile.failure svg {{ stroke: #fb7185; }}
  h1 {{
    font-size: 17px;
    font-weight: 600;
    margin-bottom: 8px;
    color: #edeaf5;
  }}
  p {{
    font-size: 13px;
    line-height: 1.5;
    color: #9c96ae;
  }}
  .detail {{
    margin-top: 16px;
    padding: 10px 12px;
    font-family: ui-monospace, 'IBM Plex Mono', Menlo, monospace;
    font-size: 12px;
    line-height: 1.5;
    color: #edeaf5;
    text-align: left;
    word-break: break-word;
    background: rgba(251, 113, 133, 0.06);
    border: 1px solid rgba(251, 113, 133, 0.20);
    border-radius: 8px;
  }}
  .brand {{
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 12px;
    color: #9c96ae;
  }}
  .brand .dot {{
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: #7c3aed;
  }}
</style>
</head>
<body>
<div class="card">
  <div class="{tile_class}">{tile_svg}</div>
  <h1>{title}</h1>
  <p>{body}</p>
  {detail}
</div>
<div class="brand"><span class="dot"></span>Asylum</div>
</body>
</html>"#
        )
    }

    fn html_escape(input: &str) -> String {
        let mut output = String::with_capacity(input.len());
        for ch in input.chars() {
            match ch {
                '&' => output.push_str("&amp;"),
                '<' => output.push_str("&lt;"),
                '>' => output.push_str("&gt;"),
                '"' => output.push_str("&quot;"),
                '\'' => output.push_str("&#x27;"),
                _ => output.push(ch),
            }
        }
        output
    }

    /// Parsed OAuth callback parameters from the authorization server redirect.
    pub struct OAuthCallbackParams {
        pub code: String,
        pub state: String,
    }

    /// Configuration for the loopback OAuth callback server.
    ///
    /// OAuth servers compare `redirect_uri` against a per-client allow-list using
    /// exact string matching (RFC 6749 §3.1.2), so the `host`, `preferred_port`,
    /// and `path` here must match what's registered for the OAuth client_id.
    #[derive(Clone, Copy)]
    pub struct OAuthCallbackServerConfig {
        /// Host portion of the redirect URI (typically `127.0.0.1` or `localhost`).
        pub host: &'static str,
        /// Preferred port. Use `0` for an OS-assigned ephemeral port.
        pub preferred_port: u16,
        /// Optional fallback port if `preferred_port` is unavailable. Only used
        /// when `preferred_port` is non-zero.
        pub fallback_port: Option<u16>,
        /// Callback path on the redirect URI (e.g. `/callback`, `/auth/callback`).
        pub path: &'static str,
    }

    impl Default for OAuthCallbackServerConfig {
        fn default() -> Self {
            Self {
                host: "127.0.0.1",
                preferred_port: 0,
                fallback_port: None,
                path: "/callback",
            }
        }
    }

    impl OAuthCallbackParams {
        /// Parse the query string from a callback URL like
        /// `http://127.0.0.1:<port>/callback?code=...&state=...`.
        pub fn parse_query(query: &str) -> Result<Self> {
            let mut code: Option<String> = None;
            let mut state: Option<String> = None;
            let mut error: Option<String> = None;
            let mut error_description: Option<String> = None;

            for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
                match key.as_ref() {
                    "code" => {
                        if !value.is_empty() {
                            code = Some(value.into_owned());
                        }
                    }
                    "state" => {
                        if !value.is_empty() {
                            state = Some(value.into_owned());
                        }
                    }
                    "error" => {
                        if !value.is_empty() {
                            error = Some(value.into_owned());
                        }
                    }
                    "error_description" => {
                        if !value.is_empty() {
                            error_description = Some(value.into_owned());
                        }
                    }
                    _ => {}
                }
            }

            if let Some(error_code) = error {
                anyhow::bail!(
                    "OAuth authorization failed: {} ({})",
                    error_code,
                    error_description.as_deref().unwrap_or("no description")
                );
            }

            let code = code.ok_or_else(|| anyhow!("missing 'code' parameter in OAuth callback"))?;
            let state =
                state.ok_or_else(|| anyhow!("missing 'state' parameter in OAuth callback"))?;

            Ok(Self { code, state })
        }
    }

    /// How long to wait for the browser to complete the OAuth flow before giving
    /// up and releasing the loopback port.
    const OAUTH_CALLBACK_TIMEOUT: Duration = Duration::from_secs(2 * 60);

    /// Start a loopback HTTP server to receive the OAuth authorization callback.
    ///
    /// Binds to an ephemeral loopback port. Returns `(redirect_uri, callback_future)`.
    /// The caller should use the redirect URI in the authorization request, open
    /// the browser, then await the future to receive the callback.
    pub fn start_oauth_callback_server() -> Result<(
        String,
        futures::channel::oneshot::Receiver<Result<OAuthCallbackParams>>,
    )> {
        start_oauth_callback_server_with_config(OAuthCallbackServerConfig::default())
    }

    /// Start a loopback HTTP server with custom host/port/path.
    ///
    /// Use this when the OAuth client requires a specific redirect URI that the
    /// default ephemeral-port `http://127.0.0.1:<port>/callback` doesn't match.
    pub fn start_oauth_callback_server_with_config(
        config: OAuthCallbackServerConfig,
    ) -> Result<(
        String,
        futures::channel::oneshot::Receiver<Result<OAuthCallbackParams>>,
    )> {
        let server = bind_callback_server(&config)?;
        let port = server
            .server_addr()
            .to_ip()
            .ok_or_else(|| anyhow!("server not bound to a TCP address"))?
            .port();

        let redirect_uri = format!("http://{}:{}{}", config.host, port, config.path);
        let expected_path = config.path;

        let (tx, rx) = futures::channel::oneshot::channel();

        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + OAUTH_CALLBACK_TIMEOUT;

            loop {
                if tx.is_canceled() {
                    return;
                }
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    return;
                }

                let timeout = remaining.min(Duration::from_millis(500));
                let Some(request) = (match server.recv_timeout(timeout) {
                    Ok(req) => req,
                    Err(_) => {
                        let _ = tx.send(Err(anyhow!("OAuth callback server I/O error")));
                        return;
                    }
                }) else {
                    continue;
                };

                let raw_url = request.url().to_string();
                let raw_path = raw_url.split('?').next().unwrap_or(&raw_url);
                if raw_path == CANCEL_PATH {
                    let response = tiny_http::Response::from_string("Cancelled")
                        .with_status_code(200)
                        .with_header(
                            tiny_http::Header::from_str("Content-Type: text/plain")
                                .expect("failed to construct response header"),
                        )
                        .with_header(
                            tiny_http::Header::from_str("Connection: close")
                                .expect("failed to construct response header"),
                        );
                    if let Err(err) = request.respond(response) {
                        log::error!("Failed to send OAuth cancel response: {}", err);
                    }
                    let _ = tx.send(Err(anyhow!(
                        "OAuth callback server was cancelled by another sign-in attempt"
                    )));
                    return;
                }

                let result = handle_oauth_callback_request(&request, expected_path);

                let (status_code, body) = match &result {
                    Ok(_) => (
                        200,
                        oauth_callback_page(CallbackOutcome::Success),
                    ),
                    Err(err) => {
                        log::error!("OAuth callback error: {}", err);
                        (
                            400,
                            oauth_callback_page(CallbackOutcome::Failure {
                                detail: &format!("{err}"),
                            }),
                        )
                    }
                };

                let response = tiny_http::Response::from_string(body)
                    .with_status_code(status_code)
                    .with_header(
                        tiny_http::Header::from_str("Content-Type: text/html; charset=utf-8")
                            .expect("failed to construct response header"),
                    )
                    .with_header(
                        tiny_http::Header::from_str("Keep-Alive: timeout=0,max=0")
                            .expect("failed to construct response header"),
                    );
                if let Err(err) = request.respond(response) {
                    log::error!("Failed to send OAuth callback response: {}", err);
                }

                let _ = tx.send(result);
                return;
            }
        });

        Ok((redirect_uri, rx))
    }

    fn handle_oauth_callback_request(
        request: &tiny_http::Request,
        expected_path: &str,
    ) -> Result<OAuthCallbackParams> {
        let url = Url::parse(&format!("http://localhost{}", request.url()))
            .context("malformed callback request URL")?;

        if url.path() != expected_path {
            anyhow::bail!("unexpected path in OAuth callback: {}", url.path());
        }

        let query = url
            .query()
            .ok_or_else(|| anyhow!("OAuth callback has no query string"))?;
        OAuthCallbackParams::parse_query(query)
    }

    /// Callback path reserved for evicting a previously-running OAuth callback
    /// server bound to the same port. Always handled, regardless of `config.path`.
    const CANCEL_PATH: &str = "/cancel";

    const BIND_MAX_ATTEMPTS: u32 = 10;
    const BIND_RETRY_DELAY: Duration = Duration::from_millis(200);
    const CANCEL_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

    fn bind_callback_server(config: &OAuthCallbackServerConfig) -> Result<tiny_http::Server> {
        // Ephemeral ports always succeed; skip the cancel-retry dance entirely.
        if config.preferred_port == 0 {
            let addr = format!("{}:0", config.host);
            return tiny_http::Server::http(&addr).map_err(|err| {
                anyhow!(err).context(format!(
                    "Failed to bind loopback listener for OAuth callback on {addr}"
                ))
            });
        }

        match try_bind_with_cancel(config.host, config.preferred_port) {
            Ok(server) => Ok(server),
            Err(primary_err) => {
                let Some(fallback_port) = config.fallback_port else {
                    return Err(primary_err.context(format!(
                        "Failed to bind loopback listener for OAuth callback on {}:{}",
                        config.host, config.preferred_port,
                    )));
                };
                log::warn!(
                    "OAuth callback port {}:{} unavailable; falling back to port {}",
                    config.host,
                    config.preferred_port,
                    fallback_port,
                );
                try_bind_with_cancel(config.host, fallback_port).map_err(|fallback_err| {
                    fallback_err.context(format!(
                        "Failed to bind loopback listener for OAuth callback on {}:{} or {}:{}",
                        config.host, config.preferred_port, config.host, fallback_port,
                    ))
                })
            }
        }
    }

    /// Attempts to bind to a fixed `host:port`. On `AddrInUse`, sends a single
    /// `GET /cancel` to the existing listener (to evict a previous OAuth flow
    /// from this or a compatible client) and retries.
    fn try_bind_with_cancel(host: &'static str, port: u16) -> Result<tiny_http::Server> {
        let addr = format!("{host}:{port}");
        let mut cancel_attempted = false;
        let mut last_err: Option<anyhow::Error> = None;

        for _ in 0..BIND_MAX_ATTEMPTS {
            match tiny_http::Server::http(&addr) {
                Ok(server) => return Ok(server),
                Err(err) => {
                    let is_addr_in_use = err
                        .downcast_ref::<std::io::Error>()
                        .map(|io_err| io_err.kind() == std::io::ErrorKind::AddrInUse)
                        .unwrap_or(false);

                    if !is_addr_in_use {
                        return Err(anyhow!(err).context(format!(
                            "Failed to bind loopback listener for OAuth callback on {addr}"
                        )));
                    }

                    if !cancel_attempted {
                        cancel_attempted = true;
                        if let Err(cancel_err) = send_cancel_request(host, port) {
                            log::warn!(
                                "Failed to cancel previous OAuth callback server on {addr}: {cancel_err}"
                            );
                        }
                    }

                    last_err = Some(anyhow!(err));
                    std::thread::sleep(BIND_RETRY_DELAY);
                }
            }
        }

        Err(last_err
            .unwrap_or_else(|| anyhow!("unknown bind error"))
            .context(format!(
                "OAuth callback port {addr} remained in use after {BIND_MAX_ATTEMPTS} attempts"
            )))
    }

    /// Sends `GET /cancel` to a listener on `host:port`, asking it to shut down.
    ///
    /// Best-effort: errors here are surfaced to the caller for logging but do
    /// not block the subsequent rebind attempt.
    fn send_cancel_request(host: &str, port: u16) -> std::io::Result<()> {
        use std::io::{Read as _, Write as _};
        use std::net::{TcpStream, ToSocketAddrs as _};

        let addr = format!("{host}:{port}")
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("could not resolve {host}:{port}"),
                )
            })?;
        let mut stream = TcpStream::connect_timeout(&addr, CANCEL_REQUEST_TIMEOUT)?;
        stream.set_read_timeout(Some(CANCEL_REQUEST_TIMEOUT))?;
        stream.set_write_timeout(Some(CANCEL_REQUEST_TIMEOUT))?;

        stream.write_all(b"GET /cancel HTTP/1.1\r\n")?;
        stream.write_all(format!("Host: {host}:{port}\r\n").as_bytes())?;
        stream.write_all(b"Connection: close\r\n\r\n")?;

        // Drain the response so the server can close cleanly. We don't care
        // about the body; errors here are harmless.
        let mut buf = [0u8; 64];
        let _ = stream.read(&mut buf);
        Ok(())
    }
}

#[cfg(not(target_family = "wasm"))]
pub use server::{
    OAuthCallbackParams, OAuthCallbackServerConfig, start_oauth_callback_server,
    start_oauth_callback_server_with_config,
};
