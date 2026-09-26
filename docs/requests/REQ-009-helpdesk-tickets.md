# REQ-009 — Helpdesk / Ticket System

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** module (`modules/helpdesk`)
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

```text
Customer
   ↓
Ticket
   ↓
Assignment
   ↓
Agent
   ↓
Resolution
```

SLA support:

```text
Priority: Critical
Response SLA: 30 min
Resolution SLA: 4 hours
```

## Notes

- Part of the broader business suite — [`docs/08-BUSINESS-SUITE.md`](../08-BUSINESS-SUITE.md)
  (which defines tiered SLAs: Critical 1h / High 4h / Normal 24h).

## Implementation spec

> **Module:** `modules/helpdesk` (crate `omnion-module-helpdesk`, workspace member) · **Migration:** `database/migrations/0014_helpdesk.sql` (next free slot at build time) · **Admin routes:** `/helpdesk/*` · **Public routes:** `/api/v1/public/helpdesk/*` plus the customer portal on the site renderer · **Permission family:** `helpdesk.*` · **Depends on:** core crates (`identity`, `permissions`, `audit`, `events`, `media`, `notifications`, `workflows`, `search`) · **Bridges:** CRM contacts (REQ-051) and orders/invoices (REQ-008/REQ-054) appear in the ticket sidebar when those modules are installed.

### Scope (in / out)

**In**

- **Ticket core:** create (agent, portal, e-mail, API), subject and description, requester (registered customer, CRM contact or e-mail-only), queue, assignee, status, priority, type, tags, custom fields, attachments through the media pipeline (REQ-010), merge, watch, related tickets.
- **Conversation:** customer replies and agent replies form one chronological thread; internal notes are structurally separate and never leave the platform; canned responses (macros) with variables; CC handling; e-mail threading by message id so a reply lands in the right ticket and reopens a resolved one.
- **Routing:** queues with members and one strategy each (`round_robin`, `least_busy`, `manual`), default queue and priority; rule-based routing by tag, type or keyword is delegated to the automation engine (REQ-003) using the events this module emits.
- **SLA:** policies per priority (optionally per queue or tag) with first-response and resolution targets, a business-hours calendar (working days, hours, timezone, holidays), pause on `pending customer`, a warning threshold (default 80%), escalation actions (notify, reassign, raise priority) and breach recording.
- **Customer portal:** public submit form (guest allowed when enabled, rate limited), ticket status and reply by opaque token, attachment upload, and a confirmation e-mail.
- **Inbound e-mail:** a signed provider webhook maps an incoming message to a ticket by subject token or message id, reopens a resolved ticket on a new message, and stores attachments through the media pipeline.
- **Reports and CSAT:** SLA compliance, average first response, average resolution, tickets by priority/queue/agent, a workload table, and an optional one-question satisfaction survey sent on resolution with a rating, an optional comment and a summary report.
- **Settings:** ticket number format, auto-close after N days in `resolved`, default queue, guest submissions toggle, attachment size limit, spam blocklist (addresses and keywords), survey toggle.

**Out (tracked elsewhere)**

- Knowledge base articles and the portal article surface → REQ-058; AI answer drafting and ticket summarisation → REQ-042 (read-only suggestions); live chat and presence → REQ-041; the e-mail sending stack → REQ-021.
- Quoting and returns → REQ-052; project tasks spawned from a ticket → REQ-056; further channels (chat, social) are a later enum value plus a connector, not a schema change.

### Screens (UI)

Nav: **Inbox · Tickets · Queues · SLA · Business hours · Macros · Reports · Settings** (the inbox is the landing screen).

| Route | Screen |
|---|---|
| `/helpdesk` | Inbox: saved-view tabs plus list on the left, conversation on the right |
| `/helpdesk/tickets` | Full table view with the widest filter set |
| `/helpdesk/tickets/{id}` | Full-page ticket detail (deep link, printable) |
| `/helpdesk/queues` | Queue list, members, routing strategy |
| `/helpdesk/sla` | SLA policies |
| `/helpdesk/business-hours` | Calendars |
| `/helpdesk/macros` | Canned responses |
| `/helpdesk/reports` | SLA, workload and satisfaction reports |
| `/helpdesk/settings` | Module settings and portal configuration |

