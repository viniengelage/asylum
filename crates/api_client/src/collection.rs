//! One linked API: its spec, the shared settings file, and this profile's credentials and
//! session. Requests go through here so login, renewal and retries happen in one place.

use crate::{
    config::{self, CollectionFile, Environment, LoginConfig, RequestDraft, SavedRequest},
    jsonpath,
    schema::{self, Validation},
    send::{
        self, PreparedRequest, REFRESH_TOKEN_VARIABLE, ReceivedResponse, TOKEN_VARIABLE, jwt_expiry,
    },
    spec::{self, Spec},
    vars::{self, SECRET_PREFIX},
};
use anyhow::{Context as _, Result, anyhow};
use chrono::{DateTime, Local, Utc};
use collections::{BTreeMap, HashMap};
use credentials_provider::CredentialsProvider;
use db::kvp::KeyValueStore;
use fs::Fs;
use futures::StreamExt as _;
use gpui::{AppContext as _, AsyncApp, Context, SharedString, Task, TaskExt as _, WeakEntity};
use http_client::HttpClient;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use util::ResultExt as _;

/// Tokens this close to expiring are renewed before the request goes out.
const RENEW_MARGIN_SECONDS: i64 = 5 * 60;
const WATCH_LATENCY: Duration = Duration::from_millis(300);

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub token: Option<String>,
    #[serde(rename = "refreshToken")]
    pub refresh_token: Option<String>,
    /// Seconds since the epoch.
    #[serde(rename = "expiresAt")]
    pub expires_at: Option<i64>,
}

impl Session {
    pub fn is_logged_in(&self) -> bool {
        self.token.is_some()
    }

