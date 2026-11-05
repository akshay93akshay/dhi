//! Firecrawl Search and Scrape (`web_read`) provider.
//!
//! `web_parse` is folded into `web_read`: `/v2/scrape` is also Firecrawl's
//! documented URL path for PDF and DOCX extraction, so callers use one read
//! tool regardless of the document type.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use url::{Host, Url};

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

pub const DEFAULT_CLOUD_BASE_URL: &str = "https://api.firecrawl.dev";
pub const DEFAULT_LOCAL_BASE_URL: &str = "http://127.0.0.1:3002";
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Firecrawl connection settings. A key is optional only for a local base URL.
#[derive(Clone)]
pub struct FirecrawlConfig {
    pub api_key: Option<Secret>,
    pub base_url: Option<String>,
    pub timeout: Duration,
    pub allow_private_targets: bool,
}

impl std::fmt::Debug for FirecrawlConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FirecrawlConfig")
            .field("api_key", &self.api_key)
            .field("base_url", &self.base_url.as_deref().map(redact_url))
            .field("timeout", &self.timeout)
            .field("allow_private_targets", &self.allow_private_targets)
            .finish()
    }
}

impl Default for FirecrawlConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            base_url: None,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            allow_private_targets: false,
        }
    }
}

impl FirecrawlConfig {
    pub fn new(base_url: impl Into<String>, api_key: Option<Secret>, timeout: Duration) -> Self {
        Self {
            api_key,
            base_url: Some(base_url.into()),
            timeout,
            allow_private_targets: false,
        }
    }

    pub fn with_allow_private_targets(mut self, allow: bool) -> Self {
        self.allow_private_targets = allow;
        self
    }

    /// The effective endpoint, if any. A key without a base URL selects
    /// Firecrawl Cloud; with neither, the provider is unconfigured.
    pub fn endpoint(&self) -> Option<String> {
        match (&self.base_url, &self.api_key) {
            (Some(base_url), _) if !base_url.trim().is_empty() => {
                Some(base_url.trim().trim_end_matches('/').to_string())
            }
            (_, Some(_)) => Some(DEFAULT_CLOUD_BASE_URL.to_string()),
            _ => None,
        }
    }

    /// `None` when usable; otherwise a clear skip reason for the router.
    pub fn unavailable_reason(&self) -> Option<String> {
        let Some(endpoint) = self.endpoint() else {
            return Some(
                "Firecrawl is not configured (set FIRECRAWL_BASE_URL or FIRECRAWL_API_KEY)"
                    .to_string(),
            );
        };
        if self.api_key.is_none() && !is_local_base_url(&endpoint) {
            return Some(format!(
                "Firecrawl has no FIRECRAWL_API_KEY and {} is not a local base URL",
                redact_url(&endpoint)
            ));
        }
        None
    }
}

/// Whether a base URL points at loopback or a `.local` host. Only these may be
/// used without an API key. IPs are parsed, so `127.evil.com` is not local.
pub fn is_local_base_url(raw: &str) -> bool {
    let Ok(url) = Url::parse(raw.trim()) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    match url.host() {
        Some(Host::Ipv4(ip)) => ip.is_loopback() || ip.is_unspecified(),
        Some(Host::Ipv6(ip)) => ip.is_loopback() || ip.is_unspecified(),
        Some(Host::Domain(domain)) => {
            let domain = domain.trim_end_matches('.').to_ascii_lowercase();
            domain == "localhost" || domain.ends_with(".localhost") || domain.ends_with(".local")
        }
        None => false,
    }
}

/// Firecrawl REST client.
pub struct FirecrawlProvider {
    http: reqwest::Client,
    api_key: Option<Secret>,
    base_url: String,
    timeout: Duration,
    allow_private_targets: AtomicBool,
}

impl FirecrawlProvider {
    pub fn new(config: FirecrawlConfig) -> Result<Self, WebError> {
        if let Some(reason) = config.unavailable_reason() {
            return Err(WebError::invalid_input(reason));
        }
        let base_url = config
            .endpoint()
            .ok_or_else(|| WebError::invalid_input("Firecrawl is not configured"))?;
        let base_url = validate_url("Firecrawl base URL", &base_url)?;
        let http = build_client(config.timeout)?;
        Ok(Self {
            http,
            api_key: config.api_key,
            base_url: base_url.trim_end_matches('/').to_string(),
            timeout: config.timeout,
            allow_private_targets: AtomicBool::new(config.allow_private_targets),
        })
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    fn secrets(&self) -> Vec<&Secret> {
        self.api_key.iter().collect()
    }

    fn authorization(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => request.bearer_auth(key.expose()),
            None => request,
        }
    }

