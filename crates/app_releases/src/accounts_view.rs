//! "Lojas e EAS": the accounts Lançamentos reads with, each tested before it is saved, and the
//! app the project is linked to.

use crate::{
    accounts::{
        AccountKind, AccountToSave, AppLink, AppStoreAccount, ExpoAccount, GooglePlayAccount,
        save_link_override,
    },
    app_store_connect,
    google_play::ServiceAccount,
    store::{AccountsState, AccountsStore, CheckState, ReleasesStore},
};
use anyhow::Context as _;
use chrono::NaiveDate;
use collections::HashMap;
use db::kvp::KeyValueStore;
use gpui::{Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Subscription, Task, px};
use i18n::t;
use std::{path::PathBuf, sync::Arc};
use ui::{
    StatusTone, card, error_callout, prelude::*, primary_button, secondary_button, section_label,
    status_pill,
};
use ui_input::InputField;
use util::ResultExt as _;
use workspace::{
    Workspace,
    item::{Item as WorkspaceItem, ItemEvent},
};

pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let existing = workspace.items_of_type::<AccountsView>(cx).next();
    if let Some(view) = existing {
        workspace.activate_item(&view, true, true, window, cx);
        return;
    }
    let store = ReleasesStore::for_project(workspace.project(), cx);
    let view = cx.new(|cx| AccountsView::new(store, window, cx));
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}

fn set_field_text(field: &Entity<InputField>, text: &str, window: &mut Window, cx: &mut App) {
    let editor = field.read(cx).editor().clone();
    editor.set_text(text, window, cx);
}

/// A key or JSON picked from disk; only its file name is ever shown.
struct PickedFile {
    name: String,
    contents: Arc<str>,
}

