use std::sync::Arc;

use agent_settings::AgentSettings;
use collections::{HashMap, HashSet};
use context_server::ContextServerId;
use editor::Editor;
use extension_host::ExtensionStore;
use gpui::{
    Action as _, AnyElement, Entity, EntityId, Focusable as _, FontWeight, ReadGlobal as _,
    ScrollHandle, Stateful, Subscription, Task, prelude::*, px,
};
use project::context_server_store::{
    ContextServerConfiguration, ContextServerStatus, ContextServerStore,
};
use project::project_settings::ContextServerSettings;
use settings::{
    ContextServerCommand, ContextServerSettingsContent, OAuthClientSettings, Settings as _,
    SettingsScope, SettingsStore,
};
use ui::{
    ContextMenu, EmptyState, IconButton, PopoverMenu, PopoverTriggerArea, StatusTone, Tooltip,
    checkbox_indicator, grouped_block, prelude::*, primary_button, secondary_button,
    section_header, segment_option, segment_option_with_count, segmented_control, status_pill,
    text_field, toggle_switch,
};
use util::ResultExt as _;
use zed_actions::ExtensionCategoryFilter;

use crate::{SettingsUiFile, SettingsWindow};

fn t(text: &'static str) -> SharedString {
    i18n::translate_text("settings_ui_text", text)
}

fn t_count(text: &'static str, count: usize) -> SharedString {
    t(text).replace("{count}", &count.to_string()).into()
}

fn t_name(text: &'static str, name: &str) -> SharedString {
    t(text).replace("{name}", name).into()
}

/// State of the MCP page: what is selected, the filters and the open forms.
#[derive(Default)]
pub(crate) struct McpPage {
    selection: Option<Selection>,
    filter: Filter,
    search: Option<Entity<Editor>>,
    form: Option<McpServerForm>,
    paste: Option<McpPasteForm>,
    tools: HashMap<ContextServerId, Tools>,
    observed_store: Option<(EntityId, Subscription)>,
    _search_subscription: Option<Subscription>,
}

#[derive(Clone, PartialEq)]
enum Selection {
    Server(ContextServerId),
    New,
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Filter {
    #[default]
    All,
    Enabled,
    Problems,
}

enum Tools {
    Loading { _task: Task<()> },
    Loaded(Vec<SharedString>),
    Failed(SharedString),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ServerScope {
    Profile,
    Project,
}

impl ServerScope {
    fn settings_scope(self) -> SettingsScope {
        match self {
            ServerScope::Profile => SettingsScope::Profile,
            ServerScope::Project => SettingsScope::Project,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum McpTransport {
    /// Local server launched via stdin/stdout.
    Stdio,
    /// Remote server connected over HTTP.
    Http,
}

/// What the list and the detail know about one server.
struct ServerEntry {
    id: ContextServerId,
    name: SharedString,
    invocation: Option<SharedString>,
    status: ContextServerStatus,
    enabled: bool,
    scope: ServerScope,
    extension: bool,
    /// Listed in `agent.on_demand_context_servers`: the agent asks before loading its tools.
    on_demand: bool,
    settings: Option<ContextServerSettings>,
}

impl ServerEntry {
    fn has_problem(&self) -> bool {
        self.enabled
            && matches!(
                self.status,
                ContextServerStatus::Error(_)
                    | ContextServerStatus::AuthRequired
                    | ContextServerStatus::ClientSecretRequired { .. }
            )
    }

    fn matches(&self, query: &str) -> bool {
        query.is_empty()
            || self.name.to_lowercase().contains(query)
            || self
                .invocation
                .as_ref()
                .is_some_and(|invocation| invocation.to_lowercase().contains(query))
    }
}

pub(crate) fn render_mcp_servers_page(
    settings_window: &SettingsWindow,
    _scroll_handle: &ScrollHandle,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let store = settings_window
        .active_project(cx)
        .map(|project| project.read(cx).context_server_store());
    ensure_page_state(settings_window, store.as_ref(), window, cx);
    let page = &settings_window.mcp_page;

    let body = match store.as_ref() {
        None => EmptyState::new(IconName::Server, t("No project open"))
            .description(t("Open a project to manage its MCP servers."))
            .into_any_element(),
        Some(store) => {
            let entries = collect_entries(store, cx);
            if entries.is_empty() && page.form.is_none() {
                render_empty(cx)
            } else {
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .items_start()
                    .gap_5()
                    .child(render_list(page, &entries, cx))
                    .child(render_detail(page, &entries, store, window, cx))
                    .into_any_element()
            }
        }
    };

    v_flex()
        .id("mcp-servers-page")
        .relative()
        .size_full()
        .pt_2()
        .px_8()
        .pb_6()
        .gap(px(18.))
        .child(render_header(cx))
        .child(body)
        .when_some(page.paste.as_ref(), |this, paste| {
            this.child(render_paste_modal(paste, store.as_ref(), cx))
        })
        .into_any_element()
}

/// Creates the search box and subscribes to the server store once the page is visible; both
/// need a `&mut SettingsWindow`, which render does not have.
fn ensure_page_state(
    settings_window: &SettingsWindow,
    store: Option<&Entity<ContextServerStore>>,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let page = &settings_window.mcp_page;
    let store_changed =
        store.map(|store| store.entity_id()) != page.observed_store.as_ref().map(|(id, _)| *id);
    let stale_tools: Vec<ContextServerId> = store
        .map(|store| {
            let store = store.read(cx);
            page.tools
                .keys()
                .filter(|id| store.get_running_server(id).is_none())
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if page.search.is_some() && !store_changed && stale_tools.is_empty() {
        return;
    }
    let store = store.cloned();
    cx.defer_in(window, move |this, window, cx| {
        if this.mcp_page.search.is_none() {
            let search = new_input("Search by name or command", None, window, cx);
            this.mcp_page._search_subscription = Some(cx.observe(&search, |_, _, cx| cx.notify()));
            this.mcp_page.search = Some(search);
        }
        if store_changed {
            this.mcp_page.observed_store = store.map(|store| {
                (
                    store.entity_id(),
                    cx.observe(&store, |_, _, cx| cx.notify()),
                )
            });
        }
        for id in stale_tools {
            this.mcp_page.tools.remove(&id);
        }
        cx.notify();
    });
}

fn collect_entries(store: &Entity<ContextServerStore>, cx: &App) -> Vec<ServerEntry> {
    let settings_store = SettingsStore::global(cx);
    let project_servers = settings_store
        .asylum_project_settings()
        .map(|project| &project.project.context_servers);
    let on_demand_servers = &AgentSettings::get_global(cx).on_demand_context_servers;
    let store = store.read(cx);
    let mut entries: Vec<ServerEntry> = store
        .server_ids()
        .iter()
        .map(|id| {
            let extension = store.is_extension_provided(id, cx);
            let settings = store.settings_for_server(id).cloned();
            let defined_in_project = project_servers.is_some_and(|servers| {
                servers.get(&id.0).is_some_and(|server| {
                    !matches!(server, ContextServerSettingsContent::Extension { .. })
                })
            });
            let name = if extension {
                resolve_extension_display_name(id, cx)
                    .unwrap_or_else(|| SharedString::from(id.0.to_string()))
            } else {
                SharedString::from(id.0.to_string())
            };
            ServerEntry {
                id: id.clone(),
                name,
                invocation: settings.as_ref().and_then(server_invocation),
                status: store
                    .status_for_server(id)
                    .unwrap_or(ContextServerStatus::Stopped),
                enabled: store.is_server_enabled(id, cx),
                scope: if defined_in_project {
                    ServerScope::Project
                } else {
                    ServerScope::Profile
                },
                extension,
                on_demand: on_demand_servers
                    .iter()
                    .any(|server| server == id.0.as_ref()),
                settings,
            }
        })
        .collect();
    entries.sort_by_key(|entry| entry.name.to_lowercase());
    entries
}

fn render_header(cx: &mut Context<SettingsWindow>) -> impl IntoElement {
    h_flex()
        .w_full()
        .flex_none()
        .gap_3()
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    Label::new(t("MCP servers"))
                        .size(LabelSize::Large)
                        .weight(FontWeight::BOLD),
                )
                .child(
                    Label::new(t(
                        "Outside tools the agent can use. Profile servers work in every project; project servers only in that project.",
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                ),
        )
        .child(render_add_menu(cx))
}

fn render_add_menu(cx: &mut Context<SettingsWindow>) -> impl IntoElement {
    let settings_window = cx.entity().downgrade();
    PopoverMenu::new("mcp-add-menu")
        .trigger(PopoverTriggerArea::new(
            primary_button("mcp-add", Some(IconName::Plus), t("Add server"), None, cx).child(
                Icon::new(IconName::ChevronDown)
                    .size(IconSize::XSmall)
                    .color(Color::Custom(gpui::white())),
            ),
        ))
        .anchor(gpui::Anchor::TopRight)
        .menu(move |window, cx| {
            let settings_window = settings_window.clone();
            Some(ContextMenu::build(window, cx, move |menu, _window, _cx| {
                let item = |icon: IconName, title: &'static str, description: &'static str| {
                    move |_window: &mut Window, _cx: &mut App| {
                        h_flex()
                            .gap_2p5()
                            .py_0p5()
                            .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
                            .child(
                                v_flex()
                                    .child(Label::new(t(title)).size(LabelSize::Small))
                                    .child(
                                        Label::new(t(description))
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    ),
                            )
                            .into_any_element()
                    }
                };
                let open_form = |transport: McpTransport| {
                    let settings_window = settings_window.clone();
                    move |window: &mut Window, cx: &mut App| {
                        settings_window
                            .update(cx, |this, cx| {
                                open_new_form(this, transport, None, window, cx)
                            })
                            .log_err();
                    }
                };
                menu.custom_entry(
                    item(
                        IconName::Terminal,
                        "Local server",
                        "A command on this machine",
                    ),
                    open_form(McpTransport::Stdio),
                )
                .custom_entry(
                    item(
                        IconName::Public,
                        "Remote server",
                        "An HTTP URL, with a header or OAuth",
                    ),
                    open_form(McpTransport::Http),
                )
                .custom_entry(
                    item(
                        IconName::Json,
                        "Paste JSON…",
                        "From Claude, Cursor or VS Code",
                    ),
                    {
                        let settings_window = settings_window.clone();
                        move |window, cx| {
                            settings_window
                                .update(cx, |this, cx| open_paste_form(this, window, cx))
                                .log_err();
                        }
                    },
                )
                .separator()
                .custom_entry(
                    item(
                        IconName::Blocks,
                        "Extension catalog",
                        "Ready-made servers to install",
                    ),
                    |window, cx| open_extension_catalog(window, cx),
                )
            }))
        })
}

fn open_extension_catalog(window: &mut Window, cx: &mut App) {
    window.dispatch_action(
        zed_actions::Extensions {
            category_filter: Some(ExtensionCategoryFilter::ContextServers),
            id: None,
        }
        .boxed_clone(),
        cx,
    );
}

fn render_empty(cx: &mut Context<SettingsWindow>) -> AnyElement {
    let settings_window = cx.entity().downgrade();
    v_flex()
        .flex_1()
        .w_full()
        .rounded(px(12.))
        .bg(cx.theme().colors().element_background)
        .child(
            EmptyState::new(IconName::Server, t("No MCP servers"))
                .description(t(
                    "An MCP server gives the agent outside tools: Sentry, Figma, databases, tasks. Add one here or paste the JSON its docs show.",
                ))
                .action(
                    h_flex()
                        .gap_2()
                        .child(
                            primary_button("mcp-empty-add", None, t("Add server"), None, cx)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    open_new_form(this, McpTransport::Stdio, None, window, cx)
                                })),
                        )
                        .child(
                            secondary_button("mcp-empty-paste", None, t("Paste JSON"), true, cx)
                                .on_click(move |_, window, cx| {
                                    settings_window
                                        .update(cx, |this, cx| open_paste_form(this, window, cx))
                                        .log_err();
                                }),
                        )
                        .child(
                            Button::new("mcp-empty-catalog", t("Extension catalog"))
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .on_click(|_, window, cx| open_extension_catalog(window, cx)),
                        ),
                ),
        )
        .into_any_element()
}

// === List ===

fn render_list(
    page: &McpPage,
    entries: &[ServerEntry],
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let query = page
        .search
        .as_ref()
        .map(|search| search.read(cx).text(cx).trim().to_lowercase())
        .unwrap_or_default();
    let enabled_count = entries.iter().filter(|entry| entry.enabled).count();
    let problem_count = entries.iter().filter(|entry| entry.has_problem()).count();
    let visible = |entry: &&ServerEntry| {
        entry.matches(&query)
            && match page.filter {
                Filter::All => true,
                Filter::Enabled => entry.enabled,
                Filter::Problems => entry.has_problem(),
            }
    };
    let in_project = SettingsStore::global(cx)
        .asylum_project_settings_path()
        .is_some();

    let filter_option = |id: &'static str, label: &'static str, count: usize, filter: Filter| {
        segment_option_with_count(
            id,
            t(label),
            count.to_string(),
            page.filter == filter,
            true,
            cx,
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            this.mcp_page.filter = filter;
            cx.notify();
        }))
    };

