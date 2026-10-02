# REQ-117 — Forms → CRM Lead Pipeline

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** modules/website + modules/crm
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

The website/business integration that sells the platform.

- Website form builder (REQ-064) fields mapped to CRM lead fields with validation.
- Request-a-quote form → Lead → Opportunity → Quotation → Customer (the documented flow).
- Assignment rules (owner by round-robin/territory/product) and SLA timers.
- UTM and source capture stored on the lead; deduplication against existing contacts.
- Notification to the owner, autoresponder to the submitter, and full audit trail.

## Implementation spec

> **Where:** split by install — `modules/website` carries capture (public intake endpoint, consent, spam heuristics) and `modules/crm` carries the lead lifecycle (record, assignment, SLA, dedupe, conversion) · **Migration:** `0121_crm_lead_intake.sql` (reserved band 0116–0129 for the content-and-commerce wave; the ledger is append-only — take the next free number if taken) · **Admin routes:** `/crm/leads/*`, `/crm/settings/intake`, `/crm/settings/assignment`, `/crm/settings/sla` · **Permission family:** `crm.leads.*`, `crm.intake.manage`, `crm.sla.manage` · **Depends on:** core crates (`permissions`, `audit`, `events`, `notifications`, `workflows`) + `modules/crm` (REQ-051 contacts, companies, deals, activities) · **Bridges:** REQ-064 forms emit the submission, REQ-052 turns an opportunity into a quotation, REQ-060 owns campaign attribution reporting, REQ-008 links the customer record when commerce is installed.

### Scope (in / out)

**In**

