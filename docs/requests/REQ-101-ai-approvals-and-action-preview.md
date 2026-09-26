# REQ-101 — AI Approvals & Action Preview

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/ai-hub` + admin
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Nothing dangerous happens without a human.

- Approval gates by tool class: publish, delete, plugin install, theme change, deployment, database operation.
- Action preview: the exact field-level diff an AI operation would produce, before it runs.
- Conversation-to-operations: the AI proposes a change set; the user edits and confirms it.
- Dangerous-action warnings with a typed confirmation for irreversible steps.
- Every decision recorded in the audit log with the requester, model and diff.

## Implementation spec

### Scope (in / out)

**In** — the gate, the preview and the record:

- **Gates by tool class.** A policy row maps each dangerous class — `content_publish`, `content_delete`, `plugin_install`, `theme_change`, `deployment`, `database_operation` — to `require` (the default for all six) or `allow`, with an expiry window and whether typed confirmation is required. A tool whose class requires approval parks its run instead of executing (REQ-099), whatever permissions the caller holds.
- **Action preview as a first-class object.** Before a gated tool executes, the runtime computes the exact field-level diff it *would* produce, using the same service the execution path calls — one code path, so a preview cannot promise something the apply does not do. The diff is stored on the approval with a content hash and the base revision it was computed against.
- **Change sets from conversation.** A conversation (docs/06 §11) may end in a proposed change set: an ordered list of operations (`create`, `update`, `delete` per resource), each carrying its target and field-level diff. The user edits values, drops operations, reorders them and confirms; confirmation applies through the same gated pipeline, so a set containing a gated operation still parks for approval.
- **Typed confirmation** for irreversible operations (`content_delete`, `deployment`, `database_operation`): the server requires a confirmation phrase — the resource's name — in the approve call; a checkbox is not a confirmation and the API refuses without the phrase.
- **Stale detection.** Every approval carries the resource revision and a preview hash; if the resource changed between preview and decision, applying refuses with `stale` and the screen offers Re-preview. The decision is single-use and bound to the hash.
- **Audit.** Every request, decision and application writes an append-only `audit_log` row with `actor_type = 'agent'` for the requesting run and metadata naming the requester, the agent, the model, the decision maker, the preview hash and the operation count; rejections carry their reason.

**Out**

- Deciding what a model may *ask* for — tools and permissions are REQ-100's.
- The loop's stop conditions and run persistence (REQ-099); this request parks and resumes runs.
- Notification delivery (REQ-021 emits from the same events) and general audit browsing (REQ-014).
- Rollback execution: undoing a deletion restores the deleted resource, and rolling back a deployment is a new deployment (REQ-024).
- Stepping through a model's prose — the reviewer decides on operations, not on a paragraph.

### Screens (UI)

- **`/ai/approvals`** — inbox table: Requested (relative + tooltip), Requester, Agent, Tool, Class, Resource (`type` + name/`id` truncated), Risk, Operations, Expires in, Status. Status tabs (Pending · Decided · Expired · Stale · Applied · Failed) with counts; filters agent, tool, class, date range, requester; search by resource. Row actions Review, quick Reject (with reason). Empty state "Nothing waiting for you" linking to the policy screen; `LoadingTable`; error banner with the API message and Retry; a pending count badge sits next to the AI Hub sidebar entry.
- **`/ai/approvals/[id]`** — the review screen, the heart of this request. **Header**: who asked (user and agent), the goal sentence, the proposing model, the run link, tokens and cost so far, class, risk badge, expiry countdown.
- **Proposed operations** — one card per operation: resource link, action (`create`/`update`/`delete`), and the **field-level diff** — two columns OLD/NEW with unchanged fields collapsed behind a "Show unchanged (n)" toggle, changed fields highlighted, long values wrapped (never silently truncated), media operations rendering before/after thumbnails, deletes rendering the full record plus its cascade count ("1 page, 4 revisions").
- **Edit mode** — every NEW value is editable in place, an operation can be dropped and operations can be reordered; each edit re-renders the diff, records the editor and time on the change set and updates the preview hash; editing after a decision is refused (`409`).
- **Danger zone** — irreversible operations render an inline warning stating the exact consequence, the typed-confirmation field (with the required phrase shown: type the resource name), and what the action cannot undo.
- **Decision bar** — Approve (primary), Reject with a required reason (≤ 500), Re-preview when the stale banner is up, and "Apply and open result" after success; the bar sticks to the bottom on mobile. **Timeline** — requested, each edit, the decision and the application attempt, with actor and timestamp.
- **Change set editor (sheet)** — opened from a chat reply proposing operations ("Proposed changes (3)") or from the inbox; the same operation cards and diff engine in a resizable sheet with Confirm / Discard; Confirm applies directly when nothing is gated, otherwise it creates the approval request.
- **`/ai/approvals/policies`** — one row per class: Class, Mode (Require approval · Allow without approval), Typed confirmation, Expiry minutes (5–1440, default 60), Approvers (the `ai.approvals.act` permission), Updated by/at, Reset to default. Switching a dangerous class to `allow` requires `ai.policies.manage` *and* a typed confirmation naming the class; the row then keeps a warning stripe, and every policy change writes an audit row.
- **States** — every screen has `LoadingTable` and a real empty state; a stale approval renders the banner with Re-preview and disables Approve; an expired approval is read-only with the reason; a failed application shows the error beside the failing operation and states whether the rest was applied (all-or-nothing).
- **Keyboard** — `⌘K` palette, `⌘⇧A` AI Hub, `G` then `V` approvals, `J`/`K` next/previous item, `A` approve, `R` reject, `E` edit mode, `P` re-preview, `/` focus search, `↑/↓` + `Enter` move/open, `Esc` close.
- **Mobile (<1024px)** — the diff stacks (OLD above NEW with both labels), the table becomes cards, the review screen and the editor are full-height sheets, the action bar is sticky with Approve/Reject side by side, and typed confirmation stays a real text field; nothing is hover-only.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/ai/approvals` | Inbox (`status`, `agent`, `tool`, `class`, `requester`, `from`, `to`) with counts per status | `ai.approvals.read` |
| GET | `/api/v1/ai/approvals/{id}` | One request: operations, frozen preview, timeline, run link | `ai.approvals.read` |
| POST | `/api/v1/ai/approvals/{id}/approve` | Decide; body carries `confirmation` (typed phrase) when required; applies through the gated pipeline | `ai.approvals.act` |
| POST | `/api/v1/ai/approvals/{id}/reject` | Decide with a required `reason` (≤ 500); the run ends without the effect | `ai.approvals.act` |
| POST | `/api/v1/ai/approvals/{id}/preview` | Recompute the diff against the current revision; refuses when the hash already matches | `ai.approvals.read` |
| GET | `/api/v1/ai/approvals/{id}/audit` | The audit rows for one request and decision | `ai.approvals.read` |
| POST | `/api/v1/ai/change-sets` | Create a change set (operations with targets and values) | `ai.chat` |
| GET | `/api/v1/ai/change-sets/{id}` | Read a change set with its computed diffs | `ai.approvals.read` |
| PATCH | `/api/v1/ai/change-sets/{id}` | Edit operations/values before confirmation (refused after apply) | `ai.approvals.act` |
| POST | `/api/v1/ai/change-sets/{id}/confirm` | Confirm: apply directly, or create an approval request when a gated operation is present | `ai.approvals.act` |
| POST | `/api/v1/ai/change-sets/{id}/discard` | Drop it with a required reason | `ai.approvals.act` |
| GET/PUT | `/api/v1/ai/approval-policies` | Read / replace the class policy table | `ai.approvals.read` / `ai.policies.manage` |

