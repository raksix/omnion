# REQ-023 — Marketplace Expansion

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** `apps/marketplace`
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

Grow the Marketplace into a full ecosystem surface:

```text
Plugins
Themes
Modules
Integrations
Templates
AI Agents
Blocks
Widgets
```

License types:

```text
Free
Open Source
Paid
Subscription
Enterprise
```

## Notes

- Extends the marketplace vision in docs/03-FRONTEND.md.

## Implementation spec

### Scope (in / out)

**In**

- New workspace app `apps/marketplace` (Next.js App Router, same conventions as `apps/admin` and `apps/web`) as the **catalogue surface**: browse, search, item detail, publisher pages, reviews, and the install intent that hands off to the package engine.
- Eight item types on one item model: `plugin`, `theme`, `module`, `integration`, `template`, `ai_agent`, `block`, `widget`. Type changes only the detail-page extras and compatibility block, never the table shape.
- Five licence types: `free`, `open_source`, `paid`, `subscription`, `enterprise`, each with its own CTA (`Get` / `Get` / `Buy` / `Subscribe` / `Contact sales`) and licence panel. Money movement and entitlement checks are out of scope — the CTA hands off to REQ-048.
- Publisher workflow: register a handle, create a listing draft, upload version records (semver, changelog, compatibility range, artifact checksum), submit for review, respond to changes requested, publish a version, unpublish.
- Moderation in `apps/admin`: review queue (claim → approve / request changes / reject with a reason), abuse-report queue, suspend/unpublish on a listing or publisher.
- Reviews: 1–5 stars plus text, one per user per item, edit/delete own, report a review, "verified install" badge where an install record exists.
- Real counters: install count and per-version downloads fed by events; rating average computed from visible reviews only.
- Compatibility gate: each version declares a core version range; an item excluding the running core version shows "not compatible with this instance" and a disabled install button with a reason.

**Out**

- Install/uninstall/unpack execution (REQ-044), payment processing, invoices, subscription billing, licence-key issuance (REQ-008/REQ-048).
- Theme rendering (REQ-062), plugin runtime isolation (REQ-017), AI agent execution (REQ-001), block/page rendering (REQ-063).
- A public SaaS marketplace with third-party payouts.

### Screens (UI)

| Route | Purpose |
|---|---|
| `/` | Home: editorial hero, featured row, per-type rows, popular, recently updated |
| `/browse`, `/browse/{type}` | Full catalogue with facets and sort, scoped per type |
| `/items/{slug}` | Detail: icon, tagline, gallery, README/changelog/licence/reviews tabs, sidebar, similar items |
| `/items/{slug}/versions` | Version history with changelog and compatibility per version |
| `/publishers/{handle}` | Publisher profile: bio, verified badge, their items |
| `/publish`, `/publish/{id}`, `/publish/{id}/versions` | Publisher onboarding, listing editor, version upload |
| `/admin/queue`, `/admin/reports` | Moderator queue and abuse reports (admin app) |

- Catalogue card: icon · name · publisher · type badge · licence badge · rating · install count. Grid 4 columns ≥ 1280px, 3 ≥ 1024px, 2 ≥ 768px, 1 below; table toggle for `/browse`.
- Table columns: Name · Type · Licence · Rating · Installs · Updated · Publisher.
- Facets: type, licence, category, core compatibility, rating (≥ 3 / 4 / 4.5), updated window, verified publisher. Active filters render as removable chips; all facet state lives in the query string so a view is shareable and back-navigable. Sort: relevance, installs, rating, updated, newest, name.
- Detail header: icon, name, publisher, type, licence badge, install/download CTA, version selector. Install opens a confirm dialog naming version, licence and compatibility; on confirm it hands off to the package engine and shows progress with a link to the install record. `Paid`/`Subscription` open the purchase handoff; `Enterprise` opens a contact form whose submission is **stored**, not mailed.
- Review form: keyboard-accessible star selector (arrow keys), text 10–2000 chars, verified-install badge, inline errors, submit disabled while sending.
- Listing form: name (3–60) · slug `^[a-z0-9-]{3,60}$` unique per publisher · type · tagline (≤ 120) · description markdown (≤ 20 000) · licence · price + currency when paid/subscription · exactly one category · tags (≤ 10) · homepage/repo URL (absolute) · icon 512×512 · gallery (≤ 8, ≤ 5 MB each) · compatibility range.
- Version form: strict semver, changelog markdown, core min/max, artifact upload or source reference, SHA-256 computed and shown read-only, yank toggle with reason.
- States: gallery skeleton, empty facet result with clear-filters action, item-not-found page, publisher with no items, empty queue, submit-locked states, error state with request id.
- Keyboard: `/` focus search, `g` `b` to browse, `g` `p` to publish, `Enter` submit, `Esc` close, arrow keys in the star selector.
- Mobile: facet rail becomes a bottom sheet with an Apply bar, detail tabs scroll horizontally, sticky install bar above the safe area, single-column forms.

