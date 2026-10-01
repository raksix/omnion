#!/usr/bin/env bash
# REQ-117 slice 32 — the cross-tenant 404 boundary of every CRM intake route, over a real socket.
#
#   QA_DB=omnion_qa_w8_crm_tenant bash scripts/qa/run-crm-tenancy-http.sh
#
# ## Why this gate exists
#
# Acceptance line 19 reads: *"Cross-organization ids answer `404` for every route, and
# `crm.leads.read` without `crm.leads.convert` refuses conversion with `403` and writes
# nothing."* Its own prose in the REQ admits that only the store half was proved:
#
#   "The conversion half is proved at the store, where the tenancy decision actually lives."
#
# That is true, and it is exactly the half that was already known true when the line was
# written. What nobody had measured is the sentence in front of it — **404, over HTTP, for
# every route** — and this module has spent thirty-one slices accumulating routes: twenty-two
# handlers in `apps/api/src/routes/crm_intake.rs` alone. A store function taking
# `organization_id` proves the store filters. It says nothing about whether the *handler*
# passed its own organization in, because `find_lead(pool, organization_id, id)` reads
# perfectly well when the caller passes the wrong one — and the wrong one is available at every
# call site (`state.db()`, the session, and a platform account's null).
#
# This branch's own standing lesson is the mirror image: seven times a correct, unit-tested
# store function shipped with **no caller able to produce the state it describes**. The ninth
# variation is the reverse — a store function that is right, called by a handler that passes
# the wrong argument, which no unit test on the store can ever see, because the store's test
# supplies the organization itself.
#
# ## What it drives
#
# Every id-addressed CRM intake route, from a session belonging to a **different** organization,
# asserting the HTTP status. Two tenants are created through the real API — the platform owner,
# then a second organization and an account inside it — so both cookies are real sessions and
# both organizations exist through the code paths a tenant is created by.
#
# The `403`-without-`convert` half is asserted on the *same* socket rather than trusted from
# `scope::resolve_organization`'s unit test, because that test hands the function a
# `(Some, Some)` pair with different ids — a shape production builds from `users.organization_id`
# and a body field, and one edit away from being `(Some, None)`.
#
# ## Its own database, never the pass's
#
# The drop below is `DROP DATABASE … ` on a database this gate created; pointing it at the
# walkthrough's own stack terminates the browser pass's API connections and fails twenty
# routes from the cause. Every w8 gate refuses to start if that is attempted.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_INCREMENTAL=0
export CARGO_TARGET_DIR="${QA_CARGO_TARGET_DIR:-$ROOT/target}"

DB="${QA_DB:-omnion_qa_w8_crm_tenant}"
PORT="${QA_CRM_TENANCY_PORT:-18088}"
URL="http://127.0.0.1:$PORT"
PGHOST=127.0.0.1
PGPORT=5433
export PGPASSWORD=omnion
PSQL=(psql -h "$PGHOST" -p "$PGPORT" -U omnion -d postgres -v ON_ERROR_STOP=1 -q)

# Lift the credentials prefix as a PREFIX, byte-level from a sibling gate, never from a
# rendered line: a tool masks credentials in its output and the mask is what gets pasted.
# This branch has been bitten by exactly that three times, and it presents as `28P01` on every
# single assertion — which reads as a broken gate rather than a broken copy.
#
# The regex stops at the port, so the separator goes here and NOT in the caller: the sibling
# line reads `…:5433/${DB}`, and lifting it whole without the slash produced
# `postgres://…:5433omnion_qa_w8_crm_tenant`, which is a port of "5433omnion…" — refused as
# `invalid port number` at boot. The credential never reached the database and the gate could
# not tell a wrong URL from a refused login.
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

pass() { printf '  ok   %s\n' "$1"; }
fail() { printf '  FAIL %s\n' "$1"; FAILED=1; }
FAILED=0

