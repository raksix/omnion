#!/usr/bin/env bash
# Ownership transfer (REQ-133, acceptance 12, the confirmations half) — over a real socket.
#
#   bash scripts/qa/run-transfer-ownership.sh
#
# ## The defect class this gate exists for
#
# Acceptance 12: *"Removing the last owner of a project is refused with a message naming the
# remedy; transferring ownership requires both confirmations and is audited."*
#
# Three claims. They are proved in three different places and have been conflated:
#
#   * **"removing the last owner is refused by name"** — slice 11, at the store
#     (`removing_the_last_owner_is_refused_by_name`).
#   * **"transferring moves both facts and is audited"** — the store, and `run-project-audit.sh`.
#   * **"requires both confirmations"** — a property of the **handler**, and this is the one that
#     had nothing driving it. `POST /projects/{id}/transfer-ownership` refuses a request whose
#     `confirm_owner` or `confirm_audit` is absent with `ownership_transfer_unconfirmed`, and the
#     dialog asks for each separately — but a handler branch that no gate ever calls is a branch
#     whose removal is invisible. Deleting the `if !input.confirm_owner || !input.confirm_audit`
#     guard would leave every store test green, because the store never received the flags.
#
# The asymmetry is the whole reason this is HTTP: the flags exist **only** in the request body.
# There is no store-level assertion that can reach them, and the previous ticks' reasoning — "the
# store proves it, the handler compiles" — is the greenest possible lie available on this branch.
#
# ## What makes the refusal PROVED rather than merely seen
#
# Three ways this gate can pass for the wrong reason, and all three are closed here:
#
#   1. **A guard answer masquerading as the validation answer.** `require_capability` runs *before*
#      the confirmation check, so a caller without the capability is refused with a permission
#      code. The gate therefore only asserts the confirmation refusal over the **owner's** own
#      session, and asserts the body names `ownership_transfer_unconfirmed` — the guard's code is
#      never that string.
#   2. **A `400` that is really a `422`.** A body that fails schema validation answers `422` with
#      the field list, which would also mean "the request was refused". The gate requires `400`
#      **and** the specific code, so a schema rejection cannot stand in for the confirmation rule.
#   3. **A transfer that half-happened.** The refusal must leave the project **and** the
#      membership row exactly as they were. `transfer_ownership` moves both `owner_user_id` and
#      the `owner` membership in one transaction; a guard that returned the error *after* calling
#      the store would still pass every status-code check above and would have handed the project
#      away. This is the assertion the whole gate is built around.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_INCREMENTAL=0
export CARGO_TARGET_DIR="${QA_CARGO_TARGET_DIR:-/mnt/apopic/w8build}"
export TMPDIR="${CARGO_TARGET_DIR}/tmp"
mkdir -p "$TMPDIR"

DB="omnion_qa_w8_transfer"
# Never the browser pass's database, never its port: the DROP below terminates the pass's API
# connections and then fails twenty routes from the cause.
PORT="${QA_API_PORT:-18091}"
URL="http://127.0.0.1:$PORT"
PGHOST=127.0.0.1
PGPORT=5433

if [ "${QA_DB:-}" = "$DB" ] || [ "$DB" = "omnion_qa" ] || [ "$DB" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the browser pass's own database (${DB})." >&2
  exit 1
fi

PGPASS_PREFIX="$(python3 - <<'PY'
import re
text = open('scripts/qa/run-crm-assign.sh', encoding='utf-8').read()
m = re.search(r'DATABASE_URL="(postgres://[^@"\[\]]+@127\.0\.0\.1:5433)/', text)
print(m.group(1) if m else '')
PY
)"
if [ -z "${PGPASS_PREFIX}" ]; then
  echo "  FAIL: could not read the QA database prefix out of run-crm-assign.sh." >&2
  exit 1
fi
case "${PGPASS_PREFIX}" in
  *'*'*) echo "  FAIL: the lifted prefix contains an asterisk run — the credential mask was copied." >&2
         exit 1 ;;
esac
export PGPASSWORD=omnion
PSQL=(psql -h "$PGHOST" -p "$PGPORT" -U omnion -d postgres -v ON_ERROR_STOP=1 -q)

pass() { printf '  ok   %s\n' "$1"; }
fail() { printf '  FAIL %s\n' "$1"; FAILED=1; }
FAILED=0

