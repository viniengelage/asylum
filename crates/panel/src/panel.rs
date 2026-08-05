/// Marker trait for panels that draw their own header strip.
///
/// The header itself is built with `ui::HeaderBar` (or `ui::TabBar` when it is made of tabs), which
/// owns the shared height, fill and divider so that headers of neighbouring panes align.
pub trait PanelHeader: workspace::Panel {}
