-- Omnion · 0177 · backup: queued restore jobs with an abort window (REQ-013, slice 2c)
--
-- Slice 2b runs a restore inside the `POST` and refuses to pretend otherwise: the panel
-- offers a dry run and no cancel button, because a progress bar an operator could stop
-- halfway through would leave a library that is half old and half new with **no safety
-- backup covering the half that was written** — the object store is not transactional and
-- the loop deliberately does not roll back, so a "cancel" that had already written three of
-- four objects could not undo them. Shipping a control that cannot undo is a dead control,
-- and the acceptance criterion for that decision was honest about where a real abort belongs:
-- "a genuine abort belongs with a queued restore".
--
-- This is that queued restore. The shape of the answer, and each decision is a place the
-- obvious shortcut is wrong:
--
--   * **The window is BEFORE the first write, and it is enforced by the schema.** A restore
-- job moves `queued -> running -> (succeeded | failed | aborted)`, and a `cancel_requested`
-- boolean exists so the operator's intent is recorded even when the worker already has the
-- job in hand. The whole point is that the moment the first byte is written there is no
-- cancel, so the state that permits one has to be a state the database can name — a boolean
-- on a `succeeded` row would be a control that says "cancel" about a restore that already
-- finished, and the panel would render it.
--   * **`started_at` is what bounds the window, and it is set by the worker, not the
-- request.** A request that stamped its own `started_at` would be able to age itself out of
-- the cancellable window before a single object had been read, and the abort control would
-- be a control that never works.
--   * **An abort is recorded, not deleted.** The row is the answer to "who cancelled this and
-- when" long after the objects are back, and a `delete` would leave the restore list unable
-- to show a restore that an operator stopped on purpose.
--   * **The job carries the operator's own selection and phrase-hash, not a reference to a
-- wizard session.** A queued restore that had to be re-derived at execution time would be
-- re-derived from *live* data — and the whole safety argument of the preview is that the
-- price was computed once, on the data as it was, and the operator agreed to *that*.
--
-- Numbering: 0177 is above the high-water mark across every worktree (0176 in omnion-w8 at
-- the time of writing). The migration namespace is SHARED — picking the next free number in
-- this tree alone has already produced a duplicate-version `VersionMismatch` that killed
-- every suite at once.
create table backup_restore_jobs (
    id                uuid        primary key default gen_random_uuid(),
    organization_id   uuid        references organizations (id) on delete cascade,
    backup_id         uuid        not null references backups (id) on delete cascade,
    -- The parts the operator left ticked, in request order. **Not** "all of them" when
    -- empty: an empty array is a refusal, exactly as it is in the synchronous route, because
    -- a form that posted nothing and got the whole archive would restore more than it showed.
    parts             text[]      not null,
    -- The confirmation they typed. Stored so the audit trail can say the restore was
    -- authorised, and compared at execution time against the phrase the run's id hashes to —
    -- a job queued under a phrase that has since been superseded must not run.
    confirmation      text        not null default '',
    status            text        not null default 'queued',
    -- The operator's intent to stop, recorded even when the worker is already in hand. A
    -- cancel that is only ever read at the start of the job is a cancel that does nothing
    -- for a job that is halfway through reading the index.
    cancel_requested  boolean     not null default false,
    -- Set by the worker, never by the request. See the header.
    started_at        timestamptz,
    finished_at       timestamptz,
    safety_backup_id  uuid        references backups (id) on delete set null,
    -- The live items the preview priced as dropped, carried from the moment it was agreed to.
    live_dropped      bigint      not null default 0,
    live_matches      bigint      not null default 0,
    result            jsonb,
    error             text,
    created_by        uuid        references users (id) on delete set null,
    created_at        timestamptz not null default now(),
    updated_at        timestamptz not null default now(),
    -- The window itself: a cancellable job has not started. A constraint rather than a
    -- validator in Rust because the worker's own write is the one that has to obey it, and a
    -- validator a worker can forget is a validator that is sometimes false.
    --
    -- **`aborted` is the one terminal state that may have `started_at IS NULL`, and that is
    -- the whole design rather than an oversight.** A worker *does* stamp `started_at` when it
    -- claims a job, and it may then discover the cancel flag — so an abort arrives from a row
    -- that says `running`. But an abort is a statement that **nothing was written**, and the
    -- first version of this constraint required a start for it anyway, which the
    -- abort-specific constraint below directly contradicts. Both cannot hold, so **every
    -- cancellation would have been refused by the database** — and the walk caught it on the
    -- first cancel, which is the only place it can be caught: nothing else in the platform
    -- writes this table.
    check (
        (status = 'queued' and started_at is null and finished_at is null)
        or (status = 'running' and started_at is not null and finished_at is null)
        or (status in ('succeeded', 'failed')
            and started_at is not null and finished_at is not null)
        or (status = 'aborted' and finished_at is not null)
    ),
    check (cardinality(parts) between 1 and 5),
    check (parts <@ array['database', 'media', 'configuration', 'themes', 'plugins']::text[]),
    check (cardinality(parts) = omnion_array_distinct_count(parts)),
    -- An aborted job is a job that was cancelled **while it was still cancellable**, so it
    -- must never have started. Without this, a worker that marked a running job `aborted`
    -- would be a restore that stopped halfway and claimed it was clean.
    check (status <> 'aborted' or (started_at is null and cancel_requested))
);

comment on table backup_restore_jobs is
    'Queued restores. The window before the first write is the only place a restore can be '
    'cancelled, and the schema above is what makes that statement true rather than aspirational.';

alter table backup_restore_jobs
    add constraint backup_restore_jobs_status_known check (
        status in ('queued', 'running', 'succeeded', 'failed', 'aborted')
    ),
    -- A job that finished cleanly has a result and a safety backup to go back to; a job that
    -- did not has a reason. Two states that could both mean either is how a "succeeded" row
    -- with an error string, and no safety backup, reaches a screen.
    add constraint backup_restore_jobs_succeeded_has_result check (
        status <> 'succeeded' or (result is not null and safety_backup_id is not null)
    ),
    add constraint backup_restore_jobs_failed_has_reason check (
        status <> 'failed' or length(btrim(coalesce(error, ''))) > 0
    );

-- A run's jobs, most recent first, for the detail screen and the panel's "cancel" list.
--
-- Scoped by `organization_id` in the `where` and joined to the run, so the same statement
-- that answers "which restores are queued" cannot also answer for another tenant. A `404`
-- rather than a `403`: a restore job's existence is information about another tenant.
create index backup_restore_jobs_run_idx
    on backup_restore_jobs (backup_id, created_at desc);

create index backup_restore_jobs_pending_idx
    on backup_restore_jobs (organization_id, created_at)
    where status = 'queued';

-- At most one restore in flight per run. Not a correctness requirement — two queued restores
-- of the same archive would both be legitimate requests — but a panel that offered "cancel"
-- for both would be a panel whose list is a coin flip, and the operator pressing the wrong
-- one is the outcome this feature exists to prevent. The index is partial for the same
-- reason the query is: a terminal job is history, and history must not block a new attempt.
create unique index backup_restore_jobs_one_live_per_run
    on backup_restore_jobs (backup_id)
    where status in ('queued', 'running');
