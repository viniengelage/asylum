use editor::Editor;
use gpui::{Action as _, Entity, EventEmitter, FocusHandle, Focusable, Pixels, actions, px};
use language::LanguageRegistry;
use std::sync::Arc;
use ui::{ButtonSize, ElevationIndex, Tooltip, prelude::*};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    json_prettier,
    [
        /// Toggles focus on the JSON Prettier panel.
        ToggleFocus,
        /// Reformats the JSON in the panel.
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

/// Reformats `text`, or returns the parse error to show the user. The message carries the
/// line and column of the offending byte, which is what makes it worth surfacing verbatim.
///
/// `serde_json`'s `preserve_order` feature is what keeps object keys in the order they were
/// written: formatting a document must not reshuffle it.
fn format_json(text: &str) -> Result<String, String> {
    serde_json::from_str::<serde_json::Value>(text)
        .and_then(|value| serde_json::to_string_pretty(&value))
        .map_err(|error| error.to_string())
}

/// What the last format attempt made of the editor's contents.
enum Status {
    /// Nothing to say yet: no attempt, or an empty editor.
    Idle,
    Formatted,
    Invalid(SharedString),
}

pub struct JsonPrettierPanel {
    focus_handle: FocusHandle,
    editor: Entity<Editor>,
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
            status: Status::Idle,
        }
    }

    fn format(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).text(cx);
        let text = text.trim();

        if text.is_empty() {
            self.status = Status::Idle;
            cx.notify();
            return;
        }

        self.status = match format_json(text) {
            Ok(formatted) => {
                self.editor.update(cx, |editor, cx| {
                    editor.set_text(formatted, window, cx);
                });
                Status::Formatted
            }
            Err(message) => Status::Invalid(message.into()),
        };
        cx.notify();
    }

    fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            editor.set_text("", window, cx);
        });
        self.status = Status::Idle;
        cx.notify();
    }
}

impl Render for JsonPrettierPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();

        v_flex()
            .key_context("JsonPrettierPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &FormatJson, window, cx| this.format(window, cx)))
            .size_full()
            .gap_1p5()
            .p_2()
            .child(
                h_flex()
                    .gap_1()
                    .justify_between()
                    .child(
                        // Same treatment as the git panel's "Stage All": the modal surface
                        // layer is the darker variant that reads as a control rather than
                        // as part of the panel body.
                        Button::new("format-json", "Format")
                            .layer(ElevationIndex::ModalSurface)
                            .size(ButtonSize::Compact)
                            .label_size(LabelSize::Small)
                            .tooltip(Tooltip::for_action_title("Format", &FormatJson))
                            .on_click(cx.listener(|this, _, window, cx| this.format(window, cx))),
                    )
                    .child(
                        Button::new("clear-json", "Clear")
                            .layer(ElevationIndex::ModalSurface)
                            .size(ButtonSize::Compact)
                            .label_size(LabelSize::Small)
                            .color(Color::Muted)
                            .on_click(cx.listener(|this, _, window, cx| this.clear(window, cx))),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .rounded_sm()
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.editor_background)
                    .child(self.editor.clone()),
            )
            .child(match &self.status {
                Status::Idle => div().into_any_element(),
                Status::Formatted => Label::new("Valid JSON")
                    .size(LabelSize::Small)
                    .color(Color::Success)
                    .into_any_element(),
                Status::Invalid(message) => Label::new(message.clone())
                    .size(LabelSize::Small)
                    .color(Color::Error)
                    .into_any_element(),
            })
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
    use super::format_json;

    #[test]
    fn formats_inline_json() {
        assert_eq!(
            format_json(r#"{"a":1,"b":[2,3]}"#).unwrap(),
            "{\n  \"a\": 1,\n  \"b\": [\n    2,\n    3\n  ]\n}"
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
        let error = format_json(r#"{"a": 1,}"#).unwrap_err();
        assert!(error.contains("line 1"), "unhelpful message: {error}");
    }
}
