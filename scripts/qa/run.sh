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
#
# A pass that never reached the walkthrough has to be able to say so. The artifact
# directory used to be created on the first line of the script, BEFORE this wait and
# before any trap was installed, so a pass that was killed while queued left an empty
# `qa-artifacts/<ts>/` behind. An empty directory reads exactly like a pass that started
# and died: `ls -1t qa-artifacts` shows the newest entry either way, and the next tick
# spends itself deciding which of the two it is looking at. On 2026-09-30 five of nine
# worktrees had empty artifact directories, two of them mine, and two ticks were lost to
# directories that were never passes.
#
# So the directory is created BEFORE the wait, stamped as a queued record, and converted
# into a real pass only once this script has a QA slot and is about to do real work. A pass
# killed while queued therefore leaves a summary that declares itself void instead of an
# empty directory that claims a walkthrough happened.
mkdir -p "$OUT"
write_queued_record() {
  # The stack name is expanded on purpose. Escaping the `$` (which a heredoc does not need for
  # anything else on these lines) leaves the literal text `${QA_STACK:-main}` in the record, so
  # every artifact says it belongs to a stack called `${QA_STACK:-main}` and none of them name
  # the stack they actually wrote to — which is the one field that makes a void record useful.
  local stack_label="${QA_STACK:-main}"
  cat > "$OUT/QUEUED.md" <<EOF
# QA pass queued, not run

- When: $TS · stack: $stack_label
- This pass was created and never reached the browser walkthrough.

The artifact directory is created before the QA slot is taken, so a pass killed while
queued would otherwise leave an empty directory that reads like a pass that ran. This file
is the record of the queue; \`summary.json\` marks the pass void until the walkthrough runs.
EOF
  python3 - "$OUT/summary.json" <<'PY'
import json, sys
json.dump(
    {
        "void": True,
        "reason": "queued-and-never-ran",
        "detail": "the pass did not reach the browser walkthrough; it was killed or timed out while waiting for a QA slot",
        "counts": {"clicks": 0, "screenshots": 0, "shotFailures": 0},
        "findings": [],
    },
    open(sys.argv[1], "w"),
    indent=2,
)
PY
}
write_queued_record
# Stamped as soon as this pass owns a place and is about to do real work. `release` below
# is the only trap once a place is held; this one exists for the window before that.
QA_PASS_STARTED=0
queued_exit() {
  [ "${QA_PASS_STARTED:-0}" = "1" ] && return 0
  printf '[qa] queued pass never reached the walkthrough; void record kept in %s\n' "$OUT" >&2
}
trap queued_exit EXIT INT TERM

QA_SLOT_PID=""
# Free the place whenever this pass ends, however it ends.
if [ "${QA_SLOTS:-1}" != "0" ]; then
  step "waiting for a QA slot (max ${QA_SLOTS:-1} concurrent pass)"
  QA_SLOT_PID="$(QA_SLOT_WAIT="${QA_SLOT_WAIT:-1800}" bash "$(dirname "${BASH_SOURCE[0]}")/qa-slot.sh" | tail -n 1)"
  export QA_SLOT_PID
fi

if [ -n "$QA_SLOT_PID" ]; then
  trap 'kill "$QA_SLOT_PID" 2>/dev/null || true' EXIT INT TERM
fi

# The pass has a place and is about to do real work, so the queued record is retired: the
# walkthrough owns `summary.json` from here, and a stale void summary next to a real one is
# the same ambiguity this change exists to remove.
QA_PASS_STARTED=1
rm -f "$OUT/QUEUED.md"

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

# A place in the global slot is not the same thing as the right to use *this* stack.
#
# `qa-slot.sh` counts places in one shared directory, so it correctly serialises two passes
# that want two DIFFERENT stacks — each stack owns its own database and its own ports, and
# they are safe to run side by side. What nothing stopped was a second pass taking the SAME
# stack, and that is destructive rather than merely wasteful: both passes resolve to
# `QA_DB_NAME` and to ports 18080/3100/3200, so the later one runs `reset-db.sh` —
# `DROP DATABASE … WITH (FORCE)` — while the earlier one is mid-walkthrough.
#
# Observed on 2026-10-01 (tick 90). Tick 89's pass was still walking when the tick ended,
# so tick 90 started a second pass on the same stack:
#
#   02:47:57  tick-89 pass: reset-db.sh drops and recreates omnion_qa
#   02:48:07  tick-89 pass: API boots, "no accounts exist yet"
#   02:48:14  a user appears: qa-sample@omnion.test / "QA Provider" / organization_id NULL
#   02:48:16  tick-89 pass: session created for that user
#
# `qa-sample@omnion.test` and "QA Provider" are literal return values of the walkthrough's own
# `sampleValueFor()` / `fillSubtree()` helpers — the generic form filler, not a credential.
# Tick-89 had filled a dialog on its way past. Tick 90 then opened `/`, found an account
# already present, was told `needs_setup: false`, correctly skipped the wizard, and failed to
# sign in as `CREDS.email` — an account that had never been created. The sign-in failure had
# nothing to do with sign-in, and every browser box in wave 1 stayed open for a reason that
# reads exactly like a broken product.
#
# The lock is per stack, so sibling stacks keep their parallelism and only the same stack is
# serialised. It is taken around the whole pass rather than just the reset, because the second
# pass would otherwise `pm2 delete` the first pass's servers three lines later — the reset is
# only the first of several ways two passes on one stack destroy each other.
QA_STACK_LOCK="${QA_STACK_LOCK_DIR:-/tmp/omnion-qa-stack}-${STACK}.lock"
# `9>>` and not `9>`: the redirect mode is the whole bug. `9>` truncates on open, so a waiter
# empties the file *as it opens it* and then reads back the zero bytes it just wrote — the
# holder's recorded pid is destroyed by the act of asking who the holder is, and the refusal
# degrades to "unknown" in exactly the situation where an operator wants the pid most. Appending
# opens without truncating, so the waiter's read sees what the holder wrote.
exec 9>>"$QA_STACK_LOCK"
if ! flock -n 9; then
  holder="$(head -n 1 "$QA_STACK_LOCK" 2>/dev/null | tr -d '[:space:]' || true)"
  if [ -z "$holder" ] || ! kill -0 "$holder" 2>/dev/null; then
    holder="unknown (the pass holding this stack did not record a live pid)"
  fi
  echo "[qa] stack '${STACK}' already has a pass running (pid ${holder}); refusing to start a second one" >&2
  echo "[qa] two passes on one stack reset the database out from under each other — wait for the running pass to finish" >&2
  exit 4
fi
# Written after the lock is held, so a waiter reports the pass that actually owns the stack
# rather than the one that merely got there first. `BASHPID`, not `$$`: `$$` is the pid of the
# *shell* and does not change inside a subshell or a `bash -c`, so a pass launched through one
# would record its parent's pid and the refusal would name a process that has nothing to do
# with the stack.
printf '%s\n' "$BASHPID" >&9

step "resetting the QA database"
bash scripts/qa/reset-db.sh

step "API on :$API_PORT (database $QA_DB)"
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

# The precondition every org-scoped screen in this report depends on, checked BEFORE the walk
# rather than inferred from it afterwards. A pass that runs with no organization answers every
# `/sites`, `/notifications`, `/organizations` and `/reliability/*` read with 403, and still
# produces a complete report: per-page diagnostics, a vision review, a high-finding count. The
# screens render their shell, their title and their `h1` regardless, so each page looks healthy
# while every number under it was refused — which is how a pass can measure three new screens and
# report "clean" about a tenant that does not exist.
#
# It is one query, and it is the difference between failing in the first thirty seconds and
# spending an hour measuring an installation that has no tenant.
qa_scalar() {
  docker exec "${QA_PG_CONTAINER:-omnion-postgres}" psql -U omnion -d "$QA_DB" -t -A -c "$1" 2>/dev/null || echo ""
}
# The guard below is correct — and it had no way to be satisfied. The only thing that created the
# organization was the browser's first-run wizard, and the wizard is the exact thing this branch's
# own `wizard-gate.test.cjs` exists because it does not always reach its submit on a cold dev
# server. So the pass reset the database, required a tenant, and could not make one: every pass
# aborted at the guard before measuring a single screen. A precondition with no way to be met is a
# harness that can only fail.
#
# The seed runs BEFORE the guard, over the API and through the same endpoints the wizard calls —
# never by inserting rows. A SQL-inserted organization would satisfy the count while leaving
# `onboarding_state`, the membership row and the site row unwritten, and each of those is what an
# org-scoped read joins against: the pass would measure screens answering 403 and report it as the
# product's answer, which is the precise failure the guard was added to catch.
if [ "$(qa_scalar 'select count(*) from organizations')" = "0" ]; then
  step "seeding the QA tenant (the wizard creates it in a browser; this does it over the API)"
  node scripts/qa/ensure-organization.mjs || {
    echo "[qa] FATAL: the QA tenant could not be seeded, so the walk below would measure 403s."
    echo "[qa] Check the API log for the reason; the seeder prints the status and body it got."
    exit 1
  }
fi

QA_ORGS="$(qa_scalar 'select count(*) from organizations')"
QA_USERS="$(qa_scalar 'select count(*) from users')"
if [ "$QA_ORGS" = "0" ] || [ -z "$QA_ORGS" ]; then
  echo "[qa] FATAL: ${QA_DB} has ${QA_USERS:-0} user(s) and ${QA_ORGS:-no} organization(s)."
  echo "[qa] The browser creates the organization through the first-run wizard; if it did not run,"
  echo "[qa] every org-scoped screen below is answered 403 and the report describes nothing."
  echo "[qa] Check the wizard's POST /api/v1/onboarding/organization in the API log."
  exit 1
fi
step "precondition ok: ${QA_USERS} user(s), ${QA_ORGS} organization(s) in ${QA_DB}"

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
