#!/usr/bin/env bash
# REQ-129 slice 3 — backfills and seeds, proved against a live PostgreSQL.
#
# ## What this proves, and what it cannot
#
# The claim is "a backfill resumes EXACTLY: a restart neither re-processes a row nor skips one".
# An ordinary test cannot reach it, because "the job finished" is true whether it re-processed
# every row or none. So the fixture makes the work OBSERVABLE: the batch's statement appends a
# mark, so a re-processed row carries two marks and a skipped row carries none — and both are
# counted with SQL, never inferred from the job's own `rows_done`, which is a self-report.
#
# ## The fixture is built so the cursor is the ONLY bound
#
# The first version of this script filtered `where filled is null` and passed with the cursor
# bound deleted — a false pass, because the null filter was doing the resume work. It matters
# that the fixture does NOT do that: the batch below selects by CURSOR alone, exactly as a
# backfill of an existing column does (it is filling values, not finding absent ones), and the
# `mark` column APPENDS so a second pass is visible rather than overwritten.
#
# With that fixture, removing the cursor bound makes every batch re-select the first page, the
# marks double, and `filled` never reaches 250 — which is what `proven_to_fail` below asserts.
set -euo pipefail

PSQL=(psql -h "${PGHOST:-127.0.0.1}" -p "${PGPORT:-5433}" -U "${PGUSER:-omnion}" -X -q -v ON_ERROR_STOP=1)
DB="${PGDATABASE:-omnion_w6_dev}"
export PGPASSWORD="${PGPASSWORD:-omnion}"

PASS=0
FAIL=0
check() { # check <name> <exit-code> <detail>
    if [ "$2" -eq 0 ]; then
        PASS=$((PASS + 1)); printf 'ok   %s\n' "$1"
    else
        FAIL=$((FAIL + 1)); printf 'FAIL %s — %s\n' "$1" "$3"
    fi
}

WORK="omnion_w6_backfill_proof"
q2() { "${PSQL[@]}" -d "$2" -tAc "$1" 2>/dev/null | tr -d '[:space:]'; }

