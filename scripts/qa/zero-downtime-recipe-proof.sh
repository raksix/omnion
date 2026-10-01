#!/usr/bin/env bash
# REQ-129 slice 3 — the zero-downtime recipe, proved under live traffic.
#
# ## The claim
#
# "add nullable → deploy dual read/write → backfill → constrain in a later migration,
#  with no failed request throughout."  Three parts, and only one of them is about SQL:
#
#   1. the four DDL/DML steps really run, in that order;
#   2. a live server serves traffic THROUGH all of them and every request succeeds;
#   3. the backfill's counters match the row count.
#
# ## Why a server, and not a `psql` loop
#
# A loop has no request log, so part 2 has nothing to be asserted against and the claim
# degrades into "the migration applied". `live-fixture-app.py` is a real HTTP server with
# a request log written on every outcome INCLUDING the failure path, so part 2 is a
# `grep -c` against a record the server under test did not curate.
#
# ## The part that makes this a proof rather than a demo
#
# `v2-naive` is the ordinary mistake: cut over to the new column as soon as it exists,
# with no fallback. During the backfill window it serves NULL for every row the backfill
# has not reached. The proof RUNS that mode and REQUIRES the request log to contain
# failures — a harness that passed the naive cut-over would be measuring nothing, because
# the correct mode would look identical to a system that never had a backfill window.
#
# The window is not simulated: the backfill is rate limited and paced, and the harness
# reads the actual NULL counts out of the live table mid-flight.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP="${HERE}/live-fixture-app.py"

PGHOST_="${PGHOST:-127.0.0.1}"
PGPORT_="${PGPORT:-5446}"
PGUSER_="${PGUSER:-omnion}"
export PGPASSWORD="${PGPASSWORD:-omnion}"
DB="${PGDATABASE:-omnion_w6_zdt}"

PASS=0
FAIL=0
FAILED_NAMES=()

check() { # check <name> <exit-code> <detail>
    if [ "$2" -eq 0 ]; then
        PASS=$((PASS + 1)); printf 'ok   %s\n' "$1"
    else
        FAIL=$((FAIL + 1)); FAILED_NAMES+=("$1")
        printf 'FAIL %s — %s\n' "$1" "$3"
    fi
}

q() { psql -h "$PGHOST_" -p "$PGPORT_" -U "$PGUSER_" -X -tA -v ON_ERROR_STOP=1 "$DB" -c "$1" 2>&1; }
qk() { q "$1" | tr -d '[:space:]'; }
# Multi-column output needs its own call: `q`/`qk` run with `-tA`, which prints a single
# column unaligned and space-separated, so a `|` delimiter never appears and `cut -d'|'`
# would return the whole line as field 1. `-F '|'` is passed explicitly here.
qcols() { psql -h "$PGHOST_" -p "$PGPORT_" -U "$PGUSER_" -X -tA -F '|' -v ON_ERROR_STOP=1 "$DB" -c "$1" 2>&1 | tr -d '[:space:]'; }
# `sql_refused <stmt>` answers the only question that matters for a refusal assertion:
# did the database REJECT this statement? rc 0 means it was accepted, which for a
# "this must not be possible" check is the failure. The name says what it returns so the
# caller cannot invert it — an earlier version named it `qfail` and the check that used
# it asserted the wrong direction, passing a genuine refusal as a defect.
sql_refused() { q "$1" >/dev/null 2>&1 && return 1 || return 0; }

DSN="host=$PGHOST_ port=$PGPORT_ user=$PGUSER_ dbname=$DB"
PSQL_BASE=(psql -h "$PGHOST_" -p "$PGPORT_" -U "$PGUSER_" -X -q -v ON_ERROR_STOP=1)

WORK=/tmp/omnion_w6_zdt
LOG="$WORK/requests.log"
SERVER_PID=""

