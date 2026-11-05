//! LLM profiles: which provider, route, model, and budget each role uses.
//!
//! copy ARTEX: providers, base URL as an API base, output cap, three retry
//! layers (connect, empty, stream), no per-request timeout, per-role
//! `max_turns` and `run_seconds`.
//!
//! DHI adaptation, reason: DHI drives the engine loop in-process, so a hung
//! HTTP call hangs the task. The client keeps ARTEX's "no per-request timeout"
//! and the runtime instead wraps one agent run in `run_seconds`; the run is the
//! unit DHI can checkpoint and resume.
//!
//! DHI adaptation, reason: River is an OpenAI-compatible gateway, not OpenAI.
//! The `max_completion_tokens` spelling is opt-in (`DHI_LLM_MAX_TOKENS_FIELD`),
//! never auto-detected.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    OpenAi,
    Anthropic,
    OpenAiResponses,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "open_ai",
            Self::Anthropic => "anthropic",
            Self::OpenAiResponses => "open_ai_responses",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "open_ai" | "openai" => Some(Self::OpenAi),
            "anthropic" | "claude" => Some(Self::Anthropic),
            "open_ai_responses" | "openai_responses" | "responses" => Some(Self::OpenAiResponses),
            _ => None,
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Planner,
    Worker,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planner => "planner",
            Self::Worker => "worker",
        }
    }

    fn env_suffix(self) -> &'static str {
        match self {
            Self::Planner => "PLANNER",
            Self::Worker => "WORKER",
        }
    }

    /// The template file this role loads: `ops/prompts/<role>.md`.
    pub fn prompt_file(self) -> String {
        format!("{}.md", self.as_str())
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Name of the JSON field the output cap is sent as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
    /// Default: every OpenAI-compatible gateway, including River.
    MaxTokens,
    MaxCompletionTokens,
}

impl MaxTokensField {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MaxTokens => "max_tokens",
            Self::MaxCompletionTokens => "max_completion_tokens",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "max_tokens" => Some(Self::MaxTokens),
            "max_completion_tokens" => Some(Self::MaxCompletionTokens),
            _ => None,
        }
    }
}

impl fmt::Display for MaxTokensField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct LlmProfile {
    pub provider: Provider,
    /// API base, e.g. `https://api.river.ai/v1`. Not an endpoint; Rig appends the route.
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_tokens: u64,
    /// Name of the JSON field the output cap is sent as: `max_tokens` (default,
    /// every OpenAI-compatible gateway) or `max_completion_tokens`.
    pub max_tokens_field: MaxTokensField,
    /// Default true. A true profile must be able to stream; see `provider_gate`.
    pub stream: bool,
    /// `''` (send nothing) | `disabled` | `enabled`.
    pub thinking_type: String,
    /// `''` (send nothing) | `low` | `medium` | `high` | `xhigh` | `max`.
    pub reasoning_effort: String,
    /// Connect retry inside the transport (fixed backoff, capped attempts).
    pub connect_retries: u32,
    /// Empty-response retry: the run produced no accepted content.
    pub empty_retries: u32,
    /// Stream-failure retry: the stream died before a terminal response.
    pub stream_retries: u32,
    pub retry_backoff_ms: u64,
    /// Per-role conversation budget (plan: 6 model calls, worker: 24).
    pub max_turns: usize,
    /// Per-role wall-clock budget for one agent run.
    pub run_seconds: u64,
    /// Must contain every `{{variable}}` the role's template declares.
    pub variables: BTreeMap<Role, String>,
}

/// The API key is a secret: it is never logged and never written to
/// `agent_runs`.
impl fmt::Debug for LlmProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LlmProfile")
            .field("provider", &self.provider)
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("model", &self.model)
            .field("max_tokens", &self.max_tokens)
            .field("max_tokens_field", &self.max_tokens_field)
            .field("stream", &self.stream)
            .field("thinking_type", &self.thinking_type)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("connect_retries", &self.connect_retries)
            .field("empty_retries", &self.empty_retries)
            .field("stream_retries", &self.stream_retries)
            .field("retry_backoff_ms", &self.retry_backoff_ms)
            .field("max_turns", &self.max_turns)
            .field("run_seconds", &self.run_seconds)
            .field("variables", &self.variables)
            .finish()
    }
}

