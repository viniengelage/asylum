use crate::{
    Account, Connection, RepoStore, ToggleFocus,
    api::{
        self, CheckState, CheckSummary, FileChange, PullRequest, PullRequestDetail,
        PullRequestState, RemoteRepository,
    },
    bitbucket::{self, MergeOutcome, MergeStrategy},
};
use anyhow::Context as _;
use askpass::AskPassDelegate;
use chrono::{DateTime, Utc};
use editor::Editor;
use git::repository::PushOptions;
use git::repository::{FetchOptions, Remote};
use git_ui::{branch_diff::BranchDiff, commit_view::CommitView};
use git_ui_core::askpass_modal::AskPassModal;
use gpui::AsyncApp;
use gpui::{
    Action as _, ClipboardItem, Div, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    Hsla, Pixels, Stateful, Subscription, Task, WeakEntity, px,
};
use http_client::HttpClient;
use project::{
    Project,
    git_store::{GitStoreEvent, Repository},
};
use std::{collections::HashSet, future::Future, sync::Arc, time::Duration, time::Instant};
use ui::{
    Checkbox, CommonAnimationExt, ContextMenu, ContextMenuEntry, PopoverMenu, Tab, TabBar,
    TabPosition, TabStyle, Tooltip, prelude::*,
};
use ui_input::{ErasedEditorEvent, InputField};
use util::ResultExt as _;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

/// Bitbucket allows 1,000 repository requests an hour; a list and one checks call every 90
/// seconds stays far below it with several windows open.
const SYNC_INTERVAL: Duration = Duration::from_secs(90);
/// Longer descriptions are cut here until "Mostrar mais" is clicked.
const DESCRIPTION_PREVIEW_CHARS: usize = 280;

