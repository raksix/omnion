-- Omnion · 0044 · media: the scanning pipeline (REQ-010, slice 4)
--
-- Slice 1 gave the library folders, slice 2 a history, slice 3 transformations, storage
-- settings, share links and duplicate detection. The `scan_status` column arrived back in
-- `0025` as `pending` on every row and nothing ever moved it: the library could show a badge
-- and the badge could only ever read `pending`. This migration gives that column a pipeline
-- behind it — a per-site policy, a run log, and the one table that makes quarantine a state
-- rather than a flag.
--
-- Four decisions carry it, and each is a place the obvious shortcut is wrong:
--
--   * **Quarantine is a row, not a status on the file.** A file can be flagged by a scanner
--     *and* held by an operator who said "I looked at it, release it" — a boolean on `media`
--     has room for exactly one of those two facts, and the moment somebody wants to say "this
--     is quarantined but a human has been through it" the flag has to become a row that
--     carries who, when, and why. `media_quarantines` is that row, and the file's
--     `scan_status` stays the *scan* half of the story: the scanner owns it, the operator
--     owns the quarantine.
--   * **The scanner is a client, not an engine.** The request says the antivirus engine and
--     its operations "stay a pluggable scan client, not a bundled engine" (REQ-010, *Out*).
--     So there is no `clamd` dependency, no signature database and no bundled signature
--     format: the row stores an endpoint and the client posts the bytes and reads back a
--     verdict. That is what makes "the scanner is unreachable" a state the platform can
--     survive, which is the one property the ingest rule below depends on.
--   * **A settings row per site, seeded by a trigger.** Same gap `0028` and `0029` closed:
--     a seed in the migration covers the sites that existed when it ran, and a site created
--     afterwards would have a *pleasant* `GET` of platform defaults and an edit that silently
--     writes nothing. Scanning is the one setting where that gap is dangerous rather than
--     merely surprising, because a site with no row reads as "scanning off" to an operator
--     who never turned it off.
--   * **Two nullable columns and nothing else.** `scan_status` and `scan_detail` arrived in
--     `0025`; this migration adds only `scanned_at` and `scan_engine`, both null and both
--     un-backfilled. Every existing row keeps reading `pending` with no timestamp, which is
--     exactly the truth — a file uploaded before the pipeline existed has not been scanned by
--     it. Reclassifying them, or stamping them `now()`, would claim a scan that never
--     happened and make "what did we scan this morning" answer with every file in the
--     library.
--
-- The run log (`media_scan_runs`) is part of this migration rather than slice 5's
-- retention log on purpose: an operator watching a quarantine list needs to be able to say
-- "the last run was at 02:00 and it found nothing", and a pipeline that reports only through
-- the rows it changed cannot answer that on the day nothing was found.

-- ---------------------------------------------------------------------------------------------
-- Per-site scanning policy
-- ---------------------------------------------------------------------------------------------

create table media_scan_settings (
    site_id                 uuid        primary key references sites (id) on delete cascade,
    -- Off by default. A site that has not been pointed at a scanner is a site whose uploads
    -- are served the moment they land, and that must be a decision somebody made rather than
    -- an accident of a missing row.
    enabled                 boolean     not null default false,
    -- Where the scanner lives. A bare origin, same shape as the storage endpoint: a path, a
    -- query or a fragment here produces a 404 at 03:00 that reads as "the scanner is down".
    endpoint                text        not null default '',
    -- Shared secret the scanner expects, held as a *reference*: the name the deployment keeps
    -- the value under in its environment. Same rule as `media_storage_settings` — a settings
    -- table that holds key material is a table that has to be guarded as a credential, and
    -- this one is read by a screen every site owner can open.
    secret_env              text        not null default '',
    -- How long one scan may take, 1-120 seconds. A scanner that never answers must not hold
    -- an upload's response open; the client bounds the wait and records `error`, which is a
    -- state the platform survives rather than one it propagates to the uploader.
    timeout_seconds         integer     not null default 30,
    -- What to do with a file whose scan could not complete: `hold` (do not serve until a human
    -- or a later run clears it) or `serve` (serve it anyway, with the row saying `error`).
    -- The choice belongs to the operator because it is a risk decision about their own site,
    -- and the safe default is the one that can lose a working upload.
    on_error                text        not null default 'hold',
    -- Largest file the scanner will accept, in megabytes, 1-1024. Files above it are marked
    -- `skipped`, not `error`: nobody scanned them and nobody failed to scan them, and
    -- collapsing those two states is how a "clean library" turns out to be an unscanned one.
    max_scan_mb             integer     not null default 100,
    created_at              timestamptz not null default now(),
    updated_at              timestamptz not null default now(),
    constraint media_scan_endpoint_shape
        check (endpoint = '' or endpoint ~ '^https?://[^/?#]+$'),
    constraint media_scan_timeout_range
        check (timeout_seconds between 1 and 120),
    constraint media_scan_on_error_known
        check (on_error in ('hold', 'serve')),
    constraint media_scan_max_mb
        check (max_scan_mb between 1 and 1024),
    -- An enabled scanner with nowhere to send the bytes is a row that fails on first use.
    -- Refusing it at the database means the settings screen cannot save it either.
    constraint media_scan_enabled_has_endpoint
        check (not enabled or endpoint <> '')
);

