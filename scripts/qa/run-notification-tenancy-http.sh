#!/usr/bin/env bash
# The tenancy boundary of POST /notifications/emit, over a real socket with two real tenants.
#
#   QA_DB=omnion_qa_w8_notif_tenant bash scripts/qa/run-notification-tenancy-http.sh
#
# ## Why this gate exists
#
# Tick 73 found that the SLA worker's recipient guard asked a different question from the one
# its own comment described, and fixed it by ADDING `existing_users_in_organization` rather
# than by changing the shared helper — because the emit route genuinely wanted the weaker
# question. The tick closed with the question that gate could not answer:
#
#   *is the weaker question actually legitimate here, or was it the third instance of the same
#   defect one route up?*
#
# This gate answers it as a measurement rather than an argument. The emit route does three
# things at once, and only the first was ever checked:
#
#   1. `existing_users` — "is this a real account". The friendly pre-check.
#   2. `record_with_deliveries(pool, session.user.organization_id, …)` — the row's
#      `organization_id` is bound from the **sender's** session.
#   3. nothing — whether the recipient belongs to the sender's tenant.
#
# So a tenant with `notifications.send` could address any account on the platform, and the row
# it wrote carried the **sending** organization next to a recipient belonging to a different
# one. That is the exact shape tick 73 called "a tenant leak wearing a select element", one
# route up, with the leak in the *insert* rather than in the *pre-check*.
#
# ## What it drives
#
# Six legs, in this order, and the order is load-bearing:
#
#   0. the premise (B's session resolves to tenant B, and B can reach the handler at all) —
#      tick 72's lesson, where eleven green-looking FAILs measured the *permission catalogue*
#      because `403 considered: 0` says "no rule even looked";
#   1. a **positive control**: B emits to a colleague inside its own tenant and the row lands.
#      A gate whose only cross-tenant assertions are "nothing was written" is also satisfied by
#      a route that writes nothing for anybody, which is the shape `run-crm-intake` hit at
#      tick 68;
#   2. the leak: B emits to tenant A's account → refused, and **no row anywhere**, counted over
#      every inbox rather than over A's ("A's inbox is empty" passes on a two-person fixture
#      even when the row reached a third recipient);
#   3. the **existence oracle**: an id that does not exist and an id that exists on another
#      tenant must be indistinguishable in status code and in error code. REQ-021's own error
#      contract says 404-for-another-user's-notification exists "so existence does not leak";
#      the emit route honours that on the read side and, until this gate, violated it on the
#      write side by distinguishing the two ids;
#   4. **atomicity across the boundary**: a batch of [own tenant, other tenant] writes neither;
#   5. the **platform sender**: an orgless (platform) account addressing a tenant's user is
#      allowed — that is the router's documented unscoped branch — and the row it writes is
#      stamped with the **recipient's** organization rather than the sender's `null`.
#
# Leg 5 is the half that explains why the fix is not "reject every cross-tenant emit": the
# recipient, not the sender, is the tenant a notification belongs to. Stamping the sender's
# column is what made a platform announcement invisible in the tenant's own admin log while
# being perfectly visible in its user's bell.
#
# ## Its own database, never the pass's
#
# `run-crm-tenancy-http.sh` is the model, and so is its refusal to start if pointed at
# `omnion_qa` or `omnion_qa_w8`: `DROP DATABASE` on the walkthrough's own stack terminates the
# browser pass's API connections and fails twenty routes from the cause.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_INCREMENTAL=0
export CARGO_TARGET_DIR="${QA_CARGO_TARGET_DIR:-$ROOT/target}"

DB="${QA_DB:-omnion_qa_w8_notif_tenant}"
# Read by the API launch further down, so it is defined here rather than beside the flush.
REDIS_DB="${QA_NOTIF_TENANCY_REDIS_DB:-15}"
PORT="${QA_NOTIF_TENANCY_PORT:-18089}"
URL="http://127.0.0.1:$PORT"
PGHOST=127.0.0.1
PGPORT=5433
export PGPASSWORD=omnion
PSQL=(psql -h "$PGHOST" -p "$PGPORT" -U omnion -d postgres -v ON_ERROR_STOP=1 -q)