/// What the project's active repository is, as far as the panel is concerned.
#[derive(Clone, PartialEq, Eq)]
enum Target {
    NoRepository,
    /// A remote the panel can't talk to yet, or none at all.
    Unsupported {
        remote_url: Option<String>,
    },
    Hosted {
        repository: RemoteRepository,
        remote_name: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Group {
    AwaitingYourReview,
    Yours,
    Others,
}

impl Group {
    fn title(self) -> &'static str {
        match self {
            Group::AwaitingYourReview => "PEDEM SUA REVISÃO",
            Group::Yours => "SUAS",
            Group::Others => "OUTRAS ABERTAS",
        }
    }

    fn color(self, cx: &App) -> Hsla {
        match self {
            Group::AwaitingYourReview => Color::Info.color(cx),
            Group::Yours => Color::Accent.color(cx),
            Group::Others => Color::Muted.color(cx),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ListTab {
    PullRequests,
    Issues,
    Pipelines,
}

/// What the list's other tabs show, loaded when they are first opened.
struct Loadable<T> {
    value: Option<T>,
    loading: bool,
    error: Option<SharedString>,
    task: Option<Task<()>>,
}

impl<T> Default for Loadable<T> {
    fn default() -> Self {
        Self {
            value: None,
            loading: false,
            error: None,
            task: None,
        }
    }
}

/// The open pull requests of the target repository, as of the last sync.
#[derive(Default)]
struct PullRequests {
    list: Vec<PullRequest>,
    /// The checks of the checked-out branch's pull request, by number.
    branch_checks: Option<(u64, Vec<api::Check>)>,
    synced_at: Option<Instant>,
    is_syncing: bool,
    /// Why the last sync failed; the pull requests from the sync before it stay visible.
    error: Option<SharedString>,
}

enum PendingAction {
    Checkout,
    Approval,
    /// Fetching a commit that isn't in the local repository yet, to open it.
    OpeningCommit,
    /// Fetching the destination branch, to diff against its latest state.
    OpeningDiff,
    Declining,
    /// Downloading the pull request's diff for the agent.
    Reviewing,
}

/// The merge card that replaces the approvals card while a merge is being set up.
struct MergeForm {
    strategies: Vec<MergeStrategy>,
    strategy: Option<MergeStrategy>,
    message_editor: Entity<Editor>,
    close_source_branch: bool,
    /// Switch to the destination, pull it and delete the local branch once merged.
    clean_up_local: bool,
    submitting: bool,
    progress: Option<SharedString>,
    _load_task: Task<()>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DetailTab {
    Overview,
    Commits,
    Files,
}

/// A pull request opened from the list. The row it came from shows right away while the rest
/// loads.
struct OpenPullRequest {
    summary: PullRequest,
    detail: Option<PullRequestDetail>,
    error: Option<SharedString>,
    comment_input: Entity<InputField>,
    posting_comment: bool,
    description_expanded: bool,
    tab: DetailTab,
    merging: Option<MergeForm>,
    commits_focus: FocusHandle,
    /// The commit ↑↓ moved to in the Commits tab.
    selected_commit: Option<usize>,
    /// A success worth saying, such as a merge whose local cleanup also went through.
    notice: Option<SharedString>,
    pending_action: Option<PendingAction>,
    load_task: Option<Task<()>>,
}

/// What the local checkout says about the branch a new pull request would come from.
struct LocalBranchStatus {
    local_name: String,
    /// No upstream on the target remote yet, or it was deleted there: pushing publishes it.
    needs_upstream: bool,
    ahead: u32,
    behind: u32,
    /// Tracking a branch on another remote, which is a fork's pull request.
    tracks_other_remote: bool,
    has_uncommitted_changes: bool,
    head_subject: Option<String>,
}

/// The "Novo pull request" form, shown in place of the list.
struct CreateForm {
    source_branch: String,
    destination: Option<String>,
    branches: Vec<String>,
    title_input: Entity<InputField>,
    description_editor: Entity<Editor>,
    reviewers: Vec<api::Person>,
    candidates: Vec<api::Person>,
    close_source_branch: bool,
    draft: bool,
    /// Already on the remote and not on the destination, newest first.
    commits: Vec<api::Commit>,
    commits_loaded: bool,
    /// Title and description are filled from the commits once; after that they are the user's.
    autofilled: bool,
    error: Option<SharedString>,
    submitting: bool,
    _load_task: Task<()>,
    commits_task: Option<Task<()>>,
}

pub struct RepoPanel {
    focus_handle: FocusHandle,
    store: Entity<RepoStore>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    email_input: Entity<InputField>,
    token_input: Entity<InputField>,
    connect_error: Option<SharedString>,
    connect_task: Option<Task<()>>,
    /// Set from the account menu: shows the token fields again while the current token keeps
    /// working until a new one replaces it.
    replacing_token: bool,
    target: Target,
    /// The checked-out branch as the remote names it, which is what pull requests refer to.
    branch: Option<String>,
    pull_requests: PullRequests,
    sync_task: Option<Task<()>>,
    filter_input: Entity<InputField>,
    collapsed_groups: HashSet<Group>,
    open_pull_request: Option<OpenPullRequest>,
    creating: Option<CreateForm>,
    list_tab: ListTab,
    /// Which pull requests the list shows; only the open ones are kept in sync.
    state_filter: PullRequestState,
    closed_pull_requests: Loadable<(PullRequestState, Vec<PullRequest>)>,
    pipelines: Loadable<Vec<api::Pipeline>>,
    /// `None` inside when the repository has no issue tracker.
    issues: Loadable<Option<Vec<api::Issue>>>,
    _subscriptions: Vec<Subscription>,
}

impl RepoPanel {
    pub(crate) fn new(
        store: Entity<RepoStore>,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let email_input = cx
            .new(|cx| InputField::new(window, cx, "voce@empresa.com").start_icon(IconName::AtSign));
        let token_input = cx.new(|cx| {
            InputField::new(window, cx, "ATATT…")
                .start_icon(IconName::Lock)
                .masked(true)
        });
        let filter_input = cx.new(|cx| {
            InputField::new(window, cx, "Filtrar por título, # ou autor")
                .start_icon(IconName::MagnifyingGlass)
        });

        let mut subscriptions = Vec::new();
        for input in [&email_input, &token_input, &filter_input] {
            let editor = input.read(cx).editor().clone();
            let this = cx.weak_entity();
            subscriptions.push(editor.subscribe(
                Box::new(move |event, _window, cx| {
                    if event == ErasedEditorEvent::BufferEdited {
                        this.update(cx, |this, cx| {
                            this.connect_error = None;
                            cx.notify();
                        })
                        .log_err();
                    }
                }),
                window,
                cx,
            ));
        }
        subscriptions.push(cx.observe(&store, |this, _, cx| this.connection_changed(cx)));
        let git_store = project.read(cx).git_store().clone();
        subscriptions.push(cx.subscribe(&git_store, |this, _, event, cx| match event {
            GitStoreEvent::ActiveRepositoryChanged(_)
            | GitStoreEvent::RepositoryUpdated(_, _, true)
            | GitStoreEvent::RepositoryAdded
            | GitStoreEvent::RepositoryRemoved(_) => this.refresh_target(cx),
            _ => {}
        }));

        let mut this = Self {
            focus_handle: cx.focus_handle(),
            store,
            project,
            workspace,
            email_input,
            token_input,
            connect_error: None,
            connect_task: None,
            replacing_token: false,
            target: Target::NoRepository,
            branch: None,
            pull_requests: PullRequests::default(),
            sync_task: None,
            filter_input,
            collapsed_groups: HashSet::default(),
            open_pull_request: None,
            creating: None,
            list_tab: ListTab::PullRequests,
            state_filter: PullRequestState::Open,
            closed_pull_requests: Loadable::default(),
            pipelines: Loadable::default(),
            issues: Loadable::default(),
            _subscriptions: subscriptions,
        };
        this.refresh_target(cx);
        this
    }

    fn account(&self, cx: &App) -> Option<Account> {
        let Target::Hosted { repository, .. } = &self.target else {
            return None;
        };
        match self.store.read(cx).connection(repository.hosting) {
            Connection::Connected(account) => Some(account.clone()),
            _ => None,
        }
    }

    fn refresh_target(&mut self, cx: &mut Context<Self>) {
        let (target, branch) = detect_target(&self.project, cx);
        let branch_changed = branch != self.branch;
        self.branch = branch;
        if target != self.target {
            self.target = target;
            self.pull_requests = PullRequests::default();
            self.open_pull_request = None;
            self.creating = None;
            self.closed_pull_requests = Loadable::default();
            self.pipelines = Loadable::default();
            self.issues = Loadable::default();
            self.sync(cx);
            self.load_tab_data(cx);
        } else if branch_changed {
            // The branch's pull request may be a different one, with other checks.
            self.sync(cx);
        }
        cx.notify();
    }

    fn connection_changed(&mut self, cx: &mut Context<Self>) {
        let is_connected = self.account(cx).is_some();
        if !is_connected {
            self.sync_task = None;
            self.pull_requests = PullRequests::default();
            self.open_pull_request = None;
            self.creating = None;
        } else if self.sync_task.is_none() {
            self.sync(cx);
        }
        cx.notify();
    }

    /// Syncs now and then every [`SYNC_INTERVAL`].
    fn sync(&mut self, cx: &mut Context<Self>) {
        let Target::Hosted { repository, .. } = self.target.clone() else {
            self.sync_task = None;
            return;
        };
        let Some(credentials) = self.store.read(cx).bitbucket_credentials() else {
            self.sync_task = None;
            return;
        };
        let http_client = self.store.read(cx).http_client();
        self.pull_requests.is_syncing = true;
        cx.notify();

        self.sync_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let Ok(branch) = this.read_with(cx, |this, _| this.branch.clone()) else {
                    return;
                };
                let result = cx
                    .background_spawn({
                        let http_client = http_client.clone();
                        let credentials = credentials.clone();
                        let repository = repository.clone();
                        async move {
                            let list = bitbucket::list_open_pull_requests(
                                &http_client,
                                &credentials,
                                &repository,
                            )
                            .await?;
                            let branch_number = branch.and_then(|branch| {
                                list.iter()
                                    .find(|pull_request| pull_request.source_branch == branch)
                                    .map(|pull_request| pull_request.number)
                            });
                            let branch_checks = match branch_number {
                                Some(number) => bitbucket::get_checks(
                                    &http_client,
                                    &credentials,
                                    &repository,
                                    number,
                                )
                                .await
                                .log_err()
                                .map(|checks| (number, checks)),
                                None => None,
                            };
                            anyhow::Ok((list, branch_checks))
                        }
                    })
                    .await;

                let keep_syncing = this
                    .update(cx, |this, cx| this.finish_sync(result, cx))
                    .unwrap_or(false);
                if !keep_syncing {
                    return;
                }
                cx.background_executor().timer(SYNC_INTERVAL).await;
                this.update(cx, |this, cx| {
                    this.pull_requests.is_syncing = true;
                    cx.notify();
                })
                .log_err();
            }
        }));
    }

    /// Returns whether syncing should go on.
    fn finish_sync(
        &mut self,
        result: anyhow::Result<(Vec<PullRequest>, Option<(u64, Vec<api::Check>)>)>,
        cx: &mut Context<Self>,
    ) -> bool {
        self.pull_requests.is_syncing = false;
        let keep_syncing = match result {
            Ok((list, branch_checks)) => {
                if matches!(self.list_tab, ListTab::Pipelines | ListTab::Issues) {
                    self.load_tab_data(cx);
                }
                self.pull_requests.list = list;
                self.pull_requests.branch_checks = branch_checks;
                self.pull_requests.synced_at = Some(Instant::now());
                self.pull_requests.error = None;
                true
            }
            Err(error) if crate::is_unauthorized(&error) => {
                self.store.update(cx, |store, cx| store.mark_rejected(cx));
                false
            }
            Err(error) => {
                log::warn!("Repo: falha ao sincronizar os pull requests: {error:#}");
                self.pull_requests.error = Some(format!("{error:#}").into());
                true
            }
        };
        cx.notify();
        keep_syncing
    }

    fn connect(&mut self, _: &menu::Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        if self.connect_task.is_some() {
            return;
        }
        let email = self.email_input.read(cx).text(cx).trim().to_string();
        let token = self.token_input.read(cx).text(cx).trim().to_string();
        if email.is_empty() || token.is_empty() {
            return;
        }
        if !email.contains('@') {
            self.connect_error =
                Some("Use o e-mail da conta Atlassian, não o usuário do Bitbucket.".into());
            cx.notify();
            return;
        }
        let validation = self.store.read(cx).validate_bitbucket(email, token, cx);
        self.connect_error = None;
        cx.notify();

        self.connect_task = Some(cx.spawn(async move |this, cx| {
            let result = match validation.await {
                Ok(account) => {
                    let saved = this
                        .update(cx, |this, cx| {
                            this.store.update(cx, |store, cx| store.save(account, cx))
                        })
                        .log_err();
                    match saved {
                        Some(saved) => saved.await.map_err(|error| {
                            log::error!("Repo: falha ao salvar o token no keychain: {error:#}");
                            SharedString::from("Não foi possível salvar o token no keychain.")
                        }),
                        None => return,
                    }
                }
                Err(error) if crate::is_unauthorized(&error) => Err(
                    "O Bitbucket recusou. Confira o e-mail da conta e se o token foi copiado inteiro."
                        .into(),
                ),
                // Checking the token only reads the account, so a missing scope here is that one.
                Err(error) if crate::is_forbidden(&error) => Err(
                    "Falta o escopo read:user:bitbucket. Escopos não podem ser editados: crie outro token marcando-o também."
                        .into(),
                ),
                Err(error) => Err(format!("Não foi possível falar com o Bitbucket: {error}").into()),
            };
            this.update_in(cx, |this, window, cx| {
                this.connect_task = None;
                match result {
                    Ok(()) => {
                        this.replacing_token = false;
                        this.token_input
                            .update(cx, |input, cx| input.clear(window, cx));
                    }
                    Err(message) => this.connect_error = Some(message),
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    fn start_replacing_token(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.replacing_token = true;
        self.connect_error = None;
        if let Some(account) = self.account(cx) {
            let email = account.credentials.email;
            self.email_input
                .update(cx, |input, cx| input.set_text(&email, window, cx));
        }
        self.token_input
            .update(cx, |input, cx| input.clear(window, cx));
        window.focus(&self.token_input.focus_handle(cx), cx);
        cx.notify();
    }

    fn render_badge(&self, cx: &App) -> impl IntoElement {
        h_flex()
            .size(DynamicSpacing::Base48.px(cx))
            .justify_center()
            .rounded_lg()
            .bg(cx.theme().colors().ghost_element_hover)
            .border_1()
            .border_color(cx.theme().colors().border_focused.opacity(0.35))
            .child(
                Icon::new(IconName::PullRequest)
                    .size(IconSize::Medium)
                    .color(Color::Accent),
            )
    }

    fn render_step(&self, number: usize, title: &'static str, cx: &App) -> impl IntoElement {
        h_flex()
            .items_start()
            .gap(DynamicSpacing::Base10.px(cx))
            .child(
                h_flex()
                    .flex_none()
                    .size(DynamicSpacing::Base20.px(cx))
                    .justify_center()
                    .rounded_full()
                    .bg(cx.theme().colors().ghost_element_hover)
                    .child(
                        Label::new(number.to_string())
                            .size(LabelSize::XSmall)
                            .weight(FontWeight::SEMIBOLD)
                            .color(Color::Accent),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Label::new(title).size(LabelSize::Small)),
            )
    }

    fn render_connect(
        &self,
        repository: &RemoteRepository,
        show_rejected: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_busy = self.connect_task.is_some();
        let can_connect = !is_busy
            && !self.email_input.read(cx).is_empty(cx)
            && !self.token_input.read(cx).is_empty(cx);

        let hint: AnyElement = match &self.connect_error {
            Some(error) => Label::new(error.clone())
                .size(LabelSize::XSmall)
                .color(Color::Error)
                .into_any_element(),
            None if is_busy => h_flex()
                .gap(DynamicSpacing::Base06.px(cx))
                .child(
                    Icon::new(IconName::LoadCircle)
                        .size(IconSize::XSmall)
                        .color(Color::Muted)
                        .with_rotate_animation(2),
                )
                .child(
                    Label::new("Verificando com o Bitbucket…")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .into_any_element(),
            None => Label::new("Autentica com o e-mail Atlassian. Fica no Keychain do perfil.")
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .into_any_element(),
        };

        let card = v_flex()
            .w_full()
            .gap(DynamicSpacing::Base12.px(cx))
            .p(DynamicSpacing::Base12.px(cx))
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().editor_background)
            .child(
                v_flex()
                    .gap(DynamicSpacing::Base08.px(cx))
                    .child(self.render_step(
                        1,
                        "Crie um API token com escopos para o Bitbucket, marcando estes:",
                        cx,
                    ))
                    .child(v_flex().pl(DynamicSpacing::Base32.px(cx)).children(
                        bitbucket::TOKEN_SCOPES.iter().map(|scope| {
                            Label::new(*scope)
                                .size(LabelSize::XSmall)
                                .buffer_font(cx)
                                .color(Color::Muted)
                        }),
                    ))
                    .child(
                        h_flex().pl(DynamicSpacing::Base32.px(cx)).child(
                            Button::new("repo-open-token-settings", "Abrir id.atlassian.com")
                                .style(ButtonStyle::Outlined)
                                .size(ButtonSize::Medium)
                                .label_size(LabelSize::Small)
                                .start_icon(
                                    Icon::new(IconName::ArrowUpRight)
                                        .size(IconSize::Small)
                                        .color(Color::Muted),
                                )
                                .on_click(|_, _, cx| cx.open_url(bitbucket::TOKEN_SETTINGS_URL)),
                        ),
                    ),
            )
            .child(div().h_px().w_full().bg(cx.theme().colors().border_variant))
            .child(
                v_flex()
                    .gap(DynamicSpacing::Base08.px(cx))
                    .child(self.render_step(2, "Cole o token e o e-mail da conta", cx))
                    .child(self.email_input.clone())
                    .child(self.token_input.clone())
                    .child(hint),
            )
            .child(
                Button::new("repo-connect", "Conectar")
                    .full_width()
                    .style(ButtonStyle::Filled)
                    .size(ButtonSize::Large)
                    .label_size(LabelSize::Small)
                    .disabled(!can_connect)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.connect(&menu::Confirm, window, cx)),
                    ),
            )
            .when(self.replacing_token, |this| {
                this.child(
                    h_flex().justify_center().child(
                        Button::new("repo-cancel-replace", "Manter o token atual")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.replacing_token = false;
                                this.connect_error = None;
                                cx.notify();
                            })),
                    ),
                )
            });

        v_flex()
            .key_context("RepoConnect")
            .on_action(cx.listener(Self::connect))
            .size_full()
            .child(
                v_flex()
                    .id("repo-connect-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .justify_center()
                    .items_center()
                    .gap(DynamicSpacing::Base20.px(cx))
                    .px(DynamicSpacing::Base24.px(cx))
                    .py(DynamicSpacing::Base24.px(cx))
                    .when(show_rejected, |this| {
                        this.child(render_warning(
                            "O Bitbucket recusou o token salvo",
                            "Expirou ou foi revogado? Cole um token novo abaixo.",
                            cx,
                        ))
                    })
                    .child(self.render_badge(cx))
                    .child(
                        v_flex()
                            .items_center()
                            .gap(DynamicSpacing::Base06.px(cx))
                            .child(
                                Label::new(format!("Conecte o {}", repository.hosting.name()))
                                    .size(LabelSize::Large)
                                    .weight(FontWeight::SEMIBOLD),
                            )
                            .child(
                                Label::new(format!(
                                    "origin aponta para bitbucket.org/{}.",
                                    repository.owner
                                ))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            ),
                    )
                    .child(card),
            )
            .child(
                h_flex()
                    .items_start()
                    .gap(DynamicSpacing::Base08.px(cx))
                    .px(DynamicSpacing::Base16.px(cx))
                    .py(DynamicSpacing::Base12.px(cx))
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        Icon::new(IconName::Lock)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(
                            "O token só vale para este perfil. Outros perfis não veem este repositório nem os PRs dele.",
                        )
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    ),
            )
    }

    fn render_centered_message(
        &self,
        icon: IconName,
        title: SharedString,
        detail: Option<SharedString>,
        actions: Option<AnyElement>,
        cx: &App,
    ) -> impl IntoElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap(DynamicSpacing::Base10.px(cx))
            .px(DynamicSpacing::Base24.px(cx))
            .child(Icon::new(icon).size(IconSize::Medium).color(Color::Muted))
            .child(
                Label::new(title)
                    .size(LabelSize::Small)
                    .weight(FontWeight::SEMIBOLD),
            )
            .when_some(detail, |this, detail| {
                this.child(
                    Label::new(detail)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .children(actions)
    }

    fn render_loading(&self, title: &'static str, cx: &App) -> AnyElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap(DynamicSpacing::Base10.px(cx))
            .child(spinning_if(
                Icon::new(IconName::LoadCircle)
                    .size(IconSize::Medium)
                    .color(Color::Muted),
                true,
            ))
            .child(Label::new(title).size(LabelSize::Small).color(Color::Muted))
            .into_any_element()
    }

    fn render_account_menu(&self, cx: &Context<Self>) -> impl IntoElement {
        let panel = cx.weak_entity();
        let store = self.store.clone();
        PopoverMenu::new("repo-account-menu")
            .trigger_with_tooltip(
                IconButton::new("repo-account-settings", IconName::Settings)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted),
                Tooltip::text("Conta do Bitbucket"),
            )
            .anchor(gpui::Anchor::BottomRight)
            .menu(move |window, cx| {
                let panel = panel.clone();
                let store = store.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    menu.item(
                        ContextMenuEntry::new("Trocar token")
                            .icon(IconName::Pencil)
                            .icon_color(Color::Muted)
                            .handler({
                                let panel = panel.clone();
                                move |window, cx| {
                                    panel
                                        .update(cx, |panel, cx| {
                                            panel.start_replacing_token(window, cx)
                                        })
                                        .log_err();
                                }
                            }),
                    )
                    .item(
                        ContextMenuEntry::new("Gerenciar tokens na Atlassian")
                            .icon(IconName::ArrowUpRight)
                            .icon_color(Color::Muted)
                            .handler(|_, cx| cx.open_url(bitbucket::TOKEN_SETTINGS_URL)),
                    )
                    .separator()
                    .item(
                        ContextMenuEntry::new("Desconectar")
                            .icon(IconName::Exit)
                            .icon_color(Color::Error)
                            .handler(move |_, cx| {
                                store
                                    .update(cx, |store, cx| store.disconnect(cx))
                                    .detach_and_log_err(cx);
                            }),
                    )
                }))
            })
    }

    fn render_context_bar(
        &self,
        repository: &RemoteRepository,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let web_url = repository.web_url();
        h_flex()
            .gap(DynamicSpacing::Base08.px(cx))
            .px(DynamicSpacing::Base12.px(cx))
            .py(DynamicSpacing::Base08.px(cx))
            .child(
                h_flex()
                    .flex_none()
                    .size(DynamicSpacing::Base16.px(cx))
                    .justify_center()
                    .rounded_sm()
                    .bg(Color::Info.color(cx))
                    .child(
                        Label::new("B")
                            .size(LabelSize::XSmall)
                            .weight(FontWeight::BOLD)
                            .color(Color::Custom(gpui::white())),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(DynamicSpacing::Base06.px(cx))
                    .child(
                        Label::new(repository.full_name())
                            .size(LabelSize::Small)
                            .truncate(),
                    )
                    .child(
                        Label::new("/")
                            .size(LabelSize::Small)
                            .color(Color::Disabled),
                    )
                    .child(
                        Label::new(repository.hosting.name())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                IconButton::new("repo-open-web", IconName::ArrowUpRight)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text("Abrir no Bitbucket"))
                    .on_click(move |_, _, cx| cx.open_url(&web_url)),
            )
            .child(
                IconButton::new("repo-sync", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .disabled(self.pull_requests.is_syncing)
                    .tooltip(Tooltip::text("Sincronizar agora"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sync(cx);
                        this.load_tab_data(cx);
                    })),
            )
    }

    fn branch_pull_request(&self) -> Option<&PullRequest> {
        let branch = self.branch.as_deref()?;
        self.pull_requests
            .list
            .iter()
            .find(|pull_request| pull_request.source_branch == branch)
    }

    fn render_branch_card(
        &self,
        repository: &RemoteRepository,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let branch = self.branch.clone()?;
        let card = v_flex()
            .mx(DynamicSpacing::Base08.px(cx))
            .mb(DynamicSpacing::Base08.px(cx))
            .gap(DynamicSpacing::Base08.px(cx))
            .p(DynamicSpacing::Base10.px(cx))
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().colors().border_focused.opacity(0.6))
            .bg(cx.theme().colors().element_background);
        let header = |number: Option<u64>| {
            h_flex()
                .gap(DynamicSpacing::Base06.px(cx))
                .child(
                    Label::new("PR DO BRANCH")
                        .size(LabelSize::XSmall)
                        .weight(FontWeight::SEMIBOLD)
                        .color(Color::Accent),
                )
                .child(
                    Label::new("·")
                        .size(LabelSize::XSmall)
                        .color(Color::Disabled),
                )
                .child(
                    div().flex_1().min_w_0().child(
                        Label::new(branch.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .buffer_font(cx)
                            .truncate(),
                    ),
                )
                .when_some(number, |this, number| {
                    this.child(
                        Label::new(format!("#{number}"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                })
        };

        let Some(pull_request) = self.branch_pull_request() else {
            if self.pull_requests.synced_at.is_none() {
                return None;
            }
            let new_url = format!(
                "{}/pull-requests/new?source={}",
                repository.web_url(),
                urlencoding::encode(&branch)
            );
            return Some(
                card.child(header(None))
                    .child(
                        Label::new("Nenhum pull request aberto para este branch")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        h_flex()
                            .gap(DynamicSpacing::Base04.px(cx))
                            .child(
                                Button::new("repo-branch-create", "Criar pull request")
                                    .style(ButtonStyle::Filled)
                                    .label_size(LabelSize::Small)
                                    .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.start_creating(window, cx)
                                    })),
                            )
                            .child(
                                IconButton::new("repo-branch-create-web", IconName::ArrowUpRight)
                                    .icon_size(IconSize::Small)
                                    .icon_color(Color::Muted)
                                    .tooltip(Tooltip::text("Criar no Bitbucket"))
                                    .on_click(move |_, _, cx| cx.open_url(&new_url)),
                            ),
                    )
                    .into_any_element(),
            );
        };

        let checks = self
            .pull_requests
            .branch_checks
            .as_ref()
            .filter(|(number, _)| *number == pull_request.number)
            .map(|(_, checks)| CheckSummary::of(checks));
        let reviewer_count = pull_request.reviewers().count();
        let approvals = pull_request.approval_count();
        let task_id = clickup_task_id(&pull_request.source_branch);
        let opened = pull_request.clone();
        let opened_for_comment = pull_request.clone();
        let url = pull_request.url.clone();

        Some(
            card.child(header(Some(pull_request.number)))
                .child(
                    Label::new(pull_request.title.clone())
                        .size(LabelSize::Default)
                        .weight(FontWeight::SEMIBOLD),
                )
                .child(
                    h_flex()
                        .flex_wrap()
                        .gap(DynamicSpacing::Base06.px(cx))
                        .child(state_pill(pull_request, cx))
                        .when_some(task_id, |this, task_id| {
                            this.child(pill(task_id, Color::Success, None, cx))
                        })
                        .when(pull_request.comment_count > 0, |this| {
                            this.child(pill(
                                comment_count_label(pull_request.comment_count),
                                Color::Warning,
                                Some(IconName::Chat),
                                cx,
                            ))
                        }),
                )
                .child(
                    h_flex()
                        .gap(DynamicSpacing::Base08.px(cx))
                        .p(DynamicSpacing::Base08.px(cx))
                        .rounded_md()
                        .bg(cx.theme().colors().editor_background)
                        .child(match checks {
                            Some(summary) if summary.total > 0 => h_flex()
                                .gap(DynamicSpacing::Base04.px(cx))
                                .child(
                                    div()
                                        .size(px(6.))
                                        .rounded_full()
                                        .bg(check_summary_color(summary).color(cx)),
                                )
                                .child(
                                    Label::new(format!("{}/{}", summary.passed, summary.total))
                                        .size(LabelSize::Default)
                                        .weight(FontWeight::SEMIBOLD)
                                        .color(check_summary_color(summary)),
                                )
                                .into_any_element(),
                            _ => Label::new("—")
                                .size(LabelSize::Default)
                                .color(Color::Muted)
                                .into_any_element(),
                        })
                        .child(
                            div().flex_1().min_w_0().child(
                                Label::new(format!(
                                    "checks · {}",
                                    approval_label(approvals, reviewer_count)
                                ))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                            ),
                        )
                        .child(
                            Button::new("repo-branch-open", "Abrir")
                                .style(ButtonStyle::Filled)
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_pull_request(opened.clone(), false, window, cx)
                                })),
                        ),
                )
                .child(
                    h_flex()
                        .gap(DynamicSpacing::Base04.px(cx))
                        .child(
                            Button::new("repo-branch-comment", "Comentar")
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .start_icon(Icon::new(IconName::Chat).size(IconSize::XSmall))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_pull_request(
                                        opened_for_comment.clone(),
                                        true,
                                        window,
                                        cx,
                                    )
                                })),
                        )
                        .child(div().flex_1())
                        .child(
                            Button::new("repo-branch-web", "Abrir no Bitbucket")
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .start_icon(
                                    Icon::new(IconName::ArrowUpRight).size(IconSize::XSmall),
                                )
                                .on_click(move |_, _, cx| cx.open_url(&url)),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_row(
        &self,
        pull_request: &PullRequest,
        dot: Hsla,
        highlight_recent: bool,
        now: DateTime<Utc>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let opened = pull_request.clone();
        let is_branch = self.branch.as_deref() == Some(pull_request.source_branch.as_str());
        let age = pull_request
            .updated_at
            .map(|updated_at| compact_age(updated_at, now));
        let is_recent = pull_request
            .updated_at
            .is_some_and(|updated_at| (now - updated_at).num_hours() < 24);
        let author = if self
            .account(cx)
            .is_some_and(|account| account.user.id == pull_request.author.id)
        {
            "você".to_string()
        } else {
            pull_request.author.short_name().to_string()
        };

        h_flex()
            .id(SharedString::from(format!(
                "repo-pr-{}",
                pull_request.number
            )))
            .items_start()
            .gap(DynamicSpacing::Base10.px(cx))
            .px(DynamicSpacing::Base08.px(cx))
            .py(DynamicSpacing::Base06.px(cx))
            .rounded_md()
            .cursor_pointer()
            .when(is_branch, |this| {
                this.bg(cx.theme().colors().ghost_element_selected)
            })
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_pull_request(opened.clone(), false, window, cx)
            }))
            .child(
                h_flex()
                    .flex_none()
                    .h(DynamicSpacing::Base20.px(cx))
                    .child(div().size(px(8.)).rounded_full().bg(dot)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap(DynamicSpacing::Base06.px(cx))
                            .when(pull_request.draft, |this| {
                                this.child(
                                    Label::new("Rascunho")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                            })
                            .child(
                                Label::new(pull_request.title.clone())
                                    .size(LabelSize::Small)
                                    .truncate(),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap(DynamicSpacing::Base06.px(cx))
                            .min_w_0()
                            .child(
                                Label::new(format!("#{}", pull_request.number))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .buffer_font(cx),
                            )
                            .child(
                                Label::new("·")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Disabled),
                            )
                            .child(
                                Label::new(format!(
                                    "{author} → {}",
                                    pull_request.destination_branch
                                ))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .flex_none()
                    .h(DynamicSpacing::Base20.px(cx))
                    .gap(DynamicSpacing::Base06.px(cx))
                    .when(pull_request.comment_count > 0, |this| {
                        this.child(
                            h_flex()
                                .gap_0p5()
                                .child(
                                    Icon::new(IconName::Chat)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new(pull_request.comment_count.to_string())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                ),
                        )
                    })
                    .when_some(age, |this, age| {
                        this.child(Label::new(age).size(LabelSize::XSmall).color(
                            if is_recent && highlight_recent {
                                Color::Warning
                            } else {
                                Color::Muted
                            },
                        ))
                    }),
            )
    }

    fn render_group_header(
        &self,
        group: Group,
        count: usize,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let is_collapsed = self.collapsed_groups.contains(&group);
        h_flex()
            .id(SharedString::from(format!("repo-group-{}", group.title())))
            .gap(DynamicSpacing::Base06.px(cx))
            .px(DynamicSpacing::Base06.px(cx))
            .py(DynamicSpacing::Base04.px(cx))
            .mt(DynamicSpacing::Base04.px(cx))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.collapsed_groups.remove(&group) {
                    this.collapsed_groups.insert(group);
                }
                cx.notify();
            }))
            .child(
                Icon::new(if is_collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(IconSize::XSmall)
                .color(Color::Muted),
            )
            .child(div().size(px(6.)).rounded_full().bg(group.color(cx)))
            .child(
                Label::new(group.title())
                    .size(LabelSize::XSmall)
                    .weight(FontWeight::SEMIBOLD)
                    .color(Color::Muted),
            )
            .child(
                Label::new(count.to_string())
                    .size(LabelSize::XSmall)
                    .color(Color::Disabled),
            )
    }

    fn render_list(&self, account: &Account, cx: &Context<Self>) -> AnyElement {
        let filter = self.filter_input.read(cx).text(cx).trim().to_lowercase();
        let visible: Vec<&PullRequest> = self
            .pull_requests
            .list
            .iter()
            .filter(|pull_request| pull_request_matches(pull_request, &filter))
            .collect();

        if visible.is_empty() {
            if self.pull_requests.synced_at.is_none() {
                return self.render_loading("Buscando os pull requests…", cx);
            }
            let (icon, title) = if filter.is_empty() {
                (IconName::Check, "Nenhum pull request aberto")
            } else {
                (
                    IconName::MagnifyingGlass,
                    "Nenhum pull request com esse filtro",
                )
            };
            return self
                .render_centered_message(icon, title.into(), None, None, cx)
                .into_any_element();
        }

        let now = Utc::now();
        let mut list = v_flex()
            .id("repo-pr-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(DynamicSpacing::Base08.px(cx))
            .pb(DynamicSpacing::Base08.px(cx));
        for (group, pull_requests) in group_pull_requests(&visible, &account.user.id) {
            list = list.child(self.render_group_header(group, pull_requests.len(), cx));
            if !self.collapsed_groups.contains(&group) {
                for pull_request in pull_requests {
                    list = list.child(self.render_row(
                        pull_request,
                        group.color(cx),
                        group == Group::Yours,
                        now,
                        cx,
                    ));
                }
            }
        }
        list.into_any_element()
    }

    fn render_connected(
        &self,
        repository: &RemoteRepository,
        account: &Account,
        cx: &Context<Self>,
    ) -> AnyElement {
        if let Some(form) = &self.creating {
            return self.render_create_form(repository, form, cx);
        }
        if let Some(open_pull_request) = &self.open_pull_request {
            return self
                .render_detail(repository, account, open_pull_request, cx)
                .into_any_element();
        }
        let sync_label = match self.pull_requests.synced_at {
            None => "Sincronizando…".to_string(),
            Some(synced_at) => match synced_at.elapsed().as_secs() / 60 {
                0 => format!("{} · sincronizado agora", repository.hosting.name()),
                minutes => format!("{} · há {minutes} min", repository.hosting.name()),
            },
        };

        v_flex()
            .size_full()
            .child(self.render_context_bar(repository, cx))
            .children(self.render_branch_card(repository, cx))
            .child(
                div()
                    .px(DynamicSpacing::Base08.px(cx))
                    .pb(DynamicSpacing::Base08.px(cx))
                    .child(self.render_list_tabs(cx)),
            )
            .when(self.list_tab == ListTab::PullRequests, |this| {
                this.child(
                    h_flex()
                        .gap(DynamicSpacing::Base06.px(cx))
                        .px(DynamicSpacing::Base08.px(cx))
                        .pb(DynamicSpacing::Base08.px(cx))
                        .child(div().flex_1().min_w_0().child(self.filter_input.clone()))
                        .child(self.render_state_filter(cx)),
                )
                .when_some(self.pull_requests.error.clone(), |this, error| {
                    this.child(render_inline_error(
                        format!("Falha ao sincronizar: {error}"),
                        cx,
                    ))
                })
                .child(if self.state_filter == PullRequestState::Open {
                    self.render_list(account, cx)
                } else {
                    self.render_closed_list(cx)
                })
            })
            .when(self.list_tab == ListTab::Pipelines, |this| {
                this.child(self.render_pipelines(cx))
            })
            .when(self.list_tab == ListTab::Issues, |this| {
                this.child(self.render_issues(cx))
            })
            .child(
                h_flex()
                    .gap(DynamicSpacing::Base08.px(cx))
                    .px(DynamicSpacing::Base12.px(cx))
                    .py(DynamicSpacing::Base08.px(cx))
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(avatar(&account.user.display_name, Color::Accent, cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                Label::new(account.user.short_name().to_string())
                                    .size(LabelSize::Small)
                                    .truncate(),
                            )
                            .child(
                                Label::new(sync_label)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                    )
                    .child(self.render_account_menu(cx)),
            )
            .into_any_element()
    }
}

impl RepoPanel {
    fn open_pull_request(
        &mut self,
        pull_request: PullRequest,
        focus_composer: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let comment_input = cx.new(|cx| {
            InputField::new(
                window,
                cx,
                &format!("Responder no PR #{}…", pull_request.number),
            )
        });
        if focus_composer {
            window.focus(&comment_input.focus_handle(cx), cx);
        }
        self.open_pull_request = Some(OpenPullRequest {
            summary: pull_request,
            detail: None,
            error: None,
            comment_input,
            posting_comment: false,
            description_expanded: false,
            tab: DetailTab::Overview,
            merging: None,
            commits_focus: cx.focus_handle(),
            selected_commit: None,
            notice: None,
            pending_action: None,
            load_task: None,
        });
        self.reload_open_pull_request(cx);
        cx.notify();
    }

    fn close_pull_request(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_pull_request = None;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn reload_open_pull_request(&mut self, cx: &mut Context<Self>) {
        let Target::Hosted { repository, .. } = self.target.clone() else {
            return;
        };
        let Some(credentials) = self.store.read(cx).bitbucket_credentials() else {
            return;
        };
        let http_client = self.store.read(cx).http_client();
        let Some(open_pull_request) = &mut self.open_pull_request else {
            return;
        };
        let number = open_pull_request.summary.number;
        let load = cx.background_spawn(async move {
            bitbucket::get_pull_request_detail(&http_client, &credentials, &repository, number)
                .await
        });
        open_pull_request.load_task = Some(cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                let Some(open_pull_request) = &mut this.open_pull_request else {
                    return;
                };
                match result {
                    Ok(detail) => {
                        open_pull_request.summary = detail.pull_request.clone();
                        open_pull_request.detail = Some(detail);
                        open_pull_request.error = None;
                    }
                    Err(error) => {
                        if crate::is_unauthorized(&error) {
                            this.store.update(cx, |store, cx| store.mark_rejected(cx));
                            return;
                        }
                        open_pull_request.error = Some(format!("{error:#}").into());
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    fn post_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Target::Hosted { repository, .. } = self.target.clone() else {
            return;
        };
        let Some(credentials) = self.store.read(cx).bitbucket_credentials() else {
            return;
        };
        let http_client = self.store.read(cx).http_client();
        let Some(open_pull_request) = &mut self.open_pull_request else {
            return;
        };
        let text = open_pull_request
            .comment_input
            .read(cx)
            .text(cx)
            .trim()
            .to_string();
        if text.is_empty() || open_pull_request.posting_comment {
            return;
        }
        let number = open_pull_request.summary.number;
        open_pull_request.posting_comment = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    bitbucket::post_comment(&http_client, &credentials, &repository, number, &text)
                        .await
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                let Some(open_pull_request) = &mut this.open_pull_request else {
                    return;
                };
                open_pull_request.posting_comment = false;
                match result {
                    Ok(()) => {
                        open_pull_request
                            .comment_input
                            .update(cx, |input, cx| input.clear(window, cx));
                        this.reload_open_pull_request(cx);
                    }
                    Err(error) => {
                        open_pull_request.error =
                            Some(format!("Não deu para comentar: {error:#}").into());
                    }
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    fn toggle_approval(&mut self, approve: bool, cx: &mut Context<Self>) {
        let Target::Hosted { repository, .. } = self.target.clone() else {
            return;
        };
        let Some(credentials) = self.store.read(cx).bitbucket_credentials() else {
            return;
        };
        let http_client = self.store.read(cx).http_client();
        let Some(open_pull_request) = &mut self.open_pull_request else {
            return;
        };
        if open_pull_request.pending_action.is_some() {
            return;
        }
        let number = open_pull_request.summary.number;
        open_pull_request.pending_action = Some(PendingAction::Approval);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    bitbucket::set_approval(
                        &http_client,
                        &credentials,
                        &repository,
                        number,
                        approve,
                    )
                    .await
                })
                .await;
            this.update(cx, |this, cx| {
                let Some(open_pull_request) = &mut this.open_pull_request else {
                    return;
                };
                open_pull_request.pending_action = None;
                if let Err(error) = result {
                    open_pull_request.error = Some(
                        format!(
                            "Não deu para {}: {error:#}",
                            if approve {
                                "aprovar"
                            } else {
                                "remover a aprovação"
                            }
                        )
                        .into(),
                    );
                }
                this.reload_open_pull_request(cx);
                this.sync(cx);
            })
        })
        .detach_and_log_err(cx);
    }

    /// Asks for a password the same way the git panel does, in a modal over the workspace.
    fn askpass_delegate(
        &self,
        operation: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AskPassDelegate {
        let workspace = self.workspace.clone();
        let window = window.window_handle();
        AskPassDelegate::new_with_cancellation(
            &mut cx.to_async(),
            move |prompt, tx, cancellation, cx| {
                window
                    .update(cx, |_, window, cx| {
                        workspace.update(cx, |workspace, cx| {
                            workspace.toggle_modal(window, cx, |window, cx| {
                                AskPassModal::new(
                                    operation.into(),
                                    prompt.into(),
                                    tx,
                                    cancellation,
                                    window,
                                    cx,
                                )
                            });
                        })
                    })
                    .log_err();
            },
        )
    }

    fn set_pending_action(&mut self, action: Option<PendingAction>, cx: &mut Context<Self>) {
        if let Some(open_pull_request) = &mut self.open_pull_request {
            open_pull_request.pending_action = action;
            cx.notify();
        }
    }

    fn set_open_error(&mut self, error: String, cx: &mut Context<Self>) {
        if let Some(open_pull_request) = &mut self.open_pull_request {
            open_pull_request.error = Some(error.into());
            cx.notify();
        }
    }

    fn is_busy(&self) -> bool {
        self.open_pull_request
            .as_ref()
            .is_some_and(|open_pull_request| open_pull_request.pending_action.is_some())
    }

    /// Fetches the pull request's branch from the remote and switches to it, tracking it.
    fn checkout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Target::Hosted { remote_name, .. } = self.target.clone() else {
            return;
        };
        let Some(repository) = self.project.read(cx).active_repository(cx) else {
            return;
        };
        let Some(open_pull_request) = &self.open_pull_request else {
            return;
        };
        if self.is_busy() {
            return;
        }
        let branch = open_pull_request.summary.source_branch.clone();
        self.set_pending_action(Some(PendingAction::Checkout), cx);
        let askpass = self.askpass_delegate("git fetch", window, cx);
        cx.spawn(async move |this, cx| {
            let result = async {
                fetch_remote(&repository, &remote_name, askpass, cx).await?;
                let switch = repository.update(cx, |repository, _| {
                    repository.change_branch(format!("{remote_name}/{branch}"))
                });
                switch.await??;
                anyhow::Ok(())
            }
            .await;
            this.update(cx, |this, cx| {
                this.set_pending_action(None, cx);
                if let Err(error) = result {
                    this.set_open_error(format!("Não deu para fazer checkout: {error:#}"), cx);
                }
            })
        })
        .detach_and_log_err(cx);
    }

    /// Opens a commit in Zed's commit view. A commit this clone doesn't have yet is fetched
    /// first; one the remote doesn't have either (a fork's, or a force-pushed one) opens on the
    /// web instead.
    fn open_commit(&mut self, hash: String, window: &mut Window, cx: &mut Context<Self>) {
        let Target::Hosted {
            repository: remote_repository,
            remote_name,
        } = self.target.clone()
        else {
            return;
        };
        let web_url = format!("{}/commits/{hash}", remote_repository.web_url());
        let Some(repository) = self.project.read(cx).active_repository(cx) else {
            cx.open_url(&web_url);
            return;
        };
        if self.is_busy() {
            return;
        }
        self.set_pending_action(Some(PendingAction::OpeningCommit), cx);
        let askpass = self.askpass_delegate("git fetch", window, cx);
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |this, cx| {
            let mut is_local = has_commit(&repository, &hash, cx).await;
            if !is_local {
                match fetch_remote(&repository, &remote_name, askpass, cx).await {
                    Ok(()) => is_local = has_commit(&repository, &hash, cx).await,
                    Err(error) => log::warn!("Repo: fetch para abrir {hash} falhou: {error:#}"),
                }
            }
            this.update_in(cx, |this, window, cx| {
                this.set_pending_action(None, cx);
                if is_local {
                    CommitView::open(
                        hash,
                        repository.downgrade(),
                        workspace,
                        None,
                        None,
                        window,
                        cx,
                    );
                } else {
                    cx.open_url(&web_url);
                }
            })
        })
        .detach_and_log_err(cx);
    }

    /// Zed's branch diff against the pull request's destination as the remote has it now, which
    /// is what the pull request itself shows. The diff is of what's on disk, so the pull
    /// request's branch is checked out first when it isn't already.
    fn open_branch_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Target::Hosted {
            repository: remote_repository,
            remote_name,
        } = self.target.clone()
        else {
            return;
        };
        let Some(open_pull_request) = &self.open_pull_request else {
            return;
        };
        let Some(repository) = self.project.read(cx).active_repository(cx) else {
            return;
        };
        if self.is_busy() || open_pull_request.summary.is_from_fork(&remote_repository) {
            return;
        }
        let source_branch = open_pull_request.summary.source_branch.clone();
        let needs_checkout = self.branch.as_deref() != Some(source_branch.as_str());
        let base_ref: SharedString = format!(
            "{remote_name}/{}",
            open_pull_request.summary.destination_branch
        )
        .into();
        self.set_pending_action(Some(PendingAction::OpeningDiff), cx);
        let askpass = self.askpass_delegate("git fetch", window, cx);
        let workspace = self.workspace.clone();
        let project = self.project.clone();
        cx.spawn_in(window, async move |this, cx| {
            // A stale destination would show changes others already merged as if they were ours.
            let fetched = fetch_remote(&repository, &remote_name, askpass, cx).await;
            if needs_checkout {
                let switched = async {
                    fetched?;
                    let switch = repository.update(cx, |repository, _| {
                        repository.change_branch(format!("{remote_name}/{source_branch}"))
                    });
                    switch.await?
                }
                .await;
                if let Err(error) = switched {
                    this.update(cx, |this, cx| {
                        this.set_pending_action(None, cx);
                        this.set_open_error(
                            format!("Não deu para fazer checkout de {source_branch}: {error:#}"),
                            cx,
                        );
                    })?;
                    return anyhow::Ok(());
                }
            } else if let Err(error) = fetched {
                log::warn!("Repo: fetch antes do diff falhou, usando {base_ref} local: {error:#}");
            }
            this.update_in(cx, |this, window, cx| {
                this.set_pending_action(None, cx);
                workspace
                    .update(cx, |workspace, cx| {
                        BranchDiff::deploy_branch_diff_with_base_ref(
                            workspace, project, repository, base_ref, None, window, cx,
                        );
                    })
                    .log_err();
            })
        })
        .detach_and_log_err(cx);
    }

    fn render_detail_tabs(
        &self,
        open_pull_request: &OpenPullRequest,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let detail = open_pull_request.detail.as_ref();
        let tabs = [
            (DetailTab::Overview, "Visão geral", None),
            (
                DetailTab::Commits,
                "Commits",
                detail.map(|detail| detail.commits.len()),
            ),
            (
                DetailTab::Files,
                "Arquivos",
                detail.map(|detail| detail.files.len()),
            ),
        ];
        TabBar::new("repo-pr-tabs").style(TabStyle::Pill).children(
            tabs.into_iter()
                .enumerate()
                .map(|(index, (tab, label, count))| {
                    let selected = open_pull_request.tab == tab;
                    let position = match index {
                        0 => TabPosition::First,
                        2 => TabPosition::Last,
                        _ => TabPosition::Middle(std::cmp::Ordering::Equal),
                    };
                    Tab::new(label)
                        .style(TabStyle::Pill)
                        .position(position)
                        .toggle_state(selected)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(open_pull_request) = &mut this.open_pull_request {
                                open_pull_request.tab = tab;
                                cx.notify();
                            }
                        }))
                        .child(
                            h_flex()
                                .gap(DynamicSpacing::Base06.px(cx))
                                .child(Label::new(label).size(LabelSize::Small).color(
                                    if selected {
                                        Color::Default
                                    } else {
                                        Color::Muted
                                    },
                                ))
                                .when_some(count, |this, count| {
                                    this.child(
                                        Label::new(count.to_string())
                                            .size(LabelSize::XSmall)
                                            .color(if selected {
                                                Color::Accent
                                            } else {
                                                Color::Muted
                                            }),
                                    )
                                }),
                        )
                }),
        )
    }

    fn render_commits(
        &self,
        open_pull_request: &OpenPullRequest,
        is_checked_out: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let Some(detail) = open_pull_request.detail.as_ref() else {
            return self.render_loading("Carregando os commits…", cx);
        };
        let now = Utc::now();
        let opening = matches!(
            open_pull_request.pending_action,
            Some(PendingAction::OpeningCommit)
        );
        let mut list = v_flex()
            .track_focus(&open_pull_request.commits_focus)
            .key_context("RepoCommits")
            .on_action(
                cx.listener(|this, _: &menu::SelectNext, _, cx| this.move_commit_selection(1, cx)),
            )
            .on_action(cx.listener(|this, _: &menu::SelectPrevious, _, cx| {
                this.move_commit_selection(-1, cx)
            }))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                this.open_selected_commit(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &menu::SecondaryConfirm, window, cx| {
                this.open_selected_commit(true, window, cx)
            }))
            .gap_0p5()
            .pt(DynamicSpacing::Base08.px(cx));
        if detail.commits.is_empty() {
            list = list.child(
                Label::new("Nenhum commit")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }
        for (index, commit) in detail.commits.iter().enumerate() {
            let selected = open_pull_request.selected_commit == Some(index);
            list = list.child(self.render_commit_row(commit, opening, selected, now, cx));
        }

        let added: u64 = detail.files.iter().map(|file| file.lines_added).sum();
        let removed: u64 = detail.files.iter().map(|file| file.lines_removed).sum();
        v_flex()
            .gap(DynamicSpacing::Base08.px(cx))
            .child(list)
            .child(
                Label::new(if opening {
                    "Buscando o commit no remoto…"
                } else {
                    "↑↓ navega · Enter abre no Zed · ⌘Enter no Bitbucket"
                })
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            )
            .child(self.render_whole_diff_card(
                open_pull_request,
                format!("+{added} −{removed}"),
                is_checked_out,
                cx,
            ))
            .into_any_element()
    }

    fn render_commit_row(
        &self,
        commit: &api::Commit,
        opening: bool,
        selected: bool,
        now: DateTime<Utc>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let hash = commit.hash.clone();
        let mut meta = vec![commit.author.clone()];
        if let Some(date) = commit.date {
            meta.push(relative_age(date, now));
        }
        meta.retain(|part| !part.is_empty());
        h_flex()
            .id(SharedString::from(format!(
                "repo-pr-commit-{}",
                commit.hash
            )))
            .items_start()
            .gap(DynamicSpacing::Base10.px(cx))
            .px(DynamicSpacing::Base08.px(cx))
            .py(DynamicSpacing::Base06.px(cx))
            .rounded_md()
            .when(selected, |this| {
                this.bg(cx.theme().colors().ghost_element_selected)
            })
            .when(!opening, |this| {
                this.cursor_pointer()
                    .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            })
            .tooltip(Tooltip::text("Abrir o commit no Zed"))
            .on_click(
                cx.listener(move |this, _, window, cx| this.open_commit(hash.clone(), window, cx)),
            )
            .child(
                h_flex().h(DynamicSpacing::Base20.px(cx)).child(
                    Icon::new(IconName::GitCommit)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        Label::new(commit.summary.clone())
                            .size(LabelSize::Small)
                            .truncate(),
                    )
                    .child(
                        h_flex()
                            .gap(DynamicSpacing::Base06.px(cx))
                            .child(
                                Label::new(commit.short_hash().to_string())
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .color(Color::Accent),
                            )
                            .child(
                                Label::new("·")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Disabled),
                            )
                            .child(
                                Label::new(meta.join(" · "))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                    ),
            )
    }

    fn render_whole_diff_card(
        &self,
        open_pull_request: &OpenPullRequest,
        stats: String,
        is_checked_out: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let Target::Hosted {
            repository,
            remote_name,
        } = &self.target
        else {
            return div().into_any_element();
        };
        let is_fork = open_pull_request.summary.is_from_fork(repository);
        let opening = matches!(
            open_pull_request.pending_action,
            Some(PendingAction::OpeningDiff)
        );
        h_flex()
            .gap(DynamicSpacing::Base08.px(cx))
            .p(DynamicSpacing::Base08.px(cx))
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(cx.theme().colors().element_background)
            .child(
                Icon::new(IconName::DiffSplit)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(Label::new("PR inteiro no Zed").size(LabelSize::Small))
                    .child(
                        Label::new(format!(
                            "Mudanças desde {remote_name}/{} · {stats}",
                            open_pull_request.summary.destination_branch
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .truncate(),
                    ),
            )
            .child(
                Button::new("repo-pr-diff", if opening { "Abrindo…" } else { "Abrir" })
                    .style(ButtonStyle::Outlined)
                    .label_size(LabelSize::Small)
                    .disabled(is_fork || self.is_busy())
                    .tooltip(Tooltip::text(if is_fork {
                        "O branch está num fork; abra o diff no Bitbucket".to_string()
                    } else if is_checked_out {
                        "Abre o diff no Zed".to_string()
                    } else {
                        format!(
                            "Faz checkout de {} e abre o diff no Zed",
                            open_pull_request.summary.source_branch
                        )
                    }))
                    .on_click(cx.listener(|this, _, window, cx| this.open_branch_diff(window, cx))),
            )
            .into_any_element()
    }

    fn render_files(
        &self,
        open_pull_request: &OpenPullRequest,
        is_checked_out: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let Some(detail) = open_pull_request.detail.as_ref() else {
            return self.render_loading("Carregando os arquivos…", cx);
        };
        let added: u64 = detail.files.iter().map(|file| file.lines_added).sum();
        let removed: u64 = detail.files.iter().map(|file| file.lines_removed).sum();
        let mut list = v_flex()
            .gap(DynamicSpacing::Base04.px(cx))
            .pt(DynamicSpacing::Base08.px(cx))
            .child(self.render_whole_diff_card(
                open_pull_request,
                format!("+{added} −{removed}"),
                is_checked_out,
                cx,
            ));
        for (index, file) in detail.files.iter().enumerate() {
            list = list.child(render_file_row(index, file, cx));
        }
        list.into_any_element()
    }

    fn render_field(label: &'static str, value: AnyElement, cx: &App) -> impl IntoElement {
        h_flex()
            .min_h(DynamicSpacing::Base24.px(cx))
            .gap(DynamicSpacing::Base08.px(cx))
            .child(
                div()
                    .flex_none()
                    .w(px(96.))
                    .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
            )
            .child(div().flex_1().min_w_0().child(value))
    }

    fn render_section_header(
        title: &'static str,
        count: Option<usize>,
        trailing: Option<AnyElement>,
        cx: &App,
    ) -> impl IntoElement {
        h_flex()
            .justify_between()
            .pt(DynamicSpacing::Base12.px(cx))
            .pb(DynamicSpacing::Base04.px(cx))
            .child(
                h_flex()
                    .gap(DynamicSpacing::Base06.px(cx))
                    .child(
                        Label::new(title)
                            .size(LabelSize::XSmall)
                            .weight(FontWeight::SEMIBOLD)
                            .color(Color::Muted),
                    )
                    .when_some(count, |this, count| {
                        this.child(
                            Label::new(count.to_string())
                                .size(LabelSize::XSmall)
                                .color(Color::Disabled),
                        )
                    }),
            )
            .children(trailing)
    }

    fn render_approval_card(
        &self,
        repository: &RemoteRepository,
        account: &Account,
        open_pull_request: &OpenPullRequest,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let pull_request = &open_pull_request.summary;
        let reviewer_count = pull_request.reviewers().count();
        let approvals = pull_request.approval_count();
        let missing: Vec<&str> = pull_request
            .reviewers()
            .filter(|reviewer| !reviewer.approved)
            .map(|reviewer| reviewer.person.short_name())
            .collect();
        let requested_changes = pull_request
            .participants
            .iter()
            .any(|participant| participant.requested_changes);
        let caption = if requested_changes {
            "mudanças pedidas".to_string()
        } else if missing.is_empty() {
            "nenhuma aprovação pendente".to_string()
        } else {
            format!("falta {}", missing.join(", "))
        };
        let dot = if requested_changes {
            Color::Error
        } else if missing.is_empty() && reviewer_count > 0 {
            Color::Success
        } else {
            Color::Warning
        };

        let is_author = pull_request.author.id == account.user.id;
        let approved_by_me = pull_request.has_approval_from(&account.user.id);
        let is_checked_out = self.branch.as_deref() == Some(pull_request.source_branch.as_str());
        let is_fork = pull_request.is_from_fork(repository);
        let is_pending = open_pull_request.pending_action.is_some();
        let checking_out = matches!(
            open_pull_request.pending_action,
            Some(PendingAction::Checkout)
        );

        h_flex()
            .gap(DynamicSpacing::Base08.px(cx))
            .p(DynamicSpacing::Base10.px(cx))
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().colors().border_focused.opacity(0.6))
            .bg(cx.theme().colors().element_background)
            .child(
                div()
                    .flex_none()
                    .size(px(8.))
                    .rounded_full()
                    .bg(dot.color(cx)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        Label::new(approval_label(approvals, reviewer_count))
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .child(
                        Label::new(caption)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
            .when(
                !is_author && pull_request.state == PullRequestState::Open,
                |this| {
                    this.child(
                        Button::new(
                            "repo-pr-approve",
                            if approved_by_me {
                                "Aprovado"
                            } else {
                                "Aprovar"
                            },
                        )
                        .style(ButtonStyle::Outlined)
                        .label_size(LabelSize::Small)
                        .start_icon(Icon::new(IconName::Check).size(IconSize::XSmall).color(
                            if approved_by_me {
                                Color::Success
                            } else {
                                Color::Muted
                            },
                        ))
                        .disabled(is_pending)
                        .when(approved_by_me, |this| {
                            this.tooltip(Tooltip::text("Clique para remover sua aprovação"))
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_approval(!approved_by_me, cx)
                        })),
                    )
                },
            )
            .when(pull_request.state == PullRequestState::Open, |this| {
                this.child(
                    Button::new("repo-pr-merge", "Merge")
                        .style(ButtonStyle::Filled)
                        .label_size(LabelSize::Small)
                        .disabled(is_pending)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.start_merging(window, cx)),
                        ),
                )
            })
            .child(
                Button::new(
                    "repo-pr-checkout",
                    if is_checked_out {
                        "No branch"
                    } else if checking_out {
                        "Checkout…"
                    } else {
                        "Checkout"
                    },
                )
                .style(ButtonStyle::Outlined)
                .label_size(LabelSize::Small)
                .start_icon(Icon::new(IconName::GitBranch).size(IconSize::XSmall))
                .disabled(is_checked_out || is_fork || is_pending)
                .when(is_fork, |this| {
                    this.tooltip(Tooltip::text("O branch está num fork"))
                })
                .on_click(cx.listener(|this, _, window, cx| this.checkout(window, cx))),
            )
    }

    fn render_detail(
        &self,
        repository: &RemoteRepository,
        account: &Account,
        open_pull_request: &OpenPullRequest,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let pull_request = &open_pull_request.summary;
        let now = Utc::now();
        let url = pull_request.url.clone();
        let copied_url = pull_request.url.clone();
        let is_checked_out = self.branch.as_deref() == Some(pull_request.source_branch.as_str());

        let mut content = v_flex()
            .id("repo-pr-detail")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(DynamicSpacing::Base12.px(cx))
            .pb(DynamicSpacing::Base12.px(cx))
            .gap(DynamicSpacing::Base04.px(cx))
            .child(
                Label::new(format!(
                    "{}  →  {}",
                    pull_request.source_branch, pull_request.destination_branch
                ))
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .buffer_font(cx)
                .truncate(),
            )
            .child(
                Label::new(pull_request.title.clone())
                    .size(LabelSize::Large)
                    .weight(FontWeight::SEMIBOLD),
            )
            .child(div().h(DynamicSpacing::Base08.px(cx)))
            .child(self.render_detail_tabs(open_pull_request, cx));

        if open_pull_request.tab == DetailTab::Commits {
            content = content.child(self.render_commits(open_pull_request, is_checked_out, cx));
        } else if open_pull_request.tab == DetailTab::Files {
            content = content.child(self.render_files(open_pull_request, is_checked_out, cx));
        } else {
            content =
                self.render_overview(content, repository, account, open_pull_request, now, cx);
        }

        v_flex()
            .on_action(
                cx.listener(|this, _: &menu::Cancel, window, cx| {
                    this.close_pull_request(window, cx)
                }),
            )
            .size_full()
            .child(self.render_detail_header(pull_request, url, copied_url, cx))
            .when_some(open_pull_request.error.clone(), |this, error| {
                this.child(render_inline_error(error.to_string(), cx))
            })
            .child(content)
            .when_some(open_pull_request.notice.clone(), |this, notice| {
                this.child(
                    h_flex()
                        .gap(DynamicSpacing::Base06.px(cx))
                        .px(DynamicSpacing::Base12.px(cx))
                        .py(DynamicSpacing::Base06.px(cx))
                        .border_t_1()
                        .border_color(cx.theme().colors().border_variant)
                        .child(
                            Icon::new(IconName::Check)
                                .size(IconSize::XSmall)
                                .color(Color::Success),
                        )
                        .child(
                            Label::new(notice)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
            })
            .map(|this| match &open_pull_request.merging {
                Some(form) => this.child(self.render_merge_actions(open_pull_request, form, cx)),
                None if open_pull_request.tab == DetailTab::Overview => {
                    this.child(self.render_composer(open_pull_request, cx))
                }
                None => this,
            })
    }

    fn render_overview(
        &self,
        content: Stateful<Div>,
        repository: &RemoteRepository,
        account: &Account,
        open_pull_request: &OpenPullRequest,
        now: DateTime<Utc>,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let pull_request = &open_pull_request.summary;
        let detail = open_pull_request.detail.as_ref();
        let reviewers: Vec<&api::Participant> = pull_request.reviewers().collect();
        let reviewers = if reviewers.is_empty() {
            Label::new(if detail.is_some() { "Ninguém" } else { "…" })
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element()
        } else {
            h_flex()
                .gap(DynamicSpacing::Base04.px(cx))
                .children(reviewers.iter().take(3).map(|reviewer| {
                    avatar(
                        &reviewer.person.display_name,
                        if reviewer.approved {
                            Color::Success
                        } else {
                            Color::Info
                        },
                        cx,
                    )
                }))
                .child(
                    Label::new(
                        reviewers
                            .iter()
                            .map(|reviewer| {
                                let name = reviewer.person.short_name();
                                if reviewer.approved {
                                    format!("{name} ✓")
                                } else {
                                    name.to_string()
                                }
                            })
                            .collect::<Vec<_>>()
                            .join(", "),
                    )
                    .size(LabelSize::Small)
                    .truncate(),
                )
                .into_any_element()
        };

        let checks = match detail {
            None => Label::new("…")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
            Some(detail) if detail.checks.is_empty() => Label::new("Nenhum")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
            Some(detail) => {
                let summary = CheckSummary::of(&detail.checks);
                let text = if summary.failed > 0 {
                    format!(
                        "{}/{} · {} falhou",
                        summary.passed, summary.total, summary.failed
                    )
                } else if summary.running > 0 {
                    format!(
                        "{}/{} · {} rodando",
                        summary.passed, summary.total, summary.running
                    )
                } else {
                    format!("{}/{} passaram", summary.passed, summary.total)
                };
                Label::new(text)
                    .size(LabelSize::Small)
                    .color(check_summary_color(summary))
                    .into_any_element()
            }
        };

        let task = match clickup_task_id(&pull_request.source_branch) {
            Some(task_id) => {
                let task_url = format!(
                    "https://app.clickup.com/t/{}",
                    task_id.trim_start_matches("CU-")
                );
                Button::new("repo-pr-task", task_id)
                    .style(ButtonStyle::Transparent)
                    .label_size(LabelSize::Small)
                    .color(Color::Success)
                    .end_icon(Icon::new(IconName::ArrowUpRight).size(IconSize::XSmall))
                    .tooltip(Tooltip::text("Abrir a tarefa no ClickUp"))
                    .on_click(move |_, _, cx| cx.open_url(&task_url))
                    .into_any_element()
            }
            None => Label::new("Nenhuma no nome do branch")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
        };

        let changes = match detail {
            None => Label::new("…")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
            Some(detail) => {
                let added: u64 = detail.files.iter().map(|file| file.lines_added).sum();
                let removed: u64 = detail.files.iter().map(|file| file.lines_removed).sum();
                h_flex()
                    .gap(DynamicSpacing::Base06.px(cx))
                    .child(
                        Label::new(format!("+{added}"))
                            .size(LabelSize::Small)
                            .color(Color::Success),
                    )
                    .child(
                        Label::new(format!("−{removed}"))
                            .size(LabelSize::Small)
                            .color(Color::Error),
                    )
                    .child(
                        Label::new(file_count_label(detail.files.len()))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .into_any_element()
            }
        };

        let mut content = content
            .child(div().h(DynamicSpacing::Base04.px(cx)))
            .child(Self::render_field(
                "Estado",
                h_flex()
                    .child(state_pill(pull_request, cx))
                    .into_any_element(),
                cx,
            ))
            .child(Self::render_field("Revisores", reviewers, cx))
            .child(Self::render_field("Checks", checks, cx))
            .child(Self::render_field("Tarefa", task, cx))
            .child(Self::render_field("Mudanças", changes, cx))
            .child(div().h(DynamicSpacing::Base08.px(cx)))
            .child(match &open_pull_request.merging {
                Some(form) => self
                    .render_merge_card(open_pull_request, form, cx)
                    .into_any_element(),
                None => self
                    .render_approval_card(repository, account, open_pull_request, cx)
                    .into_any_element(),
            });

        let description = pull_request.description.trim().to_string();
        content = content.child(Self::render_section_header("DESCRIÇÃO", None, None, cx));
        content = if description.is_empty() {
            content.child(
                Label::new("Sem descrição")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
        } else {
            let is_long = description.chars().count() > DESCRIPTION_PREVIEW_CHARS;
            let shown = if is_long && !open_pull_request.description_expanded {
                let preview: String = description
                    .chars()
                    .take(DESCRIPTION_PREVIEW_CHARS)
                    .collect();
                format!("{}…", preview.trim_end())
            } else {
                description
            };
            content
                .child(Label::new(shown).size(LabelSize::Small).color(Color::Muted))
                .when(is_long, |this| {
                    let expanded = open_pull_request.description_expanded;
                    this.child(
                        h_flex().child(
                            Button::new(
                                "repo-pr-description-toggle",
                                if expanded {
                                    "Mostrar menos"
                                } else {
                                    "Mostrar mais"
                                },
                            )
                            .style(ButtonStyle::Transparent)
                            .label_size(LabelSize::Small)
                            .color(Color::Accent)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(open_pull_request) = &mut this.open_pull_request {
                                    open_pull_request.description_expanded =
                                        !open_pull_request.description_expanded;
                                    cx.notify();
                                }
                            })),
                        ),
                    )
                })
        };

        let comments = detail
            .map(|detail| detail.comments.as_slice())
            .unwrap_or_default();
        content = content.child(Self::render_section_header(
            "COMENTÁRIOS",
            Some(comments.len()),
            None,
            cx,
        ));
        if comments.is_empty() {
            content = content.child(
                Label::new(if detail.is_some() {
                    "Nenhum comentário ainda"
                } else {
                    "…"
                })
                .size(LabelSize::Small)
                .color(Color::Muted),
            );
        }
        for comment in comments {
            content = content.child(render_comment(comment, now, cx));
        }
        if let Some(detail) = detail {
            for (index, event) in activity_events(&detail.pull_request, &detail.checks, now)
                .into_iter()
                .enumerate()
            {
                content = content.child(
                    h_flex()
                        .id(SharedString::from(format!("repo-pr-event-{index}")))
                        .gap(DynamicSpacing::Base08.px(cx))
                        .py(DynamicSpacing::Base02.px(cx))
                        .pl(DynamicSpacing::Base06.px(cx))
                        .when_some(event.url, |this, url| {
                            this.cursor_pointer()
                                .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
                                .tooltip(Tooltip::text("Abrir no navegador"))
                                .on_click(move |_, _, cx| cx.open_url(&url))
                        })
                        .child(div().size(px(4.)).rounded_full().bg(event.color.color(cx)))
                        .child(
                            Label::new(event.text)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                );
            }
        }
        content
    }

    fn render_detail_header(
        &self,
        pull_request: &PullRequest,
        url: String,
        copied_url: String,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .justify_between()
            .px(DynamicSpacing::Base08.px(cx))
            .py(DynamicSpacing::Base06.px(cx))
            .child(
                Button::new("repo-pr-back", "Pull requests")
                    .style(ButtonStyle::Transparent)
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(IconName::ChevronLeft).size(IconSize::XSmall))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.close_pull_request(window, cx)),
                    ),
            )
            .child(
                h_flex()
                    .gap(DynamicSpacing::Base02.px(cx))
                    .child(
                        Label::new(format!("#{}", pull_request.number))
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Muted)
                            .mr(DynamicSpacing::Base04.px(cx)),
                    )
                    .child(
                        IconButton::new("repo-pr-review-agent", IconName::ZedAgent)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Revisar com o Agent"))
                            .disabled(self.is_busy())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.review_with_agent(window, cx)
                            })),
                    )
                    .child(
                        IconButton::new("repo-pr-copy-link", IconName::Copy)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Copiar link"))
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copied_url.clone()))
                            }),
                    )
                    .child(
                        IconButton::new("repo-pr-open", IconName::ArrowUpRight)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Abrir no Bitbucket"))
                            .on_click(move |_, _, cx| cx.open_url(&url)),
                    ),
            )
    }

    fn render_composer(
        &self,
        open_pull_request: &OpenPullRequest,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .on_action(
                cx.listener(|this, _: &menu::Confirm, window, cx| this.post_comment(window, cx)),
            )
            .gap(DynamicSpacing::Base06.px(cx))
            .p(DynamicSpacing::Base08.px(cx))
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .child(open_pull_request.comment_input.clone())
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        Label::new("Enter envia · vai como comentário geral")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Button::new("repo-pr-comment", "Comentar")
                            .style(ButtonStyle::Filled)
                            .label_size(LabelSize::Small)
                            .start_icon(Icon::new(IconName::Send).size(IconSize::XSmall))
                            .disabled(open_pull_request.posting_comment)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.post_comment(window, cx)),
                            ),
                    ),
            )
    }
}

