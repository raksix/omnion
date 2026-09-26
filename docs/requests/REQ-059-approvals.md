# REQ-059 — Approvals

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** module (`modules/approvals`)
> **Source:** owner brief — business suite / frontend depth (docs/08-BUSINESS-SUITE.md, docs/03-FRONTEND.md)

## Request

Approval workflows as a first-class module.

- **Approval flows**: define chains (sequential/parallel), conditions (amount > X, role = Y), escalation on timeout.
- **Requests inbox**: my pending approvals, requested-by-me, history; approve / reject / delegate with comments.
- **Attachments & context:** each request links to its subject (quote, leave, expense, page publish…).
- **Audit**: every decision recorded (who, when, comment) and surfaced in the audit log.
- **Integrations**: sales (discounts), HR (leave), accounting (expenses), CMS (publish gates).
- **Events**: `approvals.request.created`, `approvals.request.decided`.

## Implementation spec

### Scope (in / out)

**In**
- Flow definitions: ordered steps, sequential or parallel step resolution (`any` / `all` approvers), conditions over subject fields and requester context (amount, department, role, site), per-step SLA and escalation (escalate to user, role, or auto-approve on timeout with a recorded reason).
- Generic requests: every request points at a subject through `subject_type` + `subject_id` + `subject_url` + a label, so any module can raise one without the approvals module knowing it.
- Inbox: my pending approvals, requested by me, completed, all (with filters), decisions with comments, delegation (reassign one step, or delegate my inbox for a time window).
- Attachments and context: the subject card plus any documents (REQ-058) linked to the request.
- Audit: every transition lands in the audit log and is rendered on the request timeline.
- Integrations through a small subject-adapter registry: sales discount, HR leave, accounting expense, CMS publish gate — each adapter owns its own "apply the decision" side effect.
- Engine: durable steps on the workflow engine (`omnion-workflows`) — a request is a run whose steps are approvals, so retries, waits and restarts behave like every other workflow.

**Out**
- A free-form node canvas (the visual builder is REQ-004); flows are edited as a step list here.
- E-signature (REQ-030), accounting posting, HR balance arithmetic (owned by their modules).
- Multi-step financial disbursement/payment rails.

### Screens (UI)

| Route | Screen |
|---|---|
| `/approvals` | Inbox — tabs My pending / Requested by me / Completed / All |
| `/approvals/<id>` | Request detail — subject, chain, decision panel, timeline, attachments |
| `/approvals/flows` | Flow definition list |
| `/approvals/flows/<id>/edit` | Flow builder — steps, approvers, conditions, SLA, escalation |
| `/approvals/flows/<id>/test` | Dry-run against a sample subject (no writes) |
| `/approvals/new` | Quick-create a request for a supported subject type |

- **Inbox.** Table columns: Request, Type, Subject, Amount, Requester, Current step, Waiting since, SLA. Filters: scope tab, type, status, requester, date range, `overdue only`. Bulk actions: Reassign (single target), Add note, Export CSV — no bulk approve, decisions are always individual and explicit. Row expands a preview drawer with the subject summary and the decision buttons, so a reviewer can decide without leaving the list. Tabs keep counts in badges (e.g. `My pending · 4`). Keyboard: `j`/`k` move the focused row, `a` approve, `r` reject, `e` delegate, `⌘Enter` submits the open decision dialog, `Esc` closes.
- **Request detail.** Left: subject card — type badge, subject reference, amount/currency when present, a deep link "Open quote #Q-1042" (or leave request, expense, page), requester, created at, and the attachments list. Centre: chain timeline — one row per step with approver, mode, status icon, decision, comment, decided at, SLA badge (`in 6h`, `overdue 1d`); pending steps show the resolved approvers with avatar chips. Right: decision panel — `Approve`, `Reject`, `Delegate`, `Request changes`; comment box (required for reject and request-changes, optional otherwise, 2000 chars) with mention of the previous comment; a confirm dialog summarising the consequence text the adapter declares ("Discount 22% will be applied to quote Q-1042"). Cancelled/approved requests render read-only with the same timeline.
- **Flow list and builder.** List columns: Name, Key, Subject type, Steps, Enabled, Last updated, Requests (30d). Builder: header (name, key, subject type, enabled toggle); steps as a reorderable list, each card carries label, approver mode (Any of / All of), approver kind (User, Group, Role, Manager of requester), approver picker, SLA hours, escalation target, and an optional condition row builder (field, operator `= ≠ > ≥ < ≤ in`, value) with AND semantics inside a step and OR between steps evaluated top-down. A read-only "Resolved approvers" preview shows who would be asked for a sample request, and the Test screen runs the flow against a sample subject and prints the step-by-step resolution without writing rows.
- **Quick-create.** `/approvals/new` lists subject types the caller may request; picking one opens the subject's own form (when the module provides one) or a minimal form (reference, amount, note), then submits into the matching enabled flow — or shows "no flow matches, nothing to approve" with a link to the flows screen.
- **States and mobile.** Empty inbox states differ per tab ("Nothing waiting on you — nice."), loading uses skeleton rows, a failed decision keeps the dialog open with the error and keeps the comment typed. On mobile the inbox becomes a card list (type, subject, amount, SLA chip), the detail stacks subject → decision → chain, and the decision dialog becomes a full-height sheet.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET · POST | `/api/v1/approvals/flows` | List flows · create a flow | `approvals.read` · `approvals.flows.manage` |
| GET · PUT · DELETE | `/api/v1/approvals/flows/{id}` | Read · update (steps, conditions, enabled) · delete when it has no requests | `approvals.read` · `approvals.flows.manage` |
| POST | `/api/v1/approvals/flows/{id}/duplicate` | Clone a flow as a draft | `approvals.flows.manage` |
| POST | `/api/v1/approvals/flows/{id}/test` | Resolve a sample request, no writes | `approvals.flows.manage` |
| POST | `/api/v1/approvals/requests` | Raise a request (subject type/id, amount, note, attachments) | `approvals.request` |
| GET | `/api/v1/approvals/requests` | Inbox (`?scope=mine\|requested\|completed\|all&type=&status=&overdue=`) | `approvals.read` |
| GET | `/api/v1/approvals/requests/{id}` | Request with chain, attachments, timeline | `approvals.read` |
| POST | `/api/v1/approvals/requests/{id}/decide` | `{decision: approve\|reject\|changes, comment}` | `approvals.decide` |
| POST | `/api/v1/approvals/requests/{id}/delegate` | Reassign the caller's pending step | `approvals.delegate` |
| POST | `/api/v1/approvals/requests/{id}/cancel` | Requester cancels their own pending request | `approvals.request` |
| GET | `/api/v1/approvals/requests/{id}/timeline` | Audit-backed timeline rows | `approvals.read` |
| GET | `/api/v1/approvals/subjects` | Subject types this caller may request | `approvals.request` |
| POST | `/api/v1/approvals/requests/{id}/attachments` | Attach an existing document (REQ-058) | `approvals.decide` |

