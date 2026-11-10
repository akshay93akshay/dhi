# DHI<sup>+</sup>

**Dynamic Heuristic Intelligence<sup>+</sup>**

DHI<sup>+</sup> is the team-execution upgrade to DHI. A goal still becomes a task and a plan, but the planner can choose a team, not only one worker. The team runs through a planner-selected topology, aggregates its work into one deterministic fact, and proves the goal with that fact. LLMs reason, tools act, PostgreSQL remembers, and the guard checks every action.

## DHI vs DHI<sup>+</sup>

| | DHI | DHI<sup>+</sup> |
| --- | --- | --- |
| Execution | One planner, one worker. | Planner-selected `manager_worker` or `swarm`. |
| Fact | One planner, one worker, one fact. | One deterministic aggregated fact per intent. |
| Capacity | Engine round and no-progress budgets. | Visible, extendable capacity ledger; exhaustion pauses and extension resumes. |
| UI | Individual work view. | Team UI for team runs, capacity, and swarm messages. |

## Core architecture

```mermaid
flowchart TD
    G[Goal] --> P[Planner]
    P -->|add intent + IntentPlan| T{Topology}

    T -->|manager_worker| M[Manager selects workers]
    M --> W[Workers]
    T -->|swarm| S[Coordinator agent + shared team]
    S --> W

    W --> GU[Guard + audit]
    GU --> R{Worker results}

    R -->|success or partial success| AG[Deterministic aggregate]
    AG --> F[One confirmed fact]
    F --> PR[Planner reviews fact]

    R -->|all workers failed| RJ[Rejected result + team_failed]
    RJ --> PF[Planner reviews failure]

    PR -->|add next intent| P
    PR -->|prove goal| DONE[Goal met; task done]

    PF -->|retry with new plan or higher max_turns| P
    PF -->|give up| STOP[Goal abandoned; task stops]

    C[Capacity ledger] -. pause / resume .-> P
    H[Host guards: no-progress, rounds] -. stop .-> P
```

A zero-success team run becomes a rejected result and a `team_failed` row.
The planner reviews that failure and can retry with a different plan or a
larger worker `max_turns`, change approach, or give up. Host guards still
cap the loop.

A DHI<sup>+</sup> intent still follows the DHI lifecycle. The difference is the execution layer: the planner can expand one intent into a team, and the engine still records one fact.

## DHI<sup>+</sup> orchestration

The planner selects the topology for each intent: `single`, `manager_worker`, or `swarm`. The team topologies, `manager_worker` and `swarm`, are planner-selected. `manager_worker` uses a manager to choose workers and subtasks within the planner's limits, then runs independent workers in parallel. `swarm` is the Cline Teams model: one coordinator agent receives the claimed intent and works with domain tools plus team tools over a shared task board, mailbox, mission log, outcome fragments, and sync/async runs. The coordinator calls tools directly; there is no JSON decision loop in the Swarm path. Its one `record_fact` becomes the intent fact. Teammates are normal agent runs with the same team tools except spawn, shutdown, cleanup, and outcome finalization. A `team_stop` returns to the planner without a fact.

The persisted team role string remains `lead` so existing state needs no migration; `TeamRole::Lead` is the coordinator role. Manager-worker keeps its separate no-tools lead loop and `LeadDecision` contract.

Swarm team tools:

```text
team_spawn_teammate  team_shutdown_teammate  team_status
team_task            team_run_task           team_list_runs
team_await_runs      team_cancel_run         team_send_message
team_broadcast       team_read_mailbox       team_mission_log
team_create_outcome  team_attach_outcome_fragment
team_review_outcome_fragment  team_finalize_outcome
team_list_outcomes   team_cleanup            team_stop
```

The coordinator may call every tool. A teammate gets the same set except
`team_spawn_teammate`, `team_shutdown_teammate`, `team_cleanup`, and
`team_finalize_outcome`; teammates can create and review outcomes but not
finalize them. The host enforces the `max_teammates` cap at spawn.

Each task has a visible capacity ledger: `max_rounds`, `max_model_calls`, `max_wall_secs`, and `max_parallel_workers`. Omitted limits use the engine defaults. `POST /api/tasks` accepts the ledger at creation, `POST /api/tasks/{id}/capacity` extends it, and the Work view shows and edits it. Exhaustion pauses the task (never fails it); an extension resumes a paused task. Swarm broadcasts are durable and readable at `GET /api/tasks/{id}/swarm-messages`.

The host keeps compiled engine safety ceilings for rounds and no-progress
rounds. They are internal defaults, not task settings.

