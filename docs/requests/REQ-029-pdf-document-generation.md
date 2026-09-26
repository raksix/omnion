# REQ-029 — PDF / Document Generation

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core service (documents)
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

Any business object can produce documents:

```text
Quotation
Invoice
Contract
Report
Certificate
Employee Document
```

Templates:

```text
{{ customer.name }}
{{ invoice.number }}
{{ invoice.total }}
```

→ render to PDF.

## Notes

- Template engine mirrors the expression sandboxing requirements of docs/09 (§4); keep
  template evaluation sandboxed.

## Implementation spec

> Buildable contract. Renderer: new crate `crates/documents` (`omnion-documents`); HTTP surface `apps/api/src/routes/documents.rs`; panels under `/documents/templates` and `/documents/generated` in `apps/admin` (`features/documents/`). Templates are HTML plus a whitelisted `{{ }}` expression language, evaluated in a sandboxed worker that never holds database credentials — it receives a resolved data payload and returns bytes.

### Scope (in / out)

**In**

- Template lifecycle: draft → published version → archived, with an immutable version list, compare and restore (docs/05-VERSIONING.md §4–§7).
- Resource binding: a template declares the resource type it renders (`content.page`, `entity.{slug}`, `sales.quotation`, later module types) and receives the resolved record as its data payload.
- Expression language: `{{ path.to.value }}`, optional formatters `{{ invoice.total | currency("TRY") }}`, `{{ customer.name | upper }}`, `{{ invoice.date | date("d MMM yyyy") }}`; a small helper set (`if_empty`, `sum`, `count`) and no arbitrary function calls.
- Render targets: HTML preview in the panel and PDF through the render worker; the PDF is stored through the storage abstraction and appears as a `documents` row with a media object.
- Generation entry points: panel action on a record ("Generate document"), the documents library, the API (`resource_type` + `resource_id` or an explicit payload) and automation rules (REQ-003).
- Batch generation: a queue job renders one document per selected record and produces a downloadable ZIP summary.
- Starter templates for Quotation, Invoice, Contract, Certificate and Report, each seeded per organization on first use.
- Regenerate, delete and retention (default 180 days, configurable per organization).

**Out** — e-signature flows (REQ-030), a WYSIWYG page designer (v0 edits template source with a live preview), DOCX/XLSX output, OCR, digital stamping or vendor e-invoice compliance (module-localizations).

### Screens (UI)

| Route | Screen | Contents |
|---|---|---|
| `/documents/templates` | Template list | Table: Name, Resource type, Language, Version, Status, Updated. Filters: resource type, language, status, text. Row actions: Duplicate, Archive, Delete, Preview. Empty state "No templates yet — start from a starter template". |
| `/documents/templates/new` | Create | Name (required, 2–80), Resource type (select from bound types), Language, Starter template (blank or one of the five), Description. |
| `/documents/templates/{id}` | Editor | Split view: left the template source (HTML + expressions) with line numbers and a variable insert menu from the bound resource; right a live preview rendered from a selectable sample record. Validation of unknown variables, unbalanced braces and forbidden helpers appears inline under the editor. Save draft (⌘S), Publish, Preview device width toggle. |
| `/documents/templates/{id}/versions` | Versions | Version list (number, author, created, published); side-by-side diff; Restore into a new draft version. |
| `/documents/generated` | Generated library | Table: Name, Template, Source record, Size, Created, Status (queued, rendering, ready, failed). Filters: template, status, date range, text. Row actions: Download, Regenerate, Delete. Bulk: regenerate, delete. |
| Record drawer action | Generate document | Available on `/studio/data/{slug}/records/{id}` and content pages: pick a template, preview, Generate, then Download or open the library entry. |