**Inbox** — saved views as tabs: `All open`, `Unassigned`, `Mine`, `Breached`, `Pending customer`, `Resolved today`. Rows show an unread dot, `#number`, subject, requester, priority chip, SLA countdown or a red `Breached`, assignee avatar and last activity. Keyboard: `j`/`k` move the selection, `enter` opens the full page, `e` resolves, `r` focuses the reply composer, `n` focuses an internal note, `a` opens the assignee picker, `p` cycles priority, `x` selects for bulk, `/` searches, `?` shows the shortcut sheet. On mobile it is list → detail with a back button and a sticky composer.

**Conversation pane** — header (subject inline-edit, status, priority, assignee, queue, SLA badges, `Merge`, `Watch`, `Link`), thread (customer left, agent right, internal notes with a distinct background and an `Internal` label, attachments as file chips opening the preview), composer (`Reply`/`Note` tabs, macro picker `⌘.`, attachment button, CC field, `Send` with `⌘+enter`, optimistic append with a retry affordance), and a sidebar with customer context (contact card, company, open tickets, recent orders when REQ-008 is installed) and properties (type, tags, custom fields, watchers).

**Ticket table** — columns `Ticket`, `Subject`, `Requester`, `Queue`, `Assignee`, `Priority`, `Status`, `Created`, `First response`, `Resolution SLA`, `SLA` badge (`on track`/`warning`/`breached`); filters: search (subject, requester, body), status, priority, queue, assignee (me/unassigned/specific), tag, type, channel, created and resolved ranges, SLA state; sort by any column and save the view. Bulk actions: assign, set priority, add or remove tag, move queue, close, merge selected into one; row actions: open, assign to me, snooze until a date, resolve.

**Queues, SLA, hours** — queues: `Queue`, `Members`, `Strategy`, `Default priority`, `SLA policy`, `Open tickets`, with a live “would route to” preview for a sample ticket. SLA editor: name, applies-to (priority multi-select plus optional queue or tag), first response `1–10080` minutes, resolution `1–43200` minutes, calendar, `Pause while pending customer`, warning threshold `50–95%`, escalation actions with a delivery target; resolution shorter than first response is refused. Business hours: day-by-day open/close grid, holidays list, timezone picker that warns when a queue uses a different timezone than its calendar.

**Macros, reports, settings** — macros: `Macro`, `Scope` (global/queue), `Updated`, `Used`; editor with a variable insert menu (`{{customer.first_name}}`, `{{ticket.number}}`, `{{agent.name}}`), a live preview against a sample ticket and a highlighted unresolved variable — a sample body: `Merhaba {{customer.first_name}}, talebiniz için teşekkür ederiz. Kaydınızı {{ticket.number}} numarasıyla takip edebilirsiniz.` Reports: SLA compliance cards (first response and resolution per policy), average response and resolution times, tickets by priority/queue, a workload table (`Agent`, `Open`, `Resolved today`, `Avg first response`) and satisfaction (`Sent`, `Responses`, `Average score`, `Breakdown`), each with a range selector and CSV export. Settings: number format, auto-close days, default queue, guest submissions, attachment limit, blocklist and survey toggle.

