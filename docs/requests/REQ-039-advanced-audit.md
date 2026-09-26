# REQ-039 — Advanced Audit

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core (`crates/audit`)
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

Not just "Mehmet changed a page" — everything correlated:

```text
Who
What
When
Where
Before
After
IP
Device
API / UI
Request ID
```

## Notes

- Expands the audit views in docs/07-IAM.md §19 and docs/06-AI-HUB.md §17; the Request ID
  thread must link API gateway (REQ-040), realtime events (REQ-041) and AI actions.

## Implementation spec

### Scope (in / out)

**In**

- The existing append-only `audit_log` grows from "who did what to which target" into a correlated record: request id, channel (`ui` `api` `agent` `automation` `system`), result (`success` `failure` `denied`), severity, method and path, device and user-agent, before/after snapshots and a computed field-level diff.
- **Hash-chained rows**: each row carries a sequence number, the previous row's digest and its own digest, so tampering is detectable by a check that can run on demand.
- A **timeline explorer**: filter rail + virtualised table + detail panel, with saved views.
- **Exports** as queued jobs (CSV and JSONL) with row ceilings, checksums and expiry.
- **Correlation**: one click from a row to every row sharing its request id, plus links to the related event, workflow execution or AI action.
- Retention handoff to the Compliance Center (REQ-038): the audit class is immutable by default and supports `pseudonymize`, never `delete`.

**Out**

