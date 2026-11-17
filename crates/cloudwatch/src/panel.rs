pub(crate) mod credentials;

use std::{
    any::TypeId,
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, SystemTime},
};

use fs::Fs;
use gpui::{
    Action as _, AnyElement, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Pixels,
    ScrollHandle, Subscription, Task, WeakEntity, actions, px,
};
use gpui_tokio::Tokio;
use i18n::t;
use project::Project;
use ui::{
    ContextMenu, KeyBinding, PopoverMenu, PopoverTriggerArea, Tooltip, checkbox_indicator,
    error_callout, prelude::*, radio_indicator,
};
use ui_input::{ErasedEditorEvent, InputField};
use util::ResultExt as _;
use workspace::{
    HideStatusItem, ItemHandle, StatusItemView, Toast, ToolkitEntry, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::NotificationId,
};

use crate::{
    AwsProfile, CloudWatchStore, Connection, LogGroup, NewQuery, OpenLiveTail, ProfileKind,
    QUERIES_DIR, QUERY_EXTENSION, RunAwsLogin, StoreEvent, Target, ToggleFocus, aws_login,
    live_tail_view, query_view,
    store::{CLOUDWATCH_DIR, GROUPS_FILE},
};
use credentials::Validity;

actions!(
    cloudwatch,
    [
        /// Focuses the filter above the log groups in the CloudWatch dock.
        FocusFilter,
    ]
);

/// What the dock asks AWS for, listed when the session expires so the user knows what to grant.
const PERMISSIONS: [&str; 4] = [
    "logs:DescribeLogGroups",
    "logs:StartQuery · GetQueryResults",
    "logs:FilterLogEvents",
    "logs:StartLiveTail",
];

const REGIONS: [&str; 20] = [
    "us-east-1",
    "us-east-2",
    "us-west-1",
    "us-west-2",
    "sa-east-1",
    "ca-central-1",
    "eu-west-1",
    "eu-west-2",
    "eu-west-3",
    "eu-central-1",
    "eu-north-1",
    "eu-south-1",
    "ap-south-1",
    "ap-southeast-1",
    "ap-southeast-2",
    "ap-northeast-1",
    "ap-northeast-2",
    "ap-northeast-3",
    "af-south-1",
    "me-south-1",
];

/// The countdowns in the dock ("sessão · 6 h 48 min", "há 3 d") move in minutes.
const CLOCK_INTERVAL: Duration = Duration::from_secs(30);

pub(crate) fn init(cx: &mut App) {
    workspace::register_panel_item::<CloudWatchPanel>(cx);
    workspace::register_lazy_panel::<CloudWatchPanel>(
        |workspace, window, cx| {
            if workspace.panel::<CloudWatchPanel>(cx).is_some()
                || !task_agents::toolkit_enabled(
                    task_agents::toolkit_ids::CLOUDWATCH,
                    workspace.project(),
                    cx,
                )
            {
                return;
            }
            let panel = cx.new(|cx| CloudWatchPanel::new(workspace, window, cx));
            workspace.add_panel(panel, window, cx);
        },
        cx,
    );
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            toggle_focus(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &NewQuery, window, cx| {
            if toolkit_allowed(workspace, cx) {
                new_query(workspace, window, cx);
            }
        });
        workspace.register_action(|workspace, _: &OpenLiveTail, window, cx| {
            if !toolkit_allowed(workspace, cx) {
                return;
            }
            let groups = store_for(workspace, cx).read(cx).project_groups().to_vec();
            live_tail_view::open_live_tail(workspace, groups, window, cx);
        });
        workspace.register_action(|workspace, _: &RunAwsLogin, window, cx| {
            if !toolkit_allowed(workspace, cx) {
                return;
            }
            let store = store_for(workspace, cx);
            aws_login::run_aws_login(workspace, store, window, cx);
        });
    })
    .detach();
}

fn store_for(workspace: &Workspace, cx: &mut App) -> Entity<CloudWatchStore> {
    CloudWatchStore::for_workspace(workspace.weak_handle(), workspace.project().clone(), cx)
}

/// Whether the CloudWatch toolkit is on for this workspace's project; says so when it isn't.
fn toolkit_allowed(workspace: &mut Workspace, cx: &mut Context<Workspace>) -> bool {
    if task_agents::toolkit_enabled(
        task_agents::toolkit_ids::CLOUDWATCH,
        workspace.project(),
        cx,
    ) {
        return true;
    }
    workspace.show_toast(
        Toast::new(
            NotificationId::unique::<CloudWatchPanel>(),
            task_agents::toolkit_disabled_message("CloudWatch"),
        )
        .autohide(),
        cx,
    );
    false
}

/// Adds the panel the first time it is asked for, so projects without AWS don't get a tab.
pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if !toolkit_allowed(workspace, cx) {
        return;
    }
    if workspace.panel::<CloudWatchPanel>(cx).is_none() {
        let panel = cx.new(|cx| CloudWatchPanel::new(workspace, window, cx));
        workspace.add_panel(panel, window, cx);
    }
    workspace.focus_panel::<CloudWatchPanel>(window, cx);
}

fn toggle_focus(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if !toolkit_allowed(workspace, cx) {
        return;
    }
    if workspace.panel::<CloudWatchPanel>(cx).is_none() {
        open(workspace, window, cx);
        return;
    }
    workspace.toggle_panel_focus::<CloudWatchPanel>(window, cx);
}

fn panel_is_visible(workspace: &Workspace, cx: &App) -> bool {
    workspace.all_docks().iter().any(|dock| {
        dock.read(cx)
            .visible_panel()
            .is_some_and(|panel| panel.panel_type_id() == TypeId::of::<CloudWatchPanel>())
    })
}

