use crate::WebPreviewView;
use crate::webkit_browser::WebKitPage;
use anyhow::{Context as _, Result, anyhow};
use async_tungstenite::tungstenite::Message;
use base64::Engine as _;
use futures::StreamExt as _;
use gpui::{App, AppContext as _, AsyncApp, Entity, Global, SharedString, WeakEntity};
use i18n::t;
use project::Project;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use task_agents::{ToolAccess, Toolkit, ToolkitOutput, ToolkitTool, scale_screenshot};
use util::ResultExt as _;

const CDP_TIMEOUT: Duration = Duration::from_secs(15);
const EVAL_TIMEOUT: Duration = Duration::from_secs(60);
const LOAD_TIMEOUT: Duration = Duration::from_secs(20);
const BROWSER_START_TIMEOUT: Duration = Duration::from_secs(15);
/// Chromium reports the page's DevTools target right after creating it; a browser still
/// without one by then is used anyway, matching its page by URL.
const TARGET_ID_GRACE: Duration = Duration::from_secs(3);
const READY_POLL_INTERVAL: Duration = Duration::from_millis(250);
const CONSOLE_COLLECT_TIME: Duration = Duration::from_millis(400);

/// Every browser tab, in the order they were opened, so agent tools find the page the user is
/// looking at.
#[derive(Default)]
struct BrowserViews(Vec<WeakEntity<WebPreviewView>>);

impl Global for BrowserViews {}

pub(crate) fn register_view(view: WeakEntity<WebPreviewView>, cx: &mut App) {
    let views = cx.default_global::<BrowserViews>();
    views.0.retain(|view| view.upgrade().is_some());
    views.0.push(view);
}

/// The visible tab if there is one, otherwise the most recently opened.
fn active_view(cx: &App) -> Option<Entity<WebPreviewView>> {
    let views = cx
        .try_global::<BrowserViews>()
        .map(|views| views.0.clone())
        .unwrap_or_default();
    let views = views
        .iter()
        .filter_map(|view| view.upgrade())
        .collect::<Vec<_>>();
    views
        .iter()
        .rev()
        .find(|view| !view.read(cx).hidden && view.read(cx).has_page())
        .or_else(|| views.iter().rev().find(|view| view.read(cx).has_page()))
        .or_else(|| views.last())
        .cloned()
}

struct PageInfo {
    url: String,
    title: String,
    loading: bool,
    hidden: bool,
    target_id: Option<String>,
    /// Why the last navigation failed. Only WebKit reports this; a failed Chromium page
    /// shows up as a `chrome-error://` address instead.
    load_error: Option<String>,
}

impl WebPreviewView {
    fn has_page(&self) -> bool {
        self.browser.is_some() || self.webkit.is_some()
    }
}

fn page_info(view: &Entity<WebPreviewView>, cx: &App) -> PageInfo {
    let view = view.read(cx);
    // Read straight from WebKit: the tab's own copy is refreshed on a timer and still
    // describes the previous page right after a navigation is requested.
    if let Some(page) = &view.webkit {
        let state = page.state();
        return PageInfo {
            url: if state.url.is_empty() {
                view.url.clone()
            } else {
                state.url
            },
            title: if state.title.is_empty() {
                view.title.clone()
            } else {
                state.title
            },
            loading: state.loading,
            hidden: view.hidden,
            target_id: None,
            load_error: state.load_error.map(|error| error.text),
        };
    }
    let state = view
        .browser
        .as_ref()
        .and_then(|browser| browser.current_state());
    PageInfo {
        url: state
            .as_ref()
            .map(|state| state.url.clone())
            .unwrap_or_else(|| view.url.clone()),
        title: state
            .as_ref()
            .map(|state| state.title.clone())
            .unwrap_or_else(|| view.title.clone()),
        loading: state.as_ref().map_or(view.loading, |state| state.loading),
        hidden: view.hidden,
        target_id: view.target_id.clone(),
        load_error: None,
    }
}

async fn with_timeout<T>(
    future: impl std::future::Future<Output = Result<T>>,
    what: &str,
    cx: &AsyncApp,
) -> Result<T> {
    with_timeout_of(CDP_TIMEOUT, future, what, cx).await
}

async fn with_timeout_of<T>(
    timeout: Duration,
    future: impl std::future::Future<Output = Result<T>>,
    what: &str,
    cx: &AsyncApp,
) -> Result<T> {
    let timer = cx.background_executor().timer(timeout);
    futures::pin_mut!(future);
    match futures::future::select(future, timer).await {
        futures::future::Either::Left((result, _)) => result,
        futures::future::Either::Right(_) => Err(anyhow!(t!(
            "web_preview.agent.error.timed_out",
            action = what
        ))),
    }
}

type Socket = async_tungstenite::WebSocketStream<smol::net::TcpStream>;

/// One DevTools connection to a page. Commands are answered in order; events that arrive in
/// between are kept for callers that want them (the console).
struct CdpSession {
    socket: Socket,
    next_id: u64,
    events: Vec<Value>,
}