- A SIEM product: no streaming to external collectors, no detection rules, no alerting engine (that is REQ-012's surface).
- Storing request or response bodies. Only structured before/after snapshots and metadata, with redaction applied on write.
- Real-time push of new rows while a query is open (that is REQ-041).
- Editing or deleting rows by any API path.

### Screens (UI)

- `/audit` — the timeline. Left filter rail, centre table, right detail panel that opens on row click without leaving the page.
  - Table columns: **Time · Actor · Type · Action · Target · Channel · Request · Result · Severity · IP**. The request cell is a short chip (first 8 characters) with a copy action.
  - Filters: date range with `24h` / `7d` / `30d` / custom presets, actor picker (person, service account, agent), action autocomplete fed by the action catalogue, target type, channel, result, severity, organization, site, exact request id, IP or CIDR.
  - Bulk actions: **Export selection · Copy request ids · Save as view · Clear**. There is no edit or delete action anywhere on this screen, by design.
  - Time renders in the viewer's timezone with the UTC value in the tooltip and in the detail panel.
- `/audit/{id}` — detail. Blocks: **Request** (method, path, status, duration, request id),
  **Actor** (name, type, IP, device, user agent), **Target** (type, id, deep link), **Change** (side-by-side before/after with the changed fields highlighted and redacted fields masked),
  **Correlation** (event, workflow execution, AI action, related rows), **Raw** (the stored JSON, copy-only). Deep linkable; `Esc` returns to the filtered list.
- `/audit/exports` — table: **Created · Requested by · Format · Range · Rows · Size · Status · Expires · Actions**. Actions: download (when `ready`), re-run, delete. Filters: status, format, requester, date range.
- `/audit/views` — saved views table (**Name · Owner · Shared · Filters · Updated**) with rename, share and delete; the rail lists them in the same order.
- States: virtualised skeleton rows while loading; "no rows match these filters" with a **Clear filters** button; a query that times out shows the narrowed-time-range hint instead of a blank table; an export still running shows a progress chip that survives a page reload.
- Keyboard: `/` focus search, `j` / `k` next and previous row, `Enter` open detail, `Esc` close, `e` export the selection, `v` save the current filters as a view, `⌘K` command palette.
- Mobile: the rail collapses to a filter sheet, the table becomes a card list with the same fields, the detail becomes a full-screen sheet.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/audit` | Query the trail with filters and cursor paging | `audit.read` |
| GET | `/api/v1/audit/{id}` | One row with diff and correlation links | `audit.read` |
| GET | `/api/v1/audit/actions` | Action catalogue for autocomplete | `audit.read` |
| GET | `/api/v1/audit/actors` | Actor suggestions for the picker | `audit.read` |
| GET | `/api/v1/audit/requests/{request_id}` | The correlated thread for one request | `audit.read` |
| GET | `/api/v1/audit/stats` | Counters by action, channel, result, severity | `audit.read` |
| GET | `/api/v1/audit/views` | Saved views for the caller | `audit.read` |
| POST | `/api/v1/audit/views` | Save a view | `audit.read` |
| PATCH | `/api/v1/audit/views/{id}` | Rename or change sharing | `audit.read` |
| DELETE | `/api/v1/audit/views/{id}` | Delete a view | `audit.read` |
| POST | `/api/v1/audit/exports` | Queue an export job | `audit.export` |
| GET | `/api/v1/audit/exports` | List export jobs | `audit.export` |
| GET | `/api/v1/audit/exports/{id}` | Job status | `audit.export` |
| GET | `/api/v1/audit/exports/{id}/download` | Download a ready export | `audit.export` |
| POST | `/api/v1/audit/integrity/check` | Verify the hash chain over a range | `audit.integrity` |

Query parameters: `from`, `to`, `actor`, `action`, `target_type`, `channel`, `result`, `severity`, `request_id`, `ip`, `organization_id`, `site_id`, `cursor`, `limit` (default 50, max 200). Exports cap at 100 000 rows and are refused above it with a hint to narrow the range. Errors: `403` without the permission, `422` for an inverted date range, `409` while an identical export job is queued.

### Data model

`database/migrations/0014_audit_depth.sql` extends the existing table rather than replacing it:

```sql
alter table audit_log
  add column request_id uuid,
  add column channel text not null default 'ui',
  add column result text not null default 'success',
  add column severity text not null default 'info',
  add column method text,
  add column path text,
  add column device text,
  add column user_agent text,
  add column before jsonb,
  add column after jsonb,
  add column diff jsonb,
  add column seq bigint,
  add column prev_hash text,
  add column entry_hash text;
```

with check constraints `channel in ('ui','api','agent','automation','system')`, `result in ('success','failure','denied')`, `severity in ('info','notice','warning','critical')`.

New tables:

- `audit_saved_views` — `id uuid pk`, `organization_id uuid`, `user_id uuid not null`, `name text not null`, `filters jsonb not null default '{}'`, `shared boolean not null default false`, `created_at timestamptz not null default now()`, `updated_at timestamptz not null default now()`. Unique `(user_id, name)`.
- `audit_export_jobs` — `id uuid pk`, `organization_id uuid`, `requested_by uuid not null`, `filters jsonb not null default '{}'`, `format text not null default 'csv' check (format in ('csv','jsonl'))`, `status text not null default 'queued' check (status in ('queued','running','ready','failed','expired'))`, `storage_key text`, `size_bytes bigint`, `sha256 text`, `row_count bigint`, `error text`, `expires_at timestamptz`, `created_at timestamptz not null default now()`, `completed_at timestamptz`.

Indexes: `audit_log_request_id_idx (request_id)`, `audit_log_org_created_desc_idx (organization_id, created_at desc)`, `audit_log_actor_created_idx (actor_user_id, created_at desc)`, `audit_log_action_created_idx (action, created_at desc)`, `audit_log_channel_result_idx (channel, result, created_at desc)`, `audit_export_jobs_status_idx (status, created_at desc)`.

Writes go through the existing `crates/audit` record path, which now also computes the digest over the canonical row JSON plus `prev_hash`, and applies the redaction patterns before the row is stored. There is no `update` or `delete` SQL against `audit_log` anywhere in the codebase; a test asserts that.

### Events

- Emitted: `audit.export.ready` — `{job_id, format, row_count, sha256, expires_at}`; `audit.export.failed`; `audit.integrity.check.completed` — `{from, to, checked, broken_at}`.
- Consumed: the event bus subscriber records one audit row per platform event with `channel = 'automation'`, `action = event.<name>` and the event id in the metadata, deduplicated on that id — which is what makes "what happened around this request" answerable.
- Webhook relevance: exports and integrity failures are worth a webhook; row-level events are not, because the bus already carries the fact itself and echoing it back would double the volume.

### Acceptance criteria

- [ ] Every privileged action writes one row with actor, action, target, timestamp, channel and result.
- [ ] API-originated rows carry the request id that the gateway assigned and the exact path.
- [ ] A tag changed through the API produces a row whose `diff` names the changed field only.
- [ ] `before` and `after` are absent for actions that have no snapshot, and the detail panel says so instead of rendering an empty diff.
- [ ] Redaction replaces sensitive values in snapshots and metadata; a test greps the stored row for the fixture secret and finds the mask.
- [ ] `/audit` filters by date range, actor, action, channel, result, severity, request id and IP and returns matching rows for each filter.
- [ ] Cursor paging returns the next page without duplicates or gaps.
- [ ] Opening a row's detail shows request, actor, device, IP and target blocks populated.
- [ ] Following a request-id chip lists every row sharing that id, across channels.
- [ ] Saved views persist, reload and reproduce the same result count.
- [ ] Export jobs queue, run, report a row count, produce a downloadable file with a checksum and expire afterwards.
- [ ] An export above the row ceiling is refused with a narrowing hint.
- [ ] `audit.integrity/check` passes on an untouched range and reports the first broken sequence number after a deliberate row edit in a test.
- [ ] `audit.read` alone cannot export (`403`); `audit.export` alone cannot read the trail.
- [ ] The audit trail has no update or delete path; a test asserts the absence.
- [ ] `cargo test --workspace` and `pnpm typecheck && pnpm build` pass; the walkthrough covers the new routes with zero high findings.

### QA plan

- The browser walkthrough visits `/audit`, `/audit/views` and `/audit/exports`; the inventory gains those routes plus the detail panel.
- Controls to exercise: each filter in the rail, the `24h`/`7d` presets, the actor picker, the action autocomplete, sorting by time, row click, the request-id chip, the copy action, the raw JSON toggle, save-as-view, export selection, and the download of a finished export.
- Assertions: a seeded action performed through the API appears in the list within the page refresh; `j`/`k` move the selection and `Enter` opens the detail; the detail panel shows the changed field for a seeded edit.
- The visual check must see: a real table with data (never an empty shell), severity rendered with icon and text, masked values, no layout shift when the detail panel opens, and the card layout under 640 px.

### Slices

1. **Schema + write path + integrity** — the migration, request-id and channel propagation from the API middleware into `crates/audit`, the digest chain, `GET /audit` with paging, integrity check.
   *Done:* a privileged API call writes a complete row, filters return it, and the integrity check detects a tampered row in a test.
2. **Read API + exports** — detail, action catalogue, actor suggestions, stats, export jobs with download and expiry.
   *Done:* an export produces a checksummed file and an expired one is `410`.
3. **Timeline UI** — the `/audit` screen: rail, table, detail panel, keyboard navigation, mobile layout, empty and error states.
   *Done:* the walkthrough filters, opens a row and follows a request id without a high finding.
4. **Saved views + correlation + retention handoff** — views CRUD, the event-bus subscriber, the Compliance Center links and the pseudonymize hook.
   *Done:* views reload identically, correlation lists rows from all channels, and the retention action `pseudonymize` runs over a seeded range.

### Risks / notes

- Audit writes must never fail the request they describe: when the row cannot be written the action must fail too, and a deferred retry path is needed for the rare case where the database is briefly unavailable.
- The hash chain gets cheaper if writes are batched per transaction; keep one chain per organization to avoid a global hot row.
- Snapshots are where personal data leaks into the trail: the redaction patterns are shared with REQ-037 and applied on write, not on read.
- This table grows fastest of all — plan monthly partitions and move old partitions to cheaper storage under a retention policy instead of deleting rows.
- Request-id propagation must survive background work (workers, queued jobs) or correlation breaks exactly where it is most useful.
- Wording: the screen explains who changed what; it must not read like surveillance copy.
