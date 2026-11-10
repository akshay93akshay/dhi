# Deploying browser-folder web projects

Pure-web projects let a user start a task from a folder in their browser
instead of a project folder that already exists on the Docker host. The
browser still works against a real host directory, because DHI keeps an
uploaded copy under `DHI_WEB_PROJECTS_ROOT` and task containers bind-mount
that copy.

This page covers the deployment side. The UI and API are already built in.

## How the pure-web model works

1. **The browser holds the handle.** The user picks a folder with the File
   System Access API (Chrome or Edge). The browser stores that directory
   handle in IndexedDB. The handle itself is never uploaded; the server only
   ever sees copied files.
2. **The snapshot is uploaded.** Creating a web project makes the server
   create `<root>/<id>`, where `<root>` is `DHI_WEB_PROJECTS_ROOT` and `<id>`
   is the project UUID. The UI then uploads the folder contents, one file per
   request, preserving relative paths under `<root>/<id>`.
3. **The task uses that path.** When a task selects the web project, the
   server records `<root>/<id>` as the task's `project_folder` and derives the
   task folder `<root>/<id>/.dhi/<task-id>`. The task container mounts the
   project at `/project` read-only and the task folder at `/work`
   read-write, exactly like a configured host project folder.
4. **Outputs land under `.dhi/<task-id>`.** Agent writes stay in `/work`, so
   the host sees them under `<root>/<id>/.dhi/<task-id>`. The UI downloads
   them through `GET /api/tasks/{id}/outputs` and writes them back through the
   saved browser folder handle, nested under `.dhi/<task-id>` (paths that
   already start with `.dhi/` keep their relative path).

The uploaded copy is the working copy for the task. Editing the original
browser folder does not affect a task that has already started. Sync writes
the task results back into the browser folder; it does not mirror the whole
project back.

## Limits and exclusions

- **8 MB per file.** A larger file is rejected by the server and by the
  browser before upload.
- **256 MB per project snapshot.** The server enforces the cap on the derived
  project tree after each write. Upload again to replace files, or start a
  new project for a larger tree.
- **`.dhi` is excluded.** The server-managed `.dhi` directory does not count
  toward the snapshot total, and upload paths cannot target `.dhi`.
- The browser skips `.git`, `.dhi`, `node_modules`, `target`, `.next`,
  `dist`, `build`, `out`, and `.cache` while collecting a snapshot.

`AGENTS.md` is uploaded with the snapshot like any other file. As with host
project folders, the planner reads only the selected project root's
`AGENTS.md`, capped at 32 KiB.

## Requirements

- `DHI_WEB_PROJECTS_ROOT` must be an **absolute** path.
- The directory must exist or be creatable by the server, be writable by the
  server, and not be a symlink. Startup fails when a configured root is
  invalid.
- The **same absolute path** must be visible to both the `dhi-server`
  container and the Docker daemon. There is no host-root remapping variable
  for web projects. Each task's bind-mount source is `<root>/<id>` exactly as
  the server sees it.
- A browser with the File System Access API (Chrome or Edge) is required for
  upload and result sync.

## Deploy steps

1. Choose an absolute host root, create it, and make it writable by the
   server's user:

   ```bash
   export DHI_WEB_PROJECTS_ROOT=/srv/dhi/web-projects
   sudo mkdir -p "$DHI_WEB_PROJECTS_ROOT"
   ```

   Use a real directory, not a symlink. A path under the user's home also
   works, for example `~/dhi-web-projects`; resolve it to an absolute path
   before putting it in `.env`.

2. Persist the root in `.env` (Compose reads `.env` automatically) or export
   it in the shell that runs Compose:

   ```dotenv
   DHI_WEB_PROJECTS_ROOT=/srv/dhi/web-projects
   ```

3. Start the stack with the base file plus the web override:

   ```bash
   docker compose -f docker-compose.yml -f docker-compose.web.yml up -d
   ```

   For the deployment path that pulls the published image:

   ```bash
   docker compose -f docker-compose.yml -f docker-compose.web.yml pull dhi-server
   docker compose -f docker-compose.yml -f docker-compose.web.yml up -d --no-build
   ```

   `docker-compose.web.yml` adds exactly one volume to `dhi-server`:

   ```yaml
   services:
     dhi-server:
       volumes:
         - ${DHI_WEB_PROJECTS_ROOT}:${DHI_WEB_PROJECTS_ROOT}
   ```

   That is the same absolute path on both sides, read-write. Use this
   override only when `DHI_WEB_PROJECTS_ROOT` is set; an unset variable would
   produce an empty and invalid bind mount.

4. Keep the existing mounts. The override appends the web root and does not
   replace anything, so the base `./workspace` mount, the Docker socket, and
   every `DHI_PROJECT_FOLDERS` same-path mount stay in effect. Do not remove
   the existing host-folder mount used by this deployment, the `Build` folder
   under the home directory (`${HOME}/Desktop/Build`), or any other project
   folder you have mounted.

5. Make the path visible to the Docker daemon:

   - **Local Linux daemon on the same host:** the host path is already the
     daemon path. No extra setup.
   - **Docker Desktop on macOS:** the daemon runs in a Linux VM. Add the
     web-project root under Settings → Resources → File sharing, then apply
     the change. Paths under an already shared tree need no new entry. The
     existing `Build` share is separate and remains unchanged.
   - **Remote or separate daemon (`DOCKER_HOST`, another machine, or a
     different VM):** create the same absolute path on that host or VM and
     mount/share it there. If the daemon cannot resolve `<root>/<id>`, task
     creation fails at container start even though the API accepted the
     project.

6. Verify that the feature is enabled:

   ```bash
   curl -fsS http://127.0.0.1:8080/api/web-projects
   ```

   With the root configured, this returns a JSON envelope such as
   `{"projects":[]}`. Open the UI and create a web project from a browser
   folder.

## When `DHI_WEB_PROJECTS_ROOT` is unset

An unset or empty value disables web projects:

- `GET /api/web-projects` answers `404`, so the UI treats the feature as
  unavailable.
- Creating a task that names a web project answers `400`.
- No web-project mount is added by the base Compose file. `docker compose`
  without `docker-compose.web.yml` remains valid with the variable unset.

This is the default. Host project folders and legacy workspace tasks are
unaffected.

## Troubleshooting

- **`404` on `/api/web-projects`** — the server saw an empty
  `DHI_WEB_PROJECTS_ROOT`. Set it in `.env` or the shell, then recreate the
  `dhi-server` container.
- **`400` when creating a task** — web projects are disabled, or the named
  project does not exist.
- **Startup fails on the root** — the path is relative, contains `..`, is a
  symlink, is not a directory, or is not writable by the server.
- **Task container fails to mount** — the daemon does not see the same
  absolute path. Check Docker Desktop file sharing or the remote daemon's
  filesystem.
- **Upload rejected at 8 MB or 256 MB** — the file or derived project tree is
  over the limit. `.dhi` does not count toward either cap.
- **Result sync unavailable** — use Chrome or Edge and keep the original
  folder handle permission. The user may need to re-grant access after a
  browser restart.