    fn body_timeout_millis(&self) -> u64 {
        (self.timeout.as_secs().saturating_mul(1_000)).clamp(1, 300_000)
    }

    async fn response_text(
        &self,
        operation: &str,
        response: reqwest::Response,
    ) -> Result<String, WebError> {
        let status = response.status();
        let headers = response.headers().clone();
        let secrets = self.secrets();
        let limit = if status.is_success() {
            MAX_SUCCESS_BODY_BYTES
        } else {
            MAX_ERROR_BODY_BYTES
        };
        let text = read_body_capped(response, limit)
            .await
            .map_err(|err| send_error("firecrawl", operation, err, self.timeout, &secrets))?;
        if !status.is_success() {
            return Err(status_error(
                "firecrawl",
                operation,
                status,
                retry_after(&headers),
                &text,
                &secrets,
            ));
        }
        Ok(text)
    }
}

#[async_trait]
impl WebProvider for FirecrawlProvider {
    fn name(&self) -> &'static str {
        "firecrawl"
    }

    fn cache_namespace(&self) -> String {
        format!("firecrawl|{}", self.base_url)
    }

    fn set_allow_private_targets(&self, allow: bool) {
        self.allow_private_targets.store(allow, Ordering::Relaxed);
    }

    fn cost_hint(&self, _operation: Operation) -> Cost {
        Cost::Unknown
    }

    async fn search(&self, request: &SearchRequest) -> Result<Vec<SearchResult>, WebError> {
        validate_search(request)?;
        let mut body = json!({
            "query": request.query.trim(),
            "limit": request.limit,
            "timeout": self.body_timeout_millis(),
        });
        if !request.include_domains.is_empty() {
            body["includeDomains"] = json!(request.include_domains);
        }
        if !request.exclude_domains.is_empty() {
            body["excludeDomains"] = json!(request.exclude_domains);
        }
        if let Some(minutes) = request.recency_minutes {
            body["tbs"] = json!(tbs_for_recency(minutes));
        }
        let response = self
            .authorization(self.http.post(self.endpoint("/v2/search")))
            .json(&body)
            .send()
            .await
            .map_err(|err| send_error("firecrawl", "search", err, self.timeout, &self.secrets()))?;
        let text = self.response_text("search", response).await?;
        let body = parse_json("firecrawl", "search", &text, &self.secrets())?;
        ensure_success("search", &body, &self.secrets())?;
        let data = body
            .get("data")
            .filter(|data| !data.is_null())
            .ok_or_else(|| {
                WebError::provider_error("firecrawl", "Firecrawl search response carries no data")
            })?;
        let raw = data
            .get("web")
            .and_then(Value::as_array)
            .or_else(|| data.as_array())
            .ok_or_else(|| {
                WebError::provider_error(
                    "firecrawl",
                    "Firecrawl search response carries no data.web results",
                )
            })?;
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
            "url": url,
            "formats": ["markdown"],
            "timeout": self.body_timeout_millis(),
            // Explicitly bypass Firecrawl's page cache; the router cache is
            // the only cache layer above us.
            "maxAge": 0,
        });
        // `web_parse` is folded into `web_read`: Firecrawl's PDF parser option
        // is added only for document URLs, leaving ordinary HTML reads
        // unchanged.
        if is_document_url(&url) {
            body["parsers"] = json!([{ "type": "pdf", "mode": "auto" }]);
        }
        let response = self
            .authorization(self.http.post(self.endpoint("/v2/scrape")))
            .json(&body)
            .send()
            .await
            .map_err(|err| send_error("firecrawl", "read", err, self.timeout, &self.secrets()))?;
        let text = self.response_text("read", response).await?;
        let body = parse_json("firecrawl", "read", &text, &self.secrets())?;
        ensure_success("read", &body, &self.secrets())?;
        let data = body
            .get("data")
            .filter(|data| !data.is_null())
            .ok_or_else(|| {
                WebError::provider_error("firecrawl", "Firecrawl read response carries no data")
            })?;
        let metadata = data.get("metadata").unwrap_or(&Value::Null);
        let markdown = data
            .get("markdown")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let status_code = metadata
            .get("statusCode")
            .and_then(Value::as_u64)
            .and_then(|status| u16::try_from(status).ok());
        if markdown.trim().is_empty() {
            let detail = metadata
                .get("error")
                .and_then(Value::as_str)
                .or_else(|| data.get("error").and_then(Value::as_str))
                .or_else(|| body.get("error").and_then(Value::as_str))
                .unwrap_or("the response carried no markdown");
            let status_note = status_code
                .map(|status| format!(" (page HTTP {status})"))
                .unwrap_or_default();
            let message = format!(
                "Firecrawl read could not read {}{status_note}: {}",
                redact_url(&url),
                short_error(detail, &self.secrets())
            );
            if looks_like_quota(detail) {
                return Err(WebError::quota("firecrawl", status_code, None, message));
            }
            return Err(WebError::upstream("firecrawl", message));
        }
        let title = metadata
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let final_url = metadata
            .get("url")
            .or_else(|| metadata.get("sourceURL"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .unwrap_or(&url);
        let (markdown, truncated) = cap_document(markdown);
        Ok(ReadResult {
            url: cap_result_url(final_url),
            title: cap_title(title),
            markdown,
            truncated,
            status_code,
            provider: "firecrawl".to_string(),
        })
    }

    async fn research(&self, _request: &ResearchRequest) -> Result<ResearchResult, WebError> {
        Err(WebError::unsupported(
            "firecrawl",
            "Firecrawl does not expose a research endpoint; configure PARALLEL_API_KEY for web_research",
        ))
    }
}