cleanup() {
  [ -n "${API_PID:-}" ] && kill "$API_PID" 2>/dev/null || true
  if command -v ss >/dev/null 2>&1; then
    ss -tlnp 2>/dev/null | grep ":$PORT " | grep -o 'pid=[0-9]*' | cut -d= -f2 \
      | xargs -r kill -9 2>/dev/null || true
  fi
  "${PSQL[@]}" -c "drop database if exists $DB" >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "[transfer] building the API"
# The build's own exit status is the authority. Piping into grep and testing *its* status
# reports failure whenever the word "error" appears inside a *warning*.
if ! cargo build -q -p omnion-api 2>/tmp/transfer-build.log; then
  grep -E "^error" /tmp/transfer-build.log || tail -20 /tmp/transfer-build.log
  echo "  build failed"
  exit 1
fi

echo "[transfer] creating a disposable database"
"${PSQL[@]}" -c "drop database if exists $DB" >/dev/null
"${PSQL[@]}" -c "create database $DB"
# The migrations are NOT applied here: the API applies them on boot, and a file pre-applied here
# is one the API then refuses with "relation already exists" -- at which point it exits during
# boot and whatever already held the port keeps answering under the new build's name.

echo "[transfer] starting the API on :$PORT"
# Without OMNION_CSRF_SECRET the CSRF middleware refuses EVERY cookie-authenticated write with
# `403 csrf_unavailable`. That failure mode is invisible here in the worst way: a 403 is not the
# 400 this gate asserts, so the positives stay honest -- but a CSRF refusal would also make the
# one "was it actually applied?" call ambiguous. Set it, and throwaway: the process points at a
# database dropped on the cleanup line and listens on loopback.
OMNION_DATABASE_URL="${PGPASS_PREFIX}/${DB}" \
OMNION_REDIS_URL="redis://127.0.0.1:6380" \
OMNION_CSRF_SECRET="qa-transfer-throwaway-secret-not-a-real-key" \
OMNION_PORT="$PORT" \
OMNION_ENV=development \
  "$CARGO_TARGET_DIR/debug/omnion-api" >/tmp/transfer-api.log 2>&1 &
API_PID=$!

for _ in $(seq 1 120); do
  code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "$URL/readyz" || true)
  [ "$code" = "200" ] && break
  kill -0 "$API_PID" 2>/dev/null || break
  sleep 1
done
if ! curl -fsS --max-time 5 "$URL/readyz" >/dev/null 2>&1; then
  echo "  API did not reach /readyz:"
  tail -20 /tmp/transfer-api.log
  exit 1
fi
pass "the API migrated the database and answers /readyz on :$PORT"

DBQ() { psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -t -A -q -c "$1"; }

# ---------------------------------------------------------------------------------------------
# 1. An owner and a successor, in one organization.
# ---------------------------------------------------------------------------------------------
echo "[transfer] an owner and a successor in the same organization"

# The first-run flow is once-only, so a re-runnable gate clears what the last run left.
psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -q <<'SQL'
delete from sessions where true;
delete from users where true;
delete from organizations where true;
SQL

COOKIE_ADMIN=/tmp/transfer-admin.cookie
COOKIE_OWNER=/tmp/transfer-owner.cookie
COOKIE_NEXT=/tmp/transfer-next.cookie
rm -f "$COOKIE_ADMIN" "$COOKIE_OWNER" "$COOKIE_NEXT"

admin=$(curl -s -c "$COOKIE_ADMIN" -X POST "$URL/api/v1/onboarding/owner" \
  -H 'content-type: application/json' \
  -d '{"display_name":"QA Owner","email":"owner@qa.test","password":"qa-password-123"}')
if ! echo "$admin" | grep -q '"user"'; then
  echo "  FAIL the instance owner could not be created: $admin"
  exit 1
fi
pass "created the instance owner through the real sign-up flow"

