//! TinyFish Search and Fetch REST providers.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::error::WebError;
use crate::http::{
    MAX_ERROR_BODY_BYTES, MAX_SUCCESS_BODY_BYTES, build_client, looks_like_quota, parse_json,
    read_body_capped, retry_after, send_error, status_error,
};
use crate::ledger::{Cost, Operation};
use crate::model::{
    ReadRequest, ReadResult, ResearchRequest, ResearchResult, SearchRequest, SearchResult,
};
use crate::secret::redact_url;
use crate::validation::{
    cap_document, cap_result_url, cap_snippet, cap_title, short_error, validate_read_request,
    validate_search, validate_target_url, validate_url,
};
use crate::{Secret, WebProvider};

pub const DEFAULT_SEARCH_URL: &str = "https://api.search.tinyfish.ai";
pub const DEFAULT_FETCH_URL: &str = "https://api.fetch.tinyfish.ai";
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;
pub const DEFAULT_FETCH_TIMEOUT_SECS: u64 = 180;
pub const SEARCH_FREE_DAILY_LIMIT: u64 = 12_000;
pub const FETCH_FREE_DAILY_LIMIT: u64 = 1_000;

/// TinyFish connection settings. The key is optional so the router can skip an
/// unconfigured provider with a clear reason.
#[derive(Clone)]
pub struct TinyFishConfig {
    pub api_key: Option<Secret>,
    pub search_url: String,
    pub fetch_url: String,
    pub timeout: Duration,
    /// Fetch gets its own, longer deadline; browsers are slower than search.
    pub fetch_timeout: Duration,
    pub allow_private_targets: bool,
}

impl std::fmt::Debug for TinyFishConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TinyFishConfig")
            .field("api_key", &self.api_key)
            .field("search_url", &redact_url(&self.search_url))
            .field("fetch_url", &redact_url(&self.fetch_url))
            .field("timeout", &self.timeout)
            .field("fetch_timeout", &self.fetch_timeout)
            .field("allow_private_targets", &self.allow_private_targets)
            .finish()
    }
}

impl Default for TinyFishConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            search_url: DEFAULT_SEARCH_URL.to_string(),
            fetch_url: DEFAULT_FETCH_URL.to_string(),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            fetch_timeout: Duration::from_secs(DEFAULT_FETCH_TIMEOUT_SECS),
            allow_private_targets: false,
        }
    }
}

impl TinyFishConfig {
    pub fn new(api_key: Secret, timeout: Duration) -> Self {
        Self {
            api_key: Some(api_key),
            timeout,
            ..Self::default()
        }
    }

    pub fn with_search_url(mut self, url: impl Into<String>) -> Self {
        self.search_url = url.into();
        self
    }

    pub fn with_fetch_url(mut self, url: impl Into<String>) -> Self {
        self.fetch_url = url.into();
        self
    }

    pub fn with_fetch_timeout(mut self, timeout: Duration) -> Self {
        self.fetch_timeout = timeout;
        self
    }

    pub fn with_allow_private_targets(mut self, allow: bool) -> Self {
        self.allow_private_targets = allow;
        self
    }
}

/// TinyFish search/fetch client.
pub struct TinyFishProvider {
    http: reqwest::Client,
    api_key: Secret,
    search_url: String,
    fetch_url: String,
    timeout: Duration,
    fetch_timeout: Duration,
    allow_private_targets: AtomicBool,
    search_calls: AtomicU64,
    fetch_calls: AtomicU64,
}

impl TinyFishProvider {
    pub fn new(config: TinyFishConfig) -> Result<Self, WebError> {
        let api_key = config.api_key.clone().ok_or_else(|| {
            WebError::invalid_input("TinyFish is not configured (set TINYFISH_API_KEY)")
        })?;
        let search_url = validate_url("TinyFish search endpoint", &config.search_url)?;
        let fetch_url = validate_url("TinyFish fetch endpoint", &config.fetch_url)?;
        let fetch_timeout = config.fetch_timeout.max(Duration::from_secs(150));
        let http = build_client(config.timeout)?;
        Ok(Self {
            http,
            api_key,
            search_url,
            fetch_url,
            timeout: config.timeout,
            fetch_timeout,
            allow_private_targets: AtomicBool::new(config.allow_private_targets),
            search_calls: AtomicU64::new(0),
            fetch_calls: AtomicU64::new(0),
        })
    }

