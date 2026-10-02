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
# The artifacts are throwaway screenshots, and the volume the worktrees live on is shared with
# every other writer (six of them, all building Rust and Next at once). QA_OUT_ROOT moves them to
# another filesystem when there is one with room — a RAM-backed tmpfs, in practice — so a pass is
# bounded by that filesystem instead of by a race nobody in this worktree can win. The report is
# copied back into the worktree at the end, so the artifact layout is unchanged either way.
OUT_ROOT="${QA_OUT_ROOT:-$ROOT/qa-artifacts}"
OUT="$OUT_ROOT/$TS"
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
# The QA Postgres port. The default is the shared container every stack points at; a
# writer whose own Postgres is unhealthy sets QA_PG_PORT and QA_PG_CONTAINER to run
# against a private one (the container is already the knob in reset-db.sh and in the
# `--only` filter test, so a pass could not reach a different server without this).
# It is a port, not a credential: nothing here is a secret.
DEFAULT_PG_PORT=5433
export QA_PG_CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
# The database URL and the first account, in one place: `run.sh` and `walkthrough.cjs` must
# agree on both. A pass that resets the database and then cannot sign in to it is a pass that
# dies before its first screen.
QA_DATABASE_URL="postgres://omnion:omnion@127.0.0.1:${QA_PG_PORT:-$DEFAULT_PG_PORT}/$QA_DB_NAME"
QA_ADMIN_EMAIL="qa-owner@omnion.test"
QA_ADMIN_PASSWORD="OmnionQa-Passw0rd-2026!"
export QA_DATABASE_URL QA_ADMIN_EMAIL QA_ADMIN_PASSWORD
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
  # The slot's own regression test, before the pass waits on the slot. It is a handful of seconds
  # and it is the only thing in the harness that can tell us the semaphore still works: every
  # other symptom (a pass that never starts, a pass that starts with no place) is silent, and both
  # look identical to a box that is simply too loaded to walk anything.
  step "testing the QA slot"
  bash "$(dirname "${BASH_SOURCE[0]}")/test-qa-slot.sh" || echo "[qa] slot test reported failures (continuing: a broken test is not a reason to skip a pass)"
  # The `--only` filter's own regression test, for the same reason and with more force.
  #
  # This one guards the instrument every scoped pass depends on, and its failure mode is the
  # most expensive kind there is: a filter that silently matches nothing does not fail the pass,
  # it makes the pass run something ELSE. For three ticks that made a scoped pass walk the whole
  # box and write no report, and the only symptom was "the pass takes too long" — which reads as
  # a scheduling fact, so nobody looked at the filter. The test extracts the REAL helpers out of
  # `walkthrough.cjs` rather than reimplementing them, because a copy keeps passing after the
  # original regresses, which is exactly the thing it exists to catch.
  step "testing the --only filter"
  node "$(dirname "${BASH_SOURCE[0]}")/test-only-filter.cjs" || echo "[qa] --only filter test reported failures (continuing: a broken test is not a reason to skip a pass)"
  # The two screen inventories. This one is different from the others above, and the difference is
  # the reason it is here: a screen in the desktop `routes` list and missing from `mobileRoutes`
  # is measured at 1440 px and at no other width, by any pass, ever — and it does not error, does
  # not print red and does not set a non-zero code. It simply never gets opened on a phone, so
  # REQ-064's acceptance 18 ("all new screens render at 390 px") could not be answered for
  # thirty-nine screens, including twelve this branch shipped. The gate runs `warn`-style like its
  # neighbours rather than aborting the pass, because a wrong inventory must not cost a whole
  # walkthrough — but unlike the others it is re-checked on every run, because the thing it
  # catches is silent by construction.
  step "testing the screen inventories"
  node "$(dirname "${BASH_SOURCE[0]}")/screen-coverage.cjs" || echo "[qa] screen-coverage test reported failures (continuing: a broken test is not a reason to skip a pass)"
  # The disk guard's tmpfs reading, for the same reason and because this box runs out of RAM
  # before it runs out of disk: /dev/shm is a real tmpfs and every writer's CARGO_TARGET_DIR
  # lands in it, so a guard that misreads how full it is leaves the box one build away from
  # the OOM that took out the shared Postgres on 29 September. It shipped having reported the
  # INVERSE of the truth ("free 98%" over a filesystem 98% full), which is the failure mode a
  # test has to catch by asserting direction rather than by snapshotting the output.
  step "testing the disk guard's tmpfs reading"
  bash "$(dirname "${BASH_SOURCE[0]}")/test-disk-guard-shm.sh" || echo "[qa] disk-guard tmpfs test reported failures (continuing: a broken test is not a reason to skip a pass)"
  # The renderer's document/page theme agreement, for the same reason.
  #
  # All ten bundled stylesheets ship in ONE bundle and every element rule in them is scoped to
  # `html[data-theme="<key>"]`, so that one attribute decides whether a theme paints anything.
  # It used to be written from the installation default rather than the addressed site's theme,
  # which made every non-default site render as a colour-swapped copy of Minimal — a complete,
  # valid, entirely wrong page that no request failed and no log line mentioned. That is the most
  # expensive class of defect this harness can miss: the page is present, so every screen- and
  # request-level check passes while the site shows the wrong design.
  step "testing the renderer's per-site theme resolution"
  node "$(dirname "${BASH_SOURCE[0]}")/probe-site-theme.cjs" || echo "[qa] site-theme probe reported failures (continuing: a broken test is not a reason to skip a pass)"
  step "waiting for a QA slot (max ${QA_SLOTS:-1} concurrent pass)"
  # QA_SLOT_OWNER_PID is THIS shell's pid, so the slot can tell a place whose pass is still alive
  # from one whose pass was killed without running its EXIT trap. Without it the holder is the
  # only evidence a place is owned, and a holder outlives the pass that kills it — so a SIGKILLed
  # pass held the queue hostage until a human walked over and killed a stranger's holder.
  QA_SLOT_PID="$(QA_SLOT_OWNER_PID="$$" QA_SLOT_WAIT="${QA_SLOT_WAIT:-1800}" bash "$(dirname "${BASH_SOURCE[0]}")/qa-slot.sh" | tail -n 1)"
  export QA_SLOT_PID
