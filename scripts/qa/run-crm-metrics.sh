#!/usr/bin/env bash
# CRM intake — the metrics endpoint answers about every lead, and "no median" is not zero.
#
#   QA_DB=omnion_qa_w8_metrics bash scripts/qa/run-crm-metrics.sh
#
# ## The gap this gate measures
#
# `GET /api/v1/crm/leads/metrics` has been in REQ-117's API table since the module shipped,
# described as *"inbox metrics (breached, unassigned, median response)"*, and nothing answered
# it — no handler, no store function, and no field on `LeadMetrics` that could hold a response
# time. The counters that *did* exist made the gap invisible: the inbox list returns a
# `metrics` object with the same six fields, and every CRM gate read one. The surface looked
# covered by a neighbour, which is the eighth variation of this branch's signature defect and
# the narrowest yet — not a missing caller but a missing *fact*.
#
# ## Why this gate writes 61 leads
#
# `list_leads` returns one page and counts **that page** on purpose: the inbox's counters must
# describe the rows on screen or "3 new leads" sits above a table of five. An endpoint that
# answered "every lead the organization holds" by delegating to it would cap itself at 51 rows
# and call the answer a total. 61 is one more than `MAX_PAGE`, so no page-sized
# implementation can pass by accident.
#
# ## Why the median is checked for parity on both spellings
#
# The store computes it in SQL (`percentile_cont` — the continuous percentile, which averages
# the middle two on an even count) and the model has the same arithmetic in Rust. Two
# definitions of one word is deliberate, and the gate is what holds them together: an odd
# fixture is not enough, because `percentile_disc` also agrees on an odd count.
#
# ## Its own database, never the pass's
#
# These gates open with `DROP DATABASE … WITH (FORCE)`, which terminates the browser pass's
# API connections; the failure then lands twenty routes from the cause and cost three ticks of
# false CRM walkthroughs before the guard was inherited here.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"
export PGOPTIONS="-c client_min_messages=warning"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_metrics}"

if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database ($DB)." >&2
  echo "        Give the gate its own: the other crm gates use omnion_qa_w8_*." >&2
  exit 1
fi

# Lifted as a whole `postgres://user:***@host:port` prefix out of a sibling gate rather than
# retyped: a hand-written URL produces "N failed" that is the script's configuration, not a
# regression. Byte-level, never from a rendered line — a tool masks credentials in output and
# the mask is what gets copied.
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

export CARGO_TARGET_DIR="${QA_CARGO_TARGET_DIR:-/dev/shm/w8-target}"
export CARGO_INCREMENTAL=0

echo "[crm-metrics] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# `--test-threads=1`: each test creates and drops an organization, and two of them assert on
# the *absence* of another organization's rows — which a concurrent drop would satisfy for the
# wrong reason.
echo "[crm-metrics] crm_metrics"
cargo test -p omnion-module-crm-intake --test crm_metrics -- --test-threads=1

echo "[crm-metrics] PASS"