# REQ-007 — Analytics

> **Status:** done (e085129…16a600a) — all four slices shipped · **Captured:** 2026-09-25 · **Layer:** module (`modules/analytics`)
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

An in-platform analytics engine:

- Page views
- Visitors
- Referrers
- Devices
- Browsers
- Countries
- Events
- Conversion
- Forms
- Downloads

Dashboard example:

```text
Visitors        124,832
Page Views      481,920
Conversions       8,421
Forms             1,284
```

A privacy-friendly analytics option should be available as well.

## Implementation spec

> **Module:** `modules/analytics` (crate `omnion-module-analytics`, workspace member) · **Migration:** `database/migrations/0012_analytics.sql` (next free slot at build time) · **Admin routes:** `/analytics/*` · **Permission family:** `analytics.*` · **Tracking surface:** `POST /api/v1/public/analytics/collect` plus a snippet the site renderer loads · **Depends on:** `crates/identity` (site resolution), `crates/events` (automations + webhooks), `modules/forms` (REQ-064) and `modules/ecommerce` (REQ-008) for server-side conversions.

### Scope (in / out)

**In**

- **Collection:** one batched beacon per page for pageviews, custom events, downloads, outbound clicks and scroll depth; form submissions and paid orders arrive as server-side conversions from their own modules.
- **Cookieless by default:** the visitor identifier is a daily-salted hash of `(site, IP, user agent, salt)` — nothing is written to the browser, and with IP anonymization on no raw address is stored (IPv4 to `/24`, IPv6 to `/48`). A cookie mode exists per site for cross-day counting only when explicitly enabled; `Do Not Track` and `Global Privacy Control` are honoured in both modes.
- **Bot filtering** at ingest (user-agent list plus a rate heuristic) with a per-day `filtered` counter, so dashboards never count crawler noise.
- **Rollups:** hourly and daily rollup tables maintained by a worker from raw rows, idempotent per bucket (re-running a bucket changes nothing); dashboards read rollups, detail reports read raw rows inside the retention window.
- **Reports:** overview (KPI cards, series, previous-period comparison), pages, sources/referrers/UTM, audience (devices, browsers, operating systems, languages, countries), events with property breakdown, downloads, forms, conversions/goals with ordered-step funnels (2–5 steps) and a realtime view of the last 30 minutes over SSE.
- **Goals:** a conversion is a `pageview`, `event`, `download` or `form_submit` match, optionally an ordered step list; hits are deduplicated per visitor per step.
- **Export:** CSV/JSON of any report with the active filters, streamed; a scheduled e-mail digest is out of scope.
- **Privacy operations:** retention days, sample rate, excluded paths, excluded IPs, a per-site tracking switch, “erase this visitor” (deletes every row for one hash), and a purge job that records each run.
- **Events for the rest of the platform:** goals reached, traffic spikes, purge and erasure completions.

**Out (tracked elsewhere)**

- Session recording, heatmaps and per-field form drop-off → REQ-064; A/B testing and campaign automation → REQ-060; revenue attribution → REQ-008 (a matched `order.paid` is recorded as a conversion here).
- Custom BI reports and dashboards → REQ-028 / REQ-027; infrastructure metrics → REQ-014; cross-site identity, audience profiles and ad-network integrations are explicitly out — Omnion never profiles a person across sites.

### Screens (UI)

Nav: **Overview · Pages · Sources · Audience · Events · Downloads · Forms · Goals · Realtime · Settings** (the site switcher scopes every screen).

| Route | Screen |
|---|---|
| `/analytics` | Overview: KPI cards (Visitors, Page views, Conversions, Forms, Downloads), timeseries, top pages, top sources, device split |
| `/analytics/pages` | Page report |
| `/analytics/sources` | Referrers and UTM table (source/medium/campaign/term/content) |
| `/analytics/audience` | Devices, browsers, OS, screen sizes, languages, countries |
| `/analytics/events` | Custom events with counts, unique visitors and value sums |
| `/analytics/downloads` | Downloads by file and by page |
| `/analytics/forms` | Submissions, completion rate, abandonment |
| `/analytics/goals` | Goal list, funnel editor, per-goal conversion rate |
| `/analytics/realtime` | Live counters for the last 30 minutes |
| `/analytics/settings` | Tracking and privacy settings, snippet, retention, exclusions, purge, erasure |

**Shared toolbar** — date range (`Today`, `Yesterday`, `7 days`, `30 days`, `12 months`, `Custom` with two pickers), a `Compare` toggle (previous period as a muted series), a granularity select enabled only when the range allows it, `Export CSV`, and `Refresh` with a last-updated time.

**Overview** — KPI cards show value, delta badge versus the comparison period and a sparkline; the chart is a line/area with legend, hover tooltip and keyboard-focusable points; side panels list the top five pages and sources with `View all` links; empty state reads “No data yet — add the tracking snippet” and links to Settings.

