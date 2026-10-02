#!/usr/bin/env bash
# A fresh installation can actually be installed. (REQ-117, slice 21 defect; REQ-133 slice 18)
#
#   bash scripts/qa/run-migration-installs.sh
#
# ## The defect this gate exists for
#
# `0200_crm_intake_source_key_lookup.sql` shipped with `create index concurrently` and **no
# `-- no-transaction` directive**. sqlx's `migrate!` runs each migration inside a transaction
# unless the file's first bytes are `-- no-transaction`, so every fresh installation failed:
#
#     migration: while executing migration 200: CREATE INDEX CONCURRENTLY cannot run inside a
#     transaction block
#
# The API then restart-looped and never served a request. It was found by a browser walkthrough
# whose API had died — a failure four layers from the cause, which is exactly how this branch has
# been bitten before.
#
# ## Why the index's own gate stayed green, and why that is the reusable part
#
# `run-crm-key-lookup-index.sh` applies the migrations with
# `psql < migration.sql`, one file per invocation. **psql is not the product's runner**, it has
# no transaction wrapper, and it therefore *cannot* reproduce this failure even in principle. The
# gate measured the query plan — legitimately, and correctly — on a database that the shipped
# binary could never have reached.
#
# A gate that provisions its fixture by a different mechanism than the product proves the fixture
# is right, not that the product can build it. So this gate provisions the ONE way that matters:
#
#     the API binary boots against an empty database and migrates it itself
#
# That is literally the code path an operator runs (`apps/api/src/main.rs` → `db.migrate()` →
# `sqlx::migrate!` over `database/migrations`), so a migration this gate accepts is a migration
# `omnion-api` can apply.
#
# ## The negative control, and why it is not optional
#
# A boot-and-check gate passes on a binary that is merely *running*, and the failure mode of a
# naive version of this script is "the API started, therefore migrations applied" — which is
# green on an API that started, logged the same error, and served nothing. So:
#
#   1. the assertion is `_sqlx_migrations` — read from the DATABASE, listing the versions the
#      runner recorded, not the log, the exit code or the process being alive;
#   2. `migrate()` returning is asserted separately, because a runner that gives up silently
#      leaves a table the gate would then read;
#   3. `concurrently` and `-- no-transaction` are cross-checked as a *static* pair, so the gate
#      also fails for a migration nobody has run yet — the case where every database-based check
#      is silent.
#
# ## Its own database, never the pass's
#
# This gate drops a database. The browser pass's API connects to the pass's own database, and
# `DROP … WITH (FORCE)` would terminate its connections — the failure then lands twenty routes
# from the cause (see `run-crm-request-id.sh`, three ticks of false CRM walkthrough failures).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="${QA_DB:-omnion_qa_w8_install}"

if [ "${DB}" = "omnion_qa" ] || [ "${DB}" = "omnion_qa_w8" ]; then
  echo "  FAIL: this gate would drop the QA pass's own database ($DB)." >&2
  exit 1
fi

# Lifted byte-level out of a sibling gate rather than retyped: a hand-written URL produces
# "N failed" that is the script's configuration, not a regression, and a tool masks credentials
# in rendered output — the mask is what gets copied.
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

export OMNION_DATABASE_URL="${PGPASS_PREFIX}/${DB}"
# `development`, not `test`: `Config::from_env` accepts exactly two values and rejects the
# process before `db.migrate()` is ever reached — which this gate's first run did, and reported
# as "the API never recorded a migration". A gate that cannot start the product cannot judge the
# product, and the honest fix is the configuration rather than the assertion.
export OMNION_ENV="${OMNION_ENV:-development}"
# `OMNION_DATABASE_URL`, NOT `DATABASE_URL`. Config reads the prefixed name only
# (`config.rs`: `read("OMNION_DATABASE_URL").unwrap_or(DEFAULT_DATABASE_URL)`), so the unprefixed
# spelling is silently ignored and the process falls back to `…/omnion` — the *installation*
# database. This gate's first run set `DATABASE_URL`, said so in this comment, and then booted the
# API against the wrong database, where migration 19 was already applied from another writer's
# worktree and the install failed for an unrelated reason. Worse than a false pass: a migration
# gate that points at the installation's own database is a gate that can mutate it.
export OMNION_WORKFLOWS_RUNNER=false
# The API binds a port; the walkthrough stack owns 18087, so this instance takes a free one and
# is only ever reached over loopback for the boot-and-migrate check.
PORT="${QA_INSTALL_PORT:-18987}"

PASS=0
FAIL=0
ok()   { echo "  ok: $1"; PASS=$((PASS + 1)); }
bad()  { echo "  FAIL: $1" >&2; FAIL=$((FAIL + 1)); }

