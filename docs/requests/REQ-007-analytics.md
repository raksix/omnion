# REQ-007 — Analytics

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** module (`modules/analytics`)
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

- [ ] `0012_analytics.sql` applies on a fresh and on a populated database; `cargo test -p omnion-module-analytics` is green.
- [ ] A beacon to `/public/analytics/collect` appears in realtime within 5 seconds and in the overview after the rollup tick.
- [ ] Cookieless mode sets no cookie and writes no `localStorage` entry.
- [ ] With `anonymize_ip` on, no row stores a raw address (`ip_prefix` is null) and IPv4/IPv6 truncation matches the documented widths.
- [ ] DNT and GPC requests are dropped before any row is written and increase the filtered counter.
- [ ] Rollup idempotency: running a bucket twice leaves `analytics_daily` identical (byte-for-byte comparison in a test).
- [ ] Overview numbers match a seeded fixture for visitors, page views, conversions and forms across a custom range.
- [ ] Comparison mode returns the previous equal-length period; a range without prior data shows “no comparison” instead of zero.
- [ ] Page-report filters combine (path + device + country + source), sorting and paging stay consistent, and the CSV contains exactly the filtered rows.
- [ ] A three-step goal reports monotonically non-increasing funnel counts with per-step drop-offs.
- [ ] Goal hits are deduplicated per visitor per step — re-sending the same beacon does not double-count.
- [ ] Excluded paths and IPs produce no rows; changing the lists affects new hits only.
- [ ] Retention purge deletes rows older than the cutoff for one site, writes `analytics_purges` with the removed count, and never touches another site.
- [ ] `DELETE /analytics/visitors/{hash}` removes every visit, pageview, event and goal hit for that visitor and emits `analytics.erasure_completed`.
- [ ] The collect endpoint answers 429 above the rate limit and stays responsive under a burst test.
- [ ] Permission guards answer 401/403/200 as documented and an organization cannot read another organization’s sites; all ten screens have empty, loading and error states with zero high findings and a clean mobile pass.

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