pub struct AccountsView {
    focus_handle: FocusHandle,
    accounts: Entity<AccountsStore>,
    store: Entity<ReleasesStore>,
    expo_token: Entity<InputField>,
    expo_expires: Entity<InputField>,
    issuer_id: Entity<InputField>,
    key_id: Entity<InputField>,
    p8_file: Option<PickedFile>,
    service_account_file: Option<PickedFile>,
    bundle_id: Entity<InputField>,
    package: Entity<InputField>,
    eas_project_id: Entity<InputField>,
    errors: HashMap<AccountKind, SharedString>,
    link_error: Option<SharedString>,
    prefilled_link: bool,
    pending_tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl AccountsView {
    fn new(store: Entity<ReleasesStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let accounts = store.read(cx).accounts().clone();
        let input = |placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputField::new(window, cx, placeholder))
        };
        let expo_token = cx.new(|cx| {
            InputField::new(
                window,
                cx,
                &t!("app_releases.accounts.expo_token_placeholder"),
            )
            .start_icon(IconName::Lock)
            .masked(true)
        });
        let subscriptions = vec![
            cx.observe(&accounts, |_, _, cx| cx.notify()),
            cx.observe_in(&store, window, |this: &mut Self, _, window, cx| {
                this.prefill_link(window, cx);
                cx.notify();
            }),
        ];
        let mut view = Self {
            focus_handle: cx.focus_handle(),
            accounts,
            store,
            expo_token,
            expo_expires: input("AAAA-MM-DD", window, cx),
            issuer_id: input("57246542-96fe-1a63-e053-0824d011072a", window, cx),
            key_id: input("2X9R4HXF34", window, cx),
            p8_file: None,
            service_account_file: None,
            bundle_id: input("com.empresa.app", window, cx),
            package: input("com.empresa.app", window, cx),
            eas_project_id: input("00000000-0000-0000-0000-000000000000", window, cx),
            errors: HashMap::default(),
            link_error: None,
            prefilled_link: false,
            pending_tasks: Vec::new(),
            _subscriptions: subscriptions,
        };
        view.prefill(window, cx);
        view
    }

    fn prefill(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(app_store) = self
            .accounts
            .read(cx)
            .accounts()
            .and_then(|accounts| accounts.app_store.clone())
        {
            set_field_text(&self.issuer_id, &app_store.issuer_id, window, cx);
            set_field_text(&self.key_id, &app_store.key_id, window, cx);
        }
        if let Some(expires_on) = self
            .accounts
            .read(cx)
            .accounts()
            .and_then(|accounts| accounts.expo.as_ref()?.expires_on)
        {
            set_field_text(
                &self.expo_expires,
                &expires_on.format("%Y-%m-%d").to_string(),
                window,
                cx,
            );
        }
        self.prefill_link(window, cx);
    }

    /// The link fields start from what the project's config says, once detection is done.
    fn prefill_link(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.prefilled_link {
            return;
        }
        let Some(app) = self.store.read(cx).selected_app() else {
            return;
        };
        let link = app.link.clone();
        self.prefilled_link = true;
        let fields = [
            (&self.bundle_id, link.bundle_id),
            (&self.package, link.package),
            (&self.eas_project_id, link.eas_project_id),
        ];
        for (field, value) in fields {
            if let Some(value) = value {
                set_field_text(field, &value, window, cx);
            }
        }
    }

    fn set_error(
        &mut self,
        kind: AccountKind,
        error: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        match error {
            Some(error) => self.errors.insert(kind, error),
            None => self.errors.remove(&kind),
        };
        cx.notify();
    }

    fn save_expo(&mut self, cx: &mut Context<Self>) {
        let token = self.expo_token.read(cx).text(cx).trim().to_string();
        if token.is_empty() {
            self.set_error(
                AccountKind::Expo,
                Some(t!("app_releases.accounts.expo_token_missing")),
                cx,
            );
            return;
        }
        let expires_text = self.expo_expires.read(cx).text(cx).trim().to_string();
        let expires_on = if expires_text.is_empty() {
            None
        } else {
            match NaiveDate::parse_from_str(&expires_text, "%Y-%m-%d") {
                Ok(date) => Some(date),
                Err(_) => {
                    self.set_error(
                        AccountKind::Expo,
                        Some(t!("app_releases.accounts.invalid_date")),
                        cx,
                    );
                    return;
                }
            }
        };
        self.set_error(AccountKind::Expo, None, cx);
        self.submit(
            AccountToSave::Expo(ExpoAccount {
                token: token.into(),
                expires_on,
            }),
            cx,
        );
    }

    fn save_app_store(&mut self, cx: &mut Context<Self>) {
        let issuer_id = self.issuer_id.read(cx).text(cx).trim().to_string();
        let key_id = self.key_id.read(cx).text(cx).trim().to_string();
        let saved_key = self.accounts.read(cx).accounts().and_then(|accounts| {
            accounts
                .app_store
                .as_ref()
                .map(|account| account.private_key.clone())
        });
        let private_key = self
            .p8_file
            .as_ref()
            .map(|file| file.contents.clone())
            .or(saved_key);
        let (Some(private_key), false, false) =
            (private_key, issuer_id.is_empty(), key_id.is_empty())
        else {
            self.set_error(
                AccountKind::AppStore,
                Some(t!("app_releases.accounts.app_store_missing")),
                cx,
            );
            return;
        };
        if let Err(error) = app_store_connect::signing_key(&private_key) {
            self.set_error(AccountKind::AppStore, Some(error.to_string().into()), cx);
            return;
        }
        self.set_error(AccountKind::AppStore, None, cx);
        self.submit(
            AccountToSave::AppStore(AppStoreAccount {
                issuer_id,
                key_id,
                private_key,
            }),
            cx,
        );
    }

    fn save_google_play(&mut self, cx: &mut Context<Self>) {
        let Some(file) = self.service_account_file.as_ref() else {
            self.set_error(
                AccountKind::GooglePlay,
                Some(t!("app_releases.accounts.service_account_missing")),
                cx,
            );
            return;
        };
        let json = file.contents.clone();
        match ServiceAccount::parse(&json) {
            Ok(service_account) => {
                self.set_error(AccountKind::GooglePlay, None, cx);
                self.submit(
                    AccountToSave::GooglePlay(GooglePlayAccount {
                        service_account,
                        json,
                    }),
                    cx,
                );
            }
            Err(error) => {
                self.set_error(AccountKind::GooglePlay, Some(error.to_string().into()), cx)
            }
        }
    }

    fn submit(&mut self, account: AccountToSave, cx: &mut Context<Self>) {
        let task = self
            .accounts
            .update(cx, |accounts, cx| accounts.test_and_save(account, cx));
        self.pending_tasks.push(task);
    }

    fn pick_file(&mut self, kind: AccountKind, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(match kind {
                AccountKind::AppStore => t!("app_releases.accounts.pick_p8"),
                _ => t!("app_releases.accounts.pick_service_account"),
            }),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Some(path): Option<PathBuf> =
                paths.await??.and_then(|paths| paths.into_iter().next())
            else {
                return anyhow::Ok(());
            };
            let contents = cx
                .background_spawn({
                    let path = path.clone();
                    async move { std::fs::read_to_string(&path) }
                })
                .await
                .with_context(|| format!("Lançamentos: falha ao ler {}", path.display()))?;
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            this.update_in(cx, |this, window, cx| {
                let file = PickedFile {
                    name: name.clone(),
                    contents: contents.into(),
                };
                match kind {
                    AccountKind::AppStore => {
                        // Apple names the download AuthKey_<KEY ID>.p8.
                        if let Some(key_id) = name
                            .strip_prefix("AuthKey_")
                            .and_then(|rest| rest.strip_suffix(".p8"))
                            && this.key_id.read(cx).is_empty(cx)
                        {
                            set_field_text(&this.key_id, key_id, window, cx);
                        }
                        this.p8_file = Some(file);
                    }
                    _ => this.service_account_file = Some(file),
                }
                cx.notify();
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn save_link(&mut self, cx: &mut Context<Self>) {
        let value = |field: &Entity<InputField>, cx: &App| {
            let text = field.read(cx).text(cx).trim().to_string();
            (!text.is_empty()).then_some(text)
        };
        let store = self.store.read(cx);
        let Some(folder) = store
            .selected_app()
            .map(|app| app.folder.clone())
            .or_else(|| store.first_root())
        else {
            self.link_error = Some(t!("app_releases.accounts.no_folder"));
            cx.notify();
            return;
        };
        let detected = store
            .selected_app()
            .map(|app| app.detected.clone())
            .unwrap_or_default();
        let typed = AppLink {
            name: None,
            bundle_id: value(&self.bundle_id, cx),
            package: value(&self.package, cx),
            eas_project_id: value(&self.eas_project_id, cx),
        };
        // Only what differs from the config is kept, so editing app.json later still counts.
        let overrides = AppLink {
            name: None,
            bundle_id: typed
                .bundle_id
                .filter(|id| Some(id) != detected.bundle_id.as_ref()),
            package: typed
                .package
                .filter(|id| Some(id) != detected.package.as_ref()),
            eas_project_id: typed
                .eas_project_id
                .filter(|id| Some(id) != detected.eas_project_id.as_ref()),
        };
        self.link_error = None;
        let key_value_store = KeyValueStore::global(cx);
        let folder_key = folder.to_string_lossy().to_string();
        let task = cx.spawn(async move |this, cx| {
            let result = save_link_override(key_value_store, folder_key, overrides).await;
            this.update(cx, |this, cx| match result {
                Ok(()) => {
                    this.store.update(cx, |store, cx| store.detect(cx));
                }
                Err(error) => {
                    this.link_error = Some(format!("{error:#}").into());
                    cx.notify();
                }
            })
            .log_err();
        });
        self.pending_tasks.push(task);
    }

    fn render_status(&self, kind: AccountKind, cx: &App) -> AnyElement {
        let accounts = self.accounts.read(cx);
        let saved = accounts.accounts().is_some_and(|stored| stored.has(kind));
        let (label, tone) = match accounts.check(kind) {
            Some(CheckState::Checking) => (t!("app_releases.accounts.checking"), StatusTone::Info),
            Some(CheckState::Passed(_)) => {
                (t!("app_releases.accounts.connected"), StatusTone::Success)
            }
            Some(CheckState::Refused) => (t!("app_releases.accounts.refused"), StatusTone::Error),
            Some(CheckState::Failed(_)) => {
                (t!("app_releases.accounts.unreachable"), StatusTone::Warning)
            }
            None if saved => (t!("app_releases.accounts.saved"), StatusTone::Neutral),
            None => (t!("app_releases.accounts.not_set"), StatusTone::Neutral),
        };
        status_pill(label, tone, cx).into_any_element()
    }

    fn render_check_detail(&self, kind: AccountKind, cx: &App) -> Option<AnyElement> {
        let error = self.errors.get(&kind).cloned();
        if let Some(error) = error {
            return Some(
                error_callout(t!("app_releases.accounts.check_failed"), error, cx)
                    .into_any_element(),
            );
        }
        match self.accounts.read(cx).check(kind)? {
            CheckState::Passed(detail) => Some(
                Label::new(detail.clone())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .into_any_element(),
            ),
            CheckState::Refused => Some(
                error_callout(
                    t!(
                        "app_releases.accounts.refused_title",
                        service = kind.label()
                    ),
                    t!("app_releases.accounts.refused_detail"),
                    cx,
                )
                .into_any_element(),
            ),
            CheckState::Failed(message) => Some(
                error_callout(
                    t!("app_releases.accounts.check_failed"),
                    message.clone(),
                    cx,
                )
                .into_any_element(),
            ),
            CheckState::Checking => None,
        }
    }

    fn render_section(
        &self,
        kind: AccountKind,
        description: SharedString,
        fields: Vec<AnyElement>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let saved = self
            .accounts
            .read(cx)
            .accounts()
            .is_some_and(|stored| stored.has(kind));
        let checking = matches!(
            self.accounts.read(cx).check(kind),
            Some(CheckState::Checking)
        );
        let accounts = self.accounts.clone();
        let save_button = primary_button(
            ("accounts-save", kind as usize),
            None,
            t!("app_releases.accounts.test_and_save"),
            None,
            cx,
        )
        .when(!checking, |this| {
            this.on_click(cx.listener(move |this, _, _window, cx| match kind {
                AccountKind::Expo => this.save_expo(cx),
                AccountKind::AppStore => this.save_app_store(cx),
                AccountKind::GooglePlay => this.save_google_play(cx),
            }))
        });
        card(cx)
            .p_4()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Label::new(kind.label())
                            .size(LabelSize::Default)
                            .weight(FontWeight::SEMIBOLD),
                    )
                    .child(self.render_status(kind, cx)),
            )
            .child(
                Label::new(description)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .children(fields)
            .children(self.render_check_detail(kind, cx))
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .when(saved, |this| {
                        let test_accounts = accounts.clone();
                        let forget_accounts = accounts.clone();
                        this.child(
                            Button::new(
                                ("accounts-forget", kind as usize),
                                t!("app_releases.accounts.forget"),
                            )
                            .style(ButtonStyle::Subtle)
                            .label_size(LabelSize::Small)
                            .on_click(move |_, _window, cx| {
                                forget_accounts
                                    .update(cx, |accounts, cx| accounts.forget(kind, cx))
                                    .detach_and_log_err(cx);
                            }),
                        )
                        .child(
                            secondary_button(
                                ("accounts-test", kind as usize),
                                None,
                                t!("app_releases.accounts.test"),
                                !checking,
                                cx,
                            )
                            .on_click(move |_, _window, cx| {
                                test_accounts
                                    .update(cx, |accounts, cx| accounts.test_saved(kind, cx));
                            }),
                        )
                    })
                    .child(save_button),
            )
            .into_any_element()
    }

    fn field(label: SharedString, input: AnyElement) -> AnyElement {
        v_flex()
            .gap_1()
            .child(section_label(label))
            .child(input)
            .into_any_element()
    }

    fn file_field(
        &self,
        kind: AccountKind,
        label: SharedString,
        picked: Option<&PickedFile>,
        saved_hint: Option<SharedString>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let name = picked
            .map(|file| SharedString::from(file.name.clone()))
            .or(saved_hint)
            .unwrap_or_else(|| t!("app_releases.accounts.no_file"));
        Self::field(
            label,
            h_flex()
                .gap_2()
                .child(
                    secondary_button(
                        ("accounts-pick", kind as usize),
                        Some((IconName::FolderOpen, Color::Muted)),
                        t!("app_releases.accounts.choose_file"),
                        true,
                        cx,
                    )
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.pick_file(kind, window, cx)),
                    ),
                )
                .child(
                    Label::new(name)
                        .size(LabelSize::Small)
                        .buffer_font(cx)
                        .color(Color::Muted)
                        .truncate(),
                )
                .into_any_element(),
        )
    }

    fn render_link(&self, cx: &Context<Self>) -> AnyElement {
        let store = self.store.read(cx);
        let folder = store
            .selected_app()
            .map(|app| app.folder.clone())
            .or_else(|| store.first_root());
        let detected = store
            .selected_app()
            .is_some_and(|app| app.detected.is_linked());
        card(cx)
            .p_4()
            .gap_3()
            .child(
                Label::new(t!("app_releases.accounts.link_title"))
                    .size(LabelSize::Default)
                    .weight(FontWeight::SEMIBOLD),
            )
            .child(
                Label::new(if detected {
                    t!("app_releases.accounts.link_detected")
                } else {
                    t!("app_releases.accounts.link_not_detected")
                })
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .children(folder.map(|folder| {
                Label::new(folder.to_string_lossy().to_string())
                    .size(LabelSize::XSmall)
                    .buffer_font(cx)
                    .color(Color::Placeholder)
            }))
            .child(Self::field(
                t!("app_releases.accounts.bundle_id"),
                self.bundle_id.clone().into_any_element(),
            ))
            .child(Self::field(
                t!("app_releases.accounts.package"),
                self.package.clone().into_any_element(),
            ))
            .child(Self::field(
                t!("app_releases.accounts.eas_project_id"),
                self.eas_project_id.clone().into_any_element(),
            ))
            .children(
                self.link_error
                    .clone()
                    .map(|error| error_callout(t!("app_releases.accounts.link_failed"), error, cx)),
            )
            .child(
                h_flex().justify_end().child(
                    primary_button(
                        "accounts-save-link",
                        None,
                        t!("app_releases.accounts.save_link"),
                        None,
                        cx,
                    )
                    .on_click(cx.listener(|this, _, _window, cx| this.save_link(cx))),
                ),
            )
            .into_any_element()
    }
}

