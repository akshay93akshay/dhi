//! The provider-neutral error type used by every DHI web provider.
//!
//! The router falls back for rate limits, auth/quota failures, timeouts, 5xx,
//! network failures, and upstream content failures. Malformed caller input,
//! 400/404/422, and unsupported operations stop or skip according to the
//! router's documented policy.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Why a provider call failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The request never reached a provider because it was malformed.
    InvalidInput,
    /// No provider is configured or available for this operation.
    NoProviders,
    /// HTTP 429 or an explicit rate limit.
    RateLimited,
    /// A request/response deadline elapsed.
    Timeout,
    /// HTTP 5xx.
    Server,
    /// DNS, connection, or other transport failure.
    Network,
    /// HTTP 4xx caller error that must not fall back (400, 404, 422, ...).
    Client,
    /// HTTP 401/403 or another credential failure.
    Auth,
    /// HTTP 402 or a known quota/allowance error.
    Quota,
    /// A 2xx response whose payload reports an upstream fetch/content failure.
    Upstream,
    /// The provider has no implementation for this operation.
    Unsupported,
    /// A malformed or otherwise unusable provider response.
    Other,
    /// Every eligible provider failed with a fallback-eligible error.
    AllFailed,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::NoProviders => "no_providers",
            Self::RateLimited => "rate_limited",
            Self::Timeout => "timeout",
            Self::Server => "server",
            Self::Network => "network",
            Self::Client => "client",
            Self::Auth => "auth",
            Self::Quota => "quota",
            Self::Upstream => "upstream",
            Self::Unsupported => "unsupported",
            Self::Other => "other",
            Self::AllFailed => "all_failed",
        }
    }

    /// Whether the router may try the next provider for this failure.
    pub fn is_fallback(self) -> bool {
        matches!(
            self,
            Self::RateLimited
                | Self::Timeout
                | Self::Server
                | Self::Network
                | Self::Auth
                | Self::Quota
                | Self::Upstream
        )
    }
}

/// A provider-neutral failure. API keys are redacted before a body or message
/// reaches this type. `attempts` is populated only for an aggregate
/// [`ErrorKind::AllFailed`] error.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct WebError {
    pub kind: ErrorKind,
    pub provider: Option<String>,
    pub status: Option<u16>,
    pub retry_after: Option<Duration>,
    message: String,
    attempts: Vec<WebError>,
}

impl WebError {
    pub fn new(
        kind: ErrorKind,
        provider: Option<impl Into<String>>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            provider: provider.map(Into::into),
            status: None,
            retry_after: None,
            message: message.into(),
            attempts: Vec::new(),
        }
    }

    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidInput, None::<String>, message)
    }

    pub fn no_providers(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NoProviders, None::<String>, message)
    }

    pub fn unsupported(provider: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unsupported, Some(provider), message)
    }

    pub fn rate_limited(
        provider: impl Into<String>,
        status: u16,
        retry_after: Option<Duration>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind: ErrorKind::RateLimited,
            provider: Some(provider.into()),
            status: Some(status),
            retry_after,
            message: message.into(),
            attempts: Vec::new(),
        }
    }

    pub fn timeout(provider: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Timeout, Some(provider), message)
    }

    pub fn http_timeout(
        provider: impl Into<String>,
        status: u16,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind: ErrorKind::Timeout,
            provider: Some(provider.into()),
            status: Some(status),
            retry_after: None,
            message: message.into(),
            attempts: Vec::new(),
        }
    }

    pub fn server(provider: impl Into<String>, status: u16, message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Server,
            provider: Some(provider.into()),
            status: Some(status),
            retry_after: None,
            message: message.into(),
            attempts: Vec::new(),
        }
    }

    pub fn network(provider: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Network, Some(provider), message)
    }

    pub fn client(provider: impl Into<String>, status: u16, message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Client,
            provider: Some(provider.into()),
            status: Some(status),
            retry_after: None,
            message: message.into(),
            attempts: Vec::new(),
        }
    }

    pub fn auth(
        provider: impl Into<String>,
        status: u16,
        retry_after: Option<Duration>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind: ErrorKind::Auth,
            provider: Some(provider.into()),
            status: Some(status),
            retry_after,
            message: message.into(),
            attempts: Vec::new(),
        }
    }

    pub fn quota(
        provider: impl Into<String>,
        status: Option<u16>,
        retry_after: Option<Duration>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind: ErrorKind::Quota,
            provider: Some(provider.into()),
            status,
            retry_after,
            message: message.into(),
            attempts: Vec::new(),
        }
    }

    pub fn upstream(provider: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Upstream, Some(provider), message)
    }

    pub fn provider_error(provider: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Other, Some(provider), message)
    }

    /// Builds the aggregate error while preserving the last concrete attempt's
    /// provider, status, and retry-after fields.
    pub fn all_failed(operation: &str, attempts: Vec<WebError>) -> Self {
        let last = attempts.last().cloned();
        let detail = attempts
            .iter()
            .map(|attempt| {
                let provider = attempt.provider.as_deref().unwrap_or("router");
                format!(
                    "{provider}: {} ({})",
                    attempt.message(),
                    attempt.kind.as_str()
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        Self {
            kind: ErrorKind::AllFailed,
            provider: last.as_ref().and_then(|error| error.provider.clone()),
            status: last.as_ref().and_then(|error| error.status),
            retry_after: last.as_ref().and_then(|error| error.retry_after),
            message: format!("all {operation} providers failed: {detail}"),
            attempts,
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn attempts(&self) -> &[WebError] {
        &self.attempts
    }

    /// Whether the router may fall back after this error.
    pub fn is_fallback(&self) -> bool {
        self.kind.is_fallback()
    }

    pub fn is_unsupported(&self) -> bool {
        self.kind == ErrorKind::Unsupported
    }
}
