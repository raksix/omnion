#!/usr/bin/env bash
# Storefront settings (REQ-118 slice 1a) — the store against a database that really applied 0169.
#
#   QA_DB=omnion_qa_w8_storefront bash scripts/qa/run-storefront-settings.sh
#
# ## The defect class this gate exists for
#
# Slice 1a writes the same closed list twice — once as a Rust constant, once as a `check`
# constraint in `0169_storefront_settings.sql` — and this branch has been bitten by exactly
# that class twice already:
#
#   * `run-project-isolation.sh`: migration 0164 made `workflows.project_id` NOT NULL while
#     `insert_workflow` named twelve columns and none was it. Every workflow creation raised
#     23502 and the crate reported 48/48, because **no unit test executes SQL**.
#   * `run-crm-verdict-rows.sh`: `capture` wrote a `rejected` lead with no address and
#     `crm_leads_contactable_check` refused it with 23514, while the acceptance line promised
#     the row was written. The unit tests agreed with the workaround two sibling files had
#     written in prose.
#
# So the split on this branch is standing: a **gate on the schema** (a shell script that
# applies the migrations and reads `information_schema`) and a **test on the code** (a crate
# test) are both necessary and neither is sufficient. A test that calls `store::save` against
# a migrated database is the only place where "the column the store writes" and "the column
# the table has" are the same fact.
#
# ## Why it is a crate test and not a psql script
#
# A psql script would re-implement the insert, and then it would prove the script. This runs
# `cargo test -p omnion-module-ecommerce --test storefront_settings`, which calls
# `store::load`, `store::save` and `store::load_all` directly — so changing any of them is
# what changes this gate's result.
#
# Its own database, never the pass's: the DROP below is `WITH (FORCE)`, which terminates the
# browser pass's API connections and then fails twenty routes away from the cause.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_storefront}"

if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database (${DB})." >&2
  echo "        Give the gate its own, as the other w8 gates do." >&2
  exit 1
fi

# Lifted as a whole `postgres://user:***@host:port` PREFIX from a sibling gate, byte-level and
# never from a rendered line: a tool masks credentials in its output, and the mask is what
# gets copied. That has bitten this branch repeatedly, and the symptom is 28P01 on every
# test with a green build.
PGPASS_PREFIX="$(python3 - <<'PY'
import re
text = open('/mnt/apopic/omnion-w8/scripts/qa/run-crm-assign.sh', encoding='utf-8').read()
m = re.search(r'DATABASE_URL="(postgres://[^@"\[\]]+@127\.0\.0\.1:5433)/', text)
print(m.group(1) if m else '')
PY
)"
if [ -z "${PGPASS_PREFIX}" ]; then
  echo "  FAIL: could not read the QA database prefix out of run-crm-assign.sh." >&2
  exit 1
fi
case "${PGPASS_PREFIX}" in
  *'*'*) echo "  FAIL: the lifted prefix contains an asterisk run — the credential mask was copied." >&2
         exit 1 ;;
esac

export DATABASE_URL="${PGPASS_PREFIX}/${DB}"
export CARGO_TARGET_DIR="${QA_CARGO_TARGET_DIR:-/dev/shm/w8-target}"
export CARGO_INCREMENTAL=0

echo "[storefront] applying migrations to ${DB} (0169 included — it is the subject)"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done
echo "done"

# The pre-flight the whole gate rests on. If 0169 did not apply, every test below is measuring
# a schema that does not exist and reporting a true sentence about the wrong world — which is
# precisely how run-project-isolation.sh's first run went if its check had been missing.
TABLE=$(docker exec "$CONTAINER" psql -U omnion -d "$DB" -q -A -t \
  -c "select count(*) from information_schema.tables where table_name = 'storefront_settings'")
if [ "${TABLE}" != "1" ]; then
  echo "  FAIL: storefront_settings does not exist — 0169 did not apply." >&2
  exit 1
fi

# Every constraint the crate's vocabulary duplicates, named. A missing one is a band that has
# silently stopped being enforced, and the crate's unit tests cannot see that: they check the
# Rust constant, which is still there and still right.
COLS=$(docker exec "$CONTAINER" psql -U omnion -d "$DB" -q -A -t \
  -c "select count(*) from information_schema.columns
       where table_name = 'storefront_settings'
         and column_name in ('site_id','guest_checkout','tax_display','listing_variant',
                             'page_size','pagination','per_order_item_max','wishlist_enabled',
                             'low_stock_badge_threshold','abandonment_hours',
                             'confirmation_template','currency')")
if [ "${COLS}" != "12" ]; then
  echo "  FAIL: expected 12 storefront_settings columns, found ${COLS}." >&2
  exit 1
fi

CHECKS=$(docker exec "$CONTAINER" psql -U omnion -d "$DB" -q -A -t \
  -c "select count(*) from pg_constraint
       where conrelid = 'storefront_settings'::regclass and contype = 'c'")
if [ "${CHECKS}" -lt 6 ]; then
  echo "  FAIL: expected the 6 band checks, found ${CHECKS}." >&2
  exit 1
fi
echo "[storefront] 0169 applied: table + 12 columns + ${CHECKS} check constraints"

echo
set +e
cargo test -p omnion-module-ecommerce --test storefront_settings -- --test-threads=4 --nocapture
STATUS=$?
set -e

echo
if [ "${STATUS}" != "0" ]; then
  echo "[storefront] FAILED (exit ${STATUS}) — read the error above before theorising."
  echo "            A failure in the cross-organization test means the write path accepted a"
  echo "            foreign site id, which is the defect this gate was added for."
  exit 1
fi
echo "[storefront] passed"
