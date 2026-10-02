-- Automation project limit notices (REQ-133 slice 4) -- the "fires once" half of acceptance 9.
--
-- The REQ asks for `automation.project.limit.warning` and `.limit_exceeded`, "80 and 100 percent
-- crossings, edge-triggered once per limit per period". Everything except the word *once* existed:
-- `Limits::warns` computed the crossing, the limits screen rendered it, and the refusal named the
-- limit and the owner -- but nothing was ever *emitted*, and the warning a client saw was computed
-- per read, so reloading the screen re-warned for ever. A warning that repeats on every read is
-- not a warning; it is the state of the world restated, and an operations team subscribed to the
-- event would have had to filter it themselves.
--
-- So the claim is the feature. One row per (project, limit, period, kind): the insert's row count
-- is the decision, exactly the shape the CRM module has now used three times (the round-robin
-- cursor, the autoresponder reservation, the submission claim) and the same shape the SLA reminder
-- uses. `on conflict do nothing` plus a row count is a fact of the data rather than a read that a
-- second worker can interleave with.
--
-- `period_key` is the day for the two run counters (which reset at midnight) and the epoch for
-- workflow/credential caps (which do not reset at all, so their period is "ever"). Writing the
-- date as a string rather than reusing `usage_date` keeps one column for both shapes, and the
-- default is the epoch so a row written by a future caller without a period is still scoped.
--
-- `observed_at` exists for the same reason the counters do: an operator asking "when did this
-- project cross its cap" wants the instant, and a claim row that can only say *that* it fired
-- answers half a question.

create table if not exists automation_project_limit_notices (
    project_id uuid not null references automation_projects (id) on delete cascade,
    limit_name text not null,
    period_key text not null default 'ever',
    kind text not null,
    observed_at timestamptz not null default now(),
    primary key (project_id, limit_name, period_key, kind)
);

-- The sweep's query: every project that owes a notice, oldest claim first so a backlog drains in
-- the order it arrived rather than in an order the planner likes.
create index if not exists automation_project_limit_notices_observed_idx
    on automation_project_limit_notices (observed_at desc);

comment on table automation_project_limit_notices is
    'One row per automation project limit crossing, per period. The primary key IS the "once": the '
    'insert''s row count decides whether the warning is emitted, so a second worker, a screen '
    'reload and a redelivered webhook all agree that it already fired.';
comment on column automation_project_limit_notices.period_key is
    'The day (YYYY-MM-DD) for counters that reset, or ''ever'' for caps that do not.';
comment on column automation_project_limit_notices.kind is
    'Either ''warning'' (the warn_at_percent crossing) or ''exceeded'' (the cap).';
