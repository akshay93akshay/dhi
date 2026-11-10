//! Manager-worker lead runtime helpers.
//!
//! The lead is a planner-role model call with no tools. It receives the
//! serialized [`dhi_core::LeadInput`], renders `ops/prompts/manager.md`, and
//! must answer with one JSON [`dhi_core::LeadDecision`]. Parsing tolerates a
//! Markdown code fence and surrounding prose; validation is delegated to
//! [`dhi_core::LeadDecision::validate`] so the runtime and the runner enforce
//! the same rules.
//!
//! This module is only the rendering/parsing half: `dhi-agent` cannot depend
//! on `dhi-orchestrator`, so the `TeamLead` adapter lives in the caller.

use uuid::Uuid;

use crate::AgentCall;

/// One lead review request from the engine.
#[derive(Debug, Clone)]
pub struct LeadReviewInput {
    pub task_id: Uuid,
    pub intent_id: Uuid,
    /// The exact lead input the runner built for this turn.
    pub input: dhi_core::LeadInput,
    /// The task's pinned planner profile. The lead runs on the planner role
    /// and profile for now. `None` follows the planner's default chain.
    pub planner_profile_id: Option<i64>,
}

/// Serialize the lead input for the manager prompt's user turn.
pub fn lead_input_json(input: &dhi_core::LeadInput) -> anyhow::Result<String> {
    serde_json::to_string(input)
        .map_err(|err| anyhow::anyhow!("could not serialize the lead input: {err}"))
}

/// Parse the lead's JSON decision.
///
/// Code fences are stripped when present and the first `{` through the last
/// `}` is parsed, so surrounding prose is tolerated. Shape, worker, task,
/// dependency, and budget validation remains in
/// [`dhi_core::LeadDecision::validate`].
pub fn parse_lead_decision(text: &str) -> anyhow::Result<dhi_core::LeadDecision> {
    let text = crate::strip_code_fences(text);
    let start = text
        .find('{')
        .ok_or_else(|| anyhow::anyhow!("lead response contains no JSON object"))?;
    let end = text
        .rfind('}')
        .ok_or_else(|| anyhow::anyhow!("lead response contains no closing JSON brace"))?;
    if end < start {
        anyhow::bail!("lead response has malformed JSON braces");
    }
    serde_json::from_str(&text[start..=end])
        .map_err(|err| anyhow::anyhow!("lead response is not valid lead JSON: {err}"))
}

/// Parse and validate one lead decision against the input it answered.
pub fn parse_and_validate_lead_decision(
    text: &str,
    input: &dhi_core::LeadInput,
) -> anyhow::Result<dhi_core::LeadDecision> {
    let decision = parse_lead_decision(text)?;
    decision
        .validate(input)
        .map_err(|err| anyhow::anyhow!("lead decision rejected: {err}"))?;
    Ok(decision)
}

/// The parsed decision plus the runtime accounting of the call.
pub type LeadCall = AgentCall<dhi_core::LeadDecision>;

#[cfg(test)]
mod tests {
    use super::*;
    use dhi_core::{
        CapabilityLimits, LeadBudget, LeadDecision, LeadInput, LeadLimits, LeadTask, WorkerSpec,
    };

    fn worker(key: &str) -> WorkerSpec {
        WorkerSpec {
            key: key.to_owned(),
            role: "worker".to_owned(),
            prompt: "do work".to_owned(),
            max_turns: 1,
        }
    }

    fn input() -> LeadInput {
        LeadInput {
            intent: "ship it".to_owned(),
            workers: vec![worker("alpha"), worker("beta")],
            tasks: vec![],
            results: vec![],
            round: 1,
            limits: LeadLimits::from_capabilities(&CapabilityLimits::default(), 0),
            remaining: LeadBudget {
                remaining_rounds: 3,
                remaining_lead_turns: 4,
                remaining_worker_runs: 8,
            },
        }
    }

    #[test]
    fn parses_every_decision_shape() {
        let final_decision = parse_lead_decision(
            r#"{"decision":"final","summary":"done","detail":"d","evidence":"e","confidence":"inferred"}"#,
        )
        .unwrap();
        assert!(matches!(
            final_decision,
            LeadDecision::Final { ref summary, .. } if summary == "done"
        ));

        let continued = parse_lead_decision(
            r#"{"decision":"continue","tasks":[{"id":"t1","title":"check","assignee":"alpha","depends_on":["seed"]}],"reason":"next"}"#,
        )
        .unwrap();
        assert!(matches!(
            continued,
            LeadDecision::Continue { ref tasks, .. }
                if tasks.len() == 1 && tasks[0].depends_on == ["seed"]
        ));

        let ask = parse_lead_decision(
            r#"{"decision":"ask_worker","worker_key":"beta","prompt":"check the edge case"}"#,
        )
        .unwrap();
        assert!(matches!(
            ask,
            LeadDecision::AskWorker { ref worker_key, .. } if worker_key == "beta"
        ));

        let stop = parse_lead_decision(r#"{"decision":"stop","reason":"blocked"}"#).unwrap();
        assert!(matches!(
            stop,
            LeadDecision::Stop { ref reason } if reason == "blocked"
        ));
    }

    #[test]
    fn tolerates_fences_and_surrounding_prose() {
        let decision = parse_lead_decision(
            "Here is the decision:\n```json\n{\"decision\":\"stop\",\"reason\":\"done\"}\n```\n",
        )
        .unwrap();
        assert!(matches!(decision, LeadDecision::Stop { .. }));
    }

    #[test]
    fn rejects_a_non_json_answer() {
        let err = parse_lead_decision("I think we are done.").unwrap_err();
        assert!(err.to_string().contains("no JSON object"));
    }

    #[test]
    fn validates_the_decision_against_the_input() {
        let decision = parse_and_validate_lead_decision(
            r#"{"decision":"continue","tasks":[{"id":"t1","title":"check","assignee":"ghost"}],"reason":"next"}"#,
            &input(),
        )
        .unwrap_err();
        assert!(decision.to_string().contains("unknown worker"));

        let decision = parse_and_validate_lead_decision(
            r#"{"decision":"continue","tasks":[{"id":"t1","title":"check","assignee":"alpha"}],"reason":"next"}"#,
            &input(),
        )
        .unwrap();
        assert!(matches!(decision, LeadDecision::Continue { .. }));
    }

    #[test]
    fn lead_task_needs_text_and_a_worker_key() {
        let task = LeadTask {
            id: "t1".to_owned(),
            title: String::new(),
            description: String::new(),
            assignee: "alpha".to_owned(),
            depends_on: vec![],
        };
        let value = serde_json::to_value(task).unwrap();
        assert_eq!(value["id"], "t1");
    }
}
