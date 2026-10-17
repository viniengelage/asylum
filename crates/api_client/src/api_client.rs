//! An API client whose collection comes from the project's OpenAPI or Swagger file: folders
//! are tags, requests are operations, environments are `servers` and logging in follows the
//! spec's security schemes. What the spec doesn't say lives in `.asylum/api/<id>.json`.

mod agent_toolkit;
mod collection;
mod config;
mod cookies;
mod example;
mod jsonpath;
mod lens;
mod panel;
mod request_view;
mod schema;
mod send;
mod spec;
mod vars;

pub use agent_toolkit::register_toolkit as register_agent_toolkit;
pub use panel::ApiPanel;
pub use request_view::ApiRequestView;

use gpui::{Subscription, WeakEntity, actions};
use std::any::TypeId;
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{HideStatusItem, ItemHandle, StatusItemView, Workspace, dock::StatusBarButton};

actions!(
    api_client,
    [
        /// Opens the API panel, or hands focus back if it already has it.
        ToggleFocus,
        /// Sends the request in the focused API tab.
        SendRequest,
    ]
);

pub fn init(cx: &mut App) {
    workspace::register_panel_item::<ApiPanel>(cx);
    lens::init(cx);
    cx.observe_new(|workspace: &mut Workspace, window, cx| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            toggle_focus(workspace, window, cx);
        });
        // `panel`, `settings` and `link` open the dock on that screen; `op:<METHOD path>[:send]`
        // also opens that operation's tab and sends it, `tab:headers` shows its headers and
        // `wait:<ms>` waits. Steps can be chained with `|`.
        if let (Ok(step), Some(window)) = (std::env::var("API_CLIENT_DEBUG_OPEN"), window) {
            cx.spawn_in(window, async move |workspace, cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(3))
                    .await;
                let panel = workspace.update_in(cx, |workspace, window, cx| {
                    open(workspace, window, cx);
                    workspace.panel::<ApiPanel>(cx)
                })?;
                let Some(panel) = panel else {
                    return anyhow::Ok(());
                };
                for _ in 0..40 {
                    if panel.read_with(cx, |panel, cx| panel.is_loaded(cx)) {
                        break;
                    }
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(250))
                        .await;
                }
                for step in step.split('|') {
                    if let Some(milliseconds) =
                        step.strip_prefix("wait:").and_then(|value| value.parse().ok())
                    {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(milliseconds))
                            .await;
                        continue;
                    }
                    if step == "tab:headers" {
                        workspace.update(cx, |workspace, cx| {
                            if let Some(view) = workspace.active_item_as::<ApiRequestView>(cx) {
                                view.update(cx, |view, cx| view.show_headers_tab(cx));
                            }
                        })?;
                        continue;
                    }
                    panel.update_in(cx, |panel, window, cx| match step {
                        "settings" => panel.open_settings(window, cx),
                        "link" => panel.show_link(cx),
                        _ => {
                            if let Some(target) = step.strip_prefix("op:") {
                                let (key, send) = match target.strip_suffix(":send") {
                                    Some(key) => (key, true),
                                    None => (target, false),
                                };
                                panel.debug_open_operation(key.to_string(), send, window, cx);
                            }
                        }
                    })?;
                }
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
        }
    })
    .detach();
}

/// Adds the panel the first time it is asked for, so projects without an API don't get a tab.
pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if workspace.panel::<ApiPanel>(cx).is_none() {
        let panel = cx.new(|cx| ApiPanel::new(workspace, window, cx));
        workspace.add_panel(panel, window, cx);
    }
    workspace.focus_panel::<ApiPanel>(window, cx);
}

fn toggle_focus(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if workspace.panel::<ApiPanel>(cx).is_none() {
        open(workspace, window, cx);
        return;
    }
    workspace.toggle_panel_focus::<ApiPanel>(window, cx);
}

fn panel_is_visible(workspace: &Workspace, cx: &App) -> bool {
    workspace.all_docks().iter().any(|dock| {
        dock.read(cx)
            .visible_panel()
            .is_some_and(|panel| panel.panel_type_id() == TypeId::of::<ApiPanel>())
    })
}

/// The API button in the status bar's toolkit group, lit while the panel is open.
pub struct ApiToolkitButton {
    workspace: WeakEntity<Workspace>,
    _dock_subscriptions: Vec<Subscription>,
}

impl ApiToolkitButton {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let dock_subscriptions = workspace
            .all_docks()
            .into_iter()
            .map(|dock| cx.observe(dock, |_, _, cx| cx.notify()))
            .collect();
        Self {
            workspace: workspace.weak_handle(),
            _dock_subscriptions: dock_subscriptions,
        }
    }
}

impl Render for ApiToolkitButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_open = self
            .workspace
            .upgrade()
            .is_some_and(|workspace| panel_is_visible(workspace.read(cx), cx));
        let workspace = self.workspace.clone();

        StatusBarButton::new("toolkit-api", IconName::ArrowRightLeft, is_open)
            .tab_index(0isize)
            .aria_label("API")
            .tooltip(|_window, cx| Tooltip::for_action("API", &ToggleFocus, cx))
            .on_click(move |_, window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        if is_open {
                            workspace.close_panel::<ApiPanel>(window, cx);
                        } else {
                            open(workspace, window, cx);
                        }
                    })
                    .log_err();
            })
    }
}

impl StatusItemView for ApiToolkitButton {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _cx: &App) -> Option<HideStatusItem> {
        // The panel has no status bar button of its own, so this is the only visible way in.
        None
    }
}

#[cfg(test)]
mod tests {
    use crate::{config, schema, spec};
    use std::path::Path;

    /// Reads a real spec: `API_CLIENT_SPEC=/path/to/openapi.yml cargo test -p api_client -- --ignored`.
    #[test]
    #[ignore]
    fn reads_a_real_spec() {
        let path = std::env::var("API_CLIENT_SPEC").expect("API_CLIENT_SPEC");
        let path = Path::new(&path);
        let text = std::fs::read_to_string(path).unwrap();
        let started = std::time::Instant::now();
        let spec = spec::load(path, &text, &|path| Ok(std::fs::read_to_string(path)?)).unwrap();
        let elapsed = started.elapsed();
        let root = schema::validation_root(&spec);
        let mut unchecked = 0;
        for operation in &spec.operations {
            if let Some(schema) = operation.success_response().and_then(|r| r.schema.as_ref()) {
                let example = schema::example(&spec.document, schema);
                if let schema::Validation::Unchecked { reason } =
                    schema::validate(&root, spec.format, schema, &example)
                {
                    unchecked += 1;
                    eprintln!("{}: {reason}", operation.key);
                }
            }
        }
        eprintln!(
            "{} {} ({:?}) · {} operações · {} pastas · {} esquemas · {} avisos · parse {:?} · {} sem validação",
            spec.title,
            spec.api_version,
            spec.format,
            spec.operations.len(),
            spec.folders().len(),
            spec.security_schemes.len(),
            spec.warnings.len(),
            elapsed,
            unchecked,
        );
        for (tag, operations) in spec.folders().iter().take(4) {
            eprintln!("  {tag} ({})", operations.len());
        }
        eprintln!("login: {:#?}", config::guess_login(&spec));
    }
}
