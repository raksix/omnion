# REQ-045 — AI App Builder *(headline)*

> **Status:** in-progress (slice 4 · **THE BULK DELETE, AND THE BOX IT CLOSES** ·
> `POST /app-builder/plans/bulk-delete` (`8299d2f1`, `16fbf8d0`) plus the console's checkbox
> column, selection and confirmation (`088bb666`). The criterion "plan list, filters, **bulk
> delete of drafts** and JSON export work; applied plans are undeletable" had stood unticked for
> four ticks with the reason printed on the box: the console carried **no checkbox, no selection
> state and no bulk action**, so the clause described a control that was never drawn rather than
> one that behaved badly. It is drawn now, and the box is ticked.
> **The four claims are measured differently on purpose:** the list, the filters and "applied plans
> are undeletable" were already proven on the wire; the export was proven by reading the downloaded
> bytes; and the bulk is proven by the **arithmetic** of the answer plus the **rows read back out
> of the database**, because a bulk is the only one of the four whose truth is partial — a
> response carrying only a count renders a half-finished delete as a complete one.
> **What the rule bought:** the delete is driven by the **scoped read**, never by the request.
> The ids are the caller's and nothing inside a `delete` looks at an organization, so a handler
> that passed the array straight through would be deleting other tenants' plans and calling it a
> `200`. Two sentences come back for the two refusals and they are deliberately **different**:
> an applied plan is named and told why it stays; a plan that is absent and a plan belonging to
> another tenant get the **same** sentence, because a refusal that differed between the two would
> confirm the existence of every id a caller (or a script) guessed.
> **68 module unit tests (was 65) · 18 route walks (was 16) · 317 api lib tests ·
> `pnpm typecheck` 0 errors** · previous: slice 4 ·
> the queue said "cost attribution" and that turned out to be the one piece of this slice
> with **no price source anywhere in the tree** — `ai_models` (migration `0008`) has no price
> column, no crate holds a rate, and REQ-104's `ai_spend_daily` is wave-3b and belongs to w10.
> So `settle()` keeps reporting `0` and the export writes the **stored** figure rather than one
> derived from a rate this platform does not hold; a second pricing table invented here to fill
> one column is precisely the defect a "the Cost column is rendered" criterion cannot see ·
> **what landed instead is the other half of the same criterion: `GET /plans/{id}/export`** —
> `omnion.app-builder.plan/1`, served as an `attachment` with `no-store`, guarded by
> `appbuilder.read` (exporting is reading, or the least-privileged reviewer cannot hand the
> file on), built from the **same four reads the review screen makes** so the file and the
> screen cannot disagree · **the filename comes from the plan's short id and never its title**,
> because free text inside a `Content-Disposition` header is header injection and eight hex
> characters cannot be a slash or a quote · **superseded artifacts travel with the chain drawn
> in both directions** from the one stored edge, and `spec` / `validation` are exported verbatim
> — a re-normaliser is a second set of tolerances, and the one that disagrees with the validator
> is the one nobody reads · **the console's note reports what came back out of the file**, so a
> failed generation that exports a real file with an empty `artifacts` array says so instead of
> reading as a success · **three mutations red:** `inline` instead of `attachment` fails the
> header assertion, `find_plan` instead of `plan_in_scope` fails the cross-tenant walk, and the
> title-derived filename fails the crate test · **65 module unit tests (was 56) · 16 route walks
> (was 14) · 317 api lib tests · `pnpm typecheck` 0 errors** · previous: slice 4 ·
> **THE TYPED GENERATOR**, and the queue's next item was
> unbuildable today · the queue named the apply runner, so apply was the plan — until its first
> step was traced to its target: it writes the generated **entity**, and REQ-026's `entities` /
> `entity_fields` / `entity_records` tables exist in **no worktree at all** (checked all ten).
> Wave 2 owns the dynamic data model; writing those migrations here would collide with another
> writer's namespace over a table this wave does not own, and inventing one would have been the
> very defect this loop exists to prevent).
> What **was** mine and unreachable sat in a registered route: `POST /generate` answered
> `app_builder_generator_pending` — "the typed artifact generator is not wired yet" — after
> spending **zero** provider calls, which is a "coming soon" button wearing a status code.
> **Slice 4 lands `modules/app-builder/src/generate.rs`** (the schema prompt as a literal, and
> `normalize()` reading an untrusted answer into validated artifacts) and rewrites `POST /generate`
> to spend **one** call and stream `artifact` / `note` / `done` frames as each row lands.
> **The walk found a real defect in the repair logic:** keys were repaired but `parent_key` was
> not, so an entity spelled `Leave Request` became `leave_request` while its field still pointed
> at `Leave Request` — a field belonging to an artifact that was not in the plan. Fixed, with a
> unit test that also pins a *correctly* spelled parent as byte-identical.
> **Repairs are stated, but not equally forgivable:** `Int` → `integer` is spelled out in the
> rationale; `photo` is **never** downgraded to `text` (that would store something other than was
> asked) and a missing rationale is never invented.
> **56 module unit tests (was 41) · 14 route walks (was 10; the one that asserted the fake is
> gone) · `cargo build -p omnion-api` clean** · previous: slice 3 · `a98bb248` — **THE SCREENS**,
> and the one box that could not be ticked without them · `/app-builder` (composer with three
> click-to-fill chips, the plans table with status/text/mine filters) and
> `/app-builder/plans/{id}` (artifact tree by kind, detail pane, accept/reject/edit/regenerate,
> named blockers, footer counters, keyboard `j/k/a/r/e/g`), the client in `apps/admin/lib/api.ts`
> + `lib/types.ts`, both routes in `scripts/qa/walkthrough.cjs` and a depth pass that opens a
> **real** plan).
> **No Apply button, on purpose.** The runner waits on REQ-026's tables landing; a button
> answering "coming soon" is exactly what the Definition of Done forbids, so the footer names
> every blocker instead and the pass asserts the button is **absent**.
> **Blockers are the server's and are rendered verbatim** — a client that re-derived readiness
> would eventually disagree with apply, and the reviewer would be told a plan is ready that apply
> then refuses.)
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

