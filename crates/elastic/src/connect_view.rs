use crate::{
    SaveConnection,
    client::{Elastic, ElasticError},
    connection::{self, AuthKind, Environment, SavedConnection, Scope, Via},
    panel::ElasticPanel,
};
use gpui::{
    AnyElement, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Subscription, Task,
    WeakEntity, px,
};
use http_client::HttpClient;
use std::{sync::Arc, time::Instant};
use ui::{
    TintColor, ToggleButtonGroup, ToggleButtonGroupSize, ToggleButtonGroupStyle,
    ToggleButtonSimple, prelude::*,
};
use ui_input::{ErasedEditorEvent, InputField};
use util::ResultExt as _;
use workspace::{
    Workspace,
    item::{Item, ItemEvent, TabContentParams},
};

pub enum Prefill {
    New,
    Edit(SavedConnection, Scope),
}

/// One step of "Testar conexão", as the form lists them.
struct Check {
    passed: bool,
    /// Missing, but nothing depends on it: shown as a note, not a failure.
    warning: bool,
    text: String,
}

enum TestState {
    Idle,
    Running,
    Done(Vec<Check>),
}

pub struct ConnectView {
    panel: WeakEntity<ElasticPanel>,
    http: Arc<dyn HttpClient>,
    editing: Option<(String, Scope)>,
    focus_handle: FocusHandle,
    name_input: Entity<InputField>,
    url_input: Entity<InputField>,
    username_input: Entity<InputField>,
    password_input: Entity<InputField>,
    api_key_input: Entity<InputField>,
    via: Via,
    auth: AuthKind,
    environment: Environment,
    scope: Scope,
    test: TestState,
    test_task: Task<()>,
    save_task: Option<Task<()>>,
    save_error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl ConnectView {
    fn new(
        panel: WeakEntity<ElasticPanel>,
        prefill: Prefill,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input =
            |placeholder: &str, label: &str, window: &mut Window, cx: &mut Context<Self>| {
                cx.new(|cx| InputField::new(window, cx, placeholder).label(label.to_owned()))
            };
        let name_input = input("trix-logs", "Nome", window, cx);
        let url_input = cx.new(|cx| {
            InputField::new(window, cx, "https://kibana.suaempresa.com")
                .label("URL")
                .start_icon(IconName::Link)
        });
        let username_input = input("o mesmo do login no Kibana", "Usuário", window, cx);
        // `masked(false)` still shows the reveal toggle, so only the secrets call it.
        let password_input = cx.new(|cx| {
            InputField::new(window, cx, "fica no Keychain do perfil")
                .label("Senha")
                .masked(true)
        });
        let api_key_input = cx.new(|cx| {
            InputField::new(window, cx, "a chave encoded, fica no Keychain do perfil")
                .label("API key")
                .masked(true)
        });

        let mut subscriptions = Vec::new();
        for field in [&url_input, &username_input, &password_input, &api_key_input] {
            let editor = field.read(cx).editor().clone();
            let this = cx.weak_entity();
            subscriptions.push(editor.subscribe(
                Box::new(move |event, _window, cx| {
                    if event == ErasedEditorEvent::BufferEdited {
                        this.update(cx, |this, cx| {
                            this.test = TestState::Idle;
                            cx.notify();
                        })
                        .log_err();
                    }
                }),
                window,
                cx,
            ));
        }

        let mut this = Self {
            panel,
            http: cx.http_client(),
            editing: None,
            focus_handle: cx.focus_handle(),
            name_input,
            url_input,
            username_input,
            password_input,
            api_key_input,
            via: Via::Kibana,
            auth: AuthKind::Password,
            environment: Environment::Prod,
            scope: Scope::Profile,
            test: TestState::Idle,
            test_task: Task::ready(()),
            save_task: None,
            save_error: None,
            _subscriptions: subscriptions,
        };
        this.apply_prefill(prefill, window, cx);
        this
    }

    fn apply_prefill(&mut self, prefill: Prefill, window: &mut Window, cx: &mut Context<Self>) {
        match prefill {
            Prefill::New => {}
            Prefill::Edit(connection, scope) => {
                set_input(&self.name_input, &connection.name, window, cx);
                set_input(&self.url_input, &connection.url, window, cx);
                set_input(&self.username_input, &connection.username, window, cx);
                self.via = connection.via;
                self.auth = connection.auth;
                self.environment = connection.environment;
                self.scope = scope;
                self.editing = Some((connection.id, scope));
            }
        }
    }

    fn form(&self, cx: &App) -> Result<(SavedConnection, String), SharedString> {
        let text = |input: &Entity<InputField>| input.read(cx).text(cx).trim().to_owned();
        let url = text(&self.url_input);
        if url.is_empty() {
            return Err("Preencha a URL.".into());
        }
        let url = if url.contains("://") {
            url
        } else {
            format!("https://{url}")
        };
        let username = text(&self.username_input);
        if self.auth == AuthKind::Password && username.is_empty() {
            return Err("Preencha o usuário.".into());
        }
        let host = http_client::Url::parse(&url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or_default();
        let name = Some(text(&self.name_input))
            .filter(|name| !name.is_empty())
            .unwrap_or(host);
        let id = self
            .editing
            .as_ref()
            .map(|(id, _)| id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let connection = SavedConnection {
            id,
            name,
            environment: self.environment,
            via: self.via,
            url,
            auth: self.auth,
            username: if self.auth == AuthKind::Password {
                username
            } else {
                String::new()
            },
        };
        connection
            .endpoint()
            .map_err(|error| SharedString::from(format!("{error:#}")))?;
        let secret = match self.auth {
            AuthKind::Password => self.password_input.read(cx).text(cx),
            AuthKind::ApiKey => self.api_key_input.read(cx).text(cx).trim().to_owned(),
            AuthKind::None => String::new(),
        };
        Ok((connection, secret))
    }

    fn test_connection(&mut self, cx: &mut Context<Self>) {
        let (connection, secret) = match self.form(cx) {
            Ok(form) => form,
            Err(error) => {
                self.test = TestState::Done(vec![Check {
                    passed: false,
                    warning: false,
                    text: error.to_string(),
                }]);
                cx.notify();
                return;
            }
        };
        self.test = TestState::Running;
        cx.notify();
        let http = self.http.clone();
        let editing = self.editing.is_some();
        self.test_task = cx.spawn(async move |this, cx| {
            let checks = run_checks(connection, secret, editing, http).await;
            this.update(cx, |this, cx| {
                this.test = TestState::Done(checks);
                cx.notify();
            })
            .log_err();
        });
    }

    fn save(&mut self, _: &SaveConnection, _window: &mut Window, cx: &mut Context<Self>) {
        if self.save_task.is_some() {
            return;
        }
        let (connection, secret) = match self.form(cx) {
            Ok(form) => form,
            Err(error) => {
                self.save_error = Some(error);
                cx.notify();
                return;
            }
        };
        let Some(panel) = self.panel.upgrade() else {
            self.save_error = Some("O dock Elastic foi fechado.".into());
            cx.notify();
            return;
        };
        let scope = self.scope;
        let previous = self.editing.clone();
        let task = panel.update(cx, |panel, cx| {
            panel.save_connection(connection, scope, secret, previous, cx)
        });
        self.save_error = None;
        self.save_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.save_task = None;
                match result {
                    Ok(()) => cx.emit(ItemEvent::CloseItem),
                    Err(error) => {
                        this.save_error = Some(format!("{error:#}").into());
                        cx.notify();
                    }
                }
            })
            .log_err();
        }));
        cx.notify();
    }

    fn set_auth(&mut self, auth: AuthKind, cx: &mut Context<Self>) {
        self.auth = auth;
        self.test = TestState::Idle;
        cx.notify();
    }

    fn render_test_result(&self, cx: &App) -> Option<AnyElement> {
        let checks = match &self.test {
            TestState::Idle => return None,
            TestState::Running => {
                return Some(
                    h_flex()
                        .gap_2()
                        .child(
                            Icon::new(IconName::LoadCircle)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new("Testando…")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .into_any_element(),
                );
            }
            TestState::Done(checks) => checks,
        };
        let failed = checks.iter().any(|check| !check.passed && !check.warning);
        let warned = checks.iter().any(|check| check.warning);
        let color = if failed {
            Color::Error
        } else if warned {
            Color::Warning
        } else {
            Color::Success
        };
        Some(
            v_flex()
                .p_2p5()
                .gap_1()
                .rounded_md()
                .bg(color.color(cx).opacity(0.06))
                .border_1()
                .border_color(color.color(cx).opacity(0.3))
                .child(
                    h_flex()
                        .gap_1p5()
                        .pb_1()
                        .child(
                            Icon::new(IconName::Terminal)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            Label::new("Testar conexão")
                                .size(LabelSize::Small)
                                .weight(FontWeight::SEMIBOLD),
                        ),
                )
                .children(checks.iter().map(|check| {
                    h_flex()
                        .gap_1p5()
                        .items_start()
                        .child(
                            Label::new(if check.passed {
                                "✓"
                            } else if check.warning {
                                "!"
                            } else {
                                "✗"
                            })
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .color(if check.passed {
                                Color::Success
                            } else if check.warning {
                                Color::Warning
                            } else {
                                Color::Error
                            }),
                        )
                        .child(
                            div().flex_1().min_w_0().child(
                                Label::new(check.text.clone())
                                    .size(LabelSize::Small)
                                    .buffer_font(cx)
                                    .color(if check.passed || check.warning {
                                        Color::Muted
                                    } else {
                                        Color::Error
                                    }),
                            ),
                        )
                }))
                .when(failed && checks.len() > 2, |this| {
                    this.child(
                        Label::new(
                            "Dá para salvar assim: o dock e o Agent escondem os índices sem \
                             permissão e dizem qual privilégio pedir.",
                        )
                        .size(LabelSize::XSmall)
                        .color(Color::Accent),
                    )
                })
                .into_any_element(),
        )
    }
}

