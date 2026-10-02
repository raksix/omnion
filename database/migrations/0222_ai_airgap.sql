-- 0222_ai_airgap.sql — REQ-106 slice 2: the air-gap switch and its two working parts.
--
-- # What this is
--
-- Slice 1 made a local endpoint a thing the platform can prove is local. Slice 2 is the switch
-- that makes "never leaves the instance" a checked fact instead of a promise: with the gap on,
-- every call whose resolved provider is not local is refused BEFORE any network call.
--
-- Three tables, because the request asks for three different kinds of fact and conflating them
-- is how a compliance switch becomes decorative:
--
--   * `ai_airgap_state`  — the switch itself. One row, `id = 1` enforced by the check, so two
--                          writers cannot produce two answers to "is the gap on?".
--   * `ai_airgap_hosts`  — the internal-host allow-list. This *widens* what counts as local; it
--                          never replaces the loopback/private rules (see local_host.rs).
--   * `ai_local_doctor_runs` — what the doctor found, kept as history so a regression is visible
--                          after the run that found it has scrolled away. Slice 4 fills the rows.
--
-- # The switch is seeded off, and `reason` is required to turn it ON — not to store one
--
-- `enabled bool default false` with a seeded row means an installation that never touches the
-- screen reads `false`, which is the honest answer. The requirement that a reason exists is
-- enforced on the *write path* rather than by a `check (enabled = false or reason is not null)`,
-- and that is deliberate: a constraint would make "turn the gap off" fail too (the row would have
-- to clear the reason to satisfy it), and the honest shape is a table where turning the gap OFF
-- keeps the last reason and the actor as a record of why it was ever on. `enabled_by` and
-- `enabled_at` are therefore named for what they mean while it is on and are left in place when
-- it goes off, which is what a screen reads to answer "was this ever in effect, and who did it".
--
-- `low_confidence_ack` records the acknowledgement the confirmation asked for. It is a column
-- rather than nothing because the request asks that enabling lists the non-local providers in use
-- and takes an acknowledgement: without somewhere to record that the operator saw the list, the
-- acknowledgement is a click that proves nothing after the fact.
--
-- # `ai_airgap_hosts.host` is unique, and uniqueness is the safety property
--
-- An allow-list is a set of names the platform will treat as internal. A duplicate entry is not
-- untidy, it is ambiguous about which note an operator meant, so the constraint is on the
-- normalized (lowercased) form: the write path lowercases before inserting, and `unique (host)`
-- then also makes a second `INSERT ... ON CONFLICT DO NOTHING` an honest no-op rather than an
-- error the caller has to recognise.
--
-- # IDEMPOTENCE
--
-- Every statement is guarded as a FILE, not as a first statement: the table, the indexes and the
-- named CHECK/constraints each carry their own guard, because a migration that dies on its third
-- statement never records its ledger row and is therefore re-run on every `migrate()` — the
-- half-protected shape documented in 0219. Postgres has no `if not exists` for a constraint, so
-- those go through a `do` block that asks pg_constraint first.

-- # The switch. One row for the whole installation, enforced by the primary key check.
create table if not exists ai_airgap_state (
    id smallint primary key,
    -- Whether non-local provider calls are refused right now.
    enabled bool not null default false,
    -- Why it was turned on. Required by the API when enabling, kept after disabling.
    reason text,
    -- Who turned it on; a disabled user must not erase the audit trail.
    enabled_by uuid references users (id) on delete set null,
    enabled_at timestamptz,
    -- The operator acknowledged the list of non-local providers that will stop working.
    low_confidence_ack bool not null default false,
    -- Last egress verification (REQ-106 slice 4 writes the result).
    egress_verified_at timestamptz,
    egress_verify_target text,
    egress_verify_result text,
    updated_at timestamptz not null default now()
    -- The single-row check is added by the `do` block below under one name, not inline: a
    -- second copy of the same rule under a different name is two constraints to reason about,
    -- and the narrower one silently wins when a later migration relaxes one of them.
);

