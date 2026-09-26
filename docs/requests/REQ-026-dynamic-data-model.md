# REQ-026 — Dynamic Data Model

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core (dynamic entity layer)
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

Users can create their own entities:

```text
Create Entity

Name:
Vehicle

Fields:
├── Plate
├── Brand
├── Model
├── Year
└── Employee
```

Omnion then creates automatically:

```text
Vehicle API
Vehicle Admin UI
Vehicle Permissions
Vehicle Search
Vehicle Audit
Vehicle Workflow Events
```

## Notes

- Foundation for the App Builder (REQ-025); must emit workflow events (REQ-016) and
  audit entries (REQ-039) for every generated entity from day one.

## Implementation spec

> Buildable contract. Entity store: new crate `crates/datamodel` (`omnion-datamodel`). HTTP surface: `apps/api/src/routes/datamodel.rs`. Panels under `/studio/data` in `apps/admin` (view modules in `apps/admin/features/datamodel/`). Per-entity CRUD is generated at runtime from the definition — no code generation step and no per-entity physical table in v0.

### Scope (in / out)

**In**

- Entity definitions (name, slug, description, icon, status) with an ordered field list.
- Field types v0: `text`, `long_text`, `number`, `decimal`, `boolean`, `date`, `datetime`, `select`, `multi_select`, `reference` (another entity), `user`, `media`, `json`.
- Field flags: `required`, `unique_value`, `indexed`, `default_value`, `help_text`, `position`.
- Generated per-entity CRUD API, panel list/form screens and per-entity permission keys.
- Audit entry (REQ-039) and event (REQ-016) for every record mutation; search projection for REQ-002.
- Definition lifecycle `draft` → `active` → `archived`, with archive leaving records readable.

**Out** — CSV/Excel import mapping (REQ-031), AI-assisted schema drafting (REQ-045), visual form/page layout (REQ-025, REQ-063), per-entity physical PostgreSQL tables, cascade rules on references, field-label translations (REQ-020), formula or computed fields.

### Screens (UI)

Routes live under the existing `AppShell`; the nav gains a "Data" section below "Pages".

| Route | Screen | Contents |
|---|---|---|
| `/studio/data` | Entity list | Table: Name, Slug, Fields, Records, Status, Updated. Filters: status, text. Row actions: Duplicate, Archive/Activate, Delete. Bulk: archive, delete. |
| `/studio/data/new` | Create entity | Name (required, 2–80 chars), Slug (auto from name, editable, `^[a-z][a-z0-9_]{0,62}$`, unique per organization, reserved words blocked), Description (≤280), Icon picker, Status. |
| `/studio/data/{slug}` | Detail shell | Tabs: Fields, Records, Permissions, Activity. |
| `/studio/data/{slug}/fields` | Field editor | Ordered rows: Label, Key, Type, Required, Unique, Indexed, Default. Drag handle to reorder, Add/Edit field sheet, Delete with a data-loss preview ("3 records hold a value"). |
| `/studio/data/{slug}/records` | Record table | Columns generated from fields with a persisted column picker; per-column sort; filter bar (field + operator + value); 50/page pagination; row click → drawer; bulk select → delete; link to Import/Export (REQ-031). |
| `/studio/data/{slug}/records/new`, `…/{recordId}` | Record form | One widget per type: text, textarea, number, decimal, toggle, date/datetime picker, select, multi-select, record picker, user picker, media picker, JSON editor. Save / Save & new / Discard. |
| `/studio/data/{slug}/permissions` | Permissions tab | Read-only list of the entity's four permission keys with a deep link into the IAM roles screen (REQ-006). |
| `/studio/data/{slug}/activity` | Activity tab | Audit feed: actor, action, record, timestamp, change diff link. |

- Empty: "No entities yet — create your first entity"; records tab "No records yet — add the first record"; fields tab "Add your first field".
- Loading: skeleton rows (`components/loading-table.tsx`); saving buttons spinner + disabled; optimistic create with rollback on error.
- Error: 403 "You do not have permission to view this entity", 404 for an unknown slug, mutation failures as a toast carrying the API message, load failure with Retry.
- Keyboard: `/` focus list filter, `j`/`k` row cursor, `Enter` open record, `Esc` close drawer/sheet, `⌘Enter` save record, `⌘⇧N` new entity; `⌘K` (REQ-032) resolves entity names.
- Mobile: record table becomes stacked record cards, form is single column, filters open in a sheet, field order edited through a numeric position input instead of dragging.

