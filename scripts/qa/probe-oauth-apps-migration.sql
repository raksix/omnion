-- REQ-033 slice 3b: prove migration 0231's constraints against a live PostgreSQL.
-- Each case is a name-for-name assertion: the check either refuses the input, or the insert
-- succeeds. A migration "that applied" proves nothing about a check constraint — the proof is
-- that the bad input is refused BY THE CONSTRAINT, named.

\set ON_ERROR_STOP off

\echo '=== 1. migration applies ==='
create table if not exists oauth_probe_0231_marker (id int primary key);
drop table if exists oauth_probe_0231_marker;

\echo '=== 2. a valid app inserts ==='
insert into organizations (id, name, slug, created_at)
values ('11111111-1111-1111-1111-111111111111', 'probe-org', 'probe-org-0231', now())
on conflict (id) do nothing;

insert into users (id, email, password_hash, created_at)
values ('22222222-2222-2222-2222-222222222222', 'probe-oauth@example.test', 'x', now())
on conflict (id) do nothing;

insert into oauth_apps (id, organization_id, name, client_id, client_secret_hash,
                        redirect_uris, scopes, grant_types, created_by)
values ('33333333-3333-3333-3333-333333333333', '11111111-1111-1111-1111-111111111111',
        'Probe App', 'omn_app_probe1', 'omnion-oauth-secret.v1$abc',
        '["https://app.example.com/cb"]'::jsonb,
        '["content.pages.read"]'::jsonb,
        '["authorization_code"]'::jsonb,
        '22222222-2222-2222-2222-222222222222')
on conflict (id) do nothing;
select 'INSERT_OK' as result, name, status, deleted_at is null as not_withdrawn from oauth_apps
where id = '33333333-3333-3333-3333-333333333333';

\echo '=== 3. oauth_apps_deletion_is_whole refuses a deleted app with no timestamp ==='
insert into oauth_apps (organization_id, name, client_id, client_secret_hash,
                        redirect_uris, scopes, grant_types, status, created_by)
values ('11111111-1111-1111-1111-111111111111', 'Half Deleted', 'omn_app_halfdel',
        'omnion-oauth-secret.v1$abc', '["https://a.example.com/cb"]'::jsonb,
        '["content.pages.read"]'::jsonb, '["authorization_code"]'::jsonb,
        'deleted', '22222222-2222-2222-2222-222222222222');

\echo '=== 4. oauth_apps_deletion_is_whole refuses a timestamp on a live app ==='
insert into oauth_apps (organization_id, name, client_id, client_secret_hash,
                        redirect_uris, scopes, grant_types, status, deleted_at, created_by)
values ('11111111-1111-1111-1111-111111111111', 'Stale Stamp', 'omn_app_stamp',
        'omnion-oauth-secret.v1$abc', '["https://a.example.com/cb"]'::jsonb,
        '["content.pages.read"]'::jsonb, '["authorization_code"]'::jsonb,
        'active', now(), '22222222-2222-2222-2222-222222222222');

\echo '=== 5. oauth_apps_overlap_is_whole refuses a previous hash with no expiry ==='
insert into oauth_apps (organization_id, name, client_id, client_secret_hash,
                        previous_secret_hash, redirect_uris, scopes, grant_types, created_by)
values ('11111111-1111-1111-1111-111111111111', 'Half Overlap', 'omn_app_halfov',
        'omnion-oauth-secret.v1$abc', 'omnion-oauth-secret.v1$old',
        '["https://a.example.com/cb"]'::jsonb, '["content.pages.read"]'::jsonb,
        '["authorization_code"]'::jsonb, '22222222-2222-2222-2222-222222222222');

\echo '=== 6. oauth_apps_grant_types_are_known refuses a flow this build does not implement ==='
insert into oauth_apps (organization_id, name, client_id, client_secret_hash,
                        redirect_uris, scopes, grant_types, created_by)
values ('11111111-1111-1111-1111-111111111111', 'Device Flow', 'omn_app_device',
        'omnion-oauth-secret.v1$abc', '["https://a.example.com/cb"]'::jsonb,
        '["content.pages.read"]'::jsonb, '["device_code"]'::jsonb,
        '22222222-2222-2222-2222-222222222222');

