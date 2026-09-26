# REQ-071 — Groups & Teams

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core (`crates/identity`) + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Bulk identity management.

- Group registry (name, description, membership), groups list/detail screens.
- Group → role bindings; automatic grant on join, automatic revoke on leave.
- Default group set for new users per organization/site.
- Team entity for organizational structure (department, manager, members) feeding HR and approvals.
- Bulk assignment: add/remove many users at once, CSV import of memberships.

## Implementation spec

> **Where:** `crates/permissions` (group store, membership, group-bound grants) · `apps/api/src/routes/iam.rs` (group surface) · `apps/admin/features/iam/groups/**` (new screens) · **Migration:** `database/migrations/0017_groups_and_teams.sql` (next free number at land time; the ledger is append-only — take the next free slot if one is already used) · **Admin routes:** `/settings/iam/groups`, `/settings/iam/groups/{id}`, `/settings/iam/teams` · **Permission family:** `iam.groups.*`, `users.read` · **Depends on:** REQ-006 (binding model — a group is a first-class `subject_type`), REQ-070 (scope levels a group binding can be placed at), REQ-067 (role picker in the group drawer), REQ-065 (JIT provisioning assigns default groups), REQ-055 HR and REQ-059 approvals consume the team entity.

### Scope (in / out)

**In**

- **One concept, one table.** A team is a group that carries organizational structure: `groups.kind` is `group` (permission container) or `team` (organizational unit with a manager and a place in the department tree from REQ-070). Two parallel tables would drift, so `groups` grows structure columns instead of a second registry.
- **Group registry:** name (unique per organization, case-insensitive), slug, description, colour/initial for the avatar chip, membership mode (`manual` or `provisioned`), member count, created/updated, archived flag. Deleting is refused while a role binding or resource grant still references the group — archive instead, which hides it from pickers and keeps resolution correct.
- **Membership:** add/remove single members, replace the full set, and bulk operations; every mutation records `added_by`/`removed_by` and a reason for imports. Removing a member revokes the grants that flowed from the group inside the same transaction, so the next request already sees the narrower set (no waiting for a nightly job).
- **Group → role bindings:** a binding whose `subject_type = 'group'` (already accepted by the REQ-006 model) at any scope level of REQ-070; the effective set of a member is the union of their direct bindings and the bindings of every group they belong to, with precedence unchanged (`deny > allow`, scope-first as documented).
- **Nested groups:** a group may contain another group (depth ≤ 4, cycles refused with a field-level error naming the offending path). Nesting is opt-in per organization through the security policy; when disabled the API refuses the edge with `409` and the UI hides the control.
- **Default group set:** per organization (optionally narrowed to one site) a list of groups every new or invited account joins automatically — evaluated for local creation, invitations and SSO/JIT provisioning alike, applied inside the creating transaction and evented once.
- **Team entity:** a team carries `manager_id`, `department_id` (REQ-070), a purpose label and member roles (`member`, `lead`, `deputy`). Exactly one manager per team; a manager leaving the team is refused until a replacement is set, so approvals (REQ-059) and HR (REQ-055) always resolve an owner.
- **Bulk assignment and CSV import:** add/remove many users in one request (each row reported individually), plus a CSV import with a mandatory dry run that returns a row-by-row preview (matched, unknown e-mail, duplicate, would-remove-last-manager) before anything is written. Imports are capped (10 000 rows, 2 MB) and every applied row writes an audit entry.

**Out (tracked elsewhere)**

- Role matrix editing → REQ-067; scope levels and resource allow/deny lists → REQ-070; ABAC conditions on group membership → REQ-069; SCIM group provisioning and its token/sync log → REQ-006 (this request exposes the store SCIM writes into); sign-in and MFA → REQ-065/REQ-066; approval policy definitions → REQ-059; HR records themselves → REQ-055.

### Screens (UI)

Nav entries under the existing IAM section: **Groups · Teams**.

