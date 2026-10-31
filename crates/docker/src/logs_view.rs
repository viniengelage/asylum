//! A container's logs as a tab next to the terminals: the last lines, then every new one while
//! it follows, with a level filter.

use crate::{
    ToggleFollow,
    client::{LogLine, LogStream},
    store::{DockerStore, StoreEvent},
};
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, ScrollStrategy,
    SharedString, Subscription, Task, UniformListScrollHandle, px, uniform_list,
};
use std::{collections::VecDeque, ops::Range, sync::LazyLock};
use ui::{ContextMenu, PopoverMenu, PopoverTriggerArea, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    item::{Item, ItemEvent, TabContentParams},
};

const MAX_LINES: usize = 10_000;
const INITIAL_TAIL: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }

    fn color(self) -> Color {
        match self {
            Self::Trace | Self::Debug => Color::Placeholder,
            Self::Info => Color::Info,
            Self::Warn => Color::Warning,
            Self::Error => Color::Error,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LevelFilter {
    All,
    WarningsAndErrors,
    Errors,
}

impl LevelFilter {
    fn label(self) -> &'static str {
        match self {
            Self::All => "Todos os níveis",
            Self::WarningsAndErrors => "Avisos e erros",
            Self::Errors => "Só erros",
        }
    }

    fn accepts(self, level: Option<Level>) -> bool {
        match self {
            Self::All => true,
            Self::WarningsAndErrors => level.is_some_and(|level| level >= Level::Warn),
            Self::Errors => level == Some(Level::Error),
        }
    }
}

struct Line {
    clock: SharedString,
    level: Option<Level>,
    text: SharedString,
}

static LEVEL_PATTERN: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r#"(?i)^\s*\[?(trace|debug|info|warn|warning|error|err|fatal|critical)\]?[\s:|-]+|"level"\s*:\s*"?(trace|debug|info|warn|warning|error|fatal|critical|10|20|30|40|50|60)"?|\blevel=(trace|debug|info|warn|warning|error|fatal)\b|\b(TRACE|DEBUG|INFO|WARN|WARNING|ERROR|FATAL)\b"#,
    )
    .expect("valid level pattern")
});

fn level_from(word: &str) -> Option<Level> {
    Some(match word.to_ascii_lowercase().as_str() {
        "trace" | "10" => Level::Trace,
        "debug" | "20" => Level::Debug,
        "info" | "30" => Level::Info,
        "warn" | "warning" | "40" => Level::Warn,
        "error" | "err" | "fatal" | "critical" | "50" | "60" => Level::Error,
        _ => return None,
    })
}

/// The level written in the line, and the line without it when it led the line.
fn parse_level(text: &str) -> (Option<Level>, &str) {
    let Some(captures) = LEVEL_PATTERN.captures(text) else {
        return (None, text);
    };
    if let Some(leading) = captures.get(1) {
        let rest = captures.get(0).map_or(text, |whole| &text[whole.end()..]);
        return (level_from(leading.as_str()), rest);
    }
    let word = captures
        .get(2)
        .or_else(|| captures.get(3))
        .or_else(|| captures.get(4))
        .map(|word| word.as_str());
    (word.and_then(level_from), text)
}

