-- Omnion · 0009 · Events and webhooks: the bus and its signed deliveries
--
-- The event bus (docs/01-VISION.md §13) records what happened on the platform, and every
-- webhook endpoint subscribed to an event receives one signed delivery of it
-- (docs/02-ARCHITECTURE.md — "Webhooks can emit events such as page.published"). Three tables
-- carry that: the events themselves, the endpoints an organization registered, and the delivery
-- queue the background worker drains. Released migrations are append-only
-- (docs/05-VERSIONING.md).

-- One recorded fact. `name` is a dotted, lower-case event name (`page.published`), and the
-- payload carries the identifiers a consumer needs — never a rendered document and never a
-- secret. `organization_id` is how fan-out finds the endpoints an event belongs to: a
-- platform-level event without an organization stays on the bus and fans out to nobody.
create table events (
    id              bigint      generated always as identity primary key,
    name            text        not null,
    organization_id uuid        references organizations (id) on delete cascade,
    site_id         uuid        references sites (id) on delete set null,
    actor_user_id   uuid        references users (id) on delete set null,
    payload         jsonb       not null default '{}'::jsonb,
    created_at      timestamptz not null default now(),
    constraint events_name_check check (name ~ '^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)+$')
);

create index events_name_id_idx on events (name, id desc);
create index events_organization_id_idx on events (organization_id, id desc);

-- One webhook endpoint of one organization. `url` receives a POST per subscribed event and
-- `secret` signs it (HMAC-SHA256): the row stores the secret so deliveries can be signed, and
-- the API never hands it back — an operator who loses it rotates in a new one.
create table webhook_endpoints (
    id              uuid        primary key default gen_random_uuid(),
    organization_id uuid        not null references organizations (id) on delete cascade,
    name            text        not null,
    url             text        not null,
    secret          text        not null,
    events          text[]      not null,
    enabled         boolean     not null default true,
    created_by      uuid        references users (id) on delete set null,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),
    constraint webhook_endpoints_name_check check (length(btrim(name)) between 1 and 64),
    constraint webhook_endpoints_url_check check (url ~ '^https?://[^[:space:]]+$'),
    constraint webhook_endpoints_secret_check check (length(secret) between 16 and 128),
    constraint webhook_endpoints_events_check check (cardinality(events) between 1 and 32)
);

-- An operator tells endpoints apart by name inside one organization.
create unique index webhook_endpoints_org_name_key
    on webhook_endpoints (organization_id, lower(name));

-- One queued delivery of one event to one endpoint. The worker claims due rows in batches
-- (`for update skip locked`, with a lease) so several instances never hand the same delivery
-- to two runners; `attempts` counts the try in flight, and a row that ran out of attempts stays
-- in the table as `failed` — the record of what the endpoint never accepted.
create table webhook_deliveries (
    id              uuid        primary key default gen_random_uuid(),
    endpoint_id     uuid        not null references webhook_endpoints (id) on delete cascade,
    event_id        bigint      not null references events (id) on delete cascade,
    status          text        not null default 'pending',
    attempts        integer     not null default 0,
    max_attempts    integer     not null default 5,
    next_attempt_at timestamptz not null default now(),
    claimed_at      timestamptz,
    response_status integer,
    error           text,
    delivered_at    timestamptz,
    created_at      timestamptz not null default now(),
    constraint webhook_deliveries_status_check check (status in ('pending', 'delivered', 'failed')),
    constraint webhook_deliveries_attempts_check
        check (attempts >= 0 and max_attempts between 1 and 10)
);

-- One delivery per endpoint per event: recording an event twice can never double-deliver.
create unique index webhook_deliveries_endpoint_event_key
    on webhook_deliveries (endpoint_id, event_id);
create index webhook_deliveries_due_idx
    on webhook_deliveries (next_attempt_at) where status = 'pending';
create index webhook_deliveries_endpoint_created_idx
    on webhook_deliveries (endpoint_id, created_at desc);