impl LlmProfile {
    /// Contract defaults (DHI adaptation): planner 4096 tokens / 6 turns /
    /// 120 s, worker 8192 tokens / 24 turns / 600 s, 2 retries per layer at
    /// 3000 ms, streaming on.
    pub fn defaults(role: Role) -> Self {
        let (max_tokens, max_turns, run_seconds) = match role {
            Role::Planner => (4096, 6, 120),
            Role::Worker => (8192, 24, 600),
        };
        Self {
            provider: Provider::OpenAi,
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            max_tokens,
            max_tokens_field: MaxTokensField::MaxTokens,
            stream: true,
            thinking_type: String::new(),
            reasoning_effort: String::new(),
            connect_retries: 2,
            empty_retries: 2,
            stream_retries: 2,
            retry_backoff_ms: 3000,
            max_turns,
            run_seconds,
            variables: BTreeMap::new(),
        }
    }

    /// Reads both role profiles from the environment, starting from
    /// `defaults`. Shared keys apply to both roles; per-role keys override
    /// them. A missing base URL, key, or model is a startup error.
    pub fn from_env() -> Result<RoleProfiles, ProfileError> {
        let base_url = read_first(&["DHI_LLM_BASE_URL", "RIVER_BASE_URL"])
            .ok_or(ProfileError::Missing("DHI_LLM_BASE_URL or RIVER_BASE_URL"))?;
        let api_key = read_first(&["DHI_LLM_API_KEY", "RIVER_API_KEY"])
            .ok_or(ProfileError::Missing("DHI_LLM_API_KEY or RIVER_API_KEY"))?;
        let shared_model = read_var("DHI_LLM_MODEL");

        let shared_max_tokens = read_u64("DHI_LLM_MAX_TOKENS")?;
        let shared_field = read_max_tokens_field("DHI_LLM_MAX_TOKENS_FIELD")?;
        let shared_stream = read_bool("DHI_LLM_STREAM")?;
        let shared_thinking_type = read_thinking_type("DHI_LLM_THINKING_TYPE")?;
        let shared_reasoning_effort = read_reasoning_effort("DHI_LLM_REASONING_EFFORT")?;
        let shared_connect = read_u32("DHI_LLM_RETRY_CONNECT")?;
        let shared_empty = read_u32("DHI_LLM_RETRY_EMPTY")?;
        let shared_stream_retries = read_u32("DHI_LLM_RETRY_STREAM")?;
        let shared_backoff = read_u64("DHI_LLM_RETRY_BACKOFF_MS")?;

        let mut profiles = RoleProfiles::default();
        for (role, profile) in [
            (Role::Planner, &mut profiles.planner),
            (Role::Worker, &mut profiles.worker),
        ] {
            let suffix = role.env_suffix();
            profile.base_url = base_url.clone();
            profile.api_key = api_key.clone();
            let (role_model, role_missing) = match role {
                Role::Planner => (
                    read_first(&["DHI_LLM_PLANNER_MODEL", "RIVER_MODEL_PLANNER"]),
                    "DHI_LLM_PLANNER_MODEL or RIVER_MODEL_PLANNER",
                ),
                Role::Worker => (
                    read_first(&["DHI_LLM_WORKER_MODEL", "RIVER_MODEL_WORKER"]),
                    "DHI_LLM_WORKER_MODEL or RIVER_MODEL_WORKER",
                ),
            };
            profile.model = role_model
                .or_else(|| shared_model.clone())
                .ok_or(ProfileError::Missing(role_missing))?;
            if let Some(provider) = read_provider()? {
                profile.provider = provider;
            }
            profile.max_tokens = shared_max_tokens.unwrap_or(profile.max_tokens);
            profile.max_tokens_field = shared_field.unwrap_or(profile.max_tokens_field);
            profile.stream = shared_stream.unwrap_or(profile.stream);
            profile.thinking_type = read_thinking_type(&format!("DHI_LLM_THINKING_TYPE_{suffix}"))?
                .or_else(|| shared_thinking_type.clone())
                .unwrap_or_default();
            profile.reasoning_effort =
                read_reasoning_effort(&format!("DHI_LLM_REASONING_EFFORT_{suffix}"))?
                    .or_else(|| shared_reasoning_effort.clone())
                    .unwrap_or_default();
            profile.connect_retries = shared_connect.unwrap_or(profile.connect_retries);
            profile.empty_retries = shared_empty.unwrap_or(profile.empty_retries);
            profile.stream_retries = shared_stream_retries.unwrap_or(profile.stream_retries);
            profile.retry_backoff_ms = shared_backoff.unwrap_or(profile.retry_backoff_ms);
            profile.max_turns =
                read_usize(&format!("DHI_LLM_MAX_TURNS_{suffix}"))?.unwrap_or(profile.max_turns);
            profile.run_seconds =
                read_u64(&format!("DHI_LLM_RUN_SECONDS_{suffix}"))?.unwrap_or(profile.run_seconds);
        }

        Ok(profiles)
    }

