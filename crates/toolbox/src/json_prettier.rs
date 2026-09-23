use editor::Editor;
use gpui::{
    Action as _, ClipboardItem, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Pixels,
    actions, px,
};
use language::LanguageRegistry;
use std::{fmt, sync::Arc};
use ui::{HeaderBar, KeyBinding, Tab, TabBar, TabPosition, TabStyle, Tooltip, prelude::*};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    json_prettier,
    [
        /// Toggles focus on the JSON Prettier panel.
        ToggleFocus,
        /// Applies the selected mode (format, minify or validate) to the JSON in the panel.
        FormatJson,
    ]
);

/// Opens the tool, adding its panel to the dock the first time it is asked for. The panel
/// is not registered at startup because a tool nobody opened should not take up a tab.
pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if workspace.panel::<JsonPrettierPanel>(cx).is_none() {
        let languages = workspace.app_state().languages.clone();
        let panel = cx.new(|cx| JsonPrettierPanel::new(languages, window, cx));
        workspace.add_panel(panel, window, cx);
    }
    workspace.focus_panel::<JsonPrettierPanel>(window, cx);
}

/// Focuses the tool, or hands focus back if it already has it. Opening it first is what
/// makes the action work before the panel has ever been asked for.
pub fn toggle_focus(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if workspace.panel::<JsonPrettierPanel>(cx).is_none() {
        open(workspace, window, cx);
        return;
    }
    workspace.toggle_panel_focus::<JsonPrettierPanel>(window, cx);
}

/// Where and why a document failed to parse. The position is what makes the error worth
/// surfacing verbatim, so it leads the message.
#[derive(Debug)]
struct ParseError {
    line: usize,
    column: usize,
    message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            return write!(formatter, "{}", self.message);
        }
        write!(
            formatter,
            "line {}, column {}: {}",
            self.line, self.column, self.message
        )
    }
}

impl From<serde_json::Error> for ParseError {
    fn from(error: serde_json::Error) -> Self {
        let line = error.line();
        let column = error.column();
        // `serde_json` appends the position to its message; it is shown in front instead.
        let full_message = error.to_string();
        let position_suffix = format!(" at line {line} column {column}");
        let message = full_message
            .strip_suffix(&position_suffix)
            .unwrap_or(&full_message)
            .to_string();
        Self {
            line,
            column,
            message,
        }
    }
}

/// `serde_json`'s `preserve_order` feature is what keeps object keys in the order they were
/// written: formatting a document must not reshuffle it.
fn parse_json(text: &str) -> Result<serde_json::Value, ParseError> {
    Ok(serde_json::from_str::<serde_json::Value>(text)?)
}

fn format_json(text: &str) -> Result<String, ParseError> {
    Ok(serde_json::to_string_pretty(&parse_json(text)?)?)
}

fn minify_json(text: &str) -> Result<String, ParseError> {
    Ok(serde_json::to_string(&parse_json(text)?)?)
}

fn describe_size(text: &str) -> String {
    let line_count = text.lines().count().max(1);
    let lines = if line_count == 1 {
        "1 line".to_string()
    } else {
        format!("{line_count} lines")
    };
    let byte_count = text.len();
    let size = if byte_count < 1024 {
        format!("{byte_count} B")
    } else if byte_count < 1024 * 1024 {
        format!("{:.1} KB", byte_count as f64 / 1024.)
    } else {
        format!("{:.1} MB", byte_count as f64 / (1024. * 1024.))
    };
    format!("{lines} · {size}")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Format,
    Minify,
    Validate,
}

impl Mode {
    const ALL: [Mode; 3] = [Mode::Format, Mode::Minify, Mode::Validate];

    fn label(self) -> &'static str {
        match self {
            Mode::Format => "Format",
            Mode::Minify => "Minify",
            Mode::Validate => "Validate",
        }
    }
}

/// What the last attempt made of the editor's contents.
enum Status {
    /// Nothing to say yet: no attempt, or an empty editor.
    Idle,
    Valid {
        summary: SharedString,
    },
    Invalid(SharedString),
}

pub struct JsonPrettierPanel {
    focus_handle: FocusHandle,
    editor: Entity<Editor>,
    mode: Mode,
    status: Status,
}

