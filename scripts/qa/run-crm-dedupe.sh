#!/usr/bin/env bash
# CRM intake — the duplicate verdict, on an installation that HAS the CRM.
#
#   QA_DB=omnion_qa_w8_dedupe bash scripts/qa/run-crm-dedupe.sh
#
# This gate exists for one reason, and the reason is a defect fourteen ticks of tests could not
# see. `crm_leads.duplicate_of` is a foreign key to `crm_leads`; `capture` wrote a
# `crm_contacts` id into it. On an installation with the CRM that is a 23503 and a
# `reject_duplicate` source answers 500 to every visitor who writes in. On THIS branch
# `crm_contacts` does not exist, so the dedupe pass never returns a candidate and the arm is
# dead code in every other gate — the absence branch that is supposed to be the *default*
# behaviour is what hid the bug on the non-default installation.
#
# So the other five gates keep testing the CRM-less world (it is a real product, and the
# degradation path is worth a gate of its own — see run-crm-convert.sh), and this one builds the
# CRM's tables and tests the world where the feature can actually run. **Testing one
# installation is testing one product.**
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_dedupe}"

# Its OWN database, never the pass's: these gates open with `DROP DATABASE … WITH (FORCE)`,
# which terminates the browser pass's API connections, and the failure then lands twenty routes
# from the cause on whichever screens come next.
if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database ($DB)." >&2
  echo "        Give the gate its own: the other crm gates use omnion_qa_w8_*." >&2
  exit 1
fi

# The password is read out of the pass's own script rather than retyped: a hand-written URL
# produces "N failed" that is the script's configuration, not a regression. Byte-level, never
# from a rendered line — the tool masks credentials in output and the mask is what gets copied.
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

echo "[crm-dedupe] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# The migration has to be the one this branch ships — a silently-skipped `if not exists` is how a
# gate passes against a stale database. Counted in SQL and compared in the shell, because a
# check that prints a number nobody compares is the silent-pass shape these gates exist to avoid.
MIGRATED=$(docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -A -t -v ON_ERROR_STOP=1 -c \
  "select count(*) from information_schema.columns
    where table_name = 'crm_leads'
      and column_name in ('dedupe_contact_id', 'dedupe_score', 'dedupe_key')")
if [ "${MIGRATED}" != "3" ]; then
  echo "  FAIL: the dedupe columns are not all present (found ${MIGRATED}/3)." >&2
  echo "        Migration 0144_crm_lead_dedupe_pointers.sql did not apply to ${DB}." >&2
  exit 1
fi
echo "[crm-dedupe] migration present: dedupe_contact_id, dedupe_score, dedupe_key"

# `--test-threads=1`: every test in this file creates and drops rows for its own organization,
# and concurrent tests on one database make each other's fixtures disappear mid-run.
cargo test -p omnion-module-crm-intake --test crm_dedupe -- --test-threads=1 --nocapture