| Route | Screen |
|---|---|
| `/settings/iam/groups` | Group list — counts, member totals, bindings, membership mode, archive state |
| `/settings/iam/groups/{id}` | Group detail — tabs `Members`, `Roles & bindings`, `Nested groups`, `Import log`, `Audit` |
| `/settings/iam/teams` | Team overview — department tree from REQ-070 with manager, head-count and approval load |
| `/settings/iam/teams/{id}` | Team detail — members with role chips, manager, department breadcrumb, linked HR/approval references |

- **Group list** columns `Name` (colour chip + initial, link), `Slug`, `Kind`, `Members`, `Bindings`, `Default for new users`, `Updated`; filters: search (250 ms debounce), kind, membership mode, has binding, archived; bulk actions: archive, add members, remove members, assign role, duplicate as new group. Row actions: open, edit drawer, manage members.
- **Members tab:** virtualised table (5 000 rows test fixture) with `Name`, `E-mail`, `Source` (`direct`, `SSO`, `SCIM`, `CSV import`), `Added by`, `Added`, `Actions`; multi-select drives the same bulk endpoints the API exposes, and a progress line reports per-row outcomes when the batch finishes with partial failures.
- **Import wizard (3 steps):** upload/paste CSV → preview table with per-row status chips and a summary band (`N matched · M already members · K unknown · J would fail`) → apply, which is disabled until the dry run passed. The mapping step lets the operator pick the e-mail and role columns; the file is never stored beyond the request.
- **Group detail header** shows member count, binding count, the scope badges of its bindings (organisation, site, department) and the `Default for new users` toggle with its REQ-070-style scope picker.
- **Nested groups tab** renders an indented path list (`Marketing → Events Team`) with add/remove, the depth indicator and a disabled control with an explanatory tooltip when nesting is off.
- **States, keyboard, mobile:** `EmptyState` with a primary action when no groups exist, `LoadingTable` on first paint, inline error with retry; `/` focuses search, `j`/`k` move focus, `enter` opens, `e` edits, `?` opens the shortcut sheet; below `lg` tables become cards and the wizard goes single-column with a sticky footer.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET, POST | `/api/v1/iam/groups` | List with counts and filters / create a group | `iam.groups.read` / `iam.groups.manage` |
| GET, PATCH, DELETE | `/api/v1/iam/groups/{id}` | Detail / rename, describe, archive / archive (delete only when unreferenced) | `iam.groups.read` / `iam.groups.manage` |
| GET, PUT, POST, DELETE | `/api/v1/iam/groups/{id}/memberships` | Read membership / replace, add, remove | `iam.groups.read` / `iam.groups.manage` |
| POST | `/api/v1/iam/groups/{id}/memberships/bulk` | Add or remove many users; per-row outcome | `iam.groups.manage` |
| POST | `/api/v1/iam/groups/{id}/imports` · `/imports/{import_id}/apply` | Dry run a CSV / apply the previewed rows | `iam.groups.manage` |
| GET, POST, DELETE | `/api/v1/iam/groups/{id}/parents` | Nested groups: list, add edge, remove edge | `iam.groups.read` / `iam.groups.manage` |
| GET, POST | `/api/v1/iam/groups/{id}/bindings` | Bindings of the group / attach a role at a scope | `iam.bindings.read` / `iam.bindings.manage` |
| GET, PUT | `/api/v1/iam/groups/defaults` | Default group set per organization (and site) / replace it | `iam.groups.read` / `iam.groups.manage` |
| GET, POST | `/api/v1/iam/teams` | Team list with manager and head-count / create or promote a group to a team | `iam.groups.read` / `iam.groups.manage` |
| PATCH | `/api/v1/iam/teams/{id}` | Set manager, department, purpose; refuse an orphaned team | `iam.groups.manage` |
| GET | `/api/v1/iam/users/{id}/groups` | Groups and derived grants of one user | `users.read` |

Every route carries `guards::require("<key>")`; handlers scope the target group to the caller's organization and answer `404` for a foreign group id, and `409` for a delete that a binding still references.

