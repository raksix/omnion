-- Evidence for migration 0148: a staging environment can hold a page with the same slug as
-- production, and the per-environment natural keys still refuse a duplicate inside one
-- environment.
--
-- Run against a database that has applied the whole migration set.
--   psql -h 127.0.0.1 -p 5433 -U omnion -d <db> -f scripts/qa/environment-key-proof.sql

\set ON_ERROR_STOP on

-- Self-cleaning, so this is a gate that can be re-run rather than a one-shot demonstration.
delete from organizations where slug = 'key-proof';

insert into organizations (name, slug) values ('Key proof', 'key-proof');
insert into sites (organization_id, key, name) select id, 'proof', 'Proof site' from organizations;

-- The production page the staging copy will mirror. No environment named: the 0147 trigger
-- resolves it, which is the same path the content crate takes.
insert into pages (site_id, slug) select id, 'about' from sites;

-- A staging environment (the after-insert trigger gave the organization its production one).
insert into environments (organization_id, key, name, type, status)
select id, 'staging-2', 'Staging', 'staging', 'active' from organizations;

\echo '--- a staging page with the same slug coexists with the production one'
insert into pages (site_id, slug, environment_id)
select s.id, 'about', e.id from sites s, environments e where e.type = 'staging';
select p.slug, e.type, count(*) as rows
from pages p join environments e on e.id = p.environment_id
group by p.slug, e.type order by e.type;

\echo '--- a second staging page with that slug is refused (the key is per environment)'
\set ON_ERROR_STOP off
insert into pages (site_id, slug, environment_id)
select s.id, 'about', e.id from sites s, environments e where e.type = 'staging';
\set ON_ERROR_STOP on

\echo '--- and a duplicate slug inside production is refused too'
\set ON_ERROR_STOP off
insert into pages (site_id, slug)
select id, 'about' from sites;
\set ON_ERROR_STOP on

\echo '--- one production page, one staging page: the two environments hold distinct rows'
select e.type, p.id, p.slug from pages p join environments e on e.id = p.environment_id order by e.type;
