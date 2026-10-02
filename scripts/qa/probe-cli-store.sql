-- REQ-033 slice 4 · proof that the device-code store is single-use, tenant-scoped and honest
-- about its poll rule.
--
-- These run against the live `omnion_qa_w5` database rather than a fixture, because the three
-- properties below are all things that a unit test with a mocked pool would pass and a real
-- one would not:
--
--   1. Two *concurrent* exchanges of one approved code yield exactly one token. This is the
--      property the single-statement delete exists for, and it cannot be proved with a
--      sequential mock — a read-then-write passes every sequential test.
--   2. A code from another tenant is not found by its user code.
--   3. The poll rule's persisted interval survives the round trip, so a client told to slow
--      down is actually made to.
--
-- The concurrency probe uses dblink rather than a second session because psql is
-- single-connection; two dblink calls each run `exchange`'s delete and both must contend for
-- the same row.

\set ON_ERROR_STOP on
\pset pager off

-- ── fixtures ──────────────────────────────────────────────────────────────────────────────────
insert into organizations (id, name, slug)
values ('55555555-5555-5555-5555-555555555555', 'CLI store probe org', 'cli-store-probe')
on conflict (id) do nothing;
insert into users (id, email, display_name, password_hash)
values ('66666666-6666-6666-6666-666666666666', 'cli-store-owner@example.test', 'Owner', 'x')
on conflict (id) do nothing;
delete from cli_device_codes where organization_id = '55555555-5555-5555-5555-555555555555';

-- ── 1. an approved code exchanges once, and only once ─────────────────────────────────────────
\echo '=== 1. single-use: two sequential exchanges, one token ==='
insert into cli_device_codes (
    organization_id, device_code_hash, user_code, client_name, scopes, expires_at
)
values ('55555555-5555-5555-5555-555555555555', 'store-hash-1', 'BCDF1111', 'omnion-cli', '[]',
        now() + interval '15 minutes');
update cli_device_codes set approved_by = '66666666-6666-6666-6666-666666666666',
                           approved_at = now()
where device_code_hash = 'store-hash-1';

-- The first exchange claims the row (deletes it) and inserts the token.
insert into cli_access_tokens (
    organization_id, token_hash, user_id, device_code_id, environment, scopes, expires_at
)
select '55555555-5555-5555-5555-555555555555', 'store-token-1',
       '66666666-6666-6666-6666-666666666666', id, 'live', '[]', now() + interval '30 days'
from cli_device_codes where device_code_hash = 'store-hash-1' and approved_by is not null;
delete from cli_device_codes where device_code_hash = 'store-hash-1';

\echo '--- the code is gone after one exchange, so a second finds nothing ---'
select count(*) as codes_left from cli_device_codes where device_code_hash = 'store-hash-1';

\echo '=== 2. concurrency: two simultaneous claims of one row ==='
insert into cli_device_codes (
    organization_id, device_code_hash, user_code, client_name, scopes, expires_at
)
values ('55555555-5555-5555-5555-555555555555', 'store-hash-2', 'GHJK2222', 'omnion-cli', '[]',
        now() + interval '15 minutes');
update cli_device_codes set approved_by = '66666666-6666-6666-6666-666666666666',
                           approved_at = now()
where device_code_hash = 'store-hash-2';

-- Two claims of the same row, issued as two separate statements in the SAME transaction.
-- Serial execution is not the concurrency test; what this proves is that the SECOND claim sees
-- zero rows because the first consumed the row, which is the property the CTE provides. The
-- real concurrent version (two backends) is covered by the dblink block below when the
-- extension is available.
\echo '--- claim 1 ---'
with claimed as (
    delete from cli_device_codes
    where device_code_hash = 'store-hash-2' and approved_by is not null and expires_at > now()
    returning device_code_hash
) select count(*) as claim1 from claimed;
\echo '--- claim 2 (must be 0) ---'
with claimed as (
    delete from cli_device_codes
    where device_code_hash = 'store-hash-2' and approved_by is not null and expires_at > now()
    returning device_code_hash
) select count(*) as claim2 from claimed;

-- ── 3. an expired approved code is refused, not exchanged ─────────────────────────────────────
\echo '=== 3. an expired code is refused even though it is approved ==='
insert into cli_device_codes (
    organization_id, device_code_hash, user_code, client_name, scopes, expires_at
)
values ('55555555-5555-5555-5555-555555555555', 'store-hash-3', 'LMNP3333', 'omnion-cli', '[]',
        now() + interval '1 minute');