comment on table media_scan_settings is
    'Per-site virus-scanning policy. The scanner itself is a pluggable client, not a bundled '
    'engine (REQ-010); this row says whether it runs, where it lives and what an unreachable '
    'it means. Credentials are a reference, never a value.';
comment on column media_scan_settings.on_error is
    '`hold` refuses to serve a file whose scan could not complete; `serve` serves it with the '
    'row marked `error`. The default is `hold`, because a scanner outage must not publish an '
    'unscanned file, but an operator running a scanner purely for information may prefer to '
    'keep their site online.';

-- The pipeline needs to say *when* a scan happened, and `0025` never created the column:
-- it gave `scan_status` and `scan_detail` and stopped, so the whole library carried a state
-- with no timestamp. Nullable and without a backfill, because a file uploaded before this
-- migration has genuinely never been scanned and a backfilled `now()` would be a lie.
--
-- This is an `alter table add column`, which takes a brief lock and rewrites nothing in
-- PostgreSQL 11+; the alternative — leaving the column out and storing the instant in
-- `scan_detail` — would make the detail column a place two different facts live, and a
-- report could no longer ask "what files were scanned this morning".
alter table media add column scanned_at timestamptz;
alter table media add column scan_engine text;

create index media_scanned_at_idx on media (scanned_at desc) where scanned_at is not null;

comment on column media.scanned_at is
    'When the scanner last wrote this file''s `scan_status`. Null means the pipeline has never '
    'run against this file, which is not the same as "scanned and found nothing" (REQ-010).';
comment on column media.scan_engine is
    'The engine name the scanner reported for the last verdict, for a report that asks which '
    'engine produced a result.';

-- ---------------------------------------------------------------------------------------------
-- Quarantine
-- ---------------------------------------------------------------------------------------------

-- One row per *quarantine event*, not per file. Releasing a file closes the row rather than
-- deleting it, and a file that is flagged again later gets a second row: "was this ever
-- looked at, and when" is the question a security review actually asks, and a single boolean
-- column cannot answer it for the second event.
create table media_quarantines (
    id                      uuid        primary key default gen_random_uuid(),
    media_id                uuid        not null references media (id) on delete cascade,
    site_id                 uuid        not null references sites (id) on delete cascade,
    -- What the scanner said, verbatim, in words. Not a signature name the platform parses:
    -- the client records what it was told so an operator reads the scanner's own finding and
    -- not a translation of it.
    detail                  text        not null default '',
    -- Which run produced this, when it came from one. Null for a quarantine an operator
    -- raised by hand, which is a real case: the file-detail screen's "quarantine" action.
    run_id                  uuid,
    -- When the file was held and by whom.
    quarantined_at          timestamptz not null default now(),
    quarantined_by          uuid,
    -- When a human let it go, or when it was deleted instead. Both are `released_by_*` /
    -- `released_at`, never a delete: "this was quarantined and then deleted" is a fact about
    -- the system, and deleting the row destroys it.
    released_at             timestamptz,
    released_by             uuid,
    -- Why it was released or deleted: `released`, `deleted` or a free-text reason an operator
    -- typed. The reason is what makes a release auditable — a release with no stated reason
    -- is a release nobody can review.
    release_reason          text        not null default '',
    constraint media_quarantine_detail_present check (detail <> '')
);

-- The live quarantine list is the screen's whole query: one open row per file.
create index media_quarantines_open_idx
    on media_quarantines (site_id, quarantined_at desc) where released_at is null;
-- Every quarantine event of one file, newest first, for the detail screen's history line.
create index media_quarantines_media_idx on media_quarantines (media_id, quarantined_at desc);

comment on table media_quarantines is
    'Quarantine events. A row is *opened* when a scanner flags a file and *closed* when a human '
    'releases or deletes it; rows are never deleted, so the file''s quarantine history survives '
    'a release (REQ-010).';

-- ---------------------------------------------------------------------------------------------
-- The run log
-- ---------------------------------------------------------------------------------------------

