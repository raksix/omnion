#!/usr/bin/env bash
# The delivery log's retention, against a real database (REQ-021, slice 7).
#
#   bash scripts/qa/run-notification-retention.sh
#
# ## Why this gate is its own file
#
# Two functions had **zero call sites for their whole life**: `push::prune_deliveries` and
# `push::prune_stale`. The outbox screen publishes "The log goes back 60 days" from the constant
# those two were the only readers of, so an administrator was told a floor the installation
# never enforced. That is invisible to a unit test — the functions compiled, were exported, and
# answered nothing; there was no caller to be wrong.
#
# The crate's own tests assert on the *statements' text* (the settle clock is read, there is no
# status list, the pending guard is present), because those properties cannot be made
# behavioural: the right and wrong implementations answer the same query the same way. This gate
# is the other half — it runs the sweep and reads **which rows are gone**.
#
# ## Its own database, never the pass's
#
# These gates open with `DROP DATABASE … WITH (FORCE)`, which terminates the browser pass's API
# connections; the failure then lands twenty routes from the cause. The lesson that cost
# `run-crm-request-id.sh` three ticks of false CRM walkthroughs.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

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
export DATABASE_URL="${PGPASS_PREFIX}postgres"

# The warm target dir this branch's other gates and its own build share, on purpose: a fresh one
# recompiles the whole dependency graph, and on a box where nine writers share one 32G tmpfs
# that is how a gate dies of "No space left on device" while its own code is fine.
export CARGO_TARGET_DIR="${QA_CARGO_TARGET_DIR:-/dev/shm/w8-target}"
export CARGO_INCREMENTAL=0

echo "[notification-retention] checking the schema the gate's premise assumes"

# **Ask the schema, do not assume it.** The suite's central claim is that the clock moved from
# `created_at` to `settled_at`; if a later migration edited the column away the suite would fail
# with "column does not exist", which reads as a product defect and is a gate defect. The same
# three lines `run-crm-key-lifecycle.sh` writes for the key columns.
COLUMNS="$(docker exec -i "${QA_PG_CONTAINER:-omnion-postgres}" psql -U omnion -d "${QA_DB:-omnion_qa_w8}" \
  -q -A -t -v ON_ERROR_STOP=1 -c \
  "select string_agg(column_name || ':' || data_type, ' ' order by column_name)
   from information_schema.columns
   where table_name = 'notification_deliveries'
     and column_name in ('settled_at', 'created_at')")"
# **Both clauses below are there because the first two versions of this check were wrong**, and
# both failures looked exactly like a product defect:
#
#   1. It matched `timestamp`, and `information_schema` reports `timestamp with time zone`, so a
#      database with the column reported it missing.
#   2. It then assumed an order. The `string_agg` is `order by column_name`, which puts
#      `created_at` first — so requiring `settled_at` first could never match, whatever the
#      schema said.
#
# **A premise check that can report a false negative is worse than no check**: it sends the next
# reader into the migration instead of into the two lines they just wrote, and it does so with
# the confidence of a gate. Each column is therefore tested on its own, by name, with no order
# assumed anywhere.
for column in settled_at created_at; do
  case "${COLUMNS}" in
    *"${column}:timestamp with time zone"*) ;;
    *)
      echo "  FAIL: notification_deliveries.${column} is missing or is not a timestamptz." >&2
      echo "        found: ${COLUMNS}" >&2
      exit 1
      ;;
  esac
done
# The loop above already proved both columns exist; there is deliberately no third pattern
# here. The version that had one required `settled_at` to sort before `created_at`, which the
# query's own `order by column_name` guarantees it never will — so the gate reported a missing
# column against a schema that had both, and the two failures above were the same mistake twice.
echo "  ok: both clocks exist, so the settle instant is representable"

# --test-threads=1: each walk creates its own organizations and reads back rows its own fixture
# wrote, and the platform-arm walk counts every row in its table — which a sibling walk running
# at the same time would change.
cargo test -p omnion-api --test notification_retention -- \
  --test-threads=1 --nocapture ${QA_TEST_FILTER:+"${QA_TEST_FILTER}"}