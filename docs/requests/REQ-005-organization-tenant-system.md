# REQ-005 — Organization / Tenant System

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core (`crates/identity`)
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

A first-class organization/tenant layer:

```text
Omnion
│
├── Acme Corp
│   ├── Marketing
│   ├── HR
│   └── IT
│
├── Company B
│   ├── Site A
│   └── Site B
│
└── Company C
```

Each organization can have its own:

- users
- roles
- sites
- API keys
- billing
- plugins
- settings

## Implementation spec

### Scope (in / out)

**In** — `organizations` exists since migration `0001`, and `sites`, `roles`, `role_bindings`, pages,
media and workflows all carry an `organization_id`. What is missing is the tenant layer itself: a
user belongs to exactly one organization today (`users.organization_id`, the "home" organization),
with no membership row, no invitation, no department, no per-organization settings and no limits.
This request makes it first class:

- **Memberships** — `organization_members` is the truth for "who belongs where": a user may belong to several organizations, each membership carrying a status (`active`, `invited`, `suspended`) and an optional primary flag. `users.organization_id` stays and is backfilled as the primary membership, so nothing existing breaks.
- **Invitations** — an owner or administrator invites by e-mail with a role (and optionally a department); the invitation is a hashed single-use token with an expiry, accepted by signing in or creating an account. A user without a membership row stops seeing an organization.
- **Departments / teams** — an organization-scoped department tree; members join departments, and roles can be bound at department scope (`role_bindings.scope_type = 'department'`), the IAM level docs/07-IAM.md §6 promised.
- **Organization switcher** — the panel's current organization is a session-scoped choice (an `omnion_org` cookie) resolved on the server; every scoped query filters by it.
- **Per-organization settings** (`organization_settings`): invite policy, default invite role, locale, timezone, logo, accent colour, audit retention.
- **Per-organization modules** (`organization_modules`): an installation enables a module, an organization can be granted or denied it; navigation and API read the same table.
- **Plans and limits** (`organization_limits`): plan, seats, sites, storage bytes, monthly AI budget — enforced on invitation acceptance, member activation, site creation and AI spend, because a limit that only decorates the UI is a lie.
- **Billing panel** (read-only here): plan, seats used/limit, sites used/limit, storage used/limit, AI spend this month with a link to the AI cost screen. Payment-provider integration stays out.
- **Isolation** — every tenant-scoped handler resolves the organization from the session and the row, answering 404 (not 403) for another tenant's id; a platform account (no primary organization) sees all tenants and must name one to write.
- **API keys** enter this request only as a scope question: the key table gains `organization_id`, the keys screen ships with REQ-022, and this request proves an org-scoped key cannot act outside its tenant.

**Out**

- Payment processing, invoices and tax (REQ-054 / REQ-008 own money).
- Per-organization SSO/LDAP configuration — REQ-006 (advanced IAM).
- Per-organization custom domains and deep white-labelling — REQ-043.
- Cross-organization sharing of content or media (a deliberate non-feature in v1) and data residency / separate databases per tenant.

### Screens (UI)

- **`/organizations`** (platform accounts only; organization accounts are redirected to their own overview) — table: Name, Slug, Status (active/suspended/archived), Members, Sites, Plan, Created, Updated; search by name/slug; filters status and plan; bulk Suspend and Archive; row menu Open, Suspend, Reactivate, Archive, Delete. Empty state "No organizations yet" with Create. Delete requires typing the slug and refuses while the organization still has sites, more than one member, or content — the blocking counts are listed.
- **`/organizations/[id]`** — tabs, each with its own loading, empty and error state:
  - *Overview*: stat cards (members, sites, storage, AI spend), plan card with usage bars, recent activity from the audit feed, quick actions Invite member, New site, Organization settings.
  - *Members*: table of User (name + e-mail), Roles (chips, "via Department" qualifier), Departments, Status, Last active, Joined; filters role, status, department; search by name or e-mail; bulk Invite, Suspend, Remove — never the last owner (the API refuses and the control is disabled with that reason). The member drawer shows identity, membership status, role bindings with scope and expiry (add / extend / revoke), department memberships and that member's recent audit.
  - *Departments*: tree table (Name, Parent, Members, Roles, Status, Updated) with Add, Rename, Move (parent picker that refuses cycles), Archive; bulk archive; a row links to its members and to the roles bound at that scope.
  - *Roles*: the organization's roles (name, priority, inherited, permission count) with links into the IAM role screen (REQ-006) — this tab never duplicates the role editor.
  - *Modules*: one row per installed module with a toggle, the plan ceiling shown when a module is outside the plan, and a sentence about what changes when it is switched off.
  - *API keys*: count, last used, link to REQ-022's screen. *Billing*: plan selector (a request, not a payment), seats/sites/storage/AI bars naming the limit source, Download usage CSV.
  - *Settings*: name and slug (changing the slug warns that public URLs move), locale, timezone, invite policy (owner approval / self-serve / closed), default invite role, logo upload, accent colour (hex with live preview), audit retention (30–3650 days), danger zone (Suspend, Archive).
  - *Audit*: organization-scoped feed with filters (action, actor, date) and CSV export.
