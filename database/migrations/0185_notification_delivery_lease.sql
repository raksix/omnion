-- 0185_notification_delivery_lease.sql — the claim window for a notification delivery
-- (REQ-021, slice 4).
--
-- `notification_deliveries` shipped with slice 1 and every later slice *read* it — the outbox
-- lists it, the retry button re-queues it, the channel filter joins it — but nothing ever
-- wrote a row or claimed one. The table had no lease column, and that is the specific reason a
-- runner could not be written against it: a claim has to be able to say "somebody is working on
-- this row right now", and it has to be able to say "nobody is any more" after a process dies
-- mid-send. Without a timestamp for the second half, a crashed runner would strand the row
-- either by re-sending it forever or by leaving it claimed for good, and there is no column in
-- which to record which.
--
-- The shape is the same one `webhook_deliveries` uses, deliberately: claim = `for update skip
-- locked` over the due rows, stamp `claimed_at`, increment `attempts` in the same statement, and
-- treat a claim older than the lease as abandoned. The attempt that died still counts, which is
-- what stops a crash loop from retrying forever.
--
-- **The index gains the claim column because the due query gains the clause.** The existing
-- partial index is on `(next_attempt_at, created_at) where status = 'pending'`, and the claim
-- reads that predicate plus `claimed_at <= now() - interval`. Leaving the index alone would
-- mean Postgres sorts every pending row by next_attempt and filters the leases afterwards,
-- which is exactly the shape that degrades on the table an outbox grows fastest.
--
-- **The column is nullable on purpose.** A row nobody has claimed yet has no claim, and
-- `claimed_at is null` is that state — writing a sentinel timestamp for "never" would make
-- "claimed 1970" and "never claimed" indistinguishable in every query that reads it.

alter table notification_deliveries
    add column claimed_at timestamptz;

comment on column notification_deliveries.claimed_at is
    'When a runner claimed this row for an attempt. Null means unclaimed; a value older than the lease is treated as abandoned by a process that died mid-send.';

-- The due-ordering index, now carrying the claim column so the lease filter is index-resident
-- rather than a post-sort scan over every pending row.
drop index notification_deliveries_due_idx;

create index notification_deliveries_due_idx
    on notification_deliveries (next_attempt_at, created_at, claimed_at)
    where status = 'pending';