    let mut list = v_flex()
        .id("mcp-list")
        .w(px(420.))
        .flex_none()
        .h_full()
        .gap_3()
        .overflow_y_scroll()
        .child(
            text_field(false, cx)
                .gap_2()
                .child(
                    Icon::new(IconName::MagnifyingGlass)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .children(page.search.clone()),
        )
        .child(
            segmented_control(cx)
                .child(filter_option(
                    "mcp-filter-all",
                    "All",
                    entries.len(),
                    Filter::All,
                ))
                .child(filter_option(
                    "mcp-filter-enabled",
                    "On",
                    enabled_count,
                    Filter::Enabled,
                ))
                .child(filter_option(
                    "mcp-filter-problems",
                    "With problems",
                    problem_count,
                    Filter::Problems,
                )),
        );

    if page.selection == Some(Selection::New) {
        let name = page
            .form
            .as_ref()
            .map(|form| form.name.read(cx).text(cx))
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| t("New server").to_string());
        list = list.child(
            h_flex()
                .w_full()
                .h(px(46.))
                .px(px(14.))
                .gap(px(10.))
                .rounded(px(10.))
                .bg(cx.theme().colors().element_selected)
                .child(Icon::new(IconName::Plus).size(IconSize::Small))
                .child(
                    v_flex()
                        .min_w_0()
                        .child(Label::new(name).size(LabelSize::Small).truncate())
                        .child(
                            Label::new(t("not saved"))
                                .size(LabelSize::XSmall)
                                .color(Color::Placeholder),
                        ),
                ),
        );
    }

