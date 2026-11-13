# Native macOS sandbox

DHI<sup>+</sup> runs task tools on macOS without Docker and without a VM. Every
sandboxed command runs as the **same macOS user** as the server under
`/usr/bin/sandbox-exec` against a profile in this repository.

There is no privileged setup step and nothing to install: no task user, no
ACLs, no `sudo`, no reboot, and no install script. The server only needs the
profile path in `DHI_SANDBOX_PROFILE`:

- the value is trimmed; an empty or unset value fails startup;
- a value containing `~` (including `~/...`) is rejected — no shell expansion
  is done, so write the full `/Users/<you>/...` path;
- the path must be absolute, the server canonicalizes it (`fs::canonicalize`,
  so symlinks are resolved once at startup), and it must name an existing
  regular file; otherwise startup fails with the raw value and the reason;
- the canonical path is what the backend logs and passes to `sandbox-exec`.

## Real paths, one per task

Each task gets its own real absolute host paths. No path is global, so any
number of tasks can run at the same time. The profile receives them with `-D`
parameters:

| Parameter | Meaning |
| --- | --- |
| `PROJECT` | the project root; readable, never writable |
| `WORK` | the task work folder; readable and writable |
| `SPILL` | `<WORK>/.dhi`; the host spill directory, not writable |
| `PIDFILE` | the server-owned file that records a command's session-leader PID |

Project tasks and legacy tasks differ only in where the paths point:

- **project task** — `PROJECT=<project>` and `WORK=<project>/.dhi/<task-id>`,
  so `SPILL=<WORK>/.dhi`;
- **legacy task** — there is no project folder:
  `PROJECT=WORK=<DHI_WORKSPACE_ROOT>/<task-id>`, so `SPILL=<WORK>/.dhi`.

The command is started as
`/usr/bin/sandbox-exec -f <profile> -D PROJECT=... -D WORK=... -D SPILL=... -D PIDFILE=...`.

## Guarantees

The profile gives four guarantees. They are precise, including where they
stop:

1. **Project files are read-only.** The project root is readable, but no
   write is allowed there. A task tool that writes into the project fails with
   `Operation not permitted`.
2. **Writes are confined to the task work folder, except the private
   pidfile.** Writes are allowed below `WORK`, below `/dev`, and to exactly one
   file: the server-owned `<DHI_WORKSPACE_ROOT>/pids/<task-id>.pid` named by
   `PIDFILE`. The spill directory `<WORK>/.dhi` is write- and delete-denied
   even though it lives inside `WORK`, and moving `WORK` fails because that
   would move a whole writable tree elsewhere.
3. **No read access to user file content outside the project and work
   folder.** Home directories, `~/.ssh`, browser profiles, and other host
   files cannot be read or listed. System binaries, libraries, and
   configuration that tools need (`/bin`, `/usr/bin`, `/usr/sbin`, `/sbin`,
   `/etc`, `/dev`, Homebrew, and the Command Line Tools) are readable.
   **This is not a security boundary against malicious code:** the task runs
   with your user's privileges and can `fork`/`exec`, so it can do whatever
   your user can do within what the profile allows.
4. **Ordinary commands are stopped at task end and at startup.** The wrapper
   around every sandboxed command appends its session-leader PID to
   `<DHI_WORKSPACE_ROOT>/pids/<task-id>.pid`, then runs the real command as a
   child and **waits for it instead of `exec`ing it**. Not replacing the
   wrapper keeps `dhi-pids <pidfile>` in the session leader's command line,
   and the recorded PID heads the command's whole process group. When the task
   ends, cleanup kills each verified process group and removes the pidfile; at
   server startup every leftover pidfile group is reaped. Cleanup is
   pidfile-only. A leader whose recorded command no longer contains
   `dhi-pids <pidfile>` is treated as a reused PID and is **not** killed. A
   child that deliberately calls `setsid` leaves the group and **can
   survive**. `DHI_TASK_ID` is set for diagnostics only and is never used for
   cleanup.

## Required `.env`

The native block below is the complete native configuration and matches
`.env.example` exactly. Uncomment it; the `DATABASE_URL` and
`MIGRATION_DATABASE_URL` lines are the runtime defaults and stay commented:

