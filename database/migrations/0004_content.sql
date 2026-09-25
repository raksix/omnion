-- Omnion · 0004 · content: pages, their revision history and the translations skeleton
--
-- v0 of the content model (docs/05-VERSIONING.md §4–§7, docs/01-VISION.md §5, §7). A page is
-- one addressable piece of content of one site; its history is a list of append-only revision
-- rows stamped with a revision number. Drafts are edited by appending a new revision,
-- publishing freezes one revision and retires the one before it, and restoring copies an
-- older revision forward — history is never rewritten, which is what makes compare and
-- restore possible. Translations hang off a revision as rows (content → translations[lang],
-- never `title_tr` columns). Released migrations are append-only (docs/05-VERSIONING.md).

-- Pages: the addressable unit of content. `slug` is the page's address inside its site and is
-- unique per site; `status` mirrors the lifecycle (draft → published, or archived);
-- `published_revision_id` points at the revision visitors currently see (null until the first
-- publish). The foreign key to `page_revisions` is added after that table exists.
create table pages (
    id                    uuid        primary key default gen_random_uuid(),
    site_id               uuid        not null references sites (id) on delete cascade,
    slug                  text        not null,
    page_type             text        not null default 'page',
    status                text        not null default 'draft',
    published_revision_id uuid,
    created_by            uuid        references users (id) on delete set null,
    created_at            timestamptz not null default now(),
    updated_at            timestamptz not null default now(),
    constraint pages_slug_format check (
        slug = lower(slug) and slug ~ '^[a-z0-9]([a-z0-9-]{0,94}[a-z0-9])?$'
    ),
    constraint pages_page_type_format check (page_type ~ '^[a-z][a-z0-9_]{0,62}$'),
    constraint pages_status_check check (status in ('draft', 'published', 'archived')),
    constraint pages_site_slug_key unique (site_id, slug)
);

create index pages_site_id_idx on pages (site_id);

-- Revision history: one immutable content version of one page. At most one revision is the
-- working draft and at most one is published — the partial unique indexes below hold the
-- store to that invariant even under concurrency. A restored revision records the row it was
-- copied from, so the history view can explain where a version came from.
create table page_revisions (
    id               uuid        primary key default gen_random_uuid(),
    page_id          uuid        not null references pages (id) on delete cascade,
    revision_no      integer     not null,
    state            text        not null default 'draft',
    title            text        not null,
    body             text        not null default '',
    summary          text,
    restored_from_id uuid        references page_revisions (id) on delete set null,
    created_by       uuid        references users (id) on delete set null,
    created_at       timestamptz not null default now(),
    published_at     timestamptz,
    constraint page_revisions_number_positive check (revision_no >= 1),
    constraint page_revisions_state_check check (state in ('draft', 'published', 'archived')),
    constraint page_revisions_title_not_blank check (length(btrim(title)) > 0),
    constraint page_revisions_page_number_key unique (page_id, revision_no)
);

create index page_revisions_page_id_idx on page_revisions (page_id);
create unique index page_revisions_draft_key on page_revisions (page_id) where state = 'draft';
create unique index page_revisions_published_key on page_revisions (page_id)
    where state = 'published';

alter table pages
    add constraint pages_published_revision_fkey
    foreign key (published_revision_id) references page_revisions (id) on delete set null;

-- Translations are rows, not columns (docs/01-VISION.md §5): one value of one field of one
-- resource in one language. v0 writes them for page revisions; the shape already accepts any
-- resource type, which is what the Translation Center and the Translation Memory grow on.
create table translations (
    id              uuid        primary key default gen_random_uuid(),
    organization_id uuid        not null references organizations (id) on delete cascade,
    resource_type   text        not null,
    resource_id     uuid        not null,
    language        text        not null,
    field           text        not null,
    value           text        not null,
    created_by      uuid        references users (id) on delete set null,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),
    constraint translations_resource_format check (resource_type ~ '^[a-z][a-z0-9_]{0,62}$'),
    constraint translations_language_format check (language ~ '^[a-z]{2,8}(-[a-z0-9]{2,8})*$'),
    constraint translations_field_format check (field ~ '^[a-z][a-z0-9_]{0,62}$'),
    constraint translations_unique_key unique (resource_type, resource_id, language, field)
);

create index translations_resource_idx on translations (resource_type, resource_id);
create index translations_organization_id_idx on translations (organization_id);
