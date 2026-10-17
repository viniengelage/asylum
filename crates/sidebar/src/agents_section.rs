//! The AGENTES section of the threads sidebar: every agent the threads can run on (the Zed
//! Agent, the external ACP agents and the project's task agents), each filtering the history to
//! its own threads and starting new ones.

use crate::{ListEntry, Sidebar, ThreadEntryWorkspace, workspace_path_list};
use agent::ZED_AGENT_ID;
use agent_ui::{
    AgentPanel, AgentThreadSource, NewExternalAgentThread,
    task_agents_view::{RunTaskAgent, run_task_agent},
    thread_metadata_store::{ThreadMetadata, ThreadMetadataStore},
};
use gpui::{AnyElement, App, Context, SharedString, Window};
use project::AgentId;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use task_agents::{TaskAgent, TaskAgentsRevision};
use ui::{AgentThreadStatus, ContextMenu, Tooltip, prelude::*, right_click_menu};
use util::ResultExt as _;

/// Which agent's threads the history shows.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) enum AgentFilter {
    Agent(AgentId),
    /// A task agent, by command; its threads run on the Zed Agent.
    TaskAgent(SharedString),
}

impl AgentFilter {
    pub(crate) fn matches(&self, metadata: &ThreadMetadata) -> bool {
        match self {
            Self::Agent(agent_id) => &metadata.agent_id == agent_id,
            Self::TaskAgent(command) => metadata.task_agent.as_ref() == Some(command),
        }
    }
}

/// The project's task agents, loaded for the active workspace's folders.
#[derive(Default)]
pub(crate) struct LoadedTaskAgents {
    agents: Vec<TaskAgent>,
    loaded_for: Option<(Vec<PathBuf>, usize)>,
    _load: Option<gpui::Task<()>>,
}

struct AgentRow {
    filter: Option<AgentFilter>,
    name: SharedString,
    icon: IconName,
    external_icon: Option<SharedString>,
    command: Option<SharedString>,
    thread_count: usize,
    running: bool,
}

#[derive(Default)]
struct ThreadCounts {
    total: usize,
    by_filter: HashMap<AgentFilter, (usize, bool)>,
}

impl Sidebar {
    /// Reloads the project's task agents when the active workspace's folders changed or an agent
    /// file was written from the app.
    pub(crate) fn refresh_task_agents(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self
            .multi_workspace
            .upgrade()
            .map(|multi_workspace| multi_workspace.read(cx).workspace().clone())
        else {
            return;
        };
        let roots = workspace
            .read(cx)
            .project()
            .read(cx)
            .visible_worktrees(cx)
            .filter_map(|worktree| {
                let worktree = worktree.read(cx);
                worktree.as_local().map(|local| {
                    (
                        std::sync::Arc::<str>::from(worktree.root_name_str()),
                        local.abs_path().to_path_buf(),
                    )
                })
            })
            .collect::<Vec<_>>();
        let revision = cx
            .try_global::<TaskAgentsRevision>()
            .map_or(0, |revision| revision.0);
        let key = (
            roots
                .iter()
                .map(|(_, path)| path.clone())
                .collect::<Vec<_>>(),
            revision,
        );
        if self.task_agents.loaded_for.as_ref() == Some(&key) {
            return;
        }
        self.task_agents.loaded_for = Some(key);
        let fs = workspace.read(cx).project().read(cx).fs().clone();
        self.task_agents._load = Some(cx.spawn(async move |this, cx| {
            let (agents, errors) = task_agents::load_task_agents(fs, roots).await;
            for error in errors {
                log::warn!(
                    "task agent {} not loaded: {}",
                    error.path.display(),
                    error.message
                );
            }
            this.update(cx, |this, cx| {
                this.task_agents.agents = agents;
                cx.notify();
            })
            .log_err();
        }));
    }

    fn toggle_pinned_agent(&mut self, filter: &AgentFilter, cx: &mut Context<Self>) {
        if let Some(index) = self
            .pinned_agents
            .iter()
            .position(|pinned| pinned == filter)
        {
            self.pinned_agents.remove(index);
        } else {
            self.pinned_agents.push(filter.clone());
        }
        self.serialize(cx);
        cx.notify();
    }

