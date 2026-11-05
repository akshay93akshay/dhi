//! Environment/config-file configuration and provider ordering.

use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::Secret;
use crate::error::WebError;
use crate::providers::{
    FirecrawlConfig, ParallelConfig, TinyFishConfig, firecrawl::DEFAULT_LOCAL_BASE_URL,
};

pub const DEFAULT_CACHE_TTL_SECS: u64 = 300;
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;
pub const DEFAULT_RESEARCH_TIMEOUT_SECS: u64 = 900;
pub const DEFAULT_OPERATION_TIMEOUT_SECS: u64 = 180;
pub const DEFAULT_TINYFISH_FETCH_TIMEOUT_SECS: u64 = 180;
pub const MAX_CACHE_TTL_SECS: u64 = 7 * 24 * 60 * 60;
pub const MAX_OPERATION_TIMEOUT_SECS: u64 = 3_600;
pub const MAX_RESEARCH_TIMEOUT_SECS: u64 = 3_600;
pub const MIN_TINYFISH_FETCH_TIMEOUT_SECS: u64 = 150;

/// One concrete provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    TinyFish,
    Parallel,
    Firecrawl,
}

impl ProviderKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::TinyFish => "tinyfish",
            Self::Parallel => "parallel",
            Self::Firecrawl => "firecrawl",
        }
    }
}

impl std::fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for ProviderKind {
    type Err = WebError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "tinyfish" => Ok(Self::TinyFish),
            "parallel" => Ok(Self::Parallel),
            "firecrawl" => Ok(Self::Firecrawl),
            other => Err(WebError::invalid_input(format!(
                "unknown web provider {other:?}; expected tinyfish, parallel, or firecrawl"
            ))),
        }
    }
}

/// `DHI_WEB_PROVIDER`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderMode {
    /// Try every configured provider in the documented default order.
    #[default]
    Auto,
    TinyFish,
    Parallel,
    Firecrawl,
}

impl ProviderMode {
    /// Providers tried by `web_search` and `web_read`.
    pub fn search_read_order(self) -> &'static [ProviderKind] {
        match self {
            Self::Auto => &[
                ProviderKind::TinyFish,
                ProviderKind::Parallel,
                ProviderKind::Firecrawl,
            ],
            Self::TinyFish => &[ProviderKind::TinyFish],
            Self::Parallel => &[ProviderKind::Parallel],
            Self::Firecrawl => &[ProviderKind::Firecrawl],
        }
    }

    /// Providers tried by `web_research`. TinyFish and Firecrawl have no
    /// research endpoint, so they never appear here.
    pub fn research_order(self) -> &'static [ProviderKind] {
        match self {
            Self::Auto | Self::Parallel => &[ProviderKind::Parallel],
            Self::TinyFish | Self::Firecrawl => &[],
        }
    }
}

impl FromStr for ProviderMode {
    type Err = WebError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(Self::Auto),
            "tinyfish" => Ok(Self::TinyFish),
            "parallel" => Ok(Self::Parallel),
            "firecrawl" => Ok(Self::Firecrawl),
            other => Err(WebError::invalid_input(format!(
                "DHI_WEB_PROVIDER must be auto, tinyfish, parallel, or firecrawl; got {other:?}"
            ))),
        }
    }
}

/// The user-level DHI config file. Only the `firecrawl` section is read.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DhiConfigFile {
    #[serde(default)]
    pub firecrawl: Option<FirecrawlFileConfig>,
}

/// The `firecrawl` section of `~/.config/dhi/config.json`.
#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FirecrawlFileConfig {
    #[serde(default, alias = "base_url")]
    pub base_url: Option<String>,
    #[serde(default, alias = "api_key")]
    pub api_key: Option<String>,
    #[serde(default, alias = "timeout_secs")]
    pub timeout_secs: Option<u64>,
}

impl std::fmt::Debug for FirecrawlFileConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FirecrawlFileConfig")
            .field(
                "base_url",
                &self.base_url.as_deref().map(crate::secret::redact_url),
            )
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .field("timeout_secs", &self.timeout_secs)
            .finish()
    }
}

