#!/usr/bin/env bash
# CRM intake — one submission, one lead, under concurrency.
#
#   QA_DB=omnion_qa_w8_claims bash scripts/qa/run-crm-claims.sh
#
# ## Why this gate is its own file
#
# The acceptance line "one submission, one lead, no duplicates" was the last unticked box in
# REQ-117, and the code that was supposed to enforce it was a `select … where exists` followed
# by an unguarded insert. Nothing about a second lead is an error: it is a well-formed row with
# its own dedupe verdict, so no log, no assertion and no screen distinguishes it.
#
# A sequential test cannot see it. Two calls in a row pass against the old code — the second
# finds the first's row. Only *simultaneous* deliveries collide, and only then does the count
# become two. That is the same sentence `run-crm-assignment.sh` already carries about the
# round-robin cursor ("a read-then-write cursor passes every sequential test and fails here,
# intermittently"), which is why this defect is a second instance of a shape this branch has
# now shipped twice.
#
# **Its own database, never the pass's.** These gates open with `DROP DATABASE … WITH (FORCE)`,
# which terminates the browser pass's API connections; the failure then lands on whichever
# screens come next, twenty routes from the cause.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_claims}"

if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database ($DB)." >&2
  echo "        Give the gate its own: the other crm gates use omnion_qa_w8_*." >&2
  exit 1
fi

# The password is read out of a sibling gate's script rather than retyped: a hand-written URL
# produces "N failed" that is the script's configuration, not a regression. Byte-level, never
# from a rendered line — a tool masks credentials in output and the mask is what gets copied.
PGPASS_PORT="$(grep -oE '127\.0\.0\.1:5433' scripts/qa/run-crm-assign.sh | head -1)"
PGPASS_USER="$(grep -oE 'postgres://[a-z_]+' scripts/qa/run-crm-assign.sh | head -1 | cut -d/ -f3)"
PGPASS_PASS="$(python3 - <<'PY'
import re
text = open('/mnt/apopic/omnion-w8/scripts/qa/run-crm-assign.sh', encoding='utf-8').read()
m = re.search(r'DATABASE_URL="postgres://[^:]+:([^@]+)@', text)
print(m.group(1) if m else '')
PY
)"
if [ -z "${PGPASS_PASS}" ]; then
  echo "  FAIL: could not read the QA database password out of run-crm-assign.sh." >&2
  exit 1
fi
export DATABASE_URL="postgres://${PGPASS_USER}:${PGPASS_PASS}@127.0.0.1:5433/${DB}"
export CARGO_TARGET_DIR="${QA_CARGO_TARGET_DIR:-/dev/shm/w8-target}"
export CARGO_INCREMENTAL=0

echo "[crm-claims] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# The claim table has to be the one THIS branch ships. Counted in SQL and compared in the
# shell, because a check that prints a number nobody compares is the silent-pass shape.
CLAIMED=$(docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -A -t -v ON_ERROR_STOP=1 -c \
  "select count(*) from information_schema.columns
    where table_name = 'crm_lead_submissions'
      and column_name in ('source_id','submission_id','lead_id','claimed_at','completed_at')")
if [ "${CLAIMED}" != "5" ]; then
  echo "  FAIL: the claim columns are not all present (found ${CLAIMED}/5)." >&2
  echo "        Migration 0150_crm_lead_submission_claims.sql did not apply to ${DB}." >&2
  exit 1
fi
# And the primary key is the mechanism — a table without it passes every sequential test and
# still writes two leads under concurrency, which is the defect this gate exists to catch.
KEYED=$(docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -A -t -v ON_ERROR_STOP=1 -c \
  "select count(*) from pg_constraint
    where conrelid = 'crm_lead_submissions'::regclass and contype = 'p'
      and conkey = array[
        (select attnum from pg_attribute where attrelid = 'crm_lead_submissions'::regclass
           and attname = 'source_id'),
        (select attnum from pg_attribute where attrelid = 'crm_lead_submissions'::regclass
           and attname = 'submission_id')]::smallint[]")
if [ "${KEYED}" != "1" ]; then
  echo "  FAIL: crm_lead_submissions has no primary key on (source_id, submission_id)." >&2
  echo "        Without it this gate is measuring nothing: every sequential test still passes." >&2
  exit 1
fi
echo "[crm-claims] migration present: 5 columns, primary key (source_id, submission_id)"

# `--test-threads=1`: each test creates and drops rows for its own organization, and concurrent
# tests on one database make each other's fixtures disappear mid-run. The concurrency this gate
# measures is *inside* each test (a barrier over one submission), not between tests.
cargo test -p omnion-module-crm-intake --test crm_claims -- --test-threads=1 --nocapture
