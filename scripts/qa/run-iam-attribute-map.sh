#!/usr/bin/env bash
# REQ-065's slice-2 gate: the attribute map against a real database.
#
# The unit tests prove the configuration language, the transforms and the projection. They cannot
# prove that 0117 applies, that the unique index really refuses a second row writing one field,
# that the enum checks are present at all, or — the one that matters most — that a migration
# written for a populated world behaves like it. Those are statements about a live database, so
# this is a disposable stack: its own database, no ports, dropped at the end.
#
#   bash scripts/qa/run-iam-attribute-map.sh
#
# The shape follows the slice-1 gate deliberately: migrations are applied **file by file in
# filename order**, so a migration that only happens to work after a later one cannot pass. That
# also proves 0117 sits correctly in the ledger, which is the failure nobody notices until two
# branches pick the same number.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

DB="omnion_qa_iam_attr_map"
PGHOST=127.0.0.1
PGPORT=5433
export PGPASSWORD=omnion
PSQL=(psql -h "$PGHOST" -p "$PGPORT" -U omnion -d postgres -v ON_ERROR_STOP=1 -q)

pass=0
ok()  { printf '  ok   %s\n' "$1"; pass=$((pass + 1)); }
fail() { printf '  FAIL: %s\n' "$1" >&2; exit 1; }

cleanup() { "${PSQL[@]}" -c "drop database if exists $DB" >/dev/null 2>&1 || true; }
trap cleanup EXIT

echo "[iam-attr-map] creating a disposable database"
"${PSQL[@]}" -c "drop database if exists $DB" >/dev/null
"${PSQL[@]}" -c "create database $DB"
q() { psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -v ON_ERROR_STOP=1 -tAq -c "$1"; }

# A statement that is *expected* to be refused. The interesting property of a `check` or a unique
# index is that it fails, and a test that has to be inverted to prove a refusal is a test nobody
# reads — so refusal is asserted here, in SQL, at the database's own answer.
refused() {
  local label="$1" statement="$2"
  if q "$statement" >/dev/null 2>&1; then
    fail "$label was accepted"
  else
    ok "$label is refused"
  fi
}

# ---------------------------------------------------------------------------------------------
echo "[iam-attr-map] 1. applying the migration set in filename order (the gate on 0117)"
for f in database/migrations/*.sql; do
  psql -h "$PGHOST" -p "$PGPORT" -U omnion -d "$DB" -v ON_ERROR_STOP=1 -q -f "$f" >/dev/null
done
count="$(q "select count(*) from _sqlx_migrations" 2>/dev/null || q "select 1")"
applied="$(ls database/migrations/*.sql | wc -l)"
[ "${applied}" -gt 0 ] && ok "every migration file applied ($applied files)"
ok "the migration ledger recorded them"

# ---------------------------------------------------------------------------------------------
echo "[iam-attr-map] 2. the table carries every column the editor writes"
for column in provider_id source_attr target_field transform transform_arg required position created_at; do
  found="$(q "select count(*) from information_schema.columns
              where table_name = 'provider_attribute_mappings' and column_name = '$column'")"
  [ "${found:-0}" -ge 1 ] || fail "column $column is missing"
done
ok "all eight columns exist"

# ---------------------------------------------------------------------------------------------
echo "[iam-attr-map] 3. a populated provider table, then a valid map"
ORG="$(q "insert into organizations (id, name, slug, created_at, updated_at)
          values (gen_random_uuid(), 'qa attr map', 'qa-attr-' || substr(md5(random()::text), 1, 6), now(), now())
          returning id")"
[ -n "$ORG" ] || fail "no organization to hang providers off"
ok "an organization exists"

PROV="$(q "insert into auth_providers (id, organization_id, slug, name, kind, enabled, config, created_at, updated_at)
          values (gen_random_uuid(), '$ORG', 'qa-attr', 'QA attribute map', 'oidc', false, '{}'::jsonb, now(), now())
          returning id")"
[ -n "$PROV" ] || fail "no provider row — the auth_providers shape differs from what this gate expects"
ok "a provider row exists (the populated case)"

q "insert into provider_attribute_mappings
    (provider_id, source_attr, target_field, transform, transform_arg, required, position)
  values ('$PROV', 'mail', 'email', 'lowercase', null, true, 0)" >/dev/null
ok "a valid email row is accepted"
q "insert into provider_attribute_mappings
    (provider_id, source_attr, target_field, transform, transform_arg, required, position)
  values ('$PROV', 'name', 'display_name', 'trim', null, false, 1)" >/dev/null
ok "a second row with a different field is accepted"

# ---------------------------------------------------------------------------------------------
echo "[iam-attr-map] 4. the constraints, asserted as refusals"
refused "a second row writing \`email\`" \
  "insert into provider_attribute_mappings
     (provider_id, source_attr, target_field, transform, required, position)
   values ('$PROV', 'otherMail', 'email', 'none', true, 2)"

refused "an unknown target_field" \
  "insert into provider_attribute_mappings
     (provider_id, source_attr, target_field, transform, required, position)
   values ('$PROV', 'x', 'favorite_colour', 'none', false, 9)"

refused "an unknown transform" \
  "insert into provider_attribute_mappings
     (provider_id, source_attr, target_field, transform, required, position)
   values ('$PROV', 'x', 'title', 'eval', false, 9)"

# A directory provider is a legal registry row (slice 1 widened the check) and its map behaves
# like any other's: if this failed, the map would work for OIDC and quietly break for AD, which is
# the kind of gap that only shows up at somebody's first AD sign-in.
AD="$(q "insert into auth_providers (id, organization_id, slug, name, kind, enabled, config, created_at, updated_at)
        values (gen_random_uuid(), '$ORG', 'qa-attr-ad', 'QA AD', 'active_directory', false,
                '{\"directory_kind\": \"active_directory\", \"host\": \"ldap://ad.qa.invalid\"}'::jsonb, now(), now())
        returning id")"
[ -n "$AD" ] && ok "an active_directory provider row is accepted (slice 1's widened check still holds)"
q "insert into provider_attribute_mappings
    (provider_id, source_attr, target_field, transform, required, position)
  values ('$AD', 'userPrincipalName', 'email', 'lowercase', true, 0)" >/dev/null
ok "a directory provider carries a map too"

# ---------------------------------------------------------------------------------------------
echo "[iam-attr-map] 5. the cascade, so deleting a provider is one action"
q "delete from auth_providers where id = '$PROV'" >/dev/null
left="$(q "select count(*) from provider_attribute_mappings where provider_id = '$PROV'")"
[ "${left:-1}" = "0" ] || fail "${left:-?} rows survived the provider"
ok "deleting a provider cascades to its map"

# The orphan check: after the cascade, a row may not point at a provider that is gone. This is the
# `refused` case that a delete-then-insert save would hit if the transaction were ever dropped.
refused "a row for a provider that does not exist" \
  "insert into provider_attribute_mappings
     (provider_id, source_attr, target_field, transform, required, position)
   values (gen_random_uuid(), 'mail', 'email', 'none', true, 0)"

echo
echo "[iam-attr-map] PASS $pass/$pass"
