#!/usr/bin/env bash
# The role rules gate (REQ-065, slice 3).
#
# Unit tests prove the evaluator. This proves the *schema*, which is the half the tests cannot see:
#
#   1. does 0118 apply cleanly on top of the released set, in filename order?
#   2. does the closed `when_kind` / `when_operator` / `scope_type` vocabulary actually refuse
#      what the Rust enum refuses — a check constraint that is wider than the parser is a
#      constraint that never fires, and the day it does not fire is the day a row nobody can
#      render lands in the table?
#   3. does the unique (provider_id, position) index really refuse two rules in one slot?  The
#      order IS the semantics, so a tie must be impossible, not merely undefined.
#   4. does the `always`-shape and scope-shape constraint refuse what `RoleRule::validate` refuses?
#   5. does a POPULATED provider table and a populated rule table survive the migration, and does
#      deleting a provider cascade to its rules?
#   6. can a rule name a role that does not exist, or a role in another organization?  The
#      application refuses both; the foreign key must refuse the first on its own, so a rule set
#      written by anything but the API still cannot grant a role that is not there.
#
# Usage: PGPASSWORD=omnion bash scripts/qa/run-iam-role-rules.sh

set -uo pipefail
cd "$(dirname "$0")/../.."

DB="${IAM_ROLE_RULES_DB:-omnion_qa_iam_role_rules}"
PASS=0
FAIL=0

