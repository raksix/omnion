# REQ-046 — AI Workflow Builder *(headline)*

> **Status:** in-progress (slice 4 · `f3943740` (the decision bar: approve materialises a **disabled** workflow with `enabled: false` written at the call site, approval is refused when the *approver* lacks a permission a step needs, `revise` streams like `generate`, and `test-run` provably dispatches nothing — plus the two refusals that were correct and useless: `reject` told the operator the status instead of naming the builder, and the "already a rule" message named no path), `a600e272` (the end-to-end probe), `61f227c6` (three criteria closed), `c63a6a1b` (**the approval bar was dead on every draft** — `has_definition` was declared in the TypeScript type and never sent by the API, so `approvable` was permanently `false` and every decision button was disabled; only the probe clicking the screen could find it, and the same commit fixed the psql `returning`-id bug that had the walkthrough reporting a visit it never made) · slice 3 · `65af45f5`, `b97661e0`, `54fb8c91`, `a7a70815`, `375da98c`; slice 2 · `06f28399`; slice 1 · `b6392a80`)
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

> "If an invoice is 7 days overdue, email the customer; if 14 days, create a task for the
> sales owner."

→ produces the workflow directly.

## Notes

- Prior art: n8n's AI workflow-builder + MCP workflow tools (docs/09-N8N-TEARDOWN.md §10);
  generated workflows land in the visual builder (REQ-004) as an editable draft with an
  approval step before activation.

## Implementation spec

### Scope (in / out)

**In**

- Natural language → workflow definition for the P09 engine (`crates/workflows`): the model is
  asked for the *same* shape the workflow API already accepts (`trigger_kind` + ordered `steps`),
  so a generated workflow is an ordinary workflow — no parallel execution path.
- A draft store with the lifecycle `generating → draft → approved → activated` (plus `rejected`,
  `failed`), a prompted revision loop, and the human approval gate the request asks for.
- One new **action** in the closed action registry: `ai.prompt` (prompt template, optional model
  key, output budget), executed as a normal `kind = 'task'` step — the node vocabulary and the
  `workflow_steps.kind` constraint do not change.
- Server-side validation before anything is stored, plus exactly one automatic repair round-trip
  when an answer fails; the console covers generate, list, review, edit, revise, approve, test-run,
  activate and delete.

**Out**

- The node canvas itself (REQ-004) — a draft opens there once it exists; until then the definition
  is edited as validated JSON.
- Model routing, streaming framing and the provider registry (P11, docs/06 §2–§4); agent tool
  calling, RAG and multi-agent orchestration (REQ-001); the app builder (REQ-045).
- Any capability the approver does not hold: a generated step needing a permission the approver
  lacks is refused at approval time, never silently granted.

### Screens (UI)

Admin panel, inside `RequireAuth` + `AppShell`:

| Route | Purpose |
|---|---|
| `/ai/workflows` | Draft console: generate form + draft list |
| `/ai/workflows/[id]` | Draft review: rationale, definition, approval bar, test run |

- **List table** columns: Status badge (`draft`, `approved`, `activated`, `rejected`, `failed`),
  Title, Model, Created by, Updated, row actions (Open, Duplicate prompt, Delete); badge colours
  come from the shared status tokens.
- **Filters**: status (multi-select), free-text search over title + prompt, created-by select,
  updated date range; all persisted in the URL (`?status=&q=&by=&page=`) so a reload and the QA
  walkthrough land on the same view. **Bulk actions**: delete selected own drafts in
  `draft`/`rejected` only — no bulk approve, no bulk activation.
- **Generate form**: Prompt (textarea, 10–2000 chars, required, live counter), Model (select over
  enabled registry models, default preselected), Trigger hint (manual / schedule — the hint
  narrows the prompt, the model proposes the schedule string), Site scope (optional). Client
  validates length; the server owns everything else and its message is shown verbatim. While
  generating, a progress panel (plan → validate → repair?) shows a Cancel that keeps nothing.
- **Review screen**: rationale (markdown), definition editor (JSON textarea, schema-checked on
  blur with inline messages), read-only step list, approval bar: `Approve`, `Reject` (reason
  required), `Ask for changes` (revision prompt), `Test run`, `Open in builder` (disabled with a
  tooltip until REQ-004 exists).
- **Empty state**: “Describe the workflow you want” with three click-to-fill examples;
  **loading**: skeleton rows; **error**: human-readable cause + Retry, never a raw provider body;
  **no provider connected**: the form is replaced by a link to `/ai`.
