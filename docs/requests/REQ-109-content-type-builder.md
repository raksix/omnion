# REQ-109 — Content Type Builder

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core (`crates/content`) + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

User-defined content models, not just pages.

- Content type registry with fields: text, rich text, number, date, boolean, image, file, relation, select, JSON.
- Built-in types shipped: Page, Blog Post, Product, Employee, Event — as editable starting points.
- Field options: required, unique, default, min/max, localisation per field.
- Relations between types (one-to-many, many-to-many) with referential integrity.
- Generated REST endpoints + panel screens + API docs per type; migrations generated safely.
- Validation rules and a preview of the JSON schema the API exposes.

## Implementation spec

### Scope (in / out)

**In**
- Content type registry: key, singular/plural label, description, icon, status (`draft`, `active`, `archived`), a monotonic `schema_version`, and a site or organization scope. A type becomes usable only when `active`.
- Field kinds: `text`, `rich_text`, `number`, `date`, `boolean`, `image`, `file`, `relation`, `select`, `json`. Every field carries key, label, help text, position, and a per-kind option set.
- Field options: `required`, `unique`, `default` value, `min`/`max` (number, date, text length), `pattern` (text), allowed options with labels (select), allowed media kinds and size cap (image, file), and `localized` (per-field opt-in; unlocalized fields fall back to the base locale).
- Built-in types seeded on first migration and materialised as ordinary editable types: Page, Blog Post, Product, Employee, Event. The seeded `page` type keeps the existing `pages` behaviour (slug, revisions, translations) as its backing store; other built-ins use the generic entry store.
- Relations between types: `one_to_many`, `many_to_one`, `many_to_many`, with a key, label, target type, and an integrity rule (`restrict`, `cascade`, `set_null`). Integrity is enforced on delete and on save, never by the database alone.
- Generated REST surface per active type under `/api/v1/content/types/{key}/entries` with the same verbs as pages, plus a per-type JSON Schema document at `/api/v1/content/types/{key}/schema` that the API itself validates against.
- Generated panel screens per type: list with filters and search, create/edit form driven by the field definitions, and a read-only view. No per-type hand-written code.
- Safe migration generation: additive changes (new optional field, new type, widened min/max, new option) apply in one step; destructive changes (field removal, type change, new required field) are refused until the operator confirms a data-handling plan (map, backfill or discard) and the engine shows the affected row count.
- Validation errors are field-keyed and returned in one payload; the same validation engine re-validates on read of legacy rows so a narrowed rule never silently hides broken data.
- Localisation per field reuses the existing translation storage (`REVISION_RESOURCE` pattern) rather than a parallel table.

**Out**
- Cross-site relations and relations to arbitrary non-content tables; a relation target must be an active type in the same site.
- Custom SQL, joins, computed/aggregate fields, and user-written validation code (formulas are a later REQ at most).
- GraphQL and SDK generation (REQ-130), CLI generators (REQ-131), plugin-provided field widgets (REQ-121).
- Replacing the `pages` table: pages keep their dedicated lifecycle, revisions and translations; the builder generalises everything else and describes `page` so the panel can show it uniformly.
- Entry-level permissions beyond the type: the builder emits standard `content.types.*` permissions, not per-field grants.

### Screens (UI)

| Route | Screen |
|---|---|
| `/content-types` | Type registry list |
| `/content-types/new` | Create type |
| `/content-types/<key>` | Type detail: fields, relations, entries, schema, danger zone |
| `/content-types/<key>/edit` | Field builder |
| `/content-types/<key>/relations` | Relation editor |
| `/content-types/<key>/schema` | JSON Schema and sample payload preview |
| `/content/types/<key>` | Generated entry list for any active type |
| `/content/types/<key>/<id>` | Generated entry editor |