# Lift the credentials prefix as a PREFIX, byte-level from a sibling gate, never from a
# rendered line: a tool masks credentials in its output and the mask is what gets pasted.
# Three times on this branch that has presented as `28P01` on every single assertion, which
# reads as a broken gate rather than a broken copy. The regex stops at the port, so the
# separator goes HERE and not in the caller — lifting it whole produced
# `postgres://…:5433omnion_qa_w8_notif_tenant`, refused at boot as `invalid port number`.
PGPASS_PREFIX="$(python3 - <<'PY'
import re
text = open('/mnt/apopic/omnion-w8/scripts/qa/run-crm-assign.sh', encoding='utf-8').read()
m = re.search(r'DATABASE_URL="(postgres://[^@"\[\]]+@127\.0\.0\.1:5433)/', text)
print(m.group(1) if m else '')
PY
)"
if [ -z "$PGPASS_PREFIX" ]; then
  echo "  FAIL could not lift the database credentials prefix from the sibling gate" >&2
  exit 1
fi

if [ "$DB" = "omnion_qa" ] || [ "$DB" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database (${DB})." >&2
  echo "        Give the gate its own, as the other w8 gates do." >&2
  exit 1
fi

# Two verbs, and the split is the point. `pass` is a LOG line: the fixture uses it for its own
# setup steps ("created tenant A", "seeded a second account"), and counting those as assertions
# produced `PASS 16/7` on a run with seven green legs — arithmetically impossible, so it reads
# as a lie even though every leg passed. `leg` is an assertion and only a leg increments it.
pass() { printf '  ok   %s\n' "$1"; }
leg() {
  printf '  ok   %s\n' "$1"
  LEGS=$(( LEGS + 1 ))
}
# Every failure increments a counter *and* raises the flag. The flag alone made the summary
# line claim "1 failing leg(s)" on a run with four red legs — the count is what a reader acts on.
fail() { printf '  FAIL %s\n' "$1"; FAILED=1; FAILURES=$(( FAILURES + 1 )); }
FAILED=0
FAILURES=0
LEGS=0

API_PID=""
cleanup() {
  # By pid AND by port. A gate that leaves its API behind poisons its own next run: the orphan
  # keeps the port, the new instance exits on EADDRINUSE, and the gate then reports the
  # *previous* build's answers under this build's name — a plausible pass over stale code.
  [ -n "${API_PID}" ] && kill "$API_PID" 2>/dev/null || true
  if command -v ss >/dev/null 2>&1; then
    # `|| true` is load-bearing under `set -o pipefail`: a grep that matches nothing returns 1
    # and the pipeline's status IS grep's, so on a clean port — the case where there is
    # nothing to kill — the gate would die on its own pre-flight check and print one line.
    ss -tlnp 2>/dev/null | grep ":$PORT " | grep -o 'pid=[0-9]*' | cut -d= -f2 \
      | while read -r pid; do [ "$pid" != "$$" ] && kill "$pid" 2>/dev/null || true; done \
      || true
  fi
  "${PSQL[@]}" -c "drop database if exists $DB" >/dev/null 2>&1 || true
  redis-cli -h 127.0.0.1 -p "${QA_REDIS_PORT:-6380}" -n "$REDIS_DB" flushdb >/dev/null 2>&1 || true
  rm -f /tmp/notif-tenant-*.cookie /tmp/notif-tenant-*.json
}
trap cleanup EXIT

echo "[notif-tenancy] creating a disposable database"
"${PSQL[@]}" -c "drop database if exists $DB" >/dev/null
"${PSQL[@]}" -c "create database $DB"
# The migrations are NOT applied here: the API applies them on boot. A file this script
# pre-applied is a file the API then refuses with "relation already exists", and it exits
# during boot while whatever is already listening on the port keeps answering.

if command -v ss >/dev/null 2>&1; then
  ss -tlnp 2>/dev/null | grep ":$PORT " | grep -o 'pid=[0-9]*' | cut -d= -f2 \
    | while read -r pid; do [ "$pid" != "$$" ] && kill "$pid" 2>/dev/null || true; done \
    || true
  sleep 1
fi

echo "[notif-tenancy] starting the API on :$PORT"
# `OMNION_CSRF_SECRET` decides whether a cookie-authenticated mutation is refused before its
# handler runs. Without one every write answers `csrf_unavailable`, which would make every
# mutation assertion a measurement of the harness's configuration rather than of the route.
# The value is throwaway: the process points at a database the trap drops, on loopback.
export OMNION_CSRF_SECRET="${OMNION_CSRF_SECRET:-qa-local-throwaway-value}"

# **The sign-in ceiling is a SHARED bucket, and this gate is not the only thing filling it.**
# The first run of this gate died two lines later with
#   `the rate limiter refused a request scope="sign_in" count=14 ceiling=10`
# and count 14 for a script that makes *four* sign-ins is the whole diagnosis: the counter is
# Redis, keyed `omnion:rl:<scope>:<bucket>:<hash of the client>` (`RatePolicy::counter_key`), so
# every writer on this box — nine sibling loops, their gates, the QA pass — counts against the
# same key for the same client (127.0.0.1 from the gate's own loopback). **The limit was never
# this route's problem; the gate was measuring its neighbours' traffic.**
#
# Two fixes were wrong before this one and both are worth naming:
#
#   * **Retrying** would have measured the same shared bucket again, later, when it happened to
#     be empty — a flaky gate that passes for a reason it cannot name.
#   * **Editing `security_settings.rate_limits` in this disposable database** is the fix that
#     reads best and is useless: the *policy* is per-database, but the *counter* is in Redis, so
#     the edit changes what the limiter would decide without changing what it has already
#     counted — and it needs the API's `reload` to take effect at all. It also failed loudly
#     (`null value in column "rate_limits" … violates not-null constraint`) because the
#     document is `[]` at boot and my positional guess at the `sign_in` entry rebuilt it as NULL.
#
# The fix is a Redis **database index** of the gate's own. It changes no product code, cannot
# collide with a sibling (nobody else uses this index), and is torn down with the trap — which
# is also the honest scope statement: this gate is not measuring the platform's rate limiter, and
# asserting otherwise would be claiming a test that does not exist. The rate limiter has its own
# gates, which drive it deliberately.
# **Flushed before the API is launched, and that order matters.** Counters are written by the
# limiter as requests arrive, so a flush issued after boot would leave everything the API did
# during startup already counted — the gate would then measure its own pre-flight against the
# ceiling it is trying to clear. The trap flushes again on the way out.
redis-cli -h 127.0.0.1 -p "${QA_REDIS_PORT:-6380}" -n "$REDIS_DB" ping >/dev/null 2>&1 || {
  echo "  FAIL Redis db $REDIS_DB is not reachable on 127.0.0.1:${QA_REDIS_PORT:-6380} — this" >&2
  echo "        gate counts its own sign-ins there and cannot measure the route without it." >&2
  exit 1
}
pass "the gate's rate-limit counters live in Redis db $REDIS_DB, not the box's shared db 0"
redis-cli -h 127.0.0.1 -p "${QA_REDIS_PORT:-6380}" -n "$REDIS_DB" flushdb >/dev/null 2>&1 || true

API_BIN="$CARGO_TARGET_DIR/debug/omnion-api"
if [ ! -x "$API_BIN" ]; then
  echo "  building omnion-api (no binary at $API_BIN)"
  cargo build -p omnion-api >/tmp/notif-tenant-build.log 2>&1 || {
    echo "  FAIL cargo build -p omnion-api failed:" >&2
    tail -25 /tmp/notif-tenant-build.log >&2
    exit 1
  }
fi

OMNION_DATABASE_URL="${PGPASS_PREFIX}/${DB}" \
OMNION_REDIS_URL="redis://127.0.0.1:${QA_REDIS_PORT:-6380}/${REDIS_DB}" \
OMNION_PORT="$PORT" \
OMNION_ENV=development \
  "$API_BIN" >/tmp/notif-tenant-api.log 2>&1 &
API_PID=$!

for _ in $(seq 1 90); do
  code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "$URL/readyz" || true)
  [ "$code" = "200" ] && break
  # A dead API must not be left to look like a slow one: the loop would poll a port nothing is
  # listening on for the full 90 s and report a boot failure as a timeout.
  kill -0 "$API_PID" 2>/dev/null || break
  sleep 1
