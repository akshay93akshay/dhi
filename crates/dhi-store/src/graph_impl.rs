use std::future::Future;
use std::pin::Pin;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dhi_core::{
    Activity, AgentRun, Approval, ApprovalDecision, ApprovalId, Fact, FactId, Goal, GoalId,
    GraphStore, GuardDecision, GuardDecisionId, GuardStore, GuardVerdict, Intent, IntentId,
    IntentPlan, IntentState, NewSideQuestionRequest, ReopenOutcome, ResultRecord, ResultStatus,
    SideQuestionRequest, SideQuestionSession, SideQuestionThread, SwarmMessageRecord, Task,
    TaskCapacity, TaskId, TaskInstruction, TaskState, ToolCall, ToolCallId,
};
use uuid::Uuid;

use crate::Store;

#[async_trait]
impl GraphStore for Store {
    async fn create_task(&self, goal: String) -> anyhow::Result<Task> {
        Ok(Store::create_task(self, &goal).await?)
    }

    async fn create_task_with_pins(
        &self,
        goal: String,
        planner_pin: Option<i64>,
        worker_pin: Option<i64>,
        topology: Option<dhi_core::Topology>,
        swarm_workers: Option<usize>,
    ) -> anyhow::Result<Task> {
        Ok(Store::create_task_with_pins(
            self,
            &goal,
            planner_pin,
            worker_pin,
            topology,
            swarm_workers,
        )
        .await?)
    }

    async fn get_task(&self, id: TaskId) -> anyhow::Result<Option<Task>> {
        Ok(Store::get_task(self, id).await?)
    }

    async fn list_tasks(&self, limit: i64) -> anyhow::Result<Vec<Task>> {
        Ok(Store::list_tasks(self, limit).await?)
    }

    async fn set_task_state_guarded(&self, id: TaskId, state: TaskState) -> anyhow::Result<bool> {
        Ok(Store::set_task_state_guarded(self, id, state).await?)
    }

    async fn resume_paused_task(&self, id: TaskId) -> anyhow::Result<bool> {
        Ok(Store::resume_paused_task(self, id).await?)
    }

    async fn set_task_capacity(
        &self,
        task_id: TaskId,
        capacity: TaskCapacity,
    ) -> anyhow::Result<Task> {
        Ok(Store::set_task_capacity(self, task_id, capacity).await?)
    }

    async fn add_goal(&self, task_id: TaskId, description: String) -> anyhow::Result<Goal> {
        Ok(Store::add_goal(self, task_id, description).await?)
    }

    async fn list_goals(&self, task_id: TaskId) -> anyhow::Result<Vec<Goal>> {
        Ok(Store::list_goals(self, task_id).await?)
    }

    async fn reopen_task_with_goal(
        &self,
        task_id: TaskId,
        description: String,
    ) -> anyhow::Result<ReopenOutcome> {
        Ok(Store::reopen_task_with_goal(self, task_id, description).await?)
    }

    async fn prove_goal(
        &self,
        task_id: TaskId,
        goal_id: GoalId,
        fact_id: FactId,
        reason: String,
    ) -> anyhow::Result<bool> {
        Ok(Store::prove_goal(self, task_id, goal_id, fact_id, reason).await?)
    }

    async fn add_intent(
        &self,
        task_id: TaskId,
        fact_id: FactId,
        description: String,
    ) -> anyhow::Result<Intent> {
        Ok(Store::add_intent(self, task_id, fact_id, description).await?)
    }

    async fn list_open_intents(&self, task_id: TaskId, limit: i64) -> anyhow::Result<Vec<Intent>> {
        Ok(Store::list_open_intents(self, task_id, limit).await?)
    }

    async fn list_intents(&self, task_id: TaskId, limit: i64) -> anyhow::Result<Vec<Intent>> {
        Ok(Store::list_intents(self, task_id, limit).await?)
    }

    async fn save_intent_plan(&self, intent_id: IntentId, plan: &IntentPlan) -> anyhow::Result<()> {
        Ok(Store::save_intent_plan(self, intent_id, plan).await?)
    }

    async fn get_intent_plan(&self, intent_id: IntentId) -> anyhow::Result<Option<IntentPlan>> {
        Ok(Store::get_intent_plan(self, intent_id).await?)
    }

    async fn append_swarm_message(
        &self,
        task_id: TaskId,
        intent_id: IntentId,
        kind: &str,
        payload: serde_json::Value,
    ) -> anyhow::Result<()> {
        Ok(Store::append_swarm_message(self, task_id, intent_id, kind, payload).await?)
    }