**Report tables** — pages: `Page` (path + link), `Title`, `Views`, `Visitors`, `Views per visitor`, `Avg time`, `Bounce rate`, `Entrances`, `Exits`; sources: `Source`, `Medium`, `Campaign`, `Visits`, `Visitors`, `Conversions`, `Conversion rate` with a `Group by` selector; audience: four bar panels plus a countries table (`Country`, `Visitors`, `Views`, `Share`); events: `Event`, `Count`, `Unique visitors`, `Value sum`, `Last seen` with a property-breakdown drawer; goals: `Goal`, `Kind`, `Match`, `Conversions`, `Rate vs visitors`, `Last hit`, `Enabled`. Filters combine (path, title, device, country, source, date) and every table sorts, pages (50 rows) and opens a detail drawer with the same series.

**Goals editor** — vertical step list (add/remove/reorder, kind + match per step), live funnel preview for the chosen range, validation for 2–5 steps with no empty match and unique positions.

**Realtime** — counters for the last 5 and 30 minutes, a live current-pages table and a compact event feed; the pill switches to `Paused` when the tab is hidden.

**Settings** — tracking (`Enabled`, `Mode` cookieless/cookie-with-consent, `Sample rate 1–100`, `Bot filter`), privacy (`Anonymize IP`, `Respect DNT/GPC`, `Retention 7–1080` days), exclusions (path and IP textareas with per-line validation and a glob preview), snippet (read-only code block, copy button, resolved site id), data (`Run purge now` naming the cutoff, `Erase visitor` by hash with typed confirmation) and a “what we store” table (column, purpose, personal data yes/no).

**States, keyboard, mobile** — skeletons on load, a real empty state per panel (never an unexplained zero chart), error state with retry and request id; `/` focuses search, `d` opens the date range, `c` toggles compare, `r` refreshes, `e` exports, `g` then `o|p|s|a` jumps to overview/pages/sources/audience; below `lg` cards stack, charts scroll horizontally and tables become card lists.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| POST | `/api/v1/public/analytics/collect` | Batched beacon: pageviews + events (`sendBeacon`, `text/plain`) | public, rate limited per site/IP |
| GET | `/api/v1/analytics/overview` | KPIs + series for a range, with comparison | `analytics.read` |
| GET | `/api/v1/analytics/timeseries` | Metric series with explicit granularity | `analytics.read` |
| GET | `/api/v1/analytics/pages` · `/pages/{path}/series` | Page report and one page’s series | `analytics.read` |
| GET | `/api/v1/analytics/sources` | Referrers, mediums, campaigns, UTM | `analytics.read` |
| GET | `/api/v1/analytics/audience` | Devices, browsers, OS, screens, languages, countries | `analytics.read` |
| GET | `/api/v1/analytics/events` | Event counts and property breakdown | `analytics.read` |
| GET | `/api/v1/analytics/downloads` | Downloads by file and page | `analytics.read` |
| GET | `/api/v1/analytics/forms` | Submissions, completion, abandonment | `analytics.read` |
| GET | `/api/v1/analytics/realtime` · `/realtime/stream` | Snapshot of the last 30 minutes / SSE counters | `analytics.read` |
| GET, POST | `/api/v1/analytics/goals` | Goal list with rates / create a goal with steps | `analytics.read` / `analytics.goals.manage` |
| PATCH, DELETE, GET | `/api/v1/analytics/goals/{id}` · `/funnel` | Update or delete a goal / step funnel for a range | `analytics.goals.manage` / `analytics.read` |
| GET | `/api/v1/analytics/export` | CSV/JSON of a report with the active filters | `analytics.export` |
| GET, PUT | `/api/v1/analytics/settings` | Tracking, privacy and retention settings of a site | `analytics.read` / `analytics.settings.manage` |
| GET | `/api/v1/analytics/snippet` | The snippet with the resolved site id | `analytics.read` |
| POST | `/api/v1/analytics/purge` | Run the retention purge now (audited) | `analytics.settings.manage` |
| DELETE | `/api/v1/analytics/visitors/{hash}` | Erase every row of one visitor | `analytics.settings.manage` |

### Data model

```text
analytics_visits(id bigint identity pk, site_id uuid not null, visitor_hash text not null, started_at timestamptz not null,
  last_seen_at timestamptz not null, pageview_count integer not null default 0, is_bounce boolean not null default true,
  entry_path text, exit_path text, referrer_host text, referrer_path text, source text, medium text, campaign text,
  term text, content text, device_type text, os text, browser text, screen_width integer, screen_height integer,
  language text, country_code char(2), ip_prefix inet)                       -- ip_prefix null when anonymized
analytics_pageviews(id bigint identity pk, site_id uuid not null, visit_id bigint not null, path text not null, title text,
  occurred_at timestamptz not null, duration_ms integer, scroll_depth smallint, is_entry boolean not null default false,
  is_exit boolean not null default false)
analytics_events(id bigint identity pk, site_id uuid not null, visit_id bigint, name text not null, path text,
  value numeric(14,2), properties jsonb not null default '{}'::jsonb, occurred_at timestamptz not null)
analytics_daily(site_id uuid not null, day date not null, metric text not null, dimension_kind text not null,
  dimension_value text not null, count bigint not null default 0, unique (site_id, day, metric, dimension_kind, dimension_value))
analytics_hourly(-- same shape with bucket timestamptz; keeps the last 48 hours queryable)
analytics_goals(id uuid pk, site_id uuid not null, name text not null, kind text not null, match jsonb not null default '{}'::jsonb,
  enabled boolean not null default true, created_by uuid, created_at timestamptz not null default now())
analytics_goal_steps(goal_id uuid not null, position integer not null, kind text not null, match jsonb not null, pk (goal_id, position))
analytics_goal_hits(id bigint identity pk, goal_id uuid not null, visitor_hash text not null, step_position integer not null,
  occurred_at timestamptz not null, unique (goal_id, visitor_hash, step_position))
analytics_settings(site_id uuid pk, tracking_enabled boolean not null default true, mode text not null default 'cookieless',
  anonymize_ip boolean not null default true, respect_dnt boolean not null default true, bot_filter boolean not null default true,
  sample_rate integer not null default 100, retention_days integer not null default 180, excluded_paths text[] not null default '{}',
  excluded_ips cidr[] not null default '{}', updated_by uuid, updated_at timestamptz)
analytics_purges(id uuid pk, site_id uuid, kind text not null, cutoff timestamptz, rows_removed bigint not null default 0,
  actor_user_id uuid, created_at timestamptz not null default now())
```

