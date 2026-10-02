-- REQ-024 slice 1 · the update check's own state.
--
-- Migration 0211 created the tables a *deployment* writes. This file adds the two the update
-- check writes, and both exist for the same reason: the screen must keep rendering when the
-- release feed cannot be reached (the spec's air-gapped and offline requirements, and REQ-036's
-- whole premise), so "when did we last hear from the feed" and "what have we already announced"
-- have to be storage rather than process state.
--
-- Migration number 0212: one shared namespace across the ten worktrees, chosen above the union
-- high-water. Re-check that with
--   ls /mnt/apopic/omnion*/database/migrations/*.sql | grep -o '[0-9]\{4\}_' | sort -u | tail -1
-- before adding the next one.

-- ──────────────────────────────────────────────────────── deployment_update_check

-- One row for the installation. `id` is a fixed primary key rather than a natural key because
-- there is exactly one of these per instance: the card asks "what did the last check say" and
-- the answer is a row, not a history.
create table if not exists deployment_update_check (
    id             smallint primary key default 1,
    channel        text not null default 'stable',
    feed_url       text,
    last_run_at    timestamptz,
    last_finished_at timestamptz,
    -- `completed` or `failed`. Deliberately not nullable after a run: a first-run row exists
    -- before anything has run, and NULL there means "not yet", which is a different fact from
    -- "the last run failed".
    last_status    text,
    last_error     text,
    last_seen      int,
    last_announced text[] not null default '{}',
    constraint deployment_update_check_one_row
        check (id = 1),
    constraint deployment_update_check_channel_known
        check (channel in ('stable', 'beta', 'nightly')),
    constraint deployment_update_check_status_known
        check (last_status is null or last_status in ('completed', 'failed')),
    -- A failed run carries a reason, and a completed run does not: a failure with no reason
    -- renders a banner that says "unreachable" and nothing else, which is the state the spec
    -- asks the screen to avoid.
    constraint deployment_update_check_failure_names_a_reason
        check (last_status is distinct from 'failed' or (last_error is not null and length(btrim(last_error)) > 0)),
    -- A completed run carries when it finished and what it found. Without this, a "completed"
    -- check can claim a successful read of a feed that was never contacted.
    constraint deployment_update_check_completion_is_stamped
        check (
            last_status is distinct from 'completed'
            or (last_finished_at is not null and last_run_at is not null and last_seen is not null)
        )
);

comment on table deployment_update_check is
    'The installation''s single update-check row (REQ-024). GET /api/v1/deployment/checks reads it; the scheduled runner writes it. last_announced is what the last successful run announced, so the card can say "new: 2.5.0" without re-deriving it.';

-- ─────────────────────────────────────────────────── deployment_seen_releases

-- The `update.available` dedupe, as storage.
--
-- The spec requires the event to be emitted "once per newly seen version", and the crate's
-- `SeenSet` is keyed on (channel, version) — so this table has exactly that key. A row per pair
-- rather than a serialized set in the row above, for three reasons: the claim is *survivable*
-- (a restart must not re-announce a hundred releases), it is *inspectable* (an operator can ask
-- "has this instance seen 2.5.0 yet?"), and the insert is a single `on conflict do nothing`
-- whose affected-row count is the dedupe, with no read-modify-write in Rust to get wrong.
create table if not exists deployment_seen_releases (
    channel      text not null,
    version      text not null,
    first_seen_at timestamptz not null default now(),
    primary key (channel, version),
    constraint deployment_seen_releases_channel_known
        check (channel in ('stable', 'beta', 'nightly'))
);

comment on table deployment_seen_releases is
    'Every (channel, version) the update check has already announced. The event is emitted for the rows this insert actually added, so a feed that republishes the same manifest nightly emits nothing after the first night.';

-- Retention is a job, not a cascade: the rows are tiny, and deleting the ones older than a year
-- on the same schedule as the release cache keeps the table honest without a foreign key to
-- cascade from.
create index if not exists deployment_seen_releases_first_seen_idx
    on deployment_seen_releases (first_seen_at);