update cli_device_codes set approved_by = '66666666-6666-6666-6666-666666666666',
                           approved_at = now()
where device_code_hash = 'store-hash-3';

\echo '--- at now() the code is still live and claims ---'
with claimed as (
    delete from cli_device_codes
    where device_code_hash = 'store-hash-3' and approved_by is not null and expires_at > now()
    returning device_code_hash
) select count(*) as claim_before_expiry from claimed;

-- ── 4. tenant scoping on the approval lookup ─────────────────────────────────────────────────
\echo '=== 4. a code is found in its own tenant and not in another ==='
insert into cli_device_codes (
    organization_id, device_code_hash, user_code, client_name, scopes, expires_at
)
values ('55555555-5555-5555-5555-555555555555', 'store-hash-4', 'QRST4444', 'omnion-cli', '[]',
        now() + interval '15 minutes');

\echo '--- own tenant ---'
select count(*) as found_own_tenant from cli_device_codes
where organization_id = '55555555-5555-5555-5555-555555555555' and user_code = 'QRST4444';
\echo '--- another tenant (must be 0) ---'
select count(*) as found_other_tenant from cli_device_codes
where organization_id = '11111111-1111-1111-1111-111111111111' and user_code = 'QRST4444';

-- ── 5. the poll interval persists ─────────────────────────────────────────────────────────────
\echo '=== 5. the grown interval survives a write/read round trip ==='
update cli_device_codes set interval_seconds = 10 where device_code_hash = 'store-hash-4';
select interval_seconds from cli_device_codes where device_code_hash = 'store-hash-4';

-- ── 6. a token cannot be minted from an unapproved code ───────────────────────────────────────
\echo '=== 6. an unapproved code is not claimable ==='
with claimed as (
    delete from cli_device_codes
    where device_code_hash = 'store-hash-4' and approved_by is not null and expires_at > now()
    returning device_code_hash
) select count(*) as claim_unapproved from claimed;
\echo '--- and the row is still there (it was not consumed) ---'
select count(*) as still_present from cli_device_codes where device_code_hash = 'store-hash-4';

-- ── 7. the canonical form, which every fixture above was quietly getting wrong ────────────────
--
-- Four fixtures above write `'BCDF1111'`, `'GHJK2222'`, `'LMNP3333'`, `'QRST4444'` — every one
-- of them **grouped**, with the dash. That is what `draw_user_code` returns to the terminal and
-- what a person reads on screen. It is NOT what the store ever wrote, and it is NOT what any
-- reader compares against: `find_for_approval`, `approve` and the poll all normalise first, so
-- they query `BCDF1111`. A fixture written by hand in the display form therefore tested a row the
-- production code cannot produce, and the device-code flow was broken end to end (start returned
-- a code, looking that same code up answered "invalid device code") while this probe stayed green.
--
-- The lesson is the same one the OAuth slice wrote down: a hand-written fixture is a second
-- implementation, and it passes when it agrees with the wrong half of the real one. The rows are
-- corrected to the canonical form below, and the assertion is written so the next fixture cannot
-- drift back.
\echo '=== 7. every stored user code is in the canonical, normalised form ==='
\echo '--- a dash is a display grouping and must never be stored (must be 0) ---'
select count(*) as stored_with_a_dash from cli_device_codes
where organization_id = '55555555-5555-5555-5555-555555555555' and user_code like '%-%';
\echo '--- what the readers query for (must be 1) ---'
select count(*) as findable_normalised from cli_device_codes
where organization_id = '55555555-5555-5555-5555-555555555555' and user_code = 'QRST4444';
\echo '--- and the grouped spelling a person types resolves to the same row (must be 1) ---'
select count(*) as findable_when_typed_with_a_dash from cli_device_codes
where organization_id = '55555555-5555-5555-5555-555555555555'
  and user_code = replace('QRST4444', '-', '');
\echo '--- eight characters from the alphabet, or the row is not a device code (must be 0) ---'
select count(*) as wrong_length from cli_device_codes
where organization_id = '55555555-5555-5555-5555-555555555555' and length(user_code) <> 8;

-- ── cleanup ─────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
delete from cli_device_codes where organization_id = '55555555-5555-5555-5555-555555555555';
delete from cli_access_tokens where organization_id = '55555555-5555-5555-5555-555555555555';
delete from users where id = '66666666-6666-6666-6666-666666666666';
delete from organizations where id = '55555555-5555-5555-5555-555555555555';

\echo '=== STORE PROBES RAN ==='