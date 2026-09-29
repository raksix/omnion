-- REQ-133 slice 4 — per-project limits, usage counters and the ownership-transfer audit.
--
-- ## Why this is its own migration and not more of 0164
--
-- 0164 shipped the entity, the members and the scoping. Limits are a separate concern with a
-- separate lifecycle: 0164's tables are read on every automation request, and a limits table is
-- read only by the engine's admission check and by the limits screen. Adding it there would put a
-- rarely-read table in the path of a hot one, and — more importantly — it would make the limits
-- feature impossible to back out of without rolling back the scoping every other writer is building
-- on.
--
-- ## What each decision is for
--
-- * **`max_runs_per_day = 0` means "no limit", not "no runs".** A zero cap that read as zero runs
--   would lock a project out of automation entirely on a fresh install, where every row is created
--   with the defaults below. "Unlimited" is the only reading that is safe for a default value.
-- * **Counters are a row per project per day, upserted atomically.** The REQ says counters "update
--   atomically" and may overshoot by in-flight work, so `on conflict … do update … runs + 1` is
--   the whole enforcement primitive: the increment is inside the statement, so two engines
--   counting the same run cannot read-modify-write over each other.
-- * **`usage_date` is a `date`, not a `timestamptz`.** A counter bucket that follows the
--   engine's clock would roll over mid-day for a caller in another timezone; the project's own
--   reset time is a screen concern (slice 4's limits page), not a storage one.
-- * **The transfer audit is `project_ownership.transferred`, a new action name.** Reusing
--   `project.updated` would make an ownership change indistinguishable from a rename in the audit
--   stream, and REQ-133 asks for two confirmations *because* it is a different act.
create table automation_project_limits (
    project_id             uuid        primary key references automation_projects (id) on delete cascade,
    -- Zero means unlimited. See the header: the default row below is the only place a project can
    -- be born with no cap, and a cap that reads as "no automation" would be a dead platform.
    max_workflows          integer     not null default 0 check (max_workflows >= 0),
    max_credentials        integer     not null default 0 check (max_credentials >= 0),
    max_runs_per_day       integer     not null default 0 check (max_runs_per_day >= 0),
    max_concurrent_runs    integer     not null default 0 check (max_concurrent_runs >= 0),
    -- Fraction of a limit at which the screen warns, as a whole percent. Bounded 1..100 so "warn
    -- at 0 percent" and "warn at 250 percent" cannot both be typed in.
    warn_at_percent        integer     not null default 80 check (warn_at_percent between 1 and 100),
    updated_by             uuid        references users (id) on delete set null,
    updated_at             timestamptz not null default now()
);

-- One row per project per day. The primary key is the atomicity: `runs = usage.runs + 1` inside an
-- upsert is one statement, so concurrent engines counting the same run cannot overwrite each other
-- the way a read-then-write would.
create table automation_project_usage (
    project_id     uuid        not null references automation_projects (id) on delete cascade,
    usage_date     date        not null,
    runs           integer     not null default 0 check (runs >= 0),
    failures       integer     not null default 0 check (failures >= 0),
    compute_ms     bigint      not null default 0 check (compute_ms >= 0),
    updated_at     timestamptz not null default now(),
    primary key (project_id, usage_date)
);

-- The limits screen reads "today" for every project it shows, so the index leads with the bucket.
create index automation_project_usage_date_idx on automation_project_usage (usage_date);

-- Backfill: every existing project gets the instance default limit row, so no project is born
-- without one and no read has to cope with a missing row as a separate state.
--
-- `insert … select … on conflict do nothing` rather than a `left join` check: the table is created
-- empty above, so the conflict arm is unreachable today and exists only for the re-run of this
-- migration on a database where an earlier attempt created some rows.
insert into automation_project_limits (project_id)
select id from automation_projects
on conflict (project_id) do nothing;

-- Ownership transfer is an auditable act with its own action name, so the audit stream can answer
-- "who owns this project" historically rather than by reading the current row. The table is not
-- created: `audit_log` already carries `project_id` (0164) and `metadata` is a jsonb bag, so the
-- event is a row, not a schema change — and a table that only ever held this one event would be a
-- schema that outlives the reason for it.
--
-- Recorded in `move_workflow.rs`'s sibling `transfer_ownership`, which writes:
--   action = 'project_ownership.transferred'
--   target_type = 'automation_project', target_id = <project id>
--   metadata = { "from_user_id": …, "to_user_id": …, "previous_owner_user_id": … }
--
-- The audit action list lives in `crates/audit`, so nothing here constrains it — deliberately: the
-- action name is a string in one place and a check constraint here would make adding the next
-- audited act a migration.
