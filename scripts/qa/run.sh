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
  # Invoked through `bash` for the same reason as cargo-slot.sh below: the executable bit is
  # not carried by every clone, and a pass that dies holding the slot blocks the other seven
  # writers behind it.
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
# A gate that only builds on a missing binary silently exercises the last binary that happened
# to be there: the stack restarts fine, every request answers, and the route or column added this
# tick is simply not there — which reads as a broken screen rather than as a stale build. That is
# not hypothetical: this file's `if [ ! -x … ]` once made a REQ-close pass report "green" against
# a binary that predated the migration under test. A newer mtime is the only signal available
# without asking cargo, and it is exactly the one that matters. `database/migrations` is in the
# list because sqlx embeds the SQL at COMPILE time — an edited migration with an older binary
# replays the old statement, and the failure ("column does not exist" / "duplicate table")
# reads like a missing `alter` or a re-run rather than like a stale build. Sources are in it
# too, which is the half a migration-only check misses: a route added this tick compiles into
# nothing until the next full build.
NEEDS_BUILD=0
if [ ! -x target/debug/omnion-api ]; then
  NEEDS_BUILD=1
  step "building the API (no binary yet)"
elif [ -n "$(find apps/api crates database/migrations Cargo.toml -newer target/debug/omnion-api -print -quit 2>/dev/null)" ]; then
  NEEDS_BUILD=1
  step "building the API (sources or migrations are newer than the binary)"
fi
if [ "$NEEDS_BUILD" = "1" ]; then
  # Eight writers share six cores: a global semaphore keeps at most CARGO_SLOTS builds
  # compiling at once instead of every pass grabbing all six threads for itself.
  #
  # `bash <script>`, not `<script>`: git records the executable bit as a MODE, so a helper
  # added in one commit and invoked in the next arrives 100644 on every fresh clone and every
  # worktree that merges it, and the pass dies at the build step with "Permission denied"
  # AFTER it has reset the database and taken the QA slot. Running it through the interpreter
  # the `set -euo pipefail` above already implies costs nothing and cannot be broken by a mode
  # bit; a harness that only runs for the writer who happened to chmod it locally is a
  # harness that fails on every other writer.
  # `CARGO_INCREMENTAL=0`: eight writers share one box and their target directories sit side
  # by side, and a build that is interrupted or raced leaves the incremental session's
  # `dep-graph.bin` / `query-cache.bin` half-written. The next build then fails with
  # "failed to move dependency graph ... No such file or directory (os error 2)" - which is
  # os error 2, NOT the os error 28 of a full tmpfs, and reads like a source problem rather
  # than a cache one. Incremental compilation buys minutes on a developer's machine and buys
  # nothing for a pass that must produce a binary it can trust, so the build that a pass
  # depends on does without it. (The unit tests keep it: they are re-run constantly and are
  # not gating a browser against a stack somebody else may reset.)
  CARGO_INCREMENTAL=0 bash "$(dirname "$0")/cargo-slot.sh" cargo build -p omnion-api
fi
# The CSRF secret is the one variable the platform refuses to invent: a deployment that sets
# none still boots, and every cookie-authenticated mutation then answers 403
# `csrf_unavailable` (apps/api/src/headers_middleware.rs). Refusing writes beats silently
# dropping the control, so the product is right -- but a QA stack started without the secret
# loses EVERY write, and the pass reports that as a product defect: the builder could not
# create its rule, the rule list stayed empty, and every depth note downstream read as a
# broken screen rather than a stack that cannot write.
#
# It is a test-only value derived from the stack name, it never leaves this box, and it is
# passed to the RESTART branch too on purpose: `pm2 restart` re-reads the env the process was
# created with, so a stack started before this line existed keeps the old (empty) env and
# stays broken for every later pass until it is deleted.
API_ENV=(
  "OMNION_DATABASE_URL=postgres://omnion:omnion@127.0.0.1:5433/$QA_DB_NAME"
  "OMNION_REDIS_URL=redis://127.0.0.1:6380"
  "OMNION_PORT=$API_PORT"
  "OMNION_ENV=development"
)
API_ENV+=("OMNION_CSRF_SECRET=qa-${QA_STACK:-default}-$(printf %s "$QA_DB_NAME" | cksum | cut -d' ' -f1)")

if pm2 describe "$API_NAME" >/dev/null 2>&1; then
  env "${API_ENV[@]}" pm2 restart "$API_NAME" --update-env >/dev/null
else
  env "${API_ENV[@]}" pm2 start "$ROOT/target/debug/omnion-api" --name "$API_NAME" --time >/dev/null
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

# Every rule belongs to a tenant, and a tenant is not something the wizard leaves behind: it
# creates the owner account and stops there, so a freshly reset database has a platform account
# whose organization list is EMPTY. The rule editor then refuses its own save with "Choose an
# organization before saving a rule.", the list stays empty, and every depth note downstream
# reads as a broken screen. The tenant picker does not even render (`organizations.length > 1`),
# so there is nothing on the page to click -- this is a harness gap, not a product defect.
#
# `POST /onboarding/organization` exists for exactly this state and refuses once a tenant exists,
# so this is idempotent: it fills the gap when it is there and is a no-op when it is not.
step "ensuring the QA organization exists"
node scripts/qa/ensure-organization.mjs --url "$API_URL" --admin "http://127.0.0.1:$ADMIN_PORT" || {
  echo "[qa] the QA organization could not be created; rule screens will report empty"
}

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