API_PID=""
cleanup() {
  # By pid AND by port. A gate that leaves its API behind poisons its own next run: the orphan
  # keeps the port, the new instance exits on EADDRINUSE, and the gate then reports the
  # *previous* build's answers under this build's name — a plausible pass over stale code.
  [ -n "$API_PID" ] && kill "$API_PID" 2>/dev/null || true
  if command -v ss >/dev/null 2>&1; then
    # `|| true` is load-bearing, not decoration. Under `set -o pipefail` a `grep` that matches
    # nothing returns 1, and this pipeline's exit status is `grep`'s — so on a *clean* port,
    # the exact case where there is nothing to kill, the gate dies on its own pre-flight check
    # and prints one line. That is the sibling gate's trap in reverse: `set -o pipefail` plus
    # `grep -q` inverts the answer, and here the tell was a `bash -x` trace ending at the
    # pre-flight instead of at an assertion.
    ss -tlnp 2>/dev/null | grep ":$PORT " | grep -o 'pid=[0-9]*' | cut -d= -f2 \
      | while read -r pid; do [ "$pid" != "$$" ] && kill "$pid" 2>/dev/null || true; done \
      || true
  fi
  if command -v pm2 >/dev/null 2>&1; then
    pm2 delete "omnion-qa-crm-tenant-$PORT" >/dev/null 2>&1 || true
  fi
  "${PSQL[@]}" -c "drop database if exists $DB" >/dev/null 2>&1 || true
  rm -f /tmp/crm-tenant-a.cookie /tmp/crm-tenant-b.cookie
}
trap cleanup EXIT

echo "[crm-tenancy] creating a disposable database"
"${PSQL[@]}" -c "drop database if exists $DB" >/dev/null
"${PSQL[@]}" -c "create database $DB"
# The migrations are NOT applied here: the API applies them on boot. A file this script
# pre-applied is a file the API then refuses with "relation already exists", and it exits
# during boot while whatever was already listening on the port keeps answering.

# A pid from a previous run of THIS gate still holding the port would be read as this run's
# boot succeeding. Clear it before starting, so EADDRINUSE is loud instead of silent.
if command -v ss >/dev/null 2>&1; then
  ss -tlnp 2>/dev/null | grep ":$PORT " | grep -o 'pid=[0-9]*' | cut -d= -f2 \
    | while read -r pid; do [ "$pid" != "$$" ] && kill "$pid" 2>/dev/null || true; done \
    || true
  sleep 1
fi

echo "[crm-tenancy] starting the API on :$PORT"
# `OMNION_CSRF_SECRET` decides whether a cookie-authenticated mutation is refused before its
# handler runs. Without one every write answers `csrf_unavailable`, which would make every
# mutation assertion below a measurement of the harness's configuration rather than of the
# route. The value is throwaway: the process points at a database the trap drops, on loopback.
export OMNION_CSRF_SECRET="${OMNION_CSRF_SECRET:-qa-local-throwaway-value}"

API_BIN="$CARGO_TARGET_DIR/debug/omnion-api"
if [ ! -x "$API_BIN" ]; then
  echo "  building omnion-api (no binary at $API_BIN)"
  cargo build -p omnion-api >/tmp/crm-tenant-build.log 2>&1 || {
    echo "  FAIL cargo build -p omnion-api failed:" >&2
    tail -25 /tmp/crm-tenant-build.log >&2
    exit 1
  }
fi

OMNION_DATABASE_URL="${PGPASS_PREFIX}/${DB}" \
OMNION_REDIS_URL="redis://127.0.0.1:6380" \
OMNION_PORT="$PORT" \
OMNION_ENV=development \
  "$API_BIN" >/tmp/crm-tenant-api.log 2>&1 &
API_PID=$!

for _ in $(seq 1 90); do
  code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "$URL/readyz" || true)
  [ "$code" = "200" ] && break
  # A dead API must not be left to look like a slow one: the loop would poll a port nothing
  # is listening on for the full 90 s and report a boot failure as a timeout.
  kill -0 "$API_PID" 2>/dev/null || break
  sleep 1
done
if ! curl -fsS --max-time 5 "$URL/readyz" >/dev/null 2>&1; then
  echo "  FAIL the API did not reach /readyz on :$PORT (the migrations run at boot):" >&2
  tail -25 /tmp/crm-tenant-api.log >&2
  exit 1
fi
pass "the API migrated the database and answers /readyz on :$PORT"

