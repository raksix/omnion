# REQ-092 — Expressions, Data Mapping & Variables

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/workflows`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Moving data between nodes safely.

- Expression language with sandboxing (AST allow-list, resource limits, violation errors).
- Data mapping UI: drag a field from upstream data into a parameter.
- Pinned data for development runs; mock data per node.
- Instance and project variables (used in expressions) with per-environment values.
- Environments: production/staging value sets for variables and credentials.

## Implementation spec

### Scope (in / out)

**In**

- **Expression language** — `{{ … }}` bindings compiled to an AST at save time. Closed grammar: member access, indexing, literals, `+ - * / %`, comparisons, `&& || !`, ternary, `??`, and a fixed function allow-list (`lower`, `upper`, `trim`, `split`, `join`, `replace`, `slice`, `length`, `first`, `last`, `item`, `keys`, `pick`, `omit`, `merge`, `round`, `floor`, `ceil`, `min`, `max`, `abs`, `parseJson`, `toJson`, `now`, `today`, `formatDate`, `addDays`, `diffDays`, `uuid`, `sha256`, `coalesce`, `default`). No assignment, no loops, no user-defined functions, no host access.
- **Resolution context** — `input` (this node's incoming items), `json` (current item shorthand), upstream node outputs addressed by node id, `vars` (instance and project variables), `env` (environment name only), `credentials` (reference handles, never values), `now`/`today`, and execution metadata (run id, workflow id, trigger kind).
- **Sandbox limits** — expression ≤ 2000 characters, ≤ 200 AST nodes, evaluation ≤ 50 ms CPU, string ≤ 1 MiB, collection walk ≤ 100 000 elements, nesting depth ≤ 20. Violations return named errors: `EXPR_SYNTAX`, `EXPR_UNKNOWN_MEMBER`, `EXPR_FORBIDDEN`, `EXPR_DEPTH`, `EXPR_TIMEOUT`, `EXPR_OUTPUT_TOO_LARGE`. Prototype and constructor access is a forbidden-construct error, not a runtime surprise.
- **Save-time validation** — every expression is parsed and type-checked against the upstream node schemas known statically. Unknown members on a *known* node are save-blocking errors; bindings into payloads that are dynamic by nature (webhook bodies, HTTP responses) are warnings with an explanation. Forbidden constructs always block, naming the line and construct.
- **Data mapping UI** — drag a field from the upstream data panel into a parameter. Type-aware insertion: text parameters get the binding as text, numeric parameters get a numeric cast, whole-object parameters get the object path. Manual editing keeps `{{ }}` autocomplete, and hovering a chip shows the last resolved value from pinned data.
- **Pinned data and mocks** — a node can carry pinned output (JSON) used by development runs without touching the upstream side of the workflow, and a mock output used only when the upstream produced no items. Both are marked in the trace so nobody mistakes mock data for real data.
- **Variables** — two scopes: **instance** (deployment-wide, operator-managed) and **project** (organization-scoped, managed inside the workspace). Each variable has `name` (dotted key), `type` (`string | number | bool | json | secret`), a default value and optional per-environment overrides. `secret` values are write-only: masked after save, rotatable, never returned by the API, never written to logs.
- **Environments** — the platform's two value sets, `production` and `staging`. A run resolves variables and credential references for its environment; development runs default to staging and switching to production values requires an explicit confirmation. Unknown variable in an expression evaluates to null unless a `default(...)`/`??` fallback is written, and the warning is logged on the step.
- **Precedence, defined once** — node mock > pinned data > real node output; for values: step parameter literal > project variable production/staging value > project default > instance environment value > instance default.
- **Resolution is pure and memoised** — a step resolves each distinct expression once per input item; resolution cannot mutate context, call hosts or write anything.

**Out**

- Arbitrary user code (JavaScript/Python nodes) — deliberately excluded; the closed AST is the whole language.
- Secret storage, rotation policy and audit for credential material — REQ-037 owns the secrets store; `secret` variables are resolved through it.
- Sandbox environment cloning (staging stacks) — REQ-034; here an environment is a value set, not a second deployment.
- Canvas layout and node chrome — REQ-004; this request ships the affordances the canvas embeds.
- Workflow versions and publishing — REQ-095; pinned data is development-only and never reaches a published version's runtime behaviour.

### Screens (UI)

- **`/automations/[id]` (editor)** — parameter inputs accept `{{ }}` with inline validation: a red underline plus message for blocking errors, an amber note for dynamic paths. A **data picker** side sheet lists upstream nodes with a searchable schema tree, sample values from the last run or pinned data, and per-field actions (Insert as text, Insert as number, Insert whole object, Copy path). Dragging a field from the sheet into any parameter performs the same insert. Chips render the friendly node label, not raw JSON paths.
- **`/automations/[id]/parameters` (mapping view)** — a full-screen mapping table: rows are parameters across all nodes, columns are Binding, Resolved preview (from pinned data), Status (ok, warning, error), and quick-jump to the node. Search by parameter, binding or node.
- **`/settings/variables`** — instance variables table: Key, Type, Default, Production, Staging, Used by (count), Updated, By. Create/edit dialog with tabs per environment, secret fields masked with Rotate, delete blocked while `Used by > 0` with a "Find usages" link that opens the mapping view filtered to that key.
- **Workspace variables** — a Variables tab on the workflow list's workspace settings, same table scoped to the organization, visible only to holders of the project-variable permission.
- **Run detail** — each step shows the resolved value per parameter (Collapsed by default, expandable) and unresolved bindings are highlighted with the error code. Mock and pinned inputs are visibly labelled.
- **States and access** — empty, loading and error states on both tables; secret values never render even to their author; the picker is keyboard-navigable; mapping view is usable on mobile in read-only form.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/variables` | List instance variables (values masked for `secret`) | `variables.read` |
| POST | `/api/v1/variables` | Create an instance variable | `variables.manage` |
| GET/PATCH/DELETE | `/api/v1/variables/{id}` | Read (masked) / update / delete an instance variable | `variables.read` / `variables.manage` |
| POST | `/api/v1/variables/{id}/rotate` | Replace a secret value | `variables.manage` |
| GET | `/api/v1/variables/usages?key=` | Nodes and workflows binding a key | `variables.read` |
| GET | `/api/v1/workspaces/{id}/variables` | Project variables for one workspace | `variables.read` |
| POST/PATCH/DELETE | `/api/v1/workspaces/{id}/variables/{var_id}` | Manage project variables | `variables.manage` |
| POST | `/api/v1/workflows/{id}/validate` | Validate expressions: blocking errors, warnings, resolved samples | `workflows.read` |
| POST | `/api/v1/workflows/{id}/preview-binding` | Resolve a single expression against a node's pinned data | `workflows.read` |
| GET/PUT | `/api/v1/workflows/{id}/nodes/{node_id}/pinned-data` | Read / set pinned output for a node | `workflows.manage` |
| GET/PUT | `/api/v1/workflows/{id}/nodes/{node_id}/mock` | Read / set mock output for a node | `workflows.manage` |
| GET | `/api/v1/workflows/{id}/mapping` | Parameter mapping table rows (binding, status, preview) | `workflows.read` |