impl CdpSession {
    /// Attaches to the tab's own target when its id is known; matching on the URL alone
    /// picks the wrong page when a script has the same site open in several tabs.
    async fn connect(target_id: Option<&str>, page_url: &str) -> Result<Self> {
        let port = crate::runtime_discovery::remote_debugging_port();
        let targets = http_get_json(port, "/json/list").await?;
        let targets = targets
            .as_array()
            .context(t!("web_preview.agent.error.empty_page_list"))?;
        let pages = targets
            .iter()
            .filter(|target| target["type"] == "page")
            .filter(|target| {
                !target["url"]
                    .as_str()
                    .is_some_and(|url| url.starts_with("devtools://"))
            })
            .collect::<Vec<_>>();
        let target = pages
            .iter()
            .find(|target| target_id.is_some() && target["id"].as_str() == target_id)
            .or_else(|| {
                pages
                    .iter()
                    .find(|target| target["url"].as_str() == Some(page_url))
            })
            .or_else(|| pages.first())
            .context(t!("web_preview.agent.error.no_page_open"))?;
        let socket_url = target["webSocketDebuggerUrl"]
            .as_str()
            .context(t!("web_preview.agent.error.no_websocket"))?
            .to_string();
        let stream = smol::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .context(t!("web_preview.agent.error.cdp_connect_failed"))?;
        let (socket, _) = async_tungstenite::client_async(socket_url, stream)
            .await
            .context(t!("web_preview.agent.error.cdp_refused"))?;
        Ok(Self {
            socket,
            next_id: 1,
            events: Vec::new(),
        })
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({ "id": id, "method": method, "params": params });
        self.socket
            .send(Message::Text(message.to_string().into()))
            .await?;
        while let Some(message) = self.socket.next().await {
            let text = match message? {
                Message::Text(text) => text.to_string(),
                Message::Close(_) => break,
                _ => continue,
            };
            let value: Value = serde_json::from_str(&text)?;
            if value["id"].as_u64() == Some(id) {
                if let Some(error) = value.get("error") {
                    let message = error["message"]
                        .as_str()
                        .map(SharedString::from)
                        .unwrap_or_else(|| t!("web_preview.agent.error.cdp_error"));
                    anyhow::bail!("{method}: {message}");
                }
                return Ok(value["result"].clone());
            }
            if value.get("method").is_some() {
                self.events.push(value);
            }
        }
        Err(anyhow!(t!(
            "web_preview.agent.error.cdp_closed_during",
            method = method
        )))
    }

    /// Reads events for a while without sending anything.
    async fn collect_events(&mut self, duration: Duration, cx: &AsyncApp) {
        let deadline = cx.background_executor().timer(duration);
        futures::pin_mut!(deadline);
        loop {
            let next = self.socket.next();
            futures::pin_mut!(next);
            match futures::future::select(next, &mut deadline).await {
                futures::future::Either::Left((Some(Ok(Message::Text(text))), _)) => {
                    if let Ok(value) = serde_json::from_str::<Value>(&text) {
                        self.events.push(value);
                    }
                }
                futures::future::Either::Left((Some(Ok(_)), _)) => {}
                futures::future::Either::Left(_) => break,
                futures::future::Either::Right(_) => break,
            }
        }
    }

    async fn evaluate(&mut self, expression: &str) -> Result<Value> {
        let result = self
            .call(
                "Runtime.evaluate",
                json!({ "expression": expression, "returnByValue": true, "awaitPromise": true }),
            )
            .await?;
        if let Some(exception) = result.get("exceptionDetails") {
            let description = exception["exception"]["description"]
                .as_str()
                .or_else(|| exception["text"].as_str())
                .map(SharedString::from)
                .unwrap_or_else(|| t!("web_preview.agent.error.javascript_exception"));
            anyhow::bail!("{description}");
        }
        Ok(result["result"]["value"].clone())
    }
}

/// A page the tools drive: over DevTools for Chromium, or by calling into the WebKit
/// view in-process.
enum PageSession {
    Cdp(CdpSession),
    WebKit(WebKitSession),
}

struct WebKitSession {
    view: Entity<WebPreviewView>,
    cx: AsyncApp,
}

impl WebKitSession {
    fn with_page<T>(&self, operation: impl FnOnce(&WebKitPage) -> T) -> Result<T> {
        self.cx
            .update(|cx| self.view.read(cx).webkit.as_ref().map(operation))
            .context(t!("web_preview.agent.error.no_page_open"))
    }

    async fn wait<T>(&self, pending: crate::webkit_browser::PendingResult<T>) -> Result<T> {
        pending
            .await
            .map_err(|_| anyhow!(t!("web_preview.agent.error.no_page_open")))?
    }

    async fn evaluate(&self, expression: &str) -> Result<Value> {
        let pending = self.with_page(|page| page.evaluate_expression(expression))?;
        let json = match self.wait(pending).await {
            Ok(json) => json,
            // Statements, or several expressions, don't compile as one expression.
            Err(error) if error.to_string().contains("SyntaxError") => {
                let pending = self.with_page(|page| page.evaluate_script(expression))?;
                self.wait(pending).await?
            }
            Err(error) => return Err(error),
        };
        serde_json::from_str(&json).context(t!("web_preview.agent.error.javascript_exception"))
    }
}