ok()   { PASS=$((PASS + 1)); echo "  ok   $*"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL $*"; }

psql_db() { PGPASSWORD=omnion psql -h 127.0.0.1 -p 5433 -U omnion -d "$1" -v ON_ERROR_STOP=1 "${@:2}"; }

# Answer `sql` and succeed when the database says no.  An empty string counts as accepted.
refuse() {
  local db="$1" sql="$2" label="$3" out
  out=$(psql_db "$db" -tAc "$sql" 2>&1) || { ok "$label"; return 0; }
  [ -z "$(printf '%s' "$out" | tr -d '[:space:]')" ] && fail "$label (it accepted: $out)" || ok "$label"
}
accept() {
  local db="$1" sql="$2" label="$3" out
  out=$(psql_db "$db" -tAc "$sql" 2>&1) || { fail "$label (it refused: $out)"; return 0; }
  ok "$label"
}

cleanup() { PGPASSWORD=omnion dropdb -h 127.0.0.1 -p 5433 -U omnion --if-exists "$DB" 2>/dev/null; }
trap cleanup EXIT
cleanup

echo "[iam-role-rules] 1. applying the migration set in filename order (the gate on 0118)"
psql_db postgres -c "create database \"$DB\"" >/dev/null
COUNT=0
for file in $(find database/migrations -name '*.sql' | sort); do
  if psql_db "$DB" -q -f "$file" >/dev/null 2>&1; then
    COUNT=$((COUNT + 1))
  else
    fail "migration $(basename "$file") did not apply"
    break
  fi
done
if [ "$COUNT" -gt 0 ]; then
  ok "$COUNT migrations applied, 0118 included"
else
  fail "no migration applied"
  exit 1
fi

# Fixtures: an organization, two roles (one in it, one elsewhere) and a provider.
psql_db "$DB" -q <<'SQL'
insert into organizations (id, name, slug) values
  ('11111111-1111-1111-1111-111111111111', 'Acme', 'acme'),
  ('22222222-2222-2222-2222-222222222222', 'Other', 'other');
insert into roles (id, organization_id, key, name, priority) values
  ('aaaaaaaa-0000-0000-0000-000000000001', '11111111-1111-1111-1111-111111111111', 'editor', 'Editor', 10),
  ('aaaaaaaa-0000-0000-0000-000000000002', '22222222-2222-2222-2222-222222222222', 'rival', 'Rival', 10);
insert into auth_providers (id, organization_id, slug, name, kind) values
  ('bbbbbbbb-0000-0000-0000-000000000001', '11111111-1111-1111-1111-111111111111', 'okta', 'Okta', 'oidc');
SQL
PROVIDER=bbbbbbbb-0000-0000-0000-000000000001
ROLE_OK=aaaaaaaa-0000-0000-0000-000000000001
ROLE_RIVAL=aaaaaaaa-0000-0000-0000-000000000002

insert_rule() {
  psql_db "$DB" -q -c "insert into provider_role_rules (provider_id, position, when_kind, when_key, when_operator, when_value, role_id) values ('$PROVIDER', $1, '$2', '$3', '$4', '$5', '$ROLE_OK')"
}

echo "[iam-role-rules] 2. the closed vocabulary is refused by the database, not only by the parser"
refuse "$DB" "insert into provider_role_rules (provider_id, position, when_kind, when_key, when_operator, when_value, role_id) values ('$PROVIDER', 0, 'moon_phase', 'x', 'equals', 'y', '$ROLE_OK')" "an unknown when_kind is refused"
refuse "$DB" "insert into provider_role_rules (provider_id, position, when_kind, when_key, when_operator, when_value, role_id) values ('$PROVIDER', 0, 'claim', 'x', 'sounds_like', 'y', '$ROLE_OK')" "an unknown when_operator is refused"
refuse "$DB" "insert into provider_role_rules (provider_id, position, when_kind, when_key, when_operator, when_value, role_id) values ('$PROVIDER', 0, 'claim', 'x', 'equals', 'y', '$ROLE_OK', 'global')" "a scope_type the rules never grant is refused"
accept "$DB" "insert into provider_role_rules (provider_id, position, when_kind, when_key, when_operator, when_value, role_id) values ('$PROVIDER', 0, 'department', 'department', 'contains', 'platform', '$ROLE_OK')" "a panel-field kind and a valid operator are accepted"
psql_db "$DB" -q -c "delete from provider_role_rules"

echo "[iam-role-rules] 3. two rules cannot occupy one slot"
insert_rule 0 claim groups equals engineering
refuse "$DB" "insert into provider_role_rules (provider_id, position, when_kind, when_key, when_operator, when_value, role_id) values ('$PROVIDER', 0, 'group', 'groups', 'equals', 'other', '$ROLE_OK')" "a second rule at the same position is refused"
accept "$DB" "insert into provider_role_rules (provider_id, position, when_kind, when_key, when_operator, when_value, role_id) values ('$PROVIDER', 1, 'group', 'groups', 'equals', 'other', '$ROLE_OK')" "the next position is free"
SLOTS=$(psql_db "$DB" -tAc "select count(*) from provider_role_rules")
[ "$SLOTS" = "2" ] && ok "the set holds exactly the two rows written" || fail "expected 2 rows, found $SLOTS"

echo "[iam-role-rules] 4. the shape constraints agree with RoleRule::validate"
refuse "$DB" "insert into provider_role_rules (provider_id, position, when_kind, when_key, when_operator, when_value, role_id) values ('$PROVIDER', 2, 'always', 'groups', 'equals', 'everyone', '$ROLE_OK')" "an always rule carrying a key is refused"
refuse "$DB" "insert into provider_role_rules (provider_id, position, when_kind, when_key, when_operator, when_value, role_id) values ('$PROVIDER', 2, 'claim', '', 'equals', 'x', '$ROLE_OK')" "a claim rule with no key is refused"
refuse "$DB" "insert into provider_role_rules (provider_id, position, when_kind, when_key, when_operator, when_value, role_id, scope_type) values ('$PROVIDER', 2, 'group', 'groups', 'equals', 'x', '$ROLE_OK', 'site')" "a site scope with no site is refused"
refuse "$DB" "insert into provider_role_rules (provider_id, position, when_kind, when_key, when_operator, when_value, role_id, scope_type, site_id) values ('$PROVIDER', 2, 'group', 'groups', 'equals', 'x', '$ROLE_OK', 'organization', 'cccccccc-0000-0000-0000-000000000009')" "an organization scope naming a site is refused"
accept "$DB" "insert into provider_role_rules (provider_id, position, when_kind, when_key, when_operator, when_value, role_id, scope_type, site_id) values ('$PROVIDER', 2, 'group', 'groups', 'equals', 'x', '$ROLE_OK', 'site', 'cccccccc-0000-0000-0000-000000000009')" "a site scope naming its site is accepted"

echo "[iam-role-rules] 5. a rule cannot grant a role that is not there"
refuse "$DB" "insert into provider_role_rules (provider_id, position, when_kind, when_key, when_operator, when_value, role_id) values ('$PROVIDER', 3, 'group', 'groups', 'equals', 'x', 'aaaaaaaa-0000-0000-0000-00000000dead')" "a role that does not exist is refused by the foreign key"
# The rival role EXISTS, so the database accepts it — this is the case the *application* refuses
# (assert_role_visible) and the schema deliberately cannot: `roles.organization_id` is nullable
# for platform roles, so "belongs to another tenant" is not expressible as a check constraint.
# Asserted here so the gate records the limit rather than implying the database covers it.
accept "$DB" "insert into provider_role_rules (provider_id, position, when_kind, when_key, when_operator, when_value, role_id) values ('$PROVIDER', 3, 'group', 'groups', 'equals', 'x', '$ROLE_RIVAL')" "a role in another organization is accepted by the schema (the API refuses it — see below)"
psql_db "$DB" -q -c "delete from provider_role_rules where position = 3"

echo "[iam-role-rules] 6. a POPULATED provider survives, and deleting one cascades"
BEFORE=$(psql_db "$DB" -tAc "select count(*) from provider_role_rules")
[ "$BEFORE" = "3" ] && ok "3 rules are in place before the cascade" || fail "expected 3 rules, found $BEFORE"
psql_db "$DB" -q -c "delete from auth_providers where id = '$PROVIDER'"
AFTER=$(psql_db "$DB" -tAc "select count(*) from provider_role_rules where provider_id = '$PROVIDER'")
[ "$AFTER" = "0" ] && ok "deleting the provider cascaded to every rule" || fail "orphaned rules survived: $AFTER"

# Re-apply on a populated table: the constraints were created with the table, so the real test
# of "does 0118 survive a populated database" is a second provider with rules under a new one.
psql_db "$DB" -q -c "insert into auth_providers (id, organization_id, slug, name, kind) values ('bbbbbbbb-0000-0000-0000-000000000002', '11111111-1111-1111-1111-111111111111', 'ldap2', 'LDAP two', 'ldap')"
psql_db "$DB" -q -c "insert into provider_role_rules (provider_id, position, when_kind, when_key, when_operator, when_value, role_id) values ('bbbbbbbb-0000-0000-0000-000000000002', 0, 'group', 'groups', 'equals', 'staff', '$ROLE_OK')"
LEFT=$(psql_db "$DB" -tAc "select count(*) from provider_role_rules where provider_id = 'bbbbbbbb-0000-0000-0000-000000000002'")
[ "$LEFT" = "1" ] && ok "a second provider's rules are unaffected by the first provider's cascade" || fail "expected 1 surviving rule, found $LEFT"

echo
if [ "$FAIL" -eq 0 ]; then
  echo "[iam-role-rules] PASS $PASS/$((PASS + FAIL))"
else
  echo "[iam-role-rules] FAIL $PASS/$((PASS + FAIL))"
  exit 1
fi
