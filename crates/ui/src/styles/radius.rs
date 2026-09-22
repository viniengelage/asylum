use gpui::{Pixels, Rems, Window, rems};

/// The corner radius shared by every pane-level surface: the pane itself, the tab strip that
/// sits on top of it, and the content clipped against those corners.
///
/// Panes, tab bars and pane content each used to spell this out on their own — three
/// `rounded_lg` calls and one literal `px(8.)` in a `paint_quad` — so changing one of them
/// silently left the others behind. Read the radius from here instead of restating it.
pub fn pane_corner_radius() -> Rems {
    rems(0.5)
}

/// [`pane_corner_radius`] resolved against the window's rem size, for paint-time callers that
/// build [`gpui::Corners`] directly instead of going through a style method.
pub fn pane_corner_radius_px(window: &Window) -> Pixels {
    pane_corner_radius() * window.rem_size()
}
