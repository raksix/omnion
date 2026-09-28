-- Omnion · 0031 · automation operations: rate window, concurrency policy, last error
--
-- Slice 4 of the automation depth pass (docs/requests/REQ-003). Slices 1–3 gave the
-- layer its trigger library, its action library and its approvals; this migration gives
-- a rule the two bounds that make it safe to *leave armed*, plus the one column the
-- operations surface reads when a rule is not doing what its author expects:
--
--   * `workflows.rate_limit_per_hour` — how many runs a rule may start in a rolling
--     hour, counted in `workflow_rate_windows`. A rule on `user.created` that sends an
--     email is fine until a signup loop starts sending forty thousand of them; the
--     window is the thing that turns that into a refusal instead of an incident.
--   * `workflows.concurrency` — what a second trigger does while the first run is
--     still going: `queue` (the run waits) or `skip` (the trigger is dropped). A rule
--     whose action is slow needs one of the two; without a policy the answer is
--     "however many arrive", which is how one rule ends up running six times over
--     the same row.
--   * `workflows.last_error` — the message a bound produced the last time it refused
--     a run, so the rule's own list can say *why* it has not fired since Tuesday
--     instead of only counting its runs.
--
-- The bound is enforced in the SAME TRANSACTION that starts the run (see
-- `omnion_automation::limits`), because a check that runs before the insert is two
-- statements and anything between them — a second API instance, a sweep, a second
-- tab — turns one decision into two. The window row is the lock that makes the two
-- facts one: `insert … on conflict do nothing` then `select … for update` guarantees
-- a row exists and that two callers serialise on it, so both the rate count and the
-- concurrency count are read under the same lock.
--
-- `automation_hook_windows` (migration 0020) stays the *inbound* surface's own window,
-- keyed by rule and independent of this one: an inbound caller must never be able to
-- spend the budget of the rule's own event trigger, and vice versa.
--
-- Released migrations are append-only (docs/05-VERSIONING.md).

-- ---------------------------------------------------------------------------------------------
-- workflows: the two bounds and the one line about them
-- ---------------------------------------------------------------------------------------------

alter table workflows add column rate_limit_per_hour integer not null default 60;
alter table workflows add column concurrency text not null default 'queue';
alter table workflows add column last_error text;

-- The bounds the panel may set, enforced by the database as well as by the layer:
-- a rule that stored 0 or a million would be a rule the panel cannot explain.
alter table workflows add constraint workflows_rate_limit_positive
    check (rate_limit_per_hour between 1 and 10000);

alter table workflows add constraint workflows_concurrency_valid
    check (concurrency in ('queue', 'skip'));

-- The last refusal names the bound that produced it, so a run history that says
-- "nothing happened" can always be joined to a sentence. A null is the ordinary
-- state: the rule has never been refused.
alter table workflows add constraint workflows_last_error_bounded
    check (last_error is null or char_length(last_error) <= 400);

-- ---------------------------------------------------------------------------------------------
-- workflow_rate_windows: the rolling hour, one row per rule
-- ---------------------------------------------------------------------------------------------

-- The window is a *counter*, not a list of runs: a rule that starts a thousand runs
-- in an hour holds one integer, and the window's reset is a timestamp compared
-- against `now()` rather than a scheduled job. The row is created on first use (the
-- guard's `insert … on conflict do nothing`) so the lock always exists — a guard that
-- had to insert the row it was about to lock has a window between the two.
create table workflow_rate_windows (
    workflow_id  uuid        primary key references workflows (id) on delete cascade,
    window_start timestamptz not null default now(),
    run_count    integer     not null default 0,
    constraint workflow_rate_windows_non_negative check (run_count >= 0)
);

-- ---------------------------------------------------------------------------------------------
-- The count a rule's operations view reads without taking the lock
-- ---------------------------------------------------------------------------------------------

-- The guarded read is `for update`; this is the same question asked without a lock,
-- for the panel's "Runs in the last hour: 7 of 60" line. A read that is a few
-- seconds stale is a counter; a read that is stale in a *decision* is a bug, and the
-- decision is the one that takes the lock.
create index workflow_rate_windows_start_idx
    on workflow_rate_windows (window_start);

-- The rule list orders by "last fired" and the operations surface filters on the last
-- refusal; both are reads of the workflow row itself, and a partial index keeps the
-- arm of the query that means "something is wrong here" cheap.
create index workflows_last_error_idx
    on workflows (organization_id, updated_at desc)
    where last_error is not null;
