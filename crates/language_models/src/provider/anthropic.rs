pub mod telemetry;

use anthropic::oauth::{self, AnthropicOAuth};
use anthropic::{ANTHROPIC_API_URL, AnthropicError, AnthropicModelMode};
use anyhow::Result;
use collections::BTreeMap;
use credentials_provider::CredentialsProvider;
use futures::{FutureExt, StreamExt, future::BoxFuture, stream::BoxStream};
use gpui::{App, AppContext, AsyncApp, Context, Entity, SharedString, Task, Window};
use http_client::{CustomHeaders, HttpClient};
use language_model::{
    ANTHROPIC_PROVIDER_ID, ANTHROPIC_PROVIDER_NAME, ApiKeyState, AuthenticateError,
    CompactionResult, EnvVar, FastModeConfirmation, IconOrSvg, InlineDescription, LanguageModel,
    LanguageModelCompletionError, LanguageModelCompletionEvent, LanguageModelId, LanguageModelName,
    LanguageModelProvider, LanguageModelProviderId, LanguageModelProviderName,
    LanguageModelProviderState, LanguageModelRequest, LanguageModelToolChoice,
    ProviderSettingsView, RateLimiter, env_var,
};
use settings::{Settings, SettingsStore};
use std::sync::{Arc, LazyLock};
use ui::{ConfiguredApiCard, IconName, prelude::*};
use util::ResultExt as _;

use anthropic::completion::collect_compaction_result;
pub use anthropic::completion::{AnthropicEventMapper, AnthropicPromptCacheMode, into_anthropic};
pub use settings::AnthropicAvailableModel as AvailableModel;

const PROVIDER_ID: LanguageModelProviderId = ANTHROPIC_PROVIDER_ID;
const PROVIDER_NAME: LanguageModelProviderName = ANTHROPIC_PROVIDER_NAME;

#[derive(Default, Clone, Debug, PartialEq)]
pub struct AnthropicSettings {
    pub api_url: String,
    /// Extend Zed's list of Anthropic models.
    pub available_models: Vec<AvailableModel>,
    /// User-configured headers added to every Anthropic request.
    pub custom_headers: CustomHeaders,
}

/// Wrapper to store the Anthropic provider state as a GPUI Global so that the
/// Claude OAuth sign-in flow can access it without going through the registry.
struct AnthropicGlobal {
    state: Entity<State>,
}

impl gpui::Global for AnthropicGlobal {}

/// A lightweight handle to the Anthropic provider state, accessible via
/// `AnthropicLanguageModelProvider::global()`.
pub struct AnthropicProviderHandle {
    state: Entity<State>,
}

const SUBSCRIPTION_DESCRIPTION: &str =
    "Sign in with your Claude Pro, Max or Team subscription to use Anthropic models in Zed's agent.";

impl AnthropicProviderHandle {
    pub fn oauth_sign_out(&self, cx: &mut App) -> Task<Result<()>> {
        self.state
            .update(cx, |state, cx| state.clear_oauth(cx))
    }

    pub fn is_oauth_authenticated(&self, cx: &App) -> bool {
        self.state.read(cx).oauth.is_some()
    }

    /// Start the OAuth PKCE flow: stores the verifier in state and returns
    /// the authorization URL the user should open in a browser.
    pub fn start_oauth_flow(&self, cx: &mut App) -> Result<String> {
        let params = anthropic::oauth::build_authorize_url()?;
        self.state.update(cx, |state, _cx| {
            state.oauth_pending_verifier = Some(params.verifier);
        });
        Ok(params.url)
    }

    pub fn set_bearer_token(&self, token: String, cx: &mut App) -> Task<Result<()>> {
        let auth = AnthropicOAuth {
            refresh_token: String::new(),
            access_token: token,
            expires_ms: u64::MAX,
        };
        self.state.update(cx, |state, cx| state.set_oauth(auth, cx))
    }
}

pub struct AnthropicLanguageModelProvider {
    http_client: Arc<dyn HttpClient>,
    state: Entity<State>,
}

const API_KEY_ENV_VAR_NAME: &str = "ANTHROPIC_API_KEY";
static API_KEY_ENV_VAR: LazyLock<EnvVar> = env_var!(API_KEY_ENV_VAR_NAME);

pub(crate) const RESERVED_HEADER_NAMES: &[&str] =
    &["X-Api-Key", "Anthropic-Version", "Anthropic-Beta"];

const OAUTH_CREDENTIAL_URL: &str = "https://claude.ai/oauth/zed-fork";
const OAUTH_CREDENTIAL_USERNAME: &str = "anthropic-oauth";

pub struct State {
    api_key_state: ApiKeyState,
    oauth: Option<AnthropicOAuth>,
    /// PKCE verifier stored temporarily during OAuth sign-in flow.
    oauth_pending_verifier: Option<String>,
    credentials_provider: Arc<dyn CredentialsProvider>,
    http_client: Arc<dyn HttpClient>,
    fetched_models: Vec<anthropic::Model>,
    fetch_models_task: Option<Task<Result<()>>>,
    sign_in_task: Option<Task<Result<()>>>,
    last_auth_error: Option<SharedString>,
}

impl State {
    fn is_authenticated(&self) -> bool {
        self.api_key_state.has_key() || self.oauth.is_some()
    }

    /// Returns the effective API key: prefers the explicit API key, then falls
    /// back to the OAuth access token (refreshing if expired).
    fn effective_api_key(&self, api_url: &str) -> Option<Arc<str>> {
        if let Some(key) = self.api_key_state.key(api_url) {
            return Some(key);
        }
        if let Some(ref auth) = self.oauth {
            if !auth.is_expired() {
                return Some(Arc::from(auth.access_token()));
            }
        }
        None
    }

    /// Whether the current credential is an OAuth subscription token.
    fn is_using_oauth(&self, api_url: &str) -> bool {
        self.effective_api_key(api_url)
            .map_or(false, |key| anthropic::is_oauth_token(&key))
    }

