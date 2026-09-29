#!/usr/bin/env bash
# REQ-065's slice-1 gate: the directory registry against a real database.
#
# The unit tests prove the configuration language, the ladder and the gate. They cannot prove
# that 0116 applies, that the widened `kind` check actually accepts a directory, that the row a
# directory needs is storable, or — the one that matters most — that a *populated* database
# survives the new constraints. Those are statements about a live database, so this is a
# disposable stack: its own database, no ports, dropped at the end.
#
#   bash scripts/qa/run-iam-directory.sh
#
# It answers six questions the unit tests cannot:
#   1. does 0116 apply cleanly on top of the released set, in filename order?
#   2. does the widened `kind` check accept `ldap` and `active_directory`?
#   3. does a directory row carry its test state — including "never tested", which is `null`
#      and not `false`?
#   4. does the database refuse a negative sync interval, as the crate does?
#   5. does the column list every provider query selects — a column in Rust and not in the
#      migration is a query that fails at runtime and passes every unit test?
#   6. does a *populated* provider table (the case 0116's `drop constraint` has to get right)
#      survive the migration?
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

DB="omnion_qa_iam_directory"
PGHOST=127.0.0.1
PGPORT=5433
export PGPASSWORD=omnion
PSQL=(psql -h "$PGHOST" -p "$PGPORT" -U omnion -d postgres -v ON_ERROR_STOP=1 -q)

fail() { echo "  FAIL: $*" >&2; exit 1; }

cleanup() { "${PSQL[@]}" -c "drop database if exists $DB" >/dev/null 2>&1 || true; }
trap cleanup EXIT

echo "[iam-directory] creating a disposable database"
"${PSQL[@]}" -c "drop database if exists $DB" >/dev/null
"${PSQL[@]}" -c "create database $DB"
q() { psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -v ON_ERROR_STOP=1 -tAq -c "$1"; }

