-- 0219_ai_local.sql — REQ-106 slice 1: local endpoints and model management.
--
-- # What this is
--
-- The first third of "run with no external calls at all" (docs/requests/REQ-106). Slice 1 is the
-- part that has to exist before the air-gap switch can mean anything: a *local endpoint* the
-- platform treats as an ordinary provider, the models such an endpoint serves, and the one
-- function that decides what "local" means — shared by the endpoint save path, the air-gap check
-- and the doctor, so the three cannot drift apart.
--
-- # `locality` is derived on write, and `host_kind` records *which* rule matched
--
-- The request is explicit that locality is not taken on trust: a row claiming `local` whose host
-- is `api.openai.com` would make the air-gap switch a decoration, because the check reads this
-- column. So the column is written from `is_local_host()` at save time and `host_kind` names the
-- rule that fired (`loopback`, `private`, `allowlisted`) — a screen can then answer "why does the
-- platform call this local?" without re-deriving it, and a later change to the allow-list is
-- visible as a `host_kind` that no longer matches the current rule set.
--
-- The default is `remote`, and it is a *default*, not a default value anyone is meant to keep:
-- every existing provider row already points at a remote host, so `remote` is the true answer for
-- all of them and no backfill is needed. `null` would have been a third answer ("unknown"), and
-- "unknown" in the column the air-gap check reads is exactly the shape that makes a security
-- switch quietly permissive.
--
-- # `ai_local_models` is what an endpoint *serves*, and the server owns the truth
--
-- Size, quantization, context window and the capability flags are all things the local server
-- knows and the platform can only ask about. They are cached here, not owned here: `status` is
-- the platform's last known word, and a pull that fails updates the row rather than deleting it,
-- so a failed pull is visible as a failed model instead of vanishing.
--
-- The pull-progress partial index is on `status = 'pulling'` because that is the only value whose
-- rows are written on a timer. Everything else is written once per change, so indexing the column
-- for them would be paying for lookups nobody performs.
--
-- # `unique (provider_id, model_key)` is the claim that makes a pull single-flight
--
-- Two operators clicking Pull on the same missing model, or one operator double-submitting the
-- drawer, must not start two downloads of the same weights onto the same disk. The unique index is
-- that claim, and the route reads `rows_affected` to learn whether it won it — the same shape as
-- `claim_next_run` in the agent queue, where a queue's correctness is decided by which writer won
-- rather than by who checked last.
-- Which kind of endpoint this is, and *why* the platform believes it.
--
-- `locality` is read by the air-gap check, so it must never be a label a caller can type. The
-- column exists because a screen must be able to group endpoints without re-parsing base URLs, and
-- the write path is the only thing that sets it — see the module docs in
-- `crates/ai-hub/src/local_host.rs` for the single function that decides it.
-- IDEMPOTENCE: every statement is guarded, deliberately, and not just the `create table` below.
-- A guard on the table alone is the half-protected shape and it is the worst of the two: the file
-- dies on the third statement, so the ledger row is never written, so every later `migrate()`
-- re-runs this file and dies in the same place. An idempotent migration has to be idempotent *as a
-- file*, not as its first statement.
--
-- `add column if not exists` is the only part that is one keyword. The CHECKs are named and cannot
-- be guarded by the same keyword, and Postgres has no `if not exists` for a constraint — so they
-- are added through a `do` block that asks the catalogue first. Left unguarded, a second run dies
-- with "constraint already exists"; that was measured, not assumed. Same rule as 0218.
alter table ai_providers
    add column if not exists locality text not null default 'remote';

-- Which rule admitted the host: `loopback`, `private` or `allowlisted`, or NULL for a remote
-- provider. A screen reading "local" can then answer *why* without re-deriving the rule, and an
-- allow-list edit shows up here as a mismatch instead of as a stale claim.
alter table ai_providers
    add column if not exists host_kind text;

-- When a local endpoint was last seen answering. NULL is honest: a never-probed endpoint has no
-- reachability claim yet, which is different from "reachable".
alter table ai_providers
    add column if not exists last_seen_at timestamptz;

do $$
begin
    if not exists (
        select 1 from pg_constraint where conname = 'ai_providers_locality_check'
    ) then
        alter table ai_providers
            add constraint ai_providers_locality_check check (locality in ('remote', 'local'));
    end if;
end
$$;

do $$
begin
    if not exists (
        select 1 from pg_constraint where conname = 'ai_providers_host_kind_check'
    ) then
        alter table ai_providers
            add constraint ai_providers_host_kind_check
            check (host_kind is null or host_kind in ('loopback', 'private', 'allowlisted'));
    end if;
end
$$;

-- The models a local endpoint serves.
--
-- Cascades with the provider: a model row whose endpoint is gone describes nothing. Everything
-- else here outlives nothing, so there are no further foreign keys to reason about.
create table if not exists ai_local_models (
    id uuid primary key,
    provider_id uuid not null references ai_providers (id) on delete cascade,
    model_key text not null,
    display_name text,
    size_bytes bigint,
    parameter_count bigint,
    quantization text,
    context_window integer,
    supports_tools boolean not null default false,
    supports_vision boolean not null default false,
    supports_embeddings boolean not null default false,
    supports_rerank boolean not null default false,
    embedding_dimension integer,
    status text not null default 'missing',
    -- 0–100. `pulling` is the only state where this moves, and it moves from the server's own
    -- progress lines; a value outside the range is not a number a percentage can be.
    pull_progress integer not null default 0,
    pull_message text,
    -- `true` while the server holds the weights in memory/VRAM. The screen shows it because
    -- "installed" and "resident" are different facts and only one of them costs host memory.
    resident boolean not null default false,
    last_used_at timestamptz,
    updated_at timestamptz not null default now(),
    constraint ai_local_models_status_check
        check (status in ('available', 'pulling', 'missing', 'error')),
    constraint ai_local_models_progress_check
        check (pull_progress between 0 and 100),
    constraint ai_local_models_dimension_check
        check (embedding_dimension is null or embedding_dimension > 0),
    -- The claim that makes a pull single-flight. See the note at the head of this file.
    constraint ai_local_models_provider_key unique (provider_id, model_key)
);

create index if not exists ai_local_models_by_provider on ai_local_models (provider_id);

create index if not exists ai_local_models_by_status on ai_local_models (status);

-- The only rows written on a timer are the pulls, so only those are indexed for status.
create index if not exists ai_local_models_pulling on ai_local_models (provider_id)
    where status = 'pulling';
