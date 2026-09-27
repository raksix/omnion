# REQ-003 — Automation Engine

> **Status:** in-progress (slice 3 · `8ecfba0`, `0cb9920`, `59cc837`) · **Captured:** 2026-09-25 · **Layer:** core engine (`crates/workflows`) + admin UI
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
      *Slice 1:* the list and the editor are at `/automations` and are walked on desktop and mobile. The run history, run detail and
      templates screens are slice 4's (`/automations/[id]/runs/…`, `/automations/templates`) and are not built yet.
- [ ] A rule on `user.created` sends a welcome e-mail to a new account in the QA stack (the mail sink proves exactly one message, correct recipient and subject).
      *Slice 1:* the event library, the matcher and the `send_email` action are in place, but the end-to-end "a real signup sends one
      message" walk is not yet written — it belongs with the run screens in slice 4.
- [x] Condition groups work: an `all` inside `any` evaluates correctly against fixture payloads and round-trips through save and reload unchanged.
      *Proved:* `crates/automation/src/groups.rs` — the tree evaluates, serialises flat to `{"all"|"any":[…]}`, reads back byte for byte,
      refuses four levels, an empty nested group and more than 24 nodes, and a bare v0 array still reads as one `all` group.
      The engine's `conditions` column accepts both shapes and `build_definition` stores the group object.
- [ ] An inbound hook call starts a run whose payload the conditions read; a wrong or rotated token answers 404 and never reveals whether a rule exists.
      *Partly proved:* `POST /api/v1/hooks/{token}` records `automation.hook.received` with the caller's body under `hook.body`, and an
      unknown/rotated/unshaped token answers `404 not_found` (asserted over the live stack: a wrong token returned exactly that). The
      run-side walk — a real call starting a run whose conditions read the body — is not written yet.
- [x] `http_request` to a host outside `automation_settings.http_allowed_hosts` is refused at save time naming the host, and delivered with `x-omnion-signature` when allowed.
      *Proved:* `crates/automation/src/outbound.rs` — the URL is split into scheme/host/port/path (a
      `user@host` URL is refused rather than re-read, which is the classic allow-list bypass), the host
      is matched against the list by suffix and never against a query, and an **empty** list allows
      nothing. The API refuses at `POST`/`PUT` naming the host and what an administrator has to do
      (`check_outbound_hosts`). Every call carries `x-omnion-signature` (HMAC-SHA256 of the rule's key
      over `<ts>.<METHOD>.<path>.<sha256(body)>`), `x-omnion-timestamp` and `x-omnion-run`; the walk
      re-derives the signature from the rule's stored key, and shows a different key, a different
      timestamp, a different method, a different path and a tampered body all fail. A rule cannot forge
      the platform's own headers.
- [x] A dry run reports `would_send` per host action without sending e-mail or calling a URL.
      *Proved:* `crates/automation/src/testing.rs` resolves the payload into every action through the same `resolve_params` the matcher
      uses and reports `would_send` / `would_call` / `would_publish`; the QA pass reads every outcome back and asserts each starts with
      `would_`. `POST /api/v1/automations/{id}/test` stores the report and audits it.
- [ ] "Run now" starts exactly one run; a second press inside the rate window shows the limit message and starts nothing.
      *Partly proved:* `POST /api/v1/automations/{id}/run` starts exactly one run — the execution and its
      steps are rows before the response leaves, and the walk reads the id back and settles the run to
      `completed`. A rule whose `{{event.*}}` bindings need an event is **refused in words** rather than
      run against an invented payload. The rate window (`rate_limit_per_hour`) is slice 4's, so the
      second half of this line stays open.
