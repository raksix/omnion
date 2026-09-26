# REQ-116 — Blog / Magazine Module

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** modules/blog + themes
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

The classic CMS content type, done properly.

- Posts with categories, tags, authors, featured image, excerpt, reading time.
- Listing templates (grid, list, featured-first) driven by the theme; pagination and infinite scroll options.
- Author pages, category/tag archives, related posts, RSS/Atom feeds, sitemap integration.
- Editorial features: scheduled posts, revisions (shared), comments toggle, sticky posts.
- Multi-site and multilingual aware (post per language tree).

## Implementation spec

> **Module:** `modules/blog` (crate `omnion-module-blog`, workspace member) · **Migration:** `0120_blog.sql` (reserved band 0116–0129 for the content-and-commerce wave; the ledger is append-only — take the next free number if taken) · **Admin routes:** `/blog/*` · **Renderer routes:** `/blog/*` in `apps/web` · **Permission family:** `blog.*` plus `content.pages.publish` for publication · **Depends on:** `crates/content` (the `pages` table and revisions), `crates/events`, `crates/audit`, `crates/media`, `crates/search` (REQ-002 provider registration) · **Reuses:** REQ-064 (SEO fields, comments, publishing queue), REQ-063 (blocks), REQ-110 (editorial states), REQ-018 (draft preview), REQ-020/REQ-114 (translations), REQ-083/REQ-084 (theme sections and SDK).

### Scope (in / out)

**In**

- **Posts as a content type, not a second CMS.** A post is a `pages` row with `page_type = 'post'`, so revisions, scheduled publishing, preview tokens, translations, SEO fields and the block editor work unchanged; this module owns the blog-specific projection (`subtitle`, `excerpt`, `reading_minutes`, hero media, sticky flags, series membership, related overrides, co-authors) in `blog_post_meta` and the taxonomy around it. A second posts table with its own revision store is explicitly rejected.
- **Taxonomy.** Hierarchical categories per site (drag order, nesting to three levels, description, per-archives SEO overrides, posts count) and flat tags, both slug-unique; a post gets one primary category plus any number of categories and tags; category and tag merges move associations instead of orphaning posts.
- **Authors.** Author profiles per site, either linked to a panel user (byline follows the profile) or a guest author with no account; display name, slug, bio, avatar from the media library, up to five social links, active toggle; one primary author per post plus optional co-authors with ordering. Author archive pages paginate like any other archive.
- **Listing and templates.** The theme declares which blog page types and listing variants it ships (`blog-index`, `blog-archive`, `blog-post`; variants `grid`, `list`, `featured-first`) in its manifest; per site the settings pick the default variant, page size, date grouping and the pagination mode `pagination` / `load_more` / `infinite` (infinite and load-more pages are server-rendered links underneath, so crawlers and no-JS visitors still get every page). A declared variant the active theme does not ship falls back to the theme's default listing with a settings warning, never a broken page.
- **Archives and discovery.** Category, tag, author and series archives with their own SEO overrides; related posts computed from tag and category overlap with recency decay (deterministic order, count from settings, optional manual override list per post); featured-first listings honour a `featured_rank` so editors can pin the top slot without making posts sticky.
- **Feeds and sitemap.** RSS 2.0 and Atom per site with per-site paths, item count, excerpt or full-text mode, plus optional per-category, per-tag and per-author feeds; feeds are cached with `ETag` and `Last-Modified`, emit absolute URLs, and are language-aware (`/tr/blog/feed.xml`). Posts, archives and author pages register into REQ-064's sitemap generator with real `lastmod` values.
- **Editorial.** Scheduled publish and unpublish via the existing queue, shared revision history and diff (REQ-077/REQ-111), per-post comments toggle over the site default (REQ-064 comments), sticky posts with an optional expiry and a configured sticky cap, draft preview through REQ-018 tokens, and publication gated by `content.pages.publish`.
- **Multi-site and multilingual.** Posts are per-site; a post and its translations share a `post_group_id` so alternates link as one article tree, localized slugs come from the existing `translations` rows (`field = 'slug'`), listings and feeds resolve per requested locale, and a translation without its own revision falls back to the default locale with a visible marker in the panel.
- **Search.** Registers the `posts` provider into the REQ-002 registry (title, subtitle, excerpt, body text, taxonomy names, author names) with the same reindex and status surfaces.

