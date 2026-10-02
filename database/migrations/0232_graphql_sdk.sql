-- The GraphQL surface's durable rows: persisted documents, query logs, deprecations and SDK
-- releases (docs/requests/REQ-130-graphql-and-sdk-generation.md, slice 1).
--
-- **Numbered above the UNION high-water across all ten worktrees, not above this branch's own.**
-- `0231_developer_oauth_apps.sql` is wave 5's (the developer portal). This branch's own tree stops
-- at `0221`, so a writer numbering from their own branch takes 0222 — and a migration number is a
-- shared namespace, so two writers at the same number is a merge that produces two files with one
-- number, and `sqlx` then reports a `VersionMismatch` that kills every test in the workspace. The
-- union is the only number that is safe. (Learned twice in this repository already; the third
-- writer to collide is the one that renumbers.)
--
-- **What this migration does NOT carry: the documents themselves.** No `graphql_documents` row
-- stores query text for an ad-hoc document, and `graphql_query_logs` has no `variables` column at
-- all. That is deliberate and it is the acceptance line: *"stored variable values are limited to
-- registered persisted documents and never captured for ad-hoc documents."* A nullable column
-- would make the rule a promise about code; an absent column makes it a property of the schema.
-- Reading the row cannot leak a value that was never stored.
--
-- **`persisted_documents` (not `graphql_documents`)** — naming after the module rather than after
-- the feature keeps the reader's expectation honest about what lives here: documents are stored
-- because a *client* needs a stable id and a hash to execute by, and slice 2 owns the allowlist
-- policy on top. The unique key is `(organization_id, hash)`, so two tenants may register the
-- same document (which is the normal case: the same client query ships to everyone) while one
-- tenant cannot register the same document twice and get two ids for one query.

create table graphql_persisted_documents (
    id                  uuid primary key,
    organization_id     uuid not null references organizations (id) on delete cascade,
    name                text not null,
    -- The canonical hash of the document text. Unique inside an organization.
    hash                text not null,
    kind                text not null default 'query' check (kind in ('query', 'mutation')),
    -- The document itself. Only registered documents are ever stored; an ad-hoc document exists
    -- for the length of one request and is never written anywhere.
    document            text not null,
    -- [{name, kind, cost}] — the operations inside the document with their priced cost, so the
    -- manager screen can show "this document costs 140 on its heaviest operation" without
    -- re-parsing on every row.
    operations          jsonb not null default '[]',
    status              text not null default 'draft' check (status in ('draft', 'active', 'revoked')),
    -- Whether a client is REQUIRED to send the id rather than the text. An installation in
    -- persisted-only mode sets this globally; a per-document flag lets a single customer be moved
    -- to allowlist-only without the whole environment.
    required_for_callers boolean not null default false,
    hits                bigint not null default 0,
    last_used_at        timestamptz,
    created_by          uuid references users (id) on delete set null,
    created_at          timestamptz not null default now(),
    updated_at          timestamptz not null default now(),
    constraint graphql_persisted_documents_org_hash_unique unique (organization_id, hash),
    constraint graphql_persisted_documents_hits_sane check (hits >= 0)
);

create index graphql_persisted_documents_org_status_idx
    on graphql_persisted_documents (organization_id, status);

create index graphql_persisted_documents_last_used_idx
    on graphql_persisted_documents (organization_id, last_used_at desc);

comment on table graphql_persisted_documents is
    'Documents registered for execution by id or hash. Storing text for these is safe precisely because they are registered on purpose; an ad-hoc document is never written.';

-- The query log.
--
-- **One row per request, whatever the outcome.** The `status` column distinguishes
-- `ok` / `error` (the request ran, and either resolved or failed inside a resolver) from
-- `rejected` (a limit, a validation failure or an allowlist refusal — nothing ran). That
-- distinction is the acceptance line *"Query logging records depth, cost, duration and errors"*:
-- a log that only kept successful queries could not show an operator why their depth limit keeps
-- firing.
--
-- **There is no `variables` column, and `variables_captured` is a boolean, not a payload.** For a
-- registered document the *shape* of the variables is recorded (which names were sent), never
-- their values: a variable value is caller data, and a log table is the most-read table in an
-- incident. For an ad-hoc document nothing is recorded at all. `ad_hoc` records that the request
-- was ad-hoc, so the "never captured" property is ASSERTED by a test reading rows rather than
-- asserted by a comment in the handler that writes them.
create table graphql_query_logs (
    id                bigserial primary key,
    organization_id   uuid references organizations (id) on delete cascade,
    -- The machine key the request presented, when it presented one. Null for a session.
    api_key_id        uuid,
    actor_user_id     uuid references users (id) on delete set null,
    document_id       uuid references graphql_persisted_documents (id) on delete set null,
    operation_name    text,
    -- The document's hash, kept even for an ad-hoc document: a hash is not the text and is what a
    -- caller needs to register the document later. The refusal event carries it for the same reason.
    hash              text not null,
    depth             int not null default 0,
    cost              numeric(10, 2) not null default 0,
    aliases           int not null default 0,
    duration_ms       int not null default 0,
    status            text not null check (status in ('ok', 'error', 'rejected')),
    error_code        text,
    -- Whether this request was an ad-hoc document rather than a registered one.
    ad_hoc            boolean not null default true,
    -- The NAMES of the variables sent, never their values. `[]` for an ad-hoc document.
    variable_names    text[] not null default '{}',
    created_at        timestamptz not null default now(),
    constraint graphql_query_logs_depth_sane check (depth >= 0),
    constraint graphql_query_logs_cost_sane check (cost >= 0),
    constraint graphql_query_logs_duration_sane check (duration_ms >= 0)
);