    fn secrets(&self) -> [&Secret; 1] {
        [&self.api_key]
    }

    async fn response_text(
        &self,
        operation: &str,
        response: reqwest::Response,
        timeout: Duration,
    ) -> Result<String, WebError> {
        let status = response.status();
        let headers = response.headers().clone();
        let limit = if status.is_success() {
            MAX_SUCCESS_BODY_BYTES
        } else {
            MAX_ERROR_BODY_BYTES
        };
        let text = read_body_capped(response, limit)
            .await
            .map_err(|err| send_error("tinyfish", operation, err, timeout, &self.secrets()))?;
        if !status.is_success() {
            return Err(status_error(
                "tinyfish",
                operation,
                status,
                retry_after(&headers),
                &text,
                &self.secrets(),
            ));
        }
        Ok(text)
    }
}

#[async_trait]
impl WebProvider for TinyFishProvider {
    fn name(&self) -> &'static str {
        "tinyfish"
    }

    fn cache_namespace(&self) -> String {
        format!("tinyfish|{}|{}", self.search_url, self.fetch_url)
    }

    fn set_allow_private_targets(&self, allow: bool) {
        self.allow_private_targets.store(allow, Ordering::Relaxed);
    }

    fn cost_hint(&self, operation: Operation) -> Cost {
        match operation {
            Operation::Search
                if self.search_calls.load(Ordering::Relaxed) <= SEARCH_FREE_DAILY_LIMIT =>
            {
                Cost::Free
            }
            Operation::Read
                if self.fetch_calls.load(Ordering::Relaxed) <= FETCH_FREE_DAILY_LIMIT =>
            {
                Cost::Free
            }
            _ => Cost::Unknown,
        }
    }

    async fn search(&self, request: &SearchRequest) -> Result<Vec<SearchResult>, WebError> {
        validate_search(request)?;
        let mut params: Vec<(&str, String)> = vec![("query", request.query.trim().to_string())];
        if let Some(objective) = request
            .objective
            .as_deref()
            .map(str::trim)
            .filter(|objective| !objective.is_empty())
        {
            params.push(("purpose", objective.to_string()));
        }
        if !request.include_domains.is_empty() {
            params.push(("include_domains", request.include_domains.join(",")));
        }
        if !request.exclude_domains.is_empty() {
            params.push(("exclude_domains", request.exclude_domains.join(",")));
        }
        if let Some(minutes) = request.recency_minutes {
            params.push(("recency_minutes", minutes.to_string()));
        }
        self.search_calls.fetch_add(1, Ordering::Relaxed);
        let response = self
            .http
            .get(&self.search_url)
            .query(&params)
            .header("X-API-Key", self.api_key.expose())
            .send()
            .await
            .map_err(|err| send_error("tinyfish", "search", err, self.timeout, &self.secrets()))?;
        let text = self.response_text("search", response, self.timeout).await?;
        let body = parse_json("tinyfish", "search", &text, &self.secrets())?;
        let Some(raw) = body.get("results").and_then(Value::as_array) else {
            let message = format!(
                "TinyFish search response carries no results array: {}",
                short_error(&text, &self.secrets())
            );
            return Err(if looks_like_quota(&text) {
                WebError::quota("tinyfish", None, None, message)
            } else {
                WebError::provider_error("tinyfish", message)
            });
        };
        Ok(raw
            .iter()
            .filter_map(normalize_search_item)
            .take(request.limit)
            .collect())
    }

    async fn read(&self, request: &ReadRequest) -> Result<ReadResult, WebError> {
        validate_read_request(&request.url, request.objective.as_deref())?;
        let url = validate_target_url(
            "web_read",
            &request.url,
            self.allow_private_targets.load(Ordering::Relaxed),
        )
        .await?;
        let mut body = json!({
            "urls": [url],
            "format": "markdown",
            // Never accept an upstream cached page when the router asks for a
            // fresh read. The router cache is the only cache layer above us.
            "ttl": 0,
        });
        if let Some(objective) = request
            .objective
            .as_deref()
            .map(str::trim)
            .filter(|objective| !objective.is_empty())
        {
            body["purpose"] = json!(objective);
        }
        self.fetch_calls.fetch_add(1, Ordering::Relaxed);
        let response = self
            .http
            .post(&self.fetch_url)
            .timeout(self.fetch_timeout)
            .header("X-API-Key", self.api_key.expose())
            .json(&body)
            .send()
            .await
            .map_err(|err| {
                send_error("tinyfish", "read", err, self.fetch_timeout, &self.secrets())
            })?;
        let text = self
            .response_text("read", response, self.fetch_timeout)
            .await?;
        let body = parse_json("tinyfish", "read", &text, &self.secrets())?;
        normalize_read(&url, &body, &text, &self.secrets())
    }

    async fn research(&self, _request: &ResearchRequest) -> Result<ResearchResult, WebError> {
        Err(WebError::unsupported(
            "tinyfish",
            "TinyFish does not expose a research endpoint; configure PARALLEL_API_KEY for web_research",
        ))
    }
}