pub struct LogsView {
    store: Entity<DockerStore>,
    container_id: String,
    name: SharedString,
    lines: VecDeque<Line>,
    visible: Vec<usize>,
    filter: LevelFilter,
    following: bool,
    missed: usize,
    ended: bool,
    error: Option<SharedString>,
    scroll_handle: UniformListScrollHandle,
    focus_handle: FocusHandle,
    stream_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl LogsView {
    fn new(
        store: Entity<DockerStore>,
        container_id: String,
        name: SharedString,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![cx.subscribe(&store, |this, store, event, cx| match event {
            StoreEvent::ContainersChanged => {
                let running = store
                    .read(cx)
                    .container(&this.container_id)
                    .is_some_and(|container| container.state.is_running());
                if this.ended && running {
                    this.ended = false;
                    this.start(0, cx);
                }
                cx.notify();
            }
        })];
        let mut this = Self {
            store,
            container_id,
            name,
            lines: VecDeque::new(),
            visible: Vec::new(),
            filter: LevelFilter::All,
            following: true,
            missed: 0,
            ended: false,
            error: None,
            scroll_handle: UniformListScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            stream_task: Task::ready(()),
            _subscriptions: subscriptions,
        };
        this.start(INITIAL_TAIL, cx);
        this
    }

    fn start(&mut self, tail: usize, cx: &mut Context<Self>) {
        let Some(docker) = self.store.read(cx).docker().cloned() else {
            self.error = Some("O Docker não está conectado.".into());
            return;
        };
        let id = self.container_id.clone();
        self.stream_task = cx.spawn(async move |this, cx| {
            let mut lines = match docker.logs(&id, tail, true).await {
                Ok(lines) => lines,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.error = Some(format!("Os logs não abriram: {error}").into());
                        cx.notify();
                    })
                    .log_err();
                    return;
                }
            };
            while let Some(line) = lines.recv().await {
                let mut batch = vec![line];
                while let Ok(line) = lines.try_recv() {
                    batch.push(line);
                }
                if this.update(cx, |this, cx| this.push(batch, cx)).is_err() {
                    return;
                }
            }
            this.update(cx, |this, cx| {
                this.ended = true;
                cx.notify();
            })
            .log_err();
        });
    }

    fn push(&mut self, batch: Vec<LogLine>, cx: &mut Context<Self>) {
        for line in batch {
            let (level, text) = parse_level(&line.text);
            let level = level.or((line.stream == LogStream::Stderr
                && text.to_ascii_lowercase().contains("error"))
            .then_some(Level::Error));
            self.lines.push_back(Line {
                clock: line.clock().unwrap_or_default().into(),
                level,
                text: text.to_string().into(),
            });
            if !self.following {
                self.missed += 1;
            }
        }
        while self.lines.len() > MAX_LINES {
            self.lines.pop_front();
        }
        self.refilter();
        if self.following {
            self.scroll_to_end();
        }
        cx.notify();
    }

    fn refilter(&mut self) {
        let filter = self.filter;
        self.visible = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| filter.accepts(line.level))
            .map(|(index, _)| index)
            .collect();
    }

    fn scroll_to_end(&mut self) {
        if let Some(last) = self.visible.len().checked_sub(1) {
            self.scroll_handle
                .scroll_to_item(last, ScrollStrategy::Bottom);
        }
    }

    fn toggle_follow(&mut self, cx: &mut Context<Self>) {
        self.following = !self.following;
        if self.following {
            self.missed = 0;
            self.scroll_to_end();
        }
        cx.notify();
    }

    fn set_filter(&mut self, filter: LevelFilter, cx: &mut Context<Self>) {
        self.filter = filter;
        self.refilter();
        self.scroll_to_end();
        cx.notify();
    }

    fn ask_agent(&self, window: &mut Window, cx: &mut App) {
        window.dispatch_action(
            Box::new(zed_actions::agent::MentionLogs {
                id: format!("container:{}", self.container_id),
                title: format!("{} · logs", self.name),
                prompt: Some(
                    "Leia os logs deste container e explique o que está acontecendo e o que \
                     merece atenção."
                        .to_string(),
                ),
                submit: false,
            }),
            cx,
        );
    }

    fn render_line(&self, index: usize, cx: &App) -> AnyElement {
        let Some(line) = self
            .visible
            .get(index)
            .and_then(|index| self.lines.get(*index))
        else {
            return div().into_any_element();
        };
        let text_color = match line.level {
            Some(Level::Warn) => Color::Warning,
            Some(Level::Error) => Color::Error,
            _ => Color::Custom(cx.theme().colors().editor_foreground),
        };
        h_flex()
            .id(("docker-log-line", index))
            .w_full()
            .h(px(19.))
            .gap(px(12.))
            .child(
                Label::new(line.clock.clone())
                    .size(LabelSize::Custom(rems_from_px(12_f32)))
                    .buffer_font(cx)
                    .color(Color::Placeholder),
            )
            .child(
                div()
                    .w(px(44.))
                    .flex_none()
                    .when_some(line.level, |this, level| {
                        this.child(
                            Label::new(level.label())
                                .size(LabelSize::Custom(rems_from_px(12_f32)))
                                .buffer_font(cx)
                                .weight(gpui::FontWeight::SEMIBOLD)
                                .color(level.color()),
                        )
                    }),
            )
            .child(
                Label::new(line.text.clone())
                    .size(LabelSize::Custom(rems_from_px(12_f32)))
                    .buffer_font(cx)
                    .color(text_color)
                    .single_line(),
            )
            .into_any_element()
    }

    fn render_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let accent = Color::Accent.color(cx);
        let following = self.following;
        let missed = self.missed;
        let this = cx.weak_entity();
        h_flex()
            .gap(px(4.))
            .child(
                h_flex()
                    .id("docker-logs-follow")
                    .h(px(24.))
                    .px(px(8.))
                    .gap(px(6.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .when(following, |this| this.bg(accent.opacity(0.16)))
                    .when(!following, |this| {
                        this.border_1()
                            .border_color(colors.border)
                            .bg(colors.panel_background)
                    })
                    .child(div().size(px(6.)).rounded_full().bg(if following {
                        accent
                    } else {
                        colors.border_selected
                    }))
                    .child(
                        Label::new(if following {
                            "Seguindo".to_string()
                        } else if missed > 0 {
                            format!("Pausado · {missed} novas")
                        } else {
                            "Pausado".to_string()
                        })
                        .size(LabelSize::Custom(rems_from_px(11.5_f32)))
                        .weight(gpui::FontWeight::MEDIUM)
                        .color(if following {
                            Color::Accent
                        } else {
                            Color::Muted
                        }),
                    )
                    .tooltip(Tooltip::text(if following {
                        "Pausar a rolagem"
                    } else {
                        "Voltar a seguir"
                    }))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_follow(cx))),
            )
            .child(
                PopoverMenu::new("docker-logs-level")
                    .trigger(PopoverTriggerArea::new(
                        h_flex()
                            .id("docker-logs-level-trigger")
                            .h(px(24.))
                            .px(px(8.))
                            .gap(px(6.))
                            .rounded(px(6.))
                            .border_1()
                            .border_color(colors.border)
                            .bg(colors.panel_background)
                            .hover(|this| this.bg(colors.ghost_element_hover))
                            .child(
                                Icon::new(IconName::Filter)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(self.filter.label())
                                    .size(LabelSize::Custom(rems_from_px(11.5_f32)))
                                    .color(Color::Muted),
                            ),
                    ))
                    .menu(move |window, cx| {
                        let this = this.clone();
                        Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                            for filter in [
                                LevelFilter::All,
                                LevelFilter::WarningsAndErrors,
                                LevelFilter::Errors,
                            ] {
                                let this = this.clone();
                                menu = menu.entry(filter.label(), None, move |_, cx| {
                                    this.update(cx, |this, cx| this.set_filter(filter, cx))
                                        .log_err();
                                });
                            }
                            menu
                        }))
                    }),
            )
            .child(
                IconButton::new("docker-logs-agent", IconName::Sparkle)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text("Perguntar ao Agent sobre estes logs"))
                    .on_click(cx.listener(|this, _, window, cx| this.ask_agent(window, cx))),
            )
            .child(
                IconButton::new("docker-logs-zoom", IconName::Maximize)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text("Maximizar"))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(workspace::ToggleZoom), cx)
                    }),
            )
    }
}