    let groups: &[(ServerScope, &'static str)] = if in_project {
        &[
            (ServerScope::Profile, "FROM THE PROFILE"),
            (ServerScope::Project, "ONLY IN THIS PROJECT"),
        ]
    } else {
        &[(ServerScope::Profile, "FROM THE PROFILE")]
    };
    for (scope, label) in groups {
        let in_scope: Vec<&ServerEntry> = entries
            .iter()
            .filter(|entry| entry.scope == *scope)
            .collect();
        let shown: Vec<&ServerEntry> = in_scope.iter().copied().filter(visible).collect();
        if in_scope.is_empty() && *scope == ServerScope::Project {
            continue;
        }
        let enabled = in_scope.iter().filter(|entry| entry.enabled).count();
        list = list.child(section_header(
            t(label),
            Some(
                Label::new(
                    t("{enabled} of {total} on")
                        .replace("{enabled}", &enabled.to_string())
                        .replace("{total}", &in_scope.len().to_string()),
                )
                .size(LabelSize::XSmall)
                .color(Color::Placeholder)
                .into_any_element(),
            ),
        ));
        if shown.is_empty() {
            list = list.child(
                Label::new(t("Nothing matches the search or the filter."))
                    .size(LabelSize::Small)
                    .color(Color::Placeholder),
            );
            continue;
        }
        let mut rows = Vec::with_capacity(shown.len());
        for entry in shown {
            let selected = page.selection.as_ref() == Some(&Selection::Server(entry.id.clone()));
            rows.push(render_row(entry, selected, cx));
        }
        list = list.child(grouped_block(cx).children(rows));
    }
    list
}

fn render_row(entry: &ServerEntry, selected: bool, cx: &mut Context<SettingsWindow>) -> AnyElement {
    let colors = cx.theme().colors();
    let pill = status_pill_for(&entry.status, entry.enabled)
        .filter(|(_, tone)| *tone != StatusTone::Success)
        .or_else(|| {
            (entry.enabled && entry.on_demand).then(|| (t("on demand"), StatusTone::Neutral))
        });
    let dot_color = if entry.enabled {
        status_tone(&entry.status).color(cx)
    } else {
        colors.border_selected
    };
    let id = entry.id.clone();
    h_flex()
        .id(SharedString::from(format!("mcp-row-{}", entry.id.0)))
        .w_full()
        .h(px(46.))
        .px(px(10.))
        .gap(px(10.))
        .rounded(px(8.))
        .cursor_pointer()
        .when(selected, |this| this.bg(colors.element_selected))
        .when(!selected, |this| {
            this.hover(|this| this.bg(colors.element_hover))
        })
        .when(!entry.enabled, |this| this.opacity(0.55))
        .on_click(cx.listener(move |this, _, _, cx| {
            select_server(this, id.clone(), cx);
        }))
        .child(div().flex_none().size(px(8.)).rounded_full().bg(dot_color))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    Label::new(entry.name.clone())
                        .size(LabelSize::Small)
                        .weight(FontWeight::MEDIUM)
                        .truncate(),
                )
                .child(
                    Label::new(entry.invocation.clone().unwrap_or_else(|| t("extension")))
                        .size(LabelSize::XSmall)
                        .buffer_font(cx)
                        .color(Color::Placeholder)
                        .truncate(),
                ),
        )
        .children(pill.map(|(label, tone)| status_pill(label, tone, cx)))
        .child(render_toggle(entry, cx))
        .into_any_element()
}

fn select_server(this: &mut SettingsWindow, id: ContextServerId, cx: &mut Context<SettingsWindow>) {
    let page = &mut this.mcp_page;
    if page
        .form
        .as_ref()
        .is_some_and(|form| form.original_id.as_ref() != Some(&id))
    {
        page.form = None;
    }
    page.selection = Some(Selection::Server(id));
    cx.notify();
}

fn render_toggle(entry: &ServerEntry, cx: &mut Context<SettingsWindow>) -> impl IntoElement {
    let transitioning = matches!(
        entry.status,
        ContextServerStatus::Starting | ContextServerStatus::Authenticating
    );
    let id = entry.id.clone();
    let enabled = entry.enabled;
    toggle_switch(
        SharedString::from(format!("mcp-toggle-{}", entry.id.0)),
        enabled,
        !transitioning,
        cx,
    )
    .when(!transitioning, |this| {
        this.on_click(cx.listener(move |this, _, _, cx| {
            cx.stop_propagation();
            set_server_enabled(this, &id, !enabled, cx);
        }))
    })
}

fn set_server_enabled(
    settings_window: &SettingsWindow,
    id: &ContextServerId,
    enabled: bool,
    cx: &mut Context<SettingsWindow>,
) {
    if let Some(project) = settings_window.active_project(cx) {
        project
            .read(cx)
            .context_server_store()
            .update(cx, |store, cx| {
                if enabled {
                    if let Some(server) = store.get_server(id) {
                        store.start_server(server, cx);
                    }
                } else {
                    store.stop_server(id, cx).log_err();
                }
            });
    }

    let fs = <dyn fs::Fs>::global(cx);
    let name = id.0.clone();
    if SettingsStore::global(cx)
        .asylum_project_settings_path()
        .is_some()
    {
        let written = SettingsStore::global(cx).update_settings_file_in_scope(
            fs,
            SettingsScope::Project,
            move |settings, _| set_enabled_in_project(settings, name, enabled),
        );
        log_write_error(written, cx);
        return;
    }
    settings::update_settings_file(fs, cx, move |settings, _| {
        settings
            .project
            .context_servers
            .entry(name)
            .or_insert_with(|| ContextServerSettingsContent::Extension {
                enabled,
                remote: false,
                settings: serde_json::json!({}),
            })
            .set_enabled(enabled);
    });
}

fn log_write_error(
    written: futures::channel::oneshot::Receiver<anyhow::Result<()>>,
    cx: &mut Context<SettingsWindow>,
) {
    cx.background_spawn(async move {
        if let Ok(Err(error)) = written.await {
            log::error!("Failed to write the MCP settings: {error:#}");
        }
    })
    .detach();
}

/// In an Asylum project the toggle only changes the project: turning a profile server off writes
/// `{"enabled": false}` there, and turning it back on removes that entry.
fn set_enabled_in_project(
    settings: &mut settings::SettingsContent,
    name: Arc<str>,
    is_enabled: bool,
) {
    let servers = &mut settings.project.context_servers;
    let only_turns_off = matches!(
        servers.get(&name),
        Some(ContextServerSettingsContent::Extension { enabled: false, remote: false, settings })
            if settings.is_null() || settings.as_object().is_some_and(|object| object.is_empty())
    );
    if is_enabled && only_turns_off {
        servers.remove(&name);
        return;
    }
    servers
        .entry(name)
        .or_insert_with(|| ContextServerSettingsContent::Extension {
            enabled: is_enabled,
            remote: false,
            settings: serde_json::json!({}),
        })
        .set_enabled(is_enabled);
}

fn status_tone(status: &ContextServerStatus) -> StatusTone {
    match status {
        ContextServerStatus::Running => StatusTone::Success,
        ContextServerStatus::Starting | ContextServerStatus::Authenticating => StatusTone::Info,
        ContextServerStatus::Stopped => StatusTone::Neutral,
        ContextServerStatus::AuthRequired | ContextServerStatus::ClientSecretRequired { .. } => {
            StatusTone::Warning
        }
        ContextServerStatus::Error(_) => StatusTone::Error,
    }
}

/// Disabled servers show no pill: the toggle already says they are off.
fn status_pill_for(
    status: &ContextServerStatus,
    is_enabled: bool,
) -> Option<(SharedString, StatusTone)> {
    if !is_enabled {
        return None;
    }
    let text = match status {
        ContextServerStatus::Running => "running",
        ContextServerStatus::Starting => "starting",
        ContextServerStatus::Authenticating => "signing in",
        ContextServerStatus::Stopped => "stopped",
        ContextServerStatus::AuthRequired | ContextServerStatus::ClientSecretRequired { .. } => {
            "needs sign-in"
        }
        ContextServerStatus::Error(_) => "error",
    };
    Some((t(text), status_tone(status)))
}

fn server_invocation(settings: &ContextServerSettings) -> Option<SharedString> {
    match settings {
        ContextServerSettings::Stdio { command, .. } => {
            Some(command_line(&command.path.to_string_lossy(), &command.args).into())
        }
        ContextServerSettings::Http { url, .. } => Some(url.to_string().into()),
        ContextServerSettings::Extension { .. } => None,
    }
}

fn command_line(program: &str, args: &[String]) -> String {
    let parts = std::iter::once(program).chain(args.iter().map(String::as_str));
    shlex::try_join(parts.clone()).unwrap_or_else(|_| parts.collect::<Vec<_>>().join(" "))
}

fn resolve_extension_display_name(id: &ContextServerId, cx: &App) -> Option<SharedString> {
    ExtensionStore::global(cx)
        .read(cx)
        .installed_extensions()
        .iter()
        .find(|(_, entry)| entry.manifest.context_servers.contains_key(&id.0))
        .map(|(_, entry)| {
            let name = entry.manifest.name.as_str();
            let stripped = name
                .strip_suffix(" MCP Server")
                .or_else(|| name.strip_suffix(" MCP"))
                .or_else(|| name.strip_suffix(" Context Server"))
                .unwrap_or(name);
            SharedString::from(stripped.to_string())
        })
}

// === Detail ===

fn detail_surface(cx: &App) -> Stateful<Div> {
    v_flex()
        .id("mcp-detail")
        .flex_1()
        .min_w_0()
        .h_full()
        .p_5()
        .gap_5()
        .rounded(px(12.))
        .bg(cx.theme().colors().element_background)
        .overflow_y_scroll()
}

fn render_detail(
    page: &McpPage,
    entries: &[ServerEntry],
    store: &Entity<ContextServerStore>,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    if let Some(form) = page.form.as_ref() {
        return render_form(form, window, cx).into_any_element();
    }
    let selected = match &page.selection {
        Some(Selection::Server(id)) => entries.iter().find(|entry| &entry.id == id),
        _ => None,
    };
    let Some(entry) = selected.or_else(|| entries.first()) else {
        return detail_surface(cx).into_any_element();
    };
    ensure_tools(page, entry, store, window, cx);
    render_server_detail(page, entry, store, cx).into_any_element()
}

fn ensure_tools(
    page: &McpPage,
    entry: &ServerEntry,
    store: &Entity<ContextServerStore>,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    if page.tools.contains_key(&entry.id) {
        return;
    }
    let Some(client) = store
        .read(cx)
        .get_running_server(&entry.id)
        .and_then(|server| server.client())
    else {
        return;
    };
    let id = entry.id.clone();
    cx.defer_in(window, move |this, _window, cx| {
        if this.mcp_page.tools.contains_key(&id) {
            return;
        }
        if !client.capable(context_server::protocol::ServerCapability::Tools) {
            this.mcp_page.tools.insert(id, Tools::Loaded(Vec::new()));
            cx.notify();
            return;
        }
        let task = cx.spawn({
            let id = id.clone();
            async move |this, cx| {
                let response = client
                    .request::<context_server::types::requests::ListTools>(())
                    .await;
                this.update(cx, |this, cx| {
                    let tools = match response {
                        Ok(response) => Tools::Loaded(
                            response
                                .tools
                                .into_iter()
                                .map(|tool| SharedString::from(tool.name))
                                .collect(),
                        ),
                        Err(error) => Tools::Failed(error.to_string().into()),
                    };
                    this.mcp_page.tools.insert(id, tools);
                    cx.notify();
                })
                .log_err();
            }
        });
        this.mcp_page
            .tools
            .insert(id, Tools::Loading { _task: task });
    });
}

fn render_server_detail(
    page: &McpPage,
    entry: &ServerEntry,
    store: &Entity<ContextServerStore>,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let colors = cx.theme().colors();
    let is_error = entry.enabled && matches!(entry.status, ContextServerStatus::Error(_));
    let tile_background = if is_error {
        Color::Error.color(cx).opacity(0.12)
    } else {
        colors.element_selected
    };
    let configuration = store.read(cx).configuration_for_server(&entry.id);
    let uses_oauth = configuration.as_ref().is_some_and(|configuration| {
        matches!(
            configuration.as_ref(),
            ContextServerConfiguration::Http { .. }
        ) && !configuration.has_static_auth_header()
    });
    let tools = page.tools.get(&entry.id);

    let mut summary = vec![match &entry.settings {
        Some(ContextServerSettings::Http { .. }) => t("Remote"),
        Some(ContextServerSettings::Stdio { .. }) => t("Local"),
        _ => t("Extension"),
    }];
    if uses_oauth {
        summary.push(t("OAuth"));
    }
    if let Some(Tools::Loaded(tools)) = tools {
        summary.push(t_count("{count} tools", tools.len()));
    }

    let id = entry.id.clone();
    let top = h_flex()
        .w_full()
        .gap_3()
        .child(
            h_flex()
                .size(px(36.))
                .flex_none()
                .justify_center()
                .rounded(px(8.))
                .bg(tile_background)
                .child(Icon::new(IconName::Server).color(if is_error {
                    Color::Error
                } else {
                    Color::Accent
                })),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Label::new(entry.name.clone())
                                .size(LabelSize::Large)
                                .weight(FontWeight::BOLD)
                                .truncate(),
                        )
                        .children(
                            status_pill_for(&entry.status, entry.enabled)
                                .map(|(label, tone)| status_pill(label, tone, cx)),
                        ),
                )
                .children(entry.invocation.clone().map(|invocation| {
                    Label::new(invocation)
                        .size(LabelSize::XSmall)
                        .buffer_font(cx)
                        .color(Color::Placeholder)
                        .truncate()
                })),
        )
        .when(entry.enabled, |this| {
            this.child(
                secondary_button("mcp-restart", None, t("Restart"), true, cx).on_click({
                    let store = store.clone();
                    let id = id.clone();
                    move |_, _, cx| {
                        store.update(cx, |store, cx| {
                            if let Some(server) = store.get_server(&id) {
                                store.start_server(server, cx);
                            }
                        });
                    }
                }),
            )
        })
        .when(!entry.extension, |this| {
            this.child(
                secondary_button("mcp-edit", None, t("Edit"), true, cx).on_click(cx.listener({
                    let id = id.clone();
                    move |this, _, window, cx| open_edit_form(this, &id, false, window, cx)
                })),
            )
        })
        .child(render_server_menu(entry, cx))
        .child(render_toggle(entry, cx));

    let mut detail = detail_surface(cx).child(top).child(
        Label::new(summary.join(" · "))
            .size(LabelSize::Small)
            .color(Color::Muted),
    );

    if let Some(element) = render_status_callout(entry, store, uses_oauth, cx) {
        detail = detail.child(element);
    }

    detail = detail
        .child(section_header(t("CONNECTION"), None))
        .child(render_connection(entry, uses_oauth, store, cx));

    if let Some(section) = render_secrets(entry, cx) {
        detail = detail.child(section);
    }

    let tools_count = match tools {
        Some(Tools::Loaded(tools)) => Some(tools.len()),
        _ => None,
    };
    let tools_title = match tools_count {
        Some(count) => t_count("TOOLS · {count}", count),
        None => t("TOOLS"),
    };
    detail
        .child(section_header(
            tools_title,
            Some(
                Label::new(t("what the agent sees from this server"))
                    .size(LabelSize::XSmall)
                    .color(Color::Placeholder)
                    .into_any_element(),
            ),
        ))
        .when(entry.enabled, |this| {
            this.child(render_on_demand(entry, cx))
        })
        .child(render_tools(tools, cx))
}

fn render_on_demand(entry: &ServerEntry, cx: &mut Context<SettingsWindow>) -> impl IntoElement {
    let id = entry.id.0.to_string();
    let on_demand = entry.on_demand;
    h_flex()
        .id("mcp-on-demand")
        .w_full()
        .gap_3()
        .py_1()
        .cursor_pointer()
        .on_click(cx.listener(move |_, _, _, cx| set_on_demand(id.clone(), !on_demand, cx)))
        .child(toggle_switch("mcp-on-demand-switch", on_demand, true, cx))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(Label::new(t("Load on demand")).size(LabelSize::Small))
                .child(
                    Label::new(t(
                        "Keeps these tools out of the agent's context until it asks to load them and you approve",
                    ))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                ),
        )
}

fn set_on_demand(server: String, on_demand: bool, cx: &mut Context<SettingsWindow>) {
    let fs = <dyn fs::Fs>::global(cx);
    settings::update_settings_file(fs, cx, move |settings, _| {
        let servers = settings
            .agent
            .get_or_insert_default()
            .on_demand_context_servers
            .get_or_insert_default();
        servers.retain(|id| id != &server);
        if on_demand {
            servers.push(server);
            servers.sort();
        }
    });
}