fn tbs_for_recency(minutes: u64) -> &'static str {
    match minutes {
        0..=60 => "qdr:h",
        61..=1_440 => "qdr:d",
        1_441..=10_080 => "qdr:w",
        10_081..=43_200 => "qdr:m",
        _ => "qdr:y",
    }
}

fn is_document_url(raw: &str) -> bool {
    let Ok(url) = Url::parse(raw) else {
        return false;
    };
    let path = url.path().to_ascii_lowercase();
    path.ends_with(".pdf") || path.ends_with(".docx")
}

fn ensure_success(operation: &str, body: &Value, secrets: &[&Secret]) -> Result<(), WebError> {
    if body.get("success").and_then(Value::as_bool) != Some(false) {
        return Ok(());
    }
    let detail = match body.get("error") {
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => body.to_string(),
    };
    let message = format!(
        "Firecrawl {operation} failed: {}",
        short_error(&detail, secrets)
    );
    if looks_like_quota(&detail) {
        Err(WebError::quota("firecrawl", None, None, message))
    } else {
        Err(WebError::upstream("firecrawl", message))
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
    let snippet = ["description", "snippet", "markdown", "content"]
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
        provider: "firecrawl".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_local_base_urls_may_skip_the_key() {
        assert!(is_local_base_url("http://127.0.0.1:3002"));
        assert!(is_local_base_url("http://localhost:3002"));
        assert!(is_local_base_url("http://[::1]:3002"));
        assert!(!is_local_base_url("https://api.firecrawl.dev"));
        assert!(!is_local_base_url("http://127.evil.com:3002"));
    }

    #[test]
    fn detects_pdf_and_docx_urls() {
        assert!(is_document_url("https://example.com/report.pdf"));
        assert!(is_document_url("https://example.com/report.docx?raw=1"));
        assert!(!is_document_url("https://example.com/report.html"));
    }

    #[test]
    fn maps_recency_to_tbs() {
        assert_eq!(tbs_for_recency(60), "qdr:h");
        assert_eq!(tbs_for_recency(1_440), "qdr:d");
        assert_eq!(tbs_for_recency(10_080), "qdr:w");
        assert_eq!(tbs_for_recency(43_200), "qdr:m");
        assert_eq!(tbs_for_recency(43_201), "qdr:y");
    }

    #[test]
    fn keyless_cloud_is_rejected() {
        let config = FirecrawlConfig {
            api_key: None,
            base_url: Some("https://api.firecrawl.dev".to_string()),
            timeout: Duration::from_secs(1),
            allow_private_targets: false,
        };
        assert!(config.unavailable_reason().is_some());
    }

    #[test]
    fn skip_reason_redacts_url_credentials() {
        let config = FirecrawlConfig {
            api_key: None,
            base_url: Some("https://user:pass@api.firecrawl.dev".to_string()),
            timeout: Duration::from_secs(1),
            allow_private_targets: false,
        };
        let reason = config.unavailable_reason().expect("reason");
        assert!(!reason.contains("user"), "{reason}");
        assert!(!reason.contains("pass"), "{reason}");
        assert!(reason.contains("[redacted]") || reason.contains("api.firecrawl.dev"));
    }
}