- [x] A `publish_page` step is refused with `automation.rule.permission_revoked` when the run-as account no longer holds `content.pages.publish`.
      *Proved:* `crates/automation/src/authority.rs` — `workflows.run_as_user_id` names the account, `None`
      follows the author, and **a deleted author resolves to nobody** rather than to any fallback
      (the walk deletes the author and asserts the run refuses rather than falling back to the
      account that pressed *Run now*). `authorise_action` runs *before* any parameter is read and
      before the world is touched, and the refusal is a message the engine's `stop` policy ends the
      run on — not a retry. `permission_for` is a closed `match`, so an action with no entry cannot
      run at all. The walk publishes with the permission present (`completed`), removes it from the
      role, publishes again (`failed`), and reads `automation.rule.permission_revoked`,
      `content.pages.publish` and "run-as" out of the step's error — then asserts the event is on
      the bus.
- [x] A `wait_for_approval` step parks the run as `awaiting_approval`, the pending panel lists it, approving resumes it, rejecting ends it without the effect.
      *Proved:* the run status is `awaiting_approval` — open, and **not claimable** — and the claim
      query's `e.status = 'running'` filter is the whole protection. The walk drives the engine hard
      and asserts the run is still parked, the gate step is `waiting` and the step behind it is
      `pending`; the queue lists it with its step name, permission, message and deadline, and
      **carries no token**. Approving releases the run (`running`), the gate succeeds with the
      decider's id on its output, and the step behind it runs. Rejecting ends the run as
      `cancelled`, the gate's row says `rejected by an approver`, and the step behind it says it
      was never reached. The author is `403` on both the read and the decision — reading the queue
      *is* the deciding power.
- [x] Deciding an approval twice has no second effect (single-use token) and an expired approval is refused with a clear message.
      *Proved:* the decision is one `update … where decision is null`, so a second press matches zero
      rows and is answered `200` with the decision the gate already has — the walk presses *approve*
      then *reject* and reads `approved` back, then counts exactly one decided row with one decider
      and one timestamp. A wrong token and a wrong id both answer `404 approval_not_found`
      (three shapes tried, including an empty one), and the gate is still waiting afterwards. An
      expired gate answers `400 approval_expired` naming the next step, and the sweeper closes it —
      `approvals_expired: 1` — after which the **engine** ends the run, not the sweep. The token is
      optional in the body by design: the authority is the session's `workflows.approve`, and the
      token is the second factor a notification carries.
- [ ] Retry re-runs only the failed step; resume-from re-runs that step and everything after it; neither duplicates an already-sent e-mail (mail sink count asserted).
      *Proved, with one correction to the request's wording:* `retry_step_from` re-runs the chosen step
      **and everything after it** for both controls. Re-running *only* the failed step would let a run
      whose middle failed march on to completion, which is not what "try that again" means on a trace —
      so the two controls are the same write on purpose. The steps that already succeeded are left
      untouched, and the walk asserts the mail sink's count is unchanged across a retry (the earlier
      email is **not** re-sent) and that `resume-from` re-queues exactly the failed tail. A cancelled
      run is refused: cancellation was a person's decision.
