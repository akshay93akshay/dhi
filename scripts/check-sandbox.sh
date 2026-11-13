#!/usr/bin/env bash
# Acceptance checks for the native macOS sandbox profile.
#
# Usage: scripts/check-sandbox.sh [project-dir]
#
# Everything runs as the current user; no sudo, no task user, no aliases and no
# system changes. The optional project-dir is an extra read-only readability
# check against a real project tree.
#
# The checks run the profile against the two layouts the backend produces:
#
# - a plain layout: PROJECT and WORK are unrelated temporary directories;
# - the production layout: WORK is `<project>/.dhi/<task-id>` and SPILL is
#   `<WORK>/.dhi`, exactly as the server lays out a project task.
#
# Each test prints PASS, FAIL, or SKIP (a test that cannot run on this host).
# Any FAIL exits nonzero. Only the temporary directories are removed.
set -euo pipefail

usage() {
  echo "usage: $0 [project-dir]" >&2
  exit 2
}

[ "$#" -le 1 ] || usage

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd -P)"
PROFILE="${DHI_SANDBOX_PROFILE:-$REPO_ROOT/ops/macos/dhi-task.sb}"
SANDBOX_EXEC="/usr/bin/sandbox-exec"

[ -r "$PROFILE" ] || {
  echo "profile not found: $PROFILE" >&2
  exit 1
}
[ -x "$SANDBOX_EXEC" ] || {
  echo "sandbox-exec not found: $SANDBOX_EXEC" >&2
  exit 1
}
# The server canonicalizes the profile path; do the same here so the printed
# path is the one the backend would use.
PROFILE="$(cd "$(dirname "$PROFILE")" && pwd -P)/$(basename "$PROFILE")"

EXTRA_PROJECT=""
if [ "$#" -eq 1 ]; then
  [ -d "$1" ] || {
    echo "not a directory: $1" >&2
    exit 2
  }
  EXTRA_PROJECT="$(cd "$1" && pwd -P)"
fi

FAILS=0
pass() { printf 'PASS  %s\n' "$*"; }
fail() { printf 'FAIL  %s\n' "$*"; FAILS=$((FAILS + 1)); }
skip() { printf 'SKIP  %s\n' "$*"; }

TMP_BASE="${TMPDIR:-/tmp}"
TMP_BASE="${TMP_BASE%/}"
TMP_ROOT="$(mktemp -d "$TMP_BASE/dhi-sandbox-check.XXXXXX")"
# Seatbelt sees /private/var, not /var, so every parameter is canonical.
TMP_ROOT="$(cd "$TMP_ROOT" && pwd -P)"

PROJECT="$TMP_ROOT/project"
WORK="$TMP_ROOT/work"
SPILL="$WORK/.dhi"
PIDS="$TMP_ROOT/pids"
PIDFILE="$PIDS/pid"

cleanup() {
  rm -rf "$TMP_ROOT"
}
trap cleanup EXIT

mkdir -p "$PROJECT" "$WORK" "$SPILL" "$PIDS"
printf 'project file\n' > "$PROJECT/file"
chmod 0644 "$PROJECT/file"
printf 'secret\n' > "$PROJECT/secret"
chmod 0600 "$PROJECT/secret"
printf 'spill\n' > "$SPILL/spill"

# Runs one command under the profile. The four parameters are always passed,
# as the backend passes them; `-D PROJECT=...` must come before the command.
sandbox_at() {
  local project="$1"
  local work="$2"
  local spill="$3"
  local pidfile="$4"
  shift 4
  "$SANDBOX_EXEC" -f "$PROFILE" \
    -D "PROJECT=$project" \
    -D "WORK=$work" \
    -D "SPILL=$spill" \
    -D "PIDFILE=$pidfile" \
    "$@"
}

run_sandbox() {
  sandbox_at "$PROJECT" "$WORK" "$SPILL" "$PIDFILE" "$@"
}

echo "user:    $(id -un) ($(id -u))"
echo "profile: $PROFILE"
echo "project: $PROJECT"
echo "work:    $WORK"
echo "spill:   $SPILL"
echo "pidfile: $PIDFILE"
echo

echo "== plain layout =="

# 1. The project is readable.
if out="$(run_sandbox /bin/cat "$PROJECT/file" 2>/dev/null)" \
   && [ "$out" = "project file" ]; then
  pass "project file is readable"
else
  fail "project file is not readable"
fi

# 2. A mode-0600 project file is readable because the task is the same user.
if out="$(run_sandbox /bin/cat "$PROJECT/secret" 2>/dev/null)" \
   && [ "$out" = "secret" ]; then
  pass "project 0600 file is readable (same user)"
