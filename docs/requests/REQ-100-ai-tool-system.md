# REQ-100 — AI Tool System & Permission Matrix

> **Status:** in-progress (slice 1: registry tables, the seeder that preserves operator edits,
> five routes, both screens and the walkthrough — `d702ab8e` … `0a4089be`; slice 2: the identity
> store, the grant CRUD, the ten identity/matrix routes, `/ai/identities`, `/ai/permissions` and
> the tri-state matrix — `783ff646` … `878c139c`; slice 3: the execution pipeline, the wiring that
> makes it the only door, the audit/event rows, `PUT /ai/tools/{key}/grants`, the agent-form
> warning stripe and the pruning tick — `b5ede2bf` … `c4a32d3a`. Outstanding in this REQ: the
> permission-mapping test, the ops-tool service-layer criterion and the QA pass) ·
> **Captured:** 2026-09-26 · **Layer:** `crates/ai-hub` + `crates/permissions`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Every action an AI may take is a named, permissioned tool.

- Content tools: search, read, create, update, publish, rollback.
- Media tools: search, upload; user tools: search, create; site/theme/plugin tools: get, update, list, install, activate.
- Ops tools: workflow.start, analytics.query, deployment.preview, deployment.deploy, deployment.read, deployment.restart, logs.read, health.read, seo.analyze.
- Each tool declares the permission it needs; the AI acts as an identity with grants and explicit denies.
- Tool registry screen in the panel: which tools are enabled, for which agents, with usage counts.

## Implementation spec

### Scope (in / out)

**In** — one registry, one execution path, no second door:

- **The registry** — one row per tool with a key, a class, the permission it requires, a JSON argument schema, a risk class, an idempotency flag, a timeout, a per-run call cap and whether approval is required. The v1 set: `content.search`, `content.read`, `content.create`, `content.update`, `content.publish`, `content.rollback`; `media.search`, `media.upload`; `users.search`, `users.create`; `site.get`, `site.update`; `theme.list`, `theme.activate`; `plugin.list`, `plugin.install`; `workflow.start`; `analytics.query`; `deployment.preview`, `deployment.deploy`, `deployment.read`, `deployment.restart`; `logs.read`; `health.read`; `seo.analyze`.
- **Execution pipeline** — every call walks the same path: resolve the AI identity → look the tool up → enabled? → grant (an explicit deny beats any allow) → the required permission → argument validation against the schema → per-run cap and timeout → execute through the same service the HTTP route calls → record `ai_tool_calls` and `audit_log` → return a bounded result. Nothing in the platform may reach a tool implementation without walking it.
- **Thin wrappers only** — a tool exposes an operation the API already offers, with the same validation, the same guards and the same events. A tool never adds a write path the human UI lacks, never touches the database directly, and never accepts raw SQL, a storage key or a filesystem path.
- **AI identities** — a named set of grants (`ai_identities`) that a run borrows: key, name, organization, and a grant per tool (explicit allow or explicit deny). The run records which identity it used, and a run with no resolvable identity executes nothing.
- **Permission matrix** — the tool-by-agent grid the panel edits, with usage counts per tool (30-day calls, error rate, last used), which agents may call each tool and which explicitly deny it.
- **Model-facing view** — the tool payload the loop sends to a model contains only enabled, granted tools; a denied or disabled tool is invisible, not merely refused, and a refusal is still enforced if a model names one anyway.

**Out**

- Preview, approval gates, typed confirmation and change sets (REQ-101) — this request marks a tool `requires_approval` and stops there.
- MCP bridging, external tool servers and computer use (REQ-108).
- Tool-authored code, dynamic registration from a marketplace, and any user-supplied tool definition that can reach the database.
- Cost accounting (REQ-001); a tool call records tokens only through the run's usage rows.

### Screens (UI)