impl RepoPanel {
    fn local_branch_status(&self, cx: &App) -> Option<LocalBranchStatus> {
        let Target::Hosted { remote_name, .. } = &self.target else {
            return None;
        };
        let repository = self.project.read(cx).active_repository(cx)?;
        let repository = repository.read(cx);
        let branch = repository.branch.as_ref()?;
        let mut status = LocalBranchStatus {
            local_name: branch.name().to_string(),
            needs_upstream: true,
            ahead: 0,
            behind: 0,
            tracks_other_remote: false,
            has_uncommitted_changes: !repository.statuses_by_path.is_empty(),
            head_subject: repository
                .head_commit
                .as_ref()
                .and_then(|commit| commit.message.lines().next().map(str::to_string)),
        };
        if let Some(upstream) = &branch.upstream {
            if upstream.remote_name() != Some(remote_name.as_str()) {
                status.tracks_other_remote = true;
            } else if let Some(tracking) = upstream.tracking.status() {
                status.needs_upstream = false;
                status.ahead = tracking.ahead;
                status.behind = tracking.behind;
            }
        }
        Some(status)
    }

    fn start_creating(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Target::Hosted { repository, .. } = self.target.clone() else {
            return;
        };
        let Some(source_branch) = self.branch.clone() else {
            return;
        };
        let Some(credentials) = self.store.read(cx).bitbucket_credentials() else {
            return;
        };
        let my_id = self.account(cx).map(|account| account.user.id);
        let http_client = self.store.read(cx).http_client();

        let title_input = cx.new(|cx| InputField::new(window, cx, "Título do pull request"));
        let description_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(4, 14, window, cx);
            editor.set_placeholder_text("Descrição em Markdown", window, cx);
            editor
        });
        window.focus(&title_input.focus_handle(cx), cx);

        // People already reviewing or authoring here are who you'd most likely ask; listing
        // the workspace's members needs a scope the token doesn't have.
        let mut candidates: Vec<api::Person> = Vec::new();
        for pull_request in &self.pull_requests.list {
            let people = std::iter::once(&pull_request.author).chain(
                pull_request
                    .participants
                    .iter()
                    .map(|participant| &participant.person),
            );
            for person in people {
                add_person(&mut candidates, person, my_id.as_deref());
            }
        }

        let load = cx.background_spawn(async move {
            let destination =
                bitbucket::get_default_destination(&http_client, &credentials, &repository);
            let branches = bitbucket::list_branches(&http_client, &credentials, &repository);
            let reviewers =
                bitbucket::get_default_reviewers(&http_client, &credentials, &repository);
            futures::join!(destination, branches, reviewers)
        });
        let load_task = cx.spawn_in(window, async move |this, cx| {
            let (destination, branches, reviewers) = load.await;
            this.update_in(cx, |this, window, cx| {
                let Some(form) = &mut this.creating else {
                    return;
                };
                let branches = branches
                    .context("Repo: falha ao listar os branches")
                    .log_err()
                    .unwrap_or_default();
                form.destination = destination
                    .context("Repo: falha ao achar o branch de destino padrão")
                    .log_err()
                    .flatten()
                    .or_else(|| {
                        ["develop", "main", "master"]
                            .into_iter()
                            .find(|name| branches.iter().any(|branch| branch == name))
                            .map(str::to_string)
                    });
                form.branches = branches;
                // Default reviewers need a repository admin to have set them; without them
                // (or without permission to read them) the form just starts empty.
                for reviewer in reviewers
                    .context("Repo: revisores padrão indisponíveis")
                    .log_err()
                    .unwrap_or_default()
                {
                    add_person(&mut form.candidates, &reviewer, my_id.as_deref());
                    if Some(reviewer.id.as_str()) != my_id.as_deref() {
                        add_person(&mut form.reviewers, &reviewer, None);
                    }
                }
                this.reload_create_commits(window, cx);
                cx.notify();
            })
            .log_err();
        });

        self.open_pull_request = None;
        self.creating = Some(CreateForm {
            source_branch,
            destination: None,
            branches: Vec::new(),
            title_input,
            description_editor,
            reviewers: Vec::new(),
            candidates,
            close_source_branch: true,
            draft: false,
            commits: Vec::new(),
            commits_loaded: false,
            autofilled: false,
            error: None,
            submitting: false,
            _load_task: load_task,
            commits_task: None,
        });
        cx.notify();
    }

    fn close_create_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.creating = None;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn set_destination(
        &mut self,
        destination: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = &mut self.creating else {
            return;
        };
        form.destination = Some(destination);
        form.error = None;
        self.reload_create_commits(window, cx);
        cx.notify();
    }

    /// The commits the remote already has, then the title and description made from them.
    fn reload_create_commits(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Target::Hosted { repository, .. } = self.target.clone() else {
            return;
        };
        let Some(credentials) = self.store.read(cx).bitbucket_credentials() else {
            return;
        };
        let needs_upstream = self
            .local_branch_status(cx)
            .is_none_or(|status| status.needs_upstream);
        let http_client = self.store.read(cx).http_client();
        let Some(form) = &mut self.creating else {
            return;
        };
        let Some(destination) = form.destination.clone() else {
            form.commits_loaded = true;
            self.autofill(window, cx);
            return;
        };
        if needs_upstream {
            form.commits.clear();
            form.commits_loaded = true;
            self.autofill(window, cx);
            return;
        }
        let source = form.source_branch.clone();
        form.commits_loaded = false;
        let load = cx.background_spawn(async move {
            bitbucket::list_commits_between(
                &http_client,
                &credentials,
                &repository,
                &source,
                &destination,
            )
            .await
        });
        form.commits_task = Some(cx.spawn_in(window, async move |this, cx| {
            let commits = load.await;
            this.update_in(cx, |this, window, cx| {
                let Some(form) = &mut this.creating else {
                    return;
                };
                form.commits = commits
                    .context("Repo: falha ao listar os commits do branch")
                    .log_err()
                    .unwrap_or_default();
                form.commits_loaded = true;
                this.autofill(window, cx);
                cx.notify();
            })
            .log_err();
        }));
    }

    fn autofill(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let status = self.local_branch_status(cx);
        let Some(form) = &mut self.creating else {
            return;
        };
        if form.autofilled {
            return;
        }
        form.autofilled = true;
        let head_subject = status
            .as_ref()
            .and_then(|status| status.head_subject.clone());
        let unpushed = status.as_ref().map_or(0, |status| {
            if status.needs_upstream {
                1
            } else {
                status.ahead
            }
        });

        let mut subjects: Vec<String> = form
            .commits
            .iter()
            .rev()
            .map(|commit| commit.summary.clone())
            .collect();
        if unpushed > 0
            && let Some(head_subject) = &head_subject
            && !subjects.contains(head_subject)
        {
            subjects.push(head_subject.clone());
        }

        if form.title_input.read(cx).is_empty(cx) {
            let title = match subjects.as_slice() {
                [only] => only.clone(),
                _ => humanize_branch(&form.source_branch)
                    .or(head_subject)
                    .unwrap_or_default(),
            };
            form.title_input
                .update(cx, |input, cx| input.set_text(&title, window, cx));
        }
        if form.description_editor.read(cx).text(cx).trim().is_empty() {
            let description = pull_request_description(&subjects, &form.source_branch);
            form.description_editor
                .update(cx, |editor, cx| editor.set_text(description, window, cx));
        }
    }

    fn submit_create_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Target::Hosted {
            repository: remote_repository,
            remote_name,
        } = self.target.clone()
        else {
            return;
        };
        let Some(credentials) = self.store.read(cx).bitbucket_credentials() else {
            return;
        };
        let http_client = self.store.read(cx).http_client();
        let status = self.local_branch_status(cx);
        let repository = self.project.read(cx).active_repository(cx);
        let is_via_collab = self.project.read(cx).is_via_collab();
        let Some(form) = &mut self.creating else {
            return;
        };
        if form.submitting {
            return;
        }

        let title = form.title_input.read(cx).text(cx).trim().to_string();
        let description = form.description_editor.read(cx).text(cx).trim().to_string();
        let problem = match (&form.destination, &status) {
            _ if title.is_empty() => Some("Dê um título ao pull request.".to_string()),
            (None, _) => Some("Escolha o branch de destino.".to_string()),
            (Some(destination), _) if *destination == form.source_branch => {
                Some("O destino é o próprio branch.".to_string())
            }
            (_, None) => Some("Nenhum branch em checkout.".to_string()),
            (_, Some(status)) if status.tracks_other_remote => Some(
                "O branch acompanha outro remoto (fork); crie esse PR pelo Bitbucket.".to_string(),
            ),
            (_, Some(status)) if status.behind > 0 => Some(format!(
                "O origin tem {} commit(s) que você não tem. Faça pull antes de criar.",
                status.behind
            )),
            (_, Some(status)) if is_via_collab && (status.needs_upstream || status.ahead > 0) => {
                Some(
                    "Numa sessão colaborativa só o dono do projeto pode enviar o branch."
                        .to_string(),
                )
            }
            _ => None,
        };
        if let Some(problem) = problem {
            form.error = Some(problem.into());
            cx.notify();
            return;
        }
        let (Some(destination), Some(status), Some(repository)) =
            (form.destination.clone(), status, repository)
        else {
            return;
        };

        let new_pull_request = bitbucket::NewPullRequest {
            title,
            description,
            source_branch: form.source_branch.clone(),
            destination_branch: destination,
            reviewer_ids: form
                .reviewers
                .iter()
                .map(|person| person.id.clone())
                .collect(),
            close_source_branch: form.close_source_branch,
            draft: form.draft,
        };
        form.submitting = true;
        form.error = None;
        cx.notify();

        let needs_push = status.needs_upstream || status.ahead > 0;
        let askpass = self.askpass_delegate("git push", window, cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = async {
                if needs_push {
                    let push = repository.update(cx, |repository, cx| {
                        repository.push(
                            status.local_name.clone().into(),
                            new_pull_request.source_branch.clone().into(),
                            remote_name.clone().into(),
                            status.needs_upstream.then_some(PushOptions::SetUpstream),
                            askpass,
                            cx,
                        )
                    });
                    push.await?
                        .with_context(|| format!("falha no git push para {remote_name}"))?;
                }
                cx.background_spawn(async move {
                    bitbucket::create_pull_request(
                        &http_client,
                        &credentials,
                        &remote_repository,
                        new_pull_request,
                    )
                    .await
                })
                .await
            }
            .await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(created) => {
                    this.creating = None;
                    this.sync(cx);
                    this.open_pull_request(created, false, window, cx);
                }
                Err(error) => {
                    if let Some(form) = &mut this.creating {
                        form.submitting = false;
                        form.error = Some(format!("Não deu para criar: {error:#}").into());
                        cx.notify();
                    }
                }
            })
        })
        .detach_and_log_err(cx);
    }

    fn render_create_form(
        &self,
        repository: &RemoteRepository,
        form: &CreateForm,
        cx: &Context<Self>,
    ) -> AnyElement {
        let status = self.local_branch_status(cx);
        let Target::Hosted { remote_name, .. } = &self.target else {
            return div().into_any_element();
        };
        let needs_push = status
            .as_ref()
            .is_some_and(|status| status.needs_upstream || status.ahead > 0);

        let (dot, sync_text, sync_note) = match &status {
            Some(status) if status.tracks_other_remote => (
                Color::Error,
                "O branch acompanha outro remoto".to_string(),
                "fork".to_string(),
            ),
            Some(status) if status.behind > 0 => (
                Color::Error,
                format!(
                    "{remote_name} tem {} commit(s) que você não tem",
                    status.behind
                ),
                "faça pull antes".to_string(),
            ),
            Some(status) if status.needs_upstream => (
                Color::Warning,
                format!("O branch ainda não está no {remote_name}"),
                "publicado ao criar".to_string(),
            ),
            Some(status) if status.ahead > 0 => (
                Color::Warning,
                format!(
                    "{} commit(s) ainda não estão no {remote_name}",
                    status.ahead
                ),
                "enviados ao criar".to_string(),
            ),
            Some(_) => (
                Color::Success,
                format!("Em dia com o {remote_name}"),
                String::new(),
            ),
            None => (
                Color::Error,
                "Nenhum branch em checkout".to_string(),
                String::new(),
            ),
        };

        let destination_label: SharedString = form
            .destination
            .clone()
            .unwrap_or_else(|| "destino…".to_string())
            .into();
        let panel = cx.weak_entity();
        let mut branches: Vec<String> = form
            .branches
            .iter()
            .filter(|branch| **branch != form.source_branch)
            .cloned()
            .collect();
        if let Some(destination) = &form.destination
            && let Some(index) = branches.iter().position(|branch| branch == destination)
        {
            let destination = branches.remove(index);
            branches.insert(0, destination);
        }
        let selected_destination = form.destination.clone();
        let destination_picker = PopoverMenu::new("repo-create-destination")
            .trigger(
                Button::new("repo-create-destination-button", destination_label)
                    .style(ButtonStyle::Outlined)
                    .label_size(LabelSize::Small)
                    .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall)),
            )
            .menu(move |window, cx| {
                let branches = branches.clone();
                let panel = panel.clone();
                let selected = selected_destination.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for branch in branches.iter().take(60) {
                        let panel = panel.clone();
                        let name = branch.clone();
                        menu = menu.toggleable_entry(
                            branch.clone(),
                            selected.as_deref() == Some(branch.as_str()),
                            IconPosition::End,
                            None,
                            move |window, cx| {
                                panel
                                    .update(cx, |panel, cx| {
                                        panel.set_destination(name.clone(), window, cx)
                                    })
                                    .log_err();
                            },
                        );
                    }
                    menu
                }))
            });

        let route = v_flex()
            .gap(DynamicSpacing::Base08.px(cx))
            .p(DynamicSpacing::Base10.px(cx))
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(cx.theme().colors().element_background)
            .child(
                h_flex()
                    .gap(DynamicSpacing::Base06.px(cx))
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap(DynamicSpacing::Base04.px(cx))
                            .px(DynamicSpacing::Base06.px(cx))
                            .py_0p5()
                            .rounded_md()
                            .bg(cx.theme().colors().editor_background)
                            .child(
                                Icon::new(IconName::GitBranch)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(form.source_branch.clone())
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .truncate(),
                            ),
                    )
                    .child(
                        Icon::new(IconName::ArrowRight)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(destination_picker),
            )
            .child(
                h_flex()
                    .gap(DynamicSpacing::Base06.px(cx))
                    .child(div().size(px(7.)).rounded_full().bg(dot.color(cx)))
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(sync_text)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                    )
                    .child(
                        Label::new(sync_note)
                            .size(LabelSize::XSmall)
                            .color(Color::Disabled),
                    ),
            )
            .when(
                status
                    .as_ref()
                    .is_some_and(|status| status.has_uncommitted_changes),
                |this| {
                    this.child(
                        h_flex()
                            .gap(DynamicSpacing::Base06.px(cx))
                            .child(
                                Icon::new(IconName::Warning)
                                    .size(IconSize::XSmall)
                                    .color(Color::Warning),
                            )
                            .child(
                                Label::new("Mudanças não commitadas ficam de fora do PR")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                },
            );

        let field_label = |label: &'static str, hint: Option<&'static str>| {
            h_flex()
                .justify_between()
                .child(
                    Label::new(label)
                        .size(LabelSize::Small)
                        .weight(FontWeight::SEMIBOLD)
                        .color(Color::Muted),
                )
                .when_some(hint, |this, hint| {
                    this.child(
                        Label::new(hint)
                            .size(LabelSize::XSmall)
                            .color(Color::Disabled),
                    )
                })
        };

        let reviewers = self.render_reviewer_picker(form, cx);
        let commits = self.render_create_commits(form, status.as_ref(), cx);

        let content = v_flex()
            .id("repo-create-form")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(DynamicSpacing::Base12.px(cx))
            .pb(DynamicSpacing::Base12.px(cx))
            .gap(DynamicSpacing::Base12.px(cx))
            .child(
                v_flex()
                    .child(
                        Label::new(format!("Em {}", repository.full_name()))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new("Novo pull request")
                            .size(LabelSize::Large)
                            .weight(FontWeight::SEMIBOLD),
                    ),
            )
            .child(route)
            .child(
                v_flex()
                    .gap(DynamicSpacing::Base06.px(cx))
                    .child(field_label("Título", None))
                    .child(form.title_input.clone()),
            )
            .child(
                v_flex()
                    .gap(DynamicSpacing::Base06.px(cx))
                    .child(field_label(
                        "Descrição",
                        Some("Markdown · gerada dos commits"),
                    ))
                    .child(
                        div()
                            .p(DynamicSpacing::Base08.px(cx))
                            .rounded_md()
                            .border_1()
                            .border_color(cx.theme().colors().border)
                            .bg(cx.theme().colors().editor_background)
                            .child(form.description_editor.clone()),
                    ),
            )
            .child(
                v_flex()
                    .gap(DynamicSpacing::Base06.px(cx))
                    .child(field_label("Revisores", None))
                    .child(reviewers),
            )
            .child(
                v_flex()
                    .gap(DynamicSpacing::Base06.px(cx))
                    .child(
                        Checkbox::new("repo-create-close-source", form.close_source_branch.into())
                            .label("Fechar o branch depois do merge")
                            .on_click({
                                let panel = cx.weak_entity();
                                move |state, _, cx| {
                                    let checked = state.selected();
                                    panel
                                        .update(cx, |panel, cx| {
                                            if let Some(form) = &mut panel.creating {
                                                form.close_source_branch = checked;
                                                cx.notify();
                                            }
                                        })
                                        .log_err();
                                }
                            }),
                    )
                    .child(
                        Checkbox::new("repo-create-draft", form.draft.into())
                            .label("Criar como rascunho")
                            .on_click({
                                let panel = cx.weak_entity();
                                move |state, _, cx| {
                                    let checked = state.selected();
                                    panel
                                        .update(cx, |panel, cx| {
                                            if let Some(form) = &mut panel.creating {
                                                form.draft = checked;
                                                cx.notify();
                                            }
                                        })
                                        .log_err();
                                }
                            }),
                    ),
            )
            .child(commits);

        let submit_label = if form.submitting {
            if needs_push {
                "Enviando…"
            } else {
                "Criando…"
            }
        } else if needs_push {
            "Publicar e criar PR"
        } else {
            "Criar pull request"
        };

        v_flex()
            .key_context("RepoCreatePullRequest")
            .on_action(
                cx.listener(|this, _: &menu::Cancel, window, cx| {
                    this.close_create_form(window, cx)
                }),
            )
            .size_full()
            .child(
                h_flex()
                    .justify_between()
                    .px(DynamicSpacing::Base08.px(cx))
                    .py(DynamicSpacing::Base06.px(cx))
                    .child(
                        Button::new("repo-create-back", "Pull requests")
                            .style(ButtonStyle::Transparent)
                            .label_size(LabelSize::Small)
                            .start_icon(Icon::new(IconName::ChevronLeft).size(IconSize::XSmall))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_create_form(window, cx)
                            })),
                    )
                    .child(
                        Label::new("novo")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(content)
            .child(
                v_flex()
                    .gap(DynamicSpacing::Base06.px(cx))
                    .p(DynamicSpacing::Base08.px(cx))
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .when_some(form.error.clone(), |this, error| {
                        this.child(
                            Label::new(error)
                                .size(LabelSize::XSmall)
                                .color(Color::Error),
                        )
                    })
                    .when(needs_push && status.is_some(), |this| {
                        let status = status.as_ref();
                        let local = status
                            .map(|status| status.local_name.clone())
                            .unwrap_or_default();
                        let flag = if status.is_some_and(|status| status.needs_upstream) {
                            "-u "
                        } else {
                            ""
                        };
                        this.child(
                            Label::new(format!(
                                "git push {flag}{remote_name} {local} antes de criar"
                            ))
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Muted)
                            .truncate(),
                        )
                    })
                    .child(
                        h_flex()
                            .gap(DynamicSpacing::Base08.px(cx))
                            .child(
                                Button::new("repo-create-cancel", "Cancelar")
                                    .style(ButtonStyle::Outlined)
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close_create_form(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("repo-create-submit", submit_label)
                                    .full_width()
                                    .style(ButtonStyle::Filled)
                                    .label_size(LabelSize::Small)
                                    .start_icon(
                                        Icon::new(IconName::PullRequest).size(IconSize::XSmall),
                                    )
                                    .disabled(form.submitting)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.submit_create_form(window, cx)
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_reviewer_picker(&self, form: &CreateForm, cx: &Context<Self>) -> impl IntoElement {
        let panel = cx.weak_entity();
        let candidates = form.candidates.clone();
        let selected: Vec<String> = form
            .reviewers
            .iter()
            .map(|person| person.id.clone())
            .collect();
        let add = PopoverMenu::new("repo-create-reviewers")
            .trigger(
                Button::new("repo-create-add-reviewer", "Adicionar")
                    .style(ButtonStyle::Outlined)
                    .label_size(LabelSize::Small)
                    .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
                    .disabled(candidates.is_empty()),
            )
            .menu(move |window, cx| {
                let candidates = candidates.clone();
                let selected = selected.clone();
                let panel = panel.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for person in candidates.iter() {
                        let panel = panel.clone();
                        let toggled = person.clone();
                        menu = menu.toggleable_entry(
                            person.short_name().to_string(),
                            selected.contains(&person.id),
                            IconPosition::End,
                            None,
                            move |_, cx| {
                                panel
                                    .update(cx, |panel, cx| {
                                        if let Some(form) = &mut panel.creating {
                                            toggle_person(&mut form.reviewers, &toggled);
                                            cx.notify();
                                        }
                                    })
                                    .log_err();
                            },
                        );
                    }
                    menu
                }))
            });

        h_flex()
            .flex_wrap()
            .gap(DynamicSpacing::Base06.px(cx))
            .children(form.reviewers.iter().map(|person| {
                let removed = person.clone();
                h_flex()
                    .id(SharedString::from(format!(
                        "repo-create-reviewer-{}",
                        person.id
                    )))
                    .gap(DynamicSpacing::Base04.px(cx))
                    .pl(DynamicSpacing::Base02.px(cx))
                    .pr(DynamicSpacing::Base04.px(cx))
                    .py_0p5()
                    .rounded_full()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .child(avatar(&person.display_name, Color::Info, cx))
                    .child(Label::new(person.short_name().to_string()).size(LabelSize::Small))
                    .child(
                        IconButton::new(
                            SharedString::from(format!(
                                "repo-create-reviewer-remove-{}",
                                person.id
                            )),
                            IconName::Close,
                        )
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Muted)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(form) = &mut this.creating {
                                toggle_person(&mut form.reviewers, &removed);
                                cx.notify();
                            }
                        })),
                    )
            }))
            .child(add)
    }

    fn render_create_commits(
        &self,
        form: &CreateForm,
        status: Option<&LocalBranchStatus>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let now = Utc::now();
        let unpushed = status.map_or(0, |status| {
            if status.needs_upstream {
                0
            } else {
                status.ahead
            }
        });
        let mut list = v_flex().gap_0p5().child(Self::render_section_header(
            "COMMITS",
            Some(form.commits.len() + unpushed as usize),
            Some(
                Label::new("clique abre no Zed")
                    .size(LabelSize::XSmall)
                    .color(Color::Disabled)
                    .into_any_element(),
            ),
            cx,
        ));
        if status.is_some_and(|status| status.needs_upstream) {
            list = list.child(
                Label::new(
                    "O branch ainda não está no remoto; os commits aparecem depois de publicar.",
                )
                .size(LabelSize::Small)
                .color(Color::Muted),
            );
        } else if !form.commits_loaded {
            list = list.child(
                Label::new("Carregando os commits…")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            );
        }
        if unpushed > 0 {
            list = list.child(
                h_flex()
                    .gap(DynamicSpacing::Base06.px(cx))
                    .px(DynamicSpacing::Base08.px(cx))
                    .py(DynamicSpacing::Base04.px(cx))
                    .child(
                        Icon::new(IconName::ArrowUp)
                            .size(IconSize::XSmall)
                            .color(Color::Warning),
                    )
                    .child(
                        Label::new(format!(
                            "{unpushed} commit(s) local(is) ainda não enviado(s)"
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Warning),
                    ),
            );
        }
        for commit in &form.commits {
            list = list.child(self.render_commit_row(commit, false, false, now, cx));
        }
        list
    }
}

