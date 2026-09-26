# REQ-080 — Admin Activity Timeline

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** admin (`apps/admin`) + core (`crates/audit`)
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

What happened in this installation, in human language.

- Timeline screen: merges audit entries, deploys, AI jobs, automation runs and login events.
- Filters: actor, module, severity, date range; free-text search.
- Row detail with metadata, related resource links and (for AI) the model/prompt reference.
- Live updates while work is running; export to CSV.
- Powers the "who changed this?" affordance everywhere else in the panel.

## Implementation spec

### Scope (in / out)

**In**
- A read model (`activity_events`) projected from the audit trail and the bus, so one row answers "who did what, to what, when, with what result" in human language while the audit log stays the source of truth and keeps the compliance guarantees.
- The projector: a subscriber in `crates/audit` that turns audit rows into activity rows (mapping action → module, severity, human summary and subject links) plus bus-only sources that never write an audit row of their own (deploy step transitions, AI job step transitions, automation run progress, session events), with idempotent writes keyed by `(source, source_id)` so a replayed delivery never duplicates a row.
- A human-readable rendering layer: templates per action (`page.published` → "Emre published <em>Pricing</em>"), actor labels resolved for users (display name), API tokens (token name only — never the value), automations ("Automation <em>Nightly sync</em>"), AI jobs ("AI job #128 — find overdue invoices") and the system itself, with a fallback that still reads as a sentence when no template exists.
- Module, severity and result mapping: module from the action's namespace with an explicit override table, so the filter means one thing everywhere; `severity` (`info`, `notice`, `warning`, `critical`) and `result` (`success`, `failure`, `denied`) carried or derived (denied permission checks are warnings, backup failures critical) so the timeline can answer "show me only what went wrong".
- Filters: actor picker (users, automations, AI jobs, token names, system), module, severity, result, channel, date range with presets (last hour, 24 hours, 7 days, 30 days, custom) and free-text search over the human summary, the action key, the actor label and the subject label.
- The stream: newest first, grouped by request when consecutive rows share a `request_id` ("Emre edited 4 pages · 1 request") with expansion; each row shows time (relative, exact on hover), actor, verb plus object, module chip, severity marker (icon plus label, never colour alone), result badge and source icon. Clicking a chip applies it as a filter.
- Live tail: an SSE stream (REQ-041 transport, topic `activity`) with `Last-Event-ID` resume, a `Live` toggle that pauses and resumes with an "N new" pill, and a bounded insertion budget so a busy installation cannot melt the browser.
- Row detail: metadata table (key/value with a JSON viewer and copy), the change diff when the row carries one (REQ-111 change sets, field-level), related resource links (page, theme, plugin, deployment run, AI job, automation) rendered permission-aware, the request thread from REQ-039, and for AI rows the model key plus a *reference* to the prompt that renders only for a caller holding `ai.usage.read` — otherwise the row says the prompt is restricted and names the reason.
- "Who changed this?" everywhere: a reusable drawer and a subject-scoped query so a page, theme, plugin, site or setting shows its own history without leaving the screen it is on, plus a `History` control on resource headers opening the same drawer.
- Export: CSV (and JSONL) of the current filters as a background job with row counts, checksum, expiry and a `Redact sensitive metadata` toggle (on by default) that applies the installation's redaction rules before the file is written.
- Saved views: named filter sets per user, optionally shared, with a default view and `Reset to default`; the timeline's views are its own (REQ-039's auditor views are a different surface with different filters).
- Retention and rebuild: activity rows follow the audit retention policy and are prunable (they are a projection); a rebuild for a date range repopulates the read model and is idempotent; the audit log itself is never shortened by this REQ.

**Out**
- Authoring audit rows: REQ-039 owns the write path, its hash chain, the diff computation and the redaction patterns. The timeline consumes them and never writes to `audit_log`.
- The raw audit *log* screen (unfiltered rows, integrity checks, auditor views) — REQ-039 keeps it; this screen is the human-language view and links to the raw row. Compliance evidence packs, retention policy administration and legal holds — REQ-038/REQ-039.
- Charts and trends over activity — REQ-007/REQ-028; the timeline may link to a filtered analytics view but does not become a dashboard. External SIEM shipping and webhook delivery of activity — REQ-016 and the installation's own integrations.

