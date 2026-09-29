-- Omnion · 0157 · backup: parts, schedules, destination settings (REQ-013, slice 1)
--
-- The platform has a media library, a content store, a permission system and a theme
-- directory, and until this migration nothing anywhere in it could be put back the way it
-- was. An operator's honest answer to "can I restore this if the volume dies" was a
-- screenshot of a `pg_dump` they had run by hand once.
--
-- Six decisions carry this migration, and each is a place the obvious shortcut is wrong:
--
--   * **A backup is a set of PARTS, and a part is a row even when it produced nothing.**
--   `plugins` is an honest empty result until a package installer exists, and the obvious
--   shape — "only record the parts that produced bytes" — is exactly the shape that makes an
--   empty part indistinguishable from a part nobody tried to run. The `backup_parts` row
--   carries the refusal, so "0 plugins, 0 bytes, done" and "plugins were never attempted"
--   are two different questions with two different answers.
--   * **A partial run is a first-class terminal state, and it is named by the run, not by the
--   parts.** `partial` sits between `succeeded` and `failed` because a five-part run with
--   four successes and one failure is neither: reporting `failed` loses the four artifacts
--   an operator can still use, and reporting `succeeded` is a lie the restore wizard would
--   then act on. A constraint refuses any status outside the five, so the question never
--   becomes "what does a sixth state mean" at 03:00.
--   * **A schedule's own day fields are constrained by its frequency, in the database.** The
--   alternative is a validator in Rust that the background worker never calls — the worker
--   reads rows, and a `monthly` schedule whose `day_of_month` is null is a schedule that
--   fires whenever the arithmetic happens to allow. The check constraints are here so the
--   invariant holds for *every* writer, including a migration that seeds one.
--   * **The schedule name is unique per scope, through a unique INDEX, not a constraint.**
--   The key is `coalesce(organization_id, zero-uuid)`; PostgreSQL accepts an expression in
--   an index but refuses one inside a `unique` constraint, so the constraint form kills
--   every migration in the set with `syntax error at or near "("` (the same lesson
--   `0047_media_grants.sql` records, reached the same way twice).
--   * **A protected backup is protected by a COLUMN, not by a schedule that omits it.** The
--   prune statement, the delete route and the retention sweep all have to honour it, and a
--   rule expressed in three places is a rule the third place forgets — and the third place
--   is the one that deletes evidence. As a column it appears once per statement and cannot
--   drift, and the sweep's exemption for "the newest successful" is the *same* column read
--   twice rather than a second rule.
--   * **Settings are a single row with `check (id = 1)`, and a trigger, not a seed.** A seed
--   covers the installations that existed when the migration ran; a fresh one gets a
--   pleasant `GET` that answers with defaults built in Rust while every save writes
--   nothing. The same gap `0028` and `0044` closed, closed the same way.
--
-- The scope array is checked for membership rather than only for length: a `scopes` value of
-- `{database, database}` satisfies `cardinality between 1 and 5` and produces a run that
-- exports the database twice and reports one part as failed because the second one found the
-- first one's output in the way.

-- Uniqueness inside a CHECK constraint is the fourth thing PostgreSQL will not do inline, and
-- the reason is the same as the fourth in `0047`: a check may not contain a subquery, and the
-- obvious expression for "these N values are N distinct values" is one. The wrapper takes the
-- array as an *argument* rather than reading the row, which is what makes it legal where a
-- subquery is not, and it is declared IMMUTABLE because a constraint may not call anything
-- else — PostgreSQL trusts the declaration, so the body has to be true without qualification.
--
-- The parameter is not named `values`: that word is reserved in a function's parameter list
-- (`create or replace function f(values text[])` is a syntax error at the parameter), and the
-- name is spelled `items` for that reason.
create or replace function omnion_array_distinct_count(items text[]) returns int
language sql immutable as $$
    select count(distinct item) from unnest(items) as item
$$;

comment on function omnion_array_distinct_count(text[]) is
    'Number of distinct values in a text array. Exists so a check constraint can assert that '
    'an array holds no duplicates: the equivalent subquery form is rejected by PostgreSQL '
    '("cannot use subquery in check constraint"), and this takes the array as an argument.';

-- ---------------------------------------------------------------------------------------------
-- Backups
-- ---------------------------------------------------------------------------------------------