- Empty: templates list, versions list and generated library each get their own empty state with a CTA; a template with no published version cannot be generated from, and the CTA says so.
- Loading: preview pane shows a skeleton page while rendering; generate buttons show a spinner; the library shows queued/rendering badges that update on refresh.
- Error: expression errors list the offending key and line; a failed render shows a readable reason in the library row and a Retry action; a missing template version renders an explicit "version withdrawn" page rather than a blank PDF.
- Keyboard: `⌘S` save draft, `⌘⇧P` publish, `⌘Enter` render preview, `Esc` close a dialog, Tab moves through the variable insert menu.
- Mobile: the editor is desktop-first (below `md` the source pane becomes read-only with a note and the preview stays usable); the library and the generate dialog work fully on a phone.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/document-templates` | List templates | `documents.templates.read` |
| POST | `/document-templates` | Create a template (draft) | `documents.templates.manage` |
| GET | `/document-templates/{id}` | Template with its current draft | `documents.templates.read` |
| PUT | `/document-templates/{id}` | Update the draft body and metadata | `documents.templates.manage` |
| POST | `/document-templates/{id}/publish` | Publish the draft as a new version | `documents.templates.publish` |
| POST | `/document-templates/{id}/preview` | Render a preview (HTML or PDF) from sample data | `documents.templates.read` |
| GET | `/document-templates/{id}/versions` | Version history | `documents.templates.read` |
| POST | `/document-templates/{id}/versions/{version}/restore` | Restore a version as a new draft | `documents.templates.manage` |
| DELETE | `/document-templates/{id}` | Archive or delete a template | `documents.templates.manage` |
| POST | `/documents/render` | Render one document from a resource or payload | `documents.generate` |
| POST | `/documents/render/batch` | Queue a batch render | `documents.generate` |
| GET | `/documents` | List generated documents | `documents.read` |
| GET | `/documents/{id}` | One document's metadata | `documents.read` |
| GET | `/documents/{id}/file` | Download the rendered bytes | `documents.read` |
| POST | `/documents/{id}/regenerate` | Re-render with the current template version | `documents.generate` |
| DELETE | `/documents/{id}` | Delete a document and its object | `documents.delete` |

Template preview and render both require the caller to be able to read the bound record; a render request for a resource the caller cannot read is refused with 403 before any data is loaded into the sandbox.

### Data model

`database/migrations/0014_document_generation.sql` (numeric prefix = next free slot at tick time). Additive-only.

- `document_templates` — `id uuid pk`, `organization_id uuid not null → organizations on delete cascade`, `name text not null`, `slug text not null`, `resource_type text not null`, `language text not null default 'tr'`, `description text`, `status text not null default 'draft'`, `current_version_id uuid`, `created_by uuid → users`, `created_at`, `updated_at`. Checks: slug format, `resource_type` format `^[a-z][a-z0-9_.]{0,62}$`, `status in ('draft','active','archived')`; `unique (organization_id, slug, language)`. The foreign key to `document_template_versions` is added after that table exists.
- `document_template_versions` — `id uuid pk`, `template_id uuid not null → document_templates on delete cascade`, `version_no integer not null`, `body text not null` (capped at 256 KB by a check on `length`), `sample_data jsonb`, `created_by uuid → users`, `created_at`, `published_at`; `unique (template_id, version_no)`, `version_no >= 1`.
- `documents` — `id uuid pk`, `organization_id uuid not null`, `template_id uuid → document_templates on delete set null`, `template_version_no integer`, `resource_type text`, `resource_id uuid`, `file_name text not null`, `mime text not null default 'application/pdf'`, `size_bytes bigint`, `media_id uuid → media on delete set null`, `status text not null default 'queued'`, `error text`, `checksum_sha256 text`, `created_by uuid → users`, `created_at`, `rendered_at`. Checks: `status in ('queued','rendering','ready','failed')`. Indexes: `documents_organization_created_idx (organization_id, created_at desc)`, `documents_resource_idx (resource_type, resource_id)`.

### Events

- Emitted: `document.template.published`, `document.rendered`, `document.render.failed`, `document.deleted`.
- Consumed: module events such as a quotation being finalized can trigger generation through automation rules; `esignature.request.completed` (REQ-030) can attach the signed copy as a new document version.
- Webhook relevance: medium — subscribers can react to `document.rendered` (for example to e-mail a customer copy); payloads carry document id, template id, resource reference and checksum, never the rendered bytes.

Sandbox rules: template evaluation runs in a separate worker process with a CPU-time and memory limit, a render timeout, no network access, no database credentials and a fixed helper whitelist; unknown variables fail the render instead of rendering blank.

### Acceptance criteria

- [ ] Starter templates for Quotation, Invoice, Contract, Certificate and Report can be seeded and rendered.
- [ ] A template can be created, edited, previewed and published; the published version is immutable.
- [ ] Editing after publish creates a new draft without touching the published version, and Restore produces a new version.
- [ ] The preview pane renders real sample data with no leftover `{{ }}` markers.
- [ ] Unknown variables, unbalanced braces and forbidden helpers produce inline errors that block saving.
- [ ] A PDF can be generated from a real record through the panel action, the API and the automation path.
- [ ] The generated PDF opens in a viewer, embeds the fonts needed for Turkish characters and is paginated sanely for a multi-line item table.
- [ ] Every generated document appears in `/documents/generated` with source, size, template version and status.
- [ ] Downloading a document returns bytes whose SHA-256 matches the stored checksum.
- [ ] A failed render stores its reason, shows it in the library row and can be retried.
- [ ] Batch generation over ten selected records produces ten documents and a downloadable ZIP.
- [ ] Regenerating uses the current published template version and records the new version number.
- [ ] A caller without `documents.generate` receives 403; a caller who cannot read the bound record is refused before rendering.
- [ ] Generated files live in object storage, are removed when the document row is deleted, and follow the retention setting.
- [ ] A template body over the size cap is rejected with a clear message.
- [ ] `cargo test -p omnion-documents` covers expression parsing, unknown-variable failure, formatter output and the render timeout.
- [ ] `cargo clippy --workspace` and `pnpm typecheck && pnpm build` are clean.
- [ ] All `/documents` routes appear in the QA walkthrough inventory and every visible control is clicked.
- [ ] Mobile 390 px: the library and the generate dialog are usable; the editor shows its read-only note.
- [ ] No template author can execute code, reach the network or read another organization's data from the sandbox.

### QA plan

Add `/documents/templates` and `/documents/generated` to the QA walkthrough route list, then walk:

1. Empty state → create a template from the Invoice starter → edit a heading and add `{{ invoice.total | currency("TRY") }}` → preview (a Turkish sample invoice must render with real values) → publish.
2. Trigger Generate document from a seeded record, then confirm the row appears in the generated library and downloads.
3. Introduce an unknown variable on purpose and confirm the inline error blocks the save; restore it and re-save.
4. Open Versions, compare the change, restore the earlier version and confirm a new draft version number.
5. Batch-generate over ten records and download the ZIP.
6. Mobile viewport pass: the library table and the generate dialog.

A focused probe should extract text from a freshly generated PDF and assert the sample customer name, the document number and the formatted total appear — this turns "it renders" into a number instead of an opinion. The visual check must see: a real rendered invoice with a table, totals and a logo placeholder, correct Turkish characters, no clipped table columns, no leftover template markers, and a preview pane aligned with the source pane.

### Slices

1. **Templates + versions + HTML preview** — migration, expression sandbox and renderer, template CRUD API, list/create/editor screens, version history with restore, HTML preview from a sample record. Done when a template renders real data in preview with errors blocking an invalid save.
2. **PDF pipeline + library** — render worker, storage write, `documents` rows, generate endpoint, generated library with download, regenerate, retention. Done when a real PDF downloads and its checksum matches, plus the text probe passes.
3. **Business entry points + batch** — record-level Generate document action, API payload mode for external callers, batch queue with ZIP, starter template seeding per organization, event emission. Done when a batch of ten renders end to end and the events appear in the feed.

### Risks / notes

- PDF engine choice is a slice-1 decision with explicit criteria: correct Turkish font embedding, no external network access, bounded memory per render and a package the deployment can ship. A heavyweight browser renderer is a fallback, not the default.
- Template expressions are the largest injection surface: the sandbox denies network, filesystem and database access, whitelists helpers, caps render time and memory, and returns a failure rather than a partially rendered document.
- Object storage growth: generated documents are the fastest-growing table on a busy tenant — retention, per-organization quotas and a cleanup job are part of slice 2, not later.
- Rendered bytes are immutable snapshots: regenerating creates a new document row so an issued invoice is never silently rewritten; the UI must say which template version a document came from.
- Sample data for previews is stored on the template version, so previews stay reproducible; it must be scrubbed of real personal data before it is saved.
- Batch generation needs per-organization concurrency limits, or one large batch starves interactive renders.
- Renders must be idempotent per (resource, template version, request key) so a retried request does not produce duplicate documents.
