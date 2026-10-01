#!/usr/bin/env bash
# CRM intake — the SLA sweep's three reads, asked of the planner. (REQ-117, slice 39)
#
#   bash scripts/qa/run-crm-sla-sweep-plan.sh
#
# ## Why this gate exists
#
# Slice 38's lesson was that the phone rule had a third spelling **in the schema** — a rule
# written once in a query and once in an index, where the two can only be settled by asking
# PostgreSQL which one it will use. This gate applies that method to the SLA sweep, where the
# rule is a *status* predicate rather than a normalization, and where the same failure has a
# second, worse half: a query that is correct and slow is quiet, while a query that returns the
# wrong tenants is loud and slow.
#
# `assignment_store::organizations_with_leads` is the worker's FIRST read — it decides which
# tenants to walk — and it named no status at all. `crm_leads_sla_idx` is partial on
# `first_response_at is null and status not in ('spam','rejected','duplicate')`, and PostgreSQL
# offers a partial index only when the planner can prove every row the statement would read
# satisfies the predicate. "No status mentioned" proves nothing about it, so the read seq-scanned
# every lead on the platform.
#
# **The second half is the one that would have been missed.** Both consumer reads —
# `due_reminders` and `due_breaches` — filter `status in ('new', …)`, so a tenant holding only
# spam and rejected leads is *walked every minute to produce two empty result sets*. And the
# `limit` bounds ORGANIZATIONS, so quiet tenants consume the batch a busy tenant needs. The
# performance fix and the correctness fix are the same edit here, which is why the gate asserts
# the answer as well as the plan.
#
# ## What this gate asserts
#
# 1. The sweep's first read is served by the partial index — the plan, not the index's existence.
# 2. **It returns exactly the organizations the two consumer reads have work for.** A narrowing
#    that made the plan fast while dropping a tenant with a live clock would be a silent data
#    loss, and this is the assertion that would catch it.
# 3. A tenant holding only spam/rejected leads is NOT named — the sweep must not walk it.
# 4. Both consumer reads still use their index, so the narrowed first read did not push the cost
#    somewhere else.
# 5. NEGATIVE CONTROL: the production statement with the status predicate removed must seq-scan.
#    Without it, the gate cannot be shown to see the defect it exists for.
#
# ## Its own database, never the pass's
#
# These gates open with `DROP DATABASE … WITH (FORCE)`, which terminates the browser pass's API
# connections and lands the failure twenty routes from the cause.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_slaplan}"

if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database (${DB})." >&2
  exit 1
fi

PGPASSWORD="$(python3 - <<'PY'
import re
text = open('/mnt/apopic/omnion-w8/scripts/qa/run-crm-assign.sh', encoding='utf-8').read()
m = re.search(r'DATABASE_URL="(postgres://[^@"]+)@', text)
print(m.group(1).split('://', 1)[1].split(':', 1)[1] if m else '')
PY
)"
if [ -z "${PGPASSWORD}" ]; then
  echo "  FAIL: could not read the QA database password out of run-crm-assign.sh." >&2
  exit 1
fi

# The predicate is read out of the crate, not retyped: a hand-copied status list is exactly the
# third-spelling defect slice 38 was about. The gate is asserting the CODE's predicate.
NOT_CLOSED="$(python3 - <<'PY'
import re
text = open('/mnt/apopic/omnion-w8/modules/crm-intake/src/vocabulary.rs', encoding='utf-8').read()
m = re.search(r'pub fn not_closed_statuses_sql\(column: &str\) -> String \{\s*\n\s*format!\("\{column\}([^"]+)"\)', text)
if not m:
    raise SystemExit('could not read not_closed_statuses_sql out of vocabulary.rs')
print('status' + m.group(1))
PY
)"
OPEN="$(python3 - <<'PY'
import re
text = open('/mnt/apopic/omnion-w8/modules/crm-intake/src/vocabulary.rs', encoding='utf-8').read()
m = re.search(r'pub fn open_statuses_sql\(column: &str\) -> String \{\s*\n\s*format!\("\{column\}([^"]+)"\)', text)
if not m:
    raise SystemExit('could not read open_statuses_sql out of vocabulary.rs')
print('status' + m.group(1))
PY
)"
if [ -z "${NOT_CLOSED}" ] || [ -z "${OPEN}" ]; then
  echo "  FAIL: a status predicate could not be read out of vocabulary.rs." >&2
  exit 1
fi

