use std::path::PathBuf;

use gpui::{App, Window};
use workspace::Workspace;

pub struct QueryView;

pub fn open_query(
    _workspace: &mut Workspace,
    _path: PathBuf,
    _window: &mut Window,
    _cx: &mut gpui::Context<Workspace>,
) {
}

pub(crate) fn init(_cx: &mut App) {}