New catalogue keys: `ai.approvals.read`, `ai.approvals.act`, `ai.policies.manage`. Every route sits behind `guards::require("…")`, and decisions are refused with stable codes per shape: `already_decided` (409), `expired` (410), `stale` (409, with the current revision), `confirmation_required` (422), `confirmation_mismatch` (422) and `not_permitted` (403 with the missing key).

### Data model

Migration `database/migrations/0020_ai_approvals.sql` (take the next free number at implementation time).

| Table | Columns (types) | Indexes |
|---|---|---|
| `ai_approvals` | id uuid pk, organization_id uuid → organizations cascade, site_id uuid → sites cascade null, run_id uuid → ai_runs set null, step_id uuid → ai_run_steps set null, agent_id uuid → ai_agents set null, identity_id uuid → ai_identities set null, change_set_id uuid → ai_change_sets set null, tool_key text, tool_class text, resource_type text, resource_id text, resource_label text, risk text (`low`,`medium`,`high`), title text, summary text, operation_count int ≥ 1, irreversible bool default false, requires_confirmation bool default false, confirmation_phrase text null, preview jsonb not null, preview_hash text not null, base_revision text null, status text (`pending`,`approved`,`rejected`,`expired`,`stale`,`applied`,`failed`), requested_by uuid → users set null, model_id uuid → ai_models set null, expires_at timestamptz not null, decided_by uuid → users set null, decided_at, decision_note text null, applied_at, error text null, created_at | checks `(status = 'pending') = (decided_at is null)` and `expires_at > created_at`; indexes `(organization_id, status, expires_at)`, `(status)` where `status = 'pending'`, `(run_id)`, `(created_at)` for the sweeper |
| `ai_change_sets` | id uuid pk, organization_id uuid cascade, site_id uuid cascade null, title text, status text (`draft`,`pending`,`confirmed`,`applied`,`discarded`,`expired`), operations jsonb not null default `'[]'`, base_revisions jsonb default `'{}'`, created_by uuid → users set null, created_by_agent uuid → ai_agents set null, created_by_run uuid → ai_runs set null, confirmed_at, applied_at, discarded_reason text, created_at, updated_at | `(organization_id, created_at desc)`; `(status)` where `status in ('draft','pending')` |
| `ai_approval_policies` | id uuid pk, organization_id uuid cascade null (null = platform default row), tool_class text, mode text (`require`,`allow`), typed_confirmation bool default true, expires_minutes int default 60 (5–1440), updated_by uuid → users set null, updated_at | unique folded `(coalesce(organization_id…), tool_class)` |

