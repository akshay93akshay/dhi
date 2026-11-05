//! The provider router: ordering, fallback, validation, caching, and ledger.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::cache::{self, Cache, Cached};
use crate::config::{ProviderKind, WebConfig};
use crate::error::WebError;
use crate::ledger::{Ledger, Operation};
use crate::model::{
    ReadRequest, ReadResult, ResearchRequest, ResearchResult, SearchRequest, SearchResult,
};
use crate::providers::{FirecrawlProvider, ParallelProvider, TinyFishProvider, WebProvider};
use crate::tools::{parse_read_input, parse_research_input, parse_search_input};
use crate::validation::{
    validate_read_request, validate_research_question, validate_search, validate_target_url,
};

/// Default overall search/read deadline across the fallback chain.
pub const DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_secs(180);
/// Default overall research deadline.
pub const DEFAULT_RESEARCH_TIMEOUT: Duration = Duration::from_secs(900);

/// A provider the router did not construct, with a human-readable reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedProvider {
    pub provider: String,
    pub reason: String,
}

/// The single router behind `web_search`, `web_read`, and `web_research`.
#[derive(Clone)]
pub struct WebRouter {
    providers: Vec<Arc<dyn WebProvider>>,
    order: Vec<String>,
    research_order: Vec<String>,
    cache: Arc<Cache>,
    ledger: Arc<Ledger>,
    skips: Vec<SkippedProvider>,
    settings_tag: String,
    allow_private_targets: bool,
    operation_timeout: Duration,
    research_timeout: Duration,
}

impl fmt::Debug for WebRouter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebRouter")
            .field("order", &self.order)
            .field("research_order", &self.research_order)
            .field("cache_ttl", &self.cache.ttl())
            .field("allow_private_targets", &self.allow_private_targets)
            .field("operation_timeout", &self.operation_timeout)
            .field("research_timeout", &self.research_timeout)
            .field("skipped", &self.skips)
            .finish_non_exhaustive()
    }
}

impl WebRouter {
    /// Builds a router from already-constructed providers. `order` and
    /// `research_order` are provider names in attempt order.
    pub fn new(
        providers: Vec<Arc<dyn WebProvider>>,
        order: Vec<String>,
        research_order: Vec<String>,
        cache_ttl: Duration,
    ) -> Self {
        let settings_tag = settings_tag(&providers);
        Self {
            providers,
            order,
            research_order,
            cache: Arc::new(Cache::new(cache_ttl)),
            ledger: Arc::new(Ledger::new()),
            skips: Vec::new(),
            settings_tag,
            allow_private_targets: false,
            operation_timeout: DEFAULT_OPERATION_TIMEOUT,
            research_timeout: DEFAULT_RESEARCH_TIMEOUT,
        }
    }