```dotenv
# Host file browser (Docker only): absolute host root listed by the task
# file browser. The server and the Docker daemon must see the same absolute
# path; mount it with docker-compose.fs.yml. Unset disables the file browser.
# Example: /srv/dhi/projects
# DHI_FILE_BROWSER_ROOT=/srv/dhi/projects

# Native macOS sandbox backend (no Docker, no VM). See docs/macos-native.md.
# This block is the complete native configuration: it replaces the Docker
# DHI_FILE_BROWSER_ROOT example above when the sandbox backend is selected.
# DHI_EXEC_BACKEND=sandbox
# DHI_SANDBOX_PROFILE=/Users/drakshayr/Desktop/dhi/ops/macos/dhi-task.sb
# DHI_WORKSPACE_ROOT=/Users/Shared/dhi/work
# DHI_FILE_BROWSER_ROOT=/Users/Shared/dhi/projects
```

The Docker `/srv/dhi/projects` example above is Docker-only; it is not a
native default. `DHI_SANDBOX_PROFILE` must be absolute (see above).
`DHI_FILE_BROWSER_ROOT` must already exist or be creatable, and
`/Users/Shared/dhi/projects` is preferred over `~/Desktop`, `~/Documents`, and
`~/Downloads` because those folders are subject to TCC (see Limits).

## Native services

Everything else runs natively too; Docker is not required:

- **PostgreSQL 16** — run it natively and point `DATABASE_URL` and
  `MIGRATION_DATABASE_URL` at it. The server applies migrations itself at
  startup. `scripts/migrate.sh` is Docker-only; do not use it.
- **Rust server** — build with `cargo build -p dhi-server`.
- **UI** — build the static export with
  `cd web && npm ci && NEXT_EXPORT=1 npm run build`. The server serves
  `web/out`; `scripts/dev.sh` runs the development UI instead.

## Verification

```bash
scripts/check-sandbox.sh
scripts/check-sandbox.sh /path/to/project
```

The script needs no root. It uses temporary directories, prints `PASS`,
`FAIL`, or `SKIP` per test, exits nonzero on any `FAIL`, and removes its
temporary directories. It runs two phases:

- **plain layout** — project file and mode-0600 project file are readable, the
  project is not writable, `WORK` is writable, the spill directory cannot be
  written or deleted, `~/.ssh` and the home directory are not readable or
  listable, network access works, the server-owned pidfile is writable while
  sibling files in its directory are denied, and two `sandbox-exec` processes
  run at the same time in separate work directories.
- **production layout** — `PROJECT=<project>`, `WORK=<project>/.dhi/<uuid>`,
  `SPILL=<WORK>/.dhi`. A project file is readable and a project write is
  denied; a write in `WORK` is allowed and a write in `SPILL` is denied; a hard
  link from `WORK` to a project file is denied; a symlink from `WORK` to a
  project file cannot write through it; moving `WORK` is denied; and
  `/usr/bin/sudo -n true` is denied (skipped when sudo is not installed).

A failed curl is reported as `SKIP` when the host is offline. The optional
project argument adds a read-only readability check against a real project
tree.

## Limits

- `sandbox-exec` is deprecated by Apple. It can change or disappear in a
  future macOS release. Re-run `scripts/check-sandbox.sh` after every macOS
  update.
- `file-read-metadata` is broad so tools can `stat` parent directories. File
  contents and directory listings are still denied outside the allowed paths.
- Homebrew configuration and state directories such as `/usr/local/etc` and
  `/opt/homebrew/etc` stay blocked.
- macOS has no memory limit for the sandbox. CPU time limits are not imposed
  by the profile.
- The task runs as your user, so Unix file permissions still apply: a project
  file you cannot read, the task cannot read.
- **TCC:** if the DHI server itself lacks access to `~/Desktop`, `~/Documents`,
  or `~/Downloads`, the sandboxed task inherits that denial. Prefer
  `/Users/Shared/dhi/projects`, or grant the server access in System Settings →
  Privacy & Security. This is not a new sandbox requirement.
- Network is allowed. There is no network rule and no egress filtering.