/// Writes a new `.insights` file in the project's queries folder and opens it.
fn new_query(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let store = store_for(workspace, cx);
    let Some(directory) = store.read(cx).queries_dir(cx) else {
        workspace.show_toast(
            Toast::new(
                NotificationId::unique::<CloudWatchPanel>(),
                t!("cloudwatch_panel.no_project").to_string(),
            )
            .autohide(),
            cx,
        );
        return;
    };
    let fs = workspace.app_state().fs.clone();
    let stem = t!("cloudwatch_panel.new_query.file_stem").to_string();
    let contents = default_query(&t!("cloudwatch_panel.new_query.comment"));
    let store = store.downgrade();
    cx.spawn_in(window, async move |workspace, cx| {
        fs.create_dir(&directory).await?;
        let path = unused_query_path(fs.as_ref(), &directory, &stem).await;
        fs.atomic_write(path.clone(), contents).await?;
        store.update(cx, |store, cx| store.reload_project_files(cx))?;
        workspace.update_in(cx, |workspace, window, cx| {
            query_view::open_query(workspace, path, window, cx)
        })?;
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

fn default_query(comment: &str) -> String {
    format!(
        "# {comment}\nfields @timestamp, @logStream, @message\n| sort @timestamp desc\n| limit 200\n"
    )
}

async fn unused_query_path(fs: &dyn Fs, directory: &Path, stem: &str) -> PathBuf {
    let mut number = 1;
    loop {
        let name = if number == 1 {
            format!("{stem}.{QUERY_EXTENSION}")
        } else {
            format!("{stem}_{number}.{QUERY_EXTENSION}")
        };
        let path = directory.join(name);
        if !fs.is_file(&path).await {
            return path;
        }
        number += 1;
    }
}

#[derive(Clone)]
enum Row {
    Header {
        title: SharedString,
        count: Option<usize>,
        hint: Option<&'static str>,
        refresh: bool,
    },
    Divider,
    Note(SharedString),
    ProjectGroup {
        name: String,
        checked: bool,
        detail: Option<String>,
    },
    Prefix {
        label: String,
        count: usize,
        expanded: bool,
    },
    Leaf {
        name: String,
        in_project: bool,
    },
    Query {
        path: PathBuf,
        name: String,
        age: Option<SharedString>,
    },
    Profile {
        name: String,
        detail: String,
        selected: bool,
        validity: Option<Validity>,
    },
    Permission(&'static str),
}

pub struct CloudWatchPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    store: Entity<CloudWatchStore>,
    focus_handle: FocusHandle,
    filter_input: Entity<InputField>,
    scroll_handle: ScrollHandle,
    expanded: HashSet<String>,
    /// Groups unchecked in this session stay listed under the project so the click can be undone.
    unchecked_groups: Vec<String>,
    validities: HashMap<String, Validity>,
    filter_shows_profiles: bool,
    _validities_task: Task<()>,
    _clock: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl CloudWatchPanel {
    pub fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let store = CloudWatchStore::for_workspace(workspace.weak_handle(), project.clone(), cx);
        let expired = session_expired(store.read(cx).connection());
        let filter_input = cx.new(|cx| {
            InputField::new(window, cx, &filter_placeholder(expired))
                .start_icon(IconName::MagnifyingGlass)
                .end_hint("⌘F")
        });
        let mut subscriptions = Vec::new();
        {
            let editor = filter_input.read(cx).editor().clone();
            let this = cx.weak_entity();
            subscriptions.push(editor.subscribe(
                Box::new(move |event, _window, cx| {
                    if event == ErasedEditorEvent::BufferEdited {
                        this.update(cx, |_, cx| cx.notify()).log_err();
                    }
                }),
                window,
                cx,
            ));
        }
        subscriptions.push(cx.observe(&store, |_, _, cx| cx.notify()));
        subscriptions.push(
            cx.subscribe_in(&store, window, |this, _, event, window, cx| {
                if *event == StoreEvent::TargetChanged {
                    this.unchecked_groups.clear();
                }
                this.sync_filter_placeholder(window, cx);
                this.reload_validities(cx);
            }),
        );
        let clock = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(CLOCK_INTERVAL).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        });
        let mut this = Self {
            workspace: workspace.weak_handle(),
            project,
            store,
            focus_handle: cx.focus_handle(),
            filter_input,
            scroll_handle: ScrollHandle::new(),
            expanded: HashSet::new(),
            unchecked_groups: Vec::new(),
            validities: HashMap::new(),
            filter_shows_profiles: expired,
            _validities_task: Task::ready(()),
            _clock: clock,
            _subscriptions: subscriptions,
        };
        this.reload_validities(cx);
        this
    }

    fn is_expired(&self, cx: &App) -> bool {
        session_expired(self.store.read(cx).connection())
    }

    fn sync_filter_placeholder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let expired = self.is_expired(cx);
        if expired == self.filter_shows_profiles {
            return;
        }
        self.filter_shows_profiles = expired;
        let editor = self.filter_input.read(cx).editor().clone();
        editor.set_placeholder_text(&filter_placeholder(expired), window, cx);
    }

    fn reload_validities(&mut self, cx: &mut Context<Self>) {
        let load = Tokio::spawn_result(cx, credentials::load_validities());
        self._validities_task = cx.spawn(async move |this, cx| {
            let Some(validities) = load.await.log_err() else {
                return;
            };
            this.update(cx, |this, cx| {
                this.validities = validities;
                cx.notify();
            })
            .log_err();
        });
    }

    fn reload_profiles(&mut self, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| store.reload_profiles(cx));
        self.reload_validities(cx);
    }

    fn set_profile(&mut self, name: String, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| {
            store.set_target(
                Target {
                    profile: Some(name),
                    region: None,
                },
                cx,
            )
        });
    }

    fn set_region(&mut self, region: Option<String>, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| {
            let profile = store.target().profile.clone();
            store.set_target(Target { profile, region }, cx)
        });
    }

    fn toggle_project_group(&mut self, name: String, cx: &mut Context<Self>) {
        let included = self.store.read(cx).project_groups().contains(&name);
        if included {
            if !self.unchecked_groups.contains(&name) {
                self.unchecked_groups.push(name.clone());
            }
        } else {
            self.unchecked_groups.retain(|existing| *existing != name);
        }
        self.store.update(cx, |store, cx| {
            store.set_project_group(&name, !included, cx)
        });
    }

    fn toggle_prefix(&mut self, prefix: String, cx: &mut Context<Self>) {
        if !self.expanded.remove(&prefix) {
            self.expanded.insert(prefix);
        }
        cx.notify();
    }

    fn open_saved_query(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| {
                query_view::open_query(workspace, path, window, cx)
            })
            .log_err();
    }

    fn rows(&self, cx: &App) -> Vec<Row> {
        let store = self.store.read(cx);
        let filter = self.filter_input.read(cx).text(cx).trim().to_lowercase();
        let filtering = !filter.is_empty();
        let matches = |text: &str| !filtering || text.to_lowercase().contains(&filter);
        let mut rows = Vec::new();

        if self.is_expired(cx) {
            rows.push(Row::Header {
                title: t!("cloudwatch_panel.section.profiles"),
                count: None,
                hint: None,
                refresh: false,
            });
            let current = store.profile_name();
            rows.extend(
                store
                    .profiles()
                    .iter()
                    .filter(|profile| matches(&profile.name))
                    .map(|profile| Row::Profile {
                        name: profile.name.clone(),
                        detail: profile_detail(profile),
                        selected: profile.name == current,
                        validity: self.validities.get(&profile.name).copied(),
                    }),
            );
            rows.push(Row::Note(t!("cloudwatch_panel.profiles_note")));
            rows.push(Row::Divider);
            rows.push(Row::Header {
                title: t!("cloudwatch_panel.section.permissions"),
                count: None,
                hint: None,
                refresh: false,
            });
            rows.extend(PERMISSIONS.into_iter().map(Row::Permission));
            rows.push(Row::Note(t!("cloudwatch_panel.permissions_note")));
        } else {
            let groups = store.log_groups();
            let project_groups = store.project_groups();
            rows.push(Row::Header {
                title: t!("cloudwatch_panel.section.project"),
                count: None,
                hint: Some(CLOUDWATCH_DIR),
                refresh: false,
            });
            let listed: Vec<(&String, bool)> = project_groups
                .iter()
                .map(|name| (name, true))
                .chain(
                    self.unchecked_groups
                        .iter()
                        .filter(|name| !project_groups.contains(name))
                        .map(|name| (name, false)),
                )
                .filter(|(name, _)| matches(name))
                .collect();
            if listed.is_empty() && !filtering {
                rows.push(Row::Note(t!("cloudwatch_panel.project_empty")));
            }
            rows.extend(listed.into_iter().map(|(name, checked)| {
                Row::ProjectGroup {
                    name: name.clone(),
                    checked,
                    detail: groups
                        .iter()
                        .find(|group| group.name == *name)
                        .and_then(group_detail),
                }
            }));

            rows.push(Row::Divider);
            rows.push(Row::Header {
                title: t!("cloudwatch_panel.section.all"),
                count: Some(groups.len()),
                hint: None,
                refresh: true,
            });
            if store.is_loading_log_groups() && groups.is_empty() {
                rows.push(Row::Note(t!("cloudwatch_panel.loading_groups")));
            }
            if let Some(error) = store.log_groups_error() {
                rows.push(Row::Note(error.to_string().into()));
            }
            let mut prefixes: BTreeMap<&str, Vec<&LogGroup>> = BTreeMap::new();
            for group in groups.iter().filter(|group| matches(&group.name)) {
                prefixes
                    .entry(log_group_prefix(&group.name))
                    .or_default()
                    .push(group);
            }
            for (prefix, members) in prefixes {
                let expanded = filtering || self.expanded.contains(prefix);
                rows.push(Row::Prefix {
                    label: prefix.to_string(),
                    count: members.len(),
                    expanded,
                });
                if expanded {
                    rows.extend(members.into_iter().map(|group| Row::Leaf {
                        name: group.name.clone(),
                        in_project: project_groups.contains(&group.name),
                    }));
                }
            }
            rows.push(Row::Divider);
        }

        rows.push(Row::Header {
            title: t!("cloudwatch_panel.section.queries"),
            count: None,
            hint: Some(QUERIES_DIR),
            refresh: false,
        });
        let now = SystemTime::now();
        rows.extend(store.saved_queries().iter().map(|query| {
            Row::Query {
                path: query.path.clone(),
                name: query.name.clone(),
                age: query
                    .modified
                    .map(|modified| age_label(now.duration_since(modified).unwrap_or_default())),
            }
        }));
        rows
    }

    fn render_row(&self, index: usize, row: Row, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let hover = colors.element_hover;
        let row_base = |height: f32| {
            h_flex()
                .id(("cloudwatch-row", index))
                .group("cloudwatch-row")
                .w_full()
                .h(px(height))
                .flex_none()
                .rounded(px(6.))
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
        };
        let mono = |text: SharedString, size: f32, color: Color| {
            Label::new(text)
                .size(LabelSize::Custom(rems_from_px(size)))
                .buffer_font(cx)
                .color(color)
                .single_line()
        };
        match row {
            Row::Header {
                title,
                count,
                hint,
                refresh,
            } => h_flex()
                .w_full()
                .h(px(34.))
                .flex_none()
                .px_2()
                .pt(px(14.))
                .pb(px(6.))
                .gap_2()
                .child(
                    Label::new(title)
                        .size(LabelSize::Custom(rems_from_px(11_f32)))
                        .weight(FontWeight::SEMIBOLD)
                        .color(Color::Muted),
                )
                .when_some(count, |this, count| {
                    this.child(mono(count.to_string().into(), 11., Color::Placeholder))
                })
                .child(div().flex_1())
                .when_some(hint, |this, hint| {
                    this.child(mono(hint.into(), 10.5, Color::Placeholder))
                })
                .when(refresh, |this| {
                    this.child(
                        IconButton::new("cloudwatch-refresh", IconName::RotateCw)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text(t!("cloudwatch_panel.refresh_tooltip")))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.store
                                    .update(cx, |store, cx| store.reload_log_groups(cx));
                            })),
                    )
                })
                .into_any_element(),
            Row::Divider => div()
                .w_full()
                .flex_none()
                .px_2()
                .pt(px(12.))
                .child(div().w_full().h_px().bg(colors.border_variant))
                .into_any_element(),
            Row::Note(text) => div()
                .w_full()
                .px_2()
                .py_1()
                .child(
                    Label::new(text)
                        .size(LabelSize::Custom(rems_from_px(11.5_f32)))
                        .color(Color::Placeholder),
                )
                .into_any_element(),
            Row::ProjectGroup {
                name,
                checked,
                detail,
            } => {
                let toggled = name.clone();
                row_base(28.)
                    .pl(px(10.))
                    .pr(px(12.))
                    .gap(px(10.))
                    .child(checkbox_indicator(checked, cx))
                    .child(
                        div().flex_1().min_w_0().child(
                            mono(
                                name.into(),
                                11.5,
                                if checked {
                                    Color::Default
                                } else {
                                    Color::Muted
                                },
                            )
                            .truncate(),
                        ),
                    )
                    .when_some(detail, |this, detail| {
                        this.child(mono(detail.into(), 10.5, Color::Placeholder))
                    })
                    .tooltip(Tooltip::text(if checked {
                        t!("cloudwatch_panel.remove_from_project", file = GROUPS_FILE)
                    } else {
                        t!("cloudwatch_panel.add_to_project", file = GROUPS_FILE)
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_project_group(toggled.clone(), cx)
                    }))
                    .into_any_element()
            }
            Row::Prefix {
                label,
                count,
                expanded,
            } => {
                let toggled = label.clone();
                row_base(26.)
                    .pl_2()
                    .pr(px(12.))
                    .gap(px(4.))
                    .child(
                        Icon::new(if expanded {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(IconSize::Small)
                        .color(Color::Muted),
                    )
                    .child(
                        Icon::new(if expanded {
                            IconName::FolderOpen
                        } else {
                            IconName::Folder
                        })
                        .size(IconSize::Small)
                        .color(Color::Muted),
                    )
                    .child(
                        div().flex_1().min_w_0().pl(px(2.)).child(
                            Label::new(label)
                                .size(LabelSize::Custom(rems_from_px(12.5_f32)))
                                .single_line()
                                .truncate(),
                        ),
                    )
                    .child(mono(count.to_string().into(), 10.5, Color::Placeholder))
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.toggle_prefix(toggled.clone(), cx)),
                    )
                    .into_any_element()
            }
            Row::Leaf { name, in_project } => {
                let added = name.clone();
                row_base(26.)
                    .pl(px(44.))
                    .pr(px(8.))
                    .gap(px(6.))
                    .child(
                        Icon::new(IconName::CloudPulse)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(mono(name.into(), 11.5, Color::Muted).truncate()),
                    )
                    .when(!in_project, |this| {
                        this.child(
                            div().visible_on_hover("cloudwatch-row").child(
                                IconButton::new(("cloudwatch-add-group", index), IconName::Plus)
                                    .icon_size(IconSize::Small)
                                    .icon_color(Color::Accent)
                                    .tooltip(Tooltip::text(t!(
                                        "cloudwatch_panel.add_to_project",
                                        file = GROUPS_FILE
                                    )))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.unchecked_groups.retain(|name| *name != added);
                                        this.store.update(cx, |store, cx| {
                                            store.set_project_group(&added, true, cx)
                                        });
                                    })),
                            ),
                        )
                    })
                    .into_any_element()
            }
            Row::Query { path, name, age } => row_base(28.)
                .pl(px(18.))
                .pr(px(12.))
                .gap_2()
                .child(
                    Icon::new(IconName::FileCode)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(
                    div().flex_1().min_w_0().child(
                        Label::new(name)
                            .size(LabelSize::Custom(rems_from_px(12.5_f32)))
                            .single_line()
                            .truncate(),
                    ),
                )
                .when_some(age, |this, age| {
                    this.child(mono(age, 10.5, Color::Placeholder))
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_saved_query(path.clone(), window, cx)
                }))
                .into_any_element(),
            Row::Profile {
                name,
                detail,
                selected,
                validity,
            } => {
                let chosen = name.clone();
                let validity = validity.map(|validity| validity_label(validity, SystemTime::now()));
                row_base(44.)
                    .rounded(px(8.))
                    .pl(px(10.))
                    .pr(px(14.))
                    .gap(px(10.))
                    .when(selected, |this| {
                        this.bg(colors.element_selected)
                            .border_1()
                            .border_color(colors.border_focused)
                    })
                    .child(radio_indicator(selected, cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.))
                            .child(
                                Label::new(name)
                                    .size(LabelSize::Custom(rems_from_px(12.5_f32)))
                                    .single_line()
                                    .truncate(),
                            )
                            .child(mono(detail.into(), 10.5, Color::Placeholder).truncate()),
                    )
                    .when_some(validity, |this, (label, color)| {
                        this.child(
                            Label::new(label)
                                .size(LabelSize::Custom(rems_from_px(11_f32)))
                                .color(color)
                                .single_line(),
                        )
                    })
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.set_profile(chosen.clone(), cx)),
                    )
                    .into_any_element()
            }
            Row::Permission(permission) => h_flex()
                .w_full()
                .h(px(22.))
                .flex_none()
                .px_2()
                .gap(px(6.))
                .child(
                    Icon::new(IconName::Check)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(mono(permission.into(), 11., Color::Muted))
                .into_any_element(),
        }
    }

    fn render_profile_card(&self, cx: &mut Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let expired = self.is_expired(cx);
        let profile_name = store.profile_name();
        let profile = store
            .profiles()
            .iter()
            .find(|profile| profile.name == profile_name)
            .cloned();
        let region = store
            .region()
            .unwrap_or_else(|| t!("cloudwatch_panel.no_region").to_string());
        let subtitle = match store.account_id() {
            Some(account) => t!(
                "cloudwatch_panel.account",
                account = masked_account(account),
                region = region
            )
            .to_string(),
            None => region,
        };
        let profiles: Vec<AwsProfile> = store.profiles().to_vec();
        let colors = cx.theme().colors();
        let tint = if expired {
            Color::Error.color(cx)
        } else {
            Color::Accent.color(cx)
        };
        let this = cx.weak_entity();
        let trigger = h_flex()
            .id("cloudwatch-profile-card")
            .w_full()
            .h(px(64.))
            .px_3()
            .gap(px(10.))
            .rounded(px(10.))
            .border_1()
            .border_color(if expired {
                tint.opacity(0.25)
            } else {
                colors.border_variant
            })
            .bg(colors.element_background)
            .hover(|style| style.bg(colors.element_hover))
            .child(
                div()
                    .size(px(36.))
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .rounded(px(8.))
                    .bg(tint.opacity(0.15))
                    .child(
                        Icon::new(IconName::CloudPulse)
                            .size(IconSize::Medium)
                            .color(if expired { Color::Error } else { Color::Accent }),
                    ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(4.))
                    .child(
                        h_flex()
                            .gap(px(9.))
                            .child(
                                Label::new(profile_name.clone())
                                    .size(LabelSize::Custom(rems_from_px(13.5_f32)))
                                    .weight(FontWeight::SEMIBOLD)
                                    .single_line()
                                    .truncate(),
                            )
                            .when_some(profile.as_ref(), |this, profile| {
                                this.child(kind_chip(profile.kind, cx))
                            }),
                    )
                    .child(
                        Label::new(subtitle)
                            .size(LabelSize::Custom(rems_from_px(11_f32)))
                            .buffer_font(cx)
                            .color(Color::Muted)
                            .single_line()
                            .truncate(),
                    ),
            )
            .child(
                Icon::new(IconName::ChevronDown)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            );
        PopoverMenu::new("cloudwatch-profile-menu")
            .full_width(true)
            .trigger(PopoverTriggerArea::new(trigger))
            .anchor(gpui::Anchor::TopRight)
            .menu(move |window, cx| {
                let this = this.clone();
                let profiles = profiles.clone();
                let current = profile_name.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    menu = menu.header(t!("cloudwatch_panel.switch_profile"));
                    for profile in profiles.iter() {
                        let this = this.clone();
                        let name = profile.name.clone();
                        menu = menu.toggleable_entry(
                            format!("{} · {}", profile.name, kind_label(profile.kind)),
                            profile.name == current,
                            IconPosition::Start,
                            None,
                            move |_, cx| {
                                this.update(cx, |this, cx| this.set_profile(name.clone(), cx))
                                    .log_err();
                            },
                        );
                    }
                    menu.separator()
                        .entry(
                            t!("cloudwatch_panel.menu.run_login"),
                            Some(RunAwsLogin.boxed_clone()),
                            |window, cx| window.dispatch_action(RunAwsLogin.boxed_clone(), cx),
                        )
                        .entry(t!("cloudwatch_panel.menu.reconnect"), None, {
                            let this = this.clone();
                            move |_, cx| {
                                this.update(cx, |this, cx| {
                                    this.store.update(cx, |store, cx| store.connect(cx))
                                })
                                .log_err();
                            }
                        })
                        .entry(t!("cloudwatch_panel.menu.reload_profiles"), None, {
                            move |_, cx| {
                                this.update(cx, |this, cx| this.reload_profiles(cx))
                                    .log_err();
                            }
                        })
                }))
            })
            .into_any_element()
    }

    fn render_status_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let now = SystemTime::now();
        let session_end = match self.validities.get(&store.profile_name()) {
            Some(Validity::Until(end)) => Some(*end),
            _ => None,
        };
        let (label, color, failure) = match store.connection() {
            Connection::Idle | Connection::Connecting => {
                (t!("cloudwatch_panel.status.connecting"), Color::Muted, None)
            }
            Connection::Connected(_) => {
                match session_end.and_then(|end| end.duration_since(now).ok()) {
                    Some(remaining) => (
                        t!(
                            "cloudwatch_panel.status.session",
                            remaining = format_duration(remaining)
                        ),
                        Color::Success,
                        None,
                    ),
                    None => (
                        t!("cloudwatch_panel.status.connected"),
                        Color::Success,
                        None,
                    ),
                }
            }
            Connection::SessionExpired { since, message, .. } => {
                let expired_at = session_end.filter(|end| *end <= now).unwrap_or(*since);
                (
                    t!(
                        "cloudwatch_panel.status.expired",
                        elapsed =
                            format_duration(now.duration_since(expired_at).unwrap_or_default())
                    ),
                    Color::Error,
                    Some(SharedString::from(message.clone())),
                )
            }
            Connection::Failed(message) => (
                t!("cloudwatch_panel.status.failed"),
                Color::Error,
                Some(SharedString::from(message.clone())),
            ),
        };
        let current_region = store.region();
        let profile_region = {
            let profile_name = store.profile_name();
            store
                .profiles()
                .iter()
                .find(|profile| profile.name == profile_name)
                .and_then(|profile| profile.region.clone())
        };
        let explicit_region = store.target().region.clone();
        let colors = cx.theme().colors();
        let chip = |id: &'static str| {
            h_flex()
                .id(id)
                .h(px(22.))
                .flex_none()
                .px_2()
                .gap(px(6.))
                .rounded(px(6.))
        };
        let tone = color.color(cx);
        let status = chip("cloudwatch-session")
            .bg(tone.opacity(0.12))
            .child(div().size(px(6.)).flex_none().rounded_full().bg(tone))
            .child(
                Label::new(label)
                    .size(LabelSize::Custom(rems_from_px(11.5_f32)))
                    .color(color)
                    .single_line(),
            )
            .when_some(failure, |this, failure| {
                this.tooltip(Tooltip::text(failure))
            });
        let region_trigger = chip("cloudwatch-region")
            .bg(colors.element_background)
            .border_1()
            .border_color(colors.border_variant)
            .hover(|style| style.bg(colors.element_hover))
            .child(
                Label::new(
                    current_region.unwrap_or_else(|| t!("cloudwatch_panel.no_region").to_string()),
                )
                .size(LabelSize::Custom(rems_from_px(11_f32)))
                .buffer_font(cx)
                .color(Color::Muted)
                .single_line(),
            )
            .child(
                Icon::new(IconName::ChevronDown)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            );
        let this = cx.weak_entity();
        let region = PopoverMenu::new("cloudwatch-region-menu")
            .trigger(PopoverTriggerArea::new(region_trigger))
            .anchor(gpui::Anchor::TopLeft)
            .menu(move |window, cx| {
                let this = this.clone();
                let profile_region = profile_region.clone();
                let explicit_region = explicit_region.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    if let Some(profile_region) = profile_region.clone() {
                        let this = this.clone();
                        menu = menu
                            .toggleable_entry(
                                t!(
                                    "cloudwatch_panel.region.profile_default",
                                    region = profile_region
                                ),
                                explicit_region.is_none(),
                                IconPosition::Start,
                                None,
                                move |_, cx| {
                                    this.update(cx, |this, cx| this.set_region(None, cx))
                                        .log_err();
                                },
                            )
                            .separator();
                    }
                    menu = menu.header(t!("cloudwatch_panel.region.header"));
                    for region in REGIONS {
                        let this = this.clone();
                        menu = menu.toggleable_entry(
                            region,
                            explicit_region.as_deref() == Some(region),
                            IconPosition::Start,
                            None,
                            move |_, cx| {
                                this.update(cx, |this, cx| {
                                    this.set_region(Some(region.to_string()), cx)
                                })
                                .log_err();
                            },
                        );
                    }
                    menu
                }))
            });
        let read_only = chip("cloudwatch-read-only")
            .bg(colors.element_background)
            .border_1()
            .border_color(colors.border_variant)
            .child(
                Icon::new(IconName::Lock)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Label::new(t!("cloudwatch_panel.read_only"))
                    .size(LabelSize::Custom(rems_from_px(11.5_f32)))
                    .color(Color::Muted)
                    .single_line(),
            )
            .tooltip(Tooltip::text(t!("cloudwatch_panel.read_only_tooltip")));
        h_flex()
            .px_3()
            .pt(px(10.))
            .gap(px(6.))
            .flex_wrap()
            .child(status)
            .child(region)
            .child(read_only)
            .into_any_element()
    }

    fn render_failure(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let Connection::Failed(message) = self.store.read(cx).connection() else {
            return None;
        };
        Some(
            v_flex()
                .px_3()
                .pt_2()
                .gap_1p5()
                .child(error_callout(
                    t!("cloudwatch_panel.failure.title"),
                    message.clone(),
                    cx,
                ))
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("cloudwatch-retry", t!("cloudwatch_panel.retry"))
                                .style(ButtonStyle::Filled)
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.store.update(cx, |store, cx| store.connect(cx))
                                })),
                        )
                        .child(
                            Button::new("cloudwatch-login", t!("cloudwatch_panel.menu.run_login"))
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(RunAwsLogin.boxed_clone(), cx)
                                }),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_usage(&self, cx: &mut Context<Self>) -> AnyElement {
        let usage = self.store.read(cx).usage().clone();
        let bytes = format_bytes(usage.bytes_scanned, portuguese_numbers());
        let minutes = usage.tail_seconds / 60;
        let colors = cx.theme().colors();
        v_flex()
            .mx_3()
            .mb(px(14.))
            .px_3()
            .py(px(10.))
            .gap(px(4.))
            .flex_none()
            .rounded(px(10.))
            .border_1()
            .border_color(colors.border_variant)
            .bg(colors.element_background)
            .child(
                Label::new(t!("cloudwatch_panel.usage.title"))
                    .size(LabelSize::Custom(rems_from_px(11_f32)))
                    .color(Color::Muted),
            )
            .child(
                Label::new(if usage.queries == 1 {
                    t!(
                        "cloudwatch_panel.usage.summary.one",
                        bytes = bytes,
                        count = usage.queries,
                        minutes = minutes
                    )
                } else {
                    t!(
                        "cloudwatch_panel.usage.summary.other",
                        bytes = bytes,
                        count = usage.queries,
                        minutes = minutes
                    )
                })
                .size(LabelSize::Custom(rems_from_px(12_f32))),
            )
            .child(
                Label::new(t!("cloudwatch_panel.usage.hint"))
                    .size(LabelSize::Custom(rems_from_px(11_f32)))
                    .color(Color::Placeholder),
            )
            .into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let summary = t!(
            "cloudwatch_panel.footer_summary",
            groups = store.log_groups().len(),
            project = store.project_groups().len()
        );
        h_flex()
            .h(px(44.))
            .flex_none()
            .px_3()
            .gap_2()
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(summary)
                        .size(LabelSize::Custom(rems_from_px(11.5_f32)))
                        .color(Color::Muted)
                        .single_line()
                        .truncate(),
                ),
            )
            .child(
                Button::new("cloudwatch-live-tail", t!("cloudwatch_panel.live_tail"))
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Custom(rems_from_px(11.5_f32)))
                    .color(Color::Accent)
                    .key_binding(
                        KeyBinding::for_action_in(&OpenLiveTail, &self.focus_handle, cx)
                            .size(rems_from_px(11_f32)),
                    )
                    .on_click(|_, window, cx| {
                        window.dispatch_action(OpenLiveTail.boxed_clone(), cx)
                    }),
            )
            .into_any_element()
    }
}

