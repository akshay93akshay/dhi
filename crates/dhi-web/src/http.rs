//! Shared HTTP client construction, body caps, and error mapping.

use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::RETRY_AFTER;

use crate::Secret;
use crate::error::WebError;
use crate::validation::short_error;

/// Largest successful provider body accepted (JSON only).
pub(crate) const MAX_SUCCESS_BODY_BYTES: usize = 4 * 1024 * 1024;
/// Largest error body retained for a message.
pub(crate) const MAX_ERROR_BODY_BYTES: usize = 16 * 1024;

pub(crate) fn build_client(timeout: Duration) -> Result<reqwest::Client, WebError> {
    reqwest::Client::builder()
        .timeout(timeout)
        // Redirects are disabled deliberately: following a redirect could send
        // provider auth headers to another host. Providers that return their own
        // final URL still surface it in ReadResult.url.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|err| {
            WebError::provider_error("http", format!("could not build HTTP client: {err}"))
        })
}

/// Reads at most `max` bytes from a response body. The stream is dropped once
/// the cap is reached, so a hostile server cannot force an unbounded read.
pub(crate) async fn read_body_capped(
    mut response: reqwest::Response,
    max: usize,
) -> Result<String, reqwest::Error> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        let remaining = max.saturating_sub(body.len());
        if chunk.len() > remaining {
            body.extend_from_slice(&chunk[..remaining]);
            break;
        }
        body.extend_from_slice(&chunk);
        if body.len() >= max {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Maps a transport failure to timeout or network. Callers pass the real
/// secret so even unusual transport diagnostics are scrubbed.
pub(crate) fn send_error(
    provider: &'static str,
    operation: &str,
    err: reqwest::Error,
    timeout: Duration,
    secrets: &[&Secret],
) -> WebError {
    if err.is_timeout() {
        WebError::timeout(
            provider,
            format!(
                "{provider} {operation} timed out after {}s",
                timeout.as_secs_f32()
            ),
        )
    } else {
        WebError::network(
            provider,
            format!(
                "{provider} {operation} request failed: {}",
                short_error(&err.to_string(), secrets)
            ),
        )
    }
}

/// Maps a non-success status to the router's fallback classes.
pub(crate) fn status_error(
    provider: &'static str,
    operation: &str,
    status: StatusCode,
    retry_after: Option<Duration>,
    body: &str,
    secrets: &[&Secret],
) -> WebError {
    let code = status.as_u16();
    let detail = short_error(body, secrets);
    let message = format!("{provider} {operation} failed with HTTP {status}: {detail}");
    if (401..=403).contains(&code) && looks_like_quota(body) {
        return WebError::quota(provider, Some(code), retry_after, message);
    }
    match code {
        401 | 403 => WebError::auth(provider, code, retry_after, message),
        402 => WebError::quota(provider, Some(code), retry_after, message),
        408 => WebError::http_timeout(provider, code, message),
        429 => WebError::rate_limited(provider, code, retry_after, message),
        500..=599 => WebError::server(provider, code, message),
        _ => WebError::client(provider, code, message),
    }
}

/// Whether a provider body names a quota, allowance, or billing failure. Used
/// for 200 responses whose payload reports the failure instead of the status.
pub(crate) fn looks_like_quota(body: &str) -> bool {
    let body = body.to_ascii_lowercase();
    [
        "insufficient_credits",
        "free_allowance_exhausted",
        "quota",
        "allowance",
        "insufficient credit",
        "credit balance",
        "billing",
        "rate limit",
        "rate_limit",
        "too many requests",
    ]
    .iter()
    .any(|needle| body.contains(needle))
}

pub(crate) fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

pub(crate) fn parse_json(
    provider: &'static str,
    operation: &str,
    text: &str,
    secrets: &[&Secret],
) -> Result<serde_json::Value, WebError> {
    serde_json::from_str(text).map_err(|err| {
        WebError::provider_error(
            provider,
            format!(
                "{provider} {operation} returned invalid JSON: {err}: {}",
                short_error(text, secrets)
            ),
        )
    })
}
