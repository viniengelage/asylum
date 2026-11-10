//! One operation open as a workspace tab: the request on top, the response below.

use crate::{
    SendRequest,
    collection::{Collection, Exchange},
    config::{self, BodyKind, CaptureRule, FormField, HeaderEntry, ParamEntry, RequestDraft},
    example,
    panel::variable_chip,
    schema::Validation,
    script::{self, Failure, LineKind, Outcome, Script, TestResult},
    send::{self, HeaderOrigin, REFRESH_TOKEN_VARIABLE, TOKEN_VARIABLE},
    spec::{Operation, ParameterLocation},
    vars::{self, SECRET_PREFIX},
};
use editor::{Editor, EditorEvent, HighlightKey, MultiBufferOffset};
use gpui::{
    AnyElement, ClipboardItem, Div, DragMoveEvent, Empty, Entity, EventEmitter, FocusHandle,
    Focusable, FontWeight, HighlightStyle, Pixels, Stateful, Subscription, Task, WeakEntity,
};
use i18n::t;
use language::LanguageRegistry;
use std::{sync::Arc, time::Duration};
use ui::section_label;
use ui::{Checkbox, EmptyState, StatusTone, Tooltip, prelude::*};
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
    Scripts,
    Docs,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ResponseTab {
    Body,
    Headers,
    Cookies,
    Tests,
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
const MIN_REQUEST_AREA_HEIGHT: Pixels = px(200.);
const MIN_RESPONSE_HEIGHT: Pixels = px(120.);
/// Until the divider is dragged, a long form doesn't push the response off screen.
const MAX_REQUEST_CARD_HEIGHT: Pixels = px(380.);

/// Dragged by the bar between the request and the response.
struct DraggedResponseDivider;

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
    highlighted_script_line: Option<usize>,
    exchange: Option<Exchange>,
    body_validation: Option<Validation>,
    send_task: Option<Task<()>>,
    /// Set once the divider is dragged; until then both halves share the space.
    response_height: Option<Pixels>,
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

pub fn status_tone(status: u16) -> StatusTone {
    match status {
        200..=299 => StatusTone::Success,
        300..=399 => StatusTone::Info,
        _ => StatusTone::Error,
    }
}

fn method_chip(method: &str, cx: &App) -> Div {
    let color = method_color(method);
    h_flex()
        .flex_none()
        .h(px(32.))
        .px_3()
        .rounded(Radius::Sm.rems())
        .bg(color.color(cx).opacity(0.12))
        .child(
            Label::new(SharedString::from(method.to_string()))
                .size(LabelSize::Small)
                .weight(FontWeight::BOLD)
                .buffer_font(cx)
                .color(color),
        )
}

fn tinted_row(tone: StatusTone, cx: &App) -> Div {
    let color = tone.color(cx);
    h_flex()
        .w_full()
        .min_h(px(40.))
        .px_3p5()
        .py_1p5()
        .gap_2p5()
        .rounded(Radius::Lg.rems())
        .bg(color.opacity(0.06))
        .border_1()
        .border_color(color.opacity(0.33))
}

fn render_script_lines(script: &Script, highlighted: Option<usize>, cx: &App) -> impl IntoElement {
    let warning = StatusTone::Warning.color(cx);
    let error = StatusTone::Error.color(cx);
    v_flex().py_1().children(script.lines.iter().map(|line| {
        let is_off = matches!(line.kind, LineKind::Off { .. });
        h_flex()
            .min_h(px(22.))
            .px_3()
            .gap_3()
            .when(is_off, |this| this.bg(warning.opacity(0.08)))
            .when(highlighted == Some(line.number), |this| {
                this.bg(error.opacity(0.12))
            })
            .child(
                div().flex_none().w(px(28.)).child(
                    Label::new(line.number.to_string())
                        .size(LabelSize::XSmall)
                        .buffer_font(cx)
                        .color(Color::Muted),
                ),
            )
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(line.text.clone())
                        .size(LabelSize::Small)
                        .buffer_font(cx)
                        .single_line(),
                ),
            )
            .map(|this| match &line.kind {
                LineKind::Off { call: Some(call) } => this.child(ui::status_chip(
                    Some(IconName::Warning),
                    t!("api_client.request.script_line_off", call = call),
                    Color::Warning,
                    cx,
                )),
                LineKind::Capture { variable, path } => this.child(
                    h_flex()
                        .flex_none()
                        .gap_1p5()
                        .child(
                            Label::new(t!(
                                "api_client.request.script_becomes_capture",
                                path = path
                            ))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                        )
                        .child(variable_chip(format!("{{{{{variable}}}}}"), cx)),
                ),
                _ => this,
            })
    }))
}

fn tests_label(tests: &[TestResult]) -> String {
    let label = t!("api_client.request.response_tab_tests");
    let ran: Vec<&TestResult> = tests
        .iter()
        .filter(|test| test.outcome != Outcome::Off)
        .collect();
    if ran.is_empty() {
        return label.to_string();
    }
    let passed = ran
        .iter()
        .filter(|test| test.outcome == Outcome::Passed)
        .count();
    format!("{label} {passed}/{}", ran.len())
}