- **Intake sources.** A source is the binding between a capture surface and the CRM: a REQ-064 form (validated through its API by `form_key`), a keyed inbound endpoint for hand-written or third-party forms (`/api/v1/crm/intake/{source_key}` with a per-source signing key stored hashed and shown once), or a manual paste/import. Each source holds the field mapping, the consent requirement, the dedupe policy, the target pipeline and stage, the assignment rule, the SLA policy, the autoresponder template and an active toggle.
- **Field mapping with validation.** Mapping is a ordered list of target CRM field → source key, with per-field transforms (`trim`, `lowercase`, `title_case`, `strip_html`, `e164_lite`, `split_full_name`), a required flag and a fallback constant. Targets: contact first/last name, e-mail, phone, company name/domain, job title, message/notes, product interest, quantity or budget band, preferred contact time, consent text, custom fields when REQ-026 definitions exist. Saving validates that every required target has a source and that each mapped source key exists on the form; a rename on the form side is detected by a binding health check that flags broken mappings instead of silently dropping data.
- **Lead capture.** One `crm_leads` row per accepted submission with the raw payload retained (bounded size), the mapped values, first-touch and last-touch attribution (UTM source/medium/campaign/term/content, referrer host, landing path, click id, and the source page path), consent text as accepted, spam heuristics' outcome (honeypot, submit-too-fast, rate-limited), and a dedupe verdict. A rejection (spam or invalid) is stored with its reason so the inbox can show what was discarded rather than losing it.
- **Deduplication.** Policy per source: `link` (attach the lead to the best existing contact), `create_anyway` (always a new contact), or `reject_duplicate` (store as duplicate with a pointer to the match). Matching order: normalized e-mail, then digits-only phone, then company domain plus name similarity; every match decision records which key matched and the score. Merging contacts later re-points their leads (`crm.contact.merged`).
- **Assignment rules.** Ordered rules evaluated top-down: conditions (country/region from the address or phone prefix, product interest, budget band, source, form, language) with a target (`specific user`, `round-robin pool`, `team queue` when REQ-071 is installed). Round-robin advances a persisted pointer atomically so two simultaneous leads cannot take the same slot twice in a row beyond the pool size; a lead that matches no rule lands in an unassigned queue visible on the overview. Reassignment keeps a full history.
- **SLA timers.** Policies per source or per rule: first-response target in minutes, business-hours-only clock (a simple per-organization window with weekend flags, no holiday calendar in v1), escalation after a breach (notify the assignee's manager or a configured fallback owner), and a reminder before the deadline. The lead carries `first_response_due_at`, `first_response_at` and a computed state (`on_track`, `at_risk`, `breached`, `met`); the first response is logged by any of: an activity logged against the lead, a quotation sent from it, or an explicit `Mark as responded`.
- **The documented flow.** Request-a-quote form → Lead → Opportunity → Quotation → Customer: `Convert` creates or links the contact, creates a deal in the source's pipeline and stage (an "opportunity") with the mapped amount band as the initial amount, optionally opens a REQ-052 quotation draft with the product interest pre-filled, and the quotation's acceptance promotes both the deal and the lead. When the quote is accepted and commerce is installed, the linked customer is created through REQ-008's customer path. Each step is a separate audited action with its own permission.
- **Notifications and audit.** Assignment notifies the owner through REQ-021 (and e-mail through REQ-119 templates when installed), an SLA breach notifies the escalation target, and the autoresponder — content from a template with the submitter's name and the source's details, never free text typed by an operator in the request — is sent once per lead. Every state change writes an audit entry with actor, before/after and request id, and the lead detail renders the same trail as a timeline.

**Out**

- The form builder itself, its inbox, spam heuristics and export → REQ-064; this request consumes its submission event and reuses its honeypot and rate limits.
- Contact, company, deal and activity tables and their CRUD → REQ-051; quotation documents, price lists and approval gates → REQ-052; e-mail campaign bodies and tracking → REQ-060.
- Marketing attribution reporting and segments → REQ-060 (this request stores the attribution fields and emits events; the reporting surface stays there).
- Telephony/CTI call logging, live chat capture, and inbound e-mail parsing → later integrations (REQ-015); no scam or enrichment provider lookups.

### Screens (UI)

| Route | Screen |
|---|---|
| `/crm/leads` | Lead inbox (table with SLA state, assignment, dedupe verdict) |
| `/crm/leads/{id}` | Lead detail: payload, attribution, match panel, timeline, conversion stepper |
| `/crm/leads/duplicates` | Duplicate queue with merge/link/reject actions |
| `/crm/settings/intake` · `/crm/settings/intake/{id}` | Source list · source editor (mapping, dedupe, autoresponder, health) |
| `/crm/settings/assignment` | Assignment rules with ordered evaluation and a simulator |
| `/crm/settings/sla` | SLA policies with targets, business hours and escalation |
| `/website/forms/{id}` (additive tab) | REQ-064 form editor gains a `Lead delivery` card linking to the bound source |

- **Lead inbox.** Columns: Received (relative, with the absolute in a tooltip), Contact (name + e-mail), Source, Product interest, Owner (avatar, or an `Unassigned` badge), SLA (state badge with the due time), Status (`new`, `assigned`, `contacted`, `qualified`, `converted`, `duplicate`, `spam`, `rejected`), Duplicate hint. Filters: source, status, owner (me / unassigned / pick), SLA state, date range, product interest, tag, text search across name/e-mail/message. Sort default is SLA due time ascending (breached first). Bulk: assign, reassign, mark responded, mark spam, reject with reason, export CSV. Empty state explains the intake endpoint and offers `Copy intake URL` for the first source.
- **Lead detail.** Header: contact identity, source, status, owner selector, SLA chip with countdown. Left column: the submission payload (every answer as submitted, with the consent text quote), attribution (first touch, last touch, referrer, landing path, click id), spam verdict, and the raw payload toggle. Right column: the match panel (candidate contacts with the matched key and score, `Link to this contact`, `Create new`), the conversion stepper (Lead → Opportunity → Quotation → Customer with the state of each step and the linked records), and the activity timeline (assignment, response, notification sent, autoresponder sent, conversion, edits) rendered as one merged trail. Footer actions: `Mark responded`, `Convert`, `Reject` (reason required), `Delete` (permission-gated, audited).
- **Duplicates.** Columns: New lead, Existing contact, Matched key, Score, Received, Decision (`link`, `new`, `merge`, `reject`). Actions: `Link`, `Merge into existing` (calls REQ-051's merge with the lead's values pre-filled), `Keep separate`, `Reject duplicate`. A merge shows the field-by-field preview before committing.
- **Source editor.** Four sections — **Surface** (form picker or keyed endpoint with `Copy URL` and a one-time key reveal plus `Rotate`), **Mapping** (ordered target list with transforms, required flags and fallbacks; a live preview resolves a sample submission), **Rules** (dedupe policy, target pipeline/stage, assignment rule, SLA policy, tags to apply, consent required toggle with the consent text), **Autoresponder** (template picker, subject, send delay, `Send test to myself`). A `Test mapping` action runs a pasted or sample payload through the mapping and shows the produced contact/lead fields without writing anything. Health line: last received, last error, broken mappings count.
- **Assignment and SLA settings.** Rules table with drag order: Name, Conditions (summary chips), Target (user or pool), Active. The simulator takes a sample payload (country, product, budget, source) and shows which rule wins and to whom the lead would go, matched against the last 50 real leads so an operator can see the effect before saving. SLA: policies table `Name`, `Applies to` (source/rule), `First response within`, `Business hours only`, `Reminder`, `Escalate to`, `Active`, plus the organization business-hours window (days, start, end, timezone) and a note that holidays are out of scope in v1.
- **States and mobile.** Skeletons on every table; the inbox shows a live count badge for breached leads; empty states name the next action; the source editor's mapping table becomes a stacked card list on mobile with drag replaced by move up/down; the detail page stacks the payload above the timeline and keeps `Mark responded` and `Convert` in a sticky bar. Errors (broken mapping, form deleted behind a binding) surface as a banner on the source row with the reason and a `Revalidate` action.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/crm/leads` | Inbox list (filters, sort by SLA, cursor) | `crm.leads.read` |
| GET · PATCH · DELETE | `/api/v1/crm/leads/{id}` | Detail (payload, matches, timeline) · edit · delete (audited) | `crm.leads.read` · `crm.leads.manage` |
| POST | `/api/v1/crm/leads/{id}/assign` | Assign or reassign with a reason | `crm.leads.assign` |
| POST | `/api/v1/crm/leads/{id}/respond` | Mark the first response (logs the SLA stop) | `crm.leads.manage` |
| POST | `/api/v1/crm/leads/{id}/convert` | Create/link contact, deal and optional quotation draft | `crm.leads.convert` |
| POST | `/api/v1/crm/leads/{id}/reject` · `/spam` | Reject with a reason · mark as spam | `crm.leads.manage` |
| GET | `/api/v1/crm/leads/duplicates` · `/metrics` | Duplicate queue · inbox metrics (breached, unassigned, median response) | `crm.leads.read` |
| GET · POST | `/api/v1/crm/intake/sources` | Source list with health · create | `crm.intake.manage` |
| GET · PATCH · DELETE | `/api/v1/crm/intake/sources/{id}` | Read, update, disable or delete a source | `crm.intake.manage` |
| POST | `/api/v1/crm/intake/sources/{id}/test` | Run a sample payload through the mapping (no writes) | `crm.intake.manage` |
| POST | `/api/v1/crm/intake/sources/{id}/rotate-key` | Rotate the endpoint key (revealed once) | `crm.intake.manage` |
| POST | `/api/v1/crm/intake/{source_key}` | Inbound submission from a keyed endpoint (rate limited, honeypot field honoured) | source key |
| GET · POST | `/api/v1/crm/assignment/rules` | Rules list · create | `crm.intake.manage` |
| PATCH · DELETE · PUT | `/api/v1/crm/assignment/rules/{id}` (+ `/order`) | Update, delete, reorder | `crm.intake.manage` |
| POST | `/api/v1/crm/assignment/simulate` | Evaluate a sample payload against the active rules | `crm.intake.manage` |
| GET · PUT | `/api/v1/crm/sla/policies` | SLA policies and business hours | `crm.sla.manage` |

The keyed endpoint authenticates by the hashed source key, applies the same honeypot, minimum-fill-time and per-IP rate limits as REQ-064's public forms, answers `202` with a lead reference and never discloses whether a matching contact exists. Every other route is organization-scoped; a lead of another organization answers `404`.

### Data model

Migration `0121_crm_lead_intake.sql` — additive, commented in the `0009` style; seeds one `Web default` SLA policy (first response within 240 minutes, business hours off) and one assignment rule (`Default → unassigned queue`) per existing organization.

```sql
crm_intake_sources (id uuid pk, organization_id uuid not null, site_id uuid null, name text not null,
  kind text not null check (kind in ('form','endpoint','import')), form_key text, endpoint_key_hash text,
  endpoint_key_hint text, mapping jsonb not null default '[]', required_targets text[] not null default '{}',
  consent_required bool not null default true, consent_text text, dedupe_policy text not null default 'link'
    check (dedupe_policy in ('link','create_anyway','reject_duplicate')), pipeline_id uuid, stage_id uuid,
  assignment_rule_id uuid null -> crm_assignment_rules on delete set null, sla_policy_id uuid null -> crm_sla_policies on delete set null,
  auto_tags text[] not null default '{}', autoresponder jsonb not null default '{}', active bool not null default true,
  rate_limit_per_hour int not null default 30, last_received_at timestamptz, last_error text,
  broken_mappings text[] not null default '{}', created_by uuid -> users, created_at/updated_at)
  unique (organization_id, name), unique (organization_id, form_key) where form_key is not null
crm_leads (id uuid pk, organization_id uuid not null, site_id uuid null, source_id uuid -> crm_intake_sources on delete set null,
  status text not null default 'new' check (status in ('new','assigned','contacted','qualified','converted','duplicate','spam','rejected')),
  contact_id uuid null -> crm_contacts on delete set null, company_id uuid null -> crm_companies on delete set null,
  deal_id uuid null -> crm_deals on delete set null, quote_id uuid null, owner_user_id uuid null -> users on delete set null,
  first_name text, last_name text, email citext, phone text, company_name text, job_title text,
  product_interest text, message text, consent_text text, consent_given bool not null default false,
  utm_source text, utm_medium text, utm_campaign text, utm_term text, utm_content text, click_id text,
  referrer_host text, landing_path text, source_path text,
  payload jsonb not null default '{}', payload_bytes int not null default 0,
  dedupe_key text, duplicate_of uuid null -> crm_leads on delete set null,
  decision text check (decision in ('linked','created','duplicate','rejected','spam')),
  assignment_rule_id uuid, assignment_reason text,
  sla_policy_id uuid, first_response_due_at timestamptz, first_response_at timestamptz, escalated_at timestamptz,
  spam_score int not null default 0, rejection_reason text,
  received_at timestamptz not null default now(), converted_at timestamptz, created_at/updated_at)
  constraint lead_email_or_phone check (coalesce(email::text,'') <> '' or coalesce(phone,'') <> '')
crm_lead_events (id bigint identity pk, lead_id uuid not null -> crm_leads on delete cascade, kind text not null,
  actor_user_id uuid -> users, detail jsonb not null default '{}', created_at timestamptz not null default now())
crm_assignment_rules (id uuid pk, organization_id uuid not null, name text not null, position int not null default 0,
  conditions jsonb not null default '{}', target_kind text not null check (target_kind in ('user','pool','queue')),
  target_user_id uuid null -> users, pool_user_ids uuid[] not null default '{}', round_robin_cursor int not null default 0,
  active bool not null default true, created_at/updated_at)
crm_sla_policies (id uuid pk, organization_id uuid not null, name text not null, first_response_minutes int not null default 240,
  business_hours_only bool not null default false, reminder_minutes int, escalate_to_user_id uuid -> users,
  business_hours jsonb not null default '{}', active bool not null default true, created_at/updated_at)
```

Checks: `first_response_minutes between 1 and 20160`, `reminder_minutes != first_response_minutes`, `payload_bytes <= 262144` (a larger payload is refused, not truncated), `spam_score between 0 and 100`, `utm_*` trimmed to 200 characters. Indexes: `crm_leads_inbox_idx (organization_id, status, received_at desc)`, partial `crm_leads_sla_idx (organization_id, first_response_due_at) where first_response_at is null and status not in ('spam','rejected','duplicate')`, `crm_leads_dedupe_idx (organization_id, dedupe_key) where dedupe_key is not null`, `crm_leads_email_idx (organization_id, lower(email::text))`, `crm_leads_phone_idx (organization_id, regexp_replace(phone,'[^0-9]','','g'))`, `crm_lead_events (lead_id, created_at desc)`. `dedupe_key` is the normalized match key that produced the verdict, stored so the duplicate queue can be rebuilt without re-scanning. Leads are never hard-deleted by automation; the reject/spam paths keep the row and record the reason, and a retention sweep (configurable, default 730 days) archives payloads while keeping the lead row.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `crm.lead.received` | A submission is stored (accepted or rejected) | `lead_id`, `source_id`, `form_key`, `status`, `decision`, `product_interest` |
| `crm.lead.assigned` | Assignment or reassignment | `lead_id`, `owner_user_id`, `previous_owner_user_id`, `rule_id`, `reason` |
| `crm.lead.duplicate_detected` | A match crossed the confidence bar | `lead_id`, `matched_contact_id`, `matched_key`, `score` |
| `crm.lead.sla_breached` | The due time passed without a response | `lead_id`, `due_at`, `owner_user_id`, `escalated_to` |
| `crm.lead.responded` | The first response is recorded | `lead_id`, `minutes_to_response`, `sla_state` |
| `crm.lead.converted` | Contact/deal/quotation created | `lead_id`, `contact_id`, `deal_id`, `quote_id` |
| `crm.lead.rejected` | Rejected or marked spam | `lead_id`, `reason_code`, `spam_score` |
| `crm.intake.source.updated` | Source, mapping or key change (rotation included) | `source_id`, `changed_keys` |

Consumed: `content.form.submitted` (the single intake trigger — the marketing copy of the same submission is for audience consent only and never creates a second lead), `crm.contact.merged` (re-points leads and duplicate pointers), `sales.quote.accepted` (advances the lead to `converted` and stops its SLA if still running), `sales.quote.sent` (stops the first-response clock). Webhook relevance: `crm.lead.received` is a common automation entry point ("new quote request → notify Slack"), `crm.lead.sla_breached` drives escalation rules; payloads carry ids, source keys and timings — never the message body, never the submitter's raw payload, never the endpoint key.

### Acceptance criteria

- [ ] `cargo test -p omnion-module-crm` (intake target) is green, covering mapping transforms, dedupe key normalization, rule evaluation order, round-robin distribution and SLA arithmetic including business hours.
- [ ] `0121_crm_lead_intake.sql` applies on a fresh and on a populated database and seeds the default SLA policy and rule per organization.
- [ ] A REQ-064 form bound to a source submits, produces one lead with mapped fields, consent stored as accepted, and the submission still lands in the forms inbox (one submission, one lead, no duplicates).
- [ ] A mapping that would drop a required target is refused at save with the field named; renaming a form field afterwards marks the binding broken and the health line names the missing key.
- [ ] A submission missing both e-mail and phone is refused with a readable reason, and no partial lead row is written.
- [ ] An accepted submission with UTM parameters stores first-touch and last-touch attribution, the referrer host and the landing path; a second submission from the same visitor keeps the original first touch.
- [ ] Dedupe policies behave as documented against a seeded contact: `link` attaches the lead to the contact and records `matched_key`, `create_anyway` makes a second contact, `reject_duplicate` stores a duplicate row pointing at the match and appears in the duplicate queue.
- [ ] Assignment: a country-conditioned rule wins over a catch-all, a pool distributes ten consecutive leads across three users without repeating a user twice in a row, and an unmatched lead lands unassigned and visible on the overview.
- [ ] The assignment simulator returns the winning rule and owner for a pasted payload without writing anything.
- [ ] SLA: a lead assigned under a business-hours policy has a due time computed inside the window (a Friday-after-cutoff lead is due Monday), `first_response_due_at` is visible in the list, and a breach emits `crm.lead.sla_breached` and notifies the escalation target exactly once.
- [ ] `Mark responded` inside the window sets `first_response_at`, computes the state `met`, and stops reminders; responding after the breach keeps the breach recorded.
- [ ] `Convert` creates or links the contact, creates a deal in the configured pipeline and stage with the mapped amount, and optionally opens a REQ-052 quotation draft linked back to the lead; the stepper shows each step's state.
- [ ] A quotation accepted through REQ-052's public link marks the lead and its deal converted, and the customer path through REQ-008 runs when commerce is installed.
- [ ] The autoresponder is sent once per accepted lead through the mail path and its delivery is recorded on the timeline; a rejected spam submission sends nothing.
- [ ] The keyed endpoint answers `202` to a valid payload, `401` with a wrong or rotated key, `202` but stores nothing when the honeypot is filled, and `429` after the configured rate limit.
- [ ] Every state change writes an audit entry with actor, before/after and request id, and the detail timeline renders exactly those entries.
- [ ] Cross-organization ids answer `404` for every route, and `crm.leads.read` without `crm.leads.convert` refuses conversion with `403` and writes nothing.
- [ ] All seven screens have empty, loading and error states with zero high findings, and the inbox plus lead detail work at 390 px with the sticky action bar usable.

### QA plan

The walkthrough extends `scripts/qa/walkthrough.cjs` with `/crm/settings/intake` (create a source bound to a seeded form, fix a deliberate broken mapping, run `Test mapping` with a sample payload, rotate the endpoint key once and see it revealed once), `/crm/settings/assignment` (create a country rule above the catch-all, run the simulator, then reorder and see the winner change), `/crm/settings/sla` (create a business-hours policy with a short reminder and an escalation user), `/crm/leads` (submit twice from the public form — once clean, once with a duplicated e-mail and once with the honeypot filled — then filter by SLA state and bulk-assign), `/crm/leads/{id}` (`Mark responded`, inspect attribution, open the match panel, convert, follow the quotation link) and `/crm/leads/duplicates` (link one duplicate and reject another). The visual check must see: an SLA badge with a real countdown, a mapping table with transform chips and a working preview, an assignment simulator naming the winning rule, a duplicate panel with a matched key and score, and a conversion stepper with completed steps — never a blank timeline, a raw JSON blob as the only view, or a disabled control without explanation. Screenshots: `page-crm-leads`, `page-crm-lead-detail`, `page-crm-intake-source`, `page-crm-assignment-simulator`.

### Slices

1. **Capture and inbox.** Migration `0121` (sources, leads, events), the intake event listener with mapping, transforms, consent and spam verdicts, the keyed endpoint with rate limits and key rotation, dedupe policies and the duplicate queue, plus `/crm/leads`, `/crm/leads/{id}` (read-side) and `/crm/settings/intake`. *Done when:* acceptance 1–7 and 15–16 pass and a public form submission lands as a lead with attribution and a dedupe verdict.
2. **Assignment and SLA.** Rules with ordered evaluation and the atomic round-robin cursor, the simulator, SLA policies with business hours, the reminder and breach worker, notifications to owner and escalation target, the autoresponder path, and `/crm/settings/assignment` plus `/crm/settings/sla`. *Done when:* acceptance 8–11 and 14 pass and a breach escalates inside one worker tick.
3. **Conversion flow and depth.** `Convert` through contact → deal → quotation → customer, the sales-event consumers, the REQ-064 form-editor card, metrics on the inbox, audit-trail rendering, retention sweep and the degradation paths (CRM absent, forms absent, sales absent). *Done when:* acceptance 12–13 and 17–18 pass and a full request-a-quote submission reaches a quotation and back through acceptance in one walkthrough.

### Risks / notes

- **The public intake surface is the attack surface.** Per-IP and per-source rate limits, honeypot, minimum-fill-time, payload size caps and a per-hour ceiling on leads per source; a flood is throttled and logged, and the keyed endpoint accepts only the hashed key it was issued.
- **One submission, one lead.** `content.form.submitted` is the only intake trigger and the handler is idempotent on the submission id, so a retried delivery or a second subscriber cannot create a second lead.
- **Dedupe must be predictable.** The matched key and score are always recorded and shown, policies are per source and never global, and `reject_duplicate` keeps the data rather than discarding it, so an operator can reverse a wrong verdict.
- **Round-robin fairness under concurrency.** The cursor advances inside one transaction on the rule row (`for update`); a naive in-memory counter double-assigns once the org has two app instances.
- **SLA arithmetic is server-side and timezone-aware.** Due times are computed in UTC from the organization's business window, displayed in the viewer's timezone, and holidays are explicitly out of scope in v1 rather than silently ignored.
- **PII discipline.** Payloads are size-capped, retention is configurable with an archive sweep, events never carry the message body or the submitter's raw payload, and the lead's data is deletable through the documented routes with an audit entry.
- **Module-absence degradation is designed, not accidental.** Without CRM the form keeps only REQ-064's inbox; without the forms module the keyed endpoint is the capture path; without sales the stepper ends at the opportunity with an explanatory note.
- **Mapping drift.** The binding health check runs on every submission and flags missing source keys instead of writing a lead with silently empty fields, and the source editor refuses to save a mapping whose required targets are unsatisfied.
- **Notification storms.** One notification per event per lead, burst-collapsed per owner per hour, and the autoresponder is once-per-lead regardless of retries.
