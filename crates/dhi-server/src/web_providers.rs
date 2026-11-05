//! Live web-provider settings, key management, and router reload.
//!
//! The three providers are fixed: TinyFish, Parallel, and Firecrawl. Database
//! rows are optional and authoritative once they exist, including when they
//! have no key. Reads never select or serialize keys; the only key-derived
//! value that leaves this module is a `…last4` hint.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path, State};
use dhi_store::Store;
use dhi_store::mgmt::{WebProviderSettingFull, WebProviderSettingUpsert};
use dhi_web::{
    ProviderKind, ProviderMode, ProviderSettingsOverride, SearchRequest, Secret, WebConfig,
    WebError, WebRouter, load_config_file,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{ApiError, AppState};

/// The fixed provider order and the complete set of valid ids.
pub const PROVIDERS: [ProviderKind; 3] = [
    ProviderKind::TinyFish,
    ProviderKind::Parallel,
    ProviderKind::Firecrawl,
];

/// The connection-test call is intentionally small and quick.
const TEST_QUERY: &str = "DHI connection test";
const TEST_TIMEOUT: Duration = Duration::from_secs(15);

fn display_name(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::TinyFish => "TinyFish",
        ProviderKind::Parallel => "Parallel",
        ProviderKind::Firecrawl => "Firecrawl",
    }
}

fn env_key_name(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::TinyFish => "TINYFISH_API_KEY",
        ProviderKind::Parallel => "PARALLEL_API_KEY",
        ProviderKind::Firecrawl => "FIRECRAWL_API_KEY",
    }
}

fn effective_key(config: &WebConfig, provider: ProviderKind) -> Option<&Secret> {
    match provider {
        ProviderKind::TinyFish => config.tinyfish.api_key.as_ref(),
        ProviderKind::Parallel => config.parallel.api_key.as_ref(),
        ProviderKind::Firecrawl => config.firecrawl.api_key.as_ref(),
    }
}

/// `…` plus at most the last four characters. Works on characters, not bytes,
/// so a non-ASCII key cannot be split. This is the only key-derived value a
/// read path may return.
fn display_hint(key: &str) -> String {
    let mut tail = key.chars().rev().take(4).collect::<Vec<_>>();
    tail.reverse();
    format!("…{}", tail.into_iter().collect::<String>())
}

fn mode_for(provider: ProviderKind) -> ProviderMode {
    match provider {
        ProviderKind::TinyFish => ProviderMode::TinyFish,
        ProviderKind::Parallel => ProviderMode::Parallel,
        ProviderKind::Firecrawl => ProviderMode::Firecrawl,
    }
}

/// Where an effective key came from. Serialized as the contract's strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeySource {
    Database,
    Environment,
    ConfigFile,
    None,
}

impl KeySource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Database => "database",
            Self::Environment => "environment",
            Self::ConfigFile => "config_file",
            Self::None => "none",
        }
    }
}

/// One provider entry of `GET /api/web/providers`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WebProviderDto {
    pub provider: String,
    pub display_name: String,
    pub configured: bool,
    pub key_hint: Option<String>,
    pub source: String,
    pub base_url: Option<String>,
    pub timeout_secs: u64,
    pub enabled: bool,
    pub status: String,
}

/// The full provider list shape.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WebProvidersDto {
    pub search_read_order: Vec<String>,
    pub research_order: Vec<String>,
    pub providers: Vec<WebProviderDto>,
}

/// The effective configuration plus the metadata a settings read needs. The
/// router is built from this same snapshot, so orders and statuses agree with
/// what is actually registered.
#[derive(Clone, Debug)]
struct EffectiveWebSettings {
    config: WebConfig,
    sources: HashMap<ProviderKind, KeySource>,
    /// The display hint for every configured provider. Database rows use the
    /// hint stored at save time; environment/config-file keys get a safe
    /// `…last4` hint computed from the effective key. No other key-derived
    /// value is exposed.
    hints: HashMap<ProviderKind, String>,
    search_read_order: Vec<String>,
    research_order: Vec<String>,
}

impl EffectiveWebSettings {
    fn empty() -> Self {
        Self {
            config: WebConfig::default(),
            sources: PROVIDERS
                .iter()
                .map(|provider| (*provider, KeySource::None))
                .collect(),
            hints: HashMap::new(),
            search_read_order: Vec::new(),
            research_order: Vec::new(),
        }
    }

