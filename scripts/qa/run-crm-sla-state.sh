#!/usr/bin/env bash
# CRM intake — the first-response clock's state is the module's answer, not the panel's.
#
#   QA_DB=omnion_qa_w8_slastate bash scripts/qa/run-crm-sla-state.sh
#
# ## The gap this gate measures
#
# `assignment::SlaState::of` is the module's definition of a lead's first-response clock. It
# was exported, documented and unit-tested, and it had **no production caller at all** — the
# panel derived the same four states itself, in `apps/admin/lib/crm-intake.ts`, from a
# hard-coded `AT_RISK_MINUTES = 60`. That client's own doc said *"when slice 2 lands it takes
# over this function rather than the screens, so nothing here has to change"*. Slice 2 landed,
# and nothing changed: for three ticks the two answers were different **by construction**, and
# nothing in the workspace could see it, because both halves were correct in isolation.
#
# This is the tenth variation of this branch's standing defect class and the first where the
# dead function was not merely unused but *superseded* — the seven before it shipped a correct
# rule with no caller; this one shipped a correct rule, a correct screen, and a second
# implementation of the rule between them.
#
# ## Why the fixtures never use the seeded 240-minute policy
#
# The two disagreements are both invisible at 240 minutes: a quarter of 240 is 60, which is
# exactly the client's hard-coded threshold, so the seeded default is the one policy where the
# two implementations agree. Every test here creates its own window — 15, 60 and 480 minutes —
# because agreeing on the default is not evidence of anything.
#
# ## Why the tenancy half is in this file rather than a route gate
#
# `policy_windows` takes an `organization_id` and predicates on it, and a lead's clock
# answered with another company's promise would be a silent cross-tenant read rather than a
# `403` — the shape `run-crm-tenancy-http.sh` was written for, but one join further in.
#
# ## Its own database, never the pass's
#
# These gates open with `DROP DATABASE … WITH (FORCE)`, which terminates the browser pass's
# API connections; the failure then lands twenty routes from the cause, and it cost three
# ticks of false CRM walkthroughs before the guard was inherited here.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"
export PGOPTIONS="-c client_min_messages=warning"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_slastate}"

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

echo "[crm-sla-state] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# `--test-threads=1`: each test creates and drops an organization, and two of them assert on
# the *absence* of another organization's rows — which a concurrent drop would satisfy for the
# wrong reason.
echo "[crm-sla-state] crm_sla_state"
cargo test -p omnion-module-crm-intake --test crm_sla_state -- --test-threads=1

echo "[crm-sla-state] PASS"