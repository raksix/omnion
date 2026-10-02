-- Omnion · 0139 · content: the SEO toolkit — page metadata, redirects, sitemap, broken links
-- (REQ-064, slice 3).
--
-- Search tooling is the one part of a CMS that touches every page read, so the schema here is
-- built around one rule: **nothing that a visitor can see is computed on the request path**.
-- A crawler gets a page's tags from columns, a sitemap from a stored string, and a redirect from
-- one indexed lookup. The generators exist, and the panel previews their output, but a page view
-- never waits for one.
--
-- 1. **Page metadata is columns on `pages`, not a table.** Every page render reads it, one row
--    at a time, and a side table would make the renderer's hot path a join for data that is
--    1:1 with the row it already has. The trade is that `ALTER TABLE` rather than `CREATE`, and
--    the fields are all nullable, so existing rows are untouched and a page that never had an
--    editor still renders.
--
-- 2. **`structured_data` is a JSON object with a checked `type`, not free text.** A schema.org
--    type is a closed vocabulary: letting the owner type `Articel` produces JSON-LD that every
--    consumer silently ignores, and the panel would be showing them a green check on a tag that
--    does nothing. The type is validated in the store AND constrained in the column, and the
--    generated body is built from the page's own fields rather than typed by the owner.
--
-- 3. **Redirects are per-site and unique on `(site_id, from_path, pattern)`.** The pattern
--    column is in the key because a literal `/old` and a regex `/old-.*` are different rules
--    that may both be wanted; without it the second one cannot be saved and the owner is left
--    deleting the first to find out why.
--
-- 4. **Broken links are found, never guessed at request time.** `crawl-lite` walks the stored
--    page bodies (REQ-064 slice 3) and writes rows; the panel reads rows. A link checker that
--    refetches the site on every page view is a load generator pointed at itself.

alter table pages
    add column seo_title text,
    add column seo_description text,
    add column canonical_url text,
    add column og_title text,
    add column og_description text,
    add column og_image_media_id uuid references media (id) on delete set null,
    add column twitter_card text not null default 'summary_large_image',
    add column robots text not null default 'index,follow',
    add column structured_data_type text,
    add column structured_data jsonb not null default '{}'::jsonb;

-- Every CHECK below is its own ALTER rather than a clause in the column block above, and the
-- reason is specific: a `--` comment between two clauses makes PostgreSQL drop the REST OF THE
-- LINE, which here is the entire remaining statement, and the migration dies at the first
-- constraint with `syntax error at or near "constraint"` and a character offset into the middle
-- of a comment. Comments belong BETWEEN statements.
alter table pages
    add constraint pages_twitter_card_check
        check (twitter_card in ('summary', 'summary_large_image')),
    add constraint pages_structured_data_type_check check (
        structured_data_type is null or structured_data_type in (
            'Article', 'Organization', 'FAQPage', 'Product', 'BreadcrumbList', 'WebSite'
        )
    );

-- A canonical URL is absolute: a site-relative one would be resolved against whatever host the
-- crawler happened to reach, which is exactly the ambiguity canonical exists to remove.
alter table pages
    add constraint pages_canonical_url_check check (
        canonical_url is null or canonical_url ~ '^https?://[^/]+'
    );

alter table pages
    add constraint pages_seo_title_length
        check (seo_title is null or length(seo_title) <= 255);

alter table pages
    add constraint pages_seo_description_length
        check (seo_description is null or length(seo_description) <= 500);

create index pages_sitemap_idx on pages (site_id, page_type, status) where status = 'published';

-- Redirect rules. `from_path` is a site-relative path WITHOUT a query string: the query is not
-- part of a page's identity on this platform, and matching on it turns every campaign link into
-- a rule of its own.
create table cms_seo_redirects (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    site_id uuid not null references sites (id) on delete cascade,
    from_path text not null,
    to_path text not null,
    status_code integer not null default 301,
    pattern text not null default 'literal',
    enabled boolean not null default true,
    -- `bigint`, not `integer`: the store's `Redirect.hits` is an `i64` because it is bound as a
    -- `i64` in the incrementing UPDATE, and sqlx refuses to decode INT4 into one. The column
    -- type and the Rust type have to agree exactly or every read of the list is a 500.
    hits bigint not null default 0,
    last_hit_at timestamptz,
    created_by uuid references users (id) on delete set null,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now(),
    constraint cms_seo_redirects_from_path_format check (from_path ~ '^/[^?#]*$'),
    constraint cms_seo_redirects_to_path_format check (to_path ~ '^/[^?#]*$'),
    constraint cms_seo_redirects_status_code check (status_code in (301, 302)),
    constraint cms_seo_redirects_pattern check (pattern in ('literal', 'regex')),
    constraint cms_seo_redirects_hits_positive check (hits >= 0),
    unique (site_id, from_path, pattern)
);

create index cms_seo_redirects_site_id_idx on cms_seo_redirects (site_id) where enabled;
-- The public resolver's inner loop is "every enabled rule of this site, literal first" — this
-- index is the whole reason a 2,000-rule site resolves a request without a sequential scan.
create index cms_seo_redirects_lookup_idx on cms_seo_redirects (site_id, pattern) where enabled;

-- Broken internal links, found by the crawl-lite pass and read by the panel.
create table cms_broken_links (
    id uuid primary key default gen_random_uuid(),
    site_id uuid not null references sites (id) on delete cascade,
    source_page_id uuid references pages (id) on delete cascade,
    target_url text not null,
    anchor_text text,
    status integer,
    ignored boolean not null default false,
    found_at timestamptz not null default now(),
    unique (site_id, source_page_id, target_url)
);

create index cms_broken_links_site_id_idx on cms_broken_links (site_id) where not ignored;

-- Per-site SEO settings. The sitemap is stored, not generated per request (see the header).
create table cms_seo_settings (
    site_id uuid primary key references sites (id) on delete cascade,
    organization_id uuid not null references organizations (id) on delete cascade,
    sitemap_types text[] not null default '{}',
    -- `double precision`, not `numeric` and not `real`: the workspace carries no decimal crate, so
    -- a NUMERIC column cannot be decoded into the `f64` the settings row declares, and `real` is
    -- FLOAT4 where `f64` is FLOAT8 — both answer 500 on every read. A sitemap priority has one
    -- decimal place and no arithmetic beyond a bound check, so double precision loses nothing,
    -- and a type the store cannot read is worth less than a type that is exact.
    default_priority double precision not null default 0.5,
    default_change_frequency text not null default 'weekly',
    sitemap_xml text,
    sitemap_last_generated_at timestamptz,
    robots_txt text not null default E'User-agent: *\nAllow: /',
    updated_by uuid references users (id) on delete set null,
    updated_at timestamptz not null default now(),
    constraint cms_seo_settings_priority_range check (default_priority between 0.0 and 1.0),
    constraint cms_seo_settings_frequency_check check (
        default_change_frequency in ('always', 'hourly', 'daily', 'weekly', 'monthly', 'yearly', 'never')
    )
);
