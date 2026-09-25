//! One operation open as a workspace tab: the request on top, the response below.

use crate::{
    SendRequest,
    collection::{Collection, Exchange},
    config::{self, BodyKind, CaptureRule, FormField, HeaderEntry, ParamEntry, RequestDraft},
    example,
    schema::Validation,
    send::{self, HeaderOrigin, REFRESH_TOKEN_VARIABLE, TOKEN_VARIABLE},
    spec::{Operation, ParameterLocation},
    vars::{self, SECRET_PREFIX},
};
use editor::{Editor, EditorEvent, HighlightKey, MultiBufferOffset};
use gpui::{
    AnyElement, ClipboardItem, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    HighlightStyle, Hsla, Subscription, Task, WeakEntity,
};
use language::LanguageRegistry;
use std::{sync::Arc, time::Duration};
use ui::{Checkbox, Tooltip, prelude::*};
use ui_input::{ErasedEditorEvent, InputField};
use util::ResultExt as _;
use workspace::{
    Item, OpenOptions, Workspace,
    item::{ItemEvent, TabContentParams},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum RequestTab {
    Params,
    Headers,
    Body,
    Auth,
    Captures,
    Docs,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ResponseTab {
    Body,
    Headers,
    Cookies,
    Timeline,
}

struct Row {
    name: Entity<InputField>,
    value: Entity<InputField>,
    enabled: bool,
}

struct FormRow {
    name: Entity<InputField>,
    value: Entity<InputField>,
    is_file: bool,
    enabled: bool,
}

struct CaptureRow {
    path: Entity<InputField>,
    variable: Entity<InputField>,
}

/// Capture rules are written to the collection file this long after the last keystroke.
const CAPTURE_SAVE_DELAY: Duration = Duration::from_millis(600);

pub struct ApiRequestView {
    workspace: WeakEntity<Workspace>,
    collection: Entity<Collection>,
    pub(crate) operation_key: String,
    pub(crate) saved_id: Option<String>,
    saved_name: Option<String>,
    focus_handle: FocusHandle,
    url_input: Entity<InputField>,
    path_params: Vec<(String, Entity<InputField>)>,
    query_rows: Vec<Row>,
    header_rows: Vec<Row>,
    form_rows: Vec<FormRow>,
    capture_rows: Vec<CaptureRow>,
    capture_save_task: Option<Task<()>>,
    /// What happened to the last "Salvar como exemplo".
    example_status: Option<(SharedString, bool)>,
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

/// The editor behind an input, to color the placeholders in it.
fn input_editor(input: &Entity<InputField>, cx: &App) -> Option<Entity<Editor>> {
    input
        .read(cx)
        .editor()
        .as_any()
        .downcast_ref::<Entity<Editor>>()
        .cloned()
}

fn is_sensitive_variable(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    name.starts_with(SECRET_PREFIX)
        || name == TOKEN_VARIABLE
        || name == REFRESH_TOKEN_VARIABLE
        || ["token", "password", "senha", "secret", "key"]
            .iter()
            .any(|word| lower.contains(word))
}

/// What a placeholder resolves to right now, as it can be shown on screen.
fn resolved_variable(collection: &Collection, name: &str) -> Option<String> {
    if name.starts_with('$') {
        return vars::dynamic_value(name).map(|_| "gerado a cada envio".to_string());
    }
    let value = collection.variable(name)?;
    Some(if is_sensitive_variable(name) {
        send::mask(&value)
    } else {
        value
    })
}

/// Colors `{{…}}` in the editor: accent when something defines it, warning when not.
fn highlight_placeholders(editor: &Entity<Editor>, collection: &Collection, cx: &mut App) {
    let text = editor.read(cx).text(cx);
    let (mut known, mut missing) = (Vec::new(), Vec::new());
    for range in vars::placeholder_ranges(&text) {
        let name = text[range.start + 2..range.end - 2].trim();
        if resolved_variable(collection, name).is_some() {
            known.push(range);
        } else {
            missing.push(range);
        }
    }
    let accent = cx.theme().colors().text_accent;
    let warning = cx.theme().status().warning;
    editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let anchors = |ranges: Vec<std::ops::Range<usize>>| {
            ranges
                .into_iter()
                .map(|range| {
                    snapshot.anchor_before(MultiBufferOffset(range.start))
                        ..snapshot.anchor_after(MultiBufferOffset(range.end))
                })
                .collect::<Vec<_>>()
        };
        for (key, ranges, color) in [
            (HighlightKey::ApiVariable, known, accent),
            (HighlightKey::ApiVariableMissing, missing, warning),
        ] {
            if ranges.is_empty() {
                editor.clear_highlights(key, cx);
            } else {
                editor.highlight_text(
                    key,
                    anchors(ranges),
                    HighlightStyle {
                        color: Some(color),
                        background_color: Some(color.opacity(0.12)),
                        ..HighlightStyle::default()
                    },
                    cx,
                );
            }
        }
    });
}

/// A header or URL template as labels, with its placeholders set off like in the editors.
fn render_template(text: &str, collection: &Collection, muted: bool, cx: &App) -> AnyElement {
    let mut row = h_flex().min_w_0().overflow_hidden();
    let mut last = 0;
    let text_color = if muted { Color::Muted } else { Color::Default };
    for range in vars::placeholder_ranges(text) {
        if range.start > last {
            row = row.child(
                Label::new(text[last..range.start].to_string())
                    .size(LabelSize::Small)
                    .buffer_font(cx)
                    .color(text_color),
            );
        }
        let name = text[range.start + 2..range.end - 2].trim();
        let color = if resolved_variable(collection, name).is_some() {
            Color::Accent
        } else {
            Color::Warning
        };
        row = row.child(
            div()
                .px_0p5()
                .rounded_sm()
                .bg(color.color(cx).opacity(0.12))
                .child(
                    Label::new(text[range.clone()].to_string())
                        .size(LabelSize::Small)
                        .buffer_font(cx)
                        .color(color),
                ),
        );
        last = range.end;
    }
    if last < text.len() {
        row = row.child(
            Label::new(text[last..].to_string())
                .size(LabelSize::Small)
                .buffer_font(cx)
                .color(text_color)
                .truncate(),
        );
    }
    row.into_any_element()
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
        workspace: WeakEntity<Workspace>,
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
            workspace,
            collection: collection.clone(),
            operation_key,
            saved_id,
            saved_name,
            focus_handle: cx.focus_handle(),
            url_input,
            path_params: Vec::new(),
            query_rows: Vec::new(),
            header_rows: Vec::new(),
            form_rows: Vec::new(),
            capture_rows: Vec::new(),
            capture_save_task: None,
            example_status: None,
            disabled_inherited: draft.disabled_inherited.clone(),
            body_editor: body_editor.clone(),
            response_editor,
            request_tab: if draft.body.is_some() || !draft.form.is_empty() {
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
        for field in &draft.form {
            let row = this.new_form_row(field, window, cx);
            this.form_rows.push(row);
        }
        let rules = collection.read(cx).capture_rules(&this.operation_key);
        for rule in rules {
            let row = this.new_capture_row(&rule.path, &rule.variable, window, cx);
            this.capture_rows.push(row);
        }
        this.watch_input(&this.url_input.clone(), window, cx);
        this._subscriptions.push(
            cx.subscribe(&body_editor, |this, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    this.body_changed(cx);
                }
            }),
        );
        // Switching environments or logging in changes which placeholders have values.
        this._subscriptions
            .push(cx.observe(&collection, |this, _, cx| {
                this.highlight_placeholders(cx);
                cx.notify();
            }));
        this.body_changed(cx);
        this.highlight_placeholders(cx);
        this
    }

    fn placeholder_editors(&self, cx: &App) -> Vec<Entity<Editor>> {
        let inputs = std::iter::once(&self.url_input)
            .chain(self.path_params.iter().map(|(_, input)| input))
            .chain(self.query_rows.iter().map(|row| &row.value))
            .chain(self.header_rows.iter().map(|row| &row.value))
            .chain(
                self.form_rows
                    .iter()
                    .filter(|row| !row.is_file)
                    .map(|row| &row.value),
            );
        std::iter::once(self.body_editor.clone())
            .chain(inputs.filter_map(|input| input_editor(input, cx)))
            .collect()
    }

    fn highlight_placeholders(&self, cx: &mut Context<Self>) {
        let editors = self.placeholder_editors(cx);
        // `update` hands out the collection and the app together, which the editors need.
        self.collection.update(cx, |collection, cx| {
            for editor in &editors {
                highlight_placeholders(editor, collection, cx);
            }
        });
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

    fn new_form_row(
        &mut self,
        field: &FormField,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> FormRow {
        FormRow {
            name: self.new_input("campo", &field.name, window, cx),
            value: self.new_input(
                if field.is_file {
                    "caminho do arquivo"
                } else {
                    "valor"
                },
                &field.value,
                window,
                cx,
            ),
            is_file: field.is_file,
            enabled: field.enabled,
        }
    }

    fn new_capture_row(
        &mut self,
        path: &str,
        variable: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> CaptureRow {
        let mut make =
            |placeholder: &str, text: &str, window: &mut Window, cx: &mut Context<Self>| {
                let input = cx.new(|cx| InputField::new(window, cx, placeholder));
                let editor = input.read(cx).editor().clone();
                editor.set_text(text, window, cx);
                let this = cx.weak_entity();
                self._subscriptions.push(editor.subscribe(
                    Box::new(move |event, _window, cx| {
                        if event == ErasedEditorEvent::BufferEdited {
                            this.update(cx, |this, cx| this.captures_changed(cx))
                                .log_err();
                        }
                    }),
                    window,
                    cx,
                ));
                input
            };
        CaptureRow {
            path: make("$.id", path, window, cx),
            variable: make("variável", variable, window, cx),
        }
    }

    fn capture_rules(&self, cx: &App) -> Vec<CaptureRule> {
        self.capture_rows
            .iter()
            .map(|row| CaptureRule {
                operation: self.operation_key.clone(),
                path: row.path.read(cx).text(cx).trim().to_string(),
                variable: row
                    .variable
                    .read(cx)
                    .text(cx)
                    .trim()
                    .trim_start_matches("{{")
                    .trim_end_matches("}}")
                    .to_string(),
            })
            .filter(|rule| !rule.path.is_empty() || !rule.variable.is_empty())
            .collect()
    }

    fn captures_changed(&mut self, cx: &mut Context<Self>) {
        self.capture_save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CAPTURE_SAVE_DELAY).await;
            this.update(cx, |this, cx| this.save_captures(cx)).log_err();
        }));
        cx.notify();
    }

    fn save_captures(&mut self, cx: &mut Context<Self>) {
        self.capture_save_task = None;
        let rules = self.capture_rules(cx);
        let key = self.operation_key.clone();
        self.collection.update(cx, |collection, cx| {
            collection.set_capture_rules(&key, rules, cx)
        });
    }

    fn body_kind(&self, cx: &App) -> BodyKind {
        self.operation(cx)
            .and_then(|operation| operation.request_body.as_ref())
            .map(|body| BodyKind::for_content_type(&body.content_type))
            .unwrap_or(BodyKind::Json)
    }

    fn pick_file(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Anexar".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Some(path) = paths.await??.and_then(|paths| paths.into_iter().next()) else {
                return anyhow::Ok(());
            };
            this.update_in(cx, |this, window, cx| {
                if let Some(row) = this.form_rows.get(index) {
                    let editor = row.value.read(cx).editor().clone();
                    editor.set_text(&path.to_string_lossy(), window, cx);
                }
                this.store_draft(cx);
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
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
                    this.update(cx, |this, cx| {
                        this.highlight_placeholders(cx);
                        this.store_draft(cx);
                    })
                    .log_err();
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
            form: self
                .form_rows
                .iter()
                .map(|row| FormField {
                    name: row.name.read(cx).text(cx).trim().to_string(),
                    value: row.value.read(cx).text(cx),
                    is_file: row.is_file,
                    enabled: row.enabled,
                })
                .collect(),
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
        self.highlight_placeholders(cx);
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
        if self.capture_save_task.is_some() {
            self.save_captures(cx);
        }
        self.example_status = None;
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

    /// `local → http://127.0.0.1:8765`: the environment the URL is sent to.
    fn environment_hint(&self, cx: &App) -> Option<String> {
        let collection = self.collection.read(cx);
        let environment = collection.active_environment()?;
        let base_url = environment.get("baseUrl").filter(|url| !url.is_empty());
        Some(match base_url {
            Some(base_url) => format!("{} → {base_url}", environment.name),
            None => format!("{} → sem baseUrl", environment.name),
        })
    }

    /// Every placeholder the request uses, in the order they appear, once each.
    fn used_placeholders(&self, cx: &App) -> Vec<String> {
        let draft = self.draft(cx);
        let mut texts: Vec<String> = vec![draft.url.clone()];
        texts.extend(draft.path_params.iter().map(|entry| entry.value.clone()));
        texts.extend(
            draft
                .query
                .iter()
                .filter(|entry| entry.enabled)
                .map(|entry| entry.value.clone()),
        );
        texts.extend(
            self.inherited_headers(cx)
                .into_iter()
                .filter(|header| {
                    header.enabled
                        && !draft
                            .disabled_inherited
                            .iter()
                            .any(|name| name.eq_ignore_ascii_case(&header.name))
                })
                .map(|header| header.value),
        );
        texts.extend(
            draft
                .headers
                .iter()
                .filter(|entry| entry.enabled)
                .map(|entry| entry.value.clone()),
        );
        texts.extend(draft.body.clone());
        let mut names: Vec<String> = Vec::new();
        for text in texts {
            for name in vars::placeholder_names(&text) {
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        names
    }

    /// Opens the spec at this operation.
    fn open_in_spec(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(operation) = self.operation(cx).cloned() else {
            return;
        };
        let spec_path = self.collection.read(cx).spec_path();
        let fs = self.collection.read(cx).fs();
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_, cx| {
            let text = fs.load(&spec_path).await.unwrap_or_default();
            let row = operation_row(&text, &operation.path, &operation.method);
            let item = workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.open_abs_path(spec_path, OpenOptions::default(), window, cx)
                })?
                .await?;
            if let Some(editor) = item.downcast::<Editor>() {
                editor.update_in(cx, |editor, window, cx| {
                    editor.go_to_singleton_buffer_point(language::Point::new(row, 0), window, cx);
                })?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
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
                    .relative()
                    .capture_action(cx.listener(
                        |this, _: &editor::actions::Newline, window, cx| {
                            this.send(window, cx);
                        },
                    ))
                    .child(self.url_input.clone())
                    .when_some(self.environment_hint(cx), |this, hint| {
                        this.child(
                            div()
                                .absolute()
                                .top_0()
                                .bottom_0()
                                .right_2()
                                .max_w(px(320.))
                                .flex()
                                .items_center()
                                .child(
                                    Label::new(hint)
                                        .size(LabelSize::XSmall)
                                        .buffer_font(cx)
                                        .color(Color::Muted)
                                        .truncate(),
                                ),
                        )
                    }),
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

    fn render_meta(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
        let variables: Vec<(String, Option<String>)> = self
            .used_placeholders(cx)
            .into_iter()
            .map(|name| {
                let value = resolved_variable(collection, &name);
                (name, value)
            })
            .collect();
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
                    )
                    .child(div().flex_1())
                    .when(operation.is_some(), |this| {
                        this.child(
                            Button::new("api-open-in-spec", "Ver na spec")
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::XSmall)
                                .color(Color::Accent)
                                .on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.open_in_spec(window, cx)
                                    }),
                                ),
                        )
                    }),
            )
            .when(!variables.is_empty(), |this| {
                this.child(
                    h_flex()
                        .flex_wrap()
                        .gap_1()
                        .children(variables.into_iter().map(|(name, value)| {
                            let color = if value.is_some() {
                                Color::Accent
                            } else {
                                Color::Warning
                            };
                            h_flex()
                                .gap_1()
                                .px_1()
                                .rounded_sm()
                                .bg(color.color(cx).opacity(0.1))
                                .child(
                                    Label::new(format!("{{{{{name}}}}}"))
                                        .size(LabelSize::XSmall)
                                        .buffer_font(cx)
                                        .color(color),
                                )
                                .child(
                                    Label::new(value.unwrap_or_else(|| "sem valor".to_string()))
                                        .size(LabelSize::XSmall)
                                        .buffer_font(cx)
                                        .color(Color::Muted),
                                )
                        })),
                )
            })
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
                "api-tab-captures",
                if self.capture_rows.is_empty() {
                    "Capturas".to_string()
                } else {
                    format!("Capturas {}", self.capture_rows.len())
                },
                tab == RequestTab::Captures,
                |this, _| this.request_tab = RequestTab::Captures,
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
                    .child(div().flex_1().min_w_0().child(render_template(
                        &header.value,
                        self.collection.read(cx),
                        !enabled,
                        cx,
                    )))
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

    fn render_form_body(&self, kind: BodyKind, cx: &mut Context<Self>) -> AnyElement {
        let content_type = self
            .operation(cx)
            .and_then(|operation| operation.request_body.as_ref())
            .map(|body| body.content_type.clone())
            .unwrap_or_default();
        v_flex()
            .id("api-form-body")
            .size_full()
            .overflow_y_scroll()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(chip(content_type, Color::Muted, cx))
                    .child(
                        Label::new(if kind == BodyKind::Multipart {
                            "Campos de arquivo vão com o conteúdo do arquivo; o boundary é gerado no envio."
                        } else {
                            "Os campos vão codificados como formulário."
                        })
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    ),
            )
            .children(self.form_rows.iter().enumerate().map(|(index, row)| {
                h_flex()
                    .gap_2()
                    .child(
                        Checkbox::new(("api-form-enabled", index), row.enabled.into()).on_click(
                            cx.listener(move |this, _, _, cx| {
                                if let Some(row) = this.form_rows.get_mut(index) {
                                    row.enabled = !row.enabled;
                                }
                                this.store_draft(cx);
                            }),
                        ),
                    )
                    .child(div().w(px(180.)).child(row.name.clone()))
                    .child(div().flex_1().child(row.value.clone()))
                    .when(kind == BodyKind::Multipart, |this| {
                        this.child(
                            Button::new(
                                ("api-form-kind", index),
                                if row.is_file { "Arquivo" } else { "Texto" },
                            )
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::XSmall)
                            .tooltip(Tooltip::text("Alternar entre texto e arquivo"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(row) = this.form_rows.get_mut(index) {
                                    row.is_file = !row.is_file;
                                }
                                this.store_draft(cx);
                            })),
                        )
                    })
                    .when(row.is_file, |this| {
                        this.child(
                            IconButton::new(("api-form-pick", index), IconName::FolderOpen)
                                .icon_size(IconSize::Small)
                                .icon_color(Color::Muted)
                                .tooltip(Tooltip::text("Escolher arquivo"))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.pick_file(index, window, cx)
                                })),
                        )
                    })
                    .child(
                        IconButton::new(("api-form-remove", index), IconName::Trash)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if index < this.form_rows.len() {
                                    this.form_rows.remove(index);
                                }
                                this.store_draft(cx);
                            })),
                    )
            }))
            .child(
                Button::new("api-form-add", "Adicionar campo")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .color(Color::Accent)
                    .start_icon(
                        Icon::new(IconName::Plus)
                            .size(IconSize::XSmall)
                            .color(Color::Accent),
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        let row = this.new_form_row(
                            &FormField {
                                enabled: true,
                                ..FormField::default()
                            },
                            window,
                            cx,
                        );
                        window.focus(&row.name.focus_handle(cx), cx);
                        this.form_rows.push(row);
                        this.store_draft(cx);
                    })),
            )
            .into_any_element()
    }

    fn render_captures(&self, cx: &mut Context<Self>) -> AnyElement {
        let collection = self.collection.read(cx);
        let login = collection
            .file
            .auth
            .login
            .clone()
            .filter(|login| login.operation == self.operation_key);
        v_flex()
            .gap_2()
            .child(
                Label::new(
                    "Depois de uma resposta 2xx, cada caminho vira uma variável que as outras requests usam como {{nome}}.",
                )
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            )
            .when_some(login, |this, login| {
                this.child(
                    h_flex()
                        .gap_2()
                        .p_2()
                        .rounded_md()
                        .bg(Color::Success.color(cx).opacity(0.08))
                        .child(
                            Icon::new(IconName::UserCheck)
                                .size(IconSize::Small)
                                .color(Color::Success),
                        )
                        .child(
                            Label::new(format!(
                                "Esta é a request de login: {} → {{{{token}}}}{}",
                                login.token_path,
                                login
                                    .refresh_token_path
                                    .map(|path| format!(", {path} → {{{{refreshToken}}}}"))
                                    .unwrap_or_default()
                            ))
                            .size(LabelSize::Small)
                            .buffer_font(cx),
                        ),
                )
            })
            .children(self.capture_rows.iter().enumerate().map(|(index, row)| {
                let variable = row
                    .variable
                    .read(cx)
                    .text(cx)
                    .trim()
                    .trim_start_matches("{{")
                    .trim_end_matches("}}")
                    .to_string();
                let current = (!variable.is_empty())
                    .then(|| collection.captured_value(&variable))
                    .flatten()
                    .map(|value| {
                        if is_sensitive_variable(&variable) {
                            send::mask(value)
                        } else {
                            value.to_string()
                        }
                    });
                let path_valid = crate::jsonpath::is_valid(&row.path.read(cx).text(cx));
                h_flex()
                    .gap_2()
                    .child(div().w(px(220.)).child(row.path.clone()))
                    .child(
                        Icon::new(IconName::ArrowRight)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(div().w(px(180.)).child(row.variable.clone()))
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(if !path_valid {
                                "caminho não suportado".to_string()
                            } else {
                                current.unwrap_or_else(|| "ainda sem valor".to_string())
                            })
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(if path_valid { Color::Muted } else { Color::Warning })
                            .truncate(),
                        ),
                    )
                    .child(
                        IconButton::new(("api-capture-remove", index), IconName::Trash)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if index < this.capture_rows.len() {
                                    this.capture_rows.remove(index);
                                }
                                this.save_captures(cx);
                                cx.notify();
                            })),
                    )
            }))
            .child(
                Button::new("api-capture-add", "Adicionar captura")
                    .style(ButtonStyle::Subtle)
                    .label_size(LabelSize::Small)
                    .color(Color::Accent)
                    .start_icon(
                        Icon::new(IconName::Plus)
                            .size(IconSize::XSmall)
                            .color(Color::Accent),
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        let row = this.new_capture_row("", "", window, cx);
                        window.focus(&row.path.focus_handle(cx), cx);
                        this.capture_rows.push(row);
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    fn render_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let kind = self.body_kind(cx);
        if kind.is_form() {
            return self.render_form_body(kind, cx);
        }
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
                                .on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.fill_example(window, cx)
                                    }),
                                ),
                        )
                    }),
            )
            .when(!is_json, |this| {
                this.child(
                    Label::new("Este body não é JSON: o texto vai como está.")
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
                    .capture_action(cx.listener(
                        |this, _: &editor::actions::NewlineBelow, window, cx| {
                            this.send(window, cx);
                        },
                    ))
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
            RequestTab::Captures => self.render_captures(cx),
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
                "api-response-cookies",
                if exchange.cookies_set.is_empty() {
                    "Cookies".to_string()
                } else {
                    format!("Cookies {}", exchange.cookies_set.len())
                },
                tab == ResponseTab::Cookies,
                |this, _| this.response_tab = ResponseTab::Cookies,
                cx,
            ))
            .child(self.render_tab_button(
                "api-response-timeline",
                "Timeline".to_string(),
                tab == ResponseTab::Timeline,
                |this, _| this.response_tab = ResponseTab::Timeline,
                cx,
            ))
            .when(
                exchange
                    .response
                    .as_ref()
                    .is_some_and(|response| response.json().is_some()),
                |this| {
                    this.child(
                        IconButton::new("api-save-example", IconName::FileCode)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text("Salvar esta resposta como exemplo no spec"))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.save_as_example(window, cx)),
                            ),
                    )
                },
            )
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

        let content: AnyElement =
            match tab {
                ResponseTab::Body => v_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_1()
                    .children(exchange.captures.iter().map(|capture| {
                        let found = capture.value.is_some();
                        h_flex()
                        .gap_1p5()
                        .child(
                            Icon::new(if found { IconName::Check } else { IconName::Warning })
                                .size(IconSize::XSmall)
                                .color(if found { Color::Success } else { Color::Warning }),
                        )
                        .child(
                            Label::new(match &capture.value {
                                Some(value) => format!(
                                    "Capturado: {} → {{{{{}}}}} = {}",
                                    capture.path,
                                    capture.variable,
                                    if is_sensitive_variable(&capture.variable) {
                                        send::mask(value)
                                    } else {
                                        value.clone()
                                    }
                                ),
                                None => format!(
                                    "{} não existe nesta resposta; {{{{{}}}}} ficou como estava",
                                    capture.path, capture.variable
                                ),
                            })
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(if found { Color::Muted } else { Color::Warning })
                            .truncate(),
                        )
                    }))
                    .when_some(self.example_status.clone(), |this, (message, ok)| {
                        this.child(Label::new(message).size(LabelSize::XSmall).color(if ok {
                            Color::Success
                        } else {
                            Color::Warning
                        }))
                    })
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
                ResponseTab::Cookies => self.render_cookies(exchange, cx),
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

    fn render_cookies(&self, exchange: &Exchange, cx: &mut Context<Self>) -> AnyElement {
        let jar = self.collection.read(cx).cookies.cookies.clone();
        let collection = self.collection.clone();
        let now = chrono::Utc::now().timestamp();
        let describe = |cookie: &crate::cookies::Cookie| {
            let mut parts = vec![
                if cookie.host_only {
                    cookie.domain.clone()
                } else {
                    format!(".{}", cookie.domain)
                },
                cookie.path.clone(),
            ];
            parts.push(match cookie.expires_at {
                Some(expires_at) if expires_at <= now => "apagado".to_string(),
                Some(expires_at) => chrono::DateTime::from_timestamp(expires_at, 0)
                    .map(|expires| {
                        format!(
                            "expira {}",
                            expires.with_timezone(&chrono::Local).format("%d/%m %H:%M")
                        )
                    })
                    .unwrap_or_default(),
                None => "sessão".to_string(),
            });
            if cookie.http_only {
                parts.push("HttpOnly".to_string());
            }
            if cookie.secure {
                parts.push("Secure".to_string());
            }
            parts.join(" · ")
        };
        let row = |cookie: &crate::cookies::Cookie, cx: &App| {
            h_flex()
                .gap_3()
                .child(
                    div().w(px(180.)).child(
                        Label::new(cookie.name.clone())
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .color(Color::Accent),
                    ),
                )
                .child(
                    div().w(px(220.)).child(
                        Label::new(send::mask(&cookie.value))
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .truncate(),
                    ),
                )
                .child(
                    Label::new(describe(cookie))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
        };
        v_flex()
            .id("api-response-cookies-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap_1()
            .child(Self::section_label("DEFINIDOS NESTA RESPOSTA"))
            .when(exchange.cookies_set.is_empty(), |this| {
                this.child(
                    Label::new("Nenhum Set-Cookie.")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .children(exchange.cookies_set.iter().map(|cookie| row(cookie, cx)))
            .child(
                h_flex()
                    .pt_2()
                    .child(Self::section_label("GUARDADOS NA COLEÇÃO"))
                    .child(div().flex_1())
                    .when(!jar.is_empty(), |this| {
                        this.child(
                            Button::new("api-clear-cookies", "Limpar cookies")
                                .style(ButtonStyle::Outlined)
                                .label_size(LabelSize::Small)
                                .on_click(move |_, _, cx| {
                                    collection.update(cx, |collection, cx| {
                                        collection.clear_cookies(cx)
                                    });
                                }),
                        )
                    }),
            )
            .when(jar.is_empty(), |this| {
                this.child(
                    Label::new("Vazio. Os cookies das respostas voltam sozinhos nas próximas requests ao mesmo domínio.")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .children(jar.iter().map(|cookie| row(cookie, cx)))
            .into_any_element()
    }

    /// Puts the response in the spec as the example of its status, in the editor, unsaved,
    /// so the change can be looked at before it's kept.
    fn save_as_example(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(exchange) = self.exchange.as_ref() else {
            return;
        };
        let Some(response) = exchange.response.as_ref() else {
            return;
        };
        let Some(body) = response.json() else {
            return;
        };
        let Some(operation) = self.operation(cx).cloned() else {
            return;
        };
        let status = response.status;
        let content_type = response
            .content_type()
            .map(|content_type| {
                content_type
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string()
            })
            .filter(|content_type| !content_type.is_empty())
            .or_else(|| {
                operation
                    .response_for_status(status)
                    .and_then(|response| response.content_type.clone())
            })
            .unwrap_or_else(|| "application/json".to_string());
        let spec_path = self.collection.read(cx).spec_path();
        let is_yaml = spec_path
            .extension()
            .is_some_and(|extension| extension == "yml" || extension == "yaml");
        if !is_yaml {
            self.example_status = Some((
                "Salvar como exemplo só funciona em specs YAML.".into(),
                false,
            ));
            cx.notify();
            return;
        }
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |this, cx| {
            let item = workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.open_abs_path(spec_path.clone(), OpenOptions::default(), window, cx)
                })?
                .await?;
            let Some(editor) = item.downcast::<Editor>() else {
                return anyhow::Ok(());
            };
            let outcome = editor.update_in(cx, |editor, window, cx| {
                let text = editor.text(cx);
                let (range, replacement) = example::example_edit(
                    &text,
                    &operation.path,
                    &operation.method,
                    status,
                    &content_type,
                    &body,
                )?;
                let row = example::edited_row(&text, &range, &replacement);
                editor.edit(
                    [(
                        MultiBufferOffset(range.start)..MultiBufferOffset(range.end),
                        replacement,
                    )],
                    cx,
                );
                editor.go_to_singleton_buffer_point(language::Point::new(row, 0), window, cx);
                anyhow::Ok(())
            })?;
            let file_name = spec_path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            this.update(cx, |this, cx| {
                this.example_status = Some(match outcome {
                    Ok(()) => (
                        format!("Exemplo de {status} inserido em {file_name}; salve o arquivo para manter.")
                            .into(),
                        true,
                    ),
                    Err(error) => (format!("{error:#}").into(), false),
                });
                cx.notify();
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
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

/// The 0-based line of `method:` under `path:` in the spec's text, or of the path itself.
fn operation_row(text: &str, path: &str, method: &str) -> u32 {
    let method = format!("{}:", method.to_ascii_lowercase());
    let path_keys = [
        format!("{path}:"),
        format!("'{path}':"),
        format!("\"{path}\":"),
    ];
    let lines: Vec<&str> = text.lines().collect();
    let Some(path_row) = lines.iter().position(|line| {
        path_keys
            .iter()
            .any(|key| line.trim_start() == key.as_str())
    }) else {
        return 0;
    };
    let indent = lines[path_row].len() - lines[path_row].trim_start().len();
    for (offset, line) in lines.iter().enumerate().skip(path_row + 1) {
        let line_indent = line.len() - line.trim_start().len();
        if !line.trim().is_empty() && line_indent <= indent {
            break;
        }
        if line.trim_start().starts_with(&method) {
            return offset as u32;
        }
    }
    path_row as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_operation_in_the_spec() {
        let text = "paths:\n  /auth/:\n    get:\n      x: 1\n    post:\n      y: 2\n  /me:\n    post: {}\n";
        assert_eq!(operation_row(text, "/auth/", "POST"), 4);
        assert_eq!(operation_row(text, "/me", "POST"), 7);
        assert_eq!(operation_row(text, "/me", "GET"), 6);
        assert_eq!(operation_row(text, "/nope", "GET"), 0);
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
            let workspace_handle = workspace.weak_handle();
            let view = cx.new(|cx| {
                ApiRequestView::new(
                    workspace_handle,
                    collection,
                    operation_key,
                    saved,
                    languages,
                    window,
                    cx,
                )
            });
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        })
        .log_err();
}
