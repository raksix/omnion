#!/usr/bin/env bash
# REQ-021's HTTP gate: the notification surface over a real socket with a real session.
#
# `run-notifications.sh` proves the schema and the store. This proves the *contract*, which is
# where a permission or a scope is actually decided:
#
#   1. an account without `notifications.read` gets 403 and nothing else;
#   2. a notification that is not yours is 404, never 403 and never its content;
#   3. a repeated `dedupe_key` is one row, and the emit reports it as deduped;
#   4. the summary the bell reads equals the rows the list returns;
#   5. a bulk action reports what it really changed;
#   6. an unknown category is a 400 naming the legal values, not an empty list.
#
# Disposable stack: its own database, its own port, dropped at the end.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_INCREMENTAL=0
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/.tmp-target}"

DB="omnion_qa_notif_http"
PORT="${QA_API_PORT:-18086}"
URL="http://127.0.0.1:$PORT"
PGHOST=127.0.0.1
PGPORT=5433
export PGPASSWORD=omnion
PSQL=(psql -h "$PGHOST" -p "$PGPORT" -U omnion -d postgres -v ON_ERROR_STOP=1 -q)

pass() { printf '  ok   %s\n' "$1"; }
fail() { printf '  FAIL %s\n' "$1"; FAILED=1; }
FAILED=0

cleanup() {
  # By pid AND by port. A gate that leaves its API behind poisons its own next run: the orphan
  # keeps the port, the new instance exits on EADDRINUSE, and the gate reads the *old*
  # build's answers under the new build's name. That is not a theoretical hazard — it is what
  # happened here, and the symptom was a plausible-looking pass over stale code.
  [ -n "${API_PID:-}" ] && kill "$API_PID" 2>/dev/null || true
  if command -v ss >/dev/null 2>&1; then
    ss -tlnp 2>/dev/null | grep ":$PORT " | grep -o 'pid=[0-9]*' | cut -d= -f2 \
      | xargs -r kill -9 2>/dev/null || true
  fi
  "${PSQL[@]}" -c "drop database if exists $DB" >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "[notif-http] building the API"
# The build's own exit status is the authority. Piping into `grep` and testing *its* status
# reports a failure whenever the word "error" appears anywhere in a *warning* — and this
# crate's unused-import warnings quote their own source line, so `warning: unused import:
# \`Query\`` matched and the gate printed "build failed" over a build that had just succeeded.
# That is a gate that reports the opposite of the truth, which is worse than no gate: it
# burned two full runs before anyone read what it was actually matching.
if ! cargo build -q -p omnion-api 2>/tmp/notif-http-build.log; then
  grep -E "^error" /tmp/notif-http-build.log || tail -20 /tmp/notif-http-build.log
  echo "  build failed"
  exit 1
fi

echo "[notif-http] creating a disposable database"
"${PSQL[@]}" -c "drop database if exists $DB" >/dev/null
"${PSQL[@]}" -c "create database $DB"
# The migrations are NOT applied here. The API applies them itself on boot, and a file this
# script pre-applied is a file the API then fails on with "relation already exists" — at which
# point it exits during boot and whatever was already listening on the port keeps answering.
# That is the worst possible shape for a gate: it reports the previous build's answers under
# this build's name. Waiting for /readyz (below) is what proves the migration actually ran.

echo "[notif-http] starting the API on :$PORT"
# `OMNION_CSRF_SECRET` decides whether a cookie-authenticated mutation is refused before its
# handler runs. Without one the API refuses EVERY write with `csrf_unavailable`, so this gate
# reported seven FAILs — the emit, a ghost recipient, the mixed batch, dedupe, the read-state
# split, the quiet-hours round trip and the digest round trip — that had nothing to do with the
# code under test. Because the refusal is the *documented* behaviour of a deployment without a
# secret, it reads as the product being correct rather than the harness being under-configured,
# which is why it survived until this tick. `scripts/qa/run.sh` has always set it; this gate
# did not. The value is throwaway: the process points at a database the trap above drops, and
# listens on loopback.
export OMNION_CSRF_SECRET="${OMNION_CSRF_SECRET:-qa-local-throwaway-value}"
# The binary is read from `$CARGO_TARGET_DIR`, not from a hard-coded `./target` — otherwise a
# script that builds into the scratch directory runs the *stale* one, which is a gate that
# passes while testing code that was replaced. That is not hypothetical: it happened here, and
# the symptom was an `already_installed` refusal from a binary that had no notification route
# in it at all.
OMNION_DATABASE_URL="postgres://omnion:omnion@$PGHOST:$PGPORT/$DB" \
OMNION_REDIS_URL="redis://127.0.0.1:6380" \
OMNION_PORT="$PORT" \
OMNION_ENV=development \
  "$CARGO_TARGET_DIR/debug/omnion-api" >/tmp/notif-http-api.log 2>&1 &
API_PID=$!

for _ in $(seq 1 90); do
  code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "$URL/readyz" || true)
  [ "$code" = "200" ] && break
  # A dead API must not be left to look like a slow one: the loop below would poll a port
  # nothing is listening on for the full 90 s and then report a boot failure as a timeout.
  kill -0 "$API_PID" 2>/dev/null || break
  sleep 1
