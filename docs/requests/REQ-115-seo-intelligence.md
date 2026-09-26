# REQ-115 — SEO Intelligence & AI SEO Agent

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** modules/seo + `crates/ai-hub`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

SEO as an assistant, not a checklist.

- Site-wide SEO audit: titles, descriptions, headings, internal links, images alt, canonical, structured data.
- Weakness detection ranked by impact, with per-page fix suggestions.
- AI SEO agent: generate/rewrite meta content, propose internal links, flag thin pages.
- Keyword tracking panel (targets per page, current position, history) fed by imported/API data.
- Structured-data editor with JSON-LD validation per content type.

## Implementation spec

> **Module:** `modules/seo` (crate `omnion-module-seo`, workspace member) · **Migrations:** `0118_seo_intelligence.sql`, `0119_seo_keywords.sql` (reserved band 0116–0129 for the content-and-commerce wave; the ledger is append-only — take the next free number if taken) · **Admin routes:** `/seo/*` · **Depends on:** core crates (`permissions`, `audit`, `events`, `media`, `search`, `notifications`) · `crates/ai-hub` + REQ-097 for the agent, REQ-104 for cost caps · **Extends:** REQ-064 (the page SEO fields, redirects, sitemap and broken-link view stay there; this module audits, suggests and reports, and creates redirects through REQ-064's API).

### Scope (in / out)

**In**

- **Site-wide audit engine.** A scheduled (daily by default) and on-demand pass over one site's content: every published page plus drafts optionally, reading the content store (titles, descriptions, headings from the block tree when REQ-063 is installed), the internal link graph between pages, image usage with alt text from the media library (REQ-010), canonical fields, and the JSON-LD stored per page. A bounded link check fetches the site's own internal targets only (same host, `HEAD` with a `GET` fallback, rate-limited, robots-respecting, 2 s timeout, ≤ 5 redirects) and never crawls external sites.
- **Rule catalogue with deterministic scoring.** Rules: missing/too-short/too-long title; missing/too-short/too-long description; missing or multiple `h1`; heading level skips; thin content (word count under a per-content-type floor); or no internal links out; orphan page (no inbound internal links); broken internal target; image without alt; missing or non-self canonical; missing structured data for the content type; duplicate title or description across pages; URL that does not match its canonical. Each finding carries `rule_key`, severity (`critical`, `high`, `medium`, `low`), an impact score computed from severity × page importance (inbound links, traffic bucket from REQ-007 analytics when installed, or a neutral default) × fix effort, and a plain-language message with the offending value trimmed.
- **Findings list and per-page fix suggestions.** Filter by site, rule, severity, page, status (`open`, `fixed`, `ignored`, `snoozed`), and sort by impact; each row deep-links to the page editor (or the schema editor), suggests a concrete fix in one sentence, and offers `Fix now` (jumps to the field), `Ignore this rule for this page`, `Snooze 30 days`, `Resolved manually`. Resolutions are re-checked on the next audit, so a finding returns only if the underlying condition returns.
- **AI SEO agent.** Suggestion tasks through `crates/ai-hub`: rewrite or generate a title and description within length limits for one page or a selection; propose internal links from the search index (target page must exist and not already be linked); flag thin pages and propose an outline of sections to add; propose a short meta description for a product or post when commerce/blog are installed. Every suggestion is stored as a draft record with the model, prompt version, token cost and a diff preview; `Apply` writes through the page API under the caller's own permissions (`content.pages.update` / `seo.fix.apply`), records a revision and the audit entry. Applying is always an explicit, per-suggestion action.
- **Keyword tracking.** Keyword targets per page (`target keyword`, optional secondary keywords, target URL, locale, country, device), free-text entry plus CSV import, and a position history table fed by two honest sources: manual entry after a check, and an org-configured third-party rank-data API connection (generic adapter, base URL and credential by reference, no scraping of search engines). The panel shows current position, best position, 30/90-day change, and a per-keyword history chart; the "targets per page" view lists pages whose keyword targets are missing.
- **Structured-data editor.** JSON-LD templates per content type (Article, BlogPosting, FAQPage, Product, BreadcrumbList, Organization and a custom type), required and recommended fields per type, and a validation engine that parses the JSON, checks required fields, resolves their values against the page (empty value = error), and previews the exact script tag the renderer would emit. Pages can override a field or disable structured data entirely, with the reason recorded.

**Out**

- Redirect manager, sitemap and robots.txt editors, the broken-link view and the per-page SEO field storage → REQ-064; this module reads them and drives them through their API rather than duplicating tables.
- Search-engine crawling at scale, backlink profiles, domain authority metrics, competitor rank scraping and paid keyword databases.
- Publishing content or translating meta text → REQ-064 publish path and REQ-114 translation engine.
- Analytics collection and traffic reporting → REQ-007 (this module reads its buckets for impact ranking and an uninstall leaves the neutral default in place).

### Screens (UI)

| Route | Screen |
|---|---|
| `/seo` | Overview: health score with trend, findings by severity, top 10 highest-impact fixes, audit freshness, keyword movers |
| `/seo/audit` · `/seo/audit/{run_id}` | Audit runs list · run detail with per-rule and per-page results |
| `/seo/findings` | Findings table with filters, bulk resolve/ignore/snooze, deep links into the editor |
| `/seo/agent` | AI suggestion queue (proposals with previews, apply/dismiss, cost so far) |
| `/seo/keywords` · `/seo/keywords/{id}` | Keyword list with targets and positions · one keyword with history chart |
| `/seo/schema` | Structured-data templates per content type, validation report |
| `/seo/settings` | Audit schedule, rule thresholds and weights, rank provider connection, cost caps |

- **Overview.** A health score (0–100) computed from weighted open findings, a sparkline of the last 30 runs, four cards (critical, high, pages with no metadata, orphan pages), a "highest impact" list of ten fixes with `Fix now` links, and one `Run audit now` button. Empty state (no audit yet) shows the schedule and the same button.
- **Findings table.** Columns: Impact (score with sort), Severity badge, Page (link), Rule, Message (trimmed), Detected, Status, Actions. Filters: site, severity, rule, page type, status, date range, text search; saved as a view. Bulk: Ignore, Snooze 30 days, Mark resolved. Row drawer: the full message, the offending value, the suggested fix, the rule's severity, a `Fix now` deep link, and the history of this rule on this page.
- **Audit run detail.** Header with started/finished, duration, pages scanned (and skipped), rule counts; a per-rule table (Rule, Severity, Findings, Pages affected, New since last run) and a per-page table (Page, Score, Findings, Critical). `Re-run this page` and a CSV export of the run.
- **Agent queue.** Cards or rows: Page, Task (`meta_rewrite`, `meta_generate`, `internal_links`, `thin_page_outline`, `product_meta`), Proposal (side-by-side current vs proposed with length counters and a diff), Model, Cost, Status (`proposed`, `applied`, `dismissed`, `failed`). Actions: `Apply` (with a confirmation naming the revision it creates), `Edit then apply`, `Dismiss with reason`, `Regenerate`. The header shows the monthly cost against the REQ-104 cap and a link to raise it. A failed suggestion shows the provider error text.
- **Keywords.** Columns: Keyword, Target page, Locale, Country/Device, Position (current), Change 30d (up/down arrow), Best, Volume (only when the provider supplies it), Updated. Filters: site, locale, position band, tags, "no target page" and "not updated in 14 days". Row actions: `Log position`, `Open page`, `Open history`. Keyword detail: a line chart of positions over time (inverted axis so higher is better), a table of every logged observation (date, position, URL, source), and target page metadata side by side.
- **Schema editor.** Left: content type list with a completeness badge. Right: template JSON editor with line numbers, a field checklist (required first, showing whether the page resolves a value), validation output with line references, and a live preview of the rendered `<script type="application/ld+json">` block. Toolbar: `Validate`, `Reset to template default`, `Apply to N pages of this type`.
- **Settings.** Audit schedule (off / daily / weekly + hour in site timezone), thin-content word floors per content type, which rules are enabled, weight overrides for the health score, the rank provider connection (adapter, base URL, credential by reference shown as `••••`, `Test connection`), the agent's monthly cost cap and per-task model, and a keyword-refresh reminder interval.
- **States and mobile.** Skeletons on every table and the chart; empty states with the primary action (findings with zero rows inside a filter say so and offer `Clear filters`); an audit running shows live progress with pages scanned; a failed audit shows the error and retry. On mobile (<768 px) the findings table becomes cards (severity, page, message, impact), the agent queue shows one proposal at a time, tables scroll horizontally with sticky first column, and the schema editor opens read-only with a preview.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/seo/overview` | Health score, counts, freshness, movers | `seo.read` |
| POST | `/api/v1/seo/audits` | Start an audit (`site_id`, scope, `dry_run` count first) | `seo.audit.run` |
| GET | `/api/v1/seo/audits` · `/{id}` | Run list · run detail with per-rule and per-page results | `seo.read` |
| GET | `/api/v1/seo/findings` | Findings (filters, sort by impact, cursor) | `seo.read` |
| PATCH | `/api/v1/seo/findings/{id}` | Resolve, ignore (rule or page scope), snooze | `seo.fix.apply` |
| POST | `/api/v1/seo/findings/bulk` | Bulk resolve / ignore / snooze (≤ 200 ids) | `seo.fix.apply` |
| POST | `/api/v1/seo/agent/tasks` | Queue an agent task (page ids or filter, task kind) | `seo.agent.use` |
| GET | `/api/v1/seo/agent/suggestions` | Suggestion queue with previews and cost | `seo.read` |
| POST | `/api/v1/seo/agent/suggestions/{id}/apply` · `/dismiss` | Apply (writes a revision) · dismiss with reason | `seo.fix.apply` |
| GET · POST · PATCH · DELETE | `/api/v1/seo/keywords` (+ `/{id}`, `/import`, `/export`) | Keyword targets, CSV, position logging | `seo.keywords.manage` |
| GET | `/api/v1/seo/keywords/{id}/positions` | Position history for the chart | `seo.read` |
| GET · PUT | `/api/v1/seo/schema/profiles` (+ `/{content_type}`) | Structured-data templates per type | `seo.schema.manage` |
| POST | `/api/v1/seo/schema/validate` | Validate a JSON-LD payload or a page's stored value | `seo.schema.manage` |
| POST | `/api/v1/seo/schema/apply` | Apply a template to pages of one type | `seo.schema.manage` |
| GET · PUT | `/api/v1/seo/settings` | Schedule, thresholds, weights, provider, caps | `seo.settings.manage` |
| POST | `/api/v1/seo/settings/provider/test` | Rank provider connection test | `seo.settings.manage` |

`seo.read` already exists in the REQ-064 family and is reused; the new keys are `seo.audit.run`, `seo.fix.apply`, `seo.agent.use`, `seo.keywords.manage`, `seo.schema.manage` and `seo.settings.manage`, all denied to Editor by default except `seo.read` and `seo.audit.run`. Every route is site-scoped and organization-scoped; a site of another organization answers `404`.

### Data model

Migrations `0118_seo_intelligence.sql` (audits, findings, suggestions, schema) and `0119_seo_keywords.sql` (keywords, positions, provider) — additive, commented in the `0009` style.

```sql
seo_audits (id uuid pk, organization_id uuid not null, site_id uuid not null, trigger text not null
  check (trigger in ('manual','schedule','event')), status text not null default 'queued'
  check (status in ('queued','running','done','failed','cancelled')), include_drafts bool not null default false,
  pages_scanned int not null default 0, pages_skipped int not null default 0, findings_opened int not null default 0,
  findings_closed int not null default 0, health_score int, error text, started_at/finished_at timestamptz,
  created_by uuid -> users, created_at)
seo_findings (id uuid pk, organization_id uuid not null, site_id uuid not null, audit_id uuid -> seo_audits on delete set null,
  page_id uuid null -> pages on delete cascade, rule_key text not null, severity text not null
  check (severity in ('critical','high','medium','low')), impact_score int not null default 0,
  message text not null, offending_value text, suggestion text, status text not null default 'open'
  check (status in ('open','fixed','ignored','snoozed')), ignored_until timestamptz, resolved_by uuid -> users,
  first_seen_at/created_at/updated_at timestamptz)  unique (site_id, page_id, rule_key) where status = 'open'
seo_suggestions (id uuid pk, organization_id uuid not null, site_id uuid not null, page_id uuid not null -> pages on delete cascade,
  task text not null check (task in ('meta_generate','meta_rewrite','internal_links','thin_page_outline','product_meta')),
  payload jsonb not null default '{}', current_snapshot jsonb not null default '{}',
  status text not null default 'proposed' check (status in ('proposed','applied','dismissed','failed')),
  model text, provider_id uuid, cost_estimate numeric(10,4), tokens_in/tokens_out int, error text,
  revision_id uuid null -> page_revisions, decided_by uuid -> users, created_at/decided_at)
seo_schema_profiles (id uuid pk, organization_id uuid not null, content_type text not null, schema_type text not null,
  template jsonb not null default '{}', required_fields text[] not null default '{}', enabled bool not null default true,
  updated_by uuid -> users, created_at/updated_at)  unique (organization_id, content_type, schema_type)
pages += seo_schema_override jsonb, seo_schema_disabled bool not null default false  -- additive columns
seo_rank_providers (id uuid pk, organization_id uuid not null, adapter text not null, base_url text, secret_ref text not null default '',
  enabled bool not null default false, last_used_at timestamptz, last_error text, created_at/updated_at)
seo_keywords (id uuid pk, organization_id uuid not null, site_id uuid not null, phrase text not null, locale text not null,
  country char(2), device text not null default 'desktop' check (device in ('desktop','mobile','tablet')),
  page_id uuid null -> pages on delete set null, target_url text, tags text[] not null default '{}',
  volume int, difficulty numeric(5,2), current_position int, best_position int, last_checked_at timestamptz,
  created_by uuid -> users, created_at/updated_at)  unique (site_id, phrase, locale, country, device)
seo_keyword_positions (id bigint identity pk, keyword_id uuid not null -> seo_keywords on delete cascade,
  checked_at timestamptz not null default now(), position int, url text, source text not null
  check (source in ('manual','import','provider')), note text)  index (keyword_id, checked_at desc)
```

Checks: `health_score between 0 and 100`, `impact_score >= 0`, `position >= 1`, `best_position >= 1`, `tokens_* >= 0`. Indexes: `seo_findings_site_impact_idx (site_id, status, impact_score desc)`, partial `seo_findings_open_idx where status = 'open'`, `seo_audits_site_created_idx (site_id, created_at desc)`, `seo_suggestions (site_id, status, created_at desc)`, `seo_keywords (site_id, current_position nulls last)`, the positions index above. Findings are currently tri-state per page and rule; the partial unique index guarantees one open finding per (page, rule) so re-audits update rather than pile up. Deleting a page cascades its findings and suggestions; deleting an audit run keeps findings but drops the link.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `seo.audit.started` · `.completed` · `.failed` | Audit lifecycle | `audit_id`, `site_id`, `pages_scanned`, `findings_opened`, `health_score`, `error_code` |
| `seo.finding.opened` | A new open finding appears | `finding_id`, `page_id`, `rule_key`, `severity`, `impact_score` |
| `seo.finding.closed` | Fixed or resolved manually | `finding_id`, `page_id`, `rule_key`, `resolution` |
| `seo.suggestion.proposed` · `.applied` · `.dismissed` | Agent task outcomes | `suggestion_id`, `page_id`, `task`, `model`, `cost_estimate` |
| `seo.keyword.rank_updated` | A position observation moves the current position | `keyword_id`, `from_position`, `to_position`, `source` |
| `seo.schema.updated` | Template or per-page override change | `content_type`, `schema_type`, `scope` (`template`/`page`) |

Consumed: `content.page.published` (queues a single-page audit and refreshes the internal link graph), `content.page.unpublished`, `content.blocks.updated` (heading and link re-check), `media.deleted` (re-checks alt-text findings and schema image references), `product.updated` (product pages re-audited when commerce is installed), `approvals.request.decided` (a gated bulk apply releases). Webhook relevance: `seo.finding.opened` with severity `critical` feeds notification rules (REQ-021) and `seo.audit.completed` suits a weekly digest; payloads carry ids, rule keys and scores — never page bodies or keywords of another site.

### Acceptance criteria

- [ ] `cargo test -p omnion-module-seo` is green, covering every rule, the impact score formula, the schema validator and the health-score computation.
- [ ] Both migrations apply on a fresh and on a populated database without touching existing rows.
- [ ] An audit on a seeded site with deliberate defects (missing title, duplicate description, image without alt, orphan page, broken internal link, missing schema) reports exactly the expected findings with the expected severities.
- [ ] Every finding carries an impact score, and the list sorted by impact is deterministic for the same input.
- [ ] `Fix now` opens the exact field in the page editor, and editing the field makes the finding disappear on the next audit while a re-broken page reopens it.
- [ ] `Ignore` on one page suppresses that rule for that page only, and the same rule on other pages stays open.
- [ ] A broken internal link is detected with its target URL, and `Create redirect` opens REQ-064's redirect form pre-filled (proven end to end).
- [ ] An AI agent task over five pages produces five suggestions with current-vs-proposed previews and a cost estimate; `Apply` writes the field, creates one revision per page and records the audit entry; `Dismiss` never writes.
- [ ] Applying a suggestion without `content.pages.update` fails with 403 and writes nothing; `Regenerate` creates a new suggestion without touching the previous one.
- [ ] Keyword CSV import maps columns with a dry-run preview, refuses rows without a phrase or locale, and one valid keyword plus its target page round-trips through export.
- [ ] A logged position moves `current_position` and `best_position` correctly (a worse new position lowers current only) and appears on the history chart.
- [ ] The keyword panel flags keywords with no target page and keywords not updated in 14 days, and both filters narrow the table.
- [ ] The schema editor refuses invalid JSON with a line reference, refuses a template missing a required field, and applying a template to a content type updates the pages listed in the confirmation preview.
- [ ] The rendered public page emits the JSON-LD script exactly as the editor previewed it, and disabling it per page removes the tag while keeping the rest of the page intact.
- [ ] A scheduled audit runs at the configured hour in the site timezone and records `trigger = 'schedule'`.
- [ ] Cost cap: when the agent's monthly cap is reached, new tasks are refused with the cap named and a link to `/seo/settings`, and no provider call is made.
- [ ] All seven screens have real empty, loading, running and error states with zero high findings at desktop and 390 px.

### QA plan

The walkthrough extends `scripts/qa/walkthrough.cjs` with `/seo` (score, cards, `Run audit now`), `/seo/audit` and one run detail (per-rule and per-page tables populated), `/seo/findings` (filter by severity, sort by impact, open one drawer, `Ignore` one rule on one page, confirm the row leaves the filtered list), `/seo/agent` (queue a meta rewrite for two seeded pages with a fixture provider, open a proposal, `Edit then apply` one and `Dismiss` the other, then confirm the page revision and the field value), `/seo/keywords` (import a CSV with one invalid row, log a position, open the history chart), `/seo/schema` (validate an invalid JSON-LD, see the line error, fix it, apply to a type, then check the public page for the emitted script) and `/seo/settings` (change the schedule, set a cap of zero, confirm a new agent task is refused with a readable message). The visual check must see: a real health score with a trend, severity badges that are visually distinct, side-by-side meta previews with length counters, a position chart with points and labels, a JSON-LD preview rendered as real markup, and no screen showing raw keys or a disabled mystery control. Screenshots: `page-seo-overview`, `page-seo-findings`, `page-seo-agent`, `page-seo-keywords`, `page-seo-schema`.

### Slices

1. **Audit engine and findings.** Migration `0118` (audits, findings, schema), the rule catalogue with deterministic scoring, the bounded internal link check, the runner with retry and the scheduled trigger, `/seo/overview`, `/seo/audit` and `/seo/findings` with resolve/ignore/snooze and the deep links into the editor. *Done when:* acceptance 1–7 and 15 pass on a seeded site and a second run reports exactly the still-broken items.
2. **AI SEO agent.** Agent tasks through `crates/ai-hub` with prompt versions, suggestion storage, the apply path that writes a revision under the caller's permissions, dismissal with reason, cost capture against the REQ-104 cap, and the `/seo/agent` queue with previews. *Done when:* acceptance 8–9 and 16 pass and one real suggestion is applied to a page with the revision visible in history.
3. **Keywords, schema and settings.** Migration `0119`, keyword targets with CSV import/export and position logging, the history chart, the rank-provider connection, JSON-LD templates per content type with validation and apply, per-page overrides and the settings screen with schedule, thresholds and caps. *Done when:* acceptance 10–14 and 17 pass and the public page emits the schema the editor validated.

### Risks / notes

- **Rule noise kills this module.** The findings list is worthless if it is a wall of low-value items, so impact ranking is transparent (the formula and its inputs are visible in the drawer), every rule can be disabled or snoozed, and the health score never counts ignored findings.
- **Crawl-lite is bounded on purpose.** Only the site's own internal targets, rate-limited, small concurrency, respects `robots.txt`, and gives up on a target after a small retry budget — a self-inflicted load spike on the public site would be worse than the missing finding.
- **Findings compared honestly.** A re-audit updates the existing open finding instead of duplicating it, and a finding only closes when the check that opened it passes; `Resolved manually` is recorded as such rather than silently deleted.
- **The agent proposes, a person applies.** No path writes content without an explicit action from a caller who holds the content permission, suggestions always show a diff, and a rejected suggestion is kept with its reason for quality review.
- **Cost visibility.** Estimates are shown before a run, the cap stops work with a named limit, and per-suggestion cost is stored so the monthly figure reconciles with REQ-104.
- **Keyword data honesty.** The panel states where a number came from; the product never claims to measure positions itself and never scrapes search engines — imported and provider data are labelled respectively.
- **Schema validation scope.** The validator checks structure, required fields and value resolution against the page; it does not claim to satisfy a search engine's rich-result rules, and the UI wording says so.
- **Large sites.** Audits process pages in cursor-paginated chunks with a resumable cursor and per-chunk commits, so a cancelled or failed run leaves consistent state and can resume.
- **REQ-064 overlap discipline.** One family of `seo.*` permissions, one set of redirect/sitemap tables; this module must call REQ-064's APIs for fixes rather than writing its tables directly, or the two surfaces will drift.
