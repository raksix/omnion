# REQ-073 — Temporary & Approval-Based Access

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** core (`crates/permissions`) + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Time-boxed and approved elevation.

- Temporary roles with automatic expiry (duration picker, countdown, auto-revoke).
- Emergency Administrator elevation (logged, notified, expiring by default).
- Access request workflow: request → approver → time-boxed grant → record.
- Integration with the approvals module (REQ-059) and the audit log.
- "Who has elevated access right now" view.

## Implementation spec

> **Where:** `crates/permissions` (request store, grant windows, expiry sweep) · `apps/api/src/routes/iam.rs` · `apps/admin/features/iam/access/**` (new screens) · **Migration:** `database/migrations/0019_temporary_and_approval_based_access.sql` (next free number at land time; the ledger is append-only — take the next free slot if one is already used) · **Admin routes:** `/settings/iam/access/requests`, `/settings/iam/access/approvals`, `/settings/iam/access/grants` · **Permission family:** `iam.approvals.*`, `iam.bindings.*`, `users.read` · **Depends on:** REQ-006 (`role_bindings.expires_at` and the sketch of `permission_requests` live there — this request makes them a product), REQ-070 (the scope a grant is placed at), REQ-067 (role picker), REQ-021 notifications (reminders), REQ-012 security centre (break-glass signal), REQ-059 stays the generic approvals module for business documents — this is the access-specific flow.

### Scope (in / out)

**In**

