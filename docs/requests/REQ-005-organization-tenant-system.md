# REQ-005 — Organization / Tenant System

> **Status:** in-progress (`3eff59f`) · **Captured:** 2026-09-25 · **Layer:** core (`crates/identity`)
> **Source:** owner brief — platform feature pool (2026-09-25)
>
> Slices 1 and 2 shipped (`0c63b73` for slice 2). Slice 3's API, migration and the Settings,
> Modules and Billing tabs shipped (`9b5f268`); the invite-policy behaviours and the Audit tab
> shipped with them (`337c5bc`), and the suspend/archive flows closed the slice (`a25267a`).
> **Slice 3 is complete** — a frozen tenant keeps every read, refuses every write by name, and
> the status change itself is the one write that gets through, so a tenant can always be brought
> back. Slice 4's events, retention sweep and webhook-isolation walk shipped earlier; **the
> member drawer shipped with them** — the one screen the spec and the QA plan both name and
> neither had. What remains in slice 4 is the mobile pass and the green QA walkthrough.

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

- [x] The organizations list, organization detail with every tab, the switcher, the invite dialog and the invitation page exist at the routes above and appear in the QA walkthrough inventory.
- [x] A user belonging to two organizations can switch between them with the header switcher, and the site list, pages and media follow the switch.
- [x] Backfill is proven: every pre-existing user has exactly one primary membership, every organization has a settings and a limits row, and no orphan row exists.
  _Membership half proven by `the_backfill_gives_every_home_organization_one_primary_membership`. The settings and limits half by `the_backfill_gives_every_organization_a_settings_and_a_limits_row` in `apps/api/tests/tenancy_limits.rs`: no orphan row in either table, exactly one row per organization however often it is read, and a hand-edited ceiling survives a later read (the read path upserts the defaults, so it must not reset what it did not write). An organization created *after* the migration legitimately has no row until the tab is first opened, which is why the read path upserts — the acceptance line is about the tenants that existed when it ran._
- [x] Inviting an existing member is refused naming them; inviting the same address twice returns the pending invitation instead of creating a duplicate.
- [x] The invitation link opens the preview, acceptance works for an existing account and for a new sign-up, and both land on the organization overview.
- [x] Expired, revoked and already-accepted tokens each render their own explanation, and a rate-limited preview does not reveal whether the organization exists.
- [x] Accepting an invitation at the seat limit is refused with `organization.limit.reached` naming the ceiling; raising the limit makes the same invitation acceptable.
  _`a_ceiling_really_bounds_accepting_an_invitation`: inviting a third address is *not* refused — the REQ puts enforcement at acceptance, not at inviting — while accepting is, with `resource: seats` and the ceiling in `details`. The refused sign-up leaves no account behind (asserted with a count), and raising `seat_limit` lets the same token through._
- [x] Creating a site beyond `site_limit` is refused the same way and the panel disables the control with that reason.
  _`a_ceiling_really_bounds_creating_a_site`: one site fits, the second is `403 organization.limit.reached` naming the ceiling, and raising `site_limit` lets the *same* create succeed — which is what proves the refusal came from the stored plan. The panel half (a disabled control carrying the reason) is the Billing tab's bars plus the `organization.limit.reached` error surface._
- [x] Removing the last owner is refused by the API and disabled in the UI with the reason shown.
- [x] A member with `content.pages.read` in organization A gets 404 for a page id of organization B, and cannot see B's audit feed.
- [x] A platform account can list every organization and must send `organization_id` on a write; omitting it is a 400 naming the field.
  _`e00c53c`. "Naming the field" was the clause that was actually missing: the sentence already contained the string `organization_id`, so a check on the message passed against an error no client could act on. `organization_required()` in `apps/api/src/scope.rs` is now the single refusal every scope-resolving route shares, answering the same code, the same sentence and the same `details: { field: "organization_id", reason: "no_primary_organization" }`. The two refusals a client renders differently are kept apart on purpose and unit-proved (`a_missing_tenant_names_the_field_a_client_has_to_send`): a missing tenant names a field because it is the caller's problem to fix, and `cross_organization` names none because a picker there would only ever reproduce the same refusal. The walk `a_platform_account_names_the_tenant_every_write_needs` proves all three clauses in order — the list first, so the later assertions are known to be about a platform account rather than a session that failed to sign in — and asserts the 400 is `organization_required` and not `permission_denied`, since the account holds its permissions and a status-only check would have recorded the platform's rule as proven while proving the permission. Panel half: `TenantPicker` renders a labelled organization select for a subject with no organization of its own and is absent for everybody else (and in `global` scope, where "no tenant" is the point), and the grant form refuses in the browser with the server's own sentence so the answer lands at the field instead of in a banner above a form that has no such control._