else
  fail "project 0600 file is not readable"
fi

# 3. The project is read-only.
if run_sandbox /usr/bin/touch "$PROJECT/x" >/dev/null 2>&1; then
  fail "project write was allowed"
else
  pass "project write is denied"
fi

# 4. The task work directory is writable.
if run_sandbox /usr/bin/touch "$WORK/x" >/dev/null 2>&1; then
  pass "work write is allowed"
else
  fail "work write is denied"
fi

# 5. The spill directory stays read-only even though it lives under work.
if run_sandbox /usr/bin/touch "$SPILL/x" >/dev/null 2>&1; then
  fail "spill write was allowed"
else
  pass "spill write is denied"
fi

# 6. The spill directory and its contents cannot be deleted.
if run_sandbox /bin/rm -rf "$SPILL" >/dev/null 2>&1; then
  fail "spill delete was allowed"
elif [ ! -f "$SPILL/spill" ] || [ ! -d "$SPILL" ]; then
  fail "spill delete partially succeeded"
else
  pass "spill delete is denied"
fi

# 7. User data outside the project and work is not readable.
if [ -f "$HOME/.ssh/config" ]; then
  if run_sandbox /bin/cat "$HOME/.ssh/config" >/dev/null 2>&1; then
    fail "~/.ssh/config is readable"
  else
    pass "~/.ssh/config read is denied"
  fi
else
  skip "~/.ssh/config read (file does not exist)"
fi

# 8. The home directory itself is not listable.
if run_sandbox /bin/ls "$HOME" >/dev/null 2>&1; then
  fail "home directory is listable"
else
  pass "home directory listing is denied"
fi

# 9. Network is allowed. If the host itself is offline the check is skipped
# instead of failing.
if ! /usr/bin/curl --max-time 3 -fsS -o /dev/null https://example.com \
     >/dev/null 2>&1; then
  skip "network test (host is offline)"
elif run_sandbox /usr/bin/curl --max-time 3 -fsS -o /dev/null https://example.com \
       >/dev/null 2>&1; then
  pass "network is allowed"
else
  fail "network is denied"
fi

# 10. The server-owned pidfile is writable; sibling files in its directory are
# not. This is the only write allowed outside WORK.
if run_sandbox /bin/sh -c 'echo $$ >> "$1"' sh "$PIDFILE" >/dev/null 2>&1 \
   && [ -s "$PIDFILE" ] \
   && grep -Eq '^[0-9]+$' "$PIDFILE"; then
  pass "pidfile write is allowed"
else
  fail "pidfile write is denied or not numeric"
fi

if run_sandbox /usr/bin/touch "$PIDS/other" >/dev/null 2>&1; then
  fail "pidfile sibling write was allowed"
else
  pass "pidfile sibling write is denied"
fi

# 11. Two tasks run at the same time, each in its own real work directory.
# Both sandbox-exec processes start together and each records its session
# leader in a different server-owned pidfile.
WORK_A="$TMP_ROOT/work-a"
WORK_B="$TMP_ROOT/work-b"
PIDFILE_A="$PIDS/a.pid"
PIDFILE_B="$PIDS/b.pid"
mkdir -p "$WORK_A/.dhi" "$WORK_B/.dhi"

sandbox_at "$PROJECT" "$WORK_A" "$WORK_A/.dhi" "$PIDFILE_A" \
  /bin/sh -c 'printf "%s\n" "$$" >> "$1"; sleep 1; printf "a\n" > "$2/a"' \
  sh "$PIDFILE_A" "$WORK_A" &
PID_A=$!
sandbox_at "$PROJECT" "$WORK_B" "$WORK_B/.dhi" "$PIDFILE_B" \
  /bin/sh -c 'printf "%s\n" "$$" >> "$1"; sleep 1; printf "b\n" > "$2/b"' \
  sh "$PIDFILE_B" "$WORK_B" &
PID_B=$!

# Both must still be running while they sleep, otherwise they did not overlap.
OVERLAPPED=0
if kill -0 "$PID_A" 2>/dev/null && kill -0 "$PID_B" 2>/dev/null; then
  OVERLAPPED=1
fi

STATUS_A=0
STATUS_B=0
wait "$PID_A" || STATUS_A=$?
wait "$PID_B" || STATUS_B=$?