- **Registry list.** Columns: Label, Key, Kind (built-in / custom), Fields, Entries (live count), Status, Updated. Filters by status and kind, text search on key and label. Row actions: Open, Duplicate, Archive. `New type` opens a small dialog first (key, labels, icon) and hands off to the field builder; the key is immutable after creation and validated as lowercase snake case.
- **Field builder.** Left rail lists fields in order with kind icons; canvas shows one row per field with inline key, label, required badge and unique badge; right inspector edits the selected field. Kind picker is a grid with a one-line description per kind. Options render per kind (number: min/max/step, text: min/max/pattern, select: options table with add/remove/reorder, media: kinds and size cap, relation: target type and cardinality, general: default, localized toggle, required, unique). Drag to reorder; `Add field` appends; `Delete` marks a destructive change and warns while it is pending. A sticky bar shows `Draft changes: n` with `Save`, `Preview impact` and `Apply`.
- **Impact preview.** `Preview impact` shows a change list where each line is one change: additive (green), destructive (red) with the affected entry count, and the data-handling choice for each destructive line (map to another field, backfill a default, discard values). `Apply` is enabled only when every destructive line has a choice.
- **Relations tab.** Table of relations with Key, Label, From type, To type, Cardinality, Integrity; create/edit drawer validates that the target type is active and warns that `cascade` deletes dependent entries (with the current dependent count shown).
- **Schema tab.** Read-only JSON Schema (draft 2020-12 compatible subset) with copy button, plus a sample request and response body generated from the current fields, a `Validate a payload` box that runs the real validator, and a list of API routes the type exposes with their methods.
- **Generated entry screens.** List columns come from the type's fields (first text field as title, date field formatted in the site timezone, boolean as badge, media as thumbnail, relation as a count linking to the target type). Filters are generated from `select`, `boolean` and `date` fields only. The editor groups fields into a single column on mobile, uses the media picker for image and file fields, and a searchable picker for relations. Draft/published states appear only if the type opted into the editorial workflow (REQ-110).
- **States and keys.** Empty registry shows a starter panel with the five built-in types and a `Start from a built-in` shortcut. Keys: `g t` registry, `g e` entries of the open type, `n` new field, `j`/`k` rows, `Esc` closes drawers. Every list has skeleton loading, a retryable error strip and a real empty state naming the create action.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET · POST | `/api/v1/content/types` | List types · create a type | `content.types.read` · `content.types.manage` |
| GET · PUT · DELETE | `/api/v1/content/types/{key}` | Read · update labels and status · archive | `content.types.read` · `content.types.manage` |
| POST | `/api/v1/content/types/{key}/fields` | Add a field | `content.types.manage` |
| PUT · DELETE | `/api/v1/content/types/{key}/fields/{field_key}` | Edit · remove a field (destructive, needs a data plan) | `content.types.manage` |
| POST | `/api/v1/content/types/{key}/fields/reorder` | Persist field order | `content.types.manage` |
| GET · POST · DELETE | `/api/v1/content/types/{key}/relations` | List · create · delete relations | `content.types.read` · `content.types.manage` |
| POST | `/api/v1/content/types/{key}/changes/preview` | Impact preview for pending changes | `content.types.manage` |
| POST | `/api/v1/content/types/{key}/changes/apply` | Apply pending changes as one schema version | `content.types.manage` |
| GET | `/api/v1/content/types/{key}/schema` | JSON Schema plus generated route list | `content.types.read` |
| POST | `/api/v1/content/types/{key}/schema/validate` | Validate a payload against the live schema | `content.types.read` |
| GET · POST | `/api/v1/content/types/{key}/entries` | List (filters, search, cursor) · create an entry | `content.types.entries.read` · `content.types.entries.write` |
| GET · PUT · DELETE | `/api/v1/content/types/{key}/entries/{id}` | Read · update · delete an entry | `content.types.entries.read` · `content.types.entries.write` · `content.types.entries.delete` |
| GET | `/api/v1/sites/{site_id}/content/types` | Types enabled for one site (switcher scope) | `content.types.read` |

Entry routes resolve the site from the entry, apply the type's validation before any write, enforce relation integrity inside the same transaction, and return `409` with a machine-readable `code` for integrity failures (`relation_in_use`, `unique_violation`, `required_field_missing`).

### Data model

Migration: `0116_content_types.sql` (next free number in the 0100+ band; append-only ledger — if taken, shift up and note it in the file).

