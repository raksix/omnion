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
# `QA_OUT_ROOT` moves the artifacts off the worktree. A pass writes screenshots for every
# screen and every mobile viewport, and seven worktrees on one volume at 96% means the
# pass that measures the most is the one that fills the disk — which is how a QA run
# becomes the reason the next build fails with "No space left on device". `/dev/shm` is
# the right default for a *disposable* pass: the artifacts are read by the vision review
# and the summary in the same run and are worthless the next morning. The default is
# unchanged so nobody loses their history by accident.
OUT="${QA_OUT_ROOT:-$ROOT/qa-artifacts}/$TS"
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
#
# The second half of that condition is the one this guard got wrong, and it cost a tick: it watched
# `database/migrations` only, so a change to a **.rs** file left the binary stale and the pass
# measured the previous build. Symptom, exactly: the device-code fix was committed, `cargo test`
# passed on it, and the browser pass still reported `invalid device code` — because the API
# under test was 30 minutes older than the fix.
#
# A migration is a special case of "the source is newer than the binary", not a separate concern:
# sqlx embeds the SQL at compile time, so watching the Rust sources covers it too, and watching
# both keeps the comment's warning intact for anyone who wonders why a `.sql` shows up here.
#
# The cost is one `find` over the workspace's source dirs, and the payoff is that a pass can no
# longer report a verdict about code it did not run. `crates/` is included because every route and
# store in this platform lives there; `apps/` because that is where the binary's own crate is.
if [ ! -x "$API_BIN" ] \
   || [ -n "$(find database/migrations -name '*.sql' -newer "$API_BIN" -print -quit)" ] \
   || [ -n "$(find crates apps/api -name '*.rs' -newer "$API_BIN" -print -quit)" ]; then
  step "building the API (first pass, or a source or migration changed since the last build)"
  # Eight writers share six cores: a global semaphore keeps at most CARGO_SLOTS builds
  # compiling at once instead of every pass grabbing all six threads for itself.
  "$(dirname "$0")/cargo-slot.sh" cargo build -p omnion-api
fi
# Nothing is restarted in place: the block further down deletes the entry and re-registers it, so
# a `pm2 restart` here could only ever be a worse version of what happens anyway. Two earlier
# versions of this script did restart, and both had a way to keep a *dead* registration alive --
# one because a pass had started the API from `$CARGO_TARGET_DIR` and a later pass that built
# somewhere else inherited an entry pointing at a path that no longer exists, and one because
# `pm2 restart` succeeds on an entry whose script is gone, so the pass died at `wait_http`
# blaming the product for a harness that had already lost the binary.
#
# `OMNION_CSRF_SECRET` decides whether a cookie-authenticated mutation is refused before its
# handler runs. Without one the QA API refuses EVERY write with `csrf_unavailable`, so a
# walkthrough that saves a header policy, uploads a file or takes a backup would record screens
# that "work" while the API answered 403 the whole time -- and because that refusal is the
# documented behaviour of a deployment *without* a secret, it reads as the product being correct
# rather than the harness being under-configured. It is a throwaway value: the process points at
# a database that was dropped two lines above and listens on loopback.
#
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
OMNION_CSRF_SECRET="${QA_CSRF_SECRET:-qa-pass-throwaway-secret-not-a-real-key}" \
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

# `QA_ONLY` narrows the pass to named routes and depth passes. The default runs every one of
# them, which is the right thing for a full acceptance run and the wrong thing for a loop that
# has just built two screens and needs them proven before the tick ends. It is a filter on the
# walk, never on the harness around it: the stack, the reset, the vision review and the report
# all run exactly as they do for a full pass.
#
# The filter is passed as ONE `--only=` argument however many names it holds, because
# walkthrough.cjs's parser is `split(",")` on a single value. Handed the words separately the
# first becomes the flag's value and the rest are unknown argv entries the parser silently
# ignores -- so a two-name scope would run as a one-name scope, or as none, and the artifact
# would carry a coverage claim nobody checked.
QA_ONLY_ARGS=()
[ -n "${QA_ONLY:-}" ] && QA_ONLY_ARGS=(--only="$QA_ONLY")

step "browser walkthrough${QA_ONLY:+ (focused: $QA_ONLY)}"
node scripts/qa/walkthrough.cjs --url "http://127.0.0.1:$ADMIN_PORT" --web "http://127.0.0.1:$WEB_PORT" --out "$OUT" "${QA_ONLY_ARGS[@]}"
WALK_RC=$?

# A walkthrough that died still leaves a `summary.json` behind, and that file is the most
# dangerous artifact in this harness: `{"fatal": "could not sign in"}` is a *pass* to anything
# that only checks whether the file exists or whether it has findings, and this script used to
# go on to write a clean QA-LATEST report and exit 0. Absence of evidence was being filed as
# evidence. Treat a dead run -- or a scope that walked no pages -- as a failed gate, loudly.
if [ "$WALK_RC" -ne 0 ]; then
  echo "[qa] the walkthrough exited $WALK_RC -- see $OUT/summary.json" >&2
  exit "$WALK_RC"
fi
if node -e '
const fs = require("fs");
const out = process.argv[1];
const scope = process.argv[2] || "";
const s = JSON.parse(fs.readFileSync(out + "/summary.json", "utf8"));
if (s.fatal) { console.error("[qa] the walkthrough was fatal: " + s.fatal); process.exit(1); }
const pages = (s.pages || []).length;
if (pages === 0) { console.error("[qa] the walkthrough recorded no pages" + (scope ? " for scope " + scope : "") + " -- a scope that matches nothing is a finding, not a pass"); process.exit(1); }
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
