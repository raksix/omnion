#!/usr/bin/env bash
# Project role freshness (REQ-133, acceptance 6) — against a real database.
#
#   bash scripts/qa/run-project-role-freshness.sh
#
# ## The defect this gate exists for
#
# Acceptance 6: *"Project member role changes take effect without re-login: the very next request
# reflects the new role, **in both directions (grant and revoke)**."* The REQ's own risk note names
# the mechanism it expected: "the cache key includes a membership revision, and a test proves grant
# and revoke both bite on the next request."
#
# Checked on this branch before writing a line of it: **there was nothing to invalidate.**
# `grep -rn "cache" crates/permissions/src crates/workflows/src` returns nothing that memoizes a
# role, `crates/permissions/src/groups.rs` says it outright — "resolution never caches" — and
# `role_of` is a single indexed primary-key lookup. So the REQ's named mechanism is *unnecessary*
# here, which is a claim about the code and not about the feature: **`role_of` existed and
# `role_of` was never called by any write path.** `PUT /workflows/{id}`, `DELETE /workflows/{id}`,
# `POST /workflows/{id}/run`, `POST /workflows` and the two `POST/PUT/DELETE /automations` routes
# all reached the store through `workflow_in_scope` / `automation_in_scope` / `resolve_target`,
# each of which answers *visibility* ("may this caller see the row") — a question every member of a
# project answers yes to. The matrix was correct, unit-tested, rendered on the members screen, and
# **unreachable from any write**.
#
# So the tenth instance of this branch's signature defect had its purest form yet: not a function
# that computed the right answer, but a function that computed the right answer *and was right about
# caching*, with no caller to be stale.
#
# ## What it pins
#
# 1. **Grant bites.** One session, one project, one membership write between two assertions: the
#    first request is refused, the second succeeds — same token, no re-login.
# 2. **Revoke bites.** The mirror image, because a guard that reads the role once per session bites
#    on the grant and stays silent on the revoke, and that is the common shape.
# 3. **No re-login is what makes 1 and 2 true.** The assertions run on ONE session cookie created
#    before the role changed, so a suite that silently re-authenticates cannot pass this gate.
# 4. The refusal **names the role** — a `403` that says only "forbidden" is what an operator reads
#    as a bug, and acceptance 6 is about the role being legible as much as enforced.
# 5. A `viewer` cannot start a run (`can_run`), which is the one verb where the fourth role earns
#    its keep: `operator` may run, `viewer` may not, and a route that asks only "can you see it"
#    cannot tell those apart.
# 6. A non-member of a **non-default** project is refused (`None` may do nothing) — the default
#    project is deliberately visible to everyone, so a suite that only tests the default proves
#    nothing about the rule.
# 7. The migration's `default_member_role` column is honoured: a project whose default role is
#    `operator` lets a non-member run, which is the only way to tell "the column is read" from
#    "the column is decoration".
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_rolefresh}"

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

echo "[rolefresh] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# The pre-flight the whole gate rests on: without 0181 there is no `default_member_role`, and
# `effective_role` would have nothing to fall back to for the accounts that are not members.
HAS_COLUMN=$(docker exec "$CONTAINER" psql -U omnion -d "$DB" -q -A -t \
  -c "select count(*) from information_schema.columns
       where table_name = 'automation_projects' and column_name = 'default_member_role'")
if [ "${HAS_COLUMN}" != "1" ]; then
  echo "  FAIL: automation_projects.default_member_role is missing — 0181 did not apply." >&2
  exit 1
fi
echo "[rolefresh] 0181 applied: automation_projects.default_member_role exists"

echo
set +e
cargo test -p omnion-workflows --test project_role_freshness -- --test-threads=1 --nocapture
STATUS=$?
set -e

echo
if [ "${STATUS}" != "0" ]; then
  echo "[rolefresh] FAILED (exit ${STATUS}) — read the error above before theorising: the first" >&2
  echo "           failure is the guard, and everything after it is an echo of the same miss." >&2
  exit 1
fi
echo "[rolefresh] passed"
