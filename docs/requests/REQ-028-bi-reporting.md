# REQ-028 — BI / Reporting Engine

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** module (`modules/reporting`)
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

Ask in natural language:

> "Show me the last 6 months of sales by department."

AI translates it into a query:

```text
Natural Language
      ↓
AI
      ↓
Query Builder
      ↓
PostgreSQL
      ↓
Chart
```

A report builder also ships with:

- Pivot
- Grouping
- Filters
- Charts
- Export
- Scheduled reports

## Notes

- Natural-language querying is an AI Hub consumer (docs/06-AI-HUB.md §11 pattern); all
  queries must run through permission scoping (docs/07-IAM.md).

## Implementation spec

> Buildable contract. Engine: new crate `crates/reporting` (`omnion-reporting`), surfaced as the `modules/reporting` feature through `apps/api/src/routes/reporting.rs`; panels under `/reports` in `apps/admin` (`apps/admin/features/reporting/`). A natural-language question is compiled into a closed query plan (dataset + fields + filters + grouping) that a human reviews before it runs — never into text SQL.

### Scope (in / out)

**In**

- Dataset catalog: named, versioned, permission-tagged read models over core and module tables (e.g. `content.pages`, `media.assets`, `workflow.executions`, `entity.{slug}`), declared in a Rust registry and mirrored into `report_datasets`.
- Report builder: field picker, typed filters (operators per column type, saved filter sets), row/column grouping, measures with aggregates (`count`, `sum`, `avg`, `min`, `max`, `count_distinct`), sort, limit, chart type, table/pivot/chart switch.
- Execution: read-only, permission-scoped, row-capped, statement-timeout-bounded, paginated; short-lived result cache keyed by definition hash + filters.
- Exports: CSV, XLSX and PDF (PDF rendering is REQ-029); run history with per-run rows, duration, status, error and a downloadable artifact.
- Schedules: cadence (daily, weekly, monthly or a cron expression), timezone, recipients, format, "only when rows > 0", failure notification (REQ-021).
- Natural-language ask: question → plan → review → run or save as a report.

**Out** — an ad-hoc SQL console for end users, cross-database federation, streaming results (REQ-041), anomaly detection or forecasting, write-back from a report, column-level masking policy (REQ-038 owns the policy, this engine applies it).

### Screens (UI)

| Route | Screen | Contents |
|---|---|---|
| `/reports` | Report list | Table: Name, Dataset, Owner, Schedule, Last run (status + relative time), Updated. Filters: dataset, owner, has-schedule, text. Row actions: Run, Duplicate, Rename, Delete. Bulk: delete, enable/disable schedule. Empty state "No reports yet — build your first report". |
| `/reports/new` | Builder | Three panes: dataset fields (left), result canvas (centre, Table / Pivot / Chart), configuration (right) with filters, grouping rows/columns, measures, sort, limit and chart options. Run (⌘Enter), Save (⌘S), Save as. |
| `/reports/{id}` | Report view | Result table/pivot/chart plus the filter bar (date range, saved filter sets), Run, Export menu (CSV, XLSX, PDF), tabs Definition / Runs / Schedule. |
| `/reports/{id}?tab=runs` | Run history | Table: Started, Duration, Rows, Status, Triggered by, Download. Row actions: Download, Re-run, Copy error. |
| `/reports/{id}?tab=schedule` | Schedule editor | Enable toggle, cadence picker (cron expression with a plain-language preview), timezone, recipients (users and e-mail addresses), format, "only when rows > 0", next-run preview. |
| `/reports/ask` | Ask | Prompt box with example questions, plan panel rendering the interpretation in plain language, "Show query" (only with `reports.sql.read`), Run, Save as report, refinement chips, and a clear state for an unrecognised intent. |
| `/reports/datasets` | Dataset catalog | Table: Dataset, Source, Columns, Permission key, Updated; detail panel lists columns with types and the permission required. Visible to `reports.datasets.read` holders. |