- [x] Department CRUD works, a department cannot become its own ancestor, and a role bound at department scope appears in `/iam/effective-permissions` for its members.
  _Proven by the 7 HTTP walks in `apps/api/tests/tenancy_departments.rs`: the cycle refused as `department_cycle`, the role resolving for a member and not for an outsider, the grant gone once the member leaves, a parent's binding reaching its child until it is archived, and another tenant's department a 404._
- [x] Switching a module off for an organization hides its navigation entry and makes its API answer 403 naming the module; switching it back on restores both.
  _Slice 3 proved the half that stores the decision (`a_module_with_no_decision_is_on_and_the_toggle_persists`, `a_module_the_installation_does_not_ship_leaves_the_others_alone`). Slice 4 proved the half that applies it, in `switching_a_module_off_hides_its_api_and_switching_it_back_restores_it`: media answers 200, the switch is flipped, and both the collection route *and* a sub-route (`/media/files`) answer `403 organization.module.disabled` with the module key and its product name; analytics, tenancy and pages keep answering 200; the other tenant's own media read is untouched while A's site is `cross_organization` to them; `/me/organizations` reports exactly `["media"]` for A and `[]` for B; and switching it back restores 200. The round trip is the proof — a guard that only ever refuses would satisfy every refusal assertion alone. Panel half: the sidebar is filtered on `disabled_modules` and the Modules tab's consequence sentence now names the screen that disappears._
- [x] Invite policy `closed` refuses new invitations; `self_serve` lets any member with `organizations.manage` invite; `owner_approval` queues the invitation until the owner releases it.
  _All three behaviours are enforced in the create-invitation path, and the queue is a real state rather than a stored intention. `a_closed_organization_refuses_an_invitation_and_names_its_policy` proves `closed` refuses *by name* with the policy in `details` **and** leaves no row behind — a policy could otherwise "refuse" by writing a dead invitation — then the same address succeeds once the tenant is opened. `self_serve_hands_over_a_working_link_to_anyone_who_may_manage` proves the link it returns actually opens (`usable: true`). `owner_approval_queues_the_link_until_an_owner_releases_it` is the whole policy: the create answers **202 with no token at all** (a manager who cannot release has nothing to forward), the queue lists the row, the manager who raised it is refused `not_an_organization_owner` — the only thing that can refuse them is the owner check, since they already hold `organizations.manage` — the owner's release mints the link once, and a *second* release is refused `invitation_not_queued` rather than minting a second link and orphaning the first. `a_queued_link_never_works_and_says_so` holds a leaked token directly from the store and proves it is inert *and* that it is indistinguishable from a token nobody issued. `an_owner_inviting_into_their_own_tenant_is_not_stuck_behind_the_queue` proves the policy cannot deadlock on the owner. `one_tenants_queue_is_another_tenants_invisible_row` proves the isolation rule._
- [x] The Billing tab shows seats, sites, storage and AI spend against their limits, and the CSV matches the on-screen numbers.
  _`the_usage_csv_repeats_the_numbers_the_tab_renders` parses the CSV and compares each `used` figure against the value the JSON endpoint returned, and asserts every row repeats the plan; each bar also names its limit source and, for AI, the window it measures. A `null` ceiling reads as "unlimited" rather than as a zero (`an_unlimited_ceiling_reads_as_unlimited_everywhere`); the API refuses a literal `0`, which would render identically to "unlimited" while meaning the opposite._
