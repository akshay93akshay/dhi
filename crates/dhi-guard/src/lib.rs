//! Guard policy evaluation.
//!
//! The guard is the single choke point for tool calls. It returns allow,
//! deny, or ask. The first matching rule wins. If no rule matches, the
//! policy default applies. The default is deny.

pub mod executor;
pub mod snapshot;

pub use executor::{GuardedExecutor, ToolBackend, ToolExecutor};
pub use snapshot::{CompiledPolicy, CompiledRule, MatchTarget, Matcher, RuleSpec, SharedPolicy};

use dhi_core::GuardVerdict;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub name: String,
    pub tool: String,
    pub action: GuardVerdict,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Policy {
    pub version: String,
    pub default: GuardVerdict,
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub verdict: GuardVerdict,
    pub matched_rule: Option<String>,
    pub rule_version: String,
    pub reason: String,
    /// Ask-approval budget the matched rule prescribes in seconds; 0 = no
    /// rule override (the snapshot's settings default or the executor's
    /// construction default applies). Populated only by the snapshot
    /// evaluation; the legacy frozen-policy path leaves it at 0.
    #[serde(default)]
    pub approval_timeout_secs: i64,
}

impl Policy {
    pub fn evaluate(&self, tool_name: &str, _input: &serde_json::Value) -> Decision {
        for rule in &self.rules {
            if rule.tool == "*" || rule.tool == tool_name {
                return Decision {
                    verdict: rule.action,
                    matched_rule: Some(rule.name.clone()),
                    rule_version: self.version.clone(),
                    reason: rule.reason.clone(),
                    approval_timeout_secs: 0,
                };
            }
        }
        Decision {
            verdict: self.default,
            matched_rule: None,
            rule_version: self.version.clone(),
            reason: "no matching rule; default policy applied".to_string(),
            approval_timeout_secs: 0,
        }
    }

    pub fn chess() -> Self {
        let allow = |name: &str, why: &str| Rule {
            name: format!("allow-{name}"),
            tool: name.to_string(),
            action: GuardVerdict::Allow,
            reason: why.to_string(),
        };
        Self {
            version: "chess-v1".to_string(),
            default: GuardVerdict::Deny,
            rules: vec![
                allow("createGame", "start a new chess game"),
                allow("joinGame", "join an existing chess game"),
                allow("waitForNextTurn", "read the board after the opponent moves"),
                allow("finishTurn", "submit a legal move"),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chess_policy_allows_known_tools() {
        let policy = Policy::chess();
        let decision = policy.evaluate("finishTurn", &serde_json::json!({}));
        assert_eq!(decision.verdict, GuardVerdict::Allow);
    }

    #[test]
    fn chess_policy_denies_unknown_tools() {
        let policy = Policy::chess();
        let decision = policy.evaluate("shell", &serde_json::json!({}));
        assert_eq!(decision.verdict, GuardVerdict::Deny);
    }
}