    fn set_oauth(
        &mut self,
        auth: AnthropicOAuth,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let credentials_provider = self.credentials_provider.clone();
        let serialized = serde_json::to_string(&auth).unwrap_or_default();
        log::info!("Claude OAuth: storing token (expires_ms={})", auth.expires_ms);
        self.oauth = Some(auth);
        cx.notify();

        cx.spawn(async move |this, cx| {
            credentials_provider
                .write_credentials(
                    OAUTH_CREDENTIAL_URL,
                    OAUTH_CREDENTIAL_USERNAME,
                    serialized.as_bytes(),
                    cx,
                )
                .await?;
            log::info!("Claude OAuth: credentials persisted, fetching models");
            this.update(cx, |this, cx| this.restart_fetch_models_task(cx))
                .ok();
            Ok(())
        })
    }

    fn clear_oauth(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        self.oauth = None;
        self.fetched_models.clear();
        cx.notify();
        let credentials_provider = self.credentials_provider.clone();
        cx.spawn(async move |_this, cx| {
            credentials_provider
                .delete_credentials(OAUTH_CREDENTIAL_URL, cx)
                .await?;
            Ok(())
        })
    }

    fn load_oauth(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let credentials_provider = self.credentials_provider.clone();
        cx.spawn(async move |this, cx| {
            let credentials = credentials_provider
                .read_credentials(OAUTH_CREDENTIAL_URL, cx)
                .await?;
            if let Some((username, bytes)) = credentials {
                if username == OAUTH_CREDENTIAL_USERNAME {
                    let json = String::from_utf8(bytes)?;
                    let auth: AnthropicOAuth = serde_json::from_str(&json)?;
                    this.update(cx, |this, cx| {
                        this.oauth = Some(auth);
                        this.restart_fetch_models_task(cx);
                        cx.notify();
                    }).ok();
                }
            }
            Ok(())
        })
    }

    fn refresh_oauth_if_needed(&mut self, cx: &mut Context<Self>) -> Option<Task<Result<()>>> {
        let auth = self.oauth.as_ref()?;
        if !auth.is_expired() {
            return None;
        }

        log::info!("Claude OAuth: token expired, refreshing via Claude Code CLI");

        // If we have a real refresh token, try the OAuth refresh endpoint first.
        // Otherwise (CLI-imported tokens), re-run `claude auth token`.
        if !auth.refresh_token.is_empty() {
            let http_client = self.http_client.clone();
            let auth_clone = auth.clone();
            let credentials_provider = self.credentials_provider.clone();
            Some(cx.spawn(async move |this, cx| {
                let new_auth = oauth::refresh_token(http_client.as_ref(), &auth_clone)
                    .await
                    .map_err(|e| {
                        log::error!("Claude OAuth: token refresh failed: {e:#}");
                        e
                    })?;
                log::info!("Claude OAuth: token refreshed successfully");
                let serialized = serde_json::to_string(&new_auth).unwrap_or_default();
                credentials_provider
                    .write_credentials(
                        OAUTH_CREDENTIAL_URL,
                        OAUTH_CREDENTIAL_USERNAME,
                        serialized.as_bytes(),
                        cx,
                    )
                    .await?;
                this.update(cx, |this, cx| {
                    this.oauth = Some(new_auth);
                    this.restart_fetch_models_task(cx);
                    cx.notify();
                }).ok();
                Ok(())
            }))
        } else {
            Some(self.refresh_oauth_from_cli(cx))
        }
    }

    fn refresh_oauth_from_cli(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        self.oauth = None;
        cx.notify();
        Task::ready(Err(anyhow::anyhow!(
            "Claude OAuth token expired and has no refresh token. \
             Please sign in again via 'Claude OAuth Sign In' in the command palette."
        )))
    }