### Screens (UI)

| Route | Screen |
|---|---|
| `/activity` | Timeline: filter bar, grouped stream, live tail, saved views |
| `/activity/<id>` | Row detail (drawer on desktop, page on mobile and for direct links) |
| `/activity/subjects/<type>/<id>` | Subject thread — everything that happened to one resource |
| `/activity?request_id=<id>` | Request thread (all rows from one request), also reachable from the detail |
| `/activity/exports` | Export jobs with status, row count, download and expiry |
| `/activity/views` | Saved views management (create, rename, share, delete, set default) |

- **Filter bar.** Sticky: actor picker (searchable, grouped by kind, avatars for users, a chip for token names), module select, severity select, result select, date range with presets plus a custom calendar, and a search box reaching summary, action, actor and subject labels. Active filters render as removable chips; `Save view` names the current set (1–40 characters) with a `Shared` toggle; the header shows `N events` for the current filter and the exact range. A `Live` toggle with a pause state and an "N new" pill sits at the end.
- **Stream.** Reverse-chronological rows, grouped by request when consecutive: the group header shows the actor, the count and the request id, and expanding lists the individual actions. Row anatomy: relative time with an absolute tooltip, actor chip, the human sentence with a clickable subject, module chip, severity marker with icon and text label, result badge, and a `⋯` menu (`Open detail`, `Open resource`, `Copy link`, `Copy request id`, `Filter by this actor`, `Filter by this module`). A row carrying a change diff shows a `+3 / −1` hint that expands inline.
- **Detail.** Summary (the same sentence with the full timestamp), actor (user, token name, automation or AI job identity), context (request id, channel, IP, user agent, device, method and path where present), metadata (key/value table with expandable values and `Copy JSON`), change diff (field-level before/after, never a raw JSON blob), related resources (a link the caller may not open renders as plain text with the reason), the AI reference (model key and prompt link only with `ai.usage.read`), and the request thread. Actions: `Copy link`, `Open the raw audit row` (REQ-039), `Open the resource`.
- **"Who changed this?" drawer.** The same component in a right-side drawer: a subject-scoped stream with the same row anatomy and filters, a `History` header and an `Open full timeline` link that carries the subject filter into `/activity`. It opens without leaving the resource screen and becomes a full-screen route on mobile.
- **States, keyboard and mobile.** No matches: a suggestion state that offers to relax the range or the actor filter while still showing the newest event. Empty installation: an explainer. Loading: skeleton rows. Error: banner with the failing call and `Retry`, filters preserved. `audit.read` governs the whole screen; without `audit.export` the export action is disabled with the permission named; read-model rebuild shows a banner (`history is rebuilding since 09:00`) instead of an unexplained gap. Keyboard: `g a` Activity, `j`/`k` move rows, `Enter` opens the detail, `o` opens the subject resource, `f` cycles the focused filter, `p` pauses live, `e` exports, `/` focuses search, `Esc` closes the drawer. Below 900 px the stream is single column with filters in a bottom sheet, the detail is a full page, the request thread collapses into an accordion, and the drawer becomes the subject route.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/activity` | Paged stream (`actor`, `module`, `severity`, `result`, `channel`, `from`, `to`, `q`, `subject_type`, `subject_id`, `request_id`, cursor) | `audit.read` |
| GET | `/api/v1/activity/{id}` | Row detail with metadata, diff, links and the request thread | `audit.read` |
| GET | `/api/v1/activity/subjects/{type}/{id}` | Everything that happened to one resource | `audit.read` |
| GET | `/api/v1/activity/stream` | SSE live tail (topic `activity`, `Last-Event-ID` resume) | `audit.read` |
| GET | `/api/v1/activity/facets` | Filter options with counts for the current range (actors, modules, severities, results) | `audit.read` |
| GET · POST | `/api/v1/activity/views` | List · create a saved view (`name`, `filters`, `shared`) | `audit.read` |
| PATCH · DELETE | `/api/v1/activity/views/{id}` | Rename, change sharing, set default · delete | `audit.read` (own views; shared views need `audit.views.share`) |
| POST | `/api/v1/activity/exports` | Queue a CSV/JSONL export of the current filters (`redact` flag) | `audit.export` |
| GET | `/api/v1/activity/exports` · `/{id}` | Export job list · one job with row count, size, checksum, expiry | `audit.export` |
| GET | `/api/v1/activity/exports/{id}/download` | Download a ready export (short-lived signed URL) | `audit.export` |
| POST | `/api/v1/activity/rebuild` | Rebuild the read model for a range (idempotent, queued) | `audit.manage` |
| GET | `/api/v1/audit/{id}` | The raw audit row behind an activity row — REQ-039 route | `audit.read` |
| GET | `/api/v1/ai/jobs/{id}` | The AI job an activity row references — REQ-079 route; prompt rendered only with `ai.usage.read` | `ai.jobs.read` |

Errors are actionable rather than generic: `activity_range_too_wide` (with the installation's maximum), `activity_export_too_large` (with the row count and a suggestion to narrow the range), `activity_view_name_taken`, `activity_rebuild_in_progress`, `activity_export_expired`. The stream degrades to cursor polling where SSE is unavailable and the response names the fallback cursor.

### Data model

Migrations: `0123_activity_events.sql`, `0124_activity_views_exports.sql` (reserved band 0116–0127; append-only ledger — take the next free number if taken).

```sql
-- 0123_activity_events.sql
activity_events (
  id bigint generated always as identity primary key,
  organization_id uuid null references organizations (id) on delete set null,
  site_id uuid null references sites (id) on delete set null,
  source text not null, source_id text null,           -- 'audit' | 'deploy' | 'ai_job' | 'automation' | 'auth' | 'system'
  occurred_at timestamptz not null, recorded_at timestamptz not null default now(),
  module text not null, action text not null, severity text not null default 'info',
  result text not null default 'success', channel text null,
  actor_type text not null, actor_user_id uuid null references users (id) on delete set null,
  actor_label text not null,
  subject_type text null, subject_id text null, subject_label text null,
  summary text not null, metadata jsonb not null default '{}', change_set jsonb null,
  request_id text null, ip_address inet null,
  search_vector tsvector generated always as (to_tsvector('simple',
    coalesce(summary,'') || ' ' || coalesce(action,'') || ' ' || coalesce(actor_label,'') || ' ' || coalesce(subject_label,''))) stored,
  constraint activity_events_source_check check (source in ('audit','deploy','ai_job','automation','auth','system')),
  constraint activity_events_severity_check check (severity in ('info','notice','warning','critical')),
  constraint activity_events_result_check check (result in ('success','failure','denied'))
);
create unique index activity_events_source_key on activity_events (source, source_id) where source_id is not null;
create index activity_events_org_time_idx on activity_events (organization_id, occurred_at desc);
create index activity_events_actor_idx on activity_events (actor_user_id, occurred_at desc);
create index activity_events_module_idx on activity_events (module, occurred_at desc);
create index activity_events_severity_idx on activity_events (severity, occurred_at desc) where severity <> 'info';
create index activity_events_subject_idx on activity_events (subject_type, subject_id, occurred_at desc);
create index activity_events_request_idx on activity_events (request_id) where request_id is not null;
create index activity_events_search_idx on activity_events using gin (search_vector);

