-- REQ-099 · slice 4 — the output-schema stop reason, and the repair budget that survives a restart.
--
-- Two things land here, and the second is only discoverable because the first is.
--
-- **1. `output_schema` joins the stop-reason vocabulary.** Slice 4 gave the loop a new way to
-- end: the final answer did not match the shape the caller required and the single repair turn
-- did not fix it. A new `StopReason` with no entry in this constraint is a run the database
-- refuses to write — the loop would produce a perfectly good `Outcome`, `finish_run` would
-- bind `output_schema`, and the whole transaction would fail with a check-constraint name
-- instead of the sentence the panel needs. The constraint is the vocabulary's only
-- enforcement, so a new reason is a migration and not just a Rust variant.
--
-- It is a separate reason rather than a re-use of `error` for the reason the crate gives: an
-- answer that fails its schema is the one failure where the operator's next action is
-- *different*. Nothing is broken — the model produced a shape nobody asked for — and a run
-- reported as `error` sends the reader to the provider, the tools and the network instead of
-- to the rule.
--
-- **2. `output_repairs` on the run row.** The budget for repairing a malformed answer is one
-- turn, and a turn is a provider call that costs money. The loop counts what it spent and
-- reports it on the `Outcome`, but a count that only lives in the loop's stack is a `0` after
-- a process restart — so a run that was interrupted mid-repair resumes with a fresh allowance
-- and repairs forever, which is the exact failure the constant exists to prevent. The number
-- is persisted here and the resume path reads it back.
--
-- The default is 0 and the constraint is `between 0 and 1`: a row claiming three repairs is a
-- row written by a bug or by a hand, and a stored budget above the constant would silently
-- re-define the policy rather than report a violation of it.

do $$
begin
    -- 1. The new stop reason. Written as a drop-and-recreate rather than as an `alter
    --    check ... drop constraint if exists` because a bare `alter table ... drop constraint`
    --    inside the same transaction that recreates it takes an ACCESS EXCLUSIVE lock twice
    --    instead of once, and `ai_runs` is written by every run in the installation.
    if exists (
        select 1 from pg_constraint
        where conname = 'ai_runs_stop_reason_known'
    ) then
        alter table ai_runs drop constraint ai_runs_stop_reason_known;
    end if;

    alter table ai_runs
        add constraint ai_runs_stop_reason_known check (
            stop_reason is null or stop_reason in (
                'final_answer', 'max_steps', 'deadline', 'token_budget',
                'cancelled', 'loop_detected', 'error', 'output_schema'
            )
        );

    -- 2. The persisted repair budget.
    if not exists (
        select 1 from information_schema.columns
        where table_name = 'ai_runs' and column_name = 'output_repairs'
    ) then
        alter table ai_runs
            add column output_repairs int not null default 0
                constraint ai_runs_output_repairs_bounded check (output_repairs between 0 and 1);
    end if;
end
$$;

-- The column is read on resume, which is a read of a handful of rows by id — the primary key
-- answers it. An index here would be a write cost on every step boundary of every run to
-- serve a query that already uses the primary key.
