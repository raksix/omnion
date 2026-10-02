#!/usr/bin/env bash
# Omnion QA — one full pass.
#
#   reset QA database → API(:18080) + admin(:3100) + web(:3200) on a disposable stack
#   → browser walkthrough (screenshots + click every control) → vision review
#   → qa-artifacts/<ts>/report.md + docs/qa/QA-LATEST.md
#
# Servers live under pm2 (omnion-qa-api / omnion-qa-admin / omnion-qa-web) so a pass costs
# seconds, not minutes. They only ever point at the QA database (omnion_qa).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

TS="$(date -u +%Y%m%d-%H%M%S)"
OUT="$ROOT/qa-artifacts/$TS"
mkdir -p "$OUT"

API_PORT="${QA_API_PORT:-18080}"
ADMIN_PORT="${QA_ADMIN_PORT:-3100}"
WEB_PORT="${QA_WEB_PORT:-3200}"
# A stack prefix lets several worktrees run their own QA pass side by side: the ports
# above are then overridable per stack and the three pm2 processes get their own names,
# so no pass restarts or talks to another worktree's servers.
STACK="${QA_STACK:-main}"
API_NAME="omnion-qa-api-$STACK"
ADMIN_NAME="omnion-qa-admin-$STACK"
WEB_NAME="omnion-qa-web-$STACK"
API_URL="http://127.0.0.1:$API_PORT"
# Every stack owns its database, or two passes would drop each other's rows.
QA_DB_NAME="omnion_qa"
[ "$STACK" != "main" ] && QA_DB_NAME="omnion_qa_$STACK"
QA_DB="$QA_DB_NAME"
export QA_DB
export NODE_PATH="${QA_NODE_PATH:-/root/test-hermes/node_modules}"
export QA_CHROME="${QA_CHROME:-/root/.cache/ms-playwright/chromium-1234/chrome-linux64/chrome}"
export PATH="$HOME/.cargo/bin:$PATH"
# A browser pass on a six-core box is CPU work. Without this, seven concurrent passes
# put the machine at a load average of 20 with a half-full swap. Half the cores per
# build keeps a pass readable and leaves the rest of the box alone.
export CARGO_BUILD_JOBS="${QA_CARGO_JOBS:-3}"

step() { printf '\n[qa] %s\n' "$*"; }

wait_http() { # url, seconds
  local url="$1" deadline=$(( $(date +%s) + ${2:-120} ))
  while [ "$(date +%s)" -lt "$deadline" ]; do
    local code
    code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 "$url" || true)"
    if [ -n "$code" ] && [ "$code" != "000" ]; then return 0; fi
    sleep 2
  done
  return 1
}

# The browser pass is the heaviest step in the loop and several worktrees may run
# side by side. Take a slot first so the passes queue instead of all landing on the
# machine at once; the wait is bounded and then the pass proceeds regardless.
# One pass at a time on this box: it is the difference between load 20 and load 6.
QA_SLOT_PID=""
if [ "${QA_SLOTS:-1}" != "0" ]; then
  step "waiting for a QA slot (max ${QA_SLOTS:-1} concurrent pass)"
  QA_SLOT_PID="$(QA_SLOT_WAIT="${QA_SLOT_WAIT:-1800}" bash "$(dirname "${BASH_SOURCE[0]}")/qa-slot.sh" | tail -n 1)"
  export QA_SLOT_PID
fi
# Free the place whenever this pass ends, however it ends.
if [ -n "$QA_SLOT_PID" ]; then
  trap 'kill "$QA_SLOT_PID" 2>/dev/null || true' EXIT INT TERM
fi

# The QA servers are disposable: a pass starts them, walks, and the next pass can
# start them again. Leaving seven stacks of three servers running between passes cost
# this box about 5 GB of resident memory. Stop this stack's servers now so a pass
# begins from a clean set and the box is not carrying yesterday's processes.
stop_stack() {
  pm2 delete "$API_NAME" "$ADMIN_NAME" "$WEB_NAME" >/dev/null 2>&1 || true
}
# One trap, both cleanups: a second trap would replace the first and leave the slot held.
release() {
  [ -n "$QA_SLOT_PID" ] && kill "$QA_SLOT_PID" 2>/dev/null
  stop_stack
  return 0
}
trap release EXIT INT TERM
stop_stack