/// Resolves the config file path. `DHI_CONFIG` wins; otherwise the
/// OpenCode-style user config directory.
pub fn config_path() -> Option<PathBuf> {
    if let Ok(raw) = std::env::var("DHI_CONFIG") {
        let raw = raw.trim();
        if !raw.is_empty() {
            return Some(PathBuf::from(raw));
        }
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/dhi/config.json"))
}

/// Reads the config file. A missing file is not an error; a malformed file
/// warns and falls back to env/defaults. No key is ever logged.
pub fn load_config_file() -> Option<DhiConfigFile> {
    let path = config_path()?;
    load_config_file_from(&path)
}

pub fn load_config_file_from(path: &std::path::Path) -> Option<DhiConfigFile> {
    match std::fs::read_to_string(path) {
        Ok(raw) => match serde_json::from_str::<DhiConfigFile>(&raw) {
            Ok(config) => Some(config),
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "DHI config is not valid JSON; using env/defaults"
                );
                None
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "could not read DHI config; using env/defaults"
            );
            None
        }
    }
}

/// Per-provider settings supplied by a management store.
///
/// The field-level precedence is: a `Some` override wins over the environment,
/// the environment wins over the config file, and the file wins over defaults.
/// `authoritative` marks a database row, which is also authoritative for the
/// key: `api_key = None` with `authoritative = true` means "this provider has
/// no key", never "fall back to the environment".
#[derive(Clone, Default)]
pub struct ProviderSettingsOverride {
    pub authoritative: bool,
    pub api_key: Option<Secret>,
    pub base_url: Option<String>,
    pub timeout_secs: Option<u64>,
    pub enabled: Option<bool>,
}

impl std::fmt::Debug for ProviderSettingsOverride {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderSettingsOverride")
            .field("authoritative", &self.authoritative)
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .field(
                "base_url",
                &self.base_url.as_deref().map(crate::secret::redact_url),
            )
            .field("timeout_secs", &self.timeout_secs)
            .field("enabled", &self.enabled)
            .finish()
    }
}

/// Resolved web configuration. Secrets live in [`Secret`] and provider
/// `Debug` implementations sanitize URLs.
#[derive(Debug, Clone)]
pub struct WebConfig {
    pub mode: ProviderMode,
    pub tinyfish: TinyFishConfig,
    pub parallel: ParallelConfig,
    pub firecrawl: FirecrawlConfig,
    /// Router cache lifetime; zero disables caching and in-flight de-duplication.
    pub cache_ttl: Duration,
    /// Overall search/read deadline across the fallback chain.
    pub operation_timeout: Duration,
    /// Explicit opt-in for private/loopback target URLs.
    pub allow_private_targets: bool,
    /// Providers switched off by management settings. They are omitted from
    /// both effective orders and are reported as skipped by the router.
    pub disabled: Vec<ProviderKind>,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            mode: ProviderMode::Auto,
            tinyfish: TinyFishConfig::default(),
            parallel: ParallelConfig::default(),
            firecrawl: FirecrawlConfig::default(),
            cache_ttl: Duration::from_secs(DEFAULT_CACHE_TTL_SECS),
            operation_timeout: Duration::from_secs(DEFAULT_OPERATION_TIMEOUT_SECS),
            allow_private_targets: false,
            disabled: Vec::new(),
        }
    }
}

impl WebConfig {
    /// Reads process environment variables and the user config file.
    pub fn from_env() -> Result<Self, WebError> {
        Self::from_env_with_overrides(&HashMap::new())
    }

    /// Reads the environment and config file, then applies management-store
    /// overrides. Database rows win per field; a database row is also
    /// authoritative for its key (including the absence of one).
    pub fn from_env_with_overrides(
        overrides: &HashMap<ProviderKind, ProviderSettingsOverride>,
    ) -> Result<Self, WebError> {
        let file = load_config_file();
        Self::from_lookup_and_file_with_overrides(
            |name| std::env::var(name).ok(),
            file.as_ref().and_then(|config| config.firecrawl.as_ref()),
            overrides,
        )
    }

    /// Deterministic constructor for tests and hosts.
    pub fn from_lookup<F>(lookup: F) -> Result<Self, WebError>
    where
        F: Fn(&str) -> Option<String>,
    {
        Self::from_lookup_and_file_with_overrides(lookup, None, &HashMap::new())
    }