for spec in "lead@qa.test|Project Lead" "successor@qa.test|Successor"; do
  email="${spec%%|*}"; name="${spec#*|}"
  created=$(curl -s -X POST "$URL/api/v1/iam/users" -b "$COOKIE_ADMIN" \
    -H 'content-type: application/json' \
    -d "{\"email\":\"$email\",\"display_name\":\"$name\",\"password\":\"qa-password-123\"}")
  case "$email" in
    lead@qa.test) OWNER_ID=$(echo "$created" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p') ;;
    successor@qa.test) NEXT_ID=$(echo "$created" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p') ;;
  esac
done
if [ -z "${OWNER_ID:-}" ] || [ -z "${NEXT_ID:-}" ]; then
  echo "  FAIL the two accounts could not be created."
  exit 1
fi

# The first owner is *platform-level* (`organization_id is null`), so both accounts have none
# either. Delegated administration is an organization account managing a project inside its own
# organization, so one is created here -- as a ROW, not as an invented uuid: `gen_random_uuid()`
# as the organization id fails the foreign key (`users_organization_id_fkey`), which is the
# database correctly refusing an organization that does not exist.
DBQ "insert into organizations (id, name, slug, created_at, updated_at)
     values ('$(DBQ "select gen_random_uuid()")', 'QA Organization', 'qa-organization', now(), now())" >/dev/null
ORG_ID=$(DBQ "select id from organizations where slug = 'qa-organization' limit 1")
if [ -z "$ORG_ID" ]; then
  echo "  FAIL: the QA organization could not be created." >&2
  exit 1
fi
DBQ "update users set organization_id = '$ORG_ID' where id = '$OWNER_ID'" >/dev/null
DBQ "update users set organization_id = '$ORG_ID' where id = '$NEXT_ID'" >/dev/null

for spec in "lead@qa.test|$COOKIE_OWNER" "successor@qa.test|$COOKIE_NEXT"; do
  email="${spec%%|*}"; jar="${spec#*|}"
  curl -s -c "$jar" -X POST "$URL/api/v1/auth/login" \
    -H 'content-type: application/json' \
    -d "{\"email\":\"$email\",\"password\":\"qa-password-123\"}" >/dev/null
  if [ ! -s "$jar" ]; then
    echo "  FAIL $email could not sign in -- every claim below would run unauthenticated."
    exit 1
  fi
done
pass "both accounts signed in with their own sessions"

# **The load-bearing fixture step, and the one the first run of this gate got wrong.**
# `POST /iam/users` creates an account with the *default* base role, which on this platform is
# `member` — and `member` carries no `projects.*` key. Both accounts therefore answered
# `403 permission_denied` on every transfer, including the confirmed one, and the gate reported
# seven failures that all had this single cause.
#
# Worth stating plainly, because it is the shape this branch keeps meeting in its nastiest form:
# **the guard's refusal is not the validation's refusal.** Both are `403` and `400` respectively,
# so a gate asserting "refused" without reading the body would have called the confirmation rule
# broken when it was never reached. `transfer()` requires `400` *and* the specific code, which is
# why the failure named `projects.manage` instead of a missing confirmation.
MEMBER_ROLE_ID=$(DBQ "select id from roles where key = 'member' limit 1")
if [ -z "$MEMBER_ROLE_ID" ]; then
  echo "  FAIL: the base 'member' role is missing — the permissions seed did not run."
  exit 1
fi
for key in projects.read projects.manage projects.members.manage projects.limits.manage \
           projects.audit.read; do
  if [ "$(DBQ "select count(*) from permissions where key = '$key'")" != "1" ]; then
    echo "  FAIL: permission '$key' is not in the catalogue." >&2
    exit 1
  fi
  DBQ "insert into role_permissions (role_id, permission_key, effect)
       values ('$MEMBER_ROLE_ID', '$key', 'allow')
       on conflict (role_id, permission_key) do update set effect = 'allow'" >/dev/null
done
# `role_bindings` carries BOTH `user_id` and the generic `subject_id`/`subject_type` pair, and the
# authorization path reads the generic one — filling only the legacy column is a binding that
# exists in the table and is invisible to the guard.
for uid in "$OWNER_ID" "$NEXT_ID"; do
  DBQ "insert into role_bindings (role_id, user_id, subject_id, subject_type, scope_type, organization_id)
       values ('$MEMBER_ROLE_ID', '$uid', '$uid', 'user', 'organization', '$ORG_ID')
       on conflict do nothing" >/dev/null
done
pass "granted the project keys to both accounts, and nothing else"

# And the negative: neither may hold `projects.admin`, the one key that overrides membership. An
# account holding it would answer 200 where the gate expects the store's own visibility rules to
# decide, turning the isolation assertions into a green lie.
for uid in "$OWNER_ID" "$NEXT_ID"; do
  if [ "$(DBQ "select count(*) from role_permissions rp
                join role_bindings rb on rb.role_id = rp.role_id
                where rb.subject_id = '$uid' and rp.permission_key = 'projects.admin'")" != "0" ]; then
    echo "  FAIL: account $uid holds projects.admin — the delegated half cannot be measured." >&2
    exit 1
  fi
done
pass "neither account holds instance-wide project power"

# ---------------------------------------------------------------------------------------------
# 2. The project, owned by `lead`.
# ---------------------------------------------------------------------------------------------
echo "[transfer] a project owned by the caller"

mine=$(curl -s -X POST "$URL/api/v1/projects" -b "$COOKIE_OWNER" \
  -H 'content-type: application/json' \
  -d "{\"key\":\"LEAD\",\"name\":\"Lead Team\",\"owner_user_id\":\"$OWNER_ID\",\"organization_id\":\"$ORG_ID\"}")
PROJECT_ID=$(echo "$mine" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
if [ -z "$PROJECT_ID" ]; then
  echo "  FAIL the project could not be created: $mine"
  exit 1
fi
pass "created the project with the caller as its owner"

# The owner column and the membership row are TWO representations, and `transfer_ownership`
# moves both. A project can carry an owner nobody is recorded as owning (`upsert_member` writes
# only the membership), so a gate that reads one of them proves half the sentence.
owner_col=$(DBQ "select owner_user_id from automation_projects where id = '$PROJECT_ID'")
owner_row=$(DBQ "select user_id from automation_project_members
                 where project_id = '$PROJECT_ID' and role = 'owner'")
if [ "$owner_col" = "$OWNER_ID" ] && [ "$owner_row" = "$OWNER_ID" ]; then
  pass "both owner representations name the caller (the column AND the membership row)"
else
  fail "owner representations disagree: column='$owner_col' row='$owner_row' expected='$OWNER_ID'"
fi

# The successor must be a member with a role that CAN hold the project afterwards, or the gate
# would be proving that ownership can be handed to somebody who cannot administer it. `editor`
# keeps `can_run` and `can_edit`, which is what a transfer to an ordinary member means.
DBQ "insert into automation_project_members (project_id, user_id, role, created_at)
     values ('$PROJECT_ID', '$NEXT_ID', 'editor', now())
     on conflict do nothing" >/dev/null

# ---------------------------------------------------------------------------------------------
# 3. The three confirmation refusals.
# ---------------------------------------------------------------------------------------------
echo "[transfer] each confirmation is refused on its own"

transfer() { # label payload expect
  local label="$1" payload="$2" expect="$3" code
  code=$(curl -s -o /tmp/transfer-out.json -w '%{http_code}' --max-time 8 \
    -X POST "$URL/api/v1/projects/$PROJECT_ID/transfer-ownership" \
    -b "$COOKIE_OWNER" -H 'content-type: application/json' -d "$payload")
  if [ "$code" = "400" ] && grep -q 'ownership_transfer_unconfirmed' /tmp/transfer-out.json; then
    pass "$label is refused with ownership_transfer_unconfirmed"
  elif [ "$code" = "$expect" ] && [ "$expect" != "400" ]; then
    pass "$label is refused"
  else
    fail "$label answered $code (expected $expect): $(head -c 240 /tmp/transfer-out.json)"
  fi
}

# **One flag.** Both arms are needed: a guard that only checked `confirm_owner` would refuse
# these two and let the one below through.
transfer "transferring with no confirmation" \
  "{\"to_user_id\":\"$NEXT_ID\",\"organization_id\":\"$ORG_ID\"}" 400
transfer "transferring with only the demotion confirmation" \
  "{\"to_user_id\":\"$NEXT_ID\",\"organization_id\":\"$ORG_ID\",\"confirm_owner\":true}" 400
transfer "transferring with only the audit confirmation" \
  "{\"to_user_id\":\"$NEXT_ID\",\"organization_id\":\"$ORG_ID\",\"confirm_audit\":true}" 400

# ---------------------------------------------------------------------------------------------
# 4. The refusals must not have moved the project.
# ---------------------------------------------------------------------------------------------
echo "[transfer] a refused transfer leaves both owner representations untouched"

col_after=$(DBQ "select owner_user_id from automation_projects where id = '$PROJECT_ID'")
row_after=$(DBQ "select user_id from automation_project_members
                 where project_id = '$PROJECT_ID' and role = 'owner'")
if [ "$col_after" = "$OWNER_ID" ] && [ "$row_after" = "$OWNER_ID" ]; then
  pass "the owner column and the owner membership row are unchanged after three refusals"
else
  fail "a refused transfer moved ownership: column='$col_after' row='$row_after'"
fi

# And no audit row was written for a transfer that did not happen. An event on the bus for a write
# that did not occur is a receiver acting on nothing -- the same reason `DELETE /scim/v2/Users/{id}`
# is checked before `user.deleted` is emitted.
events=$(DBQ "select count(*) from audit_log
             where action = 'project_ownership.transferred' and project_id = '$PROJECT_ID'")
if [ "$events" = "0" ]; then
  pass "no audit row claims a transfer that never happened"
else
  fail "a refused transfer wrote $events audit rows naming project_ownership.transferred"
fi

# ---------------------------------------------------------------------------------------------
# 5. The confirmed transfer, and what it must actually move.
# ---------------------------------------------------------------------------------------------
echo "[transfer] both confirmations together do transfer, and move both facts"

done_body=$(curl -s -o /tmp/transfer-ok.json -w '%{http_code}' --max-time 8 \
  -X POST "$URL/api/v1/projects/$PROJECT_ID/transfer-ownership" \
  -b "$COOKIE_OWNER" -H 'content-type: application/json' \
  -d "{\"to_user_id\":\"$NEXT_ID\",\"organization_id\":\"$ORG_ID\",\"confirm_owner\":true,\"confirm_audit\":true}")
if [ "$done_body" = "200" ]; then
  pass "the confirmed transfer succeeded"
else
  fail "the confirmed transfer answered $done_body: $(head -c 240 /tmp/transfer-ok.json)"
fi

col_new=$(DBQ "select owner_user_id from automation_projects where id = '$PROJECT_ID'")
row_new=$(DBQ "select user_id from automation_project_members
                where project_id = '$PROJECT_ID' and role = 'owner'")
if [ "$col_new" = "$NEXT_ID" ] && [ "$row_new" = "$NEXT_ID" ]; then
  pass "both owner representations moved to the successor -- neither was left behind"
else
  fail "the transfer moved only one representation: column='$col_new' row='$row_new' expected='$NEXT_ID'"
fi

# The previous owner is DEMOTED, not removed: the dialog says so, and a transfer that deleted the
# membership would silently strip somebody's access. The body answers the previous owner id, so
# the panel can name them rather than render an empty field.
prev_col=$(DBQ "select role from automation_project_members
                where project_id = '$PROJECT_ID' and user_id = '$OWNER_ID'")
if [ "$prev_col" = "editor" ]; then
  pass "the previous owner was demoted to editor, keeping access"
else
  fail "the previous owner's membership role is '$prev_col' (expected 'editor', or no row at all)"
fi

if grep -q "\"previous_owner_user_id\":\"$OWNER_ID\"" /tmp/transfer-ok.json; then
  pass "the response names the previous owner, so the dialog can say who was demoted"
else
  fail "the response does not name the previous owner: $(head -c 240 /tmp/transfer-ok.json)"
fi

# And the audit row is the third fact the sentence promises.
audited=$(DBQ "select count(*) from audit_log
                where action = 'project_ownership.transferred'
                  and project_id = '$PROJECT_ID'")
if [ "$audited" = "1" ]; then
  pass "exactly one project_ownership.transferred audit row names the project"
else
  fail "expected exactly 1 transfer audit row, found $audited"
fi

# The successor can now do the thing ownership is FOR: administer the project. Without this the
# gate would pass on a handover that left the new owner unable to act -- the mirror of the
# "removing the last owner is refused" rule, in the other direction.
as_next=$(curl -s -o /dev/null -w '%{http_code}' "$URL/api/v1/projects/$PROJECT_ID" -b "$COOKIE_NEXT")
if [ "$as_next" = "200" ]; then
  pass "the successor reads the project after the handover"
else
  fail "the successor answered $as_next on the project it now owns"
fi

echo
if [ "$FAILED" != "0" ]; then
  echo "[transfer] FAILED -- read the first FAIL above; the rest are echoes of the same miss." >&2
  exit 1
fi
echo "[transfer] passed"