async fn run_checks(
    connection: SavedConnection,
    secret: String,
    editing: bool,
    http: Arc<dyn HttpClient>,
) -> Vec<Check> {
    let mut checks = Vec::new();
    let endpoint = match connection.endpoint() {
        Ok(endpoint) => endpoint,
        Err(error) => {
            checks.push(Check {
                passed: false,
                warning: false,
                text: format!("{error:#}"),
            });
            return checks;
        }
    };
    if secret.is_empty() && editing && connection.auth != AuthKind::None {
        checks.push(Check {
            passed: true,
            warning: false,
            text: "senha em branco: o teste usa a que já está no keychain só depois de salvar"
                .to_string(),
        });
    }
    let elastic = Elastic::new(http, endpoint, connection.auth_with(Some(secret)));
    let started = Instant::now();
    match elastic.kibana_version().await {
        Ok(Some(version)) => checks.push(Check {
            passed: true,
            warning: false,
            text: format!(
                "GET /api/status · Kibana {version} · {} ms",
                started.elapsed().as_millis()
            ),
        }),
        Ok(None) => {}
        Err(error) => {
            checks.push(Check {
                passed: false,
                warning: false,
                text: format!("GET /api/status · {error}"),
            });
            if matches!(error, ElasticError::Unauthorized { .. }) {
                return checks;
            }
        }
    }
    let mut without_monitor = false;
    match elastic.info().await {
        Ok(info) => checks.push(Check {
            passed: true,
            warning: false,
            text: format!(
                "GET / · cluster {} · Elasticsearch {}",
                info.cluster_name, info.version.number
            ),
        }),
        // Without `monitor` everything else still works; the version comes from Kibana.
        Err(ElasticError::Forbidden { .. }) => {
            without_monitor = true;
            checks.push(Check {
                passed: false,
                warning: true,
                text: "GET / · sem o privilégio monitor: a versão vem do Kibana e o dock não \
                       mostra o tamanho dos data streams"
                    .to_string(),
            })
        }
        Err(error) => {
            checks.push(Check {
                passed: false,
                warning: false,
                text: format!("GET / · {error}"),
            });
            return checks;
        }
    }
    match elastic.current_user().await {
        Ok(user) => checks.push(Check {
            passed: true,
            warning: false,
            text: format!(
                "_security/_authenticate · {} · realm {} · roles {}",
                user.username,
                user.authentication_realm.name,
                if user.roles.is_empty() {
                    "nenhuma".to_string()
                } else {
                    user.roles.join(", ")
                }
            ),
        }),
        Err(error) => checks.push(Check {
            passed: false,
            warning: false,
            text: format!("_security/_authenticate · {error}"),
        }),
    }
    match elastic
        .missing_privileges(
            &["monitor"],
            &["logs-*", "traces-*", "metrics-*"],
            &["read", "view_index_metadata"],
        )
        .await
    {
        Ok(missing) if missing.is_empty() => checks.push(Check {
            passed: true,
            warning: false,
            text: "privilégios: read em logs-*, traces-*, metrics-* e monitor no cluster"
                .to_string(),
        }),
        Ok(missing) => {
            for privilege in &missing.cluster {
                if privilege == "monitor" && without_monitor {
                    continue;
                }
                checks.push(Check {
                    passed: false,
                    warning: privilege == "monitor",
                    text: format!("cluster · sem {privilege}"),
                });
            }
            for (pattern, privileges) in &missing.index {
                checks.push(Check {
                    passed: false,
                    warning: false,
                    text: format!("{pattern} · sem {}", privileges.join(", ")),
                });
            }
        }
        Err(error) => checks.push(Check {
            passed: false,
            warning: false,
            text: format!("_has_privileges · {error}"),
        }),
    }
    match elastic
        .esql("FROM logs-* | STATS docs = COUNT(*)", None)
        .await
    {
        Ok(result) => checks.push(Check {
            passed: true,
            warning: false,
            text: format!(
                "ES|QL em logs-* · {} docs{}",
                result
                    .values
                    .first()
                    .and_then(|row| row.first())
                    .map(|count| match count.as_u64() {
                        Some(count) => crate::results::format_count(count),
                        None => crate::results::display(count),
                    })
                    .unwrap_or_default(),
                result
                    .took
                    .map(|took| format!(" · {took} ms"))
                    .unwrap_or_default()
            ),
        }),
        Err(error) => checks.push(Check {
            passed: false,
            warning: false,
            text: format!("ES|QL em logs-* · {error}"),
        }),
    }
    checks
}