**States, keyboard, mobile** — skeleton rows while loading, a real empty state per view (“No tickets — nothing is waiting on you”), error state with retry and request id; an empty message cannot be sent; on mobile the sidebar moves below the thread, tables become cards and bulk actions use a bottom sheet.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET, POST | `/api/v1/helpdesk/tickets` | Ticket list (filters, paging, saved view) / create on behalf of a requester | `helpdesk.tickets.read` / `helpdesk.tickets.manage` |
| GET, PATCH, DELETE | `/api/v1/helpdesk/tickets/{id}` | Detail with thread page / update subject, type, tags, watchers / soft delete | `helpdesk.tickets.read` / `helpdesk.tickets.manage` / `helpdesk.tickets.delete` |
| POST | `/api/v1/helpdesk/tickets/{id}/replies` · `/notes` | Agent reply (outgoing) / internal note | `helpdesk.tickets.reply` |
| POST | `/api/v1/helpdesk/tickets/{id}/assign` · `/status` · `/priority` · `/watch` | Assignment, status (with note), priority, watchers | `helpdesk.tickets.manage` / `helpdesk.tickets.reply` |
| POST | `/api/v1/helpdesk/tickets/{id}/merge` | Merge into another ticket | `helpdesk.tickets.manage` |
| GET | `/api/v1/helpdesk/tickets/{id}/events` | Timeline (status, assignment, SLA, messages) | `helpdesk.tickets.read` |
| POST | `/api/v1/helpdesk/tickets/bulk` | Bulk assign, priority, tag, queue move, close | `helpdesk.tickets.manage` |
| GET, POST, PATCH, DELETE | `/api/v1/helpdesk/queues` · `/{id}` | Queue list with load counts and CRUD (members included) | `helpdesk.tickets.read` / `helpdesk.queues.manage` |
| GET, POST, PATCH, DELETE | `/api/v1/helpdesk/sla-policies` · `/{id}` | SLA policies | `helpdesk.tickets.read` / `helpdesk.sla.manage` |
| GET, POST, PATCH, DELETE | `/api/v1/helpdesk/business-hours` · `/{id}` | Calendars | `helpdesk.tickets.read` / `helpdesk.sla.manage` |
| GET, POST, PATCH, DELETE | `/api/v1/helpdesk/macros` · `/{id}` | Canned responses | `helpdesk.tickets.read` / `helpdesk.macros.manage` |
| GET | `/api/v1/helpdesk/reports/summary` · `/reports/export` | SLA, response-time and workload rollups / CSV export | `helpdesk.reports.read` |
| GET, PUT | `/api/v1/helpdesk/settings` | Module and portal settings | `helpdesk.settings.manage` |
| POST | `/api/v1/helpdesk/inbound/email` | Inbound message webhook (signature verified) | public, signature required |
| POST | `/api/v1/public/helpdesk/tickets` | Portal submission (guest allowed when enabled) | public, rate limited |
| GET, POST | `/api/v1/public/helpdesk/tickets/{token}` · `/replies` · `/attachments/{id}` | Status, reply and attachment download by token | public token |
| POST | `/api/v1/public/helpdesk/csat/{token}` | Submit a satisfaction rating | public token |

### Data model

```text
helpdesk_queues(id uuid pk, organization_id uuid not null, name text not null, slug text not null, strategy text not null default 'round_robin', default_priority text not null default 'normal', business_hours_id uuid, sla_policy_id uuid, position integer not null default 0, active boolean not null default true) · helpdesk_queue_members(queue_id uuid not null, user_id uuid not null, pk (queue_id, user_id))
helpdesk_tickets(id uuid pk, organization_id uuid not null, number bigint not null, subject text not null, description text not null default '', requester_user_id uuid, requester_email text, requester_name text, contact_id uuid, queue_id uuid, assignee_id uuid, status text not null default 'new', priority text not null default 'normal', kind text not null default 'question', channel text not null default 'portal', tags text[] not null default '{}', custom_fields jsonb not null default '{}', sla_policy_id uuid, first_response_due_at timestamptz, resolution_due_at timestamptz, first_responded_at timestamptz, first_response_breached boolean not null default false, resolution_breached boolean not null default false, paused_at timestamptz, pause_seconds bigint not null default 0, resolved_at, closed_at, last_customer_reply_at, last_agent_reply_at, merged_into_id uuid, created_by uuid, created_at timestamptz not null default now(), updated_at)
helpdesk_messages(id bigint identity pk, ticket_id uuid not null, kind text not null default 'reply', author_user_id uuid, author_kind text not null default 'agent', from_email text, to_email text, body text not null, is_internal boolean not null default false, email_message_id text, created_at timestamptz not null default now()) · helpdesk_attachments(id uuid pk, message_id bigint not null, media_id uuid not null, created_at)
helpdesk_events(id bigint identity pk, ticket_id uuid not null, kind text not null, actor_user_id uuid, from_value text, to_value text, detail jsonb not null default '{}', created_at)   -- timeline
helpdesk_watchers(ticket_id uuid not null, user_id uuid not null, pk (ticket_id, user_id)) · helpdesk_portal_tokens(id uuid pk, ticket_id uuid not null, token_hash text not null, expires_at timestamptz, last_used_at, revoked_at) · helpdesk_csat(id uuid pk, ticket_id uuid not null, rating smallint not null, comment text not null default '', token_hash text not null, responded_at timestamptz not null default now())
helpdesk_sla_policies(id uuid pk, organization_id uuid not null, name text not null, applies_priorities text[] not null default '{}', queue_id uuid, tag text, first_response_minutes integer not null, resolution_minutes integer not null, business_hours_id uuid, pause_on_pending boolean not null default true, warning_percent integer not null default 80, escalation jsonb not null default '{}', position integer, active boolean not null default true)
helpdesk_business_hours(id uuid pk, organization_id uuid not null, name text not null, timezone text not null, windows jsonb not null default '{}', holidays date[] not null default '{}') · helpdesk_macros(id uuid pk, organization_id uuid not null, name text not null, scope text not null default 'global', queue_id uuid, body text not null, usage_count bigint not null default 0, updated_by uuid, created_at, updated_at)
helpdesk_settings(organization_id uuid pk, number_format text not null default 'T-{seq}', auto_close_days integer not null default 7, default_queue_id uuid, guest_submissions boolean not null default false, max_attachment_bytes bigint not null default 26214400, blocklist jsonb not null default '{}', csat_enabled boolean not null default false, updated_by uuid, updated_at timestamptz)
```