create table media_scan_runs (
    id                      uuid        primary key default gen_random_uuid(),
    site_id                 uuid        not null references sites (id) on delete cascade,
    -- `scan` (the ordinary sweep of `pending` rows), `rescan` (an operator re-ran one file),
    -- or `manual` (an operator pressed the button for a whole site).
    kind                    text        not null default 'scan',
    -- What the run found. `clean` and `flagged` are the two verdicts; `error` is "the scanner
    -- did not answer", which is a fact about the run and not about any one file.
    outcome                 text        not null default 'clean',
    -- Every number here is a count of *rows the run wrote*, never a count of rows it looked
    -- at. A run that examined four thousand files and changed nothing reports `scanned = 0`
    -- and `flagged = 0` with `errors = 4000`, which is the sentence an operator needs: the
    -- scanner was down, not the library clean.
    -- `bigint`, not `integer`, and the choice is load-bearing rather than cosmetic: the Rust
    -- row type reads these as `i64`, and sqlx matches a column's type to the field's *exactly*
    -- — an `integer` column read as `i64` fails at decode with "mismatched types", so a run
    -- log that looked right in the database could not be listed at all. A run's counts are
    -- also cumulative in a report, and the platform's other count columns are `bigint`.
    scanned                 bigint      not null default 0,
    flagged                 bigint      not null default 0,
    errors                  bigint      not null default 0,
    skipped                 bigint      not null default 0,
    -- What the client was configured with, recorded so a run can be explained months later:
    -- "why did this file get `error`" answers with the endpoint and the timeout that produced
    -- it, not with whatever the settings say today.
    endpoint                text        not null default '',
    engine                  text        not null default '',
    actor_user_id           uuid,
    started_at              timestamptz not null default now(),
    finished_at             timestamptz,
    constraint media_scan_run_kind_known check (kind in ('scan', 'rescan', 'manual')),
    constraint media_scan_run_outcome_known check (outcome in ('clean', 'flagged', 'error')),
    constraint media_scan_run_counts_sane
        check (scanned >= 0 and flagged >= 0 and errors >= 0 and skipped >= 0),
    -- A run that has not finished has no finish line; a run that claims one has ended. The
    -- check is what makes "the scanner is still running" a queryable state rather than a
    -- guess from a missing timestamp.
    constraint media_scan_run_finished_pair
        check (finished_at is null or finished_at >= started_at)
);

create index media_scan_runs_site_idx on media_scan_runs (site_id, started_at desc);

comment on table media_scan_runs is
    'One row per scanning pass. A run that found nothing still writes a row, because "the last '
    'run was at 02:00 and it was clean" is the answer an operator needs on the day nothing was '
    'found (REQ-010).';

-- The quarantine rows point back at the run that produced them; added after the run table
-- exists so the reference is a plain foreign key rather than a dangling uuid.
alter table media_quarantines
    add constraint media_quarantines_run_fk
    foreign key (run_id) references media_scan_runs (id) on delete set null;

-- ---------------------------------------------------------------------------------------------
-- Seed
-- ---------------------------------------------------------------------------------------------

insert into media_scan_settings (site_id)
select id from sites
on conflict (site_id) do nothing;

-- The seed above covers the sites that existed when this migration ran. A site created
-- afterwards would have no row: `GET /api/v1/media/scan-settings` would answer with
-- platform defaults (a pleasant, readable screen) and a save would insert nothing, so an
-- operator who enabled scanning on a new site would believe it was on. A trigger is the only
-- thing guaranteed to see every site.
create or replace function media_scan_settings_default() returns trigger
language plpgsql as $$
begin
    insert into media_scan_settings (site_id) values (new.id) on conflict do nothing;
    return new;
end $$;

create trigger media_scan_settings_default
    after insert on sites
    for each row execute function media_scan_settings_default();

-- ---------------------------------------------------------------------------------------------
-- The claim query
-- ---------------------------------------------------------------------------------------------

-- `0025` already indexes `(created_at) where scan_status = 'pending'`. The claim is per site,
-- so the site column has to be in front of it or every worker's claim scans the whole
-- installation's backlog to find one site's rows. This is a *new* index rather than a
-- replacement because the old one is still what "everything pending, oldest first" reads.
create index media_scan_claim_idx
    on media (site_id, created_at) where scan_status = 'pending';

comment on column media.scan_status is
    'Scan state written by the scanner client, never by an operator: `pending`, `clean`, '
    '`flagged`, `skipped` (above the scanner''s size ceiling) or `error` (the scanner did not '
    'answer). Whether a flagged file is actually withheld is `media_quarantines`, not this '
    'column (REQ-010).';
comment on column media.scan_detail is
    'What the scanner said, in its own words. A row whose scan is still pending carries the '
    'empty string rather than a placeholder, so "we never scanned it" and "we scanned it and '
    'it said nothing" stay two different rows in a report.';