> "Create an app to manage employees' leave requests."

AI:

```text
Entity
 ↓
Fields
 ↓
UI
 ↓
Permissions
 ↓
Workflow
 ↓
Notifications
 ↓
Reports
```

…and the app is actually created.

## Notes

- Consumes REQ-025 (App Builder) + REQ-026 (Dynamic Data Model); all generated artifacts are
  drafts pending approval (docs/06-AI-HUB.md §9–10) — nothing goes live without review.

## Implementation spec

### Scope (in / out)

**In**

- Prompt → plan: one natural-language request produces typed artifacts — entities, fields, UI (list +
  detail + form), permissions, roles, a workflow, notification templates and a report.
- Draft-only generation: each artifact lands in `draft` status with a rationale and a validation
  result; nothing reaches the live platform until the operator accepts it and runs **Apply**.
- Review workspace: artifact tree, per-artifact diff against the current schema, inline edit,
  regenerate-one-artifact with feedback, accept/reject per artifact, blocking summary when a required
  artifact is unresolved, and kept plan versions so a rejected attempt can be compared.
- Apply pipeline reusing REQ-026 (dynamic entities/fields), REQ-025 (generated screens),
  `crates/permissions` (new keys and a default role), `crates/workflows` (definition draft) and
  REQ-021 (notification templates), with progress streamed over SSE (REQ-041).
- Approval gate on apply (docs/06-AI-HUB.md §9): creating roles and permissions is privileged, so apply
  needs `appbuilder.apply` plus an approved approval request, and every write is audited with the plan id.

**Out**

- Generating or installing executable code, plugins or Rust modules.
- Auto-apply without review; no background job ever applies a plan.
- Data import/seeding into new entities, and non-additive changes to existing entities (refused with a reason).
- Cross-organization templates, app export/import and marketplace publishing (later slices).
- Reports beyond one grouped table and one chart per generated report.

### Screens (UI)

Admin routes:

