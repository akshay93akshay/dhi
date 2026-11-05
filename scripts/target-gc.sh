#!/usr/bin/env bash
# Remove stale workspace build artifacts from the shared target.
#
# Keeps:
# - every external dependency artifact
# - current workspace artifacts newer than the cutoff
# - current workspace fingerprints and incremental cache
#
# Removes:
# - old workspace test binaries and libraries
# - old workspace fingerprint directories
# - old incremental directories
#
# Default is a dry run. Use --apply to delete.
#
# Usage:
#   scripts/target-gc.sh
#   scripts/target-gc.sh --apply
#   DHI_GC_GRACE_MINUTES=20 scripts/target-gc.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null || (cd "$SCRIPT_DIR/.." && pwd))"
TARGET="$ROOT/target"
DEBUG="$TARGET/debug"
MARKER="${DHI_GC_MARKER:-$DEBUG/dhi-server}"
GRACE="${DHI_GC_GRACE_MINUTES:-15}"
APPLY=0

case "${1:-}" in
  --apply) APPLY=1 ;;
  ""|--dry-run) APPLY=0 ;;
  *)
    echo "usage: $0 [--dry-run|--apply]" >&2
    exit 2
    ;;
esac

[ -d "$DEBUG" ] || { echo "missing $DEBUG" >&2; exit 1; }
[ -f "$MARKER" ] || { echo "missing build marker: $MARKER" >&2; exit 1; }

CUTOFF="$(python3 - "$MARKER" "$GRACE" <<'PY'
import os, sys, time
print(int(os.path.getmtime(sys.argv[1]) - int(sys.argv[2]) * 60))
PY
)"

METADATA="$(cd "$ROOT" && cargo metadata --no-deps --format-version=1 2>/dev/null)"
export GC_METADATA="$METADATA"

python3 - "$DEBUG" "$CUTOFF" "$APPLY" <<'PY'
import json
import os
import shutil
import sys

debug = sys.argv[1]
cutoff = int(sys.argv[2])
apply = sys.argv[3] == "1"
metadata = json.loads(os.environ["GC_METADATA"])
workspace = {package["name"] for package in metadata["packages"]}
workspace_dash = {name.replace("_", "-") for name in workspace}
workspace_under = {name.replace("-", "_") for name in workspace}


def size_of(path):
    if os.path.isfile(path) or os.path.islink(path):
        try:
            return os.path.getsize(path)
        except OSError:
            return 0
    total = 0
    for root, _dirs, files in os.walk(path):
        for name in files:
            try:
                total += os.path.getsize(os.path.join(root, name))
            except OSError:
                pass
    return total


def old(path):
    try:
        return os.path.getmtime(path) < cutoff
    except OSError:
        return False


def workspace_lib(name):
    if not name.startswith("lib"):
        return False
    stem = name[3:]
    if "-" not in stem:
        return False
    crate = stem.rsplit("-", 1)[0]
    return crate in workspace_under


candidates = []

deps = os.path.join(debug, "deps")
if os.path.isdir(deps):
    for name in os.listdir(deps):
        path = os.path.join(deps, name)
        if not os.path.isfile(path) or not old(path):
            continue
        if name.startswith(("build_script_build-", "build-script-build", "rustc-", "rustdoc-")):
            continue
        if workspace_lib(name):
            candidates.append(path)
        elif not name.startswith("lib"):
            # Test binaries, old dhi-server binaries, old dep-info files.
            candidates.append(path)

fingerprints = os.path.join(debug, ".fingerprint")
if os.path.isdir(fingerprints):
    for name in os.listdir(fingerprints):
        path = os.path.join(fingerprints, name)
        if not os.path.isdir(path) or not old(path) or "-" not in name:
            continue
        package = name.rsplit("-", 1)[0]
        if package in workspace_dash or package in workspace_under:
            candidates.append(path)

incremental = os.path.join(debug, "incremental")
if os.path.isdir(incremental):
    for name in os.listdir(incremental):
        path = os.path.join(incremental, name)
        if os.path.isdir(path) and old(path):
            candidates.append(path)

rows = sorted(((size_of(path), path) for path in candidates), reverse=True)
total = sum(size for size, _ in rows)
print(f"cutoff: {cutoff}")
print(f"workspace packages: {len(workspace)}")
print(f"candidates: {len(rows)}")
print(f"reclaimable: {total / (1024 ** 3):.2f} GiB")
for size, path in rows[:40]:
    print(f"{size / (1024 ** 2):9.1f} MiB  {path}")
if len(rows) > 40:
    print(f"... and {len(rows) - 40} more")

if not apply:
    print("dry run only; rerun with --apply")
    sys.exit(0)

removed = 0
for size, path in rows:
    try:
        if os.path.isdir(path) and not os.path.islink(path):
            shutil.rmtree(path)
        else:
            os.unlink(path)
        removed += size
    except FileNotFoundError:
        pass
    except OSError as error:
        print(f"failed: {path}: {error}", file=sys.stderr)
print(f"removed: {removed / (1024 ** 3):.2f} GiB")
PY