    /// Resolves configuration, constructs available providers, and records a
    /// clear skip reason for every provider without a usable key.
    pub fn from_config(config: &WebConfig) -> Result<Self, WebError> {
        let mut built: Vec<(ProviderKind, Arc<dyn WebProvider>)> = Vec::new();
        let mut skips = Vec::new();
        let mut seen = HashSet::new();
        let kinds = config
            .search_read_order()
            .iter()
            .chain(config.research_order().iter())
            .copied()
            .collect::<Vec<_>>();
        for kind in kinds {
            if !seen.insert(kind) {
                continue;
            }
            if !config.provider_enabled(kind) {
                tracing::info!(
                    provider = kind.name(),
                    "web provider skipped: disabled by management settings"
                );
                skips.push(SkippedProvider {
                    provider: kind.name().to_string(),
                    reason: "disabled by web provider settings".to_string(),
                });
                continue;
            }
            if let Err(reason) = config.provider_available(kind) {
                tracing::info!(
                    provider = kind.name(),
                    reason = %reason,
                    "web provider skipped"
                );
                skips.push(SkippedProvider {
                    provider: kind.name().to_string(),
                    reason,
                });
                continue;
            }
            let provider: Result<Arc<dyn WebProvider>, WebError> = match kind {
                ProviderKind::TinyFish => {
                    let mut provider_config = config.tinyfish.clone();
                    provider_config.allow_private_targets = config.allow_private_targets;
                    TinyFishProvider::new(provider_config)
                        .map(|provider| Arc::new(provider) as Arc<dyn WebProvider>)
                }
                ProviderKind::Parallel => {
                    let mut provider_config = config.parallel.clone();
                    provider_config.allow_private_targets = config.allow_private_targets;
                    ParallelProvider::new(provider_config)
                        .map(|provider| Arc::new(provider) as Arc<dyn WebProvider>)
                }
                ProviderKind::Firecrawl => {
                    let mut provider_config = config.firecrawl.clone();
                    provider_config.allow_private_targets = config.allow_private_targets;
                    FirecrawlProvider::new(provider_config)
                        .map(|provider| Arc::new(provider) as Arc<dyn WebProvider>)
                }
            };
            match provider {
                Ok(provider) => built.push((kind, provider)),
                Err(err) => {
                    tracing::info!(
                        provider = kind.name(),
                        reason = %err.message(),
                        "web provider skipped"
                    );
                    skips.push(SkippedProvider {
                        provider: kind.name().to_string(),
                        reason: err.message().to_string(),
                    });
                }
            }
        }
        let order = config
            .effective_search_read_order()
            .iter()
            .filter(|kind| built.iter().any(|(built, _)| built == *kind))
            .map(|kind| kind.name().to_string())
            .collect::<Vec<_>>();
        let research_order = config
            .effective_research_order()
            .iter()
            .filter(|kind| built.iter().any(|(built, _)| built == *kind))
            .map(|kind| kind.name().to_string())
            .collect::<Vec<_>>();
        let providers = built
            .into_iter()
            .map(|(_, provider)| provider)
            .collect::<Vec<_>>();
        let settings_tag = settings_tag(&providers);
        Ok(Self {
            providers,
            order,
            research_order,
            cache: Arc::new(Cache::new(config.cache_ttl)),
            ledger: Arc::new(Ledger::new()),
            skips,
            settings_tag,
            allow_private_targets: config.allow_private_targets,
            operation_timeout: config.operation_timeout,
            research_timeout: config.parallel.research_timeout,
        })
    }

    /// Reads `WebConfig` from the process environment and config file.
    pub fn from_env() -> Result<Self, WebError> {
        Self::from_config(&WebConfig::from_env()?)
    }

    /// Explicit opt-in for loopback/private/RFC1918 target URLs. The policy is
    /// propagated to directly constructed providers as well as enforced by the
    /// router.
    pub fn with_allow_private_targets(mut self, allow: bool) -> Self {
        self.allow_private_targets = allow;
        for provider in &self.providers {
            provider.set_allow_private_targets(allow);
        }
        self
    }

    pub fn with_operation_timeout(mut self, timeout: Duration) -> Self {
        self.operation_timeout = timeout;
        self
    }

    pub fn with_research_timeout(mut self, timeout: Duration) -> Self {
        self.research_timeout = timeout;
        self
    }

    /// Runs one of the three stable DHI tools with model-provided JSON input.
    pub async fn execute(&self, tool: &str, input: &Value) -> Result<Value, WebError> {
        match tool {
            "web_search" => {
                let request = parse_search_input(input)?;
                let results = self.search(request.clone()).await?;
                Ok(json!({
                    "query": request.query,
                    "results": results,
                    "count": results.len(),
                }))
            }
            "web_read" => {
                let request = parse_read_input(input)?;
                to_json(self.read(request).await?)
            }
            "web_research" => {
                let request = parse_research_input(input)?;
                to_json(self.research(request).await?)
            }
            other => Err(WebError::invalid_input(format!(
                "unknown web tool {other:?}; expected web_search, web_read, or web_research"
            ))),
        }
    }

    /// Searches through the configured order with fallback on retryable errors.
    pub async fn search(&self, request: SearchRequest) -> Result<Vec<SearchResult>, WebError> {
        validate_search(&request)?;
        let key = cache::search_key(&self.settings_tag, &request);
        match self
            .cache
            .get_or_load(key, || async {
                self.with_deadline(Operation::Search, self.search_uncached(&request))
                    .await
                    .map(Cached::Search)
            })
            .await?
        {
            Cached::Search(results) => Ok(results),
            _ => Err(WebError::provider_error(
                "cache",
                "cache returned the wrong value type for web_search",
            )),
        }
    }