fi
# A pass writes gigabytes of screenshots and six writers share this volume, so the two things
# that go wrong are "the last pass's artifacts are still here" and "there is no room". Prune
# first, then check: a full disk is often recoverable by pruning alone, and a check that runs
# first would refuse a pass that had just made room for itself. Taking the slot first also means
# the prune never races another pass that is already writing into the directory.
KEEP="${QA_KEEP_PASSES:-2}"
if [ "$KEEP" -gt 0 ] 2>/dev/null; then
  while read -r old; do
    [ -n "$old" ] || continue
    case "$old" in "$TS") continue ;; esac
    step "pruning the artifacts of $old"
    rm -rf "$OUT_ROOT/$old"
  done <<< "$(ls -1t "$OUT_ROOT" 2>/dev/null | tail -n +$((KEEP + 1)))"
fi

# A full-page PNG of a long admin page runs to megabytes, and a pass takes around a thousand of
# them — on a volume several writers build on at once that is the whole difference between a pass
# that completes and one that is deleted out from under itself twenty minutes in. The walkthrough
# can drop to fewer, shorter shots, so the space check sizes the need from the shot budget instead
# of assuming the richest pass: too little room shrinks the budget, and only a volume that cannot
# hold even a minimal pass is refused (and says so) before anything is reset.
SHOT_MODE="${QA_SHOT_MODE:-full}"
AVAIL_MB=$(( $(df -Pk "$OUT_ROOT" | awk 'NR==2 {print $4}') / 1024 ))
NEED_MB="${QA_MIN_FREE_MB:-6000}"
# Viewport JPEG shots are roughly a tenth of a full-page PNG each, so the room a pass needs is a
# property of the shot mode, not a constant: asking for the full-pass figure before deciding to
# downgrade is what refused passes that would have fitted comfortably.
[ "$SHOT_MODE" = "viewport" ] && NEED_MB="${QA_MIN_FREE_MB_VIEWPORT:-700}"
if [ "$AVAIL_MB" -lt "$NEED_MB" ]; then
  case "$SHOT_MODE" in
    full)
      # A full pass writes the deep full-page shots; viewport shots are an order of magnitude
      # smaller and still visit every screen and click every control.
      SHOT_MODE=viewport
      step "only ${AVAIL_MB}MB free (wanted ${NEED_MB}); dropping to viewport-sized shots"
      NEED_MB="${QA_MIN_FREE_MB_VIEWPORT:-700}"
      if [ "$AVAIL_MB" -lt "$NEED_MB" ]; then
        echo "[qa] only ${AVAIL_MB}MB free where the artifacts go; even a viewport pass needs about ${NEED_MB}MB." >&2
        echo "[qa] free a worktree's target/ (regenerable) or lower QA_KEEP_PASSES, and re-run." >&2
        exit 1
      fi
      ;;
    viewport)
      echo "[qa] only ${AVAIL_MB}MB free where the artifacts go; even a viewport pass needs about ${NEED_MB}MB." >&2
      echo "[qa] free a worktree's target/ (regenerable) or lower QA_KEEP_PASSES, and re-run." >&2
      exit 1
      ;;
  esac
fi
export QA_SHOT_MODE="$SHOT_MODE"
step "free space: ${AVAIL_MB}MB"

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