Checks: `sample_rate between 1 and 100`, `retention_days between 7 and 1080`, `device_type in ('desktop','mobile','tablet','other')`, `count >= 0`; goal kinds and purge kinds constrained. Indexes: `analytics_pageviews_site_occurred_idx (site_id, occurred_at desc)`, `analytics_pageviews_site_path_idx (site_id, path, occurred_at desc)`, `analytics_visits_site_started_idx (site_id, started_at desc)`, `analytics_visits_visitor_idx (site_id, visitor_hash, started_at desc)`, `analytics_events_site_name_idx (site_id, name, occurred_at desc)`, GIN on `analytics_events.properties`, `analytics_daily_site_day_idx (site_id, day desc, metric)`.

Migration `database/migrations/0012_analytics.sql` — append-only, commented in the `0005` style, seeds one `analytics_settings` row per existing site; plan monthly partitioning of `analytics_pageviews`/`analytics_events` behind the same table names before the first million rows.

### Events

- Emitted: `analytics.goal_reached` (goal, opaque visitor id, path, value), `analytics.traffic_spike` (hourly visitors above 3× the trailing 7-day median), `analytics.retention_purged`, `analytics.erasure_completed`.
- Consumed: `page.published` refreshes the path → title map used by reports; `form.submitted` (REQ-064) and `order.paid` (REQ-008) are recorded as conversions when a matching goal exists; `site.created` seeds settings.
- Webhook relevance: `analytics.goal_reached` is the marketing automation trigger (REQ-060) and can start a workflow; `analytics.traffic_spike` feeds system-health notifications; `analytics.erasure_completed` is a compliance record. Payloads never carry a raw IP or a form field value.

### Acceptance criteria

- [x] The analytics migration applies on a fresh and on a populated database (shipped as
  `0015_analytics.sql` — 0012–0014 were taken by search and the command centre); the module's
  own suite is green (`cargo test -p omnion-module-analytics` → 22/22). Fresh proof: `omnion
  migrate` against an empty database applied all fourteen migrations (`1…15`, 0011 was never used)
  and left the eleven `analytics_*` tables behind; populated proof: the walk suites apply the same
  migrations to the development database on every run.
- [x] A beacon to `/public/analytics/collect` appears in realtime within 5 seconds and in the overview after the rollup tick.
  The realtime half is proven twice: `…::realtime_reads_the_last_half_hour_and_respects_the_scope`
  posts one beacon and reads the counters, the current pages and the feed back from
  `/analytics/realtime`, and the walkthrough measures the same thing over HTTP — a beacon posted
  after the goal exists is visible in the counters **18 ms** later (`realtimeLatencyMs`), and the
  screen itself renders the live pill (`data-rt-state="live"`), the five-minute counter (`1`), the
  pages table (`2` rows) and the feed (`4` events). The overview half has been proven since slice 2
  (`analytics_runner` rebuilds the buckets on its timer and the overview reads them).
- [x] Cookieless mode sets no cookie and writes no `localStorage` entry: `apps/web/public/analytics.js`
  touches no storage API and the collector writes nothing to the browser (the visitor identifier is
  computed server-side).
- [x] With `anonymize_ip` on, no row stores a raw address (`ip_prefix` is null) and IPv4/IPv6 truncation matches the documented widths.
- [x] DNT and GPC requests are dropped before any row is written and increase the filtered counter.
- [x] Rollup idempotency: running a bucket twice leaves `analytics_daily` identical (snapshot comparison in a test).
- [x] Overview numbers match a seeded fixture for visitors, page views, conversions and forms across a custom range.
  Proven by `apps/api/tests/analytics.rs::the_overview_matches_the_seeded_fixture_and_compares_with_the_period_before`:
  four visitors over six pageviews, one goal conversion, one form, one download — and the visitor
  who came back on a second day is still one visitor (4, not 5).
- [x] Comparison mode returns the previous equal-length period; a range without prior data shows “no comparison” instead of zero.
  The same walk: `previous_has_data = false` and `previous = 0` while nothing precedes the window,
  then one seeded visitor a period earlier makes the delta real and rides beside the matching
  bucket; the screen renders `data-analytics-no-comparison` for the empty case.
