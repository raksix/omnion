# REQ-031 — Records / Data Import-Export Center

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core (data import/export) + admin UI
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

Import:

```text
Import
 ↓
Map Columns
 ↓
Validate
 ↓
Preview
 ↓
Import
```

Example — column mapping:

```text
Excel:
customer_name

Omnion:
customer.name
```

AI can match columns automatically.

Export formats:

```text
CSV
Excel
JSON
PDF
```

## Implementation spec

### Scope (in / out)

In:

- Bulk import into any registered entity (CRM customers, products, orders, helpdesk tickets, users, document metadata) from CSV and XLSX, through the five-step wizard: upload → map → validate → preview → import.
- AI-assisted column mapping (ai-hub) with a deterministic fallback chain: exact header match, normalized header match (case, separators, accents stripped), then suggestions. Every suggestion is user-editable and never auto-committed.
- A validation pass with per-row, per-field errors (type, required, max length, unique, reference-exists, enum membership) and a downloadable error report.
- Commit modes: `strict` (abort when any row is invalid), `skip_invalid`, `upsert` (match on the entity's declared natural key, update instead of insert).
- Export of any registered entity from list views and saved filters in CSV, XLSX, JSON and PDF.
- Job history with progress, counts and retention; presigned, single-use download links.
- Audit entries for every commit, cancellation, purge and download.

Out:

- Scheduled or recurring imports/exports (owned by the automation engine, REQ-003).
- Binary payload import — rows reference media by URL; the upload itself belongs to the File Manager (REQ-010).
- Cross-organization import; every write stays inside the caller's organization.
- External sync / CDC connectors and third-party data sources (Integration Hub, REQ-015).
- PDF export beyond the documented row cap (20 000); larger extracts use CSV or XLSX.
- A free-form staged-row spreadsheet editor — the wizard allows fixing a cell value, not reshaping the staging table.

### Screens (UI)

Admin routes (Next.js app router, feature dir `apps/admin/features/data-transfer/`):

```text
/data/transfer                 ← hub: Import jobs | Export jobs tabs
/data/transfer/import/new      ← 5-step wizard
/data/transfer/import/{id}     ← import job detail
/data/transfer/export/new      ← export builder
/data/transfer/export/{id}     ← export detail
```

- Layout: full width, two panes; the wizard uses a left step rail (`Upload · Map · Validate · Preview · Import`) and a sticky footer with `Back` / `Continue`; detail pages use the standard admin shell with a counts header (Total / Valid / Invalid / Imported) plus a tabbed body (`Rows`, `Errors`, `Details`).
- Hub table columns: Job, Direction, Entity, Format, Status, Rows (total / ok / failed), Created by, Started, Duration, Expires. Default sort `created_at desc`, page size 25.
- Hub filters: direction, entity, status, format, created-by, date range (default preset `last 7d`), job-id search. Filter state lives in the URL so a filtered view is shareable.
- Bulk actions on selected jobs: Cancel (queued/running), Retry (failed), Download (succeeded export), Delete (terminal import jobs, admin only). Selection shortcuts: `x` toggle, `a` select page, `Esc` clear.
- Wizard step 1 — Upload: drop zone plus file picker (`.csv`, `.xlsx`), delimiter/encoding auto-detected with an override select, and a raw preview of the first 20 rows.
- Wizard step 2 — Map: two-column mapper (source column with a sample value on the left, Omnion field select on the right), `Suggest with AI` button, unmapped required fields highlighted, `Skip column` option, header-row toggle.
- Wizard step 3 — Validate: progress bar with live counts, then a virtualized error table (Row, Field, Value, Problem, Suggested fix) and a `Download error report` link.
- Wizard step 4 — Preview: the first 50 valid rows rendered as they would be written; in `upsert` mode a per-row diff (`New` vs `Update`) with changed fields emphasized.
- Wizard step 5 — Import: commit-mode select, natural-key confirmation for `upsert`, typed confirmation dialog above 10 000 rows, then a progress screen with `Cancel`.
- Export builder: entity select → ordered column multi-select (drag to reorder) → filter builder reusing the entity's list filters → format → PDF-only sub-form (title, orientation, include-filter footnote). Validation: at least one column, valid filter set, PDF row estimate shown before submit with a warning over the cap.
- Empty states: hub — "Veri aktarımı yok. CSV veya Excel ile ilk içe aktarmayı başlatın." with the primary CTA; Errors tab of a clean job — a positive "Bu işte hata yok." panel, not failure styling.
- Loading: skeleton table rows on the hub, skeleton stepper between wizard steps, progress with counts during validate/commit. Errors: inline banner with the request id and a retry action; terminal job failures show the summary plus trace id in the Details tab — never a raw stack trace.
- Keyboard: `Ctrl+K` palette (REQ-032), `g t` transfer hub, `i` new import, `e` new export, `Enter` advance when the step is valid, `Alt+←/→` step back/forward, `?` shortcut sheet.
- Mobile: the step rail collapses to a horizontal progress bar, tables become stacked cards (Job, Entity, Status first), bulk selection moves behind a `Select` mode toggle, drag-and-drop falls back to the native file picker.

### API

| Method | Path | Purpose | Permission |
| --- | --- | --- | --- |
| GET | `/api/v1/data/entities` | Registry of importable/exportable entity types | `data.transfer.read` |
| GET | `/api/v1/data/entities/{entity}/fields` | Field metadata (type, required, unique, enum, reference) | `data.transfer.read` |
| POST | `/api/v1/import-jobs` | Create a job from an uploaded file (entity, format, header row) | `data.import.create` |
| GET | `/api/v1/import-jobs` | List import jobs with filters | `data.transfer.read` |
| GET | `/api/v1/import-jobs/{id}` | Job detail with step state and counts | `data.transfer.read` |
| POST | `/api/v1/import-jobs/{id}/mapping` | Save the column mapping | `data.import.create` |
| POST | `/api/v1/import-jobs/{id}/mapping/suggest` | AI plus heuristic column suggestions | `data.import.create` |
| POST | `/api/v1/import-jobs/{id}/validate` | Run the validation pass | `data.import.create` |
| POST | `/api/v1/import-jobs/{id}/commit` | Execute the import (mode: strict / skip_invalid / upsert) | `data.import.create` |
| POST | `/api/v1/import-jobs/{id}/cancel` | Cancel a queued or running job | `data.import.create` |
| GET | `/api/v1/import-jobs/{id}/rows` | Paged staged rows (status and field filters) | `data.transfer.read` |
| GET | `/api/v1/import-jobs/{id}/report.csv` | Error report download | `data.transfer.read` |
| DELETE | `/api/v1/import-jobs/{id}` | Purge a terminal job and its staged rows | `data.import.delete` |
| POST | `/api/v1/export-jobs` | Create an export (entity, columns, filters, format) | `data.export.create` |
| GET | `/api/v1/export-jobs` | List exports | `data.transfer.read` |
| GET | `/api/v1/export-jobs/{id}` | Export detail (counts, estimate, expiry) | `data.transfer.read` |
| GET | `/api/v1/export-jobs/{id}/download` | Single-use presigned download URL | `data.export.read` |
| POST | `/api/v1/export-jobs/{id}/cancel` | Cancel a queued or running export | `data.export.create` |

Conventions: organization scope comes from the session; `Idempotency-Key` is honoured on both job-creation endpoints; list endpoints are cursor-paginated with `limit` capped at 100.

### Data model

Migration `database/migrations/0011_import_export.sql`.

`import_jobs`

| Column | Type | Notes |
| --- | --- | --- |
| `id` | `uuid pk` | |
| `organization_id` | `uuid not null` | fk `organizations` |
| `entity` | `text not null` | registry key, e.g. `crm.customer` |
| `source_filename` | `text not null` | display only |
| `source_format` | `text not null` | check in (`csv`,`xlsx`) |
| `source_object_key` | `text not null` | staged upload, removed on purge |
| `delimiter` / `encoding` | `text` | detected values, overridable |
| `has_header` | `boolean not null default true` | |
| `row_count` | `integer` | parsed rows |
| `mapping` | `jsonb` | `{source_column: omnion_field}` |
| `commit_mode` | `text` | check in (`strict`,`skip_invalid`,`upsert`) |
| `status` | `text not null` | check in (`uploaded`,`mapping`,`validating`,`ready`,`running`,`succeeded`,`partial`,`failed`,`cancelled`) |
| `valid_rows` / `invalid_rows` / `imported_rows` / `updated_rows` / `skipped_rows` | `integer not null default 0` | always sum to `row_count` in terminal states |
| `created_by` | `uuid not null` | fk `users` |
| `created_at` / `started_at` / `finished_at` | `timestamptz` | |
| `error` | `text` | terminal failure summary |

Indexes: `(organization_id, created_at desc)`, `(organization_id, status)`.

`import_job_rows`

| Column | Type | Notes |
| --- | --- | --- |
| `id` | `bigserial pk` | |
| `job_id` | `uuid not null` | fk `import_jobs` on delete cascade |
| `row_number` | `integer not null` | 1-based source row |
| `raw` | `jsonb not null` | source values |
| `mapped` | `jsonb` | after mapping |
| `errors` | `jsonb` | `[{field, code, message}]` |
| `status` | `text not null` | check in (`pending`,`valid`,`invalid`,`imported`,`updated`,`skipped`,`failed`) |
| `target_id` | `text` | id of the written record |

Indexes: unique `(job_id, row_number)`, `(job_id, status)`.

`export_jobs`: `id uuid pk`, `organization_id uuid not null`, `entity text not null`, `format text not null` check in (`csv`,`xlsx`,`json`,`pdf`), `columns jsonb not null`, `filters jsonb`, `sort jsonb`, `row_estimate integer`, `row_count integer`, `status text not null` check in (`queued`,`running`,`succeeded`,`failed`,`expired`,`cancelled`), `object_key text`, `byte_size bigint`, `page_options jsonb`, `expires_at timestamptz`, `created_by uuid not null`, `created_at`/`started_at`/`finished_at timestamptz`, `error text`. Indexes: `(organization_id, created_at desc)`, `(status, expires_at)` for the sweeper.

Retention: staged rows and produced artifacts are purged 30 days after the terminal state by a sweeper in `apps/api`.

### Events

Emitted (organization-scoped, delivered to webhook subscribers):

- `data.import.started`, `data.import.completed` (`{job_id, entity, imported, updated, skipped, invalid}`), `data.import.failed`.
- `data.export.completed` (`{job_id, entity, format, rows, byte_size, expires_at}`), `data.export.failed`.

Consumed: `file.uploaded` (File Manager, REQ-010) so an uploaded file offers “Import this file” with the entity preselected. Webhook relevance: yes — subscribers can chain a workflow when an import finishes; payloads carry counts and ids only, never row data.

Audit: `data.import.commit`, `data.import.cancel`, `data.import.purge`, `data.export.download`, each with entity, job id, counts and request id.

### Acceptance criteria

- [ ] CSV and XLSX files up to the documented size cap parse with auto-detected delimiter/encoding, and the operator override takes effect.
- [ ] The wizard enforces the five steps in order; `Continue` stays disabled until the current step is valid.
- [ ] Mapping suggestions appear for headers that differ only by case or separators (`Customer Name` → `customer.name`).
- [ ] AI suggestions are proposals: nothing is written until the operator saves the mapping and starts the run.
- [ ] Validation reports per-row, per-field errors whose row numbers match the source file.
- [ ] `strict` mode aborts with zero writes when any row is invalid and the job ends `failed` with an accurate report.
- [ ] `skip_invalid` imports valid rows and marks the rest skipped; the five counters sum to `row_count`.
- [ ] `upsert` updates records on the natural key and never creates duplicates for the same key inside one file.
- [ ] An import above 10 000 rows requires typed confirmation and can be cancelled without unaccounted partial state.
- [ ] The error report downloads as CSV with one row per error plus a header row.
- [ ] The export builder refuses submission with zero columns or an invalid filter set.
- [ ] CSV, XLSX, JSON and PDF exports open in their target applications; JSON is a stable array of objects.
- [ ] A PDF export above the cap is refused with a message naming the cap and the alternative formats.
- [ ] Download URLs are presigned, single-use, expire (default 24h) and return a clear expired error afterwards.
- [ ] Import and export lists respect the organization boundary, and a read-only role cannot commit or cancel.
- [ ] Every commit, cancellation, purge and download appears in the audit log with actor, entity and counts.

### QA plan

Browser walkthrough (admin session):

1. Open `/data/transfer` with no jobs → empty state renders with the CTA; no console errors.
2. Start an import with the fixture `customers.csv` (`customer_name,email,phone` plus one bad email and one duplicate key) → step rail shows five steps; upload preview lists the first 20 rows.
3. Step Map → `Suggest with AI`; verify `customer_name` maps to the customer-name field, change one mapping manually, leave the step, return, and confirm the manual choice persists.
4. Step Validate → the error table lists both bad rows with field names; download the report and confirm identical row numbers.
5. Step Preview → valid rows render; switch between `New` and `Update` diff views in `upsert` mode.
6. Commit in `skip_invalid` → progress completes; header counts show Total = Valid + Invalid.
7. Repeat the same file in `upsert` → the second job reports updates and the customer list count does not grow.
8. Re-run in `strict` → job `failed`, customer list count unchanged.
9. Export the customer list as CSV and JSON from `/data/transfer/export/new`; download both; the JSON parses and matches the applied filters.
10. Request a PDF export with a filter → the footnote names the filter.
11. Keyboard pass: `Ctrl+K` opens the palette, `g t` navigates, `x`/`a` select rows, `Esc` clears.
12. Mobile viewport 390×844 → tables become cards, the file picker opens, the wizard advances, footer buttons stay reachable.

Visual check: status chips distinguish `succeeded` / `partial` / `failed` with icon plus label (never colour alone); count deltas are legible; the error table reads without horizontal scrolling at 1280px; a purged job shows no raw row data anywhere on screen.

### Slices

1. **Job models + CSV import (no AI).** Migration `0011`, the entity registry in the core crate, CSV parse/validate/commit for one entity (`crm.customer`), the list/read/commit API surface.
   Done: a CSV committed through the API writes real rows with accurate counts, and unit tests cover delimiter detection and per-field validation.
2. **Wizard UI + mapping + exports.** `/data/transfer` hub, wizard steps 1–5 against the API, error table and report download, export builder with CSV and JSON.
   Done: browser steps 1–9 pass and both formats download and open.
3. **AI mapping + XLSX + PDF.** `POST /import-jobs/{id}/mapping/suggest` on ai-hub with heuristic fallback, XLSX reader, PDF renderer with cap and filter footnote.
   Done: a messy-header fixture produces suggestions, and XLSX plus PDF fixtures import/export correctly.
4. **Lifecycle polish.** Upsert preview diffs, caps with typed confirmation, retention sweeper, bulk actions, `partial` handling, mobile layout.
   Done: the upsert, retention and mobile acceptance items are observed in the browser, and the audit log shows exactly one entry per mutating action.

### Risks / notes

- Large files: stream the parse inside the API process with hard row and byte caps; run commit in bounded batches so a cancel always leaves `partial` counters that sum to `row_count`.
- AI mapping cost and latency: suggestion runs on the header row only (never row data), has a short timeout, and falls back to heuristics — the wizard must never block on the model.
- Staged rows hold personal data: encrypt at rest, purge staged objects before the job row so no orphan is reachable, and keep the retention window documented (30 days).
- Upsert natural keys differ per entity; the registry must declare one, otherwise `upsert` is not offered in the UI at all.
- PDF export is a renderer, not a spreadsheet: the cap is enforced in the API as well as the wizard.
- CSV edge cases (quoted newlines, BOM, mixed line endings, semicolon delimiters) get fixture tests; the override select is the operator escape hatch.
- The entity registry is code-owned, not data-owned, in v1: adding an importable entity is a small pull request, not an admin setting.
- Writes go through the same service layer as the admin UI so automation rules (REQ-003) fire for imported rows exactly as for manual edits.
