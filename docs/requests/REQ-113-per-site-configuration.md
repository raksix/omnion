# REQ-113 — Per-Site Configuration Surface

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** admin + core (`crates/sites`)
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Every site is its own product.

- Per-site settings screen: name, key, domains, locale set, timezone, theme, SEO defaults, media library scope.
- Per-site users/roles (who may edit this site), with scope-aware grants (REQ-070).
- Per-site menus, forms, redirects and scripts.
- Site switcher everywhere in the panel; "all sites" views where it makes sense.
- Clone a site's configuration to a new site.

## Implementation spec

### Scope (in / out)

**In**
- One settings home per site with all of the following in one place: identity (name, key, description, logo, favourite icon), domains (one primary plus additional hostnames with a verification state and a redirect-to-primary toggle), locale set (ordered list, one default, fallback chain), timezone (used by scheduling, calendar rendering and date formatting), theme assignment (key plus an override state), SEO defaults (title suffix, description template, default OG image, robots defaults, sitemap toggle), media library scope (shared library or site-only, with an allowlist of shared collections), and an advanced section (canonical host behaviour, indexability toggle, per-site maintenance banner).
- Per-site users and roles: a grants table that binds a panel user to a role within exactly one site, with the grant recorded by whom and when. A user may hold different roles in different sites; a grant never widens beyond the role's permission set, and site-scoped checks run in addition to org-level checks (REQ-070 vocabulary, one evaluation path).
- Per-site content surfaces: menus, forms, redirects and scripts. Menus, forms and redirects are the existing implementations with the site filter made explicit and mandatory; scripts are a small per-site table of head and body snippets with an enabled flag, an environment toggle (`all`, `production_only`), a position, and an admin-only permission.
- Site switcher in the panel chrome: current site, one-click switch, a search over sites, and an "All sites" entry. Switching changes the default scope of every list route that accepts a site scope and persists per user. Routes that cannot work across sites (the site settings screen itself, site users) disable the "All sites" option with a reason rather than silently falling back to one site.
- "All sites" views for the surfaces where a cross-site view is meaningful: content list, schedules, comments, forms submissions, redirects, media, and the editorial calendar. Each shows a Site column, allows filtering to one site, and never offers an action that would apply to two sites at once except in the surfaces whose bulk semantics are explicitly defined (for example bulk spam in comments).
- Clone: copy one site's configuration into a new site — settings, locale set, theme assignment, SEO defaults, media scope, menus, forms, redirects, scripts, workflow definitions, and optionally selected pages as drafts. Never copied: content publication state (everything copied starts unpublished), submissions, comments, schedules, analytics, users and grants, preview links, and secret references (a clone that would need a secret reports the re-binding steps).
- Domain handling: hostnames are unique across the installation, the primary host cannot be removed while the site is live, removing a non-primary host offers a redirect entry, and the verification state is recorded without this REQ claiming to provision certificates (that belongs to the deployment surface).
- Every settings change writes through the configuration version recorder (REQ-112) so a site settings change has the same history as any other versioned domain.

**Out**
- Organisation and installation settings (their own surfaces; this screen only links to them).
- Cross-site content sharing, content translation between sites (REQ-020), and moving a page from one site to another — a move is a copy plus unpublish, deferred to a later REQ.
- DNS, certificate provisioning and CDN wiring (deployment surface), beyond recording verification state.
- Per-site billing, plans and quotas.
- Secrets: a clone or a settings copy never carries credential material (REQ-125); only references with a re-binding note.

### Screens (UI)

| Route | Screen |
|---|---|
| `/sites` | Site list with the "all sites" cross-site summary |
| `/sites/new` | Create a site from blank or from an existing site |
| `/sites/<id>/settings` | Settings home with tabs |
| `/sites/<id>/users` | Site grants: users, roles, granted by, granted at |
| `/sites/<id>/clone` | Clone wizard with the copy inventory |
| Panel chrome (everywhere) | Site switcher with search and `All sites` |
| `/content` · `/editorial/calendar` · `/comments` · `/forms` · `/seo/redirects` | Cross-site views with a Site column |