fn session_expired(connection: &Connection) -> bool {
    matches!(connection, Connection::SessionExpired { .. })
}

fn filter_placeholder(profiles: bool) -> SharedString {
    if profiles {
        t!("cloudwatch_panel.filter_profiles")
    } else {
        t!("cloudwatch_panel.filter_groups")
    }
}

fn kind_label(kind: ProfileKind) -> SharedString {
    match kind {
        ProfileKind::Login => t!("cloudwatch_panel.kind.login"),
        ProfileKind::Sso => t!("cloudwatch_panel.kind.sso"),
        ProfileKind::AccessKeys => t!("cloudwatch_panel.kind.access_keys"),
        ProfileKind::CredentialProcess => t!("cloudwatch_panel.kind.credential_process"),
        ProfileKind::AssumeRole => t!("cloudwatch_panel.kind.assume_role"),
        ProfileKind::Other => t!("cloudwatch_panel.kind.other"),
    }
}

fn kind_chip(kind: ProfileKind, cx: &App) -> impl IntoElement {
    h_flex()
        .h(px(18.))
        .flex_none()
        .px_2()
        .rounded(px(4.))
        .bg(cx.theme().colors().element_selected)
        .child(
            Label::new(kind_label(kind))
                .size(LabelSize::Custom(rems_from_px(10.5_f32)))
                .color(Color::Accent)
                .single_line(),
        )
}