- **One preview implementation.** `preview(operation) -> diff` and `apply(operation, diff)` live in one module and share the field mapping; a test asserts that applying a frozen preview yields exactly the previewed values, per field and per operation.
- The preview hash is `sha256` over the canonical JSON of the operations plus each resource's base revision; the approve call recomputes it and refuses on mismatch (`stale`).
- An approval is single-use: the first decision transitions it under `select … for update` and a second decision answers `already_decided` without touching anything.
- Expiry: a sweeper tick in the API process (config `OMNION_AI_APPROVALS_SWEEPER`, default on) marks `pending` rows past `expires_at` as `expired`, emits `ai.approval.expired` and resumes the parked run so it fails cleanly instead of hanging.
- Policies ship as platform defaults (`organization_id = null`) seeded by the migration: all six classes `require`, `typed_confirmation` true, 60 minutes; an organization row overrides a default and a missing row means inherit.
- Audit: the request writes one row (`action = 'ai.approval.requested'`, `actor_type = 'agent'`, `actor_user_id` = requester, metadata `{ tool_key, class, run_id, preview_hash, model_id, operation_count }`), and each decision and application writes its own row with the decider.

### Events

| Event | Kind | Payload / webhook relevance |
|---|---|---|
| `ai.approval.requested` | emitted | run, step, tool, class, requester, expires_at — drives REQ-021 notifications |
| `ai.approval.approved` | emitted | approval, decided_by, applied_at — the run resumes from here |
| `ai.approval.rejected` | emitted | approval, decided_by, reason |
| `ai.approval.expired` | emitted | approval, waited seconds |
| `ai.approval.stale` | emitted | approval, resource, base revision vs current |
| `ai.changeset.confirmed` | emitted | change set, operation count, requester |
| `ai.changeset.applied` | emitted | change set, applied and failed counts, duration |
| `ai.changeset.failed` | emitted | change set, failing operation, error code, applied count |
| `ai.changeset.discarded` | emitted | change set, reason |

All dotted lower-case on the signed webhook bus; approval events carry the organization and site so an org-scoped endpoint receives only its own deliveries. A policy change rides `audit_log` without a webhook event, because it is an administrative act rather than a workflow fact.

### Acceptance criteria

- [ ] All six dangerous classes are gated by default on a fresh installation, and a gated tool call parks its run as `awaiting_approval` with no side effect (test asserts the target row is unchanged).
- [ ] An approval is required even when the caller holds the underlying domain permission — permissions gate *who may ask*, approval gates *what happens*.
- [ ] Approving applies exactly the previewed operations: every written field value equals the value in the frozen preview, asserted per field in a test.
- [ ] The preview and the apply share one implementation: a test mutates a field mapping and fails both together.
- [ ] A resource edited between preview and decision makes the approval `stale`, the apply refuses with the `stale` code and the current revision, and the screen offers Re-preview.
- [ ] A single-use approval cannot be decided twice (`already_decided`) and the second attempt changes nothing.
- [ ] An approval past its expiry cannot be decided (`expired`), the sweeper resumes the parked run, and the inbox shows it read-only.
- [ ] Typed confirmation is enforced server-side: a missing phrase answers `confirmation_required`, a wrong phrase answers `confirmation_mismatch`, and the UI never sends the phrase unless the field is filled.
- [ ] A delete operation counts its cascades: the preview names the record and its dependents ("1 page, 4 revisions") and the applied result matches the count.
- [ ] Rejecting requires a reason and resumes the run with `stop_reason = cancelled`, no effect, and the reason visible on the approval and in the run trace.
- [ ] A change set confirmed from the chat reply lands in the same inbox (one pipeline, one screen), and one containing a gated operation creates an approval instead of applying.
- [ ] Editing a change set records the editor and time, re-renders the diff and updates the preview hash; editing after a decision is refused with `409`.
- [ ] Applying a multi-operation change set is all-or-nothing: a failure in operation 3 leaves operations 1–2 rolled back, with `failed` status, the failing operation named and an `ai.changeset.failed` event.
- [ ] Every request, decision and application has an `audit_log` row with `actor_type = 'agent'`, the requester, the model id and the preview hash (asserted against SQL).
- [ ] A viewer with `ai.approvals.read` but not `ai.approvals.act` sees Approve/Reject disabled with the missing permission named, and the API refuses with `403` naming it.
- [ ] Setting a dangerous class to `allow` requires `ai.policies.manage` plus a typed confirmation naming the class, writes an audit row and keeps a warning stripe.
- [ ] Organization A cannot read or decide organization B's approvals (404), and a decision in one organization cannot resume a run in another.
- [ ] Notification volume is bounded: requesting the same tool in a loop produces one pending approval and an `already_pending` refusal, not a flood.
- [ ] Every screen has empty, loading and error states with a real call to action; no dead control and no placeholder text.
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and the QA walkthrough are green with zero high findings.

