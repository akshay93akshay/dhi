# Native macOS sandbox

DHI<sup>+</sup> runs task tools on macOS with no container runtime and no VM.
Every sandboxed command runs as the **same macOS user** as the server under
`/usr/bin/sandbox-exec` against a profile in this repository.

There is no privileged setup step and nothing extra to install: no task user,
no ACLs, no `sudo`, no reboot, and no install script. The server only needs the
profile path in `DHI_SANDBOX_PROFILE`, which is required:

- the value is trimmed; an empty or unset value fails startup;
- a value containing `~` (including `~/...`) is rejected — no shell expansion
  is done, so write the full `/Users/<you>/...` path;
- the path must be absolute, the server canonicalizes it (`fs::canonicalize`,
  so symlinks are resolved once at startup), and it must name an existing
  regular file; otherwise startup fails with the raw value and the reason;
- the canonical path is what the backend logs and passes to `sandbox-exec`.

The profile in this repository is `ops/macos/dhi-task.sb`.

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

The block below is the complete native configuration and matches
`.env.example`:

```dotenv
# Native macOS task sandbox. See docs/macos-native.md.
# DHI_SANDBOX_PROFILE is the profile in this checkout. Replace <you>; no
# ~ expansion is done.
DHI_SANDBOX_PROFILE=/Users/<you>/Desktop/dhi/ops/macos/dhi-task.sb
DHI_WORKSPACE_ROOT=/Users/Shared/dhi/work
DHI_FILE_BROWSER_ROOT=/Users/Shared/dhi/projects
```

`DHI_SANDBOX_PROFILE` points at `ops/macos/dhi-task.sb` inside this
checkout. There is no install copy; do not copy the profile elsewhere. Replace
`<you>` with your macOS user name, and adjust the path if the checkout lives
somewhere other than `~/Desktop/dhi`. The value must be absolute, and `~` is
not expanded.

`DHI_FILE_BROWSER_ROOT` must already exist or be creatable, and
`/Users/Shared/dhi/projects` is preferred over `~/Desktop`, `~/Documents`, and
`~/Downloads` because those folders are subject to TCC (see Limits).

## PostgreSQL

PostgreSQL 16 runs natively on the same Mac. The server applies the app
migrations itself at startup, so the database, the roles, and the grants must
exist before the first server start.

The roles are `dhi_owner` (schema owner and migrations), `dhi_app` (runtime),
and `dhi_test` (test databases). The role and grant SQL lives in `ops/postgres/`:

- `ops/postgres/01-roles.sql`
- `ops/postgres/02-grants.sql`

`scripts/migrate.sh` connects with `psql` as `dhi_owner`, applies both files in
order, and verifies that `dhi_app` cannot update or delete `activity`. It never
applies the app schema.

### Install PostgreSQL 16

Use a prebuilt PostgreSQL 16 build. Both options ship complete `bin/`
directories, so nothing compiles on this Mac:

- **Postgres.app** — download PostgreSQL 16 from postgresapp.com, move it to
  `/Applications`, and let it initialize a cluster on first launch.
- **EDB PostgreSQL 16 binaries** — download the macOS PostgreSQL 16 package
  from enterprisedb.com and unpack it, for example to
  `/Users/Shared/pgsql`. There is no installer step in this guide; every step
  below uses paths inside the unpacked tree.

Then point `PG_BIN` at that directory and put it on `PATH` for the shell that
runs these steps. Every command below uses `"$PG_BIN/..."`, so it works the
same for both options:

```sh
export PG_BIN="/Applications/Postgres.app/Contents/Versions/16/bin"
# or: export PG_BIN="/path/to/pgsql/bin"
export PATH="$PG_BIN:$PATH"
```

Use `/path/to/pgsql/bin` for an EDB unpack, such as
`/Users/Shared/pgsql/bin`.

Homebrew is optional and only on systems where Homebrew ships bottles. On
Intel macOS 14, Homebrew has no PostgreSQL 16 bottle, so
`brew install postgresql@16` compiles from source. Prebuilt binaries are the
primary path here.

### Cluster, database, roles

1. Create a cluster owned by `dhi_owner`. `initdb` also creates the
   maintenance database `postgres`:

   ```sh
   "$PG_BIN/initdb" -D /Users/Shared/dhi/pgdata -U dhi_owner --auth=scram-sha-256
   ```

   Use a data directory your user owns. Skip this step if Postgres.app already
   initialized a cluster you want to use, and point `-D` at that cluster for
   the `pg_ctl` commands.