fn render_status_callout(
    entry: &ServerEntry,
    store: &Entity<ContextServerStore>,
    uses_oauth: bool,
    cx: &mut Context<SettingsWindow>,
) -> Option<AnyElement> {
    if !entry.enabled {
        return None;
    }
    let error_color = Color::Error.color(cx);
    match &entry.status {
        ContextServerStatus::Error(error) => {
            let error = error.to_string();
            Some(
                v_flex()
                    .w_full()
                    .p(px(14.))
                    .gap_2p5()
                    .rounded(px(10.))
                    .bg(error_color.opacity(0.1))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Icon::new(IconName::XCircle)
                                    .size(IconSize::Small)
                                    .color(Color::Error),
                            )
                            .child(
                                div().flex_1().child(
                                    Label::new(t("Could not start"))
                                        .size(LabelSize::Small)
                                        .weight(FontWeight::MEDIUM)
                                        .color(Color::Error),
                                ),
                            )
                            .child(
                                Button::new("mcp-copy-error", t("Copy error"))
                                    .style(ButtonStyle::Subtle)
                                    .label_size(LabelSize::Small)
                                    .on_click({
                                        let error = error.clone();
                                        move |_, _, cx| {
                                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                                error.clone(),
                                            ));
                                        }
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .p_2p5()
                            .rounded(px(6.))
                            .bg(cx.theme().colors().editor_background)
                            .child(
                                Label::new(error)
                                    .size(LabelSize::Small)
                                    .buffer_font(cx)
                                    .color(Color::Error),
                            ),
                    )
                    .when(uses_oauth, |this| {
                        this.child(h_flex().justify_end().child(sign_out_button(entry, store, cx)))
                    })
                    .into_any_element(),
            )
        }
        ContextServerStatus::AuthRequired => Some(
            notice(
                IconName::Info,
                Color::Warning,
                t("Sign in to connect this server."),
                cx,
            )
            .child(
                primary_button("mcp-sign-in", None, t("Sign in"), None, cx).on_click({
                    let store = store.clone();
                    let id = entry.id.clone();
                    move |_, _, cx| {
                        store.update(cx, |store, cx| {
                            store.authenticate_server(&id, cx).log_err();
                        });
                    }
                }),
            )
            .into_any_element(),
        ),
        ContextServerStatus::ClientSecretRequired { .. } => Some(
            notice(
                IconName::Info,
                Color::Warning,
                t("This server needs an OAuth client secret. The agent panel asks for it when it connects."),
                cx,
            )
            .into_any_element(),
        ),
        _ => None,
    }
}

fn notice(icon: IconName, color: Color, text: SharedString, cx: &App) -> Div {
    h_flex()
        .w_full()
        .p_3()
        .gap_2()
        .rounded(px(10.))
        .bg(color.color(cx).opacity(0.1))
        .child(Icon::new(icon).size(IconSize::Small).color(color))
        .child(
            div()
                .flex_1()
                .child(Label::new(text).size(LabelSize::Small)),
        )
}

fn sign_out_button(
    entry: &ServerEntry,
    store: &Entity<ContextServerStore>,
    _cx: &App,
) -> impl IntoElement {
    let store = store.clone();
    let id = entry.id.clone();
    Button::new("mcp-sign-out", t("Sign out"))
        .style(ButtonStyle::Subtle)
        .label_size(LabelSize::Small)
        .on_click(move |_, _, cx| {
            store.update(cx, |store, cx| {
                store.logout_server(&id, cx).log_err();
            });
        })
}

fn key_value(label: SharedString, value: impl IntoElement) -> Div {
    h_flex()
        .w_full()
        .min_h(px(30.))
        .gap_3()
        .child(
            div()
                .w(px(130.))
                .flex_none()
                .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
        )
        .child(h_flex().flex_1().min_w_0().gap_2().child(value))
}

fn mono(text: impl Into<SharedString>, cx: &App) -> Label {
    Label::new(text).size(LabelSize::Small).buffer_font(cx)
}

fn render_connection(
    entry: &ServerEntry,
    uses_oauth: bool,
    store: &Entity<ContextServerStore>,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let mut rows = v_flex().w_full().gap_0p5();
    match &entry.settings {
        Some(ContextServerSettings::Stdio { command, .. }) => {
            rows = rows
                .child(key_value(
                    t("Type"),
                    Label::new(t("Local (command)")).size(LabelSize::Small),
                ))
                .child(key_value(
                    t("Command"),
                    mono(
                        command_line(&command.path.to_string_lossy(), &command.args),
                        cx,
                    )
                    .truncate(),
                ))
                .child(key_value(t("Timeout"), timeout_value(command.timeout, cx)));
        }
        Some(ContextServerSettings::Http { url, timeout, .. }) => {
            rows = rows
                .child(key_value(
                    t("Type"),
                    Label::new(t("Remote (HTTP)")).size(LabelSize::Small),
                ))
                .child(key_value(t("URL"), mono(url.clone(), cx).truncate()))
                .when(uses_oauth, |this| {
                    let signed_in = matches!(entry.status, ContextServerStatus::Running);
                    this.child(key_value(
                        t("Authentication"),
                        h_flex()
                            .flex_1()
                            .gap_2()
                            .child(Label::new(t("OAuth")).size(LabelSize::Small))
                            .child(div().flex_1())
                            .when(signed_in, |this| {
                                this.child(sign_out_button(entry, store, cx))
                            }),
                    ))
                })
                .child(key_value(t("Timeout"), timeout_value(*timeout, cx)));
        }
        Some(ContextServerSettings::Extension { .. }) | None => {
            rows = rows.child(key_value(
                t("Type"),
                Label::new(t("Provided by an extension")).size(LabelSize::Small),
            ));
        }
    }

    let (path, scope) = match entry.scope {
        ServerScope::Project => (
            SettingsStore::global(cx)
                .asylum_project_settings_path()
                .map(|path| path.to_path_buf()),
            ServerScope::Project,
        ),
        ServerScope::Profile => (Some(paths::settings_file().clone()), ServerScope::Profile),
    };
    if let Some(path) = path {
        let shown = util::paths::PathExt::compact(&path)
            .to_string_lossy()
            .into_owned();
        rows = rows.child(key_value(
            t("Where it lives"),
            h_flex()
                .flex_1()
                .min_w_0()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(mono(shown, cx).color(Color::Muted).truncate()),
                )
                .child(
                    Button::new("mcp-open-json", t("Open in JSON"))
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::Small)
                        .color(Color::Accent)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            open_settings_json(this, scope, path.clone(), window, cx)
                        })),
                ),
        ));
    }
    rows
}

fn open_settings_json(
    settings_window: &mut SettingsWindow,
    scope: ServerScope,
    path: std::path::PathBuf,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    match scope {
        ServerScope::Profile => {
            settings_window.open_settings_file(SettingsUiFile::User, window, cx)
        }
        ServerScope::Project => {
            let workspace = settings_window.workspace.clone();
            window.defer(cx, move |window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        workspace
                            .open_abs_path(path, workspace::OpenOptions::default(), window, cx)
                            .detach_and_log_err(cx);
                    })
                    .log_err();
            });
        }
    }
}

fn timeout_value(timeout: Option<u64>, cx: &App) -> impl IntoElement {
    match timeout {
        Some(seconds) => h_flex().child(mono(format!("{seconds} s"), cx)),
        None => h_flex().gap_2().child(mono("60 s", cx)).child(
            Label::new(t("default"))
                .size(LabelSize::Small)
                .color(Color::Placeholder),
        ),
    }
}

fn render_secrets(entry: &ServerEntry, cx: &App) -> Option<AnyElement> {
    let (title, pairs) = match &entry.settings {
        Some(ContextServerSettings::Stdio { command, .. }) => (
            t("ENVIRONMENT VARIABLES"),
            command.env.as_ref().map(sorted_pairs).unwrap_or_default(),
        ),
        Some(ContextServerSettings::Http { headers, .. }) => (t("HEADERS"), sorted_pairs(headers)),
        _ => return None,
    };
    if pairs.is_empty() {
        return Some(
            section_header(
                title,
                Some(
                    Label::new(t("none"))
                        .size(LabelSize::XSmall)
                        .color(Color::Placeholder)
                        .into_any_element(),
                ),
            )
            .into_any_element(),
        );
    }
    Some(
        v_flex()
            .w_full()
            .gap_1()
            .child(section_header(title, None))
            .children(pairs.into_iter().map(|(key, _)| {
                h_flex()
                    .gap_2()
                    .min_h(px(26.))
                    .child(mono(key, cx))
                    .child(mono("= ••••••••", cx).color(Color::Placeholder))
            }))
            .into_any_element(),
    )
}

fn render_tools(tools: Option<&Tools>, cx: &App) -> AnyElement {
    let message = |text: SharedString| {
        Label::new(text)
            .size(LabelSize::Small)
            .color(Color::Placeholder)
            .into_any_element()
    };
    match tools {
        None => message(t("They show up once the server is running.")),
        Some(Tools::Loading { .. }) => message(t("Loading…")),
        Some(Tools::Failed(error)) => message(t_name("Could not list them: {name}", error)),
        Some(Tools::Loaded(tools)) if tools.is_empty() => message(t("This server has no tools.")),
        Some(Tools::Loaded(tools)) => h_flex()
            .w_full()
            .flex_wrap()
            .gap_1p5()
            .children(tools.iter().map(|tool| {
                div()
                    .h(px(24.))
                    .px_2()
                    .flex()
                    .items_center()
                    .rounded(px(6.))
                    .bg(cx.theme().colors().element_hover)
                    .child(mono(tool.clone(), cx).size(LabelSize::XSmall))
            }))
            .into_any_element(),
    }
}

