//! One linked API: its spec, the shared settings file, and this profile's credentials and
//! session. Requests go through here so login, renewal and retries happen in one place.

use crate::{
    config::{
        self, BodyKind, CaptureRule, CollectionFile, Environment, HeaderEntry, ImportedScript,
        LoginBodyFormat, LoginConfig, MergeOutcome, PostmanImport, RequestDraft, SavedRequest,
    },
    cookies::{Cookie, CookieJar},
    jsonpath,
    schema::{self, Validation},
    script,
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
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, Global, SharedString, Task, TaskExt as _,
    WeakEntity,
};
use http_client::HttpClient;
use i18n::t;
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

/// What survives between launches besides the login: cookies and captured values. Kept in a
/// file rather than the keychain because Windows caps a credential at 2560 bytes, which a
/// cookie jar easily outgrows.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct StoredState {
    #[serde(default)]
    cookies: CookieJar,
    #[serde(default)]
    captured: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Capture {
    pub variable: String,
    pub path: String,
    /// `None` when the path found nothing in the response.
    pub value: Option<String>,
}

/// What sending the login request itself put in the session.
#[derive(Clone, Debug, PartialEq)]
pub struct LoginCapture {
    pub token_path: String,
    pub refresh_token_path: Option<String>,
    pub expires_in: Option<i64>,
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
    /// The whole URL, host included: a login goes to another host than the API, and which
    /// one is what tells a wrong environment apart.
    pub url: String,
    pub content_type: Option<String>,
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
    /// Cookies the responses set along the way, the request's own and any login's.
    pub cookies_set: Vec<Cookie>,
    pub captures: Vec<Capture>,
    /// Set when this was the collection's login request and its token went into the session.
    pub login_capture: Option<LoginCapture>,
    pub tests: Vec<script::TestResult>,
}

/// One `Collection` per file for the whole app. The panel and the agent both send through
/// it, so they share the active environment, the session and the drafts: an agent holding
/// its own copy logged in against whatever environment it had read at startup.
#[derive(Default)]
struct OpenCollections(HashMap<PathBuf, Entity<Collection>>);

impl Global for OpenCollections {}

pub fn open(root: PathBuf, file_path: PathBuf, fs: Arc<dyn Fs>, cx: &mut App) -> Entity<Collection> {
    if let Some(existing) = cx
        .try_global::<OpenCollections>()
        .and_then(|open| open.0.get(&file_path).cloned())
    {
        return existing;
    }
    let collection = cx.new(|cx| Collection::new(root, file_path.clone(), fs, cx));
    cx.default_global::<OpenCollections>()
        .0
        .insert(file_path, collection.clone());
    collection
}

