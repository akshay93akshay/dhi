#!/usr/bin/env bash
# DHI background service: no terminal window stays open.
#
# Config: ~/.config/dhi/config.json (or $DHI_CONFIG) holds the Firecrawl key.
# The server reads it directly. Web provider keys (TinyFish, Parallel) live in
# a mode-600 env file (default ~/.local/state/dhi/web.env) that this script
# writes and exports before starting the binary.
#
# Usage: scripts/dhi-service.sh install|start|stop|restart|status
#        scripts/dhi-service.sh set-web-key tinyfish|parallel
#        scripts/dhi-service.sh set-web-keys
set -euo pipefail
cd "$(dirname "$0")/.."

STATE_DIR="${DHI_STATE_DIR:-$HOME/.local/state/dhi}"
BIN_DIR="$STATE_DIR/bin"
LOG="$STATE_DIR/server.log"
PID_FILE="$STATE_DIR/server.pid"
WEB_ENV="${DHI_WEB_ENV:-$STATE_DIR/web.env}"
BIN="${DHI_BIN:-$BIN_DIR/dhi-server}"
BUILD_BIN="${DHI_BUILD_BIN:-./target/debug/dhi-server}"

mkdir -p "$STATE_DIR"

install_bin() {
  if [ ! -x "$BUILD_BIN" ]; then
    echo "built dhi-server binary not found or not executable: $BUILD_BIN" >&2
    echo "run: cargo build -p dhi-server" >&2
    exit 1
  fi
  mkdir -p "$BIN_DIR"
  local tmp
  tmp="$(mktemp "$BIN_DIR/.dhi-server.XXXXXX")"
  if ! cp "$BUILD_BIN" "$tmp" || ! chmod 755 "$tmp"; then
    rm -f "$tmp"
    echo "failed to stage dhi-server binary in $BIN_DIR" >&2
    exit 1
  fi
  mv -f "$tmp" "$BIN"
  echo "dhi-server installed: $BIN"
}

load_web_env() {
  if [ -f "$WEB_ENV" ]; then
    # shellcheck disable=SC1090
    set -a
    . "$WEB_ENV"
    set +a
  fi
}

running_pid() {
  if [ -f "$PID_FILE" ]; then
    local pid
    pid="$(cat "$PID_FILE" 2>/dev/null || true)"
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
      printf '%s' "$pid"
      return 0
    fi
  fi
  return 1
}

start() {
  if pid="$(running_pid)"; then
    echo "dhi-server already running (pid $pid)"
    return 0
  fi
  if [ ! -x "$BIN" ]; then
    echo "dhi-server binary not found or not executable: $BIN" >&2
    echo "run: scripts/dhi-service.sh install" >&2
    exit 1
  fi
  load_web_env
  DATABASE_URL="${DATABASE_URL:-postgres://dhi_app:dhi_app@127.0.0.1:5432/dhi}" \
  MIGRATION_DATABASE_URL="${MIGRATION_DATABASE_URL:-postgres://dhi_owner:dhi_owner@127.0.0.1:5432/dhi}" \
  DHI_BIND="${DHI_BIND:-127.0.0.1:8080}" \
  DHI_WEB_DIR="${DHI_WEB_DIR:-$PWD/web/out}" \
  RUST_LOG="${RUST_LOG:-dhi=info}" \
  nohup "$BIN" >>"$LOG" 2>&1 &
  echo $! >"$PID_FILE"
  echo "dhi-server started (pid $(cat "$PID_FILE"))"
  echo "log: $LOG"
}

stop() {
  if pid="$(running_pid)"; then
    kill "$pid" 2>/dev/null || true
    for _ in $(seq 1 20); do
      kill -0 "$pid" 2>/dev/null || break
      sleep 0.25
    done
    if kill -0 "$pid" 2>/dev/null; then
      kill -9 "$pid" 2>/dev/null || true
    fi
    rm -f "$PID_FILE"
    echo "dhi-server stopped (pid $pid)"
  else
    rm -f "$PID_FILE"
    echo "dhi-server is not running"
  fi
}

status() {
  if pid="$(running_pid)"; then
    echo "dhi-server running (pid $pid)"
    echo "log: $LOG"
  else
    echo "dhi-server is not running"
    exit 1
  fi
}

CONFIG="${DHI_CONFIG:-$HOME/.config/dhi/config.json}"

mask_key() {
  python3 - "$1" <<'PY'
import sys
key = sys.argv[1]
print("…" + key[-4:] if len(key) >= 4 else "…")
PY
}

set_key() {
  local key keyfile
  read -r -s -p "Firecrawl API key: " key
  echo
  if [ -z "${key// /}" ]; then
    echo "empty key; nothing changed" >&2
    exit 2
  fi
  mkdir -p "$(dirname "$CONFIG")"
  keyfile="$(mktemp "${TMPDIR:-/tmp}/dhi-key.XXXXXX")"
  chmod 600 "$keyfile"
  printf '%s' "$key" >"$keyfile"
  python3 - "$CONFIG" "$keyfile" <<'PY'
import json, os, sys, tempfile
path, keyfile = sys.argv[1], sys.argv[2]
with open(keyfile) as fh:
    key = fh.read().strip()
os.unlink(keyfile)
data = {}
if os.path.exists(path):
    try:
        with open(path) as fh:
            data = json.load(fh)
    except json.JSONDecodeError as err:
        sys.exit(f"existing config is not valid JSON ({err}); refusing to overwrite")
fc = data.setdefault("firecrawl", {})
fc["apiKey"] = key
fc.setdefault("baseUrl", "https://api.firecrawl.dev")
fc.setdefault("timeoutSecs", 60)
directory = os.path.dirname(path) or "."
fd, tmp = tempfile.mkstemp(dir=directory, prefix=".dhi-config-")
try:
    with os.fdopen(fd, "w") as fh:
        json.dump(data, fh, indent=2)
        fh.write("\n")
    os.chmod(tmp, 0o600)
    os.replace(tmp, path)
finally:
    if os.path.exists(tmp):
        os.unlink(tmp)
hint = ("…" + key[-4:]) if len(key) >= 4 else "…"
print(f"firecrawl key saved ({hint}); mode 600; path: {path}")
PY
  rm -f "$keyfile"
}

