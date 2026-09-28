-- Omnion · 0030 · AI provider health samples and usage (REQ-097, slice 3)
--
-- 0022 gave every provider a `last_health` column, which is the *verdict* of the last probe and
-- nothing more: one string that says whether the endpoint answered. A verdict with no history
-- cannot answer the questions an operator actually asks — did it flap this morning, how long has
-- it been down, was that 900 ms spike normal for this endpoint — so this migration adds the sample
-- rows the verdict is computed from, and the per-provider call counters the Usage tab reads.
--
-- Released migrations are append-only (docs/05-VERSIONING.md), so `ai_providers.last_health` is
-- left exactly as 0022 shipped it. It is kept as the *materialised* verdict on purpose: the
-- providers table is read on every list render and every routing decision, and it must not wait
-- on a window function over thirty days of samples to say a provider is down.

-- One probe sample per enabled provider per tick. The row is the evidence; `ai_providers` carries
-- the conclusion, and `health_status()` in the crate is the only thing allowed to write it.
create table ai_provider_health (
    id bigserial primary key,
    provider_id uuid not null references ai_providers (id) on delete cascade,
    -- ok / degraded / down. `unknown` is not a sample: it is the absence of one, which is what a
    -- provider the runner has never reached still carries.
    status text not null,
    -- How long the probe took end to end. `>= 0` because a clock that went backwards is a bug in
    -- the probe, not a fact about the provider, and the constraint turns that bug into a loud
    -- error instead of a sparkline that dips below zero.
    latency_ms integer not null,
    -- The HTTP status the endpoint answered with, when it answered at all.
    http_status integer,
    -- The endpoint's own words when it refused, already stripped of anything key-shaped.
    error text,
    checked_at timestamptz not null default now()
);

alter table ai_provider_health
    add constraint ai_provider_health_status_check check (status in ('ok', 'degraded', 'down'));

alter table ai_provider_health
    add constraint ai_provider_health_latency_check check (latency_ms >= 0);

-- The Health tab reads the newest samples for one provider; the pruner and the status computation
-- read the whole recent window. Both are per-provider and newest-first, so the index is
-- `(provider_id, checked_at desc)` rather than the reverse: an operator asking "what happened to
-- this provider" never wants another provider's rows.
create index ai_provider_health_provider_recent_idx
    on ai_provider_health (provider_id, checked_at desc);

-- The pruner walks every sample older than the retention window without touching a provider, so
-- it needs its own entry.
create index ai_provider_health_checked_at_idx
    on ai_provider_health (checked_at);

-- Per-provider call counters. The runtime records one row per completed call so the Usage tab is
-- a sum over real calls rather than an in-memory number that resets when the process restarts.
--
-- `first_byte_at` is the field that makes the failover rule checkable: a retry is legal only
-- before the first byte reached a subscriber, so the column records whether that happened rather
-- than leaving the rule to a comment.
create table ai_provider_usage (
    id bigserial primary key,
    provider_id uuid not null references ai_providers (id) on delete cascade,
    model_key text,
    -- What the call was for (chat, embedding, image — the capability the call exercised).
    task text not null default 'chat',
    -- How the provider answered.
    outcome text not null,
    -- HTTP status the endpoint answered with, when it answered.
    http_status integer,
    -- Prompt and completion tokens, `null` where the endpoint reported none — a stream that ends
    -- without usage records `null` rather than inventing zero, because a zero would silently
    -- become a real cost in the Usage tab's totals.
    prompt_tokens integer,
    completion_tokens integer,
    -- End-to-end wall time of the call.
    latency_ms integer not null default 0,
    -- Whether a failover substituted another provider for this one, and which.
    substituted_from uuid references ai_providers (id) on delete set null,
    -- When the first byte reached a subscriber. `null` means it never did, which is exactly the
    -- window in which a retry is safe.
    first_byte_at timestamptz,
    created_at timestamptz not null default now()
);

alter table ai_provider_usage
    add constraint ai_provider_usage_outcome_check
    check (outcome in ('ok', 'error', 'refused'));

alter table ai_provider_usage
    add constraint ai_provider_usage_latency_check check (latency_ms >= 0);

-- The Usage tab groups by day over a 30-day window, and the cost manager (REQ-104) asks the same
-- question per model, so the index leads with the provider.
create index ai_provider_usage_provider_recent_idx
    on ai_provider_usage (provider_id, created_at desc);

create index ai_provider_usage_provider_model_idx
    on ai_provider_usage (provider_id, model_key);
