//! Brings the connections DBeaver and TablePlus saved into the dock. Nothing connects: the
//! checked rows only become saved connections.

use crate::{
    connection::{Environment, SavedConnection, Scope},
    discovery::{self, Imported, PasswordSource, Tool},
    panel::{DatabasePanel, environment_select, scope_control},
};
use collections::HashMap;
use fs::Fs;
use gpui::{
    AnyElement, DismissEvent, EventEmitter, FocusHandle, Focusable, FontWeight, PathPromptOptions,
    Task, WeakEntity, px,
};
use i18n::t;
use std::{path::PathBuf, sync::Arc};
use ui::{
    EmptyState, EmptyStateTone, KeyBinding, Tooltip, checkbox_indicator, error_callout,
    grouped_block, modal_footer, modal_surface, prelude::*, primary_button, secondary_button,
    section_label, segment_option, segmented_control, status_chip,
};
use util::ResultExt as _;
use workspace::ModalView;

struct Row {
    imported: Imported,
    environment: Environment,
    checked: bool,
    duplicate: bool,
}

pub struct ImportConnections {
    panel: WeakEntity<DatabasePanel>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    tool: Tool,
    loading: bool,
    /// A tool without an entry wasn't found.
    found: HashMap<Tool, Result<Vec<Row>, SharedString>>,
    paths: HashMap<Tool, PathBuf>,
    scope: Scope,
    save_error: Option<SharedString>,
    _load_task: Task<()>,
    _import_task: Task<()>,
}

impl EventEmitter<DismissEvent> for ImportConnections {}

impl ModalView for ImportConnections {}

impl Focusable for ImportConnections {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ImportConnections {
    pub fn new(
        panel: WeakEntity<DatabasePanel>,
        fs: Arc<dyn Fs>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            panel,
            fs,
            focus_handle: cx.focus_handle(),
            tool: Tool::DBeaver,
            loading: true,
            found: HashMap::default(),
            paths: HashMap::default(),
            scope: Scope::Profile,
            save_error: None,
            _load_task: Task::ready(()),
            _import_task: Task::ready(()),
        };
        this.load_default_paths(cx);
        this
    }

    fn load_default_paths(&mut self, cx: &mut Context<Self>) {
        let fs = self.fs.clone();
        self._load_task = cx.spawn(async move |this, cx| {
            let mut results = Vec::new();
            for tool in Tool::ALL {
                let Some(path) = tool.default_path() else {
                    continue;
                };
                if !fs.is_file(&path).await {
                    continue;
                }
                let result = discovery::read_tool(fs.as_ref(), tool, &path).await;
                results.push((tool, path, result));
            }
            this.update(cx, |this, cx| {
                for (tool, path, result) in results {
                    this.set_found(tool, path, result, cx);
                }
                let first_with_rows = Tool::ALL.into_iter().find(
                    |tool| matches!(this.found.get(tool), Some(Ok(rows)) if !rows.is_empty()),
                );
                if let Some(tool) = first_with_rows {
                    this.tool = tool;
                }
                this.loading = false;
                cx.notify();
            })
            .log_err();
        });
    }

    fn set_found(
        &mut self,
        tool: Tool,
        path: PathBuf,
        result: anyhow::Result<Vec<Imported>>,
        cx: &App,
    ) {
        let found = match result {
            Ok(imported) => Ok(imported
                .into_iter()
                .map(|imported| self.row(imported, cx))
                .collect()),
            Err(error) => {
                log::warn!("Banco: {} não foi lido: {error:#}", path.display());
                Err(format!("{error:#}").into())
            }
        };
        self.paths.insert(tool, path);
        self.found.insert(tool, found);
    }

    fn row(&self, imported: Imported, cx: &App) -> Row {
        let duplicate = self.panel.upgrade().is_some_and(|panel| {
            panel.read(cx).has_connection_like(
                &imported.host,
                imported.port,
                &imported.database,
                &imported.user,
            )
        });
        Row {
            environment: imported.environment(),
            checked: !duplicate,
            duplicate,
            imported,
        }
    }

    fn rows_mut(&mut self) -> Option<&mut Vec<Row>> {
        match self.found.get_mut(&self.tool) {
            Some(Ok(rows)) => Some(rows),
            _ => None,
        }
    }

    fn checked_count(&self) -> usize {
        match self.found.get(&self.tool) {
            Some(Ok(rows)) => rows.iter().filter(|row| row.checked).count(),
            _ => 0,
        }
    }

    fn toggle(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(row) = self.rows_mut().and_then(|rows| rows.get_mut(index)) {
            row.checked = !row.checked;
            cx.notify();
        }
    }

    fn set_environment(&mut self, index: usize, environment: Environment, cx: &mut Context<Self>) {
        if let Some(row) = self.rows_mut().and_then(|rows| rows.get_mut(index)) {
            row.environment = environment;
            cx.notify();
        }
    }

