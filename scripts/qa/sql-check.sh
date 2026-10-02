#!/usr/bin/env bash
# Omnion · scripts/qa/sql-check.sh — apply every migration to a scratch database.
#
# Why this exists: a migration's syntax error is found by the test suite, and the test suite
# needs a cargo build first. On a box six writers share, that build is three minutes, and the
# error arrives as `syntax error at or near "constraint"` with a character offset into the middle
# of a comment. This applies the whole set with psql in about seven seconds, and the two failures
# it caught on its first run were a `--` comment between two `add column` clauses and an
# unterminated dollar-quoted default whose `\n` was a literal backslash-n rather than a newline.
#
# It creates and drops its own database and touches nothing else. Run it after writing any
# migration, and before a `cargo test` that will apply them.
#
# It reads the URL from `run.sh` so the two scripts cannot disagree about the server, and that
# is a CONTRACT: a merge renamed the variable and this script matched nothing, then reported
# **every** migration as FAIL with a connection error that had nothing to do with any of them.
# A checker that cannot find its input must refuse to run, never to answer. The `grep -q`
# guard below is the whole lesson — see the ledger's "measure the value, not the exit status".
#!/usr/bin/env bash
set -uo pipefail
cd "$(dirname "$0")/../.."
LINE=$(grep -m1 -E '^(export )?QA_DATABASE_URL="postgres' scripts/qa/run.sh)
if [ -z "$LINE" ]; then
  echo "sql-check: no QA_DATABASE_URL found in scripts/qa/run.sh — refusing to report a verdict"
  exit 2
fi
RAW=$(printf '%s' "$LINE" | sed -E 's/.*"(postgres[^"]+)".*/\1/')
DB="omnion_sqlcheck"
QA_DB_NAME="$DB"
URL=$(eval echo "$RAW")
export PGPASSWORD=$(printf '%s' "$URL" | sed -E 's|.*://[^:]+:([^@]+)@.*|\1|')
psql -h 127.0.0.1 -p 5433 -U omnion -d postgres -c "drop database if exists $DB" >/dev/null 2>&1
psql -h 127.0.0.1 -p 5433 -U omnion -d postgres -c "create database $DB" >/dev/null
fail=0
for f in database/migrations/*.sql; do
  if ! out=$(psql -h 127.0.0.1 -p 5433 -U omnion -d "$DB" -v ON_ERROR_STOP=1 -q -f "$f" 2>&1); then
    echo "FAIL $f"
    echo "$out" | head -4
    fail=1
  fi
done
[ $fail -eq 0 ] && echo "ALL MIGRATIONS APPLY CLEAN"
psql -h 127.0.0.1 -p 5433 -U omnion -d postgres -c "drop database if exists $DB" >/dev/null 2>&1
exit $fail
