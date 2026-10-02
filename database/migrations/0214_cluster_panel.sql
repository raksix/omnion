-- REQ-024 slice 4 · cluster panel.
--
-- `0211` created `cluster_metric_samples` as an environment-wide table with a nullable
-- `environment` key and no workload column. The panel the spec asks for is a **per-workload**
-- table ("workload · replicas desired/ready · CPU request/limit/usage · memory …"), and a
-- sparkline that is not per-workload is the utilisation of the whole cluster drawn on every row —
-- six identical lines under six different workloads, which is worse than no sparkline because it
-- looks like data.
--
-- So this migration adds the workload dimension and the sampled_at bucketing, and keeps every
-- `0211` row readable. Nothing is dropped: `cluster_metric_samples` had no writer before this
-- slice, so the table is empty in every environment, and an `alter table … add column if not
-- exists` is idempotent for a file that may be applied to an instance that already ran `0211` and
-- to a fresh database whose `0211` runs moments earlier in the same transaction chain.
--
-- Number 0214: the file numbering is one shared namespace across this repository's ten worktrees,
-- so the number is taken above the union high-water (0213, held by wave 4's HR onboarding), not
-- above this branch's own last file (0212).

alter table cluster_metric_samples
    add column if not exists workload text not null default '';

comment on column cluster_metric_samples.workload is
    'The workload the sample belongs to. Empty on rows written before the per-workload panel existed; those rows are never read, because a series with no workload cannot be attributed to a row of the table.';

-- The bucket. `0211` sampled at `now()` per row, which makes every sample unique and lets a
-- restarted scheduler append a second sample for a minute it already has; two samplers racing
-- would then draw twice the variance. The bucket floor is the fix, and the unique index below is
-- what enforces it — a re-run *updates* its bucket rather than adding a neighbour.
alter table cluster_metric_samples
    add column if not exists bucket_at timestamptz not null default now();

comment on column cluster_metric_samples.bucket_at is
    'Sample time floored to the sampling interval. Two samplers in the same minute write the same bucket and the second updates it.';

-- One sample per (environment, workload, minute).
--
-- Partial, not a plain unique: rows written before this migration carry `workload = ''`, and
-- several of those in the same minute would violate a full unique index on a database that ran
-- `0211` against an older build. Excluding the empty workload keeps the migration applicable to
-- every instance rather than only to a fresh one — the same reason every statement above is
-- `if not exists`.
create unique index if not exists cluster_metric_samples_bucket_idx
    on cluster_metric_samples (environment, workload, bucket_at)
    where workload <> '';

-- The panel's own read: one workload's series, newest first, inside the window.
create index if not exists cluster_metric_samples_workload_recent_idx
    on cluster_metric_samples (environment, workload, bucket_at desc)
    where workload <> '';

-- A restart is a job like any other, so it needs somewhere to say which workload it was aimed
-- at. Nullable, and only `restart` rows carry it: a `deploy` has a version instead, and a column
-- that was mandatory would make the commonest two rows in the table carry a value that means
-- nothing. The constraint is what keeps a row honest — a restart with no workload, or a deploy
-- with one, is a row the history screen would render as a restart of nothing.
alter table deployments
    add column if not exists workload text;

comment on column deployments.workload is
    'The workload a restart job targets (REQ-024 slice 4). Set only for kind = ''restart''; a deploy or rollback records versions instead.';

alter table deployments
    drop constraint if exists deployments_restart_names_a_workload;

alter table deployments
    add constraint deployments_restart_names_a_workload
    check (
        (kind = 'restart' and workload is not null and length(btrim(workload)) > 0)
        or (kind <> 'restart' and workload is null)
    );

-- The name is validated by the crate before it reaches here, and this is the same rule as a
-- constraint so a second writer cannot skip it: letters, digits and `-._:`, which is what a
-- Kubernetes object name may contain and what a shell argument, a URL path segment and a history
-- cell can all carry unchanged. Anything else — a space, a slash, a leading dash — is a name that
-- means something different in one of those three places.
alter table deployments
    drop constraint if exists deployments_workload_name_is_a_name;

alter table deployments
    add constraint deployments_workload_name_is_a_name
    check (
        workload is null
        or (length(workload) <= 128
            and workload ~ '^[A-Za-z0-9._:-]+$')
    );
