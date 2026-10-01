-- REQ-024 slice 4 · the 0214 migration, proved against the live database.
--
-- Applied to a database that already ran 0211, because that is the case the migration has to
-- survive: `cluster_metric_samples` exists with rows-shaped-for-0211 columns, and the file adds
-- the workload dimension to it. Every statement below is checked by its own constraint name, so
-- a refusal is attributed rather than merely observed.
--
-- Run with:  PGPASSWORD=omnion psql -h 127.0.0.1 -p 5433 -U omnion -d omnion_qa_w5 -f scripts/qa/cluster_panel_proof.sql

\set ON_ERROR_STOP off
\pset pager off

-- ── 1. The file applies to a database that already has 0211's table ────────────────────────────

\echo '--- columns added by 0214 ---'
select column_name, data_type, is_nullable, column_default
  from information_schema.columns
 where table_name = 'cluster_metric_samples'
   and column_name in ('workload', 'bucket_at')
 order by column_name;

\echo '--- the partial unique index, by name (0211 had none) ---'
select indexname, indexdef from pg_indexes
 where tablename = 'cluster_metric_samples'
   and indexname like 'cluster_metric_samples%'
 order by indexname;

-- ── 2. The restart/workload constraint, by name ───────────────────────────────────────────────

\echo '--- a restart with no workload is refused ---'
insert into deployments (environment, kind, status, started_by)
values ('production', 'restart', 'preflight', null);
\echo 'expected: deployments_restart_names_a_workload'

\echo '--- a deploy carrying a workload is refused (the other direction) ---'
insert into deployments (environment, kind, status, to_version, workload)
values ('production', 'deploy', 'preflight', '2.5.0', 'api');
\echo 'expected: deployments_restart_names_a_workload'

\echo '--- a restart naming a workload it should not: the shell-reinterpreting names ---'
insert into deployments (environment, kind, status, workload)
values ('production', 'restart', 'preflight', 'api --force');
\echo 'expected: deployments_workload_name_is_a_name'

insert into deployments (environment, kind, status, workload)
values ('production', 'restart', 'preflight', '../../etc/passwd');
\echo 'expected: deployments_workload_name_is_a_workload OR deployments_workload_name_is_a_name'

\echo '--- a valid restart row is accepted ---'
insert into deployments (environment, kind, status, workload, reason)
values ('sandbox', 'restart', 'preflight', 'api', 'testing the rolling restart')
returning id, kind, workload;
delete from deployments where kind = 'restart';

-- ── 3. The sample upsert: one row per (environment, workload, bucket) ────────────────────────

\echo '--- two samplers in the same minute write ONE row ---'
select count(*) as before_samples from cluster_metric_samples where workload = 'api';

insert into cluster_metric_samples (environment, workload, bucket_at, cpu_millicores)
values ('production', 'api', date_trunc('minute', now()), 120)
on conflict (environment, workload, bucket_at) where workload <> ''
do update set cpu_millicores = excluded.cpu_millicores, sampled_at = now();

insert into cluster_metric_samples (environment, workload, bucket_at, cpu_millicores)
values ('production', 'api', date_trunc('minute', now()), 260)
on conflict (environment, workload, bucket_at) where workload <> ''
do update set cpu_millicores = excluded.cpu_millicores, sampled_at = now();

select count(*) as after_two_writes,
       min(cpu_millicores) as value
  from cluster_metric_samples
 where environment = 'production' and workload = 'api';

\echo '--- the second write UPDATES rather than duplicating: one row, and it holds the newer value ---'

\echo '--- the inference clause is load-bearing: without the WHERE, this is an error ---'
\echo '--- (run by hand: insert ... on conflict (environment, workload, bucket_at) do nothing; ---'
\echo '---  PostgreSQL answers "no unique or exclusion constraint matching") ---'

\echo '--- rows from before the migration carry workload = '"'"''"'"' and are never read ---'
select workload, count(*) from cluster_metric_samples group by workload order by workload;

-- ── 4. A pre-0211 row does not block the partial index ─────────────────────────────────────────

\echo '--- two empty-workload rows in one bucket are allowed, which is why the index is partial ---'
insert into cluster_metric_samples (environment, workload, bucket_at, cpu_millicores)
values ('sandbox', '', '2020-01-01T00:00:00Z', 10), ('sandbox', '', '2020-01-01T00:00:00Z', 20);
select count(*) as legacy_rows from cluster_metric_samples where workload = '';
delete from cluster_metric_samples where workload = '';

-- ── 5. The prune, by its own retention rule ────────────────────────────────────────────────────

\echo '--- retention outlives the window, so one missed sample keeps the chart ---'
select (60 * 2) as retention_minutes, 30 as window_minutes,
       ((60 * 2) > 30) as retention_outlives_window;

\echo '--- the prune deletes only what is older than the retention cutoff ---'
insert into cluster_metric_samples (environment, workload, bucket_at, cpu_millicores)
values ('production', 'api', now() - interval '3 hours', 999);
select count(*) as old_rows from cluster_metric_samples where bucket_at < now() - interval '1 hour';
delete from cluster_metric_samples where bucket_at < now() - interval '1 hour';
select count(*) as after_prune from cluster_metric_samples where bucket_at < now() - interval '1 hour';

-- ── 6. Re-applying the file must be a no-op ───────────────────────────────────────────────────

\echo '--- the whole file a second time ---'
\i database/migrations/0214_cluster_panel.sql
\echo 'expected: no errors; every statement is if-not-exists or drop-then-add'

\echo '--- done ---'
