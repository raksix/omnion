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
export CARGO_SLOTS="${QA_CARGO_SLOTS:-2}"

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
  # `QA_SLOT_OWNER_PID=$$` is THIS shell, not the qa-slot.sh child, and that distinction is the
  # fix: a pass killed with SIGKILL — the OOM killer, a terminal timeout — cannot run its EXIT
  # trap, so the holder it left behind is reparented to init and holds the one place for the
  # life of the box. The holder watches the pid it is given, so it exits on its own when the
  # pass dies the way no trap can catch. (Main's qa-slot.sh reads the variable; only the call
  # site here is mine.)
  QA_SLOT_PID="$(QA_SLOT_OWNER_PID="$$" QA_SLOT_WAIT="${QA_SLOT_WAIT:-1800}" \
    bash "$(dirname "${BASH_SOURCE[0]}")/qa-slot.sh" | tail -n 1)"
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
  # Turbopack leaves a build cache behind when the server is killed, and the cache is
  # the largest thing any worktree holds: ten stacks held 13 GB of it and filled the
  # disk twice. The next pass rebuilds what it needs, so this is pure waste — but only
  # drop it when the pass actually ran, so a stack that failed to start keeps its cache.
  if [ "${QA_KEEP_NEXT:-0}" != "1" ]; then
    rm -rf "$ROOT/apps/admin/.next" "$ROOT/apps/web/.next" 2>/dev/null || true
  fi
}
# One trap, both cleanups: a second trap would replace the first and leave the slot held.
release() {
  [ -n "$QA_SLOT_PID" ] && kill "$QA_SLOT_PID" 2>/dev/null
  stop_stack
  return 0
}
trap release EXIT INT TERM
# Only the exit path drops the build cache: the pre-pass call below is here to clear
# stale servers, and deleting .next there would throw away a warm cache every tick and
# turn each QA pass into a cold Turbopack build.
QA_KEEP_NEXT=1 stop_stack

step "resetting the QA database"
bash scripts/qa/reset-db.sh

step "API on :$API_PORT (database omnion_qa)"
# A stale binary replays the *old* SQL: sqlx embeds `database/migrations/*.sql` at compile time, so
# a migration edited after the last build is silently the previous version — and a syntax error in
# it looks like a duplicate table on the next attempt. Build when the binary is missing OR older
# than the newest migration, which is cheap when nothing changed and correct when something did.
if [ ! -x target/debug/omnion-api ] \
   || [ -n "$(find database/migrations -name '*.sql' -newer target/debug/omnion-api -print -quit)" ]; then
  step "building the API (first pass, or a migration changed since the last build)"
  # Eight writers share six cores: a global semaphore keeps at most CARGO_SLOTS builds
  # compiling at once instead of every pass grabbing all six threads for itself.
  "$(dirname "$0")/cargo-slot.sh" cargo build -p omnion-api
fi
# `OMNION_CSRF_SECRET` decides whether a cookie-authenticated mutation is refused before its
# handler runs. Without one the QA API refuses EVERY write with `csrf_unavailable`, so a
# walkthrough that saves a header policy, uploads a file or takes a backup would record screens
# that "work" while the API answered 403 the whole time -- and because that refusal is the
# documented behaviour of a deployment *without* a secret, it reads as the product being correct
# rather than the harness being under-configured. It is a throwaway value: the process points at
# a database that was dropped two lines above and listens on loopback.
#
# The comment sits here rather than inside the command because a `#` line between two backslash
# continuations is not a comment: bash keeps reading the command, `#` and the words after it
# become its arguments, and `pm2 start` is handed a stray name it never recovers from.
if pm2 describe "$API_NAME" >/dev/null 2>&1; then
  pm2 restart "$API_NAME" >/dev/null
else
  OMNION_DATABASE_URL="postgres://omnion:omnion@127.0.0.1:5433/$QA_DB_NAME" \
  OMNION_REDIS_URL="redis://127.0.0.1:6380" \
  OMNION_PORT="$API_PORT" \
  OMNION_ENV=development \
  OMNION_CSRF_SECRET="${QA_CSRF_SECRET:-qa-pass-throwaway-secret-not-a-real-key}" \
    pm2 start "$ROOT/target/debug/omnion-api" --name "$API_NAME" --time >/dev/null
fi
wait_http "$API_URL/healthz" 90 || { echo "[qa] API did not answer on :$API_PORT"; pm2 logs "$API_NAME" --lines 20 --nostream || true; exit 1; }
curl -fsS "$API_URL/readyz" >/dev/null || { echo "[qa] API /readyz is not healthy"; curl -sS "$API_URL/readyz" || true; exit 1; }

step "admin panel on :$ADMIN_PORT"
NEXT_ADMIN="$ROOT/apps/admin/node_modules/next/dist/bin/next"
if pm2 describe "$ADMIN_NAME" >/dev/null 2>&1; then
  pm2 restart "$ADMIN_NAME" >/dev/null
else
  OMNION_API_URL="$API_URL" \
    pm2 start "$NEXT_ADMIN" --name "$ADMIN_NAME" --cwd "$ROOT/apps/admin" --time -- dev --port "$ADMIN_PORT" --hostname 127.0.0.1 >/dev/null
fi
wait_http "http://127.0.0.1:$ADMIN_PORT/login" 150 || { echo "[qa] admin panel did not answer"; pm2 logs "$ADMIN_NAME" --lines 20 --nostream || true; exit 1; }

step "public renderer on :$WEB_PORT"
NEXT_WEB="$ROOT/apps/web/node_modules/next/dist/bin/next"
if pm2 describe "$WEB_NAME" >/dev/null 2>&1; then
  pm2 restart "$WEB_NAME" >/dev/null
else
  OMNION_API_URL="$API_URL" \
    pm2 start "$NEXT_WEB" --name "$WEB_NAME" --cwd "$ROOT/apps/web" --time -- dev --port "$WEB_PORT" --hostname 127.0.0.1 >/dev/null
fi
wait_http "http://127.0.0.1:$WEB_PORT/" 150 || { echo "[qa] public renderer did not answer"; pm2 logs "$WEB_NAME" --lines 20 --nostream || true; exit 1; }

# `QA_ONLY` narrows the pass to named routes and depth passes. The default runs every one of
# them, which is the right thing for a full acceptance run and the wrong thing for a loop that
# has just built two screens and needs them proven before the tick ends. It is a filter on the
# walk, never on the harness around it: the stack, the reset, the vision review and the report
# all run exactly as they do for a full pass.
QA_ONLY_ARGS=()
[ -n "${QA_ONLY:-}" ] && QA_ONLY_ARGS=(--only="$QA_ONLY")

step "browser walkthrough${QA_ONLY:+ (focused: $QA_ONLY)}"
node scripts/qa/walkthrough.cjs --url "http://127.0.0.1:$ADMIN_PORT" --web "http://127.0.0.1:$WEB_PORT" --out "$OUT" "${QA_ONLY_ARGS[@]}"

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
