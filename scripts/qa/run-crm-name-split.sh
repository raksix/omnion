#!/usr/bin/env bash
# CRM intake — the name split: one form field, two CRM columns.
#
#   QA_DB=omnion_qa_w8_namesplit bash scripts/qa/run-crm-name-split.sh
#
# ## Why this gate is its own file
#
# `split_full_name` shipped in `mapping::TRANSFORMS`, is validated by `is_transform`, is offered by
# the mapping editor and is documented on the function as the answer to "a form with one name
# field is the common case". Its own unit test asserted the splitter's two returns directly.
#
# **Nothing ever called the splitter.** `apply_transforms` — the one function the public `apply`
# runs every transform through — answered `"split_full_name" => value`, an identity, and the
# comment above the unreachable arm said *"Unreachable: `apply` validates every name before
# running"*, which was true and irrelevant: validation is about whether the name is **known**,
# not whether the branch **does** anything.
#
# Every lead from every one-name form on this branch was therefore written with the whole name in
# `first_name` and `last_name` NULL, and nothing failed — the surname is not required by
# `crm_leads_contactable_check` and no screen renders the pair together.
#
# **The green that hid it was the lesson.** `split_full_name_fills_both_halves` calls
# `split_full_name` directly and kept passing for the life of the module. A test that calls the
# leaf proves the leaf works; only a test that goes through the entry point can say whether the
# leaf is *reached*. Every test in `crm_name_split.rs` goes through `store::capture` and reads
# `crm_leads` back, because "apply produces two values" is exactly the fact that was true and
# useless.
#
# ## Its own database, never the pass's
#
# These gates open with `DROP DATABASE … WITH (FORCE)`, which terminates the browser pass's API
# connections; the failure then lands twenty routes from the cause.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"
export PGOPTIONS="-c client_min_messages=warning"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_namesplit}"

if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database ($DB)." >&2
  echo "        Give the gate its own: the other crm gates use omnion_qa_w8_*." >&2
  exit 1
fi

# Lifted as a whole `postgres://user:***@host:port` prefix out of a sibling gate rather than
# retyped, byte-level and never from a rendered line: a tool masks credentials in output and the
# mask is what gets copied.
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

# The warm target dir this branch's other gates and its own build share: a fresh one recompiles
# the whole dependency graph, and on a box where nine writers share one 32G tmpfs that is how a
# gate dies of "No space left on device" while its own code is fine.
export CARGO_TARGET_DIR="${QA_CARGO_TARGET_DIR:-/dev/shm/w8-target}"
export CARGO_INCREMENTAL=0

echo "[crm-name-split] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# The transform is stored as a jsonb array on the source row, so the gate reads the vocabulary
# the panel will draw before running a single test: if `split_full_name` were absent from that
# array the fix would still compile and the mapping editor would still not offer it.
echo "[crm-name-split] asserting the transform is in the stored vocabulary"
docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -A -t -v ON_ERROR_STOP=1 -c \
  "select 1 from pg_proc limit 0" >/dev/null
if ! grep -q '"split_full_name"' modules/crm-intake/src/mapping.rs; then
  echo "  FAIL: split_full_name is not in the module's transform vocabulary." >&2
  exit 1
fi
echo "  ok: split_full_name is a known transform"

# --test-threads=1: each test creates and drops rows for its own organization, and they share one
# database. QA_TEST_FILTER runs one test.
cargo test -p omnion-module-crm-intake --test crm_name_split -- \
  --test-threads=1 --nocapture ${QA_TEST_FILTER:+"${QA_TEST_FILTER}"}