    fn set_api_key(&mut self, api_key: Option<String>, cx: &mut Context<Self>) -> Task<Result<()>> {
        // If the provided value looks like an OAuth authorization code (not a
        // standard Anthropic API key), perform the token exchange.
        //
        // The verifier is obtained from:
        //   1. A pending verifier stored by `ClaudeOAuthSignIn`, or
        //   2. The code itself if it's in `CODE#STATE` format (the state IS the
        //      verifier since we pass it as the `state` parameter in the authorize URL).
        if let Some(key) = &api_key {
            if !key.starts_with("sk-ant-api") {
                let has_hash = key.contains('#');
                log::info!(
                    "Claude OAuth: non-API-key value detected (len={}, has_hash={has_hash}, pending_verifier={})",
                    key.len(),
                    self.oauth_pending_verifier.is_some()
                );
                let verifier = self.oauth_pending_verifier.take().or_else(|| {
                    key.split_once('#').map(|(_, state)| state.to_string())
                });

                if let Some(verifier) = verifier {
                    log::info!("Claude OAuth: detected authorization code, performing token exchange");
                    let http_client = self.http_client.clone();
                    let code = key.clone();
                    return cx.spawn(async move |this, cx| {
                        match oauth::exchange_code(http_client.as_ref(), &code, &verifier).await {
                            Ok(auth) => {
                                log::info!("Claude OAuth: token exchange successful");
                                this.update(cx, |this, cx| this.set_oauth(auth, cx))?.await
                            }
                            Err(error) => {
                                log::error!("Claude OAuth: token exchange failed: {error}");
                                Err(error)
                            }
                        }
                    });
                } else {
                    log::warn!(
                        "Claude OAuth: value doesn't look like an API key and has no verifier. \
                         Use the full CODE#STATE value from the browser, or run \
                         'Claude OAuth Sign In' from the command palette first."
                    );
                }
            }
        }

        let credentials_provider = self.credentials_provider.clone();
        let api_url = AnthropicLanguageModelProvider::api_url(cx);
        let should_fetch_models = api_key.is_some();
        let task = self.api_key_state.store(
            api_url,
            api_key,
            |this| &mut this.api_key_state,
            credentials_provider,
            cx,
        );
        self.fetched_models.clear();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            if result.is_ok() && should_fetch_models {
                this.update(cx, |this, cx| this.restart_fetch_models_task(cx))
                    .ok();
            }
            result
        })
    }

    fn authenticate(&mut self, cx: &mut Context<Self>) -> Task<Result<(), AuthenticateError>> {
        let credentials_provider = self.credentials_provider.clone();
        let api_url = AnthropicLanguageModelProvider::api_url(cx);
        let api_key_task = self.api_key_state.load_if_needed(
            api_url,
            |this| &mut this.api_key_state,
            credentials_provider,
            cx,
        );
        let oauth_task = self.load_oauth(cx);

        cx.spawn(async move |this, cx| {
            let api_key_result = api_key_task.await;
            oauth_task.await.log_err();

            let has_oauth = this
                .read_with(cx, |this, _cx| this.oauth.is_some())
                .unwrap_or(false);

            if api_key_result.is_ok() || has_oauth {
                let refresh_task = this
                    .update(cx, |this, cx| this.refresh_oauth_if_needed(cx))
                    .ok()
                    .flatten();
                if let Some(task) = refresh_task {
                    task.await.log_err();
                }
                this.update(cx, |this, cx| {
                    this.restart_fetch_models_task(cx);
                })
                .ok();
                Ok(())
            } else {
                api_key_result
            }
        })
    }

    fn fetch_models(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let http_client = self.http_client.clone();
        let api_url = AnthropicLanguageModelProvider::api_url(cx);
        let Some(api_key) = self.effective_api_key(&api_url) else {
            log::warn!("Anthropic: cannot fetch models, no credentials available");
            return Task::ready(Err(anyhow::anyhow!(
                "cannot fetch Anthropic models without credentials"
            )));
        };
        let using_oauth = self.is_using_oauth(&api_url);
        log::info!(
            "Anthropic: fetching models (url={}, oauth={}, key_prefix={}…)",
            api_url,
            using_oauth,
            &api_key[..api_key.len().min(10)]
        );
        let extra_headers = AnthropicLanguageModelProvider::settings(cx)
            .custom_headers
            .clone();

        cx.spawn(async move |this, cx| {
            match anthropic::list_models(
                http_client.as_ref(),
                &api_url,
                api_key.as_ref(),
                using_oauth,
                &extra_headers,
            )
            .await
            .map_err(LanguageModelCompletionError::from)
            {
                Ok(models) => {
                    log::info!("Anthropic: fetched {} models", models.len());
                    this.update(cx, |this, cx| {
                        this.fetched_models = models;
                        cx.notify();
                    })
                }
                Err(error) => {
                    // An expired OAuth token shows up as a 401 here; refreshing it once
                    // lets the next fetch succeed instead of leaving the model list empty.
                    let unauthorized = matches!(
                        &error,
                        LanguageModelCompletionError::ProviderRejection {
                            status: Some(status),
                            ..
                        } if status.as_u16() == 401
                    );
                    if unauthorized {
                        log::warn!("Anthropic: 401 on fetch_models, attempting token refresh");
                        let refresh_result =
                            this.update(cx, |this, cx| this.refresh_oauth_if_needed(cx));
                        if let Ok(Some(task)) = refresh_result {
                            task.await.log_err();
                        }
                    }
                    log::error!("Anthropic: failed to fetch models: {error}");
                    Err(error.into())
                }
            }
        })
    }

    fn restart_fetch_models_task(&mut self, cx: &mut Context<Self>) {
        let task = self.fetch_models(cx);
        self.fetch_models_task.replace(task);
    }
}

impl AnthropicLanguageModelProvider {
    pub fn new(
        http_client: Arc<dyn HttpClient>,
        credentials_provider: Arc<dyn CredentialsProvider>,
        cx: &mut App,
    ) -> Self {
        let state = cx.new(|cx| {
            cx.observe_global::<SettingsStore>({
                let mut last_api_url = Self::api_url(cx);
                move |this: &mut State, cx| {
                    let credentials_provider = this.credentials_provider.clone();
                    let api_url = Self::api_url(cx);
                    let url_changed = api_url != last_api_url;
                    last_api_url = api_url.clone();
                    this.api_key_state.handle_url_change(
                        api_url,
                        |this| &mut this.api_key_state,
                        credentials_provider,
                        cx,
                    );
                    if url_changed {
                        this.fetched_models.clear();
                        this.authenticate(cx).detach();
                    }
                    cx.notify();
                }
            })
            .detach();
            State {
                api_key_state: ApiKeyState::new(Self::api_url(cx), (*API_KEY_ENV_VAR).clone()),
                oauth: None,
                oauth_pending_verifier: None,
                credentials_provider,
                http_client: http_client.clone(),
                fetched_models: Vec::new(),
                fetch_models_task: None,
                sign_in_task: None,
                last_auth_error: None,
            }
        });

        cx.set_global(AnthropicGlobal {
            state: state.clone(),
        });

        Self { http_client, state }
    }

    fn create_language_model(&self, model: anthropic::Model) -> Arc<dyn LanguageModel> {
        Arc::new(AnthropicModel {
            id: LanguageModelId::from(model.id.to_string()),
            model,
            state: self.state.clone(),
            http_client: self.http_client.clone(),
            request_limiter: RateLimiter::new(4),
        })
    }

    /// Access the global Anthropic provider state.
    pub fn global(cx: &mut App) -> Result<AnthropicProviderHandle> {
        let g = cx
            .try_global::<AnthropicGlobal>()
            .ok_or_else(|| anyhow::anyhow!("Anthropic provider not initialized"))?;
        Ok(AnthropicProviderHandle {
            state: g.state.clone(),
        })
    }

