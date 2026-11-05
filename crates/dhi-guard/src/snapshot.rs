//! Snapshot-driven policy: the compiled form of the `guard_rules` +
//! `app_settings` rows, shared by the server (writer) and the engine's guard
//! executor (reader).
//!
//! Replaces "the executor owns a frozen compiled policy" with a swappable
//! handle: rule mutations and settings writes rebuild the snapshot and swap it
//! atomically; the next evaluation sees the new rules. An approval already
//! parked in-process is judged under the snapshot recorded at its call time —
//! matching ARTEX, which drops cached rule copies only going forward.
//!
//! Evaluation semantics (ARTEX parity, generalized from `Policy::chess`):
//! - enabled rules only, ordered by priority DESC, first match wins;
//! - `tool_name` + `exact` is string equality; `regex` is a compiled match
//!   against the tool name;
//! - `tool_input` matches the raw JSON string of the input: `exact` is a
//!   case-insensitive substring, `regex` a plain match;
//! - no match falls through to `guard_default_action` from `app_settings`;
//! - a matched rule's `timeout_secs` (when > 0) overrides the settings-wide
//!   approval timeout for the ask-approval this verdict opens.

use std::sync::{Arc, RwLock};

use dhi_core::GuardVerdict;
use serde::{Deserialize, Serialize};

use crate::{Decision, Policy};

/// What a rule matches against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchTarget {
    ToolName,
    ToolInput,
}

/// How the pattern compares against the match target. Regex patterns are
/// compiled once at snapshot build; a pattern that fails to compile is
/// skipped (ARTEX behavior: a bad stored regex never crashes evaluation).
#[derive(Debug, Clone)]
pub enum Matcher {
    /// Matches every tool name (the compiled policy's wildcard).
    Any,
    /// String equality for `tool_name`; case-insensitive substring for
    /// `tool_input`.
    Exact(String),
    /// Pre-compiled pattern, matched against the tool name or the raw input
    /// JSON.
    Regex(regex::Regex),
}

/// One enabled rule, ordered into the snapshot's evaluation sequence.
#[derive(Debug, Clone)]
pub struct CompiledRule {
    pub name: String,
    pub target: MatchTarget,
    pub matcher: Matcher,
    pub action: GuardVerdict,
    pub reason: String,
    /// Ask-approval budget in seconds; 0 = use the snapshot default.
    pub timeout_secs: i64,
}

/// Input to [`CompiledPolicy::build`] straight from a `guard_rules` row.
#[derive(Debug, Clone)]
pub struct RuleSpec {
    pub name: String,
    pub match_target: String,
    pub match_type: String,
    pub pattern: String,
    pub action: GuardVerdict,
    pub reason: String,
    /// Evaluation priority; higher first. Ties keep insertion (id) order.
    pub priority: i32,
    pub timeout_secs: i64,
}

/// The compiled snapshot. Cloning is an `Arc`-cheap handout to evaluators.
#[derive(Debug, Clone)]
pub struct CompiledPolicy {
    pub version: String,
    pub default: GuardVerdict,
    /// Settings-wide ask-approval budget in seconds; 0 = the executor's own
    /// construction default applies (preserves the pre-snapshot behavior).
    pub default_approval_timeout_secs: i64,
    /// Enabled rules, priority DESC, insertion-stable on ties.
    pub rules: Vec<CompiledRule>,
}

impl CompiledPolicy {
    /// Compiles rules for evaluation: sorts by priority DESC (stable), parses
    /// the match enums, and compiles regexes. Specs whose pattern fails to
    /// compile are skipped, never fatal.
    pub fn build(
        version: impl Into<String>,
        default: GuardVerdict,
        default_approval_timeout_secs: i64,
        specs: &[RuleSpec],
    ) -> Self {
        let mut ranked: Vec<&RuleSpec> = specs.iter().collect();
        ranked.sort_by_key(|spec| std::cmp::Reverse(spec.priority));

        let rules = ranked
            .iter()
            .filter_map(|spec| {
                let target = match spec.match_target.as_str() {
                    "tool_name" => MatchTarget::ToolName,
                    "tool_input" => MatchTarget::ToolInput,
                    _ => return None,
                };
                let matcher = match spec.match_type.as_str() {
                    "exact" => Matcher::Exact(spec.pattern.clone()),
                    "regex" => Matcher::Regex(regex::Regex::new(&spec.pattern).ok()?),
                    _ => return None,
                };
                Some(CompiledRule {
                    name: spec.name.clone(),
                    target,
                    matcher,
                    action: spec.action,
                    reason: spec.reason.clone(),
                    timeout_secs: spec.timeout_secs,
                })
            })
            .collect();

        Self {
            version: version.into(),
            default,
            default_approval_timeout_secs,
            rules,
        }
    }

