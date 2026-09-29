-- REQ-099 · Agent runtime & tool loop — slice 2, second commit: what a run was told to read.
--
-- The Run sheet lets a person hand a run a handful of workspace files ("summarise these"). That
-- choice is currently a number in an audit row: `"files": 3`. Three is not a record — after a run
-- fails on a missing file, the only honest answer to "which files did it name?" is nothing, and
-- the operator has to guess from the goal text.
--
-- This table is that answer. It is deliberately **not** a column on `ai_runs`, and the reason is
-- the whole design:
--
-- 1. **A reference outlives the file.** `file_id` is `on delete set null`, not cascade. A run
--    that named `q3.csv` and a person who then deleted `q3.csv` is a run whose input list now
--    reads "q3.csv — missing", which is exactly the sentence that explains the failure. Cascading
--    would delete the row and leave the trace claiming the run had no inputs at all.
--
-- 2. **A reference may name a file that does not exist *yet*.** `file_id` is nullable and the
--    path is the primary content. The Run sheet can therefore hand a run an output path ("write
--    the summary to `summary.md`"), and a resolution failure is reported per row rather than
--    refusing the run at start — a goal that quotes a future file is a normal instruction, not a
--    malformed one.
--
-- 3. **The path rules are re-checked here, by the database.** Same constraint text as
--    `ai_agent_files_path_clean`, because a reference is written from a request body and a
--    traversal that reaches this table reaches a prompt. One rule, restated, because a rule in
--    one place is a rule the next writer does not know about.
--
-- 4. **`agent_id` is carried on the row, not inferred.** A run's agent can be deleted
--    (`ai_runs.agent_id` is `set null` on purpose — the run is the evidence that money was
--    spent). Without this column the inputs would vanish with the agent and the evidence would
--    lose the half that says what it was working from. It is a plain uuid with no foreign key
--    for exactly that reason: a *reference* that cascades is a reference that deletes itself.
--
-- 5. **One row per (run, path).** The sheet's picker can produce the same path twice (a stale
--    list, a double-click), and two rows would render as a duplicate input on the run detail
--    while the model read it twice.

create table if not exists ai_run_inputs (
    id         uuid primary key default gen_random_uuid(),
    run_id     uuid        not null references ai_runs (id) on delete cascade,
    agent_id   uuid        not null,
    path       text        not null,
    file_id    uuid        references ai_agent_files (id) on delete set null,
    attached_by uuid       references users (id) on delete set null,
    created_at timestamptz not null default now(),

    constraint ai_run_inputs_path_clean check (
        char_length(path) between 1 and 512
        and path !~ '[[:cntrl:]]'
        and path !~ '(^|/)\.\.(/|$)'
        and path !~ '^/'
        -- A drive letter. `C:\notes.md` has no forward slash, no leading slash and no `..`, so
        -- every other clause passes it — and it is the shape a Windows client sends when a path
        -- is built by string concatenation instead of through an API. The Rust validator refuses
        -- it; this restates that so a row written by anything else cannot carry one either.
        and path !~ '^[A-Za-z]:'
    ),
    -- The "one row per (run, path)" the comment above promises. A unique constraint on
    -- `(run_id, path)` rather than a primary key, because the row also has to be addressable by
    -- id for the sheet's "remove this reference" action.
    constraint ai_run_inputs_run_path_unique unique (run_id, path)
);

-- The sheet's own insert order, and the run detail's read order.
create index ai_run_inputs_run_created_idx on ai_run_inputs (run_id, created_at);

-- "Which runs named this file?" is the question the workspace tab answers when somebody deletes
-- a file and needs to know what it was doing. Uniqueness is (run_id, path), so the file column
-- is deliberately not unique — the same file can be named by two runs of the same agent.
create index ai_run_inputs_file_idx on ai_run_inputs (file_id) where file_id is not null;