    /// Read-only access to the global Anthropic provider state.
    pub fn global_read(cx: &App) -> Result<AnthropicProviderHandle> {
        let g = cx
            .try_global::<AnthropicGlobal>()
            .ok_or_else(|| anyhow::anyhow!("Anthropic provider not initialized"))?;
        Ok(AnthropicProviderHandle {
            state: g.state.clone(),
        })
    }

    fn settings(cx: &App) -> &AnthropicSettings {
        &crate::AllLanguageModelSettings::get_global(cx).anthropic
    }

    fn api_url(cx: &App) -> SharedString {
        let api_url = &Self::settings(cx).api_url;
        if api_url.is_empty() {
            ANTHROPIC_API_URL.into()
        } else {
            SharedString::new(api_url.as_str())
        }
    }
}

impl LanguageModelProviderState for AnthropicLanguageModelProvider {
    type ObservableEntity = State;

    fn observable_entity(&self) -> Option<Entity<Self::ObservableEntity>> {
        Some(self.state.clone())
    }
}

impl LanguageModelProvider for AnthropicLanguageModelProvider {
    fn id(&self) -> LanguageModelProviderId {
        PROVIDER_ID
    }

    fn name(&self) -> LanguageModelProviderName {
        PROVIDER_NAME
    }

    fn icon(&self) -> IconOrSvg {
        IconOrSvg::Icon(IconName::AiAnthropic)
    }

    fn default_model(&self, cx: &App) -> Option<Arc<dyn LanguageModel>> {
        let fetched = self.state.read(cx).fetched_models.clone();
        // Pick the highest-version Sonnet we know about; otherwise the first
        // Claude model returned. Returning `None` until the fetch completes
        // matches the Ollama provider's behavior.
        pick_preferred_model(&fetched, &["claude-sonnet-", "claude-opus-", "claude-"])
            .map(|model| self.create_language_model(model))
    }

    fn default_fast_model(&self, cx: &App) -> Option<Arc<dyn LanguageModel>> {
        let fetched = self.state.read(cx).fetched_models.clone();
        pick_preferred_model(&fetched, &["claude-haiku-", "claude-"])
            .map(|model| self.create_language_model(model))
    }

    fn recommended_models(&self, cx: &App) -> Vec<Arc<dyn LanguageModel>> {
        let fetched = self.state.read(cx).fetched_models.clone();
        pick_preferred_model(&fetched, &["claude-sonnet-"])
            .map(|model| vec![self.create_language_model(model)])
            .unwrap_or_default()
    }

    fn provided_models(&self, cx: &App) -> Vec<Arc<dyn LanguageModel>> {
        let mut models: BTreeMap<String, anthropic::Model> = BTreeMap::default();

        // Models reported by Anthropic's `/v1/models` endpoint are the
        // primary source. The list will be empty until authentication has
        // succeeded and the first fetch completes.
        for model in &self.state.read(cx).fetched_models {
            models.insert(model.id.to_string(), model.clone());
        }

        // User-defined `available_models` from settings can either add
        // entirely new entries or override fields on a fetched model with
        // the same id (e.g. enable Fast mode or set a tool override).
        for available in &AnthropicLanguageModelProvider::settings(cx).available_models {
            let model = available_model_to_anthropic_model(available);
            models.insert(model.id.to_string(), model);
        }

        models
            .into_values()
            .map(|model| self.create_language_model(model))
            .collect()
    }

    fn is_authenticated(&self, cx: &App) -> bool {
        self.state.read(cx).is_authenticated()
    }

    fn authenticate(&self, cx: &mut App) -> Task<Result<(), AuthenticateError>> {
        self.state.update(cx, |state, cx| state.authenticate(cx))
    }

    fn settings_view(&self, cx: &mut App) -> Option<ProviderSettingsView> {
        let is_authenticated = self.state.read(cx).is_authenticated();
        let title = if is_authenticated {
            None
        } else {
            Some("Configure Claude".into())
        };
        let description = if is_authenticated {
            None
        } else {
            Some(InlineDescription::Text(SUBSCRIPTION_DESCRIPTION.into()))
        };

        Some(ProviderSettingsView::Inline(
            language_model::InlineProviderSettings {
                title,
                description,
                create_view: Arc::new({
                    let state = self.state.clone();
                    let http_client = self.http_client.clone();
                    move |_window, cx| {
                        cx.new(|_cx| AnthropicConfigurationView {
                            state: state.clone(),
                            http_client: http_client.clone(),
                            compact: true,
                        })
                        .into()
                    }
                }),
            },
        ))
    }

    fn set_api_key(&self, api_key: Option<String>, cx: &mut App) -> Task<Result<()>> {
        self.state
            .update(cx, |state, cx| state.set_api_key(api_key, cx))
    }

    fn fast_mode_confirmation(&self, _cx: &App) -> Option<FastModeConfirmation> {
        Some(FastModeConfirmation {
            title: "Enable Fast Mode for Anthropic?".into(),
            message: "Fast mode lets requests use your Anthropic Priority Tier capacity, which \
                Anthropic prioritizes over standard requests during peak load. Requires a \
                Priority Tier commitment with Anthropic; without one, requests behave the same \
                as the standard tier."
                .into(),
        })
    }
}

/// Pick the model from `models` whose id starts with the earliest matching
/// prefix in `preferred_prefixes`. Within a single prefix bucket the model
/// with the lexicographically greatest id wins, which roughly corresponds to
/// the highest version since Anthropic ids embed dated suffixes.
fn pick_preferred_model(
    models: &[anthropic::Model],
    preferred_prefixes: &[&str],
) -> Option<anthropic::Model> {
    for prefix in preferred_prefixes {
        let candidate = models
            .iter()
            .filter(|m| m.id.starts_with(prefix))
            .max_by(|a, b| a.id.cmp(&b.id));
        if let Some(model) = candidate {
            return Some(model.clone());
        }
    }
    None
}