- [x] Suspending an organization shows the banner, blocks writes with the reason and keeps reads available; reactivating restores writes.
  _`a_suspended_organization_keeps_reads_and_refuses_writes_by_name`: ten reads of a suspended
  tenant (the organization, its members, departments, settings, modules, limits, usage, one site,
  one site's domains, and the switcher's own list) all answer 200, while eight writes spread
  across every family — settings, modules, limits, departments, invitations, sites, a site rename
  and a domain — all answer `409 organization_not_writable` naming the tenant and its status and
  carrying `reads: true` so a panel can say "still readable" rather than "gone". The three
  refused writes are then read back out of the database, because a refusal that left the row
  behind would be a refusal that failed. `reactivating_restores_writes_and_archives_freeze_them_too`
  covers the round trip: suspend → 409, reactivate (which is the one write the freeze must not
  block) → writes work again, archive → 409 again, reads still 200, and a *rename* of a frozen
  tenant is still refused because the escape hatch is for a status change, not for any payload
  that happens to carry one. `a_status_move_is_audited_and_announced_as_its_own_event` proves the
  lifecycle action and the bus event. The panel half is `runOrganizationSuspend` in the
  walkthrough: the banner is absent while active, present on a *frozen tenant* screen with the
  right status, a settings save is refused with the reason on screen, and the reactivation takes
  the banner away._
- [x] The Audit tab lists the tenant's own trail, filters it by action, actor and date, and exports a CSV of exactly what it renders.
  _`the_audit_tab_reads_this_tenant_only_and_exports_what_it_shows`: the route is guarded by `audit.read` and **not** by `organizations.read`, and the walk asserts the refusal first — a trail names every privileged act, so "can see the member list" must not imply it. An auditor then sees the writes the walk just made, every human row names its human and a system row says `system` rather than rendering blank, the action filter is exact and narrows both the rows and the count, a typo'd actor is refused `invalid_actor_filter` rather than answered as an empty history, the CSV carries the same page (`rows == entries.len`), and another tenant's trail is a `404`._
- [x] A tenant's stored `audit_retention_days` is enforced: rows older than the window are removed on a scheduled sweep, the sweep is scoped to that tenant, and it files a system `organization.retention.swept` row and event carrying the count and the cutoff.
  _`the_retention_sweep_applies_each_tenants_own_window` and `a_tenant_keeps_rows_inside_its_window_and_one_without_settings_is_still_swept`, in `apps/api/tests/tenancy_limits.rs`. Two tenants hold two different windows (30 and 365 days) and the walk plants rows at 2, 60, 100 and 400 days old: the 30-day tenant loses both of its expired rows and keeps the 2-day one, the 365-day tenant loses the 400-day one and **keeps** the 100-day one, the sweep reports 3 in total, and its receipt repeats `rows_removed: 2` / `retention_days: 30` as a `system` actor with the matching bus event. A second sweep over the same data removes 0 and files 0 — a trail full of its own nightly housekeeping is a trail nobody reads. The second walk covers the two edges a platform-wide default would hide: a row inside the window is untouched, and a tenant with no `organization_settings` row (created after the backfill) is held to the 365-day default instead of being skipped, which would keep its history forever._
- [x] An org-scoped webhook endpoint subscribed to `organization.member.joined` delivers only that
  organization's events.
  _`a_members_join_reaches_only_the_tenant_it_belongs_to`, in `apps/api/tests/events.rs`. Two
  routes emit that name — the administrative `POST /organizations/{id}/members` and the
  invitation acceptance — so both are exercised. Each tenant connects its own endpoint to a real
  loopback receiver, plus a **third endpoint inside tenant A subscribed to a different name**,
  because subscription filtering and organization filtering are two different rules and only
  running both proves either. Each tenant's join reaches its own receiver naming its own
  `organization_id` and its own `user_id`; the per-endpoint delivery history and the
  `/events?name=` feed agree, and an acceptance into tenant A does not move tenant B's receiver.
  The walk also covers the direction a name-only fan-out gets wrong: a fact that belongs to **no**
  tenant is a platform fact, queues 0 deliveries, and the following tick claims 0 — the endpoints
  subscribed to that very name must not receive it._
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
   *Status:* **slice 3 is complete.** The ceilings are enforced and proven (`a_ceiling_really_bounds_creating_a_site`, `a_ceiling_really_bounds_accepting_an_invitation`, `the_usage_csv_repeats_the_numbers_the_tab_renders`), the invite policy is enforced in all three modes with a real queue (`1f09d86`…`ba4f6b9`), the Audit tab lists, filters and exports (`3fae4f3`, `0199709`), and the suspend/archive **behaviour** ships with a real write-path guard (`a25267a`): a frozen tenant keeps every read and refuses every write by name, the status change is the one write that gets through so a tenant can always be brought back, and the panel says so in a banner on every screen rather than letting a person discover it one refused Save at a time.