    pub(crate) fn set_agent_filter(&mut self, filter: Option<AgentFilter>, cx: &mut Context<Self>) {
        if self.agent_filter == filter {
            return;
        }
        self.agent_filter = filter;
        self.selection = None;
        self.update_entries(cx);
    }

    fn task_agent_named(&self, command: &str) -> Option<&TaskAgent> {
        self.task_agents
            .agents
            .iter()
            .find(|agent| agent.command == command)
    }

    /// Threads per agent in the open project groups, with whether any is running.
    fn agent_thread_counts(&self, cx: &App) -> ThreadCounts {
        let mut counts = ThreadCounts::default();
        let Some(multi_workspace) = self.multi_workspace.upgrade() else {
            return counts;
        };
        let running_threads = self
            .live_thread_statuses
            .values()
            .filter(|(status, _)| {
                matches!(
                    status,
                    AgentThreadStatus::Running | AgentThreadStatus::WaitingForConfirmation
                )
            })
            .map(|(_, thread_id)| *thread_id)
            .collect::<HashSet<_>>();
        let store = ThreadMetadataStore::global(cx).read(cx);
        let mut seen = HashSet::new();
        for group in multi_workspace.read(cx).project_groups(cx) {
            let host = group.key.host();
            let entries = store
                .entries_for_main_worktree_path(group.key.path_list(), host.as_ref())
                .chain(store.entries_for_path(group.key.path_list(), host.as_ref()));
            for metadata in entries {
                if metadata.archived || metadata.is_draft() || !seen.insert(metadata.thread_id) {
                    continue;
                }
                counts.total += 1;
                let running = running_threads.contains(&metadata.thread_id);
                let mut add = |filter: AgentFilter| {
                    let entry = counts.by_filter.entry(filter).or_default();
                    entry.0 += 1;
                    entry.1 |= running;
                };
                add(AgentFilter::Agent(metadata.agent_id.clone()));
                if let Some(command) = &metadata.task_agent {
                    add(AgentFilter::TaskAgent(command.clone()));
                }
            }
        }
        counts
    }

    fn agent_rows(&self, cx: &App) -> (Vec<AgentRow>, Vec<AgentRow>) {
        let counts = self.agent_thread_counts(cx);
        let count_of =
            |filter: &AgentFilter| counts.by_filter.get(filter).copied().unwrap_or_default();
        let agent_server_store = self.multi_workspace.upgrade().map(|multi_workspace| {
            multi_workspace
                .read(cx)
                .workspace()
                .read(cx)
                .project()
                .read(cx)
                .agent_server_store()
                .clone()
        });

        let mut agents = vec![AgentRow {
            filter: None,
            name: "Todos os agentes".into(),
            icon: IconName::UserGroup,
            external_icon: None,
            command: None,
            thread_count: counts.total,
            running: counts.by_filter.values().any(|(_, running)| *running),
        }];
        let zed_filter = AgentFilter::Agent(ZED_AGENT_ID.clone());
        let (thread_count, running) = count_of(&zed_filter);
        agents.push(AgentRow {
            filter: Some(zed_filter),
            name: "Zed Agent".into(),
            icon: IconName::ZedAgent,
            external_icon: None,
            command: None,
            thread_count,
            running,
        });

        let mut external_ids: Vec<AgentId> = agent_server_store
            .as_ref()
            .map(|store| store.read(cx).external_agents().cloned().collect())
            .unwrap_or_default();
        for filter in counts.by_filter.keys() {
            if let AgentFilter::Agent(agent_id) = filter
                && *agent_id != *ZED_AGENT_ID
                && !external_ids.contains(agent_id)
            {
                external_ids.push(agent_id.clone());
            }
        }
        let mut external = external_ids
            .into_iter()
            .map(|agent_id| {
                let store = agent_server_store.as_ref().map(|store| store.read(cx));
                let name = store
                    .and_then(|store| store.agent_display_name(&agent_id))
                    .unwrap_or_else(|| SharedString::from(agent_id.to_string()));
                let external_icon = store.and_then(|store| store.agent_icon(&agent_id));
                let filter = AgentFilter::Agent(agent_id);
                let (thread_count, running) = count_of(&filter);
                AgentRow {
                    filter: Some(filter),
                    name,
                    icon: IconName::SquareDot,
                    external_icon,
                    command: None,
                    thread_count,
                    running,
                }
            })
            .collect::<Vec<_>>();
        external.sort_by(|a, b| {
            b.thread_count
                .cmp(&a.thread_count)
                .then_with(|| a.name.cmp(&b.name))
        });
        agents.extend(external);

        let mut commands = self
            .task_agents
            .agents
            .iter()
            .map(|agent| {
                (
                    SharedString::from(agent.command.clone()),
                    SharedString::from(agent.name.clone()),
                )
            })
            .collect::<Vec<_>>();
        // Threads of an agent whose file was removed still show, so they can be found.
        for filter in counts.by_filter.keys() {
            if let AgentFilter::TaskAgent(command) = filter
                && !commands.iter().any(|(known, _)| known == command)
            {
                commands.push((command.clone(), command.clone()));
            }
        }
        let project_agents = commands
            .into_iter()
            .map(|(command, name)| {
                let filter = AgentFilter::TaskAgent(command.clone());
                let (thread_count, running) = count_of(&filter);
                AgentRow {
                    filter: Some(filter),
                    name,
                    icon: IconName::UserGroup,
                    external_icon: None,
                    command: Some(command),
                    thread_count,
                    running,
                }
            })
            .collect();
        (agents, project_agents)
    }

