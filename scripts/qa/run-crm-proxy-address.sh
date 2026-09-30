#!/usr/bin/env bash
# The caller's address at the moment it becomes a durable CRM row (REQ-117, slice 20).
#
#   QA_DB=omnion_qa_w8_proxy bash scripts/qa/run-crm-proxy-address.sh
#
# ## The gap this gate measures
#
# Slice 19 shipped `MAX_SUBMISSIONS_PER_ADDRESS_PER_HOUR` and counted durable
# `crm_leads.submitter_ip` rows. Its own gate could not see that behind the platform's proxy
# every one of those rows held the *proxy's* address: the gate drove `store::capture` with an
# address argument it supplied itself, so the address's **provenance** was never in question.
# The dial existed, was tested, and did nothing in the only topology the platform runs in.
#
# So this gate starts at the surface a real request enters by — the HTTP endpoint, through the
# real router, with a `ConnectInfo` extension the way `into_make_service_with_connect_info`
# puts one there — and asks the database what address it stored.
#
# ## It gets its own database, never the pass's
#
# These gates open with `DROP DATABASE … WITH (FORCE)`, which terminates the browser pass's API
# connections; the failure then lands twenty routes from the cause. That lesson cost
# `run-crm-request-id.sh` three ticks of false CRM walkthroughs, and the guard is inherited
# rather than retyped.
#
# ## The suites are not interchangeable
#
# `crm_address_ceiling` is slice 19's and stays the regression guard for the *counter*; this
# one guards the *address*. Both run here, because the defect this gate found sits exactly on
# the seam between them: a correct counter reading a wrong column.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"
export PGOPTIONS="-c client_min_messages=warning"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_proxy}"

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

echo "[crm-proxy-address] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# The column is still the counter's business, so its existence is asked before any test runs:
# without it every assertion below fails with a decode error, and "did the migration apply" is
# far cheaper to answer than "why did every test fail".
COLUMN="$(docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -A -t -v ON_ERROR_STOP=1 -c \
  "select data_type from information_schema.columns
   where table_name = 'crm_leads' and column_name = 'submitter_ip'")"
if [ "${COLUMN}" != "inet" ]; then
  echo "  FAIL: crm_leads.submitter_ip is not an inet column (found: '${COLUMN}')." >&2
  exit 1
fi
echo "  ok: crm_leads.submitter_ip is inet — the address has somewhere durable to land"

# --test-threads=1: each test opens and drops its own throwaway database, and the ceiling test
# writes a dozen rows for one source. Sequential, because the shared backend is small and a
# panic mid-suite otherwise leaves databases behind for the next writer to inherit.
# `-p omnion-api` for this file, `-p omnion-module-crm-intake` for slice 19's: they are in
# different packages, and a suite named for the wrong one is "no test target" rather than a
# red assertion — a gate that fails on its own plumbing teaches its reader to ignore the run.
cargo test -p omnion-api --test crm_proxy_address -- \
  --test-threads=1 ${QA_TEST_FILTER:+--nocapture} ${QA_TEST_FILTER:+"${QA_TEST_FILTER}"}

cargo test -p omnion-module-crm-intake --test crm_address_ceiling -- \
  --test-threads=1 ${QA_TEST_FILTER:+--nocapture} ${QA_TEST_FILTER:+"${QA_TEST_FILTER}"}