step "resetting the QA database"
bash scripts/qa/reset-db.sh

step "API on :$API_PORT (database omnion_qa)"
# A stale binary replays the *old* SQL: sqlx embeds `database/migrations/*.sql` at compile time, so
# a migration edited after the last build is silently the previous version — and a syntax error in
# it looks like a duplicate table on the next attempt. Build when the binary is missing OR older
# than the newest migration, which is cheap when nothing changed and correct when something did.
# A writer whose worktree cannot hold a build (a full /mnt/apopic) builds elsewhere and points
# at the result. Without this the pass rebuilds into a full disk and then reports "script not
# found" for a binary that exists — a failure that reads as a broken API rather than a full
# filesystem. `QA_API_BIN` is the escape hatch; the default is unchanged.
QA_API_BIN="${QA_API_BIN:-$ROOT/target/debug/omnion-api}"
if [ ! -x "$QA_API_BIN" ] \
   || [ -n "$(find database/migrations -name '*.sql' -newer "$QA_API_BIN" -print -quit)" ]; then
  step "building the API (missing or stale at $QA_API_BIN)"
  step "building the API (first pass, or a migration changed since the last build)"
  # `CARGO_TARGET_DIR` has to be honoured, or a caller that points `QA_API_BIN` at an
  # out-of-tree build gets a rebuild into the default `target/` — the full disk this override
  # exists to avoid.
  # `omnion-core` is the crate that owns `sqlx::migrate!("../../database/migrations")`, and
  # sqlx embeds the directory at *that* crate's compile. So a renamed or added migration does
  # not reach the binary through `-p omnion-api`: cargo sees the api crate as up to date, prints
  # `Finished` in under a minute, and the API then dies on `migration N was previously applied
  # but has been modified` — a failure that reads as a database problem and is a build-graph
  # one. The fix is to make the embedding crate recompile, which is a cache problem rather
  # than a source one.
  cargo clean -p omnion-core --target-dir ${CARGO_TARGET_DIR:-target} 2>/dev/null || true
  cargo build ${CARGO_TARGET_DIR:+--target-dir "$CARGO_TARGET_DIR"} -p omnion-api
fi
# `delete` then `start`, never `restart`. That looks like a pointless extra second of
# startup and it is the most expensive line in this file.
#
# `pm2 restart` **keeps the environment the process was first started with**. This script
# drops and recreates the QA database a moment earlier, so a process left over from a pass
# that used a different `QA_DB` still holds the *old* connection string — and after the drop,
# "that database does not exist" comes back as a restart loop whose log reads `migration 19
# was previously applied but is missing in the resolved migrations`. That sentence names a
# migration, so it sends you auditing 41 SQL files that are all perfectly fine.
#
# It happened here for real: an earlier attempt left a process pointing at the main writer's
# `omnion_qa` while every later pass reset `omnion_qa_w10`, and three consecutive passes
# inherited it. The ports, the CSRF secret and the binary path are inherited the same way, and
# each of those has changed between passes at least once.
#
# The binary pm2 starts must also be `QA_API_BIN`, not the default path: a writer that built
# out of tree passed a working `QA_API_BIN` through the staleness check and then had pm2 report
# `Script not found` for a path that was never built.
#
# The CSRF guard *refuses* a cookie-authenticated mutation when no secret is configured
# (REQ-012: refuse, never skip), which is right in production and useless in a QA stack — every
# create, install and delete in the walkthrough answers 403 and the pass reports a product
# defect that does not exist. The secret is a throwaway for a disposable database that is
# dropped on every pass, and it is generated per pass so a half-finished run cannot leave a
# token a later one still validates.
pm2 delete "$API_NAME" >/dev/null 2>&1 || true
OMNION_DATABASE_URL="postgres://omnion:omnion@127.0.0.1:5433/$QA_DB_NAME" \
OMNION_REDIS_URL="redis://127.0.0.1:6380" \
OMNION_PORT="$API_PORT" \
OMNION_ENV=development \
OMNION_CSRF_SECRET="${QA_CSRF_SECRET:-qa-csrf-$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')}" \
  pm2 start "$QA_API_BIN" --name "$API_NAME" --time >/dev/null
wait_http "$API_URL/healthz" 90 || { echo "[qa] API did not answer on :$API_PORT"; pm2 logs "$API_NAME" --lines 20 --nostream || true; exit 1; }
curl -fsS "$API_URL/readyz" >/dev/null || { echo "[qa] API /readyz is not healthy"; curl -sS "$API_URL/readyz" || true; exit 1; }

