# REQ-100 — AI Tool System & Permission Matrix

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/ai-hub` + `crates/permissions`
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

- [ ] Every tool key listed in the request exists in the registry with a class, a risk, an argument schema and a permission that exists in the catalogue (test iterates the list).
- [ ] For each tool, the declared permission equals the permission of the HTTP route it wraps (test walks a compiled mapping table — a tool can never be more permissive than the endpoint).
- [ ] A disabled tool is absent from the model-facing tool payload of a run and refused with a stable code when named anyway.
- [ ] An explicit deny beats an allow from any source, including an identity default and an agent allow-list (test asserts the deny).
- [ ] A tool the identity does not grant is absent from the tool payload, and the denial is recorded in `ai_tool_calls` with `status = denied` plus an `ai.tool.denied` event.
- [ ] A denied call leaves no side effect: the fixture target row is unchanged and no follow-on event fires (test asserts both).
- [ ] Argument validation refuses an unknown field, a wrong type and a missing required field before any service call, and the error names the field.
- [ ] `max_calls_per_run` and `timeout_ms` are enforced: one call past the cap is refused with `ai.tool.limited`, and a slow stub is cut off with `status = timeout`.
- [ ] A tool call writes exactly one `ai_tool_calls` row and one `audit_log` row with `actor_type = 'agent'`, both carrying the same run and step.
- [ ] The usage counts on `/ai/tools` equal the aggregation of `ai_tool_calls` for the window (asserted against SQL).
- [ ] The matrix tri-state persists exactly: an inherited cell writes no grant row, a deny writes an `effect = false` row, and re-toggling to inherit removes it.
- [ ] A viewer without a tool's permission sees the matrix cell disabled with the missing permission named, and the API refuses the same change with `403` and the key.
- [ ] Seeding preserves operator edits to limits and gated flags across a restart (test restarts the seeder and asserts the row).
- [ ] A high-risk tool enabled for an agent without an approval gate renders the warning stripe and produces a validation warning on the agent form.
- [ ] Removing a tool from the compiled set leaves its row with a retired note and never silently deletes grants.
- [ ] Organization A cannot read or change organization B's identities or grants (404), and a platform-level identity is readable but not editable by an organization admin.
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