if [ "$STATUS_A" -eq 0 ] && [ "$STATUS_B" -eq 0 ] && [ "$OVERLAPPED" -eq 1 ] \
   && [ -f "$WORK_A/a" ] && [ -f "$WORK_B/b" ] \
   && grep -Eq '^[0-9]+$' "$PIDFILE_A" && grep -Eq '^[0-9]+$' "$PIDFILE_B"; then
  pass "two tasks run concurrently in separate work dirs"
else
  fail "concurrent tasks failed (a=$STATUS_A b=$STATUS_B overlap=$OVERLAPPED)"
fi

echo
echo "== production layout =="

# WORK is <project>/.dhi/<task-id> and SPILL is <WORK>/.dhi, exactly as the
# server lays out a project task. The pidfile stays server-owned, beside the
# workspace rather than inside the project.
PROD_PROJECT="$TMP_ROOT/prod-project"
PROD_WORK="$PROD_PROJECT/.dhi/$(uuidgen)"
PROD_SPILL="$PROD_WORK/.dhi"
mkdir -p "$PROD_WORK/.dhi"
printf 'production project file\n' > "$PROD_PROJECT/file"
chmod 0644 "$PROD_PROJECT/file"

prod() {
  sandbox_at "$PROD_PROJECT" "$PROD_WORK" "$PROD_SPILL" "$PIDS/prod.pid" "$@"
}

echo "prod project: $PROD_PROJECT"
echo "prod work:    $PROD_WORK"
echo "prod spill:   $PROD_SPILL"
echo

# 12. A project file is readable in the production layout.
if out="$(prod /bin/cat "$PROD_PROJECT/file" 2>/dev/null)" \
   && [ "$out" = "production project file" ]; then
  pass "production: project file is readable"
else
  fail "production: project file is not readable"
fi

# 13. The project is read-only even though WORK lives below it.
if prod /usr/bin/touch "$PROD_PROJECT/x" >/dev/null 2>&1; then
  fail "production: project write was allowed"
else
  pass "production: project write is denied"
fi

# 14. The work folder below the project is writable.
if prod /usr/bin/touch "$PROD_WORK/x" >/dev/null 2>&1; then
  pass "production: work write is allowed"
else
  fail "production: work write is denied"
fi

# 15. The spill directory below the work folder is not writable.
if prod /usr/bin/touch "$PROD_SPILL/x" >/dev/null 2>&1; then
  fail "production: spill write was allowed"
else
  pass "production: spill write is denied"
fi

# 16. A hard link from work to a project file would republish project content
# in writable space, so links are denied outright.
if prod /bin/ln "$PROD_PROJECT/file" "$PROD_WORK/link" >/dev/null 2>&1; then
  fail "production: hard link to a project file was allowed"
  rm -f "$PROD_WORK/link"
else
  pass "production: hard link to a project file is denied"
fi

# 17. A symlink from work to a project file cannot be used to write through it.
if prod /bin/ln -s "$PROD_PROJECT/file" "$PROD_WORK/symlink" >/dev/null 2>&1; then
  if prod /bin/sh -c 'printf "tampered\n" >> "$1"' sh "$PROD_WORK/symlink" \
       >/dev/null 2>&1; then
    fail "production: write through a work symlink to a project file was allowed"
  elif out="$(prod /bin/cat "$PROD_WORK/symlink" 2>/dev/null)" \
       && [ "$out" = "production project file" ]; then
    pass "production: write through a work symlink to a project file is denied"
  else
    fail "production: write through the symlink changed or lost the project file"
  fi
else
  fail "production: cannot create the work symlink"
fi

# 18. The work folder cannot be moved out from under the project.
if prod /bin/mv "$PROD_WORK" "$PROD_WORK.moved" >/dev/null 2>&1; then
  fail "production: moving the work folder was allowed"
  rm -rf "$PROD_WORK.moved"
elif [ -d "$PROD_WORK" ]; then
  pass "production: moving the work folder is denied"
else
  fail "production: the work folder disappeared"
fi

# 19. sudo and su are denied by the profile. When the host has no sudo there
# is nothing to deny.
if [ ! -x /usr/bin/sudo ]; then
  skip "sudo exec (sudo is not installed)"
elif prod /usr/bin/sudo -n true >/dev/null 2>&1; then
  fail "sudo exec was allowed"
else
  pass "sudo exec is denied"
fi

# 21. Raw GPU access. The profile grants `(system-graphics)` and nothing more,
# so a Metal compute program compiled and run inside the sandbox must see the
# Intel GPU, compile a kernel at runtime, and return the computed values.
# MTLCreateSystemDefaultDevice is nil on this Intel Mac, so the probe uses
# MTLCopyAllDevices()[0]. The CLT clang and SDK are named directly to keep
# xcrun_db off the path.
CLANG="/Library/Developer/CommandLineTools/usr/bin/clang++"
CLT_SDK="/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk"
GPU_DIR="$WORK/gpu"
mkdir -p "$GPU_DIR/.home" "$GPU_DIR/.tmp"
cat > "$GPU_DIR/gpu_probe.mm" <<'PROBE'
#import <Foundation/Foundation.h>
#import <Metal/Metal.h>