impl PageSession {
    async fn connect(view: &Entity<WebPreviewView>, info: &PageInfo, cx: &AsyncApp) -> Result<Self> {
        if cx.update(|cx| view.read(cx).uses_webkit) {
            return Ok(Self::WebKit(WebKitSession {
                view: view.clone(),
                cx: cx.clone(),
            }));
        }
        with_timeout(
            CdpSession::connect(info.target_id.as_deref(), &info.url),
            &t!("web_preview.agent.action.connect"),
            cx,
        )
        .await
        .map(Self::Cdp)
    }

    async fn evaluate(&mut self, expression: &str) -> Result<Value> {
        match self {
            Self::Cdp(session) => session.evaluate(expression).await,
            Self::WebKit(session) => session.evaluate(expression).await,
        }
    }

    /// Maps screenshot pixels back to CSS pixels: screenshots are taken in CSS pixels, scaled
    /// down so the longest side fits the model's image budget.
    async fn viewport(&mut self) -> Result<(f64, f64, f64)> {
        let size = self
            .evaluate("[window.innerWidth, window.innerHeight]")
            .await?;
        let width = size[0].as_f64().unwrap_or(1280.0).max(1.0);
        let height = size[1].as_f64().unwrap_or(800.0).max(1.0);
        let scale = task_agents::screenshot_scale(width as u32, height as u32);
        Ok((width, height, scale))
    }

    /// A PNG of the visible page. Chromium draws it at the requested size; WebKit at the
    /// display's density, so the caller resizes it.
    async fn screenshot(&mut self, width: f64, height: f64, scale: f64) -> Result<Vec<u8>> {
        match self {
            Self::Cdp(session) => {
                let result = session
                    .call(
                        "Page.captureScreenshot",
                        json!({
                            "format": "png",
                            "clip": { "x": 0, "y": 0, "width": width, "height": height, "scale": scale },
                        }),
                    )
                    .await?;
                let data = result["data"]
                    .as_str()
                    .context(t!("web_preview.agent.error.no_image"))?;
                Ok(base64::engine::general_purpose::STANDARD.decode(data)?)
            }
            Self::WebKit(session) => {
                let pending = session.with_page(|page| page.snapshot())?;
                session.wait(pending).await
            }
        }
    }

    async fn click(&mut self, x: f64, y: f64, click_count: u32) -> Result<()> {
        match self {
            Self::Cdp(session) => {
                for event_type in ["mouseMoved", "mousePressed", "mouseReleased"] {
                    session
                        .call(
                            "Input.dispatchMouseEvent",
                            json!({
                                "type": event_type,
                                "x": x,
                                "y": y,
                                "button": if event_type == "mouseMoved" { "none" } else { "left" },
                                "clickCount": if event_type == "mouseMoved" { 0 } else { click_count },
                            }),
                        )
                        .await?;
                }
                Ok(())
            }
            Self::WebKit(session) => session.with_page(|page| page.click(x, y, click_count))?,
        }
    }

    async fn insert_text(&mut self, text: &str) -> Result<()> {
        match self {
            Self::Cdp(session) => {
                session
                    .call("Input.insertText", json!({ "text": text }))
                    .await?;
                Ok(())
            }
            Self::WebKit(session) => {
                let pending = session.with_page(|page| page.insert_text(text))?;
                let outcome = session.wait(pending).await?;
                anyhow::ensure!(
                    outcome != "no-focus",
                    t!("web_preview.agent.error.no_focused_element")
                );
                Ok(())
            }
        }
    }

    async fn press_key(&mut self, name: &str, code: &str, key_code: i64) -> Result<()> {
        match self {
            Self::Cdp(session) => {
                for event_type in ["rawKeyDown", "keyUp"] {
                    let mut params = json!({
                        "type": event_type,
                        "key": name,
                        "code": code,
                        "windowsVirtualKeyCode": key_code,
                        "nativeVirtualKeyCode": key_code,
                    });
                    if name == "Enter" && event_type == "rawKeyDown" {
                        params["type"] = json!("keyDown");
                        params["text"] = json!("\r");
                    }
                    session.call("Input.dispatchKeyEvent", params).await?;
                }
                Ok(())
            }
            Self::WebKit(session) => session.with_page(|page| page.press_key(name))?,
        }
    }

    async fn scroll(&mut self, x: f64, y: f64, delta_y: f64) -> Result<()> {
        match self {
            Self::Cdp(session) => {
                session
                    .call(
                        "Input.dispatchMouseEvent",
                        json!({ "type": "mouseWheel", "x": x, "y": y, "deltaX": 0, "deltaY": delta_y }),
                    )
                    .await?;
                Ok(())
            }
            Self::WebKit(session) => {
                let pending = session.with_page(|page| page.scroll(x, y, delta_y))?;
                session.wait(pending).await?;
                Ok(())
            }
        }
    }