    fn from_parts(
        config: WebConfig,
        rows: &[WebProviderSettingFull],
        router: &WebRouter,
        file: Option<&dhi_web::DhiConfigFile>,
    ) -> Self {
        let mut sources = HashMap::new();
        let mut hints = HashMap::new();
        for provider in PROVIDERS {
            let row = rows.iter().find(|row| row.provider == provider.name());
            match row {
                Some(row) => {
                    let has_key = row
                        .api_key
                        .as_deref()
                        .is_some_and(|key| !key.trim().is_empty());
                    sources.insert(
                        provider,
                        if has_key {
                            KeySource::Database
                        } else {
                            KeySource::None
                        },
                    );
                    if has_key
                        && let Some(hint) = row
                            .api_key_hint
                            .as_deref()
                            .map(str::trim)
                            .filter(|hint| !hint.is_empty())
                    {
                        hints.insert(provider, hint.to_string());
                    }
                }
                None => {
                    let source = if env_key_present(provider) {
                        KeySource::Environment
                    } else if provider == ProviderKind::Firecrawl
                        && file
                            .and_then(|file| file.firecrawl.as_ref())
                            .and_then(|section| section.api_key.as_deref())
                            .is_some_and(|key| !key.trim().is_empty())
                    {
                        KeySource::ConfigFile
                    } else {
                        KeySource::None
                    };
                    sources.insert(provider, source);
                }
            }
        }
        // Environment and config-file keys have no stored hint column. Derive
        // the same safe `…last4` display value from the effective key; a
        // database row with no key has no effective key and stays hintless.
        for provider in PROVIDERS {
            if hints.contains_key(&provider) {
                continue;
            }
            if let Some(key) = effective_key(&config, provider) {
                hints.insert(provider, display_hint(key.expose()));
            }
        }
        Self {
            config,
            sources,
            hints,
            search_read_order: router.provider_order().to_vec(),
            research_order: router.research_provider_order().to_vec(),
        }
    }

    fn dto(&self) -> WebProvidersDto {
        WebProvidersDto {
            search_read_order: self.search_read_order.clone(),
            research_order: self.research_order.clone(),
            providers: PROVIDERS
                .iter()
                .map(|provider| self.provider_dto(*provider))
                .collect(),
        }
    }

    fn provider_dto(&self, provider: ProviderKind) -> WebProviderDto {
        let configured = self.config.provider_available(provider).is_ok();
        let enabled = self.config.provider_enabled(provider);
        let has_key = match provider {
            ProviderKind::TinyFish => self.config.tinyfish.api_key.is_some(),
            ProviderKind::Parallel => self.config.parallel.api_key.is_some(),
            ProviderKind::Firecrawl => self.config.firecrawl.api_key.is_some(),
        };
        let base_url = match provider {
            // TinyFish uses two fixed endpoints and has no configurable base.
            ProviderKind::TinyFish => None,
            ProviderKind::Parallel => {
                let base = self.config.parallel.base_url.trim_end_matches('/');
                (base != dhi_web::providers::parallel::DEFAULT_BASE_URL).then(|| base.to_string())
            }
            ProviderKind::Firecrawl => self
                .config
                .firecrawl
                .base_url
                .as_deref()
                .map(str::trim)
                .filter(|base| !base.is_empty())
                .map(|base| base.trim_end_matches('/').to_string())
                .filter(|base| base != dhi_web::providers::DEFAULT_CLOUD_BASE_URL),
        };
        let timeout_secs = match provider {
            ProviderKind::TinyFish => self.config.tinyfish.timeout.as_secs(),
            ProviderKind::Parallel => self.config.parallel.timeout.as_secs(),
            ProviderKind::Firecrawl => self.config.firecrawl.timeout.as_secs(),
        };
        let status = if !enabled {
            "disabled"
        } else if !configured {
            "missing_key"
        } else {
            "ready"
        };
        WebProviderDto {
            provider: provider.name().to_string(),
            display_name: display_name(provider).to_string(),
            configured,
            key_hint: has_key
                .then(|| self.hints.get(&provider).cloned())
                .flatten(),
            source: self
                .sources
                .get(&provider)
                .copied()
                .unwrap_or(KeySource::None)
                .as_str()
                .to_string(),
            base_url,
            timeout_secs,
            enabled,
            status: status.to_string(),
        }
    }
}