/// `aws login · us-east-1`.
fn profile_detail(profile: &AwsProfile) -> String {
    format!(
        "{} · {}",
        kind_label(profile.kind),
        profile
            .region
            .clone()
            .unwrap_or_else(|| t!("cloudwatch_panel.no_region").to_string())
    )
}

fn validity_label(validity: Validity, now: SystemTime) -> (SharedString, Color) {
    match validity {
        Validity::NoExpiry => (t!("cloudwatch_panel.validity.no_expiry"), Color::Warning),
        Validity::Until(end) => match end.duration_since(now) {
            Ok(remaining) if !remaining.is_zero() => (
                t!(
                    "cloudwatch_panel.validity.valid_for",
                    remaining = format_duration(remaining)
                ),
                Color::Success,
            ),
            _ => (t!("cloudwatch_panel.validity.expired"), Color::Error),
        },
    }
}

/// `30 d · 18,2 GB`.
fn group_detail(group: &LogGroup) -> Option<String> {
    let retention = match group.retention_days {
        Some(days) => t!("cloudwatch_panel.retention_days", days = days),
        None => t!("cloudwatch_panel.retention_never"),
    };
    let size = group
        .stored_bytes
        .and_then(|bytes| u64::try_from(bytes).ok())
        .map(|bytes| format_bytes(bytes, portuguese_numbers()));
    Some(match size {
        Some(size) => format!("{retention} · {size}"),
        None => retention.to_string(),
    })
}

