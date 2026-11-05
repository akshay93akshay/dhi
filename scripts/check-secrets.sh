#!/usr/bin/env bash
# Read-only secret hygiene check for the DHI repo and local secret files.
# Never prints secret values.
#
# Usage: scripts/check-secrets.sh
set -euo pipefail
cd "$(dirname "$0")/.."

ok_count=0
warn_count=0
fail_count=0

usage() {
  echo "usage: $0" >&2
  exit 2
}

ok() {
  printf '[ok]   %s\n' "$*"
  ok_count=$((ok_count + 1))
}

warn() {
  printf '[warn] %s\n' "$*"
  warn_count=$((warn_count + 1))
}

fail() {
  printf '[fail] %s\n' "$*"
  fail_count=$((fail_count + 1))
}

[ "$#" -eq 0 ] || usage

# The clean source tree may not have a Git checkout yet. Fall back to scanning
# product files directly so the checks stay meaningful without Git.
have_git=0
if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  have_git=1
fi

product_files() {
  find . \
    \( -path './.git' -o -path './target' -o -path './web/node_modules' \
       -o -path './web/.next' -o -path './web/out' -o -path './backups' \
       -o -path './workspace' \) -prune -o \
    -type f -print | sed 's|^\./||'
}

# A. Secret file modes must be 600.
check_mode() {
  local file="$1" mode

  [ -e "$file" ] || return 0

  if ! mode="$(stat -f '%Lp' "$file" 2>/dev/null || stat -c '%a' "$file" 2>/dev/null)"; then
    fail "mode     $file (could not read mode)"
    return 0
  fi

  if [ "$mode" = "600" ]; then
    ok "mode     $file 600"
  else
    fail "mode     $file is $mode; expected 600"
  fi
}

for file in \
  "$HOME/.config/dhi/config.json" \
  "$HOME/.local/state/dhi/web.env" \
  ".env" \
  "config/dhi.config.json"
do
  check_mode "$file"
done

# B. No sensitive files may be tracked (or present, without Git).
tracked_bad=0
if [ "$have_git" -eq 1 ]; then
  tracked_list="$(git ls-files)"
else
  tracked_list="$(product_files)"
fi

