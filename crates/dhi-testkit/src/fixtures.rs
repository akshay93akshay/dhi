//! Deterministic fixtures shared by store, engine, and server tests.
//!
//! Fixtures return the `MemoryStore` alongside the seeded records so callers
//! can assert on the rest of the graph. They build no hidden state: the only
//! rows present are the ones the fixture names.

use dhi_core::{AgentRole, AgentRun, Intent, RunId, Task, TaskId};

use crate::MemoryStore;

/// Seed a task and its single origin fact.
pub fn seeded_task(goal: impl Into<String>, bound: i32) -> (MemoryStore, Task) {
    let store = MemoryStore::new();
    let task = store.seed_task(goal, bound);
    (store, task)
}

/// Seed a task plus one open intent that points at the task's origin fact.
pub fn seeded_intent(
    goal: impl Into<String>,
    bound: i32,
    description: impl Into<String>,
) -> (MemoryStore, Task, Intent) {
    let (store, task) = seeded_task(goal, bound);
    let intent = store.seed_intent(task.id, task.origin_fact_id, description);
    (store, task, intent)
}

/// One `agent_runs` row for `task_id`, with every agent-run column set to a
/// deterministic value. `id` is fresh, so two calls make two distinct rows.
pub fn agent_run(task_id: TaskId, run_id: RunId, call_index: u32) -> AgentRun {
    let now = chrono::Utc::now();
    AgentRun {
        id: uuid::Uuid::new_v4(),
        run_id,
        call_index,
        task_id,
        intent_id: None,
        role: AgentRole::Planner,
        model: "deepseek-v4.1".to_string(),
        profile: "test/profile".to_string(),
        prompt: "system\n\nuser".to_string(),
        prompt_hash: "planner@fixture".to_string(),
        variables: serde_json::json!({}),
        response: Some("ok".to_string()),
        thinking: None,
        tokens_in: 1,
        tokens_out: 1,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: 0,
        cost: 0.0,
        status: "ok".to_string(),
        error: None,
        latency_ms: 1,
        checkpoint_id: None,
        created_at: now,
        updated_at: now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dhi_core::{FactState, IntentState};

    #[test]
    fn seeded_task_has_one_origin_fact() {
        let (store, task) = seeded_task("forced mate", 4);

        let facts = store.facts(task.id);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].id, task.origin_fact_id);
        assert_eq!(facts[0].state, FactState::Origin);
    }

    #[test]
    fn seeded_intent_is_open_and_points_at_origin() {
        let (store, task, intent) = seeded_intent("forced mate", 4, "find the mate");

        assert_eq!(intent.task_id, task.id);
        assert_eq!(intent.fact_id, task.origin_fact_id);
        assert_eq!(intent.state, IntentState::Open);

        let intents = store.intents(task.id);
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].id, intent.id);
    }
}
