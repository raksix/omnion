# REQ-060 — Marketing

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** module (`modules/marketing`)
> **Source:** owner brief — business suite / frontend depth (docs/08-BUSINESS-SUITE.md, docs/03-FRONTEND.md)

## Request

Campaigns and audiences on top of CRM + CMS.

- **Segments**: rule-based audiences over contacts (tags, fields, activity, deal stage).
- **Campaigns**: e-mail campaigns with template editor, send test, schedule, throttling, unsubscribe handling.
- **Tracking**: opens/clicks per campaign, UTM builder, landing page link (CMS pages).
- **Forms → audience**: CMS forms write into segments (consent fields).
- **A/B-lite**: two subjects/templates with a shared metric.
- **Reports**: campaign performance, list growth, source attribution-lite.
- **Events**: `marketing.campaign.sent`, `marketing.form.submitted`.

## Implementation spec

### Scope (in / out)

**In**
- Segments: static (hand-picked) and dynamic (rule-based over tags, contact fields, activity recency, deal stage), with live member count, sampled preview and a materialised membership table refreshed on write and on a nightly sweep.
- Contacts port: marketing reads contacts through the CRM contract (`modules/crm`: contacts, tags, companies, deal stage). Until CRM ships, the port is served by a `marketing_contacts` mirror table fed by the contacts API and by CMS form submissions, so the module is testable on its own.
- Campaigns: e-mail campaigns with a block-based template editor, personalisation variables, send test, scheduled send with timezone, throttling (batch size + interval), pause/resume/cancel, unsubscribe handling with per-address suppression.
- Tracking: open pixel and click redirects, per-recipient state, link table with UTM parameters, landing-page links to CMS pages (REQ-064 pages).
- Forms → audience: a CMS form submission writes into the target segment with the consent field captured; consent state is stored per contact and honoured at send time.
- A/B-lite: two subject/template variants, split percentage, one shared primary metric (open rate default, click rate selectable), optional automatic pick of the winner at a configured sample size.
- Reports: campaign performance table, list growth over time, source attribution-lite (first-touch UTM source/medium → signups and submissions).
- Sending rides the platform SMTP path already in the automation crate's mail service; no third-party provider is required, and a provider adapter can be added later behind the same interface.

