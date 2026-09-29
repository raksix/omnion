-- REQ-017 slice 3: promotions — the frozen change set on its way to production.
--
-- The number is 0161 and not 0160 because migration numbers are a SHARED namespace: nine writers
-- push to the one public repository, and w2 took 0160 while this file was being written. Taking
-- the next free number *in this worktree* is how the same collision gets made twice — the symptom
-- is `sqlx VersionMismatch`, which fails every test in the suite at once and reads as a corrupt
-- database rather than as two branches agreeing on a filename. Always scan every worktree's
-- `database/migrations` and take a number above the high-water mark.
--
-- Slices 1 and 2 gave an organization a staging environment and a change set that says what it
-- holds that production does not. Neither writes to production. This table is the record of
-- somebody deciding to do that, and it is the only place in the request where "what was approved"
-- and "what runs" have to be the same bytes — so the change set is *stored*, not referenced.
--
-- Three decisions in the schema, each one load-bearing:
--
--   * `changes` is `jsonb` holding the frozen items, not a foreign key to rows that may move.
--     A promotion that re-read the change set at apply time would be a different promotion from
--     the one the approver read: an edit landing in staging between approval and apply would be
--     published without anybody seeing it. Freezing is the whole safety property here.
--   * `base_updated_at` and `base_digest` per item, not a single snapshot column. A conflict is
--     "production moved on", and that is a per-row question — a promotion of forty items where
--     three production rows were edited is still worth doing if the operator chooses to, as long
--     as the refusal names those three and not the other thirty-seven.
--   * `step_log` is append-only and lives in the row rather than in the audit table, because the
--     dialog's timeline has to survive a refresh and a half-applied state. The audit table answers
--     "who did this"; this answers "how far did it get", which is the question an operator has
--     when the browser was closed mid-deploy.
--
-- No row here is ever updated in place except through its status and its log. A promotion that
-- failed keeps its frozen change set, because "why did this fail" is answered by replaying exactly
-- what it tried to do.

create table promotions (
    id uuid primary key default gen_random_uuid(),
    -- The staging environment the changes come from.
    environment_id uuid not null references environments(id) on delete cascade,
    -- Where they land. Named explicitly rather than derived: production is the only legal target
    -- today, and a column that says so is a column the next feature (staging → staging fan-out)
    -- can widen without a migration that lies about the past.
    target_environment_id uuid not null references environments(id) on delete cascade,
    status text not null default 'pending_approval'
        check (status in ('pending_approval','approved','running','done','failed','cancelled')),
    -- The frozen change set: `{ environment_id, target_environment_id, items: [ { page_id,
    -- site_id, slug, kind, base_updated_at, base_digest }, … ] }`. See the header for why this is
    -- stored rather than joined. It is ONE jsonb object, not an array — the array is `items`
    -- inside it — and the two checks below are the shape guards that say so at the database level,
    -- so a future writer that stores a bare array cannot.
    changes jsonb not null default '{"items":[]}'::jsonb,
    -- The conflicting item ids, computed when the promotion is requested and refreshed at approve
    -- time. Empty is an empty array rather than null: the dialog asks "are there conflicts" on
    -- every render, and `coalesce(conflicts, '[]')` at each of those call sites is a chance to
    -- forget one.
    conflicts jsonb not null default '[]'::jsonb,
    requested_by uuid references users(id) on delete set null,
    approved_by uuid references users(id) on delete set null,
    approved_at timestamptz,
    -- Append-only: `{ step, at, detail }`, one per completed step. The four steps are
    -- `validate`, `apply`, `audit`, `done`; a `failed` row carries the step it stopped at.
    step_log jsonb not null default '[]'::jsonb,
    error text,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    finished_at timestamptz,
    -- A promotion is a statement about content, so it carries no org/site columns: tenancy is
    -- reached through `environment_id`, and a promotion of another organization's environment is
    -- a 404 on that environment rather than a second filter here that could disagree with it.
    constraint promotions_step_log_is_array check (jsonb_typeof(step_log) = 'array'),
    constraint promotions_changes_is_object check (jsonb_typeof(changes) = 'object'),
    -- `items` inside it must be an array. A writer that stores the items as the top level passes
    -- the check above and fails here, which is the point: the decode in `PromotionRow::change_set`
    -- expects the object's shape, and a bare array would decode as a set with no items — a
    -- promotion that silently says "nothing to promote".
    constraint promotions_changes_items_is_array
        check (jsonb_typeof(changes -> 'items') = 'array'),
    constraint promotions_conflicts_is_array check (jsonb_typeof(conflicts) = 'array'),
    -- A `done` or `failed` promotion has an end, and one still waiting does not.
    constraint promotions_finished_matches_status
        check ((finished_at is null) = (status in ('pending_approval','approved','running')))
);

-- The Promotions tab: newest first, newest state first within an environment.
create index promotions_by_environment_recent
    on promotions (environment_id, created_at desc);

-- The in-flight lookup, matching `PromotionStatus::is_in_flight` in `omnion-environment`.
--
-- `pending_approval` and `running` are the two states where the Promotions tab shows a live row,
-- and the status is in the predicate rather than only in the key because the tab's default filter
-- is "not finished" — the list must not degrade into a scan of every promotion the tenant has ever
-- requested. The index is deliberately NOT unique, and the distinction matters:
--
--   * Two `pending_approval` rows are normal. A second change set may legitimately be requested
--     while the first waits for an approver — that is the whole request → approve → apply flow,
--     and a unique index here would refuse the second request outright.
--   * One `running` row is a promise the request makes: "a second request while one is running is
--     refused with a named error". That is enforced below, and it *has* to be by the database:
--     two approvals arriving at once both read an empty table and both would otherwise insert.
create index promotions_in_flight
    on promotions (environment_id, created_at desc)
    where status in ('pending_approval','running');

-- Serialization of the apply itself, enforced where two requests can actually collide.
create unique index promotions_single_running
    on promotions (environment_id) where status = 'running';

-- The detail screen opens one promotion by id from any organization's route table, and the
-- requester/approver columns are read to render "requested by / approved by" without a join.
create index promotions_requested_by on promotions (requested_by);
create index promotions_approved_by on promotions (approved_by);