    async fn navigate(&mut self, url: &str) -> Result<()> {
        match self {
            Self::Cdp(session) => {
                let navigation = session
                    .call("Page.navigate", json!({ "url": url }))
                    .await?;
                if let Some(error) = navigation["errorText"]
                    .as_str()
                    .filter(|error| !error.is_empty())
                {
                    anyhow::bail!(t!(
                        "web_preview.agent.error.navigation_failed",
                        url = url,
                        error = error
                    ));
                }
                Ok(())
            }
            Self::WebKit(session) => {
                let url = url.to_string();
                session.view.update(&mut session.cx, |view, cx| view.navigate(url, cx));
                Ok(())
            }
        }
    }

    /// Console messages and browser log entries buffered since the page loaded.
    async fn console_lines(&mut self, cx: &AsyncApp) -> Result<Vec<String>> {
        match self {
            Self::Cdp(session) => {
                with_timeout(
                    session.call("Console.enable", json!({})),
                    &t!("web_preview.agent.action.console"),
                    cx,
                )
                .await?;
                with_timeout(
                    session.call("Log.enable", json!({})),
                    &t!("web_preview.agent.action.log"),
                    cx,
                )
                .await?;
                session.collect_events(CONSOLE_COLLECT_TIME, cx).await;
                Ok(session
                    .events
                    .iter()
                    .filter_map(|event| match event["method"].as_str()? {
                        "Console.messageAdded" => {
                            let message = &event["params"]["message"];
                            Some(format!(
                                "[{}] {}",
                                message["level"].as_str().unwrap_or("log"),
                                message["text"].as_str().unwrap_or_default()
                            ))
                        }
                        "Log.entryAdded" => {
                            let entry = &event["params"]["entry"];
                            Some(format!(
                                "[{} · {}] {}{}",
                                entry["level"].as_str().unwrap_or("info"),
                                entry["source"].as_str().unwrap_or("browser"),
                                entry["text"].as_str().unwrap_or_default(),
                                entry["url"]
                                    .as_str()
                                    .map(|url| format!(" ({url})"))
                                    .unwrap_or_default()
                            ))
                        }
                        _ => None,
                    })
                    .collect())
            }
            Self::WebKit(session) => {
                let pending = session.with_page(|page| page.console_entries())?;
                let json = with_timeout(session.wait(pending), &t!("web_preview.agent.action.console"), cx)
                    .await?;
                let entries: Vec<String> = serde_json::from_str(&json)?;
                Ok(entries)
            }
        }
    }
}

async fn http_get_json(port: u16, path: &str) -> Result<Value> {
    use smol::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut stream = smol::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .context(t!("web_preview.agent.error.browser_not_running"))?;
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await?;
    // Chromium's DevTools server keeps the connection open despite `Connection: close`, so
    // read the headers, then exactly `Content-Length` bytes of body.
    let mut response = Vec::new();
    let mut buffer = [0u8; 8192];
    let (header_end, content_length) = loop {
        let read = stream.read(&mut buffer).await?;
        anyhow::ensure!(
            read > 0,
            t!("web_preview.agent.error.cdp_closed_without_response")
        );
        response.extend_from_slice(&buffer[..read]);
        if let Some(position) = response.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&response[..position]).to_lowercase();
            let content_length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .context(t!("web_preview.agent.error.cdp_no_content_length"))?;
            break (position + 4, content_length);
        }
    };
    while response.len() < header_end + content_length {
        let read = stream.read(&mut buffer).await?;
        anyhow::ensure!(
            read > 0,
            t!("web_preview.agent.error.cdp_closed_mid_response")
        );
        response.extend_from_slice(&buffer[..read]);
    }
    let body = String::from_utf8_lossy(&response[header_end..header_end + content_length]);
    serde_json::from_str(body.trim()).context(t!("web_preview.agent.error.cdp_not_json"))
}

/// Which pages a tool can work with. Painting and input wait on the renderer, which
/// stops producing frames for a tab the Devices panel has hidden, so those tools would
/// only time out there.
#[derive(Clone, Copy, PartialEq)]
enum PageUse {
    Script,
    PaintOrInput,
}

async fn session_for_active_page(
    page_use: PageUse,
    activity: SharedString,
    cx: &mut AsyncApp,
) -> Result<(PageSession, PageInfo)> {
    let view = cx
        .update(|cx| active_view(cx))
        .context(t!("web_preview.agent.error.no_browser_tab"))?;
    let blank = cx.update(|cx| {
        let view = view.read(cx);
        !view.has_page() && crate::is_blank_url(&view.url)
    });
    if blank {
        anyhow::bail!(t!("web_preview.agent.error.no_page_open"));
    }
    view.update(cx, |view, cx| view.note_agent_activity(activity, cx));
    wait_for_browser(&view, cx).await?;
    let info = cx.update(|cx| page_info(&view, cx));
    if page_use == PageUse::PaintOrInput && info.hidden {
        anyhow::bail!(t!("web_preview.agent.error.tab_hidden"));
    }
    let session = PageSession::connect(&view, &info, cx).await?;
    Ok((session, info))
}