    /// Reads through the configured order with the same fallback policy.
    pub async fn read(&self, request: ReadRequest) -> Result<ReadResult, WebError> {
        validate_read_request(&request.url, request.objective.as_deref())?;
        let url = validate_target_url("web_read", &request.url, self.allow_private_targets).await?;
        let request = ReadRequest {
            url: url.clone(),
            objective: request.objective,
        };
        let key = cache::read_key(&self.settings_tag, &request, &url);
        match self
            .cache
            .get_or_load(key, || async {
                self.with_deadline(Operation::Read, self.read_uncached(&request))
                    .await
                    .map(Cached::Read)
            })
            .await?
        {
            Cached::Read(result) => Ok(result),
            _ => Err(WebError::provider_error(
                "cache",
                "cache returned the wrong value type for web_read",
            )),
        }
    }

    /// Researches through the configured research order.
    pub async fn research(&self, request: ResearchRequest) -> Result<ResearchResult, WebError> {
        validate_research_question(&request.question)?;
        let key = cache::research_key(&self.settings_tag, &request);
        match self
            .cache
            .get_or_load(key, || async {
                self.with_deadline(Operation::Research, self.research_uncached(&request))
                    .await
                    .map(Cached::Research)
            })
            .await?
        {
            Cached::Research(result) => Ok(result),
            _ => Err(WebError::provider_error(
                "cache",
                "cache returned the wrong value type for web_research",
            )),
        }
    }

    pub fn provider_order(&self) -> &[String] {
        &self.order
    }

    pub fn research_provider_order(&self) -> &[String] {
        &self.research_order
    }

    pub fn skipped_providers(&self) -> &[SkippedProvider] {
        &self.skips
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    pub fn cache_ttl(&self) -> Duration {
        self.cache.ttl()
    }

    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }

    pub fn clear_cache(&self) {
        self.cache.clear();
    }

    pub fn allow_private_targets(&self) -> bool {
        self.allow_private_targets
    }

    pub fn operation_timeout(&self) -> Duration {
        self.operation_timeout
    }

    pub fn research_timeout(&self) -> Duration {
        self.research_timeout
    }

    async fn with_deadline<T>(
        &self,
        operation: Operation,
        future: impl std::future::Future<Output = Result<T, WebError>>,
    ) -> Result<T, WebError> {
        let timeout = match operation {
            Operation::Research => self.research_timeout,
            Operation::Search | Operation::Read => self.operation_timeout,
        };
        match tokio::time::timeout(timeout, future).await {
            Ok(result) => result,
            Err(_) => Err(WebError::timeout(
                "router",
                format!(
                    "web_{} exceeded its overall {}s deadline",
                    operation.as_str(),
                    timeout.as_secs()
                ),
            )),
        }
    }

    async fn search_uncached(
        &self,
        request: &SearchRequest,
    ) -> Result<Vec<SearchResult>, WebError> {
        let mut failures = Vec::new();
        let mut last_retryable = None;
        for name in &self.order {
            let Some(provider) = self.provider(name) else {
                continue;
            };
            match self.call_search(provider, request).await {
                Ok(results) => return Ok(results),
                Err(err) if err.is_unsupported() => failures.push(err),
                Err(err) if err.is_fallback() => {
                    last_retryable = Some(err.clone());
                    failures.push(err);
                }
                Err(err) => return Err(err),
            }
        }
        self.exhausted("search", failures, last_retryable)
    }

    async fn read_uncached(&self, request: &ReadRequest) -> Result<ReadResult, WebError> {
        let mut failures = Vec::new();
        let mut last_retryable = None;
        for name in &self.order {
            let Some(provider) = self.provider(name) else {
                continue;
            };
            match self.call_read(provider, request).await {
                Ok(result) => return Ok(result),
                Err(err) if err.is_unsupported() => failures.push(err),
                Err(err) if err.is_fallback() => {
                    last_retryable = Some(err.clone());
                    failures.push(err);
                }
                Err(err) => return Err(err),
            }
        }
        self.exhausted("read", failures, last_retryable)
    }

