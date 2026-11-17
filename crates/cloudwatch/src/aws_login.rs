use std::time::{Duration, Instant};

use gpui::{Entity, Window};
use util::ResultExt as _;
use workspace::Workspace;

use crate::{CloudWatchStore, panel::credentials};

const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Long enough to finish signing in on the browser; past it the user can reconnect by hand.
const WATCH_LIMIT: Duration = Duration::from_secs(10 * 60);

/// Opens a terminal running `aws login` for the store's profile and reconnects once the login
/// cache under `~/.aws/login/cache` changes.
pub(crate) fn run_aws_login(
    workspace: &mut Workspace,
    store: Entity<CloudWatchStore>,
    window: &mut Window,
    cx: &mut gpui::Context<Workspace>,
) {
    let profile = store.read(cx).profile_name();
    let command_label = login_command(&profile);
    let task = task::SpawnInTerminal {
        id: task::TaskId(format!("cloudwatch-aws-login-{profile}")),
        full_label: command_label.clone(),
        label: "aws login".to_string(),
        command: Some("aws".into()),
        args: vec!["login".into(), "--profile".into(), profile],
        command_label,
        cwd: store.read(cx).project_root(cx),
        env: Default::default(),
        use_new_terminal: false,
        allow_concurrent_runs: false,
        reveal: task::RevealStrategy::Always,
        reveal_target: task::RevealTarget::Dock,
        hide: task::HideStrategy::Never,
        shell: task::Shell::System,
        show_summary: true,
        show_command: true,
        show_rerun: true,
        save: task::SaveStrategy::None,
    };
    workspace.spawn_in_terminal(task, window, cx).detach();

    let store = store.downgrade();
    let cache_dir = credentials::login_cache_dir();
    cx.spawn(async move |_, cx| {
        let initial = {
            let cache_dir = cache_dir.clone();
            cx.background_executor()
                .spawn(async move { credentials::login_cache_fingerprint(&cache_dir) })
                .await
        };
        let started = Instant::now();
        while started.elapsed() < WATCH_LIMIT {
            cx.background_executor().timer(POLL_INTERVAL).await;
            if store.upgrade().is_none() {
                return;
            }
            let current = {
                let cache_dir = cache_dir.clone();
                cx.background_executor()
                    .spawn(async move { credentials::login_cache_fingerprint(&cache_dir) })
                    .await
            };
            if current != initial {
                store.update(cx, |store, cx| store.connect(cx)).log_err();
                return;
            }
        }
    })
    .detach();
}

pub(crate) fn login_command(profile: &str) -> String {
    format!("aws login --profile {profile}")
}