impl Render for AccountsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state_note = match self.accounts.read(cx).state() {
            AccountsState::Failed(message) => Some(
                error_callout(t!("app_releases.dock.accounts_failed"), message.clone(), cx)
                    .into_any_element(),
            ),
            _ => None,
        };
        let saved_p8 = self
            .accounts
            .read(cx)
            .accounts()
            .and_then(|accounts| accounts.app_store.as_ref())
            .map(|_| t!("app_releases.accounts.saved_in_keychain"));
        let saved_service_account = self
            .accounts
            .read(cx)
            .accounts()
            .and_then(|accounts| accounts.google_play.as_ref())
            .map(|account| SharedString::from(account.service_account.client_email.clone()));

        let expo = self.render_section(
            AccountKind::Expo,
            t!("app_releases.accounts.expo_description"),
            vec![
                Self::field(
                    t!("app_releases.accounts.expo_token"),
                    self.expo_token.clone().into_any_element(),
                ),
                Self::field(
                    t!("app_releases.accounts.expo_expires"),
                    self.expo_expires.clone().into_any_element(),
                ),
            ],
            cx,
        );
        let app_store = self.render_section(
            AccountKind::AppStore,
            t!("app_releases.accounts.app_store_description"),
            vec![
                Self::field(
                    t!("app_releases.accounts.issuer_id"),
                    self.issuer_id.clone().into_any_element(),
                ),
                Self::field(
                    t!("app_releases.accounts.key_id"),
                    self.key_id.clone().into_any_element(),
                ),
                self.file_field(
                    AccountKind::AppStore,
                    t!("app_releases.accounts.p8"),
                    self.p8_file.as_ref(),
                    saved_p8,
                    cx,
                ),
            ],
            cx,
        );
        let google_play = self.render_section(
            AccountKind::GooglePlay,
            t!("app_releases.accounts.google_play_description"),
            vec![self.file_field(
                AccountKind::GooglePlay,
                t!("app_releases.accounts.service_account"),
                self.service_account_file.as_ref(),
                saved_service_account,
                cx,
            )],
            cx,
        );

        v_flex()
            .key_context("ReleaseAccounts")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(
                v_flex()
                    .id("release-accounts")
                    .size_full()
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .w_full()
                            .max_w(px(640.))
                            .mx_auto()
                            .p_6()
                            .gap_4()
                            .child(
                                v_flex()
                                    .gap_1()
                                    .child(
                                        Label::new(t!("app_releases.accounts.title"))
                                            .size(LabelSize::Large)
                                            .weight(FontWeight::SEMIBOLD),
                                    )
                                    .child(
                                        Label::new(t!("app_releases.accounts.subtitle"))
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    ),
                            )
                            .children(state_note)
                            .child(expo)
                            .child(app_store)
                            .child(google_play)
                            .child(self.render_link(cx)),
                    ),
            )
    }
}

impl Focusable for AccountsView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for AccountsView {}

impl WorkspaceItem for AccountsView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        t!("app_releases.accounts.title")
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Settings))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}