    async fn research_uncached(
        &self,
        request: &ResearchRequest,
    ) -> Result<ResearchResult, WebError> {
        let mut failures = Vec::new();
        let mut last_retryable = None;
        for name in &self.research_order {
            let Some(provider) = self.provider(name) else {
                continue;
            };
            match self.call_research(provider, request).await {
                Ok(result) => return Ok(result),
                Err(err) if err.is_unsupported() => failures.push(err),
                Err(err) if err.is_fallback() => {
                    last_retryable = Some(err.clone());
                    failures.push(err);
                }
                Err(err) => return Err(err),
            }
        }
        self.exhausted("research", failures, last_retryable)
    }

    fn provider(&self, name: &str) -> Option<&Arc<dyn WebProvider>> {
        self.providers
            .iter()
            .find(|provider| provider.name() == name)
    }

    async fn call_search(
        &self,
        provider: &Arc<dyn WebProvider>,
        request: &SearchRequest,
    ) -> Result<Vec<SearchResult>, WebError> {
        let started = Instant::now();
        let result = provider.search(request).await;
        self.record(provider, Operation::Search, started, &result);
        result
    }

    async fn call_read(
        &self,
        provider: &Arc<dyn WebProvider>,
        request: &ReadRequest,
    ) -> Result<ReadResult, WebError> {
        let started = Instant::now();
        let result = provider.read(request).await;
        self.record(provider, Operation::Read, started, &result);
        result
    }

    async fn call_research(
        &self,
        provider: &Arc<dyn WebProvider>,
        request: &ResearchRequest,
    ) -> Result<ResearchResult, WebError> {
        let started = Instant::now();
        let result = provider.research(request).await;
        self.record(provider, Operation::Research, started, &result);
        result
    }

    fn record<T>(
        &self,
        provider: &Arc<dyn WebProvider>,
        operation: Operation,
        started: Instant,
        result: &Result<T, WebError>,
    ) {
        let latency = started.elapsed();
        match result {
            Ok(_) => self.ledger.record(
                provider.name(),
                operation,
                latency,
                true,
                provider.cost_hint(operation),
                None,
            ),
            Err(err) => self.ledger.record(
                provider.name(),
                operation,
                latency,
                false,
                provider.cost_hint(operation),
                Some(err.kind),
            ),
        }
    }

    fn exhausted<T>(
        &self,
        operation: &str,
        failures: Vec<WebError>,
        last_retryable: Option<WebError>,
    ) -> Result<T, WebError> {
        let has_retryable = last_retryable.is_some();
        if has_retryable {
            let concrete = failures
                .into_iter()
                .filter(|error| !error.is_unsupported())
                .collect::<Vec<_>>();
            if concrete.len() <= 1 {
                return Err(last_retryable.expect("checked above"));
            }
            return Err(WebError::all_failed(operation, concrete));
        }
        let unsupported = failures
            .iter()
            .map(|error| {
                format!(
                    "{}: {}",
                    error.provider.as_deref().unwrap_or("provider"),
                    error.message()
                )
            })
            .collect::<Vec<_>>();
        Err(self.no_providers(operation, &unsupported))
    }

    fn no_providers(&self, operation: &str, unsupported: &[String]) -> WebError {
        let mut details = self
            .skips
            .iter()
            .map(|skip| format!("{}: {}", skip.provider, skip.reason))
            .collect::<Vec<_>>();
        details.extend(unsupported.iter().cloned());
        let mut message = if details.is_empty() {
            format!("no {operation} provider is configured")
        } else {
            format!(
                "no {operation} provider is available: {}",
                details.join("; ")
            )
        };
        if operation == "research" {
            message.push_str(" (web_research requires a configured PARALLEL_API_KEY)");
        }
        WebError::no_providers(message)
    }
}

fn settings_tag(providers: &[Arc<dyn WebProvider>]) -> String {
    let mut namespaces = providers
        .iter()
        .map(|provider| format!("{}={}", provider.name(), provider.cache_namespace()))
        .collect::<Vec<_>>();
    namespaces.sort();
    namespaces.join("|")
}

fn to_json<T: serde::Serialize>(value: T) -> Result<Value, WebError> {
    serde_json::to_value(value)
        .map_err(|err| WebError::provider_error("tool", format!("could not encode result: {err}")))
}