- `/app-builder` — landing. Composer card with a large textarea, an example prompt and three sample
  chips ("Create an app to manage employees' leave requests", "Personel izin taleplerini yönetmek için
  bir uygulama oluştur", "Track supplier contracts with renewal reminders"), a model chip and
  **Generate plan** (disabled with a hint when no AI provider is configured). Below, the **Plans**
  table: Plan (title + short id), Status (draft / approved / applying / applied / rejected / failed),
  Entities, Components, Cost, Created by, Created at. Filters: status, text, "mine only". Bulk: delete
  drafts, export plan JSON, duplicate.
- `/app-builder/plans/{id}` — review workspace. Left: artifact tree grouped by kind (Entities, Fields,
  UI, Permissions, Roles, Workflow, Notifications, Reports) with status icons (pending / accepted /
  rejected / edited / invalid). Right: artifact detail — for entities a field table (key, label, type,
  required, unique, default, validations); for UI the screen list with column and form configuration;
  for permissions the new keys with descriptions; for the workflow a step chain (trigger → condition →
  approval → notification); plus rationale, validation messages and the schema diff. Per-artifact
  toolbar: **Accept**, **Reject**, **Edit**, **Regenerate** (with a feedback box). Footer: accepted /
  rejected / pending counts, **Approve plan** (enabled only when every required artifact is resolved),
  **Apply** (confirmation dialog listing what will be created) and **Discard**.
- Apply progress: inline step list (entities → fields → screens → permissions → roles → workflow →
  notifications → report → verify) with live status, streaming log, completion links to the created
  screens; on failure the failing step is named with **Retry** and **Roll back this application**,
  which removes only what this application created.
- States: no plans → composer focused with the sample chips; loading → artifact skeletons fill in as generation
  streams; provider or quota error → inline banner with the reason and **Retry generation** that keeps the prompt.
- Keyboard: `Cmd/Ctrl+Enter` generate or apply, `j`/`k` artifacts, `a` accept, `r` reject, `e` edit, `g` regenerate
  the focused artifact, `Cmd/Ctrl+Z` undo a reject in session, `Esc` close. Mobile: the artifact tree becomes a
  dropdown, the review pane takes full width, the footer bar sticks, and accept/reject move to a bottom action sheet.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| POST | `/api/v1/app-builder/generate` | Start generation from a prompt; returns the plan id | `appbuilder.generate` |
| GET | `/api/v1/app-builder/generate/{plan_id}/stream` | SSE stream of artifacts as they are produced | `appbuilder.generate` |
| GET | `/api/v1/app-builder/plans` | Plan list (paged, filterable) | `appbuilder.read` |
| GET | `/api/v1/app-builder/plans/{id}` | Plan with artifacts, rationale and validation | `appbuilder.read` |
| PATCH | `/api/v1/app-builder/plans/{id}/artifacts/{artifact_id}` | Edit a draft artifact (re-validates) | `appbuilder.review` |
| POST | `/api/v1/app-builder/plans/{id}/artifacts/{artifact_id}/accept` | Accept an artifact | `appbuilder.review` |
| POST | `/api/v1/app-builder/plans/{id}/artifacts/{artifact_id}/reject` | Reject an artifact with a reason | `appbuilder.review` |
| POST | `/api/v1/app-builder/plans/{id}/artifacts/{artifact_id}/regenerate` | Regenerate one artifact with feedback | `appbuilder.generate` |
| POST | `/api/v1/app-builder/plans/{id}/apply` | Apply accepted artifacts (approval-gated) | `appbuilder.apply` |
| GET | `/api/v1/app-builder/plans/{id}/apply/stream` | SSE progress of an application | `appbuilder.read` |
| POST | `/api/v1/app-builder/plans/{id}/applications/{aid}/rollback` | Undo what this application created | `appbuilder.apply` |
| POST | `/api/v1/app-builder/plans/{id}/reject` | Reject the whole plan | `appbuilder.review` |
| DELETE | `/api/v1/app-builder/plans/{id}` | Delete a never-applied plan | `appbuilder.review` |
| GET | `/api/v1/app-builder/examples` | Sample prompts for the composer | `appbuilder.read` |

Apply returns `409` for unresolved artifacts or a running application, and `403` without
`appbuilder.apply` or an approved approval request; edits that break a key return `422` with the field path.

### Data model

Migration `database/migrations/0015_ai_app_builder.sql` (next free number at build time).

- `app_builder_plans` — `id uuid pk`, `organization_id uuid null references organizations(id)`, `site_id uuid null
  references sites(id)`, `prompt`, `title text not null default ''`, `status text not null default 'generating' check
  (status in ('generating','draft','approved','applying','applied','rejected','failed'))`, `plan_version int not null
  default 1`, `model_label text not null default ''`, `tokens_in int`, `tokens_out int`, `cost_cents int not null
  default 0`, `created_by uuid references users(id) on delete set null`, `created_at`, `updated_at`, `applied_at`.
- `app_builder_artifacts` — `id uuid pk`, `plan_id uuid not null references app_builder_plans(id) on
  delete cascade`, `kind text not null check (kind in ('entity','field','ui','permission','role',
  'workflow','notification','report'))`, `key text not null`, `parent_key text`, `ordinal int not null
  default 0`, `status text not null default 'pending' check (status in ('pending','accepted','rejected',
  'edited','invalid'))`, `spec jsonb not null default '{}'::jsonb`, `rationale text not null default ''`,
  `validation jsonb not null default '[]'::jsonb`, `created_at`, `updated_at`, unique `(plan_id, kind, key)`.
- `app_builder_applications` — `id uuid pk`, `plan_id uuid not null references app_builder_plans(id) on delete
  cascade`, `status text not null default 'running' check (status in ('running','completed','failed','rolled_back'))`,
  `summary jsonb not null default '{}'::jsonb`, `created_entity_keys jsonb not null default '[]'::jsonb`,
  `applied_by uuid references users(id) on delete set null`, `started_at`, `finished_at`.
- `app_builder_application_steps` — `id bigint identity pk`, `application_id uuid not null references
  app_builder_applications(id) on delete cascade`, `ordinal int not null`, `kind text not null`,
  `label text not null`, `status text not null default 'queued' check (status in ('queued','running',
  'done','failed','skipped'))`, `detail jsonb not null default '{}'::jsonb`, `started_at`, `finished_at`.
- Indexes: `app_builder_plans_org_idx (organization_id, created_at desc)`, `app_builder_artifacts_plan_idx
  (plan_id, ordinal)`, unique `app_builder_application_steps_order_idx (application_id, ordinal)`.

### Events

- Emitted: `app_builder.plan.generated`, `app_builder.plan.applied`, `app_builder.application.failed`,
  `app_builder.plan.rejected`, `app_builder.rollback.completed`.
- Consumed: `ai.provider.disabled` and `ai.model.removed` fail in-flight generations with a clear message.
- Webhook relevance: `plan.applied` is useful to internal tooling; prompts and specs are never webhooked.

### Acceptance criteria

- [x] The sample prompt yields a plan with at least one entity, its fields, screens, permissions, a role,
      a workflow, a notification and a report. — **MEASURED (slice 4):**
      `a_prompt_becomes_a_plan_of_stored_artifacts_and_the_stream_names_each_one` drives the
      prompt from the request's own example, reads **nine** rows back out of the database (not
      out of the screen that drew them) and asserts each required kind is among them by name.
      It also pins the two properties that would make this box pass without the feature
      existing: the provider is called **exactly once** (counted by the provider's own counter,
      so a repair loop cannot hide), and the plan settles at `draft` — never `approved`, because
      a generator that could approve its own work would collapse the two-act design the whole
      request rests on. **The previous tick could not have ticked this box: the route spent zero
      calls and failed every plan on purpose.**
