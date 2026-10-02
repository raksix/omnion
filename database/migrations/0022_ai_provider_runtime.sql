-- Omnion · 0022 · AI provider runtime: kind, timing, priority and health (REQ-097)
--
-- v0 stored one connection shape: an OpenAI-compatible provider with a name, a base URL and a
-- key. The provider runtime (docs/requests/REQ-097) turns that into a runtime, and this migration
-- is where the new facts live. Released migrations are append-only (docs/05-VERSIONING.md), so
-- 0008 is left exactly as it shipped and widened here.
--
-- Slice 1 lands what the protocol adapters and the connection test need: which kind of endpoint a
-- provider is (cloud or local), how patient a call may be, how often a call may be retried, where
-- it sits in the failover order, and what the last health probe found. Slices 2 and 3 add the
-- capability flags on `ai_models`, the health-sample table and the failover order.

-- Which kind of endpoint this is. `local` is what the panel and the operator read; the runtime
-- treats both the same way (both are dialled by the server), the flag exists so a screen can
-- group them and so an operator can see at a glance that a provider lives on their own network.
alter table ai_providers
    add column kind text not null default 'cloud';

alter table ai_providers
    add constraint ai_providers_kind_check check (kind in ('cloud', 'local'));

-- The protocol check from 0008 named one adapter. The runtime ships three, and the fourth
-- attaches through the trait without a new value here.
alter table ai_providers drop constraint ai_providers_protocol_check;

alter table ai_providers
    add constraint ai_providers_protocol_check
    check (protocol in ('openai_compatible', 'anthropic_messages', 'google_gemini'));

-- How long one call to this provider may take, and how often a *pre-first-byte* failure is
-- retried. A stream that already produced bytes is never replayed, so this bounds the phase in
-- which a retry is safe rather than the whole conversation.
alter table ai_providers
    add column timeout_ms integer not null default 30000;

alter table ai_providers
    add constraint ai_providers_timeout_check check (timeout_ms between 1000 and 120000);

alter table ai_providers
    add column max_retries integer not null default 1;

alter table ai_providers
    add constraint ai_providers_max_retries_check check (max_retries between 0 and 5);

-- Where the provider sits in the failover chain: lower numbers are asked first, and a tie is
-- broken by `lower(name)` so the order is total and reproducible.
alter table ai_providers
    add column priority integer not null default 100;

alter table ai_providers
    add constraint ai_providers_priority_check check (priority between 1 and 1000);

-- What the last probe found. `unknown` is a real value, not a placeholder for NULL: a provider
-- that has never been probed has genuinely no verdict yet, and the panel says so.
alter table ai_providers
    add column last_health text not null default 'unknown';

alter table ai_providers
    add constraint ai_providers_health_check
    check (last_health in ('ok', 'degraded', 'down', 'unknown'));

alter table ai_providers
    add column last_checked_at timestamptz;

alter table ai_providers
    add column last_error text;

-- The failover walk reads `where enabled order by priority, lower(name)`, so the order is served
-- by one index instead of a sort over the whole table.
create index ai_providers_priority_idx on ai_providers (priority, lower(name));