/// Chromium is only started once the tab lays out, so a tab that never reached the screen
/// (opened in the background, or behind another tab) is brought forward instead of
/// leaving every tool to fail on it.
async fn wait_for_browser(view: &Entity<WebPreviewView>, cx: &mut AsyncApp) -> Result<()> {
    let started_at = std::time::Instant::now();
    let mut browser_started_at = None;
    let mut activated = false;
    loop {
        let (has_browser, has_target, failure, needs_screen) = cx.update(|cx| {
            let view = view.read(cx);
            let failure = if let Some(error) = &view.error {
                Some(t!(
                    "web_preview.agent.error.browser_failed",
                    headline = error.headline,
                    detail = error.detail
                ))
            } else if !view.uses_webkit && !crate::cef_paths::is_installed() {
                Some(t!("web_preview.agent.error.chromium_not_downloaded"))
            } else {
                None
            };
            (
                view.has_page(),
                view.uses_webkit || view.target_id.is_some(),
                failure,
                view.last_bounds.is_none(),
            )
        });
        if let Some(failure) = failure {
            anyhow::bail!(failure);
        }
        if has_browser {
            let browser_started_at =
                *browser_started_at.get_or_insert_with(std::time::Instant::now);
            if has_target || browser_started_at.elapsed() > TARGET_ID_GRACE {
                return Ok(());
            }
        } else if needs_screen && !activated {
            activated = true;
            show_view(view, cx);
        }
        if started_at.elapsed() > BROWSER_START_TIMEOUT {
            anyhow::bail!(if needs_screen {
                t!("web_preview.agent.error.tab_not_on_screen")
            } else if cx.update(|cx| view.read(cx).uses_webkit) {
                t!("web_preview.agent.error.webkit_start_timeout")
            } else {
                t!("web_preview.agent.error.chromium_start_timeout")
            });
        }
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
    }
}

fn show_view(view: &Entity<WebPreviewView>, cx: &mut AsyncApp) {
    let workspaces = cx.update(|cx| {
        workspace::AppState::global(cx)
            .workspace_store
            .read(cx)
            .workspaces_with_windows()
            .filter_map(|(window, workspace)| Some((window, workspace.upgrade()?)))
            .collect::<Vec<_>>()
    });
    for (window, workspace) in workspaces {
        let shown = window
            .update(cx, |_, window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.activate_item(view, true, false, window, cx)
                })
            })
            .unwrap_or(false);
        if shown {
            return;
        }
    }
}

fn page_line(info: &PageInfo) -> String {
    let mut line = format!("{} — {}", info.title, info.url);
    if info.loading {
        line.push_str(&t!("web_preview.agent.page_loading_suffix"));
    }
    if info.hidden {
        line.push_str(&t!("web_preview.agent.page_hidden_suffix"));
    }
    line
}

/// Waits for the page itself rather than for the tab's loading flag: right after a
/// navigation is requested that flag still describes the previous page, and a brand new
/// tab reports the blank page it starts on as loaded before Chromium even exists.
async fn wait_until_loaded(navigating_to: &str, cx: &mut AsyncApp) -> Result<PageInfo> {
    let view = cx
        .update(|cx| active_view(cx))
        .context(t!("web_preview.agent.error.tab_closed_before_load"))?;
    wait_for_browser(&view, cx).await?;
    let started_at = std::time::Instant::now();
    let mut session = None;
    loop {
        let info = cx.update(|cx| page_info(&view, cx));
        if info.load_error.is_some() {
            anyhow::bail!(t!(
                "web_preview.agent.error.page_failed_to_load",
                url = navigating_to
            ));
        }
        if session.is_none() {
            session = PageSession::connect(&view, &info, cx).await.log_err();
        }
        // A navigation replaces the page's JavaScript context, so an evaluation that
        // fails here is simply retried on the next round.
        let page = match session.as_mut() {
            Some(session) => with_timeout(
                session.evaluate("[document.readyState, location.href, document.title]"),
                &t!("web_preview.agent.action.read_page_state"),
                cx,
            )
            .await
            .ok(),
            None => None,
        };
        if let Some(page) = page {
            let ready_state = page[0].as_str().unwrap_or_default();
            let url = page[1].as_str().unwrap_or_default().to_string();
            let title = page[2].as_str().unwrap_or_default().to_string();
            if url.starts_with("chrome-error://") {
                anyhow::bail!(t!(
                    "web_preview.agent.error.page_failed_to_load",
                    url = navigating_to
                ));
            }
            let left_blank_page = url != "about:blank" || navigating_to == "about:blank";
            // WebKit reports a requested navigation as loading right away, while the
            // previous document still answers `complete`.
            if ready_state == "complete" && left_blank_page && !info.loading {
                return Ok(PageInfo {
                    url,
                    title,
                    loading: false,
                    ..info
                });
            }
        }
        if started_at.elapsed() > LOAD_TIMEOUT {
            return Ok(PageInfo {
                loading: true,
                ..info
            });
        }
        cx.background_executor().timer(READY_POLL_INTERVAL).await;
    }
}