-- Seeded off: an installation that never opens the screen must still have an answer to read, and
-- 'false' is the true one for every existing provider row.
insert into ai_airgap_state (id, enabled)
values (1, false)
on conflict (id) do nothing;

-- # The internal-host allow-list. Widens locality; never replaces the built-in rules.
create table if not exists ai_airgap_hosts (
    id uuid primary key default gen_random_uuid(),
    -- Host name (or address) the platform is told to treat as internal. Stored lowercased.
    host text not null,
    -- Why the operator added it — the field that makes an allow-list reviewable.
    note text,
    created_by uuid references users (id) on delete set null,
    created_at timestamptz not null default now(),
    constraint ai_airgap_hosts_host_unique unique (host)
);

create index if not exists ai_airgap_hosts_host_idx on ai_airgap_hosts (host);

-- # Doctor runs. History, not a singleton: "it passed yesterday" is the question that matters
-- when today's verdict is a warning.
create table if not exists ai_local_doctor_runs (
    id bigserial primary key,
    organization_id uuid,
    -- 'passed', 'warned' or 'failed' — the summary line a screen renders.
    status text not null,
    -- [{key, status, detail, latency_ms, fix}] — the per-check detail, as JSON.
    checks jsonb not null default '[]'::jsonb,
    -- The gap state at the moment of the run, so a pass recorded while it was on cannot be read
    -- as a pass for a different configuration.
    airgap_enabled bool not null default false,
    triggered_by uuid references users (id) on delete set null,
    started_at timestamptz not null default now(),
    finished_at timestamptz
);

create index if not exists ai_local_doctor_runs_started_idx
    on ai_local_doctor_runs (started_at desc);

-- Only the runs that are not clean are worth an index lookup; a 'passed' run is what the history
-- list shows and it is never filtered on.
create index if not exists ai_local_doctor_runs_unclean_idx
    on ai_local_doctor_runs (status)
    where status <> 'passed';

do $$
begin
    if not exists (
        select 1 from pg_constraint where conname = 'ai_local_doctor_runs_status_check'
    ) then
        alter table ai_local_doctor_runs
            add constraint ai_local_doctor_runs_status_check
            check (status in ('passed', 'warned', 'failed'));
    end if;
    if not exists (
        select 1 from pg_constraint where conname = 'ai_airgap_state_id_check'
    ) then
        alter table ai_airgap_state
            add constraint ai_airgap_state_id_check check (id = 1);
    end if;
    if not exists (
        select 1 from pg_constraint where conname = 'ai_airgap_hosts_host_check'
    ) then
        alter table ai_airgap_hosts
            add constraint ai_airgap_hosts_host_check check (host = lower(host));
    end if;
end
$$;

-- # `blocked_airgap` joins the call-log vocabulary, in BOTH places that hold it
--
-- The air gap refuses a call before any byte is sent, and "refused" cannot carry that meaning:
-- `refused` is what the provider itself said (its own safety filter, its own 4xx), and an
-- operator reading a `refused` row looks upstream for a cause that is not there. The switch
-- decided this call would never be made, which is a different fact with a different owner and a
-- different fix — point the feature at a local endpoint, or turn the gap off.
--
-- The list lives in two places — the CHECK here and the `record_usage` guard in `health_store.rs`
-- — and **both are changed here**. Widening only the Rust guard produces rows the database
-- refuses; widening only the CHECK teaches the API a vocabulary the code rejects. The vocabulary
-- is a contract, and a contract with two halves has to be amended in the same commit.
do $$
begin
    -- The constraint is dropped and re-added rather than altered: `alter table … drop constraint
    -- if exists` then `add constraint` is the only spelling that is safe to re-run, because
    -- `add constraint` has no `if not exists` and would die on the second migrate().
    alter table ai_provider_usage
        drop constraint if exists ai_provider_usage_outcome_check;

    alter table ai_provider_usage
        add constraint ai_provider_usage_outcome_check
        check (outcome in ('ok', 'error', 'refused', 'blocked_airgap'));
end
$$;