**Out**

- Comment moderation, newsletter, memberships, redirects, sitemap generation, robots.txt and the per-page SEO fields → REQ-064, which this module reuses rather than reimplements.
- Page building blocks and patterns → REQ-063; theme packaging and the theme manifest itself → REQ-084; marketplace distribution of blog themes → REQ-048.
- Sending a "new post" newsletter → REQ-060 (the blog emits an event and marketing subscribes); social auto-posting → REQ-015 integrations.
- WordPress import tooling beyond a mapping profile → REQ-031; podcast/video-first types, paywall tiers and print/PDF issue generation (REQ-029 can render a post but there is no issue model).

### Screens (UI)

| Route | Screen |
|---|---|
| `/blog` | Overview: published this month, drafts, scheduled, comments awaiting moderation, top categories, feed health |
| `/blog/posts` | Post list with filters, bulk actions and saved views |
| `/blog/posts/new` · `/blog/posts/{id}` | Post editor (shared content editor plus blog tabs) |
| `/blog/categories` | Category tree with drag order and per-archive SEO |
| `/blog/tags` | Tag table with merge and delete |
| `/blog/authors` · `/blog/authors/{id}` | Author list · author editor |
| `/blog/series` · `/blog/series/{id}` | Series list · series editor with post ordering |
| `/blog/settings` | Per-site listing, pagination, feed, comments and related-post settings |