    /// Deterministic constructor with management-store overrides and no
    /// config file.
    pub fn from_lookup_with_overrides<F>(
        lookup: F,
        overrides: &HashMap<ProviderKind, ProviderSettingsOverride>,
    ) -> Result<Self, WebError>
    where
        F: Fn(&str) -> Option<String>,
    {
        Self::from_lookup_and_file_with_overrides(lookup, None, overrides)
    }

    /// Deterministic constructor with an optional config-file Firecrawl
    /// section. Precedence: database overrides over environment over file over
    /// defaults.
    pub fn from_lookup_and_file<F>(
        lookup: F,
        firecrawl_file: Option<&FirecrawlFileConfig>,
    ) -> Result<Self, WebError>
    where
        F: Fn(&str) -> Option<String>,
    {
        Self::from_lookup_and_file_with_overrides(lookup, firecrawl_file, &HashMap::new())
    }

    /// The full deterministic constructor: environment lookup, optional
    /// config-file Firecrawl section, and management-store overrides.
    pub fn from_lookup_and_file_with_overrides<F>(
        lookup: F,
        firecrawl_file: Option<&FirecrawlFileConfig>,
        overrides: &HashMap<ProviderKind, ProviderSettingsOverride>,
    ) -> Result<Self, WebError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let mode = match lookup("DHI_WEB_PROVIDER")
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(raw) => ProviderMode::from_str(raw)?,
            None => ProviderMode::Auto,
        };
        let timeout = Duration::from_secs(parse_secs(
            lookup("DHI_WEB_TIMEOUT_SECS").as_deref(),
            DEFAULT_TIMEOUT_SECS,
            "DHI_WEB_TIMEOUT_SECS",
            false,
            None,
        )?);
        let cache_ttl = Duration::from_secs(parse_secs(
            lookup("DHI_WEB_CACHE_TTL_SECS").as_deref(),
            DEFAULT_CACHE_TTL_SECS,
            "DHI_WEB_CACHE_TTL_SECS",
            true,
            Some(MAX_CACHE_TTL_SECS),
        )?);
        let operation_timeout = Duration::from_secs(parse_secs(
            lookup("DHI_WEB_OPERATION_TIMEOUT_SECS").as_deref(),
            DEFAULT_OPERATION_TIMEOUT_SECS,
            "DHI_WEB_OPERATION_TIMEOUT_SECS",
            false,
            Some(MAX_OPERATION_TIMEOUT_SECS),
        )?);
        let research_timeout = Duration::from_secs(parse_secs(
            lookup("DHI_WEB_RESEARCH_TIMEOUT_SECS").as_deref(),
            DEFAULT_RESEARCH_TIMEOUT_SECS,
            "DHI_WEB_RESEARCH_TIMEOUT_SECS",
            false,
            Some(MAX_RESEARCH_TIMEOUT_SECS),
        )?);
        let fetch_timeout = Duration::from_secs(
            parse_secs(
                lookup("TINYFISH_FETCH_TIMEOUT_SECS").as_deref(),
                DEFAULT_TINYFISH_FETCH_TIMEOUT_SECS,
                "TINYFISH_FETCH_TIMEOUT_SECS",
                false,
                None,
            )?
            .max(MIN_TINYFISH_FETCH_TIMEOUT_SECS),
        );
        let firecrawl_timeout = Duration::from_secs(match lookup("FIRECRAWL_TIMEOUT_SECS") {
            Some(raw) if !raw.trim().is_empty() => parse_secs(
                Some(&raw),
                DEFAULT_TIMEOUT_SECS,
                "FIRECRAWL_TIMEOUT_SECS",
                false,
                None,
            )?,
            _ => firecrawl_file
                .and_then(|section| section.timeout_secs)
                .filter(|seconds| *seconds > 0)
                .unwrap_or(DEFAULT_TIMEOUT_SECS),
        });
        let allow_private_targets = parse_bool(
            lookup("DHI_WEB_ALLOW_PRIVATE_TARGETS").as_deref(),
            "DHI_WEB_ALLOW_PRIVATE_TARGETS",
        )?;

        let tinyfish_search_url = endpoint(
            lookup("TINYFISH_SEARCH_URL"),
            crate::providers::tinyfish::DEFAULT_SEARCH_URL,
        );
        let tinyfish_fetch_url = endpoint(
            lookup("TINYFISH_FETCH_URL"),
            crate::providers::tinyfish::DEFAULT_FETCH_URL,
        );
        let parallel_base_url = endpoint(
            lookup("PARALLEL_BASE_URL"),
            crate::providers::parallel::DEFAULT_BASE_URL,
        );
        let tinyfish = TinyFishConfig {
            api_key: secret(lookup("TINYFISH_API_KEY")),
            search_url: tinyfish_search_url,
            fetch_url: tinyfish_fetch_url,
            timeout,
            fetch_timeout,
            allow_private_targets,
        };
        let parallel = ParallelConfig {
            api_key: secret(lookup("PARALLEL_API_KEY")),
            base_url: parallel_base_url,
            timeout,
            research_timeout,
            allow_private_targets,
            ..ParallelConfig::default()
        };
        let firecrawl_base_url = firecrawl_base_url(lookup("FIRECRAWL_BASE_URL"), firecrawl_file);
        let firecrawl = FirecrawlConfig {
            api_key: secret(lookup("FIRECRAWL_API_KEY")).or_else(|| {
                firecrawl_file
                    .and_then(|section| section.api_key.as_deref())
                    .and_then(Secret::new)
            }),
            base_url: firecrawl_base_url,
            timeout: firecrawl_timeout,
            allow_private_targets,
        };

        let mut config = Self {
            mode,
            tinyfish,
            parallel,
            firecrawl,
            cache_ttl,
            operation_timeout,
            allow_private_targets,
            disabled: Vec::new(),
        };
        config.apply_overrides(overrides)?;
        Ok(config)
    }

    /// Applies per-provider management overrides on top of the resolved
    /// environment/file configuration.
    fn apply_overrides(
        &mut self,
        overrides: &HashMap<ProviderKind, ProviderSettingsOverride>,
    ) -> Result<(), WebError> {
        for (provider, overrides) in overrides {
            let timeout = match overrides.timeout_secs {
                Some(seconds) if !(1..=3_600).contains(&seconds) => {
                    return Err(WebError::invalid_input(format!(
                        "{} timeout_secs must be between 1 and 3600",
                        provider.name()
                    )));
                }
                Some(seconds) => Some(Duration::from_secs(seconds)),
                None => None,
            };
            if overrides.enabled == Some(false) {
                if !self.disabled.contains(provider) {
                    self.disabled.push(*provider);
                }
            } else if overrides.enabled == Some(true) {
                self.disabled.retain(|disabled| disabled != provider);
            }
            let key_wins = overrides.authoritative || overrides.api_key.is_some();
            match provider {
                ProviderKind::TinyFish => {
                    if key_wins {
                        self.tinyfish.api_key = overrides.api_key.clone();
                    }
                    // TinyFish endpoints are fixed; base_url is deliberately
                    // ignored here even if a client submitted one.
                    if let Some(timeout) = timeout {
                        self.tinyfish.timeout = timeout;
                    }
                }
                ProviderKind::Parallel => {
                    if key_wins {
                        self.parallel.api_key = overrides.api_key.clone();
                    }
                    if let Some(raw) = overrides.base_url.as_deref() {
                        let trimmed = raw.trim();
                        self.parallel.base_url = if trimmed.is_empty() {
                            crate::providers::parallel::DEFAULT_BASE_URL.to_string()
                        } else {
                            trimmed.to_string()
                        };
                    }
                    if let Some(timeout) = timeout {
                        self.parallel.timeout = timeout;
                    }
                }
                ProviderKind::Firecrawl => {
                    if key_wins {
                        self.firecrawl.api_key = overrides.api_key.clone();
                    }
                    if let Some(raw) = overrides.base_url.as_deref() {
                        let trimmed = raw.trim();
                        self.firecrawl.base_url =
                            (!trimmed.is_empty()).then(|| trimmed.to_string());
                    }
                    if let Some(timeout) = timeout {
                        self.firecrawl.timeout = timeout;
                    }
                }
            }
        }
        Ok(())
    }

    /// Whether a provider is switched on in the effective configuration.
    pub fn provider_enabled(&self, provider: ProviderKind) -> bool {
        !self.disabled.contains(&provider)
    }

    /// The mode's search/read order with disabled providers removed.
    pub fn effective_search_read_order(&self) -> Vec<ProviderKind> {
        self.search_read_order()
            .iter()
            .copied()
            .filter(|provider| self.provider_enabled(*provider))
            .collect()
    }

    /// The mode's research order with disabled providers removed.
    pub fn effective_research_order(&self) -> Vec<ProviderKind> {
        self.research_order()
            .iter()
            .copied()
            .filter(|provider| self.provider_enabled(*provider))
            .collect()
    }

    /// The default search/read order for the selected mode.
    pub fn search_read_order(&self) -> &'static [ProviderKind] {
        self.mode.search_read_order()
    }

    /// The default research order for the selected mode.
    pub fn research_order(&self) -> &'static [ProviderKind] {
        self.mode.research_order()
    }

    /// `Ok(())` when the provider can be constructed; otherwise the reason it
    /// is skipped.
    pub fn provider_available(&self, provider: ProviderKind) -> Result<(), String> {
        match provider {
            ProviderKind::TinyFish => {
                if self.tinyfish.api_key.is_some() {
                    Ok(())
                } else {
                    Err("TinyFish is not configured (set TINYFISH_API_KEY)".to_string())
                }
            }
            ProviderKind::Parallel => {
                if self.parallel.api_key.is_some() {
                    Ok(())
                } else {
                    Err("Parallel is not configured (set PARALLEL_API_KEY)".to_string())
                }
            }
            ProviderKind::Firecrawl => self.firecrawl.unavailable_reason().map_or(Ok(()), Err),
        }
    }

    /// The local Firecrawl default, used by documentation and tests.
    pub fn local_firecrawl_base_url() -> &'static str {
        DEFAULT_LOCAL_BASE_URL
    }
}

