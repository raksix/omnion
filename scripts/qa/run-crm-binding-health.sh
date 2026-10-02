#!/usr/bin/env bash
# CRM intake — the binding health check, against a real database.
#
#   QA_DB=omnion_qa_w8_binding bash scripts/qa/run-crm-binding-health.sh
#
# ## Why this gate is its own file
#
# REQ-117's risk note says the health check "runs on every submission and flags missing source
# keys instead of writing a lead with silently empty fields". Before this slice, the *first*
# half was an exported function with two unit tests and no production caller, and the *second*
# half was not implemented at all: a form-bound source whose field was renamed kept mapping the
# old key, and `mapping::apply` drops a key the payload does not have, so the lead was written
# with the field silently empty — indistinguishable from a visitor who left it blank.
#
# The column had a model predicate and a screen branch that renders the missing keys. Both were
# correct about a state nothing could put the platform in, which is why nobody could have
# flagged it by looking at the panel.
#
# **A unit test on `health()` is not proof of a caller.** The tests here all go through
# `store::capture` and read the column back, because the defect is the absence of a caller and
# no test that does not exercise the caller can see it.
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

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_binding}"

if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database ($DB)." >&2
  echo "        Give the gate its own: the other crm gates use omnion_qa_w8_*." >&2
  exit 1
fi

# The password is lifted as a whole `postgres://user:…@host:port` prefix out of a sibling gate
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

echo "[crm-binding-health] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# The column the check writes has to exist before any of this is a fact rather than a runtime
# error, and asserting it here means a migration cannot be edited into a comment while the
# tests keep passing.
COLUMN=$(docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -A -t -v ON_ERROR_STOP=1 -c \
  "select data_type from information_schema.columns
   where table_name = 'crm_intake_sources' and column_name = 'broken_mappings'")
case "${COLUMN}" in
  *ARRAY*) : ;;
  *)
    echo "  FAIL: crm_intake_sources.broken_mappings is missing or is not an array." >&2
    echo "        found: ${COLUMN}" >&2
    exit 1
    ;;
esac

# --test-threads=1: each test creates and drops rows for its own organization, and the
# `updated_at` assertion in the idempotence test depends on its two submissions being ordered.
# QA_TEST_FILTER runs one test. Re-running the whole file to diagnose one red line costs a
# full migration sweep, and a red gate gets debugged more than once.
cargo test -p omnion-module-crm-intake --test crm_binding_health -- \
  --test-threads=1 --nocapture ${QA_TEST_FILTER:+"${QA_TEST_FILTER}"}
