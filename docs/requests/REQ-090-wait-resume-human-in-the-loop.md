# REQ-090 — Wait, Resume & Human-in-the-Loop

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/workflows`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Long-running workflows that pause for people.

- Wait node (duration or until a date) backed by the wait sweeper.
- Waiting webhooks and waiting forms: workflow pauses, a URL is issued, resume on submission.
- Send-and-wait (message + approval) and approvals as resumable webhooks.
- HMAC-signed callbacks so a resume request cannot be forged.
- Pending-response handling, timeout branches, and a "waiting" view listing paused runs.

## Implementation spec

### Scope (in / out)

**In**

- **Wait node**: `wait_for_duration` (value plus unit, minimum one second) and `wait_until` (timestamp or date expression with a timezone). Both park the step at its deadline, enforce a maximum wait window (default 90 days, configurable) with a clear error, and schedule a timer at the deadline so wake-ups stay sub-second — the periodic sweeper stays the safety net if a timer is lost (docs/09-N8N-TEARDOWN.md §13, lesson 19).
- **Wait tokens**: every resumable wait issues an opaque single-use token, and only its hash is stored, so the delivered token is not recoverable from the database. Verification is HMAC over `(execution_id, step_id, wait_id, expires_at, nonce)` with a versioned key from the secret store, plus server-side expiry and a used flag, so a forged or replayed resume is refused with a named code; the reference travels in the body, never as a secret-bearing URL path (lesson 8).
- **Waiting webhook**: the run parks and exposes a resume address; the submission (JSON body or form post) becomes the resume payload. Options: method, accepted content types, a payload schema (JSON Schema subset) validated before resume, a payload size cap, and an optional awaiting-response mode where the original HTTP caller is answered by the run instead of an immediate acknowledgement.
- **Waiting form**: the wait node may carry a form definition (title, description, field list with types and validation, submit label, success message) rendered on a platform-hosted public page addressed by the token. Submission validates against the field schema, stores the payload and resumes the run. The form is workflow-scoped; a reusable form library is REQ-117's product, and field components are shared rather than duplicated.
- **Send-and-wait and approvals**: the node sends a message through a notification channel (REQ-119) carrying the resume link (or action buttons where the channel supports them), then parks. The decision (approve, reject, custom) is the resume payload. An approval wait optionally creates a record in the approvals inbox (REQ-059) so one decision surface serves both; assignees are a user or a role, and reminders and escalation stay REQ-059's, with this REQ exposing the resume primitive they call.
- **Pending responses**: with awaiting-response enabled the incoming request is held up to a configured timeout; if the run reaches the responding node in time its mapped body and status are returned, otherwise the caller receives 202 with a status URL and the run continues. Held connections are bounded per trigger and released on timeout, failure or cancellation — never left dangling — and their count is exported to the observability stack.
- **Timeout branches**: every resumable wait declares `on_timeout` as `continue` (resume with a `timed_out` flag and no payload), `fail` (named error) or `error_output` (items on the error port). The timeout is a first-class event, so history shows why a run continued.
- **Resume semantics**: exactly-once under concurrent submissions via a conditional update (`pending` → `resumed`); the loser receives "already resumed" and its payload is recorded as rejected. Resume works after a process restart because tokens and deadlines are rows. Exactly one lifecycle transition is emitted per resumed node — the duplicate-event defect n8n hit on resume is guarded by an explicit state check (lesson 7).
- **Waiting view and operations**: paused runs across workflows with wait kind, waiting since, expiry and assignee, plus actions to open the run, copy the resume URL, resume manually with a payload, expire now (taking the timeout branch) or cancel. Operator resumes are audited with the actor recorded.
- **Cleanup and audit**: expired waits settle as `expired`; tokens and payloads are deleted per the retention settings (REQ-093); every issuance, submission, rejection, manual resume and expiry is audited with actor (user, or anonymous with source address), wait id and execution id.

**Out**

- Approval chains, escalation policy and the inbox product (REQ-059) — this REQ supplies the resumable callback.
- Notification channels, templates and delivery retries (REQ-119).
- Durable step storage, queueing, cancellation propagation and sweeper scaling (REQ-091, REQ-096).
- Public form product features (multi-step, spam protection, CRM routing) — REQ-117.
- Expression semantics used in durations and message bodies (REQ-092).

### Screens (UI)

| Route | Screen |
|---|---|
| `/workflows/waiting` | Waiting view: paused runs across workflows with filters and safe actions |
| `/workflows/waiting/<id>` | Wait detail: token status, resume URL, submissions log, manual resume, expire now |
| `/workflows/<id>/edit` | Wait node inspector: duration, form builder, message preview, timeout branch |
| `/executions/<id>` | Run detail with a waiting banner, resume and cancel (REQ-093's screen) |
| `/approvals` | Approvals inbox where a workflow wait appears as a task (REQ-059's screen) |

- **Waiting view.** Columns: Workflow, Run, Waiting on (node label + kind chip: duration, until, webhook, form, approval), Waiting since, Expires in (countdown, amber under one hour, red when overdue but unsettled), Asked, actions. Filters for workflow, kind, expiry window, expires soon and overdue; header counts ("14 waiting · 2 expire within the hour") with a link to retention settings.
- **Wait detail.** Status (kind, state, created, expires, resolved by and at), Token (hint and state only; regenerate invalidates the old link and is confirmed), Resume URL (copy, with a note that it is a bearer credential), Form preview (exactly what the recipient sees), Submissions (time, source, accepted or the rejection reason, payload viewer), Actions (resume with a JSON payload, expire now, cancel run).
- **Wait node inspector.** Tabs: Wait (duration vs until, timezone, maximum window), Form (field list with type, label, required, validation, help, reorder, preview; submit label; success message), Message (channel, recipient, template preview with sample values, action labels), Advanced (timeout behaviour, accepted types, payload schema, size cap, awaiting-response toggle and timeout), Errors.
- **Resume page (public).** Token-addressed: a form renders fields with validation and submit; an approval states the request with a decision set; a webhook wait without a form shows a neutral confirmation before accepting a payload. Expired, used and invalid tokens get distinct, calm messages with no internal detail. Responsive and keyboard-operable; a platform sign-in is required only when the assignee is a platform user, and then the decision is attributed to them.
- **Run, canvas and mobile states.** A paused run shows `Waiting`, the reason, a countdown and (for a privileged viewer) the resume URL with "resume as operator", and the node carries a waiting badge with the same countdown; skeletons cover loading, impossible actions (expire a resumed wait, resume an expired token) are disabled with a reason, and at 390 px the waiting view is a usable read-only table with resume-URL copy while the form builder asks for a wider screen.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/waits` | Pending waits — `?workflow=&kind=&expiring_within=&overdue=&state=` | `workflows.read` |
| GET | `/api/v1/waits/summary` | Counts for the waiting view header | `workflows.read` |
| GET | `/api/v1/waits/{id}` | Wait detail (token hint, state, expiry) | `workflows.read` |
| POST | `/api/v1/waits/{id}/resume` | Operator resume with a payload; actor recorded | `workflows.run` |
| POST | `/api/v1/waits/{id}/expire` | Expire now and take the timeout branch | `workflows.waits.manage` |
| POST | `/api/v1/waits/{id}/token` | Regenerate the resume token (invalidates the old link) | `workflows.waits.manage` |
| GET | `/api/v1/waits/{id}/submissions` | Submission log with accepted and rejected reasons | `workflows.read` |
| GET | `/api/v1/executions/{id}/resume` | The run's resume URL for a privileged viewer | `workflows.run` |
| POST | `/api/v1/executions/{id}/cancel` | Cancel a paused run (existing route, extended to waits) | `workflows.run` |
| GET | `/api/v1/public/waits/{token}` | Public resume page payload (fields, message, state) | — |
| POST | `/api/v1/public/waits/{token}` | Submit a resume (HMAC verified, single use, schema validated) | — |
| GET · POST | `/api/v1/public/forms/{token}` | Same mechanism used by a waiting-form submission | — |
| POST | `/api/v1/workflows/{id}/waits/preview` | Render the form or message as the recipient sees it | `workflows.manage` |