    /// Starts a thread with the agent in the active workspace. A task agent's thread opens with
    /// `/command` in the composer, which hands the thread to it on send.
    pub(crate) fn new_thread_with_agent(
        &mut self,
        filter: Option<AgentFilter>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.active_workspace(cx) else {
            return;
        };
        if workspace_path_list(&workspace, cx).paths().is_empty() {
            return;
        }
        match filter {
            None => {
                self.create_new_thread(&workspace, window, cx);
            }
            Some(AgentFilter::TaskAgent(command)) => {
                workspace.update(cx, |workspace, cx| {
                    run_task_agent(
                        workspace,
                        &RunTaskAgent {
                            command: command.to_string(),
                            prompt: None,
                        },
                        window,
                        cx,
                    );
                });
            }
            Some(AgentFilter::Agent(agent_id)) => {
                workspace.update(cx, |workspace, cx| {
                    let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
                        return;
                    };
                    panel.update(cx, |panel, cx| {
                        if agent_id == *ZED_AGENT_ID {
                            panel.activate_new_thread(true, AgentThreadSource::Sidebar, window, cx);
                        } else {
                            panel.new_external_agent_thread(
                                &NewExternalAgentThread::new(agent_id),
                                window,
                                cx,
                            );
                        }
                    });
                    workspace.focus_panel::<AgentPanel>(window, cx);
                });
            }
        }
    }

    /// Makes the next thread the workspace's Agent panel activates open in an agent tab.
    pub(crate) fn prepare_new_agent_tab(
        workspace: &gpui::Entity<workspace::Workspace>,
        cx: &mut App,
    ) {
        if let Some(panel) = workspace.read(cx).panel::<AgentPanel>(cx) {
            panel.update(cx, |panel, cx| panel.prepare_new_agent_tab(cx));
        }
    }

    fn open_agent_in_new_tab(
        &mut self,
        filter: Option<AgentFilter>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.active_workspace(cx) else {
            return;
        };
        Self::prepare_new_agent_tab(&workspace, cx);
        self.new_thread_with_agent(filter, window, cx);
    }

    /// Opens the agent's most recent thread in the sidebar.
    fn resume_last_thread(
        &mut self,
        filter: &AgentFilter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let latest = self
            .contents
            .entries
            .iter()
            .filter_map(|entry| match entry {
                ListEntry::Thread(thread) if filter.matches(&thread.metadata) => Some(thread),
                _ => None,
            })
            .filter(|thread| thread.draft.is_none())
            .max_by_key(|thread| Self::thread_display_time(&thread.metadata))
            .cloned();
        let Some(thread) = latest else {
            self.new_thread_with_agent(Some(filter.clone()), window, cx);
            return;
        };
        if let ThreadEntryWorkspace::Open(workspace) = &thread.workspace {
            let workspace = workspace.clone();
            self.activate_thread(thread.metadata.clone(), &workspace, false, window, cx);
        }
    }

    fn configure_agent(&self, filter: &AgentFilter, window: &mut Window, cx: &mut App) {
        let Some(workspace) = self.active_workspace(cx) else {
            return;
        };
        match filter {
            AgentFilter::TaskAgent(_) => workspace.update(cx, |workspace, cx| {
                agent_ui::task_agents_view::open(workspace, window, cx);
            }),
            AgentFilter::Agent(_) => {
                window.dispatch_action(Box::new(zed_actions::agent::OpenSettings), cx);
            }
        }
    }

    pub(crate) fn render_agents_section(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let collapsed = self.agents_section_collapsed;
        let (mut agents, mut project_agents) = if collapsed {
            (Vec::new(), Vec::new())
        } else {
            self.agent_rows(cx)
        };
        // Pinned agents move up, right below "Todos os agentes", in the order they were pinned.
        let mut pinned = Vec::new();
        for filter in &self.pinned_agents {
            let is_pinned = |row: &AgentRow| row.filter.as_ref() == Some(filter);
            if let Some(index) = agents.iter().position(is_pinned) {
                pinned.push(agents.remove(index));
            } else if let Some(index) = project_agents.iter().position(is_pinned) {
                pinned.push(project_agents.remove(index));
            }
        }
        let insert_at = agents.len().min(1);
        agents.splice(insert_at..insert_at, pinned);
        let active_label = self.agent_filter.as_ref().map(|filter| match filter {
            AgentFilter::Agent(agent_id) if agent_id == &*ZED_AGENT_ID => {
                SharedString::from("Zed Agent")
            }
            AgentFilter::Agent(agent_id) => SharedString::from(agent_id.to_string()),
            AgentFilter::TaskAgent(command) => self
                .task_agent_named(command)
                .map(|agent| SharedString::from(agent.name.clone()))
                .unwrap_or_else(|| command.clone()),
        });

        let header = h_flex()
            .id("agents-section-header")
            .px_3()
            .pt_2()
            .pb_1()
            .gap_1()
            .cursor_pointer()
            .child(
                Label::new("AGENTES")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                Icon::new(if collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(IconSize::XSmall)
                .color(Color::Muted),
            )
            .when_some(active_label.filter(|_| collapsed), |this, label| {
                this.child(div().flex_1()).child(
                    Label::new(label)
                        .size(LabelSize::XSmall)
                        .color(Color::Accent),
                )
            })
            .on_click(cx.listener(|this, _, _window, cx| {
                this.agents_section_collapsed = !this.agents_section_collapsed;
                this.serialize(cx);
                cx.notify();
            }));

        let mut section = v_flex()
            .flex_none()
            .pb_1()
            .border_b_1()
            .border_color(colors.border_variant)
            .child(header);
        if !collapsed {
            for (index, row) in agents.into_iter().enumerate() {
                section =
                    section.child(self.render_agent_row(("agent-row", index), row, window, cx));
            }
            if !project_agents.is_empty() {
                section = section.child(
                    div().px_3().pt_2().pb_0p5().child(
                        Label::new("DO PROJETO")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                );
                for (index, row) in project_agents.into_iter().enumerate() {
                    section = section.child(self.render_agent_row(
                        ("project-agent-row", index),
                        row,
                        window,
                        cx,
                    ));
                }
            }
        }
        if let Some(filter) = self.agent_filter.clone() {
            section = section.child(self.render_filter_banner(filter, cx));
        }
        section.into_any_element()
    }

    fn render_filter_banner(&self, filter: AgentFilter, cx: &mut Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let shown = self
            .contents
            .entries
            .iter()
            .filter(|entry| matches!(entry, ListEntry::Thread(thread) if thread.draft.is_none()))
            .count();
        let total = self.agent_thread_counts(cx).total;
        let name: SharedString = match &filter {
            AgentFilter::TaskAgent(command) => self
                .task_agent_named(command)
                .map(|agent| format!("Nova thread com {}", agent.name).into())
                .unwrap_or_else(|| format!("Nova thread com /{command}").into()),
            AgentFilter::Agent(agent_id) if agent_id == &*ZED_AGENT_ID => {
                "Nova thread com o Zed Agent".into()
            }
            AgentFilter::Agent(agent_id) => format!("Nova thread com {agent_id}").into(),
        };
        h_flex()
            .mx_2()
            .mt_1()
            .px_2()
            .py_1()
            .gap_1()
            .rounded_md()
            .bg(colors.element_selected)
            .child(
                Label::new(format!("{shown} de {total} threads"))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(div().flex_1())
            .child(
                IconButton::new("agent-filter-new-thread", IconName::Plus)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text(name))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.new_thread_with_agent(Some(filter.clone()), window, cx);
                    })),
            )
            .child(
                IconButton::new("agent-filter-clear", IconName::Close)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Mostrar todos os agentes"))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.set_agent_filter(None, cx);
                    })),
            )
            .into_any_element()
    }

    fn render_agent_row(
        &self,
        id: (&'static str, usize),
        row: AgentRow,
        _window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();
        let selected = self.agent_filter == row.filter;
        let empty = row.thread_count == 0 && row.filter.is_some();
        let element_id = ElementId::from((id.0, id.1));
        let group = SharedString::from(format!("{}-{}", id.0, id.1));
        let icon = match &row.external_icon {
            Some(path) => Icon::from_external_svg(path.clone()),
            None => Icon::new(row.icon),
        }
        .size(IconSize::Small)
        .color(if selected {
            Color::Accent
        } else {
            Color::Muted
        });
        let filter_for_click = row.filter.clone();
        let filter_for_new = row.filter.clone();
        let filter_for_tab = row.filter.clone();
        let tooltip = match &row.filter {
            None => "Nova thread".to_string(),
            Some(_) => format!("Nova thread com {}", row.name),
        };

        let line = h_flex()
            .id(element_id.clone())
            .group(group.clone())
            .mx_1()
            .px_2()
            .h(px(26.))
            .gap_2()
            .rounded_md()
            .cursor_pointer()
            .when(selected, |this| this.bg(colors.element_selected))
            .hover(|this| this.bg(colors.element_hover))
            .child(icon)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1p5()
                    .child(
                        Label::new(row.name.clone())
                            .size(LabelSize::Small)
                            .color(if empty { Color::Muted } else { Color::Default })
                            .single_line()
                            .truncate(),
                    )
                    .when_some(row.command.clone(), |this, command| {
                        this.child(
                            Label::new(format!("/{command}"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .buffer_font(cx),
                        )
                    }),
            )
            .when(
                row.filter
                    .as_ref()
                    .is_some_and(|filter| self.pinned_agents.contains(filter)),
                |this| {
                    this.child(
                        Icon::new(IconName::Pin)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                },
            )
            .when(row.running, |this| {
                this.child(div().size(px(6.)).rounded_full().bg(colors.text_accent))
            })
            .child(
                div().visible_on_hover(group.clone()).child(
                    IconButton::new(SharedString::from(format!("{group}-new")), IconName::Plus)
                        .icon_size(IconSize::XSmall)
                        .tooltip(Tooltip::text(tooltip))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.new_thread_with_agent(filter_for_new.clone(), window, cx);
                        })),
                ),
            )
            .child(
                div().visible_on_hover(group.clone()).child(
                    IconButton::new(
                        SharedString::from(format!("{group}-tab")),
                        IconName::ArrowUpRight,
                    )
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text("Abrir em nova aba"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.open_agent_in_new_tab(filter_for_tab.clone(), window, cx);
                    })),
                ),
            )
            .child(
                Label::new(row.thread_count.to_string())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .on_click(cx.listener(move |this, _, _window, cx| {
                let filter = if this.agent_filter == filter_for_click {
                    None
                } else {
                    filter_for_click.clone()
                };
                this.set_agent_filter(filter, cx);
            }));

        let Some(filter) = row.filter else {
            return line.into_any_element();
        };
        let sidebar = cx.entity().downgrade();
        let name = row.name.clone();
        let is_pinned = self.pinned_agents.contains(&filter);
        right_click_menu(element_id)
            .trigger(move |_, _, _| line)
            .menu(move |window, cx| {
                let sidebar = sidebar.clone();
                let filter = filter.clone();
                let name = name.clone();
                ContextMenu::build(window, cx, move |menu, _window, _cx| {
                    let new_sidebar = sidebar.clone();
                    let new_filter = filter.clone();
                    let resume_sidebar = sidebar.clone();
                    let resume_filter = filter.clone();
                    let tab_sidebar = sidebar.clone();
                    let tab_filter = filter.clone();
                    let show_sidebar = sidebar.clone();
                    let show_filter = filter.clone();
                    let configure_sidebar = sidebar.clone();
                    let configure_filter = filter.clone();
                    let pin_sidebar = sidebar.clone();
                    let pin_filter = filter.clone();
                    menu.entry(
                        format!("Nova thread com {name}"),
                        None,
                        move |window, cx| {
                            new_sidebar
                                .update(cx, |sidebar, cx| {
                                    sidebar.new_thread_with_agent(
                                        Some(new_filter.clone()),
                                        window,
                                        cx,
                                    )
                                })
                                .log_err();
                        },
                    )
                    .entry("Abrir em nova aba", None, move |window, cx| {
                        tab_sidebar
                            .update(cx, |sidebar, cx| {
                                sidebar.open_agent_in_new_tab(Some(tab_filter.clone()), window, cx)
                            })
                            .log_err();
                    })
                    .entry("Retomar última thread", None, move |window, cx| {
                        resume_sidebar
                            .update(cx, |sidebar, cx| {
                                sidebar.resume_last_thread(&resume_filter, window, cx)
                            })
                            .log_err();
                    })
                    .entry("Mostrar só as threads dele", None, move |_window, cx| {
                        show_sidebar
                            .update(cx, |sidebar, cx| {
                                sidebar.set_agent_filter(Some(show_filter.clone()), cx)
                            })
                            .log_err();
                    })
                    .entry(
                        if is_pinned {
                            "Desafixar"
                        } else {
                            "Fixar no topo"
                        },
                        None,
                        move |_window, cx| {
                            pin_sidebar
                                .update(cx, |sidebar, cx| {
                                    sidebar.toggle_pinned_agent(&pin_filter, cx)
                                })
                                .log_err();
                        },
                    )
                    .separator()
                    .entry("Configurar agente…", None, move |window, cx| {
                        configure_sidebar
                            .update(cx, |sidebar, cx| {
                                sidebar.configure_agent(&configure_filter, window, cx)
                            })
                            .log_err();
                    })
                })
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_ui::ThreadId;
    use chrono::Utc;
    use project::WorktreePaths;

    fn metadata(agent_id: AgentId, task_agent: Option<&str>) -> ThreadMetadata {
        ThreadMetadata {
            thread_id: ThreadId::new(),
            session_id: None,
            agent_id,
            title: None,
            title_override: None,
            updated_at: Utc::now(),
            created_at: None,
            interacted_at: None,
            worktree_paths: WorktreePaths::default(),
            remote_connection: None,
            archived: false,
            task_agent: task_agent.map(SharedString::from),
        }
    }

    #[test]
    fn test_agent_filter_matches() {
        let zed_thread = metadata(ZED_AGENT_ID.clone(), None);
        let review_thread = metadata(ZED_AGENT_ID.clone(), Some("revisar"));
        let claude_thread = metadata(AgentId::new("claude-code"), None);

        let zed = AgentFilter::Agent(ZED_AGENT_ID.clone());
        assert!(zed.matches(&zed_thread));
        assert!(zed.matches(&review_thread));
        assert!(!zed.matches(&claude_thread));

        let review = AgentFilter::TaskAgent("revisar".into());
        assert!(review.matches(&review_thread));
        assert!(!review.matches(&zed_thread));
        assert!(!review.matches(&claude_thread));
    }
}
