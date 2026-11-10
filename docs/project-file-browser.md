# Project file browser

DHI<sup>+</sup> selects projects directly from the host filesystem. The UI
file browser lists host paths under `DHI_FILE_BROWSER_ROOT`. The user selects
a project folder and, when present, its `AGENTS.md` file, and the server uses
those host paths directly. There is no upload or copied snapshot.

## How a task uses the selection

1. **Browse the host root.** `GET /api/fs` lists the directories and files
   below `DHI_FILE_BROWSER_ROOT`. Use `GET /api/fs?path=<absolute-directory>`
   to list one directory under that root. Every entry carries the absolute
   host path used for selection.
2. **Select the project folder.** The selected absolute host path becomes the
   task's required `project_folder`. The server opens that directory in place.
3. **Select `AGENTS.md`.** When the task needs one, select it in the same
   browser. It must live under `DHI_FILE_BROWSER_ROOT` and be named exactly
   `AGENTS.md`. It may be inside or outside the selected project folder.
   Its absolute host path becomes `agents_md_path`, or `null` when no file is
   selected. The file is planner context, must be at most 32 KiB, and the
   server reads that exact file without searching for another `AGENTS.md`.
4. **Create the task folder.** The server creates
   `<project>/.dhi/<task-id>` for task state and outputs.
5. **Start the task container.** The project root is mounted at `/project`
   read-only and the task folder is mounted at `/work` read-write. Existing
   files in the project are unchanged. Agent writes stay under `/work` and
   appear on the host under `<project>/.dhi/<task-id>`.

A selected path is used as-is. The server never copies the project into
`DHI_FILE_BROWSER_ROOT` or into the workspace.

## Requirements

- `DHI_FILE_BROWSER_ROOT` must be an **absolute** path to a real directory.
  The server creates the directory when it is missing, but it refuses a
  symlink and requires the directory to be readable.
- The browser lists directories and regular files, skips symlinks, and
  refuses a selected path that is or contains a symlink.
- The selected project folder must be writable by the server so it can
  create `<project>/.dhi`.
- The **same absolute path** must be visible to both the `dhi-server`
  container and the Docker daemon. The task container's project bind-mount
  source is the selected host path exactly as the server sees it. There is
  no host-root remapping variable.
- The task container must be able to read the selected project folder and
  write `<project>/.dhi/<task-id>`. The Docker daemon must be able to resolve
  that same host path.

## Deploy steps

1. Choose an absolute host root and create it. The server creates a missing
   root, but creating it first lets you set ownership and permissions:

   ```bash
   export DHI_FILE_BROWSER_ROOT=/srv/dhi/projects
   sudo mkdir -p "$DHI_FILE_BROWSER_ROOT"
   ```

   Use a real directory, not a symlink. A path under the user's home also
   works; resolve it to an absolute path before putting it in `.env`.

2. Persist the root in `.env` (Compose reads `.env` automatically) or export
   it in the shell that runs Compose:

   ```dotenv
   DHI_FILE_BROWSER_ROOT=/srv/dhi/projects
   ```

3. Start the stack with the base file plus the file-system override:

   ```bash
   docker compose -f docker-compose.yml -f docker-compose.fs.yml up -d
   ```

   For the deployment path that pulls the published image:

   ```bash
   docker compose -f docker-compose.yml -f docker-compose.fs.yml pull dhi-server
   docker compose -f docker-compose.yml -f docker-compose.fs.yml up -d --no-build
   ```

   `docker-compose.fs.yml` adds exactly one volume to `dhi-server`:

   ```yaml
   services:
     dhi-server:
       volumes:
         - ${DHI_FILE_BROWSER_ROOT}:${DHI_FILE_BROWSER_ROOT}
   ```

   That is the same absolute path on both sides, read-write. Apply this
   override only when `DHI_FILE_BROWSER_ROOT` is set; an unset variable would
   produce an empty and invalid bind mount.

4. Keep the existing mounts. The override appends the file-browser root and
   does not replace anything, so the base `./workspace` mount, the Docker
   socket, and every `DHI_PROJECT_FOLDERS` same-path mount stay in effect.
   Do not remove an existing project-folder mount while a task created
   before the file browser still uses it.

5. Make the path visible to the Docker daemon:

   - **Local Linux daemon on the same host:** the host path is already the
     daemon path. No extra setup.
   - **Docker Desktop on macOS:** add the file-browser root under Settings →
     Resources → File sharing, then apply the change. Paths under an already
     shared tree need no new entry.
   - **Remote or separate daemon (`DOCKER_HOST`, another machine, or a
     different VM):** create the same absolute path on that host or VM and
     mount/share it there. If the daemon cannot resolve the selected project
     folder, task creation fails at container start even though the API
     accepted the path.

6. Verify that the browser is enabled:

   ```bash
   curl -fsS "http://127.0.0.1:8080/api/fs?path=$DHI_FILE_BROWSER_ROOT"
   ```

   Open the UI task form, browse to a project folder, select its `AGENTS.md`,
   and create the task.

## Legacy project folders

`DHI_PROJECT_FOLDERS` is no longer the task-creation selector. New tasks
choose their project folder through the file browser.

The variable and its same-path mounts remain supported for tasks created
before the file browser. Keep each mount at the same absolute path,
read-write, for as long as those tasks exist:

```yaml
services:
  dhi-server:
    volumes:
      - /srv/projects/build:/srv/projects/build
```

Removing a legacy mount does not change a stored task, but its next container
start cannot resolve the project path.

## When `DHI_FILE_BROWSER_ROOT` is unset

An unset or empty value disables the file browser:

- The base Compose file stays valid and adds no file-browser mount.
  `docker compose -f docker-compose.yml ...` works without
  `docker-compose.fs.yml`.
- `GET /api/fs` answers `404`, so the UI cannot browse host folders or select
  a project folder and `AGENTS.md` through the file browser.
- Task creation rejects a selected `project_folder` or `agents_md_path`.
- Tasks created before the file browser are unaffected while their
  `DHI_PROJECT_FOLDERS` mounts remain in place.

## Troubleshooting

- **File browser unavailable** — the server saw an empty
  `DHI_FILE_BROWSER_ROOT`. Set it in `.env` or the shell, then recreate the
  `dhi-server` container.
- **Request rejected when creating a task** — the selected path is missing,
  is outside `DHI_FILE_BROWSER_ROOT`, passes through a symlink, or the
  project folder is not a directory.
- **`AGENTS.md` rejected** — the file is not named exactly `AGENTS.md`, is
  larger than 32 KiB, is outside `DHI_FILE_BROWSER_ROOT`, or passes through a
  symlink.
- **Task container fails to mount** — the daemon does not see the same
  absolute path. Check Docker Desktop file sharing or the remote daemon's
  filesystem.
- **Permission denied under `.dhi`** — the server or daemon cannot write the
  selected project folder. Fix ownership or permissions on the host.
- **`AGENTS.md` not applied** — the task stores the selected file path. Select
  the intended file in the browser; the server does not fall back to another
  `AGENTS.md`.

## Legacy web-project tasks

The new file browser does not use `DHI_WEB_PROJECTS_ROOT`. If old
browser-folder web-project tasks still exist, keep the old
`DHI_WEB_PROJECTS_ROOT` value and its same-path mount. Otherwise their project
paths and the legacy `/api/tasks/{id}/outputs` route may not resolve. New tasks
must use the file browser.
