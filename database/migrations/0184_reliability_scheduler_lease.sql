-- The scheduler's lease and its due-scan (REQ-127, slice 3).
--
-- `0162_reliability.sql` gave the ledger a `next_attempt_at`, which is enough to *resume* a
-- sequence and not enough to *run* it safely: two workers waking on the same due row would both
-- read the row, both call the provider, and both write an attempt. The ledger would then hold
-- attempt 4 written twice, and the timeline an operator reads afterwards would show a duplicate
-- rather than a double send.
--
-- A claim column fixes that without a queue. The claim is a COMPARE-AND-SWAP on the due row
-- itself, so the loser of the race updates zero rows and skips — the same property the
-- idempotency store gets from `unique (scope, subject_id, key)`, obtained here by a conditional
-- UPDATE instead of by an insert.
--
-- The lease is deliberately EXPIRING rather than permanent. A permanent claim turns a worker
-- that dies mid-attempt into a job nobody ever runs again, which is the exact failure the retry
-- subsystem exists to prevent; an expiring claim means a crashed worker's job is retried after
-- the lease. The cost is that a call slower than the lease can run twice, so the lease must be
-- longer than the slowest outbound timeout — which is why it is a parameter and not a constant.

alter table retry_outcomes add column claimed_at timestamptz;

-- The due scan. Partial on `next_attempt_at is not null`: a finished sequence has nothing to
-- schedule, and the index should not carry those rows.
create index retry_outcomes_due
    on retry_outcomes (next_attempt_at, subject_kind, subject_id)
    where next_attempt_at is not null;

-- Commented out, like every reversal in this tree: `Db::migrate` executes a file's LIVE
-- statements on apply, so a down script written as live SQL would drop the column this file
-- added and record a success doing it.
--
--   alter table retry_outcomes drop column claimed_at;
--   drop index retry_outcomes_due;