Two new permission keys: `variables.read` and `variables.manage`. Instance scope additionally requires the operator role; project scope follows the workspace membership of the caller.

### Data model

Migration `database/migrations/0016_variables_and_mapping.sql` (next free number if taken).

| Table | Columns (types) | Indexes / rules |
|---|---|---|
| `variables` | id uuid pk, scope text ('instance','project'), organization_id uuid → organizations cascade (null for instance), key text, type text ('string','number','bool','json','secret'), default_value jsonb, description text, created_by uuid → users set null, created_at, updated_at | unique `(coalesce(organization_id, '00000000-0000-0000-0000-000000000000'::uuid), key)`; check `(scope = 'instance') = (organization_id is null)`; check `key ~ '^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$'`; check `(type = 'secret') or (default_value is not null)` |
| `variable_values` | variable_id uuid → variables cascade, environment text ('production','staging'), value jsonb, is_secret boolean default false, updated_by uuid → users set null, updated_at | pk `(variable_id, environment)` |
| `variable_secret_refs` | variable_id uuid pk → variables cascade, secret_id uuid → platform secrets (REQ-037), rotated_at | no plaintext column exists in this schema; reads go through the secrets crate |
| `node_pinned_data` | workflow_id uuid → workflows cascade, node_id text, kind text ('pinned','mock'), data jsonb, updated_by uuid → users set null, updated_at | pk `(workflow_id, node_id, kind)`; check `jsonb_typeof(data) in ('object','array')`; size capped at 256 KiB in application code and by a check on length of the text form |

Expressions are stored inside the workflow definition JSON, not in their own table: the definition is the source of truth, and the mapping view is derived. No index is added to definition JSON.

### Events

