#!/usr/bin/env bash
# CRM intake — first touch: the MAPPED address decides whose first touch is kept.
#
#   QA_DB=omnion_qa_w8_attribution bash scripts/qa/run-crm-attribution.sh
#
# ## Why this gate is its own file
#
# REQ-117 acceptance 6 says "a second submission from the same visitor keeps the original
# first touch". `Attribution::merge_first_touch` has implemented that sentence correctly since
# slice 1, with a unit test. The defect was the line ABOVE it:
#
#     let Some(key) = submission.payload.get("email")     // the RAW payload's key
#     else { return Ok(later.clone()) };
#     select … where … and lower(email) = $3
#
# `insert_lead` writes `mapped.get("email")` into that column. So the row and the lookup used
# **two different names for one value**, and every source whose form calls the field anything
# but `email` found nothing — the merge returned the later visit untouched and the second
# submission overwrote the campaign that first brought the visitor in.
#
# It survived because every fixture in this crate maps `email` from a key called `email`, so
# the names coincided and the wrong lookup answered the right question. The unit test could
# not see it: it drove `merge_first_touch` with hand-built values and never went near the
# lookup that decides *whose row* to read. **A test on the pure function is not a test of the
# impure one that decides when to apply it** — the mirror image of the binding-health and
# round-robin defects, and the fifth of its kind in this module.
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
DB="${QA_DB:-omnion_qa_w8_attribution}"

if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database ($DB)." >&2
  echo "        Give the gate its own: the other crm gates use omnion_qa_w8_*." >&2
  exit 1
fi

# The password is lifted as a whole `postgres://user:***@host:port` prefix out of a sibling gate
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

echo "[crm-attribution] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# The columns the acceptance line is about, asserted before any test runs. A gate whose
# subject does not exist should say so, rather than passing five tests that assert nothing
# about it.
COLUMNS="$(docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -A -t -c \
  "select string_agg(column_name, ',' order by column_name) from information_schema.columns
   where table_name = 'crm_leads'
     and column_name in ('utm_campaign','utm_source','referrer_host','landing_path')")"
case "${COLUMNS}" in
  *landing_path*referrer_host*utm_campaign*utm_source*)
    echo "  ok: the four attribution columns exist (${COLUMNS})"
    ;;
  *)
    echo "  FAIL: crm_leads is missing an attribution column the first touch lives in." >&2
    echo "        found: ${COLUMNS}" >&2
    exit 1
    ;;
esac

# --test-threads=1: each test creates and drops rows for its own organization, and the
# ordering assertions read rows back oldest-first within one organization.
# QA_TEST_FILTER runs one test. Re-running the whole file to diagnose one red line costs a
# full migration sweep, and a red gate gets debugged more than once.
cargo test -p omnion-module-crm-intake --test crm_attribution -- \
  --test-threads=1 --nocapture ${QA_TEST_FILTER:+"${QA_TEST_FILTER}"}