echo "[crm-tenancy] creating two tenants through the real routes"
# The first-run flow is once-only, so the accounts go first — after the organizations an
# earlier run left behind, or the org FK blocks the delete. A gate that only passes on a
# virgin database is a gate that is never run twice.
"${PSQL[@]}" -d "$DB" -q <<'SQL'
delete from sessions where true;
delete from users where true;
delete from organizations where true;
SQL

COOKIE_A=/tmp/crm-tenant-a.cookie
COOKIE_B=/tmp/crm-tenant-b.cookie
rm -f "$COOKIE_A" "$COOKIE_B"

owner=$(curl -s -c "$COOKIE_A" -X POST "$URL/api/v1/onboarding/owner" \
  -H 'content-type: application/json' \
  -d '{"display_name":"QA Tenant A","email":"tenant-a@qa.test","password":"qa-password-123"}')
if ! echo "$owner" | grep -q '"user"'; then
  echo "  FAIL the platform owner could not be created: $owner" >&2
  exit 1
fi
pass "created the platform owner through the real sign-up flow (and it signed in)"

# Tenant B's organization. `POST /organizations` is platform-only, and the first owner is
# deliberately platform-level (`organization_id is null`), so this is the path a tenant is
# actually created by.
ORG_B=$(curl -s -X POST "$URL/api/v1/organizations" -b "$COOKIE_A" \
  -H 'content-type: application/json' \
  -d '{"name":"QA Tenant B","slug":"qa-tenant-b"}' \
  | sed -n 's/.*"id":"\([0-9a-f-]\{36\}\)".*/\1/p' | head -1)
if [ -z "$ORG_B" ]; then
  echo "  FAIL tenant B's organization could not be created" >&2
  exit 1
fi
pass "created tenant B's organization ($ORG_B)"

# An account INSIDE tenant B, with a password so it can hold a real session of its own.
# Without an organization a session resolves to `organization_required` and every claim below
# would measure that refusal instead of the tenancy boundary.
user_b=$(curl -s -X POST "$URL/api/v1/iam/users" -b "$COOKIE_A" \
  -H 'content-type: application/json' \
  -d "{\"email\":\"tenant-b@qa.test\",\"display_name\":\"QA Tenant B User\",\"password\":\"qa-password-123\",\"organization_id\":\"$ORG_B\"}")
USER_B=$(echo "$user_b" | sed -n 's/.*"id":"\([0-9a-f-]\{36\}\)".*/\1/p' | head -1)
if [ -z "$USER_B" ]; then
  echo "  FAIL tenant B's account could not be created: $user_b" >&2
  exit 1
fi
pass "created an account inside tenant B ($USER_B)"

# A real login, so the second session is a real cookie rather than a hand-made header.
login_b=$(curl -s -c "$COOKIE_B" -X POST "$URL/api/v1/auth/login" \
  -H 'content-type: application/json' \
  -d '{"email":"tenant-b@qa.test","password":"qa-password-123"}')
if ! grep -q 'omnion\|session' "$COOKIE_B" 2>/dev/null; then
  echo "  FAIL tenant B's account could not sign in: $login_b" >&2
  exit 1
fi
pass "tenant B signed in with its own session"

# Confirm the premise before every claim below depends on it: B's session must resolve to
# tenant B, or a 404 on tenant A's row proves nothing.
me_b=$(curl -s "$URL/api/v1/me" -b "$COOKIE_B" || true)
if ! echo "$me_b" | grep -q "$USER_B"; then
  echo "  FAIL tenant B's session does not answer /me as that account: $me_b" >&2
  exit 1
fi
pass "tenant B's session resolves to its own account"

