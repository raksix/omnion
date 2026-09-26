# REQ-048 — App Marketplace *(headline)*

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** `apps/marketplace`
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

Not just plugins — full applications:

```text
Apps
├── CRM
├── HR
├── ERP
├── AI Agents
├── Workflows
├── Themes
├── Integrations
└── Templates
```

## Notes

- Extends REQ-023 (Marketplace Expansion); installs flow through the Package Installer
  (REQ-044) with dependency resolution and permission provisioning.

## Implementation spec

### Scope (in / out)

**In**

- A new frontend app `apps/marketplace` (Next.js, sharing `packages/ui` and the API client): the
  catalogue, entry pages, search and the Installed view.
- One catalogue model over the eight groups: six *kinds* — `app` (CRM, HR, ERP), `ai_agent`,
  `workflow`, `theme`, `integration`, `template` — with categories naming the group. Installing
  means a module bundle; an agent plus **requested** (not granted) permissions; workflow definitions
  inserted **disabled**; a theme package; a connector with a credential placeholder, never a secret;
  a content pack with no schema change.
- Browse with filters, sort and search; entry detail with versions, declared permissions,
  dependencies and changelog; an install sheet showing exactly what an app asks for; single and
  bundle installs delegated to REQ-044; an Installed view reading the installer's ledger.