fn render_server_menu(entry: &ServerEntry, cx: &mut Context<SettingsWindow>) -> impl IntoElement {
    let settings_window = cx.entity().downgrade();
    let id = entry.id.clone();
    let scope = entry.scope;
    let extension = entry.extension;
    let in_project = SettingsStore::global(cx)
        .asylum_project_settings_path()
        .is_some();
    PopoverMenu::new(SharedString::from(format!("mcp-menu-{}", entry.id.0)))
        .trigger(
            IconButton::new("mcp-more", IconName::Ellipsis)
                .icon_size(IconSize::Small)
                .tooltip(Tooltip::text(t("More"))),
        )
        .anchor(gpui::Anchor::TopRight)
        .menu(move |window, cx| {
            let settings_window = settings_window.clone();
            let id = id.clone();
            Some(ContextMenu::build(window, cx, move |menu, _, _| {
                let with_window = |action: fn(
                    &mut SettingsWindow,
                    &ContextServerId,
                    &mut Window,
                    &mut Context<SettingsWindow>,
                )| {
                    let settings_window = settings_window.clone();
                    let id = id.clone();
                    move |window: &mut Window, cx: &mut App| {
                        settings_window
                            .update(cx, |this, cx| action(this, &id, window, cx))
                            .log_err();
                    }
                };
                menu.when(!extension, |menu| {
                    menu.entry(
                        t("Duplicate"),
                        None,
                        with_window(|this, id, window, cx| {
                            open_edit_form(this, id, true, window, cx)
                        }),
                    )
                })
                .when(!extension && in_project, |menu| {
                    let label = match scope {
                        ServerScope::Profile => t("Move to the project"),
                        ServerScope::Project => t("Move to the profile"),
                    };
                    menu.entry(
                        label,
                        None,
                        with_window(|this, id, _, cx| move_server(this, id, cx)),
                    )
                })
                .when(!extension, |menu| menu.separator())
                .entry(
                    if extension {
                        t("Uninstall")
                    } else {
                        t("Delete")
                    },
                    None,
                    with_window(|this, id, _, cx| delete_server(this, id, cx)),
                )
            }))
        })
}

fn entry_for(
    settings_window: &SettingsWindow,
    id: &ContextServerId,
    cx: &App,
) -> Option<ServerEntry> {
    let store = settings_window
        .active_project(cx)?
        .read(cx)
        .context_server_store();
    collect_entries(&store, cx)
        .into_iter()
        .find(|entry| &entry.id == id)
}

fn delete_server(
    settings_window: &mut SettingsWindow,
    id: &ContextServerId,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(entry) = entry_for(settings_window, id, cx) else {
        return;
    };
    if entry.extension
        && let Some((extension_id, manifest)) = resolve_extension_for_context_server(id, cx)
        && extension_only_provides_context_server(&manifest)
    {
        ExtensionStore::global(cx)
            .update(cx, |store, cx| store.uninstall_extension(extension_id, cx))
            .detach_and_log_err(cx);
    }
    let name = id.0.clone();
    let written = SettingsStore::global(cx).update_settings_file_in_scope(
        <dyn fs::Fs>::global(cx),
        entry.scope.settings_scope(),
        move |settings, _| {
            settings.project.context_servers.remove(&name);
        },
    );
    log_write_error(written, cx);
    settings_window.mcp_page.selection = None;
    settings_window.mcp_page.tools.remove(id);
    cx.notify();
}

fn move_server(
    settings_window: &mut SettingsWindow,
    id: &ContextServerId,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(entry) = entry_for(settings_window, id, cx) else {
        return;
    };
    let Some(content) = entry.settings.as_ref().and_then(content_from_settings) else {
        return;
    };
    let target = match entry.scope {
        ServerScope::Profile => ServerScope::Project,
        ServerScope::Project => ServerScope::Profile,
    };
    write_server(
        id.0.clone(),
        content,
        target,
        Some((id.0.clone(), entry.scope)),
        cx,
    );
}

/// Writes `content` under `name` in `target`, removing `previous` first when it is another name
/// or another file (a rename or a move).
fn write_server(
    name: Arc<str>,
    content: ContextServerSettingsContent,
    target: ServerScope,
    previous: Option<(Arc<str>, ServerScope)>,
    cx: &mut Context<SettingsWindow>,
) {
    let fs = <dyn fs::Fs>::global(cx);
    let store = SettingsStore::global(cx);
    let mut remove_in_target = None;
    let mut removed_elsewhere = None;
    if let Some((previous_name, previous_scope)) = previous {
        if previous_scope != target {
            removed_elsewhere = Some(store.update_settings_file_in_scope(
                fs.clone(),
                previous_scope.settings_scope(),
                move |settings, _| {
                    settings.project.context_servers.remove(&previous_name);
                },
            ));
        } else if previous_name != name {
            remove_in_target = Some(previous_name);
        }
    }
    let written =
        store.update_settings_file_in_scope(fs, target.settings_scope(), move |settings, _| {
            if let Some(previous_name) = remove_in_target {
                settings.project.context_servers.remove(&previous_name);
            }
            settings.project.context_servers.insert(name, content);
        });
    if let Some(removed_elsewhere) = removed_elsewhere {
        log_write_error(removed_elsewhere, cx);
    }
    log_write_error(written, cx);
}

fn content_from_settings(settings: &ContextServerSettings) -> Option<ContextServerSettingsContent> {
    match settings {
        ContextServerSettings::Stdio {
            enabled,
            remote,
            command,
        } => Some(ContextServerSettingsContent::Stdio {
            enabled: *enabled,
            remote: *remote,
            command: command.clone(),
        }),
        ContextServerSettings::Http {
            enabled,
            url,
            headers,
            timeout,
            oauth,
        } => Some(ContextServerSettingsContent::Http {
            enabled: *enabled,
            url: url.to_string(),
            headers: headers.clone(),
            timeout: *timeout,
            oauth: oauth.as_ref().map(|oauth| OAuthClientSettings {
                client_id: oauth.client_id.clone(),
                client_secret: oauth.client_secret.clone(),
            }),
        }),
        ContextServerSettings::Extension { .. } => None,
    }
}

fn resolve_extension_for_context_server(
    id: &ContextServerId,
    cx: &App,
) -> Option<(Arc<str>, Arc<extension::ExtensionManifest>)> {
    ExtensionStore::global(cx)
        .read(cx)
        .installed_extensions()
        .iter()
        .find(|(_, entry)| entry.manifest.context_servers.contains_key(&id.0))
        .map(|(id, entry)| (id.clone(), entry.manifest.clone()))
}

fn extension_only_provides_context_server(manifest: &extension::ExtensionManifest) -> bool {
    manifest.context_servers.len() == 1
        && manifest.themes.is_empty()
        && manifest.icon_themes.is_empty()
        && manifest.languages.is_empty()
        && manifest.grammars.is_empty()
        && manifest.language_servers.is_empty()
        && manifest.slash_commands.is_empty()
        && manifest.snippets.is_none()
        && manifest.debug_locators.is_empty()
}

// === Add / edit form ===

struct KeyValueRow {
    key: Entity<Editor>,
    value: Entity<Editor>,
    revealed: bool,
}

/// Editor-backed state of the add/edit form shown in the detail column.
pub(crate) struct McpServerForm {
    transport: McpTransport,
    /// `Some` when editing an existing server (to remove the old entry on rename or move).
    original_id: Option<ContextServerId>,
    original_scope: Option<ServerScope>,
    scope: ServerScope,
    name: Entity<Editor>,
    command: Entity<Editor>,
    url: Entity<Editor>,
    timeout: Entity<Editor>,
    oauth_client_id: Entity<Editor>,
    env: Vec<KeyValueRow>,
    headers: Vec<KeyValueRow>,
    error: Option<SharedString>,
}

impl McpServerForm {
    fn rows_mut(&mut self) -> &mut Vec<KeyValueRow> {
        match self.transport {
            McpTransport::Stdio => &mut self.env,
            McpTransport::Http => &mut self.headers,
        }
    }

    fn rows(&self) -> &[KeyValueRow] {
        match self.transport {
            McpTransport::Stdio => &self.env,
            McpTransport::Http => &self.headers,
        }
    }
}

fn sorted_pairs(map: &HashMap<String, String>) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = map
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    pairs
}

fn new_input(
    placeholder: &'static str,
    initial: Option<&str>,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> Entity<Editor> {
    let initial = initial.map(|text| text.to_string());
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text(t(placeholder).as_ref(), window, cx);
        if let Some(text) = initial {
            editor.set_text(text, window, cx);
        }
        editor
    })
}