step "admin panel on :$ADMIN_PORT"
NEXT_ADMIN="$ROOT/apps/admin/node_modules/next/dist/bin/next"
# Same reasoning as the API: `restart` inherits the environment and the port of the
# process that was there before, and both have moved between passes.
pm2 delete "$ADMIN_NAME" >/dev/null 2>&1 || true
OMNION_API_URL="$API_URL" \
    pm2 start "$NEXT_ADMIN" --name "$ADMIN_NAME" --cwd "$ROOT/apps/admin" --time -- dev --port "$ADMIN_PORT" --hostname 127.0.0.1 >/dev/null

wait_http "http://127.0.0.1:$ADMIN_PORT/login" 150 || { echo "[qa] admin panel did not answer"; pm2 logs "$ADMIN_NAME" --lines 20 --nostream || true; exit 1; }

step "public renderer on :$WEB_PORT"
NEXT_WEB="$ROOT/apps/web/node_modules/next/dist/bin/next"
# Same reasoning as the API: `restart` inherits the environment and the port of the
# process that was there before, and both have moved between passes.
pm2 delete "$WEB_NAME" >/dev/null 2>&1 || true
OMNION_API_URL="$API_URL" \
    pm2 start "$NEXT_WEB" --name "$WEB_NAME" --cwd "$ROOT/apps/web" --time -- dev --port "$WEB_PORT" --hostname 127.0.0.1 >/dev/null

wait_http "http://127.0.0.1:$WEB_PORT/" 150 || { echo "[qa] public renderer did not answer"; pm2 logs "$WEB_NAME" --lines 20 --nostream || true; exit 1; }

step "browser walkthrough"
node scripts/qa/walkthrough.cjs --url "http://127.0.0.1:$ADMIN_PORT" --web "http://127.0.0.1:$WEB_PORT" --out "$OUT"

step "vision review"
node scripts/qa/vision-review.cjs --dir "$OUT" || echo "[qa] vision review skipped"

step "summary"
node -e '
const fs = require("fs");
const path = require("path");
const out = process.argv[1];
const label = process.argv[2] || "main";
const summary = JSON.parse(fs.readFileSync(path.join(out, "summary.json"), "utf8"));
const visionPath = path.join(out, "findings", "vision.json");
const vision = fs.existsSync(visionPath) ? JSON.parse(fs.readFileSync(visionPath, "utf8")) : { skipped: "not run" };
const doc = [
  `# Omnion QA — latest pass (${label})`,
  "",
  `- When: ${summary.startedAt || "?"} · artifacts: \`${path.relative(process.cwd(), out)}\``,
  `- Interactions: ${summary.counts?.clicks ?? 0} clicks · ${summary.counts?.filled ?? 0} field fills · ${summary.counts?.forms ?? 0} form submissions · ${summary.counts?.screenshots ?? 0} screenshots`,
  `- Console errors: ${summary.counts?.consoleErrors ?? 0} · failed requests: ${summary.counts?.failedRequests ?? 0} · dialogs: ${summary.counts?.dialogs ?? 0}`,
  `- Programmatic findings: ${summary.findings?.length ?? 0} (high ${summary.bySeverity?.high ?? 0} · medium ${summary.bySeverity?.medium ?? 0} · low ${summary.bySeverity?.low ?? 0})`,
  `- Vision issues: ${vision.issues ? vision.issues.length : "skipped"}${vision.skipped ? ` (${vision.skipped})` : ""}`,
  "",
  "## Top findings",
  "",
  ...(summary.findings || []).filter((f) => f.severity !== "low").slice(0, 25).map((f) => `- **[${f.severity}] ${f.kind}** — ${f.detail}`),
  "",
].join("\n");
fs.mkdirSync("docs/qa", { recursive: true });
fs.writeFileSync(`docs/qa/QA-LATEST-${label}.md`, doc);
console.log(`docs/qa/QA-LATEST-${label}.md updated`);
' "$OUT" "$STACK"

step "done"
echo "QA_ARTIFACTS=$OUT"
echo "QA_REPORT=$OUT/report.md"
grep -E "^QA_|^VISION_" "$OUT"/*.log 2>/dev/null || true