    async fn list_swarm_messages(
        &self,
        task_id: TaskId,
        limit: i64,
    ) -> anyhow::Result<Vec<SwarmMessageRecord>> {
        Ok(Store::list_swarm_messages(self, task_id, limit).await?)
    }

    async fn claim_intent(
        &self,
        intent_id: IntentId,
        owner: &str,
    ) -> anyhow::Result<Option<Intent>> {
        Ok(Store::claim_intent(self, intent_id, owner).await?)
    }

    async fn resume_intent(
        &self,
        intent_id: IntentId,
        owner: &str,
    ) -> anyhow::Result<Option<Intent>> {
        Ok(Store::resume_intent(self, intent_id, owner).await?)
    }

    async fn finish_intent(&self, intent_id: IntentId, state: IntentState) -> anyhow::Result<()> {
        Store::finish_intent(self, intent_id, state).await?;
        Ok(())
    }

    async fn finish_paused_intent(
        &self,
        intent_id: IntentId,
        owner: &str,
        status: IntentState,
    ) -> anyhow::Result<Option<Intent>> {
        Ok(Store::finish_paused_intent(self, intent_id, owner, status).await?)
    }

    async fn block_orphaned_paused_intents(&self, task_id: TaskId) -> anyhow::Result<usize> {
        Ok(Store::block_orphaned_paused_intents(self, task_id).await?)
    }

    async fn reset_running_intents(&self) -> anyhow::Result<usize> {
        Ok(Store::reset_running_intents(self).await?)
    }

    async fn record_fact(
        &self,
        task_id: TaskId,
        claim: String,
        provenance: serde_json::Value,
    ) -> anyhow::Result<Fact> {
        Ok(Store::record_fact(self, task_id, claim, provenance).await?)
    }

    async fn record_fact_for_intent(
        &self,
        task_id: TaskId,
        intent_id: IntentId,
        claim: String,
        provenance: serde_json::Value,
    ) -> anyhow::Result<Fact> {
        Ok(Store::record_fact_for_intent(self, task_id, intent_id, claim, provenance).await?)
    }

    async fn list_facts(&self, task_id: TaskId) -> anyhow::Result<Vec<Fact>> {
        Ok(Store::list_facts(self, task_id).await?)
    }

    async fn record_result(
        &self,
        task_id: TaskId,
        intent_id: IntentId,
        claim: String,
        acceptance: ResultStatus,
        evidence: serde_json::Value,
    ) -> anyhow::Result<ResultRecord> {
        Ok(Store::record_result(self, task_id, intent_id, claim, acceptance, evidence).await?)
    }

    async fn list_results(&self, task_id: TaskId, limit: i64) -> anyhow::Result<Vec<ResultRecord>> {
        Ok(Store::list_results(self, task_id, limit).await?)
    }

    async fn append_activity(
        &self,
        task_id: TaskId,
        kind: &str,
        message: String,
        payload: serde_json::Value,
    ) -> anyhow::Result<Activity> {
        Ok(Store::append_activity(self, task_id, kind, message, payload).await?)
    }

    async fn list_activity(
        &self,
        task_id: TaskId,
        kind: Option<&str>,
    ) -> anyhow::Result<Vec<Activity>> {
        Ok(Store::list_activity(self, task_id, kind).await?)
    }

    async fn add_instruction(
        &self,
        task_id: TaskId,
        text: String,
    ) -> anyhow::Result<TaskInstruction> {
        Ok(Store::add_instruction(self, task_id, text).await?)
    }

    async fn list_instructions(&self, task_id: TaskId) -> anyhow::Result<Vec<TaskInstruction>> {
        Ok(Store::list_instructions(self, task_id).await?)
    }

    async fn mark_instructions_delivered(&self, task_id: TaskId) -> anyhow::Result<usize> {
        Ok(Store::mark_instructions_delivered(self, task_id).await?)
    }

    async fn mark_instructions_delivered_ids(
        &self,
        task_id: TaskId,
        instruction_ids: &[Uuid],
    ) -> anyhow::Result<usize> {
        Ok(Store::mark_instructions_delivered_ids(self, task_id, instruction_ids).await?)
    }