/// `123456789012` → `••••9012`.
fn masked_account(account: &str) -> String {
    let start = account.len().saturating_sub(4);
    format!("••••{}", account.get(start..).unwrap_or(account))
}

/// The folder a log group sits in: the service under `/aws/`, the first segment of other paths
/// and the name up to its first `/` or `_` otherwise (API Gateway's `…-Logs_<id>/<stage>`).
fn log_group_prefix(name: &str) -> &str {
    if let Some(rest) = name.strip_prefix("/aws/") {
        let end = rest
            .find('/')
            .map_or(name.len(), |index| "/aws/".len() + index);
        return &name[..end];
    }
    if let Some(rest) = name.strip_prefix('/') {
        let end = rest.find('/').map_or(name.len(), |index| 1 + index);
        return &name[..end];
    }
    let end = name.find(['/', '_']).unwrap_or(name.len());
    &name[..end]
}

fn portuguese_numbers() -> bool {
    i18n::current_language() == i18n::Language::PortugueseBrazil
}

/// `18234567890` → `17 GB`, `1181116006` → `1,1 GB` in Portuguese.
fn format_bytes(bytes: u64, portuguese: bool) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let text = if unit == 0 || value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    };
    if portuguese {
        text.replace('.', ",")
    } else {
        text
    }
}