fn firecrawl_base_url(
    env_base: Option<String>,
    file: Option<&FirecrawlFileConfig>,
) -> Option<String> {
    match env_base {
        Some(raw) => {
            let raw = raw.trim();
            if raw.is_empty() {
                Some(DEFAULT_LOCAL_BASE_URL.to_string())
            } else {
                Some(raw.to_string())
            }
        }
        None => file
            .and_then(|section| section.base_url.as_deref())
            .map(str::trim)
            .filter(|raw| !raw.is_empty())
            .map(str::to_string),
    }
}

fn endpoint(value: Option<String>, default: &str) -> String {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn secret(value: Option<String>) -> Option<Secret> {
    value.and_then(Secret::new)
}

fn parse_bool(raw: Option<&str>, name: &str) -> Result<bool, WebError> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(false);
    };
    match raw.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(WebError::invalid_input(format!(
            "{name} must be a boolean (1/0, true/false); got {raw:?}"
        ))),
    }
}

fn parse_secs(
    raw: Option<&str>,
    default: u64,
    name: &str,
    allow_zero: bool,
    max: Option<u64>,
) -> Result<u64, WebError> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(default);
    };
    let seconds = raw.parse::<u64>().map_err(|_| {
        WebError::invalid_input(format!(
            "{name} must be an integer number of seconds; got {raw:?}"
        ))
    })?;
    if seconds == 0 && !allow_zero {
        return Err(WebError::invalid_input(format!(
            "{name} must be greater than zero"
        )));
    }
    Ok(match max {
        Some(max) => seconds.min(max),
        None => seconds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WebRouter;
    use std::collections::HashMap;

    fn config(values: &[(&str, &str)]) -> WebConfig {
        let values = values
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect::<HashMap<_, _>>();
        WebConfig::from_lookup(move |name| values.get(name).cloned()).expect("valid config")
    }

    #[test]
    fn default_order_is_tinyfish_parallel_firecrawl() {
        assert_eq!(
            ProviderMode::Auto.search_read_order(),
            &[
                ProviderKind::TinyFish,
                ProviderKind::Parallel,
                ProviderKind::Firecrawl
            ]
        );
        assert_eq!(
            ProviderMode::Auto.research_order(),
            &[ProviderKind::Parallel]
        );
        assert!(ProviderMode::TinyFish.research_order().is_empty());
    }

    #[test]
    fn parses_mode_and_keys_without_leaking_them() {
        let config = config(&[
            ("DHI_WEB_PROVIDER", "parallel"),
            ("TINYFISH_API_KEY", "tf-secret"),
            ("PARALLEL_API_KEY", "par-secret"),
            ("FIRECRAWL_API_KEY", "fc-secret"),
            ("DHI_WEB_CACHE_TTL_SECS", "0"),
            ("DHI_WEB_RESEARCH_TIMEOUT_SECS", "1200"),
        ]);
        assert_eq!(config.mode, ProviderMode::Parallel);
        assert_eq!(config.cache_ttl, Duration::ZERO);
        assert_eq!(config.parallel.research_timeout, Duration::from_secs(1200));
        assert!(config.tinyfish.api_key.is_some());
        assert!(config.parallel.api_key.is_some());
        assert!(config.firecrawl.api_key.is_some());
        let debug = format!("{config:?}");
        for secret in ["tf-secret", "par-secret", "fc-secret"] {
            assert!(!debug.contains(secret), "config debug leaked a key");
        }
    }

    #[test]
    fn rejects_unknown_modes_and_bad_timeouts() {
        assert!(
            WebConfig::from_lookup(|name| (name == "DHI_WEB_PROVIDER").then_some("nope".into()))
                .is_err()
        );
        assert!(
            WebConfig::from_lookup(|name| {
                (name == "FIRECRAWL_TIMEOUT_SECS").then_some("soon".into())
            })
            .is_err()
        );
        assert!(
            WebConfig::from_lookup(|name| {
                (name == "DHI_WEB_ALLOW_PRIVATE_TARGETS").then_some("maybe".into())
            })
            .is_err()
        );
    }

    #[test]
    fn firecrawl_without_a_key_is_only_available_locally() {
        let local = config(&[("FIRECRAWL_BASE_URL", "http://127.0.0.1:3002")]);
        assert!(local.provider_available(ProviderKind::Firecrawl).is_ok());

        let cloud = config(&[("FIRECRAWL_BASE_URL", "https://api.firecrawl.dev")]);
        let reason = cloud
            .provider_available(ProviderKind::Firecrawl)
            .expect_err("cloud needs a key");
        assert!(reason.contains("FIRECRAWL_API_KEY"));

        let key_only = config(&[("FIRECRAWL_API_KEY", "fc-secret")]);
        assert!(key_only.provider_available(ProviderKind::Firecrawl).is_ok());
    }

    #[test]
    fn missing_keys_are_skipped_with_reasons() {
        let config = config(&[]);
        assert!(config.provider_available(ProviderKind::TinyFish).is_err());
        assert!(config.provider_available(ProviderKind::Parallel).is_err());
        assert!(config.provider_available(ProviderKind::Firecrawl).is_err());
        assert!(ProviderMode::Firecrawl.research_order().is_empty());
    }

    #[test]
    fn config_file_merges_with_env_precedence() {
        let file = FirecrawlFileConfig {
            base_url: Some("http://127.0.0.1:3002".to_string()),
            api_key: Some("file-secret".to_string()),
            timeout_secs: Some(11),
        };
        let env = HashMap::from([
            ("FIRECRAWL_API_KEY".to_string(), "env-secret".to_string()),
            ("FIRECRAWL_TIMEOUT_SECS".to_string(), "22".to_string()),
        ]);
        let config = WebConfig::from_lookup_and_file(|name| env.get(name).cloned(), Some(&file))
            .expect("config");
        assert_eq!(
            config.firecrawl.base_url.as_deref(),
            Some("http://127.0.0.1:3002")
        );
        assert_eq!(
            config.firecrawl.api_key.as_ref().map(Secret::expose),
            Some("env-secret")
        );
        assert_eq!(config.firecrawl.timeout, Duration::from_secs(22));

        let config = WebConfig::from_lookup_and_file(|_| None, Some(&file)).expect("config");
        assert_eq!(
            config.firecrawl.api_key.as_ref().map(Secret::expose),
            Some("file-secret")
        );
        assert_eq!(config.firecrawl.timeout, Duration::from_secs(11));
    }

    #[test]
    fn optional_base_url_envs_and_private_opt_in_parse() {
        let config = config(&[
            ("TINYFISH_SEARCH_URL", "http://search.internal:9000/search"),
            ("TINYFISH_FETCH_URL", "http://fetch.internal:9000/fetch"),
            ("PARALLEL_BASE_URL", "http://parallel.internal:9000"),
            ("DHI_WEB_ALLOW_PRIVATE_TARGETS", "true"),
            ("TINYFISH_FETCH_TIMEOUT_SECS", "5"),
            ("DHI_WEB_OPERATION_TIMEOUT_SECS", "240"),
            ("DHI_WEB_CACHE_TTL_SECS", "999999999"),
        ]);
        assert_eq!(
            config.tinyfish.search_url,
            "http://search.internal:9000/search"
        );
        assert_eq!(
            config.tinyfish.fetch_url,
            "http://fetch.internal:9000/fetch"
        );
        assert_eq!(config.parallel.base_url, "http://parallel.internal:9000");
        assert!(config.allow_private_targets);
        assert_eq!(config.tinyfish.fetch_timeout, Duration::from_secs(150));
        assert_eq!(config.operation_timeout, Duration::from_secs(240));
        assert_eq!(
            config.cache_ttl,
            Duration::from_secs(crate::cache::MAX_CACHE_TTL_SECS)
        );
        assert_eq!(
            WebConfig::default().parallel.research_timeout,
            Duration::from_secs(DEFAULT_RESEARCH_TIMEOUT_SECS)
        );
    }

    #[test]
    fn config_debug_redacts_url_credentials_and_query_values() {
        let config = config(&[
            (
                "TINYFISH_SEARCH_URL",
                "https://user:pass@search.example/path?token=topsecret",
            ),
            (
                "TINYFISH_FETCH_URL",
                "https://user:pass@fetch.example/path?token=topsecret",
            ),
            ("PARALLEL_BASE_URL", "https://user:pass@parallel.example"),
            (
                "FIRECRAWL_BASE_URL",
                "https://user:pass@firecrawl.example/?token=topsecret",
            ),
        ]);
        let debug = format!("{config:?}");
        for secret in ["user", "pass", "topsecret"] {
            assert!(!debug.contains(secret), "debug leaked {secret}: {debug}");
        }
    }

    #[test]
    fn config_file_firecrawl_section_is_read_from_disk() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"firecrawl":{"baseUrl":"http://127.0.0.1:3002","apiKey":"file-secret","timeoutSecs":9}}"#,
        )
        .expect("write config");
        let file = load_config_file_from(&path).expect("config file");
        let section = file.firecrawl.expect("firecrawl section");
        assert_eq!(section.base_url.as_deref(), Some("http://127.0.0.1:3002"));
        assert_eq!(section.api_key.as_deref(), Some("file-secret"));
        assert_eq!(section.timeout_secs, Some(9));
        let debug = format!("{section:?}");
        assert!(!debug.contains("file-secret"), "{debug}");
    }

    #[test]
    fn database_key_overrides_environment_and_redacts_debug() {
        let env = HashMap::from([("TINYFISH_API_KEY".to_string(), "env-secret".to_string())]);
        let overrides = HashMap::from([(
            ProviderKind::TinyFish,
            ProviderSettingsOverride {
                authoritative: true,
                api_key: Some(Secret::new("db-secret").expect("non-empty")),
                ..ProviderSettingsOverride::default()
            },
        )]);
        let config =
            WebConfig::from_lookup_with_overrides(|name| env.get(name).cloned(), &overrides)
                .expect("config");
        assert_eq!(
            config.tinyfish.api_key.as_ref().map(Secret::expose),
            Some("db-secret")
        );
        let debug = format!("{config:?}");
        for secret in ["db-secret", "env-secret"] {
            assert!(!debug.contains(secret), "config debug leaked a key");
        }
    }

    #[test]
    fn an_authoritative_row_without_a_key_does_not_fall_back_to_environment() {
        let env = HashMap::from([("TINYFISH_API_KEY".to_string(), "env-secret".to_string())]);
        let overrides = HashMap::from([(
            ProviderKind::TinyFish,
            ProviderSettingsOverride {
                authoritative: true,
                api_key: None,
                ..ProviderSettingsOverride::default()
            },
        )]);
        let config =
            WebConfig::from_lookup_with_overrides(|name| env.get(name).cloned(), &overrides)
                .expect("config");
        assert!(config.tinyfish.api_key.is_none());
        assert!(config.provider_available(ProviderKind::TinyFish).is_err());
    }

    #[test]
    fn disabled_provider_is_removed_from_both_orders() {
        let env = HashMap::from([
            ("TINYFISH_API_KEY".to_string(), "tf-secret".to_string()),
            ("PARALLEL_API_KEY".to_string(), "par-secret".to_string()),
            (
                "FIRECRAWL_BASE_URL".to_string(),
                "http://127.0.0.1:3002".to_string(),
            ),
        ]);
        let overrides = HashMap::from([(
            ProviderKind::Parallel,
            ProviderSettingsOverride {
                authoritative: true,
                enabled: Some(false),
                ..ProviderSettingsOverride::default()
            },
        )]);
        let config =
            WebConfig::from_lookup_with_overrides(|name| env.get(name).cloned(), &overrides)
                .expect("config");
        assert_eq!(
            config.effective_search_read_order(),
            vec![ProviderKind::TinyFish, ProviderKind::Firecrawl]
        );
        assert!(config.effective_research_order().is_empty());

        let router = WebRouter::from_config(&config).expect("router");
        assert_eq!(router.provider_order(), ["tinyfish", "firecrawl"]);
        assert!(router.research_provider_order().is_empty());
        let skipped = router
            .skipped_providers()
            .iter()
            .find(|skip| skip.provider == "parallel")
            .expect("parallel is reported as skipped");
        assert!(skipped.reason.contains("disabled"), "{skipped:?}");
    }

    #[test]
    fn firecrawl_base_url_and_timeout_overrides_win() {
        let env = HashMap::from([
            ("PARALLEL_API_KEY".to_string(), "par-secret".to_string()),
            (
                "FIRECRAWL_BASE_URL".to_string(),
                "http://127.0.0.1:3002".to_string(),
            ),
            ("FIRECRAWL_API_KEY".to_string(), "env-firecrawl".to_string()),
        ]);
        let overrides = HashMap::from([(
            ProviderKind::Firecrawl,
            ProviderSettingsOverride {
                authoritative: true,
                api_key: Some(Secret::new("db-firecrawl").expect("non-empty")),
                base_url: Some("http://127.0.0.1:9999".to_string()),
                timeout_secs: Some(30),
                enabled: Some(true),
            },
        )]);
        let config =
            WebConfig::from_lookup_with_overrides(|name| env.get(name).cloned(), &overrides)
                .expect("config");
        assert_eq!(
            config.firecrawl.base_url.as_deref(),
            Some("http://127.0.0.1:9999")
        );
        assert_eq!(config.firecrawl.timeout, Duration::from_secs(30));
        assert_eq!(
            config.firecrawl.api_key.as_ref().map(Secret::expose),
            Some("db-firecrawl")
        );
        assert_eq!(
            config.effective_research_order(),
            vec![ProviderKind::Parallel],
            "research stays Parallel-only"
        );
    }

    #[test]
    fn clearing_a_firecrawl_base_url_restores_the_built_in_default() {
        let env = HashMap::from([(
            "FIRECRAWL_BASE_URL".to_string(),
            "http://127.0.0.1:3002".to_string(),
        )]);
        let overrides = HashMap::from([(
            ProviderKind::Firecrawl,
            ProviderSettingsOverride {
                authoritative: true,
                base_url: Some(String::new()),
                ..ProviderSettingsOverride::default()
            },
        )]);
        let config =
            WebConfig::from_lookup_with_overrides(|name| env.get(name).cloned(), &overrides)
                .expect("config");
        assert!(config.firecrawl.base_url.is_none());
    }

    #[test]
    fn invalid_override_timeouts_are_rejected() {
        let overrides = HashMap::from([(
            ProviderKind::Parallel,
            ProviderSettingsOverride {
                timeout_secs: Some(0),
                ..ProviderSettingsOverride::default()
            },
        )]);
        assert!(WebConfig::from_lookup_with_overrides(|_| None, &overrides).is_err());
    }

    #[test]
    #[serial_test::serial]
    fn dhi_config_env_selects_the_file_path() {
        let previous = std::env::var_os("DHI_CONFIG");
        // SAFETY: this test is serialized and no other test reads DHI_CONFIG.
        unsafe {
            std::env::set_var("DHI_CONFIG", "/tmp/dhi-web-config-test.json");
        }
        assert_eq!(
            config_path(),
            Some(PathBuf::from("/tmp/dhi-web-config-test.json"))
        );
        unsafe {
            match previous {
                Some(value) => std::env::set_var("DHI_CONFIG", value),
                None => std::env::remove_var("DHI_CONFIG"),
            }
        }
    }
}
