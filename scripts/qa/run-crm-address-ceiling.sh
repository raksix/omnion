#!/usr/bin/env bash
# CRM intake — the per-address hourly ceiling, and the column that makes it countable.
#
#   QA_DB=omnion_qa_w8_address bash scripts/qa/run-crm-address-ceiling.sh
#
# ## The gap this gate measures
#
# `Submission.ip` has carried the doc comment "the submitter's IP, for the per-IP rate limit
# and the audit trail" since the capture struct shipped. The audit half was real. The
# per-IP rate limit **did not exist**: `submissions_this_hour` is the only ceiling in the
# capture path and it counts BY SOURCE, so one address could spend a whole source's hourly
# budget in seconds — and the single dial an operator had moved the flood and their real
# traffic together. The REQ's own Risks section calls this surface "the attack surface".
#
# ## Why the tests begin at `capture`
#
# The same rule `run-crm-capture-routing.sh` and `run-crm-assignment.sh` were both written to
# state, and both learned it the expensive way: a gate that begins at the function proves the
# function. Nine assertions about three correct functions with no production caller stayed
# green for twenty-four ticks. The claim under test here is "a submission is refused", so the
# tests drive real captures and read the stored rows back.
#
# ## Why the schema is read before any test runs
#
# The ceiling counts durable rows, and the address cannot be recovered once the request is
# gone. Without `0192` the tests would fail with a decode error rather than a sentence about
# the migration, and "did the migration apply" is far cheaper to answer than "why did every
# test fail". The index is checked for the same reason: a counter that cannot use an index is
# a sequential scan of every lead the source has ever taken — a different failure from the one
# this gate names, and one that would arrive silently under load.
#
# ## Its own database, never the pass's
#
# These gates open with `DROP DATABASE … WITH (FORCE)`, which terminates the browser pass's
# API connections; the failure then lands twenty routes from the cause. That lesson cost
# `run-crm-request-id.sh` three ticks of false CRM walkthroughs, and the guard is inherited
# below rather than re-typed.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"
export PGOPTIONS="-c client_min_messages=warning"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_address}"

if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database ($DB)." >&2
  echo "        Give the gate its own: the other crm gates use omnion_qa_w8_*." >&2
  exit 1
fi

# The password is lifted as a whole `postgres://user:...@host:port` prefix out of a sibling gate
# rather than retyped: a hand-written URL produces "N failed" that is the script's
# configuration, not a regression. Byte-level, never from a rendered line — a tool masks
# credentials in output and the mask is what gets copied.
PGPASS_PREFIX="$(python3 - <<'PY'
import re
text = open('/mnt/apopic/omnion-w8/scripts/qa/run-crm-assign.sh', encoding='utf-8').read()
m = re.search(r'DATABASE_URL="(postgres://[^@"]+@127\.0\.0\.1:5433)/', text)
print(m.group(1) if m else '')
PY
)"
if [ -z "${PGPASS_PREFIX}" ]; then
  echo "  FAIL: could not read the QA database prefix out of run-crm-assign.sh." >&2
  exit 1
fi
export DATABASE_URL="${PGPASS_PREFIX}/${DB}"

# The warm target dir this branch's other gates and its own build share, on purpose: a fresh
# one recompiles the whole dependency graph, and on a box where nine writers share one 32G
# tmpfs that is how a gate dies of "No space left on device" while its own code is fine.
export CARGO_TARGET_DIR="${QA_CARGO_TARGET_DIR:-/dev/shm/w8-target}"
export CARGO_INCREMENTAL=0

echo "[crm-address-ceiling] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done


# The column is the fix, so the gate proves it exists before it runs anything.
COLUMN="$(docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -A -t -v ON_ERROR_STOP=1 -c \
  "select data_type from information_schema.columns
   where table_name = 'crm_leads' and column_name = 'submitter_ip'")"
if [ "${COLUMN}" != "inet" ]; then
  echo "  FAIL: crm_leads.submitter_ip is not an inet column (found: '${COLUMN}')." >&2
  exit 1