2. Start the server on `127.0.0.1:5432`:

   ```sh
   "$PG_BIN/pg_ctl" -D /Users/Shared/dhi/pgdata -l /Users/Shared/dhi/pg.log start
   ```

3. Confirm the owner role can log in with a password. `initdb -U dhi_owner`
   already created that superuser in the new cluster; this block only creates
   the login role when the cluster came from Postgres.app or another owner:

   ```sh
   "$PG_BIN/psql" "postgres://dhi_owner@127.0.0.1:5432/postgres" \
     -v ON_ERROR_STOP=1 <<'SQL'
   DO $$
   BEGIN
       IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'dhi_owner') THEN
           CREATE ROLE dhi_owner LOGIN CREATEDB PASSWORD 'dhi_owner';
       END IF;
   END
   $$;
   SQL
   ```

   If the cluster's superuser has another name, connect as that user first.

4. Create the database owned by `dhi_owner`:

   ```sh
   "$PG_BIN/createdb" -h 127.0.0.1 -p 5432 -U dhi_owner -O dhi_owner dhi
   ```

5. Run the role and grant script. It applies `ops/postgres/01-roles.sql` and
   `ops/postgres/02-grants.sql` and verifies the activity lock. The server
   applies the app schema at startup. If the `activity` table does not exist
   yet, the script says so and the server installs the lock on its first run.

   ```sh
   scripts/migrate.sh
   ```

   The script needs `psql` and `pg_isready` on `PATH`, so run it in a shell
   where `PATH="$PG_BIN:$PATH"` is still set. It uses
   `MIGRATION_DATABASE_URL`, which defaults to
   `postgres://dhi_owner:dhi_owner@127.0.0.1:5432/dhi`.

6. Put the runtime and migration URLs in `.env`. With the sample passwords
   from `.env.example`:

   ```dotenv
   DATABASE_URL=postgres://dhi_app:dhi_app@127.0.0.1:5432/dhi
   MIGRATION_DATABASE_URL=postgres://dhi_owner:dhi_owner@127.0.0.1:5432/dhi
   TEST_DATABASE_URL=postgres://dhi_test:dhi_test@127.0.0.1:5432/dhi
   ```

   If you choose your own passwords, change them in `ops/postgres/01-roles.sql`
   before running the script, or set them with `ALTER ROLE` afterwards. The
   server needs both URLs: it migrates as `dhi_owner` and serves and runs tasks
   as `dhi_app`.

### Start PostgreSQL after a reboot

`pg_ctl` does not survive a reboot. Either start the server by hand after each
boot:

```sh
"$PG_BIN/pg_ctl" -D /Users/Shared/dhi/pgdata -l /Users/Shared/dhi/pg.log start
"$PG_BIN/pg_ctl" -D /Users/Shared/dhi/pgdata stop
```

or load a per-user LaunchAgent. This is optional and needs no root; it starts
PostgreSQL when you log in. Save it as
`~/Library/LaunchAgents/local.dhi.postgres.plist`, replacing `<you>` and the
data directory:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>local.dhi.postgres</string>
    <key>ProgramArguments</key>
    <array>
        <string>/Applications/Postgres.app/Contents/Versions/16/bin/postgres</string>
        <string>-D</string>
        <string>/Users/Shared/dhi/pgdata</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <false/>
    <key>StandardOutPath</key>
    <string>/Users/<you>/Library/Logs/dhi-postgres.log</string>
    <key>StandardErrorPath</key>
    <string>/Users/<you>/Library/Logs/dhi-postgres.log</string>
