//! One operation open as a workspace tab: the request on top, the response below.

use crate::{
    SendRequest,
    collection::{Collection, Exchange},
    config::{self, HeaderEntry, ParamEntry, RequestDraft},
    schema::Validation,
    send::{self, HeaderOrigin},
    spec::{Operation, ParameterLocation},
};
use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, ClipboardItem, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla,
    Subscription, Task, WeakEntity,
};
use language::LanguageRegistry;
use std::sync::Arc;
use ui::{Checkbox, Tooltip, prelude::*};
use ui_input::{ErasedEditorEvent, InputField};
use util::ResultExt as _;
use workspace::{
    Item, Workspace,
    item::{ItemEvent, TabContentParams},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum RequestTab {
    Params,
    Headers,
    Body,
    Auth,
    Docs,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ResponseTab {
    Body,
    Headers,
    Timeline,
}

struct Row {
    name: Entity<InputField>,
    value: Entity<InputField>,
    enabled: bool,
}

pub struct ApiRequestView {
    collection: Entity<Collection>,
    pub(crate) operation_key: String,
    pub(crate) saved_id: Option<String>,
    saved_name: Option<String>,
    focus_handle: FocusHandle,
    url_input: Entity<InputField>,
    path_params: Vec<(String, Entity<InputField>)>,
    query_rows: Vec<Row>,
    header_rows: Vec<Row>,
    disabled_inherited: Vec<String>,
    body_editor: Entity<Editor>,
    response_editor: Entity<Editor>,
    request_tab: RequestTab,
    response_tab: ResponseTab,
    exchange: Option<Exchange>,
    body_validation: Option<Validation>,
    send_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

pub fn method_color(method: &str) -> Color {
    match method {
        "GET" => Color::Success,
        "POST" => Color::Accent,
        "PUT" => Color::Info,
        "PATCH" => Color::Warning,
        "DELETE" => Color::Error,
        _ => Color::Muted,
    }
}

pub fn method_label(method: &str, cx: &App) -> Label {
    Label::new(SharedString::from(method.to_string()))
        .size(LabelSize::XSmall)
        .weight(FontWeight::BOLD)
        .buffer_font(cx)
        .color(method_color(method))
}

pub fn status_color(status: u16) -> Color {
    match status {
        200..=299 => Color::Success,
        300..=399 => Color::Info,
        400..=499 => Color::Warning,
        _ => Color::Error,
    }
}

fn chip(label: impl Into<SharedString>, color: Color, cx: &App) -> impl IntoElement {
    let hsla: Hsla = color.color(cx);
    div()
        .px_1p5()
        .py_0p5()
        .rounded_sm()
        .bg(hsla.opacity(0.14))
        .child(
            Label::new(label)
                .size(LabelSize::XSmall)
                .weight(FontWeight::MEDIUM)
                .color(color),
        )
}

fn human_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0).replace('.', ",")
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0)).replace('.', ",")
    }
}

