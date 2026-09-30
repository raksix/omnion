-- Allocations remember the order they were written in (REQ-054, slice 3 follow-up).
--
-- The read-back sorted by `order by a.created_at, a.id`, which looks like "the order they were
-- written" and is not one. `now()` is **transaction-stable** in PostgreSQL: every row inserted by
-- one payment gets the identical timestamp, so the tiebreak fell through to `id` — and `id` is
-- `gen_random_uuid()`, a random value. One sweep over three invoices therefore came back in a
-- random order, so the "oldest invoice first" the REQ documents was true of the write and false
-- of the read. The walk that caught it asserts 50.00 before 10.00 and read 10.00 first about half
-- the time; it is the sort order, not the arithmetic.
--
-- `position` is a small ordinal, the loop index, written by the module. It is what the read orders
-- by, with `id` still last so a hand-inserted row without a position sorts after the ones that
-- have one rather than between them.
--
-- This is additive and safe for existing rows: the backfill numbers each payment's allocations in
-- the order the *old* query happened to return, which is arbitrary but stable, and a payment that
-- nobody has read back is not a payment whose order anybody relies on.

alter table accounting_payment_allocations
    add column if not exists position smallint;

comment on column accounting_payment_allocations.position is
  'Ordinal of this allocation within its payment, 0-based, as the module walked them. The read-back '
  'sorts by it so a sweep reads oldest-invoice-first. Nullable for rows written before it existed; '
  'the read falls back to id for those.';

-- The backfill: `row_number()` over each payment's own rows, in the arbitrary-but-stable order the
-- previous query produced. `order by a.id` rather than leaving it unspecified, so a re-run of this
-- migration on the same data produces the same numbering instead of a different arbitrary one.
with numbered as (
    select
        a.id,
        row_number() over (partition by a.payment_id order by a.id) - 1 as ordinal
    from accounting_payment_allocations a
    where a.position is null
)
update accounting_payment_allocations a
set position = n.ordinal
from numbered n
where a.id = n.id;

-- Unique per payment, so two allocations can never claim to be the same step of the same payment.
-- Built as a plain index rather than a constraint because a unique constraint over a nullable
-- column lets unlimited NULLs through by definition, and the duplicate we are preventing is a
-- *position*, which is set on every row the module writes.
create unique index if not exists accounting_payment_allocations_position_idx
    on accounting_payment_allocations (payment_id, position)
    where position is not null;
