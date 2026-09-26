-- Omnion · 0012 · search: the one search box's index
--
-- docs/requests/REQ-002. The platform's search is an index, not a scatter of per-table queries:
-- every searchable thing becomes one `search_documents` row, written by the provider that owns
-- its domain (pages, media, users, sites today), maintained from the event bus and rebuilt from
-- scratch by a reindex pass. One row answers the palette, the results screen and later the
-- public site's search — so ranking, scoping and permission rules live in exactly one place.
--
-- The document vector is written by the indexer that owns the row (title A, tags B, subtitle C,
-- body D) with the `simple` configuration on purpose: it does not stem, so Turkish and English
-- content behave alike, and the language-specific configurations arrive with the localization
-- work (REQ-020). It is a plain column rather than a generated one because `array_to_string` —
-- the only sane way to fold a tag array into the vector — is STABLE, not IMMUTABLE, and a
-- generated column refuses it; the indexer is the single writer, and every write recomputes it.
-- Prefix and near-miss matching rides `pg_trgm` — hence the extension below; an installation
-- whose role may not create extensions reports the failure here, loudly, instead of silently
-- degrading search to exact-word matching.
--
-- Ranking is `ts_rank_cd` with the weights of `search_settings` (title/tags/subtitle/body), so
-- an operator can tune what "best match" means without a deployment.
--
-- Released migrations are append-only (docs/05-VERSIONING.md).

-- The extension is what carries prefix and near-miss matching. An installation whose role may
-- not create extensions gets a sentence it can act on, not a raw privilege error somewhere in a
-- migration runner's log.
do $$
begin
    create extension if not exists pg_trgm;
exception when insufficient_privilege then
    raise exception using
        message = 'Omnion search needs the pg_trgm extension',
        detail = 'Ask an administrator to run "create extension pg_trgm" in this database and re-run the migrations.',
        errcode = 'insufficient_privilege';
end
$$;

-- One searchable thing: one row per (provider, entity_type, entity_id).
create table search_documents (
    id                bigint      generated always as identity primary key,
    organization_id   uuid        references organizations (id) on delete cascade,
    site_id           uuid        references sites (id) on delete cascade,
    provider          text        not null,
    entity_type       text        not null,
    entity_id         text        not null,
    title             text        not null,
    subtitle          text        not null default '',
    url               text        not null,
    language          text        not null default 'en',
    visibility        text        not null default 'internal',
    owner_user_id     uuid        references users (id) on delete set null,
    tags              text[]      not null default '{}',
    body              text        not null default '',
    entity_updated_at timestamptz,
    indexed_at        timestamptz not null default now(),
    document          tsvector    not null default ''::tsvector,
    constraint search_documents_provider_key unique (provider, entity_type, entity_id),
    constraint search_documents_provider_format check (provider ~ '^[a-z][a-z0-9_]{0,62}$'),
    constraint search_documents_entity_type_format check (entity_type ~ '^[a-z][a-z0-9_]{0,62}$'),
    constraint search_documents_title_not_blank check (length(btrim(title)) > 0),
    constraint search_documents_visibility_check check (visibility in ('public', 'internal', 'private'))
);

create index search_documents_document_idx on search_documents using gin (document);
create index search_documents_title_trgm_idx on search_documents using gin (title gin_trgm_ops);
create index search_documents_organization_site_idx on search_documents (organization_id, site_id);
create index search_documents_entity_type_updated_idx
    on search_documents (entity_type, entity_updated_at desc);
create index search_documents_owner_idx on search_documents (owner_user_id)
    where owner_user_id is not null;

-- The caller's own search history, kept small: every write prunes to the newest 20 rows.
create table search_recent (
    id         bigserial   primary key,
    user_id    uuid        not null references users (id) on delete cascade,
    query      text        not null,
    created_at timestamptz not null default now(),
    constraint search_recent_query_not_blank check (length(btrim(query)) > 0)
);

create index search_recent_user_idx on search_recent (user_id, created_at desc);

-- One row of installation-wide search settings: the ranking weights and which providers answer.
create table search_settings (
    id                smallint    primary key default 1,
    weights           jsonb       not null default '{"title": 6, "tags": 4, "subtitle": 3, "body": 1}'::jsonb,
    enabled_providers text[]      not null default '{pages,media,users,sites}',
    updated_at        timestamptz not null default now(),
    constraint search_settings_singleton check (id = 1)
);

insert into search_settings (id) values (1);

-- The indexer's cursor over the event bus (the same lock pattern as automation_cursor): one
-- drain tick reads the events above it, applies them to the index and advances the row.
create table search_cursor (
    id            smallint    primary key default 1,
    last_event_id bigint      not null default 0,
    updated_at    timestamptz not null default now(),
    constraint search_cursor_singleton check (id = 1),
    constraint search_cursor_non_negative check (last_event_id >= 0)
);

insert into search_cursor (id, last_event_id) values (1, 0);