echo "[migration-installs] rebuilding ${DB}"
docker exec "$CONTAINER" psql -U omnion -d postgres -q \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null

# The binary. `cargo build` rather than a prebuilt path: a stale binary would migrate a database
# with an older migration set and report green for a defect the shipped code still carries.
echo "[migration-installs] building omnion-api"
export PATH="$HOME/.cargo/bin:$PATH"
# This branch's sibling gates build into `/dev/shm/w8-target` (`QA_CARGO_TARGET_DIR`), and the
# reason is the one this gate's first two runs walked into: `/mnt/apopic` is 94%+ full, so a build
# into `target/` can fail for want of space — and `cargo … --quiet >/dev/null` swallows it, leaving
# the gate to boot whatever stale binary happened to be on disk. It booted a binary from 02:47,
# hours after the fix, and reported the fix as a defect. Three separate mistakes stacked on the
# same line, so all three are closed here:
#   * build into the branch's real target dir, like every sibling gate;
#   * do not discard cargo's output — a build that fails must stop the gate;
#   * touch the crate that owns `sqlx::migrate!`, because the SQL is embedded at compile time and
#     cargo does not fingerprint `database/migrations/*.sql` (see below).
export CARGO_TARGET_DIR="${QA_CARGO_TARGET_DIR:-/dev/shm/w8-target}"
export CARGO_INCREMENTAL=0
touch crates/core/src/db.rs
if ! bash scripts/qa/cargo-slot.sh cargo build -p omnion-api 2>&1 | tail -3; then
  bad "cargo build -p omnion-api failed; the gate cannot judge the product it did not build"
fi
API_BIN="${CARGO_TARGET_DIR}/debug/omnion-api"
if [ ! -x "$API_BIN" ]; then
  echo "  FAIL: $API_BIN is not executable after a build." >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# Static rule: no migration EXECUTES a concurrent build, and a `-- no-transaction`
# directive — if one is ever written — is the file's first bytes.
#
# Comments are stripped before the search: 0200's note discusses `create index
# concurrently` at length, and a check that cannot tell prose from a statement is a
# check that will eventually be deleted rather than fixed.
# ---------------------------------------------------------------------------
python3 - <<'PY'
import pathlib, re, sys

violations, declarations, concurrent = [], [], []
for path in sorted(pathlib.Path("database/migrations").glob("*.sql")):
    sql = path.read_text(encoding="utf-8")
    code = "\n".join(l for l in sql.splitlines() if not l.strip().startswith("--"))
    lowered = code.lower()
    if "create index concurrently" in lowered or "create unique index concurrently" in lowered:
        concurrent.append(path.name)
        violations.append(
            f"{path.name} executes a concurrent build, which sqlx refuses inside the runner's "
            f"transaction — a fresh install cannot boot. Use a plain `create index`: a migration "
            f"runs before the platform serves traffic, so there is no write path to protect."
        )
    if any(l.strip() == "-- no-transaction" for l in sql.splitlines()):
        declarations.append(path.name)
        if not sql.startswith("-- no-transaction"):
            violations.append(
                f"{path.name} declares `-- no-transaction` below its comment header. sqlx tests "
                f"`sql.starts_with(\"-- no-transaction\")`, so the directive is inert and the "
                f"install fails exactly as if it were absent. Line 1, or nowhere."
            )

print(f"[migration-installs] {len(concurrent)} migration(s) execute CREATE INDEX CONCURRENTLY"
      f"{': ' + ', '.join(concurrent) if concurrent else ''}; "
      f"{len(declarations)} declare a transaction exemption"
      f"{': ' + ', '.join(declarations) if declarations else ''}")
if violations:
    print("  FAIL:\n    - " + "\n    - ".join(violations), file=sys.stderr)
    sys.exit(3)
PY
if [ $? -ne 0 ]; then
  bad "static concurrent-build / transaction-exemption rule"
else
  ok "no migration executes a concurrent build, and no exemption is declared inertly"
fi

# ---------------------------------------------------------------------------
# The real one: boot the product against an EMPTY database and let IT migrate.
# ---------------------------------------------------------------------------
echo "[migration-installs] booting omnion-api against an empty ${DB}"
BOOT_LOG="$(mktemp)"
trap 'rm -f "$BOOT_LOG"' EXIT
PORT="$PORT" "$API_BIN" >"$BOOT_LOG" 2>&1 &
API_PID=$!

