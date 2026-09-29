#!/usr/bin/env bash
# Start the private w10 QA stack and stop before the browser pass.
#
# `scripts/qa/run.sh` takes a global QA slot *before* it starts the servers, so a tick that
# cannot hold the slot learns nothing at all — not even whether the API answers. This runs the
# same steps with the wait disabled and stops at the line before the walkthrough, so the tick
# can probe the running stack and the browser pass stays where the queue puts it.
#
# The stack is the private one by construction: `QA_STACK=w10` gives the ports 18089/3109/3209,
# the database omnion_qa_w10 and the pm2 names omnion-qa-*-w10. Nothing here touches the main
# writer's default stack.
set -euo pipefail
cd "$(dirname "$0")/../.."
export QA_STACK="${QA_STACK:-w10}"
export QA_API_PORT="${QA_API_PORT:-18089}"
export QA_ADMIN_PORT="${QA_ADMIN_PORT:-3109}"
export QA_WEB_PORT="${QA_WEB_PORT:-3209}"
export QA_API_BIN="${QA_API_BIN:-/dev/shm/w10-target/debug/omnion-api}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/dev/shm/w10-target}"
export PM2_HOME="${PM2_HOME:-/root/.pm2}"
# The slot guards the *browser*, and this run has no browser in it.
export QA_SLOTS=0

if [ ! -x "$QA_API_BIN" ]; then
  export PATH="$HOME/.cargo/bin:$PATH"
  cargo clean -p omnion-core --target-dir "$CARGO_TARGET_DIR" 2>/dev/null || true
  cargo build --target-dir "$CARGO_TARGET_DIR" -p omnion-api
fi

API_NAME="omnion-qa-api-$QA_STACK"
ADMIN_NAME="omnion-qa-admin-$QA_STACK"
WEB_NAME="omnion-qa-web-$QA_STACK"
API_URL="http://127.0.0.1:$QA_API_PORT"
ROOT="$(pwd)"

wait_http() {
  local url="$1" deadline=$(( $(date +%s) + ${2:-120} ))
  while [ "$(date +%s)" -lt "$deadline" ]; do
    local code
    code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 "$url" || true)"
    if [ -n "$code" ] && [ "$code" != "000" ]; then return 0; fi
    sleep 2
  done
  return 1
}

pm2 delete "$API_NAME" "$ADMIN_NAME" "$WEB_NAME" >/dev/null 2>&1 || true

bash scripts/qa/reset-db.sh

# A pm2 log file is appended to, never truncated, so a *stale* error line survives the run that
# would have been clean. Clearing them is one line and it is the difference between reading this
# run's failure and reading the last tick's.
rm -f "$PM2_HOME/logs/$API_NAME-"*.log "$PM2_HOME/logs/$ADMIN_NAME-"*.log 2>/dev/null || true

OMNION_DATABASE_URL="postgres://omnion:omnion@127.0.0.1:5433/omnion_qa_$QA_STACK" \
OMNION_REDIS_URL="redis://127.0.0.1:6380" \
OMNION_PORT="$QA_API_PORT" \
OMNION_ENV=development \
OMNION_CSRF_SECRET="${QA_CSRF_SECRET:-qa-csrf-$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')}" \
  pm2 start "$QA_API_BIN" --name "$API_NAME" --time >/dev/null
wait_http "$API_URL/healthz" 90 || { echo "[qa] API did not answer on :$QA_API_PORT"; pm2 logs "$API_NAME" --lines 20 --nostream || true; exit 1; }

OMNION_API_URL="$API_URL" \
  pm2 start "$ROOT/apps/admin/node_modules/next/dist/bin/next" --name "$ADMIN_NAME" --cwd "$ROOT/apps/admin" --time -- dev --port "$QA_ADMIN_PORT" --hostname 127.0.0.1 >/dev/null
wait_http "http://127.0.0.1:$QA_ADMIN_PORT/login" 180 || { echo "[qa] admin panel did not answer"; pm2 logs "$ADMIN_NAME" --lines 20 --nostream || true; exit 1; }

echo "[qa] w10 stack up: api :$QA_API_PORT, admin :$QA_ADMIN_PORT, db omnion_qa_$QA_STACK"