- [x] Page-report filters combine (path + device + country + source), sorting and paging stay consistent, and the CSV contains exactly the filtered rows.
  `…::the_page_report_filters_sorts_pages_and_exports_exactly_its_rows`: three paths, six
  pageviews, the combined filter leaving two rows, `per_page=1&page=2` landing on the second row of
  the same ordering, and an export whose three lines (header + two rows) hold the filtered paths
  and not the table — with the row count in `x-export-rows`, and `analytics.export` refusing the
  reader who may only read.
- [x] A three-step goal reports monotonically non-increasing funnel counts with per-step drop-offs.
  `…::goals_record_ordered_deduplicated_hits_and_report_their_funnel` seeds four visitors (one
  completes all three steps in a single beacon, one stops after the second, one never enters, one
  skips the middle step) and reads the funnel back as **3 · 2 · 1** with drop-offs `0 · 1 · 1`,
  conversions `1`, rate `0.25` — and asserts the sequence is monotone, because a funnel that grows
  is not a funnel. The same walk proves the next line too.
- [x] Goal hits are deduplicated per visitor per step — re-sending the same beacon does not double-count.
  The same walk: a completed beacon is posted twice, the second answer carries
  `reached_goals: []` and the hit count stays at six — the unique `(goal, visitor, step)` key and
  the `on conflict do nothing` write are what make a funnel a count of visitors rather than of
  requests. A switched-off goal records no hits at all, and deleting a goal takes its steps and
  hits with it (both asserted in the same walk).
- [x] Excluded paths and IPs produce no rows; changing the lists affects new hits only.
- [x] Retention purge deletes rows older than the cutoff for one site, writes `analytics_purges` with the removed count, and never touches another site.
  `…::the_purge_and_the_erasure_remove_exactly_their_rows_and_say_so` sets a seven-day retention,
  seeds a thirty-day-old visit (with its pageview and its event) beside a two-day-old one on one
  site and a thirty-day-old visit on another: after `POST /analytics/purge` the stale rows are gone,
  the recent visit and the other site's visit are intact, the audit row carries `kind = retention`,
  the cutoff and `rows_removed = 3`, and a salt four hundred days old is pruned with them. The QA
  walkthrough repeats it on the populated QA database — the screen named the cutoff
  (`2026-09-20`), the rows past it went from 1 to 0, and the screen's own answer read
  "Removed 4 rows older than 2026-09-20 — 1 visits, 1 page views, 2 events, 0 goal hits."
- [x] `DELETE /analytics/visitors/{hash}` removes every visit, pageview, event and goal hit for that visitor and emits `analytics.erasure_completed`.
  The same walk seeds one handle on two sites (a visit with a second pageview, an event and a goal
  hit on the first): the erasure answers `1 visit · 2 pageviews · 1 event · 1 goal hit`, the
  handle's rows on its own site go to zero while the other site keeps its own, a second run answers
  `rows_removed: 0` instead of failing (an erasure that fails on a retry is a compliance problem of
  its own), a handle that is not a hash is a `400 invalid_visitor`, and the event reached a
  subscribed endpoint over HTTP — the receiver captured `analytics.erasure_completed` with the
  handle and the count. The walkthrough erases a real handle on the QA database and counts its rows
  there before (1) and after (0).
- [x] The collect endpoint answers 429 above the rate limit and stays responsive under a burst test
  (a full budget of beacons is served in-process before the 429; the limiter is per instance, see
  the slice log).
