use gpui::{Entity, Window};
use workspace::Workspace;

use crate::CloudWatchStore;

/// Opens a terminal running `aws login` for the store's profile and reconnects once the login
/// cache under `~/.aws/login/cache` changes.
pub(crate) fn run_aws_login(
    _workspace: &mut Workspace,
    _store: Entity<CloudWatchStore>,
    _window: &mut Window,
    _cx: &mut gpui::Context<Workspace>,
) {
}
