//! Agent tabs: threads pinned to a strip of their own below the dock's tabs, each kept alive
//! while it has a tab. Opening a thread from anywhere else replaces the active tab's thread,
//! unless the thread already has a tab, which is then activated.

use super::AgentPanel;
use crate::{
    Agent, AgentInitialContent, AgentThreadSource, NewExternalAgentThread, ThreadId,
    thread_metadata_store::ThreadMetadataStore,
};
use agent::ZED_AGENT_ID;
use agent_client_protocol::schema::v2 as acp;
use gpui::{AnyElement, App, Context, SharedString, Window, actions};
use task_agents::TaskAgent;
use ui::{ContextMenu, PopoverMenu, Tooltip, prelude::*};
use util::ResultExt as _;

actions!(
    agent,
    [
        /// Opens a new agent tab with the agent of the active one.
        NewAgentThreadTab,
        /// Closes the active agent tab, keeping its thread in the history.
        CloseAgentThreadTab,
        /// Activates the next agent tab.
        ActivateNextAgentThreadTab,
        /// Activates the previous agent tab.
        ActivatePreviousAgentThreadTab,
    ]
);

/// Below this many tabs the strip is hidden: one tab is just the panel.
const MIN_VISIBLE_TABS: usize = 2;

struct TabInfo {
    label: SharedString,
    title: SharedString,
    icon: IconName,
    external_icon: Option<SharedString>,
    is_task_agent: bool,
}

impl AgentPanel {
    pub(super) fn has_agent_tabs(&self) -> bool {
        self.agent_tabs.len() >= MIN_VISIBLE_TABS
    }

    /// Points the tabs at the thread that just became active.
    pub(super) fn sync_agent_tabs(&mut self, cx: &mut Context<Self>) {
        let Some(thread_id) = self.active_thread_id(cx) else {
            return;
        };
        let opening = std::mem::take(&mut self.opening_agent_tab);
        if let Some(index) = self.agent_tabs.iter().position(|id| *id == thread_id) {
            self.active_agent_tab = index;
        } else if opening {
            self.agent_tabs.push(thread_id);
            self.active_agent_tab = self.agent_tabs.len() - 1;
        } else if let Some(slot) = self.agent_tabs.get_mut(self.active_agent_tab) {
            *slot = thread_id;
        }
        self.serialize(cx);
        cx.notify();
    }

    /// Makes the next thread that becomes active open in a tab of its own.
    pub fn prepare_new_agent_tab(&mut self, cx: &App) {
        if self.agent_tabs.is_empty()
            && let Some(thread_id) = self.active_thread_id(cx)
        {
            self.agent_tabs.push(thread_id);
            self.active_agent_tab = 0;
        }
        self.opening_agent_tab = true;
    }

    pub(super) fn forget_agent_tab(&mut self, thread_id: ThreadId) {
        let Some(index) = self.agent_tabs.iter().position(|id| *id == thread_id) else {
            return;
        };
        self.agent_tabs.remove(index);
        if self.agent_tabs.len() < MIN_VISIBLE_TABS {
            self.agent_tabs.clear();
            self.active_agent_tab = 0;
        } else if self.active_agent_tab >= self.agent_tabs.len() {
            self.active_agent_tab = self.agent_tabs.len() - 1;
        }
    }

    pub(super) fn is_in_agent_tab(&self, thread_id: &ThreadId) -> bool {
        self.agent_tabs.contains(thread_id)
    }

    pub(super) fn activate_agent_tab(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(thread_id) = self.agent_tabs.get(index).copied() else {
            return;
        };
        self.active_agent_tab = index;
        if self.active_thread_id(cx) == Some(thread_id) {
            cx.notify();
            return;
        }
        let in_memory = self.retained_threads.contains_key(&thread_id)
            || self
                .draft_thread
                .as_ref()
                .is_some_and(|draft| draft.read(cx).thread_id == thread_id);
        if in_memory {
            self.activate_retained_thread(thread_id, true, window, cx);
            return;
        }
        let metadata = ThreadMetadataStore::try_global(cx)
            .and_then(|store| store.read(cx).entry(thread_id).cloned());
        match metadata {
            Some(metadata) => self.load_agent_thread(
                Agent::from(metadata.agent_id.clone()),
                thread_id,
                Some(metadata.folder_paths().clone()),
                metadata.title(),
                true,
                AgentThreadSource::AgentPanel,
                window,
                cx,
            ),
            // An empty draft is dropped when it loses focus; the tab gets a new one.
            None => self.activate_new_thread(true, AgentThreadSource::AgentPanel, window, cx),
        }
    }