- **Site list.** Columns: Name, Key, Primary domain, Sites users (count), Theme, Locales, Timezone, Content (published count), Status. Filters: status, theme, locale, text search. Row actions: Open settings, Switch to this site, Users, Clone, Archive. Archiving a site with published content is refused with the count; an archived site keeps its data, disappears from the switcher, and its hostnames return the documented not-found behaviour instead of redirecting to another site.
- **Settings home tabs.** `General` (name, key read-only after creation, description, logo and icon via the media picker, contact e-mail), `Domains` (table of hostnames: Host, Primary radio, Verified, Redirect to primary, Added by, with inline add and a verification check action; a host already used elsewhere is refused with the owning site named), `Locales` (ordered list with drag, one marked default, fallback chain picker, and a warning when a locale is enabled with no translations), `Theme` (assigned theme with a preview thumbnail, an override note when the theme was changed outside this screen), `SEO` (title suffix, description template with `{{title}}` help text, default OG image, robots defaults, sitemap toggle, a `Preview defaults` card showing how one real page would render), `Media` (shared versus site-only, shared collection allowlist), `Users` (inline summary with a link), `Advanced` (canonical host behaviour, indexability, maintenance banner with a message field and a preview that renders it in the site theme).
- **Users tab.** Table: User, Role, Scope, Granted by, Granted at, Last active. Add grant dialog searches panel users, requires a role, and shows the role's permission summary before saving. Removing a grant asks for confirmation and states that the user keeps their other sites. A user's own last-owner grant cannot be removed if it would leave the site with no grant holder.
- **Clone wizard.** Step 1 target identity (name, key, primary domain, optionally a blank host). Step 2 copy inventory: every item with a checkbox and a note (settings, locales, theme, SEO, media scope, menus, forms, redirects, scripts, workflow definitions, selected pages as drafts, with a page picker). Items that cannot be copied appear greyed with the reason (users and grants, submissions, comments, schedules, analytics, secrets). Step 3 confirmation summarising counts per item; running the clone shows progress and ends with a report naming every item copied or skipped, plus a `Switch to the new site` action.
- **Site switcher.** Keyboard-first: `s` opens it, typing filters, `Enter` switches, and the current site is always visible in the chrome. The switcher remembers the last site per user and never switches silently when the open screen is site-specific — it asks.
- **Cross-site views.** A Site column, a site filter chip row, and a persistent banner while in "All sites" mode reading "Actions apply to the filtered selection". Bulk actions in that mode name the affected site count in the confirmation.
- **States and keys.** An installation with one site hides the switcher and "All sites" until a second site exists. Keys: `s` switcher, `g s` sites, `n` new site, `j`/`k` rows, `Esc` closes dialogs. All tables degrade to cards at 390 px with the site filter pinned above the list.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET · POST | `/api/v1/sites` | List (status, theme, locale filters) · create a site | `sites.read` · `sites.create` |
| GET · PUT · DELETE | `/api/v1/sites/{id}` | Read · update identity and advanced settings · archive | `sites.read` · `sites.update` · `sites.delete` |
| GET · POST · DELETE | `/api/v1/sites/{id}/domains` | Hostnames with primary flag · add · remove | `sites.domains.manage` |
| POST | `/api/v1/sites/{id}/domains/{host}/verify` | Re-run the verification check and record the result | `sites.domains.manage` |
| GET · PUT | `/api/v1/sites/{id}/locales` | Ordered locale set, default and fallback chain | `sites.locales.manage` |
| GET · PUT | `/api/v1/sites/{id}/theme` | Assigned theme and override state | `sites.theme.manage` |
| GET · PUT | `/api/v1/sites/{id}/seo-defaults` | SEO defaults for the site | `sites.seo.manage` |
| GET · PUT | `/api/v1/sites/{id}/media-scope` | Shared versus site-only and shared collections | `sites.media.manage` |
| GET · POST · DELETE | `/api/v1/sites/{id}/grants` | Site users and roles · grant · revoke | `sites.users.manage` |
| GET · POST · PUT · DELETE | `/api/v1/sites/{id}/scripts` | Per-site snippets with enable flag and position | `sites.scripts.manage` |
| POST | `/api/v1/sites/{id}/clone` | Start a clone with the copy inventory | `sites.create` |
| GET | `/api/v1/sites/{id}/clone/{job_id}` | Clone progress, result and per-item report | `sites.read` |
| GET | `/api/v1/sites/switch-options` | Switcher payload: sites the caller can reach, last used, all-sites allowed | `sites.read` |

Every settings write accepts the reason field required by the recorder (REQ-112) and returns the new configuration version; the panel's settings screens never write around it. Site-scoped routes derive scope from the path segment and re-check the grant, so a user with a grant in one site does not read another site's settings through a shared route.

### Data model

Migration: `0119_site_configuration.sql` (next free number; append-only ledger — shift up if taken).