### QA plan

The walkthrough must: trigger a gated publish from an agent run and watch the run park with the approval in `/ai/approvals`; open the review screen and read the field-level diff with unchanged fields collapsed; switch to edit mode, change one NEW value, drop one operation and confirm the hash marker updates; approve and see the run resume with the applied values on the resource page; trigger a delete approval, try to approve without the typed phrase (refused with the field error) and then with it; edit the target resource in another tab and approve (stale banner → Re-preview → approve); reject one approval with a reason and watch the run end without the effect; let one expire under a short test policy and confirm it goes read-only and the run resumes; run the chat proposal flow and confirm a gated change set lands in the inbox; open `/ai/approvals/policies`, flip a dangerous class to `allow` with the typed confirmation, see the stripe, then reset. The mobile pass (390×844) runs the review screen, the editor sheet and the stacked diff; the fresh-database pass checks the seeded policy table and the empty inbox.

The visual check must see: OLD/NEW columns readable at 1280 px and stacked at 390 px, changed-field highlighting that is not colour-only (a marker plus the label), the danger warning distinguishable from an error, the sticky action bar not covering the last operation card, the typed-confirmation field a real input with the phrase visible, no raw i18n keys, and no text overlapping the diff rows.

### Slices

1. **Gate, inbox, decisions, audit** — `ai_approvals` and policies, the park-at-request path in the loop, the sweeper, approve/reject/re-preview endpoints, the inbox and the review screen without edit mode, audit rows and events.
   *Done when:* a gated call parks, a decision executes exactly the preview, expiry sweeps, and every step has its audit row.
2. **Preview and diff engine** — the shared preview/apply module, the field-level diff renderer, base revisions and the preview hash, stale detection, typed confirmation, the irreversible warnings.
   *Done when:* preview and apply provably agree field by field, a mid-flight edit produces `stale`, and typed confirmation cannot be bypassed through the API.
3. **Change sets, policies and polish** — `ai_change_sets`, the editor sheet, the chat proposal entry, confirm/discard, all-or-nothing application, the policy screen with its guardrail, notification events, mobile and empty-state passes.
   *Done when:* a three-operation set confirmed from chat applies atomically, the gated variant lands in the inbox, and a dangerous policy flip needs both the permission and the phrase.

### Risks / notes

- **Approval fatigue is a real failure mode.** Keep the gated set narrow (the six classes), let low-risk tools run ungated, and make a decision take one screen and two minutes — a gate people click through blindly protects nothing.
- **The preview must be the execution.** Any second implementation drifts, so the diff and the apply share one field-mapping module with a test that fails them together; previews are stored frozen so the reviewer decides on what they saw, and the hash proves it.
- **Time-of-check to time-of-use is the correctness risk.** Base revisions plus the hash make drift visible, the apply refuses rather than merging, and Re-preview is one click.
- Irreversible means irreversible: typed confirmation for deletes, deployments and database operations, consequence text in plain language, and never a "don't ask again" shortcut for those classes.
- Change set edits are still the platform applying user-supplied values — the values are re-validated by the same tool schemas (REQ-100) at apply time, so an edit cannot smuggle an argument a model would have been refused.
- An approval never widens authority: the tool's own permission check runs again at apply time with the requesting identity, and a permission revoked between request and decision fails the apply with a clear message.
- The events feed notifications (REQ-021), so a pending approval must not re-notify on every render, and the duplicate-request guard (`already_pending`) must hold across restarts rather than only in memory.