    /// Wraps a frozen [`Policy`] in a snapshot: every rule becomes an exact
    /// `tool_name` matcher (`*` matches everything), timeouts fall through to
    /// the executor default. This keeps the legacy constructors honest.
    pub fn from_policy(policy: &Policy, default_approval_timeout_secs: i64) -> Self {
        let rules = policy
            .rules
            .iter()
            .map(|rule| CompiledRule {
                matcher: if rule.tool == "*" {
                    Matcher::Any
                } else {
                    Matcher::Exact(rule.tool.clone())
                },
                target: MatchTarget::ToolName,
                name: rule.name.clone(),
                action: rule.action,
                reason: rule.reason.clone(),
                timeout_secs: 0,
            })
            .collect();
        Self {
            version: policy.version.clone(),
            default: policy.default,
            default_approval_timeout_secs,
            rules,
        }
    }

    /// Evaluates the tool call. `input` is serialized to its raw JSON string
    /// once, so `tool_input` rules see exactly what the agent sent.
    pub fn evaluate(&self, tool_name: &str, input: &serde_json::Value) -> Decision {
        let input_json = input.to_string();
        for rule in &self.rules {
            if !rule.matches(tool_name, &input_json) {
                continue;
            }
            // ARTEX parity (`intercept/intercept.go`): a stored rule without a
            // message still names itself in the decision — downstream humans
            // and approval payloads never see an empty reason.
            let reason = if rule.reason.trim().is_empty() {
                format!("matched rule {}", rule.name)
            } else {
                rule.reason.clone()
            };
            return Decision {
                verdict: rule.action,
                matched_rule: Some(rule.name.clone()),
                rule_version: self.version.clone(),
                reason,
                approval_timeout_secs: rule.timeout_secs,
            };
        }
        Decision {
            verdict: self.default,
            matched_rule: None,
            rule_version: self.version.clone(),
            reason: "no matching rule; guard default applied".to_string(),
            approval_timeout_secs: 0,
        }
    }

    /// The ask-approval budget this snapshot prescribes for the decision:
    /// the matched rule's timeout when positive, else the settings default.
    /// `0` means "unset — the caller's own default applies".
    pub fn prescribed_approval_secs(&self, decision: &Decision) -> i64 {
        if decision.approval_timeout_secs > 0 {
            decision.approval_timeout_secs
        } else {
            self.default_approval_timeout_secs
        }
    }
}

impl CompiledRule {
    fn matches(&self, tool_name: &str, input_json: &str) -> bool {
        match (&self.target, &self.matcher) {
            (MatchTarget::ToolName, Matcher::Any) => true,
            (MatchTarget::ToolName, Matcher::Exact(pattern)) => tool_name == pattern,
            (MatchTarget::ToolName, Matcher::Regex(re)) => re.is_match(tool_name),
            (MatchTarget::ToolInput, Matcher::Exact(pattern)) => {
                input_json.to_lowercase().contains(&pattern.to_lowercase())
            }
            (MatchTarget::ToolInput, Matcher::Regex(re)) => re.is_match(input_json),
            // tool_input rules never widen to a name match.
            (MatchTarget::ToolInput, Matcher::Any) => false,
        }
    }
}

/// The swappable snapshot handle. Writers (the server, after every rule or
/// settings mutation) replace the inner `Arc`; readers clone it out per
/// evaluation and never await while holding the lock.
#[derive(Clone)]
pub struct SharedPolicy(Arc<RwLock<Arc<CompiledPolicy>>>);

impl SharedPolicy {
    pub fn new(initial: CompiledPolicy) -> Self {
        Self(Arc::new(RwLock::new(Arc::new(initial))))
    }

    pub fn from_policy(policy: &Policy, default_approval_timeout_secs: i64) -> Self {
        Self::new(CompiledPolicy::from_policy(
            policy,
            default_approval_timeout_secs,
        ))
    }

