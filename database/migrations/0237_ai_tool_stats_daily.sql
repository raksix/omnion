-- REQ-107 · Agent evals and telemetry — slice 4: the daily tool statistics table.
--
-- Slice 1–3 built the suites, the cases, the runs and the runner. What is left for a screen to
-- answer "is this agent still good?" is the per-tool telemetry: which tool fails, which tool the
-- caller was *denied*, and which tool is slow at the tail. This migration is the store that makes
-- that question cheap enough to ask on a page load.
--
-- ## Why a roll-up table rather than a query over ai_run_steps
--
-- The honest query is one `group by` over `ai_run_steps` where `kind = 'tool_call'`. That query is
-- correct, unbounded in cost, and wrong for the screen: it reads every step row ever written, and
-- an installation that has been running agents for a year has millions of them. The telemetry
-- screen renders on every visit, so a query whose cost grows with lifetime traffic is a query
-- that eventually takes the page down.
--
-- The roll-up is keyed `(day, organization_id, tool)` — so the answer to "the last 30 days for
-- this organization" is at most 30×(number of tools) rows, and the primary key makes the refresh
-- idempotent. `upsert` rather than `delete + insert`: the daily row for a tool that had no calls
-- today must *disappear*, and it must do so without touching the days that are complete.
--
-- ## Why the percentiles are stored and not computed at read time
--
-- p95 of a latency distribution cannot be averaged, and the honest way to read it — a
-- `percentile_cont` over the raw steps — is exactly the scan the roll-up exists to avoid. Storing
-- the three percentiles keeps the screen's promise (p50/p95/p99 columns, per the spec) while
-- bounding the read. It also means the number on screen is *the same number* every time it is
-- read, which a recomputed percentile is not once the underlying step rows are pruned.
--
-- ## Why cost lives here as well as on the step
--
-- `ai_run_steps.cost_micros` is the per-step truth and stays the audit trail. This column is the
-- same value *aggregated*, so the "costliest failing tool" panel and the per-tool cost column can
-- be answered from one table. A panel that joins steps at read time to answer "what did this tool
-- cost" would be the unbounded query this table was created to remove.
--
-- ## Retention
--
-- None. Unlike `ai_usage` (which is a billing record), a tool-stat row is a roll-up that can be
-- recomputed from the step rows it summarises — *if* those rows still exist. REQ-098 never prunes
-- `ai_usage` because a cost row that vanishes cannot be audited; the same reasoning does not
-- extend here, and a table that is kept forever with no writer is a lie. This table is written by
-- the roll-up runner on a schedule, and the reader is always a range picker.

create table if not exists ai_tool_stats_daily (
    day             date        not null,
    organization_id uuid        not null references organizations (id) on delete cascade,
    -- The tool name as the run step recorded it (`ai_run_steps.tool`). Not a foreign key to
    -- `ai_tools`: a tool that is deleted from the catalogue must keep its historical numbers, and
    -- a stat row that cascades away with the catalogue is a stat nobody can compare a drop against.
    tool            text        not null,
    calls           int         not null default 0,
    successes       int         not null default 0,
    failures        int         not null default 0,
    -- Calls the platform *refused* — the permission or tool-allow-list said no before the tool
    -- ran. Counted separately from `failures` because a denial is an operator's configuration
    -- decision, not a defect in the tool, and a screen that merges them hides which one it was.
    denials         int         not null default 0,
    p50_ms          int,
    p95_ms          int,
    p99_ms          int,
    -- Where the cost comes from, and why it is here at all.
    --
    -- `ai_tool_calls` has no cost column — a tool call's price is billed on the *step*
    -- (`ai_run_steps.cost_micros`, snapshotted from the catalog at write time, REQ-098 slice 5),
    -- and the two tables meet at `ai_tool_calls.step_id`. So this column is a join, aggregated:
    -- `sum(coalesce(s.cost_micros, 0))` over the steps the day's calls point at.
    --
    -- It is denormalised on purpose. The step row stays the audit trail and is what a deep audit
    -- reads; this is the per-tool column the telemetry screen renders, and a screen that joined
    -- steps at read time would be the unbounded scan this table exists to remove.
    cost_micros     bigint      not null default 0,
    -- The distribution of failure reasons, as `{code: count}`.
    --
    -- Not a `text[]` and not a comma-joined string: the screen renders "what went wrong" as a
    -- ranked list, and a rank is a count. A flat list would force the reader to count in SQL to
    -- answer the only question the column exists for. Codes are stable machine strings
    -- (`tool_denied`, `permission_denied`, `tool_timeout`, …) so this is a map, not prose.
    error_codes     jsonb       not null default '{}'::jsonb,
    refreshed_at    timestamptz not null default now(),

    constraint ai_tool_stats_daily_counts_sane check (
        calls >= 0 and successes >= 0 and failures >= 0 and denials >= 0
    ),
    -- `error_codes` is an object whose values are counts, and jsonb cannot check its own shape.
    -- This does: a number in, anything else out. A reader that trusts the column can then rank
    -- without re-validating.
    constraint ai_tool_stats_daily_error_codes_shape check (
        jsonb_typeof(error_codes) = 'object'
    ),
    primary key (day, organization_id, tool)
);

-- The reader is "the last N days for this organization", which the primary key serves only as a
-- per-tool lookup; this index is what lets the range scan skip straight to the window instead of
-- walking the table day by day.
create index if not exists ai_tool_stats_daily_org_day_idx
    on ai_tool_stats_daily (organization_id, day desc);