Errors are explicit: `approval_flow_missing`, `approval_not_your_step`, `approval_already_decided`, `approval_self_approval_blocked` (a requester never approves their own step unless the flow sets `allow_self_approval`), `approval_comment_required`.

### Data model

Migrations: `0103_approvals.sql`, `0104_approvals_escalation.sql` (reserved band 0100–0115; append-only ledger — take the next free number if taken).

```sql
-- 0103_approvals.sql
approval_flows (id uuid pk, organization_id uuid not null -> organizations, site_id uuid null -> sites,
  key text not null, name text not null, subject_type text not null, description text,
  enabled boolean not null default true, version integer not null default 1,
  allow_self_approval boolean not null default false, created_by uuid null -> users,
  created_at/updated_at timestamptz not null default now())
  unique (organization_id, key); index (organization_id, subject_type, enabled)
approval_flow_steps (id uuid pk, flow_id uuid not null on delete cascade, position integer not null,
  label text not null, approver_mode text in ('any','all'), approver_kind text in
  ('user','group','role','manager_of_requester'), approver_ref uuid null,
  sla_hours integer null check (sla_hours between 1 and 720),
  escalation_kind text null in ('user','role','none'), escalation_ref uuid null,
  conditions jsonb not null default '[]')
  unique (flow_id, position)
approval_requests (id uuid pk, organization_id uuid not null, flow_id uuid not null,
  flow_version integer not null, subject_type text not null, subject_id text not null,
  subject_label text not null, subject_url text, amount numeric(14,2), currency text(3),
  requester_user_id uuid not null -> users, status text in
  ('pending','approved','rejected','cancelled','expired') not null default 'pending',
  current_position integer not null default 1, note text,
  created_at/updated_at/decided_at timestamptz)
  index (organization_id, status, created_at desc); index (requester_user_id, status)
  -- one open request per subject: partial unique (subject_type, subject_id) where status = 'pending'
approval_request_steps (id uuid pk, request_id uuid not null on delete cascade,
  position integer not null, approver_mode text, status text in
  ('pending','approved','rejected','skipped','escalated') not null default 'pending',
  assignee_user_id uuid null, delegation jsonb, decided_by uuid null, decision text,
  comment text, due_at timestamptz, decided_at timestamptz)
  unique (request_id, position); index (assignee_user_id, status)
approval_request_attachments (id uuid pk, request_id uuid not null, document_id uuid null,
  label text, added_by uuid, added_at timestamptz)
-- 0104_approvals_escalation.sql
approval_escalations (id uuid pk, request_id uuid not null, step_id uuid not null,
  kind text in ('escalated','auto_approved','expired'), reason text, target_user_id uuid null,
  created_at timestamptz)  -- one row per escalation action, in addition to the audit entry
```

The escalation runner is a background task (same shape as the workflow runner) that claims due steps with `for update skip locked`, applies the escalation, writes an audit row and emits the event.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `approvals.request.created` | A request is raised | `request_id`, `flow_key`, `subject_type`, `subject_id`, `amount`, `requester_user_id` |
| `approvals.request.decided` | Final decision (approved or rejected) | `request_id`, `decision`, `decided_by`, `subject_type`, `subject_id` |
| `approvals.step.decided` | One step decided (intermediate) | `request_id`, `position`, `decision`, `comment_present` |
| `approvals.step.escalated` | SLA passed, escalation applied | `request_id`, `position`, `target_user_id`, `kind` |
| `approvals.request.delegated` | Step reassigned | `request_id`, `position`, `from_user_id`, `to_user_id` |