    pub(super) fn close_agent_tab(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if index >= self.agent_tabs.len() {
            return;
        }
        let was_active = index == self.active_agent_tab;
        self.agent_tabs.remove(index);
        if self.agent_tabs.len() < MIN_VISIBLE_TABS {
            let remaining = self.agent_tabs.first().copied();
            self.agent_tabs.clear();
            self.active_agent_tab = 0;
            if was_active && let Some(thread_id) = remaining {
                self.agent_tabs.push(thread_id);
                self.activate_agent_tab(0, window, cx);
                self.agent_tabs.clear();
            }
        } else {
            if index < self.active_agent_tab || self.active_agent_tab >= self.agent_tabs.len() {
                self.active_agent_tab = self.active_agent_tab.saturating_sub(1);
            }
            if was_active {
                self.activate_agent_tab(self.active_agent_tab, window, cx);
            }
        }
        self.cleanup_retained_threads(cx);
        self.serialize(cx);
        cx.notify();
    }

    pub(super) fn new_agent_thread_tab(
        &mut self,
        _: &NewAgentThreadTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let agent = self
            .active_conversation_view()
            .map(|view| view.read(cx).agent_key().clone())
            .unwrap_or_else(|| self.selected_agent.clone());
        let task_agent = self.active_native_agent_thread(cx).and_then(|thread| {
            thread
                .read(cx)
                .task_agent()
                .map(|agent| agent.command.clone())
        });
        self.prepare_new_agent_tab(cx);
        match task_agent {
            Some(command) => self.new_task_agent_thread(&command, window, cx),
            None => {
                self.new_external_agent_thread(&NewExternalAgentThread::new(agent.id()), window, cx)
            }
        }
    }

    pub(super) fn close_active_agent_thread_tab(
        &mut self,
        _: &CloseAgentThreadTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.has_agent_tabs() {
            cx.propagate();
            return;
        }
        self.close_agent_tab(self.active_agent_tab, window, cx);
    }

    pub(super) fn activate_next_agent_thread_tab(
        &mut self,
        _: &ActivateNextAgentThreadTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_agent_tabs() {
            let next = (self.active_agent_tab + 1) % self.agent_tabs.len();
            self.activate_agent_tab(next, window, cx);
        }
    }

    pub(super) fn activate_previous_agent_thread_tab(
        &mut self,
        _: &ActivatePreviousAgentThreadTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.has_agent_tabs() {
            let count = self.agent_tabs.len();
            let previous = (self.active_agent_tab + count - 1) % count;
            self.activate_agent_tab(previous, window, cx);
        }
    }