### Data model

```text
groups          + kind text not null default 'group' check (kind in ('group','team')),
                  slug text not null, description text not null default '', colour text,
                  membership_mode text not null default 'manual' check (.. in ('manual','provisioned')),
                  archived_at timestamptz, created_by uuid, updated_at timestamptz
                · unique (organization_id, lower(name)) and unique (organization_id, slug)
group_members   + member_role text not null default 'member' check (member_role in ('member','lead','deputy')),
                  source text not null default 'direct' check (source in ('direct','sso','scim','csv')),
                  added_by uuid, reason text
                · primary key (group_id, user_id); index group_members_user_idx (user_id)
group_defaults  (id, organization_id, site_id, group_id, created_by, created_at)
                · unique (organization_id, coalesce(site_id, '00000000-0000-0000-0000-000000000000'::uuid), group_id)
group_nesting   (parent_group_id, child_group_id, created_by, created_at; primary key (parent_group_id, child_group_id))
teams           (group_id primary key references groups (id) on delete cascade, department_id uuid,
                 manager_id uuid references users (id) on delete set null, purpose text, updated_at)
group_imports   (id, organization_id, group_id, created_by, row_count, matched_count, applied_count,
                 status check (status in ('previewed','applied','failed','expired')), preview jsonb, applied_at, created_at)
```

Indexes: `groups_org_kind_idx (organization_id, kind) where archived_at is null`, `group_members_group_idx (group_id)` (implicit via the primary key) plus `group_members_added_idx (group_id, added_at desc)`, `group_nesting_child_idx (child_group_id)`, `teams_department_idx (department_id) where manager_id is not null`, GIN on `group_imports.preview` only if preview search is added later.

Migration `database/migrations/0017_groups_and_teams.sql`: creates the tables above (creating the REQ-006 `groups`/`group_members` pair only if that migration has not landed, matching its column names exactly — one table per concept), adds the columns to `group_members`, seeds nothing beyond the schema; previews expire after 24 hours and are cleaned by the maintenance job.

### Events

- Emitted: `iam.group_created`, `iam.group_updated`, `iam.group_archived`, `iam.group_member_added`, `iam.group_member_removed`, `iam.group_membership_bulk_applied` (counts, not one event per row), `iam.group_import_applied`, `iam.team_manager_changed`, `iam.group_nesting_changed`, `iam.default_groups_changed`.
- Consumed: `user.created` applies the default group set inside the creating transaction and emits the member events; `user.deactivated` removes membership rows and revokes derived bindings; `iam.provisioning_synced` (REQ-006) applies SCIM group changes with `source = 'scim'`; `site.deleted` (REQ-005) clears site-narrowed default entries.
- Payloads carry ids, counts and the source — never member e-mail lists. Automations (REQ-003) may trigger on `iam.group_member_added` to notify team channels; the security centre (REQ-012) subscribes to `iam.team_manager_changed` because it changes who can approve.

### Acceptance criteria

- [ ] `0017_groups_and_teams.sql` applies on a fresh and on a populated database and coexists with the REQ-006 tables without duplicate definitions; `cargo test --workspace` is green.
- [ ] A group with an attached role grants that role to every member on the next request; removing the member revokes it on the next request with one `iam.group_member_removed` event.
- [ ] Replacing the membership set is atomic: a failure on one unknown user leaves membership untouched and the response names the rejected rows.
- [ ] Bulk add/remove reports a per-row outcome and applies exactly the successful rows.
- [ ] CSV dry run writes nothing; applying requires a preview id, and a preview older than 24 hours is refused with `410`. A duplicate e-mail, an unknown e-mail and a would-remove-last-manager row each appear with their own status in the preview.
- [ ] Nesting works to depth 4; a cycle (`A → B → A`) and a self-edge are refused with the offending path in the message; with nesting disabled the API answers `409`.
- [ ] Nested membership grants exactly the union of the parent and child bindings and never a sibling's binding.
- [ ] The default group set applies to a locally created user, an invited user and a JIT-provisioned account; a site-narrowed entry only joins users of that site.
- [ ] A team always has exactly one manager; removing the manager without a replacement is refused with a message naming the team, and `iam.team_manager_changed` fires on a successful change.
- [ ] Deleting a group that still holds a binding or resource grant answers `409`; archiving hides it from pickers, keeps the bindings resolving for current members and shows it in the archived filter.
- [ ] Group name uniqueness is case-insensitive and per organization; creating `Marketing` twice answers a field-level error.
- [ ] The members table renders 5 000 rows without layout breakage and member counts in the list match the detail tab.
- [ ] Every membership, nesting, default-set and team change writes an audit entry with actor, target and before/after; an import writes one entry per applied row plus one summary entry.
- [ ] A group binding at site scope (REQ-070) grants only on that site, and the group picker in REQ-067 shows archived groups as disabled rather than hiding them silently.
- [ ] All routes answer 401/403/404/409 as documented, and every screen has empty, loading and error states with zero high findings in the QA pass.