    /// Startup gate: a non-streaming profile for a provider that only has a
    /// streaming integration must fail loudly, not silently buffer.
    pub fn provider_gate(&self) -> Result<(), ProfileError> {
        if !self.stream {
            return Err(ProfileError::NonStreaming {
                provider: self.provider,
            });
        }
        Ok(())
    }

    /// A one-line fingerprint `<provider>/<model>/max_tokens=<n>/stream=<bool>`.
    /// Contains no secret and is recorded as `agent_runs.profile`.
    pub fn fingerprint(&self) -> String {
        format!(
            "{}/{}/max_tokens={}/stream={}",
            self.provider, self.model, self.max_tokens, self.stream
        )
    }

    pub fn identity(&self) -> ProfileIdentity<'_> {
        ProfileIdentity {
            provider: self.provider.as_str(),
            model: &self.model,
            max_tokens: self.max_tokens,
            stream: self.stream,
        }
    }

    pub fn is_complete(&self) -> bool {
        !self.base_url.is_empty() && !self.api_key.is_empty() && !self.model.is_empty()
    }
}

/// Both roles of a runtime's profiles.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleProfiles {
    pub planner: LlmProfile,
    pub worker: LlmProfile,
}

impl Default for RoleProfiles {
    fn default() -> Self {
        Self {
            planner: LlmProfile::defaults(Role::Planner),
            worker: LlmProfile::defaults(Role::Worker),
        }
    }
}

impl RoleProfiles {
    pub fn get(&self, role: Role) -> &LlmProfile {
        match role {
            Role::Planner => &self.planner,
            Role::Worker => &self.worker,
        }
    }

    /// Gated for both roles.
    pub fn provider_gate(&self) -> Result<(), ProfileError> {
        self.planner.provider_gate()?;
        self.worker.provider_gate()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfileIdentity<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    pub max_tokens: u64,
    pub stream: bool,
}

#[derive(Debug, Error)]
pub enum ProfileError {
    #[error("missing environment variable: {0}")]
    Missing(&'static str),
    #[error("invalid {var}: {value}")]
    Invalid { var: String, value: String },
    #[error("profile for provider {provider} is not streaming; only streaming is supported")]
    NonStreaming { provider: Provider },
}

fn read_var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn read_first(keys: &[&'static str]) -> Option<String> {
    keys.iter().find_map(|key| read_var(key))
}

fn read_provider() -> Result<Option<Provider>, ProfileError> {
    match read_var("DHI_LLM_PROVIDER") {
        None => Ok(None),
        Some(raw) => Provider::parse(&raw)
            .map(Some)
            .ok_or(ProfileError::Invalid {
                var: "DHI_LLM_PROVIDER".to_string(),
                value: raw,
            }),
    }
}

fn read_thinking_type(key: &str) -> Result<Option<String>, ProfileError> {
    match read_var(key) {
        None => Ok(None),
        Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "disabled" | "enabled" => Ok(Some(raw.trim().to_ascii_lowercase())),
            _ => Err(ProfileError::Invalid {
                var: key.to_string(),
                value: raw,
            }),
        },
    }
}

fn read_reasoning_effort(key: &str) -> Result<Option<String>, ProfileError> {
    match read_var(key) {
        None => Ok(None),
        Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "low" | "medium" | "high" | "xhigh" | "max" => {
                Ok(Some(raw.trim().to_ascii_lowercase()))
            }
            _ => Err(ProfileError::Invalid {
                var: key.to_string(),
                value: raw,
            }),
        },
    }
}

fn read_max_tokens_field(key: &str) -> Result<Option<MaxTokensField>, ProfileError> {
    match read_var(key) {
        None => Ok(None),
        Some(raw) => MaxTokensField::parse(&raw)
            .map(Some)
            .ok_or(ProfileError::Invalid {
                var: key.to_string(),
                value: raw,
            }),
    }
}

fn read_bool(key: &str) -> Result<Option<bool>, ProfileError> {
    match read_var(key) {
        None => Ok(None),
        Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(Some(true)),
            "0" | "false" | "no" | "off" => Ok(Some(false)),
            _ => Err(ProfileError::Invalid {
                var: key.to_string(),
                value: raw,
            }),
        },
    }
}