/// Convert a settings-defined `available_models` entry into an `anthropic::Model`.
fn available_model_to_anthropic_model(available: &AvailableModel) -> anthropic::Model {
    let mode = match available.mode.unwrap_or_default() {
        settings::ModelMode::Default => AnthropicModelMode::Default,
        settings::ModelMode::Thinking { budget_tokens } => {
            AnthropicModelMode::Thinking { budget_tokens }
        }
        settings::ModelMode::Adaptive => AnthropicModelMode::AdaptiveThinking,
    };
    let supports_thinking = matches!(
        mode,
        AnthropicModelMode::Thinking { .. } | AnthropicModelMode::AdaptiveThinking
    );
    let supports_adaptive_thinking = matches!(mode, AnthropicModelMode::AdaptiveThinking);
    let supports_speed = available
        .supports_fast_mode
        .unwrap_or_else(|| anthropic::supports_fast_mode(&available.name));
    let mut extra_beta_headers = available.extra_beta_headers.clone();
    if supports_speed
        && !extra_beta_headers
            .iter()
            .any(|header| header.trim() == anthropic::FAST_MODE_BETA_HEADER)
    {
        extra_beta_headers.push(anthropic::FAST_MODE_BETA_HEADER.to_string());
    }

    anthropic::Model {
        display_name: available
            .display_name
            .clone()
            .unwrap_or_else(|| available.name.clone()),
        id: available.name.clone(),
        max_input_tokens: available.max_tokens,
        max_output_tokens: available.max_output_tokens.unwrap_or(4_096),
        default_temperature: available.default_temperature.unwrap_or(1.0),
        mode,
        supports_thinking,
        supports_adaptive_thinking,
        supports_images: true,
        supports_speed,
        supports_compaction: false,
        supported_effort_levels: if supports_adaptive_thinking {
            vec![
                anthropic::Effort::Low,
                anthropic::Effort::Medium,
                anthropic::Effort::High,
                anthropic::Effort::XHigh,
                anthropic::Effort::Max,
            ]
        } else {
            vec![]
        },
        tool_override: available.tool_override.clone(),
        extra_beta_headers,
    }
}

// ---------------------------------------------------------------------------
// OAuth sign-in flow with localhost callback server
// ---------------------------------------------------------------------------

const OAUTH_CALLBACK_PORT: u16 = 8907;
const OAUTH_CALLBACK_FALLBACK_PORT: u16 = 8909;

fn do_sign_in(state: &Entity<State>, http_client: &Arc<dyn HttpClient>, cx: &mut App) {
    if state.read(cx).sign_in_task.is_some() {
        return;
    }

    let weak_state = state.downgrade();
    let http_client = http_client.clone();

    let task = cx.spawn(async move |cx| {
        let result: anyhow::Result<()> = async {
            // Start localhost callback server.
            let (redirect_uri, callback_rx) =
                oauth_callback_server::start_oauth_callback_server_with_config(
                    oauth_callback_server::OAuthCallbackServerConfig {
                        host: "localhost",
                        preferred_port: OAUTH_CALLBACK_PORT,
                        fallback_port: Some(OAUTH_CALLBACK_FALLBACK_PORT),
                        path: "/callback",
                    },
                )
                .map_err(|e| {
                    log::error!("Claude OAuth: failed to start callback server: {e}");
                    anyhow::anyhow!("Failed to start callback server: {e}")
                })?;

            let params = oauth::build_authorize_url_with_redirect(&redirect_uri)?;
            let verifier = params.verifier.clone();
            let expected_state = params.state.clone();

            cx.update(|cx| cx.open_url(&params.url));

            let callback = callback_rx
                .await
                .map_err(|_| anyhow::anyhow!("OAuth callback cancelled"))?
                .map_err(|e| anyhow::anyhow!("OAuth callback failed: {e}"))?;

            if let Some(ref expected) = expected_state {
                if callback.state != *expected {
                    anyhow::bail!("OAuth state mismatch");
                }
            }

            log::info!("Claude OAuth: received callback, exchanging code");
            let state_str = callback.state.clone();
            let auth = oauth::exchange_code_with_redirect_and_state(
                http_client.as_ref(),
                &callback.code,
                &verifier,
                &redirect_uri,
                Some(&state_str),
            )
            .await?;

            log::info!("Claude OAuth: token exchange succeeded");
            let persist_task = weak_state.update(cx, |state, cx| {
                let task = state.set_oauth(auth, cx);
                state.last_auth_error = None;
                state.restart_fetch_models_task(cx);
                cx.notify();
                task
            })?;
            persist_task.await.log_err();
            Ok(())
        }
        .await;

        match result {
            Ok(()) => {
                weak_state
                    .update(cx, |state, cx| {
                        state.sign_in_task = None;
                        cx.notify();
                    })
                    .log_err();
            }
            Err(ref error) => {
                log::error!("Claude OAuth sign-in failed: {error:#}");
                weak_state
                    .update(cx, |state, cx| {
                        state.last_auth_error =
                            Some(SharedString::from(format!("{error:#}")));
                        state.sign_in_task = None;
                        cx.notify();
                    })
                    .log_err();
            }
        }

        result
    });

    state.update(cx, |state, cx| {
        state.last_auth_error = None;
        state.sign_in_task = Some(task);
        cx.notify();
    });
}

fn do_sign_out(state: &gpui::WeakEntity<State>, cx: &mut App) -> Task<Result<()>> {
    state
        .update(cx, |state, cx| {
            state.sign_in_task = None;
            state.last_auth_error = None;
            state.clear_oauth(cx)
        })
        .unwrap_or_else(|error| Task::ready(Err(error)))
}

struct AnthropicConfigurationView {
    state: Entity<State>,
    http_client: Arc<dyn HttpClient>,
    compact: bool,
}