    /// A Zed Agent thread whose composer starts with `/command`, which hands the thread to the
    /// task agent on send.
    pub(super) fn new_task_agent_thread(
        &mut self,
        command: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.external_thread(
            Some(Agent::NativeAgent),
            None,
            None,
            None,
            Some(AgentInitialContent::ContentBlock {
                blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(format!(
                    "/{command} "
                )))],
                auto_submit: false,
            }),
            true,
            AgentThreadSource::AgentPanel,
            window,
            cx,
        );
    }

    /// Reloads the project's task agents for the new-tab menu.
    pub(super) fn reload_tab_task_agents(&mut self, cx: &mut Context<Self>) {
        let roots = self
            .project
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
        let fs = self.fs.clone();
        self._tab_task_agents_load = cx.spawn(async move |this, cx| {
            let (agents, _) = task_agents::load_task_agents(fs, roots).await;
            this.update(cx, |this, cx| {
                this.tab_task_agents = agents;
                cx.notify();
            })
            .log_err();
        });
    }

    fn tab_info(&self, thread_id: ThreadId, cx: &App) -> TabInfo {
        let metadata = ThreadMetadataStore::try_global(cx)
            .and_then(|store| store.read(cx).entry(thread_id).cloned());
        let conversation_view = self
            .conversation_view_for_id(&thread_id, cx)
            .cloned()
            .or_else(|| {
                self.draft_thread
                    .clone()
                    .filter(|draft| draft.read(cx).thread_id == thread_id)
            });
        let agent = conversation_view
            .as_ref()
            .map(|view| view.read(cx).agent_key().clone())
            .or_else(|| {
                metadata
                    .as_ref()
                    .map(|metadata| Agent::from(metadata.agent_id.clone()))
            })
            .unwrap_or(Agent::NativeAgent);
        let task_agent: Option<std::sync::Arc<TaskAgent>> = conversation_view
            .as_ref()
            .and_then(|view| view.read(cx).as_native_thread(cx))
            .and_then(|thread| thread.read(cx).task_agent().cloned());
        let title = conversation_view
            .as_ref()
            .and_then(|view| view.read(cx).root_thread(cx))
            .and_then(|thread| thread.read(cx).title())
            .or_else(|| metadata.as_ref().and_then(|metadata| metadata.title()))
            .unwrap_or_else(|| crate::DEFAULT_THREAD_TITLE.into());

        let store = self.project.read(cx).agent_server_store().read(cx);
        let agent_name = match &agent {
            Agent::Custom { id, .. } => store
                .agent_display_name(id)
                .unwrap_or_else(|| agent.label()),
            _ => agent.label(),
        };
        let external_icon = match &agent {
            Agent::Custom { id, .. } => store.agent_icon(id),
            _ => None,
        };
        let icon = match &agent {
            Agent::NativeAgent => IconName::ZedAgent,
            _ => IconName::SquareDot,
        };
        match task_agent {
            Some(task_agent) => TabInfo {
                label: task_agent.name.clone().into(),
                title,
                icon: IconName::UserGroup,
                external_icon: None,
                is_task_agent: true,
            },
            None => {
                let task_command = metadata.and_then(|metadata| metadata.task_agent);
                TabInfo {
                    label: match &task_command {
                        Some(command) => format!("/{command}").into(),
                        None => agent_name,
                    },
                    title,
                    icon: if task_command.is_some() {
                        IconName::UserGroup
                    } else {
                        icon
                    },
                    external_icon: external_icon.filter(|_| task_command.is_none()),
                    is_task_agent: task_command.is_some(),
                }
            }
        }
    }

    pub(super) fn render_agent_tabs(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.has_agent_tabs() {
            return None;
        }
        let colors = cx.theme().colors();
        let mut strip = h_flex()
            .id("agent-tabs")
            .flex_1()
            .min_w_0()
            .gap_0p5()
            .overflow_x_scroll();
        for (index, thread_id) in self.agent_tabs.iter().copied().enumerate() {
            let info = self.tab_info(thread_id, cx);
            let active = index == self.active_agent_tab;
            let group = SharedString::from(format!("agent-tab-{index}"));
            let icon = match info.external_icon {
                Some(path) => Icon::from_external_svg(path),
                None => Icon::new(info.icon),
            }
            .size(IconSize::Small)
            .color(if active || info.is_task_agent {
                Color::Accent
            } else {
                Color::Muted
            });
            let tooltip = format!("{} · {}", info.label, info.title);
            strip = strip.child(
                h_flex()
                    .id(("agent-tab", index))
                    .group(group.clone())
                    .flex_shrink_0()
                    .h(px(26.))
                    .pl_2()
                    .pr_1()
                    .gap_1p5()
                    .rounded_md()
                    .cursor_pointer()
                    .when(active, |this| {
                        this.bg(colors.tab_active_background)
                            .border_1()
                            .border_color(colors.border)
                    })
                    .when(!active, |this| {
                        this.hover(|this| this.bg(colors.element_hover))
                    })
                    .child(icon)
                    .child(
                        Label::new(info.label)
                            .size(LabelSize::Small)
                            .color(if active { Color::Default } else { Color::Muted })
                            .single_line()
                            .truncate(),
                    )
                    .max_w(px(if active { 200. } else { 140. }))
                    .child(
                        div()
                            .when(!active, |this| this.visible_on_hover(group.clone()))
                            .child(
                                IconButton::new(("close-agent-tab", index), IconName::Close)
                                    .icon_size(IconSize::XSmall)
                                    .tooltip(Tooltip::text("Fechar aba"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.close_agent_tab(index, window, cx);
                                    })),
                            ),
                    )
                    .tooltip(Tooltip::text(tooltip))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.activate_agent_tab(index, window, cx);
                    })),
            );
        }

        Some(
            h_flex()
                .flex_none()
                .w_full()
                .h(px(34.))
                .px_1p5()
                .gap_1()
                .bg(colors.tab_bar_background)
                .border_b_1()
                .border_color(colors.border_variant)
                .child(strip)
                .child(self.render_new_agent_tab_menu(cx))
                .into_any_element(),
        )
    }

    fn render_new_agent_tab_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let panel = cx.entity().downgrade();
        let store = self.project.read(cx).agent_server_store().read(cx);
        let external = store
            .external_agents()
            .map(|id| {
                (
                    id.clone(),
                    store
                        .agent_display_name(id)
                        .unwrap_or_else(|| SharedString::from(id.to_string())),
                    store.agent_icon(id),
                )
            })
            .collect::<Vec<_>>();
        let task_agents = self
            .tab_task_agents
            .iter()
            .map(|agent| (agent.command.clone(), agent.name.clone()))
            .collect::<Vec<_>>();
        PopoverMenu::new("new-agent-tab-menu")
            .trigger_with_tooltip(
                IconButton::new("new-agent-tab", IconName::Plus).icon_size(IconSize::Small),
                Tooltip::text("Nova aba"),
            )
            .anchor(gpui::Anchor::TopRight)
            .menu(move |window, cx| {
                let panel = panel.clone();
                let external = external.clone();
                let task_agents = task_agents.clone();
                Some(ContextMenu::build(
                    window,
                    cx,
                    move |mut menu, _window, _cx| {
                        menu = menu.header("Nova aba");
                        let open_with =
                            |agent_id: project::AgentId, panel: gpui::WeakEntity<AgentPanel>| {
                                move |window: &mut Window, cx: &mut App| {
                                    panel
                                        .update(cx, |panel, cx| {
                                            panel.prepare_new_agent_tab(cx);
                                            panel.new_external_agent_thread(
                                                &NewExternalAgentThread::new(agent_id.clone()),
                                                window,
                                                cx,
                                            );
                                        })
                                        .log_err();
                                }
                            };
                        menu = menu.item(
                            ui::ContextMenuEntry::new("Zed Agent")
                                .icon(IconName::ZedAgent)
                                .icon_color(Color::Muted)
                                .handler(open_with(ZED_AGENT_ID.clone(), panel.clone())),
                        );
                        for (agent_id, name, icon) in external {
                            let entry = ui::ContextMenuEntry::new(name)
                                .icon_color(Color::Muted)
                                .handler(open_with(agent_id, panel.clone()));
                            menu = menu.item(match icon {
                                Some(path) => entry.custom_icon_svg(path),
                                None => entry.icon(IconName::SquareDot),
                            });
                        }
                        if !task_agents.is_empty() {
                            menu = menu.separator().header("Do projeto");
                            for (command, name) in task_agents {
                                let panel = panel.clone();
                                menu = menu.item(
                                    ui::ContextMenuEntry::new(format!("{name}  /{command}"))
                                        .icon(IconName::UserGroup)
                                        .icon_color(Color::Muted)
                                        .handler(move |window, cx| {
                                            panel
                                                .update(cx, |panel, cx| {
                                                    panel.prepare_new_agent_tab(cx);
                                                    panel.new_task_agent_thread(
                                                        &command, window, cx,
                                                    );
                                                })
                                                .log_err();
                                        }),
                                );
                            }
                        }
                        let manage_panel = panel;
                        menu.separator()
                            .entry("Gerenciar agentes…", None, move |window, cx| {
                                if let Some(workspace) = manage_panel
                                    .upgrade()
                                    .and_then(|panel| panel.read(cx).workspace.upgrade())
                                {
                                    workspace.update(cx, |workspace, cx| {
                                        crate::task_agents_view::open(workspace, window, cx)
                                    });
                                }
                            })
                    },
                ))
            })
    }
}