    fn pick_other_file(&mut self, cx: &mut Context<Self>) {
        let tool = self.tool;
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        let fs = self.fs.clone();
        self._load_task = cx.spawn(async move |this, cx| {
            // A dropped dialog is the same as cancelling it.
            let Some(path) = paths
                .await
                .ok()
                .and_then(|picked| picked.log_err())
                .flatten()
                .and_then(|picked| picked.into_iter().next())
            else {
                return;
            };
            let result = discovery::read_tool(fs.as_ref(), tool, &path).await;
            this.update(cx, |this, cx| {
                this.set_found(tool, path, result, cx);
                cx.notify();
            })
            .log_err();
        });
    }

    fn import(&mut self, cx: &mut Context<Self>) {
        let Some(Ok(rows)) = self.found.get(&self.tool) else {
            return;
        };
        let connections = rows
            .iter()
            .filter(|row| row.checked)
            .map(|row| {
                let imported = &row.imported;
                let password = match &imported.password {
                    PasswordSource::Included(password) => Some(password.clone()),
                    PasswordSource::AskOnConnect => None,
                };
                let connection = SavedConnection::imported(
                    imported.name.clone(),
                    row.environment,
                    imported.host.clone(),
                    imported.port,
                    imported.database.clone(),
                    imported.user.clone(),
                    imported.ssl_mode,
                );
                (connection, password)
            })
            .collect::<Vec<_>>();
        if connections.is_empty() {
            return;
        }
        let Some(panel) = self.panel.upgrade() else {
            return;
        };
        let scope = self.scope;
        let task = panel.update(cx, |panel, cx| {
            panel.import_connections(connections, scope, cx)
        });
        self.save_error = None;
        self._import_task = cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| match result {
                Ok(()) => cx.emit(DismissEvent),
                Err(error) => {
                    this.save_error = Some(format!("{error:#}").into());
                    cx.notify();
                }
            })
            .log_err();
        });
        cx.notify();
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn secondary_confirm(
        &mut self,
        _: &menu::SecondaryConfirm,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.import(cx);
    }

    fn render_tools(&self, cx: &mut Context<Self>) -> impl IntoElement {
        segmented_control(cx).children(Tool::ALL.into_iter().enumerate().map(|(index, tool)| {
            let label: SharedString = match self.found.get(&tool) {
                Some(Ok(rows)) => t!(
                    "database_client.import.tool_count",
                    tool = tool.label(),
                    count = rows.len()
                ),
                Some(Err(_)) => tool.label().into(),
                None if self.loading && tool.is_supported() => tool.label().into(),
                None => t!("database_client.import.tool_not_found", tool = tool.label()),
            };
            segment_option(
                ("db-import-tool", index),
                label,
                tool == self.tool,
                tool.is_supported(),
                cx,
            )
            .when(!self.found.contains_key(&tool), |this| this.opacity(0.5))
            .when(tool.is_supported() && tool != self.tool, |this| {
                this.on_click(cx.listener(move |this, _, _, cx| {
                    this.tool = tool;
                    cx.notify();
                }))
            })
        }))
    }

    fn render_source_path(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let path = self
            .paths
            .get(&self.tool)
            .cloned()
            .or_else(|| self.tool.default_path());
        let home = util::paths::home_dir();
        let path_text = path
            .map(|path| match path.strip_prefix(home) {
                Ok(relative) => format!("~/{}", relative.display()),
                Err(_) => path.display().to_string(),
            })
            .unwrap_or_default();
        h_flex()
            .gap_2()
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(path_text)
                        .size(LabelSize::XSmall)
                        .buffer_font(cx)
                        .color(Color::Muted)
                        .truncate(),
                ),
            )
            .when(self.tool.is_supported(), |this| {
                this.child(
                    secondary_button(
                        "db-import-other-file",
                        Some((IconName::FolderOpen, Color::Muted)),
                        t!("database_client.import.other_file"),
                        true,
                        cx,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.pick_other_file(cx))),
                )
            })
    }

    fn render_list(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.loading {
            return EmptyState::new(IconName::Database, t!("database_client.import.reading"))
                .spinning(true)
                .into_any_element();
        }
        match self.found.get(&self.tool) {
            Some(Ok(rows)) if !rows.is_empty() => div()
                .id("db-import-list")
                .max_h(px(300.))
                .overflow_y_scroll()
                .child(
                    grouped_block(cx).children(
                        rows.iter()
                            .enumerate()
                            .map(|(index, row)| self.render_row(index, row, cx)),
                    ),
                )
                .into_any_element(),
            Some(Err(error)) => {
                let path = self
                    .paths
                    .get(&self.tool)
                    .map(|path| path.display().to_string())
                    .unwrap_or_default();
                EmptyState::new(
                    IconName::Database,
                    t!("database_client.import.unreadable", path = path),
                )
                .tone(EmptyStateTone::Error)
                .detail(error.clone())
                .into_any_element()
            }
            _ => EmptyState::new(
                IconName::Database,
                t!(
                    "database_client.import.none_found",
                    tool = self.tool.label()
                ),
            )
            .into_any_element(),
        }
    }

    fn render_row(&self, index: usize, row: &Row, cx: &mut Context<Self>) -> AnyElement {
        let hover = cx.theme().colors().element_hover;
        let environment = row.environment;
        let this = cx.weak_entity();
        let environment_select = environment_select(
            ("db-import-environment", index),
            environment,
            true,
            move |candidate, cx| {
                this.update(cx, |this, cx| this.set_environment(index, candidate, cx))
                    .log_err();
            },
            cx,
        );
        let password = if row.duplicate {
            status_chip(
                None,
                t!("database_client.import.already_here"),
                Color::Muted,
                cx,
            )
            .into_any_element()
        } else {
            let (icon, label, tooltip, color) = match &row.imported.password {
                PasswordSource::Included(_) => (
                    IconName::Lock,
                    t!("database_client.import.password_included"),
                    t!("database_client.import.password_included_tooltip"),
                    Color::Success,
                ),
                PasswordSource::AskOnConnect => (
                    IconName::Warning,
                    t!("database_client.import.password_ask"),
                    t!("database_client.import.password_ask_tooltip"),
                    Color::Warning,
                ),
            };
            div()
                .id(("db-import-password", index))
                .child(status_chip(Some(icon), label, color, cx))
                .tooltip(Tooltip::text(tooltip))
                .into_any_element()
        };
        h_flex()
            .id(("db-import-line", index))
            .w_full()
            .h(px(38.))
            .px_2()
            .gap_3()
            .rounded(Radius::Sm.rems())
            .hover(move |style| style.bg(hover))
            .child(
                h_flex()
                    .id(("db-import-row", index))
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .gap_3()
                    .cursor_pointer()
                    .child(checkbox_indicator(row.checked, cx))
                    .child(
                        div().w(px(150.)).flex_none().child(
                            Label::new(row.imported.name.clone())
                                .size(LabelSize::Small)
                                .color(if row.duplicate {
                                    Color::Muted
                                } else {
                                    Color::Default
                                })
                                .truncate(),
                        ),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(row.imported.address())
                                .size(LabelSize::XSmall)
                                .buffer_font(cx)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle(index, cx))),
            )
            .child(div().w(px(120.)).flex_none().child(environment_select))
            .child(h_flex().w(px(150.)).flex_none().child(password))
            .into_any_element()
    }

    fn render_scope(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_1p5()
            .child(section_label(t!("database_client.connect.save_to")))
            .child(h_flex().child(scope_control(
                "db-import-scope",
                self.scope,
                cx.listener(|this, scope: &Scope, _, cx| {
                    this.scope = *scope;
                    cx.notify();
                }),
                cx,
            )))
            .when(self.scope == Scope::Project, |this| {
                this.child(
                    Label::new(t!("database_client.connect.scope_note"))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
    }
}

impl Render for ImportConnections {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.checked_count();
        let import_label = if count == 1 {
            t!("database_client.import.import_count.one")
        } else {
            t!("database_client.import.import_count.other", count = count)
        };
        modal_surface(cx)
            .key_context("DatabaseImportConnections")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::secondary_confirm))
            .w(px(720.))
            .child(
                v_flex()
                    .px_6()
                    .pt(px(22.))
                    .pb(px(18.))
                    .gap_4()
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                Label::new(t!("database_client.import.title"))
                                    .size(LabelSize::Large)
                                    .weight(FontWeight::SEMIBOLD),
                            )
                            .child(
                                Label::new(t!("database_client.import.subtitle"))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(self.render_tools(cx))
                    .child(self.render_source_path(cx))
                    .child(self.render_list(cx))
                    .child(self.render_scope(cx))
                    .when_some(self.save_error.clone(), |this, error| {
                        this.child(error_callout(
                            t!("database_client.import.failed"),
                            error,
                            cx,
                        ))
                    }),
            )
            .child(
                modal_footer(cx)
                    .child(
                        Label::new(t!("database_client.import.ssh_off"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(
                        secondary_button(
                            "db-import-cancel",
                            None,
                            t!("database_client.connect.cancel"),
                            true,
                            cx,
                        )
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        primary_button(
                            "db-import-confirm",
                            None,
                            import_label,
                            Some(KeyBinding::for_action_in(
                                &menu::SecondaryConfirm,
                                &self.focus_handle,
                                cx,
                            )),
                            cx,
                        )
                        .when(count == 0, |this| this.opacity(0.5).cursor_default())
                        .on_click(cx.listener(|this, _, _, cx| this.import(cx))),
                    ),
            )
    }
}