- [x] Permission guards answer 401/403/200 as documented and an organization cannot read another organization’s sites; all ten screens have empty, loading and error states with zero high findings and a clean mobile pass.
  (The guards and the organization isolation are proven for the settings, snippet, all seven
  report endpoints, the goal CRUD, the funnel and both realtime routes: a member without
  `analytics.read` gets 403 on the goal list and the realtime snapshot, a reader without
  `analytics.goals.manage` gets 403 on create, another organization's reader gets 403, the
  platform Owner reads across, and a missing goal is a `404 goal_not_found` while an unknown
  sort key or day is a `400 invalid_report_query`. Slice 4 added the write half: the purge and
  the erasure answer `403` to a reader, a member and another organization's reader, `400
  invalid_visitor` to a handle that is not a hash, and `200` to the manager — with the audit row
  naming the actor. The screens half closed with the same tick: `/analytics/settings` joined the
  walkthrough on desktop and on a 390 px viewport, and the pass reads all ten screens with 436
  clicks, 433 screenshots, **0 high findings** and **0 vision issues**
  (`qa-artifacts/20260927-002830`).)

### QA plan

Extend `scripts/qa/walkthrough.cjs` with `/analytics`, `/analytics/pages`, `/analytics/sources`, `/analytics/audience`, `/analytics/events`, `/analytics/downloads`, `/analytics/forms`, `/analytics/goals`, `/analytics/realtime`, `/analytics/settings` (desktop) and `/analytics`, `/analytics/goals` (mobile). The script first posts a synthetic batch to the public collect endpoint (fixed test paths such as `/qa/landing`, one download, one form submit, one custom event, two device types, three countries) so every panel has data, then switches the range to `7 days`, toggles `Compare`, opens a page drawer, creates a two-step goal and reads its funnel, runs an export, and in Settings flips tracking off and back on and submits an invalid retention value to see the field error. Realtime is verified by reloading after the batch and asserting a non-zero counter.

What the visual check should see: a drawn series with readable axis labels (nothing clipped or rotated off-canvas), KPI cards with delta badges, audience bars with labels and percentages, a three-step funnel, the snippet block with a copy control, and genuine empty states on a second site with no traffic — screenshots `page-analytics-overview`, `page-analytics-pages`, `page-analytics-audience`, `page-analytics-goals`, `page-analytics-realtime`, `mobile-analytics-overview`.

### Slices

1. **Ingest + storage + settings.** Migration, collect endpoint (validation, rate limit, bot/DNT/IP policy), raw and rollup tables, idempotent rollup worker, settings and snippet APIs, permission keys. Done when a beacon lands in raw and rolled-up rows, a rerun changes nothing, and settings validation is enforced.
2. **Overview + reports.** Overview screen and the six report screens with filters, paging, drawers and CSV export. Done when the fixture test matches and each report renders both populated and empty states in the QA pass.
3. **Goals, funnels, realtime.** Goal CRUD with steps, hit recording from events/pageviews/downloads/server-side conversions, funnel endpoint and screen, realtime SSE counter. Done when a three-step funnel shows correct drop-offs and the counter ticks in the browser.
4. **Privacy operations.** Retention purge with an audit row, visitor erasure, exclusions, sampling, the “what we store” table and the events (`traffic_spike`, purge, erasure). Done when purge and erasure remove exactly the intended rows on a populated QA database and their events reach a subscribed endpoint.

### Risks / notes

- **Migration number** is “next free slot at build time”; the file stays additive, which matters as this is the first high-volume table set.
- **Volume:** the collect endpoint is a public write path — enforce body size, per-site quota and drop unknowns silently instead of erroring; plan partitioning before production traffic.
- **Privacy claims must match the code:** cookieless mode, daily salt rotation, IP truncation and DNT handling are the promise, and a reviewer must see the same thing in the settings row that the code does.
- **Timezone:** a “day” is the site’s timezone; rollups and reports must share that boundary or numbers disagree by an hour.
- **High-cardinality dimensions** (paths, campaign names) can explode rollup rows — cap distinct values per dimension per day and fold the rest into `(other)`.
- **Realtime streams** are polling under the hood: cap concurrent SSE connections per organization and close idle ones.
- **Configured-but-silent conversions:** a goal with no hits for 30 days should show a hint, since the most likely cause is a mismatch with the emitting module’s event name.

### Slice 1 — shipped (ingest, storage, settings)

Delivered in one tick:

- **`database/migrations/0015_analytics.sql`** — the tables of the data model plus `analytics_salts`
  (the day's salt lives there and nowhere else; it is what rotates the visitor hash at midnight),
  the schema checks, the indexes and one `analytics_settings` row per existing site.
- **`modules/analytics`** (crate `omnion-module-analytics`, the first member of the `modules/` tree
  docs/04-MONOREPO.md reserves for features) — `collect` (validation, bot filter, DNT/GPC,
  exclusions, sampling, sessionisation, event storage), `rollup` (idempotent hourly/daily buckets
  with the dimension cap), `settings` (read/validate/write + the snippet), `visitor` (daily-salted
  hash, `/24`–`/48` truncation, exclusion rules) and `agent` (device/OS/browser/crawler reading).
  22 unit tests, no database needed.
- **API** — `POST /api/v1/public/analytics/collect` (public: 64 KB body cap, per-site-and-caller
  budget, site resolved exactly as the public renderer resolves it), `GET`/`PUT
  /api/v1/analytics/settings?site_id=` (`analytics.read` / `analytics.settings.manage`,
  organization-scoped) and `GET /api/v1/analytics/snippet?site_id=`. The rollup worker
  (`crate::analytics_runner`, `OMNION_ANALYTICS_*`) rebuilds the recent buckets on a timer.
- **Permissions** — `analytics.read`, `analytics.export`, `analytics.goals.manage`,
  `analytics.settings.manage` in the catalogue; Manager, Moderator and Editor base roles carry the
  read key and the Manager the rest.
- **The tracker** — `apps/web/public/analytics.js`: cookieless, no storage, sends the pageview,
  queued custom events, downloads and outbound clicks; it stops before any request when the browser
  reports `Do Not Track` or `Global Privacy Control`.

Deliberate choices and deviations, for the reviewer:

- The migration is **0015**, not 0012: the number is the next free slot at build time, and search
  and the command centre took 0012–0014 while this request waited.
- A day is the **UTC** day until REQ-113 adds per-site timezones; rollups and reports read the same
  boundary, which is what the note under Risks asks for.
- The collect endpoint's limiter is **per process** (a fixed window in memory), so a fleet of API
  instances each allow the budget: it is a guardrail against a runaway script, not a billing meter.
  Redis-backed counters are the follow-up when the edge deployment lands.
- `X-Forwarded-For` is honoured (the platform's own edge sets it) for the daily hash, the exclusion
  lists and the rate limit — never for authorization. A deployment that does not strip inbound
  headers lets a caller choose their own bucket, which the deployment documentation must say.
- The tracker measures engagement as a `page_engagement` **event** (path, duration, scroll depth),
  not as a second pageview: a second pageview beacon for the same page would double-count the page
  view, and a report that counts a page twice is worse than one without a duration.
- "Realtime" is not built yet: a beacon is visible in the raw rows and in the rollups that slice 2's
  overview will read.

Proof (this tick): `cargo test --workspace --no-fail-fast` → **524 passed, 0 failed** (analytics:
22 module units + 6 integration walks) · `cargo clippy --workspace --all-targets -- -D warnings` →
clean · `pnpm typecheck && pnpm build` → 2/2 · `bash scripts/qa/run.sh` → 105 clicks, 117
screenshots, **0 high findings**, **0 vision issues** (`qa-artifacts/20260926-202816`; the 5 medium
findings are the public renderer's own icon 404s, carried forward unchanged). No new screen ships in
this slice, so the walkthrough inventory is unchanged — slice 2 adds the ten `/analytics` routes to it.

Next: **slice 2** — the overview and the six report screens over these rollups, with the shared
date-range toolbar, filters, drawers and CSV export.

### Slice 2 — shipped (overview + the six report screens)

- **`modules/analytics/src/reports.rs`** — the read side in one place: the range (inclusive UTC
  days, at most 366 of them), the five filters, a `Narrowing` builder that writes only the clauses
  a report actually applies (so a filter that cannot narrow a report never appears as a no-op in
  its SQL), the overview, the page report (sorted by a whitelist, paged), the sources report with
  its group-by modes, the audience panels and countries, events and their property breakdown,
  downloads by file and by page, forms with completion and abandonment, and the CSV renderer.
  Fourteen unit tests cover the arithmetic that cannot be wrong in production: range maths, bucket
  lists, filter validation, the sort whitelist, ratios without a denominator, CSV quoting.
- **Series buckets are addressed by an offset from the range start**, not by a truncated
  timestamp: `date_trunc` follows the connection's time zone, and a report that shifts by an hour
  depending on who asks is worse than no report.
- **`apps/api`** — `GET /analytics/{overview,pages,pages/series,sources,audience,events,
  events/{name},downloads,forms}` behind `analytics.read`, and `GET /analytics/export` behind
  `analytics.export`, all resolving the site through the caller's own organization. The export
  answers the same rows the screen shows (same filters, same sort) with the row count in
  `x-export-rows`, because a silently truncated file lies.
- **The seven screens** (`apps/admin/features/analytics`): one toolbar (Today / Yesterday / 7 days
  / 30 days / 12 months / custom pickers, comparison switch, granularity select enabled only where
  the range allows hours, export, refresh with a last-updated caption), all of it held in the URL,
  plus the spec's keyboard (`d`, `c`, `r`, `e`, `g` then `o|p|s|a`). The overview's empty state
  hands over the tracking snippet rather than an empty chart; a range older than retention says
  its numbers come from the daily rollups; a comparison with no traffic behind it says
  "no comparison" instead of drawing a delta against zero.
- **The collector now stores the country an edge reported** (`CF-IPCountry`, `X-Vercel-IP-Country`,
  `X-Country-Code`): the audience report's countries column was always going to be `(unknown)`
  without it, and Omnion still never geolocates an address itself. The edges' own placeholders
  (`XX`, `T1`) are refused — a country that is not a country is not a place.
- **The walkthrough grew the section** (`scripts/qa/walkthrough.cjs`): it posts a synthetic batch
  to the public collect endpoint (four visitors: two device types, three countries, one download,
  one form submit with its start, one custom event with a value), spreads a third of those rows
  over the last thirty days through the disposable QA database so the series has a shape, then
  walks and clicks all seven screens, switches to 7 days, turns the comparison on, opens a page
  drawer and downloads a real CSV.

Deviations and choices, for the reviewer:

- `GET /analytics/pages/series` takes `?path=` rather than the spec's `/pages/{path}/series`: a URL
  path is not a path, and `/pages/%2Fpricing/series` would make every router on the way a
  participant in the encoding question.
- The overview reads the **raw rows** while the range is inside the site's retention window — that
  is where "distinct visitors across a period" is a real number — and reads the **daily rollups**
  past it, answering `exact: false` so the screen can say so. The rollup path sums per-day
  distinct counts, which counts a returning visitor once per day; that is a property of the
  rollup, not of the visitor, and the screen names it.
- The events report's property breakdown is `jsonb_each_text` over the property object: values are
  rendered as text, which is what a breakdown needs, and nothing there is a form field value by
  design (REQ-064 owns forms' own field data).
- Countries and screens are the only audience dimensions read outside the rollups; everything the
  rollups carry (pageviews by path, visitors by device/browser/OS/country/language/referrer) has
  the same numbers in both paths, which the integration walks check on a seeded fixture.

### Slice 3 — shipped (goals, funnels, realtime)

- **`modules/analytics/src/goals.rs`** — the conversion side of the engine: validation (a name of
  1–120 characters, one to five steps, and a match that means something per kind: a pageview needs
  a path, an event its name, a download a file or a path, a form its name or a path), CRUD with the
  steps written in one transaction, and the funnel arithmetic. A goal mirrors its **last** step in
  its own row (`kind` + `match`), so a list query and a single-step goal stay the same shape.
- **The recorder is ordered and deduplicated.** `record_facts` walks the earliest *missing* step
  only: a fact that matches step three while step two is unrecorded records nothing, and one beacon
  can carry a visitor over several steps because it holds several facts. Every write is an
  `on conflict (goal_id, visitor_hash, step_position) do nothing`, and the answer reports only the
  hits that were actually written — which is why a re-sent beacon answers `[]` and why the platform
  event can be emitted exactly once. `record_conversion` is the hook REQ-064 (forms) and REQ-008
  (orders) will call with the visitor hash they already computed.
- **A funnel counts a step as *reached*** by the visitor whose furthest position inside the range is
  that step or beyond. That is what keeps the counts monotonically non-increasing when a visitor's
  earlier step happened before the window began; the alternative (counting hits at position *p*)
  would let step two exceed step one. A furthest position beyond the funnel (a goal shortened after
  the fact) counts at the last step instead of being dropped.
- **`modules/analytics/src/realtime.rs`** — the last five and thirty minutes read straight from the
  raw rows: visitors, pageviews, events and goal hits, the current pages, and the event feed. No
  rollup stands between a beacon and the counter, which is what makes the screen *realtime*; the
  event's `value` is cast to `float8` in SQL, because `numeric` is not a float and a feed that fails
  on the first value only fails in production (it did, in the first QA pass of this slice).
- **API** — `GET`/`POST /analytics/goals`, `GET`/`PATCH`/`DELETE /analytics/goals/{id}`,
  `GET /analytics/goals/{id}/funnel`, `GET /analytics/realtime` and
  `GET /analytics/realtime/stream`. Reading is `analytics.read`; writing is
  `analytics.goals.manage`. A `PATCH` merges: the switch changes the switch, `kind`/`match` change
  the last step, and `steps` replaces the funnel — a screen never re-sends what it did not show.
- **The stream** is server-sent snapshots every five seconds with a keep-alive ping, capped at eight
  live streams per site through a guard moved into the stream itself: when the browser drops the
  connection the slot is free, and a reader over the cap gets a `429` while the screen keeps its
  last snapshot. `EventSource` reconnects on its own, and one stream lives at most half an hour.
- **`analytics.goal_reached`** reaches the platform bus the moment the last step is recorded, with
  the goal, the step, the opaque daily visitor handle, the path and the value — never an address and
  never a form field. A bus that cannot record the fact warns instead of failing the beacon: the
  visit is already stored, and a marketing automation that misses one conversion must not also lose
  the traffic behind it.
- **The two screens** (`apps/admin/features/analytics`): `/analytics/goals` — the list (kind, match,
  conversions, rate vs visitors, last hit, switch, edit, delete) with an editor that holds an
  ordered step list (add, remove, reorder), validates each step before sending, and shows the
  selected goal's funnel with its per-step drop-offs; `/analytics/realtime` — the two windows, the
  current pages and the feed, kept fresh over the stream, and the pill says `Paused` while the tab
  is hidden (the stream is closed, not left polling). Both screens join the section's navigation,
  and below `lg` the tables become cards like every other report.
- **The walkthrough** (`scripts/qa/walkthrough.cjs`) grew the pass: it opens the editor, tries to
  save a step without a match (refused on screen), describes a two-step funnel, creates it, posts a
  beacon that completes it *after* the goal exists, reloads, reads the funnel back (`2` steps, `1`
  · `1`, conversions `1`) and measures the beacon-to-counter latency over HTTP (18 ms), then walks
  `/analytics/realtime` and reads the live pill, the five-minute counter, the pages table and the
  feed. A request the browser itself cancelled (`net::ERR_ABORTED`) is counted, not reported: every
  screen drops in-flight fetches when the URL state changes, and the realtime stream ends when the
  tab goes away.

Deliberate choices and deviations, for the reviewer:

- A funnel is a **same-day** funnel under cookieless counting: the visitor hash rotates at
  midnight, so a returning visitor is a new visitor tomorrow here as everywhere else in this
  module. Cookie mode (the site-level switch) is what extends it across days. The REQ's own note
  under Risks is the reason the screen says nothing about "unique users across the funnel" — it
  says *visitors*.
- The funnel's step counts are **reached** counts, and the drop-off is the difference to the step
  before; the first step's drop-off is always zero because nothing precedes it. The screen shows
  `—` for a rate whose range met nobody, never `0%`.
- `PATCH` with `steps` replaces the funnel wholesale (positions are renumbered from 1), because a
  partial step list would leave the positions ambiguous; the goal row's mirror is rewritten from
  the new last step in the same transaction.
- The realtime stream cap is per instance, like the beacon limiter: the shared counter arrives with
  the Redis-backed limiter. The cap is a guardrail against a wall of forgotten tabs, not a limit on
  readers.

Proof (this tick): `cargo test --workspace --no-fail-fast` → **556 passed, 0 failed** (analytics:
33 module units — 11 of them the goal and funnel arithmetic — plus 11 integration walks, two of
them new) · `cargo clippy --workspace --all-targets -- -D warnings` → clean · `pnpm typecheck &&
pnpm build` → 2/2 · `bash scripts/qa/run.sh` → 404 clicks, 406 screenshots, **0 high findings**, 0
vision issues (`qa-artifacts/20260926-231356`; the 5 medium findings are the public renderer's own
icon 404s, carried forward unchanged). The depth pass reads the section end to end: the goal editor,
a refused empty match, a two-step funnel created through the screen, its funnel read back, the
live realtime pill and counters, and a beacon-to-counter latency of **18 ms**.

Next: **slice 4** — the privacy operations (retention purge with its audit row, visitor erasure,
exclusions and sampling in the screen, the "what we store" table and the `/analytics/settings`
screen) and the events they emit (`analytics.traffic_spike`, `analytics.retention_purged`,
`analytics.erasure_completed`).

### Slice 4 — shipped (privacy operations)

- **`modules/analytics/src/privacy.rs`** — the three promises in one file, so a reviewer reads them
  together. The **retention purge** removes one site's rows past the cutoff (pageviews first, so
  they can be counted, then events, then the visits they belonged to, then the site's goal hits)
  and writes its `analytics_purges` row *inside the same transaction*: an audit trail that can be
  missing while the rows are gone is not an audit trail. The cutoff is midnight (UTC) of the day
  `retention_days` back — whole days, so two runs an hour apart agree on the window, and the screen
  can name it before the button is pressed.
- **The salts are the one shared table**, so the prune is the one statement not scoped by site: a
  salt is only read while it is the current day (the collector hashes against *today's* salt), and
  the purge removes only those past the **longest** retention any site still asks for — one site's
  seven-day window can never delete a salt another site's window still covers.
- **`erase_visitor`** removes every row a handle appears in — its visits (and their pageviews),
  its events, its goal hits — and is idempotent by construction: the statements remove what is
  there, so a second call removes nothing and still writes its audit row, because an erasure that
  fails on a retry would be a compliance problem of its own. A handle that is not 64 lower-case
  hexadecimal characters is a `400`, never an erasure of something else.
- **`STORED_FIELDS`** is the "what we store" table the screen renders — one row per column family
  with its purpose and whether it is personal data — living in the module, so the screen cannot
  describe a different schema than the code runs. `purge_history`/`last_purge` read the audit
  trail, and `detect_spike` answers the trailing-seven-day-median question in the same place.
- **API** — `POST /analytics/purge` and `DELETE /analytics/visitors/{hash}` behind
  `analytics.settings.manage` (the permission that decides how long data lives is not the one that
  reads it); both answer with what they removed and both record their event on the bus. The
  settings payload gained `purge_cutoff`, `last_purge` and `storage`, so the screen never guesses
  any of the three.
- **Events** — `analytics.retention_purged` and `analytics.erasure_completed` carry the audit id,
  the counts and (for the erasure) the handle, which is already a pseudonym; `analytics.traffic_spike`
  is announced by the **rollup worker**: the hour that just closed against the median of the seven
  days before it, once per hour, with the guard reading the recorded events rather than a counter
  that a restart would lose. A site is only asked once per completed hour, and a failing watch is a
  warning that never stops the rollups.
- **The screen** (`apps/admin/features/analytics/settings-view.tsx`, `/analytics/settings`):
  tracking (enabled, mode, sample rate, bot filter), privacy (anonymize, DNT/GPC, retention),
  exclusions with a live glob preview that reads each pattern back ("starts with", "ends with",
  "exactly"), the snippet with its site key and a copy control, the data section (the cutoff named
  before the purge, and the erasure requiring the handle typed twice) and the storage table. Field
  validation mirrors the server's of the same field, so a refused value appears beside the input
  that sent it. The section shell learned a **toolbar-less mode** for it: a screen with no date
  range and nothing to export renders neither, and its keyboard keeps only the section navigation.

Deviations and choices, for the reviewer:

- The purge runs **on demand** (the audited `POST /analytics/purge`) and prunes raw rows, the
  salts and the goal hits; the **rollups stay**, because they hold counts and no handle — that is
  what "aggregated counts name nobody" means on the screen, and it is why the reports can still
  say `exact: false` for a range older than the window.
- The erasure answers `200` with `rows_removed: 0` when the handle has no rows here: idempotent on
  purpose, and distinguishably different from "erased a visitor" without a second endpoint.
- The walkthrough's own depth pass drives this screen, so its save, purge and erasure controls
  carry `data-qa-guard` and the generic click pass skips them — a sample value in the erasure field
  is a refused request (a finding), not a click, and the harness now says so out loud.

Proof (this tick): `cargo test --workspace --no-fail-fast` → **562 passed, 0 failed** (analytics:
52 module units — four of them the new cutoff, handle, storage-table and hour-label arithmetic —
plus 13 integration walks, two of them new) · `cargo clippy --workspace --all-targets -- -D
warnings` → clean · `pnpm typecheck && pnpm build` → 2/2 · `bash scripts/qa/run.sh` → 436 clicks,
433 screenshots, **0 high findings**, **0 vision issues** (`qa-artifacts/20260927-002830`; the 5
medium findings are the public renderer's own icon 404s, carried forward unchanged). The
walkthrough's settings pass: tracking saved and read back after a reload, a retention of 3 refused
with the field named, the purge named its cutoff (`2026-09-20`) and took the QA database's old
rows from 1 to 0, and a real handle was erased with its rows counted there before (1) and after
(0). The one finding this slice introduced — four switches with no accessible name — was fixed in
the same tick and the pass re-run to prove it.

Next: **REQ-006 (IAM)** — the next wave-1 item: user/role/permission screens plus sessions and
devices.