fn env_key_present(provider: ProviderKind) -> bool {
    std::env::var(env_key_name(provider))
        .ok()
        .is_some_and(|value| !value.trim().is_empty())
}

/// The live web provider runtime shared by the HTTP API and the guarded web
/// tools. `router()` returns one consistent snapshot per tool call, so a
/// reload never interrupts an in-flight call.
pub struct WebProviderRuntime {
    router: Arc<RwLock<Option<Arc<WebRouter>>>>,
    settings: Arc<RwLock<Option<Arc<EffectiveWebSettings>>>>,
    store: Option<Arc<Store>>,
}

impl Clone for WebProviderRuntime {
    fn clone(&self) -> Self {
        Self {
            router: self.router.clone(),
            settings: self.settings.clone(),
            store: self.store.clone(),
        }
    }
}

impl WebProviderRuntime {
    /// A runtime with no store and no providers. Used by memory-only tests and
    /// degraded boots: web names fail closed while the server serves the rest.
    pub fn empty() -> Self {
        Self {
            router: Arc::new(RwLock::new(None)),
            settings: Arc::new(RwLock::new(Some(Arc::new(EffectiveWebSettings::empty())))),
            store: None,
        }
    }

    /// Alias for [`Self::empty`] when the intent is "web tools disabled".
    pub fn disabled() -> Self {
        Self::empty()
    }

    /// A runtime backed by the management store. Call [`Self::reload`] before
    /// serving traffic.
    pub fn new(store: Option<Arc<Store>>) -> Self {
        let mut runtime = Self::empty();
        runtime.store = store;
        runtime
    }

    /// A pre-built router for tests and hosts that do not need management
    /// reads. The provider DTOs derive their orders from this router.
    pub fn with_router(router: Arc<WebRouter>) -> Self {
        let mut settings = EffectiveWebSettings::empty();
        settings.search_read_order = router.provider_order().to_vec();
        settings.research_order = router.research_provider_order().to_vec();
        Self {
            router: Arc::new(RwLock::new(Some(router))),
            settings: Arc::new(RwLock::new(Some(Arc::new(settings)))),
            store: None,
        }
    }

    /// The current router snapshot. Cheap: one short lock and an `Arc` clone.
    pub fn router(&self) -> Option<Arc<WebRouter>> {
        self.router
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The current provider list. Reads only the in-memory snapshot; no key
    /// leaves the store.
    pub fn providers(&self) -> WebProvidersDto {
        self.settings
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_deref()
            .map(EffectiveWebSettings::dto)
            .unwrap_or_else(|| EffectiveWebSettings::empty().dto())
    }

    /// One provider from the current snapshot.
    pub fn provider(&self, provider: ProviderKind) -> WebProviderDto {
        self.providers()
            .providers
            .into_iter()
            .find(|entry| entry.provider == provider.name())
            .unwrap_or_else(|| EffectiveWebSettings::empty().provider_dto(provider))
    }

    /// Rebuilds the router from the database plus environment/file fallback
    /// and swaps it live. The previous router keeps serving on any failure.
    /// Logs provider names and order only.
    pub async fn reload(&self) -> Result<WebProvidersDto, String> {
        let rows = self.load_rows().await?;
        self.install(rows)
    }

    /// Rebuilds from the environment/config file only, ignoring the database.
    /// Boot uses this after a database read failure so the server still gets
    /// an env-configured router.
    pub async fn reload_env_fallback(&self) -> Result<WebProvidersDto, String> {
        self.install(Vec::new())
    }

    /// The effective config for a test call, rebuilt from the current database
    /// rows. Does not swap the live router.
    async fn effective_config(&self) -> Result<WebConfig, String> {
        let rows = self.load_rows().await?;
        let overrides = overrides_from_rows(&rows)?;
        WebConfig::from_env_with_overrides(&overrides).map_err(|err| err.to_string())
    }

    async fn load_rows(&self) -> Result<Vec<WebProviderSettingFull>, String> {
        match &self.store {
            Some(store) => store
                .list_web_provider_settings_with_keys()
                .await
                .map_err(|err| format!("could not read web provider settings: {err}")),
            None => Ok(Vec::new()),
        }
    }

    fn install(&self, rows: Vec<WebProviderSettingFull>) -> Result<WebProvidersDto, String> {
        let overrides = overrides_from_rows(&rows)?;
        let config = WebConfig::from_env_with_overrides(&overrides)
            .map_err(|err| format!("web provider configuration is invalid: {err}"))?;
        // Build everything before touching the live snapshot: a failure here
        // leaves the previous router serving unchanged.
        let router = WebRouter::from_config(&config)
            .map_err(|err| format!("web provider router could not be built: {}", err.message()))?;
        let file = load_config_file();
        let settings = EffectiveWebSettings::from_parts(config, &rows, &router, file.as_ref());
        let dto = settings.dto();

        tracing::info!(
            providers = ?router.provider_order(),
            research_providers = ?router.research_provider_order(),
            skipped = ?router
                .skipped_providers()
                .iter()
                .map(|skip| skip.provider.as_str())
                .collect::<Vec<_>>(),
            "web provider router reloaded"
        );

        *self
            .settings
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::new(settings));
        *self
            .router
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::new(router));
        Ok(dto)
    }
}