fn add_person(people: &mut Vec<api::Person>, person: &api::Person, except_id: Option<&str>) {
    if person.id.is_empty() || Some(person.id.as_str()) == except_id {
        return;
    }
    if !people.iter().any(|existing| existing.id == person.id) {
        people.push(person.clone());
    }
}

fn toggle_person(people: &mut Vec<api::Person>, person: &api::Person) {
    if let Some(index) = people.iter().position(|existing| existing.id == person.id) {
        people.remove(index);
    } else {
        people.push(person.clone());
    }
}

/// "feat/CU-86a1b2-login-com-biometria" → "Login com biometria".
fn humanize_branch(branch: &str) -> Option<String> {
    let last = branch.rsplit('/').next().unwrap_or(branch);
    let words: Vec<&str> = last
        .split(['-', '_'])
        .filter(|word| !word.is_empty())
        .collect();
    let words: Vec<&str> = match words.as_slice() {
        [prefix, id, rest @ ..] if prefix.eq_ignore_ascii_case("cu") && !id.is_empty() => {
            rest.to_vec()
        }
        _ => words,
    };
    let sentence = words.join(" ");
    let mut characters = sentence.chars();
    let first = characters.next()?;
    Some(first.to_uppercase().chain(characters).collect())
}

/// The commit subjects, oldest first, as a list, and the ClickUp task the branch is for.
fn pull_request_description(subjects: &[String], branch: &str) -> String {
    let mut description = if subjects.len() > 1 {
        subjects
            .iter()
            .map(|subject| format!("- {subject}"))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        String::new()
    };
    if let Some(task_id) = clickup_task_id(branch) {
        if !description.is_empty() {
            description.push_str("\n\n");
        }
        description.push_str(&format!("Closes {task_id}."));
    }
    description
}

