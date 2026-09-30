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
  # QA_SLOT_OWNER is this script's own pid: the reaper has to be able to tell a pass that is
  # still walking from a place its owner walked away from, and the holder it holds the place
  # with cannot answer that — the holder is a background job of qa-slot.sh and is reparented
  # the moment that script exits, which is the *successful* case. On a host with a subreaper
  # (systemd --user here) an abandoned holder and a live one report the same ppid, so a
  # ppid test is not merely unreliable here, it never fires at all. The pass is the only
  # party that knows whether its own EXIT trap will still run, so it says so.
  QA_SLOT_PID="$(QA_SLOT_OWNER="$$" QA_SLOT_WAIT="${QA_SLOT_WAIT:-1800}" \
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

step "API on :$API_PORT (database $QA_DB_NAME)"
# The binary lives in `$CARGO_TARGET_DIR` when the caller sets one, and this box has seven
# writers sharing one 60G mount — so building into the worktree's own `target/` is how that
# mount reaches 100% and how `cargo build` starts failing with "No space left on device". The
# established answer is `CARGO_TARGET_DIR=/dev/shm/<writer>-target`, which this script ignored
# twice over: it looked for the binary at the hardcoded `target/debug/omnion-api` and it told
# pm2 to start that same path. The pass then died at `wait_http` with the API never listening,
# reporting nothing about the code under test — the binary was in `/dev/shm` the whole time and
# perfectly good. Honour the variable at both places, and say where the binary is so a failed
# pass names a path instead of a symptom.
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
API_BIN="$TARGET_DIR/debug/omnion-api"
step "API binary: $API_BIN"
# A stale binary replays the *old* SQL: sqlx embeds `database/migrations/*.sql` at compile time, so
# a migration edited after the last build is silently the previous version — and a syntax error in
# it looks like a duplicate table on the next attempt. Build when the binary is missing OR older
# than the newest migration, which is cheap when nothing changed and correct when something did.
if [ ! -x "$API_BIN" ] \
   || [ -n "$(find database/migrations -name '*.sql' -newer "$API_BIN" -print -quit)" ]; then
  step "building the API (first pass, or a migration changed since the last build)"
  # Eight writers share six cores: a global semaphore keeps at most CARGO_SLOTS builds
  # compiling at once instead of every pass grabbing all six threads for itself.
  "$(dirname "$0")/cargo-slot.sh" cargo build -p omnion-api
fi
# A pm2 entry that exists but points at a binary which is no longer there is *worse* than no
# entry: `pm2 restart` succeeds, nothing listens, and the pass dies at `wait_http` blaming the
# product. This is the second half of the same bug as the hardcoded path above — once a pass has
# started the API from `$CARGO_TARGET_DIR`, the registered script path is the tmpfs copy, and a
# later pass that builds somewhere else inherits an entry that can only fail. Compare the
# registered path to the one this pass just built and re-register when they differ.
# pm2 draws its table with a box character followed by a NON-BREAKING space (U+00A0), so a sed
# pattern that matches an ordinary space extracts nothing at all. Strip the non-breaking spaces
# and the box characters first, then cut the field.
# `pm2 describe` exits non-zero for a process that does not exist, and under `set -e` a
# failing command inside `$( )` aborts the *whole script* — so the very first pass on a
# fresh stack (nothing registered yet, which is exactly when this runs) died here with no
# message and no pass. The `|| true` is not a workaround: the exit code carries nothing
# this block needs, and the value it produces is the empty string that the `if` below
# already handles.
RUNNING_BIN="$(pm2 describe "$API_NAME" 2>/dev/null \
  | tr -d '\302\240\342\224\202' \
  | sed -n 's/^.*script path *//p' | head -1 | sed 's/[[:space:]]*$//' || true)"
if [ -n "$RUNNING_BIN" ] && [ "$RUNNING_BIN" != "$API_BIN" ]; then
  step "the pm2 entry runs $RUNNING_BIN, not $API_BIN — re-registering"
  pm2 delete "$API_NAME" >/dev/null 2>&1 || true
fi
# The admin account is seeded from the environment on *every* boot, not only when the database is
# empty. The DB reset above drops every account, so a pass that restarts an already-registered
# process boots an API with no user at all: the panel then serves `/login` instead of `/setup`,
# the walkthrough has nothing to sign in with, and the pass dies at "could not sign in after
# wizard" — a failure that names the harness and says nothing about the code under test. Passing
# the seed on the restart path too is what keeps the credentials in `walkthrough.cjs` and the
# account the API creates the same pair.
QA_ADMIN_EMAIL="${QA_ADMIN_EMAIL:-qa-owner@omnion.test}"
QA_ADMIN_PASSWORD="${QA_ADMIN_PASSWORD:-OmnionQa-Passw0rd-2026!}"
QA_ADMIN_NAME="${QA_ADMIN_NAME:-QA Owner}"
pm2 delete "$API_NAME" >/dev/null 2>&1 || true
OMNION_DATABASE_URL="postgres://omnion:omnion@127.0.0.1:5433/$QA_DB_NAME" \
OMNION_REDIS_URL="redis://127.0.0.1:6380" \
OMNION_PORT="$API_PORT" \
OMNION_ENV=development \
OMNION_ADMIN_EMAIL="$QA_ADMIN_EMAIL" \
OMNION_ADMIN_PASSWORD="$QA_ADMIN_PASSWORD" \
OMNION_ADMIN_NAME="$QA_ADMIN_NAME" \
  pm2 start "$API_BIN" --name "$API_NAME" --time >/dev/null