fn failure_message(failure: &Failure) -> SharedString {
    match failure {
        Failure::Status { expected, actual } => t!(
            "api_client.request.tests_assertion_status",
            expected = expected,
            actual = actual
        ),
        Failure::Equal {
            subject,
            expected,
            actual,
        } => t!(
            "api_client.request.tests_assertion_equal",
            path = subject,
            expected = expected,
            actual = actual
        ),
        Failure::Match { subject, pattern } => t!(
            "api_client.request.tests_assertion_match",
            path = subject,
            pattern = pattern
        ),
        Failure::Exist { subject } => {
            t!("api_client.request.tests_assertion_exist", path = subject)
        }
    }
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
        return vars::dynamic_value(name)
            .map(|_| t!("api_client.request.generated_each_send").to_string());
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
                        background_color: Some(color.opacity(0.16)),
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
        return format!("{bytes} B");
    }
    let size = if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    };
    // Brazilian Portuguese writes the decimal separator as a comma.
    if i18n::current_language() == i18n::Language::PortugueseBrazil {
        size.replace('.', ",")
    } else {
        size
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

        let url_input =
            cx.new(|cx| InputField::new(window, cx, &t!("api_client.request.url_placeholder")));
        let url_editor = url_input.read(cx).editor().clone();
        url_editor.set_text(&draft.url, window, cx);

        let body_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_placeholder_text(&t!("api_client.request.no_body"), window, cx);
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
            highlighted_script_line: None,
            exchange: None,
            body_validation: None,
            send_task: None,
            response_height: None,
            _subscriptions: Vec::new(),
        };
        for parameter in &draft.path_params {
            let input = this.new_input(
                &t!("api_client.settings.value_placeholder"),
                &parameter.value,
                window,
                cx,
            );
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
        this.update_host_hint(cx);
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
                this.update_host_hint(cx);
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
            name: self.new_input(
                &t!("api_client.request.field_placeholder"),
                &field.name,
                window,
                cx,
            ),
            value: self.new_input(
                &if field.is_file {
                    t!("api_client.request.file_path_placeholder")
                } else {
                    t!("api_client.settings.value_placeholder")
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
            variable: make(
                &t!("api_client.request.variable_placeholder"),
                variable,
                window,
                cx,
            ),
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
            prompt: Some(t!("api_client.request.attach")),
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
            name: self.new_input(
                &t!("api_client.settings.name_placeholder"),
                name,
                window,
                cx,
            ),
            value: self.new_input(
                &t!("api_client.settings.value_placeholder"),
                value,
                window,
                cx,
            ),
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

    pub(crate) fn show_headers_tab(&mut self, cx: &mut Context<Self>) {
        self.request_tab = RequestTab::Headers;
        cx.notify();
    }

    pub fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.send_task.is_some() {
            return;
        }
        if self.capture_save_task.is_some() {
            self.save_captures(cx);
        }
        self.example_status = None;
        self.highlighted_script_line = None;
        let draft = self.draft(cx);
        let key = self.operation_key.clone();
        let saved_id = self.saved_id.clone();
        let exchange = self.collection.update(cx, |collection, cx| {
            collection.send_saved(key, draft, saved_id, cx)
        });
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
            None => t!(
                "api_client.request.environment_no_base_url",
                environment = &environment.name
            )
            .to_string(),
        })
    }

    fn update_host_hint(&mut self, cx: &mut Context<Self>) {
        let hint = self
            .environment_hint(cx)
            .map(|hint| (SharedString::from(hint), Color::Muted));
        self.url_input
            .update(cx, |input, cx| input.set_end_hint(hint, cx));
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
        let send_button = ui::primary_button(
            "api-send",
            Some(IconName::Send),
            if sending {
                t!("api_client.request.sending")
            } else {
                t!("api_client.request.send")
            },
            Some(ui::KeyBinding::for_action_in(
                &SendRequest,
                &focus_handle,
                cx,
            )),
            cx,
        )
        .h(px(32.))
        .px_4()
        .map(|this| {
            if sending {
                this.opacity(0.6).cursor_default().into_any_element()
            } else {
                this.on_click(cx.listener(|this, _, window, cx| this.send(window, cx)))
                    .into_any_element()
            }
        });
        h_flex()
            .gap_2()
            .px_3()
            .pt_3()
            .child(method_chip(&method, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .capture_action(cx.listener(
                        |this, _: &editor::actions::Newline, window, cx| {
                            this.send(window, cx);
                        },
                    ))
                    .child(self.url_input.clone()),
            )
            .child(send_button)
    }

    fn render_meta(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let collection = self.collection.read(cx);
        let draft = self.draft(cx);
        let prepared = collection.prepare(&self.operation_key, &draft).ok();
        let operation = self.operation(cx);
        let context = operation.map(|operation| {
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
            (operation.tags.first().cloned(), operation.title(), schemes)
        });
        let missing = prepared
            .as_ref()
            .map(|prepared| prepared.missing.clone())
            .unwrap_or_default();
        v_flex()
            .px_3()
            .pt_2()
            .gap_1()
            .when_some(context, |this, (tag, title, schemes)| {
                this.child(
                    h_flex()
                        .gap_1p5()
                        .when_some(tag, |this, tag| {
                            this.child(
                                Label::new(tag)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .single_line(),
                            )
                            .child(Label::new("›").size(LabelSize::XSmall).color(Color::Muted))
                        })
                        .child(
                            div()
                                .min_w_0()
                                .child(Label::new(title).size(LabelSize::XSmall).truncate()),
                        )
                        .child(if schemes.is_empty() {
                            ui::status_pill(
                                t!("api_client.request.public_route_pill"),
                                StatusTone::Neutral,
                                cx,
                            )
                        } else {
                            ui::status_pill(schemes.join(", "), StatusTone::Info, cx)
                        })
                        .child(div().flex_1())
                        .child(
                            div()
                                .id("api-open-in-spec")
                                .cursor_pointer()
                                .child(
                                    Label::new(t!("api_client.request.view_in_spec"))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Accent),
                                )
                                .on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.open_in_spec(window, cx)
                                    }),
                                ),
                        ),
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
                            Label::new(t!(
                                "api_client.request.missing_values",
                                names = missing
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
                    Label::new(t!("api_client.request.operation_gone"))
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
        tab_option(id, label, active, true, cx).on_click(cx.listener(move |this, _, _, cx| {
            on_click(this, cx);
            cx.notify();
        }))
    }

    fn render_request_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let request_body = self
            .operation(cx)
            .and_then(|operation| operation.request_body.clone());
        let params_count = self.path_params.len() + self.query_rows.len();
        let headers_count = self.header_rows.len() + self.inherited_headers(cx).len();
        let tab = self.request_tab;
        let with_count = |label: SharedString, count: usize| {
            if count > 0 {
                format!("{label} · {count}")
            } else {
                label.to_string()
            }
        };
        let body_tools = request_body
            .filter(|_| tab == RequestTab::Body && !self.body_kind(cx).is_form())
            .map(|body| body.content_type);
        h_flex()
            .px_2()
            .pt_2()
            .gap_2()
            .child(
                ui::segmented_control(cx)
                    .child(self.render_tab_button(
                        "api-tab-params",
                        with_count(t!("api_client.request.tab_params"), params_count),
                        tab == RequestTab::Params,
                        |this, _| this.request_tab = RequestTab::Params,
                        cx,
                    ))
                    .child(self.render_tab_button(
                        "api-tab-headers",
                        with_count(t!("api_client.request.tab_headers"), headers_count),
                        tab == RequestTab::Headers,
                        |this, _| this.request_tab = RequestTab::Headers,
                        cx,
                    ))
                    .child(self.render_tab_button(
                        "api-tab-body",
                        t!("api_client.request.tab_body").to_string(),
                        tab == RequestTab::Body,
                        |this, _| this.request_tab = RequestTab::Body,
                        cx,
                    ))
                    .child(self.render_tab_button(
                        "api-tab-auth",
                        t!("api_client.request.tab_auth").to_string(),
                        tab == RequestTab::Auth,
                        |this, _| this.request_tab = RequestTab::Auth,
                        cx,
                    ))
                    .child(self.render_tab_button(
                        "api-tab-captures",
                        with_count(
                            t!("api_client.request.captures_tab"),
                            self.capture_rows.len(),
                        ),
                        tab == RequestTab::Captures,
                        |this, _| this.request_tab = RequestTab::Captures,
                        cx,
                    ))
                    .child(self.render_tab_button(
                        "api-tab-scripts",
                        t!("api_client.request.tab_scripts").to_string(),
                        tab == RequestTab::Scripts,
                        |this, _| this.request_tab = RequestTab::Scripts,
                        cx,
                    ))
                    .child(self.render_tab_button(
                        "api-tab-docs",
                        t!("api_client.request.tab_docs").to_string(),
                        tab == RequestTab::Docs,
                        |this, _| this.request_tab = RequestTab::Docs,
                        cx,
                    )),
            )
            .child(div().flex_1())
            .when_some(body_tools, |this, content_type| {
                this.child(
                    ui::select_field("api-content-type", content_type, cx)
                        .w_auto()
                        .flex_none()
                        .cursor_default(),
                )
                .child(
                    ui::secondary_button(
                        "api-fill-example",
                        Some((IconName::Sparkle, Color::Accent)),
                        t!("api_client.request.fill_example"),
                        true,
                        cx,
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.fill_example(window, cx))),
                )
            })
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
        label: SharedString,
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
                this.child(section_label("PATH"))
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
            .child(section_label("QUERY"))
            .child(self.render_rows("query", &self.query_rows, cx))
            .child(self.add_row_button("query", t!("api_client.request.add_parameter"), cx))
            .into_any_element()
    }

    /// Shows where an inherited header comes from: the spec at this operation, or the dock's
    /// collection settings.
    fn open_header_origin(
        &mut self,
        origin: &HeaderOrigin,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if *origin == HeaderOrigin::Spec {
            self.open_in_spec(window, cx);
            return;
        }
        self.workspace
            .update(cx, |workspace, cx| {
                crate::open(workspace, window, cx);
                if let Some(panel) = workspace.panel::<crate::ApiPanel>(cx) {
                    panel.update(cx, |panel, cx| panel.open_settings(window, cx));
                }
            })
            .log_err();
    }

    fn render_headers(&self, cx: &mut Context<Self>) -> AnyElement {
        let inherited = self.inherited_headers(cx);
        let colors = cx.theme().colors();
        let border = colors.border;
        let row = || {
            h_flex()
                .w_full()
                .min_h(px(34.))
                .px_3()
                .gap_2()
                .border_b_1()
                .border_color(border)
        };
        let name_column = |content: AnyElement| div().flex_none().w(px(220.)).child(content);
        let origin_column = |content: AnyElement| h_flex().flex_none().w(px(190.)).child(content);
        let origin_chip = |label: String, origin: Option<&HeaderOrigin>, cx: &App| {
            let (color, icon) = match origin {
                Some(HeaderOrigin::Scheme(_)) => (Color::Success, Some(IconName::Lock)),
                Some(HeaderOrigin::Spec) => (Color::Accent, Some(IconName::FileCode)),
                Some(HeaderOrigin::Collection) => (Color::Muted, Some(IconName::Folder)),
                None => (Color::Default, None),
            };
            let hsla = color.color(cx);
            h_flex()
                .flex_none()
                .h(px(22.))
                .px_2()
                .gap_1()
                .rounded_md()
                .border_1()
                .map(|this| match origin {
                    Some(HeaderOrigin::Collection) | None => this
                        .bg(cx.theme().colors().element_background)
                        .border_color(cx.theme().colors().border_selected),
                    _ => this.bg(hsla.opacity(0.1)).border_color(hsla.opacity(0.3)),
                })
                .children(icon.map(|icon| Icon::new(icon).size(IconSize::XSmall).color(color)))
                .child(Label::new(label).size(LabelSize::XSmall).color(color))
                .into_any_element()
        };
        v_flex()
            .w_full()
            .rounded_lg()
            .border_1()
            .border_color(border)
            .bg(colors.editor_background)
            .overflow_hidden()
            .child(
                row()
                    .h(px(32.))
                    .min_h(px(32.))
                    .child(div().flex_none().w(px(20.)))
                    .child(name_column(section_label("HEADER").into_any_element()))
                    .child(
                        div()
                            .flex_1()
                            .child(section_label(t!("api_client.request.value_column"))),
                    )
                    .child(origin_column(
                        section_label(t!("api_client.request.origin_column")).into_any_element(),
                    ))
                    .child(div().flex_none().w(px(24.))),
            )
            .children(inherited.into_iter().enumerate().map(|(index, header)| {
                let disabled = self
                    .disabled_inherited
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(&header.name));
                let enabled = header.enabled && !disabled;
                let name = header.name.clone();
                let origin = header.origin.clone();
                row()
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
                    .child(name_column(
                        Label::new(header.name.clone())
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .color(if enabled {
                                Color::Default
                            } else {
                                Color::Muted
                            })
                            .truncate()
                            .into_any_element(),
                    ))
                    .child(div().flex_1().min_w_0().child(render_template(
                        &header.value,
                        self.collection.read(cx),
                        !enabled,
                        cx,
                    )))
                    .child(origin_column(origin_chip(
                        header.origin.label(),
                        Some(&header.origin),
                        cx,
                    )))
                    .child(
                        IconButton::new(("api-inherited-origin", index), IconName::ArrowUpRight)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .tooltip(Tooltip::text(if header.origin == HeaderOrigin::Spec {
                                t!("api_client.request.view_in_spec")
                            } else {
                                t!("api_client.request.open_collection_headers")
                            }))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_header_origin(&origin, window, cx)
                            })),
                    )
            }))
            .children(self.header_rows.iter().enumerate().map(|(index, header)| {
                row()
                    .py_1()
                    .child(
                        Checkbox::new(
                            SharedString::from(format!("api-header-enabled-{index}")),
                            header.enabled.into(),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(row) = this.header_rows.get_mut(index) {
                                row.enabled = !row.enabled;
                            }
                            this.store_draft(cx);
                        })),
                    )
                    .child(name_column(header.name.clone().into_any_element()))
                    .child(div().flex_1().min_w_0().child(header.value.clone()))
                    .child(origin_column(origin_chip(
                        t!("api_client.request.origin_this_request").to_string(),
                        None,
                        cx,
                    )))
                    .child(
                        IconButton::new(("api-header-remove", index), IconName::Trash)
                            .icon_size(IconSize::XSmall)
                            .icon_color(Color::Muted)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if index < this.header_rows.len() {
                                    this.header_rows.remove(index);
                                }
                                this.store_draft(cx);
                            })),
                    )
            }))
            .child(
                h_flex()
                    .h(px(40.))
                    .px_3()
                    .gap_4()
                    .child(self.add_row_button("header", t!("api_client.settings.add_header"), cx))
                    .child(
                        Label::new(t!("api_client.request.inherited_note"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                    ),
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
                    .child(ui::status_chip(None, content_type, Color::Muted, cx))
                    .child(
                        Label::new(if kind == BodyKind::Multipart {
                            t!("api_client.request.multipart_note")
                        } else {
                            t!("api_client.request.url_encoded_note")
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
                                if row.is_file {
                                    t!("api_client.request.field_file")
                                } else {
                                    t!("api_client.request.field_text")
                                },
                            )
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::XSmall)
                            .tooltip(Tooltip::text(t!("api_client.request.toggle_field_kind")))
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    if let Some(row) = this.form_rows.get_mut(index) {
                                        row.is_file = !row.is_file;
                                    }
                                    this.store_draft(cx);
                                },
                            )),
                        )
                    })
                    .when(row.is_file, |this| {
                        this.child(
                            IconButton::new(("api-form-pick", index), IconName::FolderOpen)
                                .icon_size(IconSize::Small)
                                .icon_color(Color::Muted)
                                .tooltip(Tooltip::text(t!("api_client.request.choose_file")))
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
                Button::new("api-form-add", t!("api_client.request.add_field"))
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
                Label::new(t!("api_client.request.captures_note"))
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
                            Label::new(t!(
                                "api_client.request.login_captures",
                                captures = format!(
                                    "{} → {{{{token}}}}{}",
                                    login.token_path,
                                    login
                                        .refresh_token_path
                                        .map(|path| format!(", {path} → {{{{refreshToken}}}}"))
                                        .unwrap_or_default()
                                )
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
                                t!("api_client.request.unsupported_path").to_string()
                            } else {
                                current.unwrap_or_else(|| {
                                    t!("api_client.request.no_value_yet").to_string()
                                })
                            })
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(if path_valid {
                                Color::Muted
                            } else {
                                Color::Warning
                            })
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
                Button::new("api-capture-add", t!("api_client.request.add_capture"))
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
        let schema_reference = operation
            .and_then(|operation| operation.request_body.as_ref())
            .and_then(|body| body.schema.as_ref())
            .and_then(|schema| schema.get("$ref"))
            .and_then(|reference| reference.as_str())
            .map(str::to_string);
        let is_json = content_type
            .as_deref()
            .is_none_or(crate::spec::is_json_content_type);
        ui::card(cx)
            .overflow_hidden()
            .bg(cx.theme().colors().editor_background)
            .size_full()
            .when(!is_json, |this| {
                this.child(
                    div().px_3().pt_2().child(
                        Label::new(t!("api_client.request.body_not_json"))
                            .size(LabelSize::XSmall)
                            .color(Color::Warning),
                    ),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_h(px(80.))
                    .py_2()
                    .capture_action(cx.listener(
                        |this, _: &editor::actions::NewlineBelow, window, cx| {
                            this.send(window, cx);
                        },
                    ))
                    .child(self.body_editor.clone()),
            )
            .child(render_validation(
                self.body_validation.as_ref(),
                schema_reference.as_deref(),
                &t!("api_client.request.valid_against"),
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
                    Label::new(t!("api_client.request.public_route"))
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
                Label::new(t!("api_client.request.auth_note"))
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
                this.child(section_label(t!("api_client.request.parameters_section")))
                    .children(operation.parameters.iter().map(|parameter| {
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
                            .child(ui::status_chip(None, location, Color::Muted, cx))
                            .when(parameter.required, |this| {
                                this.child(ui::status_chip(
                                    None,
                                    t!("api_client.request.required"),
                                    Color::Warning,
                                    cx,
                                ))
                            })
                            .when_some(parameter.description.clone(), |this, description| {
                                this.child(
                                    Label::new(description)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted)
                                        .truncate(),
                                )
                            })
                    }))
            })
            .child(section_label(t!("api_client.request.responses_section")))
            .children(operation.responses.iter().map(|response| {
                let status: u16 = response.status.parse().unwrap_or(0);
                h_flex()
                    .gap_2()
                    .child(ui::status_chip(
                        None,
                        response.status.clone(),
                        if status == 0 {
                            Color::Muted
                        } else {
                            Color::Custom(status_tone(status).color(cx))
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
                    .pt_2p5()
                    .pb_2()
                    .child(self.render_body(cx))
                    .into_any_element();
            }
            RequestTab::Auth => self.render_auth(cx),
            RequestTab::Captures => self.render_captures(cx),
            RequestTab::Scripts => self.render_scripts(cx),
            RequestTab::Docs => self.render_docs(cx),
        };
        div()
            .id("api-request-content")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_3()
            .pt_2p5()
            .pb_2()
            .child(content)
            .into_any_element()
    }

    fn render_capture_banner(
        &self,
        exchange: &Exchange,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let pairs: Vec<(String, String)> = match &exchange.login_capture {
            Some(login) => std::iter::once((login.token_path.clone(), TOKEN_VARIABLE.to_string()))
                .chain(
                    login
                        .refresh_token_path
                        .clone()
                        .map(|path| (path, REFRESH_TOKEN_VARIABLE.to_string())),
                )
                .chain(
                    exchange
                        .captures
                        .iter()
                        .filter(|capture| capture.value.is_some())
                        .map(|capture| (capture.path.clone(), capture.variable.clone())),
                )
                .collect(),
            None => exchange
                .captures
                .iter()
                .filter(|capture| capture.value.is_some())
                .map(|capture| (capture.path.clone(), capture.variable.clone()))
                .collect(),
        };
        let missing: Vec<String> = exchange
            .captures
            .iter()
            .filter(|capture| capture.value.is_none())
            .map(|capture| capture.path.clone())
            .collect();
        if pairs.is_empty() && missing.is_empty() {
            return None;
        }
        let tone = if pairs.is_empty() {
            StatusTone::Warning
        } else {
            StatusTone::Success
        };
        Some(
            tinted_row(tone, cx)
                .child(
                    Icon::new(if pairs.is_empty() {
                        IconName::Warning
                    } else {
                        IconName::Check
                    })
                    .size(IconSize::Small)
                    .color(if pairs.is_empty() {
                        Color::Warning
                    } else {
                        Color::Success
                    }),
                )
                .when(!pairs.is_empty(), |this| {
                    this.child(
                        Label::new(if exchange.login_capture.is_some() {
                            t!("api_client.request.login_captured")
                        } else {
                            t!("api_client.request.captured")
                        })
                        .size(LabelSize::Small)
                        .weight(FontWeight::SEMIBOLD),
                    )
                })
                .children(pairs.into_iter().map(|(path, variable)| {
                    h_flex()
                        .flex_none()
                        .gap_1p5()
                        .child(Label::new(path).size(LabelSize::XSmall).buffer_font(cx))
                        .child(Label::new("→").size(LabelSize::XSmall).color(Color::Muted))
                        .child(variable_chip(format!("{{{{{variable}}}}}"), cx))
                }))
                .when_some(
                    exchange
                        .login_capture
                        .as_ref()
                        .and_then(|login| login.expires_in),
                    |this, seconds| {
                        this.child(
                            Label::new(t!(
                                "api_client.request.expires_in_seconds",
                                seconds = seconds
                            ))
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Muted)
                            .single_line(),
                        )
                    },
                )
                .when(!missing.is_empty(), |this| {
                    this.child(
                        Label::new(t!(
                            "api_client.request.missing_in_response",
                            names = missing.join(", ")
                        ))
                        .size(LabelSize::XSmall)
                        .buffer_font(cx)
                        .color(Color::Warning)
                        .truncate(),
                    )
                })
                .child(div().flex_1())
                .when(exchange.login_capture.is_some(), |this| {
                    this.child(
                        div().min_w_0().child(
                            Label::new(t!("api_client.request.authorization_applied"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                    )
                })
                .into_any_element(),
        )
    }

    fn render_response_divider(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("api-response-resize-handle")
            .group("api-response-resize-handle")
            .flex_none()
            .h(px(7.))
            .w_full()
            .flex()
            .items_center()
            .cursor_row_resize()
            .child(
                div()
                    .h_px()
                    .w_full()
                    .bg(cx.theme().colors().border)
                    .group_hover("api-response-resize-handle", |this| {
                        this.bg(cx.theme().colors().border_selected)
                    }),
            )
            .on_drag(DraggedResponseDivider, |_, _, _, cx| cx.new(|_| Empty))
            .on_click(cx.listener(|this, event: &gpui::ClickEvent, _, cx| {
                if event.click_count() >= 2 {
                    this.response_height = None;
                    cx.notify();
                }
            }))
            .into_any_element()
    }

    fn render_response(&self, cx: &mut Context<Self>) -> AnyElement {
        let save_button = IconButton::new("api-save", IconName::Bookmark)
            .icon_size(IconSize::Small)
            .icon_color(if self.saved_id.is_some() {
                Color::Accent
            } else {
                Color::Muted
            })
            .tooltip(Tooltip::text(if self.saved_id.is_some() {
                t!("api_client.request.update_saved")
            } else {
                t!("api_client.request.save_to_saved")
            }))
            .on_click(cx.listener(|this, _, _, cx| this.save(cx)));
        let header = h_flex().h(px(44.)).px_3().gap_2p5().child(
            Label::new(t!("api_client.request.response_title"))
                .size(LabelSize::Small)
                .weight(FontWeight::SEMIBOLD),
        );
        let Some(exchange) = self.exchange.as_ref() else {
            let hint = if self.send_task.is_some() {
                t!("api_client.request.sending")
            } else {
                t!("api_client.request.response_empty")
            };
            return v_flex()
                .flex_1()
                .min_h_0()
                .child(header.child(div().flex_1()).child(save_button))
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
        let mut facts = Vec::new();
        if let Some(response) = exchange.response.as_ref() {
            facts.push(format!("{} ms", response.elapsed.as_millis()));
            facts.push(format!(
                "{}{}",
                human_size(response.body.len()),
                if response.truncated {
                    t!("api_client.request.truncated")
                } else {
                    SharedString::default()
                }
            ));
        }
        let header = header
            .when_some(exchange.response.as_ref(), |this, response| {
                let status_text = match &response.reason {
                    Some(reason) => format!("{} {reason}", response.status),
                    None => response.status.to_string(),
                };
                this.child(ui::status_pill(
                    status_text,
                    status_tone(response.status),
                    cx,
                ))
            })
            .when(exchange.error.is_some(), |this| {
                this.child(ui::status_pill(
                    t!("api_client.request.failed"),
                    StatusTone::Error,
                    cx,
                ))
            })
            .child(
                Label::new(facts.join(" · "))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(div().flex_1())
            .child(
                ui::segmented_control(cx)
                    .child(self.render_tab_button(
                        "api-response-body",
                        t!("api_client.request.response_tab_body").to_string(),
                        tab == ResponseTab::Body,
                        |this, _| this.response_tab = ResponseTab::Body,
                        cx,
                    ))
                    .child(self.render_tab_button(
                        "api-response-headers",
                        format!(
                            "{} · {}",
                            t!("api_client.request.response_tab_headers"),
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
                            t!("api_client.request.response_tab_cookies").to_string()
                        } else {
                            format!(
                                "{} · {}",
                                t!("api_client.request.response_tab_cookies"),
                                exchange.cookies_set.len()
                            )
                        },
                        tab == ResponseTab::Cookies,
                        |this, _| this.response_tab = ResponseTab::Cookies,
                        cx,
                    ))
                    .child(self.render_tab_button(
                        "api-response-tests",
                        tests_label(&exchange.tests),
                        tab == ResponseTab::Tests,
                        |this, _| this.response_tab = ResponseTab::Tests,
                        cx,
                    ))
                    .child(self.render_tab_button(
                        "api-response-timeline",
                        t!("api_client.request.response_tab_timeline").to_string(),
                        tab == ResponseTab::Timeline,
                        |this, _| this.response_tab = ResponseTab::Timeline,
                        cx,
                    )),
            )
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
                            .tooltip(Tooltip::text(t!("api_client.request.save_example_tooltip")))
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
                    .tooltip(Tooltip::text(t!("api_client.request.copy_body_tooltip")))
                    .on_click(cx.listener(|this, _, _, cx| {
                        let text = this.response_editor.read(cx).text(cx);
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    })),
            )
            .child(save_button);

        let content: AnyElement = match tab {
            ResponseTab::Body => v_flex()
                .flex_1()
                .min_h_0()
                .gap_2p5()
                .children(self.render_capture_banner(exchange, cx))
                .when_some(self.example_status.clone(), |this, (message, ok)| {
                    this.child(Label::new(message).size(LabelSize::XSmall).color(if ok {
                        Color::Success
                    } else {
                        Color::Warning
                    }))
                })
                .when_some(exchange.error.clone(), |this, error| {
                    this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
                })
                .child(
                    ui::card(cx)
                        .overflow_hidden()
                        .bg(cx.theme().colors().editor_background)
                        .flex_1()
                        .min_h_0()
                        .child(
                            div()
                                .flex_1()
                                .min_h_0()
                                .py_2()
                                .child(self.response_editor.clone()),
                        )
                        .child(render_validation(
                            exchange.validation.as_ref(),
                            exchange.response_schema_name.as_deref(),
                            &t!("api_client.request.response_matches"),
                            cx,
                        )),
                )
                .into_any_element(),
            ResponseTab::Headers => ui::card(cx)
                .overflow_hidden()
                .id("api-response-headers-list")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .children(exchange.response.iter().flat_map(|response| {
                    response.headers.iter().map(|(name, value)| {
                        h_flex()
                            .min_h(px(30.))
                            .px_3()
                            .gap_3()
                            .border_b_1()
                            .border_color(cx.theme().colors().border)
                            .child(
                                div().flex_none().w(px(220.)).child(
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
            ResponseTab::Tests => self.render_tests(exchange, cx),
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
                Some(expires_at) if expires_at <= now => {
                    t!("api_client.request.cookie_deleted").to_string()
                }
                Some(expires_at) => chrono::DateTime::from_timestamp(expires_at, 0)
                    .map(|expires| {
                        t!(
                            "api_client.request.cookie_expires",
                            time = expires.with_timezone(&chrono::Local).format("%d/%m %H:%M")
                        )
                        .to_string()
                    })
                    .unwrap_or_default(),
                None => t!("api_client.request.cookie_session").to_string(),
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
            .child(section_label(t!("api_client.request.cookies_set_section")))
            .when(exchange.cookies_set.is_empty(), |this| {
                this.child(
                    Label::new(t!("api_client.request.no_set_cookie"))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .children(exchange.cookies_set.iter().map(|cookie| row(cookie, cx)))
            .child(
                h_flex()
                    .pt_2()
                    .child(section_label(t!(
                        "api_client.request.cookies_stored_section"
                    )))
                    .child(div().flex_1())
                    .when(!jar.is_empty(), |this| {
                        this.child(
                            Button::new(
                                "api-clear-cookies",
                                t!("api_client.request.clear_cookies"),
                            )
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small)
                            .on_click(move |_, _, cx| {
                                collection
                                    .update(cx, |collection, cx| collection.clear_cookies(cx));
                            }),
                        )
                    }),
            )
            .when(jar.is_empty(), |this| {
                this.child(
                    Label::new(t!("api_client.request.cookies_empty"))
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
            self.example_status = Some((t!("api_client.request.example_yaml_only"), false));
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
                        t!(
                            "api_client.request.example_inserted",
                            status = status,
                            file = file_name
                        ),
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

    fn render_scripts(&self, cx: &mut Context<Self>) -> AnyElement {
        let scripts: Vec<(String, Script)> = self
            .saved_id
            .as_deref()
            .map(|saved_id| {
                self.collection
                    .read(cx)
                    .saved_scripts(saved_id)
                    .iter()
                    .map(|script| {
                        let after_response = script.listen == "test";
                        (
                            script.listen.clone(),
                            script::parse(&script.source, after_response),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        if scripts.is_empty() {
            return EmptyState::new(IconName::Code, t!("api_client.request.scripts_empty_title"))
                .description(t!("api_client.request.scripts_empty_description"))
                .into_any_element();
        }
        let ordered = ["prerequest", "test"]
            .into_iter()
            .flat_map(|listen| scripts.iter().filter(move |(other, _)| other == listen));
        v_flex()
            .gap_3()
            .children(ordered.map(|(listen, script)| {
                let title = if listen == "test" {
                    t!("api_client.request.scripts_after_response")
                } else {
                    t!("api_client.request.scripts_before_send")
                };
                let highlighted = self.highlighted_script_line.filter(|_| listen == "test");
                ui::card(cx)
                    .overflow_hidden()
                    .child(
                        div()
                            .px_3()
                            .py_2()
                            .border_b_1()
                            .border_color(cx.theme().colors().border)
                            .child(ui::section_header(
                                title,
                                Some(
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            Label::new(listen.clone())
                                                .size(LabelSize::XSmall)
                                                .buffer_font(cx)
                                                .color(Color::Muted),
                                        )
                                        .when(script.is_off(), |this| {
                                            this.child(ui::status_pill(
                                                t!("api_client.request.script_off_pill"),
                                                StatusTone::Warning,
                                                cx,
                                            ))
                                        })
                                        .into_any_element(),
                                ),
                            )),
                    )
                    .child(render_script_lines(script, highlighted, cx))
            }))
            .into_any_element()
    }

    fn render_tests(&self, exchange: &Exchange, cx: &mut Context<Self>) -> AnyElement {
        if exchange.tests.is_empty() {
            return EmptyState::new(
                IconName::ListTodo,
                t!("api_client.request.tests_empty_title"),
            )
            .description(t!("api_client.request.tests_empty_description"))
            .into_any_element();
        }
        let captured: Vec<(String, String)> = exchange
            .captures
            .iter()
            .filter(|capture| capture.value.is_some())
            .map(|capture| (capture.path.clone(), capture.variable.clone()))
            .collect();
        ui::card(cx)
            .overflow_hidden()
            .id("api-tests")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(
                v_flex()
                    .p_3()
                    .gap_2()
                    .children(
                        exchange
                            .tests
                            .iter()
                            .enumerate()
                            .map(|(index, test)| self.render_test_row(index, test, cx)),
                    )
                    .when(!captured.is_empty(), |this| {
                        this.child(
                            tinted_row(StatusTone::Success, cx)
                                .child(
                                    Icon::new(IconName::Check)
                                        .size(IconSize::Small)
                                        .color(Color::Success),
                                )
                                .child(
                                    Label::new(t!("api_client.request.tests_capture_row"))
                                        .size(LabelSize::Small)
                                        .weight(FontWeight::SEMIBOLD),
                                )
                                .children(captured.into_iter().map(|(path, variable)| {
                                    h_flex()
                                        .flex_none()
                                        .gap_1p5()
                                        .child(
                                            Label::new(path)
                                                .size(LabelSize::XSmall)
                                                .buffer_font(cx),
                                        )
                                        .child(
                                            Label::new("→")
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                        )
                                        .child(variable_chip(format!("{{{{{variable}}}}}"), cx))
                                })),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_test_row(&self, index: usize, test: &TestResult, cx: &mut Context<Self>) -> Div {
        let (tone, icon, pill) = match &test.outcome {
            Outcome::Passed => (
                StatusTone::Success,
                IconName::Check,
                t!("api_client.request.tests_passed"),
            ),
            Outcome::Failed(_) => (
                StatusTone::Error,
                IconName::XCircle,
                t!("api_client.request.tests_failed"),
            ),
            Outcome::Off => (
                StatusTone::Warning,
                IconName::Warning,
                t!("api_client.request.script_off_pill"),
            ),
        };
        let icon_color = match tone {
            StatusTone::Success => Color::Success,
            StatusTone::Error => Color::Error,
            _ => Color::Warning,
        };
        let line = test.line;
        tinted_row(tone, cx).items_start().child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2p5()
                        .child(Icon::new(icon).size(IconSize::Small).color(icon_color))
                        .child(
                            div().flex_1().min_w_0().child(
                                Label::new(test.name.clone())
                                    .size(LabelSize::Small)
                                    .weight(FontWeight::SEMIBOLD)
                                    .truncate(),
                            ),
                        )
                        .child(ui::status_pill(pill, tone, cx))
                        .when(test.outcome != Outcome::Off, |this| {
                            this.child(
                                Label::new(format!("{:.2} ms", test.elapsed.as_secs_f64() * 1000.))
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .color(Color::Muted),
                            )
                        }),
                )
                .when_some(
                    match &test.outcome {
                        Outcome::Failed(failure) => Some(failure_message(failure)),
                        _ => None,
                    },
                    |this, message| {
                        this.child(
                            h_flex()
                                .gap_3()
                                .child(
                                    div().flex_1().min_w_0().child(
                                        Label::new(format!("AssertionError: {message}"))
                                            .size(LabelSize::XSmall)
                                            .buffer_font(cx)
                                            .color(Color::Error),
                                    ),
                                )
                                .child(
                                    Button::new(
                                        ("api-test-go-to-line", index),
                                        t!("api_client.request.tests_go_to_line", line = line),
                                    )
                                    .style(ButtonStyle::Subtle)
                                    .label_size(LabelSize::XSmall)
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.request_tab = RequestTab::Scripts;
                                            this.highlighted_script_line = Some(line);
                                            cx.notify();
                                        },
                                    )),
                                ),
                        )
                    },
                ),
        )
    }

    fn render_timeline(&self, exchange: &Exchange, cx: &mut Context<Self>) -> AnyElement {
        let steps = exchange.timeline.clone();
        let request = exchange.request.clone();
        let border = cx.theme().colors().border;
        ui::card(cx)
            .overflow_hidden()
            .id("api-timeline")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(
                v_flex()
                    .p_4()
                    .gap_1()
                    .border_b_1()
                    .border_color(border)
                    .when(steps.len() > 1, |this| {
                        this.child(
                            div().pb_2().child(
                                Label::new(t!("api_client.request.timeline_renewed"))
                                    .size(LabelSize::Small),
                            ),
                        )
                    })
                    .children(steps.into_iter().enumerate().map(|(index, step)| {
                        let failed = step.status.is_none_or(|status| status >= 400);
                        let tone = match step.status {
                            Some(status) => status_tone(status),
                            None => StatusTone::Error,
                        };
                        let status_text = match step.status {
                            Some(status) => match reason_phrase(status) {
                                Some(reason) => format!("{status} {reason}"),
                                None => status.to_string(),
                            },
                            None => t!("api_client.request.error").to_string(),
                        };
                        h_flex()
                            .min_h(px(40.))
                            .gap_3()
                            .child(
                                div()
                                    .flex()
                                    .flex_none()
                                    .size(px(20.))
                                    .items_center()
                                    .justify_center()
                                    .rounded_full()
                                    .border_1()
                                    .map(|this| {
                                        if failed {
                                            let error = Color::Error.color(cx);
                                            this.bg(error.opacity(0.12))
                                                .border_color(error.opacity(0.33))
                                        } else {
                                            this.bg(cx.theme().colors().element_background)
                                                .border_color(cx.theme().colors().border_selected)
                                        }
                                    })
                                    .child(
                                        Label::new(format!("{}", index + 1))
                                            .size(LabelSize::XSmall)
                                            .buffer_font(cx)
                                            .weight(FontWeight::SEMIBOLD)
                                            .color(if failed {
                                                Color::Error
                                            } else {
                                                Color::Default
                                            }),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .flex_none()
                                    .w(px(260.))
                                    .gap_2p5()
                                    .child(method_label(&step.method, cx))
                                    .child(
                                        Label::new(step.path)
                                            .size(LabelSize::Small)
                                            .buffer_font(cx)
                                            .truncate(),
                                    ),
                            )
                            .child(h_flex().flex_none().w(px(140.)).child(ui::status_pill(
                                status_text,
                                tone,
                                cx,
                            )))
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
                                    .buffer_font(cx)
                                    .color(Color::Muted),
                            )
                    }))
                    .when_some(exchange.error.clone(), |this, error| {
                        this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
                    }),
            )
            .when_some(request, |this, request| {
                let curl = request.curl();
                this.child(
                    v_flex()
                        .p_4()
                        .gap_1()
                        .child(
                            h_flex()
                                .pb_2()
                                .child(section_label(t!("api_client.request.sent_request_section")))
                                .child(div().flex_1())
                                .child(
                                    ui::secondary_button(
                                        "api-copy-curl",
                                        Some((IconName::Terminal, Color::Muted)),
                                        t!("api_client.request.copy_as_curl"),
                                        true,
                                        cx,
                                    )
                                    .tooltip(Tooltip::text(t!(
                                        "api_client.request.copy_as_curl_tooltip"
                                    )))
                                    .on_click(
                                        move |_, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                curl.clone(),
                                            ));
                                        },
                                    ),
                                ),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Label::new(request.method.clone())
                                        .size(LabelSize::Small)
                                        .buffer_font(cx)
                                        .weight(FontWeight::SEMIBOLD)
                                        .color(method_color(&request.method)),
                                )
                                .child(
                                    Label::new(request.url.clone())
                                        .size(LabelSize::Small)
                                        .buffer_font(cx),
                                ),
                        )
                        .children(request.headers.iter().map(|(name, value)| {
                            h_flex()
                                .child(
                                    Label::new(format!("{name}: "))
                                        .size(LabelSize::Small)
                                        .buffer_font(cx)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new(value.clone())
                                        .size(LabelSize::Small)
                                        .buffer_font(cx),
                                )
                        })),
                )
            })
            .into_any_element()
    }
}

fn tab_option(
    id: &'static str,
    label: impl Into<SharedString>,
    active: bool,
    enabled: bool,
    cx: &App,
) -> Stateful<Div> {
    ui::segment_option(id, label, active, enabled, cx)
        .flex_initial()
        .px_2p5()
        .h(px(26.))
}

/// The reason phrase servers usually send, for timeline steps that only kept the code.
fn reason_phrase(status: u16) -> Option<&'static str> {
    Some(match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => return None,
    })
}

/// The editor card's footer: whether the JSON follows its schema, and which one.
fn render_validation(
    validation: Option<&Validation>,
    schema_reference: Option<&str>,
    valid_prefix: &str,
    cx: &App,
) -> AnyElement {
    let Some(validation) = validation else {
        return div().into_any_element();
    };
    // The response side only knows the schema's name, the request side its whole `$ref`.
    let (schema, location) = match schema_reference {
        Some(reference) if reference.starts_with('#') => (
            reference
                .rsplit('/')
                .next()
                .unwrap_or(reference)
                .to_string(),
            Some(reference.trim_start_matches("#/").to_string()),
        ),
        Some(name) => (name.to_string(), None),
        None => (t!("api_client.request.spec_schema").to_string(), None),
    };
    let footer = h_flex()
        .flex_none()
        .min_h(px(34.))
        .px_3()
        .py_1()
        .gap_1p5()
        .border_t_1()
        .border_color(cx.theme().colors().border);
    match validation {
        Validation::Valid => footer
            .child(
                Icon::new(IconName::Check)
                    .size(IconSize::XSmall)
                    .color(Color::Success),
            )
            .child(
                Label::new(valid_prefix.to_string())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(Label::new(schema).size(LabelSize::XSmall).buffer_font(cx))
            .when_some(location, |this, location| {
                this.child(
                    Label::new(format!("· {location}"))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .truncate(),
                )
            })
            .into_any_element(),
        Validation::Invalid { issues, more } => footer
            .child(
                v_flex()
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
                            Label::new(t!(
                                "api_client.request.more_issues",
                                count = issues.len().saturating_sub(4) + more,
                                schema = &schema
                            ))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                        )
                    }),
            )
            .into_any_element(),
        Validation::Unchecked { reason } => footer
            .child(
                Label::new(t!("api_client.request.not_validated", reason = reason))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .truncate(),
            )
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
            .on_drag_move(cx.listener(
                |this, event: &DragMoveEvent<DraggedResponseDivider>, _, cx| {
                    let bounds = event.bounds;
                    let max_height =
                        (bounds.size.height - MIN_REQUEST_AREA_HEIGHT).max(MIN_RESPONSE_HEIGHT);
                    let height = (bounds.bottom() - event.event.position.y)
                        .clamp(MIN_RESPONSE_HEIGHT, max_height);
                    this.response_height = Some(height);
                    cx.notify();
                },
            ))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_request_bar(cx))
            .child(self.render_meta(cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .when(self.response_height.is_none(), |this| {
                        this.max_h(MAX_REQUEST_CARD_HEIGHT)
                    })
                    .px_3()
                    .pt_2()
                    .child(
                        ui::card(cx)
                            .flex_1()
                            .min_h_0()
                            .overflow_hidden()
                            .child(self.render_request_tabs(cx))
                            .child(self.render_request_content(cx)),
                    ),
            )
            .child(self.render_response_divider(cx))
            .child(
                v_flex()
                    .map(|this| match self.response_height {
                        Some(height) => this.flex_none().h(height),
                        None => this.flex_1().min_h_0(),
                    })
                    .px_3()
                    .pb_3()
                    .child(
                        ui::card(cx)
                            .flex_1()
                            .min_h_0()
                            .overflow_hidden()
                            .child(self.render_response(cx)),
                    ),
            )
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

    /// The method in the tab already says what this is, as in the boards.
    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        None
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
