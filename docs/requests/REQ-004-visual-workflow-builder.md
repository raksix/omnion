# REQ-004 — Visual Workflow Builder

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** `apps/admin` + `crates/workflows`
> **Source:** owner brief — platform feature pool (2026-09-25)

## Request

A proper node editor:

```text
[User Created]
      ↓
[Check Role]
   ↙      ↘
Admin    Customer
 ↓          ↓
Email      CRM
```

Plugins can extend it with new node types.

## Notes

- Builder UX prior art + engine research: [`docs/09-N8N-TEARDOWN.md`](../09-N8N-TEARDOWN.md)
  (§8: test webhooks / "listen for test event", waiting/resume, HITL signed callbacks).

## Implementation spec

### Scope (in / out)

**In**

- A full-screen **canvas editor** at `/workflows/[id]/builder`: node palette, infinite canvas, inspector, toolbar, problems panel, and a Table mode fallback (REQ-003's linear editor stays reachable as a tab on the same definition).
- **The graph is the source of truth.** `workflows.graph jsonb` holds `{"nodes":[…],"edges":[…]}`; node positions live in `workflows.ui_state` so semantics and layout never mix. The existing linear `steps` array becomes a projection of the graph, kept in sync by the API, so the runner keeps executing unchanged.
- **Node types v1** — `trigger` (event / schedule / manual / inbound hook), `condition` (`if`/`else`), `switch` (one branch per case + default), `action` (any catalogue action), `wait`, `approval`, `http_request`, `transform` (build fields from a template), `sub_workflow` (run another rule, then continue), `end`, and `note` (sticky, ignored by the engine).
- **Typed ports and validation** — condition exports `true`/`false`, switch one port per case plus `default`, task nodes `success`/`error`. Validation refuses a cycle, a second trigger, an orphan node, an unconnected required input, a duplicate edge, and an edge between incompatible ports; each finding names the node and offers a jump link.
- **Plugin node types** — a manifest may declare `workflowNodes` (key, label, category, ports, parameter schema, plus a declarative HTTP/transform spec or a reference to a sandboxed runner). They appear in the palette under "Plugins"; nothing runs inside the core process (docs/09 §13, lesson 14).
- **Test event listener** — "Listen for a real event" arms a one-shot listener for the selected trigger, shows the captured payload in the inspector for 15 minutes and stores it as an automation test event.
- **Run integration** — the run maps `workflow_steps.node_id` back to nodes, paints per-node status on the canvas (running / succeeded / failed / skipped) and offers **Run from here** and **Retry this node** on the selected node.
- **Canvas interaction** — pan (space-drag or middle-drag), zoom 0.25×–2×, marquee select, multi-move, 8px snap grid with alignment guides, layered auto-layout, minimap, fit-to-content, 50-step undo/redo, copy/paste of a selection as JSON, duplicate (`⌘D`), delete with edge cleanup.

**Out**

- Replacing the runtime: the engine keeps executing durable steps (REQ-003 owns run semantics); a graph compiles to the same step list.
- Real-time collaborative editing — v1 has optimistic concurrency, not presence.
- Arbitrary user code in a node.

### Screens (UI)

- **`/workflows`** — table: Name, Trigger (chip), Nodes, Status (draft/armed/paused/invalid), Runs 7d, Last run, Updated, Owner. Filters: trigger kind, status, site. Bulk Enable/Pause/Delete; row menu Open builder, Table view, Run now, Duplicate, Enable/Pause, Delete. Empty state with New workflow and Browse templates.
- **`/workflows/new`** — name, description, trigger picker; saving creates the graph with a trigger node and opens the builder.
- **`/workflows/[id]`** — overview: trigger summary, node count, validation state, recent runs (Started, Status, Duration, Triggered by) and buttons Open builder, Table view, Run now, Duplicate, Delete.
- **`/workflows/[id]/builder`** — full-viewport workspace (three panes from 1280px up): *Toolbar* (back link, editable name, autosave state "Saved 12s ago / Saving… / Unsaved changes", Validate, Listen for test event, Run once, Run from here, Arm/Pause, undo/redo, zoom controls, fit, minimap toggle, Table mode link, `⌘/` help); *Palette rail* (left, 260px, searchable, grouped Trigger / Logic / Actions / Data / Plugins, collapses to icons, adds at viewport centre on drag or click, `⌘P` focuses, `Enter` adds); *Canvas* (dotted grid, nodes as 220px cards with type icon + label + one-line summary + status pill, edges as 2px bezier curves with arrowheads and port labels, selected edge deletable with `Del`, invalid drop targets refuse with a tooltip reason); *Inspector* (right, 320px: label, type-specific parameter form from the same schemas REQ-003 uses, expression fields with `{{ }}` autocomplete from upstream fields, port list with "Connected to …", per-node errors; with no selection it shows read-only run-as / rate limit / concurrency / error policy); *Problems panel* (bottom, collapsible: severity, node name, jump link, or "No problems"); *Status bar* (nodes/edges, graph version, last run status, unsaved dot).
- **Table mode** — the same graph as REQ-003's linear list plus a "connections" column, so the feature is fully usable without a pointer and reviewable in screenshots.
- **Keyboard map** — `V` select, `H` hand, `N` add node, `Space+drag` pan, `⌘Z`/`⇧⌘Z` undo/redo, `⌘S` save, `⌘Enter` run once, `⇧⌘Enter` arm, `⌘F` find node, `⌘D` duplicate, `⌘C`/`⌘V` copy/paste, `⌘A` select all, `Del` remove, `+`/`-` zoom, `0` fit, `⌘P` palette rail. Nodes are focusable: `Tab`/`Shift+Tab` cycle, `Enter` opens the inspector, arrows nudge 8px (`Shift` 40px).
- **Also on every screen** — empty, loading (skeleton) and error states with Retry; a blocked workflow explains itself on the overview rather than failing at run time.
- **Mobile (<1024px)** — list, overview and run detail work fully; the builder is read-only with pan/zoom and a "Editing needs a larger screen" banner that still offers Table mode (editable there).
- **Plugin nodes** — present in the palette only when the plugin is enabled for the organization, badged "Plugin: <name>" with a tooltip naming the provider.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/workflows` | List (existing, extended with trigger/node/status filters) | `workflows.read` |
| POST | `/api/v1/workflows` | Create (existing) | `workflows.manage` |
| GET | `/api/v1/workflows/{id}` | Read definition incl. graph and ui_state | `workflows.read` |
| PATCH | `/api/v1/workflows/{id}` | Arm/pause and metadata (existing route) | `workflows.manage` |
| PUT | `/api/v1/workflows/{id}/graph` | Replace graph + ui_state; requires `graph_version` (409 on mismatch) | `workflows.manage` |
| POST | `/api/v1/workflows/{id}/validate` | Validate the stored or posted graph, return findings | `workflows.manage` |
| GET | `/api/v1/workflows/node-types` | Node type registry (types, categories, ports, parameter schemas) | `workflows.read` |
| POST | `/api/v1/workflows/{id}/run` | Run once (existing) | `workflows.run` |
| POST | `/api/v1/workflows/{id}/run-from` | Run from a node (`{ "node_id": "…" }`) | `workflows.run` |
| POST | `/api/v1/workflows/{id}/listen` | Arm the one-shot test-event listener | `workflows.run` |
| GET | `/api/v1/workflows/{id}/listeners` | Active listeners and captured payload | `workflows.read` |
| GET | `/api/v1/workflows/{id}/executions` | Run history (existing) | `workflows.read` |
| GET | `/api/v1/workflow-executions/{id}` | Run detail with `node_id` per step (existing) | `workflows.read` |
| POST | `/api/v1/workflow-executions/{id}/retry-step` | Retry one node (REQ-003 endpoint, node-aware here) | `workflows.run` |
| POST | `/api/v1/workflows/{id}/duplicate` | Copy graph + ui_state as a paused workflow | `workflows.manage` |

No new permission keys: the builder writes through `workflows.manage` and runs through
`workflows.run`. Plugin-provided node types are additionally gated by `plugins.read` (to see them)
and the module's enablement for the organization.

### Data model

Migration `database/migrations/0014_workflow_graph.sql` (next free number if taken). It does not
touch columns REQ-003 owns (`input`, `on_error`, `timeout_ms`) — the two migrations merge into one
definition without claiming the same column twice.

| Change | Detail |
|---|---|
| `workflows` + `graph` | `jsonb not null default '{"nodes":[],"edges":[]}'`, check `jsonb_typeof(graph) = 'object'`; the API additionally validates node/edge shapes with a typed deserializer |
| `workflows` + `ui_state` | `jsonb not null default '{}'` — positions, viewport, collapsed groups, notes; never read by the engine |
| `workflows` + `graph_version` | `int not null default 1` — optimistic concurrency; a stale `PUT /graph` answers `409 graph_version_conflict` with the current definition |
| `workflows` + `validated_at`, `validation_error` | last validation result, so the list shows "invalid" without recomputing |
| `workflow_executions` + `graph_version` | the graph a run started with, so a trace always resolves its node ids |
| `workflow_steps` + `node_id text`, `branch text` | which node and output port produced the step; index `(execution_id, node_id)` |
| `workflow_test_listeners` (new) | id uuid pk, workflow_id uuid → workflows cascade, node_id text, token_hash text, created_by uuid → users set null, created_at, expires_at not null, consumed_at; unique `(token_hash)`; `(workflow_id)` where `consumed_at is null`; single use, 15-minute expiry |

Backfill happens in SQL so no workflow is left without a graph: existing rows get `graph`
synthesised from `steps` (trigger → one node per step → end) and `ui_state` with left-to-right
positions. Node *types* are code plus plugin manifests — deliberately not a table, so a plugin
upgrade cannot leave stale rows behind.

### Events

| Event | Kind | Notes |
|---|---|---|
| `workflow.graph.updated` | emitted | workflow, graph version, editor account — the audit trail of definition changes |
| `workflow.validation.failed` | emitted | findings count, first message — surfaced on the overview |
| `workflow.test_listener.armed` / `.consumed` | emitted | node id, expiry — explains an idle listener in the panel |
| `workflow.execution.progress` | emitted | batched at most once per 5s per run (never per node); the run view streams from `workflow_steps` and uses this only for cross-instance wakeups |
| `workflow.execution.completed` / `.failed` | consumed | the canvas repaints node status when a run settles |

All four are org-scoped and subscribable, but only `workflow.execution.*` (outside this request) is
meant for external consumers; the graph events exist so audit and the notification centre can react.

### Acceptance criteria

- [ ] The builder route, palette, canvas, inspector and problems panel render at `/workflows/[id]/builder` and appear in the QA walkthrough inventory.
- [ ] A node can be added from the palette by drag, by click and from the keyboard; it lands at the viewport centre and is selected immediately.
- [ ] Nodes move by mouse and by arrow keys, multi-select works (marquee, `Shift+click`, `⌘A`) and a group move keeps edges attached.
- [ ] A connection can be drawn between compatible ports; an incompatible target refuses with a visible reason; `Del` on a selected edge removes it.
- [ ] Validation finds each error class (cycle, two triggers, orphan, missing input, duplicate edge) naming the node involved, and a clean graph reports "No problems".
- [ ] Undo/redo restores add, move, connect, delete and parameter edits at least 50 steps deep, and `⌘S` during a pending autosave does not write twice.
- [ ] Two tabs on one workflow: the second save answers `409` and the UI offers Reload while keeping the local copy visible instead of overwriting silently.
- [ ] Saving the graph re-projects `steps` and the existing runner executes it end to end with no engine change.
- [ ] The backfill gives every pre-existing workflow a valid graph that opens in the builder without manual repair.
- [ ] "Run from here" on a mid-graph node starts a run whose first step is that node, earlier nodes stay `skipped`, and the trace says why.
- [ ] After a run each node shows its status pill, and clicking the node opens that step's inputs and output.
- [ ] "Retry this node" re-runs only that node without duplicating earlier side effects (proven with the mail sink).
- [ ] "Listen for a real event" captures a real bus event into the inspector within one matcher tick, and the listener expires after 15 minutes leaving no stray token.
- [ ] A plugin node appears in the palette with its badge when the plugin is enabled and disappears when it is disabled; a definition using it then reports an honest validation error instead of failing at run time.
- [ ] Table mode renders the same definition, edits parameters, and stays consistent with the canvas after a save in either mode.
- [ ] A keyboard-only pass adds two nodes, connects them, edits a parameter, validates and runs, with the pointer untouched.
- [ ] Below 1024px the builder is read-only with the banner, Table mode stays editable, and no control is unreachable.
- [ ] Empty, loading and error states exist on every screen; no dead buttons; `cargo test --workspace`, `pnpm typecheck && pnpm build` and the QA walkthrough are green with zero high findings.

### QA plan

The walkthrough must: open `/workflows`, create one from a template, open the builder, drag three
nodes from the palette, connect them, rename one, delete an edge and undo it, move two nodes with
the keyboard, run Validate on a deliberately broken graph, fix it, press Listen for a real event
and trigger that event from another screen, run once and open the resulting trace, click a node to
see its step, press Run from here and Retry this node, switch to Table mode and back, toggle the
minimap and zoom controls, and open the `⌘/` help. Then the mobile pass over the list, overview,
run trace and read-only canvas.

The visual check must see: node cards with even padding and unclipped labels, edges meeting their
ports (no floating arrowheads), selection distinguishable from focus, a grid subtle enough not to
fight the nodes, the inspector scrolled to its first field, readable status pills at 100% zoom, and
the problems panel not covering the last row of the graph.

### Slices

1. **Graph core** — migration with `graph`/`ui_state`/`graph_version`, backfill, node-type registry, graph write/validate endpoints, canvas shell (nodes, edges, pan/zoom, autosave, optimistic concurrency), Table mode parity, `steps` projection.
   *Done when:* a workflow created before this tick opens in the builder, is edited, saved and executed by the existing runner with no engine change.
2. **Interaction depth** — palette drag/click/keyboard, marquee and multi-move, snap and alignment, undo/redo, copy/paste, duplicate, delete, minimap, fit, auto-layout, inspector forms with expression autocomplete, problems panel with jump links.
   *Done when:* the keyboard-only acceptance pass works and undo/redo survives a reload of a saved graph.
3. **Run integration** — node status on the canvas, node↔step mapping, run-from-here, retry-this-node, trace deep links, batched progress event, audit entry.
   *Done when:* a mid-run status change paints on the canvas and a trace's node link resolves to the same step the API returned.
4. **Plugins and polish** — plugin node registry from manifests, sub-workflow node, notes, mobile read-only gate, all states, `⌘/` help, accessibility assertions.
   *Done when:* an enabled sample plugin contributes a node that runs through its declarative spec, and disabling it degrades with a clear validation error.

### Risks / notes

- Two representations of one definition is the sharpest edge here: the graph is authoritative, the
  `steps` projection is generated and never hand-edited, and a projection test compares what the
  runner sees against the graph on every save.
- The canvas is built in-house (DOM nodes, SVG edges, pointer events) to avoid a new build-time
  dependency and to keep keyboard and screen-reader behaviour under our control; adopting a canvas
  library later must keep the same `WorkflowCanvas` component boundary.
- Performance target: 200 nodes / 300 edges stay interactive — paint edges on one SVG layer, move
  nodes by transform only, and do not re-render the graph on selection.
- Positions are not semantics: a layout change must never mark the definition dirty for the engine
  or bump the version the runner reads.
- Autosave plus optimistic concurrency means the `409` path, not the timer, is the guarantee.
- Plugin nodes are untrusted input: schemas are validated on save, hosts are allow-listed at run
  time (REQ-003 applies), and no plugin value ever reaches a SQL string.
