use crate::{
    SaveConnection, catalog,
    connection::{self, DEFAULT_PORT, Environment, SavedConnection, Scope, UrlFields},
    discovery::{Source, Suggestion},
    panel::{DatabasePanel, environment_color, segment},
    session::Session,
    tls::SslMode,
    tunnel::SshTunnel,
};
use gpui::{
    AnyElement, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Subscription, Task,
    WeakEntity, px,
};
use std::time::Instant;
use ui::{
    Switch, SwitchColor, ToggleState, prelude::*, primary_button, secondary_button, segmented_control,
};
use ui_input::{ErasedEditorEvent, InputField};
use util::ResultExt as _;
use workspace::{
    Workspace,
    item::{Item, ItemEvent, TabContentParams},
};

pub enum Prefill {
    New,
    Suggestion(Suggestion),
    Edit(SavedConnection, Scope),
}

enum TestState {
    Idle,
    Running,
    Passed(SharedString, Instant),
    Failed(SharedString),
}

pub struct ConnectView {
    panel: WeakEntity<DatabasePanel>,
    editing: Option<(String, Scope)>,
    focus_handle: FocusHandle,
    url_input: Entity<InputField>,
    name_input: Entity<InputField>,
    host_input: Entity<InputField>,
    port_input: Entity<InputField>,
    database_input: Entity<InputField>,
    user_input: Entity<InputField>,
    password_input: Entity<InputField>,
    ssh_enabled: bool,
    ssh_host_input: Entity<InputField>,
    ssh_port_input: Entity<InputField>,
    environment: Environment,
    ssl_mode: SslMode,
    read_only: bool,
    confirm_writes: bool,
    scope: Scope,
    url_status: Option<Result<String, SharedString>>,
    filling_from_url: bool,
    test: TestState,
    test_task: Task<()>,
    save_task: Option<Task<()>>,
    save_error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl ConnectView {
    fn new(
        panel: WeakEntity<DatabasePanel>,
        prefill: Prefill,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input =
            |placeholder: &str, label: &str, window: &mut Window, cx: &mut Context<Self>| {
                cx.new(|cx| InputField::new(window, cx, placeholder).label(label.to_owned()))
            };
        let url_input = cx.new(|cx| {
            InputField::new(
                window,
                cx,
                "postgres://usuário:senha@host:5432/banco?sslmode=prefer",
            )
            .label("Colar URL de conexão")
            .start_icon(IconName::Link)
        });
        let name_input = input("igual ao banco", "Nome", window, cx);
        let host_input = input("localhost", "Host", window, cx);
        let port_input = input("5432", "Porta", window, cx);
        let database_input = input("postgres", "Banco", window, cx);
        let user_input = input("postgres", "Usuário", window, cx);
        // `masked(false)` still shows the reveal toggle, so only the password calls it.
        let password_input = cx.new(|cx| {
            InputField::new(window, cx, "fica no Keychain do perfil")
                .label("Senha")
                .start_icon(IconName::Lock)
                .end_hint("Keychain do perfil · nunca no settings nem no repo")
                .masked(true)
        });

        let ssh_host_input = input("bastion ou usuário@bastion.exemplo.com", "Host SSH", window, cx);
        let ssh_port_input = input("22", "Porta SSH", window, cx);

        let mut subscriptions = Vec::new();
        {
            let editor = url_input.read(cx).editor().clone();
            let this = cx.weak_entity();
            subscriptions.push(editor.subscribe(
                Box::new(move |event, window, cx| {
                    if event == ErasedEditorEvent::BufferEdited {
                        this.update(cx, |this, cx| this.fill_from_url(window, cx))
                            .log_err();
                    }
                }),
                window,
                cx,
            ));
        }
        for field in [
            &host_input,
            &port_input,
            &database_input,
            &user_input,
            &password_input,
        ] {
            let editor = field.read(cx).editor().clone();
            let this = cx.weak_entity();
            subscriptions.push(editor.subscribe(
                Box::new(move |event, _window, cx| {
                    if event == ErasedEditorEvent::BufferEdited {
                        this.update(cx, |this, cx| {
                            if !this.filling_from_url {
                                this.test = TestState::Idle;
                                cx.notify();
                            }
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
            editing: None,
            focus_handle: cx.focus_handle(),
            url_input,
            name_input,
            host_input,
            port_input,
            database_input,
            user_input,
            password_input,
            ssh_enabled: false,
            ssh_host_input,
            ssh_port_input,
            environment: Environment::Local,
            ssl_mode: SslMode::Prefer,
            read_only: false,
            confirm_writes: true,
            scope: Scope::Profile,
            url_status: None,
            filling_from_url: false,
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
            Prefill::Suggestion(suggestion) => {
                let origin = match &suggestion.source {
                    Source::Env { file, .. } => format!("vindo do {file}"),
                    Source::Compose { file, .. } => format!("vindo do {file}"),
                };
                self.fill_fields(&suggestion.fields, window, cx);
                if suggestion.fields.user.is_none() {
                    set_input(&self.user_input, &suggestion.user, window, cx);
                }
                if suggestion.fields.database.is_none() {
                    set_input(&self.database_input, &suggestion.database, window, cx);
                }
                self.url_status = Some(Ok(origin));
                self.update_url_hint(cx);
                self.environment = Environment::Dev;
            }
            Prefill::Edit(connection, scope) => {
                set_input(&self.name_input, &connection.name, window, cx);
                set_input(&self.host_input, &connection.host, window, cx);
                set_input(&self.port_input, &connection.port.to_string(), window, cx);
                set_input(&self.database_input, &connection.database, window, cx);
                set_input(&self.user_input, &connection.user, window, cx);
                self.environment = connection.environment;
                self.ssl_mode = connection.ssl_mode;
                self.read_only = connection.read_only;
                self.confirm_writes = connection.confirm_writes;
                if let Some(ssh) = &connection.ssh {
                    self.ssh_enabled = true;
                    set_input(&self.ssh_host_input, &ssh.host, window, cx);
                    if let Some(port) = ssh.port {
                        set_input(&self.ssh_port_input, &port.to_string(), window, cx);
                    }
                }
                self.scope = scope;
                self.editing = Some((connection.id, scope));
            }
        }
    }

    fn fill_fields(&mut self, fields: &UrlFields, window: &mut Window, cx: &mut Context<Self>) {
        self.filling_from_url = true;
        let pairs = [
            (&self.host_input, fields.host.clone()),
            (&self.port_input, fields.port.map(|port| port.to_string())),
            (&self.database_input, fields.database.clone()),
            (&self.user_input, fields.user.clone()),
            (&self.password_input, fields.password.clone()),
        ];
        for (input, value) in pairs {
            if let Some(value) = value {
                set_input(&input, &value, window, cx);
            }
        }
        if let Some(ssl_mode) = fields.ssl_mode {
            self.ssl_mode = ssl_mode;
        }
        self.filling_from_url = false;
        self.test = TestState::Idle;
    }

    fn fill_from_url(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.url_input.read(cx).text(cx);
        let text = text.trim();
        if text.is_empty() {
            self.url_status = None;
        } else {
            match connection::parse_url(text) {
                Ok(fields) => {
                    let count = fields.count();
                    self.fill_fields(&fields, window, cx);
                    self.url_status = Some(Ok(format!(
                        "preencheu {count} {}",
                        if count == 1 { "campo" } else { "campos" }
                    )));
                }
                Err(error) => self.url_status = Some(Err(format!("{error:#}").into())),
            }
        }
        self.update_url_hint(cx);
        cx.notify();
    }

    fn set_environment(&mut self, environment: Environment, cx: &mut Context<Self>) {
        // Production opens read-only and always asks before writing, unless unticked here.
        if environment == Environment::Prod && self.environment != Environment::Prod {
            self.read_only = true;
            self.confirm_writes = true;
        }
        self.environment = environment;
        cx.notify();
    }

    fn form(&self, cx: &App) -> Result<(SavedConnection, String), SharedString> {
        let text = |input: &Entity<InputField>| input.read(cx).text(cx).trim().to_owned();
        let host = text(&self.host_input);
        let database = text(&self.database_input);
        let user = text(&self.user_input);
        let port = text(&self.port_input);
        if host.is_empty() {
            return Err("Preencha o host.".into());
        }
        if database.is_empty() {
            return Err("Preencha o banco.".into());
        }
        if user.is_empty() {
            return Err("Preencha o usuário.".into());
        }
        let port = if port.is_empty() {
            DEFAULT_PORT
        } else {
            port.parse::<u16>()
                .map_err(|_| SharedString::from(format!("Porta inválida: {port}")))?
        };
        let name = Some(text(&self.name_input))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| database.clone());
        let id = self
            .editing
            .as_ref()
            .map(|(id, _)| id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let password = self.password_input.read(cx).text(cx);
        let ssh = if self.ssh_enabled {
            let host = text(&self.ssh_host_input);
            if host.is_empty() {
                return Err("Preencha o host SSH ou desligue o túnel.".into());
            }
            let port = text(&self.ssh_port_input);
            let port = if port.is_empty() {
                None
            } else {
                Some(
                    port.parse::<u16>()
                        .map_err(|_| SharedString::from(format!("Porta SSH inválida: {port}")))?,
                )
            };
            Some(SshTunnel { host, port })
        } else {
            None
        };
        Ok((
            SavedConnection {
                id,
                name,
                environment: self.environment,
                host,
                port,
                database,
                user,
                ssl_mode: self.ssl_mode,
                ssh,
                read_only: self.read_only,
                confirm_writes: self.confirm_writes || self.environment == Environment::Prod,
            },
            password,
        ))
    }

    fn test_connection(&mut self, cx: &mut Context<Self>) {
        let (connection, password) = match self.form(cx) {
            Ok(form) => form,
            Err(error) => {
                self.test = TestState::Failed(error);
                cx.notify();
                return;
            }
        };
        self.test = TestState::Running;
        cx.notify();
        self.test_task = cx.spawn(async move |this, cx| {
            let result = async {
                let started = Instant::now();
                let target = connection
                    .open_target(Some(password).filter(|password| !password.is_empty()))
                    .await?;
                let session = Session::connect(&target).await?;
                let connected_in = started.elapsed();
                let relations = catalog::list_relations(&session).await?;
                let encrypted = session
                    .run(
                        "select ssl from pg_stat_ssl where pid = pg_backend_pid()",
                        1,
                    )
                    .await?
                    .result_sets
                    .first()
                    .and_then(|result_set| result_set.rows.first())
                    .and_then(|row| row.first().cloned().flatten())
                    .as_deref()
                    == Some("t");
                let schemas = relations
                    .iter()
                    .map(|relation| relation.schema.as_str())
                    .collect::<collections::HashSet<_>>()
                    .len();
                let version = session
                    .server_version
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_owned();
                anyhow::Ok(format!(
                    "PostgreSQL {version} · {} · {schemas} schemas, {} relações · {} ms",
                    if encrypted { "TLS" } else { "sem TLS" },
                    relations.len(),
                    connected_in.as_millis()
                ))
            }
            .await;
            this.update(cx, |this, cx| {
                this.test = match result {
                    Ok(summary) => TestState::Passed(summary.into(), Instant::now()),
                    Err(error) => TestState::Failed(format!("{error:#}").into()),
                };
                cx.notify();
            })
            .log_err();
        });
    }

    fn save(&mut self, _: &SaveConnection, _window: &mut Window, cx: &mut Context<Self>) {
        if self.save_task.is_some() {
            return;
        }
        let (connection, password) = match self.form(cx) {
            Ok(form) => form,
            Err(error) => {
                self.save_error = Some(error);
                cx.notify();
                return;
            }
        };
        let Some(panel) = self.panel.upgrade() else {
            self.save_error = Some("O painel do Banco foi fechado.".into());
            cx.notify();
            return;
        };
        let scope = self.scope;
        let previous = self.editing.clone();
        let task = panel.update(cx, |panel, cx| {
            panel.save_connection(connection, scope, password, previous, cx)
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

    fn render_test_result(&self, cx: &App) -> Option<AnyElement> {
        let (icon, title, detail, color, when) = match &self.test {
            TestState::Idle => return None,
            TestState::Running => (
                IconName::LoadCircle,
                "Testando…",
                SharedString::default(),
                Color::Muted,
                None,
            ),
            TestState::Passed(summary, at) => (
                IconName::Check,
                "Conectado",
                summary.clone(),
                Color::Success,
                Some(*at),
            ),
            TestState::Failed(error) => (
                IconName::XCircle,
                "Falhou",
                error.clone(),
                Color::Error,
                None,
            ),
        };
        Some(
            h_flex()
                .min_h(px(44.))
                .px_4()
                .py_2()
                .gap_2p5()
                .rounded_lg()
                .bg(color.color(cx).opacity(0.06))
                .border_1()
                .border_color(color.color(cx).opacity(0.33))
                .child(Icon::new(icon).size(IconSize::Small).color(color))
                .child(
                    Label::new(title)
                        .size(LabelSize::Small)
                        .weight(FontWeight::SEMIBOLD)
                        .color(color),
                )
                .child(
                    div().flex_1().min_w_0().child(
                        Label::new(detail)
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Muted),
                    ),
                )
                .when_some(when, |this, at| {
                    this.child(
                        Label::new(format!("há {} s", at.elapsed().as_secs()))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                })
                .into_any_element(),
        )
    }

    /// The URL's outcome shown at the end of its field, as in the board.
    fn update_url_hint(&mut self, cx: &mut Context<Self>) {
        let hint = self.url_status.as_ref().map(|status| match status {
            Ok(text) => (SharedString::from(text.clone()), Color::Success),
            Err(error) => (error.clone(), Color::Error),
        });
        self.url_input
            .update(cx, |input, cx| input.set_end_hint(hint, cx));
    }
}

fn set_input(input: &Entity<InputField>, text: &str, window: &mut Window, cx: &mut App) {
    input.update(cx, |input, cx| input.set_text(text, window, cx));
}

/// A switch with its title and explanation beside it, half the width of the form.
fn toggle(
    id: &'static str,
    title: &'static str,
    description: &'static str,
    on: bool,
    on_click: impl Fn(&ToggleState, &mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    h_flex()
        .flex_1()
        .min_w_0()
        .gap_2p5()
        .items_start()
        .child(
            Switch::new(id, ToggleState::from(on))
                .color(SwitchColor::Custom(cx.theme().colors().border_focused))
                .on_click(on_click),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    Label::new(title)
                        .size(LabelSize::Small)
                        .weight(FontWeight::MEDIUM),
                )
                .child(
                    Label::new(description)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
        )
}

fn field_label(text: &'static str) -> impl IntoElement {
    Label::new(text).size(LabelSize::Small).color(Color::Muted)
}

impl Render for ConnectView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ssl_modes = [
            SslMode::Disable,
            SslMode::Prefer,
            SslMode::Require,
            SslMode::VerifyCa,
            SslMode::VerifyFull,
        ];
        let ssl_hint = match self.ssl_mode {
            SslMode::Disable => "disable: sem criptografia. Só para bancos na sua máquina.",
            SslMode::Prefer => {
                "prefer: tenta TLS e cai para texto puro. Use verify-full fora da máquina local."
            }
            SslMode::Require => "require: sempre com TLS, mas não confere quem está do outro lado.",
            SslMode::VerifyCa => "verify-ca: TLS com certificado confiável, sem conferir o host.",
            SslMode::VerifyFull => "verify-full: TLS com certificado confiável para este host.",
        };
        let saving = self.save_task.is_some();
        let title = if self.editing.is_some() {
            "Editar conexão"
        } else {
            "Nova conexão"
        };
        let colors = cx.theme().colors();
        let environment_color = environment_color(self.environment).color(cx);
        let two = |left: AnyElement, right: AnyElement| {
            h_flex()
                .gap_4()
                .items_start()
                .child(div().flex_1().child(left))
                .child(div().flex_1().child(right))
        };
        let environment = v_flex()
            .gap_1()
            .child(field_label("Ambiente"))
            .child(
                segmented_control(cx).h(px(36.)).children(Environment::ALL.map(|environment| {
                    let selected = environment == self.environment;
                    segment(
                        ("db-environment", environment as usize),
                        environment.label(),
                        selected,
                        cx,
                    )
                    .h(px(28.))
                    .when(selected, |this| {
                        this.bg(environment_color.opacity(0.1))
                            .border_color(environment_color)
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_environment(environment, cx)
                    }))
                })),
            );
        let ssh_open = self.ssh_enabled;

        v_flex()
            .key_context("DatabaseConnectView")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::save))
            .id("db-connect-view")
            .size_full()
            .overflow_y_scroll()
            .bg(colors.editor_background)
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(680.))
                    .mx_auto()
                    .py_6()
                    .gap_3p5()
                    .child(
                        h_flex()
                            .gap_3()
                            .child(Headline::new(title).size(HeadlineSize::Small))
                            .child(
                                h_flex()
                                    .h(px(22.))
                                    .px_2()
                                    .gap_1()
                                    .rounded_md()
                                    .bg(colors.border_focused.opacity(0.22))
                                    .child(
                                        Icon::new(IconName::Database)
                                            .size(IconSize::XSmall)
                                            .color(Color::Accent),
                                    )
                                    .child(
                                        Label::new("PostgreSQL")
                                            .size(LabelSize::XSmall)
                                            .weight(FontWeight::MEDIUM)
                                            .color(Color::Accent),
                                    ),
                            )
                            .child(
                                Label::new("Outros drivers depois (MySQL, SQLite)")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            ),
                    )
                    .child(self.url_input.clone())
                    .child(div().h_px().w_full().bg(colors.border))
                    .child(two(
                        self.name_input.clone().into_any_element(),
                        environment.into_any_element(),
                    ))
                    .child(two(
                        self.host_input.clone().into_any_element(),
                        self.port_input.clone().into_any_element(),
                    ))
                    .child(two(
                        self.database_input.clone().into_any_element(),
                        self.user_input.clone().into_any_element(),
                    ))
                    .child(self.password_input.clone())
                    .child(
                        v_flex().gap_1().child(field_label("SSL")).child(
                            h_flex()
                                .gap_3p5()
                                .child(segmented_control(cx).h(px(36.)).children(
                                    ssl_modes.map(|mode| {
                                        let label = Label::new(mode.as_str())
                                            .size(LabelSize::Small)
                                            .buffer_font(cx);
                                        segment(
                                            ("db-ssl", mode as usize),
                                            "",
                                            mode == self.ssl_mode,
                                            cx,
                                        )
                                        .h(px(28.))
                                        .child(label.color(if mode == self.ssl_mode {
                                            Color::Default
                                        } else {
                                            Color::Muted
                                        }))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.ssl_mode = mode;
                                            this.test = TestState::Idle;
                                            cx.notify();
                                        }))
                                    }),
                                ))
                                .child(
                                    div().flex_1().min_w_0().child(
                                        Label::new(ssl_hint)
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    ),
                                ),
                        ),
                    )
                    .child(
                        h_flex()
                            .py_1()
                            .gap_4()
                            .items_start()
                            .child(toggle(
                                "db-read-only",
                                "Somente leitura",
                                "Abre toda sessão com default_transaction_read_only.",
                                self.read_only,
                                cx.listener(|this, state: &ToggleState, _, cx| {
                                    this.read_only = state.selected();
                                    cx.notify();
                                }),
                                cx,
                            ))
                            .child(toggle(
                                "db-confirm-writes",
                                "Confirmar escrita",
                                "UPDATE/DELETE sem WHERE e DDL pedem confirmação. Sempre ligado \
                                 em prod.",
                                self.confirm_writes || self.environment == Environment::Prod,
                                cx.listener(|this, state: &ToggleState, _, cx| {
                                    this.confirm_writes = state.selected();
                                    cx.notify();
                                }),
                                cx,
                            )),
                    )
                    .child(
                        v_flex()
                            .rounded_lg()
                            .bg(colors.element_background)
                            .border_1()
                            .border_color(colors.border)
                            .child(
                                h_flex()
                                    .id("db-ssh")
                                    .h(px(42.))
                                    .px_3p5()
                                    .gap_2p5()
                                    .cursor_pointer()
                                    .child(
                                        Icon::new(if ssh_open {
                                            IconName::ChevronDown
                                        } else {
                                            IconName::ChevronRight
                                        })
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                    )
                                    .child(
                                        Icon::new(IconName::Server)
                                            .size(IconSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .child(
                                        Label::new("Túnel SSH")
                                            .size(LabelSize::Small)
                                            .weight(FontWeight::MEDIUM),
                                    )
                                    .child(
                                        Label::new("usa o ssh do sistema e o ~/.ssh/config")
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .child(div().flex_1())
                                    .when(ssh_open, |this| {
                                        this.child(
                                            Label::new("ligado")
                                                .size(LabelSize::XSmall)
                                                .color(Color::Accent),
                                        )
                                    })
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.ssh_enabled = !this.ssh_enabled;
                                        this.test = TestState::Idle;
                                        cx.notify();
                                    })),
                            )
                            .when(ssh_open, |this| {
                                this.child(
                                    v_flex()
                                        .px_3p5()
                                        .pb_3p5()
                                        .gap_2()
                                        .child(
                                            h_flex()
                                                .gap_4()
                                                .items_start()
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .child(self.ssh_host_input.clone()),
                                                )
                                                .child(
                                                    div()
                                                        .w(px(140.))
                                                        .child(self.ssh_port_input.clone()),
                                                ),
                                        )
                                        .child(
                                            Label::new(
                                                "Chaves e agent do sistema, sem prompt de senha. \
                                                 Host e porta acima são os do banco visto a \
                                                 partir do servidor SSH.",
                                            )
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                        ),
                                )
                            }),
                    )
                    .child(
                        v_flex()
                            .pt_1()
                            .gap_1()
                            .child(field_label("Salvar em"))
                            .child(
                                h_flex().child(
                                    segmented_control(cx)
                                        .h(px(36.))
                                        .child(
                                            segment(
                                                "db-scope-profile",
                                                "Só para mim (perfil)",
                                                self.scope == Scope::Profile,
                                                cx,
                                            )
                                            .h(px(28.))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.scope = Scope::Profile;
                                                cx.notify();
                                            })),
                                        )
                                        .child(
                                            segment(
                                                "db-scope-project",
                                                format!("Projeto · {}", connection::PROJECT_FILE),
                                                self.scope == Scope::Project,
                                                cx,
                                            )
                                            .h(px(28.))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.scope = Scope::Project;
                                                cx.notify();
                                            })),
                                        ),
                                ),
                            )
                            .child(
                                Label::new(
                                    "No projeto vai só host, porta, banco e usuário. Senha fica \
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
                            .gap_3()
                            .child(
                                secondary_button(
                                    "db-test",
                                    Some((IconName::SignalHigh, Color::Muted)),
                                    "Testar conexão",
                                    !matches!(self.test, TestState::Running),
                                    cx,
                                )
                                .h(px(36.))
                                .on_click(cx.listener(|this, _, _, cx| this.test_connection(cx))),
                            )
                            .child(div().flex_1())
                            .child(
                                Button::new("db-cancel", "Cancelar")
                                    .style(ButtonStyle::Subtle)
                                    .on_click(
                                        cx.listener(|_, _, _, cx| cx.emit(ItemEvent::CloseItem)),
                                    ),
                            )
                            .child(
                                primary_button(
                                    "db-save",
                                    None,
                                    if saving {
                                        "Salvando…"
                                    } else {
                                        "Salvar e conectar"
                                    },
                                    Some(ui::KeyBinding::for_action_in(
                                        &SaveConnection,
                                        &self.focus_handle,
                                        cx,
                                    )),
                                    cx,
                                )
                                .h(px(36.))
                                .w(px(196.))
                                .justify_between()
                                .when(!saving, |this| {
                                    this.on_click(cx.listener(|this, _, window, cx| {
                                        this.save(&SaveConnection, window, cx)
                                    }))
                                }),
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
        Some(Icon::new(IconName::Database).color(Color::Muted))
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
    panel: Entity<DatabasePanel>,
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
    panel: Entity<DatabasePanel>,
    prefill: Prefill,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let editing = match &prefill {
        Prefill::Edit(connection, _) => Some(connection.id.clone()),
        _ => None,
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