impl RepoPanel {
    fn start_merging(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Target::Hosted { repository, .. } = self.target.clone() else {
            return;
        };
        let Some(credentials) = self.store.read(cx).bitbucket_credentials() else {
            return;
        };
        let http_client = self.store.read(cx).http_client();
        let is_checked_out = self
            .open_pull_request
            .as_ref()
            .is_some_and(|open_pull_request| {
                self.branch.as_deref() == Some(open_pull_request.summary.source_branch.as_str())
                    && !open_pull_request.summary.is_from_fork(&repository)
            });
        let Some(open_pull_request) = &mut self.open_pull_request else {
            return;
        };
        let pull_request = &open_pull_request.summary;
        let destination = pull_request.destination_branch.clone();
        let mut message = format!("{} (#{})", pull_request.title, pull_request.number);
        if let Some(task_id) = clickup_task_id(&pull_request.source_branch) {
            message.push_str(&format!("\n\nCloses {task_id}."));
        }
        let message_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(2, 8, window, cx);
            editor.set_text(message, window, cx);
            editor
        });

        let load = cx.background_spawn(async move {
            bitbucket::get_merge_strategies(&http_client, &credentials, &repository, &destination)
                .await
        });
        let load_task = cx.spawn(async move |this, cx| {
            let strategies = load.await;
            this.update(cx, |this, cx| {
                let Some(form) = this
                    .open_pull_request
                    .as_mut()
                    .and_then(|open_pull_request| open_pull_request.merging.as_mut())
                else {
                    return;
                };
                form.strategies = strategies
                    .context("Repo: estratégias de merge indisponíveis")
                    .log_err()
                    .unwrap_or_else(|| {
                        vec![
                            MergeStrategy::MergeCommit,
                            MergeStrategy::Squash,
                            MergeStrategy::FastForward,
                        ]
                    });
                form.strategy = form.strategies.first().copied();
                cx.notify();
            })
            .log_err();
        });

        open_pull_request.tab = DetailTab::Overview;
        open_pull_request.error = None;
        open_pull_request.notice = None;
        open_pull_request.merging = Some(MergeForm {
            strategies: Vec::new(),
            strategy: None,
            message_editor,
            close_source_branch: true,
            clean_up_local: is_checked_out,
            submitting: false,
            progress: None,
            _load_task: load_task,
        });
        cx.notify();
    }

    fn merge_form_mut(&mut self) -> Option<&mut MergeForm> {
        self.open_pull_request
            .as_mut()
            .and_then(|open_pull_request| open_pull_request.merging.as_mut())
    }

    fn cancel_merging(&mut self, cx: &mut Context<Self>) {
        if let Some(open_pull_request) = &mut self.open_pull_request
            && !open_pull_request
                .merging
                .as_ref()
                .is_some_and(|form| form.submitting)
        {
            open_pull_request.merging = None;
            cx.notify();
        }
    }

    fn set_merge_progress(&mut self, progress: &str, cx: &mut Context<Self>) {
        if let Some(form) = self.merge_form_mut() {
            form.progress = Some(progress.to_string().into());
            cx.notify();
        }
    }

    fn merge(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Target::Hosted {
            repository: remote_repository,
            remote_name,
        } = self.target.clone()
        else {
            return;
        };
        let Some(credentials) = self.store.read(cx).bitbucket_credentials() else {
            return;
        };
        let http_client = self.store.read(cx).http_client();
        let repository = self.project.read(cx).active_repository(cx);
        let Some(open_pull_request) = &self.open_pull_request else {
            return;
        };
        // The checkbox was set up for the branch checked out back then; only the PR's own
        // branch may be deleted, so check it's still the one checked out.
        let local_status = (self.branch.as_deref()
            == Some(open_pull_request.summary.source_branch.as_str()))
        .then(|| self.local_branch_status(cx))
        .flatten();
        let Some(open_pull_request) = &mut self.open_pull_request else {
            return;
        };
        let number = open_pull_request.summary.number;
        let destination = open_pull_request.summary.destination_branch.clone();
        let Some(form) = &mut open_pull_request.merging else {
            return;
        };
        let Some(strategy) = form.strategy else {
            return;
        };
        if form.submitting {
            return;
        }
        let message = form.message_editor.read(cx).text(cx).trim().to_string();
        let close_source_branch = form.close_source_branch;
        // Only a branch whose every commit is on the remote can be deleted safely: those
        // commits are in the pull request that just merged.
        let clean_up = form
            .clean_up_local
            .then(|| local_status.zip(repository))
            .flatten()
            .map(|(status, repository)| {
                (
                    status.ahead == 0 && !status.needs_upstream,
                    status.local_name,
                    repository,
                )
            });
        form.submitting = true;
        form.progress = Some("Fazendo o merge no Bitbucket…".into());
        open_pull_request.error = None;
        cx.notify();

        let askpass = (
            self.askpass_delegate("git fetch", window, cx),
            self.askpass_delegate("git pull", window, cx),
        );
        cx.spawn_in(window, async move |this, cx| {
            let merged = async {
                let outcome = cx
                    .background_spawn({
                        let http_client = http_client.clone();
                        let credentials = credentials.clone();
                        let remote_repository = remote_repository.clone();
                        async move {
                            bitbucket::merge_pull_request(
                                &http_client,
                                &credentials,
                                &remote_repository,
                                number,
                                strategy,
                                &message,
                                close_source_branch,
                            )
                            .await
                        }
                    })
                    .await?;
                let task_url = match outcome {
                    MergeOutcome::Merged(pull_request) => return anyhow::Ok(pull_request),
                    MergeOutcome::Pending { task_url } => task_url,
                };
                this.update(cx, |this, cx| {
                    this.set_merge_progress("O Bitbucket ainda está fazendo o merge…", cx)
                })?;
                for _ in 0..MERGE_POLL_ATTEMPTS {
                    cx.background_executor().timer(MERGE_POLL_INTERVAL).await;
                    let task = cx
                        .background_spawn({
                            let http_client = http_client.clone();
                            let credentials = credentials.clone();
                            let remote_repository = remote_repository.clone();
                            let task_url = task_url.clone();
                            async move {
                                bitbucket::get_merge_task(
                                    &http_client,
                                    &credentials,
                                    &remote_repository,
                                    &task_url,
                                )
                                .await
                            }
                        })
                        .await?;
                    if let Some(pull_request) = task {
                        return Ok(pull_request);
                    }
                }
                anyhow::bail!("o merge ainda não terminou; confira no Bitbucket")
            }
            .await;

            let merged = match merged {
                Ok(merged) => merged,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        if let Some(form) = this.merge_form_mut() {
                            form.submitting = false;
                            form.progress = None;
                        }
                        this.set_open_error(format!("Merge bloqueado: {error:#}"), cx);
                    })?;
                    return anyhow::Ok(());
                }
            };

            let cleanup = match clean_up {
                Some((force_allowed, local_branch, repository)) => {
                    this.update(cx, |this, cx| {
                        this.set_merge_progress(&format!("Voltando para {destination}…"), cx)
                    })?;
                    Some(
                        clean_up_after_merge(
                            &repository,
                            &remote_name,
                            &destination,
                            &local_branch,
                            force_allowed && strategy.rewrites_commits(),
                            askpass,
                            cx,
                        )
                        .await
                        .map(|()| local_branch),
                    )
                }
                None => None,
            };

            this.update(cx, |this, cx| {
                if let Some(open_pull_request) = &mut this.open_pull_request {
                    open_pull_request.summary = merged;
                    open_pull_request.merging = None;
                    open_pull_request.notice = Some(
                        match &cleanup {
                            Some(Ok(local_branch)) => format!(
                                "Mergeado em {destination}. Você está em {destination} e {local_branch} foi apagado."
                            ),
                            _ => format!("Mergeado em {destination}."),
                        }
                        .into(),
                    );
                    if let Some(Err(error)) = &cleanup {
                        open_pull_request.error =
                            Some(format!("O merge foi feito, mas a limpeza local falhou: {error:#}").into());
                    }
                }
                this.reload_open_pull_request(cx);
                this.sync(cx);
            })
        })
        .detach_and_log_err(cx);
    }

    fn decline(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Target::Hosted { repository, .. } = self.target.clone() else {
            return;
        };
        let Some(credentials) = self.store.read(cx).bitbucket_credentials() else {
            return;
        };
        let http_client = self.store.read(cx).http_client();
        let Some(open_pull_request) = &self.open_pull_request else {
            return;
        };
        if self.is_busy() {
            return;
        }
        let number = open_pull_request.summary.number;
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            &format!("Recusar o PR #{number}?"),
            Some("Ele fecha sem merge. Dá para reabrir pelo Bitbucket, mas não daqui."),
            &["Recusar", "Cancelar"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return anyhow::Ok(());
            }
            this.update(cx, |this, cx| {
                this.set_pending_action(Some(PendingAction::Declining), cx)
            })?;
            let result = cx
                .background_spawn(async move {
                    bitbucket::decline_pull_request(&http_client, &credentials, &repository, number)
                        .await
                })
                .await;
            this.update(cx, |this, cx| {
                this.set_pending_action(None, cx);
                match result {
                    Ok(()) => {
                        if let Some(open_pull_request) = &mut this.open_pull_request {
                            open_pull_request.merging = None;
                            open_pull_request.notice = Some("Pull request recusado.".into());
                        }
                        this.reload_open_pull_request(cx);
                        this.sync(cx);
                    }
                    Err(error) => {
                        this.set_open_error(format!("Não deu para recusar: {error:#}"), cx)
                    }
                }
            })
        })
        .detach_and_log_err(cx);
    }

    fn render_merge_card(
        &self,
        open_pull_request: &OpenPullRequest,
        form: &MergeForm,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let gates = merge_gates(
            &open_pull_request.summary,
            open_pull_request.detail.as_ref(),
        );
        let worst = gates
            .iter()
            .map(|gate| gate.level)
            .max()
            .unwrap_or(GateLevel::Ok);
        let destination = &open_pull_request.summary.destination_branch;
        let (title, dot) = match worst {
            GateLevel::Ok => (
                format!("Pronto para merge em {destination}"),
                Color::Success,
            ),
            GateLevel::Warning => (
                format!("Merge em {destination} com pendências"),
                Color::Warning,
            ),
            GateLevel::Blocking => (format!("Merge em {destination} bloqueado"), Color::Error),
        };
        let checked_out_source =
            self.branch.as_deref() == Some(open_pull_request.summary.source_branch.as_str());

        let strategies = h_flex()
            .flex_wrap()
            .gap(DynamicSpacing::Base04.px(cx))
            .children(form.strategies.iter().map(|strategy| {
                let strategy = *strategy;
                let selected = form.strategy == Some(strategy);
                Button::new(
                    SharedString::from(format!("repo-merge-{strategy:?}")),
                    strategy.label(),
                )
                .style(if selected {
                    ButtonStyle::Filled
                } else {
                    ButtonStyle::Outlined
                })
                .label_size(LabelSize::Small)
                .toggle_state(selected)
                .disabled(form.submitting)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(form) = this.merge_form_mut() {
                        form.strategy = Some(strategy);
                        cx.notify();
                    }
                }))
            }))
            .when(form.strategies.is_empty(), |this| {
                this.child(
                    Label::new("Carregando as estratégias do branch…")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            });
        let strategy_hint = form.strategy.map(|strategy| match strategy {
            MergeStrategy::Squash | MergeStrategy::SquashFastForward => {
                let commits = open_pull_request
                    .detail
                    .as_ref()
                    .map_or(0, |detail| detail.commits.len());
                format!("{commits} commit(s) viram 1 em {destination}")
            }
            MergeStrategy::MergeCommit => "Mantém os commits e cria um commit de merge".to_string(),
            MergeStrategy::FastForward => format!("Só avança {destination}; falha se ele andou"),
            MergeStrategy::RebaseFastForward | MergeStrategy::RebaseMerge => {
                format!("Reescreve os commits por cima de {destination}")
            }
        });

        v_flex()
            .gap(DynamicSpacing::Base10.px(cx))
            .p(DynamicSpacing::Base12.px(cx))
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().colors().border_focused)
            .bg(cx.theme().colors().element_background)
            .child(
                h_flex()
                    .gap(DynamicSpacing::Base08.px(cx))
                    .child(div().size(px(8.)).rounded_full().bg(dot.color(cx)))
                    .child(
                        Label::new(title)
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD),
                    ),
            )
            .child(
                v_flex()
                    .gap(DynamicSpacing::Base04.px(cx))
                    .children(gates.into_iter().map(|gate| {
                        let (icon, color) = match gate.level {
                            GateLevel::Ok => (IconName::Check, Color::Success),
                            GateLevel::Warning => (IconName::Warning, Color::Warning),
                            GateLevel::Blocking => (IconName::XCircle, Color::Error),
                        };
                        h_flex()
                            .gap(DynamicSpacing::Base08.px(cx))
                            .child(Icon::new(icon).size(IconSize::XSmall).color(color))
                            .child(div().flex_1().min_w_0().child(
                                Label::new(gate.text).size(LabelSize::Small).color(
                                    if gate.level == GateLevel::Ok {
                                        Color::Default
                                    } else {
                                        color
                                    },
                                ),
                            ))
                    })),
            )
            .child(div().h_px().w_full().bg(cx.theme().colors().border_variant))
            .child(
                v_flex()
                    .gap(DynamicSpacing::Base06.px(cx))
                    .child(
                        Label::new("Estratégia")
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD)
                            .color(Color::Muted),
                    )
                    .child(strategies)
                    .when_some(strategy_hint, |this, hint| {
                        this.child(Label::new(hint).size(LabelSize::XSmall).color(Color::Muted))
                    }),
            )
            .child(
                v_flex()
                    .gap(DynamicSpacing::Base06.px(cx))
                    .child(
                        Label::new("Mensagem do commit")
                            .size(LabelSize::Small)
                            .weight(FontWeight::SEMIBOLD)
                            .color(Color::Muted),
                    )
                    .child(
                        div()
                            .p(DynamicSpacing::Base08.px(cx))
                            .rounded_md()
                            .border_1()
                            .border_color(cx.theme().colors().border)
                            .bg(cx.theme().colors().editor_background)
                            .child(form.message_editor.clone()),
                    ),
            )
            .child(
                Checkbox::new("repo-merge-close-source", form.close_source_branch.into())
                    .label("Fechar o branch no Bitbucket")
                    .disabled(form.submitting)
                    .on_click({
                        let panel = cx.weak_entity();
                        move |state, _, cx| {
                            let checked = state.selected();
                            panel
                                .update(cx, |panel, cx| {
                                    if let Some(form) = panel.merge_form_mut() {
                                        form.close_source_branch = checked;
                                        cx.notify();
                                    }
                                })
                                .log_err();
                        }
                    }),
            )
            .when(checked_out_source, |this| {
                this.child(
                    v_flex()
                        .gap_0p5()
                        .child(
                            Checkbox::new("repo-merge-clean-up", form.clean_up_local.into())
                                .label(format!(
                                    "Depois: voltar para {destination} e apagar o branch local"
                                ))
                                .disabled(form.submitting)
                                .on_click({
                                    let panel = cx.weak_entity();
                                    move |state, _, cx| {
                                        let checked = state.selected();
                                        panel
                                            .update(cx, |panel, cx| {
                                                if let Some(form) = panel.merge_form_mut() {
                                                    form.clean_up_local = checked;
                                                    cx.notify();
                                                }
                                            })
                                            .log_err();
                                    }
                                }),
                        )
                        .child(
                            Label::new(format!(
                                "git switch {destination} · git pull · git branch -d"
                            ))
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Muted)
                            .ml(DynamicSpacing::Base24.px(cx)),
                        ),
                )
            })
    }

    fn render_merge_actions(
        &self,
        open_pull_request: &OpenPullRequest,
        form: &MergeForm,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let gates = merge_gates(
            &open_pull_request.summary,
            open_pull_request.detail.as_ref(),
        );
        let blocked = gates.iter().any(|gate| gate.level == GateLevel::Blocking);
        let destination = &open_pull_request.summary.destination_branch;
        let label = match form.strategy {
            Some(MergeStrategy::Squash | MergeStrategy::SquashFastForward) => {
                format!("Squash em {destination}")
            }
            Some(MergeStrategy::FastForward) => format!("Fast-forward em {destination}"),
            Some(MergeStrategy::RebaseFastForward | MergeStrategy::RebaseMerge) => {
                format!("Rebase em {destination}")
            }
            _ => format!("Merge em {destination}"),
        };
        let declining = matches!(
            open_pull_request.pending_action,
            Some(PendingAction::Declining)
        );
        v_flex()
            .key_context("RepoMerge")
            .on_action(cx.listener(|this, _: &menu::Cancel, _, cx| this.cancel_merging(cx)))
            .gap(DynamicSpacing::Base06.px(cx))
            .p(DynamicSpacing::Base08.px(cx))
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .when_some(form.progress.clone(), |this, progress| {
                this.child(
                    h_flex()
                        .gap(DynamicSpacing::Base06.px(cx))
                        .child(spinning_if(
                            Icon::new(IconName::LoadCircle)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                            true,
                        ))
                        .child(
                            Label::new(progress)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
            })
            .child(
                h_flex()
                    .gap(DynamicSpacing::Base06.px(cx))
                    .child(
                        Button::new("repo-merge-cancel", "Cancelar")
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .disabled(form.submitting)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_merging(cx))),
                    )
                    .child(
                        Button::new(
                            "repo-merge-decline",
                            if declining { "Recusando…" } else { "Recusar" },
                        )
                        .style(ButtonStyle::Outlined)
                        .label_size(LabelSize::Small)
                        .color(Color::Error)
                        .start_icon(
                            Icon::new(IconName::XCircle)
                                .size(IconSize::XSmall)
                                .color(Color::Error),
                        )
                        .disabled(form.submitting || declining)
                        .on_click(cx.listener(|this, _, window, cx| this.decline(window, cx))),
                    )
                    .child(
                        Button::new("repo-merge-submit", label)
                            .full_width()
                            .style(ButtonStyle::Filled)
                            .label_size(LabelSize::Small)
                            .start_icon(Icon::new(IconName::Check).size(IconSize::XSmall))
                            .disabled(form.submitting || form.strategy.is_none() || blocked)
                            .when(blocked, |this| {
                                this.tooltip(Tooltip::text("Resolva os conflitos antes do merge"))
                            })
                            .on_click(cx.listener(|this, _, window, cx| this.merge(window, cx))),
                    ),
            )
    }
}