impl Render for AnthropicConfigurationView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);

        if state.is_authenticated() {
            let label = "Signed in via Claude subscription";
            let weak_state = self.state.downgrade();

            return v_flex()
                .child(
                    ConfiguredApiCard::new(
                        "anthropic-oauth-sign-out",
                        SharedString::from(label),
                    )
                    .button_label("Sign Out")
                    .on_click(cx.listener(move |_this, _, _window, cx| {
                        do_sign_out(&weak_state, cx).detach_and_log_err(cx);
                    })),
                )
                .into_any_element();
        }

        let last_auth_error = state.last_auth_error.clone();
        let provider_state = self.state.clone();
        let http_client = self.http_client.clone();
        let is_signing_in = state.sign_in_task.is_some();
        let button_label = if is_signing_in {
            "Signing in…"
        } else {
            "Sign In"
        };

        v_flex()
            .gap_2()
            .when(!self.compact, |this| {
                this.child(Label::new(SUBSCRIPTION_DESCRIPTION))
            })
            .child(
                Button::new("sign-in", button_label)
                    .when(!self.compact, |this| this.full_width())
                    .style(ButtonStyle::Outlined)
                    .size(ButtonSize::Medium)
                    .loading(is_signing_in)
                    .disabled(is_signing_in)
                    .on_click(move |_, _window, cx| {
                        do_sign_in(&provider_state, &http_client, cx);
                    }),
            )
            .when_some(last_auth_error, |this, error| {
                this.child(
                    h_flex()
                        .gap_1()
                        .justify_center()
                        .child(
                            Icon::new(IconName::XCircle)
                                .color(Color::Error)
                                .size(IconSize::Small),
                        )
                        .child(Label::new(error).color(Color::Muted)),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::AsyncReadExt as _;
    use http_client::{AsyncBody, FakeHttpClient};
    use language_model::{LanguageModelRequestMessage, MessageContent};
    use serde_json::json;
    use std::sync::Mutex;

    fn parse_available_model(json: &str) -> AvailableModel {
        serde_json::from_str(json).expect("test fixture should parse")
    }

    #[test]
    fn adaptive_mode_maps_to_adaptive_thinking_with_all_effort_levels() {
        let available = parse_available_model(
            r#"{
                "name": "claude-opus-4-7",
                "max_tokens": 1000000,
                "max_output_tokens": 128000,
                "mode": { "type": "adaptive" }
            }"#,
        );
        let model = available_model_to_anthropic_model(&available);

        assert_eq!(model.mode, AnthropicModelMode::AdaptiveThinking);
        assert!(model.supports_thinking);
        assert!(model.supports_adaptive_thinking);
        assert_eq!(
            model.supported_effort_levels,
            vec![
                anthropic::Effort::Low,
                anthropic::Effort::Medium,
                anthropic::Effort::High,
                anthropic::Effort::XHigh,
                anthropic::Effort::Max,
            ]
        );
    }

    #[test]
    fn thinking_mode_does_not_enable_adaptive() {
        let available = parse_available_model(
            r#"{
                "name": "claude-sonnet-4-5",
                "max_tokens": 200000,
                "mode": { "type": "thinking", "budget_tokens": 4096 }
            }"#,
        );
        let model = available_model_to_anthropic_model(&available);

        assert!(matches!(model.mode, AnthropicModelMode::Thinking { .. }));
        assert!(model.supports_thinking);
        assert!(!model.supports_adaptive_thinking);
        assert!(model.supported_effort_levels.is_empty());
    }

    #[test]
    fn default_mode_disables_thinking() {
        let available = parse_available_model(
            r#"{
                "name": "claude-3-5-haiku",
                "max_tokens": 200000
            }"#,
        );
        let model = available_model_to_anthropic_model(&available);

        assert_eq!(model.mode, AnthropicModelMode::Default);
        assert!(!model.supports_thinking);
        assert!(!model.supports_adaptive_thinking);
        assert!(model.supported_effort_levels.is_empty());
    }

    #[gpui::test]
    fn direct_anthropic_supports_explicit_compaction_after_minimum_input(
        cx: &mut gpui::TestAppContext,
    ) {
        let provider = direct_anthropic_test_provider(FakeHttpClient::with_404_response(), cx);
        let model = direct_anthropic_test_model(&provider);

        assert!(model.supports_explicit_compaction());
        assert_eq!(
            model.minimum_explicit_compaction_input_tokens(),
            Some(anthropic::MIN_COMPACTION_TRIGGER_TOKENS)
        );
    }

    #[gpui::test]
    async fn direct_anthropic_explicit_compaction_uses_paused_completion(
        cx: &mut gpui::TestAppContext,
    ) {
        let captured_request = Arc::new(Mutex::new(None));
        let captured_request_for_handler = captured_request.clone();
        let http_client = FakeHttpClient::create(move |request| {
            let captured_request = captured_request_for_handler.clone();
            async move {
                if request.uri().path() == "/v1/models" {
                    return Ok(http_client::Response::builder()
                        .status(200)
                        .body(AsyncBody::from(r#"{"data":[]}"#))?);
                }

                let beta_header = request
                    .headers()
                    .get("Anthropic-Beta")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                let mut body = request.into_body();
                let mut body_text = String::new();
                body.read_to_string(&mut body_text).await?;
                *captured_request.lock().unwrap() = Some((beta_header, body_text));

                let response_lines = [
                    json!({
                        "type": "message_start",
                        "message": {
                            "id": "msg_compact",
                            "type": "message",
                            "role": "assistant",
                            "content": [],
                            "model": "claude-opus-4-6",
                            "stop_reason": null,
                            "stop_sequence": null,
                            "usage": {
                                "input_tokens": 0,
                                "output_tokens": 0
                            }
                        }
                    }),
                    json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": {
                            "type": "compaction",
                            "content": null,
                            "encrypted_content": null
                        }
                    }),
                    json!({
                        "type": "content_block_delta",
                        "index": 0,
                        "delta": {
                            "type": "compaction_delta",
                            "content": "Summary of the conversation.",
                            "encrypted_content": "opaque-state"
                        }
                    }),
                    json!({
                        "type": "content_block_stop",
                        "index": 0
                    }),
                    json!({
                        "type": "message_delta",
                        "delta": {
                            "stop_reason": "compaction",
                            "stop_sequence": null
                        },
                        "usage": {
                            "input_tokens": 0,
                            "output_tokens": 0,
                            "iterations": [{
                                "type": "compaction",
                                "input_tokens": 60_000,
                                "output_tokens": 1_000
                            }]
                        }
                    }),
                    json!({"type": "message_stop"}),
                ]
                .into_iter()
                .map(|line| format!("data: {line}"))
                .collect::<Vec<_>>()
                .join("\n");

                Ok(http_client::Response::builder()
                    .status(200)
                    .body(AsyncBody::from(format!("{response_lines}\n")))?)
            }
        });
        let provider = direct_anthropic_test_provider(http_client, cx);
        let store_key = cx.update(|cx| provider.set_api_key(Some("test-key".to_string()), cx));
        store_key.await.unwrap();
        let model = direct_anthropic_test_model(&provider);
        let request = LanguageModelRequest {
            messages: vec![LanguageModelRequestMessage {
                role: language_model::Role::User,
                content: vec![MessageContent::Text("Retain this context.".to_string())],
                cache: false,
                reasoning_details: None,
            }],
            ..Default::default()
        };

        let result = model.compact(request, &cx.to_async()).await.unwrap();

        assert_eq!(
            result.usage,
            language_model::TokenUsage {
                input_tokens: 60_000,
                output_tokens: 1_000,
                ..Default::default()
            }
        );
        let language_model::CompactedContext::Summary {
            content,
            provider_state,
        } = result.context
        else {
            panic!("expected summary compaction");
        };
        assert_eq!(content.as_ref(), "Summary of the conversation.");
        assert_eq!(
            anthropic::completion::provider_compaction_encrypted_content(
                &provider_state.expect("expected opaque provider state"),
                &ANTHROPIC_PROVIDER_ID,
            )
            .unwrap()
            .as_deref(),
            Some("opaque-state")
        );

        let (beta_header, body) = captured_request.lock().unwrap().take().unwrap();
        assert!(
            beta_header
                .as_deref()
                .is_some_and(|header| header.contains(anthropic::COMPACTION_BETA_HEADER))
        );
        let body = serde_json::from_str::<serde_json::Value>(&body).unwrap();
        assert_eq!(
            body["context_management"],
            json!({
                "edits": [{
                    "type": "compact_20260112",
                    "trigger": {
                        "type": "input_tokens",
                        "value": anthropic::MIN_COMPACTION_TRIGGER_TOKENS
                    },
                    "pause_after_compaction": true
                }]
            })
        );
        assert!(body["tools"].is_null());
    }

    fn direct_anthropic_test_provider(
        http_client: Arc<dyn HttpClient>,
        cx: &mut gpui::TestAppContext,
    ) -> AnthropicLanguageModelProvider {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            AnthropicLanguageModelProvider::new(http_client, Arc::new(TestCredentialsProvider), cx)
        })
    }

    fn direct_anthropic_test_model(
        provider: &AnthropicLanguageModelProvider,
    ) -> Arc<dyn LanguageModel> {
        provider.create_language_model(anthropic::Model::from_listed(anthropic::ListModelEntry {
            id: "claude-opus-4-6".to_string(),
            display_name: "Claude Opus 4.6".to_string(),
            max_input_tokens: 1_000_000,
            max_tokens: 128_000,
            capabilities: None,
        }))
    }

    struct TestCredentialsProvider;

    impl CredentialsProvider for TestCredentialsProvider {
        fn read_credentials<'a>(
            &'a self,
            _url: &'a str,
            _cx: &'a AsyncApp,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Option<(String, Vec<u8>)>>> + 'a>,
        > {
            Box::pin(async { Ok(None) })
        }

        fn write_credentials<'a>(
            &'a self,
            _url: &'a str,
            _username: &'a str,
            _password: &'a [u8],
            _cx: &'a AsyncApp,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + 'a>> {
            Box::pin(async { Ok(()) })
        }

        fn delete_credentials<'a>(
            &'a self,
            _url: &'a str,
            _cx: &'a AsyncApp,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + 'a>> {
            Box::pin(async { Ok(()) })
        }
    }
}