fn set_input(input: &Entity<InputField>, text: &str, window: &mut Window, cx: &mut App) {
    input.update(cx, |input, cx| input.set_text(text, window, cx));
}

fn field_label(text: &'static str) -> impl IntoElement {
    Label::new(text).size(LabelSize::Small).color(Color::Muted)
}

impl Render for ConnectView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let saving = self.save_task.is_some();
        let title = if self.editing.is_some() {
            "Editar conexão"
        } else {
            "Nova conexão"
        };
        let environment_index = Environment::ALL
            .iter()
            .position(|environment| *environment == self.environment)
            .unwrap_or(0);
        let environment_buttons = Environment::ALL.map(|environment| {
            ToggleButtonSimple::new(
                environment.label(),
                cx.listener(move |this, _, _, cx| {
                    this.environment = environment;
                    cx.notify();
                }),
            )
        });
        let via_buttons = [
            ToggleButtonSimple::new(
                "Pela URL do Kibana",
                cx.listener(|this, _, _, cx| {
                    this.via = Via::Kibana;
                    this.test = TestState::Idle;
                    cx.notify();
                }),
            ),
            ToggleButtonSimple::new(
                "Direto no Elasticsearch",
                cx.listener(|this, _, _, cx| {
                    this.via = Via::Direct;
                    this.test = TestState::Idle;
                    cx.notify();
                }),
            ),
        ];
        let auth_buttons = [
            ToggleButtonSimple::new(
                "Usuário e senha",
                cx.listener(|this, _, _, cx| this.set_auth(AuthKind::Password, cx)),
            ),
            ToggleButtonSimple::new(
                "API key",
                cx.listener(|this, _, _, cx| this.set_auth(AuthKind::ApiKey, cx)),
            ),
            ToggleButtonSimple::new(
                "Sem autenticação",
                cx.listener(|this, _, _, cx| this.set_auth(AuthKind::None, cx)),
            ),
        ];
        let scope_buttons = [
            ToggleButtonSimple::new(
                "Só para mim (perfil)",
                cx.listener(|this, _, _, cx| {
                    this.scope = Scope::Profile;
                    cx.notify();
                }),
            ),
            ToggleButtonSimple::new(
                format!("Projeto · {}", connection::PROJECT_FILE),
                cx.listener(|this, _, _, cx| {
                    this.scope = Scope::Project;
                    cx.notify();
                }),
            ),
        ];
        let via_hint = match self.via {
            Via::Kibana => {
                "A URL que você abre no navegador. As consultas passam pelo proxy do Dev Tools do \
                 Kibana, com o mesmo login."
            }
            Via::Direct => {
                "A porta 9200 do Elasticsearch. Quando só a VPC enxerga o cluster, use pelo Kibana \
                 ou uma VPN."
            }
        };

        v_flex()
            .key_context("ElasticConnectView")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::save))
            .id("elastic-connect-view")
            .size_full()
            .overflow_y_scroll()
            .bg(cx.theme().colors().editor_background)
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(680.))
                    .mx_auto()
                    .px_6()
                    .py_8()
                    .gap_4()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Headline::new(title).size(HeadlineSize::Small))
                            .child(
                                h_flex()
                                    .h(px(22.))
                                    .px_2()
                                    .gap_1()
                                    .rounded_md()
                                    .bg(Color::Accent.color(cx).opacity(0.12))
                                    .child(
                                        Icon::new(IconName::CloudPulse)
                                            .size(IconSize::XSmall)
                                            .color(Color::Accent),
                                    )
                                    .child(
                                        Label::new("Elasticsearch")
                                            .size(LabelSize::XSmall)
                                            .color(Color::Accent),
                                    ),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(field_label("Como chegar ao cluster"))
                            .child(
                                ToggleButtonGroup::single_row("elastic-via", via_buttons)
                                    .style(ToggleButtonGroupStyle::Outlined)
                                    .size(ToggleButtonGroupSize::Custom(rems_from_px(30_f32)))
                                    .label_size(LabelSize::Small)
                                    .auto_width()
                                    .selected_index(match self.via {
                                        Via::Kibana => 0,
                                        Via::Direct => 1,
                                    }),
                            )
                            .child(
                                Label::new(via_hint)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(self.url_input.clone())
                    .child(
                        h_flex()
                            .gap_4()
                            .items_end()
                            .child(div().flex_1().child(self.name_input.clone()))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .gap_1()
                                    .child(field_label("Ambiente"))
                                    .child(
                                        ToggleButtonGroup::single_row(
                                            "elastic-environment",
                                            environment_buttons,
                                        )
                                        .style(ToggleButtonGroupStyle::Outlined)
                                        .size(ToggleButtonGroupSize::Custom(rems_from_px(30_f32)))
                                        .label_size(LabelSize::Small)
                                        .selected_index(environment_index),
                                    ),
                            ),
                    )
                    .child(
                        v_flex().gap_1().child(field_label("Autenticação")).child(
                            ToggleButtonGroup::single_row("elastic-auth", auth_buttons)
                                .style(ToggleButtonGroupStyle::Outlined)
                                .size(ToggleButtonGroupSize::Custom(rems_from_px(30_f32)))
                                .label_size(LabelSize::Small)
                                .auto_width()
                                .selected_index(match self.auth {
                                    AuthKind::Password => 0,
                                    AuthKind::ApiKey => 1,
                                    AuthKind::None => 2,
                                }),
                        ),
                    )
                    .when(self.auth == AuthKind::Password, |this| {
                        this.child(self.username_input.clone())
                    })
                    .when(self.auth == AuthKind::Password, |this| {
                        this.child(self.password_input.clone())
                    })
                    .when(self.auth == AuthKind::ApiKey, |this| {
                        this.child(self.api_key_input.clone())
                    })
                    .child(
                        v_flex()
                            .gap_1()
                            .child(field_label("Salvar em"))
                            .child(
                                ToggleButtonGroup::single_row("elastic-scope", scope_buttons)
                                    .style(ToggleButtonGroupStyle::Outlined)
                                    .size(ToggleButtonGroupSize::Custom(rems_from_px(30_f32)))
                                    .label_size(LabelSize::Small)
                                    .auto_width()
                                    .selected_index(match self.scope {
                                        Scope::Profile => 0,
                                        Scope::Project => 1,
                                    }),
                            )
                            .child(
                                Label::new(
                                    "No projeto vão só a URL e o usuário. Senhas e API keys ficam \
                                     no Keychain de cada um.",
                                )
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                            ),
                    )
                    .children(self.render_test_result(cx))
                    .when_some(self.save_error.clone(), |this, error| {
                        this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
                    })
                    .child(
                        h_flex()
                            .pt_2()
                            .gap_2()
                            .child(
                                Button::new("elastic-test", "Testar conexão")
                                    .style(ButtonStyle::Outlined)
                                    .start_icon(
                                        Icon::new(IconName::SignalHigh).size(IconSize::Small),
                                    )
                                    .disabled(matches!(self.test, TestState::Running))
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.test_connection(cx)),
                                    ),
                            )
                            .child(div().flex_1())
                            .child(
                                Button::new("elastic-cancel", "Cancelar")
                                    .style(ButtonStyle::Subtle)
                                    .on_click(
                                        cx.listener(|_, _, _, cx| cx.emit(ItemEvent::CloseItem)),
                                    ),
                            )
                            .child(
                                Button::new(
                                    "elastic-save",
                                    if saving {
                                        "Salvando…"
                                    } else {
                                        "Salvar e conectar"
                                    },
                                )
                                .style(ButtonStyle::Tinted(TintColor::Accent))
                                .disabled(saving)
                                .key_binding(ui::KeyBinding::for_action_in(
                                    &SaveConnection,
                                    &self.focus_handle,
                                    cx,
                                ))
                                .on_click(cx.listener(
                                    |this, _, window, cx| this.save(&SaveConnection, window, cx),
                                )),
                            ),
                    ),
            )
    }
}