/// Bitbucket's own answer can take its whole request timeout; this covers the rest of a slow
/// merge before telling the user to look on the web.
const MERGE_POLL_INTERVAL: Duration = Duration::from_secs(2);
const MERGE_POLL_ATTEMPTS: usize = 90;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum GateLevel {
    Ok,
    Warning,
    /// Only conflicts: Bitbucket's own rules decide the rest, and its refusal is shown as is.
    Blocking,
}

struct Gate {
    level: GateLevel,
    text: String,
}

/// What the merge card lists before merging. Most are warnings: Bitbucket Cloud doesn't say
/// which merge checks the branch enforces, so it has the last word.
fn merge_gates(pull_request: &PullRequest, detail: Option<&PullRequestDetail>) -> Vec<Gate> {
    let mut gates = Vec::new();
    let mut gate = |level, text: String| gates.push(Gate { level, text });

    if let Some(detail) = detail {
        let summary = CheckSummary::of(&detail.checks);
        if summary.total == 0 {
            gate(GateLevel::Ok, "Nenhum check configurado".to_string());
        } else if summary.failed > 0 {
            gate(
                GateLevel::Warning,
                format!("{} de {} checks falharam", summary.failed, summary.total),
            );
        } else if summary.running > 0 {
            gate(
                GateLevel::Warning,
                format!(
                    "{} de {} checks ainda rodando",
                    summary.running, summary.total
                ),
            );
        } else {
            gate(
                GateLevel::Ok,
                format!("{}/{} checks passaram", summary.passed, summary.total),
            );
        }
    }

    let reviewers = pull_request.reviewers().count();
    let approvals = pull_request.approval_count();
    gate(
        if reviewers > 0 && approvals >= reviewers {
            GateLevel::Ok
        } else {
            GateLevel::Warning
        },
        approval_label(approvals, reviewers),
    );
    for participant in &pull_request.participants {
        if participant.requested_changes {
            gate(
                GateLevel::Warning,
                format!("{} pediu mudanças", participant.person.short_name()),
            );
        }
    }
    if pull_request.draft {
        gate(GateLevel::Warning, "É um rascunho".to_string());
    }
    if pull_request.open_task_count > 0 {
        gate(
            GateLevel::Warning,
            format!("{} tarefa(s) aberta(s) no PR", pull_request.open_task_count),
        );
    }
    if let Some(detail) = detail {
        let conflicts = detail
            .files
            .iter()
            .filter(|file| file.change == FileChange::Conflict)
            .count();
        if conflicts > 0 {
            gate(
                GateLevel::Blocking,
                format!("Conflitos em {conflicts} arquivo(s)"),
            );
        } else {
            gate(
                GateLevel::Ok,
                format!("Sem conflitos com {}", pull_request.destination_branch),
            );
        }
    }
    gates
}

/// After the pull request of the checked-out branch merged: switch to the destination, bring it
/// up to date and delete the merged branch.
async fn clean_up_after_merge(
    repository: &Entity<Repository>,
    remote_name: &str,
    destination: &str,
    local_branch: &str,
    force_delete: bool,
    (fetch_askpass, pull_askpass): (AskPassDelegate, AskPassDelegate),
    cx: &mut AsyncApp,
) -> anyhow::Result<()> {
    fetch_remote(repository, remote_name, fetch_askpass, cx).await?;
    let switch = repository.update(cx, |repository, _| {
        repository.change_branch(destination.to_string())
    });
    if switch.await?.is_err() {
        // No local copy of the destination yet: create one tracking the remote's.
        let switch = repository.update(cx, |repository, _| {
            repository.change_branch(format!("{remote_name}/{destination}"))
        });
        switch.await??;
    }
    let pull = repository.update(cx, |repository, cx| {
        repository.pull(
            Some(destination.to_string().into()),
            remote_name.to_string().into(),
            false,
            pull_askpass,
            cx,
        )
    });
    pull.await??;
    let delete = repository.update(cx, |repository, _| {
        repository.delete_branch(false, local_branch.to_string(), false)
    });
    if let Err(error) = delete.await? {
        // Squash and rebase merges leave commits git doesn't recognize as merged.
        if !force_delete {
            return Err(error.context(format!("{local_branch} não foi apagado")));
        }
        let delete = repository.update(cx, |repository, _| {
            repository.delete_branch(false, local_branch.to_string(), true)
        });
        delete.await??;
    }
    Ok(())
}

