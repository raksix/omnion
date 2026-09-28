-- Omnion · 0026 · content: reusable patterns and page templates
--
-- REQ-063 slice 3. A pattern is a named block group an author can drop into any page; a page
-- template is a whole page's worth of blocks with sample content. Both store exactly what a
-- revision stores — a block tree as JSON — so inserting one is a *copy with fresh block ids*,
-- not a translation step between two shapes. That is the whole design: if a pattern's blocks
-- had their own representation, "insert this pattern" would be a lossy round trip and the tree
-- the author then edits would not be the tree the pattern describes.
--
-- Both tables are organization-scoped, not site-scoped, because a pattern is reused across the
-- sites of one organization: a landing hero written once is a landing hero on every tenant. The
-- page it is inserted into decides which site the blocks land on.
--
-- `is_system` marks the templates that ship with the platform (landing, about, pricing, blog
-- post, contact). They are seeded per organization on first read rather than inserted by this
-- migration, because their blocks are the product's sample content and belong in code where they
-- can be reviewed and tested — a template row an operator hand-edited is a page nobody can
-- reproduce.

create table content_patterns (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    key text not null,
    name text not null,
    category text not null default 'general',
    description text,
    blocks jsonb not null default '[]'::jsonb,
    created_by uuid references users (id) on delete set null,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    constraint content_patterns_blocks_is_array check (jsonb_typeof(blocks) = 'array'),
    constraint content_patterns_key_shape
        check (key ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
    constraint content_patterns_category_shape
        check (category ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
    unique (organization_id, key)
);

create index content_patterns_org_category_idx
    on content_patterns (organization_id, category);

comment on table content_patterns is
    'Reusable block groups (REQ-063 slice 3). Inserting one copies its blocks with fresh ids.';

create table content_page_templates (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    key text not null,
    name text not null,
    page_type text not null default 'page',
    description text,
    blocks jsonb not null default '[]'::jsonb,
    is_system boolean not null default false,
    created_by uuid references users (id) on delete set null,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    constraint content_page_templates_blocks_is_array
        check (jsonb_typeof(blocks) = 'array'),
    constraint content_page_templates_key_shape
        check (key ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
    unique (organization_id, key)
);

create index content_page_templates_org_idx on content_page_templates (organization_id);

comment on table content_page_templates is
    'Whole-page block templates with sample content (REQ-063 slice 3). is_system templates ship with the platform.';

-- Creating a page from a template writes a real page plus its first draft revision, so it goes
-- through the same store path as any other create rather than a copy of the insert statements.
-- The revision that lands therefore carries the same append-only guarantees: the template is a
-- starting point, never a live link.
