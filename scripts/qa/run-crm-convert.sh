#!/usr/bin/env bash
# CRM intake slice 3 — conversion, the flow's module-absence degradation and the retention
# sweep, against a real database.
#
#   QA_DB=omnion_qa_w8 bash scripts/qa/run-crm-convert.sh
#
# Slice 2's gate (`run-crm-assignment.sh`) exists because a read-then-write cursor passes
# every sequential test and fails only under concurrency. This one exists for the opposite
# reason: **the interesting branch here is an absence.** `crm_contacts` and `crm_deals` are
# REQ-051's and are not on this branch, so the degradation path — create the contact, refuse
# the deal, record why — is the *default* behaviour of a conversion on this checkout, and no
# unit test can observe it. That is why one test asserts the tables are absent before
# asserting anything else: if a future merge brings REQ-051 in, this gate fails with a
# message that says what to do instead of quietly passing for the wrong reason.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8}"
export DATABASE_URL="postgres://omnion:omnion@127.0.0.1:5433/${DB}"
export CARGO_TARGET_DIR="${QA_CARGO_TARGET_DIR:-/dev/shm/w8-target}"
export CARGO_INCREMENTAL=0

echo "[crm-convert] applying migrations to ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null
for f in $(ls database/migrations/*.sql | sort); do
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -q -v ON_ERROR_STOP=1 <"$f" >/dev/null
done

# One organization per test, for the same reason slice 2's gate says it: cargo runs the
# tests in a binary concurrently, so shared fixtures mean each test deletes the others' rows
# mid-run. The present-path test additionally creates REQ-051's five tables inside a
# transaction it rolls back, so the absent-path test in the same binary still sees them
# missing — the two tests would otherwise contradict each other depending on the order
# cargo happened to pick.
cargo test -p omnion-module-crm-intake --test crm_convert -- --nocapture
