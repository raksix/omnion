-- 0203_ai_approvals_operation_key.sql — REQ-101 slice 3d: the parked approval names its operation.
--
-- # Why this column and not a join
--
-- Slice 3c files **one approval per gated operation**, which is the right shape for the inbox
-- (a reviewer deciding "the agent wants to publish three pages" sees three rows, not a count).
-- But it left the pipeline with a question it could not answer: when one of those rows is
-- approved, **which** operation of the set may now run?
--
-- Every other candidate answer is wrong in a way that only shows up later:
--
-- - `resource_id` is not it. A set may edit the same page twice — a rename and a publish in one
--   proposal — and both rows would carry the same id, so "count the approvals for this page"
--   releases a delete somebody is still reading.
-- - `preview_hash` is not it either: it identifies *what would be written*, and two operations
--   with the same effect (publish page A, publish page B — different ids, same shape) are not
--   distinguished by it, while one operation re-previewed is.
-- - The `ai_change_sets_operations` jsonb array could be searched, but the approval would then
--   have to guess which entry a row came from, by re-deriving the same classification the
--   confirm route already used to file it.
--
-- The editor's **operation key** is the one identifier that survives every edit: the set is
-- stored whole in a jsonb column, so the key exists before the row is written, and it is what
-- the editor reorders and drops by. One denormalised column, written once by the only caller
-- that files from a set, and `null` for the single-call path — which parks on its own run step
-- and has no set to walk back to.
--
-- Nullable on purpose, and **not** defaulted to '': an empty string is a key the editor could
-- legitimately mint, and a check that treats '' as "belongs to no operation" would be a lie
-- about a set whose operation was keyed that way. `null` is the honest answer for "this row
-- did not come out of a change set", and the release query reads it as such.

begin;

alter table ai_approvals
    add column if not exists operation_key text;

-- The release path asks one question and it is "which of this set's parked rows have been
-- approved", so the index is on the set, and partial: the single-call path leaves the column
-- null and must not appear in any of those entries. Without the predicate this index is
-- dominated by the ordinary run-step approvals, which is the overwhelming majority of rows.
create index if not exists ai_approvals_change_set_ops_idx
    on ai_approvals (change_set_id, operation_key)
    where change_set_id is not null;

-- The shape the release path relies on, as a constraint rather than as a hope: a row that names
-- a change set but no operation key cannot be joined back to anything, and a release that
-- silently treated it as "already decided" would be the exact hole this column exists to close.
-- The reverse (a key without a set) is left unconstrained — an operation key is meaningless on
-- its own but harmless, and forbidding it would make a future caller that files the row before
-- the set fail at a boundary that has no opinion to offer.
alter table ai_approvals
    add constraint ai_approvals_set_needs_operation_key
    check (change_set_id is null or operation_key is not null);

commit;
