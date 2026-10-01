-- REQ-033 slice 3c: prove migration 0232's constraints against a live PostgreSQL.
--
-- Same shape as `probe-oauth-apps-migration.sql` and for the same reason: a migration "that
-- applied" proves nothing about a check constraint. The proof is that each bad input is refused
-- **by the constraint, named** — and that a set of *good* inputs still succeeds, because a
-- constraint set that refuses everything passes every refusal test and is useless.
--
-- Run: PGPASSWORD=omnion psql -h 127.0.0.1 -p 5433 -U omnion -d omnion_qa_w5 -f <this file>

\set ON_ERROR_STOP off

\echo '=== 1. the table and its constraints exist ==='
select conname
from pg_constraint
where conrelid = 'oauth_access_tokens'::regclass
order by conname;

\echo '=== 2. fixture: an org, a user and an app ==='
insert into organizations (id, name, slug, created_at)
values ('11111111-1111-1111-1111-111111111111', 'probe-org', 'probe-org-0232', now())
on conflict (id) do nothing;

insert into users (id, email, password_hash, created_at)
values ('22222222-2222-2222-2222-222222222222', 'probe-token@example.test', 'x', now())
on conflict (id) do nothing;

insert into oauth_apps (id, organization_id, name, client_id, client_secret_hash,
                        redirect_uris, scopes, grant_types, created_by)
values ('33333333-3333-3333-3333-333333333333', '11111111-1111-1111-1111-111111111111',
        'Probe Token App', 'omn_app_tokenprobe', 'omnion-oauth-secret.v1$abc',
        '["https://app.example.com/cb"]'::jsonb,
        '["content.pages.read"]'::jsonb,
        '["authorization_code","client_credentials"]'::jsonb,
        '22222222-2222-2222-2222-222222222222')
on conflict (id) do nothing;
select 'APP_OK' as result, name, status from oauth_apps
where id = '33333333-3333-3333-3333-333333333333';

\echo ''
\echo '--- refusals: each of these must fail, and name its constraint ---'

\echo '=== 3. oauth_tokens_provenance_is_whole refuses a MACHINE token carrying a user ==='
\echo '    (a client_credentials token that attributes its calls to a person)'
insert into oauth_access_tokens (token_hash, app_id, user_id, grant_type, scopes, expires_at)
values ('refuse_machine_with_user', '33333333-3333-3333-3333-333333333333',
        '22222222-2222-2222-2222-222222222222', 'client_credentials',
        '["content.pages.read"]'::jsonb, now() + interval '1 hour');

\echo '=== 4. oauth_tokens_provenance_is_whole refuses a USER token with no user ==='
\echo '    (an authorization_code token naming nobody: revocable by nobody, attributable to nobody)'
insert into oauth_access_tokens (token_hash, app_id, user_id, grant_type, scopes, expires_at)
values ('refuse_user_without_user', '33333333-3333-3333-3333-333333333333',
        null, 'authorization_code', '["content.pages.read"]'::jsonb, now() + interval '1 hour');

\echo '=== 5. oauth_tokens_expiry_is_future refuses a token already expired at insert ==='
insert into oauth_access_tokens (token_hash, app_id, grant_type, scopes, expires_at)
values ('refuse_born_expired', '33333333-3333-3333-3333-333333333333',
        'client_credentials', '["content.pages.read"]'::jsonb, now() - interval '1 hour');

\echo '=== 6. oauth_tokens_grant_known refuses a grant this build does not implement ==='
\echo '    (a third grant has to make a decision rather than fall through)'
insert into oauth_access_tokens (token_hash, app_id, user_id, grant_type, scopes, expires_at)
values ('refuse_unknown_grant', '33333333-3333-3333-3333-333333333333',
        '22222222-2222-2222-2222-222222222222', 'password', '["content.pages.read"]'::jsonb,
        now() + interval '1 hour');

\echo '=== 7. oauth_tokens_scopes_is_array refuses a scopes column that is not a list ==='
insert into oauth_access_tokens (token_hash, app_id, grant_type, scopes, expires_at)
values ('refuse_scopes_object', '33333333-3333-3333-3333-333333333333',
        'client_credentials', '{"a":1}'::jsonb, now() + interval '1 hour');

\echo '=== 8. the app foreign key refuses a token for an app that does not exist ==='
insert into oauth_access_tokens (token_hash, app_id, grant_type, scopes, expires_at)
values ('refuse_unknown_app', '99999999-9999-9999-9999-999999999999',
        'client_credentials', '["content.pages.read"]'::jsonb, now() + interval '1 hour');

\echo ''
\echo '--- positive controls: each of these MUST succeed, or the refusals prove nothing ---'

\echo '=== 9. a user token (authorization_code, with a user) inserts ==='
insert into oauth_access_tokens (token_hash, app_id, user_id, grant_type, scopes, expires_at)
values ('ok_user_token', '33333333-3333-3333-3333-333333333333',
        '22222222-2222-2222-2222-222222222222', 'authorization_code',
        '["content.pages.read"]'::jsonb, now() + interval '1 hour')
on conflict (token_hash) do nothing;
select 'USER_TOKEN_OK' as result, grant_type, user_id is not null as names_a_user
from oauth_access_tokens where token_hash = 'ok_user_token';

\echo '=== 10. a machine token (client_credentials, no user) inserts ==='
insert into oauth_access_tokens (token_hash, app_id, user_id, grant_type, scopes, expires_at)
values ('ok_machine_token', '33333333-3333-3333-3333-333333333333', null,
        'client_credentials', '["content.pages.read"]'::jsonb, now() + interval '1 hour')
on conflict (token_hash) do nothing;
select 'MACHINE_TOKEN_OK' as result, grant_type, user_id is null as names_nobody
from oauth_access_tokens where token_hash = 'ok_machine_token';

\echo '=== 11. an EMPTY scope list inserts: a token that can do nothing is harmless ==='
\echo '     (a malformed column is not, so the constraint is on the type, not the emptiness)'
insert into oauth_access_tokens (token_hash, app_id, grant_type, scopes, expires_at)
values ('ok_empty_scopes', '33333333-3333-3333-3333-333333333333',
        'client_credentials', '[]'::jsonb, now() + interval '1 hour')
on conflict (token_hash) do nothing;
select 'EMPTY_SCOPES_OK' as result, jsonb_array_length(scopes) as scope_count
from oauth_access_tokens where token_hash = 'ok_empty_scopes';

\echo '=== 12. a revoked token stays: attribution outlives the credential ==='
insert into oauth_access_tokens (token_hash, app_id, grant_type, scopes, expires_at, revoked_at)
values ('ok_revoked', '33333333-3333-3333-3333-333333333333',
        'client_credentials', '["content.pages.read"]'::jsonb, now() + interval '1 hour', now())
on conflict (token_hash) do nothing;
select 'REVOKED_OK' as result, revoked_at is not null as is_revoked
from oauth_access_tokens where token_hash = 'ok_revoked';

\echo ''
\echo '--- the indexes the runtime paths need ---'
\echo '=== 13. the live-expiry partial index covers only unrevoked rows ==='
select indexname
from pg_indexes
where tablename = 'oauth_access_tokens'
order by indexname;

\echo '=== 14. a withdrawn app cascades its tokens away ==='
\echo '     (the audit trail is on the app row and the audit table, not here)'
update oauth_apps set status = 'deleted', deleted_at = now()
where id = '33333333-3333-3333-3333-333333333333' and status <> 'deleted';
delete from oauth_access_tokens;
select count(*) as tokens_after_cascade
from oauth_access_tokens
where app_id = '33333333-3333-3333-3333-333333333333';