activity_event_links (
  id bigint generated always as identity primary key,
  activity_event_id bigint not null references activity_events (id) on delete cascade,
  link_type text not null, target_type text not null, target_id text not null, label text null
);
create index activity_event_links_event_idx on activity_event_links (activity_event_id);
create index activity_event_links_target_idx on activity_event_links (target_type, target_id);

-- 0124_activity_views_exports.sql
activity_saved_views (
  id uuid primary key default gen_random_uuid(),
  organization_id uuid not null references organizations (id) on delete cascade,
  user_id uuid not null references users (id) on delete cascade,
  name text not null, filters jsonb not null default '{}', shared boolean not null default false,
  is_default boolean not null default false,
  created_at timestamptz not null default now(), updated_at timestamptz not null default now(),
  constraint activity_saved_views_name_length check (length(btrim(name)) between 1 and 40),
  constraint activity_saved_views_user_name_key unique (user_id, name)
);
activity_exports (
  id uuid primary key default gen_random_uuid(),
  organization_id uuid not null references organizations (id) on delete cascade,
  requested_by uuid not null references users (id) on delete cascade,
  filters jsonb not null default '{}', format text not null default 'csv', redact boolean not null default true,
  status text not null default 'queued', storage_key text null, row_count bigint null, size_bytes bigint null,
  sha256 text null, error text null, expires_at timestamptz null,
  created_at timestamptz not null default now(), completed_at timestamptz null,
  constraint activity_exports_format_check check (format in ('csv','jsonl')),
  constraint activity_exports_status_check check (status in ('queued','running','ready','failed','expired'))
);
create index activity_exports_org_created_idx on activity_exports (organization_id, created_at desc);
create index activity_exports_pending_idx on activity_exports (status) where status in ('queued','running');
```

The read model is disposable by design: rows are a projection with idempotent writes (`(source, source_id)` unique index), prunable under the audit retention policy, rebuildable in a range. `recorded_at` versus `occurred_at` makes projector lag observable. Nothing sensitive is copied: metadata is stored after the same redaction pass REQ-039 applies, token subjects store the token's name, and `change_set` holds field-level diffs rather than raw payloads. New permission keys in `crates/permissions/src/catalogue.rs`, category `audit`: `audit.manage` (rebuild and retention-adjacent operations) and `audit.views.share`; `audit.read` and `audit.export` already exist and are reused so existing roles keep working.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `activity.export_ready` | An export finished and is downloadable | `export_id`, `row_count`, `size_bytes`, `format`, `redacted` |
| `activity.export_failed` | An export failed | `export_id`, `error` |
| `activity.read_model_rebuilt` | A rebuild finished | `from`, `to`, `rows_written`, `duration_ms` |
| `activity.retention_pruned` | Rows pruned under the retention policy | `from`, `to`, `rows_removed` |

The timeline deliberately emits no event per projected row — that would put the audit firehose on the bus a second time and invite feedback loops. Projection inputs: audit rows through the writer hook (REQ-039) plus bus sources `deployment.job.started` · `.step_changed` · `.succeeded` · `.failed` (REQ-024), `ai.job.created` · `.step_failed` · `.completed` (REQ-079), `automation.run.recorded` (REQ-003), `auth.session.created` · `.failed` (login events), `backups.created` · `.failed` (REQ-013), `updates.run_started` · `.run_succeeded` · `.run_failed` (REQ-078). Consumed for its own hygiene: `feature.flag.changed` (projector pause switch), `organizations.archived` (retention sweep). Webhook relevance: the four operational events are internal; an installation wanting activity outbound wires the underlying audit and module events instead.

### Acceptance criteria

- [ ] Every audit row written after the feature lands appears in `/activity` within two seconds with a human sentence, an actor label and a subject, verified for a page edit, a theme activation, a permission change and a login.
- [ ] Deploy, AI job, automation and login sources appear alongside audit rows, each with its own source icon and the correct module.
- [ ] Rows sharing a `request_id` group into one expandable entry, and expanding shows the individual actions with their own subjects.
- [ ] Filters compose (actor + module + severity + result + range + free text) and the displayed `N events` count matches the rows actually returned by paging to the end.
- [ ] Free-text search finds a page by its label inside the human summary and a plugin by its key inside the action, using the indexed search vector.
- [ ] A `denied` row states the permission that was missing and links to the resource the caller tried to reach.
- [ ] Row detail renders metadata as a readable table with a copyable JSON form, and the change diff shows field-level before/after values, not a raw JSON blob.
- [ ] AI rows show the model key, and the prompt reference renders only for a caller holding `ai.usage.read`; without it the row states the restriction and never leaks prompt text.
- [ ] The request thread lists exactly the rows of that request across modules and links to the raw audit row (REQ-039).
- [ ] Live tail appends new rows without a reload, `Live` pause holds the stream with an "N new" pill, and resuming after a disconnect continues from the last event id without duplicates or gaps.
- [ ] The "who changed this?" drawer opens from a page, a theme and a plugin header, shows that subject's history, and its `Open full timeline` link carries the subject filter into `/activity`.
- [ ] A CSV export of a filtered range contains exactly the rows the UI counted, with a checksum, an expiry and the redaction toggle honoured (a credential-update row shows the key name, never the value).
- [ ] Saved views persist across sessions, a shared view is visible to others in the organization, and exactly one view can be the default.
- [ ] `audit.read` governs the screen, `audit.export` the exports and `ai.usage.read` the AI prompt reference — verified by calling the endpoints with narrower roles.
- [ ] Rebuild for a range is idempotent: running it twice leaves the row count unchanged and writes no duplicates (the unique index on `(source, source_id)` holds).
- [ ] Pruning under the retention policy removes old projection rows while `audit_log` keeps every original row, and the UI explains the resulting earliest-available date.
- [ ] A 30-day range query on a seeded dataset returns its first page in under 400 ms, and a range wider than the installation's maximum is refused with `activity_range_too_wide` naming the limit.
- [ ] The stream, filter bar and detail are usable at 390 px, and the walkthrough reports zero high findings on the new routes.

### QA plan

Add `/activity`, `/activity/exports` and a subject thread route to the `routes` array in `scripts/qa/walkthrough.cjs`, and reach the screen from the sidebar. The walkthrough must: open `/activity` and read the newest rows; apply an actor filter by clicking an actor chip, add a severity filter, switch the range to the last 24 hours and search for the seeded page's label; open a row detail and its request thread, then open the raw audit row from the detail; click through to the subject resource and confirm the target screen opens in a state the caller may read; open a page, use the `History` control and read the "who changed this?" drawer, then follow `Open full timeline` and confirm the subject filter is applied; with `Live` on, perform an edit through the API in the same run and watch the row appear without a reload, then pause, make a second edit and resume to see the "N new" pill drain; queue a CSV export, open `/activity/exports`, download it and verify the row count matches the on-screen total and that a credential-shaped value is redacted; save a view as the default and reload to confirm it applies. Visual check: rows read as sentences with a legible subject, severity is conveyed by an icon and a word rather than colour alone, the stream is dense but not cramped, the detail's metadata table is readable, no screen shows raw JSON as its primary content, and the mobile pass shows the single-column stream with a working bottom-sheet filter and a full-page detail.

### Slices

1. **Read model, projector and the stream.** Migrations `0123_activity_events.sql` and `0124_activity_views_exports.sql`, the projector with idempotent writes and the per-action template table, module and severity mapping, the timeline with filter bar, grouping and facets, and the walkthrough routes. *Done when:* acceptance 1–3, 5, 17 pass and `/activity` is in the walkthrough inventory.
2. **Detail, links and "who changed this?".** Row detail with metadata, change diff, permission-aware resource links, the request thread, the raw audit row link, the AI reference gated by `ai.usage.read`, and the subject drawer plus its `History` control on resource headers. *Done when:* acceptance 4, 6–9, 11 pass.
3. **Live tail, views, export, retention and polish.** SSE stream with resume and pause, saved views with sharing and default, CSV/JSONL exports with checksums and redaction, rebuild and retention prune with the earliest-available explainer, empty/loading/error states, keyboard and mobile coverage, and the four operational events verified against a subscribed endpoint. *Done when:* acceptance 10, 12–16, 18 pass with a green walkthrough and no high findings.

### Risks / notes

- The audit log stays the single source of truth: this read model may be dropped and rebuilt at any time, and no feature may ever read a fact *only* from `activity_events` — that constraint protects the compliance story.
- The timeline is a surveillance surface and is governed like one: viewing it is audited once per session rather than per row (avoiding rows about the timeline), exports record their filter set and row count, and shared views are visible to the organization rather than the world.
- Redaction happens before storage and again before export: token and credential values never enter the projection, metadata follows REQ-039's redaction rules, and the export's `redact` toggle can only make the file stricter, never looser.
- Free-text search over a large range is the performance cliff: the generated `search_vector`, a range cap and honest refusals (`activity_range_too_wide`, `activity_export_too_large`) are the guards, and the UI must suggest narrowing instead of timing out.
- Grouping by request is a presentation choice with a correctness cost: an expanded group must always show every underlying row, and a group must never merge two different requests even when adjacent and from the same actor.
- Projector lag and gaps are visible by design (`recorded_at` versus `occurred_at`, a rebuild banner): a silently incomplete timeline is worse than one that admits it is rebuilding.
- Actor labels for deleted users, renamed automations and rotated tokens are resolved at projection time and stored, so history keeps reading correctly after the world changes; `Former user` appears only when the stored label is empty.
- The screen states the range it shows in the installation's timezone and the viewer's locale, and never implies it shows "everything" when retention has pruned older rows.