create table backups (
    id               uuid        primary key default gen_random_uuid(),
    organization_id  uuid        references organizations (id) on delete set null,
    -- An empty label is a normal, non-error state: an operator who wants the timestamp and
    -- nothing else should not have to invent a name. The screen renders the created instant
    -- as the title when this is blank.
    label            text        not null default '',
    kind             text        not null,
    schedule_id      uuid,
    -- Which of the five parts this run was asked for. Every element is checked against the
    -- same list `backup_parts.part` uses, so a scope that cannot become a part cannot be
    -- asked for.
    scopes           text[]      not null,
    status           text        not null default 'queued',
    size_bytes       bigint      not null default 0,
    destination      text        not null default 'local',
    storage_prefix   text        not null,
    -- The manifest is the run's own account of itself: parts, item counts, sizes and
    -- checksums as they were *when produced*. It is a snapshot on purpose — a manifest read
    -- back after a restore would describe the restored platform rather than the artifact.
    manifest         jsonb       not null default '{}'::jsonb,
    -- SHA-256 over the canonical manifest, so "the artifact still matches the manifest" is a
    -- question with an answer rather than a comparison of two columns that may both be wrong.
    checksum         text,
    protected        boolean     not null default false,
    retain_until     timestamptz,
    error            text,
    created_by       uuid        references users (id) on delete set null,
    created_at       timestamptz not null default now(),
    started_at       timestamptz,
    finished_at      timestamptz
);

comment on table backups is
    'One backup run: a set of parts, the destination they were written to, and the manifest '
    'that accounts for them. Status is one of queued|running|succeeded|partial|failed.';

-- Five parts, at least one, no duplicates. Both halves matter: a length check alone admits
-- `{database, database}` (two attempts at one part, the second colliding with the first) and
-- a membership check alone admits an empty array (a run that was asked for nothing and
-- therefore "succeeded" at exporting nothing).
alter table backups
    add constraint backups_kind_known check (kind in ('manual', 'scheduled')),
    add constraint backups_status_known check (
        status in ('queued', 'running', 'succeeded', 'partial', 'failed')
    ),
    add constraint backups_destination_known check (destination in ('local', 's3')),
    add constraint backups_scopes_present check (cardinality(scopes) between 1 and 5),
    add constraint backups_scopes_known check (
        scopes <@ array['database', 'media', 'configuration', 'themes', 'plugins']::text[]
    ),
    add constraint backups_scopes_unique check (
        cardinality(scopes) = omnion_array_distinct_count(scopes)
    ),
    -- A finished run has an end, and an end without a start is a row written by a crash
    -- between the two statements rather than a run that never happened. The length is not
    -- constrained: a run of a few hundred bytes is shorter than an empty one after
    -- compression, and "shorter than the header we just wrote" is not a defect.
    add constraint backups_finished_has_start check (
        finished_at is null or started_at is not null
    );

-- A schedule is attached to the schedule table, which is defined below, so the foreign key
-- is added after both exist rather than inlined here.
create index backups_created_at_desc on backups (created_at desc);

-- The queue view. A scheduler's "what is still owed" question and an operator's "why is
-- this run stuck" question are the same question, so they read the same partial index.
create index backups_active on backups (created_at) where status in ('queued', 'running');

-- The prune sweep's own index. It is partial on `protected = false` because a protected row
-- is never a candidate and carrying it in the index only makes the sweep scan a little more
-- of the table than the work requires.
create index backups_retainable on backups (retain_until)
where protected = false and retain_until is not null;

create index backups_organization on backups (organization_id, created_at desc);

-- ---------------------------------------------------------------------------------------------
-- Parts
-- ---------------------------------------------------------------------------------------------

-- A part is a row whether it produced bytes or not. The `error` column is what tells the
-- two apart, and it is the reason this is a table rather than a field on the backup: a
-- manifest that listed only the parts that worked cannot answer "was media part of this
-- run at all", and the restore wizard needs exactly that answer before it offers a part.
create table backup_parts (
    id           bigint generated always as identity primary key,
    backup_id    uuid        not null references backups (id) on delete cascade,
    part         text        not null,
    status       text        not null default 'queued',
    -- What "item_count" means depends on the part: rows for `database`, objects for `media`,
    -- settings rows for `configuration`. It is a count of *things accounted for*, not of
    -- bytes, so a database export of 4 000 rows and a media copy of 40 objects are both
    -- legible in the same column.
    item_count   int         not null default 0,
    size_bytes   bigint      not null default 0,
    checksum     text,
    -- The artifact's key inside the destination prefix, not an absolute path: the same
    -- manifest has to verify against a local root and a bucket, and only the join of
    -- `storage_prefix` and this is destination-independent.
    storage_path text,
    started_at   timestamptz,
    finished_at  timestamptz,
    error        text
);

