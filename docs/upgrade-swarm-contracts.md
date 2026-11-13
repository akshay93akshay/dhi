# Upgrade Swarm — implementation contracts

Branch: `Upgrade_swarm`. Base: `93e3664`.

## Fixed decisions

| Item | Decision |
|---|---|
| Topologies | Single, Manager-worker, Swarm. Separate |
| DHI+ core | `planner -> intent -> engine claim -> execution -> fact -> planner` retained |
| Manager-worker | Lead loop, direct assignments, synthesis, next steps. No board or mailbox |
| Swarm | Cline Teams shared-state model |
| Swarm UI | Tab **Swarm**. Default **Task board**. Graph secondary. Mailbox **Inbox** and **Sent** |
| Worker count | 2–64 plus **Auto**. N means teammates |
| Coordinator | One extra agent. Persisted role string `lead`; not counted in N |
| Parallel | All N teammates run at once |
| Hard cap | 64 teammates |
| Fixed N | Exactly N. No approval |
| Auto | Coordinator chooses up to 8. Above 8 needs `swarm_scale` approval up to 64 |
| Scale approval | Included |
| Swarm persistence | Postgres team state |
| Manager-worker persistence | Lead runs and activity only |

## Core boundary

The engine contract is unchanged:

```text
input:  claimed intent, plan, facts, capacity
output: IntentResult { fact, worker_results, audit_results }
```

The engine still records the fact, finishes the intent, wakes the planner, and the planner still proves goals. The coordinator and team never set goal or task state.

## Task creation contract

A task selects an existing host project through the file browser. The server
does not copy or upload the project.

```text
GET /api/fs
GET /api/fs?path=<absolute-host-directory>

POST /api/tasks
{
  "goal": "...",
  "project_folder": "/abs/host/path/to/project",
  "agents_md_path": "/abs/host/path/to/project/AGENTS.md",
  ...
}
```

- `GET /api/fs` lists directories and files below `DHI_FILE_BROWSER_ROOT`,
  starting at the root. `path` is an absolute host directory under the root;
  paths outside the root, relative paths, `..`, and paths through a symlink
  are rejected. The response carries `root`, the listed `path`, its `parent`
  (`null` at the root), and `entries[]`. Each entry has `name`, the absolute
  host `path` used for selection, `kind` (`directory` or `file`), and
  optional `size` and `modified_at`. Symlink entries are skipped.
- `project_folder` is the absolute path of the selected project folder on this
  Mac. The task form requires it. The API keeps the legacy no-folder path for
  compatibility with old clients. It replaces membership in
  `DHI_PROJECT_FOLDERS` as the task-creation selector. The server and the task
  sandbox run as the same macOS user and resolve it to the same directory.
- `agents_md_path` is the absolute path of the selected `AGENTS.md`. It
  may be inside or outside `project_folder`, but it must resolve below
  `DHI_FILE_BROWSER_ROOT`, avoid symlinks, name the file exactly
  `AGENTS.md`, and be at most 32 KiB. It is `null` when no file is selected.
  The server reads only that path and does not search for another
  `AGENTS.md`.
- The task folder is `<project_folder>/.dhi/<task-id>`. The task sandbox reads
  `project_folder` and writes only in the task folder. Existing project files
  are unchanged.
- `DHI_FILE_BROWSER_ROOT` is required for the file-browser routes. The server
  creates a missing root and refuses a symlinked root.
- `DHI_PROJECT_FOLDERS` remains only for tasks created before this contract.
  Keep its paths valid while those tasks exist.

## Manager-worker lead (unchanged)

```text
LeadDecision =
  Final { summary, detail, evidence, confidence }
  Continue { tasks, reason }
  AskWorker { worker_key, prompt }
  Stop { reason }
```

- The lead is a model call on the planner profile for now.
- `Continue` and `AskWorker` stay inside the lead loop.
- `Final` returns one `FactDraft`.
- No task board, mailbox, mission log, outcomes, or team persistence.
- Workers receive direct assignments. They cannot claim tasks or message peers.
- All assigned workers run in parallel under `max_parallel`.

## Swarm — Cline Teams

Shared state per task:

- task board with dependencies and claims
- mailbox, direct and broadcast
- mission log
- sync and async runs
- outcomes and outcome fragments
- persistent and resumable team state

The Swarm is a coordinator-agent model, not a no-tools JSON decider:

- The claimed intent is given to one coordinator agent.
- The coordinator is a normal agent loop with the domain tools plus the team
  tools.
- It calls team tools directly. There is no `review_team` runtime seam and no
  `LeadDecision` action batch in the Swarm path.
- Teammates are normal agent runs with the same team tools except spawn,
  shutdown, cleanup, and outcome finalization.