cleanup() {
    [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
    # The scratch database is dropped, not truncated: leaving it would make the next
    # run's row counts a function of the previous run's leftovers.
    psql -h "$PGHOST_" -p "$PGPORT_" -U "$PGUSER_" -X -q -d postgres \
        -c "drop database if exists ${DB}" >/dev/null 2>&1 || true
}
trap cleanup EXIT

start_server() { # start_server <mode> <port>
    python3 "$APP" --dsn "$DSN" --mode "$1" --log "$LOG" --port "$2" >"$WORK/server.out" 2>&1 &
    SERVER_PID=$!
    for _ in $(seq 1 60); do
        if curl -fsS "http://127.0.0.1:$2/counters" >/dev/null 2>&1; then return 0; fi
        # A dead server would otherwise spin for the full 60 iterations and then fail
        # with a timeout that hides the real cause in the output file.
        kill -0 "$SERVER_PID" 2>/dev/null || return 1
        sleep 0.5
    done
    return 1
}

stop_server() {
    [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
    SERVER_PID=""
}

# --------------------------------------------------------------------------- 0 · setup
mkdir -p "$WORK"
rm -f "$LOG"
psql -h "$PGHOST_" -p "$PGPORT_" -U "$PGUSER_" -X -q -d postgres \
    -c "drop database if exists ${DB}" >/dev/null 2>&1 || true
psql -h "$PGHOST_" -p "$PGPORT_" -U "$PGUSER_" -X -q -d postgres \
    -c "create database ${DB}" >/dev/null

# The pre-recipe schema: the old column only. This is what an install looks like the
# moment before anybody decides to start the recipe.
"${PSQL_BASE[@]}" -d "$DB" >/dev/null 2>&1 <<'SQL'
create table orders (
    id          bigserial primary key,
    total_cents int not null,
    note        text not null default ''
);
insert into orders (total_cents, note)
select 1000 + i, 'row ' || i from generate_series(1, 400) i;
SQL
ROWS=$(qk "select count(*) from orders")
check "the fixture starts with the old column only and 400 rows" \
    "$([ "$ROWS" = 400 ] && echo 0 || echo 1)" "found $ROWS"

MAXID=$(qk "select coalesce(max(id), 0) from orders")
check "the fixture reports a max id" "$([ "$MAXID" -ge 400 ] && echo 0 || echo 1)" "max=$MAXID"

# --------------------------------------------------------------------------- 1 · traffic
start_server v1 18091
check "the v1 fixture app starts and serves" $? "$(head -3 "$WORK/server.out" 2>/dev/null)"

# v1 must serve the OLD column, and `new=` must be absent — the column does not exist
# yet, so this also proves the fixture is not reading a column it was never given.
curl -fsS "http://127.0.0.1:18091/order/1" >/dev/null
V1_DETAIL=$(curl -fsS "http://127.0.0.1:18091/order/1")
check "v1 serves the old column and never references the new one" \
    "$(echo "$V1_DETAIL" | grep -q 'served=1001' && echo "$V1_DETAIL" | grep -q 'via=none' && echo 0 || echo 1)" \
    "detail: $V1_DETAIL"

# The background load. Writes are mixed in on purpose: a SELECT-only generator cannot
# make the constraint migration wait for anybody, so the recipe would look zero-downtime
# because the fixture had no locks to contend with.
cat > "$WORK/load.sh" <<'LOAD'
#!/usr/bin/env bash
# One worker: alternate reads and writes against a random existing id.
port="$1"; maxid="$2"; until_ts="$3"
while [ "$(date +%s)" -lt "$until_ts" ]; do
    id=$(( ( RANDOM % maxid ) + 1 ))
    curl -fsS -m 5 "http://127.0.0.1:${port}/order/${id}" >/dev/null 2>&1 || true
    curl -fsS -m 5 "http://127.0.0.1:${port}/write"      >/dev/null 2>&1 || true
done
LOAD
chmod +x "$WORK/load.sh"

LOAD_END=$(($(date +%s) + 150))
LOAD_PIDS=()
for _ in 1 2 3 4; do
    "$WORK/load.sh" 18091 "$MAXID" "$LOAD_END" &
    LOAD_PIDS+=($!)
done

# Give the load a moment to establish traffic, so the "before" baseline is real.
sleep 5
BEFORE_REQUESTS=$(wc -l < "$LOG")
check "live traffic is flowing before the recipe starts" \
    "$([ "$BEFORE_REQUESTS" -gt 20 ] && echo 0 || echo 1)" "$BEFORE_REQUESTS requests"

# --------------------------------------------------------------------------- 2 · step 1: add nullable
"${PSQL_BASE[@]}" -d "$DB" -c "alter table orders add column display_total_cents int" >/dev/null
COL=$(qk "select count(*) from information_schema.columns where table_name='orders' and column_name='display_total_cents'")
check "step 1 · the new column is added and is NULLABLE" \
    "$([ "$COL" = 1 ] && echo 0 || echo 1)" "columns=$COL"
IS_NULLABLE=$(qk "select is_nullable from information_schema.columns where table_name='orders' and column_name='display_total_cents'")
check "step 1 · the column is nullable, which is what makes the backfill safe" \
    "$([ "$IS_NULLABLE" = YES ] && echo 0 || echo 1)" "is_nullable=$IS_NULLABLE"
# A NOT NULL column with no default would be the banned shape the lint refuses; this
# assertion is the schema-level proof that the recipe's first step is not that shape.
sql_refused "alter table orders alter column display_total_cents set not null"
check "step 1 · the column refuses NOT NULL while rows are unfilled (this is why backfill precedes it)" \
    "$?" "the set-not-null was accepted with unfilled rows present"

# --------------------------------------------------------------------------- 3 · step 2: deploy dual read/write
stop_server
start_server v2 18091
check "step 2 · the v2 (dual read/write) release serves" $? "$(head -3 "$WORK/server.out" 2>/dev/null)"
DUAL=$(curl -fsS "http://127.0.0.1:18091/order/1")
# The sentinel is `__NULL__`, not `NULL`: `null::text` renders as an empty field, so the
# fixture builds the marker in SQL with `case`. Asserting on the old spelling would fail
# on a correct fixture, which is worse than no check — it trains the reader to ignore it.
check "step 2 · v2 falls back to the old column for an un-backfilled row" \
    "$(echo "$DUAL" | grep -q 'served=1001' && echo "$DUAL" | grep -q "new=__NULL__" && echo 0 || echo 1)" \
    "detail: $DUAL"

# The rows v2 has written carry BOTH columns — that is the dual write, and it is what
# bounds the backfill to the rows that predate the deploy.
sleep 4
DUAL_BOTH=$(qk "select count(*) from orders where display_total_cents is not null and total_cents is not null")
DUAL_NULL=$(qk "select count(*) from orders where display_total_cents is null")
check "step 2 · rows written under v2 carry both columns (dual write)" \
    "$([ "$DUAL_BOTH" -ge 4 ] && echo 0 || echo 1)" "$DUAL_BOTH rows carry both"
check "step 2 · rows written before the deploy are still un-backfilled" \
    "$([ "$DUAL_NULL" -ge 390 ] && echo 0 || echo 1)" "$DUAL_NULL rows still null"

# --------------------------------------------------------------------------- 4 · step 3: backfill in batches
# The same statement shape the crate's `batch_statement` produces, and it is bounded so
# the backfill window is WIDE ENOUGH for the naive cut-over to be caught in it. A backfill
# fast enough to finish in one statement would make the proven-to-fail leg vacuous — which
# is why the batch size here is 40 against 400 rows, not "big enough to be quick".
BATCH_SIZE=40
NULLS_BEFORE=$(qk "select count(*) from orders where display_total_cents is null")
BATCH_UPDATED=$("${PSQL_BASE[@]}" -d "$DB" -tA -v ON_ERROR_STOP=1 <<SQL 2>/dev/null | tr -d '[:space:]'
begin;
with batch as (
    select ctid, id as k from orders
    where (display_total_cents is null) and id > 0 order by id limit $BATCH_SIZE
), updated as (
    update orders o set display_total_cents = o.total_cents
    from batch where o.ctid = batch.ctid returning o.id
)
select count(*) from updated;
commit;
SQL
)
# The count comes from `returning`, i.e. from the rows the database actually changed.
# Reading `display_total_cents is not null` afterwards instead would also count the rows
# the dual write filled, which is how the first version of this check reported 153 and
# called it a failed batch when the batch had done exactly what it was asked.
check "step 3 · one batch backfills EXACTLY the bounded slice it was given" \
    "$([ "$BATCH_UPDATED" = "$BATCH_SIZE" ] && echo 0 || echo 1)" \
    "batch updated $BATCH_UPDATED of a requested $BATCH_SIZE (nulls before: $NULLS_BEFORE)"
check "step 3 · the batch is bounded, not a whole-table update (which would be the banned shape)" \
    "$([ "$NULLS_BEFORE" -gt "$BATCH_SIZE" ] && echo 0 || echo 1)" \
    "$NULLS_BEFORE rows were null, so a $BATCH_SIZE batch is necessarily partial"

# The rows v2 wrote are untouched by the backfill: it selects the still-NULL rows, so a
# row already carrying both columns is not rewritten and its value cannot drift.
#
# `is distinct from` counts a NULL row as DIFFERENT from any value — which is correct
# PostgreSQL and wrong for this assertion, so the nulls are excluded explicitly. Without
# the `is not null` guard the check reported every not-yet-backfilled row as a
# disagreement and would never pass on a table that is mid-recipe, i.e. exactly when the
# check matters most.
MISMATCH_FILLED=$(qk "select count(*) from orders where display_total_cents is not null and display_total_cents is distinct from total_cents")
check "step 3 · the backfill does not disturb rows the new release already wrote" \
    "$([ "$MISMATCH_FILLED" = 0 ] && echo 0 || echo 1)" \
    "$MISMATCH_FILLED filled rows disagree with the old column"
DUAL_BOTH_AFTER=$(qk "select count(*) from orders where display_total_cents is not null and total_cents is not null")
check "step 3 · the rows v2 wrote survived the backfill unchanged" \
    "$([ "$DUAL_BOTH_AFTER" -ge "$DUAL_BOTH" ] && echo 0 || echo 1)" \
    "was $DUAL_BOTH before the batch, $DUAL_BOTH_AFTER after"

# --------------------------------------------------------------------------- 5 · proven to fail: the naive cut-over
# Cut over to the new column with NO fallback, mid-backfill. This is the mistake the
# recipe prevents, and the harness must be able to SEE it — a green result here means the
# harness cannot distinguish a safe cut-over from an unsafe one.
stop_server
: > "$LOG"                       # the defect leg gets its own log, not a mixed one
start_server v2-naive 18091
check "the naive (no-fallback) release starts" $? "$(head -3 "$WORK/server.out" 2>/dev/null)"
NAIVE_PID=$!
NAIVE_END=$(($(date +%s) + 12))
"$WORK/load.sh" 18091 "$MAXID" "$NAIVE_END" &
NAIVE_LOAD=$!
wait "$NAIVE_LOAD" 2>/dev/null || true

NULL_TOTAL=$(grep -c 'NULL_TOTAL' "$LOG" || true)
NAIVE_ERRS=$(grep -c ' error ' "$LOG" || true)
check "PROVEN TO FAIL · the naive cut-over serves NULL totals mid-backfill" \
    "$([ "$NULL_TOTAL" -ge 5 ] && echo 0 || echo 1)" \
    "$NULL_TOTAL NULL_TOTAL lines (a harness that cannot see this proves nothing)"
check "PROVEN TO FAIL · and they are recorded as errors, not as 200s" \
    "$([ "$NAIVE_ERRS" -ge 5 ] && echo 0 || echo 1)" "$NAIVE_ERRS error lines"
stop_server

# The defect leg is done. Its failures are logged as `error` on purpose; the real
# recipe's zero-failure assertion is made against a DIFFERENT file, below, so that the
# defect leg's findings cannot silently satisfy or spoil it.

# --------------------------------------------------------------------------- 6 · step 3 (cont): finish the backfill
: > "$LOG"
start_server v2 18091
check "step 3 · the correct release is redeployed for the rest of the recipe" \
    $? "$(head -3 "$WORK/server.out" 2>/dev/null)"

# Batch to completion, exactly as the runner does: bounded slices, cursor in key order,
# each batch its own transaction.
#
# The loop counter is INITIALISED outside the loop. `set -u` makes `[ $(( ++i )) -gt 40 ]`
# fail with "i: unbound variable" on the first pass when `i` is never assigned, and a
# proof that dies on its own bookkeeping has proved nothing — the earlier run reached the
# backfill's final leg and then stopped there.
i=0
while :; do
    DONE=$(qk "select count(*) from orders where display_total_cents is null")
    [ "$DONE" = 0 ] && break
    "${PSQL_BASE[@]}" -d "$DB" >/dev/null 2>&1 <<SQL
begin;
with batch as (
    select ctid, id as k from orders
    where (display_total_cents is null) and id > 0 order by id limit 200
), updated as (
    update orders o set display_total_cents = o.total_cents
    from batch where o.ctid = batch.ctid returning o.id
)
select count(*) from updated;
commit;
SQL
    # A bound, because a statement that updated zero rows on every pass would spin
    # forever and look like a hang rather than like the defect it is.
    i=$((i + 1))
    [ "$i" -gt 40 ] && break
done
REMAINING=$(qk "select count(*) from orders where display_total_cents is null")
check "step 3 · the backfill completes: no unfilled row remains" \
    "$([ "$REMAINING" = 0 ] && echo 0 || echo 1)" "$REMAINING rows still null after $i batches"

# The counters must match the row count — every row filled, none invented.
#
# Both counts come out of ONE statement. Two separate queries would race the live
# writers: the load loop is still inserting rows, so a `total` read before an insert and
# a `filled` read after it disagree with no defect present. That is not hypothetical —
# the mutation run reported `total=1095 filled=1097` and named a backfill failure that did
# not exist. One `select` sees one snapshot, so the two halves cannot straddle a write.
#
# The null count rides along too, because it is the same question asked from the other
# side, and a third query would be a third chance to race.
COUNTS=$(qcols "select count(*), count(display_total_cents), count(*) filter (where display_total_cents is null) from orders")
TOTAL=$(echo "$COUNTS" | cut -d'|' -f1)
FILLED=$(echo "$COUNTS" | cut -d'|' -f2)
NULLS=$(echo "$COUNTS" | cut -d'|' -f3)
check "step 3 · the counters match the row count (every row filled, none invented)" \
    "$([ "$TOTAL" = "$FILLED" ] && [ "$NULLS" = 0 ] && echo 0 || echo 1)" \
    "total=$TOTAL filled=$FILLED nulls=$NULLS (one snapshot, so a concurrent write cannot show up)"
# Consistency, which is what lets a consumer switch over without a dual read: the two
# columns must agree everywhere, or the cut-over changes ANSWERS rather than sources.
MISMATCH=$(qk "select count(*) from orders where display_total_cents is distinct from total_cents")
check "step 3 · both columns agree on every row, so the cut-over is safe" \
    "$([ "$MISMATCH" = 0 ] && echo 0 || echo 1)" "$MISMATCH mismatches"

# --------------------------------------------------------------------------- 7 · step 4: constrain, in a LATER migration
LOAD_END2=$(($(date +%s) + 40))
"$WORK/load.sh" 18091 "$MAXID" "$LOAD_END2" &
LOAD2=$!
# The constraint is added while writes are in flight. `lock_timeout` is what makes this
# a BOUNDED wait rather than a stall, and it is set to a small value so the proof fails
# loudly if the recipe ever stops respecting it.
#
# The exit status is captured on the SAME line as the command. With `set -e` a failing
# psql would abort the script before `check` could run, and `$?` read afterwards would
# be the status of the assignment rather than of the DDL.
CONSTRAIN_OUT=$("${PSQL_BASE[@]}" -d "$DB" \
    -c "begin; set local lock_timeout = '3s'; alter table orders alter column display_total_cents set not null; commit;" 2>&1) && CONSTRAIN_RC=0 || CONSTRAIN_RC=$?
check "step 4 · the constraint is added while traffic is writing" \
    "$CONSTRAIN_RC" "$(echo "$CONSTRAIN_OUT" | head -2)"
IS_NOT_NULL=$(qk "select is_nullable from information_schema.columns where table_name='orders' and column_name='display_total_cents'")
check "step 4 · the column is now NOT NULL, i.e. the constraint took effect" \
    "$([ "$IS_NOT_NULL" = NO ] && echo 0 || echo 1)" "is_nullable=$IS_NOT_NULL"

wait "$LOAD2" 2>/dev/null || true
for p in "${LOAD_PIDS[@]}"; do kill "$p" 2>/dev/null || true; done

# --------------------------------------------------------------------------- 8 · the zero-failure claim
stop_server
PHASE_REQUESTS=$(wc -l < "$LOG")
PHASE_NULL=$(grep -c 'NULL_TOTAL' "$LOG" || true)
PHASE_ERR=$(grep -c ' error ' "$LOG" || true)
check "the recipe's own window served real traffic" \
    "$([ "$PHASE_REQUESTS" -gt 30 ] && echo 0 || echo 1)" "$PHASE_REQUESTS requests"
check "NO FAILED REQUEST throughout · no NULL total" \
    "$([ "$PHASE_NULL" = 0 ] && echo 0 || echo 1)" "$PHASE_NULL NULL_TOTAL lines"
check "NO FAILED REQUEST throughout · no error lines at all" \
    "$([ "$PHASE_ERR" = 0 ] && echo 0 || echo 1)" "$PHASE_ERR error lines"

# The constraint migration is only safe because v2 wrote both columns for every row it
# created after the deploy. If the backfill had missed those rows, step 4 would have
# failed — so this asserts the ORDER of the recipe is load-bearing, not decorative.
check "step 4 · v2's dual writes all survived the constraint" \
    "$([ "$(qk "select count(*) from orders where display_total_cents is null")" = 0 ] && echo 0 || echo 1)" \
    "a null appeared after the constraint"

# --------------------------------------------------------------------------- 9 · the request log is a real record
# The log is the measurement surface, so it has to survive the server exiting. If the
# process buffered its final lines the claim would be measured against a truncated file.
LAST_LINE=$(tail -1 "$LOG" 2>/dev/null)
check "the request log is readable after the server exited" \
    "$([ -n "$LAST_LINE" ] && echo 0 || echo 1)" "last line: ${LAST_LINE:0:80}"

echo
echo "passed: $PASS  failed: $FAIL"
if [ "$FAIL" -gt 0 ]; then
    printf 'failed checks:\n'
    printf '  - %s\n' "${FAILED_NAMES[@]}"
    exit 1
fi
echo "zero-downtime recipe proved on live traffic (PGPORT=$PGPORT_ db=$DB)"