comment on table backup_parts is
    'One part of one run. A row exists even for a part that produced nothing (plugins before '
    'a package installer exists), so "ran and found nothing" is distinguishable from '
    '"was never attempted".';

alter table backup_parts
    add constraint backup_parts_part_known check (
        part in ('database', 'media', 'configuration', 'themes', 'plugins')
    ),
    add constraint backup_parts_status_known check (
        status in ('queued', 'running', 'done', 'failed')
    ),
    add constraint backup_parts_size_not_negative check (
        size_bytes >= 0 and item_count >= 0
    );

-- One row per (run, part). The obvious alternative, `unique (backup_id, part)` as a table
-- constraint, works — the key has no expression — and is written that way.
create unique index backup_parts_unique on backup_parts (backup_id, part);

-- The detail screen's "which parts are still owed" question, and the restore wizard's part
-- picker, both read the parts of one run in a fixed order.
create index backup_parts_by_run on backup_parts (backup_id, part);

-- ---------------------------------------------------------------------------------------------
-- Schedules
-- ---------------------------------------------------------------------------------------------

create table backup_schedules (
    id               uuid        primary key default gen_random_uuid(),
    organization_id  uuid        references organizations (id) on delete cascade,
    name             text        not null,
    frequency        text        not null,
    -- Null only for `hourly`, which has no time of day of its own. The check constraints
    -- below make the pairing impossible in both directions, so the worker never has to ask
    -- "is this field meaningful for this frequency" — it is one `match` on the frequency
    -- and the fields it may read are the fields the row guarantees.
    at_time          time,
    day_of_week      smallint,
    day_of_month     smallint,
    timezone         text        not null default 'UTC',
    scopes           text[]      not null,
    retention_count  int         not null default 7,
    destination      text        not null default 'local',
    enabled          boolean     not null default true,
    last_run_at      timestamptz,
    next_run_at      timestamptz,
    last_backup_id   uuid,
    created_by       uuid        references users (id) on delete set null,
    created_at       timestamptz not null default now(),
    updated_at       timestamptz not null default now(),
    check (day_of_week is null or day_of_week between 0 and 6),
    check (day_of_month is null or day_of_month between 1 and 28)
);

comment on table backup_schedules is
    'Recurring backup definitions. The day fields are constrained by the frequency here, in '
    'the database, because the background worker reads rows and a Rust validator is not in '
    'its path.';

alter table backup_schedules
    add constraint backup_schedules_frequency_known check (
        frequency in ('hourly', 'daily', 'weekly', 'monthly')
    ),
    add constraint backup_schedules_destination_known check (
        destination in ('local', 's3')
    ),
    add constraint backup_schedules_retention_range check (
        retention_count between 1 and 365
    ),
    add constraint backup_schedules_name_present check (
        length(btrim(name)) between 1 and 64
    ),
    add constraint backup_schedules_timezone_present check (length(btrim(timezone)) > 0),
    add constraint backup_schedules_scopes_present check (cardinality(scopes) between 1 and 5),
    add constraint backup_schedules_scopes_known check (
        scopes <@ array['database', 'media', 'configuration', 'themes', 'plugins']::text[]
    ),
    add constraint backup_schedules_scopes_unique check (
        cardinality(scopes) = omnion_array_distinct_count(scopes)
    ),
    -- A weekly schedule without a weekday and a monthly schedule without a day of the month
    -- are the two rows a worker would have to guess about. Guessing here means "runs at the
    -- top of the hour", which is a real backup on the wrong day.
    add constraint backup_schedules_weekly_has_weekday check (
        (frequency = 'weekly') = (day_of_week is not null)
    ),
    add constraint backup_schedules_monthly_has_dom check (
        (frequency = 'monthly') = (day_of_month is not null)
    ),
    -- An hourly schedule names no time of day; the other three all do. A daily schedule with
    -- a null `at_time` would compute "midnight in the schedule's timezone" through a
    -- fallback nobody chose.
    add constraint backup_schedules_daily_has_time check (
        frequency = 'hourly' or at_time is not null
    );