fn overrides_from_rows(
    rows: &[WebProviderSettingFull],
) -> Result<HashMap<ProviderKind, ProviderSettingsOverride>, String> {
    let mut overrides = HashMap::new();
    for row in rows {
        let provider = row
            .provider
            .parse::<ProviderKind>()
            .map_err(|_| format!("unknown stored web provider {:?}", row.provider))?;
        let timeout_secs =
            match row.timeout_secs {
                Some(seconds) if seconds > 0 => Some(u64::try_from(seconds).map_err(|_| {
                    format!("stored {} timeout_secs is out of range", provider.name())
                })?),
                Some(_) => {
                    return Err(format!(
                        "stored {} timeout_secs must be greater than zero",
                        provider.name()
                    ));
                }
                None => None,
            };
        overrides.insert(
            provider,
            ProviderSettingsOverride {
                authoritative: true,
                api_key: row.api_key.as_deref().and_then(Secret::new),
                base_url: row.base_url.clone(),
                timeout_secs,
                enabled: Some(row.enabled),
            },
        );
    }
    Ok(overrides)
}

/// `PUT /api/web/providers/{provider}` input. Every field is optional; an
/// absent or empty key keeps the stored one, and an empty `base_url` clears
/// the override.
#[derive(Default, Deserialize)]
pub(crate) struct UpdateWebProviderRequest {
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    enabled: Option<bool>,
}

impl std::fmt::Debug for UpdateWebProviderRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpdateWebProviderRequest")
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("base_url", &self.base_url)
            .field("timeout_secs", &self.timeout_secs)
            .field("enabled", &self.enabled)
            .finish()
    }
}

fn parse_provider(raw: &str) -> Result<ProviderKind, ApiError> {
    raw.parse::<ProviderKind>()
        .map_err(|_| ApiError::not_found(format!("unknown web provider {raw:?}")))
}

fn store(state: &AppState) -> Result<&Arc<Store>, ApiError> {
    state
        .mgmt
        .as_ref()
        .ok_or_else(|| ApiError::internal("no management store configured for this app state"))
}

fn validate_base_url(raw: &str) -> Result<(), ApiError> {
    if raw.is_empty() {
        return Ok(());
    }
    if raw.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
        return Err(ApiError::bad_request(
            "base_url must not contain whitespace or control characters",
        ));
    }
    let url = reqwest::Url::parse(raw)
        .map_err(|err| ApiError::bad_request(format!("base_url is not a valid URL: {err}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ApiError::bad_request(format!(
            "base_url must use http or https; got {}",
            url.scheme()
        )));
    }
    if url.host_str().is_none() {
        return Err(ApiError::bad_request("base_url must include a host"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ApiError::bad_request(
            "base_url must not include credentials",
        ));
    }
    Ok(())
}

/// `GET /api/web/providers`.
pub(crate) async fn list_providers(State(state): State<AppState>) -> Json<WebProvidersDto> {
    Json(state.web.providers())
}

