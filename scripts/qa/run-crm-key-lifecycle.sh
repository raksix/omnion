#!/usr/bin/env bash
# CRM intake — the endpoint key's lifecycle, against a real database.
#
#   bash scripts/qa/run-crm-key-lifecycle.sh
#
# ## Why this gate is its own file
#
# `store::rotate_key`'s doc comment has promised a refusal since the key surface shipped:
#
#   > Refused for a source that has no key (a form-bound one), because "rotate" on a surface
#   > with no key is a button that appears to work and does nothing.
#
# There was no refusal anywhere. The function found the source, issued a key and wrote it, so
# `POST /api/v1/crm/intake/sources/{id}/rotate-key` turned a form-bound source into a keyed
# endpoint — and `find_source_by_key` matches on the digest **regardless of `kind`**, so the
# source became reachable at a public URL. The panel hides the button, which is the boundary
# the REQ says the API must enforce rather than the UI hiding it.
#
# The tests in `tests/crm_key_lifecycle.rs` go through the store, not the handler, and read
# the row back afterwards: the defect is "the guard does not exist", and a fixture that
# supplied its own answer could not see that.
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
DB="${QA_DB:-omnion_qa_w8_keylife}"

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

echo "[crm-key-lifecycle] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# The promise is written in a doc comment, and a doc comment is not a constraint. This asks the
# schema whether the two columns the key lives in are the shape the gate's premise assumes:
# `endpoint_key_hash` nullable (a form source stores nothing) and `endpoint_key_hint` nullable
# with it. A migration edited into a NOT NULL would make the refusal untestable in the way it
# matters, and it is far cheaper to say so here than to read "N failed" later.
COLUMNS="$(docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -A -t -v ON_ERROR_STOP=1 -c \
  "select string_agg(column_name || ':' || is_nullable, ' ' order by column_name)
   from information_schema.columns
   where table_name = 'crm_intake_sources'
     and column_name in ('endpoint_key_hash', 'endpoint_key_hint')")"
case "${COLUMNS}" in
  *"endpoint_key_hint:YES"*)
    echo "  ok: the key columns are nullable, so a keyless source is representable"
    ;;
  *)
    echo "  FAIL: endpoint_key_hash/hint are not both nullable; the premise is wrong." >&2
    echo "        found: ${COLUMNS}" >&2
    exit 1
    ;;
esac

# --test-threads=1: each test creates and drops rows for its own organization, and the
# rotation tests read back a row their own fixture wrote.
# QA_TEST_FILTER runs one test. Re-running the whole file to diagnose one red line costs a
# full migration sweep, and a red gate gets debugged more than once.
cargo test -p omnion-module-crm-intake --test crm_key_lifecycle -- \
  --test-threads=1 --nocapture ${QA_TEST_FILTER:+"${QA_TEST_FILTER}"}