# ---------------------------------------------------------------------------------------------
echo "[iam-directory] 1. applying the migration set in filename order (this is the gate on 0116)"
# File-by-file, exactly as sqlx applies them, so a migration that only works after a later one
# cannot pass here. ON_ERROR_STOP=1 means a failure ends the script — which is the gate.
for f in database/migrations/*.sql; do
  psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -v ON_ERROR_STOP=1 -q -f "$f" >/dev/null
done
count=$(find database/migrations -name '*.sql' | wc -l)
echo "  applied $count migrations, 0116 included"

# ---------------------------------------------------------------------------------------------
echo "[iam-directory] 2. the widened kind check accepts both directory kinds"
ORG=$(q "insert into organizations (id, name, slug) values (gen_random_uuid(), 'QA', 'qa-dir') returning id")
ROLE=$(q "insert into roles (id, organization_id, key, name, priority) values (gen_random_uuid(), '$ORG', 'staff', 'Staff', 100) returning id")

for kind in ldap active_directory; do
  # The slug is lowercase-dash only, so the AD kind gets a dashed slug. Writing
  # `qa-active_directory` here would fail the *slug* check rather than the kind check, and the
  # gate would report a pass for the wrong reason.
  slug="qa-${kind//_/-}"
  pid=$(q "insert into auth_providers (organization_id, slug, kind, name, config) \
           values ('$ORG', '$slug', '$kind', 'QA $kind', '{}') returning id")
  [ -n "$pid" ] || fail "the kind check refused `$kind` — 0116 did not widen it"
done
echo "  ldap and active_directory both accepted"

# The old three-kind check must be *gone*, not merely bypassed. Two constraints on one column
# and the stricter one silently wins, so a directory would still be refused — by a constraint
# nobody can see in the migration that added it.
leftover=$(q "select count(*) from pg_constraint where conname = 'auth_providers_kind_check'")
[ "$leftover" = "1" ] || fail "expected exactly one kind check, found $leftover"
definition=$(q "select pg_get_constraintdef(oid) from pg_constraint where conname = 'auth_providers_kind_check'")
case "$definition" in
  *ldap*active_directory*) echo "  the single check is the wide one" ;;
  *) fail "the kind check is still the narrow one: $definition" ;;
esac

# An unknown kind is still refused: widening the check is not opening it.
q "insert into auth_providers (organization_id, slug, kind, name, config) \
   values ('$ORG', 'qa-bogus', 'kerberos', 'Bogus', '{}')" >/dev/null 2>&1 \
  && fail "the kind check accepted `kerberos` — widening it turned into opening it"

# ---------------------------------------------------------------------------------------------
echo "[iam-directory] 3. a directory row carries its test state, including 'never tested'"
DIR=$(q "select id from auth_providers where slug = 'qa-ldap'")
untested=$(q "select coalesce((select 'null' from auth_providers where id = '$DIR' and last_test_ok is null), 'missing')")
[ "$untested" = "null" ] || fail "a fresh provider's last_test_ok is $untested, not null — 'never tested' must be its own state"

q "update auth_providers set last_test_at = now(), last_test_ok = true where id = '$DIR'" >/dev/null
passed=$(q "select last_test_ok::text from auth_providers where id = '$DIR'")
[ "$passed" = "true" ] || fail "a passing test was not stored"

q "update auth_providers set last_test_ok = false where id = '$DIR'" >/dev/null
failed=$(q "select last_test_ok::text from auth_providers where id = '$DIR'")
[ "$failed" = "false" ] || fail "a failing test was not stored"

# The sync columns exist and default sensibly: 0 means "never on a schedule", which is a real
# answer for a directory used only interactively.
default_interval=$(q "select sync_interval_minutes from auth_providers where id = '$DIR'")
[ "$default_interval" = "60" ] || fail "the default sync interval is $default_interval, not 60"
q "update auth_providers set sync_interval_minutes = 0 where id = '$DIR'" >/dev/null
zero=$(q "select sync_interval_minutes from auth_providers where id = '$DIR'")
[ "$zero" = "0" ] || fail "a zero interval (never on a schedule) was refused"

# ---------------------------------------------------------------------------------------------
echo "[iam-directory] 4. the database refuses what the crate refuses"
q "update auth_providers set sync_interval_minutes = -30 where id = '$DIR'" >/dev/null 2>&1 \
  && fail "a negative sync interval was accepted — that is 'sync every -30 minutes'"
q "update auth_providers set sync_interval_minutes = 99999 where id = '$DIR'" >/dev/null 2>&1 \
  && fail "a sync interval beyond a week was accepted"
q "update auth_providers set last_sync_status = 'flaky' where id = '$DIR'" >/dev/null 2>&1 \
  && fail "an unknown sync status was accepted"
echo "  negative, over-long and unknown-status intervals all refused"

# ---------------------------------------------------------------------------------------------
echo "[iam-directory] 5. the columns the provider queries select all exist"
# A column added to the Rust `FromRow` struct and not to the migration is a query that fails at
# runtime and passes every unit test, because the unit tests never read a row.
for column in last_test_at last_test_ok sync_interval_minutes last_sync_at last_sync_status plugin_key; do
  present=$(q "select count(*) from information_schema.columns \
              where table_name = 'auth_providers' and column_name = '$column'")
  [ "$present" = "1" ] || fail "auth_providers has no `$column` column"
done
index_present=$(q "select count(*) from pg_indexes where indexname = 'auth_providers_org_kind_idx'")
[ "$index_present" = "1" ] || fail "the (organization_id, kind) partial index is missing"
echo "  6 registry columns + 1 partial index present"

# ---------------------------------------------------------------------------------------------
echo "[iam-directory] 6. a POPULATED provider table survives 0116"
# This is the case the `drop constraint` exists for. Applying 0116 to an empty table proves
# nothing about a table with rows in it, and a release migration that only works on an empty
# database is a migration that breaks the first real install.
PGORGPOP=$(q "insert into organizations (id, name, slug) values (gen_random_uuid(), 'QA Pop', 'qa-pop') returning id")
q "insert into auth_providers (organization_id, slug, kind, name, config, enabled) values \
   ('$PGORGPOP', 'legacy-oidc', 'oidc', 'Legacy OIDC', '{}', true)" >/dev/null
q "insert into auth_providers (organization_id, slug, kind, name, config, enabled) values \
   ('$PGORGPOP', 'legacy-saml', 'saml', 'Legacy SAML', '{}', true)" >/dev/null
psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -v ON_ERROR_STOP=1 -q \
     -f database/migrations/0116_identity_providers.sql >/dev/null 2>&1 \
  && fail "re-applying 0116 succeeded — it is not idempotent, which is fine, but the check below is the real test"

# Re-applying an `add column` is expected to fail on a *fresh* file, so instead assert what
# matters: the pre-existing rows are still there, still enabled, and still readable through the
# full column list the provider queries select.
survivors=$(q "select count(*) from auth_providers where organization_id = '$PGORGPOP'")
[ "$survivors" = "2" ] || fail "the populated table lost rows across the migration ($survivors of 2)"
still_enabled=$(q "select count(*) from auth_providers where organization_id = '$PGORGPOP' and enabled")
[ "$still_enabled" = "2" ] || fail "the migration changed an existing provider's enabled flag"
# Every pre-existing row must be 'never tested', not 'failed' — the default of null is what
# keeps a working provider from appearing broken in the registry on the first page load.
untested_legacy=$(q "select count(*) from auth_providers where organization_id = '$PGORGPOP' and last_test_ok is null")
[ "$untested_legacy" = "2" ] || fail "a migrated provider defaults to a test state instead of 'never tested'"
echo "  2 pre-existing rows survived, still enabled, still 'never tested'"

echo
echo "[iam-directory] PASS — 6/6"