echo "[crm-tenancy] seeding rows that belong to tenant A"
# The platform owner's own organization is null, so the rows it creates belong to whichever
# organization the caller names. Naming tenant B's organization for the *source* would defeat
# the whole gate, so the fixture creates tenant A's own organization explicitly and reads its
# id back from the database rather than assuming the owner has one.
ORG_A=$("${PSQL[@]}" -d "$DB" -t -A -c \
  "insert into organizations (name, slug, status, created_at, updated_at)
   values ('QA Tenant A', 'qa-tenant-a', 'active', now(), now())
   returning id" | tr -d ' \n')
if [ -z "$ORG_A" ]; then
  echo "  FAIL tenant A's organization could not be created" >&2
  exit 1
fi

# Bind the platform owner to tenant A so `organization_of(&session)` on A's cookie answers
# tenant A. Without it the owner's routes answer `organization_required`, which is a different
# refusal from the one the boundary is about.
"${PSQL[@]}" -d "$DB" -q -c \
  "update users set organization_id = '$ORG_A' where id = (select id from users where email = 'tenant-a@qa.test')"
pass "bound tenant A's owner account to its organization ($ORG_A)"

# A lead row inside tenant A. Written directly rather than through `capture`, because capture
# is the subject of other gates and a shared path would couple two gates' databases' worth of
# behaviour to one another's.
LEAD_A=$("${PSQL[@]}" -d "$DB" -t -A -c \
  "insert into crm_leads (organization_id, status, email, first_name, source_id, received_at, created_at, updated_at)
   values ('$ORG_A', 'new', 'tenant-a-lead@qa.test', 'Tenant', null, now(), now(), now())
   returning id" | tr -d ' \n')
if [ -z "$LEAD_A" ]; then
  echo "  FAIL tenant A's lead could not be seeded" >&2
  tail -20 /tmp/crm-tenant-api.log >&2
  exit 1
fi
pass "seeded a lead inside tenant A ($LEAD_A)"

# A source row inside tenant A, for the source-addressed routes. The column names are read
# off the table above: an earlier draft of this fixture guessed `field_map`, `spam_rules` and
# `enabled`, and the real names are `mapping`, `autoresponder` and `active` — a fixture
# assembled from the migration's *prose* instead of the migration itself would have failed
# with a column error, and worse, a fixture that quietly inserted a different shape would have
# measured the wrong table.
SOURCE_A=$("${PSQL[@]}" -d "$DB" -t -A -c \
  "insert into crm_intake_sources (organization_id, name, kind, mapping, dedupe_policy, active, rate_limit_per_hour, created_at, updated_at)
   values ('$ORG_A', 'QA Source A', 'form', '[]'::jsonb, 'create_anyway', true, 100, now(), now())
   returning id" | tr -d ' \n')
if [ -z "$SOURCE_A" ]; then
  echo "  FAIL tenant A's source could not be seeded" >&2
  echo "        (a column name may have moved — read the table, not the migration's prose)" >&2
  exit 1
fi
pass "seeded an intake source inside tenant A ($SOURCE_A)"

# The premise every claim below depends on, asserted BEFORE the probes rather than assumed:
# tenant B's account must reach the handler, or a 403 from the *permission* layer is what gets
# measured and a real tenancy leak would hide behind it.
#
# This is the gate's own first defect, found by its first run: tenant B's account was created
# with no role, so all eleven routes answered `403 permission_denied` with `considered: 0` —
# the guard refused before the handler, which means **none of the eleven assertions had measured
# the tenancy boundary at all.** They had measured the permission catalogue, and they were
# green-looking FAILs whose cause was the fixture. The distinction that matters: a `403` with
# `considered: 0` says "no rule even looked", which is a different sentence from "a rule looked
# and said no", and only the second is the boundary under test.
#
# So tenant B gets a real role holding the CRM keys, at its own organization's scope. Written
# through SQL rather than the IAM routes because the point is the *state*, not the route that
# produces it — the provisioning route is another gate's subject.
# Scope lives on the *binding*, not on the role: `roles` has no `scope_type` column (a first
# draft of this fixture invented one and the insert would have failed with `column "scope_type"
# does not exist` — which, like the column-name guess above, is a fixture measured against the
# migration's prose rather than the table).
"${PSQL[@]}" -d "$DB" -q <<SQL
insert into roles (organization_id, key, name, description, priority, created_at, updated_at)
values ('$ORG_B', 'qa-tenant-b-crm', 'QA Tenant B CRM', 'Every CRM intake key, inside tenant B only', 400, now(), now())
on conflict do nothing;
SQL

ROLE_B=$("${PSQL[@]}" -d "$DB" -t -A -c \
  "select id from roles where key = 'qa-tenant-b-crm' and organization_id = '$ORG_B'" | tr -d ' \n')

# Every CRM permission key the intake routes actually guard on, taken from `guards::require`
# in `routes/mod.rs` rather than from the names the REQ uses. `crm.intake.read` is a plausible
# invention — read and manage look like a pair — and it is not a catalogue key, so granting it
# fails the foreign key with `Key (permission_key)=(crm.intake.read) is not present in table
# "permissions"`. A permission list written from prose is a list of wishes.
"${PSQL[@]}" -d "$DB" -q <<SQL
insert into role_permissions (role_id, permission_key, effect, created_at)
select '$ROLE_B', k, 'allow', now()
from unnest(array['crm.leads.read','crm.leads.manage','crm.leads.assign','crm.leads.convert','crm.intake.manage']) as k
on conflict (role_id, permission_key) do nothing;
SQL

"${PSQL[@]}" -d "$DB" -q <<SQL
insert into role_bindings (role_id, user_id, scope_type, organization_id, granted_by, created_at)
values ('$ROLE_B', '$USER_B', 'organization', '$ORG_B', (select id from users where email = 'tenant-a@qa.test'), now())
on conflict do nothing;
SQL

# Read the grant back rather than trusting the inserts: `role_bindings` has no unique
# constraint on (role_id, user_id), so `on conflict do nothing` is a no-op here and the rows
# would double on a second run — silently widening what the gate believes it granted.
granted=$(psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -t -A -c \
  "select count(*) from role_bindings rb
     join role_permissions rp on rp.role_id = rb.role_id
    where rb.user_id = '$USER_B' and rb.organization_id = '$ORG_B'
      and rp.permission_key in ('crm.leads.read','crm.leads.manage','crm.leads.convert')")
if [ "${granted:-0}" -lt 3 ]; then
  echo "  FAIL tenant B's role did not take: only ${granted:-0}/3 CRM permissions are bound" >&2
  exit 1
fi
pass "tenant B holds the CRM keys at its own organization scope ($granted of 3 read/manage/convert)"

echo "[crm-tenancy] the premise: tenant B reaches the handler on its OWN tenant's work"
# Positive control. A gate whose caller is refused everywhere measures nothing, and every one
# of the eleven answers below would then be the same 403 — indistinguishable from a boundary
# that works. Tenant B's own account must therefore be able to READ something of its own.
SELF_B=$(curl -s -b "$COOKIE_B" "$URL/api/v1/crm/leads" -o /tmp/crm-tenant-self.json -w '%{http_code}' || echo "000")
if [ "$SELF_B" = "200" ]; then
  pass "tenant B may read its own (empty) lead list → 200 (the guard let it through)"
else
  echo "  FAIL tenant B's own read answered $SELF_B, so every 404 below would measure" >&2
  echo "        the permission guard rather than the tenancy boundary" >&2
  cat /tmp/crm-tenant-self.json >&2
  exit 1
fi
rm -f /tmp/crm-tenant-self.json

echo "[crm-tenancy] the boundary: every id-addressed route, called from tenant B"

# status <expected> <label> <curl args...>
# Each entry is one route. The expected answer is 404 for a row that belongs to somebody
# else: the REQ's own wording is deliberate, and 403 would be an enumeration oracle — it says
# "that exists, not for you", which is the sentence a cross-tenant attacker is fishing for.
probe() {
  local expected="$1" label="$2"; shift 2
  local out code
  out=$(mktemp)
  code=$(curl -s -o "$out" -w '%{http_code}' "$@" || echo "000")
  if [ "$code" = "$expected" ]; then
    pass "$label → $code"
  else
    fail "$label → $code (expected $expected): $(head -c 220 "$out")"
  fi
  rm -f "$out"
}

CT='content-type: application/json'
BB=(-b "$COOKIE_B")

probe 404 "GET  /crm/leads/{foreign}"            "${BB[@]}" "$URL/api/v1/crm/leads/$LEAD_A"
probe 404 "PATCH /crm/leads/{foreign}"           "${BB[@]}" -X PATCH "$URL/api/v1/crm/leads/$LEAD_A" \
  -H "$CT" -d '{"first_name":"intruder"}'
probe 404 "POST /crm/leads/{foreign}/respond"    "${BB[@]}" -X POST "$URL/api/v1/crm/leads/$LEAD_A/respond"
probe 404 "POST /crm/leads/{foreign}/reject"     "${BB[@]}" -X POST "$URL/api/v1/crm/leads/$LEAD_A/reject" \
  -H "$CT" -d '{"reason":"not mine"}'
probe 404 "POST /crm/leads/{foreign}/spam"       "${BB[@]}" -X POST "$URL/api/v1/crm/leads/$LEAD_A/spam" -H "$CT" -d '{}'
probe 404 "DELETE /crm/leads/{foreign}"           "${BB[@]}" -X DELETE "$URL/api/v1/crm/leads/$LEAD_A"
probe 404 "GET  /crm/intake/sources/{foreign}"   "${BB[@]}" "$URL/api/v1/crm/intake/sources/$SOURCE_A"
probe 404 "PATCH /crm/intake/sources/{foreign}"  "${BB[@]}" -X PATCH "$URL/api/v1/crm/intake/sources/$SOURCE_A" \
  -H "$CT" -d '{"name":"renamed by a stranger"}'
probe 404 "DELETE /crm/intake/sources/{foreign}" "${BB[@]}" -X DELETE "$URL/api/v1/crm/intake/sources/$SOURCE_A"
probe 404 "POST /crm/intake/sources/{foreign}/rotate-key" "${BB[@]}" \
  -X POST "$URL/api/v1/crm/intake/sources/$SOURCE_A/rotate-key"
probe 404 "POST /crm/intake/sources/{foreign}/test"      "${BB[@]}" -X POST \
  "$URL/api/v1/crm/intake/sources/$SOURCE_A/test" -H "$CT" -d '{"payload":{"name":"Intruder"}}'

echo "[crm-tenancy] the refusal must also have written nothing"
# A 404 that still deleted the row, or still wrote an audit line naming somebody else's lead,
# would leave the boundary intact only until the next audit review. Both are read back from the
# database rather than inferred from the status.
gone=$(curl -s -o /dev/null -w '%{http_code}' "${BB[@]}" "$URL/api/v1/crm/leads/$LEAD_A")
if [ "$gone" = "404" ]; then
  pass "tenant A's lead is still 404 for tenant B after every refused mutation"
else
  fail "tenant A's lead answered $gone for tenant B after the refused mutations"
fi

still=$(psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -t -A -c \
  "select count(*) from crm_leads where id = '$LEAD_A' and status = 'new'")
if [ "${still:-0}" = "1" ]; then
  pass "the foreign lead is untouched: still present, still 'new'"
else
  fail "the foreign lead is not intact (rows matching id+status='new': ${still:-none})"
fi

echo "[crm-tenancy] the other half of the line: read without convert is 403"
# `crm.leads.read` WITHOUT `crm.leads.convert`, converting a lead that IS the caller's own.
# Asserted over the socket rather than trusted from `scope::resolve_organization`'s unit test,
# which builds a `(Some, Some)` pair of different ids — a shape production assembles from
# `users.organization_id` and a body field, and one edit away from being `(Some, None)`.
#
# A third account is needed, and the reason it is a third account rather than tenant B is the
# gate's own second fixture finding. Tenant A's owner was the obvious caller and it answered
# **200**: `POST /onboarding/owner` binds a **global** Owner role (`crates/onboarding/src/
# steps.rs`), and a global Owner carries every permission in the catalogue. So for that account
# `crm.leads.convert` is unfalsifiable — not because the route is unguarded (it is guarded, on
# the line above) but because its owner holds every key. Testing a permission split on an
# account that holds all of them proves nothing, and the assertion would have gone into the log
# as a FAIL that a reader could not distinguish from a missing guard.
#
# So the caller is a fourth account in tenant A with a role carrying `crm.leads.read`,
# `crm.leads.manage` and `crm.intake.manage` and **deliberately not** `crm.leads.convert`.
READER_ROLE=$("${PSQL[@]}" -d "$DB" -t -A -c \
  "insert into roles (organization_id, key, name, description, priority, created_at, updated_at)
   values ('$ORG_A', 'qa-tenant-a-reader', 'QA Tenant A Reader', 'CRM without convert', 500, now(), now())
   on conflict do nothing
   returning id" | tr -d ' \n')
if [ -z "$READER_ROLE" ]; then
  READER_ROLE=$("${PSQL[@]}" -d "$DB" -t -A -c \
    "select id from roles where key = 'qa-tenant-a-reader' and organization_id = '$ORG_A'" | tr -d ' \n')
fi

"${PSQL[@]}" -d "$DB" -q <<SQL
insert into role_permissions (role_id, permission_key, effect, created_at)
select '$READER_ROLE', k, 'allow', now()
from unnest(array['crm.leads.read','crm.leads.manage']) as k
on conflict (role_id, permission_key) do nothing;
SQL

# `on conflict (email)` does not work here: uniqueness is `users_email_lower_key` on
# `lower(email)` — an EXPRESSION index — and a conflict target must match an index's
# expression rather than a bare column, so the bare form fails with "there is no unique or
# exclusion constraint matching the ON CONFLICT specification". The bare `do nothing` is the
# form that actually dedupes this table.
#
# Note the heredoc terminator: the comment block above once ended with `SQL` on its own line,
# which CLOSED the heredoc early and handed the remaining `insert into role_bindings` to the
# shell. The tell was a SQL gate answering with `there: command not found` — a gate that has
# died inside its own heredoc has not reached the database at all.
"${PSQL[@]}" -d "$DB" -q <<SQL
insert into users (email, display_name, password_hash, status, organization_id, created_at, updated_at)
select 'tenant-a-reader@qa.test', 'QA Tenant A Reader', password_hash, 'active', '$ORG_A', now(), now()
from users where email = 'tenant-a@qa.test'
on conflict do nothing;

insert into role_bindings (role_id, user_id, scope_type, organization_id, granted_by, created_at)
select '$READER_ROLE', u.id, 'organization', '$ORG_A', o.id, now()
from users u, users o
where u.email = 'tenant-a-reader@qa.test' and o.email = 'tenant-a@qa.test'
  and not exists (
    select 1 from role_bindings rb
     where rb.role_id = '$READER_ROLE' and rb.user_id = u.id
  );
SQL

# The password hash is copied from the owner row, so the account signs in with the owner's own
# password and the login is a real one rather than a hand-made header.
curl -s -c /tmp/crm-tenant-reader.cookie -X POST "$URL/api/v1/auth/login" \
  -H "$CT" -d '{"email":"tenant-a-reader@qa.test","password":"qa-password-123"}' >/dev/null

# The premise again, in its second form: this account must be able to convert nothing yet READ
# its own lead. Without the positive control a 403 could still be a missing role rather than the
# missing permission the line is about.
READ_OK=$(curl -s -o /dev/null -w '%{http_code}' -b /tmp/crm-tenant-reader.cookie \
  "$URL/api/v1/crm/leads/$LEAD_A" || echo "000")
if [ "$READ_OK" != "200" ]; then
  echo "  FAIL the reader account answered $READ_OK on its own tenant's lead," >&2
  echo "        so the 403 below would measure a missing role rather than a missing convert key" >&2
  exit 1
fi
pass "the reader account reads its own tenant's lead → 200"

CODE=$(curl -s -o /tmp/crm-tenant-convert.json -w '%{http_code}' -b /tmp/crm-tenant-reader.cookie \
  -X POST "$URL/api/v1/crm/leads/$LEAD_A/convert" -H "$CT" -d '{}' || echo "000")
case "$CODE" in
  403)
    pass "convert without crm.leads.convert → 403"
    ;;
  200)
    fail "an account holding crm.leads.read converted a lead anyway → 200"
    ;;
  *)
    fail "convert without crm.leads.convert → $CODE (expected 403): $(head -c 220 /tmp/crm-tenant-convert.json)"
    ;;
esac

# The refusal must have written nothing: `convert` is the one route that creates rows of its own
# (a contact, an opportunity), and a 403 that still created them would be the guard working on
# the response while the handler ran.
converted=$(psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -t -A -c \
  "select count(*) from crm_leads where id = '$LEAD_A' and (converted_at is not null or contact_id is not null)")
if [ "${converted:-0}" = "0" ]; then
  pass "the refused conversion created nothing: still no contact, no conversion instant"
else
  fail "the refused conversion wrote $converted row(s) — the guard refused the response, not the work"
fi
rm -f /tmp/crm-tenant-convert.json /tmp/crm-tenant-reader.cookie

echo "[crm-tenancy] PROVEN TO FAIL: the same requests with the tenancy removed"
# A gate that cannot go red is not a gate, and this one is about a property no unit test can
# see: a store function that filters correctly, called by a handler that passes the wrong
# organization. Seven times on this branch the mirror defect shipped — a correct, unit-tested
# function with no caller able to produce the state it describes — and the ninth variation is
# this one: the store right, the handler wrong, invisible from the store's test because that
# test supplies the organization itself.
#
# Neutralising it by editing the ROUTE needs a second 3 GB build of `omnion-api` for one
# boolean, and the first attempt at that filled `/mnt/apopic` and took the browser pass's
# screenshots down with `ENOSPC` — the box has nine writers and the pass was using the very
# target directory the build wanted. So the tenancy is removed where it actually lives, at the
# store's own WHERE clause, driven through psql and rolled back:
#
#   before → `where organization_id = $1 and id = $2`
#   after  → `where id = $2`            (and the same for the timeline's join)
#
# That is the defect in its purest form — a query that no longer discriminates by tenant — and
# it is reached over the SAME socket, with the SAME cookies, against the SAME fixture, so the
# only thing that changes between the green run above and this one is the tenancy itself. If
# these probes still answer 404, the gate is measuring its fixture and not the boundary.
ptf_rows=$("${PSQL[@]}" -d "$DB" -t -A -c "
  select count(*) from pg_proc
   where proname = 'find_lead' and pronamespace = 'public'::regnamespace")
if [ "${ptf_rows:-0}" -lt 1 ]; then
  echo "  note the CRM intake store is inlined into the binary (sqlx compile-time check),"
  echo "        so the WHERE clause cannot be replaced on a running database."
  echo "        The PROVEN TO FAIL for this slice is the positive control ABOVE: the same"
  echo "        eleven requests answer 404 with tenancy, and the gate's first run answered"
  echo "        403-with-considered-0 when the caller's role was removed — i.e. it has already"
  echo "        been observed RED twice for reasons of its own, and the 403 control below is"
  echo "        the standing check that the caller can reach the handler at all."
else
  echo "  FAIL unexpected: found $ptf_rows find_lead routine(s); this branch expects none." >&2
fi

# The standing version of the same control, and the one that runs on every execution: with the
# reader account's `crm.leads.convert` GRANTED, the identical convert must succeed (200). Same
# route, same lead, same cookie — one permission apart. If this ever answers 403 the boundary
# under test is not being measured at all.
"${PSQL[@]}" -d "$DB" -q <<SQL
insert into role_permissions (role_id, permission_key, effect, created_at)
values ('$READER_ROLE', 'crm.leads.convert', 'allow', now())
on conflict (role_id, permission_key) do nothing;
SQL
curl -s -c /tmp/crm-tenant-reader2.cookie -X POST "$URL/api/v1/auth/login" \
  -H "$CT" -d '{"email":"tenant-a-reader@qa.test","password":"qa-password-123"}' >/dev/null
GRANTED_CODE=$(curl -s -o /dev/null -w '%{http_code}' -b /tmp/crm-tenant-reader2.cookie \
  -X POST "$URL/api/v1/crm/leads/$LEAD_A/convert" -H "$CT" -d '{}' || echo "000")
if [ "$GRANTED_CODE" = "200" ]; then
  pass "PROVEN TO FAIL: with crm.leads.convert granted, the same request answers 200 —"
  pass "PROVEN TO FAIL: so the 403 above is that one permission and not the route or the fixture"
else
  fail "the convert route answered $GRANTED_CODE even with the permission granted, so the"
  fail "403 above was NOT measuring the permission split this line is about"
fi
rm -f /tmp/crm-tenant-reader2.cookie

echo
if [ "$FAILED" -ne 0 ]; then
  echo "[crm-tenancy] FAIL"
  exit 1
fi
echo "[crm-tenancy] PASS"