- **Keyboard**: `n` focus prompt, `Cmd/Ctrl+Enter` generate/approve, `j`/`k` move the list, `Esc`
  cancels generation or closes the revision dialog. **Mobile** (< 768 px): list becomes cards, the
  editor is read-only with an “edit on desktop” hint, the approval bar sticks to the bottom.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/ai/workflows/drafts` | List drafts (`status`, `q`, `by`, `limit`, `offset`) | `workflows.read` |
| GET | `/api/v1/ai/workflows/drafts/{id}` | One draft: prompt, rationale, definition, decision trail | `workflows.read` |
| POST | `/api/v1/ai/workflows/generate` | Prompt → new draft, streamed as `text/event-stream` | `ai.chat` |
| POST | `/api/v1/ai/workflows/drafts/{id}/revise` | Prompted revision of an existing draft | `ai.chat` |
| PATCH | `/api/v1/ai/workflows/drafts/{id}` | Save an operator-edited definition (revalidated) | `workflows.manage` |
| POST | `/api/v1/ai/workflows/drafts/{id}/approve` | Approve → materialise a **disabled** workflow | `workflows.manage` |
| POST | `/api/v1/ai/workflows/drafts/{id}/reject` | Reject with a required reason | `workflows.manage` |
| POST | `/api/v1/ai/workflows/drafts/{id}/test-run` | Dry-run the definition, no external side effects | `workflows.run` |
| DELETE | `/api/v1/ai/workflows/drafts/{id}` | Delete a draft (never the workflow it produced) | `workflows.manage` |
| GET | `/api/v1/ai/workflows/examples` | Worked examples + the action vocabulary | `workflows.read` |

Every route is scoped like the workflow surface (the caller's organization, narrowed by the
selected site). `generate` and `revise` are the only routes that spend AI, both behind `ai.chat`;
approve is idempotent and a second call answers `409` naming the workflow it already created.

### Data model

One table, one migration: `database/migrations/00NN_ai_workflow_builder.sql` (number = next free at
land time; released migrations are append-only, docs/05-VERSIONING.md §8).

`ai_workflow_drafts`: `id uuid pk default gen_random_uuid()`; `organization_id uuid not null
references organizations (id) on delete cascade`; `site_id uuid references sites (id) on delete set
null`; `title text not null` (check 1–120); `prompt text not null` (check 1–4000); `rationale text`
(model's explanation, markdown); `definition jsonb` (`null` until a validated answer,
`jsonb_typeof = 'object'`); `status text not null default 'generating'` check in
(`generating`,`draft`,`approved`,`activated`,`rejected`,`failed`); `workflow_id uuid references
workflows (id) on delete set null` (set at approval); `model_key text` (frozen at generation);
`tokens_input integer`, `tokens_output integer` (mirrored to the AI ledger, REQ-047); `error text`;
`created_by uuid references users (id) on delete set null`; `decided_by uuid`; `created_at`,
`updated_at`, `decided_at timestamptz`.

Indexes: `ai_workflow_drafts_org_idx (organization_id, created_at desc)`;
`ai_workflow_drafts_open_idx (status) where status in ('generating','draft','approved')`;
`ai_workflow_drafts_workflow_key unique (workflow_id) where workflow_id is not null`.

No run-side tables: `workflows.steps` is already `jsonb`, so approval is a plain insert, and
`ai.prompt` is a registry action in `crates/workflows` (installed by the API's workflow runner),
not schema.

### Events

| Event | When | Payload |
|---|---|---|
| `ai.workflow_draft.created` | generation validated | `draft_id`, `title`, `model_key` |
| `ai.workflow_draft.failed` | generation + repair ran out | `draft_id`, `reason` |
| `ai.workflow_draft.approved` | workflow materialised (disabled) | `draft_id`, `workflow_id` |
| `ai.workflow_draft.rejected` | operator rejected | `draft_id`, `reason` |
| `ai.workflow_draft.activated` | the workflow was enabled | `draft_id`, `workflow_id` |

All organization-scoped, so they fan out to the organization's subscribed webhook endpoints (P12)
— a team can wire “draft ready for review” into chat without polling. Payloads carry identifiers
only: never the prompt body, never the definition, never a provider key.

### Acceptance criteria

- [x] The request's own example prompt produces a validated draft whose definition the workflow API
      accepts unchanged — *proved by the round trip through `POST /workflows` (the create path; the
      definition is handed over with no adaptation) plus per-field comparison of the author's own
      step fields, and the API's filled defaults asserted separately.*
- [x] An unvalidatable answer triggers exactly one repair round-trip; a second failure lands the
      draft in `failed` with a readable `error`.
- [x] Approval materialises a **disabled** workflow with the draft's steps (enabling it is a
      separate action); a second approve answers `409` naming the workflow id. — *`enabled:
      false` is written at the call site, not inherited, because `POST /workflows` arms a new
      rule by default; asserted by reading the stored rule back (`approval_materialises_a_
      disabled_workflow_with_the_drafts_steps`) and by `a_second_approve_answers_409_naming_
      the_workflow_it_already_created`.*
- [x] An edited definition is revalidated server-side; an invalid save changes nothing and returns
      a field-level message. — *the save runs the module's own `definition::validate` — the same
      one the generation path runs, so the secret rule covers an operator's edit too — before any
      write, and the store's update is conditional on `draft`; `an_edited_definition_is_
      revalidated_and_an_invalid_save_changes_nothing`.*
- [ ] `ai.prompt` runs as an ordinary task step: a run passes a template through the model, later
      steps read the output, and a provider failure is retried by the existing step backoff.
      *(Partly proven: the action is a registry host action with a bounded budget, it is wired
      to the AI Hub router, and its output shape (`text`) is what a later step reads. The run
      itself needs a connected provider — slice 4's probe.)*
- [x] Test-run performs no external side effects — asserted by “no events emitted during the test
      run”. — *true by construction rather than by assertion: a run row needs a `workflow_id`
      (`not null`), and a draft under review has none, so there is nothing to dispatch. The
      handler projects the definition step by step and the organization event feed is asserted
      to be unchanged; the screen says so in words instead of implying a rule was exercised
      (`a_test_run_reports_a_plan_and_emits_no_event_and_writes_no_run`).*
- [x] Permission keys hold — *generate without `ai.chat` is `403` and never reaches the provider
      (the call counter proves zero calls), a reader holding only `workflows.read` reads the list and
      the detail, another organization sees an empty list and `404` on a direct fetch and on a
      delete, and the row survives. `approve` is slice 4.*
- [x] Empty, loading, error and no-provider states all exist; a failed generation never leaves the
      UI stuck in `generating`. — *the console's own states are asserted by the walkthrough
      (`runAiWorkflowConsole` reaches either the no-provider panel or the form, and refuses the
      form in the first), and the focus probe drives the review screen's decision states
      (`probe-ai-workflow-builder.cjs`, 14/14 against the live stack).*
- [x] Every list filter is URL-persisted and survives a reload. — *typed live against the w3
      stack: the query reaches the URL (`?q=`) and the input still holds it after a reload,
      which is the assertion that matters — a screen can write the query string and still
      re-seed its own state. Now in `probe-ai-workflow-builder.cjs` so it cannot rot.*
- [x] A generated definition never carries a secret: params are limited to the closed vocabulary,
      asserted by a validation test, and a deleted draft never disables its workflow.
- [x] Both screens use Lucide icons only, render in light and dark mode with visible focus rings,
      and show no clipped text at 1280 px or 390 px. — *the only non-ASCII glyphs in either file
      are `⌘`, `↵` and `≥`: a key label, a key label and a length bound, not an icon. Measured
      live on the w3 stack in both themes at 1280 and 390: zero clipped controls, and
      `:focus-visible` matches with a 2px solid outline on the console's own controls. The
      first measurement said "no focus ring" at 390 — it had focused the header's **Sign out**
      button, because `document.querySelector('button')` is the first button on the page and
      not one of the screen's. A probe that measures the wrong element reports a defect that
      is not there, which is the same failure the three assertions in this REQ started with.*
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and `bash scripts/qa/run.sh` are
      green and both screens appear in the walkthrough inventory (no untested screen).

### QA plan

- The walkthrough (`scripts/qa/run.sh`: browser pass → visual review) gains a `/ai/workflows`
  segment: open the console, generate a draft from the example prompt, open the draft, edit one
  field, ask for a change, approve, follow the link to `/workflows`, activate, return. Every button
  and link on both screens is pressed and recorded in `clicks.jsonl`; a failed click fails the pass.
- Empty state, no-provider state and failed-generation state are each forced and screenshotted; the
  visual review must see legible status badges, an unclipped editor and a visible approval bar.
- A probe (`scripts/qa/probe-ai-workflow-builder.cjs`) asserts what a screenshot cannot: generate →
  approve → activate via the API, then the workflow is enabled and the draft `activated`; it exits
  non-zero on mismatch.
- Mobile viewport pass: cards instead of the table, the desktop hint in the editor.

### Slices

1. **Draft store + generation** — migration, draft service, `generate` + list/get/delete routes,
   validation + one repair attempt. *Done when:* `cargo test --workspace` is green and a prompt
   against a connected provider stores a `draft` row with a validated definition.
2. **`ai.prompt` in the engine** — action registry entry plus runner wiring, tests, documented
   params. *Done when:* a manual run of a workflow containing an `ai.prompt` step completes and its
   output is visible in the execution's step rows.
3. **Console UI** — list + generate + review screens, filters, streaming panel, empty/error states,
   keyboard shortcuts. *Done when:* the walkthrough exercises both screens with zero high findings
   and the probe passes.
4. **Approval, activation, revise, test-run** — the remaining routes, workflow materialisation,
   events, docs. *Done when:* the end-to-end probe passes and each emitted event appears on the
   event feed.

### Risks / notes

- Model output drift is the core risk: the closed action registry plus strict validation plus a
  single repair round-trip is what keeps a bad answer from becoming a bad workflow — never widen
  the registry to accept “anything the model wrote”.
- Approval that creates a disabled workflow is deliberate: the definition is reviewed by a human
  *and* by a test-run before it can fire on a schedule.
- Cost: generation and revision both spend tokens; cap repair at one round-trip and surface token
  counts in the console so operators see the spend.
- `model_key` may be removed from the registry later; activation then refuses with a hint to pick a
  live model rather than failing at run time.