fn new_kv_row(
    key: Option<&str>,
    value: Option<&str>,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> KeyValueRow {
    let value = new_input("Value", value, window, cx);
    value.update(cx, |editor, cx| editor.set_masked(true, cx));
    KeyValueRow {
        key: new_input("NAME", key, window, cx),
        value,
        revealed: false,
    }
}

fn build_form(
    transport: McpTransport,
    name: Option<String>,
    settings: Option<&ContextServerSettings>,
    scope: ServerScope,
    original: Option<(ContextServerId, ServerScope)>,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> McpServerForm {
    let mut command_initial = None;
    let mut url_initial = None;
    let mut timeout_initial = None;
    let mut oauth_initial = None;
    let mut env = Vec::new();
    let mut headers = Vec::new();

    // Pre-fill from the raw settings so invalid values (e.g. a malformed URL typed straight into
    // settings.json) still load into the form for correction.
    match settings {
        Some(ContextServerSettings::Stdio { command, .. }) => {
            command_initial = Some(command_line(&command.path.to_string_lossy(), &command.args));
            timeout_initial = command.timeout.map(|timeout| timeout.to_string());
            for (key, value) in command.env.as_ref().map(sorted_pairs).unwrap_or_default() {
                env.push(new_kv_row(Some(&key), Some(&value), window, cx));
            }
        }
        Some(ContextServerSettings::Http {
            url,
            headers: header_map,
            timeout,
            oauth,
            ..
        }) => {
            url_initial = Some(url.clone());
            timeout_initial = timeout.map(|timeout| timeout.to_string());
            for (key, value) in sorted_pairs(header_map) {
                headers.push(new_kv_row(Some(&key), Some(&value), window, cx));
            }
            oauth_initial = oauth.as_ref().map(|oauth| oauth.client_id.clone());
        }
        _ => {}
    }

    let (original_id, original_scope) = original.unzip();
    McpServerForm {
        transport,
        original_id,
        original_scope,
        scope,
        name: new_input("github", name.as_deref(), window, cx),
        command: new_input(
            "npx -y @modelcontextprotocol/server-github",
            command_initial.as_deref(),
            window,
            cx,
        ),
        url: new_input(
            "https://example.com/mcp",
            url_initial.as_deref(),
            window,
            cx,
        ),
        timeout: new_input("60", timeout_initial.as_deref(), window, cx),
        oauth_client_id: new_input(
            "Optional OAuth client ID",
            oauth_initial.as_deref(),
            window,
            cx,
        ),
        env,
        headers,
        error: None,
    }
}

fn open_new_form(
    settings_window: &mut SettingsWindow,
    transport: McpTransport,
    name: Option<String>,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let form = build_form(
        transport,
        name,
        None,
        ServerScope::Profile,
        None,
        window,
        cx,
    );
    form.name.focus_handle(cx).focus(window, cx);
    settings_window.mcp_page.paste = None;
    settings_window.mcp_page.form = Some(form);
    settings_window.mcp_page.selection = Some(Selection::New);
    cx.notify();
}

fn open_edit_form(
    settings_window: &mut SettingsWindow,
    id: &ContextServerId,
    duplicate: bool,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(entry) = entry_for(settings_window, id, cx) else {
        return;
    };
    let transport = match entry.settings {
        Some(ContextServerSettings::Http { .. }) => McpTransport::Http,
        _ => McpTransport::Stdio,
    };
    let (name, original) = if duplicate {
        (t_name("{name}-copy", &id.0).to_string(), None)
    } else {
        (id.0.to_string(), Some((id.clone(), entry.scope)))
    };
    let form = build_form(
        transport,
        Some(name),
        entry.settings.as_ref(),
        entry.scope,
        original,
        window,
        cx,
    );
    form.name.focus_handle(cx).focus(window, cx);
    settings_window.mcp_page.form = Some(form);
    settings_window.mcp_page.selection = Some(if duplicate {
        Selection::New
    } else {
        Selection::Server(id.clone())
    });
    cx.notify();
}

fn field(
    label: &'static str,
    help: Option<&'static str>,
    control: impl IntoElement,
) -> impl IntoElement {
    v_flex()
        .w_full()
        .gap_1p5()
        .child(
            Label::new(t(label))
                .size(LabelSize::Small)
                .weight(FontWeight::MEDIUM),
        )
        .child(control)
        .children(help.map(|help| {
            Label::new(t(help))
                .size(LabelSize::XSmall)
                .color(Color::Placeholder)
        }))
}

fn input(editor: &Entity<Editor>, window: &Window, cx: &App) -> Div {
    let focused = editor.focus_handle(cx).is_focused(window);
    text_field(focused, cx).child(div().flex_1().min_w_0().child(editor.clone()))
}

fn render_form(
    form: &McpServerForm,
    window: &Window,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let field_input = |editor: &Entity<Editor>, cx: &App| input(editor, window, cx);
    let is_edit = form.original_id.is_some();
    let in_project = SettingsStore::global(cx)
        .asylum_project_settings_path()
        .is_some();
    let (icon, title, description) = match (form.transport, is_edit) {
        (_, true) => (
            IconName::Pencil,
            t_name(
                "Edit {name}",
                form.original_id.as_ref().map_or("", |id| &id.0),
            ),
            t("Changes restart the server."),
        ),
        (McpTransport::Stdio, false) => (
            IconName::Terminal,
            t("New server"),
            t("Runs a command on this machine and talks to it over stdin/stdout."),
        ),
        (McpTransport::Http, false) => (
            IconName::Public,
            t("New server"),
            t("Connects to a server over HTTP."),
        ),
    };

    let transport_option = |id: &'static str, label: &'static str, transport: McpTransport| {
        segment_option(id, t(label), form.transport == transport, true, cx)
            .px_3()
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(form) = this.mcp_page.form.as_mut() {
                    form.transport = transport;
                    form.error = None;
                }
                cx.notify();
            }))
    };

    let rows_label = match form.transport {
        McpTransport::Stdio => "Environment variables",
        McpTransport::Http => "Headers",
    };
    let add_row_label = match form.transport {
        McpTransport::Stdio => "Add variable",
        McpTransport::Http => "Add header",
    };
    let rows = v_flex()
        .w_full()
        .gap_1p5()
        .child(
            h_flex()
                .w_full()
                .child(
                    div().flex_1().child(
                        Label::new(t(rows_label))
                            .size(LabelSize::Small)
                            .weight(FontWeight::MEDIUM),
                    ),
                )
                .child(
                    Button::new("mcp-add-row", t(add_row_label))
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::Small)
                        .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
                        .on_click(cx.listener(|this, _, window, cx| {
                            let row = new_kv_row(None, None, window, cx);
                            if let Some(form) = this.mcp_page.form.as_mut() {
                                row.key.focus_handle(cx).focus(window, cx);
                                form.rows_mut().push(row);
                            }
                            cx.notify();
                        })),
                ),
        )
        .children(form.rows().iter().enumerate().map(|(index, row)| {
            h_flex()
                .w_full()
                .gap_2()
                .child(div().w(px(280.)).flex_none().child(field_input(&row.key, cx)))
                .child(
                    field_input(&row.value, cx).child(
                        IconButton::new(
                            ("mcp-reveal", index),
                            if row.revealed {
                                IconName::EyeOff
                            } else {
                                IconName::Eye
                            },
                        )
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text(if row.revealed {
                            t("Hide")
                        } else {
                            t("Show")
                        }))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(row) = this
                                .mcp_page
                                .form
                                .as_mut()
                                .and_then(|form| form.rows_mut().get_mut(index))
                            {
                                row.revealed = !row.revealed;
                                let masked = !row.revealed;
                                row.value.update(cx, |editor, cx| editor.set_masked(masked, cx));
                            }
                            cx.notify();
                        })),
                    ),
                )
                .child(
                    IconButton::new(("mcp-remove-row", index), IconName::Trash)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text(t("Remove")))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(form) = this.mcp_page.form.as_mut() {
                                let rows = form.rows_mut();
                                if index < rows.len() {
                                    rows.remove(index);
                                }
                            }
                            cx.notify();
                        })),
                )
        }))
        .child(
            Label::new(t(
                "Values are saved in the settings.json of the place chosen below. The eye shows or hides them.",
            ))
            .size(LabelSize::XSmall)
            .color(Color::Placeholder),
        );

    let scope_option = |id: &'static str, label: SharedString, scope: ServerScope| {
        segment_option(id, label, form.scope == scope, true, cx)
            .px_3()
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(form) = this.mcp_page.form.as_mut() {
                    form.scope = scope;
                }
                cx.notify();
            }))
    };

    let save_label = if is_edit {
        t("Save and restart")
    } else {
        t("Add and start")
    };

    detail_surface(cx)
        .child(
            h_flex()
                .w_full()
                .gap_3()
                .child(
                    h_flex()
                        .size(px(36.))
                        .flex_none()
                        .justify_center()
                        .rounded(px(8.))
                        .bg(cx.theme().colors().element_selected)
                        .child(Icon::new(icon).color(Color::Accent)),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(
                            Label::new(title)
                                .size(LabelSize::Large)
                                .weight(FontWeight::BOLD),
                        )
                        .child(
                            Label::new(description)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                )
                .child(
                    segmented_control(cx)
                        .flex_none()
                        .child(transport_option("mcp-transport-local", "Local", McpTransport::Stdio))
                        .child(transport_option("mcp-transport-remote", "Remote", McpTransport::Http)),
                ),
        )
        .child(field(
            "Name",
            Some("The agent sees it as the prefix of the server's tools."),
            field_input(&form.name, cx),
        ))
        .map(|this| match form.transport {
            McpTransport::Stdio => this.child(field(
                "Command",
                Some("The first word is the program; the rest become arguments. Quotes work as in the terminal."),
                field_input(&form.command, cx),
            )),
            McpTransport::Http => this
                .child(field("URL", None, field_input(&form.url, cx)))
                .child(field(
                    "OAuth client ID",
                    Some("Only for servers that need a pre-registered OAuth app."),
                    field_input(&form.oauth_client_id, cx),
                )),
        })
        .child(rows)
        .child(
            h_flex()
                .w_full()
                .items_start()
                .gap_6()
                .child(
                    div()
                        .w(px(200.))
                        .flex_none()
                        .child(field(
                            "Timeout",
                            Some("Seconds; empty uses the default."),
                            div().w(px(120.)).child(field_input(&form.timeout, cx)),
                        )),
                )
                .when(in_project, |this| {
                    let project_name = SettingsStore::global(cx)
                        .asylum_project_settings_path()
                        .and_then(|path| path.parent()?.parent()?.file_name())
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    this.child(
                        v_flex()
                            .flex_1()
                            .gap_1p5()
                            .child(
                                Label::new(t("Save in"))
                                    .size(LabelSize::Small)
                                    .weight(FontWeight::MEDIUM),
                            )
                            .child(
                                segmented_control(cx)
                                    .flex_none()
                                    .w(px(320.))
                                    .child(scope_option(
                                        "mcp-scope-profile",
                                        t("Profile"),
                                        ServerScope::Profile,
                                    ))
                                    .child(scope_option(
                                        "mcp-scope-project",
                                        t_name("Only in {name}", &project_name),
                                        ServerScope::Project,
                                    )),
                            )
                            .child(
                                Label::new(match form.scope {
                                    ServerScope::Profile => {
                                        t("In the profile it works in every project.")
                                    }
                                    ServerScope::Project => {
                                        t("Saved in the project's .asylum/settings.json.")
                                    }
                                })
                                .size(LabelSize::XSmall)
                                .color(Color::Placeholder),
                            ),
                    )
                }),
        )
        .when_some(form.error.clone(), |this, error| {
            this.child(
                h_flex()
                    .gap_2()
                    .child(Icon::new(IconName::XCircle).size(IconSize::Small).color(Color::Error))
                    .child(Label::new(error).size(LabelSize::Small).color(Color::Error)),
            )
        })
        .child(div().flex_1())
        .child(
            h_flex()
                .w_full()
                .gap_2()
                .child(
                    div().flex_1().child(
                        Label::new(t(
                            "On save, Asylum starts the server and lists its tools here.",
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Placeholder),
                    ),
                )
                .child(
                    secondary_button("mcp-form-cancel", None, t("Cancel"), true, cx).on_click(
                        cx.listener(|this, _, _, cx| {
                            let page = &mut this.mcp_page;
                            page.form = None;
                            if page.selection == Some(Selection::New) {
                                page.selection = None;
                            }
                            cx.notify();
                        }),
                    ),
                )
                .child(
                    primary_button("mcp-form-save", None, save_label, None, cx).on_click(
                        cx.listener(|this, _, _, cx| save_form(this, cx)),
                    ),
                ),
        )
}

fn save_form(settings_window: &mut SettingsWindow, cx: &mut Context<SettingsWindow>) {
    let Some(form) = settings_window.mcp_page.form.as_ref() else {
        return;
    };
    let values = McpServerFormValues {
        transport: form.transport,
        original_id: form.original_id.clone(),
        name: form.name.read(cx).text(cx),
        command: form.command.read(cx).text(cx),
        url: form.url.read(cx).text(cx),
        timeout: form.timeout.read(cx).text(cx),
        oauth_client_id: form.oauth_client_id.read(cx).text(cx),
        env: read_kv(&form.env, cx),
        headers: read_kv(&form.headers, cx),
    };
    let scope = form.scope;
    let previous = form
        .original_id
        .clone()
        .zip(form.original_scope)
        .map(|(id, scope)| (id.0, scope));

    let result = build_settings_from_values(&values).and_then(|(id, original_id, content)| {
        let existing = settings_window
            .active_project(cx)
            .map(|project| {
                project
                    .read(cx)
                    .context_server_store()
                    .read(cx)
                    .server_ids()
                    .to_vec()
            })
            .unwrap_or_default();
        if name_collides_with_other_server(&id, original_id.as_ref(), &existing) {
            Err(t_name("A server named \"{name}\" already exists.", &id.0))
        } else {
            Ok((id, content))
        }
    });

    match result {
        Ok((id, content)) => {
            write_server(id.0.clone(), content, scope, previous, cx);
            let page = &mut settings_window.mcp_page;
            page.tools.remove(&id);
            page.form = None;
            page.selection = Some(Selection::Server(id));
        }
        Err(error) => {
            if let Some(form) = settings_window.mcp_page.form.as_mut() {
                form.error = Some(error);
            }
        }
    }
    cx.notify();
}

/// Plain (editor-free) snapshot of the form's contents, so the validation and build logic can
/// be exercised without a GPUI context.
struct McpServerFormValues {
    transport: McpTransport,
    original_id: Option<ContextServerId>,
    name: String,
    command: String,
    url: String,
    timeout: String,
    oauth_client_id: String,
    env: Vec<(String, String)>,
    headers: Vec<(String, String)>,
}

fn read_kv(rows: &[KeyValueRow], cx: &App) -> Vec<(String, String)> {
    rows.iter()
        .map(|row| (row.key.read(cx).text(cx), row.value.read(cx).text(cx)))
        .collect()
}

fn build_settings_from_values(
    values: &McpServerFormValues,
) -> Result<
    (
        ContextServerId,
        Option<ContextServerId>,
        ContextServerSettingsContent,
    ),
    SharedString,
> {
    let name = values.name.trim().to_string();
    if name.is_empty() {
        return Err(t("Server name is required."));
    }

    let timeout = parse_timeout(&values.timeout)?;

    let content = match values.transport {
        McpTransport::Stdio => {
            let line = values.command.trim();
            if line.is_empty() {
                return Err(t("Command is required."));
            }
            let mut words = shlex::split(line)
                .ok_or_else(|| t("The command has a quote that is never closed."))?
                .into_iter();
            let Some(program) = words.next() else {
                return Err(t("Command is required."));
            };
            let env = collect_kv(&values.env, "environment variable")?;
            ContextServerSettingsContent::Stdio {
                enabled: true,
                remote: false,
                command: ContextServerCommand {
                    path: program.into(),
                    args: words.collect(),
                    env: (!env.is_empty()).then_some(env),
                    timeout,
                },
            }
        }
        McpTransport::Http => {
            let url = values.url.trim().to_string();
            if url.is_empty() {
                return Err(t("URL is required."));
            }
            if let Err(error) = url::Url::parse(&url) {
                return Err(t_name("Invalid URL: {name}", &error.to_string()));
            }
            let headers = collect_kv(&values.headers, "header")?;
            let oauth_client_id = values.oauth_client_id.trim().to_string();
            let oauth = (!oauth_client_id.is_empty()).then(|| OAuthClientSettings {
                client_id: oauth_client_id,
                client_secret: None,
            });
            ContextServerSettingsContent::Http {
                enabled: true,
                url,
                headers,
                timeout,
                oauth,
            }
        }
    };

    Ok((
        ContextServerId(name.into()),
        values.original_id.clone(),
        content,
    ))
}

/// Returns whether saving under `id` would overwrite a *different* existing server. Editing a
/// server in place (`id == original_id`) is allowed.
fn name_collides_with_other_server(
    id: &ContextServerId,
    original_id: Option<&ContextServerId>,
    existing_ids: &[ContextServerId],
) -> bool {
    original_id.is_none_or(|original| original.0 != id.0)
        && existing_ids.iter().any(|existing| existing.0 == id.0)
}

fn parse_timeout(text: &str) -> Result<Option<u64>, SharedString> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    text.parse::<u64>()
        .map(Some)
        .map_err(|_| t("Timeout must be a positive whole number of seconds."))
}