Checks: `status in ('new','open','pending','resolved','closed')`, `priority in ('low','normal','high','critical')`, `kind in ('question','incident','problem','task')`, `channel in ('portal','email','api','agent')`, message `kind in ('reply','note','system')`, `first_response_minutes between 1 and 10080`, `resolution_minutes between 1 and 43200` and greater than the response target, `rating between 1 and 5`. Indexes: unique `(organization_id, number)`, `helpdesk_tickets_queue_status_idx (queue_id, status, priority)`, `helpdesk_tickets_assignee_idx (assignee_id, status)`, `helpdesk_tickets_due_idx (resolution_due_at) where status in ('new','open','pending')`, `helpdesk_tickets_requester_idx (lower(requester_email))`, GIN on `tags` and `custom_fields`, `helpdesk_messages_ticket_idx (ticket_id, created_at)`, `helpdesk_events_ticket_idx (ticket_id, created_at desc)`.

Migration `database/migrations/0014_helpdesk.sql` — append-only and commented in the `0009` style; seeds a `General` queue, the tiered policies from docs/08 (Critical 1h, High 4h, Normal 24h) plus the request example’s 30-minute first response for Critical, a `Mon–Fri 09:00–18:00` calendar per organization timezone and a `helpdesk_settings` row.

### Events

- Emitted: `ticket.created`, `ticket.assigned`, `ticket.replied`, `ticket.customer_replied`, `ticket.resolved`, `ticket.closed`, `ticket.reopened`, `ticket.merged`, `ticket.sla_warning`, `ticket.sla_response_breached`, `ticket.sla_resolution_breached`, `ticket.csat_received`.
- Payloads carry ticket id and number, queue, priority, status, assignee id and SLA state — never message bodies, so customer content cannot leak to third-party endpoints. Consumed: `form.submitted` (REQ-064) creates a ticket when the form is configured to; `order.created`/`invoice.overdue` (REQ-008) may raise one through an automation rule.
- Webhook relevance: `ticket.sla_*_breached` drives notification rules and escalations; `ticket.created`/`ticket.resolved` feed CRM activities and automations; `ticket.csat_received` feeds reports and notifications.

### Acceptance criteria

- [ ] `0014_helpdesk.sql` applies on a fresh and on a populated database; `cargo test -p omnion-module-helpdesk` is green.
- [ ] Ticket numbers are gap-free per organization and follow the configured format.
- [ ] Creating a ticket starts the SLA clock from the policy matching its priority and queue, storing due timestamps rather than computing them on read.
- [ ] First response is stamped on the first outgoing agent reply; an internal note does not count as a response (both asserted).
- [ ] A `pending customer` ticket pauses the clock and resumes on the customer’s next message with `pause_seconds` accounted for.
- [ ] Warning and breach events fire once per clock, not on every worker tick.
- [ ] Business hours hold: a ticket created Friday 17:30 with a Mon–Fri 09:00–18:00 calendar and a 4-hour policy is due Monday, not Saturday.
- [ ] A policy with resolution shorter than first response is refused with a field-level error; a one-year target is accepted, a longer one is not.
- [ ] Routing strategies hold: `round_robin` alternates members, `least_busy` picks the smallest open count, `manual` leaves the ticket unassigned.
- [ ] Assignment, priority and status changes each write a timeline event with actor and before/after.
- [ ] Bulk actions apply atomically to the selection and report per-ticket failures instead of silently skipping.
- [ ] Merge moves messages and attachments, closes the loser with a pointer, and keeps the surviving thread ordered and readable.
- [ ] Internal notes never appear in outgoing payloads or the portal view (integration test).
- [ ] Portal tokens work for submit, status, reply and attachments; an expired or revoked token is refused; the token is stored hashed.
- [ ] Inbound e-mail maps a reply onto the right ticket, reopens a resolved one, stores attachments through the media pipeline, and rejects an unsigned call.
- [ ] Attachments respect the size limit, are scanned when the file-manager scanner is enabled, and never cross organizations.
- [ ] Satisfaction survey sends once on resolution when enabled, accepts one response per token and appears in the report.
- [ ] Reports match a seeded fixture for SLA compliance, average first response, average resolution and workload.
- [ ] Permissions answer 401/403/200 as documented and a foreign organization’s ticket answers 404.
- [ ] Every screen has empty, loading and error states with zero high findings; mobile inbox → detail works with the composer usable.

