# REQ-094 — Workflow Templates Gallery

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** admin + `crates/workflows`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Starting from something that works.

- Template gallery: categories, search, preview of the graph, one-click import.
- Curated starter templates (invoice reminders, lead routing, content publishing, backup, AI triage).
- Import/export single workflows and whole folders as JSON.
- Community submission path (feeds the marketplace, REQ-048).
- Template versioning so an imported workflow keeps working after a template update.

## Implementation spec

### Scope (in / out)

**In**

- **Catalogue and gallery** — templates are real workflow definitions plus metadata: slug, name, summary, long description, category (Operations, Sales & CRM, Content, Publishing, Data & Reporting, IT & Backup, AI), tags, difficulty, node count, required capabilities (installed modules, permissions, credential kinds, variable keys, outbound hosts), the trigger kind, and a version number. The gallery browses, searches and filters the catalogue, shows a read-only graph preview rendered by the builder's preview renderer (REQ-004), and marks templates whose requirements the current workspace cannot satisfy ("needs the helpdesk module", "needs a credential of kind SMTP").
- **Curated starters** — an in-repo set that ships with the platform and is seeded on boot when missing: invoice reminders, lead routing, content publishing, backup and retention, AI triage, plus a scheduled operations digest and a failure-notification starter. Each is a working definition that runs green in the QA stack after its own credentials are filled.
- **One-click import** — imports into a chosen workspace and folder, creates the workflow **paused**, opens the editor with a setup checklist (fill variables, connect credentials, allow outbound hosts, confirm schedule timezone), and records `template_id` and `template_version` on the workflow so its origin is never lost. Import validates the definition first and refuses with a readable list instead of importing something half-broken.
- **Single workflow import/export as JSON** — a versioned bundle format: envelope schema version, workflow definition, folder path, variable and credential *references* by key and kind, capability list, pinned data excluded by default, webhook trigger tokens re-generated on import, and no secret material ever. Import maps references interactively (choose an existing credential, create a variable, rename a conflicting workflow) and reports per item.
- **Folder import/export** — a folder bundle containing several workflows plus their folder path, producing a conflict report per workflow (create, skip, rename) and one import report file listing every outcome.
- **Community submission path** — an exported bundle can be submitted for review. Submission automates a scan (secret-like strings, hosts outside the platform allow-list, disabled or empty nodes, oversized payload nodes, external URLs in descriptions) and produces a report; statuses move draft → submitted → in review → changes requested → accepted or rejected with reviewer notes visible to the submitter, and a withdraw action. Acceptance only publishes into the organization catalogue; public listing is REQ-048's marketplace and is never automatic.
- **Template versioning** — template versions are immutable and checksummed. A workflow that came from a template shows an "update available" banner when a newer version exists, with a definition diff (using REQ-095's diff) and an explicit apply; applying never touches pinned data, variable values, credentials or the workflow's name and schedule, and it never re-enables a paused workflow.
- **Air-gapped parity** — the gallery works from the bundled catalogue with no network access; remote catalogue fetching, when the deployment allows it, is a separate opt-in setting and its absence degrades to the bundled set with a clear note.

**Out**

- Public marketplace listing, pricing and reviews — REQ-048 (submission ends there).
- The graph renderer and editor chrome — REQ-004; the gallery reuses the preview renderer read-only.
- Definition diffing and version history mechanics — REQ-095.
- Expression language and variable semantics — REQ-092 (templates ship variable *references*, never values).
- Import of foreign automation formats (other products' exports) — a future migration request; this request defines the native bundle only.

### Screens (UI)

- **`/automations/templates`** — gallery grid with search, category rail, tag and capability filters, and a compatibility toggle ("works in this workspace"). Cards show name, one-line summary, category chips, node count, trigger chip, difficulty, and a compact graph preview. Empty state when filters match nothing offers one-click filter reset.
- **`/automations/templates/{slug}`** — detail page: description, what it does / what it needs / what it will not do, read-only graph preview with node list, requirements checklist with live satisfaction state, version history (version, changelog, published date, checksum), and the Import action with a target picker. Buttons that would fail stay disabled with the reason attached.
- **`/automations/templates/import`** — bundle import wizard: upload file → inspect (envelope version, templates, capabilities, references) → map variables and credentials → resolve conflicts (create, rename, skip) → review → result report with a downloadable summary. The wizard states clearly that nothing is imported until the review step is confirmed.
- **`/automations/templates/submissions`** and **`/automations/templates/submissions/new`** — my submissions with status, reviewer notes and a withdraw action; the new-submission screen picks a workflow or a bundle, runs the scan, shows the report, and requires the no-secrets confirmation before submitting.
- **`/settings/templates`** (curators, operator role) — publish a new template version from a workflow or bundle, edit catalogue metadata, set visibility, and see which imported workflows are on older versions (count, not a personal list).
- **Editor banner** — inside `/automations/[id]`: "This workflow came from template X v3; v4 is available" with View diff and Apply v4, or "Import a template" in the empty state of a fresh workspace.
- **States** — empty, loading, error and no-network states; disabled actions carry a tooltip reason; the grid reflows to cards on mobile with import usable but the graph preview collapsed.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/workflow-templates` | Catalogue with category, tag, capability and compatibility filters | `workflows.read` |
| GET | `/api/v1/workflow-templates/{slug}` | Full template: metadata, definition preview, requirements | `workflows.read` |
| GET | `/api/v1/workflow-templates/{slug}/versions` | Version history with changelogs | `workflows.read` |
| POST | `/api/v1/workflow-templates/{slug}/import` | Import into a workspace and folder (paused) | `workflows.manage` |
| POST | `/api/v1/workflow-templates/{slug}/versions` | Publish a new template version (curator) | `workflows.manage` |
| GET | `/api/v1/workflows/{id}/export` | Export one workflow as a bundle | `workflows.read` |
| GET | `/api/v1/workflow-folders/{id}/export` | Export a folder as a bundle | `workflows.read` |
| POST | `/api/v1/workflow-bundles/inspect` | Parse and validate an uploaded bundle without importing | `workflows.manage` |
| POST | `/api/v1/workflow-bundles/import` | Import after mapping and conflict resolution | `workflows.manage` |
| GET | `/api/v1/workflows/{id}/template-status` | Origin, current and latest version, update available | `workflows.read` |
| POST | `/api/v1/workflows/{id}/template-update` | Apply a template version with a diff preview token | `workflows.manage` |
| POST | `/api/v1/workflow-template-submissions` | Submit a bundle or workflow for review | `workflows.manage` |
| GET | `/api/v1/workflow-template-submissions` | Own submissions; curators see all | `workflows.read` |
| POST | `/api/v1/workflow-template-submissions/{id}/withdraw` | Withdraw a submission | `workflows.manage` |

No new permission keys; publishing a template version additionally requires the operator role.

### Data model

Migration `database/migrations/0018_workflow_templates.sql` (next free number if taken).

| Table | Columns (types) | Indexes / rules |
|---|---|---|
| `workflow_templates` | id uuid pk, slug text unique, name text, summary text, description text, category text, tags text[], difficulty text ('starter','standard','advanced'), visibility text ('bundled','organization','marketplace'), latest_version int default 1, created_by uuid → users set null, created_at, updated_at | unique `(slug)`; index `(category, latest_version)`; check `slug ~ '^[a-z0-9][a-z0-9-]*$'`; check `length(btrim(name)) > 0` |
| `workflow_template_versions` | id uuid pk, template_id uuid → workflow_templates cascade, version int, definition jsonb, meta jsonb (requirements, node counts, changelog), checksum text, published_by uuid → users set null, published_at | unique `(template_id, version)`; checksum is the canonical hash of the definition, verified on seed and import |
| `workflow_template_imports` | id uuid pk, template_id uuid, template_version int, workflow_id uuid → workflows cascade, organization_id uuid, imported_by uuid → users set null, imported_at | index `(template_id, template_version)` and `(organization_id, imported_at desc)` — powers the update-available count |
| `workflow_bundle_imports` | id uuid pk, organization_id uuid → organizations cascade, source text ('upload','marketplace','cli'), actor_id uuid → users set null, envelope_version int, report jsonb, created_at | index `(organization_id, created_at desc)`; report holds per-item outcomes, never definition bodies |
| `workflow_template_submissions` | id uuid pk, template_id uuid, organization_id uuid, submitted_by uuid → users set null, status text ('draft','submitted','in_review','changes_requested','accepted','rejected','withdrawn'), scan_report jsonb, reviewer_id uuid → users set null, review_notes text, submitted_at, decided_at | index `(status, submitted_at desc)`; check `(status in ('accepted','rejected')) = (decided_at is not null)` |

Adds to `workflows`: `template_id uuid → workflow_templates set null`, `template_version int`, `source text default 'manual'` `('manual','template','import')`. Seeding reads the bundled catalogue directory on boot: a template whose slug and checksum are unchanged is left alone; a new version is appended, never rewritten.

### Events

| Event | Kind | Notes |
|---|---|---|
| `workflow.template.imported` | emitted | slug, version, workflow id, workspace |
| `workflow.template.published` | emitted | slug, version, publisher |
| `workflow.template.update_available` | emitted | workflow id, from and to version |
| `workflow.template.update_applied` | emitted | workflow id, from and to version, actor |
| `workflow.bundle.exported` | emitted | scope (workflow or folder), item count, actor |
| `workflow.bundle.inspected` | emitted | envelope version, item count, blocking issues count |
| `workflow.bundle.imported` | emitted | created, skipped, failed counts |
| `workflow.template.submitted` | emitted | slug, submitter, scan summary |
| `workflow.template.submission_decided` | emitted | decision, reviewer, notes present |

Events never carry definition bodies or bundle contents; ids and counts only.

### Acceptance criteria

- [ ] The gallery lists the bundled starters with category, tag and compatibility filters, and search matches name, summary and tag.
- [ ] A template detail page shows the read-only graph preview, node list, requirements checklist and version history.
- [ ] Requirements are live: a template needing a helpdesk module or a credential kind shows that requirement as unsatisfied in a workspace without it, with import still permitted after an explicit acknowledgement.
- [ ] One-click import creates a paused workflow in the chosen workspace and folder, records template origin and version, and opens the setup checklist listing exactly the missing pieces.
- [ ] Import of a definition that fails validation is refused with a readable, per-node list of problems and imports nothing.
- [ ] Exporting a workflow produces a bundle that re-imports into a second workspace with variables and credentials remapped and identical behaviour on a test run.
- [ ] The bundle contains no secret material and no webhook token: an exported file greps clean and the imported workflow receives a fresh hook URL.
- [ ] Folder export and import move several workflows, report per-workflow outcomes, and honour the rename-on-conflict choice.
- [ ] The import wizard imports nothing before the review step is confirmed, and Inspect reports blocking issues without side effects.
- [ ] A template version published by a curator appears in the gallery; imported workflows of the older version show "update available" with a count in the curator view.
- [ ] Update availability is computed from `workflow_template_imports` and clears after an apply or a dismiss.
- [ ] Applying a template update changes only definition content: name, schedule, credentials, variable values, pinned data and enabled state are untouched, and a paused workflow stays paused.
- [ ] The apply flow shows the definition diff before applying and refuses when the local definition conflicts in ways the diff cannot merge.
- [ ] A submission runs the scan, blocks on a secret-like string, requires the no-secrets confirmation, and moves through the statuses with reviewer notes visible and a working withdraw.
- [ ] Deployments without network access show the bundled catalogue with a clear note and no error surfaces.
- [ ] All seven starter templates import, fill their checklist, and run green in the QA stack.
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and the QA walkthrough pass with zero high findings; the secret-scan and update-preview tests fail when their checks are removed.

### QA plan

The walkthrough must: browse the gallery, filter by category and compatibility, open a starter, read its requirements and graph preview, and import it into a fresh workspace; open the created workflow with its setup checklist and complete it; export that workflow, inspect the file for secrets, import it into a second workspace with a renamed conflict and run it; export a folder and import it with one workflow skipped; publish a new template version as curator, then open the imported workflow and apply the update from the diff screen, verifying the schedule and a variable value survived; submit a bundle that contains a deliberate secret-like string and read the blocked scan, fix it, submit, and withdraw; then disconnect network access and reload the gallery.

The visual check must see: card previews not distorted, requirement badges legible, the diff view readable at 1280px, disabled import buttons with their reasons, and the mobile grid collapsed cleanly.

### Slices

1. **Catalogue and gallery** — template tables, bundled seeding with checksums, gallery and detail screens with filters, requirements evaluation, read-only preview.
   *Done when:* every bundled starter renders with a correct requirement checklist and the seed is idempotent across restarts.
2. **Import and bundles** — one-click import with paused creation, setup checklist, workflow and folder export, bundle inspect/import with mapping, conflict resolution and per-item reports.
   *Done when:* a workflow exported from one workspace imports into another and produces the same test-run result.
3. **Versioning and submissions** — template versions with changelogs, update-available calculation and banner, diff-gated apply, submission statuses with scanning and reviewer notes.
   *Done when:* applying v4 leaves name, schedule, credentials and paused state untouched and the banner clears.

### Risks / notes

- The gallery is an ingestion path for definitions, so treat imported bundles as untrusted: validate the envelope and definition strictly, reject unknown node kinds and oversized payloads, and never execute anything at import time.
- Templates must not embed secrets or absolute URLs pointing at private infrastructure; the scan is a safety net, not the control — curation is.
- "One-click" must never enable a workflow: an imported definition that immediately fires on a schedule is how a template causes an incident.
- Template updates must be opt-in per workflow and diff-gated; the platform never mutates a running workflow because the catalogue changed.
- Keep the bundle envelope versioned from day one — an unversioned import format is unmaintainable within two releases.
- Compatibility evaluation must degrade quietly when an optional module is absent; a hard gallery error on a missing module makes the whole surface look broken.