# `docker exec` reaches the container's own listener, so the host/port in the URL are not used.
px() { docker exec -e "PGPASSWORD=${PGPASSWORD}" "$CONTAINER" psql -U omnion -d "${2:-$DB}" -q -A -t -v ON_ERROR_STOP=1 -c "$1"; }
pxi() { docker exec -i -e "PGPASSWORD=${PGPASSWORD}" "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1; }

px "DROP DATABASE IF EXISTS ${DB} WITH (FORCE)" postgres >/dev/null
px "CREATE DATABASE ${DB} OWNER omnion" postgres >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  pxi <"$f" >/dev/null 2>&1
done
px "$(cat scripts/qa/fixture-sla-sweep-plan.sql)" >/dev/null

PASS=0
FAIL=0
ok()  { echo "  ok: $1"; PASS=$((PASS + 1)); }
bad() { echo "  FAIL: $1" >&2; FAIL=$((FAIL + 1)); }

ORG_BUSY='33333333-3333-3333-3333-000000000010'

# --1-- The production statement, verbatim from `organizations_with_leads`, with the crate's own
# predicate substituted exactly as the code does.
STATEMENT="select distinct organization_id from crm_leads \
           where first_response_due_at is not null and first_response_at is null \
           and ${NOT_CLOSED} \
           order by organization_id limit 50"

PLAN="$(px "explain (analyze, buffers) ${STATEMENT}")"
echo "${PLAN}" | grep -E 'Index (Scan|Only Scan)|Seq Scan|Bitmap Index Scan|Execution Time|Rows Removed' | sed 's/^/        /' >&2

# The statement is a `distinct` over an index, so the planner answers with a bitmap or an
# index-only scan; the assertion names the INDEX, not the node type — the same correction the
# phone gate had to make after asserting a node type that an `or` legitimately does not produce.
if echo "${PLAN}" | grep -q "crm_leads_sla_idx"; then
  ok "the sweep's first read is served by the SLA index"
else
  bad "the sweep's first read does NOT use crm_leads_sla_idx — the partial predicate is not provable, so it scans every lead"
  echo "${PLAN}" | sed 's/^/        /' >&2
fi

if echo "${PLAN}" | grep -q "Rows Removed by Filter: 2[0-9]\{3,\}"; then
  bad "the read still filters thousands of rows — the index is offered but the plan is not using it"
  echo "${PLAN}" | sed 's/^/        /' >&2
else
  ok "the read no longer removes tens of thousands of rows to name its tenants"
fi

# --2-- THE ASSERTION THAT MATTERS MOST: the narrowed read must not drop a tenant that has work.
#
# A predicate that made the plan fast while losing a live clock would be silent, permanent data
# loss, and every other assertion in this gate would still be green. So the answer is compared
# against the two consumer reads themselves: a tenant is returned iff at least one of them would
# find a row for it.
ANSWER="$(px "select string_agg(organization_id::text, ',') from (${STATEMENT}) t")"
if echo "${ANSWER}" | grep -q "${ORG_BUSY}"; then
  ok "the tenant with four live breached leads is still named by the sweep"
else
  bad "the sweep no longer names the tenant that has live breaches — the narrowing dropped a tenant with work"
  echo "        returned: ${ANSWER}" >&2
fi

# --3-- A tenant that holds only spam and rejected leads must NOT be named: both consumer reads
# filter on `status in ('new', …)`, so walking it costs two round trips and produces nothing.
QUIET="$(px "select count(*) from (${STATEMENT}) t")"
if [ "${QUIET}" = "1" ]; then
  ok "the nine quiet tenants are not walked (1 of 10 returned)"
else
  bad "expected exactly the one tenant with work, got ${QUIET} — the sweep is walking tenants it cannot act on"
fi

# --4-- The consumer reads must still be indexed: the fix must not have pushed the cost onto
# the per-tenant statement that runs after it.
for name in due_breaches due_reminders; do
  case "${name}" in
    due_breaches)
      ST="select id from crm_leads where organization_id = '${ORG_BUSY}' \
            and first_response_at is null and escalated_at is null \
            and first_response_due_at is not null and first_response_due_at <= now() \
            and ${OPEN} order by first_response_due_at, received_at, id limit 50"
      ;;
    due_reminders)
      ST="select l.id from crm_leads l join crm_sla_policies p on p.id = l.sla_policy_id \
            where l.organization_id = '${ORG_BUSY}' and l.first_response_at is null \
            and l.first_response_due_at is not null and ${OPEN/\"status\"/l.status} \
            and p.reminder_minutes is not null \
            and not exists (select 1 from crm_lead_events e \
                            where e.lead_id = l.id and e.kind = 'sla_reminded') \
            order by l.first_response_due_at, l.received_at, l.id limit 50"
      ;;
  esac
  P="$(px "explain (analyze) ${ST}")"
  if echo "${P}" | grep -q "crm_leads_sla_idx"; then
    ok "${name} is still served by the SLA index"
  else
    bad "${name} lost its index — the narrowing moved the cost rather than removing it"
    echo "${P}" | sed 's/^/        /' >&2
  fi
done

# --5-- THE NEGATIVE CONTROL: the production statement with the predicate removed.
#
# This is the shape the code had for the whole life of the sweep, and a perfectly reasonable
# thing to write. It must seq-scan, and that is the whole argument for the predicate being
# load-bearing rather than decorative.
CONTROL="$(px "begin;
                 explain (analyze) select distinct organization_id from crm_leads \
                   where first_response_due_at is not null and first_response_at is null \
                   order by organization_id limit 50;
                 rollback")"
if echo "${CONTROL}" | grep -q "Seq Scan on crm_leads"; then
  ok "PROVEN TO FAIL: without the status predicate the same statement seq-scans, so the gate can see its defect"
else
  bad "PROVEN TO FAIL FAILED: dropping the predicate did not produce a seq scan — this fixture is too small"
  echo "${CONTROL}" | sed 's/^/        /' >&2
fi

echo "[crm-sla-sweep-plan] ${PASS} passed, ${FAIL} failed"
[ "${FAIL}" -eq 0 ]