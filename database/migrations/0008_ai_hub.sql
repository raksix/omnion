-- Omnion · 0008 · AI Hub: providers and the models they serve
--
-- The AI Hub (docs/06-AI-HUB.md §1–§3, phase P11) needs two tables: the connections an
-- installation made (providers) and the models those connections serve, with the capability
-- metadata the router picks by. Released migrations are append-only (docs/05-VERSIONING.md).

-- One connected AI provider. `api_key` is written through the API and never read back out of
-- it: the platform stores it to sign its calls, the panel only ever learns whether one exists.
create table ai_providers (
    id          uuid        primary key default gen_random_uuid(),
    name        text        not null,
    protocol    text        not null default 'openai_compatible',
    base_url    text        not null,
    api_key     text,
    enabled     boolean     not null default true,
    is_default  boolean     not null default false,
    created_at  timestamptz not null default now(),
    updated_at  timestamptz not null default now(),
    constraint ai_providers_protocol_check check (protocol in ('openai_compatible')),
    constraint ai_providers_name_check check (length(btrim(name)) between 1 and 64),
    constraint ai_providers_base_url_check check (base_url ~ '^https?://[^[:space:]]+$')
);

-- Provider names are how an operator (and a `provider/model` address) refers to a connection,
-- so they are unique per installation, case-insensitively.
create unique index ai_providers_name_key on ai_providers (lower(name));

-- An installation has at most one default provider: the one an ambiguous model key prefers.
create unique index ai_providers_default_key on ai_providers (is_default) where is_default;

-- One model of one provider (docs/06-AI-HUB.md §3). The capability flags are what the router
-- and, later, the agent runtime read to decide whether a model fits a job. Exactly one enabled
-- model may be the installation's default — the partial index below keeps that a data rule, and
-- the store repairs the default whenever a model or a provider goes away.
create table ai_models (
    id                  uuid        primary key default gen_random_uuid(),
    provider_id         uuid        not null references ai_providers (id) on delete cascade,
    model_key           text        not null,
    display_name        text,
    context_window      integer,
    supports_tools      boolean     not null default false,
    supports_vision     boolean     not null default false,
    supports_streaming  boolean     not null default true,
    supports_embeddings boolean     not null default false,
    enabled             boolean     not null default true,
    is_default          boolean     not null default false,
    created_at          timestamptz not null default now(),
    updated_at          timestamptz not null default now(),
    constraint ai_models_key_check check (length(btrim(model_key)) between 1 and 200),
    constraint ai_models_context_window_check check (context_window is null or context_window > 0)
);

create unique index ai_models_provider_key_key on ai_models (provider_id, model_key);
create unique index ai_models_default_key on ai_models (is_default) where is_default;
create index ai_models_provider_id_idx on ai_models (provider_id);