/// `PUT /api/web/providers/{provider}`: persist partial settings and swap the
/// router live. Response is the single provider DTO.
pub(crate) async fn update_provider(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Json(body): Json<UpdateWebProviderRequest>,
) -> Result<Json<WebProviderDto>, ApiError> {
    let provider = parse_provider(&provider)?;
    if let Some(seconds) = body.timeout_secs
        && !(1..=3_600).contains(&seconds)
    {
        return Err(ApiError::bad_request(
            "timeout_secs must be between 1 and 3600",
        ));
    }
    if let Some(base_url) = body.base_url.as_deref() {
        validate_base_url(base_url)?;
    }
    // TinyFish endpoints are fixed; accept the field but never persist it.
    let base_url = match provider {
        ProviderKind::TinyFish => None,
        _ => body.base_url.clone(),
    };
    let store = store(&state)?;
    // First save of an environment/config-file provider: a database row is
    // authoritative for the key, so importing the effective key here prevents
    // a timeout/enabled-only save from silently disabling the provider. An
    // existing row keeps its own key (or its explicit no-key state).
    let explicit_key = body
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty());
    let api_key = match explicit_key {
        Some(key) => Some(key.to_string()),
        None => {
            let stored = store
                .list_web_provider_settings_with_keys()
                .await
                .map_err(ApiError::from)?;
            let has_row = stored.iter().any(|row| row.provider == provider.name());
            if has_row {
                None
            } else {
                let config = state
                    .web
                    .effective_config()
                    .await
                    .map_err(ApiError::internal)?;
                effective_key(&config, provider).map(|key| key.expose().to_string())
            }
        }
    };
    let upsert = WebProviderSettingUpsert {
        provider: provider.name().to_string(),
        api_key,
        base_url,
        timeout_secs: body.timeout_secs,
        enabled: body.enabled,
    };
    store
        .upsert_web_provider_setting(&upsert)
        .await
        .map_err(ApiError::from)?;
    state
        .web
        .reload()
        .await
        .map_err(|err| ApiError::internal(format!("provider saved but reload failed: {err}")))?;
    Ok(Json(state.web.provider(provider)))
}

/// `DELETE /api/web/providers/{provider}/key`: clears the stored key and hint
/// but keeps the row, so an environment key cannot silently return.
pub(crate) async fn delete_provider_key(
    State(state): State<AppState>,
    Path(provider): Path<String>,
) -> Result<Json<WebProviderDto>, ApiError> {
    let provider = parse_provider(&provider)?;
    store(&state)?
        .clear_web_provider_key(provider.name())
        .await
        .map_err(ApiError::from)?;
    state
        .web
        .reload()
        .await
        .map_err(|err| ApiError::internal(format!("key cleared but reload failed: {err}")))?;
    Ok(Json(state.web.provider(provider)))
}

/// `POST /api/web/providers/{provider}/test`: one real, minimal search call
/// with a short deadline. Allowed to be billable.
pub(crate) async fn test_provider(
    State(state): State<AppState>,
    Path(provider): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let provider = parse_provider(&provider)?;
    let config = state
        .web
        .effective_config()
        .await
        .map_err(ApiError::internal)?;
    if let Err(reason) = config.provider_available(provider) {
        return Err(ApiError::bad_request(format!(
            "{} is not configured: {reason}",
            display_name(provider)
        )));
    }

    let started = Instant::now();
    let _results = run_provider_test(provider, config).await.map_err(|err| {
        ApiError::bad_gateway(format!(
            "{} connection test failed: {}",
            display_name(provider),
            err.message()
        ))
    })?;
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(Json(json!({
        "ok": true,
        "provider": provider.name(),
        "message": format!("{} responded", display_name(provider)),
        "latency_ms": latency_ms,
    })))
}

async fn run_provider_test(
    provider: ProviderKind,
    mut config: WebConfig,
) -> Result<Vec<dhi_web::SearchResult>, WebError> {
    config.mode = mode_for(provider);
    config.disabled.clear();
    config.cache_ttl = Duration::ZERO;
    config.operation_timeout = TEST_TIMEOUT;
    match provider {
        ProviderKind::TinyFish => config.tinyfish.timeout = TEST_TIMEOUT,
        ProviderKind::Parallel => config.parallel.timeout = TEST_TIMEOUT,
        ProviderKind::Firecrawl => config.firecrawl.timeout = TEST_TIMEOUT,
    }
    let router = WebRouter::from_config(&config)?;
    router.search(SearchRequest::new(TEST_QUERY).limit(1)).await
}