- **Organization switcher** (header, next to the site switcher) — dropdown of the caller's memberships (name, role chips, plan badge) with a check on the current one, a search box past six entries, "Create organization" when plan and permissions allow, `⌘⇧O` as the shortcut. Switching sets the cookie, revalidates the session and reloads the current screen; a screen the caller cannot see in the new organization redirects to its overview.
- **`/invite/[token]`** (public) — shows the organization name, the inviting member and the offered role, then sign-in or a short sign-up (name, e-mail pre-filled and locked, password ≥12 with a strength hint). Accepting lands on the organization overview with a welcome banner; expired, revoked or already-used tokens get their own explanation plus "Ask for a new invitation".
- **Invite dialog** (from Members) — e-mails (multi-entry, validated, duplicates refused naming the existing member), role (default from settings), optional department, optional personal message (≤400), and a preview of what the recipient receives.
- **Keyboard** — `N` invite member, `/` focus search, `⌘⇧O` switch organization, `↑/↓` + `Enter` in the switcher, `Esc` closes dialogs and drawers, `S` toggles a module on the Modules tab.
- **Mobile (<1024px)** — the member table becomes cards, the department tree an indented list, tabs a horizontal scroller, the switcher a sheet, usage bars stay labelled. A suspended organization shows its banner on every screen and disables writing controls with the reason.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/organizations` | List tenants (platform accounts) or the caller's own | `organizations.read` |
| POST | `/api/v1/organizations` | Create a tenant | `organizations.manage` |
| GET/PATCH | `/api/v1/organizations/{id}` | Read / change name, slug, status | `organizations.read` / `organizations.manage` |
| DELETE | `/api/v1/organizations/{id}` | Archive-then-delete with the guards above | `organizations.manage` |
| GET | `/api/v1/organizations/{id}/members` | Members with roles, departments, last active | `organizations.read` |
| PATCH/DELETE | `/api/v1/organizations/{id}/members/{user_id}` | Change membership status / remove a member | `organizations.manage` |
| POST/DELETE | `/api/v1/organizations/{id}/role-bindings[/{binding_id}]` | Grant or revoke a role for a member or department | `iam.bindings.manage` |
| GET/POST | `/api/v1/organizations/{id}/invitations` | List / create invitations | `organizations.read` / `organizations.manage` |
| DELETE | `/api/v1/organizations/{id}/invitations/{invitation_id}` | Revoke an invitation | `organizations.manage` |
| GET | `/api/v1/invitations/{token}` | Public preview (organization, inviter, role) — rate-limited | none |
| POST | `/api/v1/invitations/{token}/accept` | Accept (session or sign-up payload) | none (token) |
| GET/POST/PATCH | `/api/v1/organizations/{id}/departments[/{department_id}]` | Department tree CRUD | `organizations.read` / `organizations.manage` |
| GET/PUT | `/api/v1/organizations/{id}/settings` | Settings, invite policy, logo, accent | `organizations.read` / `organizations.manage` |
| GET/PUT | `/api/v1/organizations/{id}/modules` | Module enablement per organization | `organizations.read` / `organizations.manage` |
| GET/PUT | `/api/v1/organizations/{id}/limits` | Plan and limits | `organizations.read` / `organizations.manage` |
| GET | `/api/v1/organizations/{id}/usage` | Members, sites, storage, AI spend vs limits (+ CSV) | `organizations.read` |
| GET | `/api/v1/me/organizations` · POST `/api/v1/me/organization` | The caller's memberships (switcher) / switch the current organization | session only |

No new permission keys: the tenant surface rides the existing `organizations.read`,
`organizations.manage`, `iam.bindings.read/manage`, `sites.*` and `domains.manage`. Ceilings come
from `organization_limits` and are enforced inside the handlers, never trusted from the panel.

### Data model

Migration `database/migrations/0015_organization_members.sql` (next free number if taken; released
migrations are append-only).

| Table | Columns (types) | Indexes / rules |
|---|---|---|
| `organization_members` | id uuid pk, organization_id uuid → organizations cascade, user_id uuid → users cascade, status text ('active','invited','suspended') default 'active', is_primary bool default false, joined_at timestamptz, created_at, updated_at | unique `(organization_id, user_id)`; unique `(user_id)` where `is_primary`; `(user_id)` where `status='active'`; `(organization_id, status)` |
| `organization_invitations` | id uuid pk, organization_id uuid cascade, email text, role_id uuid → roles set null, department_id uuid → departments set null, token_hash text, invited_by uuid → users set null, status text ('pending','accepted','revoked','expired') default 'pending', message text default '', expires_at timestamptz, accepted_by uuid → users set null, accepted_at, created_at | unique `(token_hash)`; unique `(organization_id, lower(email))` where `status='pending'`; `(expires_at)` where pending |
| `departments` | id uuid pk, organization_id uuid cascade, parent_id uuid → departments set null, key text, name text, description text default '', status text ('active','archived') default 'active', created_at, updated_at | unique `(organization_id, key)`; `(organization_id, parent_id)`; check `parent_id <> id`; key format as `sites.key` |
| `department_members` | department_id uuid → departments cascade, user_id uuid → users cascade, created_at, primary key `(department_id, user_id)` | `(user_id)` |
| `organization_settings` | organization_id uuid pk → organizations cascade, locale text default 'en', timezone text default 'UTC', invite_policy text ('owner_approval','self_serve','closed') default 'owner_approval', default_invite_role_id uuid → roles set null, logo_media_id uuid → media set null, accent_color text, audit_retention_days int default 365, updated_at | locale/timezone non-blank; `accent_color` matches `^#[0-9a-f]{6}$` or null; retention 30–3650 |
| `organization_modules` | organization_id uuid cascade, module_key text, enabled bool default true, enabled_at, updated_at, primary key `(organization_id, module_key)` | module_key format as `permissions.key` |
| `organization_limits` | organization_id uuid pk cascade, plan text ('standard','business','enterprise') default 'standard', seat_limit int, site_limit int, storage_bytes_limit bigint, ai_monthly_limit_micros bigint, updated_at | every limit null (unlimited) or `> 0` |