fn collect_kv(
    rows: &[(String, String)],
    label: &'static str,
) -> Result<HashMap<String, String>, SharedString> {
    let mut map = HashMap::default();
    for (key, value) in rows {
        let key = key.trim().to_string();
        if key.is_empty() {
            continue;
        }
        if map.contains_key(&key) {
            let message = match label {
                "header" => "Duplicate header \"{name}\".",
                _ => "Duplicate environment variable \"{name}\".",
            };
            return Err(t_name(message, &key));
        }
        map.insert(key, value.clone());
    }
    Ok(map)
}

// === Paste JSON ===

pub(crate) struct McpPasteForm {
    editor: Entity<Editor>,
    scope: ServerScope,
    unchecked: HashSet<String>,
    _subscription: Subscription,
}

fn open_paste_form(
    settings_window: &mut SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let editor = cx.new(|cx| {
        let mut editor = Editor::auto_height(6, 14, window, cx);
        editor.set_placeholder_text(
            "{ \"mcpServers\": { \"github\": { \"command\": \"npx\", \"args\": [\"-y\", \"@modelcontextprotocol/server-github\"] } } }",
            window,
            cx,
        );
        editor
    });
    editor.focus_handle(cx).focus(window, cx);
    let subscription = cx.observe(&editor, |_, _, cx| cx.notify());
    settings_window.mcp_page.paste = Some(McpPasteForm {
        editor,
        scope: ServerScope::Profile,
        unchecked: HashSet::default(),
        _subscription: subscription,
    });
    cx.notify();
}

/// One server read from pasted JSON.
#[derive(Debug, PartialEq)]
struct PastedServer {
    name: String,
    content: ContextServerSettingsContent,
}

/// Reads the `mcpServers` block of Claude, Cursor and others, VS Code's `servers`, Asylum's
/// `context_servers`, or a bare map of servers.
fn parse_pasted_servers(text: &str) -> Result<Vec<PastedServer>, SharedString> {
    let value: serde_json::Value = serde_json_lenient::from_str(text)
        .map_err(|error| t_name("This is not valid JSON: {name}", &error.to_string()))?;
    let object = value
        .as_object()
        .ok_or_else(|| t("Expected a JSON object."))?;
    let servers = ["mcpServers", "servers", "context_servers"]
        .iter()
        .find_map(|key| object.get(*key).and_then(|servers| servers.as_object()))
        .unwrap_or(object);

    let mut parsed = Vec::new();
    for (name, server) in servers {
        let Some(server) = server.as_object() else {
            continue;
        };
        if !server.contains_key("command") && !server.contains_key("url") {
            continue;
        }
        let mut server = server.clone();
        // Other clients mark the transport with "type"; Asylum infers it from command or url.
        server.remove("type");
        if let Some(serde_json::Value::Object(env)) = server.get_mut("env") {
            env.retain(|_, value| value.is_string());
        }
        let content: ContextServerSettingsContent =
            serde_json::from_value(serde_json::Value::Object(server))
                .map_err(|error| t_name("Could not read a server: {name}", &error.to_string()))?;
        parsed.push(PastedServer {
            name: name.clone(),
            content,
        });
    }
    if parsed.is_empty() {
        return Err(t("No server with a command or a url was found."));
    }
    Ok(parsed)
}

fn pasted_invocation(content: &ContextServerSettingsContent) -> String {
    match content {
        ContextServerSettingsContent::Stdio { command, .. } => {
            command_line(&command.path.to_string_lossy(), &command.args)
        }
        ContextServerSettingsContent::Http { url, .. } => url.clone(),
        ContextServerSettingsContent::Extension { .. } => String::new(),
    }
}

fn render_paste_modal(
    paste: &McpPasteForm,
    store: Option<&Entity<ContextServerStore>>,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let text = paste.editor.read(cx).text(cx);
    let parsed = (!text.trim().is_empty()).then(|| parse_pasted_servers(&text));
    let existing: Vec<ContextServerId> = store
        .map(|store| store.read(cx).server_ids().to_vec())
        .unwrap_or_default();
    let in_project = SettingsStore::global(cx)
        .asylum_project_settings_path()
        .is_some();
    let colors = cx.theme().colors();

    let chosen = match &parsed {
        Some(Ok(servers)) => servers
            .iter()
            .filter(|server| !paste.unchecked.contains(&server.name))
            .count(),
        _ => 0,
    };

    let found = match &parsed {
        None => None,
        Some(Err(error)) => Some(
            Label::new(error.clone())
                .size(LabelSize::Small)
                .color(Color::Error)
                .into_any_element(),
        ),
        Some(Ok(servers)) => Some(
            v_flex()
                .w_full()
                .gap_2()
                .child(section_header(
                    t_count("FOUND · {count}", servers.len()),
                    None,
                ))
                .child(grouped_block(cx).children(servers.iter().map(|server| {
                    let checked = !paste.unchecked.contains(&server.name);
                    let exists = existing.iter().any(|id| id.0.as_ref() == server.name);
                    let (label, tone) = match (&server.content, exists) {
                        (_, true) => (t("exists · replaces"), StatusTone::Warning),
                        (ContextServerSettingsContent::Http { .. }, false) => {
                            (t("new · remote"), StatusTone::Success)
                        }
                        _ => (t("new · local"), StatusTone::Success),
                    };
                    let name = server.name.clone();
                    h_flex()
                        .id(SharedString::from(format!("mcp-paste-{}", server.name)))
                        .w_full()
                        .h(px(44.))
                        .px(px(10.))
                        .gap(px(10.))
                        .rounded(px(8.))
                        .cursor_pointer()
                        .hover(|this| this.bg(colors.element_hover))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(paste) = this.mcp_page.paste.as_mut()
                                && !paste.unchecked.remove(&name)
                            {
                                paste.unchecked.insert(name.clone());
                            }
                            cx.notify();
                        }))
                        .child(checkbox_indicator(checked, cx))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(Label::new(server.name.clone()).size(LabelSize::Small))
                                .child(
                                    mono(pasted_invocation(&server.content), cx)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Placeholder)
                                        .truncate(),
                                ),
                        )
                        .child(status_pill(label, tone, cx))
                })))
                .into_any_element(),
        ),
    };

    let scope_option = |id: &'static str, label: SharedString, scope: ServerScope| {
        segment_option(id, label, paste.scope == scope, true, cx)
            .px_3()
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(paste) = this.mcp_page.paste.as_mut() {
                    paste.scope = scope;
                }
                cx.notify();
            }))
    };

    div()
        .id("mcp-paste-overlay")
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui::black().opacity(0.5))
        .occlude()
        .child(
            v_flex()
                .w(px(580.))
                .p_5()
                .gap_4()
                .rounded(px(14.))
                .border_1()
                .border_color(colors.border_selected)
                .bg(colors.elevated_surface_background)
                .shadow(ui::ElevationIndex::ModalSurface.shadow(cx))
                .child(
                    v_flex()
                        .gap_1()
                        .child(
                            Label::new(t("Paste configuration"))
                                .weight(FontWeight::BOLD),
                        )
                        .child(
                            Label::new(t(
                                "Takes the mcpServers block from Claude, Cursor and VS Code, or Asylum's context_servers.",
                            ))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        ),
                )
                .child(
                    div()
                        .w_full()
                        .p_3()
                        .rounded(px(8.))
                        .border_1()
                        .border_color(colors.border_focused.opacity(0.5))
                        .bg(colors.editor_background)
                        .child(paste.editor.clone()),
                )
                .children(found)
                .when(in_project, |this| {
                    this.child(
                        h_flex()
                            .gap_3()
                            .child(Label::new(t("Save in")).size(LabelSize::Small))
                            .child(
                                segmented_control(cx)
                                    .w(px(280.))
                                    .child(scope_option(
                                        "mcp-paste-profile",
                                        t("Profile"),
                                        ServerScope::Profile,
                                    ))
                                    .child(scope_option(
                                        "mcp-paste-project",
                                        t("Only in the project"),
                                        ServerScope::Project,
                                    )),
                            ),
                    )
                })
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .justify_end()
                        .child(
                            secondary_button("mcp-paste-cancel", None, t("Cancel"), true, cx)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.mcp_page.paste = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            primary_button(
                                "mcp-paste-add",
                                None,
                                t_count("Add {count}", chosen),
                                None,
                                cx,
                            )
                            .when(chosen == 0, |this| this.opacity(0.5))
                            .when(chosen > 0, |this| {
                                this.on_click(cx.listener(|this, _, _, cx| add_pasted(this, cx)))
                            }),
                        ),
                ),
        )
}