#[derive(Deserialize, JsonSchema)]
struct NoInput {}

#[derive(Deserialize, JsonSchema)]
struct OpenInput {
    /// The address to load, e.g. `http://localhost:3000/login`.
    url: String,
}

#[derive(Deserialize, JsonSchema)]
struct ClickInput {
    /// Horizontal position in the pixels of the latest `browser_screenshot`.
    x: f64,
    /// Vertical position in the pixels of the latest `browser_screenshot`.
    y: f64,
    /// 2 for a double click. Defaults to 1.
    #[serde(default)]
    click_count: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
struct TypeInput {
    /// Text inserted into the focused element. Click the field first.
    text: String,
}

#[derive(Deserialize, JsonSchema)]
struct KeyInput {
    /// One of: enter, tab, backspace, escape, up, down, left, right.
    key: String,
}

#[derive(Deserialize, JsonSchema)]
struct ScrollInput {
    /// Pixels to scroll down; negative scrolls up.
    delta_y: f64,
    /// Where to scroll, in screenshot pixels. Defaults to the middle of the page.
    #[serde(default)]
    x: Option<f64>,
    #[serde(default)]
    y: Option<f64>,
}

#[derive(Deserialize, JsonSchema)]
struct EvalInput {
    /// A JavaScript expression evaluated in the page. Promises are awaited and the result is
    /// returned as JSON, e.g. `document.title` or `localStorage.getItem('token')`.
    expression: String,
}

fn key_event(key: &str) -> Result<(&'static str, &'static str, i64)> {
    Ok(match key {
        "enter" | "return" => ("Enter", "Enter", 13),
        "tab" => ("Tab", "Tab", 9),
        "backspace" => ("Backspace", "Backspace", 8),
        "escape" | "esc" => ("Escape", "Escape", 27),
        "left" => ("ArrowLeft", "ArrowLeft", 37),
        "up" => ("ArrowUp", "ArrowUp", 38),
        "right" => ("ArrowRight", "ArrowRight", 39),
        "down" => ("ArrowDown", "ArrowDown", 40),
        other => anyhow::bail!(t!("web_preview.agent.error.unknown_key", key = other)),
    })
}

fn tool<I>(
    name: &'static str,
    title: SharedString,
    description: &'static str,
    access: ToolAccess,
    run: impl AsyncFn(Entity<Project>, I, &mut AsyncApp) -> Result<ToolkitOutput>
    + Clone
    + Send
    + Sync
    + 'static,
) -> ToolkitTool
where
    I: JsonSchema + serde::de::DeserializeOwned + 'static,
{
    ToolkitTool::new(
        name,
        title,
        description,
        access,
        move |project: Entity<Project>, input: I, cx: &mut App| {
            let run = run.clone();
            cx.spawn(async move |cx| run(project, input, cx).await)
        },
    )
}

pub fn register_toolkit(cx: &mut App) {
    let tools = vec![
        tool(
            "browser_open",
            t!("web_preview.agent.tool.open"),
            "Loads a URL in the embedded browser tab (opening one if none is open) and waits for \
             the page to finish loading.",
            ToolAccess::Act,
            async |project, input: OpenInput, cx| {
                // A new tab shows its start page and only creates Chromium once it has an
                // address, so there is no page to drive over CDP yet.
                let blank_view = cx.update(|cx| {
                    active_view(cx).filter(|view| {
                        let view = view.read(cx);
                        !view.has_page() && crate::is_blank_url(&view.url)
                    })
                });
                let has_view = cx.update(|cx| active_view(cx).is_some());
                if let Some(view) = blank_view {
                    view.update(cx, |view, cx| {
                        view.note_agent_activity(t!("web_preview.agent.action.navigate"), cx);
                        view.navigate(input.url.clone(), cx);
                    });
                    show_view(&view, cx);
                } else if has_view {
                    let (mut session, _) = session_for_active_page(
                        PageUse::Script,
                        t!("web_preview.agent.action.navigate"),
                        cx,
                    )
                    .await?;
                    with_timeout(
                        session.navigate(&input.url),
                        &t!("web_preview.agent.action.navigate"),
                        cx,
                    )
                    .await?;
                } else {
                    let target = cx.update(|cx| {
                        workspace::AppState::global(cx)
                            .workspace_store
                            .read(cx)
                            .workspaces_with_windows()
                            .find_map(|(window, workspace)| {
                                let workspace = workspace.upgrade()?;
                                (workspace.read(cx).project() == &project)
                                    .then_some((window, workspace))
                            })
                    });
                    let (window, workspace) =
                        target.context(t!("web_preview.agent.error.no_project_window"))?;
                    let url = input.url.clone();
                    window.update(cx, |_, window, cx| {
                        workspace.update(cx, |workspace, cx| {
                            let view = cx.new(|cx| WebPreviewView::new_with_url(url, window, cx));
                            WebPreviewView::add_to_workspace(view, workspace, window, cx);
                        })
                    })?;
                }
                let info = wait_until_loaded(&input.url, cx).await?;
                Ok(ToolkitOutput::text(
                    t!("web_preview.agent.output.page", page = page_line(&info)).to_string(),
                ))
            },
        ),
        tool(
            "browser_screenshot",
            t!("web_preview.agent.tool.screenshot"),
            "Captures the visible part of the page in the embedded browser. Coordinates for \
             browser_click and browser_scroll are in the pixels of this image.",
            ToolAccess::Read,
            async |_project, _input: NoInput, cx| {
                let (mut session, info) = session_for_active_page(
                    PageUse::PaintOrInput,
                    t!("web_preview.agent.action.capture_page"),
                    cx,
                )
                .await?;
                let (width, height, scale) = with_timeout(
                    session.viewport(),
                    &t!("web_preview.agent.action.measure_page"),
                    cx,
                )
                .await?;
                let png = with_timeout(
                    session.screenshot(width, height, scale),
                    &t!("web_preview.agent.action.capture_page"),
                    cx,
                )
                .await?;
                let screenshot = cx
                    .background_spawn(async move {
                        let image =
                            image::load_from_memory_with_format(&png, image::ImageFormat::Png)?;
                        // Click coordinates are read as CSS pixels times `scale`, so the image
                        // has to be exactly that size whatever density it was drawn at.
                        let target_width = (width * scale).round().max(1.0) as u32;
                        let target_height = (height * scale).round().max(1.0) as u32;
                        let image = if image.width() == target_width
                            && image.height() == target_height
                        {
                            image
                        } else {
                            image.resize_exact(
                                target_width,
                                target_height,
                                image::imageops::FilterType::Triangle,
                            )
                        };
                        scale_screenshot(image)
                    })
                    .await?;
                let mut output = ToolkitOutput::text(
                    t!(
                        "web_preview.agent.output.screenshot",
                        page = page_line(&info),
                        width = screenshot.width,
                        height = screenshot.height,
                        css_width = format!("{width:.0}"),
                        css_height = format!("{height:.0}")
                    )
                    .to_string(),
                );
                output.push_png(&screenshot.png);
                Ok(output)
            },
        ),
        tool(
            "browser_click",
            t!("web_preview.agent.tool.click"),
            "Clicks the page at a point given in the pixels of the latest browser_screenshot.",
            ToolAccess::Act,
            async |_project, input: ClickInput, cx| {
                let (mut session, _) = session_for_active_page(
                    PageUse::PaintOrInput,
                    t!("web_preview.agent.action.click"),
                    cx,
                )
                .await?;
                let (_, _, scale) = with_timeout(
                    session.viewport(),
                    &t!("web_preview.agent.action.measure_page"),
                    cx,
                )
                .await?;
                let x = input.x / scale;
                let y = input.y / scale;
                let click_count = input.click_count.unwrap_or(1).clamp(1, 3);
                with_timeout(
                    session.click(x, y, click_count),
                    &t!("web_preview.agent.action.click"),
                    cx,
                )
                .await?;
                Ok(ToolkitOutput::text(
                    t!(
                        "web_preview.agent.output.clicked",
                        x = format!("{:.0}", input.x),
                        y = format!("{:.0}", input.y)
                    )
                    .to_string(),
                ))
            },
        ),
        tool(
            "browser_type",
            t!("web_preview.agent.tool.type"),
            "Types text into the focused element of the page.",
            ToolAccess::Act,
            async |_project, input: TypeInput, cx| {
                let (mut session, _) = session_for_active_page(
                    PageUse::PaintOrInput,
                    t!("web_preview.agent.action.type"),
                    cx,
                )
                .await?;
                with_timeout(
                    session.insert_text(&input.text),
                    &t!("web_preview.agent.action.type"),
                    cx,
                )
                .await?;
                Ok(ToolkitOutput::text(
                    t!(
                        "web_preview.agent.output.typed",
                        count = input.text.chars().count()
                    )
                    .to_string(),
                ))
            },
        ),
        tool(
            "browser_key",
            t!("web_preview.agent.tool.key"),
            "Presses a key in the page: enter, tab, backspace, escape, up, down, left, right.",
            ToolAccess::Act,
            async |_project, input: KeyInput, cx| {
                let key = input.key.trim().to_lowercase();
                let (name, code, key_code) = key_event(&key)?;
                let (mut session, _) = session_for_active_page(
                    PageUse::PaintOrInput,
                    t!("web_preview.agent.action.key"),
                    cx,
                )
                .await?;
                with_timeout(
                    session.press_key(name, code, key_code),
                    &t!("web_preview.agent.action.key"),
                    cx,
                )
                .await?;
                Ok(ToolkitOutput::text(
                    t!("web_preview.agent.output.key_pressed", key = key).to_string(),
                ))
            },
        ),
        tool(
            "browser_scroll",
            t!("web_preview.agent.tool.scroll"),
            "Scrolls the page with the mouse wheel.",
            ToolAccess::Act,
            async |_project, input: ScrollInput, cx| {
                let (mut session, _) = session_for_active_page(
                    PageUse::PaintOrInput,
                    t!("web_preview.agent.action.scroll"),
                    cx,
                )
                .await?;
                let (width, height, scale) = with_timeout(
                    session.viewport(),
                    &t!("web_preview.agent.action.measure_page"),
                    cx,
                )
                .await?;
                let x = input.x.map_or(width / 2.0, |x| x / scale);
                let y = input.y.map_or(height / 2.0, |y| y / scale);
                with_timeout(
                    session.scroll(x, y, input.delta_y),
                    &t!("web_preview.agent.action.scroll"),
                    cx,
                )
                .await?;
                Ok(ToolkitOutput::text(
                    t!(
                        "web_preview.agent.output.scrolled",
                        pixels = format!("{:.0}", input.delta_y)
                    )
                    .to_string(),
                ))
            },
        ),
        tool(
            "browser_eval",
            t!("web_preview.agent.tool.eval"),
            "Evaluates a JavaScript expression in the page and returns the result as JSON. Use it \
             to read the DOM, storage or app state that a screenshot can't show. Promises are \
             awaited for at most 60 seconds; to navigate, use browser_open instead of \
             assigning `location`.",
            ToolAccess::Act,
            async |_project, input: EvalInput, cx| {
                let (mut session, _) = session_for_active_page(
                    PageUse::Script,
                    t!("web_preview.agent.action.evaluate"),
                    cx,
                )
                .await?;
                let value = with_timeout_of(
                    EVAL_TIMEOUT,
                    session.evaluate(&input.expression),
                    &t!("web_preview.agent.action.evaluate"),
                    cx,
                )
                .await?;
                let text = serde_json::to_string_pretty(&value)?;
                let text: String = text.chars().take(20_000).collect();
                Ok(ToolkitOutput::text(text))
            },
        ),
        tool(
            "browser_console",
            t!("web_preview.agent.tool.console"),
            "Returns the page's console messages and browser log entries (errors, failed \
             requests) buffered since the page loaded.",
            ToolAccess::Read,
            async |_project, _input: NoInput, cx| {
                let (mut session, info) = session_for_active_page(
                    PageUse::Script,
                    t!("web_preview.agent.action.console"),
                    cx,
                )
                .await?;
                let lines = session.console_lines(cx).await?;
                let body = if lines.is_empty() {
                    t!("web_preview.agent.output.console_empty").to_string()
                } else {
                    lines.join("\n")
                };
                Ok(ToolkitOutput::text(format!("{}\n{body}", page_line(&info))))
            },
        ),
    ];

    task_agents::register_toolkit(
        Toolkit {
            id: task_agents::toolkit_ids::BROWSER.into(),
            name: "Browser".into(),
            description: t!("web_preview.agent.toolkit_description"),
            icon: "public".into(),
            mention: None,
            tools,
        },
        cx,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Talks to a real Chromium on the CDP port: start one with
    /// `"Google Chrome" --headless=new --remote-debugging-port=9223 https://example.com`.
    #[test]
    #[ignore]
    fn test_cdp_session_against_chromium() {
        let result: Result<()> = smol::block_on(async {
            let mut session = CdpSession::connect(None, "https://example.com/").await?;
            let size = session
                .evaluate("[window.innerWidth, window.innerHeight]")
                .await?;
            let width = size[0].as_f64().unwrap_or_default();
            let height = size[1].as_f64().unwrap_or_default();
            anyhow::ensure!(width > 0.0 && height > 0.0);
            let scale = task_agents::screenshot_scale(width as u32, height as u32);
            let shot = session
                .call(
                    "Page.captureScreenshot",
                    json!({ "format": "png", "clip": { "x": 0, "y": 0, "width": width, "height": height, "scale": scale } }),
                )
                .await?;
            let png = base64::engine::general_purpose::STANDARD
                .decode(shot["data"].as_str().unwrap_or_default())?;
            anyhow::ensure!(png.starts_with(b"\x89PNG"), "not a PNG");
            let title = session.evaluate("document.title").await?;
            anyhow::ensure!(
                title
                    .as_str()
                    .is_some_and(|title| title.contains("Example")),
                "unexpected title {title}"
            );
            for event_type in ["mouseMoved", "mousePressed", "mouseReleased"] {
                session
                    .call(
                        "Input.dispatchMouseEvent",
                        json!({ "type": event_type, "x": 10, "y": 10, "button": "left", "clickCount": 1 }),
                    )
                    .await?;
            }
            session
                .call("Input.insertText", json!({ "text": "oi" }))
                .await?;
            session.evaluate("console.error('agent-test')").await?;
            // Enabling the console replays buffered messages before its own response.
            session.call("Console.enable", json!({})).await?;
            anyhow::ensure!(
                session
                    .events
                    .iter()
                    .any(|event| event.to_string().contains("agent-test")),
                "console message was not replayed"
            );
            Ok(())
        });
        result.expect("CDP session against Chromium");
    }
}