- **Temporary grants:** a binding with `expires_at` — created directly by an administrator inside an allowed cap, or produced by an approved request. Expiry resolves at request time (`now() < expires_at`), so a lagging sweep never widens access; the sweep only emits the event and revokes the row. The UI shows the remaining time on every surface that lists the grant.
- **Request flow:** a user (or an administrator on behalf of a user) requests a role or a single permission at a scope, with a mandatory justification (min 20 characters), a requested window, an optional resource pattern (REQ-070 globs) and an optional template. A request that would exceed the organization's maximum window or the requester's eligibility fails at submit time with the reason — never after approval.
- **Approver resolution:** whoever holds `iam.approvals.decide` **inside the requested scope** may decide (organization scope → organization approvers; site scope → that site's approvers; the same rule REQ-006 uses for guards). Self-approval is refused (`403`, four-eyes) unless the requester holds the platform-level break-glass key and marks the request as emergency. If no approver exists in scope, the request is routed to the escalation list and the screen says so instead of showing a dead inbox.
- **Decision:** approve with an explicit window (bounded by the organization cap), reject with a required reason, or approve-with-narrowing (fewer permissions, shorter window) which is recorded as a distinct outcome. Approving creates the binding in the same transaction as the status change and links it back (`resulting_binding_id`), so an approved request without an active grant is impossible.
- **Lifecycle:** pending requests expire after a configurable TTL (default 7 days) into `expired`; the requester may cancel while pending; an approved grant may be revoked early by any approver in scope with a reason; extension is a new request referencing the previous one.
- **Delegation:** an approver may delegate the inbox for a scope and a window (for example during leave); each delegation is visible on the approvals screen, expires on its own, and never grants the delegate anything beyond deciding on that scope's requests.
- **Emergency access:** a break-glass path for a user who holds the platform key — the grant is immediate, capped (default 4 hours, configurable), announced to the security channel, and lands in the post-hoc review queue with a mandatory review and outcome within 72 hours.
- **Review queue:** standing grants (non-expiring bindings) older than a configurable age (default 180 days) surface for periodic review where the reviewer confirms, narrows or revokes; the decision and its note are kept as evidence.
- **Reminders:** `access.expiring_soon` at 24 h and 60 min before a grant ends, `access.request_pending` daily to the approver, `access.request_expired` to the requester, `access.break_glass_review_due` to the security reviewers.
- **Audit:** every request, decision, grant, revocation and expiry writes an audit entry with actor, target, scope, window and justification; the compliance export (REQ-038) reads these rows and nothing else.

**Out (tracked elsewhere)**

- Multi-step document approval chains, signature steps and business-process approvals → REQ-059; notification rendering and channels → REQ-021; anomaly scoring on break-glass → REQ-012; compliance report templates and retention → REQ-038; ABAC conditions → REQ-069; the simulator → REQ-074; service-account key issuance (already step-up protected) → REQ-072.

### Screens (UI)

Nav entries under the existing IAM section: **Requests · Approvals · Grants**.

| Route | Screen |
|---|---|
| `/settings/iam/access/requests` | My requests and (for approvers) the scope's requests, with tabs `Pending`, `Approved`, `Rejected`, `Expired` |
| `/settings/iam/access/requests/{id}` | Request detail — subject, scope, requested permissions, justification, timeline, decision panel |
| `/settings/iam/access/approvals` | Approver inbox grouped by scope with age, requester and the decision controls |
| `/settings/iam/access/grants` | Active time-boxed grants, remaining time, early-revoke action |
| `/settings/iam/access/review` | Standing-access review queue and the break-glass review list |

- **Request wizard (3 steps):** what (template chips — *Temporary Editor*, *Emergency Administrator*, *Temporary Deployment Access* — or a role/permission picker), scope (REQ-070 scope picker plus optional resource pattern with a live example), why (justification, requested window, urgency note). The review step shows the derived effective delta: *“adds `content.pages.publish` on `acme.com` → `/blog/*`, for 2 hours”*.
- **Approver inbox** columns `Requester`, `Scope`, `What`, `Window`, `Age`, `Urgency`, `Actions`; the decision panel opens inline with `Approve` (window stepper, cap shown), `Reject` (required reason) and `Narrow & approve`; no dialog may be dismissed by a single stray `Esc` before a decision is confirmed — the confirm step is explicit.
- **Grants screen** columns `Holder`, `What`, `Scope`, `Granted by`, `Started`, `Remaining` (live countdown), `Source` (`request`, `direct`, `break-glass`); the break-glass rows carry a distinct chip and link to their review.
- **Timeline** on the request detail: submitted → notified → decided (with who and when) → grant active → expiring → expired/revoked, each entry carrying the actor and the note.
- **States, keyboard, mobile:** `EmptyState` with the primary action (`Request access` / *Nothing waiting for you*), `LoadingTable` on first paint, inline error with retry; `/` focuses search, `j`/`k` move focus, `enter` opens, `a` approves and `r` rejects the focused inbox row (both still confirming), `?` opens the shortcut sheet; below `lg` tables become cards, the wizard is single-column with a sticky footer.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET, POST | `/api/v1/iam/access/requests` | List (own or scope-visible) / submit a request | signed-in session (`users.read` to see others') |
| GET, DELETE | `/api/v1/iam/access/requests/{id}` | Detail with timeline / cancel while pending | requester or `iam.approvals.read` |
| POST | `/api/v1/iam/access/requests/{id}/decide` | Approve with a window, reject with a reason, or narrow-and-approve | `iam.approvals.decide` in scope |
| GET | `/api/v1/iam/access/approvals` | Approver inbox for the caller's scopes | `iam.approvals.read` |
| POST, DELETE | `/api/v1/iam/access/delegations` | Delegate an inbox for a scope and window / end a delegation | `iam.approvals.decide` |
| GET | `/api/v1/iam/access/grants` | Active time-boxed grants with remaining time | `iam.bindings.read` |
| DELETE, POST | `/api/v1/iam/access/grants/{id}` · `/{id}/extend` | Revoke early with a reason / request an extension | `iam.approvals.decide` / signed-in session |
| GET, POST | `/api/v1/iam/access/templates` | Request templates / create one | `iam.approvals.read` / `iam.approvals.manage` |
| POST | `/api/v1/iam/access/break-glass` | Emergency grant with the platform key | step-up + break-glass key |
| GET, POST | `/api/v1/iam/access/review` · `/{id}/decide` | Review queue / confirm, narrow or revoke with a note | `iam.approvals.decide` |
| GET, PUT | `/api/v1/iam/access/policy` | Window caps, request TTL, review age, break-glass cap | `iam.approvals.read` / `iam.approvals.manage` |

Every route carries `guards::require("<key>")` where the permission column says so; handlers additionally verify that the addressed request, grant or scope belongs to the caller's organization and answer `404` for a foreign id.

### Data model

```text
permission_requests  (id, organization_id, requester_id references users (id) on delete cascade,
                      subject_type check (subject_type in ('user','service_account')), subject_id uuid,
                      request_kind check (request_kind in ('role','permission','template')),
                      role_id uuid references roles (id) on delete set null, permission_keys text[],
                      scope_type text, organization_id/site_id/department_id/module_key/resource_type/resource_id,
                      justification text not null, urgency text not null default 'normal' check (urgency in ('normal','urgent')),
                      requested_minutes integer not null, status text not null default 'pending'
                        check (status in ('pending','approved','narrowed','rejected','cancelled','expired')),
                      decided_by uuid, decided_at timestamptz, decision_note text,
                      resulting_binding_id uuid references role_bindings (id) on delete set null,
                      break_glass boolean not null default false, deadline_at timestamptz, created_at, updated_at)
access_request_templates (id, organization_id, key, name, description, role_id, permission_keys text[],
                      scope_preset text, default_minutes integer, max_minutes integer,
                      break_glass boolean not null default false, enabled boolean, created_by, created_at)
approval_delegations (id, organization_id, delegator_id, delegate_id, scope_type, organization_id/site_id/department_id,
                      starts_at, ends_at, created_by, created_at, revoked_at)
break_glass_reviews   (id, organization_id, request_id references permission_requests (id) on delete cascade,
                      reviewer_id, status check (status in ('pending','confirmed','narrowed','revoked')),
                      note text, decided_at, due_at timestamptz, created_at)
access_policy         (organization_id primary key, request_ttl_hours integer not null default 168,
                      max_window_minutes integer not null default 480, break_glass_max_minutes integer not null default 240,
                      review_age_days integer not null default 180, updated_by uuid, updated_at)
```

Indexes: `permission_requests_org_status_idx (organization_id, status, created_at desc)`, `permission_requests_subject_idx (subject_type, subject_id, status)`, `permission_requests_deadline_idx (deadline_at) where status = 'pending'`, `approval_delegations_window_idx (delegate_id, starts_at, ends_at) where revoked_at is null`, `break_glass_reviews_due_idx (due_at) where status = 'pending'`, plus the reused `role_bindings_expires_idx` from REQ-006 for the sweep.

Migration `database/migrations/0019_temporary_and_approval_based_access.sql`: extends `permission_requests` if REQ-006 already created it (same table and column names — one table per concept; adding columns only) and creates `access_request_templates`, `approval_delegations`, `break_glass_reviews` and `access_policy`; it seeds one `access_policy` row per existing organization with the defaults above. Additive and commented in the `0002` style.

### Events

- Emitted: `iam.access_requested`, `iam.access_approved`, `iam.access_narrowed`, `iam.access_rejected`, `iam.access_cancelled`, `iam.access_granted` (binding created), `iam.access_expiring` (24 h and 60 min), `iam.access_expired`, `iam.access_revoked`, `iam.approval_delegated`, `iam.break_glass_used`, `iam.break_glass_review_due`, `iam.break_glass_reviewed`, `iam.standing_access_review_due`.
- Consumed: `iam.binding_created` records the reverse link when a grant is created outside the request flow; `user.deactivated` cancels pending requests of that user and revokes their time-boxed grants; `iam.role_permissions_changed` re-evaluates whether a pending request is still meaningful (a request for a permission the role no longer grants is marked stale rather than silently approved).
- Payloads carry ids, scope, window and outcome — never mail addresses or justification text (the security centre reads the row, not the event). `iam.access_*` events are what automation (REQ-003) may trigger on to open a ticket; the security centre (REQ-012) subscribes to break-glass.

### Acceptance criteria

- [ ] `0019_temporary_and_approval_based_access.sql` applies on a fresh and on a populated database (including the case where REQ-006 already created `permission_requests`); `cargo test --workspace` is green.
- [ ] A direct temporary binding stops counting at `expires_at` without being deleted — proven by a test that freezes time before and after the boundary, and the members list marks it expired.
- [ ] Submitting a request without a ≥ 20-character justification is refused with a field-level error; a window above the policy cap is refused at submit, not at approval.
- [ ] Only a holder of `iam.approvals.decide` inside the requested scope can decide; an approver outside the scope gets `403`, and a scope with no approver routes to escalation and says so on screen.
- [ ] Self-approval is refused with `403` and the four-eyes message; the break-glass path is the only exception and only with the platform key plus step-up.
- [ ] Approve creates the binding in the same transaction as the status change: there is no state where a request is `approved` with no active grant (asserted by a failure-injection test).
- [ ] Reject requires a reason and does not touch grants; narrow-and-approve grants strictly less than requested and records the difference in the decision note.
- [ ] An approved grant expires on its own and emits `iam.access_expired` exactly once even if the sweep runs on two instances concurrently.
- [ ] Early revocation by an approver in scope takes effect on the next request and writes an audit entry with the reason.
- [ ] A pending request past its TTL becomes `expired`, the requester is notified, and the same request cannot be approved afterwards (`409`).
- [ ] A delegation lets the delegate see and decide exactly the requests of the delegated scope during the window, and nothing before `starts_at` or after `ends_at`.
- [ ] Break-glass grants are capped, immediately active, announced, and appear in the review queue with a 72-hour due date; a review without a note is refused.
- [ ] Reminder events fire once per threshold per grant (idempotent job proven by running it twice), and the expiring-soon banner appears on the grants screen.
- [ ] Standing grants older than `review_age_days` surface in the review queue; confirming, narrowing and revoking each write their own evidence row.
- [ ] Every request, decision, grant, revocation and expiry writes an audit entry readable by the compliance export; no justification text appears in any event payload.
- [ ] All routes answer 401/403/404/409 as documented; every screen has empty, loading and error states with zero high findings in the QA pass.

### QA plan

Extend `scripts/qa/walkthrough.cjs` with `/settings/iam/access/requests`, `/settings/iam/access/requests/{id}`, `/settings/iam/access/approvals`, `/settings/iam/access/grants` and `/settings/iam/access/review` (desktop) plus the requests list (mobile). The script must submit a request with a 10-character justification and assert the field-level error, then submit a valid one, switch to the approver inbox, assert the age and scope columns, narrow-and-approve it and assert the grants screen shows the shorter window with a live countdown; then revoke it early with a reason and assert the state chip and the audit entry. It must attempt self-approval as the requester and assert the four-eyes refusal, and open the review tab to assert the break-glass list renders with an empty state when clean. Screenshots `page-iam-access-requests`, `page-iam-approvals`, `page-iam-access-grants`, `mobile-iam-access-requests`; the visual check looks for a readable timeline, an unmistakable countdown and AA-contrast outcome chips.

### Slices

1. **Temporary grants and the request store.** Migration, window caps and policy row, request submission with justification and scope, templates, my-requests screen, timeline, audit and events. *Done when:* acceptance 1–3 and 15 pass and a requested window above the cap is refused at submit.
2. **Approvals, delegation and decision integrity.** Approver resolution in scope, inbox screen, approve/reject/narrow with transactional grant creation, four-eyes, delegations, notification wiring, TTL expiry. *Done when:* acceptance 4–7, 10 and 11 pass and the failure-injection test proves no approved-without-grant state.
3. **Sweeps, review and break-glass.** Expiry and reminder sweeps with idempotence, grants screen, early revocation, standing-access review queue, break-glass path with its review flow. *Done when:* acceptance 8–9 and 12–14 pass and both sweeps run twice in CI without duplicate events.

### Risks / notes

- **Expiry must be resolved, not swept.** A grant is inactive because `now() >= expires_at`, never because a job ran; the sweep only cleans up and notifies. Any implementation that relies on the job to close access is a security defect.
- **One decision path again.** The grant created by an approval is an ordinary `role_bindings` row, so the guard, list predicate and simulator see it with no special case; a "temporary" flag checked anywhere else would drift.
- **Four-eyes and escalation need owners.** An organization with no approver in scope must not appear as a working inbox with zero items; route to escalation and say it on the screen.
- **Break-glass is auditable or it is a backdoor.** Platform-key access, step-up, cap, announcement and a review that cannot be skipped; if the review flow is disabled the grant must not be creatable at all.
- **Notification volume.** Reminders are per grant, not per permission inside it; daily pending digests aggregate per approver and scope, and the aggregation is what keeps the notification centre usable at scale.
- **Time source.** All windows compare against the database clock in one place; client-supplied timestamps are refused, and a grant's countdown in the UI is display only — the server answer is the truth.