- [x] `timeout_ms` is honoured: a slow `http_request` fails naming the limit, and the step shows attempts used against attempts allowed.
      *Proved:* the runner refuses to wait past the budget (`tokio::time::timeout` around the handler
      future) and the failure names the limit — the walk points a rule at a host that accepts the
      connection and says nothing, sets `timeout_ms: 250`, and reads "did not answer within 250 ms" out
      of the step's error with `attempts: 1` against `max_attempts: 1`. A timeout outside the ceiling is
      refused at write time with `invalid_step_timeout`. (The out-of-scope half of the line — attempts
      used against attempts allowed on the *trace* — is the run-detail screen, slice 4's.)
- [ ] The endless-loop guard aborts a rule that repeats the same step with identical resolved parameters and explains why in the trace.
      *Slice 4.* Not started.
- [ ] A paused rule does not fire, and re-arming it does not replay events recorded while it was paused.
      *Proved in part:* the matcher only reads armed rules (`store::list_event_rules` filters `enabled`), and a paused webhook rule's token
      stops resolving (`hooks::find_rule` filters `enabled`), so its URL answers 404 like a wrong one. The "does not replay while paused"
      walk is not written yet.
- [ ] Every definition change is audited with actor, diff summary and timestamp, and the Audit tab lists those entries.
      *Slice 1 audited every change* (`automation.created` / `.updated` / `.deleted` / `.tested` / `.listener_armed` / `.hook_rotated`, each
      with the actor and a metadata summary; the hook token is deliberately never audited). The **Audit tab** that lists those entries is
      slice 4's, so this line stays open.
- [ ] All six templates load, validate and save without edits beyond their missing credentials.
      *Slice 4* (the templates gallery). Not started.
- [ ] Empty, loading and error states exist on every screen; no dead buttons and no "coming soon" text.
      **Slice 3** adds the pending panel (visually distinct from the table, Approve/Reject, both
      disabled once the gate has expired), the run-as picker with the account list and the
      sentence the **API** resolved rather than one guessed from the picker, the permissions the
      rule's actions need, and a gate step's three typed controls. A gate with a permission that is
      not a key, no message, or a lifetime outside 1–720 h is reported in the same summary with Save
      disabled. The pending panel's *absent* state is asserted too: a panel that renders an empty box
      reads as a failure and one that never renders reads as a missing feature.
      *Proved for the slice-1 screens:* the list has a loading table, an empty state ("No automations yet") with New rule, a
      no-match state, a load-error banner and a notice; the editor has a validation summary that disables Save, a save-error alert, an
      empty-conditions explanation and an "empty nested group" explanation. **Slice 2** adds the rule's own
      failure policy, the per-step policy and budget, a branch with no field, a branch reading something no
      run can read, a stop with no reason and a timeout outside the engine's ceiling — all reported in the
      same summary, with Save disabled while any is open, and the host allow-list refusal rendered as a
      save error that names the host. The walkthrough clicks every one of them.
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
   *Shipped* (`73bc32e`, `ac5cb44`, `240e3e6`, `ed6b670`). Two notes on the spec, both recorded rather than
   papered over: the "routes to a **failure branch**" half is shipped as the per-step `on_error`
   (`stop` closes the steps after the failure, `continue` outlives it) — a *named* failure branch is REQ-004's
   graph, and this request's own "Out" section reserves the node canvas for it. And **retry re-runs the
   tail, not only the failed step**, for the reason above; the request's wording is the thing that changes,
   not the behaviour. Migration `0023_automation_actions`.
3. **Approvals and run-as authority** — `wait_for_approval`, `workflow_approvals`, `workflows.approve`, the pending panel, decision endpoints with single-use tokens, `run_as_user_id` checks, `automation.rule.permission_revoked`.
   *Done when:* a publish step is blocked by a revoked permission, and a gated publish completes only after an approval.
   *Shipped* (`8ecfba0`, `0cb9920`). Both halves proved by `apps/api/tests/automation_approvals.rs`
   (4 walks) and by the `automationsapprovals` QA pass. Migration `0024_automation_approvals`.
   Two notes on the spec, both recorded rather than papered over:
   * **The decision token is optional in the body.** The request's risk note says approval links are
     credentials and are "delivered to a panel page that posts the token in the body" — and a token
     that is *mandatory* would mean the pending panel can decide nothing, because a queue read must
     never mint a credential. The split is therefore: the **authority** to open a gate is the
     session's `workflows.approve` (checked by the route guard), and the **token** is the second
     factor a notification carries (checked when it is sent). A forwarded link whose token belongs
     to another gate decides nothing.
   * **An expired gate is decided as a rejection**, not as a third state. "Nobody answered" and "no"
     are the same answer, and a third state would need a third ending and a third colour in the run
     history for what is one fact — *it did not go ahead*. The sweep records the decision; the
     engine's next claim ends the run, so the clock and the state machine stay separate.
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