### QA plan

Extend `scripts/qa/walkthrough.cjs` with `/helpdesk`, `/helpdesk/tickets`, `/helpdesk/queues`, `/helpdesk/sla`, `/helpdesk/business-hours`, `/helpdesk/macros`, `/helpdesk/reports`, `/helpdesk/settings` (desktop) and `/helpdesk`, `/helpdesk/tickets/{id}` (mobile). The script must submit a ticket through the public portal form (exercising the guest path), find it in the inbox, assign it, reply with a macro, add an internal note, set it to `pending`, resolve it, then reopen it from a portal token reply; create a queue with two members and check the routing preview; submit an invalid SLA pair (resolution shorter than first response) to see the field error; create a macro and preview its variables; open reports and export a CSV.

What the visual check should see: an inbox with list and thread side by side, an obviously distinct internal-note style, readable (AA contrast) and unclipped SLA badges, a composer with a visible macro button, a queue editor with a routing preview, and genuine empty states for a fresh organization — screenshots `page-helpdesk-inbox`, `page-helpdesk-ticket`, `page-helpdesk-queues`, `page-helpdesk-sla`, `page-helpdesk-reports`, `mobile-helpdesk-ticket`.

### Slices

1. **Ticket core.** Migration, queues and members, ticket CRUD with numbering, messages (reply/note), assignment, status, priority, tags, timeline, attachments, list + inbox + detail screens with shortcuts, permission keys, tests. Done when an API-created ticket is visible in the inbox, an agent reply stamps the first response, and an internal note stays internal.
2. **SLA engine.** Policies, business hours, due-date computation with pause/resume, warning and breach detection in a worker, escalation actions, SLA badges, and reports v1 (compliance plus response/resolution averages). Done when the Friday-afternoon fixture is due on Monday and breach events fire exactly once.
3. **Queues depth, macros, bulk.** Routing strategies with preview, saved views, bulk actions, merge, watchers, related tickets, macro CRUD with variables and usage counts. Done when round-robin and least-busy routing are proven by test, a merge preserves the thread, and bulk actions report partial failures.
4. **Channels and reporting depth.** Public portal (submit, status, reply, attachments), inbound e-mail with threading and reopen, satisfaction survey, workload and satisfaction reports, exports, settings. Done when the guest submit → agent reply → portal reply cycle completes end to end and a survey response appears in the report.

### Risks / notes

- **Migration number** is “next free slot at build time”; additive only — the module adds tables and never touches existing rows.
- **Outbound side effects** depend on REQ-021: gate the send affordance behind a capability check and mark a message `queued` until delivery is known, rather than failing after the fact.
- **E-mail threading is the fragile part:** store the message id, match by token first and header second, and treat an unmatched message as a new ticket with a link back instead of dropping it.
- **Timezones:** SLA arithmetic must use the calendar timezone with DST-aware addition — a naive `+ interval` drifts twice a year.
- **The SLA worker must be idempotent per clock** (a flag or a dedicated event row) or a restart re-emits breaches.
- **Message bodies are personal data:** keep them out of event payloads, logs and third-party notifications; the timeline shows a preview only to actors who may read the ticket.
- **Attachment access** goes through the media permission check — a portal token must never grant media-library access; bulk queue moves recompute due dates and must say so in the confirmation.