Codes: `wait_not_found`, `wait_token_invalid`, `wait_token_expired`, `wait_token_used`,
`wait_signature_invalid`, `wait_expired`, `wait_already_resumed`, `wait_resume_payload_invalid`,
`wait_payload_too_large`, `wait_state_conflict`, `wait_max_window_exceeded`,
`wait_pending_response_timeout`, `wait_channel_failed`, `wait_assignee_required`.

### Data model

Migrations `0036_workflow_waits.sql`, `0037_workflow_form_submissions.sql` (reserved band 0030–0039 for the workflow editor family, REQ-086–096; append-only ledger — take the next free number if taken).

```sql
-- 0036: one row per resumable wait; the token is stored hashed and never recoverable
create table workflow_waits (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    workflow_id uuid not null references workflows (id) on delete cascade,
    execution_id uuid not null references workflow_executions (id) on delete cascade,
    step_id uuid not null references workflow_steps (id) on delete cascade,
    node_key text not null, kind text not null, state text not null default 'pending',
    token_hash text not null, token_hint text not null, key_version integer not null default 1,
    expires_at timestamptz not null, timeout_action text not null default 'continue',
    payload_schema jsonb, accepted_types text[], size_cap_bytes integer not null default 262144,
    await_response boolean not null default false,
    await_response_timeout_seconds integer not null default 30,
    assignee_user_id uuid references users (id) on delete set null, assignee_role text,
    channel text, reminder_after_seconds integer, created_at timestamptz not null default now(),
    resolved_at timestamptz, resolved_by uuid references users (id) on delete set null,
    resolved_reason text, resume_payload jsonb,
    constraint workflow_waits_kind_valid check (kind in ('duration', 'until', 'webhook', 'form', 'approval')),
    constraint workflow_waits_state_valid check (state in ('pending', 'resumed', 'expired', 'cancelled')),
    constraint workflow_waits_timeout_valid check (timeout_action in ('continue', 'fail', 'error_output')),
    constraint workflow_waits_expiry_after_creation check (expires_at > created_at));
create unique index workflow_waits_live_uid on workflow_waits (execution_id, step_id) where state = 'pending';
create index workflow_waits_due_idx on workflow_waits (expires_at) where state = 'pending';
create index workflow_waits_list_idx on workflow_waits (organization_id, state, created_at desc);
create index workflow_waits_assignee_idx on workflow_waits (assignee_user_id)
    where state = 'pending' and assignee_user_id is not null;
create table workflow_wait_submissions (      -- the replay and audit log: a token is accepted once
    id uuid primary key default gen_random_uuid(),
    wait_id uuid not null references workflow_waits (id) on delete cascade,
    token_hash text not null, accepted boolean not null, reason text,
    source_ip inet, user_agent text, payload jsonb, bytes integer,
    created_at timestamptz not null default now());
create unique index workflow_wait_submissions_used_uid on workflow_wait_submissions (wait_id, token_hash)
    where accepted;
create index workflow_wait_submissions_wait_idx on workflow_wait_submissions (wait_id, created_at desc);

-- the run needs a waiting state and a wake-up column for the sweeper
alter table workflow_executions add column waiting_since timestamptz, add column wait_till timestamptz;
alter table workflow_executions drop constraint workflow_executions_status_valid;
alter table workflow_executions add constraint workflow_executions_status_valid
    check (status in ('running', 'waiting', 'completed', 'failed', 'cancelled'));
alter table workflow_executions drop constraint workflow_executions_finished_shape;
alter table workflow_executions add constraint workflow_executions_finished_shape
    check ((status in ('running', 'waiting')) = (finished_at is null));
create index workflow_executions_waiting_idx on workflow_executions (wait_till) where status = 'waiting';
-- 0037: waiting-form submissions (field definitions live on the node's params)
create table workflow_form_submissions (
    id uuid primary key default gen_random_uuid(),
    wait_id uuid not null references workflow_waits (id) on delete cascade,
    organization_id uuid not null references organizations (id) on delete cascade,
    payload jsonb not null default '{}'::jsonb, accepted boolean not null default true,
    source_ip inet, user_agent text, submitted_at timestamptz not null default now(),
    constraint workflow_form_submissions_payload_object check (jsonb_typeof(payload) = 'object'));
create index workflow_form_submissions_wait_idx on workflow_form_submissions (wait_id, submitted_at);
create index workflow_form_submissions_org_idx on workflow_form_submissions (organization_id, submitted_at desc);
```