```sql
content_types (id uuid pk, organization_id uuid, site_id uuid null -> sites /* null = org-wide */, key text, label text,
  label_plural text, description text, icon text, kind text in ('builtin','custom') default 'custom',
  status text in ('draft','active','archived') default 'draft', schema_version int default 1,
  backing text in ('generic','pages') default 'generic', created_by uuid null -> users, created_at/updated_at)
  unique (coalesce(site_id, organization_id), key)
content_type_fields (id uuid pk, type_id uuid -> content_types on delete cascade, key text, label text, help_text text,
  field_kind text in ('text','rich_text','number','date','boolean','image','file','relation','select','json'),
  position int, required bool default false, unique_field bool default false, localized bool default false,
  default_value jsonb, rules jsonb default '{}' /* min, max, pattern, options, media kinds, size cap */,
  created_at/updated_at)  unique (type_id, key)  index (type_id, position)
content_type_relations (id uuid pk, organization_id uuid, site_id uuid, key text, label text, from_type_id uuid -> content_types,
  to_type_id uuid -> content_types, cardinality text in ('one_to_many','many_to_one','many_to_many'),
  integrity text in ('restrict','cascade','set_null') default 'restrict', created_at/updated_at)
  unique (site_id, key)  index (from_type_id), (to_type_id)
content_entries (id uuid pk, type_id uuid -> content_types on delete restrict, site_id uuid, locale text default 'en',
  data jsonb not null default '{}', status text in ('draft','published','archived') default 'draft',
  created_by uuid null -> users, updated_by uuid null -> users, created_at/updated_at, published_at timestamptz null)
  index (type_id, site_id, status, updated_at desc)  gin (data jsonb_path_ops)
content_entry_relations (entry_id uuid -> content_entries on delete cascade, field_key text, target_entry_id uuid -> content_entries,
  position int default 0, primary key (entry_id, field_key, target_entry_id))
  index (target_entry_id) /* integrity checks scan this direction */
content_type_changes (id uuid pk, type_id uuid, from_version int, to_version int, plan jsonb, applied_by uuid,
  applied_at timestamptz, entries_affected int)
page_type_extensions (type_id uuid pk -> content_types, page_type text)  -- binds a type to pages.page_type
```

Notes. `unique_field` is enforced by a generated partial unique expression index per field (`unique (type_id, (data->>'<key>')) where data ? '<key>'`); the applier creates and drops these as fields change. Media references are stored as media ids inside `data` and re-checked on read, so a deleted asset degrades to a placeholder rather than breaking the entry. Field kinds `image` and `file` never store URLs, only ids. Archiving a type is refused while entries exist, unless entries are archived in the same confirmation. The migration seeds the five built-in types with their field sets as real rows, so they are editable starting points and not hidden code.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `content.type.created` · `.updated` · `.archived` | Type lifecycle | `type_key`, `schema_version`, `actor_user_id` |
| `content.type.changes.applied` | A schema change set is applied | `type_key`, `from_version`, `to_version`, `destructive_count`, `entries_affected` |
| `content.entry.created` · `.updated` · `.deleted` | Entry writes on generated types | `type_key`, `entry_id`, `site_id`, `status` |
| `content.entry.relation_broken` | An integrity rule fired (`restrict` blocked a delete) | `type_key`, `entry_id`, `field_key`, `code` |

