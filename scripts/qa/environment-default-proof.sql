-- Evidence for migration 0147: the NOT NULL invariant from 0145 is satisfiable by a write
-- that names no environment, and an explicit environment is still honoured.
--
-- Run against a database that has applied the whole migration set.
--   psql -h 127.0.0.1 -p 5433 -U omnion -d <db> -f scripts/qa/environment-default-proof.sql

\set ON_ERROR_STOP on

-- Self-cleaning, so this is a gate that can be run against a database that already has rows and
-- still proves the same thing. A proof that only works once on an empty database is a one-shot
-- demonstration, not evidence.
delete from organizations where slug in ('env-proof-a', 'env-proof-b', 'env-proof-c');

-- Two organizations, so a constant default would be visibly wrong: rows must land in their own
-- organization's production environment, not in whichever one was created first.
insert into organizations (name, slug) values ('Env proof A', 'env-proof-a'), ('Env proof B', 'env-proof-b');
insert into sites (organization_id, key, name)
select o.id, 'proof', 'Proof site' from organizations o where o.slug = 'env-proof-a';
insert into sites (organization_id, key, name)
select o.id, 'proof', 'Proof site' from organizations o where o.slug = 'env-proof-b';

\echo '--- the trigger created a production environment for each new organization'
select o.slug, e.key, e.type
from organizations o join environments e on e.organization_id = o.id
where o.slug like 'env-proof-%'
order by o.slug;

\echo '--- a page insert that names no environment lands in its own org production env'
-- One page per organization, both naming no environment. `limit 1` would silently reduce this to
-- a single site and the check below would then prove nothing about the second tenant.
insert into pages (site_id, slug)
select s.id, 'proof-' || o.slug
from sites s join organizations o on o.id = s.organization_id
where o.slug in ('env-proof-a', 'env-proof-b');
\echo '--- a page insert naming an explicit environment (a clone) keeps it'
insert into environments (organization_id, key, name, type, status)
select id, 'staging', 'Staging', 'staging', 'active' from organizations where slug = 'env-proof-a';
insert into pages (site_id, slug, environment_id)
select (select s.id from sites s join organizations o on o.id = s.organization_id
        where o.slug = 'env-proof-a' limit 1), 'proof-staged',
       (select id from environments where key = 'staging' limit 1);
\echo '--- two organizations, two different production environments, no cross-tenant leak'
select o.slug as org, e.key as env, count(p.id) as pages
from pages p
join sites s on s.id = p.site_id
join organizations o on o.id = s.organization_id
join environments e on e.id = p.environment_id
group by o.slug, e.key
order by o.slug, e.key;

\echo '--- the corrupt case is now unreachable: a new organization cannot exist without one'
-- The previous revision of this proof ended here, by creating an organization with no production
-- environment and asserting that a page insert into it was refused. That case is no longer
-- constructible -- the after-insert trigger gives every organization one -- so what is asserted
-- now is the *reason* it is unreachable: a second production environment is still refused, which
-- is the half of "exactly one" the partial unique index owns.
-- A different key, so the refusal can only come from the partial unique index on
-- `type = 'production'` and not from the `(organization_id, key)` unique constraint. Using the
-- same key twice would let the second statement report success (`on conflict do nothing` on a
-- *different* statement is an error, but a shared key makes the two causes indistinguishable).
insert into environments (organization_id, key, name, type, status)
select o.id, 'production-2', 'Second production', 'production', 'active'
from organizations o where o.slug = 'env-proof-a';
\echo '--- the refusal below is expected: the partial unique index owns "exactly one"'
\set ON_ERROR_STOP off
insert into environments (organization_id, key, name, type, status)
select o.id, 'production-2', 'Second production', 'production', 'active'
from organizations o where o.slug = 'env-proof-a';
\set ON_ERROR_STOP on
select count(*) as production_rows
from environments e join organizations o on o.id = e.organization_id
where o.slug = 'env-proof-a' and e.type = 'production';