fn add_pasted(settings_window: &mut SettingsWindow, cx: &mut Context<SettingsWindow>) {
    let Some(paste) = settings_window.mcp_page.paste.as_ref() else {
        return;
    };
    let Ok(servers) = parse_pasted_servers(&paste.editor.read(cx).text(cx)) else {
        return;
    };
    let servers: Vec<PastedServer> = servers
        .into_iter()
        .filter(|server| !paste.unchecked.contains(&server.name))
        .collect();
    let Some(first) = servers.first().map(|server| server.name.clone()) else {
        return;
    };
    let written = SettingsStore::global(cx).update_settings_file_in_scope(
        <dyn fs::Fs>::global(cx),
        paste.scope.settings_scope(),
        move |settings, _| {
            for server in servers {
                settings
                    .project
                    .context_servers
                    .insert(server.name.into(), server.content);
            }
        },
    );
    log_write_error(written, cx);
    let page = &mut settings_window.mcp_page;
    page.paste = None;
    page.form = None;
    page.selection = Some(Selection::Server(ContextServerId(first.into())));
    cx.notify();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(transport: McpTransport) -> McpServerFormValues {
        McpServerFormValues {
            transport,
            original_id: None,
            name: "my-server".into(),
            command: String::new(),
            url: String::new(),
            timeout: String::new(),
            oauth_client_id: String::new(),
            env: Vec::new(),
            headers: Vec::new(),
        }
    }

    fn id(name: &str) -> ContextServerId {
        ContextServerId(name.into())
    }

    #[test]
    fn parse_timeout_handles_empty_and_invalid() {
        assert_eq!(parse_timeout(""), Ok(None));
        assert_eq!(parse_timeout("   "), Ok(None));
        assert_eq!(parse_timeout("60"), Ok(Some(60)));
        assert_eq!(parse_timeout("  90  "), Ok(Some(90)));
        assert!(parse_timeout("abc").is_err());
        assert!(parse_timeout("-5").is_err());
        assert!(parse_timeout("1.5").is_err());
    }

    #[test]
    fn requires_server_name_command_and_url() {
        let mut local = values(McpTransport::Stdio);
        assert_eq!(
            build_settings_from_values(&local).unwrap_err().as_ref(),
            "Command is required."
        );
        local.name = "   ".into();
        local.command = "/bin/server".into();
        assert_eq!(
            build_settings_from_values(&local).unwrap_err().as_ref(),
            "Server name is required."
        );
        assert_eq!(
            build_settings_from_values(&values(McpTransport::Http))
                .unwrap_err()
                .as_ref(),
            "URL is required."
        );
    }

    #[test]
    fn rejects_invalid_url_timeout_and_quotes() {
        let mut remote = values(McpTransport::Http);
        remote.url = "not a url".into();
        assert!(
            build_settings_from_values(&remote)
                .unwrap_err()
                .starts_with("Invalid URL")
        );

        let mut local = values(McpTransport::Stdio);
        local.command = "/bin/server".into();
        local.timeout = "soon".into();
        assert_eq!(
            build_settings_from_values(&local).unwrap_err().as_ref(),
            "Timeout must be a positive whole number of seconds."
        );

        local.timeout = String::new();
        local.command = "npx \"unclosed".into();
        assert_eq!(
            build_settings_from_values(&local).unwrap_err().as_ref(),
            "The command has a quote that is never closed."
        );
    }

    #[test]
    fn rejects_duplicate_keys() {
        let mut local = values(McpTransport::Stdio);
        local.command = "/bin/server".into();
        local.env = vec![("FOO".into(), "1".into()), ("FOO".into(), "2".into())];
        assert_eq!(
            build_settings_from_values(&local).unwrap_err().as_ref(),
            "Duplicate environment variable \"FOO\"."
        );

        let mut remote = values(McpTransport::Http);
        remote.url = "https://example.com/mcp".into();
        remote.headers = vec![
            ("Authorization".into(), "a".into()),
            ("Authorization".into(), "b".into()),
        ];
        assert_eq!(
            build_settings_from_values(&remote).unwrap_err().as_ref(),
            "Duplicate header \"Authorization\"."
        );
    }

    #[test]
    fn splits_the_command_line_like_a_shell() {
        let mut local = values(McpTransport::Stdio);
        local.name = "  local  ".into();
        local.command = "npx -y \"@scope/server name\" --flag  value".into();
        local.timeout = "30".into();
        local.env = vec![
            ("KEY".into(), "VALUE".into()),
            ("EMPTY".into(), String::new()),
            ("   ".into(), "ignored".into()),
        ];

        let (id, original_id, content) = build_settings_from_values(&local).unwrap();
        assert_eq!(id.0.as_ref(), "local");
        assert_eq!(original_id, None);
        assert_eq!(
            content,
            ContextServerSettingsContent::Stdio {
                enabled: true,
                remote: false,
                command: ContextServerCommand {
                    path: "npx".into(),
                    args: vec![
                        "-y".into(),
                        "@scope/server name".into(),
                        "--flag".into(),
                        "value".into()
                    ],
                    env: Some(HashMap::from_iter([
                        ("KEY".to_string(), "VALUE".to_string()),
                        ("EMPTY".to_string(), String::new()),
                    ])),
                    timeout: Some(30),
                },
            }
        );
        assert_eq!(
            command_line("npx", &["-y".into(), "@scope/server name".into()]),
            "npx -y '@scope/server name'"
        );
    }

    #[test]
    fn builds_remote_server() {
        let mut remote = values(McpTransport::Http);
        remote.name = "remote".into();
        remote.url = "https://example.com/mcp".into();
        remote.oauth_client_id = "client-123".into();
        remote.headers = vec![("Authorization".into(), "Bearer token".into())];

        let (_, _, content) = build_settings_from_values(&remote).unwrap();
        assert_eq!(
            content,
            ContextServerSettingsContent::Http {
                enabled: true,
                url: "https://example.com/mcp".into(),
                headers: HashMap::from_iter([(
                    "Authorization".to_string(),
                    "Bearer token".to_string()
                )]),
                timeout: None,
                oauth: Some(OAuthClientSettings {
                    client_id: "client-123".into(),
                    client_secret: None,
                }),
            }
        );
    }

    #[test]
    fn name_collision_covers_new_and_rename() {
        let existing = vec![id("foo"), id("bar")];
        assert!(name_collides_with_other_server(&id("foo"), None, &existing));
        assert!(!name_collides_with_other_server(
            &id("baz"),
            None,
            &existing
        ));
        assert!(!name_collides_with_other_server(
            &id("foo"),
            Some(&id("foo")),
            &existing
        ));
        assert!(name_collides_with_other_server(
            &id("bar"),
            Some(&id("foo")),
            &existing
        ));
    }

    #[test]
    fn reads_pasted_configurations_from_other_clients() {
        let claude = r#"{
            // Claude Desktop
            "mcpServers": {
                "github": {
                    "command": "npx",
                    "args": ["-y", "@modelcontextprotocol/server-github"],
                    "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "ghp_x" },
                },
                "sentry": { "type": "http", "url": "https://mcp.sentry.dev/mcp" }
            }
        }"#;
        let servers = parse_pasted_servers(claude).unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "github");
        assert!(matches!(
            &servers[0].content,
            ContextServerSettingsContent::Stdio { command, .. }
                if command.args == ["-y", "@modelcontextprotocol/server-github"]
                    && command.env.as_ref().is_some_and(|env| env.len() == 1)
        ));
        assert!(matches!(
            &servers[1].content,
            ContextServerSettingsContent::Http { url, .. } if url == "https://mcp.sentry.dev/mcp"
        ));

        let vs_code = r#"{ "servers": { "fetch": { "type": "stdio", "command": "uvx", "args": ["mcp-server-fetch"] } } }"#;
        assert_eq!(parse_pasted_servers(vs_code).unwrap()[0].name, "fetch");

        let bare = r#"{ "fetch": { "command": "uvx" } }"#;
        assert_eq!(parse_pasted_servers(bare).unwrap()[0].name, "fetch");

        assert_eq!(
            parse_pasted_servers(r#"{ "theme": "dark" }"#)
                .unwrap_err()
                .as_ref(),
            "No server with a command or a url was found."
        );
        assert!(parse_pasted_servers("not json").is_err());
    }
}