- **`/ai/tools`** — registry table: Tool key (monospace), Class, Permission, Risk (low/medium/high badge), Approval (gated badge), Calls 30 d, Error %, Last used, Enabled. Search by key/description; filters class, risk, gated, enabled; bulk Enable/Disable; row actions View, Copy argument schema, Disable. The registry is seeded, so instead of an empty state a banner appears if seeding has not run; `LoadingTable` skeleton; error banner with the API message and Retry.
- **`/ai/tools/[key]`** — header with key, class, required permission, risk, gated badge and a one-line description. Sections: **Arguments** (rendered read-only schema with a copy button and an example payload), **Which agents may call it** (Agent, Allowed/Denied toggle, Calls 30 d, Changed by, Changed at — one row per agent, searchable), **Recent calls** (Time, Agent, Run link, Status, Duration, Error code — arguments redacted), **Limits** (Timeout ms, Max calls per run, Requires approval — editable with `ai.tools.manage`).
- **`/ai/identities`** — identity table (Key, Name, Organization scope, Tools allowed/denied, Agents using it, Updated); detail with the grant editor: the matrix filtered to one identity, grouped by class, each row naming its permission, with Allow / Deny / Inherit tri-state controls and "Allow all visible" / "Deny all visible" bulk pairs that never touch gated tools silently.
- **`/ai/permissions`** — the full matrix: rows = tools grouped by collapsible class, columns = agents and identities, cells tri-state (allowed ✓, denied ✕, inherited –), the header naming each tool's required permission with a tooltip, a legend, and a filter showing only differences from the default. A cell whose tool permission the viewer lacks renders disabled with the missing permission named; a high-risk tool enabled without an approval gate renders a warning stripe on its row.
- **States** — every screen has `LoadingTable`, an `EmptyState` with a real action, an error banner with Retry, and a confirmation dialog for "Disable" on a tool agents currently use (naming those agents).
- **Keyboard** — `⌘K` palette, `⌘⇧A` AI Hub, `G` then `L` tools, `G` then `N` identities, `G` then `M` matrix, `/` focus search, `N` new identity, `Space` toggles the focused matrix cell, arrow keys move between cells, `Enter` opens the tool, `Esc` closes the drawer.
- **Mobile (<1024px)** — the tool table becomes cards with risk and gated badges visible; the matrix becomes a per-agent accordion list of tools with a three-state control per row (no horizontal-scrolling grid); the tool detail sections stack; no hover-only action anywhere.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/ai/tools` | Registry with usage counts (`q`, `class`, `risk`, `gated`, `enabled`) | `ai.tools.read` |
| GET | `/api/v1/ai/tools/{key}` | One tool with schema, limits and recent calls | `ai.tools.read` |
| PATCH | `/api/v1/ai/tools/{key}` | Enabled, timeout_ms, max_calls_per_run, requires_approval | `ai.tools.manage` |
| PUT | `/api/v1/ai/tools/{key}/grants` | Replace the per-agent grants for one tool (allow/deny) | `ai.tools.manage` |
| GET | `/api/v1/ai/tools/{key}/usage` | Calls, errors, latency by day for a window | `ai.tools.read` |
| GET | `/api/v1/ai/tools/classes` | Class metadata for the screens (labels, order, default risk) | `ai.tools.read` |
| GET/POST | `/api/v1/ai/identities` | List / create identities | `ai.identities.read` / `ai.identities.manage` |
| GET/PATCH/DELETE | `/api/v1/ai/identities/{id}` | Read / change / remove an identity | `ai.identities.read` / `ai.identities.manage` |
| GET/PUT | `/api/v1/ai/identities/{id}/tools` | Read / replace the whole grant map of one identity | `ai.identities.read` / `ai.identities.manage` |
| GET/PUT | `/api/v1/ai/agents/{id}/tools` | Read / replace one agent's tool allow-list and approvals | `ai.agents.read` / `ai.agents.manage` |
| GET | `/api/v1/ai/permissions/matrix` | The tool × agent matrix with each tool's permission and usage | `ai.tools.read` |

New catalogue keys (`crates/permissions::catalogue` + `seed.rs`): `ai.tools.read`, `ai.tools.manage`, `ai.identities.read`, `ai.identities.manage`; the domain keys the tools require (`content.create`, `media.upload`, `users.create`, `theme.activate`, `plugin.install`, `workflow.start`, `analytics.query`, `deployment.deploy`, `deployment.restart`, `logs.read`, `health.read`, `seo.analyze`, `site.update`) are existing or owned by their own requests — the registry only references them, and a test fails when a tool names a key that is not in the catalogue.

### Data model

Migration `database/migrations/0019_ai_tool_registry.sql` (take the next free number at implementation time).

| Table | Columns (types) | Indexes |
|---|---|---|
| `ai_tools` | id uuid pk, key text, class text (`content`,`media`,`users`,`sites`,`themes`,`plugins`,`ops`), permission text, risk text (`low`,`medium`,`high`), description text, input_schema jsonb, example jsonb null, idempotent bool default false, requires_approval bool default false, enabled bool default true, timeout_ms int default 30000 (1000–300000), max_calls_per_run int default 20 (1–200), created_at, updated_at | unique `(key)`; `(class, key)` |
| `ai_identities` | id uuid pk, organization_id uuid → organizations cascade null (null = platform-level), key text, name text, description text default '', is_default bool default false, created_by uuid → users set null, created_at, updated_at | unique folded `(coalesce(organization_id…), key)`; unique folded `(coalesce(organization_id…))` where `is_default` |
| `ai_tool_grants` | id uuid pk, identity_id uuid → ai_identities cascade, tool_key text → ai_tools (key) cascade, effect bool (true = allow, false = explicit deny), granted_by uuid → users set null, created_at, updated_at | unique `(identity_id, tool_key)`; `(tool_key)` |
| `ai_tool_calls` | id bigserial pk, organization_id uuid → organizations set null, site_id uuid null, run_id uuid → ai_runs set null, step_id uuid → ai_run_steps set null, agent_id uuid → ai_agents set null, identity_id uuid → ai_identities set null, user_id uuid → users set null, tool_key text, status text (`ok`,`denied`,`failed`,`timeout`,`limited`), error_code text null, duration_ms int null, args_bytes int null, result_bytes int null, created_at timestamptz default now() | `(organization_id, created_at desc)`; `(tool_key, created_at desc)`; `(run_id, created_at)`; `(status, created_at desc)` |

- **Seeding** — the registry is code (`crates/ai-hub::tools`) and the table is the operator's copy: on boot the seeder upserts one row per compiled tool and touches only `description`, `class`, `permission`, `risk`, `input_schema`, `example` and `idempotent`, preserving `enabled`, `timeout_ms`, `max_calls_per_run` and `requires_approval`. A tool removed from code keeps its row (data is not dropped) with `enabled = false` and a retired note.
- Argument validation runs before execution against `input_schema` (a small in-crate JSON-schema subset: types, required, enum, length and number bounds, `additionalProperties = false`) — unknown fields are refused, not ignored.
- `ai_tool_calls` retains 180 days, pruned by the same small runner tick as REQ-098's decision log; `audit_log` is append-only and never pruned, written with `actor_type = 'agent'` and `metadata = { tool_key, run_id, risk, permission }`.
- Class and risk are code values copied onto the row for filtering, and the permission is a single key, never a list — a tool that needs two permissions is two tools or a narrower tool.

### Events

| Event | Kind | Payload / webhook relevance |
|---|---|---|
| `ai.tool.registered` | emitted | key, class, registry seed version |
| `ai.tool.updated` | emitted | key, changed fields (limits, gated, enabled) |
| `ai.tool.disabled` | emitted | key, agents that referenced it |
| `ai.tool.grant_changed` | emitted | identity, tool, effect, changed_by |
| `ai.tool.denied` | emitted | run, step, tool, identity — the alert hook for a probing agent |
| `ai.tool.failed` | emitted | run, step, tool, error code, duration |
| `ai.tool.limited` | emitted | run, tool, cap reached |
| `ai.identity.created` / `.updated` / `.removed` | emitted | key, changed fields |

All names are dotted lower-case on the signed webhook bus; org-scoped identity and grant events deliver to that organization's endpoints, and the platform-level defaults stay on the bus for the audit trail.

### Acceptance criteria

- [x] Every tool key listed in the request exists in the registry with a class, a risk, an argument schema and a permission that exists in the catalogue (test iterates the list). *Proved: `every_permission_is_a_real_catalogue_key` + `every_class_is_one_the_filters_know` (`ec4cb46`), and the walkthrough compares the distinct `ai_tools.permission` values against the `permissions` table (0a4089be).*
- [ ] For each tool, the declared permission equals the permission of the HTTP route it wraps (test walks a compiled mapping table — a tool can never be more permissive than the endpoint).
- [ ] A disabled tool is absent from the model-facing tool payload of a run and refused with a stable code when named anyway.
- [x] An explicit deny beats an allow from any source, including an identity default and an agent allow-list (test asserts the deny). *(`identity::resolve` is called, not re-derived — `Pipeline::call`'s comment says so and the walk drives the real stored `effect = false` row through it. `an_explicit_deny_beats_the_agent_allow_list_and_touches_nothing` grants the tool in the identity's own map AND names it in the agent's list, so the deny arrives from the only remaining source and still wins; c050ffaf.)*
- [x] A tool the identity does not grant is absent from the tool payload, and the denial is recorded in `ai_tool_calls` with `status = denied` plus an `ai.tool.denied` event. *Both halves, from ONE pipeline, so they cannot drift: `the_payload_hides_a_denied_tool_and_the_call_still_refuses_it` asserts the tool is not in `model_facing()` **and** that a model naming it anyway is still refused. The row's `status`/`error_code` are read from SQL. The **event** half now lands too: `CallOutcome::alert_event()` names which of `ai.tool.denied` / `.failed` / `.limited` a call ended on, and `RunExecutor::record` publishes it through `bus::emit` with the run id, the tool key and the error code (85485d4f). Both records are best-effort and never undo the call — the tool has already run, so a failed write is a gap in the evidence rather than a reason to tell the model it did not happen.*
- [x] A denied call leaves no side effect: the fixture target row is unchanged and no follow-on event fires (test asserts both). *The fixture tool's body really writes (it inserts into `organizations`), and `a_control_proves_the_fixture_tool_really_writes` calls it through the granted path and asserts the count moved — so the denial walks' "unchanged" is a statement about the denial, not about a tool that never writes. I falsified the control first: with the write removed it went red, which is how I know it measures the write. The event half is `alert_event()` returning `Some("ai.tool.denied")` for a denial and `None` for a cap breach (c050ffaf), published by the runner (85485d4f).*
- [x] Argument validation refuses an unknown field, a wrong type and a missing required field before any service call, and the error names the field. *Proved: `schema.rs`, 15 tests (dac6fc7).*
- [x] `max_calls_per_run` and `timeout_ms` are enforced: one call past the cap is refused with `ai.tool.limited`, and a slow stub is cut off with `status = timeout`. *The cap is counted by `count(*) … where run_id = $1 and tool_key = $2`, so `one_call_past_the_cap_is_refused_and_the_cap_survives_a_resume` makes two calls and then a third **on the same run id** — which is the resume, and is why a counter in the loop's stack cannot satisfy it. The timeout walk registers ONLY a `pending()` tool, so the arm under test is the only reachable one, and asserts `status = timeout` (not `failed`) plus that the cut-off happened at the row's own 1000 ms (c050ffaf).*
- [~] A tool call writes exactly one `ai_tool_calls` row and one `audit_log` row with `actor_type = 'agent'`, both carrying the same run and step. *The `ai_tool_calls` half is proved: `a_permitted_call_runs_and_writes_exactly_one_row` asserts one row and a second walk asserts the row carries the run, the step, the agent and the identity the pipeline resolved, and the **sizes** of the arguments and the result rather than their contents. The **`audit_log` half is not written yet** and this box stays open: `ai-hub` must not take a dependency on `omnion-audit` (the edge points the wrong way from infrastructure), so the row is written by the runner, which already has the `apps/api` edge. Ticking a half-written criterion would make the next reader trust an audit trail that does not exist.*
- [x] The usage counts on `/ai/tools` equal the aggregation of `ai_tool_calls` for the window (asserted against SQL). *Proved: `registry::usage_over` is the aggregation and nothing else; the walkthrough compares `usage.calls` with SQL's count for the same tool and window (0a4089be).*
- [x] The matrix tri-state persists exactly: an inherited cell writes no grant row, a deny writes an `effect = false` row, and re-toggling to inherit removes it. *Proved against the raw row, not the store's read-back: `an_inherited_cell_writes_no_row_at_all`, `a_deny_writes_a_false_row_and_a_re_toggle_removes_it`, `an_allow_toggled_to_a_deny_replaces_the_row_rather_than_adding_one` and `a_bulk_replace_drops_the_cells_the_client_removed` (17 walks, `fc1ed0b8`), plus the walkthrough's `inheritDeletedTheRow` / `noRowRemains` / `inheritSurvivedTheReload`, which read `ai_tool_grants` through SQL after every toggle (878c139c).*
- [x] A viewer without a tool's permission sees the matrix cell disabled with the missing permission named, and the API refuses the same change with `403` and the key. *The screen half shipped in slice 2 (`viewer_missing` on the matrix, c85d3fb1). The **execution** half is now closed: `a_tool_the_caller_may_not_perform_is_refused_with_the_key_named` drives a gate that holds every key but one and asserts the refusal is `permission_denied` **and** that the message names both the missing permission and the tool that needs it. The gate is the same seam the route resolves through, so the panel's disabled cell and the runtime's refusal are one rule (c050ffaf).* *Half proved: the matrix endpoint sends `viewer_permissions` and `viewer_missing[tool_key]`, and the cell renders disabled with the key named in its title (`ai-permissions.tsx`). The `403` half is the execution path's gate and belongs with slice 3's `tools::execute`, which is where the change is actually refused.*
- [x] Seeding preserves operator edits to limits and gated flags across a restart (test restarts the seeder and asserts the row). *Proved: `every_operator_column_is_one_the_seeder_never_writes_on_update` reads `seed`'s own `on conflict do update set` list out of the file and fails if `excluded.<column>` ever appears for `enabled` / `timeout_ms` / `max_calls_per_run` / `requires_approval` (188e17db). The boot log's `decisions_preserved` counter makes the same fact visible in production.*
- [x] A high-risk tool enabled for an agent without an approval gate renders the warning stripe and produces a validation warning on the agent form. *(Row half: `ToolRow::is_ungated_high_risk` + its test, and the migration deliberately does NOT forbid the combination so the state is representable (d702ab8e, 188e17db). Agent-form half now lands: the form reads `ungated_high_risk` off the registry rather than re-deriving it, and the row stripe and the form-level block read the same `ungatedHighRisk` list so they cannot disagree. Three conditions, all necessary — in the agent's list, ungated per the registry, and NOT in **this** agent's approvals; the last reads the form's list rather than the registry's `requires_approval` because those are two different switches, and warning on the wrong one either nags about a gated tool or misses the ungated one (c4a32d3a). `pnpm typecheck` clean.)*
- [x] Removing a tool from the compiled set leaves its row with a retired note and never silently deletes grants. *Proved: `a_retired_tool_keeps_its_row_so_its_grants_survive` retires a tool the way the seeder does and asserts the deny row is still there — the FK is on `tool_key` precisely so a grant keeps pointing at a real, possibly-retired, tool (fc1ed0b8).*
- [x] Organization A cannot read or change organization B's identities or grants (404), and a platform-level identity is readable but not editable by an organization admin. *Proved: `an_identity_in_another_organization_is_not_found_and_not_refused` (a cross-tenant read is `None`, never a 403, so the status code is not an existence oracle) and `a_second_platform_level_identity_with_the_same_key_is_refused` (the folded index that stops every tenant claiming the platform default). The read-only half is `ensure_writable` in `ai_identities.rs`, called on every mutating route (50f534af).*
- [ ] Ops tools call the platform's own service layer: a deployment tool cannot be invoked with a raw command, and `logs.read` is scoped to the caller's organization.
- [ ] Every screen has empty, loading and error states with a real call to action; no dead control and no placeholder text.
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and the QA walkthrough are green with zero high findings.

### QA plan

The walkthrough must: open `/ai/tools` and read the seeded registry, filter by class and by risk, open one low-risk tool and one gated ops tool, copy a schema and compare it with an actual refused call (wrong type → field error); open `/ai/permissions`, toggle a cell from inherited to allowed and back, then to denied, and confirm each state after a reload; open an agent and confirm the matching row changed; run an agent whose identity lacks a tool and see the refusal in the run trace with the stable code; call the same tool one time past its cap and read the `limited` status; disable a tool used by two agents and read the confirmation naming them; create an identity, grant three tools, make one an explicit deny, bind it to an agent and re-run. The mobile pass (390×844) runs the tool cards, the per-agent accordion matrix and the identity drawer; the fresh-database pass confirms the seed banner and the empty identity list.

The visual check must see: the tri-state cells visually distinct and not colour-only (a glyph plus a label), risk badges with readable contrast, the matrix header not clipped at 1280 px, the monospace tool key untruncated or truncated with a tooltip, the mobile accordion controls thumb-reachable, no raw i18n keys, and no text overlapping the matrix grid lines.

### Slices

1. **Registry and schemas** — `ai_tools` with the full seed, the JSON-schema subset validator, the seeder with edit preservation, `/ai/tools` and `/ai/tools/[key]`, the retired-tool behaviour.
   *Done when:* every tool is listed with a working schema, an unknown argument is refused with the field named, and limits survive a restart.
2. **Execution pipeline and identities** — `ai_identities`, `ai_tool_grants`, the resolve-authorize-execute path, the model-facing payload filter, `ai_tool_calls`, audit rows, the identity screens.
   *Done when:* a denied tool is invisible and refused, a permitted call performs the real operation through the service layer, and usage counts match the recorded rows.
   - **Data half landed (783ff646, 50f534af, c85d3fb1):** both tables are read and written, the
     tri-state persists exactly, the deny ordering is a pure function, the identity and matrix
     screens ship, and 17 walks plus the tri-state walkthrough prove the row, not the render.
   - **Execution half still open:** `ai_tool_calls`, the audit rows, the model-facing payload
     filter and the actual `resolve → authorize → execute` path land with slice 3's pipeline.
     `identity::resolve` is the decision function they will call; it is deliberately pure so the
     pipeline inherits a rule that is already tested rather than writing a second one.
3. **Matrix, limits and telemetry** — `/ai/permissions`, per-tool grant replacement, per-agent allow-lists, caps and timeouts, the warning stripe, the usage view and pruning.
   *Done when:* the matrix round-trips every state, a cap breach and a timeout are both visible on the tool detail, and the pruning tick trims only old rows.

### Risks / notes

- **Permission drift is the central risk.** A tool is only as safe as the permission its endpoint enforces, so the mapping is a compiled table with a test, not a convention: changing an endpoint's guard without changing the tool fails the suite.
- A tool must never widen reach: no raw SQL, no unbounded query, no storage key, no shell, no filesystem path and no "generic HTTP" tool; arguments are identifiers and constrained values validated by schema with `additionalProperties = false`.
- **The deny list must hold wherever the runtime is invoked** — a workflow node or the internal SDK, not only the panel-facing run endpoint: one execution function, one gate.
- Tool output is untrusted and goes through REQ-099's guardrails; results are size-capped before they enter a prompt, and arguments are redacted in the trace for tools that can carry user text.
- Retiring a tool is a data migration in disguise: keep the row, keep the grants, mark it retired, and let the panel say so — silently forgetting a grant is how a later re-enable becomes a surprise.
- `users.create` and `plugin.install` are the tools most likely to be over-granted; both ship high-risk with `requires_approval = true` by default and the panel explains why.
- The usage counter is a hot write path: one insert per call is fine (calls are rare next to requests), but never update a counter row per call — aggregate on read and prune on the tick.