- DHI<sup>+</sup> is the default line; legacy DHI is preserved as branch `DHI` and tag `DHI-v1.0.1`.
- CI builds the container image on GitHub Actions; no local Docker build is required.

## Historical DHI baseline

Legacy DHI is the historical single-worker baseline, preserved unchanged on the
branch and tag above. The shared foundations below still apply to DHI<sup>+</sup>.

## Shared DHI foundations

The unit of work is a task with a goal. The planner can add one intent, prove one open goal with a confirmed fact, or give up on a goal. Each intent is executed by a worker that records facts through tools. Every tool call is recorded, evaluated against guard rules, and then allowed, denied, or held for human approval. PostgreSQL is the system of record for tasks, goals, intents, facts, results, approvals, tool calls, agent runs, and activity.

## Features

- **Task lifecycle** — create a task from a goal with optional capacity and profile pins, follow its planner/worker loop, stop or delete it, and restart recovery for unfinished work. Adding a goal to a finished task reopens it.
- **Graph and work views** — each task has a work graph and a work view, with its activity log and live activity stream.
- **Approvals** — a guard rule may return `ask`. The call waits for a human decision or expires on its approval deadline.
- **MCP tools** — standard `mcpServers` configuration, guarded tool names of the form `mcp__<server>__<tool>`, live server management, refresh, reload, and deferred schemas above a tool threshold.
- **Web tools** — `web_search`, `web_read`, and `web_research` behind one provider router with fallback across TinyFish, Parallel, and Firecrawl. `web_read` also handles PDF and DOCX documents.
- **Coding tools** — `read`, `write`, `edit`, `search`, `list`, and `shell` run in a task container created through the host Docker socket and rooted at the workspace.
- **Persistent terminals** — `shell_open`, `shell_send`, `shell_read`, `shell_close`, and `shell_list` keep a long-lived shell in the task container created through the host Docker socket.
- **Skills** — `skills/<name>/SKILL.md` instruction packs loaded through the guarded `load_skill` tool, with role visibility.
- **Custom tools** — file-defined tools backed by HTTP calls or direct commands, registered behind the same guard.
- **Profiles and pool** — OpenAI-compatible and Anthropic-format LLM profiles, active planner and worker profiles, an ordered failover pool with per-profile breakers, and optional per-task profile pins.
- **Specialists** — named sub-agents with scoped tool lists and turn limits, used for delegation.
- **Tool control** — one catalog for every tool source, with enablement and role visibility.
- **Embedded UI** — the static Next.js export is served by the Rust process on the same origin as the API.

## Architecture

One Rust process, `dhi-server`, serves the JSON API under `/api` and the exported web UI. PostgreSQL is the only durable state store. Docker is used for the task containers that host coding tools and persistent terminals.

Workspace crates:

- `dhi-core` — domain types, task and intent state machines, graph store traits
- `dhi-store` — PostgreSQL schema, store implementation, management tables
- `dhi-engine` — planner/worker task loop and proof-based completion
- `dhi-orchestrator` — manager-worker and swarm runners, plan validation, deterministic aggregation
- `dhi-guard` — allow, deny, and ask policy plus the guarded tool executor
- `dhi-agent` — Rig runtime, prompts, LLM profiles, failover pool, specialists
- `dhi-exec` — coding tools and per-task containers
- `dhi-pty` — persistent terminal sessions inside task containers
- `dhi-followups` — side-question snapshots and task reopening rules
- `dhi-server` — Axum API, static UI hosting, wiring, and the binary
- `dhi-testkit` — in-memory store and test fixtures
- `dhi-mcp` — MCP configuration model and `dhi mcp` CLI
- `dhi-mcp-runtime` — guarded MCP connections, discovery, registration, dispatch
- `dhi-tool-source` — registration seam for tool sources
- `dhi-tool-catalog` — read model across every tool source
- `dhi-tool-control` — enablement, role visibility, usage
- `dhi-skills` — skill catalog and the `load_skill` tool
- `dhi-custom-tools` — file-defined guarded HTTP and command tools
- `dhi-web` — provider-neutral web search, read, and research tools
- `dhi-mcp-upgrade` — MCP visibility, deferred schemas, transport, and reload planning

The web app lives in `web/` (Next.js, React, Tailwind). Operational files live in `ops/` (prompts, specialists, MCP examples, custom-tool examples). Database roles and grants live in `docker/initdb/` and `docker/grants/`.