    fn save_agent_run<'a>(
        &'a self,
        run: &'a AgentRun,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async move { Ok(Store::save_agent_run(self, run).await?) })
    }

    async fn list_agent_runs(&self, task_id: TaskId) -> anyhow::Result<Vec<AgentRun>> {
        Ok(Store::list_agent_runs(self, task_id).await?)
    }

    async fn create_side_question_session(
        &self,
        task_id: TaskId,
        snapshot: serde_json::Value,
    ) -> anyhow::Result<SideQuestionSession> {
        Ok(Store::create_side_question_session(self, task_id, snapshot).await?)
    }

    async fn get_open_side_question_session(
        &self,
        task_id: TaskId,
    ) -> anyhow::Result<Option<SideQuestionSession>> {
        Ok(Store::get_open_side_question_session(self, task_id).await?)
    }

    async fn get_side_question_session(
        &self,
        session_id: Uuid,
    ) -> anyhow::Result<Option<SideQuestionSession>> {
        Ok(Store::get_side_question_session(self, session_id).await?)
    }

    async fn list_side_question_sessions(
        &self,
        task_id: TaskId,
    ) -> anyhow::Result<Vec<SideQuestionThread>> {
        Ok(Store::list_side_question_sessions(self, task_id).await?)
    }

    async fn append_side_question(
        &self,
        request: NewSideQuestionRequest,
    ) -> anyhow::Result<SideQuestionRequest> {
        Ok(Store::append_side_question(self, request).await?)
    }
}

#[async_trait]
impl GuardStore for Store {
    async fn record_tool_call(
        &self,
        task_id: TaskId,
        intent_id: IntentId,
        tool: String,
        input: serde_json::Value,
    ) -> anyhow::Result<ToolCall> {
        Ok(Store::record_tool_call(self, task_id, intent_id, &tool, input).await?)
    }

    async fn get_tool_call(&self, tool_call_id: ToolCallId) -> anyhow::Result<Option<ToolCall>> {
        Ok(Store::get_tool_call(self, tool_call_id).await?)
    }

    async fn complete_tool_call(
        &self,
        tool_call_id: ToolCallId,
        output: Option<serde_json::Value>,
        status: &str,
    ) -> anyhow::Result<()> {
        Store::complete_tool_call(self, tool_call_id, output, status).await?;
        Ok(())
    }

    async fn record_guard_decision(
        &self,
        tool_call_id: ToolCallId,
        verdict: GuardVerdict,
        matched_rule: Option<String>,
        rule_version: Option<String>,
        reason: String,
    ) -> anyhow::Result<GuardDecision> {
        Ok(Store::record_guard_decision(
            self,
            tool_call_id,
            verdict,
            matched_rule,
            rule_version,
            reason,
        )
        .await?)
    }

    async fn create_approval(
        &self,
        task_id: TaskId,
        tool_call_id: ToolCallId,
        guard_decision_id: GuardDecisionId,
        reason: String,
        expires_at: DateTime<Utc>,
    ) -> anyhow::Result<Approval> {
        Ok(Store::create_approval(
            self,
            task_id,
            tool_call_id,
            guard_decision_id,
            reason,
            expires_at,
        )
        .await?)
    }

    async fn get_approval(&self, approval_id: ApprovalId) -> anyhow::Result<Option<Approval>> {
        Ok(Store::get_approval(self, approval_id).await?)
    }

    async fn list_pending_approvals(&self, task_id: TaskId) -> anyhow::Result<Vec<Approval>> {
        Ok(Store::list_pending_approvals(self, task_id).await?)
    }

    async fn list_pending_approvals_all(&self) -> anyhow::Result<Vec<Approval>> {
        Ok(Store::list_pending_approvals_all(self).await?)
    }

    async fn expire_pending_approvals(
        &self,
        task_id: TaskId,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Vec<Approval>> {
        Ok(Store::expire_pending_approvals(self, task_id, now).await?)
    }

    async fn list_expired_approvals(&self, task_id: TaskId) -> anyhow::Result<Vec<Approval>> {
        Ok(Store::list_expired_approvals(self, task_id).await?)
    }

    async fn block_intent_for_approval(
        &self,
        approval_id: ApprovalId,
        owner: &str,
    ) -> anyhow::Result<bool> {
        Ok(Store::block_intent_for_approval(self, approval_id, owner).await?)
    }

    async fn decide_approval(
        &self,
        approval_id: ApprovalId,
        decision: ApprovalDecision,
    ) -> anyhow::Result<Option<Approval>> {
        Ok(Store::decide_approval(self, approval_id, decision).await?)
    }
}
