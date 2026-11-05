//! Parallel Search, Extract, and Task API (research) providers.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use crate::error::WebError;
use crate::http::{
    MAX_ERROR_BODY_BYTES, MAX_SUCCESS_BODY_BYTES, build_client, parse_json, read_body_capped,
    retry_after, send_error, status_error,
};
use crate::ledger::{Cost, Operation};
use crate::model::{
    MAX_CITATIONS, ReadRequest, ReadResult, ResearchRequest, ResearchResult, SearchRequest,
    SearchResult,
};
use crate::secret::redact_url;
use crate::validation::{
    cap_document, cap_research_answer, cap_result_url, cap_snippet, cap_title, normalize_query,
    short_error, validate_read_request, validate_research_question, validate_search,
    validate_target_url, validate_url,
};
use crate::{Secret, WebProvider};

pub const DEFAULT_BASE_URL: &str = "https://api.parallel.ai";
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;
pub const DEFAULT_RESEARCH_TIMEOUT_SECS: u64 = 900;
pub const DEFAULT_RESEARCH_POLL: Duration = Duration::from_secs(2);
const RESEARCH_CLIENT_MARGIN: Duration = Duration::from_secs(5);

/// Parallel connection settings.
#[derive(Clone)]
pub struct ParallelConfig {
    pub api_key: Option<Secret>,
    pub base_url: String,
    pub timeout: Duration,
    /// Overall research deadline and the server-side blocking `timeout`.
    pub research_timeout: Duration,
    /// Delay between retries after an HTTP 408 from the result endpoint.
    pub research_poll_interval: Duration,
    pub allow_private_targets: bool,
}

impl std::fmt::Debug for ParallelConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParallelConfig")
            .field("api_key", &self.api_key)
            .field("base_url", &redact_url(&self.base_url))
            .field("timeout", &self.timeout)
            .field("research_timeout", &self.research_timeout)
            .field("research_poll_interval", &self.research_poll_interval)
            .field("allow_private_targets", &self.allow_private_targets)
            .finish()
    }
}

impl Default for ParallelConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            base_url: DEFAULT_BASE_URL.to_string(),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            research_timeout: Duration::from_secs(DEFAULT_RESEARCH_TIMEOUT_SECS),
            research_poll_interval: DEFAULT_RESEARCH_POLL,
            allow_private_targets: false,
        }
    }
}

