# REQ-045 — AI App Builder *(headline)*

> **Status:** in-progress (slice 2 · `65bdf683` — **the review surface on the wire, and the four
> permission keys** · `apps/api/src/routes/app_builder.rs` (nine routes: list, detail, examples,
> edit, accept, reject, regenerate, reject-plan, delete, generate), migrations `0226` (a decision
> carries its reason) and `0227` (one **live** version per `(plan, kind, key)`), plus four keys in
> `crates/permissions`. Slice 1's store is unchanged in shape; slice 2 extended it with
> `reject_artifact`, `reject_plan`, `accept_artifact` and the two reason columns.
> **The four keys are catalogued and three of them guard routes; `appbuilder.apply` guards nothing
> yet on purpose** — the apply runner is slice 3 and a route answering "coming soon" is exactly
> what the Definition of Done forbids. `generate` writes the plan before the provider is asked and
> fails with the reason on the row; it does **not** fake the typed generator, and a walk asserts
> the mock provider was called **zero** times.
> **Three defects closed, two of them pre-existing in slice 1:** (1) a `permission` artifact could
> **never** be written — the store applied the storage key rule to a `domain.action` key whose dot
> is vocabulary, so a REQUIRED kind was un-fillable and no plan could ever be applicable; (2)
> regeneration raised duplicate-key against the absolute `(plan_id, kind, key)` constraint, so the
> "kept plan versions" the request asks for was a 500 — `0227` makes the index partial over live
> versions; (3) a filter refusal answered `500` where the caller's input deserves `422`.
> **41 module unit tests · 10 route walks (were 0) · permissions 64/64 · `pnpm typecheck` pending** ·
> previous: **Status:** pending)
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

- [ ] The sample prompt yields a plan with at least one entity, its fields, screens, permissions, a role,
      a workflow, a notification and a report.
- [ ] Artifacts stream into the tree during generation and the tree is usable before it ends.
- [ ] Every artifact shows a rationale and its validation result.
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
- [ ] Plan list, filters, bulk delete of drafts and JSON export work; applied plans are undeletable.
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
2. **Review workspace** — `/app-builder` landing and the plan workspace tree, artifact detail, accept/reject/
   edit/regenerate, blocking summary, keyboard and mobile. **Done when:** a plan reaches a fully accepted state.
3. **Apply pipeline** — ordered application (entity → fields → screens → permissions → roles → workflow →
   notifications → report), approval gate, SSE progress. **Done when:** the plan creates a working app end to end.
4. **Safety net** — per-application rollback, failure retry, applied filters, JSON export, cost attribution.
   **Done when:** a failed apply rolls back to a clean state and history shows it.

### Risks / notes

- Generation must never partially apply: drafts are inert rows, only the apply runner writes to live tables, and
  reserved keys or collisions with existing entities are caught at validation time.
- A generated role is a privilege change: grant the smallest permission set the plan needs and audit the approval.
- Model output is untrusted input — validate every key against platform naming rules, never interpolate artifact
  text into SQL, and show tokens and cost per plan before apply.
- A plan an applied app depends on can never be deleted; keep versions so a rejected attempt can be compared.
