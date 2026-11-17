use std::{path::PathBuf, time::Duration};

use editor::Editor;
use gpui::{
    Action as _, App, Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels, Subscription,
    Task, TaskExt as _, WeakEntity, Window,
};
use ui::{
    StatusTone, destructive_button, prelude::*, primary_button, secondary_button, status_pill,
};
use util::ResultExt as _;
use workspace::{
    OpenOptions, OpenVisible, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::{
    OpenPicker, ToggleFocus,
    chain::{ChainRecord, StepState},
    format_duration, record_pill,
    store::{ExcerptLine, OutputLocation, RunRecord, RunStore},
};

/// The Executions dock: what is running, with its output, restart and stop; the recent runs
/// with their result and, for failures, the lines that matter; and the favorites.
pub struct ExecutionsPanel {
    workspace: WeakEntity<Workspace>,
    store: Entity<RunStore>,
    focus_handle: FocusHandle,
    _ticker: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ExecutionsPanel {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        store: Entity<RunStore>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![cx.observe(&store, |this, _, cx| {
            this.update_ticker(cx);
            cx.notify();
        })];
        let mut this = Self {
            workspace,
            store,
            focus_handle: cx.focus_handle(),
            _ticker: None,
            _subscriptions: subscriptions,
        };
        this.update_ticker(cx);
        this
    }

    /// Redraws every second while something runs, so the elapsed times move.
    fn update_ticker(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let any_running = store.running().next().is_some()
            || store.chains().iter().any(|chain| chain.is_running());
        if !any_running {
            self._ticker = None;
            return;
        }
        if self._ticker.is_some() {
            return;
        }
        self._ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        }));
    }

    fn open_location(
        &self,
        cwd: Option<PathBuf>,
        location: OutputLocation,
        window: &mut Window,
        cx: &mut App,
    ) {
        let path = PathBuf::from(&location.path);
        let path = if path.is_absolute() {
            path
        } else if let Some(cwd) = cwd {
            cwd.join(path)
        } else {
            path
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let open = workspace.update(cx, |workspace, cx| {
            workspace.open_abs_path(
                path,
                OpenOptions {
                    visible: Some(OpenVisible::None),
                    ..OpenOptions::default()
                },
                window,
                cx,
            )
        });
        window
            .spawn(cx, async move |cx| {
                let item = open.await?;
                if let Some(editor) = item.downcast::<Editor>() {
                    editor.update_in(cx, |editor, window, cx| {
                        let point = text::Point::new(
                            location.line.saturating_sub(1),
                            location.column.unwrap_or(1).saturating_sub(1),
                        );
                        editor.go_to_singleton_buffer_point(point, window, cx);
                    })?;
                }
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
    }

    fn render_chain(&self, chain: &ChainRecord, cx: &mut Context<Self>) -> AnyElement {
        let chain_id = chain.id;
        let colors = cx.theme().colors();
        let (pill_label, tone) = if chain.is_running() {
            (i18n::t!("runs.state.running"), StatusTone::Info)
        } else if chain.failed_step().is_some() {
            (i18n::t!("runs.state.failed"), StatusTone::Error)
        } else {
            (i18n::t!("runs.state.ok"), StatusTone::Success)
        };
        let elapsed = chain
            .finished_at
            .unwrap_or_else(std::time::Instant::now)
            .saturating_duration_since(chain.started_at);
        v_flex()
            .w_full()
            .p_3()
            .gap_1p5()
            .rounded(Radius::Lg.rems())
            .border_1()
            .border_color(colors.border_variant)
            .bg(colors.element_background)
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(status_pill(pill_label, tone, cx))
                    .child(
                        Label::new(chain.label.clone())
                            .size(LabelSize::Small)
                            .weight(gpui::FontWeight::SEMIBOLD)
                            .single_line()
                            .truncate(),
                    )
                    .child(div().flex_1())
                    .child(
                        Label::new(format_duration(elapsed))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .children(chain.steps.iter().enumerate().map(|(index, step)| {
                let (icon, color, detail) = match &step.state {
                    StepState::Pending => (IconName::Circle, Color::Muted, None),
                    StepState::Starting | StepState::Waiting => (
                        IconName::LoadCircle,
                        Color::Info,
                        Some(i18n::t!("runs.step.waiting")),
                    ),
                    StepState::Ready => (
                        IconName::Check,
                        Color::Success,
                        Some(i18n::t!("runs.step.ready")),
                    ),
                    StepState::Reused => (
                        IconName::Check,
                        Color::Success,
                        Some(i18n::t!("runs.state.reused")),
                    ),
                    StepState::Done => (IconName::Check, Color::Success, None),
                    StepState::Failed(reason) => {
                        (IconName::XCircle, Color::Error, Some(reason.clone()))
                    }
                    StepState::Skipped => (
                        IconName::Dash,
                        Color::Muted,
                        Some(i18n::t!("runs.step.skipped")),
                    ),
                };
                let terminal = step.terminal.clone();
                h_flex()
                    .id(("chain-step", chain_id * 100 + index))
                    .w_full()
                    .gap_1p5()
                    .child(Icon::new(icon).size(IconSize::XSmall).color(color))
                    .child(
                        Label::new(step.label.clone())
                            .size(LabelSize::XSmall)
                            .single_line()
                            .truncate(),
                    )
                    .when_some(detail, |this, detail| {
                        this.child(
                            Label::new(detail)
                                .size(LabelSize::XSmall)
                                .color(color)
                                .single_line()
                                .truncate(),
                        )
                    })
                    .when_some(terminal, |this, terminal| {
                        this.cursor_pointer()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let Some(terminal) = terminal.upgrade() else {
                                    return;
                                };
                                this.workspace
                                    .update(cx, |workspace, cx| {
                                        crate::store::reveal_terminal(
                                            workspace, &terminal, window, cx,
                                        )
                                    })
                                    .log_err();
                            }))
                    })
            }))
            .when(chain.is_running(), |this| {
                this.child(
                    h_flex().pt_1().child(
                        destructive_button(
                            ("chain-stop", chain_id),
                            i18n::t!("runs.action.stop"),
                            true,
                            cx,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.store
                                .update(cx, |store, cx| store.stop_chain(chain_id, cx));
                        })),
                    ),
                )
            })
            .into_any_element()
    }

    fn render_running(&self, record: &RunRecord, cx: &mut Context<Self>) -> AnyElement {
        let record_id = record.id;
        let colors = cx.theme().colors();
        v_flex()
            .w_full()
            .p_3()
            .gap_2()
            .rounded(Radius::Lg.rems())
            .border_1()
            .border_color(colors.border_variant)
            .bg(colors.element_background)
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(status_pill(
                        i18n::t!("runs.state.running"),
                        StatusTone::Info,
                        cx,
                    ))
                    .child(
                        Label::new(record.label.clone())
                            .size(LabelSize::Small)
                            .weight(gpui::FontWeight::SEMIBOLD)
                            .single_line()
                            .truncate(),
                    )
                    .child(div().flex_1())
                    .child(
                        Label::new(format_duration(record.duration()))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(
                Label::new(record.command_label.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .buffer_font(cx)
                    .single_line()
                    .truncate(),
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        secondary_button(
                            ("run-output", record_id),
                            Some((IconName::Terminal, Color::Muted)),
                            i18n::t!("runs.action.output"),
                            true,
                            cx,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.store
                                    .update(cx, |store, cx| store.reveal(record_id, window, cx));
                            },
                        )),
                    )
                    .child(
                        secondary_button(
                            ("run-restart", record_id),
                            Some((IconName::RotateCw, Color::Muted)),
                            i18n::t!("runs.action.restart"),
                            true,
                            cx,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.store
                                    .update(cx, |store, cx| store.restart(record_id, window, cx));
                            },
                        )),
                    )
                    .child(
                        destructive_button(
                            ("run-stop", record_id),
                            i18n::t!("runs.action.stop"),
                            true,
                            cx,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.store.update(cx, |store, cx| store.stop(record_id, cx));
                        })),
                    ),
            )
            .into_any_element()
    }

    fn render_recent(&self, record: &RunRecord, cx: &mut Context<Self>) -> AnyElement {
        let record_id = record.id;
        let cwd = record.cwd();
        v_flex()
            .w_full()
            .px_2()
            .py_1p5()
            .gap_1()
            .rounded(Radius::Md.rems())
            .hover(|this| this.bg(cx.theme().colors().element_hover))
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        Label::new(record.label.clone())
                            .size(LabelSize::Small)
                            .single_line()
                            .truncate(),
                    )
                    .child(record_pill(record, cx))
                    .child(div().flex_1())
                    .child(
                        Label::new(format_duration(record.duration()))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        IconButton::new(("run-rerun", record_id), IconName::Rerun)
                            .icon_size(IconSize::Small)
                            .tooltip(ui::Tooltip::text(i18n::t!("runs.action.run_again")))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.store
                                    .update(cx, |store, cx| store.restart(record_id, window, cx));
                            })),
                    )
                    .when(record.terminal.upgrade().is_some(), |this| {
                        this.child(
                            IconButton::new(("run-recent-output", record_id), IconName::Terminal)
                                .icon_size(IconSize::Small)
                                .tooltip(ui::Tooltip::text(i18n::t!("runs.action.output")))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.store.update(cx, |store, cx| {
                                        store.reveal(record_id, window, cx)
                                    });
                                })),
                        )
                    }),
            )
            .when(!record.excerpt.is_empty(), |this| {
                this.child(self.render_excerpt(record_id, &record.excerpt, cwd, cx))
            })
            .into_any_element()
    }

    fn render_excerpt(
        &self,
        record_id: usize,
        excerpt: &[ExcerptLine],
        cwd: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let error = Color::Error.color(cx);
        v_flex()
            .w_full()
            .p_2()
            .gap_0p5()
            .rounded(Radius::Md.rems())
            .border_1()
            .border_color(error.opacity(0.2))
            .bg(error.opacity(0.06))
            .children(excerpt.iter().enumerate().map(|(index, line)| {
                let label = Label::new(line.text.clone())
                    .size(LabelSize::XSmall)
                    .buffer_font(cx)
                    .single_line()
                    .truncate();
                match line.location.clone() {
                    Some(location) => {
                        let cwd = cwd.clone();
                        div()
                            .id(("run-excerpt", record_id * 100 + index))
                            .cursor_pointer()
                            .hover(|this| this.underline())
                            .child(label.color(Color::Accent))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_location(cwd.clone(), location.clone(), window, cx);
                            }))
                            .into_any_element()
                    }
                    None => label.color(Color::Muted).into_any_element(),
                }
            }))
            .child(
                h_flex().pt_1().child(
                    secondary_button(
                        ("run-fix-with-agent", record_id),
                        Some((IconName::ZedAgent, Color::Muted)),
                        i18n::t!("runs.action.fix_with_agent"),
                        true,
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        crate::fix_with_agent(&this.store, record_id, window, cx);
                    })),
                ),
            )
            .into_any_element()
    }

    fn render_favorites(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let store = self.store.read(cx);
        let favorites: Vec<(usize, SharedString, SharedString)> = store
            .favorite_slots()
            .into_iter()
            .map(|(slot, key)| {
                let label = store
                    .candidate(&key)
                    .map(|candidate| candidate.label.clone())
                    .unwrap_or_else(|| {
                        key.split_once(':')
                            .map(|(_, label)| SharedString::from(label.to_string()))
                            .unwrap_or_else(|| key.clone())
                    });
                (slot, key, label)
            })
            .collect();
        if favorites.is_empty() {
            return None;
        }
        Some(
            v_flex()
                .gap_0p5()
                .child(section_title(
                    i18n::t!("runs.picker.section_favorites"),
                    None,
                ))
                .children(favorites.into_iter().map(|(slot, key, label)| {
                    h_flex()
                        .id(("run-favorite", slot))
                        .w_full()
                        .px_2()
                        .py_1()
                        .gap_2()
                        .rounded(Radius::Md.rems())
                        .cursor_pointer()
                        .hover(|this| this.bg(cx.theme().colors().element_hover))
                        .child(
                            Icon::new(IconName::StarFilled)
                                .size(IconSize::XSmall)
                                .color(Color::Warning),
                        )
                        .child(Label::new(label).size(LabelSize::Small).truncate())
                        .child(div().flex_1())
                        .child(
                            Label::new(format!("⌃⌥{slot}"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let key = key.clone();
                            this.store
                                .update(cx, |store, cx| store.run_key(key, false, window, cx));
                        }))
                }))
                .into_any_element(),
        )
    }
}