impl ParallelConfig {
    pub fn new(api_key: Secret, timeout: Duration) -> Self {
        Self {
            api_key: Some(api_key),
            timeout,
            ..Self::default()
        }
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    pub fn with_research_timeout(mut self, timeout: Duration) -> Self {
        self.research_timeout = timeout;
        self
    }

    pub fn with_research_poll_interval(mut self, interval: Duration) -> Self {
        self.research_poll_interval = interval;
        self
    }

    pub fn with_allow_private_targets(mut self, allow: bool) -> Self {
        self.allow_private_targets = allow;
        self
    }
}

/// Parallel REST client.
pub struct ParallelProvider {
    http: reqwest::Client,
    api_key: Secret,
    base_url: String,
    timeout: Duration,
    research_timeout: Duration,
    research_poll_interval: Duration,
    allow_private_targets: AtomicBool,
}

impl ParallelProvider {
    pub fn new(config: ParallelConfig) -> Result<Self, WebError> {
        let api_key = config.api_key.clone().ok_or_else(|| {
            WebError::invalid_input("Parallel is not configured (set PARALLEL_API_KEY)")
        })?;
        let base_url = validate_url("Parallel base URL", &config.base_url)?;
        let http = build_client(config.timeout)?;
        Ok(Self {
            http,
            api_key,
            base_url: base_url.trim_end_matches('/').to_string(),
            timeout: config.timeout,
            research_timeout: config.research_timeout,
            research_poll_interval: config.research_poll_interval,
            allow_private_targets: AtomicBool::new(config.allow_private_targets),
        })
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    fn secrets(&self) -> [&Secret; 1] {
        [&self.api_key]
    }

    async fn response_text(
        &self,
        operation: &str,
        response: reqwest::Response,
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
            .map_err(|err| send_error("parallel", operation, err, self.timeout, &self.secrets()))?;
        if !status.is_success() {
            return Err(status_error(
                "parallel",
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
impl WebProvider for ParallelProvider {
    fn name(&self) -> &'static str {
        "parallel"
    }

    fn cache_namespace(&self) -> String {
        format!("parallel|{}", self.base_url)
    }

    fn set_allow_private_targets(&self, allow: bool) {
        self.allow_private_targets.store(allow, Ordering::Relaxed);
    }

    fn cost_hint(&self, _operation: Operation) -> Cost {
        Cost::Unknown
    }

    async fn search(&self, request: &SearchRequest) -> Result<Vec<SearchResult>, WebError> {
        validate_search(request)?;
        let query = normalize_query(&request.query);
        let objective = request
            .objective
            .as_deref()
            .map(normalize_query)
            .filter(|objective| !objective.is_empty())
            .unwrap_or_else(|| query.clone());
        let mut queries = request
            .queries
            .iter()
            .map(|query| normalize_query(query))
            .filter(|query| !query.is_empty())
            .collect::<Vec<_>>();
        if queries.is_empty() {
            queries.push(query);
        }
        let mut body = json!({
            "objective": objective,
            "search_queries": queries,
        });
        body["advanced_settings"] = parallel_advanced_settings(request);
        let response = self
            .http
            .post(self.endpoint("/v1/search"))
            .header("x-api-key", self.api_key.expose())
            .json(&body)
            .send()
            .await
            .map_err(|err| send_error("parallel", "search", err, self.timeout, &self.secrets()))?;
        let text = self.response_text("search", response).await?;
        let body = parse_json("parallel", "search", &text, &self.secrets())?;
        let raw = body
            .get("results")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                WebError::provider_error(
                    "parallel",
                    format!(
                        "Parallel search response carries no results array: {}",
                        short_error(&text, &self.secrets())
                    ),
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
            "urls": [url],
            "advanced_settings": {
                "full_content": true,
                "fetch_policy": {
                    // Parallel documents a 600s floor for max_age_seconds.
                    "max_age_seconds": 600,
                    "disable_cache_fallback": true
                }
            }
        });
        if let Some(objective) = request
            .objective
            .as_deref()
            .map(normalize_query)
            .filter(|objective| !objective.is_empty())
        {
            body["objective"] = json!(objective);
        }
        let response = self
            .http
            .post(self.endpoint("/v1/extract"))
            .header("x-api-key", self.api_key.expose())
            .json(&body)
            .send()
            .await
            .map_err(|err| send_error("parallel", "read", err, self.timeout, &self.secrets()))?;
        let text = self.response_text("read", response).await?;
        let body = parse_json("parallel", "read", &text, &self.secrets())?;
        normalize_read(&url, &body, &self.secrets())
    }

    async fn research(&self, request: &ResearchRequest) -> Result<ResearchResult, WebError> {
        validate_research_question(&request.question)?;
        let question = normalize_query(&request.question);
        let body = json!({
            "input": question,
            "processor": request.depth.parallel_processor(),
            "task_spec": {
                "output_schema": {
                    "type": "text",
                    "description": "A concise answer grounded in current web sources, with citations."
                }
            }
        });
        let response = self
            .http
            .post(self.endpoint("/v1/tasks/runs"))
            .header("x-api-key", self.api_key.expose())
            .json(&body)
            .send()
            .await
            .map_err(|err| {
                send_error(
                    "parallel",
                    "research create",
                    err,
                    self.research_timeout,
                    &self.secrets(),
                )
            })?;
        let text = self.response_text("research create", response).await?;
        let body = parse_json("parallel", "research create", &text, &self.secrets())?;
        let run_id = body
            .get("run_id")
            .and_then(Value::as_str)
            .filter(|run_id| !run_id.trim().is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                WebError::provider_error(
                    "parallel",
                    format!(
                        "Parallel research create returned no run_id: {}",
                        short_error(&text, &self.secrets())
                    ),
                )
            })?;

        let result_url = self.endpoint(&format!("/v1/tasks/runs/{run_id}/result"));
        let started = Instant::now();
        let deadline = started
            .checked_add(self.research_timeout)
            .unwrap_or(started + Duration::from_secs(3_600));
        loop {
            let now = Instant::now();
            let remaining = deadline.saturating_duration_since(now);
            if remaining.is_zero() {
                return Err(WebError::timeout(
                    "parallel",
                    format!(
                        "Parallel research result for run {run_id} exceeded {}s",
                        self.research_timeout.as_secs()
                    ),
                ));
            }
            // The server-side blocking timeout matches the configured research
            // timeout, while the HTTP client gets a margin over it and the
            // overall deadline stays authoritative. A 408 keeps the run id and
            // polls the same result endpoint again.
            let timeout_secs = remaining.as_secs().max(1).to_string();
            let client_budget = remaining + RESEARCH_CLIENT_MARGIN;
            let request = self
                .http
                .get(&result_url)
                .query(&[("timeout", timeout_secs)])
                .timeout(client_budget)
                .header("x-api-key", self.api_key.expose())
                .send();
            let response = match tokio::time::timeout(remaining, request).await {
                Err(_) => {
                    return Err(WebError::timeout(
                        "parallel",
                        format!(
                            "Parallel research result for run {run_id} exceeded {}s",
                            self.research_timeout.as_secs()
                        ),
                    ));
                }
                Ok(Err(err)) if err.is_timeout() => {
                    return Err(WebError::timeout(
                        "parallel",
                        format!(
                            "Parallel research result for run {run_id} timed out after {}s",
                            self.research_timeout.as_secs()
                        ),
                    ));
                }
                Ok(Err(err)) => {
                    return Err(send_error(
                        "parallel",
                        "research result",
                        err,
                        self.research_timeout,
                        &self.secrets(),
                    ));
                }
                Ok(Ok(response)) => response,
            };
            let status = response.status();
            let headers = response.headers().clone();
            let limit = if status.is_success() {
                MAX_SUCCESS_BODY_BYTES
            } else {
                MAX_ERROR_BODY_BYTES
            };
            let text = read_body_capped(response, limit).await.map_err(|err| {
                send_error(
                    "parallel",
                    "research result",
                    err,
                    self.research_timeout,
                    &self.secrets(),
                )
            })?;
            if status.as_u16() == 408 {
                let now = Instant::now();
                if now >= deadline {
                    return Err(WebError::timeout(
                        "parallel",
                        format!(
                            "Parallel research result for run {run_id} exceeded {}s",
                            self.research_timeout.as_secs()
                        ),
                    ));
                }
                let sleep = self
                    .research_poll_interval
                    .min(deadline.saturating_duration_since(now));
                tokio::time::sleep(sleep).await;
                continue;
            }
            if !status.is_success() {
                return Err(status_error(
                    "parallel",
                    "research result",
                    status,
                    retry_after(&headers),
                    &text,
                    &self.secrets(),
                ));
            }
            let body = parse_json("parallel", "research result", &text, &self.secrets())?;
            return normalize_research(&body, &self.secrets());
        }
    }
}

fn parallel_advanced_settings(request: &SearchRequest) -> Value {
    let mut settings = Map::new();
    settings.insert("max_results".to_string(), json!(request.limit));
    if !request.include_domains.is_empty() || !request.exclude_domains.is_empty() {
        let mut policy = Map::new();
        if !request.include_domains.is_empty() {
            policy.insert(
                "include_domains".to_string(),
                json!(request.include_domains),
            );
        }
        if request.include_domains.is_empty() && !request.exclude_domains.is_empty() {
            policy.insert(
                "exclude_domains".to_string(),
                json!(request.exclude_domains),
            );
        }
        settings.insert("source_policy".to_string(), Value::Object(policy));
    }
    Value::Object(settings)
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
    let snippet = item
        .get("excerpts")
        .and_then(Value::as_array)
        .map(|excerpts| {
            excerpts
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|excerpt| !excerpt.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .filter(|snippet| !snippet.is_empty())
        .or_else(|| {
            item.get("snippet")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .map(|snippet| cap_snippet(&snippet))
        .unwrap_or_default();
    Some(SearchResult {
        title: cap_title(title),
        url: cap_result_url(url),
        snippet,
        provider: "parallel".to_string(),
    })
}

fn normalize_read(
    requested_url: &str,
    body: &Value,
    secrets: &[&Secret],
) -> Result<ReadResult, WebError> {
    let results = body
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let selected = results
        .iter()
        .find(|item| item.get("url").and_then(Value::as_str) == Some(requested_url))
        .or_else(|| results.first());
    let Some(item) = selected else {
        return Err(extract_error(requested_url, body, secrets));
    };
    let markdown = item
        .get("full_content")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or_default();
    if markdown.is_empty() {
        return Err(WebError::upstream(
            "parallel",
            format!(
                "Parallel extract returned no full_content for {}",
                redact_url(requested_url)
            ),
        ));
    }
    let title = item
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let final_url = item
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .unwrap_or(requested_url);
    let status_code = item
        .get("status_code")
        .and_then(Value::as_u64)
        .and_then(|status| u16::try_from(status).ok());
    let (markdown, truncated) = cap_document(markdown);
    Ok(ReadResult {
        url: cap_result_url(final_url),
        title: cap_title(title),
        markdown,
        truncated,
        status_code,
        provider: "parallel".to_string(),
    })
}

fn extract_error(requested_url: &str, body: &Value, secrets: &[&Secret]) -> WebError {
    let errors = body.get("errors").and_then(Value::as_array);
    let Some(error) = errors.and_then(|errors| {
        errors
            .iter()
            .find(|error| error.get("url").and_then(Value::as_str) == Some(requested_url))
            .or_else(|| errors.first())
    }) else {
        return WebError::upstream(
            "parallel",
            format!(
                "Parallel extract returned no result for {}",
                redact_url(requested_url)
            ),
        );
    };
    let status = error
        .get("http_status_code")
        .and_then(Value::as_u64)
        .and_then(|status| u16::try_from(status).ok());
    let detail = error
        .get("content")
        .and_then(Value::as_str)
        .or_else(|| error.get("error_type").and_then(Value::as_str))
        .unwrap_or("no detail");
    let detail = short_error(detail, secrets);
    let message = format!(
        "Parallel extract could not read {}: {detail}",
        redact_url(requested_url)
    );
    match status {
        None => WebError::upstream("parallel", message),
        Some(429) => WebError::rate_limited("parallel", 429, None, message),
        Some(408) => WebError::http_timeout("parallel", 408, message),
        Some(code @ (401 | 403)) => WebError::auth("parallel", code, None, message),
        Some(402) => WebError::quota("parallel", status, None, message),
        Some(code @ 500..=599) => WebError::server("parallel", code, message),
        Some(code) => WebError::client("parallel", code, message),
    }
}

fn normalize_research(body: &Value, secrets: &[&Secret]) -> Result<ResearchResult, WebError> {
    let run_status = body
        .get("run")
        .and_then(|run| run.get("status"))
        .and_then(Value::as_str);
    if matches!(run_status, Some("failed" | "cancelled" | "cancelling")) {
        let detail = body
            .get("run")
            .and_then(|run| run.get("error"))
            .map(Value::to_string)
            .unwrap_or_else(|| "the task run did not complete".to_string());
        return Err(WebError::upstream(
            "parallel",
            format!(
                "Parallel research failed: {}",
                short_error(&detail, secrets)
            ),
        ));
    }
    let output = body.get("output").ok_or_else(|| {
        WebError::provider_error("parallel", "Parallel research returned no output")
    })?;
    let answer = match output.get("type").and_then(Value::as_str) {
        Some("text") => output
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        Some("json") => output
            .get("content")
            .map(|content| serde_json::to_string_pretty(content).unwrap_or_default())
            .unwrap_or_default(),
        _ => String::new(),
    };
    if answer.trim().is_empty() {
        return Err(WebError::upstream(
            "parallel",
            "Parallel research returned an empty answer",
        ));
    }
    let mut seen = HashSet::new();
    let mut citations = Vec::new();
    let mut citations_truncated = false;
    if let Some(basis) = output.get("basis").and_then(Value::as_array) {
        for field in basis {
            let Some(field_citations) = field.get("citations").and_then(Value::as_array) else {
                continue;
            };
            for citation in field_citations {
                let Some(url) = citation
                    .get("url")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|url| !url.is_empty())
                else {
                    continue;
                };
                if seen.insert(url.to_string()) {
                    if citations.len() >= MAX_CITATIONS {
                        citations_truncated = true;
                        continue;
                    }
                    citations.push(cap_result_url(url));
                }
            }
        }
    }
    let (answer, answer_truncated) = cap_research_answer(&answer);
    Ok(ResearchResult {
        answer,
        citations,
        truncated: answer_truncated || citations_truncated,
        provider: "parallel".to_string(),
    })
}