impl Focusable for ConnectView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for ConnectView {}

impl Item for ConnectView {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        if self.editing.is_some() {
            "Editar conexão".into()
        } else {
            "Nova conexão".into()
        }
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(0, cx))
            .color(params.text_color())
            .into_any_element()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::CloudPulse).color(Color::Muted))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }
}

/// Opens the form from outside a workspace update, as the dock does.
pub fn open(
    workspace: WeakEntity<Workspace>,
    panel: Entity<ElasticPanel>,
    prefill: Prefill,
    window: &mut Window,
    cx: &mut App,
) {
    workspace
        .update(cx, |workspace, cx| {
            open_in(workspace, panel, prefill, window, cx)
        })
        .log_err();
}

/// Opens the form, reusing an open one unless it edits a different connection.
pub fn open_in(
    workspace: &mut Workspace,
    panel: Entity<ElasticPanel>,
    prefill: Prefill,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let editing = match &prefill {
        Prefill::Edit(connection, _) => Some(connection.id.clone()),
        Prefill::New => None,
    };
    let existing = workspace
        .items_of_type::<ConnectView>(cx)
        .find(|view| view.read(cx).editing.as_ref().map(|(id, _)| id) == editing.as_ref());
    if let Some(existing) = existing {
        if !matches!(prefill, Prefill::New) {
            existing.update(cx, |view, cx| {
                view.apply_prefill(prefill, window, cx);
                cx.notify();
            });
        }
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let panel = panel.downgrade();
    let view = cx.new(|cx| ConnectView::new(panel, prefill, window, cx));
    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
}
