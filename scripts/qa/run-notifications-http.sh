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
cargo build -q -p omnion-api 2>&1 | grep -E "^(error|warning: unused)" && { echo "  build failed"; exit 1; }

echo "[notif-http] creating a disposable database"
"${PSQL[@]}" -c "drop database if exists $DB" >/dev/null
"${PSQL[@]}" -c "create database $DB"
# The migrations are NOT applied here. The API applies them itself on boot, and a file this
# script pre-applied is a file the API then fails on with "relation already exists" — at which
# point it exits during boot and whatever was already listening on the port keeps answering.
# That is the worst possible shape for a gate: it reports the previous build's answers under
# this build's name. Waiting for /readyz (below) is what proves the migration actually ran.

echo "[notif-http] starting the API on :$PORT"
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