static NSString *const kSrc =
    @"#include <metal_stdlib>\n"
     "using namespace metal;\n"
     "kernel void bump(device int *out [[buffer(0)]],\n"
     "                 uint i [[thread_position_in_grid]]) {\n"
     "  out[i] = int(i) + 1;\n"
     "}\n";

int main(void) {
  @autoreleasepool {
    NSError *err = nil;
    NSArray<id<MTLDevice>> *devices = MTLCopyAllDevices();
    if (devices.count == 0) { fprintf(stderr, "no Metal device\n"); return 1; }
    id<MTLDevice> dev = devices[0];
    id<MTLLibrary> lib = [dev newLibraryWithSource:kSrc options:nil error:&err];
    if (!lib) { fprintf(stderr, "compile: %s\n", err.localizedDescription.UTF8String); return 1; }
    id<MTLFunction> fn = [lib newFunctionWithName:@"bump"];
    id<MTLComputePipelineState> pso = [dev newComputePipelineStateWithFunction:fn error:&err];
    if (!pso) { fprintf(stderr, "pipeline: %s\n", err.localizedDescription.UTF8String); return 1; }

    id<MTLBuffer> buf = [dev newBufferWithLength:4 * sizeof(int) options:MTLResourceStorageModeShared];
    id<MTLCommandQueue> q = [dev newCommandQueue];
    id<MTLCommandBuffer> cb = [q commandBuffer];
    id<MTLComputeCommandEncoder> enc = [cb computeCommandEncoder];
    [enc setComputePipelineState:pso];
    [enc setBuffer:buf offset:0 atIndex:0];
    [enc dispatchThreads:MTLSizeMake(4, 1, 1) threadsPerThreadgroup:MTLSizeMake(4, 1, 1)];
    [enc endEncoding];
    [cb commit];
    [cb waitUntilCompleted];
    if (cb.status != MTLCommandBufferStatusCompleted) {
      fprintf(stderr, "command buffer status %ld\n", (long)cb.status); return 1;
    }
    int *out = (int *)buf.contents;
    printf("[%d,%d,%d,%d]\n", out[0], out[1], out[2], out[3]);
    return (out[0] == 1 && out[1] == 2 && out[2] == 3 && out[3] == 4) ? 0 : 1;
  }
}
PROBE

if [ ! -x "$CLANG" ] || [ ! -d "$CLT_SDK" ]; then
  skip "Metal compute (Command Line Tools or SDK missing)"
elif ! run_sandbox /bin/sh -c \
       'cd "$1" || exit 1; HOME="$1/.home" TMPDIR="$1/.tmp" "$2" \
          -std=c++17 -fobjc-arc -isysroot "$3" \
          -framework Metal -framework Foundation \
          -o gpu_probe gpu_probe.mm' \
       sh "$GPU_DIR" "$CLANG" "$CLT_SDK" >/dev/null 2>&1; then
  fail "Metal compute: probe does not compile inside the sandbox"
elif out="$(run_sandbox /bin/sh -c \
         'cd "$1" || exit 1; HOME="$1/.home" TMPDIR="$1/.tmp" ./gpu_probe' \
         sh "$GPU_DIR" 2>/dev/null)" \
     && [ "$out" = "[1,2,3,4]" ]; then
  pass "Metal compute inside sandbox"
else
  fail "Metal compute inside sandbox (got: ${out:-<error>})"
fi

# Optional: a real project tree is readable read-only.
if [ -n "$EXTRA_PROJECT" ]; then
  if "$SANDBOX_EXEC" -f "$PROFILE" \
       -D "PROJECT=$EXTRA_PROJECT" -D "WORK=$WORK" -D "SPILL=$SPILL" \
       -D "PIDFILE=$PIDFILE" \
       /bin/ls "$EXTRA_PROJECT" >/dev/null 2>&1; then
    pass "extra project is readable: $EXTRA_PROJECT"
  else
    fail "extra project is not readable: $EXTRA_PROJECT"
  fi
fi

echo
if [ "$FAILS" -gt 0 ]; then
  echo "sandbox check failed ($FAILS failure(s))" >&2
  exit 1
fi
echo "sandbox check ok"