impl RepoPanel {
    /// Loads a list tab's data in the background into the slot `slot` picks.
    fn load_into<T, F>(
        &mut self,
        slot: fn(&mut Self) -> &mut Loadable<T>,
        fetch: impl FnOnce(Arc<dyn HttpClient>, bitbucket::Credentials, RemoteRepository) -> F,
        cx: &mut Context<Self>,
    ) where
        T: 'static,
        F: Future<Output = anyhow::Result<T>> + Send + 'static,
        T: Send,
    {
        let Target::Hosted { repository, .. } = self.target.clone() else {
            return;
        };
        let Some(credentials) = self.store.read(cx).bitbucket_credentials() else {
            return;
        };
        let http_client = self.store.read(cx).http_client();
        let load = cx.background_spawn(fetch(http_client, credentials, repository));
        let task = cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                if let Err(error) = &result
                    && crate::is_unauthorized(error)
                {
                    this.store.update(cx, |store, cx| store.mark_rejected(cx));
                    return;
                }
                let entry = slot(this);
                entry.loading = false;
                match result {
                    Ok(value) => {
                        entry.value = Some(value);
                        entry.error = None;
                    }
                    Err(error) => entry.error = Some(describe_tab_error(&error).into()),
                }
                cx.notify();
            })
            .log_err();
        });
        let entry = slot(self);
        entry.loading = true;
        entry.task = Some(task);
        cx.notify();
    }

    /// Loads whatever the selected list tab shows that isn't kept in sync.
    fn load_tab_data(&mut self, cx: &mut Context<Self>) {
        match self.list_tab {
            ListTab::PullRequests if self.state_filter != PullRequestState::Open => {
                let state = self.state_filter;
                self.load_into(
                    |this| &mut this.closed_pull_requests,
                    move |client, credentials, repository| async move {
                        let list = bitbucket::list_closed_pull_requests(
                            &client,
                            &credentials,
                            &repository,
                            state,
                        )
                        .await?;
                        Ok((state, list))
                    },
                    cx,
                );
            }
            ListTab::PullRequests => {}
            ListTab::Pipelines => self.load_into(
                |this| &mut this.pipelines,
                |client, credentials, repository| async move {
                    bitbucket::list_pipelines(&client, &credentials, &repository).await
                },
                cx,
            ),
            ListTab::Issues => self.load_into(
                |this| &mut this.issues,
                |client, credentials, repository| async move {
                    bitbucket::list_open_issues(&client, &credentials, &repository).await
                },
                cx,
            ),
        }
    }

    fn select_list_tab(&mut self, tab: ListTab, cx: &mut Context<Self>) {
        if self.list_tab == tab {
            return;
        }
        self.list_tab = tab;
        let already_loaded = match tab {
            ListTab::PullRequests => true,
            ListTab::Pipelines => self.pipelines.value.is_some(),
            ListTab::Issues => self.issues.value.is_some(),
        };
        if !already_loaded {
            self.load_tab_data(cx);
        }
        cx.notify();
    }

    fn set_state_filter(&mut self, state: PullRequestState, cx: &mut Context<Self>) {
        self.state_filter = state;
        self.load_tab_data(cx);
        cx.notify();
    }

    fn render_list_tabs(&self, cx: &Context<Self>) -> impl IntoElement {
        let open_count = self.pull_requests.list.len();
        let issue_count = self
            .issues
            .value
            .as_ref()
            .and_then(|issues| issues.as_ref().map(Vec::len));
        let running = self.pipelines.value.as_ref().map(|pipelines| {
            pipelines
                .iter()
                .filter(|pipeline| {
                    matches!(
                        pipeline.state,
                        api::PipelineState::Running | api::PipelineState::Pending
                    )
                })
                .count()
        });
        let tabs = [
            (ListTab::PullRequests, "Pull requests", Some(open_count)),
            (ListTab::Issues, "Issues", issue_count),
            (
                ListTab::Pipelines,
                "Pipelines",
                running.filter(|count| *count > 0),
            ),
        ];
        TabBar::new("repo-list-tabs")
            .style(TabStyle::Pill)
            .children(
                tabs.into_iter()
                    .enumerate()
                    .map(|(index, (tab, label, count))| {
                        let selected = self.list_tab == tab;
                        let position = match index {
                            0 => TabPosition::First,
                            2 => TabPosition::Last,
                            _ => TabPosition::Middle(std::cmp::Ordering::Equal),
                        };
                        Tab::new(label)
                            .style(TabStyle::Pill)
                            .position(position)
                            .toggle_state(selected)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.select_list_tab(tab, cx)),
                            )
                            .child(
                                h_flex()
                                    .gap(DynamicSpacing::Base06.px(cx))
                                    .child(Label::new(label).size(LabelSize::Small).color(
                                        if selected {
                                            Color::Default
                                        } else {
                                            Color::Muted
                                        },
                                    ))
                                    .when_some(count, |this, count| {
                                        this.child(
                                            Label::new(count.to_string())
                                                .size(LabelSize::XSmall)
                                                .color(if selected {
                                                    Color::Accent
                                                } else {
                                                    Color::Muted
                                                }),
                                        )
                                    }),
                            )
                    }),
            )
    }

    fn render_state_filter(&self, cx: &Context<Self>) -> impl IntoElement {
        let panel = cx.weak_entity();
        let current = self.state_filter;
        PopoverMenu::new("repo-state-filter")
            .trigger(
                Button::new("repo-state-filter-button", state_filter_label(current))
                    .style(ButtonStyle::Outlined)
                    .label_size(LabelSize::Small)
                    .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::XSmall)),
            )
            .anchor(gpui::Anchor::TopRight)
            .menu(move |window, cx| {
                let panel = panel.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for state in [
                        PullRequestState::Open,
                        PullRequestState::Merged,
                        PullRequestState::Declined,
                    ] {
                        let panel = panel.clone();
                        menu = menu.toggleable_entry(
                            state_filter_label(state),
                            state == current,
                            IconPosition::End,
                            None,
                            move |_, cx| {
                                panel
                                    .update(cx, |panel, cx| panel.set_state_filter(state, cx))
                                    .log_err();
                            },
                        );
                    }
                    menu
                }))
            })
    }

    fn render_closed_list(&self, cx: &Context<Self>) -> AnyElement {
        let loaded = self
            .closed_pull_requests
            .value
            .as_ref()
            .filter(|(state, _)| *state == self.state_filter)
            .map(|(_, list)| list);
        let Some(list) = loaded else {
            if let Some(error) = self.closed_pull_requests.error.clone() {
                return self
                    .render_centered_message(IconName::Warning, error, None, None, cx)
                    .into_any_element();
            }
            return self.render_loading("Buscando os pull requests…", cx);
        };
        let filter = self.filter_input.read(cx).text(cx).trim().to_lowercase();
        let visible: Vec<&PullRequest> = list
            .iter()
            .filter(|pull_request| pull_request_matches(pull_request, &filter))
            .collect();
        if visible.is_empty() {
            return self
                .render_centered_message(
                    IconName::MagnifyingGlass,
                    "Nada por aqui".into(),
                    None,
                    None,
                    cx,
                )
                .into_any_element();
        }
        let now = Utc::now();
        let dot = match self.state_filter {
            PullRequestState::Merged => Color::Accent,
            PullRequestState::Declined => Color::Error,
            _ => Color::Muted,
        }
        .color(cx);
        v_flex()
            .id("repo-closed-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(DynamicSpacing::Base08.px(cx))
            .pb(DynamicSpacing::Base08.px(cx))
            .children(
                visible
                    .into_iter()
                    .map(|pull_request| self.render_row(pull_request, dot, false, now, cx)),
            )
            .into_any_element()
    }

    fn render_tab_status<T>(
        &self,
        loadable: &Loadable<T>,
        what: &'static str,
        cx: &App,
    ) -> Option<AnyElement> {
        if loadable.value.is_some() {
            return loadable.error.clone().map(|error| {
                render_inline_error(format!("Falha ao atualizar: {error}"), cx).into_any_element()
            });
        }
        Some(match &loadable.error {
            Some(error) => self
                .render_centered_message(IconName::Warning, error.clone(), None, None, cx)
                .into_any_element(),
            None => self.render_loading(what, cx),
        })
    }

    fn render_pipelines(&self, cx: &Context<Self>) -> AnyElement {
        let status = self.render_tab_status(&self.pipelines, "Buscando os pipelines…", cx);
        let Some(pipelines) = self.pipelines.value.as_ref() else {
            return status.unwrap_or_else(|| div().into_any_element());
        };
        if pipelines.is_empty() {
            return self
                .render_centered_message(
                    IconName::PlayOutlined,
                    "Nenhum pipeline rodou ainda".into(),
                    Some("Pipelines aparecem aqui quando o repositório tem um bitbucket-pipelines.yml.".into()),
                    None,
                    cx,
                )
                .into_any_element();
        }
        let now = Utc::now();
        v_flex()
            .flex_1()
            .min_h_0()
            .children(status)
            .child(
                v_flex()
                    .id("repo-pipelines")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(DynamicSpacing::Base08.px(cx))
                    .pb(DynamicSpacing::Base08.px(cx))
                    .children(
                        pipelines
                            .iter()
                            .map(|pipeline| render_pipeline_row(pipeline, now, cx)),
                    ),
            )
            .into_any_element()
    }

    fn render_issues(&self, cx: &Context<Self>) -> AnyElement {
        let status = self.render_tab_status(&self.issues, "Buscando as issues…", cx);
        let Some(issues) = self.issues.value.as_ref() else {
            return status.unwrap_or_else(|| div().into_any_element());
        };
        let Some(issues) = issues else {
            return self
                .render_centered_message(
                    IconName::ListTodo,
                    "Este repositório não usa as issues do Bitbucket".into(),
                    Some("Times que acompanham o trabalho no Jira ou no ClickUp não têm issues aqui.".into()),
                    None,
                    cx,
                )
                .into_any_element();
        };
        if issues.is_empty() {
            return self
                .render_centered_message(
                    IconName::Check,
                    "Nenhuma issue aberta".into(),
                    None,
                    None,
                    cx,
                )
                .into_any_element();
        }
        let now = Utc::now();
        v_flex()
            .flex_1()
            .min_h_0()
            .children(status)
            .child(
                v_flex()
                    .id("repo-issues")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(DynamicSpacing::Base08.px(cx))
                    .pb(DynamicSpacing::Base08.px(cx))
                    .children(issues.iter().map(|issue| render_issue_row(issue, now, cx))),
            )
            .into_any_element()
    }
}

fn state_filter_label(state: PullRequestState) -> &'static str {
    match state {
        PullRequestState::Open => "Abertas",
        PullRequestState::Merged => "Mergeadas",
        PullRequestState::Declined => "Recusadas",
        PullRequestState::Superseded => "Substituídas",
    }
}

/// Errors of the on-demand tabs, with the missing scope spelled out when that's the cause.
fn describe_tab_error(error: &anyhow::Error) -> String {
    if crate::is_forbidden(error) {
        let text = format!("{error:#}");
        let scope = if text.contains("issue") {
            "read:issue:bitbucket"
        } else if text.contains("pipeline") {
            "read:pipeline:bitbucket"
        } else {
            "de leitura"
        };
        return format!("O token não tem o escopo {scope}. Crie outro token incluindo-o.");
    }
    format!("{error:#}")
}

fn format_duration(seconds: u64) -> String {
    match (seconds / 60, seconds % 60) {
        (0, seconds) => format!("{seconds}s"),
        (minutes, 0) => format!("{minutes}m"),
        (minutes, seconds) => format!("{minutes}m {seconds}s"),
    }
}

fn render_pipeline_row(pipeline: &api::Pipeline, now: DateTime<Utc>, cx: &App) -> impl IntoElement {
    let (icon, color, spinning) = match pipeline.state {
        api::PipelineState::Passed => (IconName::Check, Color::Success, false),
        api::PipelineState::Failed => (IconName::XCircle, Color::Error, false),
        api::PipelineState::Running => (IconName::LoadCircle, Color::Warning, true),
        api::PipelineState::Pending => (IconName::Clock, Color::Muted, false),
        api::PipelineState::Paused => (IconName::Stop, Color::Warning, false),
        api::PipelineState::Stopped => (IconName::Stop, Color::Muted, false),
    };
    let title = match (&pipeline.ref_name, pipeline.pull_request) {
        (Some(ref_name), _) => ref_name.clone(),
        (None, Some(number)) => format!("PR #{number}"),
        (None, None) => "Pipeline".to_string(),
    };
    let trigger = match pipeline.trigger.as_str() {
        "PUSH" => "push".to_string(),
        "MANUAL" => "manual".to_string(),
        "SCHEDULE" => "agendado".to_string(),
        other => other.to_lowercase().replace('_', " "),
    };
    let mut meta = vec![format!("#{}", pipeline.number)];
    meta.extend((!trigger.is_empty()).then_some(trigger));
    meta.extend(pipeline.creator.clone());
    meta.extend(
        pipeline
            .created_at
            .map(|created_at| relative_age(created_at, now)),
    );
    let url = pipeline.url.clone();
    h_flex()
        .id(SharedString::from(format!(
            "repo-pipeline-{}",
            pipeline.number
        )))
        .items_start()
        .gap(DynamicSpacing::Base10.px(cx))
        .px(DynamicSpacing::Base08.px(cx))
        .py(DynamicSpacing::Base06.px(cx))
        .rounded_md()
        .cursor_pointer()
        .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
        .tooltip(Tooltip::text("Abrir no Bitbucket"))
        .on_click(move |_, _, cx| cx.open_url(&url))
        .child(h_flex().h(DynamicSpacing::Base20.px(cx)).child(spinning_if(
            Icon::new(icon).size(IconSize::Small).color(color),
            spinning,
        )))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    h_flex()
                        .gap(DynamicSpacing::Base06.px(cx))
                        .child(Label::new(title).size(LabelSize::Small).truncate())
                        .when_some(pipeline.commit.as_ref(), |this, commit| {
                            this.child(
                                Label::new(commit.get(..7).unwrap_or(commit).to_string())
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .color(Color::Accent),
                            )
                        }),
                )
                .child(
                    Label::new(meta.join(" · "))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .truncate(),
                ),
        )
        .when_some(
            pipeline.duration_seconds.filter(|seconds| *seconds > 0),
            |this, seconds| {
                this.child(
                    h_flex().h(DynamicSpacing::Base20.px(cx)).child(
                        Label::new(format_duration(seconds))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
                )
            },
        )
}

fn render_issue_row(issue: &api::Issue, now: DateTime<Utc>, cx: &App) -> impl IntoElement {
    let color = match issue.kind.as_str() {
        "bug" => Color::Error,
        "enhancement" => Color::Info,
        "proposal" => Color::Accent,
        _ => Color::Muted,
    };
    let kind = match issue.kind.as_str() {
        "bug" => "bug",
        "enhancement" => "melhoria",
        "proposal" => "proposta",
        "task" => "tarefa",
        other => other,
    };
    let mut meta = vec![format!("#{}", issue.number), kind.to_string()];
    if matches!(issue.priority.as_str(), "critical" | "blocker" | "major") {
        meta.push(issue.priority.clone());
    }
    if issue.state == "on hold" {
        meta.push("em espera".to_string());
    }
    meta.push(
        issue
            .assignee
            .clone()
            .unwrap_or_else(|| "sem responsável".to_string()),
    );
    let url = issue.url.clone();
    h_flex()
        .id(SharedString::from(format!("repo-issue-{}", issue.number)))
        .items_start()
        .gap(DynamicSpacing::Base10.px(cx))
        .px(DynamicSpacing::Base08.px(cx))
        .py(DynamicSpacing::Base06.px(cx))
        .rounded_md()
        .cursor_pointer()
        .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
        .tooltip(Tooltip::text("Abrir no Bitbucket"))
        .on_click(move |_, _, cx| cx.open_url(&url))
        .child(
            h_flex()
                .flex_none()
                .h(DynamicSpacing::Base20.px(cx))
                .child(div().size(px(8.)).rounded_full().bg(color.color(cx))),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    Label::new(issue.title.clone())
                        .size(LabelSize::Small)
                        .truncate(),
                )
                .child(
                    Label::new(meta.join(" · "))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .truncate(),
                ),
        )
        .when_some(issue.updated_at, |this, updated_at| {
            this.child(
                h_flex().h(DynamicSpacing::Base20.px(cx)).child(
                    Label::new(compact_age(updated_at, now))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            )
        })
}

impl RepoPanel {
    fn move_commit_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(open_pull_request) = &mut self.open_pull_request else {
            return;
        };
        let count = open_pull_request
            .detail
            .as_ref()
            .map_or(0, |detail| detail.commits.len());
        if count == 0 {
            return;
        }
        let next = match open_pull_request.selected_commit {
            None if delta > 0 => 0,
            None => count - 1,
            Some(current) => current.saturating_add_signed(delta).min(count - 1),
        };
        open_pull_request.selected_commit = Some(next);
        cx.notify();
    }

    fn open_selected_commit(&mut self, on_web: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Target::Hosted { repository, .. } = &self.target else {
            return;
        };
        let Some(open_pull_request) = &self.open_pull_request else {
            return;
        };
        let Some(hash) = open_pull_request
            .selected_commit
            .and_then(|index| open_pull_request.detail.as_ref()?.commits.get(index))
            .map(|commit| commit.hash.clone())
        else {
            return;
        };
        if on_web {
            cx.open_url(&format!("{}/commits/{hash}", repository.web_url()));
        } else {
            self.open_commit(hash, window, cx);
        }
    }

    /// Hands the pull request's diff, as Bitbucket computes it, to the agent for a review. No
    /// checkout needed, unlike reviewing the branch diff.
    fn review_with_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Target::Hosted { repository, .. } = self.target.clone() else {
            return;
        };
        let Some(credentials) = self.store.read(cx).bitbucket_credentials() else {
            return;
        };
        let http_client = self.store.read(cx).http_client();
        let Some(open_pull_request) = &self.open_pull_request else {
            return;
        };
        if self.is_busy() {
            return;
        }
        let number = open_pull_request.summary.number;
        let base_ref: SharedString = open_pull_request.summary.destination_branch.clone().into();
        self.set_pending_action(Some(PendingAction::Reviewing), cx);
        cx.spawn_in(window, async move |this, cx| {
            let diff = cx
                .background_spawn(async move {
                    bitbucket::get_pull_request_diff(
                        &http_client,
                        &credentials,
                        &repository,
                        number,
                    )
                    .await
                })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.set_pending_action(None, cx);
                match diff {
                    Ok(diff) if diff.trim().is_empty() => {
                        this.set_open_error("O PR não tem mudanças para revisar.".to_string(), cx)
                    }
                    Ok(diff) => window.dispatch_action(
                        zed_actions::agent::ReviewBranchDiff {
                            diff_text: diff.into(),
                            base_ref,
                        }
                        .boxed_clone(),
                        cx,
                    ),
                    Err(error) => {
                        this.set_open_error(format!("Não deu para baixar o diff: {error:#}"), cx)
                    }
                }
            })
        })
        .detach_and_log_err(cx);
    }
}

async fn fetch_remote(
    repository: &Entity<Repository>,
    remote_name: &str,
    askpass: AskPassDelegate,
    cx: &mut AsyncApp,
) -> anyhow::Result<()> {
    let fetch = repository.update(cx, |repository, cx| {
        repository.fetch(
            FetchOptions::Remote(Remote {
                name: remote_name.to_string().into(),
            }),
            askpass,
            cx,
        )
    });
    fetch.await??;
    Ok(())
}

async fn has_commit(repository: &Entity<Repository>, hash: &str, cx: &mut AsyncApp) -> bool {
    let show = repository.update(cx, |repository, _| repository.show(hash.to_string()));
    matches!(show.await, Ok(Ok(_)))
}

/// The active repository's hosted remote, preferring `origin`, and its checked-out branch as
/// that remote names it.
fn detect_target(project: &Entity<Project>, cx: &App) -> (Target, Option<String>) {
    let Some(repository) = project.read(cx).active_repository(cx) else {
        return (Target::NoRepository, None);
    };
    let repository = repository.read(cx);
    let remotes = [
        ("origin", repository.remote_origin_url.as_deref()),
        ("upstream", repository.remote_upstream_url.as_deref()),
    ];
    for (remote_name, url) in remotes {
        if let Some(hosted) = url.and_then(bitbucket::parse_remote) {
            let branch = repository
                .branch
                .as_ref()
                .map(|branch| remote_branch_name(branch, remote_name));
            return (
                Target::Hosted {
                    repository: hosted,
                    remote_name: remote_name.to_string(),
                },
                branch,
            );
        }
    }
    let remote_url = remotes
        .into_iter()
        .find_map(|(_, url)| url.map(str::to_string));
    let branch = repository
        .branch
        .as_ref()
        .map(|branch| branch.name().to_string());
    (Target::Unsupported { remote_url }, branch)
}

/// A local branch can track a remote one with another name; pull requests use the remote's.
fn remote_branch_name(branch: &git::repository::Branch, remote_name: &str) -> String {
    branch
        .upstream
        .as_ref()
        .filter(|upstream| upstream.remote_name() == Some(remote_name))
        .and_then(|upstream| upstream.branch_name())
        .unwrap_or_else(|| branch.name())
        .to_string()
}

/// Awaiting your review first, then yours, then the rest, each most recently updated first.
fn group_pull_requests<'a>(
    pull_requests: &[&'a PullRequest],
    user_id: &str,
) -> Vec<(Group, Vec<&'a PullRequest>)> {
    let mut groups: Vec<(Group, Vec<&'a PullRequest>)> = vec![
        (Group::AwaitingYourReview, Vec::new()),
        (Group::Yours, Vec::new()),
        (Group::Others, Vec::new()),
    ];
    for pull_request in pull_requests {
        let group = if pull_request.author.id == user_id {
            1
        } else if pull_request.awaits_review_from(user_id) {
            0
        } else {
            2
        };
        if let Some((_, members)) = groups.get_mut(group) {
            members.push(pull_request);
        }
    }
    for (_, members) in &mut groups {
        members.sort_by_key(|pull_request| std::cmp::Reverse(pull_request.updated_at));
    }
    groups.retain(|(_, members)| !members.is_empty());
    groups
}