- **Post list.** Columns: Title (hero thumbnail + link, sticky pin icon), Author, Primary category, Tags, Status (`draft`, `scheduled`, `published`, `archived`), Publish at (site timezone), Reading time, Comments (count), Updated. Filters: status, author, category, tag, language, date range, sticky only, text search (title and body). Bulk: publish, unpublish, archive, add/remove category, add/remove tag, make sticky / clear sticky, export CSV. Row actions: preview, duplicate, copy editor link, delete. Empty state offers `New post` and, when the list is filtered to zero, `Clear filters`.
- **Post editor.** Content tab is the shared editor (title, slug, blocks when REQ-063 is installed, status). Blog tabs: **Blog** (subtitle ≤ 160, excerpt ≤ 320 with a `Generate from first paragraph` action, hero image with alt and focal point from the media library), **Taxonomy** (primary category required, additional categories, tags with create-inline), **Authors** (primary author required, co-authors drag-ordered), **Series** (member series + position), **Related** (automatic with a preview list, or a manual override picker), **SEO** (the REQ-064 tab), **Comments** (toggle plus a link to the moderation inbox), **Revisions**, **Translations** (REQ-020), **Preview**. Validation: title required ≤ 200, slug unique per site and matching `^[a-z0-9]([a-z0-9-]{0,94}[a-z0-9])?$`, subtitle and excerpt length limits, hero alt required when a hero image and `require_alt` is on; errors render under the field and focus moves to the first invalid one. Publication without a category is refused with a message naming the field.
- **Categories.** Tree table: Name, Slug, Parent, Posts, SEO (badge when the archive has overrides), Updated. Drag to reorder and to re-parent (depth capped at three, cycles refused); inline rename; delete offers `Reassign posts to …` (target required) or refuses when the category has posts and no target is chosen.
- **Tags.** Columns: Tag, Slug, Posts, Updated. `Merge` picks a survivor and moves every association, reporting how many posts moved; delete removes associations after a confirmation naming the count.
- **Authors.** Columns: Name, Account (`Linked` or `Guest`), Slug, Posts, Active, Updated. Editor: display name, slug, bio ≤ 600 with counter, avatar (media picker, square crop preview), social links (≤ 5, each validated as a URL), byline text, active toggle, and the archive settings (title and description overrides).
- **Series.** List: Name, Slug, Posts, Published, Updated. Editor: name, slug, description, and a post picker with drag ordering; a post can belong to one series (moving it out is explicit) and series pages show ordered parts with next/previous links.
- **Settings.** Listing variant (grid / list / featured-first with a live thumbnail of each), page size 1–60 (default 12), pagination mode, date grouping, excerpt source (`excerpt` or first paragraph), reading speed words-per-minute (default 200, applied on publish), related posts toggle and count 0–6, sticky cap 0–10, comments default for new posts, feed toggles with paths and item count plus full-text mode, and a feed health line showing the last generated time, item count and HTTP status of each feed path.
- **States and mobile.** Skeletons on every table; empty states name the next action (no posts → `New post`, no authors → `Add author`); a failed feed generation surfaces an error banner with the reason; the editor autosaves drafts with a visible saved indicator and an unsaved-changes guard. On mobile (<768 px) tables become cards (title, author, status, publish date), the taxonomy pickers open as sheets, and drag ordering gains keyboard alternatives (move up/down actions) since drag is unreliable on touch.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET · POST | `/api/v1/blog/posts` | Post list (filters, saved view, cursor) · create a draft post | `blog.posts.read` · `blog.posts.manage` |
| GET · PATCH · DELETE | `/api/v1/blog/posts/{id}` | Post with blog metadata · update · archive | `blog.posts.read` · `blog.posts.manage` |
| POST | `/api/v1/blog/posts/{id}/publish` · `/unpublish` | Publication through the content path | `content.pages.publish` |
| POST · DELETE | `/api/v1/blog/posts/{id}/schedule` | Schedule publish/unpublish · cancel the entry | `content.pages.schedule` |
| POST | `/api/v1/blog/posts/{id}/sticky` | Set or clear sticky with an optional expiry | `blog.posts.manage` |
| PUT | `/api/v1/blog/posts/{id}/related` | Manual related-post override list | `blog.posts.manage` |
| GET · POST | `/api/v1/blog/categories` | Category tree · create | `blog.taxonomy.read` · `blog.taxonomy.manage` |
| PATCH · DELETE | `/api/v1/blog/categories/{id}` | Rename, re-parent, reorder, delete or reassign | `blog.taxonomy.manage` |
| GET · POST | `/api/v1/blog/tags` | Tag list · create | `blog.taxonomy.read` · `blog.taxonomy.manage` |
| PATCH · DELETE · POST | `/api/v1/blog/tags/{id}` (+ `/merge`) | Rename, delete, merge two tags | `blog.taxonomy.manage` |
| GET · POST | `/api/v1/blog/authors` | Author list · create (user-linked or guest) | `blog.authors.read` · `blog.authors.manage` |
| PATCH · DELETE | `/api/v1/blog/authors/{id}` | Update or deactivate an author | `blog.authors.manage` |
| GET · POST | `/api/v1/blog/series` | Series list · create | `blog.posts.read` · `blog.posts.manage` |
| PATCH · DELETE · PUT | `/api/v1/blog/series/{id}` (+ `/order`) | Update, delete, reorder members | `blog.posts.manage` |
| GET · PUT | `/api/v1/sites/{site_id}/blog/settings` | Listing, pagination, feeds, comments, related | `blog.settings.manage` |
| GET | `/api/v1/public/blog/posts` | Public listing (taxonomy, author, series, paging) | public, site-scoped |
| GET | `/api/v1/public/blog/posts/{slug}` | One post with taxonomy, authors, related | public, site-scoped |
| GET | `/api/v1/public/blog/terms/{kind}/{slug}` · `/authors/{slug}` | Archive payloads | public, site-scoped |
| GET | `/api/v1/public/blog/feed` | Generated RSS or Atom document (`format`, `scope`, `slug`) | public, cached |

Publishing, scheduling and revision endpoints are the existing page routes — the blog routes wrap them so one permission model stays in charge. Public routes resolve the site exactly like `/public/pages`, are cached with the post's `updated_at` as the validator key, and answer `404` (never `403`) for a draft post requested by slug.

### Data model

Migration `0120_blog.sql` — additive, commented in the `0009` style; seeds one `blog_settings` row per existing site, one `Uncategorized` category and nothing else.

