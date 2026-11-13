# Project file browser

DHI<sup>+</sup> selects projects directly from the filesystem. The UI file
browser lists paths under `DHI_FILE_BROWSER_ROOT`. The user selects a project
folder and, when present, its `AGENTS.md` file, and the server uses those paths
directly. There is no upload or copied snapshot.

The server and the task sandbox run as the same macOS user on the same Mac and
use the same real absolute paths. `DHI_FILE_BROWSER_ROOT` is a normal
directory on this Mac; there is no container runtime and no path remapping.

## How a task uses the selection

1. **Browse the root.** `GET /api/fs` lists the directories and files below
   `DHI_FILE_BROWSER_ROOT`. Use `GET /api/fs?path=<absolute-directory>` to list
   one directory under that root. Every entry carries the absolute path used
   for selection.
2. **Select the project folder.** The selected absolute path becomes the task's
   required `project_folder`. The server opens that directory in place.
3. **Select `AGENTS.md`.** When the task needs one, select it in the same
   browser. It must live under `DHI_FILE_BROWSER_ROOT` and be named exactly
   `AGENTS.md`. It may be inside or outside the selected project folder. Its
   absolute path becomes `agents_md_path`, or `null` when no file is selected.
   The file is planner context and the same text travels with the work, must be
   at most 32 KiB, and the server reads that exact file without searching for
   another `AGENTS.md`.
4. **Create the task folder.** The server creates `<project>/.dhi/<task-id>`
   for task state and outputs.
5. **Start the task sandbox.** The task sandbox reads the project root and
   writes only in the task folder. Existing files in the project are
   unchanged. Agent writes stay in the task folder and appear at
   `<project>/.dhi/<task-id>`. See [macos-native.md](macos-native.md).

A selected path is used as-is. The server never copies the project into
`DHI_FILE_BROWSER_ROOT` or into the workspace.

## Requirements

- `DHI_FILE_BROWSER_ROOT` must be an **absolute** path to a real directory.
  The server creates the directory when it is missing, but it refuses a
  symlink and requires the directory to be readable.
- The browser lists directories and regular files, skips symlinks, and refuses
  a selected path that is or contains a symlink.
- The selected project folder must be writable by the server so it can create
  `<project>/.dhi`.
- The task sandbox uses the selected path exactly as the server sees it. There
  is no host-root remapping variable.
- The task sandbox must be able to read the selected project folder and write
  `<project>/.dhi/<task-id>`, which holds when the project lives under a path
  the sandbox profile allows and your user can access it.

## Setup

1. Choose an absolute root and create it. The server creates a missing root,
   but creating it first lets you set ownership and permissions:

   ```bash
   mkdir -p /Users/Shared/dhi/projects
   ```

   Use a real directory, not a symlink. A path under your home also works, but
   avoid `~/Desktop`, `~/Documents`, and `~/Downloads`: macOS TCC blocks the
   sandboxed task there when the server itself lacks access. Resolve any path
   to its absolute form before putting it in `.env`.

2. Persist the root in `.env` (mode 600) or export it in the shell that starts
   the server:

   ```dotenv
   DHI_FILE_BROWSER_ROOT=/Users/Shared/dhi/projects
   ```

3. Start the server:

   ```bash
   cargo build -p dhi-server
   ./target/debug/dhi-server
   ```

   `scripts/dev.sh` starts the API and the Next development UI instead.

4. Verify that the browser is enabled:

   ```bash
   curl -fsS "http://127.0.0.1:8080/api/fs?path=$DHI_FILE_BROWSER_ROOT"
   ```

   Open the UI task form, browse to a project folder, select its `AGENTS.md`,
   and create the task.

## Legacy project folders

`DHI_PROJECT_FOLDERS` is no longer the task-creation selector. New tasks
choose their project folder through the file browser.

The variable remains supported for tasks created before the file browser. Its
paths are absolute paths on this Mac. Keep each path in place, readable, for as
long as those tasks exist:

```dotenv
DHI_PROJECT_FOLDERS=/Users/Shared/dhi/projects/build
```

Removing a path does not change a stored task, but its next run cannot resolve
the project path.

## When `DHI_FILE_BROWSER_ROOT` is unset

An unset or empty value disables the file browser:

- `GET /api/fs` answers `404`, so the UI cannot browse folders or select a
  project folder and `AGENTS.md` through the file browser.
- Task creation rejects a selected `project_folder` or `agents_md_path`.
- Tasks created before the file browser are unaffected while their
  `DHI_PROJECT_FOLDERS` paths remain valid.

## Troubleshooting

- **File browser unavailable** — the server saw an empty
  `DHI_FILE_BROWSER_ROOT`. Set it in `.env` or the shell, then restart the
  `dhi-server` process.
- **Request rejected when creating a task** — the selected path is missing,
  is outside `DHI_FILE_BROWSER_ROOT`, passes through a symlink, or the project
  folder is not a directory.
- **`AGENTS.md` rejected** — the file is not named exactly `AGENTS.md`, is
  larger than 32 KiB, is outside `DHI_FILE_BROWSER_ROOT`, or passes through a
  symlink.
- **Project under `~/Desktop`, `~/Documents`, or `~/Downloads` is denied** —
  macOS TCC blocks the sandboxed task when the server lacks access to that
  folder. Move the project under `/Users/Shared/dhi/projects`, or grant the
  server access in System Settings → Privacy & Security.
- **Permission denied under `.dhi`** — the server cannot write the selected
  project folder. Fix ownership or permissions on the selected folder.
- **`AGENTS.md` not applied** — the task stores the selected file path. Select
  the intended file in the browser; the server does not fall back to another
  `AGENTS.md`.