/// `POST /api/web/providers/reload`: rebuild from the database plus env/file
/// fallback and return the same shape as `GET /api/web/providers`. A failure
/// is `500` and keeps the previous router serving.
pub(crate) async fn reload_providers(
    State(state): State<AppState>,
) -> Result<Json<WebProvidersDto>, ApiError> {
    state
        .web
        .reload()
        .await
        .map(Json)
        .map_err(|err| ApiError::internal(format!("web provider reload failed: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(values: &[(&str, &str)]) -> WebConfig {
        let values = values
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect::<HashMap<_, _>>();
        WebConfig::from_lookup(move |name| values.get(name).cloned()).expect("valid config")
    }

    fn settings(config: WebConfig) -> EffectiveWebSettings {
        let router = WebRouter::from_config(&config).expect("router");
        let mut settings = EffectiveWebSettings::from_parts(config, &[], &router, None);
        // Tests inject values through `from_lookup`; mirror what the runtime
        // would report when those values came from the process environment.
        for provider in PROVIDERS {
            let has_key = match provider {
                ProviderKind::TinyFish => settings.config.tinyfish.api_key.is_some(),
                ProviderKind::Parallel => settings.config.parallel.api_key.is_some(),
                ProviderKind::Firecrawl => settings.config.firecrawl.api_key.is_some(),
            };
            if has_key && settings.sources.get(&provider) == Some(&KeySource::None) {
                settings.sources.insert(provider, KeySource::Environment);
            }
        }
        settings
    }

    #[test]
    fn display_hint_keeps_at_most_four_characters() {
        assert_eq!(display_hint("ab"), "…ab");
        assert_eq!(display_hint("abcdef"), "…cdef");
        assert_eq!(display_hint("ключ-ключ"), "…ключ");
    }

    #[test]
    fn provider_list_is_fixed_order_with_statuses() {
        let settings = settings(config(&[
            ("TINYFISH_API_KEY", "tf-secret"),
            ("PARALLEL_API_KEY", "par-secret"),
            ("FIRECRAWL_BASE_URL", "http://127.0.0.1:3002"),
        ]));
        let dto = settings.dto();
        assert_eq!(
            dto.providers
                .iter()
                .map(|provider| provider.provider.as_str())
                .collect::<Vec<_>>(),
            ["tinyfish", "parallel", "firecrawl"]
        );
        assert_eq!(dto.search_read_order, ["tinyfish", "parallel", "firecrawl"]);
        assert_eq!(dto.research_order, ["parallel"]);
        assert!(
            dto.providers
                .iter()
                .all(|provider| provider.status == "ready")
        );
        assert_eq!(dto.providers[0].key_hint.as_deref(), Some("…cret"));
        assert_eq!(dto.providers[1].key_hint.as_deref(), Some("…cret"));
        assert!(dto.providers[2].key_hint.is_none());
        assert_eq!(dto.providers[0].source, "environment");
        assert_eq!(dto.providers[2].source, "none");
        assert_eq!(
            dto.providers[2].base_url.as_deref(),
            Some("http://127.0.0.1:3002")
        );
    }

    #[test]
    fn missing_keys_are_missing_and_disabled_wins() {
        let mut config = config(&[("TINYFISH_API_KEY", "tf-secret")]);
        config.disabled = vec![ProviderKind::TinyFish];
        let dto = settings(config).dto();
        assert_eq!(dto.providers[0].status, "disabled");
        assert_eq!(dto.providers[1].status, "missing_key");
        assert_eq!(dto.providers[2].status, "missing_key");
        assert!(dto.search_read_order.is_empty());
        assert!(dto.research_order.is_empty());
    }

    #[test]
    fn stored_hint_takes_precedence_over_the_environment_key() {
        let config = config(&[("TINYFISH_API_KEY", "env-secret")]);
        let router = WebRouter::from_config(&config).expect("router");
        let row = WebProviderSettingFull {
            provider: "tinyfish".to_string(),
            api_key: Some("db-secret".to_string()),
            api_key_hint: Some("…cret".to_string()),
            base_url: None,
            timeout_secs: Some(30),
            enabled: true,
            priority: 1,
            updated_at: chrono::Utc::now(),
        };
        let overrides = overrides_from_rows(std::slice::from_ref(&row)).expect("overrides");
        let effective = WebConfig::from_lookup_with_overrides(
            |name| (name == "TINYFISH_API_KEY").then_some("env-secret".to_string()),
            &overrides,
        )
        .expect("config");
        let settings = EffectiveWebSettings::from_parts(
            effective.clone(),
            std::slice::from_ref(&row),
            &router,
            None,
        );
        let dto = settings.provider_dto(ProviderKind::TinyFish);
        assert_eq!(dto.source, "database");
        assert_eq!(dto.key_hint.as_deref(), Some("…cret"));
        assert_eq!(dto.timeout_secs, 30);
        assert_eq!(dto.status, "ready");
        assert!(!format!("{settings:?}").contains("db-secret"));
        let debug = format!("{row:?}");
        assert!(
            !debug.contains("db-secret"),
            "debug leaked a raw provider key"
        );
    }

    #[test]
    fn clearing_a_key_keeps_authority_without_a_hint() {
        let config = config(&[("TINYFISH_API_KEY", "env-secret")]);
        let router = WebRouter::from_config(&config).expect("router");
        let row = WebProviderSettingFull {
            provider: "tinyfish".to_string(),
            api_key: None,
            api_key_hint: None,
            base_url: None,
            timeout_secs: None,
            enabled: true,
            priority: 1,
            updated_at: chrono::Utc::now(),
        };
        let overrides = overrides_from_rows(std::slice::from_ref(&row)).expect("overrides");
        let effective = WebConfig::from_lookup_with_overrides(
            |name| (name == "TINYFISH_API_KEY").then_some("env-secret".to_string()),
            &overrides,
        )
        .expect("config");
        assert!(effective.tinyfish.api_key.is_none());
        let settings =
            EffectiveWebSettings::from_parts(effective, std::slice::from_ref(&row), &router, None);
        let dto = settings.provider_dto(ProviderKind::TinyFish);
        assert_eq!(dto.source, "none");
        assert!(!dto.configured);
        assert_eq!(dto.status, "missing_key");
        assert!(dto.key_hint.is_none());
    }

    #[test]
    fn base_urls_reflect_overrides_and_hide_built_in_defaults() {
        let config = config(&[
            ("PARALLEL_API_KEY", "par-secret"),
            ("FIRECRAWL_API_KEY", "fc-secret"),
        ]);
        let router = WebRouter::from_config(&config).expect("router");
        let rows = vec![
            WebProviderSettingFull {
                provider: "parallel".to_string(),
                api_key: Some("par-secret".to_string()),
                api_key_hint: Some("…cret".to_string()),
                base_url: Some("http://127.0.0.1:9090".to_string()),
                timeout_secs: Some(12),
                enabled: true,
                priority: 2,
                updated_at: chrono::Utc::now(),
            },
            WebProviderSettingFull {
                provider: "firecrawl".to_string(),
                api_key: Some("fc-secret".to_string()),
                api_key_hint: Some("…cret".to_string()),
                base_url: Some(dhi_web::providers::DEFAULT_CLOUD_BASE_URL.to_string()),
                timeout_secs: None,
                enabled: true,
                priority: 3,
                updated_at: chrono::Utc::now(),
            },
        ];
        let overrides = overrides_from_rows(&rows).expect("overrides");
        let effective = WebConfig::from_lookup_with_overrides(
            |name| match name {
                "PARALLEL_API_KEY" => Some("par-secret".to_string()),
                "FIRECRAWL_API_KEY" => Some("fc-secret".to_string()),
                _ => None,
            },
            &overrides,
        )
        .expect("config");
        let settings = EffectiveWebSettings::from_parts(effective, &rows, &router, None);
        let dto = settings.dto();
        assert_eq!(
            dto.providers[1].base_url.as_deref(),
            Some("http://127.0.0.1:9090")
        );
        assert_eq!(dto.providers[1].timeout_secs, 12);
        assert_eq!(dto.providers[2].base_url, None);
        assert_eq!(dto.providers[0].base_url, None, "TinyFish is always null");
    }
}