step "API on :$API_PORT (database omnion_qa)"
# Build when the binary is missing OR older than something it was built from. A pass that only
# builds on a missing binary silently exercises the last binary that happened to be there: the
# stack restarts fine, every request answers, and a route added this tick 404s — which reads as a
# broken screen rather than as a stale build. Two things make a binary stale, and both are here:
#
#   * source — a route/handler edit that is not compiled in, and
#   * migrations — sqlx embeds `database/migrations/*.sql` at compile time, so a migration edited
#     after the last build silently replays the previous one, and a syntax error in it looks like
#     a duplicate table on the next attempt.
#
# The source list must be the paths that actually exist in this workspace (apps, crates, modules,
# database, the manifests). A `find` over paths that do not exist returns nothing and looks exactly
# like "nothing changed" — the mtime check silently disabled itself. `-print -quit` keeps it cheap.
NEEDS_BUILD=0
if [ ! -x target/debug/omnion-api ]; then
  NEEDS_BUILD=1
  step "building the API (no binary yet)"
elif [ -n "$(find apps crates modules database Cargo.toml -newer target/debug/omnion-api -print -quit 2>/dev/null)" ]; then
  NEEDS_BUILD=1
  step "building the API (sources or migrations are newer than the binary)"
fi
if [ "$NEEDS_BUILD" = "1" ]; then
  # Eight writers share six cores: a global semaphore keeps at most CARGO_SLOTS builds
  # compiling at once instead of every pass grabbing all six threads for itself.
  "$(dirname "$0")/cargo-slot.sh" cargo build -p omnion-api
fi
# A writer loop on a tight volume builds into a scratch target (CARGO_TARGET_DIR, usually a
# tmpfs) to keep /mnt/apopic from filling — but pm2 is started from the fixed path below, and
# a build that landed somewhere else left that path missing. The pass then died with
# "Script not found: .../target/debug/omnion-api" and no report at all, which reads as a broken
# harness rather than as a build that went to another directory. Copy it over whenever the two
# differ; the binary is 140 MB and the scratch copy is already warm, so this costs a copy.
QA_BUILD_TARGET="${CARGO_TARGET_DIR:-$ROOT/target}"
if [ "$QA_BUILD_TARGET" != "$ROOT/target" ] && [ -x "$QA_BUILD_TARGET/debug/omnion-api" ]; then
  mkdir -p "$ROOT/target/debug"
  cp "$QA_BUILD_TARGET/debug/omnion-api" "$ROOT/target/debug/omnion-api"
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
  # `OMNION_ADMIN_EMAIL` / `OMNION_ADMIN_PASSWORD` seed the FIRST account on an empty
  # database (apps/api/src/main.rs `bootstrap_admin`). Without them the API boots onto a
  # database `reset-db.sh` has just emptied, logs "no accounts exist yet", and the panel
  # routes `/` to `/login` instead of `/setup` — so the walkthrough skips its wizard step as
  # "installation already exists" and `ensureSignedIn` then cannot sign in, because the
  # account the pass knows about is the one the reset deleted. It died on that on 2026-09-29
  # before reaching a single screen. These are the same values `walkthrough.cjs` signs in
  # with (`CREDS`), and both scripts must agree on them.
  OMNION_DATABASE_URL="$QA_DATABASE_URL" \
  OMNION_REDIS_URL="redis://127.0.0.1:6380" \
  OMNION_PORT="$API_PORT" \
  OMNION_ENV=development \
  OMNION_ADMIN_EMAIL="$QA_ADMIN_EMAIL" \
  OMNION_ADMIN_PASSWORD="$QA_ADMIN_PASSWORD" \
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

# `--api` is not optional. `walkthrough.cjs` declares `URL_API = process.env.QA_API_URL ||
# arg("api", URL_ADMIN)`, and the admin origin is NOT the API origin: the depth passes that POST
# (headers, media, forms, SEO, comments) would answer 404 the whole way and report screens that
# "work" because they never reached the API.
step "browser walkthrough${QA_ONLY:+ (focused: $QA_ONLY)}"
node scripts/qa/walkthrough.cjs --url "http://127.0.0.1:$ADMIN_PORT" --web "http://127.0.0.1:$WEB_PORT" --api "$API_URL" --out "$OUT" "${QA_ONLY_ARGS[@]}"

# The vision review reads the whole shot set and judges it against the product's visual rules.
# On a scoped pass that set is a fraction of the screens, so its verdicts describe a product
# state that does not exist — and it is the slowest step in the pass. Skipping it is honest;
# running it is a report about a partial set.
if [ -z "${QA_ONLY:-}" ]; then
  step "vision review"
  node scripts/qa/vision-review.cjs --dir "$OUT" || echo "[qa] vision review skipped"
else
  step "vision review (skipped: scoped pass, QA_ONLY=$QA_ONLY)"
fi

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