```sql
alter table sites add columns
  description text, contact_email text, timezone text not null default 'UTC', default_locale text not null default 'en',
  locales text[] not null default '{en}', fallback_locale text null, theme_key text null, theme_override_at timestamptz null,
  seo_defaults jsonb not null default '{}' /* title_suffix, description_template, og_media_id, robots, sitemap_enabled */,
  media_scope text not null default 'shared', shared_collection_ids uuid[] not null default '{}',
  canonical_host_behaviour text not null default 'redirect_primary', indexable bool not null default true,
  maintenance jsonb not null default '{}' /* enabled, message */, archived_at timestamptz null;
site_domains (id uuid pk, organization_id uuid, site_id uuid -> sites on delete cascade, host text, is_primary bool default false,
  verified_at timestamptz null, verification_token_hash text null, redirect_to_primary bool default true,
  created_by uuid null -> users, created_at/updated_at)
  unique (lower(host))  unique (site_id) where is_primary
site_grants (id uuid pk, organization_id uuid, site_id uuid -> sites on delete cascade, user_id uuid -> users,
  role_id uuid -> roles, granted_by uuid -> users, created_at/updated_at)  unique (site_id, user_id)
  index (user_id) /* switcher and scope checks scan this direction */
site_scripts (id uuid pk, site_id uuid on delete cascade, name text, position text in ('head','body_start','body_end'),
  content text not null, enabled bool default true, environment text in ('all','production_only') default 'all',
  created_by uuid null -> users, created_at/updated_at)  index (site_id, enabled)
site_clone_jobs (id uuid pk, organization_id uuid, source_site_id uuid -> sites, target_site_id uuid -> sites,
  inventory jsonb not null /* per item: requested, copied, skipped + reason */, status text in ('running','done','failed') default 'running',
  report jsonb default '{}', created_by uuid -> users, started_at, finished_at timestamptz null)
  index (target_site_id, started_at desc)
```

Notes. `lower(host)` uniqueness is installation-wide, so a hostname cannot point at two sites; the primary-domain uniqueness is a partial unique index per site. `site_grants` is the only place a site-scoped role binding lives; it references the existing `roles` table and never copies permission rows. Locale handling uses an array plus a default and a fallback rather than a join table because the set is small and ordered; the ordered edit is one array write. Scripts store raw snippet text by necessity; they are sanitised on render boundaries, only holders of `sites.scripts.manage` can write them, every write is versioned, and the renderer injects them in the documented positions only. The clone job reads from the source, writes new rows pointing at the target site, and copies menus, forms, redirects, workflow definitions and pages as unpublished drafts; it copies the source site's settings through the recorder so the new site has an honest v1.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `sites.site.created` · `.updated` · `.archived` | Site lifecycle, including every settings save | `site_id`, `key`, `changed_keys`, `actor_user_id` |
| `sites.domain.added` · `.removed` · `.verified` | Hostname management | `site_id`, `host`, `is_primary`, `verified` |
| `sites.grant.created` · `.revoked` | Site user grants | `site_id`, `user_id`, `role_id`, `actor_user_id` |
| `sites.clone.started` · `.completed` · `.failed` | Clone lifecycle | `job_id`, `source_site_id`, `target_site_id`, `item_counts` |
| `sites.settings.changed` | A settings save that changes routing-relevant values | `site_id`, `changed_keys`, `version_id` |

Consumed: `config.version.recorded` (keeps the settings screens' History tab honest), `themes.activated` (updates the assigned theme thumbnail and override state when a theme changes identity), `identity.user.deactivated` (flags grants held by the user and lists the sites that lose their last grant holder), `content.page.published` (keeps the site list's published count fresh). Webhook relevance: `sites.domain.added` and `sites.settings.changed` matter to external proxies and search integrations; payloads carry hosts, keys and version ids, never grant details of other users.

### Acceptance criteria

- [ ] Creating a site with name, key, primary domain, timezone and locale set makes it appear in `/sites`, in the switcher, and in the site filter of every cross-site list within one refresh.
- [ ] Saving any settings tab creates a configuration version (REQ-112) whose change set names exactly the changed keys; a save without a reason is refused.
- [ ] Adding a second hostname works, marking it primary moves the flag, and attempting to add a hostname already owned by another site is refused with the owning site named.
- [ ] Removing the primary host of a live site is refused with a reason; removing a non-primary host offers a redirect entry in one click.
- [ ] A locale enabled with no translations raises the documented warning without blocking the save, and the fallback chain resolves to a locale that is in the set.
- [ ] Changing the timezone changes the rendered time of an existing scheduled item in the editorial calendar, in the item's own display too.
- [ ] The SEO defaults preview renders a real page's metadata using the suffix and template, and saving `robots` defaults changes the generated output for that site and no other.
- [ ] Setting media scope to site-only hides shared-library assets in the media picker for that site's editors while a holder of the shared scope elsewhere still sees them.
- [ ] Granting a user the editor role in one site gives them access to that site's content list and does not grant access to a second site; revoking the grant removes access on the next request without a sign-out.
- [ ] A site-scoped token or user reading another site's settings gets `403`, including through the cross-site list routes with a site filter naming the foreign site.
- [ ] Removing the last grant holder of a site is refused, and deactivating a user who is the last grant holder of a site raises the documented flag.
- [ ] A script snippet saved with `production_only` is absent from a non-production render and present in a production render, and the renderer places it in the configured position.
- [ ] Cloning a site with menus, forms, redirects, workflow definitions and two selected pages produces a new site whose listed inventory matches the copy, with the pages unpublished drafts and no users, grants, submissions, comments, schedules, analytics or secret references copied.
- [ ] The clone report names every skipped item with a reason, and a clone interrupted mid-run reports `failed` with the completed items listed and the target site left in a clearly incomplete state.
- [ ] The switcher remembers the last site across a sign-out and sign-in, "All sites" is unavailable on the settings and users screens with a stated reason, and bulk actions in "All sites" mode name the affected site count in the confirmation.
- [ ] Archiving a site with published content is refused with the count; archiving an empty site removes it from the switcher and stops its hostnames from serving.
- [ ] All new screens render at 390 px without horizontal scroll, cross-site lists become cards with the filter pinned, and the walkthrough reports zero high findings.