```sql
blog_post_meta (page_id uuid pk -> pages on delete cascade, organization_id uuid not null, site_id uuid not null,
  subtitle text, excerpt text, reading_minutes int not null default 1, hero_media_id uuid null -> media,
  hero_alt text, sticky bool not null default false, sticky_until timestamptz, featured_rank int,
  series_id uuid null -> blog_series on delete set null, series_position int,
  primary_author_id uuid null -> blog_authors on delete set null,
  post_group_id uuid not null, related_override uuid[] not null default '{}', comments_enabled bool null,
  created_at/updated_at)
blog_post_authors (page_id uuid -> pages on delete cascade, author_id uuid -> blog_authors on delete cascade,
  position int not null default 0, primary key (page_id, author_id))
blog_post_categories (page_id uuid -> pages on delete cascade, category_id uuid -> blog_categories on delete cascade,
  is_primary bool not null default false, primary key (page_id, category_id))
blog_post_tags (page_id uuid -> pages on delete cascade, tag_id uuid -> blog_tags on delete cascade,
  primary key (page_id, tag_id))
blog_categories (id uuid pk, organization_id uuid not null, site_id uuid not null, parent_id uuid null -> blog_categories,
  name text not null, slug text not null, description text not null default '', position int not null default 0,
  seo jsonb not null default '{}', created_at/updated_at)  unique (site_id, slug)
blog_tags (id uuid pk, organization_id uuid not null, site_id uuid not null, name text not null, slug text not null,
  description text not null default '', seo jsonb not null default '{}', created_at/updated_at)  unique (site_id, slug)
blog_authors (id uuid pk, organization_id uuid not null, site_id uuid not null, user_id uuid null -> users on delete set null,
  name text not null, slug text not null, bio text not null default '', avatar_media_id uuid null -> media,
  links jsonb not null default '[]', byline text, is_guest bool not null generated always as (user_id is null) stored,
  active bool not null default true, created_at/updated_at)  unique (site_id, slug)
blog_series (id uuid pk, organization_id uuid not null, site_id uuid not null, name text not null, slug text not null,
  description text not null default '', created_at/updated_at)  unique (site_id, slug)
blog_series_posts (series_id uuid -> blog_series on delete cascade, page_id uuid -> pages on delete cascade,
  position int not null default 0, primary key (series_id, page_id))
blog_settings (site_id uuid pk -> sites on delete cascade, listing_variant text not null default 'grid'
  check (listing_variant in ('grid','list','featured_first')), page_size int not null default 12
  check (page_size between 1 and 60), pagination text not null default 'pagination'
  check (pagination in ('pagination','load_more','infinite')), date_grouping bool not null default false,
  excerpt_source text not null default 'excerpt' check (excerpt_source in ('excerpt','first_paragraph')),
  reading_speed_wpm int not null default 200 check (reading_speed_wpm between 80 and 400),
  related_enabled bool not null default true, related_count int not null default 3 check (related_count between 0 and 6),
  sticky_max int not null default 3 check (sticky_max between 0 and 10), comments_default bool not null default true,
  feed_rss bool not null default true, feed_atom bool not null default true, feed_path_rss text not null default 'blog/feed.xml',
  feed_path_atom text not null default 'blog/atom.xml', feed_items int not null default 20
  check (feed_items between 1 and 100), feed_full_text bool not null default false, require_hero_alt bool not null default true,
  updated_by uuid -> users, updated_at timestamptz not null default now())
-- additive index so post listings never table-scan pages
create index pages_site_type_status_idx on pages (site_id, page_type, status);
```

