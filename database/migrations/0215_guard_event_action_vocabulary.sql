-- REQ-105 slice 4 · the audit trail that could not be written.
--
-- `0210` created `ai_guard_events` with a closed `action` column:
--
--     check (action in ('allowed', 'flagged', 'masked', 'blocked', 'remapped'))
--
-- That is the **verdict** vocabulary. The only writer in the build, `guard_checkpoint::checkpoint`,
-- filled it with `finding.action.as_wire()` — the **per-rule action** vocabulary
-- (`allow` / `flag` / `mask` / `block`). The two differ by one letter in every case, and the two
-- sets are disjoint, so **every insert the guard ever attempted was rejected by this constraint.**
--
-- The failure was invisible because a failed audit is deliberately non-fatal: `checkpoint`
-- returns the verdict it reached and surfaces the write failure as `audit_error`, which the chat
-- route logs at `warn`. Correct in isolation — a user's chat must not fail because the audit row
-- did not fit — and the reason the whole trail could stay empty for three slices. The only
-- symptom a human would ever see is `/ai/guard/events` reporting "no events" for an
-- installation whose rules were demonstrably rewriting turns. **An audit screen that shows
-- nothing is worse than no audit screen**, because it is an affirmative statement that nothing
-- happened.
--
-- What this migration does NOT do is widen the constraint to accept the action names. That would
-- have made every future caller able to write either vocabulary into one column, and the column
-- would keep meaning whatever the last writer meant. The constraint stays the single authority
-- on the verdict names; `guard_store::EVENT_ACTIONS` is its mirror in Rust, `record_event`
-- validates against it and derives `blocked` from it, and the checkpoint now writes
-- `finding.verdict.as_wire()`.
--
-- `flagged` and `remapped` are dropped from the check, and neither drop is cosmetic. Neither is a
-- value of [`GuardVerdict`][verdict] — the enum is `clear` / `allowed` / `masked` / `blocked` — so
-- **no code path in this build could ever have produced either name**, and the database accepted
-- them. `flagged` is the one that invites the mistake: `Action::Flag` exists, so the name looks
-- like the natural spelling of "a human should look at this", and flagging in fact yields the
-- `allowed` verdict with a rule key attached. A name the database accepts but the platform cannot
-- emit is an invitation for the next writer to invent it, so the check is narrowed to the three
-- that `verdict.as_wire()` really returns.
--
-- `clear` is also absent, and that is deliberate rather than an omission: the checkpoint returns
-- before it files anything for a payload with nothing to guard, so a `clear` row could never be
-- filed either.
--
-- [verdict]: `crates/ai-hub/src/guard_data.rs`
--
-- Idempotent, and required to be: this file may run against an instance that already applied it
-- and against a fresh database whose `0210` runs moments earlier in the same chain. The
-- `drop constraint if exists` before each `add` is what makes it so — the second run drops and
-- re-adds an identical definition rather than failing on the name. Both live in one
-- transaction, so the table is never left with no constraint on this column.
--
-- No row is rewritten or deleted. Every table in this repository where `0210` has run is
-- **empty** in that table — the insert never succeeded anywhere — so there is nothing to
-- migrate and nothing an operator could have been looking at. `do $$ … $$` below asserts that
-- rather than assuming it: if a future instance somehow holds rows, the migration refuses
-- instead of silently leaving stale names behind.
--
-- Number 0215: the file numbering is one shared namespace across this repository's ten worktrees,
-- so the number is taken above the union high-water (0214, held by wave 5's cluster panel), not
-- above this branch's own last file (0210).

do $$
begin
    if exists (select 1 from ai_guard_events) then
        raise exception
            'ai_guard_events is not empty: % row(s) hold the action vocabulary. Map them to the \
             verdict vocabulary (allow->allowed, flag->flagged, mask->masked, block->blocked) before \
             applying this migration, because this one only narrows the allowed set.',
            (select count(*) from ai_guard_events);
    end if;
end
$$;

-- The two constraints that mention the column, rebuilt together with the narrowed name list.
-- `drop … if exists` first so the file is re-runnable; both are replaced in one transaction, so
-- there is no window in which the table has no constraint.
alter table ai_guard_events
    drop constraint if exists ai_guard_events_action_known,
    drop constraint if exists ai_guard_events_blocked_matches_action,
    drop constraint if exists ai_guard_events_blocked_has_reason;

alter table ai_guard_events
    add constraint ai_guard_events_action_known
        check (action in ('allowed', 'masked', 'blocked')),
    -- `blocked` and `action` are the same fact twice, and a divergence between them is a filter
    -- that lies: the screen's "blocked only" switch reads the boolean while the row's action chip
    -- reads the word. Kept from `0210` unchanged, restated here because both constraints name the
    -- vocabulary and only the first one narrowed.
    add constraint ai_guard_events_blocked_matches_action
        check (blocked = (action = 'blocked')),
    add constraint ai_guard_events_blocked_has_reason
        check (action <> 'blocked' or error_code is not null);