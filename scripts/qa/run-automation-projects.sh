#!/usr/bin/env bash
# Automation projects (REQ-133 slice 1) — the migration ORDER, against a database that has rows.
#
#   QA_DB=omnion_qa_w8_projects bash scripts/qa/run-automation-projects.sh
#
# ## What this gate is actually for
#
# Acceptance criterion 1 is: "A fresh installation and an upgraded installation both end with
# exactly one default project per organization and zero unassigned resources". The second half of
# that sentence is the only part that can fail, and it is invisible to every other check in the
# build: on an empty database the backfill has nothing to do, `not null` is satisfiable trivially,
# and the whole migration is silent-OK. **The empty case is the one that cannot fail**, so a gate
# that runs the migration on a fresh database proves nothing about the sentence it is quoting.
#
# So this gate does something the fresh-install run cannot: it builds a POPULATED database
# first — organizations, users and workflows that already exist — and only then applies
# 0164. Then it asserts the two things acceptance 1 promises:
#
#   1. every organization ends with exactly one default project (not zero, not two);
#   2. every pre-existing workflow has a project_id, and it is that organization's default.
#
# ## The order is the whole design
#
# The migration's header states it, and this gate is why the statement is not decoration: the
# `not null` on `workflows.project_id` is applied in step 4 of 5, after the backfill in step 3.
# Move it before the update and the migration is refused outright on any populated database;
# move it after and a row inserted between the two steps can never be backfilled. Both failure
# modes are invisible on a fresh install, which is why the fixture below exists.
#
# Its own database, never the pass's: this gate drops and recreates, which terminates the
# browser pass's API connections and then fails twenty routes from the cause.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_projects}"

if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database (${DB})." >&2
  echo "        Give the gate its own: the other gates use omnion_qa_w8_*." >&2
  exit 1
fi

# Lifted as a whole `postgres://user:***@host:port` PREFIX from a sibling gate rather than
# retyped, byte-level and never from a rendered line: a tool masks credentials in output and
# the mask is what gets copied. That is how a first draft of a sibling gate ended up with a
# regex full of asterisks and an "unbalanced parenthesis".
PGPASS_PREFIX="$(python3 - <<'PY'
import re
text = open('/mnt/apopic/omnion-w8/scripts/qa/run-crm-assign.sh', encoding='utf-8').read()
m = re.search(r'DATABASE_URL="(postgres://[^@"\"]+@127\.0\.0\.1:5433)/', text)
print(m.group(1) if m else '')
PY
)"
if [ -z "${PGPASS_PREFIX}" ]; then
  echo "  FAIL: could not read the QA database prefix out of run-crm-assign.sh." >&2
  exit 1
fi

psql_q() { docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -A -t -v ON_ERROR_STOP=1 -c "$1"; }

echo "[projects] building a POPULATED database at every migration before 0164"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null

# Everything up to and including 0163, in order. The fixture rows go in AFTER 0006 (workflows) so
# the backfill has something real to move, and BEFORE 0164 so the migration has to do the work.
BEFORE_0164=()
while IFS= read -r f; do
  case "$(basename "$f")" in
    0164_*) break ;;
  esac
  BEFORE_0164+=("$f")