impl ApiRequestView {
    pub fn new(
        collection: Entity<Collection>,
        operation_key: String,
        saved: Option<(String, String, RequestDraft)>,
        languages: Arc<LanguageRegistry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (saved_id, saved_name, draft) = match saved {
            Some((id, name, draft)) => (Some(id), Some(name), draft),
            None => (None, None, collection.read(cx).draft(&operation_key)),
        };

        let url_input = cx.new(|cx| InputField::new(window, cx, "{{baseUrl}}/caminho"));
        let url_editor = url_input.read(cx).editor().clone();
        url_editor.set_text(&draft.url, window, cx);

        let body_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_placeholder_text("Sem body", window, cx);
            editor.set_text(draft.body.clone().unwrap_or_default(), window, cx);
            editor
        });
        let response_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_read_only(true);
            editor
        });
        set_json_language(&languages, &body_editor, cx);
        set_json_language(&languages, &response_editor, cx);

        let mut this = Self {
            collection: collection.clone(),
            operation_key,
            saved_id,
            saved_name,
            focus_handle: cx.focus_handle(),
            url_input,
            path_params: Vec::new(),
            query_rows: Vec::new(),
            header_rows: Vec::new(),
            disabled_inherited: draft.disabled_inherited.clone(),
            body_editor: body_editor.clone(),
            response_editor,
            request_tab: if draft.body.is_some() {
                RequestTab::Body
            } else {
                RequestTab::Params
            },
            response_tab: ResponseTab::Body,
            exchange: None,
            body_validation: None,
            send_task: None,
            _subscriptions: Vec::new(),
        };
        for parameter in &draft.path_params {
            let input = this.new_input("valor", &parameter.value, window, cx);
            this.path_params.push((parameter.name.clone(), input));
        }
        for parameter in &draft.query {
            let row = this.new_row(
                &parameter.name,
                &parameter.value,
                parameter.enabled,
                window,
                cx,
            );
            this.query_rows.push(row);
        }
        for header in &draft.headers {
            let row = this.new_row(&header.name, &header.value, header.enabled, window, cx);
            this.header_rows.push(row);
        }
        this.watch_input(&this.url_input.clone(), window, cx);
        this._subscriptions.push(
            cx.subscribe(&body_editor, |this, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    this.body_changed(cx);
                }
            }),
        );
        this._subscriptions
            .push(cx.observe(&collection, |_, _, cx| cx.notify()));
        this.body_changed(cx);
        this
    }

    fn new_input(
        &mut self,
        placeholder: &str,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputField> {
        let input = cx.new(|cx| InputField::new(window, cx, placeholder));
        let editor = input.read(cx).editor().clone();
        editor.set_text(text, window, cx);
        self.watch_input(&input, window, cx);
        input
    }

    fn new_row(
        &mut self,
        name: &str,
        value: &str,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Row {
        Row {
            name: self.new_input("nome", name, window, cx),
            value: self.new_input("valor", value, window, cx),
            enabled,
        }
    }

    fn watch_input(
        &mut self,
        input: &Entity<InputField>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = input.read(cx).editor().clone();
        let this = cx.weak_entity();
        self._subscriptions.push(editor.subscribe(
            Box::new(move |event, _window, cx| {
                if event == ErasedEditorEvent::BufferEdited {
                    this.update(cx, |this, cx| this.store_draft(cx)).log_err();
                }
            }),
            window,
            cx,
        ));
    }

    fn operation<'a>(&self, cx: &'a App) -> Option<&'a Operation> {
        let spec = self.collection.read(cx).spec.as_ref()?;
        spec.operation(&self.operation_key)
    }

    fn draft(&self, cx: &App) -> RequestDraft {
        let rows_to_params = |rows: &[Row]| -> Vec<ParamEntry> {
            rows.iter()
                .map(|row| ParamEntry {
                    name: row.name.read(cx).text(cx).trim().to_string(),
                    value: row.value.read(cx).text(cx),
                    enabled: row.enabled,
                })
                .collect()
        };
        let body = self.body_editor.read(cx).text(cx);
        RequestDraft {
            url: self.url_input.read(cx).text(cx).trim().to_string(),
            path_params: self
                .path_params
                .iter()
                .map(|(name, input)| ParamEntry {
                    name: name.clone(),
                    value: input.read(cx).text(cx),
                    enabled: true,
                })
                .collect(),
            query: rows_to_params(&self.query_rows),
            headers: rows_to_params(&self.header_rows)
                .into_iter()
                .map(|entry| HeaderEntry {
                    name: entry.name,
                    value: entry.value,
                    enabled: entry.enabled,
                })
                .collect(),
            disabled_inherited: self.disabled_inherited.clone(),
            body: (!body.trim().is_empty()).then_some(body),
        }
    }

    fn store_draft(&mut self, cx: &mut Context<Self>) {
        if self.saved_id.is_some() {
            cx.notify();
            return;
        }
        let draft = self.draft(cx);
        let key = self.operation_key.clone();
        self.collection
            .update(cx, |collection, _| collection.store_draft(&key, draft));
        cx.notify();
    }

    fn body_changed(&mut self, cx: &mut Context<Self>) {
        let body = self.body_editor.read(cx).text(cx);
        self.body_validation = if body.trim().is_empty() {
            None
        } else {
            self.collection
                .read(cx)
                .validate_body(&self.operation_key, &body)
        };
        self.store_draft(cx);
    }

    fn fill_example(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let example = self
            .collection
            .read(cx)
            .spec
            .as_ref()
            .and_then(|spec| config::example_body(spec, &self.operation_key));
        if let Some(example) = example {
            self.body_editor
                .update(cx, |editor, cx| editor.set_text(example, window, cx));
        }
    }

    pub fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.send_task.is_some() {
            return;
        }
        let draft = self.draft(cx);
        let key = self.operation_key.clone();
        let exchange = self
            .collection
            .update(cx, |collection, cx| collection.send(key, draft, cx));
        self.send_task = Some(cx.spawn_in(window, async move |this, cx| {
            let exchange = exchange.await;
            this.update_in(cx, |this, window, cx| {
                let body = exchange
                    .response
                    .as_ref()
                    .map(|response| response.display_body())
                    .unwrap_or_default();
                this.response_editor
                    .update(cx, |editor, cx| editor.set_text(body, window, cx));
                if exchange.error.is_some() || exchange.timeline.len() > 1 {
                    this.response_tab = ResponseTab::Timeline;
                } else if this.response_tab == ResponseTab::Timeline {
                    this.response_tab = ResponseTab::Body;
                }
                this.exchange = Some(exchange);
                this.send_task = None;
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let draft = self.draft(cx);
        let key = self.operation_key.clone();
        if let Some(id) = self.saved_id.clone() {
            self.collection.update(cx, |collection, cx| {
                collection.update_saved_request(&id, draft, cx)
            });
            return;
        }
        let name = self
            .operation(cx)
            .map(|operation| operation.title())
            .unwrap_or_else(|| key.clone());
        let name = format!("{name} · {}", chrono::Local::now().format("%d/%m %H:%M"));
        let id = self.collection.update(cx, |collection, cx| {
            collection.save_request(name.clone(), key, draft, cx)
        });
        self.saved_id = Some(id);
        self.saved_name = Some(name);
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn render_request_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let method = self
            .operation(cx)
            .map(|operation| operation.method.clone())
            .unwrap_or_else(|| "?".to_string());
        let sending = self.send_task.is_some();
        let focus_handle = self.focus_handle.clone();
        h_flex()
            .gap_2()
            .px_3()
            .pt_3()
            .child(
                h_flex()
                    .h_8()
                    .px_2p5()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().editor_background)
                    .child(
                        Label::new(SharedString::from(method.clone()))
                            .size(LabelSize::Small)
                            .weight(FontWeight::BOLD)
                            .buffer_font(cx)
                            .color(method_color(&method)),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .capture_action(cx.listener(
                        |this, _: &editor::actions::Newline, window, cx| {
                            this.send(window, cx);
                        },
                    ))
                    .child(self.url_input.clone()),
            )
            .child(
                Button::new("api-send", if sending { "Enviando…" } else { "Enviar" })
                    .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                    .start_icon(Icon::new(IconName::Send).size(IconSize::Small))
                    .disabled(sending)
                    .key_binding(ui::KeyBinding::for_action_in(
                        &SendRequest,
                        &focus_handle,
                        cx,
                    ))
                    .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
            )
            .child(
                IconButton::new("api-save", IconName::Bookmark)
                    .icon_size(IconSize::Small)
                    .icon_color(if self.saved_id.is_some() {
                        Color::Accent
                    } else {
                        Color::Muted
                    })
                    .tooltip(Tooltip::text(if self.saved_id.is_some() {
                        "Atualizar em Salvas por mim"
                    } else {
                        "Salvar em Salvas por mim"
                    }))
                    .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
            )
    }

    fn render_meta(&self, cx: &App) -> impl IntoElement {
        let collection = self.collection.read(cx);
        let draft = self.draft(cx);
        let prepared = collection.prepare(&self.operation_key, &draft).ok();
        let operation = self.operation(cx);
        let mut parts: Vec<String> = Vec::new();
        if let Some(operation) = operation {
            if let Some(id) = &operation.operation_id {
                parts.push(format!("operationId: {id}"));
            }
            if let Some(tag) = operation.tags.first() {
                parts.push(format!("tag {tag}"));
            }
            let schemes = collection
                .spec
                .as_ref()
                .map(|spec| {
                    spec.schemes_for(operation)
                        .iter()
                        .map(|scheme| scheme.name.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            parts.push(if schemes.is_empty() {
                "security: nenhuma (rota pública)".to_string()
            } else {
                format!("security: {}", schemes.join(", "))
            });
        }
        let missing = prepared
            .as_ref()
            .map(|prepared| prepared.missing.clone())
            .unwrap_or_default();
        v_flex()
            .px_3()
            .pt_1p5()
            .gap_0p5()
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::Book)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(parts.join(" · "))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                    )
                    .when_some(
                        operation.filter(|operation| operation.deprecated),
                        |this, _| this.child(chip("deprecated", Color::Warning, cx)),
                    ),
            )
            .when_some(prepared, |this, prepared| {
                this.child(
                    Label::new(prepared.url)
                        .size(LabelSize::XSmall)
                        .buffer_font(cx)
                        .color(Color::Muted)
                        .truncate(),
                )
            })
            .when(!missing.is_empty(), |this| {
                this.child(
                    h_flex()
                        .gap_1()
                        .child(
                            Icon::new(IconName::Warning)
                                .size(IconSize::XSmall)
                                .color(Color::Warning),
                        )
                        .child(
                            Label::new(format!(
                                "Sem valor: {} — defina no ambiente ou na aba API",
                                missing
                                    .iter()
                                    .map(|name| if name.starts_with('{') {
                                        name.clone()
                                    } else {
                                        format!("{{{{{name}}}}}")
                                    })
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ))
                            .size(LabelSize::XSmall)
                            .color(Color::Warning),
                        ),
                )
            })
            .when(operation.is_none(), |this| {
                this.child(
                    Label::new("Esta operação não existe mais no spec.")
                        .size(LabelSize::XSmall)
                        .color(Color::Error),
                )
            })
    }

    fn render_tab_button(
        &self,
        id: &'static str,
        label: String,
        active: bool,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        Button::new(id, label)
            .style(if active {
                ButtonStyle::Filled
            } else {
                ButtonStyle::Subtle
            })
            .label_size(LabelSize::Small)
            .toggle_state(active)
            .on_click(cx.listener(move |this, _, _, cx| {
                on_click(this, cx);
                cx.notify();
            }))
    }

    fn render_request_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let operation = self.operation(cx).cloned();
        let has_body_schema = operation
            .as_ref()
            .is_some_and(|operation| operation.request_body.is_some());
        let params_count = self.path_params.len() + self.query_rows.len();
        let headers_count = self.header_rows.len() + self.inherited_headers(cx).len();
        let auth_label = match operation.as_ref() {
            Some(operation) => {
                let collection = self.collection.read(cx);
                let schemes = collection
                    .spec
                    .as_ref()
                    .map(|spec| spec.schemes_for(operation).len())
                    .unwrap_or(0);
                if schemes == 0 {
                    "Auth · nenhuma".to_string()
                } else {
                    "Auth · herdada".to_string()
                }
            }
            None => "Auth".to_string(),
        };
        let tab = self.request_tab;
        h_flex()
            .px_3()
            .pt_2()
            .gap_1()
            .child(self.render_tab_button(
                "api-tab-params",
                if params_count > 0 {
                    format!("Params {params_count}")
                } else {
                    "Params".to_string()
                },
                tab == RequestTab::Params,
                |this, _| this.request_tab = RequestTab::Params,
                cx,
            ))
            .child(self.render_tab_button(
                "api-tab-headers",
                format!("Headers {headers_count}"),
                tab == RequestTab::Headers,
                |this, _| this.request_tab = RequestTab::Headers,
                cx,
            ))
            .child(self.render_tab_button(
                "api-tab-body",
                if has_body_schema {
                    "Body".to_string()
                } else {
                    "Body · nenhum".to_string()
                },
                tab == RequestTab::Body,
                |this, _| this.request_tab = RequestTab::Body,
                cx,
            ))
            .child(self.render_tab_button(
                "api-tab-auth",
                auth_label,
                tab == RequestTab::Auth,
                |this, _| this.request_tab = RequestTab::Auth,
                cx,
            ))
            .child(self.render_tab_button(
                "api-tab-docs",
                "Docs".to_string(),
                tab == RequestTab::Docs,
                |this, _| this.request_tab = RequestTab::Docs,
                cx,
            ))
    }

    fn inherited_headers(&self, cx: &App) -> Vec<send::PlannedHeader> {
        let collection = self.collection.read(cx);
        let Some(spec) = collection.spec.as_ref() else {
            return Vec::new();
        };
        let has_body = !self.body_editor.read(cx).text(cx).trim().is_empty();
        send::inherited_headers(
            spec,
            &self.operation_key,
            &collection.file.headers,
            &|name: &str| collection.variable(name),
            has_body,
        )
    }

    fn render_rows(
        &self,
        kind: &'static str,
        rows: &[Row],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .gap_1()
            .children(rows.iter().enumerate().map(|(index, row)| {
                h_flex()
                    .gap_2()
                    .child(
                        Checkbox::new(
                            SharedString::from(format!("api-{kind}-enabled-{index}")),
                            row.enabled.into(),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let rows = match kind {
                                "query" => &mut this.query_rows,
                                _ => &mut this.header_rows,
                            };
                            if let Some(row) = rows.get_mut(index) {
                                row.enabled = !row.enabled;
                            }
                            this.store_draft(cx);
                        })),
                    )
                    .child(div().w(px(200.)).child(row.name.clone()))
                    .child(div().flex_1().child(row.value.clone()))
                    .child(
                        IconButton::new(
                            SharedString::from(format!("api-{kind}-remove-{index}")),
                            IconName::Trash,
                        )
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let rows = match kind {
                                "query" => &mut this.query_rows,
                                _ => &mut this.header_rows,
                            };
                            if index < rows.len() {
                                rows.remove(index);
                            }
                            this.store_draft(cx);
                        })),
                    )
            }))
    }

    fn add_row_button(
        &self,
        kind: &'static str,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        Button::new(SharedString::from(format!("api-{kind}-add")), label)
            .style(ButtonStyle::Subtle)
            .label_size(LabelSize::Small)
            .color(Color::Accent)
            .start_icon(
                Icon::new(IconName::Plus)
                    .size(IconSize::XSmall)
                    .color(Color::Accent),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                let row = this.new_row("", "", true, window, cx);
                window.focus(&row.name.focus_handle(cx), cx);
                match kind {
                    "query" => this.query_rows.push(row),
                    _ => this.header_rows.push(row),
                }
                this.store_draft(cx);
            }))
    }

    fn section_label(text: &'static str) -> impl IntoElement {
        Label::new(text)
            .size(LabelSize::XSmall)
            .weight(FontWeight::SEMIBOLD)
            .color(Color::Muted)
    }

    fn render_params(&self, cx: &mut Context<Self>) -> AnyElement {
        let descriptions: Vec<(String, Option<String>, bool)> = self
            .operation(cx)
            .map(|operation| {
                operation
                    .parameters
                    .iter()
                    .filter(|parameter| parameter.location == ParameterLocation::Path)
                    .map(|parameter| {
                        (
                            parameter.name.clone(),
                            parameter.description.clone(),
                            parameter.required,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        v_flex()
            .gap_2()
            .when(!self.path_params.is_empty(), |this| {
                this.child(Self::section_label("PATH"))
                    .children(self.path_params.iter().map(|(name, input)| {
                        let description = descriptions
                            .iter()
                            .find(|(candidate, _, _)| candidate == name)
                            .and_then(|(_, description, _)| description.clone());
                        h_flex()
                            .gap_2()
                            .child(
                                div().w(px(224.)).child(
                                    Label::new(format!("{{{name}}}"))
                                        .size(LabelSize::Small)
                                        .buffer_font(cx),
                                ),
                            )
                            .child(div().flex_1().child(input.clone()))
                            .when_some(description, |this, description| {
                                this.child(
                                    div().max_w(px(260.)).child(
                                        Label::new(description)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted)
                                            .truncate(),
                                    ),
                                )
                            })
                    }))
            })
            .child(Self::section_label("QUERY"))
            .child(self.render_rows("query", &self.query_rows, cx))
            .child(self.add_row_button("query", "Adicionar parâmetro", cx))
            .into_any_element()
    }

    fn render_headers(&self, cx: &mut Context<Self>) -> AnyElement {
        let inherited = self.inherited_headers(cx);
        v_flex()
            .gap_2()
            .child(Self::section_label("HERDADOS"))
            .when(inherited.is_empty(), |this| {
                this.child(
                    Label::new("Nenhum header vem do spec, da coleção ou do login.")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .children(inherited.into_iter().enumerate().map(|(index, header)| {
                let disabled = self
                    .disabled_inherited
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(&header.name));
                let enabled = header.enabled && !disabled;
                let origin_color = match &header.origin {
                    HeaderOrigin::Scheme(_) => Color::Success,
                    HeaderOrigin::Spec => Color::Info,
                    HeaderOrigin::Collection => Color::Muted,
                };
                let name = header.name.clone();
                h_flex()
                    .gap_2()
                    .child(
                        Checkbox::new(
                            SharedString::from(format!("api-inherited-{index}")),
                            enabled.into(),
                        )
                        .disabled(!header.enabled)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(position) = this
                                .disabled_inherited
                                .iter()
                                .position(|existing| existing.eq_ignore_ascii_case(&name))
                            {
                                this.disabled_inherited.remove(position);
                            } else {
                                this.disabled_inherited.push(name.clone());
                            }
                            this.store_draft(cx);
                        })),
                    )
                    .child(
                        div().w(px(200.)).child(
                            Label::new(header.name.clone())
                                .size(LabelSize::Small)
                                .buffer_font(cx)
                                .color(if enabled {
                                    Color::Default
                                } else {
                                    Color::Muted
                                }),
                        ),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(header.value.clone())
                                .size(LabelSize::Small)
                                .buffer_font(cx)
                                .color(if enabled {
                                    Color::Default
                                } else {
                                    Color::Muted
                                })
                                .truncate(),
                        ),
                    )
                    .child(chip(header.origin.label(), origin_color, cx))
            }))
            .child(Self::section_label("DESTA REQUEST"))
            .child(self.render_rows("header", &self.header_rows, cx))
            .child(self.add_row_button("header", "Adicionar header", cx))
            .child(
                Label::new("Desmarcar um header herdado vale só para esta request.")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .into_any_element()
    }

    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let operation = self.operation(cx);
        let content_type = operation
            .and_then(|operation| operation.request_body.as_ref())
            .map(|body| body.content_type.clone());
        let schema_name = operation
            .and_then(|operation| operation.request_body.as_ref())
            .and_then(|body| body.schema.as_ref())
            .and_then(|schema| schema.get("$ref"))
            .and_then(|reference| reference.as_str())
            .and_then(|reference| reference.rsplit('/').next())
            .map(str::to_string);
        let is_json = content_type
            .as_deref()
            .is_none_or(crate::spec::is_json_content_type);
        v_flex()
            .size_full()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .when_some(content_type.clone(), |this, content_type| {
                        this.child(chip(content_type, Color::Muted, cx))
                    })
                    .child(div().flex_1())
                    .when(content_type.is_some(), |this| {
                        this.child(
                            Button::new("api-fill-example", "Preencher com o exemplo do spec")
                                .style(ButtonStyle::Outlined)
                                .label_size(LabelSize::Small)
                                .start_icon(Icon::new(IconName::FileCode).size(IconSize::XSmall))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.fill_example(window, cx)
                                })),
                        )
                    }),
            )
            .when(!is_json, |this| {
                this.child(
                    Label::new(
                        "Este body não é JSON: o texto vai como está, sem montar multipart ou formulário.",
                    )
                    .size(LabelSize::XSmall)
                    .color(Color::Warning),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_h(px(120.))
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().editor_background)
                    .capture_action(cx.listener(|this, _: &editor::actions::NewlineBelow, window, cx| {
                        this.send(window, cx);
                    }))
                    .child(self.body_editor.clone()),
            )
            .child(render_validation(
                self.body_validation.as_ref(),
                schema_name.as_deref(),
                "Válido contra",
                cx,
            ))
            .into_any_element()
    }

    fn render_auth(&self, cx: &mut Context<Self>) -> AnyElement {
        let collection = self.collection.read(cx);
        let plans = collection
            .spec
            .as_ref()
            .map(|spec| {
                send::auth_plan(spec, &self.operation_key, &|name: &str| {
                    collection.variable(name)
                })
            })
            .unwrap_or_default();
        v_flex()
            .gap_2()
            .when(plans.is_empty(), |this| {
                this.child(
                    Label::new("Esta rota é pública: o spec declara security: [].")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .children(plans.into_iter().map(|plan| {
                h_flex()
                    .gap_2()
                    .child(
                        Icon::new(if plan.missing.is_some() {
                            IconName::Warning
                        } else {
                            IconName::Check
                        })
                        .size(IconSize::Small)
                        .color(if plan.missing.is_some() {
                            Color::Warning
                        } else {
                            Color::Success
                        }),
                    )
                    .child(
                        Label::new(plan.scheme)
                            .size(LabelSize::Small)
                            .buffer_font(cx),
                    )
                    .child(
                        Label::new(plan.summary)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .when_some(plan.missing, |this, missing| {
                        this.child(
                            Label::new(missing)
                                .size(LabelSize::XSmall)
                                .color(Color::Warning),
                        )
                    })
            }))
            .child(
                Label::new("Login, chaves e headers da coleção ficam na aba API do dock.")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .into_any_element()
    }

    fn render_docs(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(operation) = self.operation(cx).cloned() else {
            return div().into_any_element();
        };
        v_flex()
            .gap_2()
            .child(
                Label::new(operation.title())
                    .size(LabelSize::Default)
                    .weight(FontWeight::SEMIBOLD),
            )
            .when_some(operation.description.clone(), |this, description| {
                this.child(
                    Label::new(description)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .when(!operation.parameters.is_empty(), |this| {
                this.child(Self::section_label("PARÂMETROS")).children(
                    operation.parameters.iter().map(|parameter| {
                        let location = match parameter.location {
                            ParameterLocation::Path => "path",
                            ParameterLocation::Query => "query",
                            ParameterLocation::Header => "header",
                            ParameterLocation::Cookie => "cookie",
                        };
                        h_flex()
                            .gap_2()
                            .child(
                                Label::new(parameter.name.clone())
                                    .size(LabelSize::Small)
                                    .buffer_font(cx),
                            )
                            .child(chip(location, Color::Muted, cx))
                            .when(parameter.required, |this| {
                                this.child(chip("obrigatório", Color::Warning, cx))
                            })
                            .when_some(parameter.description.clone(), |this, description| {
                                this.child(
                                    Label::new(description)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .truncate(),
                                )
                            })
                    }),
                )
            })
            .child(Self::section_label("RESPOSTAS"))
            .children(operation.responses.iter().map(|response| {
                let status: u16 = response.status.parse().unwrap_or(0);
                h_flex()
                    .gap_2()
                    .child(chip(
                        response.status.clone(),
                        if status == 0 {
                            Color::Muted
                        } else {
                            status_color(status)
                        },
                        cx,
                    ))
                    .child(
                        Label::new(response.description.clone().unwrap_or_default())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
            }))
            .into_any_element()
    }

    fn render_request_content(&self, cx: &mut Context<Self>) -> AnyElement {
        let content = match self.request_tab {
            RequestTab::Params => self.render_params(cx),
            RequestTab::Headers => self.render_headers(cx),
            RequestTab::Body => {
                return div()
                    .flex_1()
                    .min_h_0()
                    .px_3()
                    .py_2()
                    .child(self.render_body(cx))
                    .into_any_element();
            }
            RequestTab::Auth => self.render_auth(cx),
            RequestTab::Docs => self.render_docs(cx),
        };
        div()
            .id("api-request-content")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_3()
            .py_2()
            .child(content)
            .into_any_element()
    }

    fn render_response(&self, cx: &mut Context<Self>) -> AnyElement {
        let header = h_flex().px_3().py_2().gap_2().child(
            Label::new("Resposta")
                .size(LabelSize::Small)
                .weight(FontWeight::SEMIBOLD),
        );
        let Some(exchange) = self.exchange.as_ref() else {
            let hint = if self.send_task.is_some() {
                "Enviando…"
            } else {
                "Envie a request (⌘↵) para ver a resposta aqui."
            };
            return v_flex()
                .flex_1()
                .min_h_0()
                .child(header)
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(Label::new(hint).size(LabelSize::Small).color(Color::Muted)),
                )
                .into_any_element();
        };

        let tab = self.response_tab;
        let header = header
            .when_some(exchange.response.as_ref(), |this, response| {
                let status_text = match &response.reason {
                    Some(reason) => format!("{} {reason}", response.status),
                    None => response.status.to_string(),
                };
                this.child(chip(status_text, status_color(response.status), cx))
                    .child(
                        Label::new(format!("{} ms", response.elapsed.as_millis()))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(format!(
                            "· {}{}",
                            human_size(response.body.len()),
                            if response.truncated { " (cortado)" } else { "" }
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    )
            })
            .when(exchange.error.is_some(), |this| {
                this.child(chip("falhou", Color::Error, cx))
            })
            .child(
                Label::new(format!("· {}", exchange.sent_at.format("%H:%M:%S")))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(div().flex_1())
            .child(self.render_tab_button(
                "api-response-body",
                "Body".to_string(),
                tab == ResponseTab::Body,
                |this, _| this.response_tab = ResponseTab::Body,
                cx,
            ))
            .child(self.render_tab_button(
                "api-response-headers",
                format!(
                    "Headers {}",
                    exchange
                        .response
                        .as_ref()
                        .map_or(0, |response| response.headers.len())
                ),
                tab == ResponseTab::Headers,
                |this, _| this.response_tab = ResponseTab::Headers,
                cx,
            ))
            .child(self.render_tab_button(
                "api-response-timeline",
                "Timeline".to_string(),
                tab == ResponseTab::Timeline,
                |this, _| this.response_tab = ResponseTab::Timeline,
                cx,
            ))
            .child(
                IconButton::new("api-copy-body", IconName::Copy)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text("Copiar o body da resposta"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        let text = this.response_editor.read(cx).text(cx);
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    })),
            );

        let content: AnyElement = match tab {
            ResponseTab::Body => v_flex()
                .flex_1()
                .min_h_0()
                .gap_1()
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .p_2()
                        .rounded_md()
                        .border_1()
                        .border_color(cx.theme().colors().border)
                        .bg(cx.theme().colors().editor_background)
                        .child(self.response_editor.clone()),
                )
                .child(render_validation(
                    exchange.validation.as_ref(),
                    exchange.response_schema_name.as_deref(),
                    "Resposta confere com",
                    cx,
                ))
                .when_some(exchange.error.clone(), |this, error| {
                    this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
                })
                .into_any_element(),
            ResponseTab::Headers => v_flex()
                .id("api-response-headers-list")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .gap_1()
                .children(exchange.response.iter().flat_map(|response| {
                    response.headers.iter().map(|(name, value)| {
                        h_flex()
                            .gap_3()
                            .child(
                                div().w(px(220.)).child(
                                    Label::new(name.clone())
                                        .size(LabelSize::Small)
                                        .buffer_font(cx)
                                        .color(Color::Accent),
                                ),
                            )
                            .child(
                                Label::new(value.clone())
                                    .size(LabelSize::Small)
                                    .buffer_font(cx),
                            )
                    })
                }))
                .into_any_element(),
            ResponseTab::Timeline => self.render_timeline(exchange, cx),
        };

        v_flex()
            .flex_1()
            .min_h_0()
            .child(header)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .px_3()
                    .pb_3()
                    .child(content),
            )
            .into_any_element()
    }

    fn render_timeline(&self, exchange: &Exchange, cx: &mut Context<Self>) -> AnyElement {
        let steps = exchange.timeline.clone();
        let request = exchange.request.clone();
        v_flex()
            .id("api-timeline")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap_2()
            .when(steps.len() > 1, |this| {
                this.child(
                    Label::new("O token não servia: o Asylum renovou o login e repetiu a request.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .children(steps.into_iter().enumerate().map(|(index, step)| {
                h_flex()
                    .gap_2()
                    .child(
                        Label::new(format!("{}", index + 1))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(method_label(&step.method, cx))
                    .child(
                        div().w(px(260.)).child(
                            Label::new(step.path)
                                .size(LabelSize::Small)
                                .buffer_font(cx)
                                .truncate(),
                        ),
                    )
                    .child(match step.status {
                        Some(status) => {
                            chip(status.to_string(), status_color(status), cx).into_any_element()
                        }
                        None => chip("erro", Color::Error, cx).into_any_element(),
                    })
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(step.note)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                    )
                    .child(
                        Label::new(format!("{} ms", step.elapsed.as_millis()))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
            }))
            .when_some(exchange.error.clone(), |this, error| {
                this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
            })
            .when_some(request, |this, request| {
                let curl = request.curl();
                this.child(
                    h_flex()
                        .pt_2()
                        .child(Self::section_label("REQUEST ENVIADA"))
                        .child(div().flex_1())
                        .child(
                            Button::new("api-copy-curl", "Copiar como cURL")
                                .style(ButtonStyle::Outlined)
                                .label_size(LabelSize::Small)
                                .start_icon(Icon::new(IconName::Terminal).size(IconSize::XSmall))
                                .tooltip(Tooltip::text("As credenciais saem mascaradas"))
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(curl.clone()));
                                }),
                        ),
                )
                .child(
                    Label::new(format!("{} {}", request.method, request.url))
                        .size(LabelSize::Small)
                        .buffer_font(cx),
                )
                .children(request.headers.iter().map(|(name, value)| {
                    Label::new(format!("{name}: {value}"))
                        .size(LabelSize::Small)
                        .buffer_font(cx)
                        .color(Color::Muted)
                }))
            })
            .into_any_element()
    }
}

fn render_validation(
    validation: Option<&Validation>,
    schema_name: Option<&str>,
    valid_prefix: &str,
    _cx: &App,
) -> AnyElement {
    let Some(validation) = validation else {
        return div().into_any_element();
    };
    let schema = schema_name.unwrap_or("o schema do spec");
    match validation {
        Validation::Valid => h_flex()
            .gap_1()
            .child(
                Icon::new(IconName::Check)
                    .size(IconSize::XSmall)
                    .color(Color::Success),
            )
            .child(
                Label::new(format!("{valid_prefix} {schema}"))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .into_any_element(),
        Validation::Invalid { issues, more } => v_flex()
            .gap_0p5()
            .children(issues.iter().take(4).map(|issue| {
                h_flex()
                    .gap_1()
                    .child(
                        Icon::new(IconName::Warning)
                            .size(IconSize::XSmall)
                            .color(Color::Warning),
                    )
                    .child(
                        Label::new(if issue.location.is_empty() {
                            issue.message.clone()
                        } else {
                            format!("{}: {}", issue.location, issue.message)
                        })
                        .size(LabelSize::XSmall)
                        .color(Color::Warning)
                        .truncate(),
                    )
            }))
            .when(issues.len() > 4 || *more > 0, |this| {
                this.child(
                    Label::new(format!(
                        "e mais {} fora de {schema}",
                        issues.len().saturating_sub(4) + more
                    ))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
                )
            })
            .into_any_element(),
        Validation::Unchecked { reason } => Label::new(format!("Sem validação: {reason}"))
            .size(LabelSize::XSmall)
            .color(Color::Muted)
            .truncate()
            .into_any_element(),
    }
}

fn set_json_language(
    languages: &Arc<LanguageRegistry>,
    editor: &Entity<Editor>,
    cx: &mut Context<ApiRequestView>,
) {
    let languages = languages.clone();
    let editor = editor.clone();
    cx.spawn(async move |_this, cx| {
        let json = languages.language_for_name("JSON").await?;
        editor.update(cx, |editor, cx| {
            if let Some(buffer) = editor.buffer().read(cx).as_singleton() {
                buffer.update(cx, |buffer, cx| buffer.set_language(Some(json), cx));
            }
        });
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

impl Render for ApiRequestView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("ApiRequestView")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &SendRequest, window, cx| this.send(window, cx)))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_request_bar(cx))
            .child(self.render_meta(cx))
            .child(self.render_request_tabs(cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_request_content(cx)),
            )
            .child(div().h_px().w_full().bg(cx.theme().colors().border))
            .child(self.render_response(cx))
    }
}

impl Focusable for ApiRequestView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for ApiRequestView {}

impl Item for ApiRequestView {
    type Event = ItemEvent;

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        let method = self
            .operation(cx)
            .map(|operation| operation.method.clone())
            .unwrap_or_default();
        h_flex()
            .gap_1p5()
            .child(method_label(&method, cx))
            .child(
                Label::new(self.tab_content_text(0, cx))
                    .single_line()
                    .color(params.text_color()),
            )
            .into_any_element()
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        if let Some(name) = &self.saved_name {
            return name.clone().into();
        }
        self.operation(cx)
            .map(|operation| operation.title())
            .unwrap_or_else(|| self.operation_key.clone())
            .into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::ArrowRightLeft).color(Color::Muted))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

/// Opens the operation, or focuses the tab that already shows it.
pub fn open(
    workspace: WeakEntity<Workspace>,
    collection: Entity<Collection>,
    operation_key: String,
    saved: Option<(String, String, RequestDraft)>,
    window: &mut Window,
    cx: &mut App,
) {
    workspace
        .update(cx, |workspace, cx| {
            let saved_id = saved.as_ref().map(|(id, _, _)| id.clone());
            let existing = workspace.items_of_type::<ApiRequestView>(cx).find(|view| {
                let view = view.read(cx);
                view.collection == collection
                    && view.operation_key == operation_key
                    && view.saved_id == saved_id
            });
            if let Some(existing) = existing {
                workspace.activate_item(&existing, true, true, window, cx);
                return;
            }
            let languages = workspace.app_state().languages.clone();
            let view = cx.new(|cx| {
                ApiRequestView::new(collection, operation_key, saved, languages, window, cx)
            });
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        })
        .log_err();
}