fn section_title(title: SharedString, count: Option<usize>) -> impl IntoElement {
    h_flex()
        .px_2()
        .pt_1()
        .gap_1p5()
        .child(
            Label::new(title)
                .size(LabelSize::XSmall)
                .weight(gpui::FontWeight::SEMIBOLD)
                .color(Color::Muted),
        )
        .when_some(count, |this, count| {
            this.child(
                Label::new(count.to_string())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
        })
}

impl Render for ExecutionsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let running: Vec<usize> = store.running().map(|record| record.id).collect();
        let recent: Vec<usize> = store.recent().map(|record| record.id).collect();
        let chains: Vec<ChainRecord> = store.chains().iter().rev().cloned().collect();
        let mut running_elements: Vec<AnyElement> = chains
            .iter()
            .filter(|chain| chain.is_running())
            .map(|chain| self.render_chain(chain, cx))
            .collect();
        let mut recent_elements: Vec<AnyElement> = chains
            .iter()
            .filter(|chain| !chain.is_running())
            .take(5)
            .map(|chain| self.render_chain(chain, cx))
            .collect();
        running_elements.extend(running.iter().filter_map(|record_id| {
            let record = self.store.read(cx).record(*record_id)?.clone();
            Some(self.render_running(&record, cx))
        }));
        recent_elements.extend(recent.iter().filter_map(|record_id| {
            let record = self.store.read(cx).record(*record_id)?.clone();
            Some(self.render_recent(&record, cx))
        }));
        let favorites = self.render_favorites(cx);
        let is_empty = running_elements.is_empty() && recent_elements.is_empty();
        v_flex()
            .id("executions-panel")
            .key_context("ExecutionsPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .p_2()
            .gap_3()
            .overflow_y_scroll()
            .child(
                h_flex()
                    .w_full()
                    .px_1()
                    .child(
                        Label::new(i18n::t!("runs.panel.title"))
                            .size(LabelSize::Default)
                            .weight(gpui::FontWeight::SEMIBOLD),
                    )
                    .child(div().flex_1())
                    .child(
                        primary_button(
                            "executions-run",
                            Some(IconName::PlayFilled),
                            i18n::t!("runs.action.run"),
                            None,
                            cx,
                        )
                        .on_click(|_, window, cx| {
                            window.dispatch_action(OpenPicker.boxed_clone(), cx);
                        }),
                    ),
            )
            .when(is_empty, |this| {
                this.child(
                    v_flex()
                        .w_full()
                        .p_4()
                        .gap_1()
                        .items_center()
                        .child(
                            Label::new(i18n::t!("runs.panel.empty_title"))
                                .size(LabelSize::Small)
                                .weight(gpui::FontWeight::MEDIUM),
                        )
                        .child(
                            Label::new(i18n::t!("runs.panel.empty_body"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
            })
            .when(!running_elements.is_empty(), |this| {
                this.child(
                    v_flex()
                        .gap_1p5()
                        .child(section_title(
                            i18n::t!("runs.panel.section_running"),
                            Some(running_elements.len()),
                        ))
                        .children(running_elements),
                )
            })
            .when(!recent_elements.is_empty(), |this| {
                this.child(
                    v_flex()
                        .gap_0p5()
                        .child(section_title(i18n::t!("runs.panel.section_recent"), None))
                        .children(recent_elements),
                )
            })
            .children(favorites)
    }
}

impl Focusable for ExecutionsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ExecutionsPanel {}

impl Panel for ExecutionsPanel {
    fn persistent_name() -> &'static str {
        "ExecutionsPanel"
    }

    fn panel_key() -> &'static str {
        "ExecutionsPanel"
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
        px(360.)
    }

    /// No icon: the run zone of the status bar is the way in.
    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        None
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::PlayOutlined)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Runs")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        ToggleFocus.boxed_clone()
    }

    fn activation_priority(&self) -> u32 {
        17
    }
}
