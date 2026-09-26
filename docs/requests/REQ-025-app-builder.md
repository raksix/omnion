# REQ-025 — App Builder / No-Code Builder

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** platform (`apps/admin` + Studio)
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

Users can create their own application without writing code:

```text
New App
 ↓
Choose Data
 ↓
Create Fields
 ↓
Create Views
 ↓
Create Workflow
 ↓
Create Permissions
 ↓
Publish
```

Example — a company builds an internal **Asset Tracking** app:

```text
Asset
├── Name
├── Serial Number
├── Employee
├── Department
├── Purchase Date
└── Status
```

Omnion then generates automatically:

- CRUD
- API
- admin panel
- permissions
- workflow
- audit
- search

## Notes

- A very large feature; built on the dynamic data model (REQ-026) and surfaced in
  Omnion Studio (REQ-049); AI-driven variant: REQ-045.

## Implementation spec

### Scope (in / out)

**In**

- The **builder**: define an app (metadata + entities + fields + views + workflow rules + permissions) entirely in the UI, save it as a draft, preview and publish it.
- The **wizard** exactly as the brief draws it: New App → Choose Data → Create Fields → Create Views → Create Workflow → Create Permissions → Publish. Every step writes real rows; exiting and returning resumes the draft in place.
- The **runtime**: publishing generates working screens (list, detail, create, edit, delete), working API endpoints, an admin navigation entry, permission keys bound to roles, audit entries for every write, search coverage, and CSV import/export.
- Field catalogue v1: `text`, `long_text`, `number`, `decimal`, `currency`, `date`, `datetime`, `boolean`, `select`, `multi_select`, `user`, `relation` (belongs-to / many-to-many), `media`, `email`, `phone`, `url`, `json`, `auto_number`, `formula`, `state` (workflow state field).
- Views v1: `list` (columns, sort, filters, saved views, grouping, inline edit), `form` (sections, column layout, field order, conditional visibility), `kanban` (group by `select`/`state`), `calendar` (by `date`/`datetime`), `detail` (read-only layout with related lists).
- Workflow rules per entity: triggers (created, updated, field changed, scheduled, manual button), conditions (field comparisons with AND/OR groups), actions (notification, e-mail, update this record, create a related record, call a webhook, start an automation from `crates/workflows`, require an approval). The existing workflow engine evaluates them — no second engine.
- Permissions: publishing generates `app.<app_slug>.<entity_slug>.<action>` keys (`read|create|update|delete|manage`) into the catalogue under category `app`, plus a role matrix with presets (owner, editor, viewer, none). Generated endpoints and screens are guarded by those keys, so a viewer genuinely cannot write.
- Schema versioning: every publish stores a snapshot and bumps a version; a destructive change (field type change, field or entity removal) is blocked while records exist unless the builder confirms a documented plan (rename instead of drop, or explicit typed acceptance of data loss).
- Sample data: a draft app can generate clearly-labelled sample records so the preview is meaningful, and `Clear sample data` removes exactly those rows before publish.

**Out**

- Writing code, custom components, custom JavaScript, custom SQL, or any code editor.
- Dashboards and charts (REQ-027), BI/reporting (REQ-028), page building (REQ-063), approval chain configuration (REQ-059 — a rule may *require* an approval; the chain lives there).
- Natural-language generation (REQ-045 prefills this builder; this REQ ships the manual path).
- Marketplace publishing of apps (REQ-023/REQ-048), multi-tenant app distribution, record storage (REQ-026 owns the dynamic record tables — this REQ owns the definition tables only).

### Screens (UI)