The same migration adds `organization_id uuid → organizations cascade` to the API-key table (that
table ships with REQ-022; the column lands here so the scope question is answered once), adds
`department_id uuid → departments cascade` to `role_bindings`, and widens its scope checks to
`('global','organization','site','department')` with the matching shape rule (`department` needs
organization_id and department_id, no site_id). Backfill: one `organization_members` row per
existing user with `organization_id` set (`is_primary = true`, status `active`), one
`organization_settings` row per organization with defaults, one `organization_limits` row per
organization inheriting the installation defaults. Usage numbers are aggregate queries, not
denormalised counters — drift in a seat count is worse than a cheap `count(*)`.

### Events

| Event | Kind | Payload / webhook relevance |
|---|---|---|
| `organization.created` / `.updated` / `.suspended` / `.archived` | emitted | tenant lifecycle; subscribable by platform endpoints |
| `organization.member.invited` | emitted | organization, e-mail masked in the payload, role, expiry |
| `organization.member.joined` / `.removed` / `.status_changed` | emitted | membership changes — the hook for onboarding automations |
| `organization.member.role_changed` | emitted | binding granted/revoked with scope and department |
| `organization.department.created` / `.updated` / `.archived` | emitted | structure changes |
| `organization.limit.reached` | emitted | which limit, current vs ceiling, action taken — a plan-upgrade trigger |
| `organization.module.enabled` / `.disabled` | emitted | module toggles, so integrations can react |
| `user.created` | consumed | if the e-mail has a pending invitation it is accepted automatically instead of creating a second tenant |

Every event carries `organization_id`, so an org-scoped webhook endpoint receives only its own
tenant's deliveries (REQ-016 filter rules). Invitation e-mails go through the same mailer the
automation engine uses; the token never appears in an event payload.

### Acceptance criteria