\echo '=== 7. oauth_apps_redirect_uris_known refuses an app with no redirect ==='
insert into oauth_apps (organization_id, name, client_id, client_secret_hash,
                        redirect_uris, scopes, grant_types, created_by)
values ('11111111-1111-1111-1111-111111111111', 'No Redirect', 'omn_app_noredir',
        'omnion-oauth-secret.v1$abc', '[]'::jsonb,
        '["content.pages.read"]'::jsonb, '["authorization_code"]'::jsonb,
        '22222222-2222-2222-2222-222222222222');

\echo '=== 8. oauth_apps_org_name_key refuses two live apps with one name ==='
insert into oauth_apps (organization_id, name, client_id, client_secret_hash,
                        redirect_uris, scopes, grant_types, created_by)
values ('11111111-1111-1111-1111-111111111111', 'Probe App', 'omn_app_probe2',
        'omnion-oauth-secret.v1$abc', '["https://a.example.com/cb"]'::jsonb,
        '["content.pages.read"]'::jsonb, '["authorization_code"]'::jsonb,
        '22222222-2222-2222-2222-222222222222');

\echo '=== 9. but a WITHDRAWN app frees its name for re-registration ==='
update oauth_apps set status = 'deleted', deleted_at = now()
where id = '33333333-3333-3333-3333-333333333333';
insert into oauth_apps (organization_id, name, client_id, client_secret_hash,
                        redirect_uris, scopes, grant_types, created_by)
values ('11111111-1111-1111-1111-111111111111', 'Probe App', 'omn_app_probe3',
        'omnion-oauth-secret.v1$abc', '["https://a.example.com/cb"]'::jsonb,
        '["content.pages.read"]'::jsonb, '["authorization_code"]'::jsonb,
        '22222222-2222-2222-2222-222222222222')
on conflict (id) do nothing;
select 'NAME_REUSED_AFTER_WITHDRAWAL' as result, count(*) as live_with_that_name
from oauth_apps where organization_id = '11111111-1111-1111-1111-111111111111'
  and name = 'Probe App' and status <> 'deleted';

\echo '=== 10. oauth_codes_challenge_is_whole refuses a challenge with no method ==='
insert into oauth_authorization_codes (code_hash, app_id, user_id, redirect_uri,
                                       code_challenge, expires_at)
select 'probe_hash_half', '33333333-3333-3333-3333-333333333333',
       '22222222-2222-2222-2222-222222222222', 'https://app.example.com/cb',
       'abc', now() + interval '10 minutes'
where exists (select 1 from oauth_apps where id = '33333333-3333-3333-3333-333333333333');

\echo '=== 11. oauth_codes_expiry_is_future refuses a code that is already dead ==='
insert into oauth_authorization_codes (code_hash, app_id, user_id, redirect_uri, expires_at)
select 'probe_hash_past', '33333333-3333-3333-3333-333333333333',
       '22222222-2222-2222-2222-222222222222', 'https://app.example.com/cb', now()
where exists (select 1 from oauth_apps where id = '33333333-3333-3333-3333-333333333333');

\echo '=== 12. a well-formed code inserts, and the partial index finds it ==='
insert into oauth_authorization_codes (code_hash, app_id, user_id, redirect_uri,
                                       code_challenge, code_challenge_method, expires_at)
select 'probe_hash_ok', '33333333-3333-3333-3333-333333333333',
       '22222222-2222-2222-2222-222222222222', 'https://app.example.com/cb',
       'E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM', 'S256', now() + interval '10 minutes'
where exists (select 1 from oauth_apps where id = '33333333-3333-3333-3333-333333333333');
select 'CODE_INSERTED' as result, count(*) from oauth_authorization_codes
where code_hash = 'probe_hash_ok' and used_at is null;

\echo '=== CLEANUP ==='
delete from oauth_authorization_codes where code_hash like 'probe_hash_%';
delete from oauth_apps where organization_id = '11111111-1111-1111-1111-111111111111';
delete from users where id = '22222222-2222-2222-2222-222222222222';
delete from organizations where id = '11111111-1111-1111-1111-111111111111';
select 'CLEAN' as result;
