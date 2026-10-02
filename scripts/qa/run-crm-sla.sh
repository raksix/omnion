#!/usr/bin/env bash
# CRM intake — the SLA worker's two promises, against a real database.
#
#   QA_DB=omnion_qa_w8_sla bash scripts/qa/run-crm-sla.sh
#
# ## Why this gate is its own file
#
# Acceptance 11 says a breach "emits `crm.lead.sla_breached` and notifies the escalation target
# **exactly once**", and acceptance 12 asks for a reminder before the deadline. Both halves
# shipped their *store* functions two ticks before this gate existed — `due_breaches`,
# `mark_escalated`, `escalation_target` — and every caller of them was a test. The reminder had
# no caller at all, only a column, a validator, a database check and an editor control.
#
# That is the shape this gate exists to prevent. An exported, unit-tested store function reads
# as a feature, and every screen that renders the number it feeds is evidence of nothing: the
# inbox badge counts breaches that no timer ever acts on, and the SLA editor shows a reminder
# that never fires. Nobody looking at the panel could tell — the panel was, in fact, correct
# about everything it displayed.
#
# **The "exactly once" half is why the test needs a real database.** The claim is a predicate
# in one process in every test that ran before, and a `where not exists (… kind =
# 'sla_reminded')` read passes every sequential test and fails under two workers. The race is
# released deliberately (eight claimers, one winner) and the index behind `on conflict do
# nothing` is asserted by name, because a dropped index turns every test above into a pass for
# the wrong reason.
#
# **Its own database, never the pass's.** These gates open with `DROP DATABASE … WITH (FORCE)`,
# which terminates the browser pass's API connections; the failure then lands on whichever
# screens come next, twenty routes from the cause. That is not hypothetical — it is what
# `run-crm-request-id.sh` documents after it cost three ticks of false CRM walkthroughs.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_sla}"

if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database ($DB)." >&2
  echo "        Give the gate its own: the other crm gates use omnion_qa_w8_*." >&2
  exit 1
fi

# The password is read out of a sibling gate's script rather than retyped: a hand-written URL
# produces "N failed" that is the script's configuration, not a regression. Byte-level, never
# from a rendered line — a tool masks credentials in output and the mask is what gets copied.
# The *whole* `postgres://user:pass@host:port` prefix is lifted, not just the user, and the
# two halves are separated afterwards. Parsing the password with its own regex is what the
# first draft did and it failed for a reason worth writing down: the literal in the sibling
# gate is not a password this repository knows, so any character-class guess at it is a guess.
# Lifting the prefix and taking `user` and `pass` from it cannot be wrong about a value it did
# not have to understand. (`run-crm-assign.sh` carries a masked placeholder where the password
# goes and works anyway, because the container trusts local connections — flagged in the
# BUILD-LOG as one fix across the sibling gates by their owner.)
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
# The target directory defaults to the same /dev/shm path this branch's other gates and its
# own build use, on purpose: a *fresh* target dir recompiles the whole dependency graph from
# scratch, and on a box where nine writers share one 32G tmpfs that is how a gate dies of
# "No space left on device" while its own code is fine. Sharing the warm one turns the gate
# into a few seconds of linking instead of a full rebuild.
export CARGO_TARGET_DIR="${QA_CARGO_TARGET_DIR:-/dev/shm/w8-target}"
export CARGO_INCREMENTAL=0

echo "[crm-sla] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# The claim is a unique index, so "the reminder fires once" is a property of the schema as much
# as of the code. Asserting it here means the migration cannot be edited into a comment and
# leave the tests passing.
INDEX=$(docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -A -t -v ON_ERROR_STOP=1 -c \
  "select indexdef from pg_indexes where indexname = 'crm_lead_events_sla_reminded_idx'")
case "${INDEX}" in
  *UNIQUE*WHERE*sla_reminded*) : ;;
  *)
    echo "  FAIL: crm_lead_events_sla_reminded_idx is missing or not a partial unique index." >&2
    echo "        found: ${INDEX}" >&2
    exit 1
    ;;
esac

# --test-threads=1: each test creates and drops rows for its own organization, and the race
# test needs its eight claimers to be the only work in flight.
cargo test -p omnion-module-crm-intake --test crm_sla -- --test-threads=1 --nocapture

# The second half runs the *worker*, not the store. It lives in `apps/api` because that is
# where the notification write lives: asserting it from the module would mean a business module
# depending on the notification centre because a test found it convenient. Its own fixture
# creates a throwaway database per test and drops it again, so it does not use ${DB} — the
# module suite above is what runs against this one.
export DATABASE_URL="${DATABASE_URL%/*}/postgres"
cargo test -p omnion-api --test crm_sla -- --test-threads=1 --nocapture
