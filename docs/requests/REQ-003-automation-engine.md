# REQ-003 — Automation Engine

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core engine (`crates/workflows`) + admin UI
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

A Zapier/Make-style automation engine:

```text
TRIGGER
   ↓
CONDITION
   ↓
ACTION
```

Example:

```text
New user created
        ↓
Role = Customer
        ↓
Send welcome email
        ↓
Create CRM record
        ↓
Call webhook
```

Users build these visually in the UI with the workflow builder (REQ-004).

## Notes

- Engine research: [`docs/09-N8N-TEARDOWN.md`](../09-N8N-TEARDOWN.md) — deep source-level
  teardown of the n8n engine v2.41 (durable steps, queues, waits, HITL approvals) with
  adopt/avoid lessons.

## Implementation spec

### Scope (in / out)

**In** — the engine v0 already runs `trigger → condition → action` durably (migration `0006`,
extended by `0010`: durable step rows, `for update skip locked` claiming, waits, retries ≤5,
cancellation, one event name, seven actions). This request is the depth pass on that machine:

- **Trigger library** — event triggers beyond `page.published`: `user.created`, `user.updated`, `media.created`, `media.deleted`, `workflow.execution.completed`, `ai.run.completed` (REQ-001); plus an **inbound webhook trigger** — each rule gets an unguessable URL (`POST /api/v1/hooks/{token}`) whose body becomes the event payload.
- **Conditions** — groups with `all`/`any` nesting (depth ≤3) around the existing nine operators, each row picking from the payload fields the trigger actually carries.
- **Action library** — the four synthetic actions stay (they prove the retry machine); `send_email` and `comment_revision` stay; new host actions registered by `crates/automation`: `http_request` (outbound call, host allow-list, HMAC-signed with the rule's secret), `publish_page`, `wait_for_approval` (human in the loop), `run_workflow` (chain another rule). Actions owned by future modules (CRM contact, invoice, notification) register through the same registry when those modules ship — no placeholders.
- **Branching and error paths** — `branch` steps (`if`/`else` on a resolved field), `stop` steps, and a per-step `on_error` policy (`stop` | `continue` | `route` to a failure branch).
- **Execution authority** — a rule records `run_as_user_id` (the author by default) and every host action resolves that account's effective permissions at run time; losing a permission stops the rule with `automation.rule.permission_revoked`. A rule is never a permission back door.
- **Reliability** — per-step `timeout_ms` (≤120s), per-rule `rate_limit_per_hour`, `concurrency` (`queue` | `skip`), an endless-loop guard (the same step kind twice in a row with identical resolved parameters aborts with a clear message), resume-from-step and retry-step on the run detail.
- **Test fire** — "Send test event" runs a rule against a captured or hand-written payload without touching the world (host actions are simulated and shown as `would_send`); "Listen for a real event" arms a one-shot listener for the next matching bus event.
- **Operations surfaces** — rule list/detail, run history, run detail with a step trace, templates gallery, rate-limit and failure visibility, audit on every definition change.

**Out**

- The visual node canvas and plugin node types — REQ-004 (this request ships the linear editor the canvas will sit on top of; both write the same definition).
- The shared approval inbox — REQ-059 owns `/approvals`; here approvals are decided on the run detail plus a compact pending panel on `/automations`.
- A public expression language or user code in a step (docs/09 §13, lesson 14): bindings stay `{{event.field}}` and `{{steps.N.output.field}}`.

### Screens (UI)

- **`/automations`** — table: Name, Event (chip), Conditions, Actions, Status (armed/paused/blocked), Runs 7d, Last fired, Owner, Updated. Filters: event, status, site, owner. Bulk Enable, Pause, Delete (confirm by typing the name). Row menu: Open, Run now, Send test event, Duplicate, Enable/Pause, Delete. A "Pending approvals" panel (rule, requested at, requester, Approve/Reject) sits above the table whenever anything waits. Empty state: "No automations yet" with New rule and Browse templates.
- **`/automations/new`** and **`/automations/[id]`** — one editor, two entry points: sections Trigger, Conditions, Actions with a sticky footer (Save, Save & arm, Validate). Trigger: kind (Event / Schedule / Manual / Inbound webhook), then a searchable event picker (grouped, each with its payload fields), or a cron builder (minute/hour/day/month with a human sentence preview and the organization timezone), or the read-only hook URL with Copy and Rotate. Conditions: rows (`field`, `operator`, `value`) inside groups with Add condition, Add group and drag to reorder; only the trigger's real fields are offered. Actions: ordered step cards (kind, action, parameters) with Add step, reorder, duplicate, delete; parameters render typed inputs per action schema with `{{ }}` autocomplete from the trigger and upstream steps; the approval step shows its approver permission and expiry. Validation is inline plus a summary at the top ("3 problems found" jumps to the first). Tabs below: **Runs** (Started, Status, Duration, Trigger, Steps succeeded/failed, Error summary → row opens the run), **Versions** (definition diffs with Restore), **Audit** (who changed what, when), **Settings** (run-as, rate limit, concurrency, error policy, hook rotation, delete).
- **`/automations/[id]/runs/[run_id]`** — run header (rule, trigger, started, duration, totals, Run-as), a vertical step trace showing kind, action, resolved inputs, output, attempts/max attempts, timings and error, with Retry and Resume-from-here on each step, and a sidebar holding the raw event payload (collapsible JSON). Errors read human-first, code second.
- **`/automations/templates`** — six starter rules, each a real editable definition: Welcome email on user created, Comment on page published, Ping a webhook on publish, Weekly digest (schedule), Publish a page after approval, Notify the owner when a run fails.
- **Keyboard** — `N` new rule, `/` focus search, `E` enable/pause the focused row, `D` duplicate, `R` run now, `T` test event, `Del` delete (single confirm), `↑/↓` move, `Enter` open, `Esc` close dialogs. Every destructive action is reachable without a mouse.
- **Mobile (<1024px)** — the list becomes cards (name, event chip, status, last fired); the editor is read-only behind "Edit on a larger screen" while Runs, Approvals and the run trace stay fully usable; the trace is an accordion.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/automations` | List rules (filters: event, status, site) | `workflows.read` |
| POST | `/api/v1/automations` | Create a rule | `workflows.manage` |
| GET/PATCH/DELETE | `/api/v1/automations/{id}` | Read / change / delete a rule | `workflows.read` / `workflows.manage` |
| POST | `/api/v1/automations/{id}/duplicate` | Copy as paused | `workflows.manage` |
| POST | `/api/v1/automations/{id}/run` | Run once now (real world) | `workflows.run` |
| POST | `/api/v1/automations/{id}/test` | Dry run against a payload | `workflows.run` |
| POST | `/api/v1/automations/{id}/listen` | Arm a one-shot real-event listener | `workflows.run` |
| POST | `/api/v1/automations/{id}/rotate-hook` | Rotate the inbound hook token | `workflows.manage` |
| GET | `/api/v1/automations/{id}/executions` | Run history | `workflows.read` |
| POST | `/api/v1/workflow-executions/{id}/cancel` | Cancel a running run | `workflows.run` |
| POST | `/api/v1/workflow-executions/{id}/retry-step` | Retry a failed step (`{ "step_no": n }`) | `workflows.run` |
| POST | `/api/v1/workflow-executions/{id}/resume-from` | Resume from a step, inputs kept | `workflows.run` |
| GET | `/api/v1/approvals?status=pending` | Pending automation approvals | `workflows.approve` |
| POST | `/api/v1/approvals/{id}/decide` | Approve/reject (token in the body, single use) | `workflows.approve` |
| GET | `/api/v1/automations/catalogue` | Closed vocabulary: events, operators, actions, binding syntax | `workflows.read` |
| GET | `/api/v1/automations/templates` | Starter rules | `workflows.read` |
| POST | `/api/v1/hooks/{token}` | Inbound trigger (no session; the token is the credential, rate-limited) | none (token) |

New key in the catalogue (`crates/permissions`): `workflows.approve` — deciding an approval is
deliberately separate from running a rule. Everything else keeps `workflows.read`,
`workflows.manage`, `workflows.run`.

### Data model

Migration `database/migrations/0013_automation_depth.sql` (next free number if taken). Adds to
`workflows`: `run_as_user_id uuid → users set null`, `concurrency text default 'queue'`
`('queue','skip')`, `on_error text default 'stop'` `('stop','continue')`, `rate_limit_per_hour int
default 60` (1–10000), `hook_token_hash text` (unique where not null; the token is shown once),
`hook_secret text` (signs outbound `http_request`, never returned by the API), `version int default
1`, `last_error text`. Adds to `workflow_steps`: `input jsonb` (resolved parameters at run time),
`on_error text default 'inherit'` `('inherit','stop','continue','route')`, `timeout_ms int default
30000` (`>0 and <=120000`), and widens `kind` to `('task','wait','branch','stop','approval')`.

| Table | Columns (types) | Indexes / rules |
|---|---|---|
| `workflow_approvals` | id uuid pk, execution_id uuid → workflow_executions cascade, step_no int, organization_id uuid, requested_at, expires_at, decision text ('approved','rejected'), decided_by uuid → users set null, decided_at, note text, decision_token_hash text | unique `(decision_token_hash)`; `(organization_id, requested_at)` where decision is null; check `(decision is null) = (decided_at is null)` |
| `workflow_rate_windows` | workflow_id uuid pk → workflows cascade, window_start timestamptz, run_count int default 0 | check `run_count >= 0` |
| `automation_test_events` | id uuid pk, organization_id uuid, workflow_id uuid cascade, payload jsonb, created_by uuid → users set null, created_at | `(workflow_id, created_at desc)`; rows older than 24h are pruned by the matcher tick |
| `automation_settings` | id smallint pk default 1, http_allowed_hosts text[] default '{}', approval_ttl_hours int default 72, updated_at | check `(id = 1)` |

Condition groups live in `workflows.conditions` as a typed JSON tree: the v0 array of comparisons
becomes `{"all":[…]}` and stays backwards compatible because the matcher reads a bare array as
`{"all":[…]}`. REQ-004 later adds `workflows.graph` plus `workflow_steps.node_id`/`branch` — this
migration deliberately does not, so the two specs never claim the same column.

### Events

| Event | Kind | Notes |
|---|---|---|
| `page.published`, `user.created`, `user.updated`, `media.created`, `media.deleted` | consumed | trigger library; each gains a documented payload schema for the condition and binding pickers |
| `workflow.execution.completed`, `ai.run.completed` | consumed | chaining across engines (REQ-001, REQ-003) |
| `automation.rule.matched` | emitted | exists — rule, event, run |
| `automation.rule.permission_revoked` | emitted | the run-as account lost a permission an action needs |
| `automation.rule.limit_reached` | emitted | rate limit or concurrency rule refused a run |
| `workflow.step.retrying`, `workflow.step.failed` | emitted | observability; subscribable |
| `workflow.approval.requested` / `.decided` | emitted | drives notifications (REQ-021) and the approvals inbox (REQ-059) |
| `workflow.execution.started` / `.completed` / `.failed` / `.cancelled` | emitted | exists |

Inbound hook calls are recorded as `automation.hook.received` with the source address redacted in
the payload; the rule id is the only identifier returned to the caller.

### Acceptance criteria

- [ ] The rule list, editor, run history, run detail and templates screens exist at the routes above and appear in the QA walkthrough inventory.
- [ ] A rule on `user.created` sends a welcome e-mail to a new account in the QA stack (the mail sink proves exactly one message, correct recipient and subject).
- [ ] Condition groups work: an `all` inside `any` evaluates correctly against fixture payloads and round-trips through save and reload unchanged.
- [ ] An inbound hook call starts a run whose payload the conditions read; a wrong or rotated token answers 404 and never reveals whether a rule exists.
- [ ] `http_request` to a host outside `automation_settings.http_allowed_hosts` is refused at save time naming the host, and delivered with `x-omnion-signature` when allowed.
- [ ] A dry run reports `would_send` per host action without sending e-mail or calling a URL.
- [ ] "Run now" starts exactly one run; a second press inside the rate window shows the limit message and starts nothing.
- [ ] A `publish_page` step is refused with `automation.rule.permission_revoked` when the run-as account no longer holds `content.pages.publish`.
- [ ] A `wait_for_approval` step parks the run as `awaiting_approval`, the pending panel lists it, approving resumes it, rejecting ends it without the effect.
- [ ] Deciding an approval twice has no second effect (single-use token) and an expired approval is refused with a clear message.
- [ ] Retry re-runs only the failed step; resume-from re-runs that step and everything after it; neither duplicates an already-sent e-mail (mail sink count asserted).
- [ ] `timeout_ms` is honoured: a slow `http_request` fails naming the limit, and the step shows attempts used against attempts allowed.
- [ ] The endless-loop guard aborts a rule that repeats the same step with identical resolved parameters and explains why in the trace.
- [ ] A paused rule does not fire, and re-arming it does not replay events recorded while it was paused.
- [ ] Every definition change is audited with actor, diff summary and timestamp, and the Audit tab lists those entries.
- [ ] All six templates load, validate and save without edits beyond their missing credentials.
- [ ] Empty, loading and error states exist on every screen; no dead buttons and no "coming soon" text.
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and the QA walkthrough pass with zero high findings.

### QA plan

The walkthrough must: open `/automations`, filter by event, toggle a rule with `E`, duplicate with
`D`, open the duplicate, add a condition group, add an `http_request` step (a disallowed host
first to see the error, then an allowed one), preview a template from Templates; press "Send test
event" with a hand-written payload and read the dry-run report; press Run now, then open the
produced run from the Runs tab; click Retry and Resume on a deliberately failing run; drive a
`wait_for_approval` rule through approve and reject; rotate the hook token and confirm the old URL
404s; then the mobile pass over the list, the approvals panel and the run trace.

The visual check must see: trace columns aligned (kind, action, duration, attempts), long JSON
collapsed rather than overflowing a card, error text wrapping inside its step, the approvals panel
visually distinct from the table, and no clipped copy in the editor's sticky footer at 1024px.

### Slices

1. **Trigger and condition depth** — event library with payload schemas, condition groups, inbound hook trigger with token and rotation, catalogue update, trigger/condition editor, test event (dry run) and listen.
   *Done when:* a rule on `user.created` with a nested condition fires from a real signup and a test event produces the same evaluation with no side effects.
2. **Action library and error paths** — `http_request` (allow-list + signature), `publish_page`, `run_workflow`, `branch`/`stop` kinds, per-step `on_error`, `timeout_ms`, retry/resume endpoints and the run-detail controls.
   *Done when:* a failing step routes to a failure branch, retry succeeds without duplicating the earlier e-mail, and a disallowed host never leaves the process.
3. **Approvals and run-as authority** — `wait_for_approval`, `workflow_approvals`, `workflows.approve`, the pending panel, decision endpoints with single-use tokens, `run_as_user_id` checks, `automation.rule.permission_revoked`.
   *Done when:* a publish step is blocked by a revoked permission, and a gated publish completes only after an approval.
4. **Operations polish** — rate limits and concurrency, endless-loop guard, templates gallery, versions/restore, audit tab, mobile and empty states, event emissions.
   *Done when:* the six templates run green on the QA database and the limit and loop guards each have a test that fails when the guard is removed.

### Risks / notes

- Every host action must be idempotent under retry: outbound calls carry the run id as an
  idempotency key and each action's spec states whether a repeat is safe.
- Inbound hooks are a public surface: rate-limit them, store only the token hash, answer 404 on
  anything unknown, and never echo the rule name back.
- Approval links are credentials: single-use, hashed at rest, expiring, and delivered to a panel
  page that posts the token in the body rather than a GET that mutates state.
- Rate limiting and concurrency must be enforced in the same transaction that starts a run, or two
  API instances both pass the check.
- The permission-revoked path is a feature test, not an edge case — write it first so a rule can
  never run with stale authority.
- Keep the vocabulary closed: a new action means its schema, its permission mapping and a matcher
  test in the same commit.