    pub fn seconds_left(&self) -> Option<i64> {
        self.expires_at
            .map(|expires_at| expires_at - Utc::now().timestamp())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum LoginStatus {
    Idle,
    LoggingIn,
    Failed(SharedString),
}

#[derive(Clone, Debug)]
pub struct TimelineStep {
    pub method: String,
    pub path: String,
    pub status: Option<u16>,
    pub note: String,
    pub elapsed: Duration,
}

/// One press of "Enviar": what went out, what came back, and what happened in between.
#[derive(Clone, Debug)]
pub struct Exchange {
    pub request: Option<PreparedRequest>,
    pub response: Option<ReceivedResponse>,
    pub error: Option<String>,
    pub timeline: Vec<TimelineStep>,
    pub validation: Option<Validation>,
    pub response_schema_name: Option<String>,
    pub sent_at: DateTime<Local>,
}

pub struct Collection {
    pub id: String,
    /// The folder holding `.asylum`.
    pub root: PathBuf,
    pub file_path: PathBuf,
    pub file: CollectionFile,
    pub file_error: Option<SharedString>,
    last_written: Option<String>,
    pub spec: Option<Arc<Spec>>,
    pub spec_error: Option<SharedString>,
    pub synced_at: Option<DateTime<Local>>,
    validation_root: Arc<Map<String, Value>>,
    active_environment: Option<String>,
    secrets: BTreeMap<String, String>,
    pub session: Session,
    pub login_status: LoginStatus,
    drafts: HashMap<String, RequestDraft>,
    fs: Arc<dyn Fs>,
    http_client: Arc<dyn HttpClient>,
    credentials_provider: Arc<dyn CredentialsProvider>,
    spec_task: Option<Task<()>>,
    login_task: Option<Task<()>>,
    _load_task: Task<()>,
    _watch_task: Task<()>,
}

impl Collection {
    pub fn new(root: PathBuf, file_path: PathBuf, fs: Arc<dyn Fs>, cx: &mut Context<Self>) -> Self {
        let id = file_path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "api".to_string());
        let active_environment = KeyValueStore::global(cx)
            .read_kvp(&active_environment_key(&file_path))
            .log_err()
            .flatten();
        let mut collection = Self {
            id,
            root,
            file_path,
            file: CollectionFile::default(),
            file_error: None,
            last_written: None,
            spec: None,
            spec_error: None,
            synced_at: None,
            validation_root: Arc::default(),
            active_environment,
            secrets: BTreeMap::default(),
            session: Session::default(),
            login_status: LoginStatus::Idle,
            drafts: HashMap::default(),
            fs,
            http_client: cx.http_client(),
            credentials_provider: zed_credentials_provider::global(cx),
            spec_task: None,
            login_task: None,
            _load_task: Task::ready(()),
            _watch_task: Task::ready(()),
        };
        collection._load_task = collection.load(cx);
        collection
    }

    pub fn fs(&self) -> Arc<dyn Fs> {
        self.fs.clone()
    }

    pub fn spec_path(&self) -> PathBuf {
        self.root.join(&self.file.spec)
    }

    pub fn title(&self) -> String {
        self.spec
            .as_ref()
            .map(|spec| spec.title.clone())
            .unwrap_or_else(|| self.id.clone())
    }

    fn load(&mut self, cx: &mut Context<Self>) -> Task<()> {
        let fs = self.fs.clone();
        let file_path = self.file_path.clone();
        let credentials_provider = self.credentials_provider.clone();
        let secrets_url = secrets_url(&self.id);
        let session_url = session_url(&self.id);
        cx.spawn(async move |this, cx| {
            let text = fs.load(&file_path).await;
            let loaded = this.update(cx, |this, cx| {
                match text.and_then(|text| config::parse(&text).map(|file| (file, text))) {
                    Ok((file, text)) => {
                        this.file = file;
                        this.last_written = Some(text);
                        this.file_error = None;
                        this.reload_spec(cx);
                        this.watch(cx);
                    }
                    Err(error) => {
                        this.file_error = Some(format!("{error:#}").into());
                    }
                }
                cx.notify();
                this.file_error.is_none()
            });
            if !matches!(loaded, Ok(true)) {
                return;
            }

            let secrets =
                read_json::<BTreeMap<String, String>>(&credentials_provider, &secrets_url, cx)
                    .await;
            let session = read_json::<Session>(&credentials_provider, &session_url, cx).await;
            this.update(cx, |this, cx| {
                if let Some(secrets) = secrets {
                    this.secrets = secrets;
                }
                if let Some(session) = session
                    && this.file.auth.remember_session
                {
                    this.session = session;
                }
                cx.notify();
            })
            .log_err();
        })
    }

    pub fn reload_spec(&mut self, cx: &mut Context<Self>) {
        let fs = self.fs.clone();
        let path = self.spec_path();
        self.spec_task = Some(cx.spawn(async move |this, cx| {
            let loaded = match fs.load(&path).await {
                Ok(text) => {
                    cx.background_spawn(async move {
                        let spec = spec::load(&path, &text, &|path: &Path| {
                            Ok(std::fs::read_to_string(path)?)
                        })?;
                        let root = schema::validation_root(&spec);
                        anyhow::Ok((spec, root))
                    })
                    .await
                }
                Err(error) => Err(error).context("não foi possível ler o spec"),
            };
            this.update(cx, |this, cx| {
                match loaded {
                    Ok((spec, root)) => {
                        // Drafts of operations that left the spec stay, so a renamed path
                        // doesn't lose what was typed; they just have nothing to open them.
                        this.spec = Some(Arc::new(spec));
                        this.validation_root = Arc::new(root);
                        this.spec_error = None;
                        this.synced_at = Some(Local::now());
                    }
                    Err(error) => {
                        log::warn!("API: falha ao ler o spec: {error:#}");
                        this.spec_error = Some(format!("{error:#}").into());
                    }
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    /// Watches the folders of the spec and the settings file: saving the yml, switching
    /// branches and a teammate's change pulled from git all show up without reopening.
    fn watch(&mut self, cx: &mut Context<Self>) {
        let fs = self.fs.clone();
        let spec_path = self.spec_path();
        let file_path = self.file_path.clone();
        self._watch_task = cx.spawn(async move |this, cx| {
            let Some(spec_dir) = spec_path.parent() else {
                return;
            };
            let Some(file_dir) = file_path.parent() else {
                return;
            };
            let (spec_events, _spec_watcher) = fs.watch(spec_dir, WATCH_LATENCY).await;
            let (file_events, _file_watcher) = fs.watch(file_dir, WATCH_LATENCY).await;
            let mut events = futures::stream::select(spec_events, file_events);
            while let Some(batch) = events.next().await {
                let spec_changed = batch.iter().any(|event| event.path == spec_path);
                let file_changed = batch.iter().any(|event| event.path == file_path);
                let result = this.update(cx, |this, cx| {
                    if file_changed {
                        this.reload_file(cx);
                    }
                    if spec_changed {
                        this.reload_spec(cx);
                    }
                });
                if result.is_err() {
                    break;
                }
            }
        });
    }

    fn reload_file(&mut self, cx: &mut Context<Self>) {
        let fs = self.fs.clone();
        let file_path = self.file_path.clone();
        cx.spawn(async move |this, cx| {
            let text = fs.load(&file_path).await?;
            this.update(cx, |this, cx| {
                if this.last_written.as_deref() == Some(text.as_str()) {
                    return;
                }
                match config::parse(&text) {
                    Ok(file) => {
                        let spec_moved = file.spec != this.file.spec;
                        this.file = file;
                        this.last_written = Some(text);
                        this.file_error = None;
                        if spec_moved {
                            this.reload_spec(cx);
                            this.watch(cx);
                        }
                    }
                    Err(error) => this.file_error = Some(format!("{error:#}").into()),
                }
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    pub fn update_file(
        &mut self,
        update: impl FnOnce(&mut CollectionFile),
        cx: &mut Context<Self>,
    ) {
        update(&mut self.file);
        cx.notify();
        self.save_file(cx);
    }

    fn save_file(&mut self, cx: &mut Context<Self>) {
        let text = match config::serialize(&self.file) {
            Ok(text) => text,
            Err(error) => {
                log::error!("API: falha ao serializar a coleção: {error:#}");
                return;
            }
        };
        self.last_written = Some(text.clone());
        let fs = self.fs.clone();
        let file_path = self.file_path.clone();
        cx.spawn(async move |this, cx| {
            if let Some(parent) = file_path.parent() {
                fs.create_dir(parent).await?;
            }
            let result = fs.atomic_write(file_path, text).await;
            if let Err(error) = &result {
                this.update(cx, |this, cx| {
                    this.file_error = Some(format!("não foi possível salvar: {error:#}").into());
                    cx.notify();
                })
                .log_err();
            }
            result
        })
        .detach_and_log_err(cx);
    }

    pub fn environments(&self) -> &[Environment] {
        &self.file.environments
    }

    pub fn active_environment(&self) -> Option<&Environment> {
        let environments = &self.file.environments;
        self.active_environment
            .as_ref()
            .and_then(|name| {
                environments
                    .iter()
                    .find(|environment| &environment.name == name)
            })
            .or_else(|| environments.first())
    }

    pub fn set_active_environment(&mut self, name: String, cx: &mut Context<Self>) {
        let key = active_environment_key(&self.file_path);
        let store = KeyValueStore::global(cx);
        self.active_environment = Some(name.clone());
        cx.background_spawn(async move { store.write_kvp(key, name).await })
            .detach_and_log_err(cx);
        cx.notify();
    }

    pub fn variable(&self, name: &str) -> Option<String> {
        if name == TOKEN_VARIABLE {
            return self.session.token.clone();
        }
        if name == REFRESH_TOKEN_VARIABLE {
            return self.session.refresh_token.clone();
        }
        if let Some(secret) = name.strip_prefix(SECRET_PREFIX) {
            return self.secrets.get(secret).cloned();
        }
        self.active_environment()
            .and_then(|environment| environment.get(name))
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    }

    pub fn secret(&self, name: &str) -> Option<&str> {
        self.secrets.get(name).map(String::as_str)
    }

    /// Saves (or, when empty, forgets) a credential in this profile's keychain.
    pub fn set_secret(&mut self, name: &str, value: String, cx: &mut Context<Self>) {
        if value.is_empty() {
            self.secrets.remove(name);
        } else {
            self.secrets.insert(name.to_string(), value);
        }
        cx.notify();
        let secrets = self.secrets.clone();
        let url = secrets_url(&self.id);
        let credentials_provider = self.credentials_provider.clone();
        cx.spawn(async move |_this, cx| {
            write_json(&credentials_provider, &url, &secrets, cx).await
        })
        .detach_and_log_err(cx);
    }

    /// The `{{secret.*}}` names the login body asks for.
    pub fn login_secret_names(&self) -> Vec<String> {
        let Some(login) = &self.file.auth.login else {
            return Vec::new();
        };
        vars::placeholder_names(&login.body)
            .into_iter()
            .filter_map(|name| name.strip_prefix(SECRET_PREFIX).map(str::to_string))
            .collect()
    }

    /// Who is logged in, from the credential that isn't a password.
    pub fn login_label(&self) -> Option<String> {
        self.login_secret_names()
            .into_iter()
            .filter(|name| {
                let lower = name.to_ascii_lowercase();
                !lower.contains("pass") && !lower.contains("senha")
            })
            .find_map(|name| self.secrets.get(&name).cloned())
    }

    pub fn draft(&self, operation_key: &str) -> RequestDraft {
        if let Some(draft) = self.drafts.get(operation_key) {
            return draft.clone();
        }
        match &self.spec {
            Some(spec) => config::initial_draft(spec, operation_key),
            None => RequestDraft::default(),
        }
    }

    pub fn store_draft(&mut self, operation_key: &str, draft: RequestDraft) {
        self.drafts.insert(operation_key.to_string(), draft);
    }

    pub fn save_request(
        &mut self,
        name: String,
        operation_key: String,
        draft: RequestDraft,
        cx: &mut Context<Self>,
    ) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let saved = SavedRequest {
            id: id.clone(),
            name,
            operation: operation_key,
            draft,
        };
        self.update_file(|file| file.saved.push(saved), cx);
        id
    }

    pub fn update_saved_request(&mut self, id: &str, draft: RequestDraft, cx: &mut Context<Self>) {
        self.update_file(
            |file| {
                if let Some(saved) = file.saved.iter_mut().find(|saved| saved.id == id) {
                    saved.draft = draft;
                }
            },
            cx,
        );
    }

    pub fn delete_saved_request(&mut self, id: &str, cx: &mut Context<Self>) {
        self.update_file(|file| file.saved.retain(|saved| saved.id != id), cx);
    }

    pub fn prepare(&self, operation_key: &str, draft: &RequestDraft) -> Result<PreparedRequest> {
        let spec = self.spec.as_ref().context("o spec ainda não foi lido")?;
        send::prepare(
            spec,
            operation_key,
            draft,
            &self.file.headers,
            &|name: &str| self.variable(name),
        )
    }

    pub fn validate_body(&self, operation_key: &str, body: &str) -> Option<Validation> {
        let spec = self.spec.as_ref()?;
        let schema = spec
            .operation(operation_key)?
            .request_body
            .as_ref()?
            .schema
            .as_ref()?;
        let value: Value = match serde_json::from_str(body) {
            Ok(value) => value,
            Err(error) => {
                return Some(Validation::Invalid {
                    issues: vec![schema::Issue {
                        location: String::new(),
                        message: format!("JSON inválido: {error}"),
                    }],
                    more: 0,
                });
            }
        };
        // Placeholders sit inside strings, so validating before filling them in is fine.
        Some(schema::validate(
            &self.validation_root,
            spec.format,
            schema,
            &value,
        ))
    }

    fn can_refresh(&self) -> bool {
        let Some(login) = &self.file.auth.login else {
            return false;
        };
        login.refresh_operation.is_some() && self.session.refresh_token.is_some()
    }

    fn can_log_in(&self) -> bool {
        let Some(login) = &self.file.auth.login else {
            return false;
        };
        vars::placeholder_names(&login.body)
            .iter()
            .all(|name| self.variable(name).is_some())
    }

    fn session_needs_renewal(&self) -> bool {
        self.file.auth.renew_before_expiry
            && self.session.is_logged_in()
            && self
                .session
                .seconds_left()
                .is_some_and(|left| left < RENEW_MARGIN_SECONDS)
            && (self.can_refresh() || self.can_log_in())
    }

    /// Logs in with the saved credentials, replacing the current session.
    pub fn login(&mut self, cx: &mut Context<Self>) -> Task<Result<TimelineStep>> {
        self.login_status = LoginStatus::LoggingIn;
        cx.notify();
        let prepared = self.login_request();
        let http_client = self.http_client.clone();
        cx.spawn(async move |this, cx| {
            let result = async {
                let (login, prepared) = prepared?;
                let (step, response) = run_step(http_client, &prepared, "login").await;
                let response = response?;
                let session = session_from_response(&login, &response)?;
                anyhow::Ok((step, session))
            }
            .await;
            this.update(cx, |this, cx| match result {
                Ok((step, session)) => {
                    this.login_status = LoginStatus::Idle;
                    this.set_session(session, cx);
                    Ok(step)
                }
                Err(error) => {
                    this.login_status = LoginStatus::Failed(format!("{error:#}").into());
                    cx.notify();
                    Err(error)
                }
            })?
        })
    }

    fn login_request(&self) -> Result<(LoginConfig, PreparedRequest)> {
        let login = self
            .file
            .auth
            .login
            .clone()
            .context("a coleção não tem login configurado")?;
        let spec = self.spec.as_ref().context("o spec ainda não foi lido")?;
        let operation = spec
            .operation(&login.operation)
            .with_context(|| format!("{} não existe mais no spec", login.operation))?;
        let draft = RequestDraft {
            url: format!("{{{{baseUrl}}}}{}", operation.path),
            body: Some(login.body.clone()),
            ..RequestDraft::default()
        };
        let prepared = self.prepare(&login.operation, &draft)?;
        if let Some(missing) = prepared.missing.first() {
            return Err(anyhow!(
                "falta preencher {{{{{missing}}}}} para fazer login"
            ));
        }
        Ok((login, prepared))
    }

    fn refresh_request(&self) -> Result<(LoginConfig, PreparedRequest)> {
        let login = self
            .file
            .auth
            .login
            .clone()
            .context("a coleção não tem login configurado")?;
        let operation_key = login
            .refresh_operation
            .clone()
            .context("a coleção não tem request de renovação")?;
        let spec = self.spec.as_ref().context("o spec ainda não foi lido")?;
        let operation = spec
            .operation(&operation_key)
            .with_context(|| format!("{operation_key} não existe mais no spec"))?;
        let draft = RequestDraft {
            url: format!("{{{{baseUrl}}}}{}", operation.path),
            body: login.refresh_body.clone(),
            ..RequestDraft::default()
        };
        let prepared = self.prepare(&operation_key, &draft)?;
        Ok((login, prepared))
    }

    /// Refreshes the token, falling back to logging in again.
    pub fn renew(&mut self, cx: &mut Context<Self>) -> Task<Result<Vec<TimelineStep>>> {
        if !self.can_refresh() {
            let login = self.login(cx);
            return cx.background_spawn(async move { Ok(vec![login.await?]) });
        }
        let prepared = self.refresh_request();
        let http_client = self.http_client.clone();
        cx.spawn(async move |this, cx| {
            let refreshed = async {
                let (login, prepared) = prepared?;
                let (step, response) = run_step(http_client, &prepared, "renovou o token").await;
                let session =
                    response.and_then(|response| session_from_response(&login, &response));
                anyhow::Ok((step, session))
            }
            .await;
            let mut steps = Vec::new();
            match refreshed {
                Ok((step, Ok(session))) => {
                    steps.push(step);
                    this.update(cx, |this, cx| this.set_session(session, cx))?;
                    return Ok(steps);
                }
                Ok((mut step, Err(error))) => {
                    step.note = format!("falhou: {error:#}");
                    steps.push(step);
                }
                Err(error) => log::warn!("API: renovação indisponível: {error:#}"),
            }
            let login = this.update(cx, |this, cx| this.can_log_in().then(|| this.login(cx)))?;
            match login {
                Some(login) => {
                    steps.push(login.await?);
                    Ok(steps)
                }
                None => Err(anyhow!(
                    "o token expirou e não há credenciais para logar de novo"
                )),
            }
        })
    }

    fn set_session(&mut self, session: Session, cx: &mut Context<Self>) {
        self.session = session;
        cx.notify();
        if !self.file.auth.remember_session {
            return;
        }
        let session = self.session.clone();
        let url = session_url(&self.id);
        let credentials_provider = self.credentials_provider.clone();
        cx.spawn(async move |_this, cx| {
            write_json(&credentials_provider, &url, &session, cx).await
        })
        .detach_and_log_err(cx);
    }

    pub fn logout(&mut self, cx: &mut Context<Self>) {
        self.session = Session::default();
        self.login_status = LoginStatus::Idle;
        cx.notify();
        let url = session_url(&self.id);
        let credentials_provider = self.credentials_provider.clone();
        cx.spawn(async move |_this, cx| credentials_provider.delete_credentials(&url, cx).await)
            .detach_and_log_err(cx);
    }

    pub fn start_login(&mut self, cx: &mut Context<Self>) {
        let login = self.login(cx);
        self.login_task = Some(cx.background_spawn(async move {
            if let Err(error) = login.await {
                log::info!("API: login falhou: {error:#}");
            }
        }));
    }

    pub fn send(
        &mut self,
        operation_key: String,
        draft: RequestDraft,
        cx: &mut Context<Self>,
    ) -> Task<Exchange> {
        let http_client = self.http_client.clone();
        let sent_at = Local::now();
        cx.spawn(async move |this, cx| {
            let mut timeline = Vec::new();
            let result = send_with_retries(
                &this,
                http_client,
                &operation_key,
                &draft,
                &mut timeline,
                cx,
            )
            .await;
            let (request, response, error) = match result {
                Ok((request, response)) => (Some(request), Some(response), None),
                Err(failure) => {
                    let (request, error) = *failure;
                    (request, None, Some(format!("{error:#}")))
                }
            };
            let (validation, response_schema_name) = response
                .as_ref()
                .and_then(|response| {
                    this.read_with(cx, |this, _| {
                        this.validate_response(&operation_key, response)
                    })
                    .ok()
                    .flatten()
                })
                .map_or((None, None), |(validation, name)| (Some(validation), name));
            Exchange {
                request: request.map(|request| request.masked()),
                response,
                error,
                timeline,
                validation,
                response_schema_name,
                sent_at,
            }
        })
    }

    fn validate_response(
        &self,
        operation_key: &str,
        response: &ReceivedResponse,
    ) -> Option<(Validation, Option<String>)> {
        let spec = self.spec.as_ref()?;
        let operation = spec.operation(operation_key)?;
        let schema = operation
            .response_for_status(response.status)?
            .schema
            .as_ref()?;
        let body = response.json()?;
        let name = schema
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|reference| reference.rsplit('/').next())
            .map(str::to_string);
        Some((
            schema::validate(&self.validation_root, spec.format, schema, &body),
            name,
        ))
    }
}

/// What went out before the failure, if anything did, and why it failed.
type SendResult = std::result::Result<
    (PreparedRequest, ReceivedResponse),
    Box<(Option<PreparedRequest>, anyhow::Error)>,
>;

async fn send_with_retries(
    this: &WeakEntity<Collection>,
    http_client: Arc<dyn HttpClient>,
    operation_key: &str,
    draft: &RequestDraft,
    timeline: &mut Vec<TimelineStep>,
    cx: &mut AsyncApp,
) -> SendResult {
    let renewal = this
        .update(cx, |this, cx| {
            this.session_needs_renewal().then(|| this.renew(cx))
        })
        .map_err(|error| Box::new((None, error)))?;
    if let Some(renewal) = renewal {
        match renewal.await {
            Ok(steps) => timeline.extend(steps),
            Err(error) => log::info!("API: renovação antecipada falhou: {error:#}"),
        }
    }

    let prepare = |cx: &mut AsyncApp| {
        this.read_with(cx, |this, _| this.prepare(operation_key, draft))
            .and_then(|prepared| prepared)
            .map_err(|error| Box::new((None, error)))
    };
    let prepared = prepare(cx)?;
    let (step, response) = run_step(http_client.clone(), &prepared, "").await;
    timeline.push(step);
    let response = response.map_err(|error| Box::new((Some(prepared.clone()), error)))?;

    let carried_token = prepared.headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("authorization") && value.starts_with("Bearer ")
    });
    let may_retry = response.status == 401
        && carried_token
        && this
            .read_with(cx, |this, _| {
                this.file.auth.retry_on_unauthorized && (this.can_refresh() || this.can_log_in())
            })
            .unwrap_or(false);
    if !may_retry {
        return Ok((prepared, response));
    }
    if let Some(step) = timeline.last_mut() {
        step.note = "token recusado".to_string();
    }
    let renewal = this
        .update(cx, |this, cx| this.renew(cx))
        .map_err(|error| Box::new((Some(prepared.clone()), error)))?;
    match renewal.await {
        Ok(steps) => timeline.extend(steps),
        Err(error) => {
            log::info!("API: renovação depois do 401 falhou: {error:#}");
            return Ok((prepared, response));
        }
    }
    let prepared = prepare(cx)?;
    let (step, retried) = run_step(http_client, &prepared, "repetida com o token novo").await;
    timeline.push(step);
    let retried = retried.map_err(|error| Box::new((Some(prepared.clone()), error)))?;
    Ok((prepared, retried))
}

async fn run_step(
    http_client: Arc<dyn HttpClient>,
    prepared: &PreparedRequest,
    note: &str,
) -> (TimelineStep, Result<ReceivedResponse>) {
    let started = std::time::Instant::now();
    let result = send::execute(http_client, prepared).await;
    let path = prepared
        .url
        .split_once("://")
        .and_then(|(_, rest)| rest.find('/').map(|index| rest[index..].to_string()))
        .unwrap_or_else(|| prepared.url.clone());
    let step = TimelineStep {
        method: prepared.method.clone(),
        path,
        status: result.as_ref().ok().map(|response| response.status),
        note: match &result {
            Ok(_) => note.to_string(),
            Err(error) => format!("{error:#}"),
        },
        elapsed: result
            .as_ref()
            .map(|response| response.elapsed)
            .unwrap_or_else(|_| started.elapsed()),
    };
    (step, result)
}

fn session_from_response(login: &LoginConfig, response: &ReceivedResponse) -> Result<Session> {
    if !(200..300).contains(&response.status) {
        let detail = response
            .json()
            .and_then(|body| {
                ["message", "error", "detail"]
                    .iter()
                    .find_map(|key| body.get(key).and_then(Value::as_str).map(str::to_string))
            })
            .unwrap_or_default();
        return Err(anyhow!("o servidor respondeu {} {detail}", response.status));
    }
    let body = response.json().context("a resposta do login não é JSON")?;
    let token = jsonpath::select_string(&body, &login.token_path)
        .with_context(|| format!("a resposta não tem {}", login.token_path))?;
    let refresh_token = login
        .refresh_token_path
        .as_deref()
        .and_then(|path| jsonpath::select_string(&body, path));
    let expires_at = login
        .expires_in_path
        .as_deref()
        .and_then(|path| jsonpath::select(&body, path))
        .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
        .map(|seconds| Utc::now().timestamp() + seconds)
        .or_else(|| jwt_expiry(&token));
    Ok(Session {
        token: Some(token),
        refresh_token,
        expires_at,
    })
}

fn active_environment_key(file_path: &Path) -> String {
    format!("api_client-active-environment-{}", file_path.display())
}

/// Keychain entries are per collection id, so every clone and worktree of the same API
/// shares one login.
fn secrets_url(id: &str) -> String {
    format!("asylum-api://{id}/secrets")
}

fn session_url(id: &str) -> String {
    format!("asylum-api://{id}/session")
}

async fn read_json<T: for<'de> Deserialize<'de>>(
    credentials_provider: &Arc<dyn CredentialsProvider>,
    url: &str,
    cx: &AsyncApp,
) -> Option<T> {
    match credentials_provider.read_credentials(url, cx).await {
        Ok(Some((_, bytes))) => serde_json::from_slice(&bytes)
            .map_err(|error| log::error!("API: entrada ilegível no keychain ({url}): {error}"))
            .ok(),
        Ok(None) => None,
        Err(error) => {
            log::error!("API: falha ao ler o keychain ({url}): {error:#}");
            None
        }
    }
}

async fn write_json<T: Serialize>(
    credentials_provider: &Arc<dyn CredentialsProvider>,
    url: &str,
    value: &T,
    cx: &AsyncApp,
) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    credentials_provider
        .write_credentials(url, "asylum", &bytes, cx)
        .await
}

/// A new collection file for a spec, written next to the project's `.asylum` folder.
pub async fn create(
    fs: Arc<dyn Fs>,
    root: PathBuf,
    spec_path: PathBuf,
    cx: &mut AsyncApp,
) -> Result<PathBuf> {
    let text = fs.load(&spec_path).await?;
    let relative = spec_path
        .strip_prefix(&root)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| spec_path.to_string_lossy().into_owned());
    let spec_path_for_load = spec_path.clone();
    let file = cx
        .background_spawn(async move {
            let spec = spec::load(&spec_path_for_load, &text, &|path: &Path| {
                Ok(std::fs::read_to_string(path)?)
            })?;
            anyhow::Ok((config::slug(&spec.title), config::initial(&spec, relative)))
        })
        .await?;
    let (slug, file) = file;
    // Linking a spec that already has a collection opens that one instead of a copy.
    for existing in discover(&fs, &root).await {
        if let Ok(text) = fs.load(&existing).await
            && let Ok(existing_file) = config::parse(&text)
            && root.join(&existing_file.spec) == spec_path
        {
            return Ok(existing);
        }
    }
    let directory = root.join(config::COLLECTIONS_DIR);
    fs.create_dir(&directory).await?;
    let mut file_path = directory.join(format!("{slug}.json"));
    let mut suffix = 2;
    while fs.is_file(&file_path).await {
        file_path = directory.join(format!("{slug}-{suffix}.json"));
        suffix += 1;
    }
    fs.atomic_write(file_path.clone(), config::serialize(&file)?)
        .await?;
    Ok(file_path)
}

/// The collection files in a folder's `.asylum/api`.
pub async fn discover(fs: &Arc<dyn Fs>, root: &Path) -> Vec<PathBuf> {
    let directory = root.join(config::COLLECTIONS_DIR);
    let Ok(mut entries) = fs.read_dir(&directory).await else {
        return Vec::new();
    };
    let mut files = Vec::new();
    while let Some(entry) = entries.next().await {
        if let Ok(path) = entry
            && path
                .extension()
                .is_some_and(|extension| extension == "json")
        {
            files.push(path);
        }
    }
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use http_client::{AsyncBody, FakeHttpClient, Response};
    use parking_lot::Mutex;
    use std::{future::Future, pin::Pin};

    #[derive(Default)]
    struct MemoryKeychain(Mutex<HashMap<String, (String, Vec<u8>)>>);

    impl CredentialsProvider for MemoryKeychain {
        fn read_credentials<'a>(
            &'a self,
            url: &'a str,
            _cx: &'a AsyncApp,
        ) -> Pin<Box<dyn Future<Output = Result<Option<(String, Vec<u8>)>>> + 'a>> {
            let value = self.0.lock().get(url).cloned();
            Box::pin(async move { Ok(value) })
        }

        fn write_credentials<'a>(
            &'a self,
            url: &'a str,
            username: &'a str,
            password: &'a [u8],
            _cx: &'a AsyncApp,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
            self.0
                .lock()
                .insert(url.to_string(), (username.to_string(), password.to_vec()));
            Box::pin(async { Ok(()) })
        }

        fn delete_credentials<'a>(
            &'a self,
            url: &'a str,
            _cx: &'a AsyncApp,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
            self.0.lock().remove(url);
            Box::pin(async { Ok(()) })
        }
    }

    const SPEC: &str = r##"
openapi: 3.0.3
info: { title: Trix, version: "1" }
security: [{ bearerAuth: [] }]
components:
  securitySchemes:
    bearerAuth: { type: http, scheme: bearer }
  schemas:
    Me: { type: object, required: [id], properties: { id: { type: integer } } }
paths:
  /auth/login:
    post:
      security: []
      requestBody:
        content: { application/json: { schema: { properties: { email: { type: string }, password: { type: string } } } } }
      responses:
        '200': { description: ok, content: { application/json: { schema: { properties: { accessToken: { type: string }, refreshToken: { type: string } } } } } }
  /auth/refresh:
    post:
      security: []
      requestBody:
        content: { application/json: { schema: { properties: { refreshToken: { type: string } } } } }
      responses: { '200': { description: ok } }
  /me:
    get:
      responses:
        '200': { description: ok, content: { application/json: { schema: { $ref: '#/components/schemas/Me' } } } }
"##;

    #[gpui::test]
    async fn renews_the_token_and_retries_after_unauthorized(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", serde_json::json!({ "openapi.yml": SPEC }))
            .await;
        let keychain = Arc::new(MemoryKeychain::default());
        let session = serde_json::to_vec(&Session {
            token: Some("old".to_string()),
            refresh_token: Some("r1".to_string()),
            expires_at: None,
        })
        .unwrap();
        keychain
            .0
            .lock()
            .insert(session_url("trix"), ("asylum".to_string(), session));

        let requests: Arc<Mutex<Vec<String>>> = Arc::default();
        let http_client = FakeHttpClient::create({
            let requests = requests.clone();
            move |request| {
                let requests = requests.clone();
                async move {
                    let authorization = request
                        .headers()
                        .get("authorization")
                        .map(|value| value.to_str().unwrap_or("").to_string())
                        .unwrap_or_default();
                    let line = format!(
                        "{} {} {authorization}",
                        request.method(),
                        request.uri().path()
                    );
                    requests.lock().push(line);
                    let (status, body) = match (request.uri().path(), authorization.as_str()) {
                        ("/me", "Bearer new") => (200, r#"{"id":7}"#),
                        ("/me", _) => (401, r#"{"message":"expired"}"#),
                        ("/auth/refresh", _) => {
                            (200, r#"{"accessToken":"new","refreshToken":"r2"}"#)
                        }
                        _ => (404, "{}"),
                    };
                    Ok(Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(AsyncBody::from(body.to_string()))
                        .unwrap())
                }
            }
        });
        cx.update(|cx| {
            cx.set_global(db::AppDatabase::test_new());
            cx.set_global(zed_credentials_provider::ZedCredentialsProvider(
                keychain.clone(),
            ));
            cx.set_http_client(http_client);
        });

        let file_path = cx
            .update(|cx| {
                let fs: Arc<dyn Fs> = fs.clone();
                cx.spawn(async move |cx| {
                    create(
                        fs,
                        PathBuf::from("/project"),
                        PathBuf::from("/project/openapi.yml"),
                        cx,
                    )
                    .await
                })
            })
            .await
            .unwrap();
        assert_eq!(file_path, PathBuf::from("/project/.asylum/api/trix.json"));
        let linked_again = cx
            .update(|cx| {
                let fs: Arc<dyn Fs> = fs.clone();
                cx.spawn(async move |cx| {
                    create(
                        fs,
                        PathBuf::from("/project"),
                        PathBuf::from("/project/openapi.yml"),
                        cx,
                    )
                    .await
                })
            })
            .await
            .unwrap();
        assert_eq!(linked_again, file_path);

        let collection =
            cx.new(|cx| Collection::new(PathBuf::from("/project"), file_path, fs.clone(), cx));
        cx.run_until_parked();
        collection.update(cx, |collection, cx| {
            assert!(collection.spec.is_some(), "{:?}", collection.spec_error);
            assert_eq!(collection.session.token.as_deref(), Some("old"));
            let login = collection.file.auth.login.clone().unwrap();
            assert_eq!(login.operation, "POST /auth/login");
            assert_eq!(
                login.refresh_operation.as_deref(),
                Some("POST /auth/refresh")
            );
            collection.update_file(
                |file| file.environments[0].set("baseUrl", "https://api.trix.test".to_string()),
                cx,
            );
        });

        let draft = collection.read_with(cx, |collection, _| collection.draft("GET /me"));
        let exchange = collection
            .update(cx, |collection, cx| {
                collection.send("GET /me".to_string(), draft, cx)
            })
            .await;

        assert_eq!(
            *requests.lock(),
            vec![
                "GET /me Bearer old".to_string(),
                "POST /auth/refresh ".to_string(),
                "GET /me Bearer new".to_string(),
            ]
        );
        assert_eq!(exchange.error, None);
        assert_eq!(
            exchange.response.as_ref().map(|response| response.status),
            Some(200)
        );
        assert_eq!(exchange.timeline.len(), 3);
        assert_eq!(exchange.timeline[0].note, "token recusado");
        assert_eq!(exchange.validation, Some(Validation::Valid));
        assert_eq!(exchange.response_schema_name.as_deref(), Some("Me"));
        let masked = exchange.request.unwrap();
        assert!(
            masked
                .headers
                .iter()
                .any(|(name, value)| name == "Authorization" && value == "Bearer ••••")
        );

        cx.run_until_parked();
        collection.read_with(cx, |collection, _| {
            assert_eq!(collection.session.token.as_deref(), Some("new"));
            assert_eq!(collection.session.refresh_token.as_deref(), Some("r2"));
        });
        let saved = keychain
            .0
            .lock()
            .get(&session_url("trix"))
            .cloned()
            .unwrap();
        let saved: Session = serde_json::from_slice(&saved.1).unwrap();
        assert_eq!(saved.token.as_deref(), Some("new"));

        let text = fs
            .load(Path::new("/project/.asylum/api/trix.json"))
            .await
            .unwrap();
        assert!(text.contains("https://api.trix.test"));
        assert!(
            !text.contains("\"new\""),
            "tokens never go to the collection file"
        );
    }
}