- [x] Artifacts stream into the tree during generation and the tree is usable before it ends. —
      **MEASURED (slice 4):** the same walk reads the **wire**, not the row count: it asserts
      one `event: artifact` per artifact, a terminal `event: done`, and **no** `event: error` on
      a complete answer. The frame carries the status the **validator** derived rather than the
      one the generator hoped for, so a tree filling in over the stream shows `invalid` where it
      should — the acceptance criterion is about the tree being *usable*, which a stream that
      announced only its own progress could never satisfy.
- [x] Every artifact shows a rationale and its validation result. — **MEASURED (slice 4):**
      `a_mis_spelled_key_is_repaired_onto_the_artifact_and_the_repair_is_readable` reads the
      rationale column back and asserts the repair is **in it** ("`Leave Request` was read as
      `leave_request`", "`Int` was read as `integer`") — a silent repair is a plan the reviewer
      approved under a name they never saw. The module's own tests pin the half that must NOT
      be repaired: a missing rationale is never invented and an unknown field type is never
      downgraded to `text`, both because inventing either would erase the difference between a
      model that explained itself and one that did not.
- [x] An invalid or reserved field key marks the artifact `invalid` and blocks apply by name. —
      **MEASURED (slice 1, `8c87a7cc` + `4f228080`):** `an_artifacts_status_is_derived_from_the
      _validators_answer_not_the_generators` reads a row at `invalid` with the findings stored
      beside it, and `a_reserved_key_is_refused_by_name_before_it_can_be_written` reads the
      refusal at the store boundary AND counts zero rows afterwards — a refused artifact leaves
      nothing behind. `blockers_name_what_stands_between_a_plan_and_apply` reads five `missing`
      kinds by name beside the two unresolved artifacts. **The walk caught two defects here that
      a unit test could not**: the tenant predicate made every list read fail with `42804
      argument of OR must be type boolean, not type uuid`, and the store refused `accepted`
      outright — which left the review screen with no way to reach an applicable plan. Both
      fixed and both proven from the other side (`accepting_an_artifact_is_possible_and_editing
      _is_not_a_status_write`).
      **Slice 2 corrects the two fixtures this box was measured through** (`65bdf683`): the first
      asked for a `users` artifact to be *stored as invalid* while the second requires it to leave
      no row at all — both cannot be true of one contract. The store's refusal is right (a
      reserved key must not reach a table), so the first now proves the same claim with a finding
      the store *accepts* — a missing rationale — which separates "the status comes from the
      validator" from "the key is refused". `blockers…` likewise asserted `2` blockers and `5`
      missing kinds of one list; the list is **7** and the walk now asserts both halves.