/// `6 h 48 min`, `3 h`, `12 min`.
fn format_duration(duration: Duration) -> String {
    let total_minutes = (duration.as_secs() / 60).max(1);
    let hours = total_minutes / 60;
    let minutes = total_minutes % 60;
    match (hours, minutes) {
        (0, minutes) => format!("{minutes} min"),
        (hours, 0) => format!("{hours} h"),
        (hours, minutes) => format!("{hours} h {minutes} min"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Age {
    Now,
    Minutes(u64),
    Hours(u64),
    Days(u64),
    Weeks(u64),
    Months(u64),
}

fn age_of(elapsed: Duration) -> Age {
    let minutes = elapsed.as_secs() / 60;
    let hours = minutes / 60;
    let days = hours / 24;
    if minutes == 0 {
        Age::Now
    } else if hours == 0 {
        Age::Minutes(minutes)
    } else if days == 0 {
        Age::Hours(hours)
    } else if days < 7 {
        Age::Days(days)
    } else if days < 60 {
        Age::Weeks(days / 7)
    } else {
        Age::Months(days / 30)
    }
}

/// `agora`, `há 3 d`, `há 1 sem`.
fn age_label(elapsed: Duration) -> SharedString {
    match age_of(elapsed) {
        Age::Now => t!("cloudwatch_panel.age.now"),
        Age::Minutes(count) => t!("cloudwatch_panel.age.minutes", count = count),
        Age::Hours(count) => t!("cloudwatch_panel.age.hours", count = count),
        Age::Days(count) => t!("cloudwatch_panel.age.days", count = count),
        Age::Weeks(count) => t!("cloudwatch_panel.age.weeks", count = count),
        Age::Months(count) => t!("cloudwatch_panel.age.months", count = count),
    }
}

impl Render for CloudWatchPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_project = self.store.read(cx).project_root(cx).is_some();
        let content = if has_project {
            let rows = self.rows(cx);
            let rows: Vec<AnyElement> = rows
                .into_iter()
                .enumerate()
                .map(|(index, row)| self.render_row(index, row, cx))
                .collect();
            v_flex()
                .size_full()
                .child(div().px_3().pt(px(10.)).child(self.render_profile_card(cx)))
                .child(self.render_status_row(cx))
                .child(div().px_3().pt(px(12.)).child(self.filter_input.clone()))
                .children(self.render_failure(cx))
                .child(
                    div().flex_1().min_h_0().pt(px(2.)).child(
                        v_flex()
                            .id("cloudwatch-tree")
                            .size_full()
                            .px_1()
                            .pb_2()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll_handle)
                            .children(rows),
                    ),
                )
                .child(self.render_usage(cx))
                .child(self.render_footer(cx))
                .into_any_element()
        } else {
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(
                    Label::new(t!("cloudwatch_panel.no_project"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element()
        };
        v_flex()
            .key_context("CloudWatchPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .on_action(cx.listener(|this, _: &FocusFilter, window, cx| {
                let editor = this.filter_input.read(cx).editor().clone();
                editor.focus_handle(cx).focus(window, cx);
            }))
            .child(content)
    }
}

impl Focusable for CloudWatchPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for CloudWatchPanel {}

impl Panel for CloudWatchPanel {
    fn enabled(&self, cx: &App) -> bool {
        task_agents::toolkit_enabled(task_agents::toolkit_ids::CLOUDWATCH, &self.project, cx)
    }

    fn persistent_name() -> &'static str {
        "CloudWatchPanel"
    }

    fn panel_key() -> &'static str {
        "CloudWatchPanel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Right
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(384.)
    }

    /// No icon: a panel icon also puts a button in the status bar's panels group, and the
    /// toolkit button is already the way in.
    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        None
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::CloudPulse)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("CloudWatch")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        ToggleFocus.boxed_clone()
    }

    fn activation_priority(&self) -> u32 {
        14
    }
}