done
if ! curl -fsS --max-time 5 "$URL/readyz" >/dev/null 2>&1; then
  echo "  FAIL the API did not reach /readyz on :$PORT (the migrations run at boot):" >&2
  tail -25 /tmp/notif-tenant-api.log >&2
  exit 1
fi
pass "the API migrated the database and answers /readyz on :$PORT"

echo "[notif-tenancy] creating two tenants through the real routes"
# The first-run flow is once-only, so the accounts go first — after the organizations an
# earlier run left behind, or the org FK blocks the delete. A gate that only passes on a
# virgin database is a gate that is never run twice.
"${PSQL[@]}" -d "$DB" -q <<'SQL'
delete from sessions where true;
delete from notification_deliveries where true;
delete from notifications where true;
delete from users where true;
delete from organizations where true;
SQL

COOKIE_P=/tmp/notif-tenant-p.cookie   # the platform account (organization_id is null)
COOKIE_A=/tmp/notif-tenant-a.cookie   # tenant A's sender
COOKIE_B=/tmp/notif-tenant-b.cookie   # tenant B's sender
rm -f "$COOKIE_P" "$COOKIE_A" "$COOKIE_B"

owner=$(curl -s -c "$COOKIE_P" -X POST "$URL/api/v1/onboarding/owner" \
  -H 'content-type: application/json' \
  -d '{"display_name":"QA Platform","email":"platform@qa.test","password":"qa-password-123"}')
