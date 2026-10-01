-- REQ-033 slice 4 · proof that `0235_developer_cli_device_codes.sql` enforces what it claims.
--
-- Every numbered refusal below is written to FAIL, and the proof is that it fails **with its own
-- named constraint**. A migration that merely applies is not a migration that works: the
-- constraints here are the enforcement for the phishing properties the request's risk note
-- names, and "the file says NOT NULL" is not the same claim as "a row without it is refused".
--
-- Run against the w5 QA database:
--   psql -h 127.0.0.1 -p 5433 -U omnion -d omnion_qa_w5 -f scripts/qa/probe-cli-device-codes-migration.sql
--
-- ── why every refusal is wrapped in a savepoint ────────────────────────────────────────────────
--
-- The first version of this file put all twelve statements in one transaction. It reported
-- twelve errors and looked like a complete proof. It was not: the first violation aborted the
-- transaction, and every statement after it answered
--
--   ERROR: current transaction is aborted, commands ignored until end of transaction block
--
-- — the same abort, eleven times, and *not one* of the remaining constraints was ever
-- exercised. A proof that reports the same error eleven times has proved one thing eleven
-- times. So each refusal below gets its own SAVEPOINT and is rolled back to it, which is what
-- makes the constraint name in each ERROR line the one that statement provoked.
--
-- `\set ON_ERROR_STOP off` is deliberate for the same reason: the run must reach the end and
-- report every refusal rather than stopping at the first.

\set ON_ERROR_STOP off
\pset pager off

-- ── fixtures ──────────────────────────────────────────────────────────────────────────────────
-- Committed, not merely inserted: the probe transactions below roll back to a savepoint, and a
-- fixture left uncommitted would be gone by the time the last one ran.

insert into organizations (id, name, slug)
values ('11111111-1111-1111-1111-111111111111', 'CLI probe org', 'cli-probe-org')
on conflict (id) do nothing;
insert into organizations (id, name, slug)
values ('44444444-4444-4444-4444-444444444444', 'CLI probe org two', 'cli-probe-org-two')
on conflict (id) do nothing;

insert into users (id, email, display_name, password_hash)
values ('22222222-2222-2222-2222-222222222222', 'cli-probe-owner@example.test', 'Owner', 'x'),
       ('33333333-3333-3333-3333-333333333333', 'cli-probe-other@example.test', 'Other', 'x')
on conflict (id) do nothing;

delete from cli_device_codes where organization_id in (
    '11111111-1111-1111-1111-111111111111', '44444444-4444-4444-4444-444444444444'
);

-- The base row each refusal perturbs by one field.
insert into cli_device_codes (
    organization_id, device_code_hash, user_code, client_name, scopes, expires_at
)
values ('11111111-1111-1111-1111-111111111111', 'hash-probe-1', 'BCDF-2345', 'omnion-cli', '[]',
        now() + interval '15 minutes');

\echo '=== 1. the base row exists ==='
select count(*) as valid_rows
from cli_device_codes
where user_code = 'BCDF-2345'
  and organization_id = '11111111-1111-1111-1111-111111111111';

begin;

-- ── 2. Property: approval is whole ──────────────────────────────────────────────────────────────
-- `approved_by` with no `approved_at` is the half-approval, and it is the dangerous case: the
-- row looks approved (a user is named) while the timestamp saying *when* is missing, so the
-- audit cannot order it against anything.
\echo '=== 2. approved_by without approved_at ==='
savepoint s2;
insert into cli_device_codes (
    organization_id, device_code_hash, user_code, approved_by, client_name, scopes, expires_at
)
values ('11111111-1111-1111-1111-111111111111', 'hash-probe-2', 'GHJK-6789',
        '22222222-2222-2222-2222-222222222222', 'omnion-cli', '[]', now() + interval '15 minutes');
rollback to s2;

-- ── 3. and the other way: a token with no owner ────────────────────────────────────────────────
\echo '=== 3. approved_at without approved_by ==='
savepoint s3;
insert into cli_device_codes (
    organization_id, device_code_hash, user_code, approved_at, client_name, scopes, expires_at
)
values ('11111111-1111-1111-1111-111111111111', 'hash-probe-3', 'LMNP-2345', now(), 'omnion-cli',
        '[]', now() + interval '15 minutes');
rollback to s3;

-- ── 4. Property: the client name is shown on the approval screen ────────────────────────────────
-- Probed with a whitespace-only value rather than null, because whitespace satisfies NOT NULL:
-- this is the row a plain constraint check lets through, and it is the one that renders as a
-- bare code with an empty caption beside it.
\echo '=== 4. an empty client_name ==='
savepoint s4;
insert into cli_device_codes (
    organization_id, device_code_hash, user_code, client_name, scopes, expires_at
)
values ('11111111-1111-1111-1111-111111111111', 'hash-probe-4', 'QRST-2345', '   ', '[]',
        now() + interval '15 minutes');
rollback to s4;

-- ── 5. The user code stays short enough to read ─────────────────────────────────────────────────
\echo '=== 5. a user code outside the length bound ==='
savepoint s5;
insert into cli_device_codes (
    organization_id, device_code_hash, user_code, client_name, scopes, expires_at
)
values ('11111111-1111-1111-1111-111111111111', 'hash-probe-5', 'BC', 'omnion-cli', '[]',
        now() + interval '15 minutes');
