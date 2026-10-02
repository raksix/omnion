#!/usr/bin/env bash
# CRM intake — the verdict row: a submission with nothing to contact is RECORDED, not lost.
#
#   QA_DB=omnion_qa_w8_verdict bash scripts/qa/run-crm-verdict-rows.sh
#
# ## Why this gate is its own file
#
# REQ-117 acceptance 5 says a submission with neither e-mail nor phone is "refused with a
# readable reason, and no partial lead row is written" — and the store honours the first half
# in the one place that matters:
#
#     if !mapped.missing_required.is_empty() || !contactable(email, phone) {
#         let lead = insert_lead(..., LeadWrite { status: "rejected", rejection_reason: Some(reason), ... })
#
# **That insert raised 23514 and the row was never written**, because `crm_leads` carried
#
#     constraint crm_leads_contactable_check
#         check (coalesce(email,'') <> '' or coalesce(phone,'') <> '')
#
# and the row is by construction a row with neither. The refusal a visitor's form was supposed
# to render arrived as a 500, and the only evidence that the submission existed at all was a
# log line nobody reads.
#
# It lasted twenty ticks because **no fixture reached it**: the branch needs a mapping that
# produces no e-mail and no phone, and every CRM gate maps `email`. Two sibling test files
# therefore carried the sentence "unreachable on this branch" in place of a test — which is the
# shape of a bug that reads as a documented limitation. Migration `0159` narrows the check to
# the three verdict statuses; the tests go through `store::capture`, because the defect was
# never in the store's logic and a unit test on the store was green throughout.
#
# ## Its own database, never the pass's
#
# These gates open with `DROP DATABASE … WITH (FORCE)`, which terminates the browser pass's
# API connections; the failure then lands twenty routes from the cause. The same lesson that
# cost `run-crm-request-id.sh` three ticks of false CRM walkthroughs.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"
export PGOPTIONS="-c client_min_messages=warning"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_verdict}"

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

echo "[crm-verdict-rows] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# The migration is the fix, so the gate reads the constraint's own text before running any
# test. A test that passes against a check which was never narrowed would say nothing about the
# defect, and "did the migration apply" is cheaper to answer than "why did nothing fail".
DEFINITION="$(docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -A -t -v ON_ERROR_STOP=1 -c \
  "select pg_get_constraintdef(oid) from pg_constraint
   where conname = 'crm_leads_contactable_check'")"
case "${DEFINITION}" in
  *rejected*spam*duplicate*)
    echo "  ok: the check exempts the three verdict statuses"
    ;;
  *)
    echo "  FAIL: crm_leads_contactable_check does not mention the verdict statuses." >&2
    echo "        found: ${DEFINITION}" >&2
    exit 1
    ;;
esac

# --test-threads=1: each test creates and drops rows for its own organization, and the claim
# test depends on its two deliveries being ordered.
# QA_TEST_FILTER runs one test. Re-running the whole file to diagnose one red line costs a
# full migration sweep, and a red gate gets debugged more than once.
cargo test -p omnion-module-crm-intake --test crm_verdict_rows -- \
  --test-threads=1 --nocapture ${QA_TEST_FILTER:+"${QA_TEST_FILTER}"}
