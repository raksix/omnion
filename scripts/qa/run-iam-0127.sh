#!/usr/bin/env bash
# Omnion — `0127` over a POPULATED `users` table (REQ-065, slice 4 part 9).
#
# `0127` is the first identity migration whose work is a **backfill**, and that is the whole
# reason this gate exists. `0126` was an `add column … default 0`: the populated table mattered
# because a rewrite under the new constraint is where it breaks. `0127` claims something stronger
# — that the accounts the platform already has can be *attributed* — and every way of being wrong
# here produces a migration that applies cleanly and reports a plausible number.
#
# The three ways, and the rows that catch them:
#
#   1. The join is unscoped. A provider slug is unique **within an organization**, so matching
#      `attributes ->> 'sso_last_provider' = p.slug` alone attributes an account to *another
#      tenant's* provider. Two organizations with a provider called `okta` is not an edge case,
#      it is the default. Two providers, two organizations, same slug: the negative control.
#
#   2. The backfill over-claims. `sso_subjects` exists on every account that ever signed in
#      through a provider; a row whose `sso_last_provider` no longer resolves to a live provider
#      must stay `local`, because a dangling `provisioned_by_provider_id` puts a row in the
#      delete guard's result that no provider owns — the count becomes a lie in the direction
#      that matters.
#
#   3. The paired constraint is toothless. `check ((external_id is null) = (provider_id is
#      null))` is the shape that has to refuse a half-written row, and a `check` added over zero
#      rows verifies against zero rows. The pre-existing rows are therefore seeded first and the
#      refusals are asserted *against them*.
#
# The SCIM half gets its own table too: an account pushed by a connector has no provider to join
# on, so it must land as `identity_source = 'scim'` with **both** halves null — the state the
# paired constraint exists to make visible rather than hidden inside `attributes`.
set -euo pipefail

CONTAINER="${QA_PG_CONTAINER:-omnion-postgres}"
DB="omnion_walk_0127_provenance"
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/dev/shm/w9-target}"
export CARGO_INCREMENTAL=0

docker exec "$CONTAINER" psql -U omnion -d postgres -v ON_ERROR_STOP=1 \
  -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" \
  -c "CREATE DATABASE ${DB} OWNER omnion;" >/dev/null

cleanup() {
  docker exec "$CONTAINER" psql -U omnion -d postgres \
    -c "DROP DATABASE IF EXISTS ${DB} WITH (FORCE);" >/dev/null 2>&1 || true
}
trap cleanup EXIT

psql_() {
  docker exec -i "$CONTAINER" psql -U omnion -d "$DB" -v ON_ERROR_STOP=1 "$@"
}

fail() { echo "FAIL: $1"; exit 1; }

