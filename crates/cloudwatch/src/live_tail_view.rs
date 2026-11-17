use gpui::{App, Window};
use workspace::Workspace;

pub struct LiveTailView;

pub fn open_live_tail(
    _workspace: &mut Workspace,
    _log_groups: Vec<String>,
    _window: &mut Window,
    _cx: &mut gpui::Context<Workspace>,
) {
}

pub(crate) fn init(_cx: &mut App) {}
