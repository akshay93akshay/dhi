//! Live River smoke test. Skipped unless `DHI_LIVE_RIVER=1`.

use std::path::Path;

use dhi_agent::{AgentRuntime, ExecuteInput, PlanInput, PlanOutcome, PromptSet, RigAgentRuntime};
use dhi_core::{Goal, GoalState, Task};

#[tokio::test]
async fn live_river_planner_smoke() {
    if std::env::var("DHI_LIVE_RIVER").as_deref() != Ok("1") {
        return;
    }

    let prompts = PromptSet::discover(Path::new(env!("CARGO_MANIFEST_DIR")))
        .expect("ops/prompts must be discoverable");
    let runtime = RigAgentRuntime::from_env(prompts).expect("River configuration must load");

    let task = Task::new(
        "Find the shortest forced mate from the starting position.",
        4,
    );
    let origin = task.origin_fact();
    let goal = Goal {
        id: uuid::Uuid::new_v4(),
        task_id: task.id,
        description: task.goal.clone(),
        state: GoalState::Open,
        proof_fact_id: None,
        created_at: task.created_at,
    };

    let call = runtime
        .plan(PlanInput {
            task_id: task.id,
            goal: task.goal.clone(),
            bound: 4,
            goals: vec![goal],
            facts: vec![origin],
            open_intents: 0,
            planner_profile_id: None,
        })
        .await
        .expect("the live planner call must succeed");

    assert!(
        !call.response.trim().is_empty(),
        "the live planner must return text"
    );
    assert!(
        call.meta.usage.input_tokens > 0 || call.meta.usage.output_tokens > 0,
        "the live planner must report token usage"
    );
    assert!(
        matches!(call.output, PlanOutcome::AddIntent { .. }),
        "a fresh goal with only an origin fact must produce AddIntent, got {:?}",
        call.output
    );
    println!(
        "live planner: outcome={:?} tokens_in={} tokens_out={}",
        call.output, call.meta.usage.input_tokens, call.meta.usage.output_tokens
    );
}

/// The worker must record one structured fact draft through `record_fact`.
/// No specific domain answer is required.
#[tokio::test]
async fn live_river_worker_smoke() {
    if std::env::var("DHI_LIVE_RIVER").as_deref() != Ok("1") {
        return;
    }

    let prompts = PromptSet::discover(Path::new(env!("CARGO_MANIFEST_DIR")))
        .expect("ops/prompts must be discoverable");
    let runtime = RigAgentRuntime::from_env(prompts).expect("River configuration must load");

    let call = runtime
        .execute(ExecuteInput {
            task_id: uuid::Uuid::new_v4(),
            // A synthetic run scope: the live smoke test has no claimed
            // intent row, so a fresh id stands in for it.
            intent_id: uuid::Uuid::new_v4(),
            intent: "State the sum of 2 and 3, and say whether you computed it or recalled it."
                .to_string(),
            context: vec![
                "This is a local reasoning task; no domain tools are available.".to_string(),
            ],
            worker_profile_id: None,
            max_turns: None,
        })
        .await
        .expect("the live worker call must succeed");

    let fact = &call.output.fact;
    assert!(
        !fact.summary.trim().is_empty(),
        "the live worker must record a summary"
    );
    assert!(
        !fact.evidence.trim().is_empty(),
        "the live worker must record evidence"
    );
    assert!(
        fact.confidence == "observed" || fact.confidence == "inferred",
        "confidence must be observed or inferred, got {:?}",
        fact.confidence
    );
    assert!(
        !call.response.trim().is_empty(),
        "the worker must answer one short sentence after the tool call"
    );
    println!(
        "live worker: summary={:?} confidence={} evidence={:?} tokens_in={} tokens_out={}",
        fact.summary,
        fact.confidence,
        fact.evidence,
        call.meta.usage.input_tokens,
        call.meta.usage.output_tokens
    );
}