echo "== applying every migration up to (but not including) 0127 =="
for file in database/migrations/*.sql; do
  base="$(basename "$file")"
  case "$base" in
    0127_*) break ;;
  esac
  psql_ -q -f - < "$file" >/dev/null
done

# ---------------------------------------------------------------------------------------------
# Rows that predate 0127. Five accounts, each one a way the backfill can be wrong.
#
# The two organizations carry a provider with the **same slug** on purpose: that collision is the
# thing an unscoped join gets wrong, and it cannot be caught by a single-tenant fixture.
# ---------------------------------------------------------------------------------------------
echo "== seeding accounts that predate 0127 =="
psql_ -q <<'SQL'
insert into organizations (name, slug) values
  ('Tenant One', 'prov-tenant-one'),
  ('Tenant Two', 'prov-tenant-two');

-- A provider of the SAME SLUG in each tenant. The join must be scoped by organization.
insert into auth_providers (organization_id, slug, name, kind, enabled, config)
select id, 'okta', 'Okta', 'oidc', true, '{"issuer":"https://a.invalid"}'::jsonb
  from organizations where slug = 'prov-tenant-one';
insert into auth_providers (organization_id, slug, name, kind, enabled, config)
select id, 'okta', 'Okta', 'oidc', true, '{"issuer":"https://b.invalid"}'::jsonb
  from organizations where slug = 'prov-tenant-two';

-- (1) tenant one's account signed in through tenant one's provider.
insert into users (email, password_hash, display_name, organization_id, attributes)
select 'one@omnion.test', '!jit:no-password', 'Tenant One Person', o.id,
       '{"sso_subjects":{"okta":"sub-one"},"sso_last_provider":"okta","sso_last_seen_at":"2026-09-01T00:00:00Z"}'::jsonb
  from organizations o where o.slug = 'prov-tenant-one';

-- (2) tenant TWO's account, signed in through tenant TWO's `okta`. Same slug, other provider.
--     Without the organization in the join this row is attributed to provider (1) — a privilege
--     record pointing at another tenant's connector.
insert into users (email, password_hash, display_name, organization_id, attributes)
select 'two@omnion.test', '!jit:no-password', 'Tenant Two Person', o.id,
       '{"sso_subjects":{"okta":"sub-two"},"sso_last_provider":"okta","sso_last_seen_at":"2026-09-01T00:00:00Z"}'::jsonb
  from organizations o where o.slug = 'prov-tenant-two';

-- (3) a purely local account. Nothing may change about it.
insert into users (email, password_hash, display_name, organization_id, attributes)
select 'local@omnion.test', 'x', 'Local Person', o.id, '{}'::jsonb
  from organizations o where o.slug = 'prov-tenant-one';

-- (4) a SCIM-provisioned account: a connector pushed it, so there is no sign-in to read a
--     provider from. It must be marked and left unattributed rather than invented for.
insert into users (email, password_hash, display_name, organization_id, attributes)
select 'pushed@omnion.test', 'random', 'Pushed Person', o.id,
       '{"scim_external_id":"dir-42"}'::jsonb
  from organizations o where o.slug = 'prov-tenant-one';

-- (5) an account whose provider has since been DELETED. Its row names a slug nothing resolves
--     to; inventing a provider id for it would put a row in the delete guard's count that no
--     provider owns.
insert into users (email, password_hash, display_name, organization_id, attributes)
select 'orphan@omnion.test', '!jit:no-password', 'Orphan Person', o.id,
       '{"sso_subjects":{"vanished":"sub-x"},"sso_last_provider":"vanished"}'::jsonb
  from organizations o where o.slug = 'prov-tenant-one';
SQL

before="$(psql_ -t -A -c "select count(*) from users")"
test "$before" = "5" || fail "expected 5 pre-existing accounts, found $before"

echo "== applying 0127 over the populated table =="
psql_ -q -f - < database/migrations/0127_user_identity_provenance.sql

echo "== no row was lost, renamed or rewritten =="
after="$(psql_ -t -A -c "select count(*) from users")"
test "$after" = "5" || fail "0127 changed the account count to $after"
emails="$(psql_ -t -A -c "select string_agg(email, ',' order by email) from users")"
test "$emails" = "local@omnion.test,one@omnion.test,orphan@omnion.test,pushed@omnion.test,two@omnion.test" \
  || fail "the account set changed: $emails"

# ---------------------------------------------------------------------------------------------
# 1. The SSO half, and the tenant boundary that makes it correct.
# ---------------------------------------------------------------------------------------------
echo "== each SSO account is attributed to its OWN organization's provider =="
crossed="$(psql_ -t -A -c "
  select count(*) from users u
  join auth_providers p on p.id = u.provisioned_by_provider_id
  where p.organization_id is distinct from u.organization_id")"
test "$crossed" = "0" \
  || fail "$crossed account(s) were attributed to a provider in another organization"

for pair in "one@omnion.test:sub-one" "two@omnion.test:sub-two"; do
  email="${pair%%:*}"
  want="${pair##*:}"
  read -r got_src got_ext got_slug <<<"$(psql_ -t -A -F' ' -c "
    select u.identity_source, coalesce(u.external_id, '-'),
           coalesce(p.slug, '-')
      from users u left join auth_providers p on p.id = u.provisioned_by_provider_id
     where u.email = '$email'")"
  test "$got_src" = "oidc" || fail "$email: identity_source is $got_src, expected oidc"
  test "$got_ext" = "$want" || fail "$email: external_id is $got_ext, expected $want"
  test "$got_slug" = "okta" || fail "$email: attributed to provider '$got_slug'"
  echo "   $email → $got_src / $got_ext / $got_slug"
done

echo "== a local account is untouched =="
read -r src prov ext <<<"$(psql_ -t -A -F' ' -c "
  select identity_source, coalesce(provisioned_by_provider_id::text,'-'),
         coalesce(external_id,'-') from users where email = 'local@omnion.test'")"
test "$src" = "local" || fail "a local account's source is $src"
test "$prov" = "-" || fail "a local account got a provider"
test "$ext" = "-" || fail "a local account got an external id"

echo "== a SCIM-provisioned account is marked, and is NOT given a provider =="
read -r src prov ext <<<"$(psql_ -t -A -F' ' -c "
  select identity_source, coalesce(provisioned_by_provider_id::text,'-'),
         coalesce(external_id,'-') from users where email = 'pushed@omnion.test'")"
test "$src" = "scim" || fail "a pushed account's source is $src, expected scim"
test "$prov" = "-" \
  || fail "a pushed account was attributed to a provider it never named — an invented link"
test "$ext" = "-" \
  || fail "a pushed account got an external id without a provider, which the pair check forbids"
echo "   pushed@omnion.test → $src / unattributed (visible, not hidden in attributes)"

echo "== an account whose provider is gone stays local, rather than dangling =="
read -r src prov <<<"$(psql_ -t -A -F' ' -c "
  select identity_source, coalesce(provisioned_by_provider_id::text,'-')
    from users where email = 'orphan@omnion.test'")"
test "$src" = "local" || fail "an orphaned account's source is $src, expected local"
test "$prov" = "-" || fail "an orphaned account was attributed to a provider that does not exist"

# ---------------------------------------------------------------------------------------------
# 2. The refusals. A `check` added over five rows runs its predicate; over zero rows it does not.
# ---------------------------------------------------------------------------------------------
echo "== an unknown source is refused by the database, not by a reader =="
if psql_ -q -c "update users set identity_source = 'magic' where email = 'local@omnion.test'" \
     >/dev/null 2>&1; then
  fail "the closed vocabulary did not refuse 'magic'"
fi
src_after="$(psql_ -t -A -c "select identity_source from users where email = 'local@omnion.test'")"
test "$src_after" = "local" || fail "a refused write left the value as $src_after"

echo "== half a provenance pair is refused in both directions =="
if psql_ -q -c "
  update users set external_id = 'invented'
   where email = 'local@omnion.test'" >/dev/null 2>&1; then
  fail "an external id with no provider was accepted — the pair check has no teeth"
fi
if psql_ -q -c "
  update users set provisioned_by_provider_id = (
      select id from auth_providers order by id limit 1), external_id = null
   where email = 'local@omnion.test'" >/dev/null 2>&1; then
  fail "a provider with no external id was accepted — the pair check has no teeth"
fi

# The definition that is actually in force, read back rather than assumed: if two constraints
# survive, the wider one silently wins and every refusal above proved nothing.
pairdef="$(psql_ -t -A -c "
  select pg_get_constraintdef(oid) from pg_constraint
   where conname = 'users_provenance_paired_check'")"
echo "   in force: $pairdef"
case "$pairdef" in
  *external_id*) : ;;
  *) fail "the surviving constraint is not the pairing one: $pairdef" ;;
esac

echo "== the unique index refuses two accounts claiming one directory id =="
# The provider is joined **through the organization**, not by slug alone. Both tenants have a
# provider called `okta`, so `from organizations o, auth_providers p where o.slug = … and
# p.slug = 'okta'` is a cross join whose `limit 1` picks whichever row the planner returns first
# — a different tenant's connector. That is the same unscoped-slug mistake the migration's own
# header warns about, committed inside the test that exists to catch it.
#
# The claim has to be *established* first, and it cannot come from the seeded pushed account: the
# migration deliberately leaves a SCIM-pushed row with **both** halves null, because a connector
# names no provider. So the first account of this pair is inserted here, where the pair is the
# thing under test.
psql_ -q -c "
  insert into users (email, password_hash, display_name, organization_id,
                     identity_source, provisioned_by_provider_id, external_id)
  select 'holder@omnion.test', 'x', 'Holder', o.id, 'scim', p.id, 'dir-42'
    from organizations o
    join auth_providers p on p.organization_id = o.id
   where o.slug = 'prov-tenant-one' and p.slug = 'okta'"
holder="$(psql_ -t -A -c "select count(*) from users where email = 'holder@omnion.test'")"
test "$holder" = "1" \
  || fail "the first claim on (provider, external id) did not take: $holder row(s)"

# The second claim on the SAME pair must now be refused. Note the email differs, so nothing else
# about the row differs — the only reason this insert can fail is the index.
if psql_ -q -c "
  insert into users (email, password_hash, display_name, organization_id,
                     identity_source, provisioned_by_provider_id, external_id)
  select 'clash@omnion.test', 'x', 'Clash', o.id, 'scim', p.id, 'dir-42'
    from organizations o
    join auth_providers p on p.organization_id = o.id
   where o.slug = 'prov-tenant-one' and p.slug = 'okta'" >/dev/null 2>&1; then
  fail "two accounts claimed the same (provider, external id)"
fi
clash="$(psql_ -t -A -c "select count(*) from users where email = 'clash@omnion.test'")"
test "$clash" = "0" || fail "the refused insert left a row behind"

echo "== and it is PARTIAL: the same external id under two providers is legal =="
psql_ -q -c "
  insert into users (email, password_hash, display_name, organization_id,
                     identity_source, provisioned_by_provider_id, external_id)
  select 'clash@omnion.test', 'x', 'Clash', o.id, 'scim', p.id, 'dir-42'
    from organizations o
    join auth_providers p on p.organization_id = o.id
   where o.slug = 'prov-tenant-two' and p.slug = 'okta'"
read -r a b <<<"$(psql_ -t -A -F' ' -c "
  select string_agg(distinct p.organization_id::text, ',')
    from users u join auth_providers p on p.id = u.provisioned_by_provider_id
   where u.external_id = 'dir-42'")"
test "$(printf '%s\n' "$a" | tr ',' '\n' | sort -u | wc -l)" = "2" \
  || fail "the unique index is not scoped per provider: providers found = $a"
echo "   external id 'dir-42' now exists under 2 providers — the index is per-provider, not global"

echo "== the provenance index exists and is partial =="
indexdef="$(psql_ -t -A -c "
  select indexdef from pg_indexes where indexname = 'users_provisioned_by_provider_idx'")"
echo "   $indexdef"
case "$indexdef" in
  *"identity_source <> 'local'"*) : ;;
  *) fail "the provenance index is not the partial one the migration describes" ;;
esac

echo "== the foreign key is RESTRICT, and deleting a provider with accounts is refused =="
# `on delete set null` is the safe-looking choice and it cannot work here: the referential action
# would null the provider and leave the external id, which is exactly the half-written provenance
# `users_provenance_paired_check` refuses — so the delete would die as 23514 instead of reaching
# the guard that can name the count. Asserted by reading the rule back, then by deleting for real.
fkdef="$(psql_ -t -A -c "
  select confdeltype from pg_constraint
   where conrelid = 'users'::regclass
     and contype = 'f'
     and conkey = array[(select attnum from pg_attribute
                          where attrelid = 'users'::regclass
                            and attname = 'provisioned_by_provider_id')]")"
echo "   confdeltype = '$fkdef' (r = restrict, a = no action, n = set null, d = set default)"
test "$fkdef" = "r" || fail "the provider link is not ON DELETE RESTRICT (confdeltype=$fkdef)"

if psql_ -q -c "delete from auth_providers where slug = 'okta' and id = (
       select provisioned_by_provider_id from users where email = 'one@omnion.test')" \
     >/dev/null 2>&1; then
  fail "deleting a provider that provisioned accounts was allowed to reach the database"
fi
survivors="$(psql_ -t -A -c "select count(*) from auth_providers where slug = 'okta'")"
test "$survivors" = "2" || fail "a refused provider delete removed a row: $survivors left"
attributed="$(psql_ -t -A -c "
  select external_id from users where email = 'one@omnion.test'")"
test "$attributed" = "sub-one" \
  || fail "a refused provider delete rewrote the provenance: external_id is '$attributed'"

echo "== after reassignment the database allows the delete =="
# **Every** account of that provider, not just the one the refusal named. Reassigning one and then
# deleting is a state the database must still refuse, and the walk proved it does — the gate
# originally reassigned a single row and the delete died on the second, which reads as a broken
# gate rather than as a working rule.
psql_ -q -c "update users set identity_source='local', provisioned_by_provider_id=null, \
                     external_id=null \
                where provisioned_by_provider_id = (
                    select p.id from auth_providers p, organizations o
                     where p.organization_id = o.id
                       and o.slug = 'prov-tenant-one' and p.slug = 'okta')"
stale="$(psql_ -t -A -c "
  select count(*) from users u
    join auth_providers p on p.id = u.provisioned_by_provider_id
   join organizations o on o.id = p.organization_id
   where o.slug = 'prov-tenant-one' and p.slug = 'okta'")"
test "$stale" = "0" || fail "$stale account(s) still claim that provider after reassignment"

psql_ -q -c "delete from auth_providers where slug = 'okta' and id = (
       select p.id from auth_providers p, organizations o
        where p.organization_id = o.id and o.slug = 'prov-tenant-one' and p.slug = 'okta')"
gone="$(psql_ -t -A -c "
  select count(*) from auth_providers p join organizations o on o.id = p.organization_id
   where o.slug = 'prov-tenant-one' and p.slug = 'okta'")"
test "$gone" = "0" || fail "a provider with nothing depending on it could not be deleted"
read -r src prov ext <<<"$(psql_ -t -A -F' ' -c "
  select identity_source, coalesce(provisioned_by_provider_id::text,'-'),
         coalesce(external_id,'-') from users where email = 'one@omnion.test'")"
test "$src$prov$ext" = "local--" \
  || fail "the reassigned account is '$src / $prov / $ext', expected a fully local row"

echo "== re-applying 0127 is refused =="
if psql_ -q -f - < database/migrations/0127_user_identity_provenance.sql >/dev/null 2>&1; then
  fail "0127 applied twice without a complaint"
fi

echo
echo "PASS 0127_provenance: 5 pre-existing accounts survived; the same provider slug in two"
echo "                        organizations did not cross tenants; a local account is untouched; a"
echo "                        SCIM-pushed account is marked and left unattributed rather than"
echo "                        invented for; an account whose provider is gone stays local; an"
echo "                        unknown source, a half-written pair and a duplicate directory id are"
echo "                        all refused; the index is per-provider and partial; the provider link"
echo "                        is ON DELETE RESTRICT, so the database refuses a delete the guard"
echo "                        also refuses, and permits it once the account is reassigned; a"
echo "                        second apply fails."