fi
echo "  ok: crm_leads.submitter_ip is inet"

# The index the counter reads through, asked of PostgreSQL rather than matched as a string.
#
# **Two wrong versions of this check, both instructive.** The first globbed `indexdef` for
# `*is\ not\ null*`; in a `case` pattern a backslash escapes the next character, so it matched
# a literal `\` and reported a correct index as missing. The second asked
# `pg_get_expr(indexprs, …)`, which is the wrong *catalog column*: `indexprs` holds
# expression trees, and a partial index's predicate lives in `indpred` — an `oid` into
# `pg_node_tree`, not an expression list. Every partial index in the schema came back with an
# empty predicate, including the one that was right there. A gate that reports a healthy
# schema as broken is worse than no gate, because it teaches the reader to skip the line.
#
# So the question is now one PostgreSQL answers as a boolean, and it asks the property rather
# than the spelling: the index exists **and** its predicate mentions the column. A *full*
# index over the same three columns would satisfy "does the index exist" while still carrying
# every null address, and the nulls are exactly the rows this count can never match.
INDEX_PREDICATE="$(docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -A -t -v ON_ERROR_STOP=1 -c \
  "select case when i.indpred is null then 'none'
                   when pg_get_expr(i.indpred, i.indrelid) like '%submitter_ip IS NOT NULL%'
                     then 'submitter_ip IS NOT NULL'
                   else pg_get_expr(i.indpred, i.indrelid)
              end
   from pg_index i
   join pg_class c on c.oid = i.indexrelid
   where i.indrelid = 'crm_leads'::regclass
     and c.relname = 'crm_leads_submitter_ip_idx'")"
if [ "${INDEX_PREDICATE}" = "submitter_ip IS NOT NULL" ]; then
  echo "  ok: the partial index excludes the null addresses"
elif [ -z "${INDEX_PREDICATE}" ]; then
  echo "  FAIL: crm_leads_submitter_ip_idx does not exist." >&2
  exit 1
else
  echo "  FAIL: the index is not partial over the non-null addresses." >&2
  echo "        predicate: ${INDEX_PREDICATE}" >&2
  exit 1
fi

# The claim's referential action is read before the tests, for the same reason as the column:
# a gate that discovers a schema contradiction only after a suite of unrelated tests has
# failed buries the actual cause under a page of 23514s from a different table.
#
# **Cascade, and the direction matters.** `on delete set null` is what the first fix tried to
# live with, and it is the *cause* of the contradiction rather than its cure: nulling
# `lead_id` while `completed_at` stays set is precisely the row
# `crm_lead_submissions_completion` refuses. So the gate asserts the action, not merely that
# a constraint with the right name exists.
FK_DEF="$(docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -A -t -v ON_ERROR_STOP=1 -c \
  "select pg_get_constraintdef(oid) from pg_constraint
   where conname = 'crm_lead_submissions_lead_id_fkey'")"
case "${FK_DEF}" in
  *"ON DELETE CASCADE"*)
    echo "  ok: a claim is deleted with its lead, so the completion check cannot be broken"
    ;;
  *)
    echo "  FAIL: the claim's foreign key is not ON DELETE CASCADE." >&2
    echo "        found: ${FK_DEF}" >&2
    echo "        with set null, deleting a lead raises 23514 on the completion check." >&2
    exit 1
    ;;
esac

# --test-threads=1: each test creates and drops rows for its own organization, and the flood
# tests write a dozen rows apiece. Both suites run in the same database because the second
# is a regression guard for the first — `a_deleted_lead_stops_counting_against_the_visitor`
# found the deletion defect, and it is the assertion that would catch it coming back.
for suite in crm_address_ceiling crm_lead_delete; do
  echo "[crm-address-ceiling] ${suite}"
  cargo test -p omnion-module-crm-intake --test "${suite}" -- \
    --test-threads=1 ${QA_TEST_FILTER:+--nocapture} ${QA_TEST_FILTER:+"${QA_TEST_FILTER}"}
done