- The coordinator's one `record_fact` is the intent fact. The engine records it
  and the planner continues.
- A coordinator `team_stop` returns to the planner without a fact.

The persisted role string remains `lead` to avoid a migration; it is the
coordinator role. `TeamRole::Lead` is the coordinator, and `team_lead*` naming
in storage or old activity is documented as coordinator.

Team tools:

```text
team_spawn_teammate          team_shutdown_teammate
team_status                  team_task (create/list/claim/complete/block)
team_run_task                team_list_runs
team_await_runs              team_cancel_run
team_send_message            team_broadcast
team_read_mailbox            team_mission_log
team_create_outcome          team_attach_outcome_fragment
team_review_outcome_fragment team_finalize_outcome
team_list_outcomes           team_cleanup
team_stop
```

Permissions:

- Coordinator: every team tool.
- Teammate: every team tool except `team_spawn_teammate`,
  `team_shutdown_teammate`, `team_cleanup`, and `team_finalize_outcome`.
- Teammates may create and review outcomes; only the coordinator finalizes.
- The host enforces the `max_teammates` cap at spawn, in addition to the
  session check.

Runner internals are reused: `TeamSession` still owns board, mailbox, mission
log, outcomes, and run rows; `TeamRunner::start` loads or seeds the persisted
team; `run_worker` keeps the run-row, board-complete, board-block, and automatic
mission-log behavior.

## Scale approval

```text
swarm_scale approval:
  id, task_id, team_id,
  requested_workers, current_cap,
  reason, status,
  created_at, expires_at, decided_at
```

- Fixed N 2–64: no approval.
- Auto: coordinator may use up to 8.
- Auto above 8: create one `swarm_scale` approval. The coordinator waits.
- Allow: cap rises to requested N, maximum 64.
- Deny or expiry: cap stays 8.
- The approval is audited in activity.

## Worker count

- UI: number input 2–64 plus Auto.
- Visible only when topology is Swarm.
- N is teammates. The coordinator is extra.
- Swarm header: `N workers + coordinator`.
- `max_parallel = N` for teammates.
- Task capacity must not throttle below N.
- Fixed N is exactly N. No approval.
- Server, planner, and team runtime enforce the same N.

## Database

Migration `0023_team_state.sql`:

- `team_members`
- `team_tasks`
- `team_messages`
- `team_mission_log`
- `team_runs`
- `team_outcomes`
- `team_outcome_fragments`

Migration `0024_scale_approvals.sql`:

- `scale_approvals`

## API

```text
GET  /api/tasks/{id}/team
GET  /api/tasks/{id}/team/tasks
GET  /api/tasks/{id}/team/mailbox?box=inbox|sent
GET  /api/tasks/{id}/team/messages
GET  /api/tasks/{id}/team/mission
GET  /api/tasks/{id}/team/runs
GET  /api/tasks/{id}/team/outcomes
GET  /api/scale-approvals
POST /api/scale-approvals/{id}/decision
```

The team endpoints are read-only for the UI. Agents mutate team state through tools.
Only the scale approval is a user action.

## UI

Swarm tab:

- default: Task board
- Mailbox: Inbox and Sent
- Mission log
- Outcomes
- Runs
- Graph secondary

Task board columns: Pending, In progress, Blocked, Completed.
Task card: title, assignee, dependencies, blocked-by, status, summary.
Mailbox row: sender, recipient, subject, task, time, unread.
Mission log: kind badge, summary, evidence, next action.
Outcomes: sections, fragments, review status, finalize state.
Runs: agent, task, status, progress, result summary.

## File ownership

| Owner | Files |
|---|---|
| `backend-lead` | New lead module in `dhi-orchestrator`; lead types in `dhi-core`; lead prompt; `dhi-agent` lead module |
| `backend-team` | New team module in `dhi-orchestrator`; `dhi-core/team.rs`; `dhi-store` team repo; `0023` migration; team tools; coordinator and teammate prompts |
| `backend-integration` | `dhi-engine/src/lib.rs`; `dhi-server/src/lib.rs`; `0024` migration; scale approval repo and route |
| `frontend-create` | `web/src/app/page.tsx`; `web/src/lib/ui.ts`; `types.ts`; `api.ts`; task detail |
| `frontend-swarm` | `SwarmMessagesCard.tsx`; `swarmGraph.ts`; `workTeam.ts`; `ApprovalsClient.tsx`; new team components and tests |

Strict ownership. The main agent coordinates any cross-owner change.

## Constraints

- No commit, push, or merge without approval.
- No local cargo builds.
- No local image builds.
- Use subagents for implementation.
- Do not delete `target/`.
- Do not touch `master`.