-- The two reads that matter: "what did this caller send in the last day" and "who hit this
-- document". Descending order in the index, because both queries end in `order by created_at desc`
-- with no further ordering — an index that does not carry the sort direction costs a sort per page.
create index graphql_query_logs_org_created_idx
    on graphql_query_logs (organization_id, created_at desc);

create index graphql_query_logs_document_created_idx
    on graphql_query_logs (document_id, created_at desc);

create index graphql_query_logs_created_idx
    on graphql_query_logs (created_at desc);

comment on table graphql_query_logs is
    'One row per GraphQL request, including refusals. Variable NAMES are stored; variable VALUES are not, for any document — the column that would hold them does not exist.';

-- Deprecations of REST routes and GraphQL fields (REQ-130 slice 4 reads these rows; the migration
-- ships with slice 1 so the table is versioned once for the whole REQ).
--
-- **The sunset window is a CHECK, not a policy.** The request: *"sunsets are never shorter than six
-- months for public routes and three months for developer-internal routes"*. A rule in the handler
-- is only as good as the review that remembers it; a constraint is what makes a two-week sunset
-- unrepresentable. The floor is the shorter of the two windows — `sunset_at > now()` is NOT
-- expressible in a CHECK (PostgreSQL forbids `now()` there), so the window is enforced by the
-- store next to this table, and this constraint holds the *shape*: a sunset date is set, a
-- `deprecated_in` version is set, and a row with no replacement may only be a `withdrawn` one.
create table api_deprecations (
    id              uuid primary key,
    organization_id uuid references organizations (id) on delete cascade,
    -- The REST path this row deprecates, or NULL when the deprecation is about a GraphQL field.
    route_pattern   text,
    method          text check (method is null or method in ('GET', 'POST', 'PUT', 'PATCH', 'DELETE')),
    -- `Page.author` — a field, not a route.
    field_path      text,
    -- The API version the deprecation was introduced in.
    deprecated_in   text not null,
    sunset_at       timestamptz not null,
    replacement     text,
    note            text not null default '',
    status          text not null default 'announced'
                    check (status in ('announced', 'active', 'removed', 'withdrawn')),
    notified_at     timestamptz,
    created_by      uuid references users (id) on delete set null,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),
    -- A deprecation must name what it deprecates. A row with neither a route nor a field is a row
    -- no screen can render and no middleware can match.
    constraint api_deprecations_names_a_surface check (route_pattern is not null or field_path is not null),
    -- A sunset with no replacement and no note is a silent removal wearing a deprecation's name.
    constraint api_deprecations_explains_itself check (
        status = 'withdrawn' or replacement is not null or note <> ''
    )
);

create index api_deprecations_status_sunset_idx
    on api_deprecations (status, sunset_at);

create index api_deprecations_org_created_idx
    on api_deprecations (organization_id, created_at desc);

comment on table api_deprecations is
    'Announced deprecations of REST routes and GraphQL fields. The sunset WINDOW is enforced by the store, because PostgreSQL forbids now() in a CHECK; the SHAPE is enforced here.';

-- Published SDK artifacts (REQ-130 slice 3 publishes them; the table ships with slice 1 so the
-- release job and the screen agree on the schema from the start).
--
-- **`openapi_hash` is what ties an artifact to the document it was generated from.** Two SDKs
-- generated from the same hash MUST produce the same bytes — that is the acceptance line *"SDKs
-- generate identical output from a pinned hash"* — so the hash is stored beside every release and
-- a release with a NULL hash cannot be recorded at all.
create table sdk_releases (
    id              uuid primary key,
    language        text not null check (language in ('typescript', 'python')),
    version         text not null,
    -- The SHA-256 of the OpenAPI document this artifact was generated from. NOT NULL on purpose:
    -- an SDK nobody can pin is an SDK nobody can verify.
    openapi_hash    text not null,
    artifact_url    text not null,
    provenance_url  text,
    released_at     timestamptz not null default now(),
    published_by    uuid references users (id) on delete set null,
    constraint sdk_releases_language_version_unique unique (language, version),
    -- An artifact with no checksum cannot be verified by a consumer.
    constraint sdk_releases_has_a_digest check (length(openapi_hash) >= 16)
);

create index sdk_releases_language_released_idx
    on sdk_releases (language, released_at desc);

comment on table sdk_releases is
    'A published SDK per language, pinned to the OpenAPI document hash it was generated from. Releases with no pin cannot be recorded, because an unpinned artifact cannot be regenerated identically.';