    /// Atomically publishes a rebuilt snapshot. Subsequent evaluations see it;
    /// already-parked approvals keep their recorded verdicts.
    pub fn swap(&self, next: CompiledPolicy) {
        if let Ok(mut guard) = self.0.write() {
            *guard = Arc::new(next);
        }
    }

    /// The current snapshot. A poisoned lock (writer panicked mid-swap) hands
    /// back the last good snapshot: guard evaluation must not lose its policy.
    pub fn load(&self) -> Arc<CompiledPolicy> {
        match self.0.read() {
            Ok(guard) => Arc::clone(&guard),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }
}

impl std::fmt::Debug for SharedPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let snapshot = self.load();
        f.debug_struct("SharedPolicy")
            .field("version", &snapshot.version)
            .field("default", &snapshot.default)
            .field("rules", &snapshot.rules.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(name: &str, target: &str, kind: &str, pattern: &str, action: GuardVerdict) -> RuleSpec {
        RuleSpec {
            name: name.to_string(),
            match_target: target.to_string(),
            match_type: kind.to_string(),
            pattern: pattern.to_string(),
            action,
            reason: format!("reason {name}"),
            priority: 0,
            timeout_secs: 0,
        }
    }

    #[test]
    fn exact_tool_name_and_default_deny_mirror_chess() {
        let policy = CompiledPolicy::from_policy(&Policy::chess(), 0);
        let allow = policy.evaluate("finishTurn", &json!({}));
        assert_eq!(allow.verdict, GuardVerdict::Allow);
        assert_eq!(allow.matched_rule.as_deref(), Some("allow-finishTurn"));

        let deny = policy.evaluate("shell", &json!({}));
        assert_eq!(deny.verdict, GuardVerdict::Deny);
        assert!(deny.matched_rule.is_none());

        let wildcard = CompiledPolicy::from_policy(
            &Policy {
                version: "wild".into(),
                default: GuardVerdict::Deny,
                rules: vec![crate::Rule {
                    name: "all".into(),
                    tool: "*".into(),
                    action: GuardVerdict::Allow,
                    reason: "anything".into(),
                }],
            },
            0,
        );
        assert_eq!(
            wildcard.evaluate("whatever", &json!({})).verdict,
            GuardVerdict::Allow
        );
    }

    #[test]
    fn first_match_wins_by_priority_desc() {
        let policy = CompiledPolicy::build(
            "ordered",
            GuardVerdict::Deny,
            0,
            &[
                spec(
                    "low",
                    "tool_name",
                    "exact",
                    "finishTurn",
                    GuardVerdict::Deny,
                ),
                RuleSpec {
                    priority: 10,
                    ..spec(
                        "high",
                        "tool_name",
                        "exact",
                        "finishTurn",
                        GuardVerdict::Allow,
                    )
                },
            ],
        );
        let decision = policy.evaluate("finishTurn", &json!({}));
        assert_eq!(decision.matched_rule.as_deref(), Some("high"));
        assert_eq!(decision.verdict, GuardVerdict::Allow);
    }

    #[test]
    fn tool_input_exact_is_case_insensitive_substring_of_raw_json() {
        let policy = CompiledPolicy::build(
            "inputs",
            GuardVerdict::Deny,
            0,
            &[spec(
                "rm",
                "tool_input",
                "exact",
                "RM -RF",
                GuardVerdict::Ask,
            )],
        );
        let decision = policy.evaluate("shell", &json!({"cmd": "rm -rf /"}));
        assert_eq!(decision.verdict, GuardVerdict::Ask);
        assert!(
            policy
                .evaluate("shell", &json!({"cmd": "ls"}))
                .matched_rule
                .is_none(),
            "unrelated input falls through to the default"
        );
    }

    #[test]
    fn regex_matches_tool_name_and_input() {
        let policy = CompiledPolicy::build(
            "regexes",
            GuardVerdict::Deny,
            0,
            &[
                spec(
                    "name-re",
                    "tool_name",
                    "regex",
                    "^deploy.*$",
                    GuardVerdict::Allow,
                ),
                spec(
                    "input-re",
                    "tool_input",
                    "regex",
                    "password",
                    GuardVerdict::Ask,
                ),
            ],
        );
        assert_eq!(
            policy.evaluate("deployment", &json!({})).verdict,
            GuardVerdict::Allow
        );
        assert_eq!(
            policy
                .evaluate("write_secret", &json!({"k": "the password here"}))
                .verdict,
            GuardVerdict::Ask
        );
    }

    #[test]
    fn bad_regex_spec_is_skipped_not_fatal() {
        let policy = CompiledPolicy::build(
            "broken",
            GuardVerdict::Deny,
            0,
            &[
                spec(
                    "bad",
                    "tool_name",
                    "regex",
                    "([unclosed",
                    GuardVerdict::Allow,
                ),
                spec("good", "tool_name", "exact", "shell", GuardVerdict::Allow),
            ],
        );
        assert_eq!(policy.rules.len(), 1, "the uncompilable rule is skipped");
        assert_eq!(
            policy.evaluate("shell", &json!({})).verdict,
            GuardVerdict::Allow
        );
    }

    #[test]
    fn disabled_rules_never_enter_the_snapshot_evaluation_order() {
        // Disabled rules are filtered by the store query; the snapshot builder
        // receives only enabled rows. This pins the contract for the builder.
        let policy = CompiledPolicy::build("only-enabled", GuardVerdict::Deny, 0, &[]);
        assert_eq!(
            policy.evaluate("anything", &json!({})).verdict,
            GuardVerdict::Deny
        );
    }

    /// ARTEX parity (`intercept/intercept.go` defaultMessage): a stored rule
    /// with an empty reason still names itself in the decision, so approval
    /// payloads never carry a blank message.
    #[test]
    fn empty_rule_reason_falls_back_to_a_message_naming_the_rule() {
        let unnamed = |name: &str, action: GuardVerdict| RuleSpec {
            name: name.to_string(),
            match_target: "tool_name".to_string(),
            match_type: "exact".to_string(),
            pattern: "deploy".to_string(),
            action,
            reason: "   ".to_string(),
            priority: 0,
            timeout_secs: 0,
        };
        let policy = CompiledPolicy::build(
            "unnamed",
            GuardVerdict::Deny,
            0,
            &[
                unnamed("ask-deploy", GuardVerdict::Ask),
                unnamed("deny-deploy", GuardVerdict::Deny),
            ],
        );

        let ask = policy.evaluate("deploy", &json!({}));
        assert_eq!(ask.verdict, GuardVerdict::Ask);
        assert_eq!(ask.matched_rule.as_deref(), Some("ask-deploy"));
        assert_eq!(ask.reason, "matched rule ask-deploy");

        let deny = CompiledPolicy::build(
            "unnamed-deny",
            GuardVerdict::Deny,
            0,
            &[unnamed("deny-only", GuardVerdict::Deny)],
        )
        .evaluate("deploy", &json!({}));
        assert_eq!(deny.verdict, GuardVerdict::Deny);
        assert_eq!(deny.reason, "matched rule deny-only");

        // A real reason is passed through untouched.
        let named = spec("loud", "tool_name", "exact", "deploy", GuardVerdict::Ask);
        let named_decision = CompiledPolicy::build("named", GuardVerdict::Deny, 0, &[named])
            .evaluate("deploy", &json!({}));
        assert_eq!(named_decision.reason, "reason loud");
    }

    #[test]
    fn rule_timeout_overrides_snapshot_default() {
        let policy = CompiledPolicy::build(
            "timeouts",
            GuardVerdict::Ask,
            900,
            &[RuleSpec {
                timeout_secs: 30,
                ..spec("quick", "tool_name", "exact", "deploy", GuardVerdict::Ask)
            }],
        );
        let decision = policy.evaluate("deploy", &json!({}));
        assert_eq!(policy.prescribed_approval_secs(&decision), 30);
        let fallback = policy.evaluate("unknown", &json!({}));
        assert_eq!(policy.prescribed_approval_secs(&fallback), 900);
        // Zero everywhere means "caller default": never a zero-length budget.
        let none = CompiledPolicy::build("none", GuardVerdict::Ask, 0, &[]);
        let unmatched = none.evaluate("anything", &json!({}));
        assert_eq!(none.prescribed_approval_secs(&unmatched), 0);
    }

    #[test]
    fn shared_policy_swaps_atomically() {
        let shared = SharedPolicy::from_policy(&Policy::chess(), 0);
        assert_eq!(shared.load().version, "chess-v1");
        shared.swap(CompiledPolicy::build("next", GuardVerdict::Allow, 0, &[]));
        let current = shared.load();
        assert_eq!(current.version, "next");
        assert_eq!(
            current.evaluate("anything", &json!({})).verdict,
            GuardVerdict::Allow
        );
    }
}