</dict>
</plist>
```

Load and unload it with:

```sh
launchctl bootstrap gui/"$(id -u)" ~/Library/LaunchAgents/local.dhi.postgres.plist
launchctl bootout gui/"$(id -u)" ~/Library/LaunchAgents/local.dhi.postgres.plist
```

The `ProgramArguments` path must stay absolute; `launchd` does not read your
shell `PATH`. Use the EDB `bin/postgres` path instead when that is the install.

## Server and UI

- **Rust server** — build with `cargo build -p dhi-server`, then run
  `./target/debug/dhi-server`, or use `cargo run -p dhi-server`. The server
  loads `.env` from its working directory.
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
- **GPU** — a Metal compute program is written, compiled, and run inside the
  sandbox with the real profile; it selects `MTLCopyAllDevices()[0]`, compiles
  a kernel at run time, and prints `[1,2,3,4]`.

A failed curl is reported as `SKIP` when the host is offline. The optional
project argument adds a read-only readability check against a real project
tree.

## GPU

The profile grants raw graphics access with a single line, `(system-graphics)`,
placed after `(import "system.sb")`. Nothing else is added: no `windowserver`
rule, no broker, no typed GPU tool, and no GPU policy. A task talks to the GPU
the way any local program does.

### Hardware

| Property | Value |
| --- | --- |
| GPU | Intel UHD Graphics 617 |
| Peak FP32 | ~155 GFLOPS |
| Memory bandwidth | ~17 GB/s |
| Memory | 1.5 GB shared with the CPU |
| FP64 | none |

### Raw access

Inside the task a program can use Metal 3 compute and offscreen render, OpenCL
1.2, MPS, CoreImage, and VideoToolbox directly. There is no intermediary: the
task links the framework and creates its own device, queues, and pipeline
states.

### Building and running

Compile and run under `$WORK`, and `cd "$WORK"` first so the compiler's output
and any temporary files land in writable space.

`MTLCreateSystemDefaultDevice` returns nil on this Intel Mac, so select the
device explicitly:

```objc
NSArray<id<MTLDevice>> *devices = MTLCopyAllDevices();
id<MTLDevice> dev = devices[0];
```

Runtime shader compilation works: `newLibraryWithSource:` compiles a Metal
source string at run time. No `metal` CLI and no prebuilt `.metallib` are
needed.

### Objective-C++ (default)

Objective-C++ is the default host language. Save the file as `.mm`, because
`clang++ -std=c++17` rejects a plain `.m` file, and build with the Command Line
Tools compiler:

```sh
cd "$WORK"
/Library/Developer/CommandLineTools/usr/bin/clang++ \
  -std=c++17 -fobjc-arc \
  -isysroot /Library/Developer/CommandLineTools/SDKs/MacOSX.sdk \
  -framework Metal -framework Foundation \
  -o gpu_probe gpu_probe.mm
./gpu_probe
```

A short compute example, `gpu_probe.mm`, prints `[1,2,3,4]`:

```objc
#import <Foundation/Foundation.h>
#import <Metal/Metal.h>

int main(void) {
  @autoreleasepool {
    NSError *err = nil;
    id<MTLDevice> dev = MTLCopyAllDevices()[0];
    // out[i] = i + 1, compiled at run time.
    id<MTLLibrary> lib = [dev newLibraryWithSource:
        @"#include <metal_stdlib>\n"
         "using namespace metal;\n"
         "kernel void bump(device int *out [[buffer(0)]],\n"
         "                 uint i [[thread_position_in_grid]]) {\n"
         "  out[i] = int(i) + 1;\n"
         "}\n"
        options:nil error:&err];
    id<MTLFunction> fn = [lib newFunctionWithName:@"bump"];
    id<MTLComputePipelineState> pso =
        [dev newComputePipelineStateWithFunction:fn error:&err];

    id<MTLBuffer> buf = [dev newBufferWithLength:4 * sizeof(int)
                                        options:MTLResourceStorageModeShared];
    id<MTLCommandQueue> q = [dev newCommandQueue];
    id<MTLCommandBuffer> cb = [q commandBuffer];
    id<MTLComputeCommandEncoder> enc = [cb computeCommandEncoder];
    [enc setComputePipelineState:pso];
    [enc setBuffer:buf offset:0 atIndex:0];
    [enc dispatchThreads:MTLSizeMake(4, 1, 1)
        threadsPerThreadgroup:MTLSizeMake(4, 1, 1)];
    [enc endEncoding];
    [cb commit];
    [cb waitUntilCompleted];

    int *out = (int *)buf.contents;
    printf("[%d,%d,%d,%d]\n", out[0], out[1], out[2], out[3]);
  }
  return 0;
}
```

### Pure C++ with metal-cpp (optional)

Apple's `metal-cpp` headers give C++ bindings to the same Metal API without
Objective-C++. The headers are not installed anywhere on this Mac, so they must
be vendored into the repository before use. Build the same shape, pointing
`-I` at the vendored headers:

```sh
clang++ -std=c++17 -I<metal-cpp> -framework Metal -framework Foundation \
  -o gpu_probe gpu_probe.cpp
```

### Gaps

- **MPSGraph fails on Intel.** Do not plan on it.
- **No CoreML model compiler.** Models cannot be compiled on this host.
- **No `metal` or `metallib`.** There is no offline shader compiler tool; compile
  shaders at run time from source, as above.

### Architecture

There is no broker, no typed GPU tool, and no server-side helper. The server
does not mediate GPU work; the task program owns the device and its command
queues, subject only to the sandbox profile.

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