# ---------------------------------------------------------------------------
# Which database did it actually reach?
#
# Two independent ways to be wrong here, and this gate hit the second on its first
# run: the config key is `OMNION_DATABASE_URL`, so setting `DATABASE_URL` left the
# process on its `…/omnion` default. The symptom was an install failure on
# migration 19 — a version another writer's worktree carries and this branch does
# not — which reads as a product defect and is in fact a fixture aimed at the wrong
# database.
#
# So the gate proves the target BEFORE trusting a word of the API's output: it
# reads back the database it is connected to, from the database. If the API is not
# on `${DB}` the run is void and nothing below it is reported as a product result.
# ---------------------------------------------------------------------------
CONNECTED=0
for _ in $(seq 1 45); do
  if ! kill -0 "$API_PID" 2>/dev/null; then break; fi
  LIVE="$(docker exec "$CONTAINER" psql -U omnion -d postgres -q -A -t \
    -c "select count(*) from pg_stat_activity where datname = '${DB}' and pid <> pg_backend_pid()" \
    2>/dev/null | tr -d '[:space:]' || echo 0)"
  if [ "${LIVE:-0}" -gt 0 ] 2>/dev/null; then CONNECTED=1; break; fi
  sleep 2
done

# A restart loop keeps the pid alive; a clean failure does not. Both are caught,
# by different questions, because each catches the case the other misses.
if grep -q 'cannot run inside a transaction block' "$BOOT_LOG"; then
  bad "the API's own migration runner rejected a migration (CREATE INDEX CONCURRENTLY in a transaction)"
elif grep -q 'was previously applied but is missing in the resolved migrations' "$BOOT_LOG"; then
  bad "the database holds a migration version this build does not carry — check the gate reached ${DB} and not another writer's database"
elif [ "$CONNECTED" = "1" ]; then
  ok "the API connected to ${DB} and ran its own migration runner on it"
else
  bad "the API never opened a connection to ${DB}; its log's last lines are:"
  tail -5 "$BOOT_LOG" >&2 || true
fi

# Poll the DATABASE for the migration ledger rather than sleeping a fixed time: the
# runner records each version only after it commits, and a fixed sleep is either too
# slow on a cold box or too short on a warm one. It waits for the WHOLE set, not for
# the table to appear — a runner that applied three of sixty-four and died has created
# the table just as surely as one that finished, and "the table exists" would pass it.
# How many migrations a correct install records. Counted from the files, not hardcoded, so a
# migration added after this gate was written is covered the day it lands.
EXPECTED="$(ls database/migrations/*.sql | wc -l | tr -d ' ')"
READY=0
for _ in $(seq 1 120); do
  if ! kill -0 "$API_PID" 2>/dev/null; then break; fi
  DONE_N="$(docker exec "$CONTAINER" psql -U omnion -d "$DB" -q -A -t \
    -c "select count(*) from _sqlx_migrations where success" 2>/dev/null | tr -d '[:space:]' || echo 0)"
  if [ "${DONE_N:-0}" -ge "$EXPECTED" ] 2>/dev/null; then READY=1; break; fi
  sleep 2
done

# A restart loop keeps the pid alive; a clean failure does not. Both are caught,
# by different questions, because each catches the case the other misses.
if grep -q 'cannot run inside a transaction block' "$BOOT_LOG"; then
  bad "the API's own migration runner rejected a migration (CREATE INDEX CONCURRENTLY in a transaction)"
elif [ "$READY" = "1" ]; then
  ok "the API applied the schema itself on a database that had nothing in it"
else
  bad "the API never recorded a migration; its log's last lines are:"
  tail -5 "$BOOT_LOG" >&2 || true
fi

kill "$API_PID" 2>/dev/null || true
wait "$API_PID" 2>/dev/null || true

# The versions, read back out of the database. A migration that half-ran (a
# transaction block that failed after creating a table) leaves a gap here.
APPLIED="$(docker exec "$CONTAINER" psql -U omnion -d "$DB" -q -A -t \
  -c "select count(*) from _sqlx_migrations where success" 2>/dev/null || echo 0)"
if [ "${APPLIED:-0}" -eq "$EXPECTED" ]; then
  ok "every migration is recorded applied (${APPLIED}/${EXPECTED})"
else
  bad "only ${APPLIED:-0} of $EXPECTED migrations are recorded applied — a migration failed or was skipped"
fi

# The index the boot was for exists in the installed schema, not just on disk.
IDX="$(docker exec "$CONTAINER" psql -U omnion -d "$DB" -q -A -t \
  -c "select count(*) from pg_indexes where indexname = 'crm_intake_sources_key_lookup_idx'" 2>/dev/null || echo 0)"
if [ "${IDX:-0}" = "1" ]; then
  ok "the intake key-lookup index really was built by the install (the runner executed it)"
else
  bad "crm_intake_sources_key_lookup_idx is absent from an installed database"
fi

echo "[migration-installs] ${PASS} passed, ${FAIL} failed"
[ "$FAIL" -eq 0 ]