| Route | Purpose |
|---|---|
| `/apps` | App list: Name · Slug · Entities · Records · Status · Owner · Updated · actions |
| `/apps/new` | The wizard with the brief's seven steps |
| `/apps/{slug}/builder` | Entity & field designer (tree + field table + inspector) |
| `/apps/{slug}/views`, `/views/{viewId}` | View list; designer with live preview |
| `/apps/{slug}/workflows`, `/workflows/{ruleId}` | Rule list; rule editor |
| `/apps/{slug}/permissions` | Permission matrix (entity × action × role) + presets |
| `/apps/{slug}/publish` | Pre-flight, snapshot diff, publish, unpublish |
| `/apps/{slug}/settings` | Name, icon, colour, description, public-read toggle, archive |
| `/apps/{slug}/{entity}`, `/{entity}/{recordId}` | **Generated** list and detail/edit screens (also reachable from the admin nav) |

- Wizard rail shows all seven steps with state (`done` / `current` / `todo`) and allows back navigation; `Save draft` is available on every step. Step 1 chooses blank app, a starting entity set (Contacts, Assets, Tasks, Inventory), or a JSON schema import.
- Field designer: entity tree left, field table centre, inspector right with type-specific settings and a live form preview below. Field table columns: Label · Key · Type · Required · Unique · In list · Filterable · Indexed · drag handle; reorder by drag or `Alt+↑/↓`.
- Field validation: label required (1–60); key `^[a-z][a-z0-9_]{1,39}$`, unique per entity, reserved-word blocklist; `select`/`multi_select` need ≥ 1 option; `relation` needs a target entity; `formula` needs an expression referencing existing fields; `decimal`/`currency` need a precision; `auto_number` needs prefix and start value.
- Workflow step: rule list with enable toggles and a builder row per rule — when (trigger) · if (condition groups, all/any) · then (ordered actions, each with its own fields).
- Permissions step: matrix of entities × actions with per-role toggles and the four presets; the key each cell will generate is visible on hover.
- Publish step: pre-flight findings (slug collisions, reserved slugs, missing permission role, views pointing at missing fields, rules referencing disabled actions, sample data still present), the snapshot diff since the last publish, and a typed confirmation when a destructive change is present.
- Generated list screen: shared admin table shell — search, saved views, column picker, filters, sort, pagination, bulk delete/export, per-row actions, inline edit where the view allows it. Generated form: sections, two-column layout, validation mirrored from field settings, relation pickers with type-ahead, media picker, and a required-field summary at the top.
- States: "This app has no entities yet — add the first one"; "No records yet" with `New record`; empty saved views; shared table skeleton while loading; error state with request id and retry — never a blank panel.
- Keyboard: `⌘↵` advance a wizard step, `⌘S` save draft, `n` new record in a generated list, `j`/`k` row cursor, `e` edit focused row, `/` focus search, `Esc` close inspector. The wizard is operable without a mouse.
- Mobile: single column with a sticky step header; entity tree collapses to a breadcrumb; inspector becomes a bottom sheet; generated lists show cards; forms stay single column with native date/time pickers.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/builder/apps` | App list with counters and status | `builder.apps.read` |
| POST | `/api/v1/builder/apps` | Create an app draft (name, slug, icon, colour) | `builder.apps.manage` |
| GET | `/api/v1/builder/apps/{id}` | Full schema: entities, fields, views, rules, permissions | `builder.apps.read` |
| PATCH | `/api/v1/builder/apps/{id}` | Update metadata, public-read toggle, nav position | `builder.apps.manage` |
| DELETE | `/api/v1/builder/apps/{id}` | Archive an app (records kept, nav entry removed) | `builder.apps.manage` |
| POST | `/api/v1/builder/apps/{id}/duplicate` | Copy a definition without records | `builder.apps.manage` |
| POST | `/api/v1/builder/apps/{id}/entities` | Add an entity | `builder.apps.manage` |
| PATCH / DELETE | `/api/v1/builder/apps/{id}/entities/{entity_id}` | Rename, describe, reorder or remove an entity (record guard applies) | `builder.apps.manage` |
| POST | `/api/v1/builder/apps/{id}/entities/{entity_id}/fields` | Add a field | `builder.apps.manage` |
| PATCH / DELETE | `/api/v1/builder/apps/{id}/entities/{entity_id}/fields/{field_id}` | Edit or remove a field | `builder.apps.manage` |
| GET / PUT | `/api/v1/builder/apps/{id}/views/{view_id}` | List or create/replace a view definition | `builder.apps.read` / `builder.apps.manage` |
| GET / PUT | `/api/v1/builder/apps/{id}/workflow-rules/{rule_id}` | List or create/replace a rule | `builder.apps.read` / `builder.apps.manage` |
| GET / PUT | `/api/v1/builder/apps/{id}/permissions` | Generated keys + role matrix; save the matrix | `builder.apps.read` / `builder.apps.publish` |
| POST | `/api/v1/builder/apps/{id}/validate` | Pre-flight; returns findings and blocks on `fail` | `builder.apps.read` |
| POST | `/api/v1/builder/apps/{id}/publish` | Publish: catalogue keys, nav entry, snapshot | `builder.apps.publish` |
| POST | `/api/v1/builder/apps/{id}/unpublish` | Remove the nav entry and generated routes | `builder.apps.publish` |
| GET / POST | `/api/v1/builder/apps/{id}/schema.json`, `/apps/import` | Export the definition; import as a new draft | `builder.apps.read` / `builder.apps.manage` |
| POST / DELETE | `/api/v1/builder/apps/{id}/entities/{entity_id}/sample-data` | Generate or remove labelled sample records (draft only) | `builder.apps.manage` |
| GET | `/api/v1/builder/apps/{id}/publishes` | Publish history with schema snapshots | `builder.apps.read` |
| GET | `/api/v1/apps/{app}/meta` | Generated app metadata (label, icon, entities, views) for the shell | `builder.apps.read` |
| GET / POST | `/api/v1/apps/{app}/{entity}` | Generated list (filters, sort, pagination, search) and create | `app.{app}.{entity}.read` / `.create` |
| GET / PATCH / DELETE | `/api/v1/apps/{app}/{entity}/{id}` | Generated read, update, delete (audited, soft delete by default) | `app.{app}.{entity}.read` / `.update` / `.delete` |
| POST | `/api/v1/apps/{app}/{entity}/export`, `/import` | CSV/JSON export of a view; CSV import with column mapping (REQ-031) | `app.{app}.{entity}.read` / `.update` |

Errors: `400` invalid key/type/settings, `403` permission miss (including a viewer writing a record), `404` unknown app/entity/record, `409` key collision or duplicate field key or a double-publish race, `422` publish blocked by a destructive change or a failed pre-flight finding.

### Data model

Migration: `database/migrations/0015_app_builder.sql` (next free number at build time).

- `built_apps` — `id uuid pk default gen_random_uuid()`, `organization_id uuid not null references organizations(id) on delete cascade`, `slug text not null`, `name text not null`, `icon text null`, `color text null`, `description text not null default ''`, `status text not null default 'draft'` (`draft|published|archived`), `schema_version int not null default 1`, `published_schema_version int null`, `public_read boolean not null default false`, `nav_position int not null default 0`, `created_by uuid null references users(id) on delete set null`, `created_at timestamptz not null default now()`, `updated_at timestamptz`, `published_at timestamptz null`; check `slug ~ '^[a-z][a-z0-9_]{1,39}$'` and not in the reserved list (`admin`, `api`, `apps`, `builder`, `system`, `settings`, `login`, `public`, `media`, `ai`); unique `(organization_id, slug)`.
- `built_entities` — `id uuid pk`, `app_id uuid not null references built_apps(id) on delete cascade`, `slug text not null`, `name text not null`, `singular_name text not null`, `description text not null default ''`, `icon text null`, `position int not null default 0`, `display_field_id uuid null`, `created_at timestamptz`, `updated_at timestamptz`; unique `(app_id, slug)`, index `(app_id, position)`.
- `built_fields` — `id uuid pk`, `entity_id uuid not null references built_entities(id) on delete cascade`, `slug text not null`, `label text not null`, `field_type text not null`, `required boolean not null default false`, `unique_value boolean not null default false`, `default_value jsonb null`, `config jsonb not null default '{}'`, `validation jsonb not null default '{}'`, `help_text text not null default ''`, `position int not null default 0`, `list_visible boolean not null default true`, `filterable boolean not null default false`, `searchable boolean not null default false`, `created_at timestamptz`, `updated_at timestamptz`; check `field_type in (text,long_text,number,decimal,currency,date,datetime,boolean,select,multi_select,user,relation,media,email,phone,url,json,auto_number,formula,state)`; unique `(entity_id, slug)`, index `(entity_id, position)`. `config` is validated against a per-type schema (options, relation target and kind, precision, prefix, formula expression) — never free text.
- `built_views` — `id uuid pk`, `app_id uuid not null`, `entity_id uuid null`, `name text not null`, `view_type text not null`, `config jsonb not null default '{}'`, `is_default boolean not null default false`, `created_by uuid null`, `created_at timestamptz`, `updated_at timestamptz`; check `view_type in (list,form,kanban,calendar,detail)`, index `(app_id, entity_id)`.
- `built_workflow_rules` — `id uuid pk`, `app_id uuid not null`, `entity_id uuid not null`, `name text not null`, `trigger text not null`, `trigger_config jsonb not null default '{}'`, `conditions jsonb not null default '{"all":[]}'`, `actions jsonb not null default '[]'`, `enabled boolean not null default true`, `position int not null default 0`, `created_by uuid null`, `created_at timestamptz`, `updated_at timestamptz`; index `(entity_id, position)`.
- `built_app_permissions` — `app_id uuid not null`, `entity_id uuid not null`, `action text not null`, `role_id uuid not null references roles(id) on delete cascade`, `created_at timestamptz`; primary key `(entity_id, action, role_id)`, check `action in (read,create,update,delete,manage)`. The matching catalogue keys are inserted inside the publish transaction, before the bindings, so a role can never reference a key that does not exist.
- `built_app_publishes` — `id uuid pk`, `app_id uuid not null references built_apps(id) on delete cascade`, `schema_version int not null`, `snapshot jsonb not null`, `findings jsonb not null default '[]'`, `published_by uuid null`, `notes text not null default ''`, `created_at timestamptz`; index `(app_id, schema_version desc)`.
- `built_sample_records` — `id bigint generated always as identity pk`, `entity_id uuid not null`, `record_id text not null`, `batch uuid not null`, `created_at timestamptz not null default now()`; this is the only record-tracking table here (which rows were generated, so they can be removed without touching real data). Record content lives in REQ-026's dynamic record tables.
- Publish is a single transaction: catalogue keys, permission bindings, snapshot row, nav entry and schema-version bump either all land or none do.

### Events

- **Emitted:** `builder.app.created`, `builder.app.schema.changed`, `builder.app.published`, `builder.app.unpublished`, `builder.app.archived`, `builder.sample_data.generated`, `builder.sample_data.cleared`, `builder.app.exported`.
- **Consumed:** `dynamic.record.created|updated|deleted` (REQ-026) drive the generated workflow rules; `permissions.changed` refreshes generated guards; `user.deleted` decides how an orphaned `user` field renders.
- Webhook relevance: `builder.app.published` and `builder.sample_data.*` matter to operators; per-rule webhook actions are configured by builders, so their payload shape is documented on the rule editor. No payload carries user secrets.
- Notification relevance: a publish that changes permissions notifies the affected roles through the REQ-021 router; a blocked publish returns its findings inline.

### Acceptance criteria

- [ ] `/apps` lists apps with real counters and shows the create-first empty state on a fresh instance.
- [ ] The wizard implements all seven steps in the brief's order and can be exited and resumed.
- [ ] The Asset Tracking example (Name, Serial Number, Employee, Department, Purchase Date, Status) is buildable without code or raw SQL.
- [ ] Field validation rejects a duplicate key, a reserved word, a `select` with no options and a `relation` with no target, with a readable message on the offending field.
- [ ] Deleting a field that holds records is blocked until the destructive change is confirmed.
- [ ] List, form, kanban, calendar and detail views can be created and previewed in the designer.
- [ ] Workflow rules save trigger + conditions + actions, and disabling one stops it firing.
- [ ] A rule action can call a webhook and require an approval; both are recorded.
- [ ] The permission matrix generates real keys; a viewer role gets `403` on a write endpoint.
- [ ] Publish is blocked while a pre-flight finding is `fail`, and the finding names the exact field.
- [ ] After publish the app appears in the admin navigation with its icon and label.
- [ ] Generated CRUD screens create, read, update and delete records with working validation.
- [ ] Generated endpoints respond at `/api/v1/apps/{app}/{entity}` with real data.
- [ ] Every record write produces an audit entry naming app, entity and record.
- [ ] Generated records are findable through global search by their display field.
- [ ] CSV round-trips through the generated screens; schema JSON round-trips through export/import.
- [ ] Sample data can be generated and cleared without touching user-entered rows.
- [ ] A second publish keeps existing records readable, and a public-read app serves its list unauthenticated while write endpoints still refuse anonymous callers.
- [ ] Mobile and keyboard behaviour match the spec; `pnpm typecheck`, `pnpm build`, `cargo test` and the browser walkthrough are green.

### QA plan

The walkthrough must build the brief's example end to end: create an app, add the `Asset` entity, add the six fields with the right types (`Status` as a `state` field), create a list view and a form view, add one workflow rule (on create → notify a role), set viewer/editor permissions, generate sample data, publish, then click into the generated list and detail screens, edit a record, run a filter and a sort, export the view, and confirm the audit entry. It must also check the denial path — signed in as a viewer, the create button is absent and the API returns `403` — and that one Turkish label example (for example `Demirbaş Adı`) renders correctly without breaking the column layout.

Visual check should see: a wizard rail with clear done/current/todo states, an inspector with no overlapping inputs, a generated list with aligned columns and legible `state` badges, a form whose required markers and inline errors are visible, the new nav entry with its icon, and dark/light parity.

### Slices

1. **Definition + wizard skeleton.** Migration, app/entity/field endpoints, `/apps` list, wizard steps 1–2 with the field designer and validation. *Done when:* a draft app with the Asset entity and all six fields can be built, reloaded and edited without code.
2. **Views + generated runtime.** View designer (list/form/kanban/calendar), publish-time nav entry, generated list/detail/create/edit screens driven by the definition and reading records from the dynamic data model. *Done when:* a published app shows a working list and form over real records.
3. **Workflow + permissions + audit + search.** Rule editor backed by the workflow engine, permission key generation and role matrix, audit entries on writes, search coverage. *Done when:* a viewer is refused a write, a rule fires on create, and the write appears in the audit list.
4. **Publish pipeline + lifecycle.** Pre-flight findings, snapshots and schema versioning, destructive-change confirmation, unpublish/archive, duplicate, schema export/import, sample data, CSV import/export. *Done when:* a second publish with a destructive change is blocked, then confirmed, and old records stay readable.

### Risks / notes

- Everything hinges on REQ-026. If the dynamic record layer is not in place this REQ starts at slice 1 and the runtime slice waits — a fake record store would poison the whole feature.
- Generated permission keys are real catalogue entries: insert them inside the publish transaction, never invent them at request time, and unpublish without leaving orphaned bindings in roles (historical audit references must stay valid).
- Slug generation is a security boundary: keys are matched against a strict whitelist and a reserved-word list, and no user-controlled string ever becomes a SQL identifier, a file path or a template to execute.
- The wizard must never become a decorative stepper: each step persists real rows, `Next` is blocked only by real validation, and abandoning it leaves an editable draft.
- Schema changes with live records are the hard case: prefer additive changes, offer rename over drop, require typed confirmation for anything destructive, and keep the previous snapshot in `built_app_publishes` for review.
- Generate the runtime screens from the same declarative config as the designer, or preview and production will drift and the preview will start lying about what publish produces.