4. **Events and hardening** — tenant lifecycle events, `organization.limit.reached`, module toggle events, per-organization retention sweep, mobile pass and empty states.
   *Done when:* an org-scoped webhook endpoint subscribed to `organization.member.joined` delivers only that organization's events, and the QA walkthrough is green.
   *Status:* **slice 4, first unit shipped** (`0d95b2d`). The module switch is no longer a stored intention: `module_routes()` in the identity crate is the single table of which module owns which path, one layer on the whole `/api/v1` router reads it, the refusal is `403 organization.module.disabled` naming the module and its product name, `/me/organizations` reports the tenant's disabled keys so the sidebar can drop the entry, and the round trip is proven in `switching_a_module_off_hides_its_api_and_switching_it_back_restores_it`. The lifecycle events named in the spec's table were already emitted by slices 1–3 (`organization.created/.updated/.suspended/.archived`, `.member.invited/.joined/.removed/.status_changed/.role_changed`, `.department.created/.updated/.archived`, `.limit.reached`, `.module.enabled/.disabled`), so what is left in this slice is the mobile pass and the `organization.member.joined` webhook-isolation walk. The retention sweep shipped after it (`b8f4c21`): `organization_settings.audit_retention_days` is no longer a stored intention. `omnion_audit::purge_before` is the only place a row leaves the trail, scoped by `organization_id` in the statement itself, and `apps/api/src/retention_runner.rs` walks the tenants whose own window has closed, removes what fell out of it, then files a system `organization.retention.swept` row and announces the same event with the count and the cutoff. Proven by `the_retention_sweep_applies_each_tenants_own_window` (two tenants, two windows, and the only assertion that catches a platform-wide default is the one on the tenant that *kept* its 100-day-old row) and `a_tenant_keeps_rows_inside_its_window_and_one_without_settings_is_still_swept` (a tenant with no settings row is held to the 365-day default rather than skipped, because a settings row missing after the backfill would otherwise keep a history forever).
   *Status:* **slice 4's done-when is now proven** (`d9e2ab3`). `a_members_join_reaches_only_the_tenant_it_belongs_to` in `apps/api/tests/events.rs` gives each of two tenants an endpoint subscribed to `organization.member.joined` and a real loopback receiver, adds a third endpoint *inside tenant A subscribed to a different name* — subscription filtering and organization filtering are separate rules, and a walk that only tests the first can pass an implementation that ignores the second — and then drives **both** routes that emit the name: the administrative add-member and the invitation acceptance. Each tenant's join reaches its own receiver naming its own organization and member; the delivery history and the event feed agree; tenant B's receiver does not move when tenant A accepts somebody. The walk's last third is the direction a name-only fan-out gets wrong: a fact belonging to **no** tenant is a platform fact, and it must queue 0 deliveries and leave the next tick idle, rather than reaching every endpoint subscribed to that very name.
   *Status:* **the member drawer shipped** — the screen the spec's Members bullet and the QA plan both name ("open a member drawer and extend a binding") and which did not exist until this slice. Reading it first turned up a gap that is worth recording on its own: the spec lists three operations — **add / extend / revoke** — and the platform had verbs for two of them. `POST /api/v1/iam/bindings` granted and `DELETE /iam/bindings/{id}` revoked, but nothing could *lengthen* a temporary grant, so giving somebody another month meant revoking and re-granting. `bindings::extend_expiry` is the new store operation and it deliberately updates **the same row**: the alternative leaves two live bindings for one role and scope, which the effective-permissions screen then renders as the same role twice with two different windows, neither of them the truth. `a_temporary_grant_is_extended_in_place_and_never_into_a_second_row` is the walk that catches that, and the assertion that catches it is a *count* — a revoke-and-re-grant passes every other line. The drawer's own surface is `GET /api/v1/organizations/{id}/members/{user_id}` (one request, so a half-filled panel is not a rendering choice but a failure), plus the three binding operations beside it; a member of another tenant is a `404`, and a grant is refused when the subject is not a member or the role belongs elsewhere, so a tenant cannot end up with a binding applying to nobody.

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
