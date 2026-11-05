//! Agent tabs: threads pinned to a strip of their own below the dock's tabs, each kept alive
//! while it has a tab. Opening a thread from anywhere else replaces the active tab's thread,
//! unless the thread already has a tab, which is then activated.

use super::AgentPanel;
use crate::{
    Agent, AgentInitialContent, AgentThreadSource, NewExternalAgentThread, ThreadId,
    thread_metadata_store::ThreadMetadataStore,
};
use agent_client_protocol::schema::v2 as acp;
use gpui::{AnyElement, App, Context, SharedString, Window, actions};
use i18n::t;
use task_agents::TaskAgent;
use ui::{Tooltip, prelude::*};
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

    /// A Asylum Agent thread whose composer starts with `/command`, which hands the thread to the
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

    /// Starting a thread from the panel's "+" never replaces a conversation.
    pub(super) fn prepare_tab_for_new_thread(&mut self, cx: &App) {
        if self.active_thread_has_messages(cx) {
            self.prepare_new_agent_tab(cx);
        }
    }

    fn render_agent_tab(
        &self,
        index: usize,
        info: TabInfo,
        active: bool,
        closable: bool,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let colors = cx.theme().colors();
        let group = SharedString::from(format!("agent-tab-{index}"));
        let icon = match info.external_icon {
            Some(path) => Icon::from_external_svg(path),
            None => Icon::new(info.icon),
        }
        .size(IconSize::Small)
        .color(if info.is_task_agent {
            Color::Accent
        } else if active {
            Color::Default
        } else {
            Color::Muted
        });
        let tooltip = format!("{} · {}", info.label, info.title);
        h_flex()
            .id(("agent-tab", index))
            .group(group.clone())
            .flex_shrink_0()
            .h(px(26.))
            .pl(px(10.))
            .pr(if closable { px(4.) } else { px(10.) })
            .gap_1p5()
            .rounded(px(6.))
            .border_1()
            .border_color(gpui::transparent_black())
            .when(active, |this| {
                this.bg(colors.element_selected)
                    .border_color(colors.border_selected)
            })
            .when(!active, |this| {
                this.cursor_pointer()
                    .hover(|this| this.bg(colors.element_hover))
            })
            .max_w(px(if active { 200. } else { 140. }))
            .child(icon)
            .child(
                Label::new(info.label)
                    .size(LabelSize::Small)
                    .weight(if active {
                        gpui::FontWeight::SEMIBOLD
                    } else {
                        gpui::FontWeight::MEDIUM
                    })
                    .color(if active { Color::Default } else { Color::Muted })
                    .single_line()
                    .truncate(),
            )
            .tooltip(Tooltip::text(tooltip))
            .when(closable, |this| {
                this.child(
                    div()
                        .when(!active, |this| this.visible_on_hover(group.clone()))
                        .child(
                            IconButton::new(("close-agent-tab", index), IconName::Close)
                                .icon_size(IconSize::XSmall)
                                .tooltip(Tooltip::text(t!("agent_ui.agent_tabs.close_tab")))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.close_agent_tab(index, window, cx);
                                })),
                        ),
                )
            })
    }

    /// Shown even with a single tab, because the panel's actions (threads, "+") live here.
    pub(super) fn render_agent_tabs(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.has_open_project(cx) {
            return None;
        }
        let new_thread_menu = self.render_new_thread_menu(cx);
        let colors = cx.theme().colors();
        let mut strip = h_flex()
            .id("agent-tabs")
            .min_w_0()
            .h(px(32.))
            .p(px(3.))
            .gap_0p5()
            .rounded(px(8.))
            .border_1()
            .border_color(colors.border)
            .bg(colors.editor_background)
            .overflow_x_scroll();

        if self.has_agent_tabs() {
            for (index, thread_id) in self.agent_tabs.iter().copied().enumerate() {
                let info = self.tab_info(thread_id, cx);
                let active = index == self.active_agent_tab;
                strip = strip.child(
                    self.render_agent_tab(index, info, active, true, cx)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.activate_agent_tab(index, window, cx);
                        })),
                );
            }
        } else {
            let info = match self.active_thread_id(cx) {
                Some(thread_id) => self.tab_info(thread_id, cx),
                None => TabInfo {
                    label: if self.active_terminal_id().is_some() {
                        "Terminal".into()
                    } else {
                        self.selected_agent.label()
                    },
                    title: SharedString::default(),
                    icon: if self.active_terminal_id().is_some() {
                        IconName::Terminal
                    } else {
                        self.selected_agent.icon().unwrap_or(IconName::ZedAgent)
                    },
                    external_icon: None,
                    is_task_agent: false,
                },
            };
            strip = strip.child(self.render_agent_tab(0, info, true, false, cx));
        }

        // The threads sidebar is where past threads live, so it stands in for the history view.
        let history_button = IconButton::new("toggle-thread-history", IconName::History)
            .icon_size(IconSize::Small)
            .tooltip(|_, cx| {
                Tooltip::for_action(
                    t!("agent_ui.agent_panel.threads"),
                    &workspace::ToggleWorkspaceSidebar,
                    cx,
                )
            })
            .on_click(|_, window, cx| {
                window.dispatch_action(Box::new(workspace::ToggleWorkspaceSidebar), cx);
            });

        Some(
            h_flex()
                .flex_none()
                .w_full()
                .h(px(44.))
                .px_2()
                .gap_1()
                .bg(colors.tab_bar_background)
                .border_b_1()
                .border_color(colors.border_variant)
                .child(strip)
                .child(div().flex_1())
                .child(history_button)
                .child(new_thread_menu)
                .into_any_element(),
        )
    }
}