Every model tool call passes through one `GuardedExecutor`: record, evaluate the current policy snapshot, allow/deny/ask, then audit. The backend chain is persistent terminals → coding tools → web tools → skills → custom tools → MCP → the closed sources delegate. Unknown tool names fail closed.

### Docker socket

The DHI<sup>+</sup> container mounts the host Docker socket and includes the Docker CLI. It starts one task container per task through that socket. Task containers do not get the socket. The DHI<sup>+</sup> container can control Docker on the host; do not use this on a host with other important services without isolation.

## Requirements

- Rust 1.99.1 (pinned in `rust-toolchain.toml`)
- Node.js 22
- Docker with Compose v2
- PostgreSQL 16 (the compose file uses `postgres:16-alpine`)
- Optional: `uv`/`uvx` on the host for the chess MCP server

## Quick start with Docker

```bash
export DHI_IMAGE=ghcr.io/akshay93akshay/dhi:sha-<short>
docker compose pull dhi-server
docker compose up -d --no-build
```

- UI and API: http://127.0.0.1:8080
- Chess board: http://127.0.0.1:8090
- Health: `curl -fsS http://127.0.0.1:8080/api/health`

Copy `.env.example` to `.env` if you need LLM or web provider keys, and authenticate to GHCR first if you have not already (see [Container image](#container-image)). The published image contains the static UI; Compose starts PostgreSQL and `dhi-server`. `scripts/dhi-health.sh` checks the API, UI, PostgreSQL, disk space, and the optional chess board.

`docker compose up --build` is only for local image development, not the deployment path.

## Container image

GitHub Actions builds the container image on every push and pull request. It pushes to GHCR only on `master` and `v*` tags; feature-branch and pull-request builds are validation-only.

- Registry: `ghcr.io/akshay93akshay/dhi`
- Tags: `sha-<short>` on every master build and the tag name on each `v*` release, such as `v1.0.0`. There is no `latest` tag.
- The package is private. Authenticate with a GitHub token that has only `read:packages`.

Pull on a server:

```bash
echo "$GHCR_TOKEN" | docker login ghcr.io -u akshay93akshay --password-stdin
export DHI_IMAGE=ghcr.io/akshay93akshay/dhi:sha-abc1234
docker compose pull dhi-server
docker compose up -d --no-build
```

To roll back, set `DHI_IMAGE` to the previous `sha-...` tag, then pull and start again. The image contains the Rust server, the static UI, `ops/`, the Docker CLI, and the pinned chess MCP runtime; it has no Rust or Node compiler.

For a final no-cache build, run the `fresh-image` workflow manually. It builds
without BuildKit cache, pulls fresh base images, publishes `fresh-<short-sha>`,
and verifies the pulled image. Use this for final deployment checks.

## Local development

```bash
docker compose up -d postgres
scripts/migrate.sh
scripts/dev.sh
```

`scripts/dev.sh` runs the API on http://127.0.0.1:8080 and the Next.js development server on http://127.0.0.1:3000. The development server proxies `/api` to port 8080.

To run the embedded static build instead:

```bash
cd web && npm ci && NEXT_EXPORT=1 npm run build
cd ..
cargo build -p dhi-server
scripts/dhi-service.sh install && scripts/dhi-service.sh start
```

The exported files are written to `web/out`, which the server serves on http://127.0.0.1:8080. Set `DHI_WEB_DIR` to serve an export from another directory. PostgreSQL must be running and migrated before the service starts.

### Build cache

Keep the Rust build cache between runs:

- Do not delete `target/`.
- Trim selectively with `cargo sweep --time 30` or `cargo clean -p <crate>`.
- Use `cargo check -p <crate>` for daily work.
- Run full `cargo test --workspace` and full clippy only before push.
- CI caches Rust dependencies now. The first run after the change is cold; the second run shows the benefit.

## Configuration

`.env.example` lists the runtime settings. Copy it to `.env` (ignored by Git) and fill in only the values you need. Docker Compose reads `.env` automatically.

Database roles:

- `dhi_owner` — migrations and schema owner
- `dhi_app` — runtime role
- `dhi_test` — test databases

Main settings:

- `DATABASE_URL` — runtime database connection
- `MIGRATION_DATABASE_URL` — migration connection
- `DHI_BIND` — API bind address, default `127.0.0.1:8080`
- `DHI_WEB_DIR` — static UI directory; when unset, `web/out` is used if present
- `DHI_WORKSPACE_ROOT` — workspace root visible to the server; defaults to `workspace`
- `DHI_WORKSPACE_HOST_ROOT` — host path of that root, used as the bind-mount source for task containers; set it when the server runs in a container
- `DHI_PROJECT_FOLDERS` — comma-separated absolute paths of project folders; each must be mounted into the server at the same absolute path
- `DHI_WEB_PROJECTS_ROOT` — absolute root for browser-folder web projects; unset disables them, and the server and Docker daemon must see the same path
- `DHI_MCP_CONFIG` — MCP config path; when unset, the nearest `ops/mcp.json`
- `DHI_LLM_BASE_URL`, `DHI_LLM_API_KEY`, `DHI_LLM_PLANNER_MODEL`, `DHI_LLM_WORKER_MODEL` — LLM endpoint and models; other OpenAI-compatible endpoints work
- `TINYFISH_API_KEY`, `PARALLEL_API_KEY`, `FIRECRAWL_API_KEY`, `DHI_WEB_PROVIDER` — web tool providers

### Project folders

Set `DHI_PROJECT_FOLDERS` to a comma-separated list of absolute paths. Mount each folder into `dhi-server` at the same absolute path, read-write (for example `-v /path:/path` in `docker run`, or the equivalent compose volume), so the server and the task containers it creates resolve the same source. Each path must exist, be a directory, not be a symlink, and be writable by the server.

DHI<sup>+</sup> creates `<project>/.dhi/<task-id>/` for each task in a project folder. The task container mounts the project root at `/project` read-only and the task folder at `/work` read-write. One configured folder is selected per task.

`AGENTS.md` is read from the selected project root only. It is planner-only: no global or nested file is read, and the file is capped at 32 KiB.

### Browser-folder web projects

Set `DHI_WEB_PROJECTS_ROOT` to one absolute directory to enable browser-folder projects. The server validates it at startup: it must exist or be creatable, be writable by the server, and not be a symlink. The same absolute path must be visible to the server process and to the Docker daemon, because each project's folder, `<root>/<id>`, is passed to the existing project bind mount unchanged. There is no host-root remapping variable for this root; mount it into `dhi-server` at the same path, read-write, and make that path meaningful to the daemon (for example with the Docker socket's host filesystem visible under the same path).

When the variable is unset, web projects are disabled: `GET /api/web-projects` answers `404` so the UI treats the feature as unavailable, and creates or tasks that name a web project answer `400`. Uploads are capped server-side at 8 MB per file and 256 MB for the snapshot tree; the server-generated `.dhi` directory is excluded from snapshot counts and the total cap. Upload paths are relative and must not target `.dhi`. `GET /api/tasks/{id}/outputs` reads the task's `.dhi/<task-id>` folder from the web project, skips symlinks, returns UTF-8 as text and other bytes as base64, and excludes files over its per-file or total response cap. Output reads revalidate that the task folder is a real directory below the configured root before reading.

On first boot, DHI<sup>+</sup> seeds guard rules plus LLM profiles and settings when an LLM key is configured. After that, PostgreSQL is the source of truth and profiles and rules are managed from the settings UI or API.

Never put secrets in Git. Keep local secret files at mode 600: `.env`, `ops/mcp.json`, `config/dhi.config.json`, and `web.env`.

## MCP and chess

`ops/mcp.example.json` is the portable example. On a host, copy it to `ops/mcp.json` (ignored by Git) and edit that file. The Docker image has its own `ops/mcp.docker.json`, copied to `/app/ops/mcp.json` at build time.

The example starts the chess MCP server through `uvx` and patches its dashboard to listen on `0.0.0.0:8090`, so the board is reachable from the host and through Docker port forwarding. Host chess requires `uv`/`uvx` on `PATH`. The chess board is served at http://127.0.0.1:8090.

Keep `ops/mcp.json` local. `ops/mcp.example.json` and `ops/mcp.docker.json` are safe to commit.

## Commands

| Command | Purpose |
| --- | --- |
| `scripts/check.sh` | Start PostgreSQL, then run rustfmt, clippy, workspace tests, web tests, and the static web build. `SKIP_DB=1` and `SKIP_WEB=1` skip those parts. |
| `scripts/test.sh` | Start PostgreSQL, run the workspace tests, and build the static web export. |
| `scripts/backup.sh [dir]` | Write a compressed logical backup of the database to `backups/` or the given directory. |
| `scripts/restore-test.sh [dump]` | Restore a backup into a throwaway database and compare row counts. |
| `scripts/dhi-health.sh [--api-only]` | Read-only check of API, UI, PostgreSQL, disk space, and the optional chess board. |
| `scripts/check-secrets.sh` | Read-only check of secret file modes and secret material in the repository. |
| `scripts/check-paths.sh` | Read-only check for machine-specific absolute paths in product files. |
| `scripts/check-stale.sh` | Read-only check for stale development-language markers in product files. |
| `scripts/target-gc.sh [--apply]` | Prune stale workspace build artifacts from `target/`; dry run by default. |
| `scripts/migrate.sh` | Apply migrations twice, apply runtime grants, and verify the activity lock. |
| `scripts/dev.sh` | Start PostgreSQL, migrate, then run the API on 8080 and Next dev on 3000. |
| `scripts/dhi-service.sh install\|start\|stop\|restart\|status` | Install and manage the background server; also manages web provider keys. |

## API overview

All API routes are under `/api`. Most return JSON; `/api/stream` is a server-sent event stream. The static UI is the fallback for non-API paths.

- **Health** — `GET /api/health`
- **Tasks** — `GET|POST /api/tasks`; `GET|DELETE /api/tasks/{id}`; `POST /api/tasks/{id}/stop`; bulk `POST /api/tasks/stop` and `POST /api/tasks/delete`; `GET /api/tasks/{id}/work`, `/activity`, `/usage`, and `/outputs`
- **Web projects** — `GET|POST /api/web-projects`; `GET /api/web-projects/{id}`; `PUT /api/web-projects/{id}/files?path=…`
- **Goals** — `POST /api/tasks/{id}/goals`
- **Side questions** — `GET|POST /api/tasks/{id}/side-questions`
- **Approvals** — `GET /api/approvals`; `POST /api/approvals/{id}/decision`
- **Activity** — `GET /api/stream`
- **Settings and LLM profiles** — `GET|PUT /api/settings`; `GET|POST /api/llm/profiles`; `DELETE /api/llm/profiles/{id}`; `POST /api/llm/profiles/active`; `GET /api/llm/models`; `GET /api/llm/pool/status`; `POST /api/llm/pool/reset`
- **Guard rules** — `GET|POST /api/intercept/rules`; `PUT|DELETE /api/intercept/rules/{id}`; `POST /api/intercept/rules/{id}/toggle`
- **Tools** — `GET /api/tools/catalog`; `GET|POST /api/tools/control`
- **MCP** — `GET|POST /api/mcp/servers`; `PUT|PATCH|DELETE /api/mcp/servers/{name}`; `GET /api/mcp/servers/{name}/tools`; `POST /api/mcp/servers/{name}/refresh`; `POST /api/mcp/reload`
- **Web providers** — `GET /api/web/providers`; `PUT /api/web/providers/{provider}`; `DELETE /api/web/providers/{provider}/key`; `POST /api/web/providers/{provider}/test`; `POST /api/web/providers/reload`

## Security

DHI<sup>+</sup> has no login or authentication layer. Keep the default localhost bind, or put the server behind a protected tunnel or reverse proxy. Do not expose port 8080 directly to an untrusted network.

- Keep `.env`, `ops/mcp.json`, `config/dhi.config.json`, and other key files out of Git and at mode 600.
- `ops/mcp.json` is ignored. The example and Docker configs contain no secrets.
- Use the database roles as designed: `dhi_owner` for migrations and `dhi_app` for runtime. The runtime role cannot modify or delete activity rows.
- The guard defaults to deny. Rules can allow, deny, or require approval, and every decision is recorded.
- Coding tools and persistent terminals run in task containers created through the host Docker socket, with the workspace as the only writable mount.
- The compose passwords are for local development. Use distinct strong credentials on any shared host.

## Troubleshooting

- **Missing `web/out`** — the server logs a warning and serves the API only. Build the export with `cd web && npm ci && NEXT_EXPORT=1 npm run build`, or point `DHI_WEB_DIR` at a directory that contains `index.html`.
- **Missing `uvx`** — host chess fails to start. Install `uv`, which provides `uvx`, or use the Docker image, which has the chess server installed at `/opt/chess`.
- **PostgreSQL is unhealthy** — check `docker compose ps`, then run `docker compose exec postgres pg_isready -U dhi_owner -d dhi`. Re-run `scripts/migrate.sh` if the schema is missing.
- **Port conflicts** — DHI<sup>+</sup> uses 8080 (API/UI), 8090 (chess), 5432 (PostgreSQL), and 3000 (Next dev). Set `DHI_BIND` or the compose port mappings, or stop the process using the port.
- **`DHI_WEB_DIR` does not take effect** — relative paths resolve from the server's working directory, and the directory must contain `index.html`. An empty value falls back to `web/out` when it exists.