rollback to s5;

-- ── 6. The poll interval cannot exceed the ceiling ──────────────────────────────────────────────
-- The slow-down rule grows this column. Without the ceiling a code polled carelessly would
-- carry 86400 and the terminal would appear to hang for a day.
\echo '=== 6. an interval above the 300s ceiling ==='
savepoint s6;
insert into cli_device_codes (
    organization_id, device_code_hash, user_code, client_name, scopes, interval_seconds, expires_at
)
values ('11111111-1111-1111-1111-111111111111', 'hash-probe-6', 'VWXY-2345', 'omnion-cli', '[]',
        86400, now() + interval '15 minutes');
rollback to s6;

-- ── 7. Tenant isolation on the code lookup ──────────────────────────────────────────────────────
-- The code is read aloud, so "this code is unique" must be true *within* a tenant. The index is
-- (organization_id, user_code), so the same short code in two tenants is allowed and the same
-- code twice in one tenant is not.
\echo '=== 7. the same user code twice in one tenant ==='
savepoint s7;
insert into cli_device_codes (
    organization_id, device_code_hash, user_code, client_name, scopes, expires_at
)
values ('11111111-1111-1111-1111-111111111111', 'hash-probe-7', 'BCDF-2345', 'omnion-cli', '[]',
        now() + interval '15 minutes');
rollback to s7;

\echo '=== 8. the same user code in a DIFFERENT tenant is allowed ==='
savepoint s8;
insert into cli_device_codes (
    organization_id, device_code_hash, user_code, client_name, scopes, expires_at
)
values ('44444444-4444-4444-4444-444444444444', 'hash-probe-8', 'BCDF-2345', 'omnion-cli', '[]',
        now() + interval '15 minutes');
select count(*) as same_code_other_tenant
from cli_device_codes where user_code = 'BCDF-2345'
  and organization_id = '44444444-4444-4444-4444-444444444444';
rollback to s8;

-- ── 9. A token must belong to a person and expire after it was written ─────────────────────────
\echo '=== 9. a token that expires before it was created ==='
savepoint s9;
insert into cli_access_tokens (
    organization_id, token_hash, user_id, device_code_id, scopes, expires_at
)
select '11111111-1111-1111-1111-111111111111', 'token-probe-1',
       '22222222-2222-2222-2222-222222222222', id, '[]', now() - interval '1 hour'
from cli_device_codes
where user_code = 'BCDF-2345'
  and organization_id = '11111111-1111-1111-1111-111111111111'
limit 1;
rollback to s9;

-- ── 10. An unknown environment ──────────────────────────────────────────────────────────────────
\echo '=== 10. an unknown environment ==='
savepoint s10;
insert into cli_access_tokens (
    organization_id, token_hash, user_id, device_code_id, environment, scopes, expires_at
)
select '11111111-1111-1111-1111-111111111111', 'token-probe-2',
       '22222222-2222-2222-2222-222222222222', id, 'staging', '[]', now() + interval '1 hour'
from cli_device_codes
where user_code = 'BCDF-2345'
  and organization_id = '11111111-1111-1111-1111-111111111111'
limit 1;
rollback to s10;

-- ── 11. The hash is unique, so a code cannot be stored twice ────────────────────────────────────
\echo '=== 11. the same device_code_hash twice ==='
savepoint s11;
insert into cli_device_codes (
    organization_id, device_code_hash, user_code, client_name, scopes, expires_at
)
values ('11111111-1111-1111-1111-111111111111', 'hash-probe-1', 'ZZXZ-9999', 'omnion-cli', '[]',
        now() + interval '15 minutes');
rollback to s11;

rollback;

-- ── 12. The indexes the code and the sweeper rely on exist ──────────────────────────────────────
\echo '=== 12. the indexes ==='
select indexname from pg_indexes
where tablename in ('cli_device_codes', 'cli_access_tokens')
order by indexname;

-- ── 13. The expiry index is partial, and the predicate is part of what it is for ────────────────
-- `cli_device_codes_live_expiry_idx` is declared `where approved_by is null`. If the predicate
-- were dropped the sweep would also walk approved rows, which are kept for attribution — so the
-- predicate is asserted rather than assumed.
\echo '=== 13. the expiry index is partial on unapproved rows ==='
select indexname,
       indexdef like '%WHERE (approved_by IS NULL)%' as is_partial
from pg_indexes
where indexname = 'cli_device_codes_live_expiry_idx';

-- ── cleanup ────────────────────────────────────────────────────────────────────────────────────
delete from cli_device_codes
where organization_id in ('11111111-1111-1111-1111-111111111111',
                          '44444444-4444-4444-4444-444444444444');
delete from users where id in ('22222222-2222-2222-2222-222222222222',
                               '33333333-3333-3333-3333-333333333333');
delete from organizations where id in ('11111111-1111-1111-1111-111111111111',
                                       '44444444-4444-4444-4444-444444444444');

\echo '=== PROBES RAN: each numbered refusal above must print an ERROR naming its own constraint ==='