wait_http "$API_URL/healthz" 90 || { echo "[qa] API did not answer on :$API_PORT"; pm2 logs "$API_NAME" --lines 20 --nostream || true; exit 1; }
curl -fsS "$API_URL/readyz" >/dev/null || { echo "[qa] API /readyz is not healthy"; curl -sS "$API_URL/readyz" || true; exit 1; }

step "seeding the QA tenant and site"
# The admin seed on every boot creates the OWNER account and nothing else: no organization,
# no site. The walkthrough's wizard then sees a user, so `needs_setup` (`!has_users`) is false,
# the wizard is skipped — and every site-scoped screen is left with nothing to render. The
# result looked like a broken product: `/cdn` asked for `?site_id=` and got a 400, the purge
# depth pass reported "no QA site to purge for", and the pass filed 170 high findings against a
# feature that had simply never been given a tenant to look at.
#
# The fixture belongs here and not in the walkthrough: the walkthrough is what is under test, and
# a pass that invents its own tenant is a pass whose empty states are its own invention.
#
# `docker exec -i` is load-bearing. Without the `-i` the heredoc never reaches psql, the command
# succeeds against an empty stdin, and the seed silently does nothing — which is the same failure
# this step exists to prevent, one layer further down. The row counts below are printed so a
# silently-empty seed is visible in the pass log rather than inferred from the findings.
docker exec -i "${QA_PG_CONTAINER:-omnion-postgres}" psql -U omnion -d "$QA_DB_NAME" -v ON_ERROR_STOP=1 <<'QA_SEED'
delete from sites;
delete from organizations;
update users set organization_id = null;
insert into organizations (id, name, slug, status, event_retention_days, created_at, updated_at)
select gen_random_uuid(), 'QA Organization', 'qa-organization', 'active', 30, now(), now()
where not exists (select 1 from organizations);
update users set organization_id = (select id from organizations limit 1) where organization_id is null;
insert into sites (organization_id, key, name, status, theme, created_at, updated_at)
select (select id from organizations limit 1), 'main', 'QA Main Site', 'active', 'minimal', now(), now()
where not exists (select 1 from sites);
QA_SEED
QA_ORG=$(docker exec "${QA_PG_CONTAINER:-omnion-postgres}" psql -U omnion -d "$QA_DB_NAME" -t -A -c "select count(*) from organizations" 2>/dev/null || echo 0)
QA_SITE=$(docker exec "${QA_PG_CONTAINER:-omnion-postgres}" psql -U omnion -d "$QA_DB_NAME" -t -A -c "select count(*) from sites where key = 'main'" 2>/dev/null || echo 0)
step "QA fixture: ${QA_ORG} organization(s), ${QA_SITE} site(s) keyed 'main'"
if [ "$QA_SITE" != "1" ]; then
  echo "[qa] the QA fixture has no site keyed 'main' — every site-scoped screen would walk empty" >&2
  exit 1
fi

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

step "browser walkthrough"
# `QA_ONLY` narrows the pass to the routes and depth passes whose name contains one of the
# comma-separated words. It is a narrowing, not a weaker gate: what it walks is walked, clicked
# and measured as usual, and both the summary and the report are stamped with the scope. Set it
# when the box cannot afford a full pass — seven writers on one 32 GB host cannot each run one.
node scripts/qa/walkthrough.cjs --url "http://127.0.0.1:$ADMIN_PORT" --web "http://127.0.0.1:$WEB_PORT" --out "$OUT" ${QA_ONLY:+--only "$QA_ONLY"}
WALK_RC=$?

# A walkthrough that died still leaves a `summary.json` behind, and that file is the most
# dangerous artifact in this harness: `{"fatal": "could not sign in"}` is a *pass* to anything
# that only checks whether the file exists or whether it has findings, and this script used to
# go on to write a clean QA-LATEST report and exit 0. Absence of evidence was being filed as
# evidence. Treat a dead run — or a scope that walked no pages — as a failed gate, loudly.
if [ "$WALK_RC" -ne 0 ]; then
  echo "[qa] the walkthrough exited $WALK_RC — see $OUT/summary.json" >&2
  exit "$WALK_RC"
fi
if node -e '
const fs = require("fs");
const out = process.argv[1];
const scope = process.argv[2] || "";
const s = JSON.parse(fs.readFileSync(out + "/summary.json", "utf8"));
if (s.fatal) { console.error("[qa] the walkthrough was fatal: " + s.fatal); process.exit(1); }
const pages = (s.pages || []).length;
if (pages === 0) { console.error("[qa] the walkthrough recorded no pages" + (scope ? " for scope " + scope : "") + " — a scope that matches nothing is a finding, not a pass"); process.exit(1); }
' "$OUT" "${QA_ONLY:-}" ; then :; else
  exit 1
fi

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