if ! echo "$owner" | grep -q '"user"'; then
  echo "  FAIL the platform owner could not be created: $owner" >&2
  exit 1
fi
pass "created the platform owner through the real sign-up flow (and it signed in)"

ORG_A=$("${PSQL[@]}" -d "$DB" -t -A -c \
  "insert into organizations (name, slug, status, created_at, updated_at)
   values ('QA Tenant A', 'qa-notif-a', 'active', now(), now())
   returning id" | tr -d ' \n')
ORG_B=$("${PSQL[@]}" -d "$DB" -t -A -c \
  "insert into organizations (name, slug, status, created_at, updated_at)
   values ('QA Tenant B', 'qa-notif-b', 'active', now(), now())
   returning id" | tr -d ' \n')
if [ -z "$ORG_A" ] || [ -z "$ORG_B" ]; then
  echo "  FAIL the tenants' organizations could not be created" >&2
  exit 1
fi
pass "created tenant A ($ORG_A) and tenant B ($ORG_B)"

# One real sender inside each tenant, created through the IAM route with its own organization
# and its own password, so both hold real sessions. Without an organization a session
# resolves to `organization_required` and every claim below would measure that refusal
# instead of the tenancy boundary.
user_a=$(curl -s -X POST "$URL/api/v1/iam/users" -b "$COOKIE_P" \
  -H 'content-type: application/json' \
  -d "{\"email\":\"tenant-a@qa.test\",\"display_name\":\"QA Tenant A\",\"password\":\"qa-password-123\",\"organization_id\":\"$ORG_A\"}")
# NO `sed -n 's/.*"id":"\([0-9a-f-]\{36\}\)".*/\1/p'` — that character class admits a HYPHEN, so
# it stops at the end of the first hyphen group and hands back `90373` instead of the whole
# id. A regex written from the shape of a UUID's *first* segment matches the whole of that
# segment; one written from the shape of the whole UUID must NOT admit `-` inside the class.
# The sibling gates carry the same sed and every one of them reads the id out of a body whose
# FIRST `"id"` is the one wanted, so the truncation has stayed hidden there. `grep -o` of a
# real uuid is unambiguous — longest match by construction.
USER_A=$(echo "$user_a" | grep -o '[0-9a-f]\{8\}-[0-9a-f]\{4\}-[0-9a-f]\{4\}-[0-9a-f]\{4\}-[0-9a-f]\{12\}' | head -1)
user_b=$(curl -s -X POST "$URL/api/v1/iam/users" -b "$COOKIE_P" \
  -H 'content-type: application/json' \
  -d "{\"email\":\"tenant-b@qa.test\",\"display_name\":\"QA Tenant B\",\"password\":\"qa-password-123\",\"organization_id\":\"$ORG_B\"}")
USER_B=$(echo "$user_b" | grep -o '[0-9a-f]\{8\}-[0-9a-f]\{4\}-[0-9a-f]\{4\}-[0-9a-f]\{4\}-[0-9a-f]\{12\}' | head -1)
if [ -z "$USER_A" ] || [ -z "$USER_B" ]; then
  echo "  FAIL the tenants' accounts could not be created: $user_a / $user_b" >&2
  exit 1
fi
pass "created an account inside each tenant ($USER_A / $USER_B)"

for who in a b; do
  # NO `"$COOKIE_$who"`. Bash expands that as `$COOKIE_` followed by `$who` — two
  # expansions, not one — so under `set -u` it dies on `COOKIE_: unbound variable`
  # before the first request. A loop over two letters is not worth a second variable
  # name: the two logins are two lines and the repetition reads as the fixture it is.
  case "$who" in
    a) want=$USER_A; cookie=$COOKIE_A ;;
    b) want=$USER_B; cookie=$COOKIE_B ;;
  esac
  login=$(curl -s -c "$cookie" -X POST "$URL/api/v1/auth/login" \
    -H 'content-type: application/json' \
    -d "{\"email\":\"tenant-$who@qa.test\",\"password\":\"qa-password-123\"}")
  # **The login body is read, not discarded.** `>/dev/null` on a sign-in means a failure arrives
  # one line later as "session does not answer /me", which names the *symptom* three steps from
  # its cause — and on this branch that symptom once hid a rate limiter refusing the request.
  if ! echo "$login" | grep -q 'session\|user'; then
    echo "  FAIL tenant $who could not sign in: $login" >&2
    exit 1
  fi
  me=$(curl -s "$URL/api/v1/me" -b "$cookie" || true)
  if ! echo "$me" | grep -q "$want"; then
    echo "  FAIL tenant $who's session does not answer /me as that account: $me" >&2
    exit 1
  fi
