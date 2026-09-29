#!/usr/bin/env bash
# CRM intake slice 2 — assignment rules, the round-robin claim, SLA policies and the
# breach sweep, against a real database.
#
#   QA_DB=omnion_qa_w8 bash scripts/qa/run-crm-assignment.sh
#
# The unit tests in `modules/crm-intake/src/assignment.rs` prove the *arithmetic*: which rule
# wins, that ten leads across a three-person pool never repeat a person twice in a row, and
# that a Friday-evening lead is due Monday. They cannot prove the *transaction* — that two
# concurrent claims do not both read cursor n — because that needs two sessions on one row.
# This gate is that proof: the claims below run through the same `claim_assignment` the
# capture path calls, and the cursor is read back between steps.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8}"
export DATABASE_URL="postgres://omnion:omnion@127.0.0.1:5433/${DB}"
export CARGO_TARGET_DIR="${QA_CARGO_TARGET_DIR:-/dev/shm/w8-target}"
export CARGO_INCREMENTAL=0

psql_qa() {
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -t -A -v ON_ERROR_STOP=1 "$@"
}

echo "[crm-assignment] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# Each test creates and drops its own organization: cargo runs a test binary's tests
# concurrently, so shared fixtures mean each test deletes the others' rows mid-run. The
# per-tenant isolation is the same isolation the product's tenancy is built on, so the gate
# doubles as a proof that two organizations in one database do not interfere.
cargo test -p omnion-module-crm-intake --test crm_assignment -- --nocapture
