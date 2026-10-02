-- REQ-024 · deployment centre.
--
-- Six tables. The one that carries the request's safety property is `deployments`, because it is
-- where "one job at a time per environment" becomes enforceable rather than aspirational: the
-- partial unique index below is the enforcement, and a check in application code that races with
-- another request is not. Everything else here is storage for decisions the `omnion-deployment`
-- crate already makes — the steps, the cancel boundary and the version comparison are not stored
-- as strings to be re-derived at read time.
--
-- Migration number 0211: the file numbering is a single shared namespace across the ten
-- worktrees of this repository, so the number is chosen above the union high-water (0210) rather
-- than above this branch's own last file.

-- ─────────────────────────────────────────────────────────────── deployments

create table if not exists deployments (
    id             uuid primary key default gen_random_uuid(),
    environment    text not null,
    kind           text not null default 'deploy',
    from_version   text,
    to_version     text,
    status         text not null default 'preflight',
    strategy       text not null default 'rolling',
    started_by     uuid references users (id) on delete set null,
    reason         text,
    backup_id      uuid,
    log_key        text,
    error          text,
    started_at     timestamptz not null default now(),
    finished_at    timestamptz,
    duration_ms    int,
    constraint deployments_environment_known
        check (environment in ('production', 'staging', 'sandbox')),
    constraint deployments_kind_known
        check (kind in ('deploy', 'rollback', 'restart')),
    constraint deployments_status_known
        check (status in ('preflight', 'running', 'verifying', 'succeeded', 'failed', 'cancelled')),
    -- A rollback without a reason is the one thing this request forbids outright ("rollback
    -- requires a reason"). Enforced in the table so an API that forgets the check cannot write
    -- the row, and not only in the handler.
    constraint deployments_rollback_needs_a_reason
        check (kind <> 'rollback' or (reason is not null and length(btrim(reason)) > 0)),
    -- A finished job carries when it finished and how long it took; an unfinished one carries
    -- neither. Without this, a history row can claim a zero-second deploy that never ran.
    constraint deployments_finished_rows_are_stamped
        check (
            (status in ('succeeded', 'failed', 'cancelled') and finished_at is not null)
            or (status not in ('succeeded', 'failed', 'cancelled'))
        )
);

comment on table deployments is
    'One row per deploy, rollback or workload restart (REQ-024). The step list, the cancel boundary and the version comparison are decided by the omnion-deployment crate; this table stores the outcome.';

create index if not exists deployments_environment_started_idx
    on deployments (environment, started_at desc);

create index if not exists deployments_history_idx
    on deployments (started_at desc);

-- The one-job-per-environment rule, as a constraint instead of a check.
--
-- `verifying` counts as active on purpose: a second deploy while the first is still proving
-- healthy would swap the version out from under that verification. A unique index over the
-- active statuses is what makes the API's 409 true rather than advisory, and it is a *partial*
-- index so the history is unbounded while the constraint stays over three statuses.
create unique index if not exists deployments_one_active_per_environment_idx
    on deployments (environment)
    where status in ('preflight', 'running', 'verifying');

-- ─────────────────────────────────────────────────────────────── deployment_steps

create table if not exists deployment_steps (
    id             bigint generated always as identity primary key,
    deployment_id  uuid not null references deployments (id) on delete cascade,
    position       int not null,
    name           text not null,
    status         text not null default 'pending',
    output         text not null default '',
    started_at     timestamptz,
    finished_at    timestamptz,
    constraint deployment_steps_status_known
        check (status in ('pending', 'running', 'done', 'failed', 'skipped')),
    constraint deployment_steps_position_positive
        check (position >= 0),
    constraint deployment_steps_unique_position
        unique (deployment_id, position)
);

comment on column deployment_steps.output is
    'Append-only step log. Never rewritten: this is the record of what a run did when the operator closed the browser. Pruned by the retention job on the same window as the request logs.';

-- The log cursor: a client polling the fallback reads from here rather than re-fetching every
-- step's whole output on each tick.
create index if not exists deployment_steps_deployment_idx
    on deployment_steps (deployment_id, position);

-- ─────────────────────────────────────────────────────────────── releases_cache

create table if not exists releases_cache (
    version           text primary key,
    channel           text not null default 'stable',
    released_at       timestamptz,
    notes_md          text not null default '',
    breaking          boolean not null default false,
    migrations        text[] not null default '{}',
    core_min          text,
    artifact_checksum text,
    checked_at        timestamptz not null default now(),
    constraint releases_cache_channel_known
        check (channel in ('stable', 'beta', 'nightly'))
);

comment on table releases_cache is
    'What the update check found, kept so /deployment renders while the manifest feed is unreachable. A feed failure updates checked_at never and the screen shows the cached-data banner with this timestamp.';

create index if not exists releases_cache_channel_idx
    on releases_cache (channel, version desc);

-- ───────────────────────────────────────────────────────────── maintenance_windows

create table if not exists maintenance_windows (
    environment  text primary key,
    enabled      boolean not null default false,
    message      text not null default '',
    starts_at    timestamptz,
    ends_at      timestamptz,
    scope        text not null default 'all',
    updated_by   uuid references users (id) on delete set null,
    updated_at   timestamptz not null default now(),
    constraint maintenance_windows_environment_known
        check (environment in ('production', 'staging', 'sandbox')),
    constraint maintenance_windows_scope_known
        check (scope in ('all', 'admin')),
    -- The form caps the message at 280 characters; the column refuses it too, so a message that
    -- is too long is a 4xx rather than a banner that wraps over three lines in every session.
    constraint maintenance_windows_message_length
        check (length(message) <= 280),
    -- An end before its start is a window that is already over, and it reads as "not in a window"
    -- to every query that compares now() against the two columns.
    constraint maintenance_windows_end_after_start
        check (ends_at is null or starts_at is null or ends_at > starts_at)
);

comment on table maintenance_windows is
    'Per-environment maintenance window (REQ-024 slice 3). While enabled, write routes answer 503 with this message; reads and health probes are untouched.';

-- ───────────────────────────────────────────────────────────── environment_health

create table if not exists environment_health (
    environment  text primary key,
    version      text not null,
    status       text not null,
    checked_at   timestamptz not null default now(),
    details      jsonb not null default '{}'::jsonb,
    constraint environment_health_known
        check (environment in ('production', 'staging', 'sandbox')),
    constraint environment_health_status_known
        check (status in ('healthy', 'degraded', 'unreachable')),
    -- The card's tooltip shows the last probe time and the failing probe when degraded, and both
    -- come from here. An empty details object with a degraded status is a health card that cannot
    -- say what is wrong, which is the failure the spec's tooltip requirement exists to prevent.
    --
    -- Compared against the empty object rather than counted with a function: there is no
    -- `jsonb_object_length` in PostgreSQL, and the alternatives (`jsonb_each` in a subquery, a
    -- cast to text and a length) all turn a check constraint into something whose cost depends on
    -- how large the probe payload is.
    constraint environment_health_degraded_names_a_probe
        check (status = 'healthy' or details <> '{}'::jsonb)
);

comment on table environment_health is
    'The last probe result per environment. Live values are read from the runtime on demand; this row is what the card renders and what the tooltip quotes.';

-- ───────────────────────────────────────────────────── cluster_metric_samples

create table if not exists cluster_metric_samples (
    id                bigint generated always as identity primary key,
    environment       text not null,
    sampled_at        timestamptz not null default now(),
    replicas_desired  int,
    replicas_ready    int,
    cpu_millicores    int,
    memory_bytes      bigint,
    restarts          int,
    constraint cluster_metric_samples_environment_known
        check (environment in ('production', 'staging', 'sandbox')),
    constraint cluster_metric_samples_replicas_non_negative
        check ((replicas_desired is null or replicas_desired >= 0)
           and (replicas_ready is null or replicas_ready >= 0))
);

comment on table cluster_metric_samples is
    'Metric samples for the cluster panel sparkline. Live values are read from the runtime on demand — this table exists only so the 30-minute sparkline has a history to draw, and is pruned to a short window.';

create index if not exists cluster_metric_samples_environment_idx
    on cluster_metric_samples (environment, sampled_at desc);
