-- Omnion · 0119 · A sync run is a row, not a log line (REQ-065, slice 4).
--
-- `0011_iam_advanced.sql` gave the platform `provisioning_tokens` and `provisioning_log`, and
-- REQ-006 shipped the SCIM endpoints that *write* the log. What nobody could answer was the
-- question an operator actually has at 09:00: "did the nightly run work, and if not, for whom?"
-- A log row records that a request arrived; it does not record a *run*, so there is no start, no
-- end, no counts, and nothing to retry. Those three tables are what this migration adds, and
-- nothing here is a column on an existing one.
--
-- Why a run and its errors are separate tables rather than a jsonb array on the run:
--
-- * **The errors outlive the run row in every sense that matters.** A run is a summary an operator
--   scans; the failures are what they act on, one at a time, and `Retry failed` has to address
--   them individually. An array means every retry rewrites the run row, so the record of what
--   failed *the first time* is destroyed by the act of retrying it.
-- * **The count is a summary, not a source of truth.** `error_count` exists so the list screen can
--   sort and filter without a join, and it is written in the same statement that inserts the
--   errors. If the two ever disagree the list is a decoration — which is why the run finalizer is
--   the only writer of both.
--
-- Three decisions that are constraints rather than comments:
--
-- * `kind` distinguishes a **full** sweep from a **delta**, because "users_updated: 0" means
--   something different in each and an operator reading the wrong one of those is misled. A
--   `manual` run is the same shape as a full run; it is named so the row says who asked.
-- * `status` is `'running'` until the run is finished, and the finalizer refuses to move a
--   finished run. A run that can be finished twice is a run whose duration depends on how often
--   somebody opened the page.
-- * `group_links` is on the run rather than derived from `provider_group_links.last_seen_at`:
--   "how many groups did this run touch" and "how many groups exist" are different questions, and
--   the second one is a different screen.
--
-- `provider_group_links` is the fourth table this migration creates. It is the *result* of a sync
-- rather than a record of one, which is why it never had a home: a link row says an external
-- group id maps to a name we last saw, and `member_count`/`last_seen_at` are the two things the
-- panel shows a stale value of. It is upserted by the sync and read by the group rules, and a
-- delete cascades from the provider so a removed directory leaves nothing behind.
--
-- Nothing here holds a credential. `triggered_by` is a user id or null for a schedule, and the
-- message column is a code with a sentence attached, never a raw provider response.

create table directory_sync_runs (
    id uuid primary key default gen_random_uuid(),
    provider_id uuid not null references auth_providers (id) on delete cascade,
    kind text not null default 'full'
        check (kind in ('full', 'delta', 'scim', 'manual')),
    status text not null default 'running'
        check (status in ('running', 'ok', 'partial', 'failed')),
    started_at timestamptz not null default now(),
    finished_at timestamptz,
    users_seen integer not null default 0,
    users_created integer not null default 0,
    users_updated integer not null default 0,
    users_deactivated integer not null default 0,
    groups_seen integer not null default 0,
    error_count integer not null default 0,
    message text,
    triggered_by uuid references users (id) on delete set null,
    -- A run that has finished carries the moment it finished, and a run still going carries
    -- neither: "how long did it take" is unreadable without a second date, and "finished at" on
    -- a run that is still running is a lie about the future.
    constraint directory_sync_runs_finish_shape check (
        (status = 'running' and finished_at is null)
        or (status <> 'running' and finished_at is not null)
    ),
    -- Nothing can be negative. A counter that went to -1 came from an unsigned subtraction, and
    -- the operator reading "-3 users deactivated" has no repair for it.
    constraint directory_sync_runs_counts_check check (
        users_seen >= 0 and users_created >= 0 and users_updated >= 0
        and users_deactivated >= 0 and groups_seen >= 0 and error_count >= 0
    )
);

-- The run list is read newest-first inside one provider, and the only query that grows past a
-- screenful is the one an operator opens *because* something failed.
create index directory_sync_runs_provider_started_idx
    on directory_sync_runs (provider_id, started_at desc);

create index directory_sync_runs_provider_status_idx
    on directory_sync_runs (provider_id, status)
    where status <> 'ok';

create table directory_sync_errors (
    id uuid primary key default gen_random_uuid(),
    run_id uuid not null references directory_sync_runs (id) on delete cascade,
    subject text not null default '',
    code text not null default '',
    message text not null default '',
    created_at timestamptz not null default now(),
    -- Retry addresses one failed subject, so the subject is the key the drawer sorts by and the
    -- attempt column is what proves a retry is a *new* attempt rather than an edit of the old one.
    subject_key text generated always as (
        case when subject = '' then code else subject end
    ) stored
);

create index directory_sync_errors_run_idx
    on directory_sync_errors (run_id, created_at);

-- The same subject may fail twice in one run (a flaky directory, a duplicate entry). The retry
-- button works on *subjects*, so the set is deduplicated by the query rather than by a constraint
-- that would refuse a second honest failure.
create index directory_sync_errors_subject_idx
    on directory_sync_errors (run_id, subject_key);

-- The groups a sync has seen. `member_count` and `last_seen_at` are the two things that go stale
-- quietly: a group the directory deleted keeps its row, its old count and a date from last week,
-- and the panel would show it beside live groups as though it were one of them.
create table provider_group_links (
    id uuid primary key default gen_random_uuid(),
    provider_id uuid not null references auth_providers (id) on delete cascade,
    external_id text not null,
    external_label text not null default '',
    member_count integer not null default 0,
    last_seen_at timestamptz not null default now(),
    synced boolean not null default true,
    unique (provider_id, external_id),
    constraint provider_group_links_member_count_check check (member_count >= 0)
);

-- The group list is read whole for one provider on every sign-in's rule evaluation, so it is
-- keyed by the provider and nothing else: a secondary index on `last_seen_at` would only serve
-- the screen that already has the full list in memory.
create index provider_group_links_provider_label_idx
    on provider_group_links (provider_id, external_label);