restart_if_running() {
  if running_pid >/dev/null; then
    stop
    start
  else
    echo "run: $0 start"
  fi
}

save_web_key() {
  local provider="${1:-}" var label key tmp
  case "$provider" in
    tinyfish) var="TINYFISH_API_KEY"; label="TinyFish" ;;
    parallel) var="PARALLEL_API_KEY"; label="Parallel" ;;
    *)
      echo "usage: $0 set-web-key tinyfish|parallel" >&2
      exit 2
      ;;
  esac

  read -r -s -p "$label API key (Enter to skip): " key || true
  echo
  if [ -z "${key// /}" ]; then
    echo "empty key; $var unchanged"
    return 0
  fi
  case "$key" in
    *"'"*|*[![:print:]]*)
      echo "key contains unsupported characters; $var unchanged" >&2
      return 2
      ;;
  esac

  mkdir -p "$(dirname "$WEB_ENV")"
  touch "$WEB_ENV"
  chmod 600 "$WEB_ENV"
  tmp="$(mktemp "${WEB_ENV}.XXXXXX")"
  chmod 600 "$tmp"
  if [ -f "$WEB_ENV" ]; then
    grep -v "^${var}=" "$WEB_ENV" >"$tmp" || true
  fi
  printf "%s='%s'\n" "$var" "$key" >>"$tmp"
  mv "$tmp" "$WEB_ENV"
  chmod 600 "$WEB_ENV"
  echo "$var saved ($(mask_key "$key")); mode 600; path: $WEB_ENV"
}

set_web_key() {
  save_web_key "${1:-}"
  restart_if_running
}

set_web_keys() {
  save_web_key tinyfish
  save_web_key parallel
  restart_if_running
}

show_config() {
  python3 - "$CONFIG" <<'PY'
import json, os, sys
path = sys.argv[1]
if not os.path.exists(path):
    print(f"firecrawl config: none ({path})")
    raise SystemExit(0)
with open(path) as fh:
    data = json.load(fh)
fc = data.get("firecrawl", {})
if "apiKey" in fc:
    key = fc["apiKey"]
    fc["apiKey"] = ("…" + key[-4:]) if len(key) >= 4 else "…"
print(json.dumps(data, indent=2))
print(f"mode: {oct(os.stat(path).st_mode & 0o777)}; path: {path}")
PY
  if [ -f "$WEB_ENV" ]; then
    echo "web env: $WEB_ENV (mode $(stat -f '%Lp' "$WEB_ENV" 2>/dev/null || stat -c '%a' "$WEB_ENV"))"
    while IFS='=' read -r name value; do
      case "$name" in
        TINYFISH_API_KEY|PARALLEL_API_KEY)
          value="${value%\'}"
          value="${value#\'}"
          echo "  $name = $(mask_key "$value")"
          ;;
      esac
    done <"$WEB_ENV"
  else
    echo "web env: none ($WEB_ENV)"
  fi
}

set_base_url() {
  local url
  read -r -p "Firecrawl base URL: " url
  if [ -z "${url// /}" ]; then
    echo "empty URL; nothing changed" >&2
    exit 2
  fi
  mkdir -p "$(dirname "$CONFIG")"
  python3 - "$CONFIG" "$url" <<'PY'
import json, os, sys, tempfile
path, url = sys.argv[1], sys.argv[2].strip()
data = {}
if os.path.exists(path):
    try:
        with open(path) as fh:
            data = json.load(fh)
    except json.JSONDecodeError as err:
        sys.exit(f"existing config is not valid JSON ({err}); refusing to overwrite")
fc = data.setdefault("firecrawl", {})
fc["baseUrl"] = url
fc.setdefault("timeoutSecs", 60)
directory = os.path.dirname(path) or "."
fd, tmp = tempfile.mkstemp(dir=directory, prefix=".dhi-config-")
try:
    with os.fdopen(fd, "w") as fh:
        json.dump(data, fh, indent=2)
        fh.write("\n")
    os.chmod(tmp, 0o600)
    os.replace(tmp, path)
finally:
    if os.path.exists(tmp):
        os.unlink(tmp)
print(f"firecrawl base URL saved: {url}; mode 600; path: {path}")
PY
}

case "${1:-}" in
  install) install_bin ;;
  start) start ;;
  stop) stop ;;
  restart) stop; start ;;
  status) status ;;
  set-key) set_key ;;
  set-web-key) set_web_key "${2:-}" ;;
  set-web-keys) set_web_keys ;;
  show-config) show_config ;;
  set-base-url) set_base_url ;;
  *)
    echo "usage: $0 install|start|stop|restart|status|set-key|set-web-key tinyfish|parallel|set-web-keys|show-config|set-base-url" >&2
    exit 2
    ;;
esac
