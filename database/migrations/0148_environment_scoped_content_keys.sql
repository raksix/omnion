-- REQ-017 slice 2: let a staging environment actually hold a copy of a page.
--
-- Migration 0145 put `environment_id` on `pages` and then left every *natural key* alone. That
-- makes the column unusable: a page's slug is unique per `(site_id, slug)` across the whole
-- installation, so a staging copy of `/about` cannot exist next to the production `/about` at
-- all. The clone runs, copies nothing, and reports `done` with 0 of 2 rows — an environment that
-- looks filled and is empty, which is the worst shape this request could take.
--
-- So the environment becomes part of every key that identifies content within an environment:
--
--   * `pages (site_id, slug)` → `(site_id, environment_id, slug)`. A slug is unique *in an
--     environment*; two environments are two different sets of pages, which is the whole point.
--   * `translations (resource_type, resource_id, language, field)` gains `environment_id`. A
--     translation of a staging page is a different row from a translation of the production
--     page with the same resource id, because after a promotion they are two pages again.
--   * `page_revisions (page_id, revision_no)` needs nothing: `page_id` is now a per-environment
--     id, so revisions follow automatically.
--
-- The copy itself therefore mints a **new page id** per row rather than reusing the production
-- one. That is the change with the widest blast radius in this request, and it is worth being
-- explicit about why: a shared id would make "which page is this" ambiguous everywhere else in
-- the platform — a revision, a translation, an analytics row and a media reference all hang off
-- `page_id`, and two environments sharing one id would make every one of them ambiguous too.
-- Fresh ids plus a remap table is the only design where a staging page is a page.
--
-- `environment_id` is on every one of these rows already (0145 made it NOT NULL), so the new
-- indexes are built on populated columns and `concurrently` is not needed: there is no window
-- where a duplicate would be readable.

-- The environment joins the page's natural key.
alter table pages drop constraint if exists pages_site_slug_key;
alter table pages add constraint pages_site_slug_key unique (site_id, environment_id, slug);

-- And the same for a translation's identity.
alter table translations drop constraint if exists translations_unique_key;
alter table translations
    add constraint translations_unique_key
    unique (resource_type, resource_id, environment_id, language, field);

-- The listing query a staging environment's own pages run: "this environment's pages for this
-- site", which the new key now serves directly.
create index pages_by_site_environment on pages (site_id, environment_id);