- A private catalogue (entries only the publisher's organization can see) plus `omnion seed` demo
  entries, so the marketplace is never an empty shell in dev or QA.

**Out**

- Payment, trials and license enforcement — `license` is displayed, not policed (REQ-023's paid
  tiers need billing first).
- The install itself: dependency resolution, permission provisioning, migrations and rollback are
  REQ-044's; the marketplace requests an install and reports its progress.
- The plugin runtime, theme preview rendering (REQ-018/062), reviews/ratings, store moderation and
  any hosted remote registry — v1 reads a catalogue that ships with the installation.

### Screens (UI)

New app `apps/marketplace`; the session is the panel's (same-site cookies), and every route
redirects to the panel sign-in when it is missing.

| Route | Purpose |
|---|---|
| `/` , `/apps` , `/kinds/[kind]` | Featured rail + catalogue with filters (kind, category, license, verified-only) |
| `/apps/[slug]` | Entry detail: Overview, Versions, Permissions, Dependencies, Changelog |
| `/installed` , `/submit` , `/search` | Installed view, private-catalogue publishing, shareable query |

- **Catalogue** columns: Name (icon, publisher, verified tick), Kind badge, Category, Version,
  License, Compatibility with the running version, Installs, Updated, action (`Install` /
  `Installed` / `Update`); grid at ≥ 1440 px, table below. Filters and sort are URL-persisted
  (`?kind=&category=&license=&q=&sort=&page=`) and bulk select opens one bundle sheet.
- **Entry detail** tabs: Overview (markdown, screenshots), Versions (released, compatibility, size,
  yanked), Permissions (each key in a human sentence, e.g. “read pages” for `content.pages.read`),
  Dependencies and Changelog, the last showing the resolved tree with installed rows marked.
- **Install sheet**: what will be installed, the resolved dependency tree, every requested
  permission with its rationale, the target scope (organization, or a site for site-scoped kinds),
  then the installer's phases — Resolve → Install → Permissions → Roles → Migrations → Workflows →
  Ready — each pending/running/done/failed. A failure names the phase and message and offers Retry.
- **Installed view** columns: Name, Kind, Version, Scope, Installed at, Status (`installed`,
  `update available`, `failed`), actions (Open in panel, Update, Uninstall via REQ-044). **Submit**
  form: manifest, name, summary, category, license, visibility (default private), errors per field.
- Empty states, skeletons and retry exist per view; `/` focuses search, `Esc` closes the sheet, and
  mobile is one column with Install pinned to the bottom. Lucide icons only, light and dark mode.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/marketplace/entries` | Browse (`kind`, `category`, `license`, `q`, `sort`, `page`) | `marketplace.read` |
| GET | `/api/v1/marketplace/entries/{slug}` | Detail: versions, permissions, dependencies | `marketplace.read` |
| GET | `/api/v1/marketplace/kinds` | The eight groups with counts | `marketplace.read` |
| GET | `/api/v1/marketplace/installed` | Installed view (REQ-044 ledger join) | `marketplace.read` |
| POST | `/api/v1/marketplace/installs` | Install one entry or a bundle `{slugs[], scope}`; answers `202` + request id | `marketplace.install` |
| GET | `/api/v1/marketplace/installs/{id}` | Progress of one request (phases + result) | `marketplace.read` |
| POST | `/api/v1/marketplace/submissions` | Publish or update a private-catalogue entry | `marketplace.publish` |
| POST | `/api/v1/marketplace/submissions/{slug}/versions` | Attach a version manifest | `marketplace.publish` |
| DELETE | `/api/v1/marketplace/submissions/{slug}` | Withdraw an own entry | `marketplace.publish` |

New catalogue keys: `marketplace.read`, `marketplace.install`, `marketplace.publish` (category
`marketplace`) — three powers, since a publisher must not be able to install what they published.

### Data model

Migration `database/migrations/00NN_marketplace_catalogue.sql`. The **executed install ledger stays
REQ-044's** (`package_installs`); the marketplace maps entry slug ↔ package id and reads it.

- `marketplace_entries` — `id uuid pk`; `slug text not null` unique on `lower(slug)`;
  `name text not null` (1–80); `summary text not null` (≤ 200); `description_md text not null
  default ''`; `publisher text not null`; `icon text`; `homepage_url text`; `kind text not null`
  check in (`app`,`ai_agent`,`workflow`,`theme`,`integration`,`template`); `category text not null`;
  `license text not null` check in (`free`,`open_source`,`paid`,`subscription`,`enterprise`);
  `verified boolean not null default false`; `featured boolean not null default false`;
  `visibility text not null default 'public'` check in (`public`,`private`) with
  `owner_organization_id uuid references organizations (id) on delete cascade` required when
  private; `deprecated boolean not null default false`; `created_by uuid references users (id)`;
  `created_at`, `updated_at`.
- `marketplace_entry_versions` — `id uuid pk`; `entry_id uuid not null references
  marketplace_entries (id) on delete cascade`; `version text not null` (semver shape check);
  `manifest jsonb not null` (`jsonb_typeof = 'object'`); `min_omnion_version text`;
  `max_omnion_version text`; `size_bytes bigint`, `checksum text`; `released_at timestamptz`;
  `yanked boolean not null default false`; unique `(entry_id, version)`.
- `marketplace_entry_permissions` — `entry_id uuid not null` (fk, cascade), `permission_key
  text not null`, `rationale text not null` (1–200), primary key `(entry_id, permission_key)`.
  Publishing validates every key against the permissions catalogue — a manifest cannot invent one.
- Indexes: `marketplace_entries_kind_idx (kind, category)`, `marketplace_entries_browse_idx
  (visibility, featured desc, updated_at desc)`, `marketplace_entry_versions_entry_idx (entry_id)`.

### Events

| Event | When | Payload |
|---|---|---|
| `marketplace.entry.published` | a submission goes live | `slug`, `kind`, `version`, `visibility` |
| `marketplace.entry.updated` | metadata or a new version lands | `slug`, `kind`, `version` |
| `marketplace.install_requested` | an install request is accepted | `request_id`, `slugs`, `scope` |

Consumed: `package.installed` and `package.install_failed` (REQ-044) flip the Installed view and the
entry badges — the marketplace never fabricates an install result. Emitted events are
organization-scoped where the entry is private, so a subscribed endpoint sees its own catalogue.

### Acceptance criteria

- [ ] The eight groups are browseable — CRM/HR/ERP via `kind = app` + category, the rest via their
      own kind — and the counts on `/` match the filtered lists.
- [ ] `omnion seed` creates one demo entry per kind, marked as seed data, so dev and QA never show
      an empty catalogue.
- [ ] A bundle install of two entries resolves one dependency graph and issues one install request;
      a circular dependency is refused with the cycle named.
- [ ] The install sheet lists every requested permission with its rationale before the request is
      sent, and the API returns the same list from the entry detail.
- [ ] Installing an `ai_agent` entry leaves its tool permissions **pending** until an owner
      approves them; installing a `workflow` entry inserts its definitions disabled.
- [ ] Installing a `template` entry changes no schema — only pages and blocks appear (comparison of
      migration state before/after).
- [ ] Publishing a manifest with an unknown permission key or a non-semver version fails validation
      with the offending value named.
- [ ] Private entries are invisible elsewhere: another organization gets an empty browse result and
      `404` on the slug.
- [ ] Permission keys hold: `marketplace.read` cannot install (`403`); `marketplace.install`
      cannot publish (`403`); publishing cannot install someone else's entry.
- [ ] The Installed view shows installer truth: a failed install surfaces as `failed` with the
      phase and message, never a generic error, and filters and sort survive reload and deep-link
      (`/kinds/ai_agent?license=free`) with a focus-trapped, `Esc`-closable install sheet.
- [ ] Screens render in light and dark mode with no clipped badges or overlapping cards at
      1440 px, 1280 px and 390 px; Lucide icons only, no emoji.
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` (new app included) and
      `bash scripts/qa/run.sh` are green, with every marketplace route in the walkthrough inventory.

### QA plan

- The walkthrough opens the marketplace from the panel nav, browses a kind, opens an entry, opens
  the install sheet and installs the seeded **template** pack (content only, safe), watches the
  phases, lands on `/installed` and follows “Open in panel”; every button, filter and tab is pressed.
- The visual review must see aligned cards with legible kind badges, a fully visible permission list
  in the sheet, and a progress strip that reaches “Ready” without a stuck spinner.
- `scripts/qa/probe-marketplace.cjs` (new, exits non-zero) asserts through the API: a template
  install creates pages and reports the entry installed; a manifest with a bogus permission key is
  refused; `marketplace.install_requested` and `package.installed` both reach the event feed.
- Signed-out and mobile passes: every route redirects; 390 px stays one column with a sticky Install.

### Slices

1. **Catalogue API + seed** — migration, entries/versions/permissions store, browse/detail/kinds
   routes, `marketplace.*` keys, demo entries in `omnion seed`. *Done when:* `cargo test
   --workspace` is green and all eight groups answer through the API.
2. **Marketplace app: shell, browse, detail** — new app, `/`, `/apps`, `/apps/[slug]`,
   `/kinds/[kind]`, filters and states. *Done when:* the walkthrough reaches every catalogue screen
   and the inventory lists them.
3. **Install flow + Installed view** — install request route, sheet, bundle installs, phase
   progress, `package.installed` consumption, deep link back to the panel. *Done when:* the probe
   installs a template end to end and the Installed view shows installer truth.
4. **Private catalogue + submissions + docs** — `/submit`, publish/update/withdraw, validation,
   author docs, probe coverage. *Done when:* an organization publishes a private entry no other
   organization can see, with tests proving both halves.

### Risks / notes

- Installs are irreversible in v1 (REQ-044's rollback is future work), so the sheet states exactly
  what will be created, and the catalogue is local to the installation — a deliberate supply-chain
  trade-off to revisit with REQ-023.
- Paid and subscription licenses are metadata only; the UI must never imply enforcement, and the
  slug ↔ package-id mapping is the only contract with REQ-044 — an unknown package degrades the
  Installed view to “unknown package” rather than guessing.