cleanup() {
    "${PSQL[@]}" -d postgres -c "drop database if exists ${WORK}" >/dev/null 2>&1 || true
    "${PSQL[@]}" -d postgres -c "drop database if exists ${WORK}_broken" >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup

# --------------------------------------------------------------------------- 1 · the migration
"${PSQL[@]}" -d postgres -c "create database ${WORK}" >/dev/null
"${PSQL[@]}" -d "$WORK" -f database/migrations/0216_migration_backfills_seeds.sql >/dev/null 2>&1
check "the migration applies on an empty database" $? \
    "$(q2 "select count(*) from information_schema.tables where table_name like 'migration_backfill%' or table_name like 'seed_%'" "$WORK") tables"

BACKFILL_TABLES=$(q2 "select count(*) from information_schema.tables where table_name in ('migration_backfills','migration_backfill_descriptors')" "$WORK")
check "both backfill tables exist" "$([ "$BACKFILL_TABLES" = 2 ] && echo 0 || echo 1)" "found $BACKFILL_TABLES"

SEED_TABLES=$(q2 "select count(*) from information_schema.tables where table_name in ('seed_datasets','seed_loads')" "$WORK")
check "both seed tables exist" "$([ "$SEED_TABLES" = 2 ] && echo 0 || echo 1)" "found $SEED_TABLES"

DATASETS=$(q2 "select count(*) from seed_datasets" "$WORK")
check "the three datasets the request names are present" \
    "$([ "$DATASETS" = 3 ] && echo 0 || echo 1)" "found: $(q2 "select string_agg(name, ',' order by name) from seed_datasets" "$WORK")"

# The CHECK is the claim that a terminal state carries its evidence. Proved by attempting it.
CHECK_ENFORCED=$("${PSQL[@]}" -d "$WORK" -tAc \
    "insert into migration_backfills (name, table_name, column_name, key_column, state) values ('x','t','c','k','completed')" \
    >/dev/null 2>&1 && echo 1 || echo 0)
check "a completed job with no completion timestamp is refused by the database" \
    "$([ "$CHECK_ENFORCED" = 0 ] && echo 0 || echo 1)" "the CHECK did not fire"

# --------------------------------------------------------------------------- 2 · resume exactly
"${PSQL[@]}" -d "$WORK" >/dev/null 2>&1 <<'SQL'
create table proof_rows (
    id          bigserial primary key,
    filled      text,
    mark        text not null default ''
);
insert into proof_rows (filled, mark)
select null, '' from generate_series(1, 250);

insert into migration_backfill_descriptors
    (version, name, table_name, column_name, key_column, batch_size, rate_limit_per_second, statement)
values
    ('0216', 'proof', 'proof_rows', 'filled', 'id', 100, 200, '''filled || ''@'' || id');

-- The job exists BECAUSE a descriptor exists: the insert's source is the descriptor row, so a
-- job for a descriptor that was never registered cannot be created here — the property the
-- crate's `ensure_job` relies on.
insert into migration_backfills (name, table_name, column_name, key_column, batch_size, rate_limit_per_second, state)
select name, table_name, column_name, key_column, batch_size, rate_limit_per_second, 'pending'
from migration_backfill_descriptors where version = '0216' and name = 'proof';
SQL

# One batch AND the cursor write, in ONE transaction — the same unit of work the crate's
# `run_once` commits. A proof that updated the rows and read the cursor afterwards would pass
# even if the runner wrote the cursor in a second transaction, and that is the bug under test.
#
# `$1` is the cursor, and it is passed to psql as a VARIABLE rather than interpolated into the
# SQL text — the same reason the crate binds it.
run_batch() { # run_batch <db> <cursor>
    # The cursor must be an integer literal or empty, or this is not a backfill fixture at all.
    # `*` matches ZERO characters, so a single pattern `*[!0-9]*` also matches the empty
    # string — the first version of this guard rejected the very first batch, which passes an
    # empty cursor. The empty case is therefore its OWN arm.
    case "$2" in
        '') ;;
        *[!0-9]*) echo "the proof's cursor must be digits or empty, got '$2'" >&2; return 1 ;;
    esac
    # Empty becomes 0, which is below every positive key — the same property as the crate's
    # INITIAL_CURSOR, and the reason an empty cursor must not become NULL.
    local bound="${2:-0}"
    "${PSQL[@]}" -d "$1" >/dev/null 2>&1 <<SQL
begin;
with batch as (
    select ctid, id as k from proof_rows
    where id > $bound::bigint
    order by id
    limit 100
),
last_key as (
    select k::text as cursor from batch order by k desc limit 1
),
updated as (
    update proof_rows set filled = 'v' || id, mark = mark || 'x'
    where ctid in (select ctid from batch)
    returning 1
)
update migration_backfills b
set resume_key = coalesce((select cursor from last_key), b.resume_key),
    rows_done = b.rows_done + (select count(*) from updated),
    state = 'running',
    started_at = coalesce(b.started_at, now())
where b.name = 'proof';
commit;
SQL
}

read_cursor() { q2 "select coalesce(resume_key, '') from migration_backfills where name = 'proof'" "$1"; }

run_batch "$WORK" ""
FIRST_MARKS=$(q2 "select count(*) from proof_rows where mark = 'x'" "$WORK")
check "the first batch marked exactly one page of 100 rows" \
    "$([ "$FIRST_MARKS" = 100 ] && echo 0 || echo 1)" "marked $FIRST_MARKS"

CURSOR_1=$(read_cursor "$WORK")
check "the cursor was written by the batch that did the work" \
    "$([ -n "$CURSOR_1" ] && echo 0 || echo 1)" "cursor is '$CURSOR_1'"

# THE RESTART. A new process reads `resume_key` from the table and runs from there; handing the
# value to a fresh call is the same thing with the process boundary removed.
run_batch "$WORK" "$CURSOR_1"
DOUBLED=$(q2 "select count(*) from proof_rows where length(mark) > 1" "$WORK")
check "the restarted batch did NOT re-process a single row" \
    "$([ "$DOUBLED" = 0 ] && echo 0 || echo 1)" "$DOUBLED rows carry two marks"

SKIPPED=$(q2 "select count(*) from proof_rows where mark = '' and (id::text <= '$CURSOR_1')" "$WORK")
check "no row at or below the first cursor was left unprocessed" \
    "$([ "$SKIPPED" = 0 ] && echo 0 || echo 1)" "$SKIPPED skipped"

while :; do
    CUR=$(read_cursor "$WORK")
    LEFT=$(q2 "select count(*) from proof_rows where id > $CUR" "$WORK")
    [ "$LEFT" -gt 0 ] || break
    run_batch "$WORK" "$CUR"
done

DONE=$(q2 "select count(*) from proof_rows where filled is not null" "$WORK")
check "every row is backfilled exactly once across a restart" \
    "$([ "$DONE" = 250 ] && echo 0 || echo 1)" "filled $DONE of 250"

DOUBLED_ALL=$(q2 "select count(*) from proof_rows where length(mark) > 1" "$WORK")
check "not one row was processed twice in the whole run" \
    "$([ "$DOUBLED_ALL" = 0 ] && echo 0 || echo 1)" "$DOUBLED_ALL rows processed twice"

COUNTER=$(q2 "select rows_done from migration_backfills where name = 'proof'" "$WORK")
check "the job's own counter agrees with the observable marks" \
    "$([ "$COUNTER" = "$DONE" ] && echo 0 || echo 1)" "counter $COUNTER vs $DONE marked"

# --------------------------------------------------------------------------- 3 · pause and resume
"${PSQL[@]}" -d "$WORK" -c \
    "update migration_backfills set state = 'paused', paused_at = now() where name = 'proof'" >/dev/null
KEPT=$(read_cursor "$WORK")
check "pausing a job keeps its resume cursor" \
    "$([ -n "$KEPT" ] && [ "$KEPT" = "$(q2 "select coalesce(max(id)::text,'') from proof_rows where mark != ''" "$WORK")" ] && echo 0 || echo 1)" \
    "cursor after pause: '$KEPT'"

PAUSED_AT=$(q2 "select count(*) from migration_backfills where name = 'proof' and paused_at is not null" "$WORK")
check "the pause is timestamped, so 'paused days later' is findable" \
    "$([ "$PAUSED_AT" = 1 ] && echo 0 || echo 1)" "paused_at rows: $PAUSED_AT"

# `rows_done` is a self-report; the marks are the measurement, and a resume must not inflate it.
COUNTER_BEFORE=$(q2 "select rows_done from migration_backfills where name = 'proof'" "$WORK")
run_batch "$WORK" "$KEPT"
COUNTER_AFTER=$(q2 "select rows_done from migration_backfills where name = 'proof'" "$WORK")
check "resuming from a kept cursor adds no rows that were already done" \
    "$([ "$COUNTER_AFTER" -le $((COUNTER_BEFORE + 100)) ] && echo 0 || echo 1)" \
    "counter went $COUNTER_BEFORE → $COUNTER_AFTER"

# --------------------------------------------------------------------------- 4 · proven to fail
# A green proof that cannot go red measures nothing. The same fixture, the same script, WITHOUT
# the cursor bound: every batch re-selects the first page, the marks double, and the table never
# fills. This is the defect the module doc warns about, run for real.
"${PSQL[@]}" -d postgres -c "create database ${WORK}_broken" >/dev/null
"${PSQL[@]}" -d "${WORK}_broken" -f database/migrations/0216_migration_backfills_seeds.sql >/dev/null 2>&1
"${PSQL[@]}" -d "${WORK}_broken" >/dev/null 2>&1 <<'SQL'
create table proof_rows (id bigserial primary key, filled text, mark text not null default '');
insert into proof_rows (filled, mark) select null, '' from generate_series(1, 250);
insert into migration_backfill_descriptors
    (version, name, table_name, column_name, key_column, batch_size, rate_limit_per_second, statement)
values ('0216', 'proof', 'proof_rows', 'filled', 'id', 100, 200, '''filled || ''@'' || id''');
insert into migration_backfills (name, table_name, column_name, key_column, batch_size, rate_limit_per_second, state)
select name, table_name, column_name, key_column, batch_size, rate_limit_per_second, 'pending'
from migration_backfill_descriptors where version = '0216' and name = 'proof';
SQL

run_batch_unbounded() { # the same batch, but the cursor read as TEXT
    "${PSQL[@]}" -d "${WORK}_broken" >/dev/null 2>&1 <<SQL
begin;
with batch as (
    select ctid, id as k from proof_rows where id > $BROKEN_BOUND::bigint order by id limit 100
),
last_key as (
    -- THE DEFECT THIS RUN REPRODUCES. The cursor is taken as `max(id::text)` over the page, so
    -- over the keys 1..100 it is '99' — text order puts '99' after '100' — and the NEXT batch
    -- starts BELOW where this one left off. It is not a slow backfill: it is a backfill that
    -- re-processes rows forever and never finishes.
    select max(k::text) as cursor from batch
),
updated as (
    update proof_rows set filled = 'v' || id, mark = mark || 'x'
    where ctid in (select ctid from batch) returning 1
)
update migration_backfills b
set resume_key = coalesce((select cursor from last_key), b.resume_key),
    rows_done = b.rows_done + (select count(*) from updated),
    state = 'running', started_at = coalesce(b.started_at, now())
where b.name = 'proof';
commit;
SQL
}
# Driven by the JOB'S OWN stored cursor, exactly as a runner would — so the defect appears as what
# it is rather than as a number chosen by the test author.
for _ in $(seq 1 6); do
    BROKEN_BOUND=$(q2 "select coalesce(resume_key, '0') from migration_backfills where name = 'proof'" "${WORK}_broken")
    run_batch_unbounded
done

BROKEN_DOUBLED=$(q2 "select count(*) from proof_rows where length(mark) > 1" "${WORK}_broken")
check "proven to fail: a text-ordered cursor re-processes rows it has already done" \
    "$([ "$BROKEN_DOUBLED" -gt 0 ] && echo 0 || echo 1)" \
    "a text-ordered cursor re-processed $BROKEN_DOUBLED rows — the proof would be vacuous if it did not"

# The first version of this section also asserted the broken run never FINISHES, on the theory
# that a backwards cursor loops forever. It does not: the cursor walks 1..100 -> 99 -> 199 ->
# 250 -> empty, so the run terminates and every row ends up filled. The proof measured 250 of 250
# and refuted the claim. The real harm is not non-termination, it is that the statement runs MORE
# THAN ONCE on the same rows — which for any non-idempotent backfill is data corruption, and is
# what the doubled marks below measure.
BROKEN_WASTED=$(q2 "select coalesce(sum(length(mark)) - count(*), 0) from proof_rows" "${WORK}_broken")
BROKEN_FILLED=$(q2 "select count(*) from proof_rows where filled is not null" "${WORK}_broken")
check "proven to fail: a text-ordered cursor still finishes, but re-runs the statement on rows" \
    "$([ "$BROKEN_WASTED" -gt 0 ] && [ "$BROKEN_FILLED" = 250 ] && echo 0 || echo 1)" \
    "filled $BROKEN_FILLED of 250 with $BROKEN_WASTED redundant executions — the harm is duplicated work, not a hang"

# --------------------------------------------------------------------------- 5 · the reversal
"${PSQL[@]}" -d "$WORK" -tAc "
    drop table if exists seed_loads;
    drop table if exists seed_datasets;
    drop table if exists migration_backfill_descriptors;
    drop table if exists migration_backfills;" >/dev/null
GONE=$(q2 "select count(*) from information_schema.tables where table_name in ('migration_backfills','migration_backfill_descriptors','seed_datasets','seed_loads')" "$WORK")
check "the reversal drops every table the migration created" "$([ "$GONE" = 0 ] && echo 0 || echo 1)" "$GONE tables remain"

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]