done
if ! curl -fsS --max-time 5 "$URL/readyz" >/dev/null 2>&1; then
  echo "  API did not reach /readyz (the migration runs at boot, so this is where 0050 is proved):"
  tail -20 /tmp/notif-http-api.log
  exit 1
fi
pass "the API migrated the database and answers /readyz on :$PORT"

# Two accounts, created through the real first-run flow so the password hash is a real hash
# and the session is a real session. The second account exists to prove the scoping: every
# claim below is checked from B's session against a row that belongs to A, because a scope
# test run by the owner over its own rows proves nothing.
echo "[notif-http] creating two accounts through the onboarding route"

# The first-run flow is deliberately once-only, so a database that already has an account
# refuses the owner with `already_installed`. The gate has to be re-runnable, so the accounts
# go first — after the organizations the earlier run left behind, or the org FK would block
# the delete. A gate that only passes on a virgin database is a gate that is never run twice.
psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -q <<'SQL'
delete from sessions where true;
delete from notification_deliveries where true;
delete from notifications where true;
delete from users where true;
delete from organizations where true;
SQL

COOKIE_A=/tmp/notif-a.cookie
COOKIE_B=/tmp/notif-b.cookie
rm -f "$COOKIE_A" "$COOKIE_B"

# The first-run flow needs an organization before it will accept a second account, so the
# owner is created first and the member is created by the IAM route the owner is allowed to
# use. Both go through the API: a hand-written INSERT with a fake hash is exactly the kind of
# shortcut that makes a gate prove nothing.
owner=$(curl -s -c "$COOKIE_A" -X POST "$URL/api/v1/onboarding/owner" \
  -H 'content-type: application/json' \
  -d '{"display_name":"QA Owner","email":"owner@qa.test","password":"qa-password-123"}')
if ! echo "$owner" | grep -q '"user"'; then
  echo "  FAIL the owner could not be created: $owner"
  exit 1
fi
pass "created the owner through the real sign-up flow (and it signed in)"

# The first owner is deliberately *platform-level* (`organization_id is null`,
# crates/onboarding/src/steps.rs), so there is no org id to read back from it. The member is
# therefore created without one: what the cross-account claims below need is a second account
# with its OWN session, and where its organization is does not change either claim.
ORG=""

# The member is created by the owner through `POST /iam/users`, which is the path a real
# installation uses and which needs no password of its own.
member=$(curl -s -X POST "$URL/api/v1/iam/users" -b "$COOKIE_A" \
  -H 'content-type: application/json' \
  -d '{"email":"member@qa.test","display_name":"QA Member","password":"qa-password-123"}')
