#!/usr/bin/env bash
# CRM intake — the assignment chain, driven from the ENTRY POINT rather than from the function.
#
#   QA_DB=omnion_qa_w8_capture bash scripts/qa/run-crm-capture-routing.sh
#
# ## Why this gate exists separately from run-crm-assignment.sh
#
# That gate has been green for twenty-four ticks and proves nine things about the assignment
# chain. Every one of them calls `claim_assignment`, `stamp_assignment` or `policy_for_source`
# directly, and not one of them begins at `store::capture` — because `capture` never called any
# of the three. The whole slice-2 routing chain had no road to a submission: leads arrived
# unassigned, with no owner and no `first_response_due_at`, while every mid-stack assertion
# stayed green.
#
# This gate is therefore not a tenth assertion about the same functions. It is the one that
# asks the only question the others could not: **does a lead arriving through the public path
# actually get routed?** It goes through `store::capture`, reads the stored row back, and checks
# the trail — so a call site that is missing, or that reads the payload where it should read
# the mapped values, is red here and invisible everywhere else.
#
# ## The lesson, recorded in the file so the next gate does not have to rediscover it
#
#   A gate that begins at the function proves the function.
#
# Every gate in this module starts mid-stack. That is what makes them fast, and it is exactly
# what makes them blind to wiring. This is the sixth time this crate shipped a correct,
# unit-tested, REQ-named function with no caller able to produce the state it describes; the
# first five were found by reading callers out of definitions by hand. The repeatable version
# is this file: one entry-point assertion per feature, then let the mid-stack gates keep doing
# what they are good at.
#
# Its own database, never the pass's: these gates open with `DROP DATABASE … WITH (FORCE)`,
# which terminates the browser pass's API connections, and the failure then lands twenty routes
# from the cause on whichever screens come next.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_capture}"

if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database ($DB)." >&2
  echo "        Give the gate its own: the other crm gates use omnion_qa_w8_*." >&2
  exit 1
fi

# The credentials are lifted as a whole `postgres://user:password@host:port` prefix out of a
# sibling gate rather than retyped: a hand-written URL produces "N failed" that is the
# script's configuration, not a regression. Byte-level, never from a rendered line — a tool
# masks credentials in output and the mask is what gets copied, which is exactly how a first
# draft of this file ended up with a regex full of asterisks and an "unbalanced parenthesis".
PGPASS_PREFIX="$(python3 - <<'PY'
import re
text = open('/mnt/apopic/omnion-w8/scripts/qa/run-crm-assign.sh', encoding='utf-8').read()
m = re.search(r'DATABASE_URL="(postgres://[^@"\\"]+@127\.0\.0\.1:5433)/', text)
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

echo "[crm-capture-routing] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# The chain is only reachable if the tables it reads exist. Counted in SQL and compared in the
# shell, because a check that prints a number nobody compares is the silent-pass shape these
# gates exist to avoid — and a gate whose database silently lacks a table reports "the lead was
# not routed", which is a true sentence about the wrong world.
TABLES=$(docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -A -t -v ON_ERROR_STOP=1 -c \
  "select count(*) from information_schema.tables
    where table_name in ('crm_leads','crm_assignment_rules','crm_sla_policies',
                         'crm_intake_sources','crm_lead_events','crm_lead_submissions')")
if [ "${TABLES}" != "6" ]; then
  echo "  FAIL: the routing chain's tables are not all present (found ${TABLES}/6)." >&2
  echo "        0055_crm_lead_intake.sql and 0056_crm_assignment_sla.sql must apply to ${DB}." >&2
  exit 1
fi
echo "[crm-capture-routing] migration present: 6 tables, capture path reachable"

# `--test-threads=1`: every test in this file creates and drops rows for its own organization,
# and concurrent tests on one database make each other's fixtures disappear mid-run. This is the
# harness bug `crm_assignment.rs` documents at length — it read as eight product failures once.
cargo test -p omnion-module-crm-intake --test crm_capture_routing -- --test-threads=1 --nocapture