### API

Public read routes are unauthenticated and rate-limited; publisher routes act on the caller's own account; moderator routes are admin-guarded.

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/marketplace/items` | Catalogue query (`q`, type, licence, category, tag, rating, sort, cursor) | public (rate-limited) |
| GET | `/api/v1/marketplace/items/{slug}` | Item detail with latest published version | public (rate-limited) |
| GET | `/api/v1/marketplace/items/{slug}/versions` | Published versions + changelogs | public (rate-limited) |
| GET | `/api/v1/marketplace/items/{slug}/reviews` | Visible reviews, paged | public (rate-limited) |
| GET | `/api/v1/marketplace/facets` | Facet counts for the current query | public (rate-limited) |
| GET | `/api/v1/marketplace/publishers/{handle}` | Publisher profile + their items | public (rate-limited) |
| GET | `/api/v1/marketplace/featured` | Editorial rows for the home page | `marketplace.read` |
| POST | `/api/v1/marketplace/items/{slug}/install` | Record intent, hand off to the package engine | `marketplace.install` |
| POST | `/api/v1/marketplace/items/{slug}/reviews` | Create or update own review | `marketplace.review` |
| DELETE | `/api/v1/marketplace/reviews/{id}` | Delete own review | `marketplace.review` |
| POST | `/api/v1/marketplace/reviews/{id}/report` | Report a review | `marketplace.review` |
| POST | `/api/v1/marketplace/publishers` | Apply for a publisher handle | `marketplace.publish` |
| GET | `/api/v1/marketplace/publisher/items` | Own listings, all statuses | `marketplace.publish` |
| POST | `/api/v1/marketplace/publisher/items` | Create a listing draft | `marketplace.publish` |
| PATCH | `/api/v1/marketplace/publisher/items/{id}` | Edit draft metadata and media | `marketplace.publish` |
| POST | `/api/v1/marketplace/publisher/items/{id}/versions` | Add a version record | `marketplace.publish` |
| POST | `/api/v1/marketplace/publisher/items/{id}/submit` | Submit for review | `marketplace.publish` |
| POST | `/api/v1/marketplace/publisher/items/{id}/unpublish` | Unpublish own listing | `marketplace.publish` |
| GET | `/api/v1/marketplace/moderation/queue` | Review queue (claimed / unclaimed) | `marketplace.moderate` |
| POST | `/api/v1/marketplace/moderation/queue/{id}/decision` | Approve / request changes / reject with reason | `marketplace.moderate` |
| GET | `/api/v1/marketplace/moderation/reports` | Abuse reports | `marketplace.moderate` |
| POST | `/api/v1/marketplace/moderation/reports/{id}/resolve` | Resolve or dismiss a report | `marketplace.moderate` |
| POST | `/api/v1/marketplace/moderation/items/{id}/suspend` | Suspend or restore a listing | `marketplace.moderate` |

Errors: `400` invalid slug/semver/checksum, `403` permission miss or a non-owner publisher write, `404` unknown or unpublished item seen by a stranger, `409` duplicate slug or duplicate review by one user, `422` a listing submitted with no compatible published version, `429` public rate limit.

### Data model

Migration: `database/migrations/0013_marketplace_catalogue.sql` (next free number at build time).

- `marketplace_publishers` — `id uuid pk`, `handle text not null unique`, `display_name text not null`, `bio text not null default ''`, `avatar_media_id uuid null references media(id) on delete set null`, `website text null`, `verified boolean not null default false`, `status text not null default 'active'` (`active|suspended`), `owner_user_id uuid not null references users(id) on delete cascade`, `created_at timestamptz not null default now()`, `updated_at timestamptz`; check `handle ~ '^[a-z0-9][a-z0-9-]{1,38}[a-z0-9]$'`.
- `marketplace_items` — `id uuid pk`, `publisher_id uuid not null references marketplace_publishers(id) on delete cascade`, `type text not null`, `slug text not null`, `name text not null`, `tagline text not null default ''`, `description_md text not null default ''`, `licence text not null`, `price_cents int null`, `currency text null`, `category_id uuid null references marketplace_categories(id) on delete set null`, `tags text[] not null default '{}'`, `homepage_url text null`, `repo_url text null`, `icon_media_id uuid null`, `status text not null default 'draft'`, `latest_version_id uuid null`, `install_count bigint not null default 0`, `rating_avg numeric(3,2) null`, `rating_count int not null default 0`, `featured_rank int null`, `rejection_reason text null`, `created_at timestamptz not null default now()`, `published_at timestamptz null`, `updated_at timestamptz`; checks `type in (plugin,theme,module,integration,template,ai_agent,block,widget)`, `licence in (free,open_source,paid,subscription,enterprise)`, `status in (draft,in_review,published,changes_requested,suspended,archived)`, price only for paid/subscription; indexes unique `(publisher_id, slug)`, `(status, type, install_count desc)`, `(status, rating_avg desc)`, `(status, featured_rank)`, GIN on `tags`.
- `marketplace_categories` — `id uuid pk`, `slug text not null unique`, `name text not null`, `position int not null default 0`, `parent_id uuid null references marketplace_categories(id)`.
- `marketplace_item_media` — `item_id uuid not null`, `media_id uuid not null`, `kind text not null` (`screenshot|logo`), `position int not null default 0`; primary key `(item_id, media_id, kind)`.
- `marketplace_versions` — `id uuid pk`, `item_id uuid not null references marketplace_items(id) on delete cascade`, `version text not null`, `changelog_md text not null default ''`, `core_min text null`, `core_max text null`, `artifact_media_id uuid null`, `artifact_url text null`, `checksum_sha256 text null`, `size_bytes bigint null`, `status text not null default 'draft'` (`draft|published|yanked`), `yank_reason text null`, `published_at timestamptz null`, `created_by uuid null`, `created_at timestamptz not null default now()`; unique `(item_id, version)`, index `(status, published_at desc)`.
- `marketplace_reviews` — `id uuid pk`, `item_id uuid not null references marketplace_items(id) on delete cascade`, `user_id uuid not null references users(id) on delete cascade`, `version text null`, `rating smallint not null`, `body text not null default ''`, `verified_install boolean not null default false`, `status text not null default 'visible'` (`visible|hidden|removed`), `created_at timestamptz not null default now()`, `updated_at timestamptz`; `rating between 1 and 5`, `length(body) <= 2000`, unique `(item_id, user_id)`.
- `marketplace_reports` — `id uuid pk`, `subject_type text not null` (`item|review|publisher`), `subject_id uuid not null`, `reporter_user_id uuid null references users(id) on delete set null`, `reason text not null`, `detail text not null default ''`, `status text not null default 'open'` (`open|resolved|dismissed`), `resolved_by uuid null`, `resolved_at timestamptz null`, `created_at timestamptz`.
- `marketplace_installs` — `id uuid pk`, `item_id uuid not null`, `version_id uuid null`, `organization_id uuid not null references organizations(id) on delete cascade`, `site_id uuid null`, `user_id uuid null`, `source text not null default 'marketplace'`, `created_at timestamptz not null default now()`; index `(item_id, created_at desc)`. `install_count`, `rating_avg` and `rating_count` are maintained by the write paths, never by a client-supplied number.

### Events

- **Emitted:** `marketplace.item.submitted`, `marketplace.item.changes_requested`, `marketplace.item.published`, `marketplace.item.unpublished`, `marketplace.item.suspended`, `marketplace.version.published`, `marketplace.version.yanked`, `marketplace.review.created`, `marketplace.review.hidden`, `marketplace.install.recorded`.
- **Consumed:** `plugin.installed` / `plugin.uninstalled` / `theme.installed` / `theme.activated` from REQ-044 and REQ-062 reconcile install counts; `user.deleted` anonymises a reviewer instead of dropping the rating; an update-available signal from the update manager feeds the "update available" badge on installed items.
- Webhook relevance: publishers and operators subscribe to `marketplace.*`; payloads carry item/version ids, slug, type, licence, status — never a publisher's private draft body and never a signed artifact URL.
- Notification relevance: a moderation decision notifies the publisher; an abuse report notifies moderators (REQ-021 router).

### Acceptance criteria

- [ ] `apps/marketplace` exists in the workspace and boots in the monorepo dev flow.
- [ ] Home, `/browse` and all eight type routes render real published items.
- [ ] Facets filter the result set, are reflected in the URL, and clearing restores the full set.
- [ ] Search matches name, tagline and tags; an empty result shows the clear-filters empty state.
- [ ] Item detail renders icon, gallery, README markdown, changelog, licence panel and reviews.
- [ ] An incompatible item shows the notice and a disabled install button with a reason.
- [ ] Install hands off to the package engine and records a `marketplace_installs` row.
- [ ] Reviews support create, edit, delete own; one per user is enforced; the average updates.
- [ ] Reporting a review creates a row visible in the admin reports queue.
- [ ] Publisher onboarding creates a handle; a duplicate handle is rejected.
- [ ] A draft listing can be created, edited, versioned and submitted for review.
- [ ] Submitting without a compatible published version is rejected server side.
- [ ] Approving in the queue publishes the listing and it appears in the catalogue.
- [ ] Rejecting requires a reason and returns the listing to `changes_requested`.
- [ ] Suspending removes the listing from browse while its publisher still sees it with a badge.
- [ ] Yanking a version hides it from new installs but keeps it on existing install records.
- [ ] Paid/subscription items route to the purchase handoff; enterprise items store a contact request.
- [ ] A publisher cannot read another publisher's drafts (`403`); mobile facets/keyboard paths work.
- [ ] `pnpm typecheck`, `pnpm build`, `cargo test` and the browser walkthrough are green.

### QA plan

The walkthrough must: load the home page and assert featured and per-type rows have real cards; open `/browse`, apply two facets and a sort, remove a chip and assert the grid changes; open an item and switch README / Changelog / Licence / Reviews; submit a review and see the average change; sign in as a publisher, create a draft, add a version, submit; sign in as a moderator, approve it, open it from the catalogue, then suspend it and confirm it leaves browse while the publisher still sees it; resize to 390px and re-check the facets sheet and install bar.

Visual check should see: uniform card heights at the longest tagline, consistent licence and type badge colours, a legible rating widget at 1 and 5 stars, a README whose code blocks do not overflow, thumbnails without letterboxing, dark/light parity.

### Slices

1. **Catalogue read path.** Migration, seed for a handful of real in-repo items, public list/detail/versions/facets endpoints, `/`, `/browse`, `/browse/{type}`, `/items/{slug}`. *Done when:* the catalogue browses with working facets, sort, pagination and an honest empty state.
2. **Publisher + moderation loop.** Onboarding, drafts, version records with checksum, submit, admin queue with claim/decision, decision notifications, suspend/unpublish. *Done when:* a draft goes `draft → published` and appears in the catalogue with no manual database edit.
3. **Reviews + stats + install intent.** Reviews, verified-install badge, reports queue, rating rollup, install recording and package-engine handoff, compatibility gate. *Done when:* a review and an install both move the displayed counters.
4. **Licence + handoff + polish.** Licence panels and CTA variants, enterprise contact capture, per-version download stats, publisher profiles, mobile and keyboard passes. *Done when:* all five licence types render their own panel and no CTA is a dead button.

### Risks / notes

- Keep the boundary with REQ-048 sharp: this REQ owns the catalogue, publishing pipeline and licence presentation; REQ-048 owns purchasing, entitlements and automated installation. Re-read both before implementing either.
- Money is not handled here: price fields are presentation until REQ-008/REQ-048 land, and the UI must say so rather than implying a completed checkout.
- Install counts come from events, so a replay could inflate them — dedupe on `(item_id, organization_id, version_id, minute bucket)`.
- Publisher markdown is untrusted input: render through the same sanitiser the CMS uses, strip scripts and inline handlers, never allow raw HTML.
- The marketplace never executes artifacts. A version without a checksum is invisible to the installer, and only REQ-044's installer may unpack one.