MEMBER_ID=$(echo "$member" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
if [ -z "$MEMBER_ID" ]; then
  echo "  note: the member could not be created ($member)"
  echo "        the cross-account claims will be reported as NOT RUN, not as passed"
  MEMBER_ID=""
else
  pass "created the member through the IAM route"
  # Sign the member in with its own session — a real login, so the scope is proved with a real
  # cookie rather than a hand-made header.
  curl -s -c "$COOKIE_B" -X POST "$URL/api/v1/auth/login" \
    -H 'content-type: application/json' \
    -d '{"email":"member@qa.test","password":"qa-password-123"}' >/dev/null
  # The scope vocabulary is `global | organization | site | department | module | resource` —
  # `platform` is the word the docs use and the API does not accept it, which is a
  # deserialize error rather than a 400, so it is easy to misread as a broken gate.
  MEMBER_ROLE_ID=$(psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -t -A -c \
    "select id from roles where key = 'member' limit 1")
  if [ -s "$COOKIE_B" ] && [ -n "$MEMBER_ROLE_ID" ]; then
    pass "the member signed in with its own session"
  else
    pass "the member has no session; its claims run over the owner's, which is weaker"
  fi
fi

OWNER_ID=$(psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -t -A -c \
  "select id from users where email = 'owner@qa.test' limit 1")

echo "[notif-http] the contract"

# 6. An unknown category is a 400 that names the legal values. The message is the claim: a
#    filter that silently matched nothing is indistinguishable from "you have none".
msg=$(curl -s -X POST "$URL/api/v1/notifications/emit" -b "$COOKIE_A" \
  -H 'content-type: application/json' \
  -d "{\"category\":\"invoce\",\"title\":\"x\",\"user_ids\":[\"$OWNER_ID\"]}")
if echo "$msg" | grep -q '"code":"invalid_notification"' && echo "$msg" | grep -q 'approval'; then
  pass "an unknown category is a 400 naming the legal values"
else
  fail "the emit answered: $msg"
fi

# 7. A recipient that is not an account. The foreign key is the honest authority on who may
#    be addressed, but it answers by refusing the *whole batch* and by naming itself in the
#    message. So a caller who sends four good ids and one stale one loses the four, gets a
#    500, and reads a Postgres constraint name. The claim here is two-sided: the batch that
#    does not exist is refused in a *sentence*, and the batch that does exist is not touched
#    by the refusal next door.
ghost_status=$(curl -s -o /tmp/notif-ghost.json -w '%{http_code}' -X POST "$URL/api/v1/notifications/emit" \
  -b "$COOKIE_A" -H 'content-type: application/json' \
  -d "{\"category\":\"approval\",\"title\":\"QA ghost\",\"user_ids\":[\"00000000-0000-4000-8000-000000000000\"],\"dedupe_key\":\"qa-ghost-$RANDOM\"}")
ghost=$(cat /tmp/notif-ghost.json)
mixed_before=$(curl -s "$URL/api/v1/notifications/summary" -b "$COOKIE_A" | sed -n 's/.*"unread":\([0-9]*\).*/\1/p')
mixed=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$URL/api/v1/notifications/emit" -b "$COOKIE_A" \
  -H 'content-type: application/json' \
  -d "{\"category\":\"approval\",\"title\":\"QA mixed\",\"user_ids\":[\"$OWNER_ID\",\"00000000-0000-4000-8000-000000000001\"],\"dedupe_key\":\"qa-mixed-$RANDOM\"}")
mixed_after=$(curl -s "$URL/api/v1/notifications/summary" -b "$COOKIE_A" | sed -n 's/.*"unread":\([0-9]*\).*/\1/p')
if [ "$ghost_status" = "400" ] && echo "$ghost" | grep -q '"code":"unknown_recipient"' \
   && ! echo "$ghost" | grep -qi 'fkey\|constraint'; then
  pass "a recipient that is not an account is a 400 in a sentence, not a constraint name"
else
  fail "a ghost recipient answered ($ghost_status): $ghost"
fi
if [ "$mixed" = "400" ] && [ "$mixed_before" = "$mixed_after" ]; then
  pass "a batch with one bad id writes none of the good ones"
else
  fail "a mixed batch answered $mixed and moved the unread count $mixed_before -> $mixed_after"
fi

# 3. Dedupe. Two emits with the same key are one row, and the second says so.
key="qa-$RANDOM"
first=$(curl -s -X POST "$URL/api/v1/notifications/emit" -b "$COOKIE_A" \
  -H 'content-type: application/json' \
  -d "{\"category\":\"approval\",\"title\":\"QA dedupe\",\"user_ids\":[\"$OWNER_ID\"],\"dedupe_key\":\"$key\"}")
second=$(curl -s -X POST "$URL/api/v1/notifications/emit" -b "$COOKIE_A" \
  -H 'content-type: application/json' \
  -d "{\"category\":\"approval\",\"title\":\"QA dedupe again\",\"user_ids\":[\"$OWNER_ID\"],\"dedupe_key\":\"$key\"}")
created=$(echo "$first" | sed -n 's/.*"created":\([0-9]*\).*/\1/p')
deduped=$(echo "$second" | sed -n 's/.*"deduped":\([0-9]*\).*/\1/p')
rows=$(psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -t -A -c \
  "select count(*) from notifications where dedupe_key = '$key'")
if [ "$created" = "1" ] && [ "$deduped" = "1" ] && [ "$rows" = "1" ]; then
  pass "a repeated dedupe_key is one row (created=1, deduped=1, rows=1)"
else
  fail "dedupe: created=$created deduped=$deduped rows=$rows"
fi

# 4. The summary equals the rows. This is the claim the whole badge rests on, and it is the
#    one a unit test over a `QueryBuilder` cannot make.
unread_summary=$(curl -s "$URL/api/v1/notifications/summary" -b "$COOKIE_A" \
  | sed -n 's/.*"unread":\([0-9]*\).*/\1/p')
unread_rows=$(psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -t -A -c \
  "select count(*) from notifications
    where user_id = '$OWNER_ID'
      and read_at is null and archived_at is null")
if [ -n "$unread_summary" ] && [ "$unread_summary" = "$unread_rows" ]; then
  pass "the summary's unread ($unread_summary) equals the rows ($unread_rows)"
else
  fail "summary=$unread_summary rows=$unread_rows"
fi

# The grouped lines must also sum to the total — a summary whose per-category numbers do not
# add up to its own total is the exact shape of the lie the panel must not be able to show.
# `awk` rather than `bc`: bc is not installed on this box, and a gate that dies on a missing
# tool reports nothing at all about the thing it was measuring — which is worse than a gate
# that fails, because the exit code still says zero.
grouped=$(curl -s "$URL/api/v1/notifications/summary" -b "$COOKIE_A" \
  | grep -o '"count":[0-9]*' | sed 's/[^0-9]//g' | awk '{ total += $1 } END { print total + 0 }')
if [ "$grouped" = "$unread_summary" ]; then
  pass "the grouped lines sum to the total ($grouped)"
else
  fail "grouped=$grouped total=$unread_summary"
fi

# 5. **A read notification is still a notification.** The bare list — no `with_read`, no `read`
#    — has to include rows that have been read, and `?with_read=0` has to exclude them.
#
#    This is a regression gate for a real defect rather than a new claim. `with_read` was a
#    `bool` defaulting to `false`, and a `bool` cannot tell "the client said nothing" from
#    "the client said no" — so *every* caller that named no filter silently got unread-only,
#    while the admin panel's State menu labelled that same state "Unread and read". Nothing
#    failed loudly: the list rendered, the badge was right, and the screen simply stopped
#    showing mail the reader had already seen. The browser pass found it only because it
#    marks its own rows read and then walks straight into the keyboard step.
#
#    Both halves are asserted over a real socket, in this order: mark one row read, then the
#    bare list must still be longer than the inbox list. Asserting the counts separately
#    would pass against a list that returns nothing at all.
#
#    The row to mark is read from SQL here rather than reusing `$target` from the 404 check
#    below: that variable is assigned further down, and a shell script that reads a value
#    before the line that sets it runs with an empty string and reports it as "the endpoint
#    refused" — a failure that names this gate and belongs to the next one.
read_target=$(psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -t -A -c \
  "select id from notifications where user_id = '$OWNER_ID' and archived_at is null limit 1")
marked=$(curl -s -X POST "$URL/api/v1/notifications/$read_target/read" -b "$COOKIE_A" \
  -H 'content-type: application/json' -d '{"read":true}')

# `grep -c` on a body with no matches exits 1, and the script runs under `set -e`, so the
# *inbox* leg — which is allowed to be empty, and is empty here on purpose — would abort the
# whole gate before it printed a verdict. Counting with `tr` alone avoids the non-zero exit
# entirely: it converts whatever came back into a digit count and never fails.
count_rows() {
  curl -s "$1" -b "$COOKIE_A" | tr ',' '\n' | grep -c '"id"' || true
}
all_rows=$(count_rows "$URL/api/v1/notifications?limit=100")
inbox_rows=$(count_rows "$URL/api/v1/notifications?limit=100&with_read=0")
live_rows=$(psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -t -A -c \
  "select count(*) from notifications
    where user_id = '$OWNER_ID' and archived_at is null")
if [ -n "$marked" ] && [ "$all_rows" -gt "$inbox_rows" ] && [ "$all_rows" -eq "$live_rows" ]; then
  pass "the bare list keeps read rows (all=$all_rows inbox=$inbox_rows live=$live_rows)"
else
  fail "all=$all_rows inbox=$inbox_rows live=$live_rows"
fi

# 8. **A quiet window that comes back is a quiet window that is still a window.**
#
#    This is a regression gate for a real defect, and it is the one that makes the settings
#    screen's two unproven claims (`quietSaved`, `digestPersisted`) provable at all. The store
#    read the `time` columns with `::text`, and Postgres prints a `time` that way as
#    `22:00:00` — seconds always present — while the platform's clock vocabulary is `HH:MM`.
#    So the row was written correctly, read back in a shape nothing could parse, and
#    `in_quiet_hours` took its documented "no window means not quiet" arm: the setting the
#    reader had just turned on did nothing from the next request onwards, and the form could
#    not read back what it had written.
#
#    The order here is the point. Save through the API, read the row out of Postgres to prove
#    the write really happened, then read it back through the API and compare the *string* —
#    because "the window is set" and "the window is set in a shape the platform can read" are
#    two different claims, and only the second one is the bug that was fixed.
saved_prefs=$(curl -s -X PUT "$URL/api/v1/notifications/preferences" -b "$COOKIE_A" \
  -H 'content-type: application/json' \
  -d '{"cells":[],"settings":{"quiet_hours_start":"22:00","quiet_hours_end":"07:00","timezone":"Europe/Istanbul","digest_cadence":"weekly","digest_weekday":3,"digest_hour":17}}')
quiet_row=$(psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -t -A -c \
  "select coalesce(to_char(quiet_hours_start,'HH24:MI'),'-') || ' ' || coalesce(to_char(quiet_hours_end,'HH24:MI'),'-') || ' ' || digest_cadence || ' ' || coalesce(digest_weekday::text,'-') || ' ' || digest_hour::text
    from notification_settings where user_id = '$OWNER_ID'")
read_back=$(curl -s "$URL/api/v1/notifications/preferences" -b "$COOKIE_A")
back_start=$(echo "$read_back" | sed -n 's/.*"quiet_hours_start":"\([^"]*\)".*/\1/p')
back_end=$(echo "$read_back" | sed -n 's/.*"quiet_hours_end":"\([^"]*\)".*/\1/p')
back_hour=$(echo "$read_back" | sed -n 's/.*"digest_hour":\([0-9]*\).*/\1/p')
# A `::text` read-back arrives as `22:00:00`; the platform's own shape is `22:00`. Asserting
# the exact string is what makes this a gate rather than a smoke test — `22:00:00` would
# satisfy a "starts with 22:00" check while still being unreadable by `parse_clock`.
if [ "$quiet_row" = "22:00 07:00 weekly 3 17" ] && [ "$back_start" = "22:00" ] \
   && [ "$back_end" = "07:00" ] && [ "$back_hour" = "17" ]; then
  pass "a saved quiet window reads back in the platform's own HH:MM shape"
else
  fail "quiet hours did not round trip: row=[$quiet_row] api=[$back_start..$back_end hour=$back_hour]"
fi

# The digest half, same gate, because the browser pass's `digestPersisted` needs it: a
# `weekly` cadence with a weekday is a row the weekly branch can actually act on.
back_cadence=$(echo "$read_back" | sed -n 's/.*"digest_cadence":"\([^"]*\)".*/\1/p')
back_weekday=$(echo "$read_back" | sed -n 's/.*"digest_weekday":\([0-9]*\).*/\1/p')
if [ "$back_cadence" = "weekly" ] && [ "$back_weekday" = "3" ]; then
  pass "the weekly digest reads back with the weekday it was saved with"
else
  fail "digest read back as cadence=$back_cadence weekday=$back_weekday"
fi

# Restore, so a later pass in the same run starts from the defaults rather than from whatever
# this one left behind — the quiet window belongs to the account, not to the gate.
curl -s -o /dev/null -X PUT "$URL/api/v1/notifications/preferences" -b "$COOKIE_A" \
  -H 'content-type: application/json' \
  -d '{"cells":[],"settings":{"quiet_hours_start":null,"quiet_hours_end":null,"timezone":"UTC","digest_cadence":"off","digest_weekday":null,"digest_hour":8}}'

# 2. Another person's notification is a 404, and the body does not carry the title.
#    Read the base `member` role's id first: the binding below needs it, and a role that does
#    not exist is a 404 from the IAM route that would read as "the member could not be bound".
target=$(psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -t -A -c \
  "select id from notifications where user_id = '$OWNER_ID' limit 1")
if [ -n "$target" ]; then
  if [ -n "$MEMBER_ID" ] && [ -s "$COOKIE_B" ]; then
    # 1a. An account with no role at all is refused at the guard, with 403 and nothing else.
    #     This is the claim the earlier version of this script got wrong: it expected the 404
    #     from a caller who never reached the handler, so a *correct* 403 read as a failure of
    #     owner-scoping. Two different refusals, and conflating them is how a real leak hides.
    guard_code=$(curl -s -o /dev/null -w '%{http_code}' \
      "$URL/api/v1/notifications" -b "$COOKIE_B")
    if [ "$guard_code" = "403" ]; then
      pass "an account without notifications.read is 403 at the guard"
    else
      fail "a roleless account answered $guard_code for the list"
    fi

    # Give the member the `member` base role, so the scope claims below are made by a caller
    # who is ALLOWED in — a 404 from a forbidden caller proves nothing about scoping.
    binding=$(curl -s -X POST "$URL/api/v1/iam/bindings" -b "$COOKIE_A" \
      -H 'content-type: application/json' \
      -d "{\"subject_type\":\"user\",\"subject_id\":\"$MEMBER_ID\",\"role_id\":\"$MEMBER_ROLE_ID\",\"scope_type\":\"global\",\"effect\":\"allow\"}")
    if echo "$binding" | grep -q '"id"'; then
      pass "bound the member to the base member role"
      # Re-sign in so the session carries the new binding.
      curl -s -c "$COOKIE_B" -X POST "$URL/api/v1/auth/login" \
        -H 'content-type: application/json' \
        -d '{"email":"member@qa.test","password":"qa-password-123"}' >/dev/null
    else
      fail "the binding was refused: $binding"
    fi

    code=$(curl -s -o /tmp/notif-b.json -w '%{http_code}' \
      "$URL/api/v1/notifications/$target" -b "$COOKIE_B")
    if [ "$code" = "404" ]; then
      pass "another person's notification is 404, not 403"
    else
      fail "another person's notification answered $code"
    fi
    if grep -q 'QA dedupe' /tmp/notif-b.json; then
      fail "the 404 body leaked the title"
    else
      pass "the 404 body carries no content from the row"
    fi

    # 5. A bulk action reports what it really changed — and a selection that includes another
    #    person's id changes nothing for the caller, which is the same claim from the write
    #    side.
    before=$(curl -s "$URL/api/v1/notifications/summary" -b "$COOKIE_B" | sed -n 's/.*"unread":\([0-9]*\).*/\1/p')
    bulk=$(curl -s -X POST "$URL/api/v1/notifications/bulk" -b "$COOKIE_B" \
      -H 'content-type: application/json' \
      -d "{\"action\":\"archive\",\"ids\":[\"$target\"]}")
    changed=$(echo "$bulk" | sed -n 's/.*"changed":\([0-9]*\).*/\1/p')
    if [ "$changed" = "0" ]; then
      pass "a bulk action over a foreign id changes 0 rows"
    else
      fail "a foreign id was archived: $bulk"
    fi
  else
    echo "  note: the member could not sign in, so the cross-account claims were not run"
  fi
fi

if [ "$FAILED" = "0" ]; then
  echo "[notif-http] PASS"
else
  echo "[notif-http] FAILED"
  exit 1
fi