- [ ] The organizations list, organization detail with every tab, the switcher, the invite dialog and the invitation page exist at the routes above and appear in the QA walkthrough inventory.
- [ ] A user belonging to two organizations can switch between them with the header switcher, and the site list, pages and media follow the switch.
- [ ] Backfill is proven: every pre-existing user has exactly one primary membership, every organization has a settings and a limits row, and no orphan row exists.
- [ ] Inviting an existing member is refused naming them; inviting the same address twice returns the pending invitation instead of creating a duplicate.
- [ ] The invitation link opens the preview, acceptance works for an existing account and for a new sign-up, and both land on the organization overview.
- [ ] Expired, revoked and already-accepted tokens each render their own explanation, and a rate-limited preview does not reveal whether the organization exists.
- [ ] Accepting an invitation at the seat limit is refused with `organization.limit.reached` naming the ceiling; raising the limit makes the same invitation acceptable.
- [ ] Creating a site beyond `site_limit` is refused the same way and the panel disables the control with that reason.
- [ ] Removing the last owner is refused by the API and disabled in the UI with the reason shown.
- [ ] A member with `content.pages.read` in organization A gets 404 for a page id of organization B, and cannot see B's audit feed.
- [ ] A platform account can list every organization and must send `organization_id` on a write; omitting it is a 400 naming the field.
- [ ] Department CRUD works, a department cannot become its own ancestor, and a role bound at department scope appears in `/iam/effective-permissions` for its members.
- [ ] Switching a module off for an organization hides its navigation entry and makes its API answer 403 naming the module; switching it back on restores both.
- [ ] Invite policy `closed` refuses new invitations; `self_serve` lets any member with `organizations.manage` invite; `owner_approval` queues the invitation until the owner releases it.
- [ ] The Billing tab shows seats, sites, storage and AI spend against their limits, and the CSV matches the on-screen numbers.
- [ ] Suspending an organization shows the banner, blocks writes with the reason and keeps reads available; reactivating restores writes.
- [ ] Empty, loading and error states exist on every screen and tab; no dead control and no placeholder copy.
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and the QA walkthrough pass with zero high findings.

### QA plan

The walkthrough must: sign in as the owner, open `/organizations`, open the seeded organization and
walk every tab (Overview, Members, Departments, Roles link, Modules, API keys link, Billing,
Settings, Audit), invite an address through the dialog (invalid e-mail first → field error), revoke
that invitation, create a department, bind a role to it, open a member drawer and extend a binding,
switch a module off and on, change the locale and the accent colour and reload to prove both
persist, press Suspend and read the banner, then use the switcher and `⌘⇧O` to move to a second
organization (created through the QA signup helper) and confirm the sidebar and site list change
with it. The invitation page is exercised for the happy path and for an expired token, at desktop
and mobile widths.

The visual check must see: tab labels not wrapping, usage bars labelled with number and ceiling,
role chips that do not overflow their cell, the switcher dropdown fitting its longest organization
name, the member drawer readable at 390×844 with reachable action buttons, and no raw i18n keys or
clipped copy on any tab.

### Slices

1. **Memberships, invitations, switcher** — `organization_members` and `organization_invitations` with backfill, membership and invitation endpoints, invite dialog, `/invite/[token]`, header switcher, session-scoped organization resolution in `crate::scope`, cross-tenant 404 tests.
   *Done when:* an invited account accepts, lands in the second organization and switches between the two with the data following the switch.
2. **Departments and scoped roles** — `departments` and `department_members`, `role_bindings` at department scope, the member drawer with binding management, the Departments tab.
   *Done when:* a role bound to a department shows up in `/iam/effective-permissions` for its members and disappears when a member leaves the department.
3. **Settings, modules, limits, billing** — `organization_settings`, `organization_modules`, `organization_limits`, plan and usage endpoints, limit enforcement on invite/site/AI, the Settings, Modules and Billing tabs, suspend/archive flows, the Audit tab with CSV.
   *Done when:* each ceiling has a test that passes only when the enforcement exists, and suspend/reactivate behaves exactly as specified.
4. **Events and hardening** — tenant lifecycle events, `organization.limit.reached`, module toggle events, per-organization retention sweep, mobile pass and empty states.
   *Done when:* an org-scoped webhook endpoint subscribed to `organization.member.joined` delivers only that organization's events, and the QA walkthrough is green.

### Risks / notes

- Isolation is the whole point: every new handler resolves the organization from the session (never
  from the request body) and answers 404 for another tenant's row; a per-handler test is the only
  real proof.
- The backfill must be safe on a live installation: membership rows inserted `on conflict do
  nothing`, nothing deleted.
- Seat limits interact with already-pending invitations; enforcement belongs to acceptance, not to
  inviting — and the UI must say so before someone invites ten people.
- `roles.organization_id` already separates platform roles from customer roles; department bindings
  must not become a route from a customer role to a platform one.
- Deleting an organization cascades to sites, members, workflows and media — hence archive-first
  with a typed confirmation and a blocking list.
- A slug change moves public URLs: the settings form warns, and REQ-064's redirect manager is the
  intended follow-up rather than a silent break.
- Do not denormalise counters (members, sites, storage); compute them, and cache only in the
  response layer if profiling demands it.