done
pass "both tenants signed in and each session resolves to its own account"

# A third account inside tenant A: the recipient of the cross-tenant emit. Two people per
# tenant is what makes the absence assertion meaningful — "A's sender's inbox is empty" would
# also pass on a fixture with one person, where the leak and the refusal look identical.
victim=$("${PSQL[@]}" -d "$DB" -t -A -c \
  "insert into users (email, display_name, password_hash, status, organization_id, created_at, updated_at)
   select 'victim-a@qa.test', 'QA Victim A', password_hash, 'active', '$ORG_A', now(), now()
   from users where id = '$USER_A' returning id" | tr -d ' \n')
if [ -z "$victim" ]; then
  echo "  FAIL tenant A's second account could not be seeded" >&2
  exit 1
fi
pass "seeded a second account inside tenant A ($victim)"

# A second account inside **tenant B**, for the leg that asks whether a *disabled* colleague is
# still addressable. Two traps in one leg, both found by running it:
#
#   * the first version disabled `$USER_B` — the sender. That is the account holding the
#     session, and disabling it answers the next request with `401 invalid_session`, so the leg
#     measured session revocation rather than addressability;
#   * the version before that disabled `$USER_A` while B sent, which is a cross-tenant emit —
#     i.e. the leak — and so passed *because of the defect* it was written to rule out.
#
# The fixture needs a third person: enabled sender, disabled colleague, same tenant.
colleague=$("${PSQL[@]}" -d "$DB" -t -A -c \
  "insert into users (email, display_name, password_hash, status, organization_id, created_at, updated_at)
   select 'colleague-b@qa.test', 'QA Colleague B', password_hash, 'active', '$ORG_B', now(), now()
   from users where id = '$USER_B' returning id" | tr -d ' \n')
if [ -z "$colleague" ]; then
  echo "  FAIL tenant B's second account could not be seeded" >&2
  exit 1
fi
pass "seeded a second account inside tenant B ($colleague)"

# The permissions the emit route actually guards on, read off `guards::require` in
# `routes/mod.rs` rather than off the request's prose. A permission list written from prose
# is a list of wishes: `notifications.emit` is a plausible invention and it is not a
# catalogue key, so granting it fails the foreign key and the gate reports a fixture error.
# Scope lives on the BINDING, not on the role — `roles` has no `scope_type` column.
"${PSQL[@]}" -d "$DB" -q <<SQL
insert into roles (organization_id, key, name, description, priority, created_at, updated_at)
values ('$ORG_A', 'qa-notif-a-sender', 'QA Tenant A Sender', 'Notifications, inside tenant A only', 400, now(), now()),
       ('$ORG_B', 'qa-notif-b-sender', 'QA Tenant B Sender', 'Notifications, inside tenant B only', 400, now(), now())
on conflict do nothing;

insert into role_permissions (role_id, permission_key, effect, created_at)
select r.id, k, 'allow', now()
from roles r
cross join unnest(array['notifications.read','notifications.send']) as k
where r.key in ('qa-notif-a-sender','qa-notif-b-sender')
on conflict (role_id, permission_key) do nothing;

insert into role_bindings (role_id, user_id, scope_type, organization_id, granted_by, created_at)
select r.id, u.id, 'organization', r.organization_id, p.id, now()
from roles r
join users u on u.organization_id = r.organization_id
join users p on p.email = 'platform@qa.test'
where r.key in ('qa-notif-a-sender','qa-notif-b-sender')
on conflict do nothing;
SQL

# Read the grant back rather than trusting the inserts. `role_bindings` has no unique
# constraint on (role_id, user_id) that `on conflict` matches by default in this form, so the
# rows would double on a second run — silently widening what the gate believes it granted.
granted=$("${PSQL[@]}" -d "$DB" -t -A -c \
  "select count(*) from role_bindings rb
     join role_permissions rp on rp.role_id = rb.role_id
    where rb.user_id = '$USER_B' and rb.organization_id = '$ORG_B'
      and rp.permission_key = 'notifications.send'" | tr -d ' \n')
if [ "${granted:-0}" -lt 1 ]; then
  echo "  FAIL tenant B does not hold notifications.send — every leg would measure the" >&2
  echo "        permission catalogue instead of the tenancy boundary." >&2
  exit 1
fi
pass "tenant B's account holds notifications.send inside its own tenant"

# The recipients go into the body as JSON STRINGS. An unquoted uuid is a bare token, and
# `4dca9cc3-f7f9-4de3-a47d-e224f9faa209` is not valid JSON at all — the hyphen is a minus sign,
# so the parse fails at `user_ids[0]` with `invalid number`, and the route answers 422.
#
# **That failure made legs 1 and 2 pass, which is the finding worth keeping.** Leg 1 asserts
# "a cross-tenant emit is refused" and leg 2 asserts "a stranger's id and a nonexistent id
# are one answer"; a 422 satisfies both, because a body the server cannot read is refused
# before any tenancy question is asked. Two legs were green against a request that never
# reached the code. **A status-only assertion is satisfied by any refusal, and a refusal is
# what a malformed body produces.** Both legs assert the STATUS *and* that the answer is a
# sentence about recipients rather than a parse error, and leg 0 — the positive control —
# exists precisely because "everything is refused" is a shape a broken request also has.
#
# The quoting is done by `sed`, not by hand: the ids are bound into a heredoc-quoted string
# above, and retyping them into quotes is how a fixture starts lying about its own ids.
emit() { # emit <cookie> <title> <id,id,…>  -> status on stdout, body in $BODY_FILE
  local cookie=$1 title=$2 ids=$3
  local quoted
  quoted=$(echo "$ids" | sed 's/\([0-9a-f-]\{36\}\)/"\1"/g')
  curl -s -o "$BODY_FILE" -w '%{http_code}' -X POST "$URL/api/v1/notifications/emit" \
    -b "$cookie" -H 'content-type: application/json' \
    -d "{\"category\":\"update\",\"title\":\"$title\",\"user_ids\":[$quoted]}"
}
BODY_FILE=/tmp/notif-tenant-body.json
GHOST_ID="00000000-0000-4000-8000-00000000dead"

# ---------------------------------------------------------------------------------------------
# Leg 0 — the positive control. A route that refuses everybody satisfies every "nothing was
# written" assertion below, which is the silent-pass shape `run-crm-intake` hit at tick 68.
# So first: B tells its OWN tenant something, and the row lands in B's own inbox.
# ---------------------------------------------------------------------------------------------
status=$(emit "$COOKIE_B" "QA own tenant" "$USER_B")
body=$(cat "$BODY_FILE")
own_row=$("${PSQL[@]}" -d "$DB" -t -A -c \
  "select count(*) from notifications where user_id = '$USER_B'" | tr -d ' \n')
if [ "$status" = "202" ] && echo "$body" | grep -q '"created":1' && [ "${own_row:-0}" = "1" ]; then
  leg "leg 0 · B emits to its own tenant and the row lands (created=1, rows=1)"
else
  fail "leg 0 · B's own-tenant emit answered $status: $body (rows=${own_row:-?})"
fi

# ---------------------------------------------------------------------------------------------
# Leg 1 — the leak. B addresses tenant A's account. Before the fix this answers 200 and the
# row carries ORG_B next to a recipient belonging to ORG_A.
#
# The absence assertion is `count(*)` over EVERY inbox, not "A's inbox is empty": with two
# people per tenant the two are identical, and the difference only shows when the row reached
# somebody else — which is exactly the population a leak reaches.
# ---------------------------------------------------------------------------------------------
status=$(emit "$COOKIE_B" "QA cross tenant" "$USER_A")
body=$(cat "$BODY_FILE")
leaked=$("${PSQL[@]}" -d "$DB" -t -A -c \
  "select count(*) from notifications where title = 'QA cross tenant'" | tr -d ' \n')
# The answer must be a sentence about recipients. A parse error also refuses, so requiring only
# the status would let a body the server could not read pass this leg — which is exactly what
# the first two runs of this gate did.
if [ "$status" = "400" ] && [ "${leaked:-1}" = "0" ] && echo "$body" | grep -q 'recipient'; then
  leg "leg 1 · an emit across the tenancy boundary is refused in a sentence and writes nothing anywhere"
else
  fail "leg 1 · a cross-tenant emit answered $status and wrote ${leaked:-?} row(s): $body"
fi

# ---------------------------------------------------------------------------------------------
# Leg 2 — the existence oracle. An id that does not exist and an id that exists on another
# tenant must be indistinguishable. REQ-021's own error contract says a notification that is
# not yours is 404 "never 403, so existence does not leak"; the emit route honoured that on
# the read side and, before this gate, violated it on the write side by telling the caller
# that a stranger's account is an account.
# ---------------------------------------------------------------------------------------------
ghost_status=$(emit "$COOKIE_B" "QA ghost" "$GHOST_ID")
ghost_body=$(cat "$BODY_FILE")
cross_status=$status   # leg 1's status, kept: the two answers must agree
cross_body=$body
ghost_code=$(echo "$ghost_body" | sed -n 's/.*"code":"\([^"]*\)".*/\1/p')
cross_code=$(echo "$cross_body" | sed -n 's/.*"code":"\([^"]*\)".*/\1/p')
if [ "$ghost_status" = "$cross_status" ] && [ -n "$ghost_code" ] && [ "$ghost_code" = "$cross_code" ]; then
  leg "leg 2 · a stranger's id and a nonexistent id are one answer ($ghost_status $ghost_code)"
else
  fail "leg 2 · nonexistent=$ghost_status/$ghost_code stranger=$cross_status/$cross_code — the error distinguishes them"
fi

# ---------------------------------------------------------------------------------------------
# Leg 3 — atomicity across the boundary. [own tenant, other tenant] writes NEITHER, which is
# the property the existing "mixed batch" assertion proved for existence and which has to
# hold for tenancy too: the loop writes row by row, so a guard that only checks the first id
# would drop B's own colleague's notification and still report a refusal.
# ---------------------------------------------------------------------------------------------
before=$("${PSQL[@]}" -d "$DB" -t -A -c "select count(*) from notifications" | tr -d ' \n')
status=$(emit "$COOKIE_B" "QA mixed tenant" "$USER_B,$USER_A")
after=$("${PSQL[@]}" -d "$DB" -t -A -c "select count(*) from notifications" | tr -d ' \n')
if [ "$status" = "400" ] && [ "$before" = "$after" ]; then
  leg "leg 3 · a batch spanning the boundary writes none of the good ones"
else
  fail "leg 3 · the mixed batch answered $status and moved the row count $before -> $after"
fi

# ---------------------------------------------------------------------------------------------
# Leg 4 — the platform sender. An orgless account addressing a tenant's user is ALLOWED:
# that is the router's documented unscoped branch ("a platform-level fact"), and refusing it
# would take the platform's own announcements away. The row it writes must carry the
# RECIPIENT's organization, not the sender's `null` — a row stamped with the sender's column
# is invisible in the tenant's admin delivery log while being perfectly visible in its user's
# bell, which is the same bug from the other direction.
# ---------------------------------------------------------------------------------------------
status=$(emit "$COOKIE_P" "QA platform announcement" "$USER_A")
body=$(cat "$BODY_FILE")
stamped=$("${PSQL[@]}" -d "$DB" -t -A -c \
  "select coalesce(organization_id::text, 'null') from notifications where title = 'QA platform announcement'" | tr -d ' \n')
if [ "$status" = "202" ] && [ "$stamped" = "$ORG_A" ]; then
  leg "leg 4 · the platform may address a tenant, and the row is stamped with the RECIPIENT's organization"
elif [ "$status" = "400" ]; then
  fail "leg 4 · the platform account cannot address a tenant's user: $body"
else
  fail "leg 4 · the platform emit answered $status and the row's organization_id is '$stamped' (want $ORG_A)"
fi

# ---------------------------------------------------------------------------------------------
# Leg 5 — the neighbour that must stay green. Tick 73's fix deliberately carried NO status
# filter, and the surviving old test was the control proving it. A tenancy guard that also
# became an access check would drop every escalation for a colleague on leave, so the same
# independence is asserted here: a DISABLED account inside the SENDER'S OWN tenant is still
# addressable.
#
# **This leg was green before the fix for the wrong reason, which is why it is worth a
# paragraph.** It disabled `$USER_A` — tenant A's account — while tenant **B** sent, so what it
# actually measured was a cross-tenant emit, i.e. the leak. After the fix the same leg answers
# 400, and the 400 is *correct*: B may not address A. The leg had been passing as a
# measurement of the defect and reading as "the disabled-account neighbour is fine".
#
# The self-address is the fix: `notifications.send` is about writing into *other* people's
# inboxes, but a sender addressing itself inside its own tenant must not be turned away either,
# and a disabled account must not become unreachable by that route. Every green leg has to be
# re-read for which fact it was accidentally measuring.
"${PSQL[@]}" -d "$DB" -q -c "update users set status = 'disabled' where id = '$colleague'"
status=$(emit "$COOKIE_B" "QA disabled own tenant" "$colleague")
body=$(cat "$BODY_FILE")
if [ "$status" = "202" ]; then
  leg "leg 5 · a disabled account inside the sender's own tenant is still addressable"
else
  fail "leg 5 · a disabled own-tenant recipient answered $status: $body"
fi

# Leg 6 — the control for leg 5's correction: the same self-address to an account that IS in
# the sender's tenant, still enabled, must behave identically. Without it, "disabled" and
# "somebody else" are indistinguishable in leg 5's answer and a guard that refused everything
# would pass both.
"${PSQL[@]}" -d "$DB" -q -c "update users set status = 'active' where id = '$colleague'"
status=$(emit "$COOKIE_B" "QA enabled own tenant" "$colleague")
body=$(cat "$BODY_FILE")
if [ "$status" = "202" ]; then
  leg "leg 6 · the same self-address with the account enabled behaves identically"
else
  fail "leg 6 · the enabled own-tenant self-address answered $status: $body"
fi

echo "[notif-tenancy] ${FAILURES} failing leg(s)"
FAILED=$(( FAILURES > 0 ))
if [ "$FAILED" = "0" ]; then
  expected_legs=7
if [ "$LEGS" != "$expected_legs" ]; then
  # A gate that reports its own denominator has measured its harness: six green lines out of
  # seven expected is a leg that never RAN, and `set -e` never notices a conditional that was
  # not taken. This is the tick-68 silent-gate shape one layer out — there the gate ran zero
  # tests and exited 0, here a leg could be skipped and the summary would still be printed.
  fail "only $LEGS of $expected_legs legs ran — a leg was skipped, not passed"
fi
echo "PASS $LEGS/$expected_legs"
else
  echo "FAIL"
  exit 1
fi