pub struct Collection {
    pub id: String,
    /// The folder holding `.asylum`.
    pub root: PathBuf,
    pub file_path: PathBuf,
    pub file: CollectionFile,
    pub file_error: Option<SharedString>,
    last_written: Option<String>,
    /// Writes of the settings file still in flight: until they land, the file on disk is
    /// older than `file` and must not be read back over it.
    pending_saves: usize,
    pub spec: Option<Arc<Spec>>,
    /// The text `spec` was parsed from, to tell whether the file changed since.
    spec_text: Option<String>,
    pub spec_error: Option<SharedString>,
    pub synced_at: Option<DateTime<Local>>,
    validation_root: Arc<Map<String, Value>>,
    active_environment: Option<String>,
    secrets: BTreeMap<String, String>,
    pub session: Session,
    pub login_status: LoginStatus,
    /// Why the last attempt to save the session, credentials or cookies failed.
    pub storage_error: Option<SharedString>,
    pub cookies: CookieJar,
    captured: BTreeMap<String, String>,
    failing_tests: HashMap<String, usize>,
    drafts: HashMap<String, RequestDraft>,
    /// The file and the keychain have been read; an import waits for this so loading doesn't
    /// overwrite it.
    loaded: bool,
    pending_import: Option<(PostmanImport, bool)>,
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
            pending_saves: 0,
            spec: None,
            spec_text: None,
            spec_error: None,
            synced_at: None,
            validation_root: Arc::default(),
            active_environment,
            secrets: BTreeMap::default(),
            session: Session::default(),
            login_status: LoginStatus::Idle,
            storage_error: None,
            cookies: CookieJar::default(),
            captured: BTreeMap::default(),
            failing_tests: HashMap::default(),
            drafts: HashMap::default(),
            loaded: false,
            pending_import: None,
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
        let state_url = state_url(&self.id);
        let state_path = state_file_path(&self.id);
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
            let state = load_state(&fs, &credentials_provider, &state_url, &state_path, cx).await;
            this.update(cx, |this, cx| {
                if let Some(secrets) = secrets {
                    this.secrets = secrets;
                }
                if let Some(state) = state
                    && this.file.auth.remember_session
                {
                    this.cookies = state.cookies;
                    this.captured = state.captured;
                }
                if let Some(session) = session
                    && this.file.auth.remember_session
                {
                    this.session = session;
                }
                this.loaded = true;
                if let Some((import, match_spec)) = this.pending_import.take() {
                    this.apply_postman_import(import, match_spec, cx);
                }
                cx.notify();
            })
            .log_err();
        })
    }

    pub fn reload_spec(&mut self, cx: &mut Context<Self>) {
        if self.file.spec.is_empty() {
            self.spec = None;
            self.spec_error = None;
            self.spec_task = None;
            return;
        }
        let fs = self.fs.clone();
        let path = self.spec_path();
        self.spec_task = Some(cx.spawn(async move |this, cx| {
            let loaded = match fs.load(&path).await {
                Ok(text) => parse_spec(path, text, cx).await,
                Err(error) => Err(error)
                    .with_context(|| t!("api_client.collection.error_read_spec").to_string()),
            };
            this.update(cx, |this, cx| this.apply_spec(loaded, cx))
                .log_err();
        }));
    }

    fn apply_spec(&mut self, loaded: Result<LoadedSpec>, cx: &mut Context<Self>) {
        match loaded {
            Ok(LoadedSpec { spec, root, text }) => {
                crate::lens::register_spec(self.spec_path(), cx.weak_entity(), cx);
                // Drafts of operations that left the spec stay, so a renamed path doesn't
                // lose what was typed; they just have nothing to open them. A draft still on
                // the old server follows the operation to its new one.
                if let Some(old) = self.spec.as_ref() {
                    for (key, draft) in &mut self.drafts {
                        if let (Some(before), Some(after)) = (old.operation(key), spec.operation(key))
                            && draft.url == before.url_template()
                        {
                            draft.url = after.url_template();
                        }
                    }
                }
                self.spec = Some(Arc::new(spec));
                self.spec_text = Some(text);
                self.validation_root = Arc::new(root);
                self.spec_error = None;
                self.synced_at = Some(Local::now());
            }
            Err(error) => {
                log::warn!("API: falha ao ler o spec: {error:#}");
                self.spec_error = Some(format!("{error:#}").into());
            }
        }
        cx.notify();
    }

    /// Watches the folders of the spec and the settings file: saving the yml, switching
    /// branches and a teammate's change pulled from git all show up without reopening.
    fn watch(&mut self, cx: &mut Context<Self>) {
        let fs = self.fs.clone();
        // Without a spec, `spec_path` is the project folder; watching it would rescan the tree.
        let spec_path = if self.file.spec.is_empty() {
            self.file_path.clone()
        } else {
            self.spec_path()
        };
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
            this.update(cx, |this, cx| this.apply_file_text(text, cx))
        })
        .detach_and_log_err(cx);
    }

    fn apply_file_text(&mut self, text: String, cx: &mut Context<Self>) {
        if self.pending_saves > 0 || self.last_written.as_deref() == Some(text.as_str()) {
            return;
        }
        match config::parse(&text) {
            Ok(file) => {
                let spec_moved = file.spec != self.file.spec;
                self.file = file;
                self.last_written = Some(text);
                self.file_error = None;
                if spec_moved {
                    self.spec_text = None;
                    self.reload_spec(cx);
                    self.watch(cx);
                }
            }
            Err(error) => self.file_error = Some(format!("{error:#}").into()),
        }
        cx.notify();
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
        self.pending_saves += 1;
        let fs = self.fs.clone();
        let file_path = self.file_path.clone();
        cx.spawn(async move |this, cx| {
            let result = async {
                if let Some(parent) = file_path.parent() {
                    fs.create_dir(parent).await?;
                }
                fs.atomic_write(file_path, text).await
            }
            .await;
            this.update(cx, |this, cx| {
                this.pending_saves = this.pending_saves.saturating_sub(1);
                if let Err(error) = &result {
                    this.file_error = Some(t!(
                        "api_client.collection.error_save_file",
                        error = format!("{error:#}")
                    ));
                    cx.notify();
                }
            })
            .log_err();
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
        if let Some(value) = self.captured.get(name) {
            return Some(value.clone());
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
        self.write_secrets(cx);
    }

    fn write_secrets(&mut self, cx: &mut Context<Self>) {
        cx.notify();
        let secrets = self.secrets.clone();
        let url = secrets_url(&self.id);
        let credentials_provider = self.credentials_provider.clone();
        cx.spawn(async move |this, cx| {
            let result = write_json(&credentials_provider, &url, &secrets, cx).await;
            report_storage_result(
                &this,
                t!("api_client.collection.storage_credentials"),
                result,
                cx,
            );
        })
        .detach();
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
            Some(spec) if spec.operation(operation_key).is_none() => {
                ad_hoc_draft(operation_key).unwrap_or_default()
            }
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
            ..SavedRequest::default()
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

    /// Brings Postman files in, merging by Postman id. Secret values go to the keychain.
    pub fn apply_postman_import(
        &mut self,
        import: PostmanImport,
        match_spec: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.loaded {
            self.pending_import = Some((import, match_spec));
            return;
        }
        let spec = if match_spec { self.spec.clone() } else { None };
        let mut outcome = MergeOutcome::default();
        self.update_file(
            |file| outcome = config::merge_postman(file, &import, spec.as_deref()),
            cx,
        );
        self.store_secrets(outcome.secrets, cx);
    }

    /// Points saved requests outside the spec at the operation with their method and path.
    pub fn match_saved_with_spec(&mut self, cx: &mut Context<Self>) -> Option<MergeOutcome> {
        let spec = self.spec.clone()?;
        let mut outcome = MergeOutcome::default();
        self.update_file(
            |file| outcome = config::match_saved_with_spec(file, &spec),
            cx,
        );
        Some(outcome)
    }

    fn store_secrets(&mut self, entries: Vec<(String, String)>, cx: &mut Context<Self>) {
        let mut changed = false;
        for (name, value) in entries {
            if !value.is_empty() && self.secrets.get(&name) != Some(&value) {
                self.secrets.insert(name, value);
                changed = true;
            }
        }
        if changed {
            self.write_secrets(cx);
        }
    }

    pub fn postman_collection_text(&self) -> Result<String> {
        config::serialize(&config::to_postman_collection(
            &self.title(),
            &self.file,
            self.spec.as_deref(),
        ))
    }

    pub fn postman_environment_texts(&self) -> Result<Vec<(String, String)>> {
        let secret_names = config::secret_names(&self.file);
        self.file
            .environments
            .iter()
            .map(|environment| {
                let text =
                    config::serialize(&config::to_postman_environment(environment, &secret_names))?;
                Ok((
                    format!(
                        "{}.postman_environment.json",
                        config::slug(&environment.name)
                    ),
                    text,
                ))
            })
            .collect()
    }

    /// Deletes the collection file, and the credentials, session and cookies this profile
    /// keeps for it.
    pub fn remove(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let fs = self.fs.clone();
        let file_path = self.file_path.clone();
        let state_path = state_file_path(&self.id);
        let credentials_provider = self.credentials_provider.clone();
        let urls = [
            secrets_url(&self.id),
            session_url(&self.id),
            state_url(&self.id),
        ];
        self._watch_task = Task::ready(());
        cx.spawn(async move |_, cx| {
            let options = fs::RemoveOptions {
                recursive: false,
                ignore_if_not_exists: true,
            };
            fs.remove_file(&file_path, options).await?;
            fs.remove_file(&state_path, options).await?;
            for url in urls {
                // Deleting an entry that was never written fails on some keychains.
                if credentials_provider
                    .read_credentials(&url, cx)
                    .await?
                    .is_some()
                {
                    credentials_provider.delete_credentials(&url, cx).await?;
                }
            }
            Ok(())
        })
    }

    pub fn prepare(&self, operation_key: &str, draft: &RequestDraft) -> Result<PreparedRequest> {
        let spec = self
            .spec
            .as_ref()
            .with_context(|| t!("api_client.collection.error_spec_not_loaded").to_string())?;
        let with_ad_hoc;
        let spec = match ad_hoc_operation(spec, operation_key, draft) {
            Some(operation) => {
                let mut copy = Spec::clone(spec);
                copy.operations.push(operation);
                with_ad_hoc = copy;
                &with_ad_hoc
            }
            None => spec.as_ref(),
        };
        let mut prepared = send::prepare(
            spec,
            operation_key,
            draft,
            &self.file.headers,
            &|name: &str| self.variable(name),
        )?;
        // Cookies the request sets itself go first; the jar's follow them.
        if let Some(jar_cookies) = self
            .cookies
            .header_for(&prepared.url, Utc::now().timestamp())
        {
            match prepared
                .headers
                .iter_mut()
                .find(|(name, _)| name.eq_ignore_ascii_case("cookie"))
            {
                Some((_, value)) => *value = format!("{value}; {jar_cookies}"),
                None => prepared.headers.push(("Cookie".to_string(), jar_cookies)),
            }
        }
        Ok(prepared)
    }

    fn store_cookies(
        &mut self,
        response: &ReceivedResponse,
        url: &str,
        cx: &mut Context<Self>,
    ) -> Vec<Cookie> {
        let set = self
            .cookies
            .store(&response.headers, url, Utc::now().timestamp());
        if !set.is_empty() {
            self.save_state(cx);
            cx.notify();
        }
        set
    }

    pub fn clear_cookies(&mut self, cx: &mut Context<Self>) {
        self.cookies = CookieJar::default();
        self.save_state(cx);
        cx.notify();
    }

    fn save_state(&mut self, cx: &mut Context<Self>) {
        if !self.file.auth.remember_session {
            return;
        }
        let state = StoredState {
            cookies: self.cookies.clone(),
            captured: self.captured.clone(),
        };
        let path = state_file_path(&self.id);
        let fs = self.fs.clone();
        cx.spawn(async move |this, cx| {
            let result = write_state_file(&fs, &path, &state).await;
            report_storage_result(
                &this,
                t!("api_client.collection.storage_cookies"),
                result,
                cx,
            );
        })
        .detach();
    }

    pub fn capture_rules(&self, operation_key: &str) -> Vec<CaptureRule> {
        self.file
            .captures
            .iter()
            .filter(|rule| rule.operation == operation_key)
            .cloned()
            .collect()
    }

    pub fn set_capture_rules(
        &mut self,
        operation_key: &str,
        rules: Vec<CaptureRule>,
        cx: &mut Context<Self>,
    ) {
        if self.capture_rules(operation_key) == rules {
            return;
        }
        self.update_file(
            |file| {
                file.captures.retain(|rule| rule.operation != operation_key);
                file.captures.extend(rules);
            },
            cx,
        );
    }

    pub fn captured_value(&self, variable: &str) -> Option<&str> {
        self.captured.get(variable).map(String::as_str)
    }

    /// Takes the operation's capture rules out of a successful JSON response.
    fn apply_captures(
        &mut self,
        operation_key: &str,
        response: &ReceivedResponse,
        cx: &mut Context<Self>,
    ) -> Vec<Capture> {
        let rules = self.capture_rules(operation_key);
        if rules.is_empty() || !(200..300).contains(&response.status) {
            return Vec::new();
        }
        let body = response.json();
        let captures: Vec<Capture> = rules
            .into_iter()
            .filter(|rule| !rule.variable.trim().is_empty() && !rule.path.trim().is_empty())
            .map(|rule| Capture {
                value: body
                    .as_ref()
                    .and_then(|body| jsonpath::select_string(body, &rule.path)),
                variable: rule.variable.trim().to_string(),
                path: rule.path,
            })
            .collect();
        for capture in &captures {
            if let Some(value) = &capture.value {
                self.captured
                    .insert(capture.variable.clone(), value.clone());
            }
        }
        if captures.iter().any(|capture| capture.value.is_some()) {
            self.save_state(cx);
            cx.notify();
        }
        captures
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
                        message: t!("api_client.collection.invalid_json", error = error)
                            .to_string(),
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
    pub fn login(&mut self, cx: &mut Context<Self>) -> Task<Result<TimelineStep, FailedRenewal>> {
        self.login_status = LoginStatus::LoggingIn;
        cx.notify();
        let operation_key = self
            .file
            .auth
            .login
            .as_ref()
            .map(|login| login.operation.clone());
        let prepared = self.login_request();
        cx.spawn(async move |this, cx| {
            let result = match prepared {
                Ok((login, prepared)) => {
                    let (mut step, response, _) = run_step(&this, &prepared, "login", cx).await;
                    match response.and_then(|response| session_from_response(&login, &response)) {
                        Ok(session) => Ok((step, session)),
                        Err(error) => {
                            step.note = failed_note(&error);
                            Err(FailedRenewal {
                                steps: vec![step],
                                error,
                            })
                        }
                    }
                }
                Err(error) => Err(FailedRenewal {
                    steps: operation_key
                        .map(|key| vec![unsent_step(&key, &error)])
                        .unwrap_or_default(),
                    error,
                }),
            };
            this.update(cx, |this, cx| match result {
                Ok((step, session)) => {
                    this.login_status = LoginStatus::Idle;
                    this.set_session(session, cx);
                    Ok(step)
                }
                Err(failure) => {
                    this.login_status = LoginStatus::Failed(format!("{:#}", failure.error).into());
                    cx.notify();
                    Err(failure)
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
            .with_context(|| t!("api_client.collection.error_no_login").to_string())?;
        let draft = self.auth_draft(&login, &login.operation, Some(login.body.clone()))?;
        let prepared = self.prepare(&login.operation, &draft)?;
        if let Some(missing) = prepared.missing.first() {
            return Err(anyhow!(
                "{}",
                t!(
                    "api_client.collection.error_login_missing_variable",
                    variable = format!("{{{{{missing}}}}}")
                )
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
            .with_context(|| t!("api_client.collection.error_no_login").to_string())?;
        let operation_key = login
            .refresh_operation
            .clone()
            .with_context(|| t!("api_client.collection.error_no_refresh").to_string())?;
        let draft = self.auth_draft(&login, &operation_key, login.refresh_body.clone())?;
        let prepared = self.prepare(&operation_key, &draft)?;
        Ok((login, prepared))
    }

    /// The draft of the login or refresh request: the operation's own server (else `baseUrl`)
    /// and the body in the format the login asks for.
    fn auth_draft(
        &self,
        login: &LoginConfig,
        operation_key: &str,
        body: Option<String>,
    ) -> Result<RequestDraft> {
        let spec = self
            .spec
            .as_ref()
            .with_context(|| t!("api_client.collection.error_spec_not_loaded").to_string())?;
        let operation_gone = || {
            t!(
                "api_client.send.error_operation_gone",
                operation = operation_key
            )
            .to_string()
        };
        let operation = spec.operation(operation_key);
        let url = match (&login.base_url, operation) {
            (Some(base_url), _) => {
                let path = match operation {
                    Some(operation) => operation.path.clone(),
                    None => operation_key
                        .split_once(char::is_whitespace)
                        .map(|(_, target)| target.trim().to_string())
                        .with_context(operation_gone)?,
                };
                format!("{}{path}", base_url.trim().trim_end_matches('/'))
            }
            (None, Some(operation)) => operation.url_template(),
            (None, None) => {
                ad_hoc_draft(operation_key)
                    .with_context(operation_gone)?
                    .url
            }
        };
        let documented = operation.and_then(|operation| operation.request_body.as_ref());
        let documented_kind =
            documented.map(|body| BodyKind::for_content_type(&body.content_type));
        // The body editor prefers JSON when a body offers it, but a token endpoint that
        // accepts a form at all is an OAuth one, and those reject JSON (Keycloak's 415).
        let documents_form = documented.is_some_and(|body| {
            std::iter::once(&body.content_type)
                .chain(&body.other_content_types)
                .any(|content_type| {
                    BodyKind::for_content_type(content_type) == BodyKind::UrlEncoded
                })
        });
        let as_form = match login.format {
            Some(LoginBodyFormat::Form) => true,
            Some(LoginBodyFormat::Json) => false,
            None => documents_form,
        };
        let content_type = if as_form {
            Some("application/x-www-form-urlencoded")
        } else if documented_kind != Some(BodyKind::Json) || documents_form {
            Some("application/json")
        } else {
            None
        };
        Ok(RequestDraft {
            url,
            headers: content_type
                .filter(|_| body.is_some())
                .map(|content_type| HeaderEntry {
                    name: "Content-Type".to_string(),
                    value: content_type.to_string(),
                    enabled: true,
                })
                .into_iter()
                .collect(),
            body,
            ..RequestDraft::default()
        })
    }

    fn is_auth_operation(&self, operation_key: &str) -> bool {
        self.file.auth.login.as_ref().is_some_and(|login| {
            login.operation == operation_key
                || login.refresh_operation.as_deref() == Some(operation_key)
        })
    }

    /// A login or refresh sent by hand goes out the way the automatic one does: same URL,
    /// same body format, the saved body when none was typed. Otherwise a stale draft or the
    /// spec's JSON preference sends a request the identity provider rejects.
    fn auth_aware_draft(&self, operation_key: &str, mut draft: RequestDraft) -> RequestDraft {
        let Some(login) = self.file.auth.login.as_ref() else {
            return draft;
        };
        let saved_body = if login.operation == operation_key {
            Some(login.body.clone())
        } else if login.refresh_operation.as_deref() == Some(operation_key) {
            login.refresh_body.clone()
        } else {
            return draft;
        };
        // The draft starts with the schema's example; only a body someone typed replaces
        // the saved one, which holds the `{{secret.*}}` credentials.
        let example = self
            .spec
            .as_ref()
            .and_then(|spec| config::initial_draft(spec, operation_key).body);
        let body = draft
            .body
            .clone()
            .filter(|body| !body.trim().is_empty() && Some(body) != example.as_ref())
            .or(saved_body);
        let auth = match self.auth_draft(login, operation_key, body) {
            Ok(auth) => auth,
            Err(error) => {
                log::warn!("API: não deu para montar o login {operation_key}: {error:#}");
                return draft;
            }
        };
        draft.url = auth.url;
        draft.body = auth.body;
        let typed_content_type = draft
            .headers
            .iter()
            .any(|header| header.enabled && header.name.eq_ignore_ascii_case("content-type"));
        if !typed_content_type {
            draft.headers.extend(auth.headers);
        }
        draft
    }

    /// Rereads the settings file and the spec when they differ from what is loaded, so a
    /// request sent right after an edit uses it instead of waiting for the file watcher.
    fn reload_if_changed(&mut self, cx: &mut Context<Self>) -> Task<()> {
        let fs = self.fs.clone();
        let file_path = self.file_path.clone();
        cx.spawn(async move |this, cx| {
            if let Ok(text) = fs.load(&file_path).await {
                this.update(cx, |this, cx| this.apply_file_text(text, cx))
                    .log_err();
            }
            let Ok(Some((path, known))) = this.read_with(cx, |this, _| {
                (!this.file.spec.is_empty()).then(|| (this.spec_path(), this.spec_text.clone()))
            }) else {
                return;
            };
            let Ok(text) = fs.load(&path).await else {
                return;
            };
            if known.as_deref() == Some(text.as_str()) {
                return;
            }
            let loaded = parse_spec(path, text, cx).await;
            this.update(cx, |this, cx| this.apply_spec(loaded, cx))
                .log_err();
        })
    }

    /// Refreshes the token, falling back to logging in again.
    pub fn renew(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<TimelineStep>, FailedRenewal>> {
        if !self.can_refresh() {
            let login = self.login(cx);
            return cx.background_spawn(async move { login.await.map(|step| vec![step]) });
        }
        let prepared = self.refresh_request();
        cx.spawn(async move |this, cx| {
            let mut steps = Vec::new();
            match prepared {
                Ok((login, prepared)) => {
                    let (mut step, response, _) = run_step(
                        &this,
                        &prepared,
                        &t!("api_client.collection.step_refreshed"),
                        cx,
                    )
                    .await;
                    match response.and_then(|response| session_from_response(&login, &response)) {
                        Ok(session) => {
                            steps.push(step);
                            this.update(cx, |this, cx| this.set_session(session, cx))?;
                            return Ok(steps);
                        }
                        Err(error) => {
                            step.note = failed_note(&error);
                            steps.push(step);
                        }
                    }
                }
                Err(error) => log::warn!("API: renovação indisponível: {error:#}"),
            }
            let login = this.update(cx, |this, cx| this.can_log_in().then(|| this.login(cx)))?;
            match login {
                Some(login) => match login.await {
                    Ok(step) => {
                        steps.push(step);
                        Ok(steps)
                    }
                    Err(mut failure) => {
                        steps.append(&mut failure.steps);
                        Err(FailedRenewal {
                            steps,
                            error: failure.error,
                        })
                    }
                },
                None => Err(FailedRenewal {
                    steps,
                    error: anyhow!("{}", t!("api_client.collection.error_token_expired")),
                }),
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
        cx.spawn(async move |this, cx| {
            let result = write_json(&credentials_provider, &url, &session, cx).await;
            report_storage_result(
                &this,
                t!("api_client.collection.storage_session"),
                result,
                cx,
            );
        })
        .detach();
    }

    pub fn logout(&mut self, cx: &mut Context<Self>) {
        self.session = Session::default();
        // Servers often keep the session in a cookie too; leaving it would stay logged in.
        self.cookies = CookieJar::default();
        self.save_state(cx);
        self.login_status = LoginStatus::Idle;
        cx.notify();
        let url = session_url(&self.id);
        let credentials_provider = self.credentials_provider.clone();
        cx.spawn(async move |this, cx| {
            let result = credentials_provider.delete_credentials(&url, cx).await;
            report_storage_result(
                &this,
                t!("api_client.collection.storage_logout"),
                result,
                cx,
            );
        })
        .detach();
    }

    pub fn start_login(&mut self, cx: &mut Context<Self>) {
        let login = self.login(cx);
        self.login_task = Some(cx.background_spawn(async move {
            if let Err(failure) = login.await {
                log::info!("API: login falhou: {:#}", failure.error);
            }
        }));
    }

    pub fn send(
        &mut self,
        operation_key: String,
        draft: RequestDraft,
        cx: &mut Context<Self>,
    ) -> Task<Exchange> {
        self.send_saved(operation_key, draft, None, cx)
    }

    pub fn send_saved(
        &mut self,
        operation_key: String,
        draft: RequestDraft,
        saved_id: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Exchange> {
        let sent_at = Local::now();
        let reload = self.reload_if_changed(cx);
        cx.spawn(async move |this, cx| {
            reload.await;
            let draft = this
                .read_with(cx, |this, _| this.auth_aware_draft(&operation_key, draft.clone()))
                .unwrap_or(draft);
            let mut timeline = Vec::new();
            let mut cookies_set = Vec::new();
            let result = send_with_retries(
                &this,
                &operation_key,
                &draft,
                &mut timeline,
                &mut cookies_set,
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
            let captures = response
                .as_ref()
                .and_then(|response| {
                    this.update(cx, |this, cx| {
                        this.apply_captures(&operation_key, response, cx)
                    })
                    .ok()
                })
                .unwrap_or_default();
            let login_capture = response
                .as_ref()
                .and_then(|response| {
                    this.update(cx, |this, cx| {
                        this.take_login_response(&operation_key, response, cx)
                    })
                    .ok()
                })
                .flatten();
            let tests = response
                .as_ref()
                .zip(saved_id.as_deref())
                .and_then(|(response, saved_id)| {
                    this.update(cx, |this, cx| this.run_tests(saved_id, response, cx))
                        .ok()
                })
                .unwrap_or_default();
            Exchange {
                request: request.map(|request| request.masked()),
                response,
                error,
                timeline,
                validation,
                response_schema_name,
                sent_at,
                cookies_set,
                captures,
                login_capture,
                tests,
            }
        })
    }

    pub fn saved_scripts(&self, saved_id: &str) -> &[ImportedScript] {
        self.file
            .saved
            .iter()
            .find(|saved| saved.id == saved_id)
            .map_or(&[], |saved| saved.scripts.as_slice())
    }

    pub fn failing_tests(&self, saved_id: &str) -> usize {
        self.failing_tests.get(saved_id).copied().unwrap_or(0)
    }

    fn run_tests(
        &mut self,
        saved_id: &str,
        response: &ReceivedResponse,
        cx: &mut Context<Self>,
    ) -> Vec<script::TestResult> {
        let results: Vec<script::TestResult> = self
            .saved_scripts(saved_id)
            .iter()
            .filter(|script| script.listen == "test")
            .flat_map(|script| script::run(&script::parse(&script.source, true), response))
            .collect();
        let failing = results
            .iter()
            .filter(|result| matches!(result.outcome, script::Outcome::Failed(_)))
            .count();
        let previous = if failing == 0 {
            self.failing_tests.remove(saved_id)
        } else {
            self.failing_tests.insert(saved_id.to_string(), failing)
        };
        if previous.unwrap_or(0) != failing {
            cx.notify();
        }
        results
    }

    /// Sending the login request by hand logs in too, so the token isn't left only in the body.
    fn take_login_response(
        &mut self,
        operation_key: &str,
        response: &ReceivedResponse,
        cx: &mut Context<Self>,
    ) -> Option<LoginCapture> {
        let login = self
            .file
            .auth
            .login
            .clone()
            .filter(|login| login.operation == operation_key)?;
        if !(200..300).contains(&response.status) {
            return None;
        }
        let session = session_from_response(&login, response)
            .map_err(|error| log::info!("API: a resposta do login não deu sessão: {error:#}"))
            .ok()?;
        let expires_in = session
            .expires_at
            .map(|expires_at| expires_at - Utc::now().timestamp());
        self.set_session(session, cx);
        self.login_status = LoginStatus::Idle;
        Some(LoginCapture {
            token_path: login.token_path,
            refresh_token_path: login.refresh_token_path,
            expires_in,
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

/// The key of an operation the spec doesn't document, called as `METHOD /path` (against the
/// environment's `baseUrl`), `METHOD https://…` or `METHOD {{someUrl}}/path`. `None` when
/// `target` is none of these.
pub fn ad_hoc_operation_key(method: &str, target: &str) -> Option<String> {
    let method = method.trim().to_ascii_uppercase();
    let known = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
    let target = target.trim();
    let is_target = target.starts_with('/')
        || target.starts_with("https://")
        || target.starts_with("http://")
        || target.starts_with("{{");
    (known.contains(&method.as_str()) && is_target).then(|| format!("{method} {target}"))
}

/// What a draft for an undocumented operation starts with: the target as its URL.
fn ad_hoc_draft(operation_key: &str) -> Option<RequestDraft> {
    let (method, target) = operation_key.split_once(' ')?;
    ad_hoc_operation_key(method, target)?;
    Some(RequestDraft {
        url: if target.starts_with('/') {
            format!("{{{{baseUrl}}}}{target}")
        } else {
            target.to_string()
        },
        ..RequestDraft::default()
    })
}

/// A stand-in for an operation missing from the spec, so it goes through the same headers,
/// login and cookies as the documented ones. Its body kind follows the draft, since there is
/// no schema to say it.
fn ad_hoc_operation(
    spec: &Spec,
    operation_key: &str,
    draft: &RequestDraft,
) -> Option<spec::Operation> {
    if spec.operation(operation_key).is_some() {
        return None;
    }
    let (method, path) = operation_key.split_once(' ')?;
    ad_hoc_operation_key(method, path)?;
    let content_type = draft
        .headers
        .iter()
        .find(|header| header.enabled && header.name.eq_ignore_ascii_case("content-type"))
        .map(|header| header.value.clone());
    let enabled_fields = || draft.form.iter().filter(|field| field.enabled);
    let request_body = if enabled_fields().next().is_some() {
        let has_file = enabled_fields().any(|field| field.is_file);
        Some(content_type.unwrap_or_else(|| {
            if has_file {
                "multipart/form-data".to_string()
            } else {
                "application/x-www-form-urlencoded".to_string()
            }
        }))
    } else {
        content_type
    }
    .map(|content_type| spec::RequestBody {
        content_type,
        other_content_types: Vec::new(),
        required: false,
        schema: None,
        example: None,
    });
    Some(spec::Operation {
        key: operation_key.to_string(),
        method: method.to_string(),
        path: path.to_string(),
        operation_id: None,
        summary: None,
        description: None,
        tags: Vec::new(),
        deprecated: false,
        parameters: Vec::new(),
        request_body,
        responses: Vec::new(),
        security: None,
        servers: Vec::new(),
    })
}

async fn send_with_retries(
    this: &WeakEntity<Collection>,
    operation_key: &str,
    draft: &RequestDraft,
    timeline: &mut Vec<TimelineStep>,
    cookies_set: &mut Vec<Cookie>,
    cx: &mut AsyncApp,
) -> SendResult {
    // Logging in by hand is the renewal; renewing first, or again after its 401, would
    // send a second login nobody asked for.
    let is_auth_operation = this
        .read_with(cx, |this, _| this.is_auth_operation(operation_key))
        .unwrap_or(false);
    let renewal = this
        .update(cx, |this, cx| {
            (!is_auth_operation && this.session_needs_renewal()).then(|| this.renew(cx))
        })
        .map_err(|error| Box::new((None, error)))?;
    if let Some(renewal) = renewal {
        match renewal.await {
            Ok(steps) => timeline.extend(steps),
            Err(failure) => {
                log::info!("API: renovação antecipada falhou: {:#}", failure.error);
                timeline.extend(failure.steps);
            }
        }
    }

    let prepare = |cx: &mut AsyncApp| {
        this.read_with(cx, |this, _| this.prepare(operation_key, draft))
            .and_then(|prepared| prepared)
            .map_err(|error| Box::new((None, error)))
    };
    let prepared = prepare(cx)?;
    let (step, response, cookies) = run_step(this, &prepared, "", cx).await;
    timeline.push(step);
    cookies_set.extend(cookies);
    let response = response.map_err(|error| Box::new((Some(prepared.clone()), error)))?;

    let carried_token = prepared.headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("authorization") && value.starts_with("Bearer ")
    });
    let may_retry = response.status == 401
        && !is_auth_operation
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
        step.note = t!("api_client.collection.step_token_rejected").to_string();
    }
    let renewal = this
        .update(cx, |this, cx| this.renew(cx))
        .map_err(|error| Box::new((Some(prepared.clone()), error)))?;
    match renewal.await {
        Ok(steps) => timeline.extend(steps),
        Err(failure) => {
            log::info!("API: renovação depois do 401 falhou: {:#}", failure.error);
            timeline.extend(failure.steps);
            return Ok((prepared, response));
        }
    }
    let prepared = prepare(cx)?;
    let (step, retried, cookies) = run_step(
        this,
        &prepared,
        &t!("api_client.collection.step_retried"),
        cx,
    )
    .await;
    timeline.push(step);
    cookies_set.extend(cookies);
    let retried = retried.map_err(|error| Box::new((Some(prepared.clone()), error)))?;
    Ok((prepared, retried))
}

/// Sends one request of an exchange and keeps the cookies it sets.
async fn run_step(
    this: &WeakEntity<Collection>,
    prepared: &PreparedRequest,
    note: &str,
    cx: &mut AsyncApp,
) -> (TimelineStep, Result<ReceivedResponse>, Vec<Cookie>) {
    let started = std::time::Instant::now();
    let result = match this.read_with(cx, |this, _| (this.http_client.clone(), this.fs.clone())) {
        Ok((http_client, fs)) => send::execute(http_client, Some(&fs), prepared).await,
        Err(error) => Err(error),
    };
    let cookies = match &result {
        Ok(response) => this
            .update(cx, |this, cx| {
                this.store_cookies(response, &prepared.url, cx)
            })
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    let path = prepared
        .url
        .split_once("://")
        .and_then(|(_, rest)| rest.find('/').map(|index| rest[index..].to_string()))
        .unwrap_or_else(|| prepared.url.clone());
    let content_type = prepared
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.clone());
    let step = TimelineStep {
        method: prepared.method.clone(),
        path,
        url: prepared.url.clone(),
        content_type,
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
    (step, result, cookies)
}

struct LoadedSpec {
    spec: Spec,
    root: Map<String, Value>,
    text: String,
}

async fn parse_spec(path: PathBuf, text: String, cx: &mut AsyncApp) -> Result<LoadedSpec> {
    cx.background_spawn(async move {
        let spec = spec::load(&path, &text, &|path: &Path| {
            Ok(std::fs::read_to_string(path)?)
        })?;
        let root = schema::validation_root(&spec);
        anyhow::Ok(LoadedSpec { spec, root, text })
    })
    .await
}

/// A login or token refresh that didn't work, with the requests it made on the way so the
/// exchange shows them.
#[derive(Debug)]
pub struct FailedRenewal {
    pub steps: Vec<TimelineStep>,
    pub error: anyhow::Error,
}

impl From<anyhow::Error> for FailedRenewal {
    fn from(error: anyhow::Error) -> Self {
        Self {
            steps: Vec::new(),
            error,
        }
    }
}

fn failed_note(error: &anyhow::Error) -> String {
    t!(
        "api_client.collection.step_failed",
        error = format!("{error:#}")
    )
    .to_string()
}

/// A request that couldn't even be built, so the timeline still says it was tried.
fn unsent_step(operation_key: &str, error: &anyhow::Error) -> TimelineStep {
    let (method, path) = operation_key.split_once(' ').unwrap_or(("", operation_key));
    TimelineStep {
        method: method.to_string(),
        path: path.to_string(),
        url: path.to_string(),
        content_type: None,
        status: None,
        note: failed_note(error),
        elapsed: Duration::ZERO,
    }
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
        return Err(anyhow!(
            "{}",
            t!(
                "api_client.collection.error_login_status",
                status = response.status,
                detail = detail
            )
        ));
    }
    let body = response
        .json()
        .with_context(|| t!("api_client.collection.error_login_not_json").to_string())?;
    let token = jsonpath::select_string(&body, &login.token_path).with_context(|| {
        t!(
            "api_client.collection.error_login_missing_token",
            path = &login.token_path
        )
        .to_string()
    })?;
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

fn state_url(id: &str) -> String {
    format!("asylum-api://{id}/state")
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

/// Where a collection's cookies and captured values live, in the active profile's data
/// directory.
fn state_file_path(id: &str) -> PathBuf {
    let file_name: String = id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect();
    paths::data_dir()
        .join("api_client")
        .join(format!("{file_name}.state.json"))
}

async fn write_state_file(fs: &Arc<dyn Fs>, path: &Path, state: &StoredState) -> Result<()> {
    if let Some(directory) = path.parent() {
        fs.create_dir(directory).await.with_context(|| {
            t!(
                "api_client.collection.error_create_dir",
                path = directory.display()
            )
            .to_string()
        })?;
    }
    let text = serde_json::to_string(state)?;
    fs.atomic_write(path.to_path_buf(), text)
        .await
        .with_context(|| {
            t!(
                "api_client.collection.error_write_file",
                path = path.display()
            )
            .to_string()
        })
}

/// Reads the stored state from its file. Earlier versions kept it in the keychain, so an entry
/// found there is moved to the file and then removed, keeping the cookies of an existing install.
async fn load_state(
    fs: &Arc<dyn Fs>,
    credentials_provider: &Arc<dyn CredentialsProvider>,
    url: &str,
    path: &Path,
    cx: &AsyncApp,
) -> Option<StoredState> {
    if fs.is_file(path).await {
        return match fs.load(path).await {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|error| {
                    log::error!(
                        "API: arquivo de estado ilegível ({}): {error}",
                        path.display()
                    )
                })
                .ok(),
            Err(error) => {
                log::error!("API: falha ao ler {}: {error:#}", path.display());
                None
            }
        };
    }

    let state = read_json::<StoredState>(credentials_provider, url, cx).await?;
    match write_state_file(fs, path, &state).await {
        Ok(()) => {
            if let Err(error) = credentials_provider.delete_credentials(url, cx).await {
                log::error!(
                    "API: estado migrado, mas a entrada antiga do keychain ficou ({url}): {error:#}"
                );
            }
        }
        // The keychain entry stays, so the next launch tries again.
        Err(error) => log::error!("API: falha ao migrar o estado do keychain: {error:#}"),
    }
    Some(state)
}

/// Shows a failed save in the panel instead of leaving it to the log, and clears the message
/// once a save goes through.
fn report_storage_result(
    this: &WeakEntity<Collection>,
    what: SharedString,
    result: Result<()>,
    cx: &mut AsyncApp,
) {
    let error = match result {
        Ok(()) => None,
        Err(error) => {
            log::error!("API: não foi possível salvar {what}: {error:#}");
            Some(t!(
                "api_client.collection.error_storage",
                what = what,
                error = format!("{error:#}")
            ))
        }
    };
    this.update(cx, |this, cx| {
        if this.storage_error != error {
            this.storage_error = error;
            cx.notify();
        }
    })
    .log_err();
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
    let file_path = unique_file_path(&fs, &directory, &slug).await;
    fs.atomic_write(file_path.clone(), config::serialize(&file)?)
        .await?;
    Ok(file_path)
}

/// An empty collection file with no spec, for an import, named after what is imported.
pub async fn create_for_import(fs: Arc<dyn Fs>, root: PathBuf, name: &str) -> Result<PathBuf> {
    let directory = root.join(config::COLLECTIONS_DIR);
    fs.create_dir(&directory).await?;
    let file_path = unique_file_path(&fs, &directory, &config::slug(name)).await;
    fs.atomic_write(
        file_path.clone(),
        config::serialize(&CollectionFile::default())?,
    )
    .await?;
    Ok(file_path)
}

async fn unique_file_path(fs: &Arc<dyn Fs>, directory: &Path, slug: &str) -> PathBuf {
    let mut file_path = directory.join(format!("{slug}.json"));
    let mut suffix = 2;
    while fs.is_file(&file_path).await {
        file_path = directory.join(format!("{slug}-{suffix}.json"));
        suffix += 1;
    }
    file_path
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
    use gpui::{Entity, TestAppContext};
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
                    let cookie = request
                        .headers()
                        .get("cookie")
                        .map(|value| format!(" cookie={}", value.to_str().unwrap_or("")))
                        .unwrap_or_default();
                    let line = format!(
                        "{} {}{} {authorization}{cookie}",
                        request.method(),
                        request.uri().path(),
                        request
                            .uri()
                            .query()
                            .map(|query| format!("?{query}"))
                            .unwrap_or_default(),
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
                    let mut response = Response::builder()
                        .status(status)
                        .header("content-type", "application/json");
                    if status == 200 && request.uri().path() == "/me" {
                        response = response.header("set-cookie", "sid=s1; Path=/; HttpOnly");
                    }
                    Ok(response.body(AsyncBody::from(body.to_string())).unwrap())
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
        assert_eq!(exchange.timeline[0].note, "token rejected");
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

        let ad_hoc = ad_hoc_operation_key("post", "/reports/7").unwrap();
        assert_eq!(ad_hoc, "POST /reports/7");
        assert_eq!(ad_hoc_operation_key("FETCH", "/x"), None);
        assert_eq!(ad_hoc_operation_key("GET", "reports"), None);
        assert_eq!(
            ad_hoc_operation_key("post", "{{authUrl}}/token").as_deref(),
            Some("POST {{authUrl}}/token")
        );
        let mut draft = collection.read_with(cx, |collection, _| collection.draft(&ad_hoc));
        assert_eq!(draft.url, "{{baseUrl}}/reports/7");
        draft.body = Some(r#"{"format":"csv"}"#.to_string());
        let ad_hoc_exchange = collection
            .update(cx, |collection, cx| {
                collection.send(ad_hoc.clone(), draft, cx)
            })
            .await;
        assert_eq!(ad_hoc_exchange.error, None);
        assert_eq!(
            requests.lock().last().map(String::as_str),
            Some("POST /reports/7 Bearer new cookie=sid=s1"),
            "an operation missing from the spec still gets the login and the cookies"
        );

        assert_eq!(exchange.cookies_set.len(), 1);
        collection.update(cx, |collection, cx| {
            collection.set_capture_rules(
                "GET /me",
                vec![CaptureRule {
                    operation: "GET /me".to_string(),
                    path: "$.id".to_string(),
                    variable: "userId".to_string(),
                }],
                cx,
            );
        });
        let exchange = collection
            .update(cx, |collection, cx| {
                let draft = collection.draft("GET /me");
                collection.send("GET /me".to_string(), draft, cx)
            })
            .await;
        assert_eq!(
            exchange.captures,
            vec![Capture {
                variable: "userId".to_string(),
                path: "$.id".to_string(),
                value: Some("7".to_string()),
            }]
        );
        let mut draft = collection.read_with(cx, |collection, _| collection.draft("GET /me"));
        draft.query.push(crate::config::ParamEntry {
            name: "user".to_string(),
            value: "{{userId}}".to_string(),
            enabled: true,
        });
        collection
            .update(cx, |collection, cx| {
                collection.send("GET /me".to_string(), draft, cx)
            })
            .await;
        assert_eq!(
            requests.lock().last().map(String::as_str),
            Some("GET /me?user=7 Bearer new cookie=sid=s1")
        );
        cx.run_until_parked();
        assert!(
            keychain.0.lock().get(&state_url("trix")).is_none(),
            "cookies no longer go to the keychain"
        );
        let state = fs.load(&state_file_path("trix")).await.unwrap();
        let state: StoredState = serde_json::from_str(&state).unwrap();
        assert_eq!(state.captured.get("userId").map(String::as_str), Some("7"));
        assert_eq!(state.cookies.cookies[0].name, "sid");
    }

    const KEYCLOAK_SPEC: &str = r##"
openapi: 3.0.3
info: { title: Backoffice, version: "1" }
security: [{ bearerAuth: [] }]
components:
  securitySchemes:
    bearerAuth: { type: http, scheme: bearer }
paths:
  /auth/realms/backoffice/protocol/openid-connect/token:
    servers: [{ url: '{{authUrl}}' }]
    post:
      security: []
      requestBody:
        content: { application/x-www-form-urlencoded: { schema: { properties: { grant_type: { type: string }, username: { type: string }, password: { type: string } } } } }
      responses: { '200': { description: ok } }
  /me:
    get:
      responses: { '200': { description: ok } }
"##;

    #[gpui::test]
    async fn logs_in_on_the_operation_server_with_a_form_body(cx: &mut TestAppContext) {
        const TOKEN_PATH: &str = "/auth/realms/backoffice/protocol/openid-connect/token";
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            serde_json::json!({ "openapi.yml": KEYCLOAK_SPEC }),
        )
        .await;
        let keychain = Arc::new(MemoryKeychain::default());
        let session = serde_json::to_vec(&Session {
            token: Some("old".to_string()),
            refresh_token: None,
            expires_at: None,
        })
        .unwrap();
        keychain
            .0
            .lock()
            .insert(session_url("backoffice"), ("asylum".to_string(), session));

        let requests: Arc<Mutex<Vec<String>>> = Arc::default();
        let http_client = FakeHttpClient::create({
            let requests = requests.clone();
            move |mut request| {
                let requests = requests.clone();
                async move {
                    let header = |name: &str| {
                        request
                            .headers()
                            .get(name)
                            .map(|value| value.to_str().unwrap_or("").to_string())
                            .unwrap_or_default()
                    };
                    let authorization = header("authorization");
                    let content_type = header("content-type");
                    let mut body = String::new();
                    futures::AsyncReadExt::read_to_string(request.body_mut(), &mut body)
                        .await
                        .unwrap();
                    let method = request.method().to_string();
                    let host = request.uri().host().unwrap_or("").to_string();
                    let path = request.uri().path().to_string();
                    requests.lock().push(
                        format!("{method} {host}{path} {authorization}{content_type} {body}")
                            .trim_end()
                            .to_string(),
                    );
                    let (status, response_body) =
                        match (host.as_str(), path.as_str(), authorization.as_str()) {
                            ("api.test", "/me", "Bearer new") => (200, r#"{"id":7}"#),
                            ("api.test", "/me", _) => (401, r#"{"message":"jwt expired"}"#),
                            ("keycloak.test", TOKEN_PATH, _)
                                if content_type == "application/x-www-form-urlencoded" =>
                            {
                                (200, r#"{"access_token":"new","expires_in":300}"#)
                            }
                            _ => (404, "{}"),
                        };
                    Ok(Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(AsyncBody::from(response_body.to_string()))
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
        let collection =
            cx.new(|cx| Collection::new(PathBuf::from("/project"), file_path, fs.clone(), cx));
        cx.run_until_parked();
        let login_key = format!("POST {TOKEN_PATH}");
        collection.update(cx, |collection, cx| {
            assert!(collection.spec.is_some(), "{:?}", collection.spec_error);
            assert_eq!(
                collection.draft(&login_key).url,
                format!("{{{{authUrl}}}}{TOKEN_PATH}"),
                "the path's own server replaces baseUrl"
            );
            collection.update_file(
                |file| {
                    file.environments[0].set("baseUrl", "https://api.test".to_string());
                    file.environments[0].set("authUrl", "https://wrong.test".to_string());
                    file.auth.login = Some(LoginConfig {
                        operation: login_key.clone(),
                        scheme: "bearerAuth".to_string(),
                        body: r#"{"grant_type":"password","username":"{{secret.username}}","password":"{{secret.password}}"}"#
                            .to_string(),
                        token_path: "$.access_token".to_string(),
                        expires_in_path: Some("$.expires_in".to_string()),
                        ..LoginConfig::default()
                    });
                },
                cx,
            );
            collection.set_secret("username", "vini".to_string(), cx);
            collection.set_secret("password", "a&b".to_string(), cx);
        });

        let send_me = |collection: &Entity<Collection>, cx: &mut TestAppContext| {
            collection.update(cx, |collection, cx| {
                let draft = collection.draft("GET /me");
                collection.send("GET /me".to_string(), draft, cx)
            })
        };

        let failed = send_me(&collection, cx).await;
        assert_eq!(
            failed.response.as_ref().map(|response| response.status),
            Some(401)
        );
        let login_step = failed
            .timeline
            .iter()
            .find(|step| step.path == TOKEN_PATH)
            .expect("the failed login shows in the exchange");
        assert_eq!(login_step.status, Some(404));
        assert!(login_step.note.contains("404"), "{}", login_step.note);

        collection.update(cx, |collection, cx| {
            collection.update_file(
                |file| file.environments[0].set("authUrl", "https://keycloak.test".to_string()),
                cx,
            );
        });
        requests.lock().clear();
        let exchange = send_me(&collection, cx).await;
        assert_eq!(exchange.error, None);
        assert_eq!(
            exchange.response.as_ref().map(|response| response.status),
            Some(200)
        );
        assert_eq!(
            *requests.lock(),
            vec![
                "GET api.test/me Bearer old".to_string(),
                format!(
                    "POST keycloak.test{TOKEN_PATH} application/x-www-form-urlencoded \
                     grant_type=password&username=vini&password=a%26b"
                ),
                "GET api.test/me Bearer new".to_string(),
            ]
        );
    }

    const KEYCLOAK_JSON_AND_FORM_SPEC: &str = r##"
openapi: 3.0.3
info: { title: Backoffice, version: "1" }
paths:
  /auth/realms/backoffice/protocol/openid-connect/token:
    servers: [{ url: '{{authUrl}}' }]
    post:
      security: []
      requestBody:
        content:
          application/json: { schema: { properties: { grant_type: { type: string } } } }
          application/x-www-form-urlencoded: { schema: { properties: { grant_type: { type: string } } } }
      responses: { '200': { description: ok } }
"##;

    #[gpui::test]
    async fn logs_in_by_hand_the_way_the_renewal_does(cx: &mut TestAppContext) {
        const TOKEN_PATH: &str = "/auth/realms/backoffice/protocol/openid-connect/token";
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            serde_json::json!({ "openapi.yml": KEYCLOAK_JSON_AND_FORM_SPEC }),
        )
        .await;
        let requests: Arc<Mutex<Vec<String>>> = Arc::default();
        let http_client = FakeHttpClient::create({
            let requests = requests.clone();
            move |mut request| {
                let requests = requests.clone();
                async move {
                    let content_type = request
                        .headers()
                        .get("content-type")
                        .map(|value| value.to_str().unwrap_or("").to_string())
                        .unwrap_or_default();
                    let mut body = String::new();
                    futures::AsyncReadExt::read_to_string(request.body_mut(), &mut body)
                        .await
                        .unwrap();
                    let host = request.uri().host().unwrap_or("").to_string();
                    let path = request.uri().path().to_string();
                    requests
                        .lock()
                        .push(format!("{host}{path} {content_type} {body}"));
                    let (status, response_body) = if body.contains("password=wrong") {
                        (401, r#"{"error":"invalid_grant"}"#)
                    } else {
                        (200, r#"{"access_token":"new","expires_in":300}"#)
                    };
                    Ok(Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(AsyncBody::from(response_body.to_string()))
                        .unwrap())
                }
            }
        });
        cx.update(|cx| {
            cx.set_global(db::AppDatabase::test_new());
            cx.set_global(zed_credentials_provider::ZedCredentialsProvider(Arc::new(
                MemoryKeychain::default(),
            )));
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
        let collection =
            cx.new(|cx| Collection::new(PathBuf::from("/project"), file_path, fs.clone(), cx));
        cx.run_until_parked();
        let login_key = format!("POST {TOKEN_PATH}");
        collection.update(cx, |collection, cx| {
            assert!(collection.spec.is_some(), "{:?}", collection.spec_error);
            collection.update_file(
                |file| {
                    file.environments[0].set("authUrl", "https://keycloak.test".to_string());
                    file.auth.login = Some(LoginConfig {
                        operation: login_key.clone(),
                        scheme: "bearerAuth".to_string(),
                        body: r#"{"grant_type":"password","password":"{{secret.password}}"}"#
                            .to_string(),
                        token_path: "$.access_token".to_string(),
                        ..LoginConfig::default()
                    });
                },
                cx,
            );
            collection.set_secret("password", "right".to_string(), cx);
        });
        cx.run_until_parked();

        let send_login = |collection: &Entity<Collection>, cx: &mut TestAppContext| {
            let login_key = login_key.clone();
            collection.update(cx, |collection, cx| {
                let draft = collection.draft(&login_key);
                collection.send(login_key, draft, cx)
            })
        };

        let exchange = send_login(&collection, cx).await;
        assert_eq!(exchange.error, None);
        assert_eq!(
            *requests.lock(),
            vec![format!(
                "keycloak.test{TOKEN_PATH} application/x-www-form-urlencoded \
                 grant_type=password&password=right"
            )],
            "a token endpoint that accepts a form gets one, with the saved credentials"
        );
        let step = exchange.timeline.first().expect("the login step");
        assert_eq!(step.url, format!("https://keycloak.test{TOKEN_PATH}"));
        assert_eq!(
            step.content_type.as_deref(),
            Some("application/x-www-form-urlencoded")
        );

        requests.lock().clear();
        collection.update(cx, |collection, cx| {
            collection.set_secret("password", "wrong".to_string(), cx)
        });
        let rejected = send_login(&collection, cx).await;
        assert_eq!(
            rejected.response.as_ref().map(|response| response.status),
            Some(401)
        );
        assert_eq!(
            requests.lock().len(),
            1,
            "a rejected login is not retried by logging in again"
        );

        requests.lock().clear();
        collection.update(cx, |collection, cx| {
            collection.set_secret("password", "right".to_string(), cx)
        });
        fs.atomic_write(
            PathBuf::from("/project/openapi.yml"),
            KEYCLOAK_JSON_AND_FORM_SPEC.replace("'{{authUrl}}'", "https://direct.test"),
        )
        .await
        .unwrap();
        send_login(&collection, cx).await;
        assert!(
            requests.lock()[0].starts_with("direct.test/"),
            "the first request after an edit already uses it: {:?}",
            requests.lock()
        );
    }

    #[gpui::test]
    async fn moves_the_keychain_state_to_a_file(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", serde_json::json!({ "openapi.yml": SPEC }))
            .await;
        let keychain = Arc::new(MemoryKeychain::default());
        let mut captured = BTreeMap::default();
        captured.insert("userId".to_string(), "7".to_string());
        let state = serde_json::to_vec(&StoredState {
            cookies: CookieJar::default(),
            captured,
        })
        .unwrap();
        keychain
            .0
            .lock()
            .insert(state_url("trix"), ("asylum".to_string(), state));
        cx.update(|cx| {
            cx.set_global(db::AppDatabase::test_new());
            cx.set_global(zed_credentials_provider::ZedCredentialsProvider(
                keychain.clone(),
            ));
            cx.set_http_client(FakeHttpClient::with_404_response());
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
        let collection =
            cx.new(|cx| Collection::new(PathBuf::from("/project"), file_path, fs.clone(), cx));
        cx.run_until_parked();

        collection.read_with(cx, |collection, _| {
            assert_eq!(
                collection.captured.get("userId").map(String::as_str),
                Some("7")
            );
            assert_eq!(collection.storage_error, None);
        });
        assert!(keychain.0.lock().get(&state_url("trix")).is_none());
        let state = fs.load(&state_file_path("trix")).await.unwrap();
        let state: StoredState = serde_json::from_str(&state).unwrap();
        assert_eq!(state.captured.get("userId").map(String::as_str), Some("7"));
    }
}