while IFS= read -r path; do
  [ -n "$path" ] || continue

  base="${path##*/}"
  case "$base" in
    .env.example|*.example.*) continue ;;
  esac

  reason=""
  case "$base" in
    .env|.env.local|web.env|id_rsa|*.pem|*.key|*.dump|*.sql.gz)
      reason="sensitive filename"
      ;;
  esac
  case "$path" in
    backups/*)
      reason="${reason:-tracked backup path}"
      ;;
    ops/*.bak)
      reason="${reason:-tracked backup file}"
      ;;
  esac

  if [ -n "$reason" ]; then
    fail "tracked  $path ($reason)"
    tracked_bad=1
  fi
done <<<"$tracked_list"

if [ "$tracked_bad" -eq 0 ]; then
  ok "tracked  no sensitive files tracked"
fi

# C. High-confidence key prefixes in tracked files.
# Only the location and rule name are printed, never the matched value.
key_bad=0
scan_keys() {
  local rule="$1" pattern="$2" hits hit location rest line

  if [ "$have_git" -eq 1 ]; then
    hits="$(git grep -n -I -E -e "$pattern" -- . \
      ':!*.md' ':!*.example.*' ':!target' \
      ':!web/node_modules' ':!web/.next' || true)"
  else
    hits="$(rg -n -I --no-heading --color never --hidden --no-ignore \
      --glob '!.git/**' --glob '!target/**' \
      --glob '!web/node_modules/**' --glob '!web/.next/**' \
      --glob '!web/out/**' --glob '!backups/**' --glob '!workspace/**' \
      --glob '!*.md' --glob '!*.example.*' \
      -e "$pattern" . 2>/dev/null || true)"
  fi

  [ -n "$hits" ] || return 0

  key_bad=1
  while IFS= read -r hit; do
    [ -n "$hit" ] || continue
    location="${hit%%:*}"
    location="${location#./}"
    rest="${hit#*:}"
    line="${rest%%:*}"
    fail "secret   $location:$line ($rule)"
  done <<<"$hits"
}

scan_keys firecrawl-key '(^|[^[:alnum:]_])fc-[A-Za-z0-9_-]{20,}'
scan_keys sk-key '(^|[^[:alnum:]_])sk-[A-Za-z0-9_-]{20,}'
scan_keys sgs-key '(^|[^[:alnum:]_])SGS[A-Za-z0-9_-]{20,}'
scan_keys aws-access-key 'AKIA[0-9A-Z]{16}'
scan_keys github-token 'gh[pousr]_[A-Za-z0-9]{30,}'
scan_keys slack-token 'xox[baprs]-[A-Za-z0-9-]{10,}'
scan_keys private-key 'BEGIN (RSA |EC |OPENSSH )?PRIVATE KEY'

if [ "$key_bad" -eq 0 ]; then
  ok "secret   no high-confidence key prefixes in tracked files"
fi

# D. MCP config secrets must be placeholders, not real values.
mcp_file="ops/mcp.json"
if [ -f "$mcp_file" ]; then
  if mcp_findings="$(python3 - "$mcp_file" <<'PY'
import json
import re
import sys

path = sys.argv[1]
try:
    with open(path, encoding="utf-8") as fh:
        data = json.load(fh)
except Exception:
    print("parse-error")
    sys.exit(0)

header_name = re.compile(r"authorization|api[-_]?key|token|secret", re.I)
env_name = re.compile(r"key|token|secret|password", re.I)
placeholder = re.compile(r"[<>]|\$\{|changeme|example|test|dummy", re.I)


def check_value(label, value, findings):
    if value is None:
        return
    if isinstance(value, str):
        if not value.strip():
            return
    elif isinstance(value, (dict, list)):
        if not value:
            return
        value = json.dumps(value)
    else:
        value = str(value)

    if placeholder.search(value):
        return
    findings.append(label)


findings = []
servers = data.get("mcpServers", {}) if isinstance(data, dict) else {}
if isinstance(servers, dict):
    for server_name, server in servers.items():
        if not isinstance(server, dict):
            continue

        headers = server.get("headers")
        if isinstance(headers, dict):
            for name, value in headers.items():
                if header_name.search(str(name)):
                    check_value(
                        f"mcpServers.{server_name}.headers.{name}", value, findings
                    )

        env = server.get("env")
        if isinstance(env, dict):
            for name, value in env.items():
                if env_name.search(str(name)):
                    check_value(
                        f"mcpServers.{server_name}.env.{name}", value, findings
                    )

for finding in findings:
    print(finding)
PY
)"; then
    if [ "$mcp_findings" = "parse-error" ]; then
      fail "mcp      $mcp_file is not valid JSON"
    elif [ -n "$mcp_findings" ]; then
      while IFS= read -r finding; do
        [ -n "$finding" ] || continue
        fail "mcp      $finding has a non-placeholder secret value"
      done <<<"$mcp_findings"
    else
      ok "mcp      no secret values in $mcp_file"
    fi
  else
    fail "mcp      could not inspect $mcp_file"
  fi
else
  ok "mcp      $mcp_file not present"
fi

# E. Warnings that do not fail the check.
if [ "$have_git" -eq 1 ]; then
  if git ls-files --error-unmatch "$mcp_file" >/dev/null 2>&1; then
    warn "mcp      $mcp_file is tracked; keep it secret-free or use DHI_MCP_CONFIG"

    index_mode="$(git ls-files -s -- "$mcp_file" | awk '{print $1}')"
    if [ "$index_mode" = "100644" ]; then
      warn "mcp      $mcp_file index mode is 100644; Git checkouts are not 600"
    fi
  fi

  if git check-ignore -q backups/ 2>/dev/null || git check-ignore -q backups 2>/dev/null; then
    ok "ignore   backups/ is ignored"
  else
    warn "ignore   backups/ is not ignored"
  fi
else
  if [ -f .gitignore ] && grep -Eq '^[[:space:]]*/?backups/?[[:space:]]*$' .gitignore; then
    ok "ignore   backups/ is ignored"
  else
    warn "ignore   backups/ is not ignored"
  fi
fi

if [ "$fail_count" -gt 0 ]; then
  echo "check-secrets: FAIL ($ok_count ok, $warn_count warn, $fail_count fail)"
  exit 1
fi

echo "check-secrets: PASS ($ok_count ok, $warn_count warn, 0 fail)"
