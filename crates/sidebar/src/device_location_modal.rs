use editor::Editor;
use gpui::{AppContext as _, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable};
use ui::{Divider, Headline, HeadlineSize, Label, LabelSize, prelude::*};
use workspace::ModalView;

/// A latitude/longitude pair the user typed in, already range-checked.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeviceLocation {
    pub latitude: f64,
    pub longitude: f64,
}

/// Asks for a simulated GPS position to hand to whichever device is selected.
pub struct DeviceLocationModal {
    latitude_editor: Entity<Editor>,
    longitude_editor: Entity<Editor>,
    last_error: Option<SharedString>,
    on_confirm: Box<dyn Fn(DeviceLocation, &mut Window, &mut App) + 'static>,
}

impl EventEmitter<DismissEvent> for DeviceLocationModal {}
impl ModalView for DeviceLocationModal {}

impl Focusable for DeviceLocationModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.latitude_editor.focus_handle(cx)
    }
}

impl DeviceLocationModal {
    pub fn new(
        initial: Option<DeviceLocation>,
        on_confirm: impl Fn(DeviceLocation, &mut Window, &mut App) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let latitude_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("-30.0346", window, cx);
            if let Some(initial) = initial {
                editor.set_text(initial.latitude.to_string(), window, cx);
            }
            editor
        });
        let longitude_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("-51.2177", window, cx);
            if let Some(initial) = initial {
                editor.set_text(initial.longitude.to_string(), window, cx);
            }
            editor
        });

        Self {
            latitude_editor,
            longitude_editor,
            last_error: None,
            on_confirm: Box::new(on_confirm),
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let latitude = self.latitude_editor.read(cx).text(cx);
        let longitude = self.longitude_editor.read(cx).text(cx);

        let location = match parse_location(&latitude, &longitude) {
            Ok(location) => location,
            Err(error) => {
                self.last_error = Some(error);
                cx.notify();
                return;
            }
        };

        (self.on_confirm)(location, window, cx);
        cx.emit(DismissEvent);
    }

    fn render_field(
        &self,
        label: &'static str,
        editor: &Entity<Editor>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .gap_1()
            .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().editor_background)
                    .child(editor.clone()),
            )
    }
}

impl Render for DeviceLocationModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("DeviceLocationModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_3(cx)
            .w(rems(30.))
            .overflow_hidden()
            .child(
                v_flex()
                    .p_3()
                    .gap_3()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(
                                Icon::new(IconName::LocationEdit)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(
                                Headline::new("Change user location").size(HeadlineSize::XSmall),
                            ),
                    )
                    .child(self.render_field("Latitude", &self.latitude_editor, cx))
                    .child(self.render_field("Longitude", &self.longitude_editor, cx)),
            )
            .child(Divider::horizontal())
            .child(
                h_flex()
                    .w_full()
                    .p_2()
                    .child(match self.last_error.clone() {
                        Some(error) => Label::new(error).size(LabelSize::Small).color(Color::Error),
                        None => Label::new("Enter para aplicar, Esc para cancelar.")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    }),
            )
    }
}

fn parse_location(latitude: &str, longitude: &str) -> Result<DeviceLocation, SharedString> {
    let latitude = parse_coordinate(latitude, "latitude", 90.0)?;
    let longitude = parse_coordinate(longitude, "longitude", 180.0)?;
    Ok(DeviceLocation {
        latitude,
        longitude,
    })
}

fn parse_coordinate(value: &str, name: &str, limit: f64) -> Result<f64, SharedString> {
    let trimmed = value.trim();
    let parsed: f64 = trimmed
        .parse()
        .map_err(|_| SharedString::from(format!("{name} inválida: \"{trimmed}\"")))?;
    if !parsed.is_finite() || parsed.abs() > limit {
        return Err(SharedString::from(format!(
            "{name} tem de estar entre -{limit} e {limit}"
        )));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_coordinates() {
        assert_eq!(
            parse_location(" -30.0346 ", "-51.2177"),
            Ok(DeviceLocation {
                latitude: -30.0346,
                longitude: -51.2177,
            })
        );
    }

    #[test]
    fn rejects_out_of_range_and_unparseable_coordinates() {
        assert!(parse_location("100", "0").is_err());
        assert!(parse_location("0", "200").is_err());
        assert!(parse_location("norte", "0").is_err());
        assert!(parse_location("", "0").is_err());
    }
}