done < <(ls database/migrations/*.sql | sort)

for f in "${BEFORE_0164[@]}"; do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# ── the fixture: an organization and a user that existed before projects did ────────────────
#
# Two organizations on purpose. The acceptance sentence is "exactly one default project per
# organization", and a single-organization fixture cannot tell "one per organization" from
# "one, globally" — which is the difference between the rule working and a constraint that
# happens to be satisfied.
#
# Column names are 0001's (`display_name`, not `name` on users), and `ON_ERROR_STOP=1` means a
# typo in a fixture aborts the gate loudly rather than inserting nothing and reporting a true
# sentence about the wrong world — so the counts are asserted immediately afterwards regardless.
psql_q "
  insert into organizations (id, name, slug, created_at, updated_at)
  values ('11111111-1111-1111-1111-111111111111', 'Acme', 'acme', now(), now()),
         ('22222222-2222-2222-2222-222222222222', 'Globex', 'globex', now(), now());
" >/dev/null

psql_q "
  insert into users (id, organization_id, email, display_name, password_hash, created_at, updated_at)
  values ('33333333-3333-3333-3333-333333333333', '11111111-1111-1111-1111-111111111111',
          'owner@acme.test', 'Acme Owner', 'x', now(), now());
" >/dev/null

# Two workflows in Acme, one in Globex, all with the columns 0006 defines. `steps` is a jsonb
# array (0006 checks jsonb_typeof), and the schedule columns are shaped by a constraint: a
# manual workflow has a null schedule, so the insert would be refused otherwise.
psql_q "
  insert into workflows (organization_id, name, trigger_kind, steps, created_by, created_at, updated_at)
  values
    ('11111111-1111-1111-1111-111111111111', 'Acme one',   'manual', '[]'::jsonb, '33333333-3333-3333-3333-333333333333', now(), now()),
    ('11111111-1111-1111-1111-111111111111', 'Acme two',   'manual', '[]'::jsonb, '33333333-3333-3333-3333-333333333333', now(), now()),
    ('22222222-2222-2222-2222-222222222222', 'Globex one', 'manual', '[]'::jsonb, null, now(), now());
" >/dev/null

PRE=$(psql_q "select count(*) from workflows")
if [ "${PRE}" != "3" ]; then
  echo "  FAIL: expected 3 pre-existing workflows, found ${PRE}." >&2
  echo "        A gate whose fixture is empty reports a true sentence about the wrong world." >&2
  exit 1
fi
echo "[projects] fixture: 2 organizations, 1 user, ${PRE} workflows that predate projects"

# ── now, and only now, 0164 ────────────────────────────────────────────────────────────────
echo "[projects] applying 0164_automation_projects.sql to a populated database"
docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 \
  <database/migrations/0164_automation_projects.sql >/dev/null

PASS=0
FAIL=0
check() { # check <label> <expected> <actual>
  if [ "$2" = "$3" ]; then
    printf '  ok   %-58s %s\n' "$1" "$3"
    PASS=$((PASS + 1))
  else
    printf '  FAIL %-58s expected %s, got %s\n' "$1" "$2" "$3"
    FAIL=$((FAIL + 1))
  fi
}

# ── 1 · one default project per organization, and no organization with two ─────────────────
check "default projects total == organizations" \
  "2" "$(psql_q "select count(*) from automation_projects where is_default")"
check "organizations with zero defaults" \
  "0" "$(psql_q "select count(*) from organizations o
                 where not exists (select 1 from automation_projects p
                                   where p.organization_id = o.id and p.is_default)")"
check "organizations with more than one default" \
  "0" "$(psql_q "select count(*) from (select organization_id from automation_projects
                 where is_default group by organization_id having count(*) > 1) d")"

# ── 2 · zero unassigned resources — the integrity counter the REQ asks to be visible ────────
check "workflows with a null project_id" \
  "0" "$(psql_q "select count(*) from workflows where project_id is null")"
check "workflows in a project of another organization" \
  "0" "$(psql_q "select count(*) from workflows w
                 join automation_projects p on p.id = w.project_id
                 where p.organization_id <> w.organization_id")"
check "workflows whose project is not their org's default" \
  "0" "$(psql_q "select count(*) from workflows w
                 join automation_projects p on p.id = w.project_id
                 where not p.is_default")"

# ── 3 · the constraints the migration states, exercised ─────────────────────────────────────
# Each of these is a refusal the REQ's risks section depends on. A gate that only checks the
# happy path leaves them unproven, and they are the half an operator hits by accident.
check "a default project is created for an org that already has one" \
  "1" "$(psql_q "select count(*) from automation_projects
                 where organization_id = '11111111-1111-1111-1111-111111111111' and is_default")"

# `not null` is in force. `insert ... select null` proves the constraint rather than the
# comment above it, and the whole point of the ORDER is that this holds for every row.
NOTNULL=$(psql_q "
  insert into workflows (organization_id, name, trigger_kind, steps, created_at, updated_at)
  values ('11111111-1111-1111-1111-111111111111', 'unassigned', 'manual', '[]'::jsonb, now(), now())
  returning project_id;" 2>&1 || true)
case "${NOTNULL}" in
  *"null value in column \"project_id\""*) printf '  ok   %-58s %s\n' "project_id is NOT NULL" "refused"; PASS=$((PASS + 1)) ;;
  *) printf '  FAIL %-58s the insert was not refused: %s\n' "project_id is NOT NULL" "${NOTNULL}"; FAIL=$((FAIL + 1)) ;;
esac

# The key format is a constraint, and the store states the same rule in Rust. Comparing them
# here is what keeps the two from drifting — the REQ's own note says the rule is stated twice.
KEYBAD=$(psql_q "insert into automation_projects (organization_id, key, name)
  values ('11111111-1111-1111-1111-111111111111', 'lower', 'bad key');" 2>&1 || true)
case "${KEYBAD}" in
  *automation_projects_key_format*) printf '  ok   %-58s %s\n' "a lower-case key is refused by the constraint" "refused"; PASS=$((PASS + 1)) ;;
  *) printf '  FAIL %-58s the insert was not refused: %s\n' "a lower-case key is refused" "${KEYBAD}"; FAIL=$((FAIL + 1)) ;;
esac

# Two defaults in one organization is what a duplicate default_project() call would produce.
DUPDEF=$(psql_q "insert into automation_projects (organization_id, key, name, is_default)
  values ('11111111-1111-1111-1111-111111111111', 'SECOND', 'Second default', true);" 2>&1 || true)
case "${DUPDEF}" in
  *automation_projects_one_default_uidx*) printf '  ok   %-58s %s\n' "a second default is refused by the index" "refused"; PASS=$((PASS + 1)) ;;
  *) printf '  FAIL %-58s the insert was not refused: %s\n' "a second default is refused" "${DUPDEF}"; FAIL=$((FAIL + 1)) ;;
esac

# The default project may not be archived: archiving it would refuse an insert on every path
# that does not pass a project.
ARCHDEF=$(psql_q "update automation_projects set status = 'archived'
  where organization_id = '11111111-1111-1111-1111-111111111111' and is_default;" 2>&1 || true)
case "${ARCHDEF}" in
  *automation_projects_default_is_active*) printf '  ok   %-58s %s\n' "the default project refuses to archive" "refused"; PASS=$((PASS + 1)) ;;
  *) printf '  FAIL %-58s the update was not refused: %s\n' "the default project refuses to archive" "${ARCHDEF}"; FAIL=$((FAIL + 1)) ;;
esac

# A project with a workflow cannot vanish: `on delete restrict`, not cascade. This is the
# difference between "delete a project" and "delete twenty workflows without an audit row".
DELDEP=$(psql_q "delete from automation_projects
  where organization_id = '11111111-1111-1111-1111-111111111111' and is_default;" 2>&1 || true)
case "${DELDEP}" in
  *"violates foreign key constraint"*) printf '  ok   %-58s %s\n' "a project with dependencies refuses to delete" "refused"; PASS=$((PASS + 1)) ;;
  *) printf '  FAIL %-58s the delete was NOT refused: %s\n' "a project with dependencies refuses to delete" "${DELDEP}"; FAIL=$((FAIL + 1)) ;;
esac

echo
echo "[projects] ${PASS} passed, ${FAIL} failed"
if [ "${FAIL}" != "0" ]; then
  exit 1
fi