Checks: slug format identical to `pages.slug`; `reading_minutes >= 1` and recomputed on publish; `series_position > 0`; `sticky_until > now()` enforced in the service; a sticky post without an expiry counts against `sticky_max`, and exceeding the cap is refused with a message naming the current sticky posts. `post_group_id` is generated as the post's own page id for a new post and copied to a translation when it is created, so the whole language tree is one group. Indexes: `blog_post_meta_site_group_idx (site_id, post_group_id)`, `blog_post_meta_series_idx (series_id, series_position)`, partial `blog_post_meta_sticky_idx (site_id) where sticky`, `blog_post_categories_category_idx (category_id)`, `blog_post_tags_tag_idx (tag_id)`, `blog_categories (site_id, parent_id, position)`, `blog_authors (site_id, active)`. Category slugs and tag slugs share one namespace per site (a category and a tag cannot both own `/blog/category/x` and `/blog/tag/x` collisions are prevented by prefix), and archive paths are reserved against page slugs of the same site.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `blog.post.created` · `.updated` | Post created or a blog tab saved | `post_id`, `site_id`, `post_group_id`, `changed_fields` |
| `blog.post.published` · `.unpublished` | Publication transitions | `post_id`, `slug`, `revision_no`, `primary_category_id`, `tags` |
| `blog.post.scheduled` | A publish entry lands in the queue | `post_id`, `scheduled_at`, `timezone` |
| `blog.post.sticky_changed` | Sticky set, cleared or expired | `post_id`, `sticky`, `sticky_until` |
| `blog.taxonomy.updated` | Category, tag or series change (incl. merges) | `kind`, `id`, `action`, `posts_moved` |
| `blog.author.updated` | Author profile created, edited or deactivated | `author_id`, `site_id`, `active` |
| `blog.settings.updated` | Per-site blog settings saved | `site_id`, `changed_keys` |
| `blog.feed.regenerated` | A feed document was rebuilt | `site_id`, `format`, `scope`, `items`, `path` |

Consumed: `content.page.published` (syncs reading time, excerpt fallback and feed invalidation for posts), `content.page.unpublished` (drops posts from feeds), `content.translation.published` (alternates and locale feeds), `media.deleted` (hero image degrades with a settings warning), `content.comment.created` (comment counters and the overview badge), `localization.language.enabled` / `.disabled` (locale feed paths). Webhook relevance: `blog.post.published` drives newsletter issues in REQ-060, social integrations in REQ-015 and cache purges; payloads carry ids, slugs and taxonomy ids — never body text, and never an author's e-mail.

### Acceptance criteria

- [ ] `cargo test -p omnion-module-blog` is green, covering slug uniqueness, sticky cap enforcement, reading-time computation, related-post ordering and feed rendering.
- [ ] `0120_blog.sql` applies on a fresh and on a populated database, seeds one settings row per site and touches no existing rows.
- [ ] A post created through the editor appears with `page_type = 'post'`, gets `blog_post_meta`, and its revision appears in the shared revision list on the existing page route.
- [ ] Creating a post without a primary category or with a duplicate slug is refused with a field-level message, and the first invalid field is focused.
- [ ] Category nesting caps at three levels, a cycle is refused, drag order persists after reload, and deleting a category with posts goes through the reassignment flow naming the post count.
- [ ] Merging two tags moves every association, reports the number moved, and the loser tag disappears from the tag list and from every post.
- [ ] A guest author with no panel account renders a byline and an archive page; deactivating an author keeps published posts intact and their byline unchanged.
- [ ] The listing honours the configured variant and page size: grid, list and featured-first render differently from the same data, featured-first pins `featured_rank` first, and date grouping groups by month.
- [ ] Pagination works in all three modes: `pagination` links to `/blog/page/2`, `load_more` appends the second page in place, and `infinite` appends on scroll while a no-JS request to `/blog/page/2` still returns the same items.
- [ ] A post with two co-authors and one primary author shows the primary byline on listings and all names on the post page.
- [ ] Related posts never include unpublished, archived or same-locale-missing items; a manual override list replaces the computed list and survives a reindex.
- [ ] RSS and Atom feeds validate as well-formed XML, contain absolute URLs and the configured item count, answer `304` when nothing changed, and full-text mode includes the rendered body.
- [ ] A post with a Turkish translation resolves `/tr/blog/{localized-slug}`, appears in the Turkish feed, and its alternates reference the English original.
- [ ] Sticky posts: the cap is enforced with a readable message, an expired sticky clears itself, and the sticky pin shows in the list and is usable as a filter.
- [ ] Publication events land on a subscribed webhook: `blog.post.published` after publish, `blog.post.scheduled` after scheduling, and neither fires twice for one transition.
- [ ] The `posts` provider appears in `/settings/search` with a document count, and `POST /api/v1/search/reindex` with `{"provider":"posts"}` completes and returns fresh hits.
- [ ] The sitemap generated by REQ-064 includes posts, archives, series and authors with real `lastmod` values, and excludes drafts.
- [ ] All seven admin screens have empty, loading and error states with zero high findings, and the post list plus editor work at 390 px with sheets for pickers.

### QA plan

