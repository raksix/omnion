-- REQ-108 slice 2 — the sixth invocation status, and the index the list screen reads.
--
-- WHAT THIS IS
-- One constraint widened and one index added. Nothing here is a new table: slice 1's three
-- tables carry slice 2 completely, and the walk proves that by counting rows rather than by
-- reading a schema.
--
-- WHY `pending_approval` HAD TO JOIN THE CONSTRAINT
-- The acceptance criterion says a gated write tool "parks an approval request and returns a
-- 'pending approval' response" and then, once approved, "the invocation row moves to `ok`".
-- A row that cannot hold `pending_approval` cannot make that second half true: the parked call
-- would have to be recorded as something else, and the approval path would then either move a
-- row that does not describe the parked call or write a *second* row — which makes the history
-- read as two calls when one happened. So the parked state is a status, not a separate table,
-- because a status is the only shape where "moves to" is expressible.
--
-- WHY THE CONSTRAINT IS REPLACED AND NOT ALTERED IN PLACE
-- `alter table … drop constraint if exists` then `add constraint` is the pattern every migration
-- in this ledger uses, and the `if exists` is load-bearing: the migration has to apply to a
-- database that has **not** run 0241 as well as one that has. A bare `add constraint` naming a
-- constraint that is already there with the old definition fails, and so does an `alter … check`
-- that assumes the original shape.
--
-- **The `drop` needs its own `alter table`.** Written as a standalone `drop constraint if exists
-- …;` the file does not parse — Postgres has no such statement, the error is `syntax error at or
-- near "constraint"` reported against the *migration number* with no statement text, and the
-- walk that hits it panics at `db.migrate()`, which reads as a broken database rather than a
-- broken migration. sqlx splits on the semicolon, so the failure is in the second statement of a
-- two-statement file that looks right on screen.
--
-- WHY `mcp_invocations_org_status_created_ix` LEADS WITH THE FILTER
-- The clients screen reads "this tenant's calls, newest first, optionally narrowed to a status".
-- The existing `(tool, status, created_at)` index cannot serve the first column of that — it
-- leads with `tool`, which the filter only supplies occasionally — so without this index a
-- tenant's log is a sequential scan that grows with the table, and an installation with a busy
-- client turns a screen into a timeout.
--
-- WHY `pending_approval` GETS ITS OWN PARTIAL INDEX
-- The approvals inbox shows what is waiting; nothing else asks that question. A partial index over
-- exactly the rows that can move is what keeps that query off the whole log, and the predicate
-- keeps it honest: the moment a row leaves `pending_approval` it leaves the index too, so the
-- index cannot report a settled call as pending.
--
-- THE `pending_approval` ROW STILL CARRIES ITS APPROVAL ID
-- `approval_id` is written when the call parks, so "which inbox item is this" is a column rather
-- than a join back through the tool arguments. The arguments are masked and truncated to keys
-- on this table, which makes them the wrong place to look for anything at all.
alter table mcp_invocations
    drop constraint if exists mcp_invocations_status_ck;

alter table mcp_invocations
    add constraint mcp_invocations_status_ck
    check (status in ('ok', 'error', 'denied', 'sandbox', 'blocked_airgap', 'pending_approval'));

create index if not exists mcp_invocations_org_status_created_ix
    on mcp_invocations (organization_id, status, created_at desc);

create index if not exists mcp_invocations_pending_ix
    on mcp_invocations (organization_id, created_at desc)
    where status = 'pending_approval';