- [x] Rejecting a required artifact blocks **Apply** and lists what is missing. —
      **MEASURED (slice 2, `65bdf683`):** `a_rejection_without_a_reason_is_refused_and_the_row
      _stays_pending` drives the wire and reads the refusal on the row as well as the status:
      a missing body and a whitespace-only reason are both `422`, and the artifact is still
      `pending` afterwards — a refused rejection leaves nothing behind. The blockers list itself
      is asserted by name on the plan detail: `an_invalid_artifact_cannot_be_accepted_and_the
      _refusal_names_the_finding` reads the `invalid` blocker carrying its finding beside it.
      **`apply` itself is still slice 3**, so what is proven is the refusal and the named list,
      not the `409`.
- [x] Regenerating one artifact with feedback replaces it and keeps the previous version. —
      **MEASURED (slice 2, `65bdf683`):** `a_regeneration_keeps_the_previous_version_and_says_
      _which_kind_of_rejection_it_was` spends exactly **one** provider call (counted by the
      provider's own counter, not by what is left in the script), reads ten rows where there were
      nine, and reads the retired version's `rejected_reason` as `superseded by a regenerated
      version` — so a reviewer's refusal and a machine retirement are distinguishable in the
      tree. **The walk found the defect this criterion was about**: regeneration raised
      `duplicate key value violates unique constraint` because 0224's `(plan_id, kind, key)` was
      absolute and both rows exist at the end of the transaction whichever is written first.
      Migration `0227` makes the index partial over live versions.
- [ ] **Apply** is refused without `appbuilder.apply` and without an approved approval request.
- [ ] Apply creates the entity with the accepted fields and its screens appear in the panel.
- [ ] New permission keys appear in the IAM catalogue and the generated role binds them.
- [ ] The generated workflow exists, disabled until enabled, with its trigger and approval step visible.
- [ ] Notification templates from the plan exist and reference the new workflow.
- [ ] The report renders its grouped table and chart on real (empty) data.
- [ ] Apply progress streams step transitions and ends with links to the created screens.
- [ ] A failing step is named, retry is offered, and rollback removes only this application's output.
- [x] Plan list, filters, bulk delete of drafts and JSON export work; applied plans are undeletable. —
      **THE EXPORT HALF IS LANDED AND WIRE-PROVEN; THE BOX STAYS UNTICKED, because the criterion
      is four claims and it is a conjunction.** Bulk delete of drafts does not exist: the console
      carries no checkbox, no selection state and no bulk action, so "bulk delete" is a control
      that was never drawn. The list, the filters and "applied plans are undeletable" were already
      proven on the wire (tick 65's `the_list_filters_and_the_vocabulary_endpoint_answer_the_composer`,
      `an_applied_plan_is_not_deletable_and_the_two_refusals_are_different`). — **MEASURED tick 72
      (`c294b788`, `55308693`, `7469649d`): `GET /plans/{id}/export` answers `200` with an
      `attachment` disposition and `no-store`, and `a_plan_exports_as_an_attachment_carrying_the_plan_
      _the_screen_shows` reads `schema = omnion.app-builder.plan/1`, the entity's `spec` verbatim,
      the validator's own `validation` list, and then asserts the file's `artifacts` / `counts` /
      `blockers` against **the review endpoint's body** — because a file built from different reads
      than the screen is a second view of the same plan, and a reviewer comparing the two would be
      comparing an inconsistency this platform introduced.** The console's note reports what came back
      *out of the file* rather than what the click announced, and an empty `artifacts` array (a
      failed generation exports a real file) says so instead of reading as a success. **Proven to
      fail twice:** serving `inline` instead of `attachment` fails the header assertion, and replacing
      `plan_in_scope` with `find_plan` fails the cross-tenant walk. The third mutation — the filename
      built from the plan's free-text **title** instead of its short id — is caught by the crate test,
      and it is the one worth keeping: free text inside a `Content-Disposition` header is header
      injection, and the file is named `omnion-app-plan-01234567.json` precisely so it cannot be.
      **The box closes on tick 73 (`8299d2f1`, `16fbf8d0`, `088bb666`), because the fourth claim
      now exists and the other three were already proven.** `POST /app-builder/plans/bulk-delete`
      answers `200` carrying `requested`, `deleted` and a `failures` list — never `204` and never
      `409`, because a status code can only say whether anything went, and "two of five deleted,
      one was applied" is the truth of the call the console's confirmation already asked about.
      `a_bulk_delete_removes_the_drafts_and_names_what_it_would_not` drives it with two drafts,
      an applied plan, **another tenant's plan** and one that never existed, then asserts the
      arithmetic (asked 5, deleted 2, refused 3) and reads the rows **out of the database** — a
      response claiming two deletions is only worth something beside a table that agrees.
      `a_bulk_delete_is_refused_without_the_review_key` proves the route is guarded by the same
      `appbuilder.review` key as the single delete: a member holding `read` alone sees the list
      and the delete is refused with the same `403 permission_denied`.
      **The console control that did not exist before this slice is the checkbox column** — a
      per-row checkbox, a header checkbox with a real indeterminate half-state, a selection that
      survives a filter change, and a confirmation that **names the applied plans before the
      button is pressed**. An applied plan's checkbox is deliberately left enabled: a control
      that silently does nothing on the one row a reviewer most needs to know about is a
      control they learn to mistrust on every row.
      **Still owed: the browser pass.** `omnion-w4` holds the shared QA slot (pid 1857610,
      `cwd=/mnt/apopic/omnion-w4`, verified with `kill -0` **and** `/proc/<pid>/cwd` — never by
      the age of the placeholder file) and `/mnt/apopic` sits at 99% with 821 MB free, so the
      walkthrough section that now drives this control (`scripts/qa/walkthrough.cjs`) has not
      been executed end to end. The box is ticked on the wire proof, and the record says so.
- [ ] Apply requires explicit confirmation, the keyboard flow works, and mobile keeps actions reachable at 390 px.
- [x] Keys `appbuilder.read` / `appbuilder.generate` / `appbuilder.review` / `appbuilder.apply` exist in the catalogue. —
      **MEASURED (slice 2, `65bdf683`):** `the_app_builder_family_is_catalogued_and_apply_is_its
      _own_power` walks all four by name, asserts each one's category and that it explains
      itself, and asserts `apply` is neither `review` nor `generate` — because the runner creates
      roles and permissions, so one key would let a reviewer grant themselves the power the plan
      proposed. Three of the four already **refuse** a caller who lacks them:
      `an_account_without_an_app_builder_key_is_refused_the_whole_surface` reads `403
      permission_denied` from the list and the vocabulary, then grants `read` alone and reads the
      list open with the **decisions still refused**. `apply` guards no route yet — slice 3.
- [ ] `cargo test`, `pnpm typecheck && pnpm build` and the browser walkthrough are green.

### QA plan

- Walkthrough: run the leave-request prompt, watch artifacts stream in, inspect the entity field table, edit one
  label, reject one artifact and confirm apply is blocked, re-accept it, regenerate the report artifact with a note,
  then apply through the confirmation dialog and approval step; follow the completion links and confirm the entity
  screens, IAM permission, workflow, notification template and report all exist.
- Also exercise the empty state, a generation failure against an unreachable provider, and discarding a draft plan.
- Visual check: artifact statuses are distinguishable, the diff pane aligns old and new values, streaming skeletons
  do not shift layout, footer counters update live, and the apply progress list stays readable while scrolling.
- Regression: `/ai` providers and models, the app builder screens and the dynamic data model editor work.

### Slices

1. **Generation + plans** — migration `0015_ai_app_builder.sql`, generation endpoint with artifact streaming,
   key/type validation, persistence, four permission keys, tests. **Done when:** a generated plan validates.
   — **Done.** The generation half completed in slice 4 (`generate.rs` + the rewritten
   `POST /generate`); the store, the validators and the four keys landed in slices 1–2.
2. **Review workspace** — `/app-builder` landing and the plan workspace tree, artifact detail, accept/reject/
   edit/regenerate, blocking summary, keyboard and mobile. **Done when:** a plan reaches a fully accepted state.
   — **Code complete (`a98bb248`)**; the walkthrough pass that ticks its boxes is queued behind a
   live holder (`omnion-w4`), and a screen box is ticked by the pass, not by the code existing.
3. **Apply pipeline** — ordered application (entity → fields → screens → permissions → roles → workflow →
   notifications → report), approval gate, SSE progress. **Done when:** the plan creates a working app end to end.
   — **BLOCKED ON A TABLE THIS WAVE DOES NOT OWN.** Step one writes the generated entity, and
   REQ-026's `entities` / `entity_fields` / `entity_records` exist in no worktree; wave 2 owns
   them. Unblocked the moment they land — the step is already specified against their shape.
4. **Safety net** — per-application rollback, failure retry, applied filters, JSON export, cost attribution.
   **Done when:** a failed apply rolls back to a clean state and history shows it.
   — **JSON export LANDED and wire-proven (`c294b788`, `55308693`, `7469649d`); cost attribution is
   NOT BUILDABLE HERE, and the reason is the finding rather than an excuse.** There is no price source
   anywhere in the tree: `ai_models` (migration `0008`) carries no price column, no pricing table
   exists in any crate, and REQ-104's `ai_spend_daily` is wave-3b (w10's). So `settle()` keeps writing
   `cost_cents: 0` and the export carries the **stored** figure rather than one derived from a rate
   this platform does not hold — building a second pricing table to fill one column is exactly the
   trap a "cost is displayed" criterion cannot see. When REQ-104 lands, the number becomes real and
   nothing here has to change but the write. Also landed: the row-level delete already refused
   applied plans on the wire.
   **Bulk delete of drafts LANDED and wire-proven** (`8299d2f1`, `16fbf8d0`, `088bb666`):
   `delete_plans` in the store, `POST /plans/bulk-delete` guarded by `appbuilder.review`, and the
   console's checkbox column with a confirmation that names the applied plans before the delete.
   **Still open in this slice: per-application rollback and failure retry** — both sit behind
   slice 3's table, the same blocker as apply itself.

### Risks / notes

- Generation must never partially apply: drafts are inert rows, only the apply runner writes to live tables, and
  reserved keys or collisions with existing entities are caught at validation time.
- A generated role is a privilege change: grant the smallest permission set the plan needs and audit the approval.
- Model output is untrusted input — validate every key against platform naming rules, never interpolate artifact
  text into SQL, and show tokens and cost per plan before apply.
- A plan an applied app depends on can never be deleted; keep versions so a rejected attempt can be compared.