macro_rules! read_number {
    ($name:ident, $ty:ty) => {
        fn $name(key: &str) -> Result<Option<$ty>, ProfileError> {
            match read_var(key) {
                None => Ok(None),
                Some(raw) => {
                    raw.trim()
                        .parse::<$ty>()
                        .map(Some)
                        .map_err(|_| ProfileError::Invalid {
                            var: key.to_string(),
                            value: raw,
                        })
                }
            }
        }
    };
}

read_number!(read_u64, u64);
read_number!(read_u32, u32);
read_number!(read_usize, usize);

/// Locates `ops/prompts` (or the given directory) without relying on the
/// process working directory: `DHI_PROMPTS_DIR`, else `ops/prompts` from
/// `start`, then from each ancestor.
pub fn find_prompts_dir(start: &Path) -> Option<std::path::PathBuf> {
    if let Some(dir) = read_var("DHI_PROMPTS_DIR") {
        let path = std::path::PathBuf::from(dir);
        if path.is_dir() {
            return Some(path);
        }
    }
    let mut current = Some(start);
    while let Some(dir) = current {
        let candidate = dir.join("ops").join("prompts");
        if candidate.is_dir() {
            return Some(candidate);
        }
        current = dir.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    const KEYS: &[&str] = &[
        "DHI_LLM_PROVIDER",
        "DHI_LLM_BASE_URL",
        "RIVER_BASE_URL",
        "DHI_LLM_API_KEY",
        "RIVER_API_KEY",
        "DHI_LLM_MODEL",
        "DHI_LLM_PLANNER_MODEL",
        "DHI_LLM_WORKER_MODEL",
        "RIVER_MODEL_PLANNER",
        "RIVER_MODEL_WORKER",
        "DHI_LLM_MAX_TOKENS",
        "DHI_LLM_MAX_TOKENS_FIELD",
        "DHI_LLM_STREAM",
        "DHI_LLM_THINKING_TYPE",
        "DHI_LLM_THINKING_TYPE_PLANNER",
        "DHI_LLM_THINKING_TYPE_WORKER",
        "DHI_LLM_REASONING_EFFORT",
        "DHI_LLM_REASONING_EFFORT_PLANNER",
        "DHI_LLM_REASONING_EFFORT_WORKER",
        "DHI_LLM_MAX_TURNS_PLANNER",
        "DHI_LLM_MAX_TURNS_WORKER",
        "DHI_LLM_RUN_SECONDS_PLANNER",
        "DHI_LLM_RUN_SECONDS_WORKER",
        "DHI_LLM_RETRY_CONNECT",
        "DHI_LLM_RETRY_EMPTY",
        "DHI_LLM_RETRY_STREAM",
        "DHI_LLM_RETRY_BACKOFF_MS",
    ];

    /// Sets exactly the given variables and clears the rest, so parallel
    /// tests in other modules never see a stray profile value.
    fn with_env(vars: &[(&'static str, &str)], f: impl FnOnce()) {
        let saved: Vec<(&str, Option<String>)> = KEYS
            .iter()
            .map(|key| (*key, std::env::var(key).ok()))
            .collect();
        for key in KEYS {
            // Edition 2024: `remove_var` is unsafe because env is global.
            unsafe { std::env::remove_var(key) };
        }
        for (key, value) in vars {
            unsafe { std::env::set_var(key, value) };
        }
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        for (key, value) in saved {
            match value {
                Some(value) => unsafe { std::env::set_var(key, value) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    #[test]
    fn defaults_match_the_contract_table() {
        let planner = LlmProfile::defaults(Role::Planner);
        let worker = LlmProfile::defaults(Role::Worker);

        assert_eq!(planner.provider, Provider::OpenAi);
        assert_eq!(planner.max_tokens, 4096);
        assert_eq!(planner.max_tokens_field, MaxTokensField::MaxTokens);
        assert!(planner.stream);
        assert_eq!(planner.connect_retries, 2);
        assert_eq!(planner.empty_retries, 2);
        assert_eq!(planner.stream_retries, 2);
        assert_eq!(planner.retry_backoff_ms, 3000);
        assert_eq!(planner.max_turns, 6);
        assert_eq!(planner.run_seconds, 120);
        assert_eq!(planner.thinking_type, "");
        assert_eq!(planner.reasoning_effort, "");

        assert_eq!(worker.provider, Provider::OpenAi);
        assert_eq!(worker.max_tokens, 8192);
        assert_eq!(worker.max_turns, 24);
        assert_eq!(worker.run_seconds, 600);
        assert_eq!(worker.retry_backoff_ms, 3000);
    }

    #[test]
    fn role_profiles_default_to_one_of_each() {
        let profiles = RoleProfiles::default();
        assert_eq!(profiles.planner.max_turns, 6);
        assert_eq!(profiles.worker.max_turns, 24);
    }

    #[test]
    #[serial]
    fn from_env_requires_base_url_key_and_model() {
        with_env(&[], || {
            assert!(matches!(
                LlmProfile::from_env(),
                Err(ProfileError::Missing(name)) if name.contains("BASE_URL")
            ));
        });
        with_env(&[("DHI_LLM_BASE_URL", "https://api.river.ai/v1")], || {
            assert!(matches!(
                LlmProfile::from_env(),
                Err(ProfileError::Missing(name)) if name.contains("API_KEY")
            ));
        });
        with_env(
            &[
                ("DHI_LLM_BASE_URL", "https://api.river.ai/v1"),
                ("DHI_LLM_API_KEY", "k"),
            ],
            || {
                assert!(matches!(
                    LlmProfile::from_env(),
                    Err(ProfileError::Missing(name)) if name.contains("MODEL")
                ));
            },
        );
        with_env(
            &[
                ("DHI_LLM_BASE_URL", "https://api.river.ai/v1"),
                ("DHI_LLM_API_KEY", "k"),
                ("DHI_LLM_PLANNER_MODEL", "p"),
            ],
            || {
                assert!(matches!(
                    LlmProfile::from_env(),
                    Err(ProfileError::Missing(name))
                        if name.contains("WORKER_MODEL") || name.contains("RIVER_MODEL_WORKER")
                ));
            },
        );
    }

    #[test]
    #[serial]
    fn from_env_reads_shared_and_per_role_keys() {
        with_env(
            &[
                ("DHI_LLM_BASE_URL", "https://api.river.ai/v1"),
                ("DHI_LLM_API_KEY", "secret-key"),
                ("DHI_LLM_PLANNER_MODEL", "planner-model"),
                ("DHI_LLM_WORKER_MODEL", "worker-model"),
                ("DHI_LLM_MAX_TOKENS", "1024"),
                ("DHI_LLM_MAX_TOKENS_FIELD", "max_completion_tokens"),
                ("DHI_LLM_STREAM", "false"),
                ("DHI_LLM_THINKING_TYPE", "enabled"),
                ("DHI_LLM_REASONING_EFFORT", "high"),
                ("DHI_LLM_MAX_TURNS_PLANNER", "3"),
                ("DHI_LLM_RUN_SECONDS_WORKER", "45"),
                ("DHI_LLM_RETRY_CONNECT", "5"),
            ],
            || {
                let profiles = LlmProfile::from_env().unwrap();
                assert_eq!(profiles.planner.base_url, "https://api.river.ai/v1");
                assert_eq!(profiles.planner.api_key, "secret-key");
                assert_eq!(profiles.planner.model, "planner-model");
                assert_eq!(profiles.worker.model, "worker-model");
                assert_eq!(profiles.planner.max_tokens, 1024);
                assert_eq!(profiles.worker.max_tokens, 1024);
                assert_eq!(
                    profiles.worker.max_tokens_field,
                    MaxTokensField::MaxCompletionTokens
                );
                assert!(!profiles.worker.stream);
                assert_eq!(profiles.planner.thinking_type, "enabled");
                assert_eq!(profiles.worker.reasoning_effort, "high");
                assert_eq!(profiles.planner.max_turns, 3);
                // Untouched per-role default survives a shared key.
                assert_eq!(profiles.worker.max_turns, 24);
                assert_eq!(profiles.worker.run_seconds, 45);
                assert_eq!(profiles.planner.run_seconds, 120);
                assert_eq!(profiles.planner.connect_retries, 5);
                // Secrets never reach Debug output.
                assert!(!format!("{profiles:?}").contains("secret-key"));
            },
        );
    }

    #[test]
    #[serial]
    fn from_env_falls_back_to_river_names() {
        with_env(
            &[
                ("RIVER_BASE_URL", "https://river.example/v1"),
                ("RIVER_API_KEY", "river-key"),
                ("RIVER_MODEL_PLANNER", "p"),
                ("RIVER_MODEL_WORKER", "w"),
            ],
            || {
                let profiles = LlmProfile::from_env().unwrap();
                assert_eq!(profiles.planner.base_url, "https://river.example/v1");
                assert_eq!(profiles.worker.api_key, "river-key");
                assert_eq!(profiles.planner.model, "p");
                assert_eq!(profiles.worker.model, "w");
            },
        );
    }

    #[test]
    #[serial]
    fn from_env_rejects_bad_values() {
        with_env(
            &[
                ("DHI_LLM_BASE_URL", "u"),
                ("DHI_LLM_API_KEY", "k"),
                ("DHI_LLM_PLANNER_MODEL", "p"),
                ("DHI_LLM_WORKER_MODEL", "w"),
                ("DHI_LLM_MAX_TOKENS", "lots"),
            ],
            || {
                assert!(matches!(
                    LlmProfile::from_env(),
                    Err(ProfileError::Invalid { .. })
                ));
            },
        );
    }

    #[test]
    #[serial]
    fn from_env_rejects_bad_thinking_values() {
        for (key, value) in [
            ("DHI_LLM_THINKING_TYPE", "on"),
            ("DHI_LLM_REASONING_EFFORT", "minimal"),
        ] {
            with_env(
                &[
                    ("DHI_LLM_BASE_URL", "u"),
                    ("DHI_LLM_API_KEY", "k"),
                    ("DHI_LLM_PLANNER_MODEL", "p"),
                    ("DHI_LLM_WORKER_MODEL", "w"),
                    (key, value),
                ],
                || {
                    assert!(matches!(
                        LlmProfile::from_env(),
                        Err(ProfileError::Invalid { var, .. }) if var.as_str() == key
                    ));
                },
            );
        }
    }

    #[test]
    fn provider_gate_rejects_non_streaming() {
        let mut profile = LlmProfile::defaults(Role::Planner);
        assert!(profile.provider_gate().is_ok());
        profile.stream = false;
        assert!(matches!(
            profile.provider_gate(),
            Err(ProfileError::NonStreaming {
                provider: Provider::OpenAi
            })
        ));

        let mut profiles = RoleProfiles::default();
        profiles.worker.stream = false;
        assert!(profiles.provider_gate().is_err());
    }

    #[test]
    fn fingerprint_names_provider_model_cap_and_stream() {
        let mut profile = LlmProfile::defaults(Role::Worker);
        profile.model = "deepseek".to_string();
        profile.api_key = "secret-key".to_string();
        let fingerprint = profile.fingerprint();
        assert_eq!(fingerprint, "open_ai/deepseek/max_tokens=8192/stream=true");
        assert!(!fingerprint.contains("secret-key"));
    }

    #[test]
    fn provider_and_field_parse_roundtrip() {
        for provider in [
            Provider::OpenAi,
            Provider::Anthropic,
            Provider::OpenAiResponses,
        ] {
            assert_eq!(Provider::parse(provider.as_str()), Some(provider));
        }
        assert_eq!(
            MaxTokensField::parse("max_completion_tokens"),
            Some(MaxTokensField::MaxCompletionTokens)
        );
        assert_eq!(MaxTokensField::parse("bogus"), None);
        assert_eq!(
            serde_json::to_string(&Provider::OpenAi).unwrap(),
            "\"open_ai\""
        );
    }
}