### API

All routes under `/api/v1`, guarded by `guards::require` and scoped to the caller's organization (docs/07-IAM.md §6).

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/entities` | List entity definitions | `entities.read` |
| POST | `/entities` | Create a definition (draft) | `entities.manage` |
| GET | `/entities/{slug}` | Definition + fields | `entities.read` |
| PUT | `/entities/{slug}` | Replace definition and field list | `entities.manage` |
| POST | `/entities/{slug}/publish` | Publish; register per-entity permission keys | `entities.publish` |
| POST | `/entities/{slug}/archive` | Archive (records become read-only) | `entities.manage` |
| DELETE | `/entities/{slug}` | Delete definition and its records | `entities.manage` |
| GET | `/entities/{slug}/records` | List records (`filter`, `sort`, `page`, `per_page`) | `entities.{slug}.read` |
| POST | `/entities/{slug}/records` | Create a record | `entities.{slug}.create` |
| GET | `/entities/{slug}/records/{id}` | One record | `entities.{slug}.read` |
| PATCH | `/entities/{slug}/records/{id}` | Update a record | `entities.{slug}.update` |
| DELETE | `/entities/{slug}/records/{id}` | Soft-delete a record | `entities.{slug}.delete` |
| POST | `/entities/{slug}/records/bulk-delete` | Delete the selected records | `entities.{slug}.delete` |
| GET | `/entities/{slug}/records/{id}/activity` | Audit entries of one record | `audit.read` |

Validation in the service layer, mirrored by the form: required, type coercion, `select` option membership, `reference` target exists in the same organization, `unique_value` per (entity, field), key format and reserved words, reference cycles refused. Record payloads are validated against the *published* definition and unlisted keys are rejected.

### Data model

`database/migrations/0011_dynamic_data_model.sql` (numeric prefix = next free slot at tick time). Additive-only, so no DDL runs at runtime.

- `entities` — `id uuid pk default gen_random_uuid()`, `organization_id uuid not null → organizations on delete cascade`, `name text not null`, `slug text not null`, `description text`, `icon text`, `status text not null default 'draft'`, `record_count integer not null default 0`, `created_by uuid → users on delete set null`, `created_at timestamptz not null default now()`, `updated_at timestamptz not null default now()`. Checks: slug format, status in `('draft','active','archived')`; `unique (organization_id, slug)`.
- `entity_fields` — `id uuid pk`, `entity_id uuid not null → entities on delete cascade`, `key text not null`, `label text not null`, `field_type text not null`, `required boolean not null default false`, `unique_value boolean not null default false`, `indexed boolean not null default true`, `options jsonb`, `default_value jsonb`, `help_text text`, `position integer not null default 0`. Checks: key format `^[a-z][a-z0-9_]{0,62}$`, known `field_type`; `unique (entity_id, key)`.
- `entity_records` — `id uuid pk`, `entity_id uuid not null → entities on delete cascade`, `data jsonb not null default '{}'::jsonb`, `created_by uuid → users`, `updated_by uuid → users`, `created_at`, `updated_at`, `deleted_at timestamptz` (soft delete). Indexes: `entity_records_entity_idx (entity_id, created_at desc)`, GIN `entity_records_data_idx (data jsonb_path_ops)`.
- `entity_record_values` — projection of indexed values, written in the same transaction; basis of search and of unique enforcement: `id uuid pk`, `record_id uuid not null → entity_records on delete cascade`, `entity_id uuid not null`, `field_key text not null`, `value_text text`, `is_unique boolean not null default false`. Indexes: `(entity_id, field_key, value_text)` and a partial unique index `(entity_id, field_key, value_text) where is_unique`.

### Events

Emitted on the platform bus (REQ-016); payload carries `entity_slug`, `record_id`, `changed_fields`, `organization_id`.

- `entity.definition.published`, `entity.definition.archived`
- `entity.record.created`, `entity.record.updated`, `entity.record.deleted`

Consumed: none in v0 — the search projection and the audit entry are written in the mutation's own transaction, so no consumer is needed for correctness.

Webhook relevance: high. `entity.record.*` is a first-class trigger for automation rules (REQ-003) and for external subscribers; a delivery is queued only against endpoints whose key resolves to a service account holding `entities.{slug}.read`, checked at queue time.

### Acceptance criteria

- [ ] Creating an entity persists a draft definition whose slug is validated and unique per organization.
- [ ] Adding, reordering and deleting fields persists and immediately changes the generated record form.
- [ ] Publishing registers `entities.{slug}.read|create|update|delete` and a role can be granted them in the IAM screen.
- [ ] A role without `entities.{slug}.read` receives 403 on the record list and sees the forbidden panel state.
- [ ] All 13 field types round-trip: create → reload → edit → value unchanged.
- [ ] Required-field and type errors render inline next to the field and block the save.
- [ ] A `unique_value` field refuses the second identical value with a field-level message, never a raw constraint name.
- [ ] The record table supports column selection, sorting, per-field filtering and 50-row pagination.
- [ ] Bulk delete removes exactly the selected rows and reports the deleted count.
- [ ] Every mutation writes an audit entry visible in `/studio/data/{slug}/activity` and in the record activity endpoint.
- [ ] Every mutation emits its `entity.record.*` event and the event appears in the events feed.
- [ ] Deleted records disappear from panel and API list while staying recoverable in the database.
- [ ] Archiving an entity blocks record writes while reads keep working.
- [ ] Deleting an entity with records requires explicit confirmation and removes records plus projections atomically.
- [ ] `cargo test -p omnion-datamodel` covers definition validation, unique enforcement, filtering and soft delete.
- [ ] `cargo clippy --workspace` and `pnpm typecheck && pnpm build` are clean.
- [ ] The new screens appear in the QA walkthrough inventory and every visible control is exercised.
- [ ] Mobile 390×844 shows record cards and a working filter sheet.
- [ ] No placeholder or dead control ships on any `/studio/data` screen.

### QA plan

Extend the admin route list in the QA walkthrough harness (the "no untested screen" rule) with `/studio/data` and its children, then walk:

1. Empty state → create "Vehicle" with Plate (text, required, unique), Brand, Model, Year (number), Owner (user) → publish.
2. Add three records; attempt a duplicate plate and an empty required field (both must show inline errors); edit one record and reload to prove persistence.
3. Sort by Year, filter `Brand = Volvo`, change the column set, and page a seeded 60-row table.
4. Open the record drawer, save, check the Activity tab shows the change, then bulk-delete two rows and read back the count message.
5. Second pass with a limited role: the records screen must render the 403 state.
6. Mobile viewport pass: card layout, filter sheet, record form usability.

The visual check must see: a populated entity table with the five declared columns, the correct widget per field type, red inline error text beside the offending field, a legible empty state before creation, an activity feed with timestamps, no raw i18n keys, no clipped or overlapping labels, and no horizontal overflow at 390 px.

### Slices

1. **Definition + fields** — migration, definition service, definition API, entity list, create form, field editor. Done when an entity with five fields can be created, published and re-opened after a reload with validation errors proven.
2. **Records** — record service (validation, unique projection, soft delete), records API, generated table and form screens, bulk delete. Done when every field type round-trips and sort/filter/pagination work on a 60-row entity.
3. **Generated platform surface** — per-entity permission registration, audit + event emission, activity tab, archive semantics, restricted-role state, search projection. Done when a limited role is blocked, the audit feed lists mutations and the events feed shows `entity.record.created`.

### Risks / notes

- v0 keeps records in JSONB plus a derived value table instead of per-entity physical tables: indexed, queryable and free of runtime DDL. A later slice can add real tables behind the same API without touching the client.
- Field type changes on a live field need a compatibility matrix and a dry-run count of unconvertible rows; v0 permits a type change only while `record_count = 0`.
- Definition updates replace the whole field list, so a removal that would drop data must be refused unless the request carries an explicit confirmation flag.
- Per-entity permission keys grow with every entity; the roles screen must page by module prefix (`entities.{slug}.*`) or the matrix becomes unusable past ~20 entities.
- Slug rename breaks deep links and consumer filters — allowed only while the entity is a draft. Reserved slugs (existing panel routes and core tables) are rejected at validation time.
- `reference` has no cascade semantics in v0: deleting a referenced record is refused with a message naming the referencing records.
- Identifiers, comments and docs stay English; field labels are user data and may be Turkish.