**Out**
- SMS and social channels (later), drips/journey orchestration (that is the automation engine's job — marketing emits events and ships two prebuilt rules as examples).
- Advanced deliverability tooling (DKIM/SPF/DMARC setup lives with the mail infrastructure), transactional e-mail (owned by the platform), full multi-touch attribution.
- Contact CRUD: contacts belong to CRM; marketing never edits a contact record, only segments and consent.

### Screens (UI)

| Route | Screen |
|---|---|
| `/campaigns` | Campaign list |
| `/campaigns/new` | Five-step campaign wizard |
| `/campaigns/<id>` | Campaign detail — overview, recipients, links, A/B, send log |
| `/segments` · `/segments/<id>/edit` | Segment list and rule builder |
| `/marketing/templates` · `/marketing/templates/<id>/edit` | E-mail template list and editor |
| `/marketing/reports` | Performance, growth and attribution reports |
| `/marketing/settings` | Sender identities, throttle defaults, UTM defaults, unsubscribe page |

- **Campaign list.** Columns: Name, Status (`draft`, `scheduled`, `sending`, `paused`, `sent`, `cancelled`), Audience, Sent, Open %, Click %, Scheduled at, Owner. Filters: status, date range, owner, segment. Bulk: Duplicate, Pause, Archive, Export report. Empty state offers `New campaign`; the sending state shows a progress bar driven by the send log.
- **Wizard.** Step 1 Details: name, subject, preheader, from name, from address (validated against the sender identity), reply-to, segment picker with live count. Step 2 Content: block editor (heading, text, image, button, divider, columns, CMS page teaser), variable palette (`{{first_name}}`, `{{company}}`, `{{unsubscribe_url}}`), preview desktop/mobile, plain-text fallback, "send test to" input accepting comma-separated addresses. Step 3 Delivery: send now / schedule with date-time and timezone, throttle batch size and interval, track opens and clicks toggles. Step 4 A/B (optional): variant B subject or template, split slider 10–50%, metric select, winner rule. Step 5 Review: recipients count, sample of ten recipients, suppression count (unsubscribed, bounced, consent missing), then `Schedule` or `Send now`. Validation blocks progress with field-level messages; a campaign cannot leave draft with zero recipients.
- **Campaign detail.** Overview tab shows KPI cards (delivered, unique opens, unique clicks, bounces, unsubscribes) and a timeline (created, scheduled, started, finished). Recipients tab: table (E-mail masked as `f***@example.com`, Name, Variant, Status, Sent at, First open, First click, Bounce kind) with filters and CSV export. Links tab: URL, UTM parameters (editable), clicks, unique clicks. A/B tab: variant comparison with a significance hint and an "apply winner" action when the rule is manual. Send log: append-only rows with batch numbers and errors.
- **Segment builder.** Left: rule rows with field picker (contact field, tag, activity recency, deal stage, source), operator and value; combiner `AND`/`OR` between groups (`ALL of` / `ANY of`); a live count refetches on change; right: sample table of the first 20 matching contacts. Header shows member count, last evaluated, kind (`static` / `dynamic`), and actions `Save`, `Re-evaluate`, `Export CSV`, `Copy to static`.
- **Reports.** Performance: sortable table (campaign, sent, delivered, open %, click %, unsubscribe %) with a date range and CSV export. Growth: line chart of subscribers/contacts per period with a source breakdown. Attribution-lite: table of UTM source/medium with form submissions and first-touch signups.
- **States, keys, mobile.** Sending failures surface a banner with the failing batch and a retry action. Keyboard: `c` new campaign, `j`/`k` list rows, `⌘Enter` in the wizard advances. On mobile the wizard steps become a vertical form with a sticky summary bar; tables become card lists; the builder stacks rules above the sample table.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET · POST | `/api/v1/marketing/segments` | List · create segment (rules or manual members) | `marketing.read` · `marketing.segments.manage` |
| GET · PUT · DELETE | `/api/v1/marketing/segments/{id}` | Read · update rules · delete | `marketing.read` · `marketing.segments.manage` |
| POST | `/api/v1/marketing/segments/{id}/preview` | Count + 20 sample members for a draft rule set | `marketing.read` |
| POST | `/api/v1/marketing/segments/{id}/members` | Add/remove manual members | `marketing.segments.manage` |
| GET · POST | `/api/v1/marketing/campaigns` | List · create (draft) | `marketing.read` · `marketing.campaigns.manage` |
| GET · PUT · DELETE | `/api/v1/marketing/campaigns/{id}` | Read · edit draft · delete draft | `marketing.read` · `marketing.campaigns.manage` |
| POST | `/api/v1/marketing/campaigns/{id}/test` | Send a test e-mail to given addresses | `marketing.send` |
| POST | `/api/v1/marketing/campaigns/{id}/schedule` | Schedule with timezone and throttle | `marketing.send` |
| POST | `/api/v1/marketing/campaigns/{id}/send` | Send now | `marketing.send` |
| POST | `/api/v1/marketing/campaigns/{id}/pause` · `/resume` · `/cancel` | Sending controls | `marketing.send` |
| GET | `/api/v1/marketing/campaigns/{id}/recipients` · `/links` · `/sends` | Recipient state · link table · batch log | `marketing.read` |
| GET · POST | `/api/v1/marketing/templates` | Template list · create | `marketing.read` · `marketing.campaigns.manage` |
| GET | `/api/v1/marketing/reports/{campaigns\|growth\|attribution}` | Report payloads | `marketing.reports.read` |
| GET · PUT | `/api/v1/marketing/settings` | Sender identities, throttle, UTM defaults | `marketing.read` · `marketing.settings.manage` |
| GET | `/api/v1/marketing/track/open/{token}` | Open pixel (1×1 gif, no auth) | — |
| GET | `/api/v1/marketing/track/click/{token}` | Click redirect to the stored URL with UTM (no auth) | — |
| GET · POST | `/api/v1/public/newsletter/{token}` | Unsubscribe page payload · confirm unsubscribe (no auth) | — |

Tracking endpoints are unauthenticated but carry only opaque per-recipient tokens; they never echo addresses, never set cookies, and are rate-limited per token and per IP.

### Data model

Migrations: `0105_marketing_segments.sql`, `0106_marketing_campaigns.sql` (reserved band 0100–0115; append-only ledger — take the next free number if taken).

```sql
-- 0105_marketing_segments.sql
marketing_contacts (id uuid pk, organization_id uuid not null, crm_contact_id uuid null unique,
  email text not null, name text, tags text[] not null default '{}', fields jsonb not null default '{}',
  deal_stage text null, last_activity_at timestamptz, source text, created_at/updated_at timestamptz)
  unique (organization_id, lower(email)); index (tags) using gin; index (organization_id, deal_stage)
marketing_segments (id uuid pk, organization_id uuid not null, key text not null, name text not null,
  kind text in ('static','dynamic') not null, rules jsonb not null default '[]',
  member_count integer not null default 0, last_evaluated_at timestamptz,
  created_by uuid null, created_at/updated_at timestamptz)
  unique (organization_id, key)
marketing_segment_members (segment_id uuid -> marketing_segments on delete cascade,
  contact_id uuid -> marketing_contacts on delete cascade, source text in ('manual','rule','form'),
  added_at timestamptz not null default now(), primary key (segment_id, contact_id))
  index (contact_id)
marketing_consents (id uuid pk, organization_id uuid not null, contact_id uuid not null,
  channel text not null default 'email', state text in ('granted','revoked') not null,
  source text not null, evidence jsonb, recorded_at timestamptz not null default now())
  index (contact_id, channel, recorded_at desc)
marketing_suppressions (id uuid pk, organization_id uuid not null, email text not null,
  reason text in ('unsubscribe','bounce_hard','complaint','manual'), created_at timestamptz)
  unique (organization_id, lower(email))
-- 0106_marketing_campaigns.sql
marketing_campaigns (id uuid pk, organization_id uuid not null, site_id uuid null, name text not null,
  status text in ('draft','scheduled','sending','paused','sent','cancelled') not null default 'draft',
  segment_id uuid null, template_id uuid null, from_name text, from_email text, reply_to text,
  subject text, preheader text, scheduled_at timestamptz, timezone text not null default 'UTC',
  batch_size integer not null default 200, batch_interval_seconds integer not null default 60,
  track_opens boolean not null default true, track_clicks boolean not null default true,
  ab_enabled boolean not null default false, ab_split_percent integer null check (between 10 and 50),
  ab_metric text in ('open','click'), ab_winner_at timestamptz, created_by uuid null,
  started_at/finished_at timestamptz, created_at/updated_at timestamptz)
  index (organization_id, status, scheduled_at); index (status, scheduled_at) where status = 'scheduled'
marketing_campaign_variants (id uuid pk, campaign_id uuid not null on delete cascade,
  variant text in ('a','b'), subject text, body_blocks jsonb not null default '[]',
  plain_text text, split_percent integer not null default 100)  unique (campaign_id, variant)
marketing_campaign_sends (id uuid pk, campaign_id uuid not null on delete cascade, variant text,
  contact_id uuid null, email text not null, status text in
  ('queued','sent','delivered','bounced','failed','suppressed') not null default 'queued',
  token text not null unique, sent_at/delivered_at/first_open_at/first_click_at/unsubscribed_at timestamptz,
  bounce_kind text, error text, batch_no integer)
  unique (campaign_id, variant, email); index (campaign_id, status); index (token)
marketing_campaign_events (id bigserial pk, campaign_id uuid not null, send_id uuid not null,
  kind text in ('open','click','bounce','complaint','unsubscribe') not null,
  link_id uuid null, at timestamptz not null default now(), ip_hash text, user_agent_hash text)
  index (campaign_id, kind, at desc)
marketing_links (id uuid pk, campaign_id uuid not null on delete cascade, url text not null,
  utm jsonb not null default '{}', clicks integer not null default 0, unique_clicks integer not null default 0)
```

### Events

| Event | When | Payload sketch |
|---|---|---|
| `marketing.campaign.sent` | A campaign finished its last batch | `campaign_id`, `segment_id`, `sent`, `delivered`, `bounced` |
| `marketing.campaign.started` · `.scheduled` | First batch began · scheduled | `campaign_id`, `scheduled_at` |
| `marketing.form.submitted` | A CMS form submission landed (REQ-064 form) | `form_key`, `submission_id`, `segment_id`, `consent` |
| `marketing.contact.unsubscribed` | Unsubscribe recorded | `contact_id`, `campaign_id`, `reason` |
| `marketing.contact.bounced` | Hard bounce recorded | `contact_id`, `campaign_id`, `bounce_kind` |
| `marketing.segment.members_changed` | Membership changed by rules or manual edit | `segment_id`, `added`, `removed`, `source` |

Consumed: `crm.contact.created` / `crm.contact.updated` (mirror refresh), `crm.deal.stage_changed` (rule inputs), `content.form.submitted` (form → audience), `documents.page.published` (link a landing page to a campaign). Webhook relevance: `marketing.campaign.sent` and `marketing.contact.unsubscribed` are the integration points for external analytics; suppression logic never leaks addresses — payloads carry contact ids and hashed e-mail hints only where a downstream system genuinely needs them.

### Acceptance criteria

- [ ] A dynamic segment with `tag = lead AND last_activity_at > 30 days` returns the members the preview counted, within one second on 5k contacts.
- [ ] Re-evaluating a segment after a contact changes adds and removes members and emits `marketing.segment.members_changed` with correct counts.
- [ ] A static segment keeps manual members across evaluations and rejects rule edits with a clear message.
- [ ] Campaign creation validates from address against a configured sender identity and blocks with a field-level error otherwise.
- [ ] Send test delivers to the given addresses only, with variant A content, and never touches the recipient list.
- [ ] A scheduled campaign starts within a minute of its `scheduled_at` (timezone respected) and can be paused mid-send and resumed from the last batch.
- [ ] Throttling holds: with batch 200 / 60 s the send log shows batches no closer than the interval.
- [ ] Opens and clicks record exactly one first open / first click per recipient and appear in the links table with UTM parameters applied to the redirect target.
- [ ] Unsubscribe from the e-mail footer lands on the public page, records consent `revoked`, adds a suppression row, and the next campaign reports it under "suppressed" instead of sending.
- [ ] A CMS form submission with the marketing target writes a member into the chosen segment and records consent `granted` with its source.
- [ ] A/B: variant B receives the configured split percentage (within 2 points over 200 sends), and the report shows both variants with the shared metric.
- [ ] "Apply winner" is offered only after the sample size is reached and, when enabled, freezes further splitting.
- [ ] Reports load for an empty database without errors and show real numbers after one sent campaign; CSV export returns the same rows as the table.
- [ ] Tracking endpoints set no cookies, log no addresses and are rate-limited; a tampered token yields 404 not 500.
- [ ] Suppressed, bounced and consent-missing addresses are excluded before queueing and counted in the review step.
- [ ] All screens render at 390 px with no horizontal scroll and pass the walkthrough without high findings.

### QA plan

The walkthrough must visit `/segments`, create a segment through the rule builder (change a rule and confirm the count changes), save it, open `/campaigns`, run the five wizard steps to a scheduled state, open the campaign detail and click every tab, run a test send against the seeded SMTP sink, hit pause/resume, open `/marketing/templates` and its editor, open `/marketing/reports` and export a CSV, and open `/marketing/settings` and save a throttle value. It must also open the seeded public unsubscribe page. Visual check: the KPI cards show real numbers after the seeded campaign, the recipients table renders masked addresses and variant badges, the preview pane shows the template with the site logo, and the mobile wizard stacks without clipped controls. The pause state must be visible as a banner, not a dead button.

### Slices

1. **Segments and contacts.** Migration `0105_marketing_segments.sql`; contacts port + mirror, rule evaluation with preview, static/dynamic segments, member screen, consent and suppression tables. *Done when:* acceptance 1–3 pass and `/segments` is in the walkthrough inventory.
2. **Campaigns and sending.** Migration `0106_marketing_campaigns.sql`; campaign CRUD, block template editor with variables, test send, scheduling, throttle worker, pause/resume/cancel, unsubscribe page, suppression at queue time. *Done when:* acceptance 4–7 and 9 pass.
3. **Tracking and A/B.** Open pixel, click redirect with UTM, links table, recipient state, variant split, winner rule. *Done when:* acceptance 8, 11, 12, 14, 15 pass.
4. **Reports and forms wiring.** Reports endpoints and screens, CSV exports, CMS form → segment target with consent, consumed events, and the two example automation rules (form → CRM lead → e-mail; open → sales notification). *Done when:* acceptance 10, 13 pass and the events appear in the events screen with a successful delivery.

### Risks / notes

- Deliverability is the real failure mode: throttling defaults must be conservative, hard bounces must suppress automatically, and the module must never send without an unsubscribe link and a recorded consent state.
- Tracking links are the privacy-sensitive surface: per-recipient tokens are opaque, hashed IP/user agent only, and no third-party pixels are embedded in templates by default.
- The contacts port keeps CRM decoupled; until REQ-051 ships, the mirror table is the source and switching to CRM must be a port swap, not a rewrite.
- Dynamic segments are refreshed on write plus a nightly sweep; the UI must say "last evaluated" so a stale count is never mistaken for live truth.
- A/B statistical claims stay descriptive ("variant B is ahead"), never "significant" without a real test — the significance hint must be labelled as an approximation.
- Turkish example copy belongs to seeded templates and the public unsubscribe page only (for example a confirmation line such as "Kaydınız güncellendi."), never in code paths or API strings.