impl JsonPrettierPanel {
    fn new(languages: Arc<LanguageRegistry>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_placeholder_text("Paste JSON here…", window, cx);
            editor
        });

        cx.spawn({
            let editor = editor.clone();
            async move |_this, cx| {
                let json = languages.language_for_name("JSON").await?;
                editor.update(cx, |editor, cx| {
                    if let Some(buffer) = editor.buffer().read(cx).as_singleton() {
                        buffer.update(cx, |buffer, cx| buffer.set_language(Some(json), cx));
                    }
                });
                anyhow::Ok(())
            }
        })
        .detach_and_log_err(cx);

        Self {
            focus_handle: cx.focus_handle(),
            editor,
            mode: Mode::Format,
            status: Status::Idle,
        }
    }

    fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if self.mode != mode {
            self.mode = mode;
            cx.notify();
        }
    }

    fn apply_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text(cx);
        let text = text.trim();

        if text.is_empty() {
            self.status = Status::Idle;
            cx.notify();
            return;
        }

        let rewritten = match self.mode {
            Mode::Format => format_json(text).map(Some),
            Mode::Minify => minify_json(text).map(Some),
            Mode::Validate => parse_json(text).map(|_| None),
        };

        self.status = match rewritten {
            Ok(Some(output)) => {
                let summary = describe_size(&output).into();
                self.editor.update(cx, |editor, cx| {
                    editor.set_text(output, window, cx);
                });
                Status::Valid { summary }
            }
            Ok(None) => Status::Valid {
                summary: describe_size(text).into(),
            },
            Err(error) => Status::Invalid(error.to_string().into()),
        };
        cx.notify();
    }

    fn paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        self.editor.update(cx, |editor, cx| {
            editor.set_text(text, window, cx);
        });
        self.status = Status::Idle;
        cx.notify();
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text(cx);
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            editor.set_text("", window, cx);
        });
        self.status = Status::Idle;
        cx.notify();
    }

    fn render_mode_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let last_index = Mode::ALL.len() - 1;
        TabBar::new("json-prettier-modes")
            .style(TabStyle::Pill)
            .children(Mode::ALL.into_iter().enumerate().map(|(index, mode)| {
                let selected = self.mode == mode;
                let position = if index == 0 {
                    TabPosition::First
                } else if index == last_index {
                    TabPosition::Last
                } else {
                    TabPosition::Middle(std::cmp::Ordering::Equal)
                };
                Tab::new(mode.label())
                    .style(TabStyle::Pill)
                    .position(position)
                    .toggle_state(selected)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_mode(mode, cx)))
                    .child(
                        Label::new(mode.label())
                            .size(LabelSize::Small)
                            .color(if selected {
                                Color::Default
                            } else {
                                Color::Muted
                            }),
                    )
            }))
    }

    fn render_status_chip(
        &self,
        label: &'static str,
        icon: IconName,
        color: Color,
        cx: &App,
    ) -> impl IntoElement {
        h_flex()
            .flex_none()
            .h(DynamicSpacing::Base20.px(cx))
            .px(DynamicSpacing::Base08.px(cx))
            .gap(DynamicSpacing::Base06.px(cx))
            .rounded_full()
            .bg(color.color(cx).opacity(0.1))
            .child(Icon::new(icon).size(IconSize::XSmall).color(color))
            .child(
                Label::new(label)
                    .size(LabelSize::Custom(rems_from_px(11_f32)))
                    .weight(FontWeight::MEDIUM)
                    .color(color),
            )
    }

    fn render_status(&self, cx: &App) -> Option<impl IntoElement> {
        let (chip, detail, detail_color) = match &self.status {
            Status::Idle => return None,
            Status::Valid { summary } => (
                self.render_status_chip("Valid JSON", IconName::Check, Color::Success, cx),
                summary.clone(),
                Color::Muted,
            ),
            Status::Invalid(message) => (
                self.render_status_chip("Invalid JSON", IconName::XCircle, Color::Error, cx),
                message.clone(),
                Color::Default,
            ),
        };

        Some(
            h_flex()
                .min_w_0()
                .gap(DynamicSpacing::Base06.px(cx))
                .child(chip)
                .child(
                    Label::new(detail)
                        .size(LabelSize::Custom(rems_from_px(11_f32)))
                        .color(detail_color)
                        .truncate(),
                ),
        )
    }

    fn render_primary_action(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let background = cx.theme().colors().border_focused;
        let label = self.mode.label();

        h_flex()
            .id("json-prettier-apply")
            .flex_none()
            .h(DynamicSpacing::Base28.px(cx))
            .px(DynamicSpacing::Base12.px(cx))
            .gap(DynamicSpacing::Base06.px(cx))
            .rounded_lg()
            .bg(background)
            .hover(|style| style.bg(background.opacity(0.85)))
            .active(|style| style.bg(background.opacity(0.7)))
            .cursor_pointer()
            .child(
                Icon::new(IconName::Json)
                    .size(IconSize::XSmall)
                    .color(Color::Default),
            )
            .child(
                Label::new(label)
                    .size(LabelSize::Small)
                    .weight(FontWeight::SEMIBOLD),
            )
            .child(div().opacity(0.6).child(KeyBinding::for_action_in(
                &FormatJson,
                &self.focus_handle,
                cx,
            )))
            .tooltip(Tooltip::for_action_title(label, &FormatJson))
            .on_click(cx.listener(|this, _, window, cx| this.apply_mode(window, cx)))
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let secondary = |id: &'static str, label: &'static str, icon: IconName| {
            Button::new(id, label)
                .style(ButtonStyle::Outlined)
                .size(ButtonSize::Medium)
                .label_size(LabelSize::Small)
                .start_icon(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
        };

        HeaderBar::footer("json-prettier-footer")
            .start_child(
                secondary("paste-json", "Paste", IconName::Notepad)
                    .on_click(cx.listener(|this, _, window, cx| this.paste(window, cx))),
            )
            .start_child(
                secondary("clear-json", "Clear", IconName::Close)
                    .on_click(cx.listener(|this, _, window, cx| this.clear(window, cx))),
            )
            .end_child(
                secondary("copy-json", "Copy", IconName::Copy)
                    .on_click(cx.listener(|this, _, _, cx| this.copy(cx))),
            )
            .end_child(self.render_primary_action(cx))
    }
}

