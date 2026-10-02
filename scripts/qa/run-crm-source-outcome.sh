#!/usr/bin/env bash
# CRM intake — every capture records what the source did with it, including every refusal.
#
#   QA_DB=omnion_qa_w8_outcome bash scripts/qa/run-crm-source-outcome.sh
#
# ## The gap this gate measures
#
# `crm_intake_sources` carries `last_received_at` and `last_error`, the editor renders both (the
# second in red), and `record_source_outcome`'s own doc comment says an operator asking "why is
# nothing arriving" wants the error "from an hour ago". `capture` stamped that row on its three
# **success-shaped** exits — rejected, spam, accepted — and on **none of its four refusal
# exits**: a paused source, an oversized body, the source's hourly ceiling and the per-address
# ceiling. Those four are exactly the situations in which a form is silently not working *and
# no lead row exists to look at*, so the source row is the only place the answer can live — and
# it said nothing. The loudest case is the rate limit: a whole source answers 429 for an hour
# while the editor reads as though it never received anything.
#
# ## Why the tests read the stored row and not the returned error
#
# The same rule `run-crm-capture-routing.sh` and `run-crm-assignment.sh` were both written to
# state, and both learned it the expensive way: a gate that begins at the function proves the
# function. `CrmIntakeError::RateLimited` has been returned by that line since the ceiling
# shipped, so an assertion on the `Err` alone passes against the pre-fix code in its entirety.
# The claim under test here is "the refusal left a trace", so every test drives a real capture
# and reads `crm_intake_sources` back out of the database.
#
# ## Why the address-ceiling suite runs here too
#
# `crm_address_ceiling` is a regression guard for two of the four exits: it is the suite that
# found the deletion defect, and it is the assertion that would catch the address ceiling
# stopping to fire. Two suites run in the same database because the second measures the
# behaviour the first's fix depends on.
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
DB="${QA_DB:-omnion_qa_w8_outcome}"

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

echo "[crm-source-outcome] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done


# --test-threads=1: the flood tests write a dozen rows apiece and each creates and drops rows
# for its own organization, and the ceiling tests are order-sensitive within one source (the
# first submission must be the one that spends the budget).
for suite in crm_source_outcome crm_address_ceiling; do
  echo "[crm-source-outcome] ${suite}"
  cargo test -p omnion-module-crm-intake --test "${suite}" -- \
    --test-threads=1 ${QA_TEST_FILTER:+--nocapture} ${QA_TEST_FILTER:+"${QA_TEST_FILTER}"}
done
