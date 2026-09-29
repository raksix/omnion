-- Omnion · 0123 · a SCIM push needs a run, not just a log (REQ-065, slice 4 part 3).
--
-- `0119` gave the platform a run, and the SCIM endpoints write every provisioning request to
-- `provisioning_log` — two ledgers describing one directory and agreeing about nothing. The log
-- is a good record of *requests* and a bad record of *work*:
--
-- * it has no run, so "did the connector's overnight push work" has no answer;
-- * it cannot say `partial`, and "four users created, one refused" is a normal hour for a
--   connector whose IdP keeps sending a user whose externalId is already taken — a boolean "ok"
--   over that is the sentence an operator acts on by doing nothing;
-- * and it is invisible from the provider screen, so the two describe the same directory and
--   disagree.
--
-- One column, not a new table. A SCIM push is a stream of independent HTTP requests with no
-- transaction and no natural end, so the platform bounds a run by *idleness* rather than by a
-- start and a finish: a run that has not been touched for fifteen minutes is closed by the next
-- request that arrives. `last_seen_at` is that clock.
--
-- It is deliberately NOT `greatest(started_at, finished_at)`: a finished run is closed, and a
-- closed run must not look live again because something touched it. The column is a liveness
-- signal for an OPEN run and nothing else, and the reader is `status = 'running'` in the same
-- query that reads it.
--
-- Backfilled from `started_at` so every run that already exists has a liveness that matches its
-- own beginning rather than null. A null here would read as "never seen", which is a third
-- state nobody asked for and one that would close a live run on the next request.
alter table directory_sync_runs
    add column last_seen_at timestamptz;

update directory_sync_runs set last_seen_at = started_at where last_seen_at is null;

alter table directory_sync_runs
    alter column last_seen_at set not null,
    alter column last_seen_at set default now();

-- The reader is "the open run for this provider", so the index is on the open run alone. A
-- partial index over a status that is `'running'` for a handful of rows at a time is a few
-- pages on a table that otherwise grows for ever.
create index directory_sync_runs_open_idx
    on directory_sync_runs (provider_id, last_seen_at desc)
    where status = 'running';

-- The SCIM push counts itself from the log, and the log is identified by the action prefix the
-- SCIM module writes. This records that prefix as a value rather than leaving it a literal in
-- three queries that have to agree — a prefix that differs in one of them is a run that counts
-- nothing and reports `error_count: 0` over three refusals.
comment on column directory_sync_runs.last_seen_at is
    'When a SCIM push last wrote to this run. Liveness for an open run only; closed runs keep the value they died with.';