pub struct AnthropicModel {
    id: LanguageModelId,
    model: anthropic::Model,
    state: Entity<State>,
    http_client: Arc<dyn HttpClient>,
    request_limiter: RateLimiter,
}

impl AnthropicModel {
    fn stream_completion(
        &self,
        request: anthropic::Request,
        cx: &AsyncApp,
    ) -> BoxFuture<
        'static,
        Result<
            BoxStream<'static, Result<anthropic::Event, AnthropicError>>,
            LanguageModelCompletionError,
        >,
    > {
        let http_client = self.http_client.clone();

        let (api_key, api_url, extra_headers, using_oauth) =
            self.state.read_with(cx, |state, cx| {
                let api_url = AnthropicLanguageModelProvider::api_url(cx);
                let extra_headers = AnthropicLanguageModelProvider::settings(cx)
                    .custom_headers
                    .clone();
                let using_oauth = state.is_using_oauth(&api_url);
                (
                    state.effective_api_key(&api_url),
                    api_url,
                    extra_headers,
                    using_oauth,
                )
            });

        let beta_headers = {
            let mut base = self.model.beta_headers().unwrap_or_default();
            if using_oauth {
                if !base.is_empty() {
                    base.push(',');
                }
                base.push_str(anthropic::OAUTH_BETA_HEADER);
            }
            if base.is_empty() { None } else { Some(base) }
        };

        async move {
            let Some(api_key) = api_key else {
                return Err(LanguageModelCompletionError::NoApiKey {
                    provider: PROVIDER_NAME,
                });
            };
            let request = anthropic::stream_completion(
                http_client.as_ref(),
                &api_url,
                &api_key,
                request,
                beta_headers,
                &extra_headers,
            );
            request.await.map_err(Into::into)
        }
        .boxed()
    }
}

