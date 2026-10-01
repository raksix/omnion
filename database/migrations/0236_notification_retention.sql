-- 0236_notification_retention.sql — the delivery log's own retention (REQ-021, slice 7).
--
-- `push::OUTBOX_RETENTION_DAYS` has been published to the panel since the outbox screen
-- shipped: `notification-outbox.tsx` renders "The log goes back 60 days" straight from the
-- `retention_days` the route sends. The number was a promise with nothing behind it —
-- `prune_deliveries` and `prune_stale` are `pub` re-exports of `crates/notifications` with
-- **zero call sites anywhere in the repository**, so the log grew for ever while the screen
-- told an administrator it had a floor. `prune_endpoints` does have a caller (the delivery
-- queue, after a push service answers 404/410); these two do not.
--
-- Three decisions carry this migration, and each is a place the obvious shortcut is wrong:
--
--   * **The clock is the SETTLE instant, not the enqueue instant.** `prune_deliveries`
--     selected on `created_at`, which is written once by `enqueue` and never again — so a
--     delivery queued on day 1 and settled on day 59 was swept on day 60 for being 60 days
--     old, while the operator reading the screen is told the log answers for 60 days. Under
--     the old column the guarantee is measured from the wrong end: a row that took a month to
--     stop retrying lost its history the day after it arrived. `settled_at` is stamped by the
--     two functions that settle a row (`mark_sent`, and the skipped writer), is `null` for a
--     row nobody has finished with, and the predicate is on it — so a `pending` or `failed`
--     row is kept by the same clause that makes it not-yet-settled, with no second status
--     list to drift.
--   * **A row nobody ever settles is still swept, on its own clock.** A delivery that is
--     `pending` for ever — a channel with no transport, a queue nobody drains — would pin its
--     row for ever under a settle-only predicate, which is the opposite of what retention is
--     for. So the sweep takes `coalesce(settled_at, created_at)`: a settled row is judged on
--     when it finished and an abandoned one on when it was written, and neither needs a
--     status list to say which is which.
--   * **The window is a column on the organization, not a global.** The event sweeper set the
--     precedent (`organizations.event_retention_days`, `0123`), and the same argument applies
--     here for a stronger reason: a delivery log is *evidence*. "Show me what was sent to this
--     customer on 3 March" is a question whose answer length is a policy, and a single global
--     number makes one customer's compliance window the whole platform's.
--
-- The index is on `(coalesce(settled_at, created_at))` alone, because that expression is the
-- only thing the sweep ever asks the table — the same argument `0123` made for
-- `events_created_at_idx`. The outbox's own read uses a completely different shape (a join to
-- `notifications` and an ordering by status), so widening this index with those columns would
-- make the sweep slightly cheaper and every write more expensive.

alter table notification_deliveries
    add column if not exists settled_at timestamptz;

comment on column notification_deliveries.settled_at is
    'When a runner finished with this row — delivered, skipped or failed. Null means nobody has settled it; the retention sweep judges such a row by created_at instead.';

-- The sweep's clock, resident in the index so the delete does not sort every settled row in
-- the table before discarding most of them.
create index if not exists notification_deliveries_settled_at_idx
    on notification_deliveries (coalesce(settled_at, created_at));

-- The window, per organization. `not null default 60` matches the constant the panel
-- publishes, and `check` refuses a zero or negative window — which would mean "delete the
-- log immediately", is never what a typed number means, and would leave an operator with an
-- empty delivery screen and a sweep that claims it ran.
--
-- `drop constraint if exists` first because this ledger is shared across nine writers and a
-- constraint is only additive the first time; the second writer must be able to re-apply this
-- file without a 42P07 that reads as a failed boot.
alter table organizations
    add column if not exists notification_retention_days integer not null default 60;

alter table organizations
    drop constraint if exists organizations_notification_retention_days_check;
alter table organizations
    add constraint organizations_notification_retention_days_check
        check (notification_retention_days between 1 and 3650);

-- One row per sweep, written whether or not it deleted anything, for the reason `0123` gives:
-- "the last sweep was at 03:00 and it found nothing" is the sentence an operator needs on
-- the day they ask why a March delivery is still on the screen. A log that only records
-- activity cannot answer it on the day nothing happened.
--
-- `organization_id` is nullable so a sweep of the platform's own (`null`) traffic is
-- representable, which is the same arm `push::OutboxScope::Platform` reads — the two are the
-- same population and a run log that cannot record it is a run log missing half its rows.
create table if not exists notification_retention_runs (
    id              uuid        primary key default gen_random_uuid(),
    organization_id uuid        references organizations (id) on delete cascade,
    started_at      timestamptz not null default now(),
    finished_at     timestamptz,
    window_days     integer     not null,
    cutoff          timestamptz not null,
    deliveries_deleted integer  not null default 0,
    devices_deleted integer      not null default 0,
    failed          integer     not null default 0,
    error           text
);

-- "The last run for this organization" is a newest-first read, and without this it is a sort
-- over every sweep the organization has ever run.
create index if not exists notification_retention_runs_org_started_idx
    on notification_retention_runs (organization_id, started_at desc);

-- A finished run is the only one a screen shows; an unfinished one is a run in flight or a
-- process that died mid-sweep, which is why the runner stamps `finished_at` even when it
-- deleted nothing.
create index if not exists notification_retention_runs_finished_idx
    on notification_retention_runs (finished_at) where finished_at is not null;

-- The backfill, and it is not optional. `mark_sent` has been writing `sent_at` since slice 4,
-- so every settled row already carries the instant this column wants — but as a *different*
-- column, and the sweep cannot read `sent_at` for the `skipped` half (which `enqueue` writes
-- with no timestamp at all). Without this, a fresh installation has an empty `settled_at` and
-- the sweep would judge every historic row by `created_at`, which is the behaviour this
-- migration exists to replace.
--
-- `sent_at` only, because it is the only settle instant that exists in the rows this runs
-- against: a `skipped` row from `enqueue` is *not* past its window on the day it was written
-- (it is born settled, so it is swept as soon as the window passes) and rewriting its
-- `created_at` as its settle instant would delete rows a reader is still asking about.
update notification_deliveries
   set settled_at = sent_at
 where settled_at is null
   and sent_at is not null;