### QA plan

The walkthrough visits `/sites` (create a site, then archive an empty one), `/sites/<id>/settings` (edit identity, add two hostnames, mark primary, verify one, set locales with a warning case, change the timezone and confirm the calendar time moved, edit SEO defaults and check the preview, flip media scope), `/sites/<id>/users` (grant a user, confirm access in that site only, attempt to remove the last holder, revoke, confirm access ends), a scripts entry (`production_only` snippet check in two environments), `/sites/<id>/clone` (clone with menus, forms, redirects, workflow definitions and two pages, read the report, verify nothing forbidden came across, then open the new site), and the switcher from a cross-site list (switch, filter, "All sites" mode, bulk confirmation). Ops checks: change a hostname's primary flag and confirm routing for a public request; re-request a site's settings with a token granted only to another site and expect `403`; run a clone twice to the same target key and confirm the second is refused by the unique key. Visual check: the switcher shows real sites with search, the settings tabs render real values, the clone wizard shows the copy inventory with skip reasons, and cross-site lists show a Site column with live data.

### Slices

1. **Settings home and domains.** Migration `0119_site_configuration.sql` (site column extensions plus domains, grants, scripts and clone tables); the settings home with all tabs, domain management with verification state, locales, theme, SEO defaults with preview, media scope, advanced section; every save through the version recorder; the History tab. *Done when:* acceptance 1–7 and 16 pass and `/sites/<id>/settings` is in the walkthrough inventory.
2. **Grants, scripts and the switcher.** Site grants screen with role summaries and last-holder protection; scripts table with the environment flag and renderer positions; the switcher payload, chrome component, per-user persistence, and "All sites" mode with its guardrails across the cross-site lists. *Done when:* acceptance 8–12, 15 and 17 pass and the switcher plus one cross-site bulk action are exercised in the QA environment.
3. **Clone and cross-site polish.** Clone wizard with the full inventory, the job runner with per-item reports and failure semantics, offline checks for the copy rules (no users, submissions, schedules, analytics or secrets), and the archived-site behaviour for hostnames. *Done when:* acceptance 13–14 pass, a clone is verified item by item against its report, and the QA report lists zero high findings for the wave.

### Risks / notes

- Site scope is a security boundary, not a filter. Every site-scoped read and write re-checks the grant server-side; a list route that trusts a `site_id` query parameter without the check is the one bug class that must never ship here.
- The switcher is convenience, not authority. It changes defaults and never widens access; a user switched to a site without a grant sees the sites they can reach and nothing else.
- Hostname uniqueness is installation-wide and normalised (lowercase, no trailing dot, punycode stored consistently). Wildcards are recorded verbatim and matched by the routing layer; this screen does not invent routing rules.
- Clone is a copy, never a move: content starts unpublished, links and schedules do not come across, and the report is the contract. A clone that silently copies a schedule is how a staging site publishes into production on Monday morning.
- Secrets do not clone. A clone of a settings surface that references a secret lists the re-binding steps and leaves the reference unbound; silently copying a reference is a leak of intent even when the value stays behind.
- Scripts are the sharpest surface in this REQ: raw text rendered on every page of one site. Admin-only permission, versioned writes, sanitising at render boundaries, and a warning banner in the editor. No site editor role may write them unless the installation explicitly grants that role the permission.
- Timezone changes ripple into scheduling, calendar rendering and date formatting. One helper resolves "site now" everywhere, and changing the timezone records the before and after in the change set so operations can explain an item that fired an hour earlier than expected.
- Media scope must be enforced in the pickers and in the API, not only in the UI; a site-only site whose editor can still query shared assets through the media list route has a scope hole.
- Cloning pages copies their drafts and blocks but not their revisions, comments, preview links or schedule entries; the report says so in those words so nobody expects revision history in the new site.