- Empty: first-run CTA; a dataset the caller cannot read explains which permission is missing; a run returning zero rows shows "No rows match these filters" with a Reset filters action.
- Loading: skeleton canvas; Run shows a spinner with a Cancel affordance for long queries; export shows progress.
- Error: query errors render as a card with the message and "Copy details" (no SQL for callers without `reports.sql.read`); a failed export keeps the result on screen; schedule validation errors appear inline.
- Keyboard: `⌘Enter` run, `⌘S` save, `/` focus the filter search, arrows move the pivot cell selection, `Esc` close dialogs.
- Mobile: the builder collapses into Fields / Result / Config tabs; result tables scroll horizontally with a sticky first column; the schedule editor stacks.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/reports` | List reports in scope | `reports.read` |
| POST | `/reports` | Create a report definition | `reports.manage` |
| GET | `/reports/{id}` | Report definition | `reports.read` |
| PUT | `/reports/{id}` | Update definition | `reports.manage` |
| DELETE | `/reports/{id}` | Delete report and its runs | `reports.manage` |
| POST | `/reports/{id}/duplicate` | Copy a definition | `reports.manage` |
| POST | `/reports/{id}/run` | Execute with filters and pagination | `reports.run` |
| GET | `/reports/{id}/runs` | Run history | `reports.read` |
| GET | `/reports/{id}/runs/{run_id}` | One run (status, counts, error) | `reports.read` |
| GET | `/reports/{id}/runs/{run_id}/export` | Download artifact (`format=csv|xlsx|pdf`) | `reports.export` |
| PUT | `/reports/{id}/schedule` | Create or replace the schedule | `reports.schedule` |
| DELETE | `/reports/{id}/schedule` | Remove the schedule | `reports.schedule` |
| POST | `/reports/ask` | Natural-language question → query plan | `reports.ask` |
| POST | `/reports/ask/run` | Execute a returned plan | `reports.run` |
| GET | `/reports/datasets` | Dataset catalog | `reports.datasets.read` |

Every execution resolves the caller's effective permissions and injects organization/site scope predicates into the compiled query. The compiled plan is returned only to callers holding `reports.sql.read`; everyone else gets the plain-language interpretation.

### Data model

`database/migrations/0013_bi_reporting.sql` (numeric prefix = next free slot at tick time). Additive-only.

- `report_datasets` — `id uuid pk`, `slug text not null unique`, `name text not null`, `description text`, `source text not null` (registry identifier), `permission_key text not null`, `columns jsonb not null` (name, type, label, filterable, aggregatable), `version integer not null default 1`, `created_at`, `updated_at`.
- `reports` — `id uuid pk`, `organization_id uuid not null → organizations on delete cascade`, `dataset_id uuid not null → report_datasets`, `name text not null`, `slug text not null`, `description text`, `definition jsonb not null` (fields, filters, grouping, measures, sort, limit, chart), `created_by uuid → users`, `created_at`, `updated_at`; `unique (organization_id, slug)`, slug format check.
- `report_runs` — `id uuid pk`, `report_id uuid not null → reports on delete cascade`, `organization_id uuid not null`, `status text not null` (`queued`, `running`, `succeeded`, `failed`), `trigger text not null` (`manual`, `schedule`, `api`), `params jsonb not null default '{}'::jsonb`, `row_count integer`, `duration_ms integer`, `error text`, `artifact_media_id uuid → media on delete set null`, `started_by uuid → users`, `started_at`, `finished_at`. Index `report_runs_report_idx (report_id, started_at desc)`.
- `report_schedules` — `id uuid pk`, `report_id uuid not null unique → reports on delete cascade`, `cron text not null`, `timezone text not null default 'UTC'`, `format text not null default 'csv'`, `recipients jsonb not null default '[]'::jsonb`, `only_when_rows boolean not null default false`, `enabled boolean not null default true`, `last_run_at`, `next_run_at`, `created_at`, `updated_at`. Checks: `format in ('csv','xlsx','pdf')`, non-empty cron.

### Events

- Emitted: `report.run.started`, `report.run.completed`, `report.run.failed`, `report.schedule.changed`, `report.export.ready`.
- Consumed: `entity.definition.published` (REQ-026) refreshes the `entity.{slug}` dataset row; `document.rendered` (REQ-029) attaches a PDF artifact to the run that requested it.
- Webhook relevance: high — an external subscriber can react to a scheduled report finishing. Payloads carry report id, run id, row count and artifact id; row data never leaves the platform on the bus.

### Acceptance criteria

- [ ] `/reports/datasets` lists the seeded datasets with their columns, types and required permissions.
- [ ] A report can be created over a dataset, run, saved and re-opened with the same definition.
- [ ] Table, pivot and chart presentations all render the same underlying result for one definition.
- [ ] Row and column grouping with measures produce totals matching a hand-computed count on seeded data.
- [ ] Typed filters (text, number, date range, select, boolean) compile correctly and are reflected in the result.
- [ ] Sorting and the row limit apply server-side and are reflected in the CSV header order.
- [ ] CSV export downloads a well-formed file whose header matches the selected columns and whose row count equals the run's `row_count`.
- [ ] XLSX and PDF exports produce openable files; the PDF path goes through REQ-029 templates.
- [ ] The run history lists every execution with duration, rows, status and a working download link.
- [ ] A failing query stores its error, shows a readable message in the panel and does not mark the run succeeded.
- [ ] A caller without the dataset permission receives 403 on run and export, and the panel says which permission is missing.
- [ ] Two organizations seeded: no report, run or export of organization A returns rows of organization B.
- [ ] A schedule with a cron cadence fires inside the QA window, produces a run, an artifact and a notification.
- [ ] The "only when rows > 0" switch suppresses a scheduled send on an empty result.
- [ ] A natural-language question returns a plan a reviewer can read; running an unmodified plan returns the same shape as an equivalent hand-built report.
- [ ] An unrecognised question produces a helpful failure, never an empty plan that silently runs.
- [ ] Queries are bounded: a wide dataset run stops at the configured row cap and a slow query stops at the statement timeout.
- [ ] `cargo test -p omnion-reporting` covers plan compilation, scope predicate injection, row caps and cron parsing.
- [ ] `pnpm typecheck && pnpm build` pass and every `/reports` route appears in the QA walkthrough inventory.
- [ ] Mobile 390 px shows the builder tabs and a horizontally scrollable result table.

### QA plan

Add `/reports` and its children to the QA walkthrough route list, then walk:

1. `/reports` empty state → `/reports/new` → pick the content dataset, add a date-range filter and a group-by, run, and confirm the result renders rows from the seeded content.
2. Switch the canvas to Pivot and to Chart, confirming the same numbers appear in all three presentations.
3. Save the report, export CSV, then open the Runs tab and download the same run's artifact.
4. Create a schedule a few minutes ahead, wait for the tick, and confirm a new run with `trigger = schedule` plus the notification.
5. `/reports/ask` with "show me page views per day for the last 30 days" → read the plan → run → confirm a chart; then ask something unintelligible and confirm the failure state.
6. Signed in with a limited role: a report over a dataset the role cannot read shows the missing-permission state.

The visual check must see: a populated result table with aligned numeric columns, a pivot with clear row/column headers, a chart with readable axis labels, export buttons in a consistent toolbar, a legible plan panel on `/reports/ask`, inline schedule validation, and no clipped or overlapping controls.

### Slices

1. **Dataset + builder + run + CSV** — migration, dataset registry, plan compiler with scope predicates, builder screen in table mode, run endpoint, run history, CSV export. Done when a report over a real dataset returns correct rows and its CSV downloads with the matching header and count.
2. **Pivot + charts + remaining exports** — grouping rows/columns, measures, saved filter sets, chart options, XLSX and PDF export through REQ-029, cancellation for long runs. Done when a pivot's totals match the hand-computed count and all three export formats open.
3. **Schedules + ask** — schedule editor and worker, run notifications, natural-language ask with the review step, dataset catalog screen. Done when a schedule fires on its own and an accepted plan runs end to end.

### Risks / notes

- Query construction never concatenates SQL: the definition is compiled to a parameterized statement whose identifiers come from the dataset registry, never from the request body.
- Every run is bounded by a row cap and `SET LOCAL statement_timeout`, and runs inside a read-only transaction; a large export is executed by the worker so the request cannot pin a connection.
- Natural-language plans are always shown for review; the AI never runs an organization-wide aggregate on its own, and ambiguous questions must ask rather than guess.
- Permission scoping applies at three levels: dataset permission key, organization/site predicates, and the compliance centre's masking policy where one exists.
- Schedules store the organization timezone; `next_run_at` is computed in UTC so a daylight-saving change neither double-runs nor skips a report.
- Dataset drift: datasets are versioned, and a saved definition referencing a removed column fails validation with a clear message instead of failing at run time.
- The XLSX writer belongs in the worker, not the API binary, to keep the request path lean.
- AI tokens for `/reports/ask` are metered through the AI Hub cost accounting (REQ-047); a per-organization monthly cap must exist before the feature is enabled by default.