fn normalize_search_item(item: &Value) -> Option<SearchResult> {
    let url = item
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty())?;
    let title = item
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let snippet = ["snippet", "description"]
        .into_iter()
        .find_map(|field| {
            item.get(field)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
        })
        .map(cap_snippet)
        .unwrap_or_default();
    Some(SearchResult {
        title: cap_title(title),
        url: cap_result_url(url),
        snippet,
        provider: "tinyfish".to_string(),
    })
}

fn normalize_read(
    requested_url: &str,
    body: &Value,
    raw_text: &str,
    secrets: &[&Secret],
) -> Result<ReadResult, WebError> {
    let results = body
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let selected = results
        .iter()
        .find(|item| {
            let candidate = item
                .get("final_url")
                .or_else(|| item.get("url"))
                .and_then(Value::as_str);
            candidate == Some(requested_url)
        })
        .or_else(|| results.first());
    let Some(item) = selected else {
        let detail = body
            .get("errors")
            .filter(|errors| !errors.is_null())
            .map(Value::to_string)
            .unwrap_or_else(|| short_error(raw_text, secrets));
        let message = format!(
            "TinyFish read could not read {}: {}",
            redact_url(requested_url),
            short_error(&detail, secrets)
        );
        return Err(if looks_like_quota(&detail) {
            WebError::quota("tinyfish", None, None, message)
        } else {
            WebError::upstream("tinyfish", message)
        });
    };
    let markdown = ["text", "markdown", "content"]
        .into_iter()
        .find_map(|field| {
            item.get(field)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
        })
        .unwrap_or_default();
    if markdown.is_empty() {
        let message = format!(
            "TinyFish read returned no markdown for {}",
            redact_url(requested_url)
        );
        return Err(if looks_like_quota(&body.to_string()) {
            WebError::quota("tinyfish", None, None, message)
        } else {
            WebError::upstream("tinyfish", message)
        });
    }
    let title = item
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let final_url = item
        .get("final_url")
        .or_else(|| item.get("url"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .unwrap_or(requested_url);
    let status_code = item
        .get("status_code")
        .or_else(|| item.get("status"))
        .and_then(Value::as_u64)
        .and_then(|status| u16::try_from(status).ok());
    let (markdown, truncated) = cap_document(markdown);
    Ok(ReadResult {
        url: cap_result_url(final_url),
        title: cap_title(title),
        markdown,
        truncated,
        status_code,
        provider: "tinyfish".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> TinyFishProvider {
        TinyFishProvider::new(TinyFishConfig::new(
            Secret::new("test-secret").expect("secret"),
            Duration::from_secs(1),
        ))
        .expect("provider")
    }

    #[test]
    fn free_cost_stops_at_the_daily_allowance() {
        let provider = provider();
        assert_eq!(provider.cost_hint(Operation::Search), Cost::Free);
        assert_eq!(provider.cost_hint(Operation::Read), Cost::Free);

        provider
            .search_calls
            .store(SEARCH_FREE_DAILY_LIMIT + 1, Ordering::Relaxed);
        assert_eq!(provider.cost_hint(Operation::Search), Cost::Unknown);

        provider
            .fetch_calls
            .store(FETCH_FREE_DAILY_LIMIT + 1, Ordering::Relaxed);
        assert_eq!(provider.cost_hint(Operation::Read), Cost::Unknown);
    }
}