/// The CloudWatch button in the status bar's toolkit group, lit while the dock is open.
pub struct CloudWatchToolkitButton {
    workspace: WeakEntity<Workspace>,
    _dock_subscriptions: Vec<Subscription>,
}

impl CloudWatchToolkitButton {
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

impl Render for CloudWatchToolkitButton {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // The status bar lists it in the toolkit menu instead; see `toolkit_entry`.
        gpui::Empty
    }
}

impl StatusItemView for CloudWatchToolkitButton {
    fn toolkit_entry(&self, cx: &App) -> Option<ToolkitEntry> {
        let is_open = self
            .workspace
            .upgrade()
            .is_some_and(|workspace| panel_is_visible(workspace.read(cx), cx));
        let workspace = self.workspace.clone();
        Some(ToolkitEntry {
            id: task_agents::toolkit_ids::CLOUDWATCH,
            is_open,
            action: Box::new(ToggleFocus),
            toggle: Rc::new(move |window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        if is_open {
                            workspace.close_panel::<CloudWatchPanel>(window, cx);
                        } else {
                            open(workspace, window, cx);
                        }
                    })
                    .log_err();
            }),
        })
    }

    fn is_hidden(&self, cx: &App) -> bool {
        self.workspace.upgrade().is_some_and(|workspace| {
            !task_agents::toolkit_enabled(
                task_agents::toolkit_ids::CLOUDWATCH,
                workspace.read(cx).project(),
                cx,
            )
        })
    }

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
    use super::*;

    #[test]
    fn groups_fall_under_their_prefix() {
        assert_eq!(log_group_prefix("/aws/lambda/trix-webhooks"), "/aws/lambda");
        assert_eq!(
            log_group_prefix("/aws/rds/instance/trix-core/postgresql"),
            "/aws/rds"
        );
        assert_eq!(log_group_prefix("/ecs/trix-api-prod"), "/ecs");
        assert_eq!(log_group_prefix("/standalone"), "/standalone");
        assert_eq!(
            log_group_prefix("API-Gateway-Execution-Logs_abc123/prod"),
            "API-Gateway-Execution-Logs"
        );
        assert_eq!(log_group_prefix("plain"), "plain");
    }

    #[test]
    fn sizes_and_durations_read_like_the_boards() {
        assert_eq!(format_bytes(19_541_340_160, true), "18,2 GB");
        assert_eq!(format_bytes(671_088_640, true), "640 MB");
        assert_eq!(format_bytes(1_181_116_006, false), "1.1 GB");
        assert_eq!(format_bytes(512, false), "512 B");
        assert_eq!(
            format_duration(Duration::from_secs(6 * 3600 + 48 * 60)),
            "6 h 48 min"
        );
        assert_eq!(format_duration(Duration::from_secs(3 * 3600 + 20)), "3 h");
        assert_eq!(format_duration(Duration::from_secs(12 * 60 + 5)), "12 min");
        assert_eq!(format_duration(Duration::from_secs(5)), "1 min");
        assert_eq!(masked_account("123456784038"), "••••4038");
        assert_eq!(masked_account("12"), "••••12");
    }

    #[test]
    fn ages_round_down_to_the_largest_unit() {
        let minutes = |count: u64| Duration::from_secs(count * 60);
        assert_eq!(age_of(Duration::from_secs(20)), Age::Now);
        assert_eq!(age_of(minutes(5)), Age::Minutes(5));
        assert_eq!(age_of(minutes(3 * 60)), Age::Hours(3));
        assert_eq!(age_of(minutes(3 * 24 * 60)), Age::Days(3));
        assert_eq!(age_of(minutes(8 * 24 * 60)), Age::Weeks(1));
        assert_eq!(age_of(minutes(90 * 24 * 60)), Age::Months(3));
    }

    #[gpui::test]
    async fn new_queries_get_a_free_name(cx: &mut gpui::TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project/.asylum/cloudwatch/queries",
            serde_json::json!({ "consulta.insights": "", "consulta_2.insights": "" }),
        )
        .await;
        let directory = Path::new("/project/.asylum/cloudwatch/queries");
        assert_eq!(
            unused_query_path(fs.as_ref(), directory, "consulta").await,
            directory.join("consulta_3.insights")
        );
        assert_eq!(
            unused_query_path(fs.as_ref(), directory, "query").await,
            directory.join("query.insights")
        );
        assert!(default_query("x").starts_with("# x\nfields @timestamp"));
    }
}
