//! The two doors for a finished task.
//!
//! This crate is deliberately independent of the store, server, and agent.
//! It owns the pure rules both doors share:
//!
//! - Door 1: the frozen side-question snapshot and its limits.
//! - Door 2: which states may reopen, goal/question validation, and the
//!   fresh-evidence rule (a fact may prove a goal only when the fact is newer
//!   than the goal).
//!
//! BASE adapters (store SQL, HTTP routes, Rig answerer, UI) call these rules.
//! Keeping the rules here lets them be built and tested alone.

use chrono::{DateTime, Utc};
use dhi_core::{Activity, Fact, Goal, Task, TaskState};
use serde_json::{Value, json};

/// Largest accepted side question, after trimming.
pub const SIDE_QUESTION_MAX_CHARS: usize = 4_000;

/// Largest accepted add-goal description, after trimming.
pub const ADD_GOAL_MAX_CHARS: usize = 500;

/// The side-question snapshot keeps only this many activity rows.
pub const SIDE_QUESTION_ACTIVITY_LIMIT: usize = 50;

/// Door 2, fresh evidence: a fact may prove a goal only when it was recorded
/// after the goal was created. This stops an old fact (already used for an
/// earlier goal) from "proving" a human-added goal without new work.
pub fn fresh_evidence(fact_created_at: DateTime<Utc>, goal_created_at: DateTime<Utc>) -> bool {
    fact_created_at > goal_created_at
}

/// Door 2, reopen rule: only terminal tasks may be reopened by a human.
pub fn can_reopen(state: TaskState) -> bool {
    state.is_terminal()
}

/// Door 1, question validation. Returns the trimmed text or a stable message.
pub fn validate_question(raw: &str) -> Result<String, &'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("question is required");
    }
    if trimmed.chars().count() > SIDE_QUESTION_MAX_CHARS {
        return Err("question is too long");
    }
    Ok(trimmed.to_string())
}

/// Door 2, goal validation. Returns the trimmed text or a stable message.
pub fn validate_goal(raw: &str) -> Result<String, &'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("goal description is required");
    }
    if trimmed.chars().count() > ADD_GOAL_MAX_CHARS {
        return Err("goal description is too long");
    }
    Ok(trimmed.to_string())
}

/// Door 1: the JSON frozen when a session opens.
///
/// The caller fetches the rows; this function only assembles and caps them,
/// so it can be tested without a database. `activity` is capped to the newest
/// [`SIDE_QUESTION_ACTIVITY_LIMIT`] rows.
pub fn build_snapshot(task: &Task, goals: &[Goal], facts: &[Fact], activity: &[Activity]) -> Value {
    let start = activity.len().saturating_sub(SIDE_QUESTION_ACTIVITY_LIMIT);
    let activity: Vec<Value> = activity[start..]
        .iter()
        .map(|row| {
            json!({
                "id": row.id,
                "kind": row.kind,
                "message": row.message,
                "payload": row.payload,
                "created_at": row.created_at,
            })
        })
        .collect();
    let goals: Vec<Value> = goals
        .iter()
        .map(|goal| {
            json!({
                "id": goal.id,
                "description": goal.description,
                "state": goal.state.as_str(),
                "proof_fact_id": goal.proof_fact_id,
                "created_at": goal.created_at,
            })
        })
        .collect();
    let facts: Vec<Value> = facts
        .iter()
        .map(|fact| {
            json!({
                "claim": fact.claim,
                "state": fact.state.as_str(),
                "provenance": fact.provenance,
            })
        })
        .collect();

    json!({
        "task": {
            "id": task.id,
            "goal": task.goal,
            "state": task.state.as_str(),
            "bound": task.bound,
            "pins": {
                "planner_profile_id": task.pinned_planner_profile_id,
                "worker_profile_id": task.pinned_worker_profile_id,
            },
            "created_at": task.created_at,
        },
        "goals": goals,
        "facts": facts,
        "activity": activity,
    })
}

/// Door 1: the short, non-sensitive summary shown in the session list.
pub fn snapshot_summary(snapshot: &Value) -> Value {
    let count = |key: &str| {
        snapshot
            .get(key)
            .and_then(Value::as_array)
            .map_or(0, Vec::len)
    };
    json!({
        "goal": snapshot.pointer("/task/goal").and_then(Value::as_str).unwrap_or(""),
        "state": snapshot.pointer("/task/state").and_then(Value::as_str).unwrap_or(""),
        "goals": count("goals"),
        "facts": count("facts"),
        "activity": count("activity"),
    })
}