Consumed: `marketing.form.submitted` (optional flow trigger), `content.page.publish_requested` (CMS publish gate), `sales.quote.discount_requested`, `hr.leave.requested`, `accounting.expense.submitted` — each consumed event carries a subject reference and raises a request through the same creation path. Webhook relevance: external systems subscribe to `approvals.request.decided` to act on decisions; payloads carry ids, subject references and decisions, never comment bodies of unrelated steps.

### Acceptance criteria

- [ ] A flow with three steps on a sample subject resolves the right approvers in `/approvals/flows/<id>/test` without writing any row.
- [ ] Raising a request with `amount` under the flow condition skips the finance step, visible in the timeline as `skipped`.
- [ ] A sequential flow asks approvers one at a time; a parallel step with mode `all` waits for every assignee before advancing.
- [ ] Approving the last step flips the request to `approved`, records the decision and calls the subject adapter exactly once (idempotent on replay).
- [ ] Rejecting without a comment is refused with `approval_comment_required`; with a comment the request ends `rejected` and the chain stops at that step.
- [ ] A requester cannot approve their own step (`approval_self_approval_blocked`) unless the flow enables self-approval.
- [ ] Delegate moves the caller's pending step to another user; both users see the change, and the timeline shows from/to.
- [ ] A step past its SLA escalates to the configured target and the escalation is visible as an event and a timeline row.
- [ ] Inbox tabs show the correct counts; bulk approve is impossible from the UI (no such control, and the API rejects multi-step decisions).
- [ ] Every decision writes one audit row with actor, subject, decision and comment, readable on the timeline and in the audit log.
- [ ] `approvals.request.created` and `approvals.request.decided` are delivered to a subscribed endpoint with redelivery working.
- [ ] The CMS publish gate blocks publishing a page while its approval request is pending and publishes on approval.
- [ ] The sales discount adapter applies an approved discount to the quote within one second of the decision.
- [ ] Two concurrent decisions on the same step leave exactly one winner; the loser gets `approval_already_decided`.
- [ ] A cancelled request stops escalation, emits no decided event, and shows as `cancelled` in Requested-by-me.
- [ ] All screens render at 390 px without horizontal scroll and pass the walkthrough without high findings.

### QA plan

The browser walkthrough must visit `/approvals` (all four tabs, every filter, the preview drawer, keyboard `j`/`k`/`a` on a populated row), open `/approvals/<id>` and exercise Approve, Reject without comment (expect the validation message), Reject with comment, Delegate, Cancel; visit `/approvals/flows`, open the builder, reorder a step, add a condition, save, hit the Test screen, and toggle `Enabled` off and on; visit `/approvals/new` and confirm the subject-type picker lists the seeded types. The walkthrough seeds one pending request per type so no list is empty. Visual check: the timeline renders step badges and avatars, SLA chips are legible, the decision dialog shows the consequence text, and the inbox shows real counts rather than zeros; the mobile viewport shows card rows rather than a clipped table.

### Slices

1. **Flows.** Migration `0103_approvals.sql` (flows + steps); flows CRUD with steps, conditions, SLA fields; the resolution engine as a pure function with unit tests; flow list + builder + test screens. *Done when:* acceptance 1–2 pass and `/approvals/flows` is in the walkthrough inventory.
2. **Requests and decisions.** Request creation with the subject registry, `approval_requests` + `approval_request_steps`, inbox with tabs and filters, decide / delegate / cancel, timeline from audit, self-approval rule. *Done when:* acceptance 3–7 and 9–10 pass.
3. **Escalation, events, integrations.** Migration `0104_approvals_escalation.sql`; escalation runner with SLA claiming, the five events on the bus, subject adapters for sales discount, HR leave, accounting expense and the CMS publish gate. *Done when:* acceptance 8, 11–13, 15 pass.
4. **Hardening.** Concurrency tests for double decisions, permission matrix tests, mobile layout pass, walkthrough extension and vision review clean. *Done when:* acceptance 14 and 16 pass and the QA report lists zero high findings for the new screens.

### Risks / notes

- The module must not grow into a second workflow engine: chains are data on the existing durable-step engine, and the approval step kind stays a thin adapter around it.
- Subject adapters are the integration seam and the most likely source of duplication — one adapter trait, one registry, side effects idempotent by `(request_id, step_id)`.
- Escalation on timeout can approve things nobody read; the default escalation is a human target, auto-approve must be opted in per step and is always recorded with a reason.
- Inbox queries need the `(organization_id, status, created_at desc)` index from day one; the "my pending" query joins assignees and must stay under 100 ms at 50k requests.
- Notification delivery rides the platform notification centre (REQ-021) once it lands; until then decisions reach people through the automation engine's mail action, triggered by the events above.
- No self-approval by default, and comment bodies never appear in webhook payloads.