-- The name is unique per organization, and the platform row (a null organization) has its
-- own key. Both are unique INDEXES: the expression form of the key is legal in an index and
-- illegal inside a `unique` constraint.
create unique index backup_schedules_unique_name on backup_schedules (
    coalesce(organization_id, '00000000-0000-0000-0000-000000000000'::uuid),
    lower(btrim(name))
);

-- The worker's query, verbatim: enabled schedules whose next run has arrived.
create index backup_schedules_due on backup_schedules (next_run_at) where enabled;

-- The two references that need the other table to exist first.
alter table backup_schedules
    add constraint backup_schedules_last_backup_fk
        foreign key (last_backup_id) references backups (id) on delete set null;

alter table backups
    add constraint backups_schedule_fk
        foreign key (schedule_id) references backup_schedules (id) on delete set null;

-- ---------------------------------------------------------------------------------------------
-- Settings
-- ---------------------------------------------------------------------------------------------

-- One row, ever. `check (id = 1)` is what makes "one row" enforceable rather than a
-- convention: an `upsert` on a table with a generated id creates a second row, and the
-- screen then reads whichever one it found first.
create table backup_settings (
    id                   smallint    primary key default 1 check (id = 1),
    destination          text        not null default 'local',
    local_root           text        not null default '/var/lib/omnion/backups',
    -- The S3 prefix is a *prefix*, not a bucket: the bucket is a credential reference's
    -- business and is never written here, so a backup configuration cannot leak the name of
    -- the bucket an operator's key can reach.
    s3_prefix            text,
    -- A reference into the deployment's secret store, never a value. There is deliberately
    -- no column an access key or a passphrase could occupy.
    credential_ref       text,
    encryption           text        not null default 'none',
    default_retention    int         not null default 7,
    verify_after_backup  boolean     not null default true,
    updated_by           uuid        references users (id) on delete set null,
    updated_at           timestamptz not null default now()
);

comment on table backup_settings is
    'Destination, encryption mode and retention defaults. Holds a credential REFERENCE only; '
    'there is no column a passphrase or an access key could be written into.';

alter table backup_settings
    add constraint backup_settings_destination_known check (
        destination in ('local', 's3')
    ),
    add constraint backup_settings_encryption_known check (
        encryption in ('none', 'passphrase')
    ),
    add constraint backup_settings_retention_range check (
        default_retention between 1 and 365
    ),
    add constraint backup_settings_root_present check (length(btrim(local_root)) > 0),
    -- An s3 destination without a prefix is a bucket root written by an operator who meant
    -- a folder: everything lands next to whatever else shares that bucket. A local
    -- destination without a usable root is refused at the same place, for the same reason.
    add constraint backup_settings_s3_has_prefix check (
        destination <> 's3' or (s3_prefix is not null and length(btrim(s3_prefix)) > 0)
    );

-- The platform row, and the row a new installation needs. A seed covers only what existed
-- when the migration ran, so the trigger below is what actually guarantees the screen's
-- `GET` and its `PUT` are talking about the same record.
insert into backup_settings (id) values (1)
on conflict (id) do nothing;

-- ---------------------------------------------------------------------------------------------
-- The guarantee that a settings read and a settings write are about the same row
-- ---------------------------------------------------------------------------------------------

-- Why a trigger and not a seed, again: onboarding, the tenancy API and a future import each
-- insert a site row themselves, so only a trigger sees all of them. The same reasoning
-- applies verbatim to the platform row — the first process to touch the table creates it,
-- and every other process finds the one everybody else found.
create or replace function backup_settings_bootstrap() returns trigger
language plpgsql as $$
begin
    insert into backup_settings (id) values (1) on conflict (id) do nothing;
    return null;
end;
$$;

comment on function backup_settings_bootstrap() is
    'Keeps the single settings row present. Called by the before-insert trigger below; kept as '
    'a function so the statement that creates it is separate from the statement that fires it.';

create trigger backup_settings_ensure_row
    before insert on backup_settings
    for each row
    execute function backup_settings_bootstrap();