### QA plan

Extend `scripts/qa/walkthrough.cjs` with `/settings/iam/groups`, `/settings/iam/groups/{id}` and `/settings/iam/teams` (desktop) plus `/settings/iam/groups` (mobile). The script must create a group, add two members through the bulk action, assert the counts, open the import wizard, upload a fixture CSV containing one valid and one unknown e-mail, assert the preview summary, cancel (nothing written), then run the dry run again and apply it, asserting the member table shows the matched row only. It must attach a role to the group, open the role's member list and assert the derived membership is marked as group-sourced, attempt a nesting cycle and assert the field-level error, and archive a group while asserting it disappears from the picker. Screenshots `page-iam-groups`, `page-iam-group-detail`, `page-iam-group-import`, `mobile-iam-groups`; the visual check looks for readable counts, AA-contrast status chips and a wizard with a clear step indicator.

### Slices

1. **Registry and membership.** Migration for the group store, CRUD with archive rules, membership single/bulk/replace with per-row outcomes, group list and detail with the members tab, audit entries and events. *Done when:* acceptance 1–4 and 11–13 pass and the walkthrough adds and removes a member through the UI.
2. **Grants, nesting and defaults.** Group bindings at every REQ-070 scope, derived-grant resolution and revocation on leave, nested groups with cycle detection and the depth cap, default group set applied on user creation, team entity with manager invariants. *Done when:* acceptance 5–7, 9 and 14 pass and a parity test proves group-derived grants equal the direct-binding path.
3. **Bulk import and teams screen.** CSV import with dry run, preview budgets and expiry, import log tab, teams overview and detail with the department breadcrumb and manager control. *Done when:* acceptance 5, 10 and 15 pass and the import fixture round-trips through the walkthrough.

### Risks / notes

- **Resolution cost.** Group membership widening multiplies the bindings considered per request; keep the same single decision path from REQ-006, benchmark a seeded organization (5 000 users in 200 groups, 5 000 bindings) and cache only behind the documented invalidation events.
- **SCIM is the source of truth for provisioned groups.** A hand edit to a `provisioned` group's membership must be refused or explicitly marked as an override, otherwise the next sync silently reverts it; document the rule on the screen rather than surprising an administrator.
- **Import is the risky write path.** Dry run mandatory, per-row audit, 10 000-row and 2 MB caps, no file retention past the request, and personal data never echoed back in the event payload.
- **Nesting is depth-capped on purpose.** Depth ≤ 4 with cycle detection keeps the resolution explainable and the SQL predicate simple; unbounded nesting would make the simulator (REQ-074) output unreadable.
- **One concept per table.** If REQ-006 (or the SCIM work) already shipped `groups`/`group_members`, this migration only adds the missing columns — never a second registry with the same name.
- **Archive, don't delete.** Bindings, resource grants and historical audit entries reference a group; deletion would either cascade history away or leave dangling subjects, so the UI path is archive and delete stays a guarded, unreferenced-only operation.