fn pull_request_matches(pull_request: &PullRequest, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    let number = filter.trim_start_matches('#');
    pull_request.title.to_lowercase().contains(filter)
        || pull_request.number.to_string() == number
        || pull_request.source_branch.to_lowercase().contains(filter)
        || pull_request
            .author
            .display_name
            .to_lowercase()
            .contains(filter)
        || pull_request
            .author
            .short_name()
            .to_lowercase()
            .contains(filter)
}

/// The ClickUp task a branch is named after, as ClickUp's own "copy branch name" writes it:
/// `CU-86a1b2` anywhere in the name.
fn clickup_task_id(branch: &str) -> Option<String> {
    let lowercase = branch.to_lowercase();
    let start = lowercase.find("cu-")?;
    let id: String = branch[start + 3..]
        .chars()
        .take_while(|character| character.is_ascii_alphanumeric())
        .collect();
    (!id.is_empty()).then(|| format!("CU-{id}"))
}

struct ActivityEvent {
    text: String,
    color: Color,
    url: Option<String>,
}

/// Approvals and check results, newest first, as one-line events under the comments.
fn activity_events(
    pull_request: &PullRequest,
    checks: &[api::Check],
    now: DateTime<Utc>,
) -> Vec<ActivityEvent> {
    let mut events: Vec<(DateTime<Utc>, String, Color, Option<String>)> = Vec::new();
    for participant in &pull_request.participants {
        let Some(at) = participant.participated_at else {
            continue;
        };
        let name = participant.person.short_name();
        if participant.approved {
            events.push((at, format!("{name} aprovou"), Color::Success, None));
        } else if participant.requested_changes {
            events.push((at, format!("{name} pediu mudanças"), Color::Error, None));
        }
    }
    for check in checks {
        let Some(at) = check.updated_at else {
            continue;
        };
        let (verb, color) = match check.state {
            CheckState::Passed => ("passou", Color::Success),
            CheckState::Failed => ("falhou", Color::Error),
            CheckState::Running => ("está rodando", Color::Warning),
            CheckState::Stopped => ("parou", Color::Muted),
        };
        events.push((
            at,
            format!("{} {verb}", check.name),
            color,
            check.url.clone(),
        ));
    }
    events.sort_by_key(|(at, _, _, _)| std::cmp::Reverse(*at));
    events
        .into_iter()
        .map(|(at, text, color, url)| ActivityEvent {
            text: format!("{text} · {}", relative_age(at, now)),
            color,
            url,
        })
        .collect()
}

/// "12 min", "3 h", "5 d": the list's right column.
fn compact_age(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let minutes = (now - at).num_minutes().max(0);
    if minutes < 60 {
        format!("{minutes} min")
    } else if minutes < 24 * 60 {
        format!("{} h", minutes / 60)
    } else {
        format!("{} d", minutes / (24 * 60))
    }
}

fn relative_age(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    if (now - at).num_minutes() < 1 {
        "agora".to_string()
    } else {
        format!("há {}", compact_age(at, now))
    }
}

fn approval_label(approvals: usize, reviewers: usize) -> String {
    match (approvals, reviewers) {
        (_, 0) => match approvals {
            0 => "sem revisores".to_string(),
            1 => "1 aprovação".to_string(),
            count => format!("{count} aprovações"),
        },
        (approvals, 1) => format!("{approvals} de 1 aprovação"),
        (approvals, reviewers) => format!("{approvals} de {reviewers} aprovações"),
    }
}

fn comment_count_label(count: u64) -> String {
    if count == 1 {
        "1 comentário".to_string()
    } else {
        format!("{count} comentários")
    }
}

fn file_count_label(count: usize) -> String {
    if count == 1 {
        "em 1 arquivo".to_string()
    } else {
        format!("em {count} arquivos")
    }
}

fn check_summary_color(summary: CheckSummary) -> Color {
    if summary.failed > 0 {
        Color::Error
    } else if summary.running > 0 || summary.passed < summary.total {
        Color::Warning
    } else {
        Color::Success
    }
}

fn pill(text: String, color: Color, icon: Option<IconName>, cx: &App) -> impl IntoElement {
    let hsla = color.color(cx);
    h_flex()
        .gap(DynamicSpacing::Base04.px(cx))
        .px(DynamicSpacing::Base06.px(cx))
        .py_0p5()
        .rounded_md()
        .bg(hsla.opacity(0.12))
        .when_some(icon, |this, icon| {
            this.child(Icon::new(icon).size(IconSize::XSmall).color(color))
        })
        .when(icon.is_none(), |this| {
            this.child(div().size(px(6.)).rounded_full().bg(hsla))
        })
        .child(Label::new(text).size(LabelSize::XSmall).color(color))
}

fn state_pill(pull_request: &PullRequest, cx: &App) -> impl IntoElement {
    let (label, color) = match pull_request.state {
        PullRequestState::Open if pull_request.draft => ("Rascunho", Color::Muted),
        PullRequestState::Open => ("Aberta", Color::Info),
        PullRequestState::Merged => ("Mergeada", Color::Accent),
        PullRequestState::Declined => ("Recusada", Color::Error),
        PullRequestState::Superseded => ("Substituída", Color::Muted),
    };
    pill(label.to_string(), color, None, cx)
}

fn avatar(name: &str, color: Color, cx: &App) -> impl IntoElement {
    h_flex()
        .flex_none()
        .size(DynamicSpacing::Base20.px(cx))
        .justify_center()
        .rounded_full()
        .bg(color.color(cx).opacity(0.16))
        .child(
            Label::new(initials(name))
                .size(LabelSize::XSmall)
                .weight(FontWeight::SEMIBOLD)
                .color(color),
        )
}

fn render_file_row(index: usize, file: &api::ChangedFile, cx: &App) -> impl IntoElement {
    let (name, directory) = match file.path.rsplit_once('/') {
        Some((directory, name)) => (name.to_string(), directory.to_string()),
        None => (file.path.clone(), String::new()),
    };
    let directory = match &file.old_path {
        Some(old_path) => format!("de {old_path}"),
        None => directory,
    };
    let (letter, color) = match file.change {
        FileChange::Added => ("A", Color::Success),
        FileChange::Removed => ("D", Color::Error),
        FileChange::Modified => ("M", Color::Warning),
        FileChange::Renamed => ("R", Color::Info),
        FileChange::Conflict => ("C", Color::Error),
    };
    let mut stats = Vec::new();
    if file.lines_added > 0 {
        stats.push(format!("+{}", file.lines_added));
    }
    if file.lines_removed > 0 {
        stats.push(format!("−{}", file.lines_removed));
    }
    h_flex()
        .id(("repo-pr-file", index))
        .gap(DynamicSpacing::Base08.px(cx))
        .p(DynamicSpacing::Base08.px(cx))
        .rounded_md()
        .border_1()
        .border_color(cx.theme().colors().border_variant)
        .child(
            Icon::new(IconName::File)
                .size(IconSize::Small)
                .color(Color::Muted),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    h_flex()
                        .gap(DynamicSpacing::Base08.px(cx))
                        .child(Label::new(name).size(LabelSize::Small).truncate())
                        .child(
                            Label::new(stats.join(" "))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
                .when(!directory.is_empty(), |this| {
                    this.child(
                        Label::new(directory)
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Muted)
                            .truncate(),
                    )
                }),
        )
        .child(
            div()
                .flex_none()
                .px(DynamicSpacing::Base04.px(cx))
                .rounded_sm()
                .bg(color.color(cx).opacity(0.14))
                .child(
                    Label::new(letter)
                        .size(LabelSize::XSmall)
                        .weight(FontWeight::SEMIBOLD)
                        .color(color),
                ),
        )
}

fn render_comment(comment: &api::Comment, now: DateTime<Utc>, cx: &App) -> impl IntoElement {
    let mut meta = Vec::new();
    if let Some((path, line)) = &comment.inline {
        let file = path.rsplit('/').next().unwrap_or(path);
        meta.push(match line {
            Some(line) => format!("{file}:{line}"),
            None => file.to_string(),
        });
    }
    if let Some(created_at) = comment.created_at {
        meta.push(relative_age(created_at, now));
    }
    h_flex()
        .id(SharedString::from(format!(
            "repo-pr-comment-{}",
            comment.id
        )))
        .items_start()
        .gap(DynamicSpacing::Base08.px(cx))
        .py(DynamicSpacing::Base04.px(cx))
        .when(comment.is_reply, |this| {
            this.pl(DynamicSpacing::Base24.px(cx))
        })
        .child(avatar(&comment.author.display_name, Color::Info, cx))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    h_flex()
                        .gap(DynamicSpacing::Base06.px(cx))
                        .child(
                            Label::new(comment.author.short_name().to_string())
                                .size(LabelSize::Small)
                                .weight(FontWeight::SEMIBOLD),
                        )
                        .child(
                            Label::new(meta.join(" · "))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                )
                .child(Label::new(comment.body.trim().to_string()).size(LabelSize::Small)),
        )
}

fn render_warning(title: &'static str, detail: &'static str, cx: &App) -> impl IntoElement {
    let warning = cx.theme().status().warning;
    h_flex()
        .w_full()
        .gap(DynamicSpacing::Base10.px(cx))
        .p(DynamicSpacing::Base10.px(cx))
        .rounded_lg()
        .border_1()
        .border_color(warning.opacity(0.35))
        .bg(warning.opacity(0.08))
        .child(
            Icon::new(IconName::Warning)
                .size(IconSize::Small)
                .color(Color::Warning),
        )
        .child(
            v_flex()
                .min_w_0()
                .child(
                    Label::new(title)
                        .size(LabelSize::Small)
                        .weight(FontWeight::SEMIBOLD),
                )
                .child(
                    Label::new(detail)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
        )
}

fn render_inline_error(message: String, cx: &App) -> impl IntoElement {
    let warning = cx.theme().status().warning;
    h_flex()
        .mx(DynamicSpacing::Base08.px(cx))
        .mb(DynamicSpacing::Base06.px(cx))
        .gap(DynamicSpacing::Base08.px(cx))
        .p(DynamicSpacing::Base08.px(cx))
        .rounded_md()
        .border_1()
        .border_color(warning.opacity(0.35))
        .bg(warning.opacity(0.08))
        .child(
            Icon::new(IconName::Warning)
                .size(IconSize::XSmall)
                .color(Color::Warning),
        )
        .child(
            div().flex_1().min_w_0().child(
                Label::new(message)
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            ),
        )
}

fn spinning_if(icon: Icon, spinning: bool) -> AnyElement {
    if spinning {
        icon.with_rotate_animation(2).into_any_element()
    } else {
        icon.into_any_element()
    }
}

fn initials(name: &str) -> String {
    name.split(|character: char| character.is_whitespace() || character == '.')
        .filter_map(|word| word.chars().next())
        .filter(|character| character.is_alphanumeric())
        .take(2)
        .flat_map(char::to_uppercase)
        .collect()
}

impl Render for RepoPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.target.clone() {
            Target::NoRepository => self
                .render_centered_message(
                    IconName::GitBranch,
                    "Nenhum repositório git no projeto".into(),
                    Some("Abra uma pasta com um .git para ver os pull requests.".into()),
                    None,
                    cx,
                )
                .into_any_element(),
            Target::Unsupported { remote_url } => self
                .render_centered_message(
                    IconName::PullRequest,
                    "Remoto ainda não suportado".into(),
                    Some(
                        match remote_url {
                            Some(url) => {
                                format!("{url} não é do bitbucket.org. GitHub vem a seguir.")
                            }
                            None => "O repositório não tem origin.".to_string(),
                        }
                        .into(),
                    ),
                    None,
                    cx,
                )
                .into_any_element(),
            Target::Hosted { repository, .. } => {
                match self.store.read(cx).connection(repository.hosting) {
                    Connection::Loading => self.render_loading("Carregando a conta…", cx),
                    Connection::Disconnected => self
                        .render_connect(&repository, false, cx)
                        .into_any_element(),
                    Connection::Rejected => self
                        .render_connect(&repository, true, cx)
                        .into_any_element(),
                    Connection::Connected(_) if self.replacing_token => self
                        .render_connect(&repository, false, cx)
                        .into_any_element(),
                    Connection::Connected(account) => {
                        let account = account.clone();
                        self.render_connected(&repository, &account, cx)
                    }
                    Connection::Unreachable { message } => {
                        let message = message.clone();
                        let store = self.store.clone();
                        let actions = h_flex()
                            .gap(DynamicSpacing::Base08.px(cx))
                            .child(
                                Button::new("repo-retry", "Tentar de novo")
                                    .style(ButtonStyle::Outlined)
                                    .label_size(LabelSize::Small)
                                    .on_click(move |_, _, cx| {
                                        store.update(cx, |store, cx| store.retry(cx))
                                    }),
                            )
                            .child(
                                Button::new("repo-replace-unreachable", "Trocar token")
                                    .style(ButtonStyle::Subtle)
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.start_replacing_token(window, cx);
                                        this.store
                                            .update(cx, |store, cx| store.disconnect(cx))
                                            .detach_and_log_err(cx);
                                    })),
                            )
                            .into_any_element();
                        self.render_centered_message(
                            IconName::Disconnected,
                            "Sem conexão com o Bitbucket".into(),
                            Some(message),
                            Some(actions),
                            cx,
                        )
                        .into_any_element()
                    }
                }
            }
        };

        v_flex()
            .key_context("RepoPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(content)
    }
}

impl Focusable for RepoPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for RepoPanel {}

impl Panel for RepoPanel {
    fn persistent_name() -> &'static str {
        "RepoPanel"
    }

    fn panel_key() -> &'static str {
        "RepoPanel"
    }

    /// Until an account is connected, filling in the token is the only thing to do here.
    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        let Target::Hosted { repository, .. } = &self.target else {
            return self.focus_handle.clone();
        };
        match self.store.read(cx).connection(repository.hosting) {
            Connection::Connected(_) if !self.replacing_token => self.focus_handle.clone(),
            Connection::Disconnected | Connection::Rejected | Connection::Connected(_) => {
                if self.email_input.read(cx).is_empty(cx) {
                    self.email_input.focus_handle(cx)
                } else {
                    self.token_input.focus_handle(cx)
                }
            }
            Connection::Loading | Connection::Unreachable { .. } => self.focus_handle.clone(),
        }
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
        px(384.)
    }

    /// No icon: a panel icon also puts a button in the status bar's panels group, and the
    /// toolkit button is already the way in.
    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        None
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Repo")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        ToggleFocus.boxed_clone()
    }

    fn activation_priority(&self) -> u32 {
        10
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Participant, ParticipantRole, Person};

    fn person(id: &str) -> Person {
        Person {
            id: id.to_string(),
            display_name: id.to_string(),
            nickname: None,
        }
    }

    fn pull_request(number: u64, author: &str, reviewers: &[(&str, bool)]) -> PullRequest {
        PullRequest {
            number,
            title: format!("PR {number}"),
            state: PullRequestState::Open,
            draft: false,
            author: person(author),
            source_branch: format!("feat/{number}"),
            source_repository: None,
            destination_branch: "develop".to_string(),
            description: String::new(),
            comment_count: 0,
            open_task_count: 0,
            updated_at: DateTime::from_timestamp(1_790_000_000 + number as i64, 0),
            url: String::new(),
            participants: reviewers
                .iter()
                .map(|(id, approved)| Participant {
                    person: person(id),
                    role: ParticipantRole::Reviewer,
                    approved: *approved,
                    requested_changes: false,
                    participated_at: None,
                })
                .collect(),
        }
    }

    #[test]
    fn groups_review_requests_before_your_own() {
        let pull_requests = [
            pull_request(1, "ana", &[("me", false)]),
            pull_request(2, "me", &[("ana", true)]),
            pull_request(3, "rafael", &[("me", true)]),
            pull_request(4, "joao", &[]),
            pull_request(5, "ana", &[("me", false)]),
        ];
        let references: Vec<&PullRequest> = pull_requests.iter().collect();
        let groups: Vec<(Group, Vec<u64>)> = group_pull_requests(&references, "me")
            .into_iter()
            .map(|(group, members)| {
                (
                    group,
                    members
                        .iter()
                        .map(|pull_request| pull_request.number)
                        .collect(),
                )
            })
            .collect();
        assert!(
            groups
                == vec![
                    (Group::AwaitingYourReview, vec![5, 1]),
                    (Group::Yours, vec![2]),
                    (Group::Others, vec![4, 3]),
                ]
        );
    }

    #[test]
    fn matches_pull_requests_by_the_upstream_branch_name() {
        let branch = |upstream: Option<&str>| git::repository::Branch {
            is_head: true,
            ref_name: "refs/heads/biometria".into(),
            upstream: upstream.map(|ref_name| git::repository::Upstream {
                ref_name: ref_name.to_string().into(),
                tracking: git::repository::UpstreamTracking::Gone,
            }),
            most_recent_commit: None,
        };
        assert_eq!(
            remote_branch_name(
                &branch(Some("refs/remotes/origin/feat/CU-86a1b2-biometria")),
                "origin"
            ),
            "feat/CU-86a1b2-biometria"
        );
        assert_eq!(
            remote_branch_name(&branch(Some("refs/remotes/fork/outro")), "origin"),
            "biometria"
        );
        assert_eq!(remote_branch_name(&branch(None), "origin"), "biometria");
    }

    #[test]
    fn fills_the_title_and_description_from_the_branch() {
        assert_eq!(
            humanize_branch("feat/CU-86a1b2-login-com-biometria").as_deref(),
            Some("Login com biometria")
        );
        assert_eq!(
            humanize_branch("fix_rota_inicial").as_deref(),
            Some("Fix rota inicial")
        );
        assert_eq!(humanize_branch("feat/"), None);

        let subjects = ["Instala expo".to_string(), "Adiciona hook".to_string()];
        assert_eq!(
            pull_request_description(&subjects, "feat/CU-86a1b2-biometria"),
            "- Instala expo\n- Adiciona hook\n\nCloses CU-86a1b2."
        );
        assert_eq!(pull_request_description(&subjects[..1], "fix/x"), "");
    }

    #[test]
    fn only_conflicts_block_the_merge() {
        let mut pull_request = pull_request(157, "me", &[("ana", true), ("rafael", false)]);
        pull_request.open_task_count = 1;
        let detail = |change| PullRequestDetail {
            pull_request: pull_request.clone(),
            checks: Vec::new(),
            files: vec![api::ChangedFile {
                path: "a.ts".to_string(),
                old_path: None,
                change,
                lines_added: 1,
                lines_removed: 0,
            }],
            comments: Vec::new(),
            commits: Vec::new(),
        };
        let levels = |detail: &PullRequestDetail| {
            merge_gates(&pull_request, Some(detail))
                .into_iter()
                .map(|gate| gate.level)
                .max()
        };
        assert_eq!(
            levels(&detail(FileChange::Modified)),
            Some(GateLevel::Warning)
        );
        assert_eq!(
            levels(&detail(FileChange::Conflict)),
            Some(GateLevel::Blocking)
        );
    }

    #[test]
    fn finds_the_clickup_task_in_the_branch() {
        assert_eq!(
            clickup_task_id("feat/CU-86a1b2-biometria").as_deref(),
            Some("CU-86a1b2")
        );
        assert_eq!(clickup_task_id("cu-9x_fix").as_deref(), Some("CU-9x"));
        assert_eq!(clickup_task_id("feat/login"), None);
        assert_eq!(clickup_task_id("feat/cu-"), None);
    }

    #[test]
    fn filters_by_title_number_or_author() {
        let pull_request = pull_request(157, "ana.souza", &[]);
        assert!(pull_request_matches(&pull_request, "#157"));
        assert!(pull_request_matches(&pull_request, "157"));
        assert!(pull_request_matches(&pull_request, "ana"));
        assert!(pull_request_matches(&pull_request, "pr 1"));
        assert!(!pull_request_matches(&pull_request, "#15"));
    }

    #[test]
    fn labels_ages_and_approvals() {
        let now = DateTime::from_timestamp(1_790_000_000, 0).unwrap();
        assert_eq!(
            compact_age(now - chrono::Duration::minutes(12), now),
            "12 min"
        );
        assert_eq!(compact_age(now - chrono::Duration::hours(3), now), "3 h");
        assert_eq!(compact_age(now - chrono::Duration::days(5), now), "5 d");
        assert_eq!(relative_age(now, now), "agora");
        assert_eq!(approval_label(1, 2), "1 de 2 aprovações");
        assert_eq!(approval_label(0, 0), "sem revisores");
        assert_eq!(initials("ana.souza"), "AS");
        assert_eq!(initials("Rafael M"), "RM");
    }
}