The wait step reuses the existing `workflow_steps` contract: `kind = 'wait'`, parked via `available_at`,
with at most two claims (park, resume), so a corrupted row cannot park itself in a loop.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `workflows.wait.created` · `.resumed` | A resumable wait is issued · a resume was accepted | `wait_id`, `execution_id`, `kind`, `resumed_by` |
| `workflows.wait.expired` · `.rejected` · `.reminder_sent` | Deadline taken · submission refused · reminder dispatched | `wait_id`, `action`, `code`, `assignee_user_id` |
| `workflows.form.submitted` | A waiting-form submission was accepted | `wait_id`, `submission_id`, `field_count` |
| `workflows.pending_response.timed_out` | A held caller was released with 202 | `execution_id`, `webhook_id`, `held_ms` |
| `workflows.approval.answered` | An approval decision resumed a run | `wait_id`, `decision`, `actor_user_id` |

Consumed: `approvals.decision.recorded` (REQ-059) resumes the matching approval wait;
`notifications.delivery.failed` (REQ-119) marks the channel failed and either retries, degrades to the
timeout branch or records the error for the assignee; `workflows.execution.cancelled` cancels live waits
and invalidates their tokens.

### Acceptance criteria

- [ ] A `wait_for_duration` node parks the run as `waiting` and it resumes on time with no manual action (wall-clock measured).
- [ ] `wait_until` resumes at the timestamp, honours the timezone, and refuses a deadline beyond the maximum window with `wait_max_window_exceeded`.
- [ ] A wait whose timer was deleted is resumed by the sweeper within one sweep interval, proving the safety net.
- [ ] The waiting view lists the paused run with kind, waiting since and a countdown matching the stored deadline.
- [ ] A valid waiting-webhook resume resumes exactly once with the submitted payload; a second post of the same token fails with `wait_token_used`, and a tampered, foreign or expired token fails with its own code writing nothing to the wait row.
- [ ] Two concurrent submissions of the same token produce one resume (`wait_already_resumed` for the loser), verified by run count.
- [ ] Tokens are stored hashed (row inspection), the URL carries no readable reference, and regenerating invalidates the previous link.
- [ ] A waiting form renders its fields, blocks an invalid submission with field messages, accepts a valid one, stores it and resumes with that payload.
- [ ] Send-and-wait delivers exactly one message, the link resolves, and each decision (approve, reject, custom) resumes with the matching payload; an approval wait appears in the approvals inbox for its assignee and deciding there records the actor.
- [ ] `continue` resumes with a `timed_out` flag and emits `workflows.wait.expired`; `fail` fails the run; `error_output` routes an item to the error port.
- [ ] An awaiting-response call answered in time returns the mapped body; a timeout returns 202 with a status URL, emits `workflows.pending_response.timed_out`, and releases the connection (no growth in held connections).
- [ ] A pending wait survives an API restart and resumes with the same token, and no wait state exists only in memory.
- [ ] Cancelling a run with live waits cancels them, invalidates their tokens, and later submissions fail with a named code.
- [ ] Resume produces exactly one lifecycle transition for the resumed node: one resume entry in history and no stuck running badge on the canvas.
- [ ] The public resume page returns distinct non-leaking messages for expired, used and invalid tokens and needs no account for a non-user recipient.
- [ ] Issuance, submission, rejection and operator actions are audited with actor and source, retrievable for a fixture wait.