The walkthrough extends `scripts/qa/walkthrough.cjs` with `/blog` (overview cards), `/blog/posts` (filter by status, make one post sticky, bulk-add a tag), `/blog/posts/{id}` (edit subtitle, excerpt, hero with alt, primary category and two tags, then save and check autosave indicator), `/blog/categories` (create a child, drag order, attempt a cycle and see the refusal, delete with reassignment), `/blog/tags` (merge two tags and see the moved count), `/blog/authors` (create a guest author, assign as primary, open the archive), `/blog/series` (create a series with three posts and reorder) and `/blog/settings` (switch the listing variant and the pagination mode, toggle a feed and see the health line update). The renderer walkthrough covers `/blog`, `/blog/page/2`, a post, a category archive, a tag archive, an author archive, `/blog/feed.xml` and the Turkish locale variants. The visual check must see: a post list with hero thumbnails and a sticky pin, visually distinct status badges, a category tree with drag handles, an author card with a real avatar, differing listing variants rendered from the same data, and a feed document shown as XML — never a blank frame, a raw key or a placeholder image. Screenshots: `page-blog-posts`, `page-blog-post-editor`, `page-blog-categories`, `page-blog-authors`, `site-blog-index`, `site-blog-post`.

### Slices

1. **Posts, metadata and taxonomy.** Migration `0120`, `page_type = 'post'` wiring with the shared editor, `blog_post_meta` (subtitle, excerpt, hero, reading time, sticky, featured rank), categories with nesting and reassignment, tags with merge, the post list and the taxonomy screens, permissions and the `blog.post.*` events. *Done when:* acceptance 1–6 and 14–15 pass and a post round-trips from the editor to the list with its taxonomy intact.
2. **Authors, series, listings and discovery.** Author profiles and archives, series with ordering, related posts with overrides, the theme contract for `blog-index`/`blog-archive`/`blog-post` with the three listing variants and the three pagination modes, the renderer routes, RSS/Atom generation with caching and the sitemap registration. *Done when:* acceptance 7–11 and 17–18 pass and the public `/blog` and a post render through a theme on desktop and mobile.
3. **Editorial depth, multilingual and search.** Scheduled publishing on the queue, revisions and diff links, the comments toggle with its site default, draft preview handoff, the translation group with localized slugs, alternates and locale feeds, the `posts` search provider and the settings screen with feed health. *Done when:* acceptance 12–13 and 16 pass, and a scheduled post publishes on time and appears in the Turkish feed and sitemap.

### Risks / notes

- **One content engine.** Posts must stay `pages` rows; the moment the module keeps its own revisions or translations the platform has two CMSs and every later feature (workflows, approvals, diff, preview) has to be built twice.
- **Archive URL space.** `/blog/category/{slug}` and `/blog/tag/{slug}` share the site's URL namespace with pages and posts; slugs are validated against the reserved prefixes at write time, and a collision is refused rather than silently shadowed.
- **Infinite scroll must not hide content.** Every mode renders a real second page server-side so search engines and no-JS users see the same items; this is verified in QA, not assumed.
- **Sticky is editorial tooling, not a ranking trick.** The cap exists so a site cannot pin everything; expiry is supported and the module never claims a search-engine benefit for stickiness.
- **Feed correctness and cost.** Feeds are cached documents regenerated on publish and on demand, with `ETag` and `Last-Modified`; a full-text feed of a large site is generated from stored revisions, not by walking the renderer, and content is XML-escaped.
- **Related posts are bounded work.** The query is capped (candidate set from the shared taxonomy, deterministic tie-breaks) and cached per post, invalidated by publish and taxonomy changes.
- **Translations are article-level.** One post tree per group; a locale without its own revision falls back with a visible marker, and the panel never pretends a fallback is a translation.
- **Delete paths are explicit.** Deleting a category or tag with posts is a reassignment flow with a named count; deleting a post archives it (soft) and never cascades a hard delete from the taxonomy side.
- **Theme contract.** The blog relies on the theme manifest declaring its page types (docs/03-FRONTEND.md, `themes/README.md`); a theme missing a variant gets the documented fallback plus a settings warning, and the theme SDK ships one reference blog layout so the feature is demonstrable on a fresh installation.