impl Render for JsonPrettierPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let card_border = cx.theme().colors().border;
        let card_background = cx.theme().colors().editor_background;
        let content_padding = DynamicSpacing::Base08.px(cx);

        v_flex()
            .key_context("JsonPrettierPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &FormatJson, window, cx| this.apply_mode(window, cx)))
            .size_full()
            .child(self.render_mode_tabs(cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .p(content_padding)
                    .gap(content_padding)
                    .children(self.render_status(cx))
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .overflow_hidden()
                            .rounded_lg()
                            .border_1()
                            .border_color(card_border)
                            .bg(card_background)
                            .child(self.editor.clone()),
                    ),
            )
            .child(self.render_footer(cx))
    }
}

impl Focusable for JsonPrettierPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for JsonPrettierPanel {}

impl Panel for JsonPrettierPanel {
    fn persistent_name() -> &'static str {
        "JsonPrettierPanel"
    }

    fn panel_key() -> &'static str {
        "JsonPrettierPanel"
    }

    /// Typing is the point of this panel, so activating it lands in the editor.
    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
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
        px(480.)
    }

    /// No status bar button: the tools menu is how this is opened, and a button of its own
    /// would duplicate that entry point.
    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        None
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("JSON Prettier")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        ToggleFocus.boxed_clone()
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

#[cfg(test)]
mod tests {
    use super::{describe_size, format_json, minify_json, parse_json};

    #[test]
    fn formats_inline_json() {
        assert_eq!(
            format_json(r#"{"a":1,"b":[2,3]}"#).unwrap(),
            "{\n  \"a\": 1,\n  \"b\": [\n    2,\n    3\n  ]\n}"
        );
    }

    #[test]
    fn minifies_json() {
        assert_eq!(
            minify_json("{\n  \"a\": 1,\n  \"b\": [2, 3]\n}").unwrap(),
            r#"{"a":1,"b":[2,3]}"#
        );
    }

    #[test]
    fn preserves_key_order() {
        // Alphabetical sorting here would mean the tool rewrites documents it was only
        // asked to indent.
        let formatted = format_json(r#"{"zebra":1,"apple":2}"#).unwrap();
        assert!(
            formatted.find("zebra") < formatted.find("apple"),
            "keys were reordered: {formatted}"
        );
    }

    #[test]
    fn reports_where_parsing_failed() {
        let error = parse_json("{\n  \"a\": 1,\n}").unwrap_err();
        assert_eq!(error.line, 3);
        let message = error.to_string();
        assert!(
            message.starts_with("line 3, column 1: "),
            "unhelpful message: {message}"
        );
        assert!(
            !message.contains(" at line "),
            "position repeated: {message}"
        );
    }

    #[test]
    fn describes_document_size() {
        assert_eq!(describe_size("{}"), "1 line · 2 B");
        assert_eq!(describe_size("{\n}"), "2 lines · 3 B");
    }
}