impl Render for LogsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let line_count = self.visible.len();
        let is_empty = line_count == 0;
        v_flex()
            .key_context("DockerLogs")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &ToggleFollow, _, cx| this.toggle_follow(cx)))
            .size_full()
            .relative()
            .bg(cx.theme().colors().panel_background)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .pt(px(10.))
                    .px(px(16.))
                    .map(|this| {
                        if let Some(error) = &self.error {
                            this.child(
                                Label::new(error.clone())
                                    .size(LabelSize::Small)
                                    .color(Color::Error),
                            )
                        } else if is_empty {
                            this.child(
                                Label::new(if self.ended {
                                    "O container não escreveu nada."
                                } else {
                                    "Esperando os logs…"
                                })
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            )
                        } else {
                            this.child(
                                uniform_list(
                                    "docker-log-lines",
                                    line_count,
                                    cx.processor(|this, range: Range<usize>, _window, cx| {
                                        range.map(|index| this.render_line(index, cx)).collect()
                                    }),
                                )
                                .track_scroll(&self.scroll_handle)
                                .size_full(),
                            )
                        }
                    }),
            )
            .when(self.ended && !is_empty, |this| {
                this.child(
                    h_flex().px(px(16.)).py_1().child(
                        Label::new("O container parou; os logs voltam quando ele subir.")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
            })
            .child(
                h_flex()
                    .absolute()
                    .top(px(6.))
                    .right(px(12.))
                    .child(self.render_controls(cx)),
            )
    }
}

impl Focusable for LogsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for LogsView {}

impl Item for LogsView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!("{} · logs", self.name).into()
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        let live = self.following
            && !self.ended
            && self
                .store
                .read(cx)
                .container(&self.container_id)
                .is_some_and(|container| container.state.is_running());
        h_flex()
            .gap(px(6.))
            .child(
                Label::new(self.tab_content_text(0, cx))
                    .color(params.text_color())
                    .when(params.selected, |label| {
                        label.weight(gpui::FontWeight::BOLD)
                    }),
            )
            .when(live, |this| {
                this.child(
                    div()
                        .size(px(6.))
                        .rounded_full()
                        .bg(Color::Success.color(cx)),
                )
            })
            .into_any_element()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Reader).color(Color::Muted))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

/// Opens the container's logs next to the terminals, or brings back the tab already following
/// it.
pub fn open(
    workspace: &mut Workspace,
    container_id: String,
    name: SharedString,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace
        .items_of_type::<LogsView>(cx)
        .find(|view| view.read(cx).container_id == container_id);
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let store = DockerStore::global(cx);
    let view = cx.new(|cx| LogsView::new(store, container_id, name, cx));
    match workspace.panel::<terminal_view::terminal_panel::TerminalPanel>(cx) {
        Some(terminal_panel) => {
            workspace.focus_panel::<terminal_view::terminal_panel::TerminalPanel>(window, cx);
            terminal_panel.update(cx, |panel, cx| {
                panel.add_item(Box::new(view), window, cx);
            });
        }
        None => workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_levels_in_common_formats() {
        assert_eq!(
            parse_level("INFO [boot] trix-api ouvindo"),
            (Some(Level::Info), "[boot] trix-api ouvindo")
        );
        assert_eq!(
            parse_level("[warn]: redis lento"),
            (Some(Level::Warn), "redis lento")
        );
        assert_eq!(
            parse_level(r#"{"level":50,"msg":"falhou"}"#).0,
            Some(Level::Error)
        );
        assert_eq!(parse_level("ts=1 level=debug msg=x").0, Some(Level::Debug));
        assert_eq!(parse_level("GET /health 200").0, None);
    }
}