| Event | Kind | Notes |
|---|---|---|
| `workflow.expression.rejected` | emitted | save blocked: workflow id, node id, error code, construct name |
| `workflow.expression.warning` | emitted | dynamic path warning, emitted at save, not per run |
| `workflow.expression.violation` | emitted | sandbox limit hit at run time (timeout, depth, size) |
| `variable.created` / `.updated` / `.deleted` | emitted | scope, key, environment touched — never the value |
| `variable.secret.rotated` | emitted | variable id only; value never appears anywhere |
| `variable.value.read` | emitted | read of a secret value through a run, for the audit trail (REQ-039) |
| `workflow.pinned_data.updated` | emitted | workflow, node, kind; used by the trace to label runs |
| `workflow.execution.step.bindings_resolved` | emitted | debug-level, behind an organization flag; resolved values excluded |

### Acceptance criteria

- [ ] `{{ }}` expressions are parsed at save time; a forbidden construct (prototype access, assignment, unknown global) blocks the save with a message naming the construct and position.
- [ ] An unknown member on a node with a known schema blocks the save; the same path into a webhook-derived payload saves with a warning and an explanation.
- [ ] Sandbox limits are enforced: a deeply nested expression hits `EXPR_DEPTH`, a large collection walk hits `EXPR_OUTPUT_TOO_LARGE`, and a hot loop-shaped expression hits `EXPR_TIMEOUT` — each with its named error in the step trace.
- [ ] No expression can reach host functions, environment access or the filesystem; the sandbox test suite asserts this and fails when the allow-list is widened.
- [ ] Dragging a field from the picker into a text parameter inserts a text binding; into a number parameter inserts a numeric one; whole-object insert produces the object path — all three verified interactively and by the persisted definition.
- [ ] Autocomplete offers only the trigger's and upstream nodes' real fields plus variables, and inserting from it produces a definition that validates unchanged.
- [ ] Pinned data drives a development run end to end without touching the upstream node, and the run labels the pinned inputs in the trace.
- [ ] Mock output is used only when the upstream produced no items, and the trace says so.
- [ ] A project variable and an instance variable with the same key resolve by documented precedence, and the resolution is visible in the step's parameter view.
- [ ] `secret` variables are masked in list, detail and run views, never appear in events, and Rotate replaces the value without editing the workflow.
- [ ] A run resolves values for its environment; switching a development run to production values requires the confirmation and is audited.
- [ ] Deleting a variable in use is refused with the usage count and a working Find usages link.
- [ ] The mapping view lists every parameter binding with status and a resolved preview, and filters by node without losing the filter on reload.
- [ ] Unresolved or failed bindings show a readable error in the trace, and the run continues per the node's error policy.
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and the QA walkthrough pass with zero high findings; the sandbox test fails when a member is added to the allow-list without a matching test.

### QA plan

The walkthrough must: create an instance variable and a project variable (one secret), bind both in expressions along with a trigger field and an upstream node field; drag a field from the picker into a parameter and verify the persisted definition; trigger each blocking error (forbidden construct, unknown member) and each sandbox limit and read the messages; pin data on a node and run so the upstream never executes; add a mock and force the empty-upstream path; open the mapping view, filter by variable key from Find usages, run a development execution against staging and repeat the confirmation path for production; then open the run and read the resolved parameters for the secret binding to confirm nothing leaks.

The visual check must see: parameter chips wrapping rather than overflowing a narrow card, long JSON collapsed, the picker's schema tree scannable at 1024px, validation messages attached to their field, and masked secret fields that look intentionally empty rather than broken.

### Slices

1. **Language and sandbox** — parser, AST allow-list, limits, named errors, save-time validation, validator endpoint and the sandbox test suite.
   *Done when:* every named error has a test and the allow-list test fails when a new member is added without a test.
2. **Data mapping UI** — picker side sheet, drag-and-drop insertion, autocomplete, mapping view, preview-binding endpoint.
   *Done when:* a field can be mapped in under five seconds with keyboard only and the persisted definition round-trips unchanged.
3. **Pinned data, mocks and variables** — pinned/mock storage and endpoints, variables and per-environment values, secret masking through the secrets store, resolution precedence, run trace labels.
   *Done when:* a pinned development run produces the same downstream result as a live upstream run over the same data.

### Risks / notes

- The sandbox is the security boundary: treat the allow-list as a reviewed artifact and add a test with every new member.
- Percent-style limits protect the process, not just the user — evaluation cost must stay bounded under 50 ms or step workers stall for everyone.
- Variable precedence must be documented in one place and tested in one place; two implementations of resolution always diverge.
- `secret` variables must never be copied into step inputs at rest — resolve at run time and keep the value in memory only.
- Dynamic-path warnings must stay warnings: blocking them would make webhook and HTTP payloads unusable.
- Pinned data can look like real data in a screenshot; label it in the trace, the export and the API response.