impl LanguageModel for AnthropicModel {
    fn id(&self) -> LanguageModelId {
        self.id.clone()
    }

    fn name(&self) -> LanguageModelName {
        LanguageModelName::from(self.model.display_name.clone())
    }

    fn provider_id(&self) -> LanguageModelProviderId {
        PROVIDER_ID
    }

    fn provider_name(&self) -> LanguageModelProviderName {
        PROVIDER_NAME
    }

    fn supports_tools(&self) -> bool {
        true
    }

    fn supports_images(&self) -> bool {
        self.model.supports_images
    }

    fn supports_streaming_tools(&self) -> bool {
        true
    }

    fn supports_tool_choice(&self, choice: LanguageModelToolChoice) -> bool {
        match choice {
            LanguageModelToolChoice::Auto
            | LanguageModelToolChoice::Any
            | LanguageModelToolChoice::None => true,
        }
    }

    fn supports_thinking(&self) -> bool {
        self.model.supports_thinking
    }

    fn supports_fast_mode(&self) -> bool {
        self.model.supports_speed
    }

    fn refusal_fallback_model_id(&self) -> Option<&'static str> {
        if self.model.id.starts_with(anthropic::FABLE_MODEL_ID_PREFIX) {
            Some(anthropic::FABLE_FALLBACK_MODEL_ID)
        } else {
            None
        }
    }

    fn supports_server_side_compaction(&self) -> bool {
        self.model.supports_compaction
    }

    fn supports_explicit_compaction(&self) -> bool {
        self.model.supports_compaction
    }

    fn minimum_explicit_compaction_input_tokens(&self) -> Option<u64> {
        self.supports_explicit_compaction()
            .then_some(anthropic::MIN_COMPACTION_TRIGGER_TOKENS)
    }

    fn compact(
        &self,
        request: LanguageModelRequest,
        cx: &AsyncApp,
    ) -> BoxFuture<'static, Result<CompactionResult, LanguageModelCompletionError>> {
        if !self.supports_explicit_compaction() {
            return async {
                Err(LanguageModelCompletionError::Other(anyhow::anyhow!(
                    "this Anthropic model does not support explicit compaction"
                )))
            }
            .boxed();
        }

        let mut request = match into_anthropic(
            request,
            self.model.request_id(false).to_string(),
            self.model.default_temperature,
            self.model.max_output_tokens,
            self.model.mode.clone(),
            AnthropicPromptCacheMode::Automatic,
            &PROVIDER_ID,
        ) {
            Ok(request) => request.into_compact_request(),
            Err(error) => return async move { Err(error.into()) }.boxed(),
        };
        if !self.model.supports_speed {
            request.speed = None;
        }
        let request = self.stream_completion(request, cx);
        let future = self.request_limiter.run(async move {
            let response = request.await?;
            let stream = AnthropicEventMapper::new(PROVIDER_NAME, PROVIDER_ID).map_stream(response);
            let (context, usage) = collect_compaction_result(stream.boxed(), PROVIDER_NAME).await?;
            Ok(CompactionResult { context, usage })
        });
        future.boxed()
    }

    fn supported_effort_levels(&self) -> Vec<language_model::LanguageModelEffortLevel> {
        self.model
            .supported_effort_levels
            .iter()
            .map(|e| {
                let is_default = matches!(e, anthropic::Effort::High);
                let (name, value) = match e {
                    anthropic::Effort::Low => ("Low".into(), "low".into()),
                    anthropic::Effort::Medium => ("Medium".into(), "medium".into()),
                    anthropic::Effort::High => ("High".into(), "high".into()),
                    anthropic::Effort::XHigh => ("XHigh".into(), "xhigh".into()),
                    anthropic::Effort::Max => ("Max".into(), "max".into()),
                };
                language_model::LanguageModelEffortLevel {
                    name,
                    value,
                    is_default,
                }
            })
            .collect::<Vec<_>>()
    }

    fn telemetry_id(&self) -> String {
        format!("anthropic/{}", self.model.id)
    }

    fn api_key(&self, cx: &App) -> Option<String> {
        self.state.read_with(cx, |state, cx| {
            let api_url = AnthropicLanguageModelProvider::api_url(cx);
            state
                .effective_api_key(&api_url)
                .map(|key| key.to_string())
        })
    }

    fn max_token_count(&self) -> u64 {
        self.model.max_input_tokens
    }

    fn max_output_tokens(&self) -> Option<u64> {
        Some(self.model.max_output_tokens)
    }

    fn stream_completion(
        &self,
        request: LanguageModelRequest,
        cx: &AsyncApp,
    ) -> BoxFuture<
        'static,
        Result<
            BoxStream<'static, Result<LanguageModelCompletionEvent, LanguageModelCompletionError>>,
            LanguageModelCompletionError,
        >,
    > {
        let has_tools = !request.tools.is_empty();
        let request_id = self.model.request_id(has_tools).to_string();
        let mut request = match into_anthropic(
            request,
            request_id,
            self.model.default_temperature,
            self.model.max_output_tokens,
            self.model.mode.clone(),
            AnthropicPromptCacheMode::Automatic,
            &PROVIDER_ID,
        ) {
            Ok(request) => request,
            Err(error) => return async move { Err(error.into()) }.boxed(),
        };
        if !self.model.supports_speed {
            request.speed = None;
        }
        let request = self.stream_completion(request, cx);
        let future = self.request_limiter.stream(async move {
            let response = request.await?;
            Ok(AnthropicEventMapper::new(PROVIDER_NAME, PROVIDER_ID).map_stream(response))
        });
        async move { Ok(future.await?.boxed()) }.boxed()
    }
}
