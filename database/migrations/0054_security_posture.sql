-- 0054_security_posture.sql — the posture check results and the findings store.
--
-- REQ-012, slice 1. Two tables, and the design decision both of them turn on is the same
-- one: **a security screen that says "pass" when it could not check is worse than a screen
-- that says nothing.** So the state column is a closed list of four words the platform can
-- truthfully utter, and the honest one — `unknown` — is a first-class value rather than a
-- null the panel has to guess at.
--
-- The vocabulary in this file is duplicated in `crates/security/src/vocabulary.rs`, which
-- cannot import SQL and this file cannot import Rust. That duplication is deliberate and
-- bounded: the lists are written out here, and the crate carries a test that reads *this*
-- file and fails if a state, source, severity or status exists in one and not the other. So
-- the two can never drift silently — the only way they can drift is visibly, as a red test.
--
-- The lists, as the Rust side holds them:
--   check state      : 'pass', 'warn', 'fail', 'unknown'
--   finding source   : 'config', 'dependency', 'platform', 'report'
--   finding severity : 'critical', 'high', 'medium', 'low', 'info'
--   finding status   : 'open', 'acknowledged', 'fixed', 'ignored'

-- ---------------------------------------------------------------------------------------------
-- security_check_results — one row per check per run; the panel reads the latest run.
-- ---------------------------------------------------------------------------------------------

create table security_check_results (
    id          bigint      generated always as identity primary key,
    organization_id uuid    references organizations (id) on delete cascade,
    check_key   text        not null,
    -- The four words above. Not a boolean, and not nullable: a check that has never been
    -- evaluated is 'unknown', which is a fact about the platform rather than an absence of one.
    state       text        not null,
    -- What the check saw, as structured detail the panel renders (never a raw dump).
    detail      jsonb       not null default '{}'::jsonb,
    -- Every check in one run shares this, so "when did we last look?" is one indexed lookup
    -- rather than a max() over the whole history.
    run_id      uuid        not null,
    checked_at  timestamptz not null default now(),
    constraint security_check_results_state_check
        check (state in ('pass', 'warn', 'fail', 'unknown')),
    -- A check key is a short slug: it is a Rust constant name in lower case, and it is what the
    -- panel's "Action" link and the API's re-run name. Free text here would mean a check the
    -- panel can list but never link to.
    constraint security_check_results_key_check
        check (check_key ~ '^[a-z0-9_]{3,64}$')
);

-- The panel's only read: the latest result for each check, ordered worst-first by the caller.
create index security_check_results_key_checked_idx
    on security_check_results (organization_id, check_key, checked_at desc);

-- One run's whole set, so a run can be fetched and compared as a unit (and so a run that
-- wrote four of six checks is visible as four, not as a silent partial success).
create index security_check_results_run_idx on security_check_results (run_id);

-- ---------------------------------------------------------------------------------------------
-- security_findings — what is wrong, where it came from and what has been done about it.
-- ---------------------------------------------------------------------------------------------

create table security_findings (
    id                uuid        primary key default gen_random_uuid(),
    organization_id   uuid        references organizations (id) on delete cascade,
    -- Where the finding came from. 'config' and 'platform' are raised by our own checks;
    -- 'dependency' and 'report' are ingested from a file CI produced.
    source            text        not null,
    severity          text        not null,
    title             text        not null,
    description       text        not null default '',
    -- The dependency this is about, when there is one. Null for a config finding, and null is
    -- honest: the panel shows a component column that says "—" rather than a guessed package.
    component         text,
    component_version text,
    -- The version that fixes it, when the report says so. A finding with no fix available is
    -- one an operator can only mitigate, and the panel says that instead of implying a version.
    fixed_in          text,
    status            text        not null default 'open',
    -- An ignore is an argument, so it carries its argument. The check constraint below is the
    -- enforcement; the store refuses the same thing one layer up with a field-level message.
    ignore_reason     text,
    -- An ignore can expire: "not now, after the release" is a real answer and a permanent one
    -- is a different claim. Null means it never expires.
    ignored_until     timestamptz,
    acknowledged_by   uuid        references users (id) on delete set null,
    acknowledged_at   timestamptz,
    -- The operator's own words, shown in the detail drawer. A follow-up task is a note here
    -- plus a link out — this crate does not own a task table.
    note              text,
    first_seen_at     timestamptz not null default now(),
    last_seen_at      timestamptz not null default now(),
    -- Component + title, hashed. Two ingests of the same report must land on the same row
    -- instead of doubling the count, and the hash is what makes that comparison cheap and
    -- independent of the version string changing under us.
    fingerprint       text        not null,
    constraint security_findings_source_check
        check (source in ('config', 'dependency', 'platform', 'report')),
    constraint security_findings_severity_check
        check (severity in ('critical', 'high', 'medium', 'low', 'info')),
    constraint security_findings_status_check
        check (status in ('open', 'acknowledged', 'fixed', 'ignored')),
    constraint security_findings_title_check
        check (char_length(title) between 1 and 200),
    -- "Ignore" without a reason is a dismissal, and a dismissal with no stated reason is the
    -- one transition that quietly erases a finding from an operator's view. Refused here, at
    -- the row, so no code path can write one.
    constraint security_findings_ignore_needs_reason_check
        check (status <> 'ignored' or (ignore_reason is not null and char_length(btrim(ignore_reason)) > 0))
);

-- Re-ingesting the same report is not a new finding. Scoped by version, because a dependency
-- that is the same title at a new version is a *different* fact: the old one is fixed-in and
-- the new one is not.
create unique index security_findings_fingerprint_idx
    on security_findings (organization_id, fingerprint, coalesce(component_version, ''));

-- The findings table's own read: the open ones, worst first, newest first within a severity.
-- Partial, because the open set is a small fraction of the table after a while and it is the
-- only read the overview screen performs.
create index security_findings_open_idx
    on security_findings (organization_id, severity, last_seen_at desc)
    where status in ('open', 'acknowledged');

-- The filter the findings screen leads with.
create index security_findings_component_idx on security_findings (organization_id, component);

-- The panel's first-visit question ("is anything new since I was last here?") and the row
-- count for the current filter.
create index security_findings_last_seen_idx
    on security_findings (organization_id, last_seen_at desc);