Consumed: `media.deleted` (drops the media id from entries the next time they are written, and shows a placeholder in the meantime), `content.pages.updated` (keeps the `page` type's entry list badge honest for pages). Webhook relevance: `content.type.changes.applied` is the one external systems should watch — it is the signal that a schema version moved and cached client schemas must refresh; payloads carry keys and counts, never entry bodies.

### Acceptance criteria

- [ ] All ten field kinds render, save, reload and validate; the number field rejects a five-character string, the date field rejects an impossible date, and the select field rejects a value outside its option list.
- [ ] Creating a type with key `author_bio` and one text field makes `/api/v1/content/types/author_bio/entries` serve a create and a list request within one session, with no redeploy.
- [ ] The five built-in types appear after migration, are editable (a label change persists), and the `page` type still drives the existing pages screens unchanged.
- [ ] A `unique` text field rejects the second entry with the same value and returns `409` with `code = unique_violation`; the first entry stays saved.
- [ ] A relation with `many_to_one` shows a picker on the entry form, stores the target id, and the reverse `one_to_many` view lists the same rows.
- [ ] Deleting a target entry that is referenced under `restrict` fails with `relation_in_use` and a link to the referencing entries; the same delete succeeds under `cascade` and the dependent rows are gone; under `set_null` the field is cleared and the row stays.
- [ ] Removing a field shows the affected entry count in the impact preview; `Apply` stays disabled until a data-handling choice is made; applying with `discard` removes the values and `map` moves them to the chosen field.
- [ ] Adding a required field with a backfill default applies in one step and reports the number of entries backfilled.
- [ ] A field marked `localized` accepts a value per locale and falls back to the base locale when a translation is missing; an unlocalized field ignores locale input.
- [ ] The schema tab's `Validate a payload` runs the real validator: a valid payload passes and an invalid one lists field-keyed errors, matching what the API returns for the same body.
- [ ] The generated entry list honours search, one `select` filter, one `boolean` filter and a date range, and the result count matches the API response.
- [ ] Archiving a type with entries is refused with a message naming the count; archiving an empty type hides it from the site's type list.
- [ ] A snapshot of a second schema is never applied: two tabs editing the same type both call `Apply`, the second fails with a stale-version conflict and the panel offers `Reload`.
- [ ] Every new screen renders at 390 px without horizontal scroll and the field builder is fully operable by keyboard.
- [ ] The walkthrough reports zero high findings for this wave and the generated route list on the schema tab matches the routes actually serving.

### QA plan

The walkthrough visits `/content-types` (create `author_bio` from scratch, duplicate `event`), `/content-types/event/edit` (reorder fields, add an optional field, add a required field with a backfill), `/content-types/event/relations` (create a `many_to_many` to `employee`, then exercise `restrict` and `cascade` deletes), `/content-types/event/schema` (validate a good and a bad payload, copy the schema), and `/content/types/event` (create two entries, filter by a select field and a date range, edit and delete one). Then the operations check runs the external API with a token: create, list with filters, update, delete, and confirm the `409` codes for unique and integrity failures. Visual check: the field builder shows real reorderable rows with kind icons, the inspector switches options per kind, the schema tab shows real JSON, and entry thumbnails load from the media library.

### Slices

1. **Registry and fields.** Migration `0116_content_types.sql` with the type, field, relation and change tables plus the five seeded built-ins; registry list, field builder with all ten kinds and the inspector, order persistence, archive guard. *Done when:* acceptance 1–2 and 12 pass and `/content-types` is in the walkthrough inventory.
2. **Entries and generated surface.** Generic entry store, generated list and editor screens, per-type REST routes with validation and field-keyed errors, JSON Schema document and the live `Validate a payload` box. *Done when:* acceptance 11, 14–15 pass and an entry round-trips through the external API.
3. **Relations and safe changes.** Relation editor with the three cardinalities and three integrity rules, integrity enforcement on save and delete, impact preview, change application with data-handling plans, version counter and events. *Done when:* acceptance 5–10 and 13 pass, the destructive-change walkthrough is recorded, and the QA report lists zero high findings for the wave.

### Risks / notes

- The generic entry store must never become a second pages implementation. Pages keep revisions, translations, previews and workflow; the `page` type is a descriptor over them. Any change that routes page writes through `content_entries` is a regression to reject in review.
- Destructive schema changes are the highest-risk action in this REQ. Impact preview must count rows with a real query, not an estimate, and the applied change must be one transaction that writes `content_type_changes` first so a crash mid-apply leaves an auditable record.
- Unique enforcement via expression indexes is per field and per type; dropping a field must drop its index in the same transaction, otherwise a stale index silently rejects valid writes.
- `cascade` is convenient and dangerous with `many_to_many`: the confirmation names the dependent entries, and bulk deletes over generated types go through the same integrity path as single deletes.
- The schema response is a compatibility surface. Additive changes only for minor clients; a removed field is a breaking change that must bump `schema_version` and emit the event so integrations can refresh.
- Rich text and JSON fields store validated JSON, never HTML strings with script content; rendering sanitises, and the JSON Schema preview must not imply the schema is a security boundary.
- Seeded built-in types are example content, not fixtures: tests create their own types with `test_` prefixes and clean them up, so seeding stays idempotent.
