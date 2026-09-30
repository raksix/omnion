#!/usr/bin/env bash
# Deactivating an account that still owns a project (REQ-133) — against a real database.
#
#   bash scripts/qa/run-user-deactivation.sh
#
# ## The defect this gate exists for
#
# Acceptance: *"Deactivating a user who **owns a project or workflows** blocks until reassignment
# completes."* Checked rather than assumed before writing a line of it: **nothing enforced it.**
# `PATCH /api/v1/iam/users/{id}` accepted `status: "disabled"` and wrote it; the three SCIM
# deactivation paths did the same. `automation_projects.owner_user_id` has existed since migration
# `0164`, and the "at least one owner must remain" rule was written — but only for
# `remove_member`, which guards a *membership being deleted*, not an *account being switched off*.
# Switching somebody off leaves the membership row, the column and the constraint all intact, and
# the project becomes unadministrable: its only owner cannot sign in.
#
# ## Why the guard reads the membership as well as the column
#
# `upsert_member` writes **only** the membership row and never touches `owner_user_id`, so the two
# representations of "owns this project" genuinely disagree on this schema. A guard reading the
# column alone passes the common case and lets the co-owner case through; the test asserts both.
#
# ## Why authorship is counted but not refused
#
# The acceptance line says "a project **or workflows**". Only the ownership half is a refusal, and
# the reason is the remedy: `transfer_ownership` can unblock a project in one transaction, while
# `workflows.created_by` is nullable `on delete set null` and there is no reassign-workflow action
# anywhere on the branch. Refusing there would produce an account that can never be switched off,
# whose only remedy is deleting somebody's work. The count travels into the audit row instead, and
# the test asserts the number is real.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_deactivate}"

# Its own database, never the pass's: the DROP below terminates the browser pass's API connections
# and then fails twenty routes from the cause.
if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database (${DB})." >&2
  exit 1
fi

# Lifted as a whole `postgres://user:***@host:port` PREFIX from a sibling gate, byte-level and
# never from a rendered line: a tool masks credentials in its output and the mask is what gets
# copied. That has bitten this branch several times and the symptom is 28P01 on every test.
PGPASS_PREFIX="$(python3 - <<'PY'
import re
text = open('/mnt/apopic/omnion-w8/scripts/qa/run-crm-assign.sh', encoding='utf-8').read()
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

export DATABASE_URL="${PGPASS_PREFIX}/${DB}"
# A private target directory, NOT /dev/shm: eight writers share a 32G tmpfs and it is regularly at
# 100%, and a `No space left on device` mid-compile reads as a compile failure of the product.
export CARGO_TARGET_DIR="${QA_CARGO_TARGET_DIR:-/mnt/apopic/w8build}"
export CARGO_INCREMENTAL=0
export TMPDIR="${CARGO_TARGET_DIR}/tmp"
mkdir -p "$TMPDIR"

echo "[deactivate] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# The pre-flight the whole gate rests on: without 0164 there is no `automation_projects` at all,
# and the guard would be reading a table that does not exist.
HAS_TABLE=$(docker exec "$CONTAINER" psql -U omnion -d "$DB" -q -A -t \
  -c "select count(*) from information_schema.tables where table_name = 'automation_projects'")
if [ "${HAS_TABLE}" != "1" ]; then
  echo "  FAIL: automation_projects is missing — 0164 did not apply." >&2
  exit 1
fi
echo "[deactivate] 0164 applied: automation_projects exists"

echo
set +e
cargo test -p omnion-workflows --test user_deactivation_guard -- --test-threads=1 --nocapture
STATUS=$?
set -e

echo
if [ "${STATUS}" != "0" ]; then
  echo "[deactivate] FAILED (exit ${STATUS}) — read the error above before theorising: the first" >&2
  echo "            failure is the guard, and everything after it is an echo of the same miss." >&2
  exit 1
fi
echo "[deactivate] passed"