### QA plan

Seed one workflow with four waits: a duration wait, a waiting webhook with a payload schema, a waiting form with three fields, and a send-and-wait approval assigned to a fixture user. Walkthrough: run the duration workflow and watch it turn `waiting` then complete on its own; open `/workflows/waiting` and check counts, kinds and countdowns; open a wait detail and copy the resume URL; submit a valid payload, replay the same URL (refused), tamper a token (refused); open the form link in a fresh session, submit an invalid value (field errors) then a valid one and see the run resume with that payload; approve from the inbox and confirm the actor; expire a wait and check the configured branch; exercise the awaiting-response webhook for the fast and timeout paths; restart the API with a wait pending and resume; cancel a run with a live wait and confirm the token stops working.
Visual check: real rows with correct relative times, recipient-view pages free of internal identifiers, the run banner matching the wait state, and never more than a token hint on screen.

### Slices

1. **Wait node and sweeper integration** — duration and until waits, deadlines, maximum window, run `waiting` state, waiting view and summary. Done: a parked run resumes on time and the sweeper recovers a lost timer.
2. **Tokens, webhook resume, pending responses** — issuance and HMAC verification, exactly-once resume, submission log, awaiting-response with timeout fallback. Done: valid, tampered, expired and used paths behave as specified and no connection leaks.
3. **Waiting forms and send-and-wait approvals** — form definition and public page, submissions table, channel message with decisions, inbox integration. Done: a submission and each decision resume once with the actor recorded.
4. **Timeout branches, cleanup, audit** — timeout behaviours, expiry settlement, retention of tokens and payloads, audit entries, reminders. Done: every timeout behaviour is visible in run history and retention removes tokens without breaking history.

### Risks / notes

- Resume tokens are bearer credentials: hashed at rest, single use, hinted only, never logged (request logging must mask the path segment), and bound to one execution, step and wait. A leak starts work, it does not read data, but it is still a credential.
- Mail scanners and proxies prefetch links, so a GET on the resume address must never resume by itself; a confirmation step performs the POST. Test with a scanner-like client.
- Exactly-once resume depends on the conditional update, never a check-then-write, or two concurrent submissions both resume.
- Duplicate lifecycle events on resume are a known trap (lesson 7): transition the step once, and let the canvas read run state from the API rather than local assumptions.
- Widening the executions status constraint is a released-migration change: additive, and it must ship in the same release as the engine's `waiting` handling, or a run can appear stuck.
- Escalation and reminders belong to REQ-059 and delivery to REQ-119; calling those primitives is required, since a second scheduler here would double-send reminders, and paused runs hold retention until they settle, so the maximum wait window stays a deliberate default rather than unlimited.
