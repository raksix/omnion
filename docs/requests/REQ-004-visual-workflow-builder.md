# REQ-004 — Visual Workflow Builder

> **Status:** in-progress (slice 3 · `633b620` (criterion 3), `9daeedb`, `26d8dbc`, `7de6e5d` (criterion 2, click half), `8f713fe`, `a021cb4` (criterion 2, pill half), `4b7df30`, `3a7ecc0`, `c355084`, `7bf3641`; slice 1 · `6c3f43b`, `775947a`, `f6d6a68`, `66442e9`, `b0dad65`; slice 2 · `c88273a`, `3739414`, `bc48938`, `c31bd75`, `c866c16`, `8611785`, `e14a5e1`, `0823f06`, `3b4cbfd`, `671b43c`, `c864ca1`, `0c9ee98`, `1814758`, `afbb92b`, `c72bcf5`, `314bcda`, `6222873`, `d995669`) · **Captured:** 2026-09-25 · **Layer:** `apps/admin` + `crates/workflows`
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

- [x] The builder route, palette, canvas, inspector and problems panel render at `/workflows/[id]/builder` and appear in the QA walkthrough inventory. — slice 1: `WorkflowBuilder` at `/workflows/[id]/builder`; `runWorkflowBuilderDepth` opens a real rule's builder and records `panes` (all four true), `paletteNodes` and `canvasNodes`.
- [x] A node can be added from the palette by drag, by click and from the keyboard; it lands at the viewport centre and is selected immediately. — all three routes proven in a browser this tick: `palette-add` (click, 2 to 3 nodes on a rule born with a trigger), `palette-keyboard-add` (4 to 5, the palette focused by the keyboard first) and `palette-drag` (`nearDropPoint: true` — the assertion measures where the card landed against the drop point, so a handler that ignored the viewport could not pass it).
- [ ] Nodes move by mouse and by arrow keys, multi-select works (marquee, `Shift+click`, `⌘A`) and a group move keeps edges attached. — **one of the two reported defects did not exist, and the other was a symptom of the same design.** Selection was one piece of state with five writers spread over two `useState` calls and a ref; the whole rule now lives in `selection.ts` (20 tests) and every gesture routes through it (`1814758`, `afbb92b`). `escape-clears`'s `stillSelected: 3` was a *probe* reading `style.includes("outline")` — React writes `outline: none` on every card, so the count was always the total and the product was never wrong (`c72bcf5`). Three real defects surfaced while routing: the shift-click toggle could not deselect (pointer-down re-focused the node it had just removed), a delete left a phantom focus that kept Duplicate enabled, and the arrow-key nudge moved only the ref's nodes when the selection lived in the focus. **Not ticked until the browser pass reads `escape-clears.cleared: true` and `shift-click-multi.ok: true` off the new marker.**
- [ ] A connection can be drawn between compatible ports; an incompatible target refuses with a visible reason; `Del` on a selected edge removes it. — **the draw gesture works now, and the reason it did not is why the pass was worth reading.** `connect()` looked the source *node id* up in the node-*type* map, so no node could ever be connected and every attempt refused with a confident "has no output ports, so nothing can leave it" (`3b4cbfd`, `671b43c`). The rules moved to `connect-edge.ts` and are tested (9 cases); the browser reads `tone: ok, "Wait · Next → Transform"` for a real connection and a named refusal ("Event · Next already leads to that node.") for a duplicate. **`Del` on a selected edge was a probe defect too, and of the same shape.** `locator.click()` aims at an element's bounding box, and a bezier's box is the rectangle *around* the arc — its centre is empty canvas. The click fell on the desk, the canvas handler cleared the selection (correctly), and the pass reported "no edge could be selected", which is indistinguishable from a dead product. The point now comes from `getPointAtLength` at the stroke midpoint, mapped through `getScreenCTM` so the viewport's pan and zoom are included, and the note reports `removed` (the *server's* edge count fell) rather than the canvas count. **Not ticked until the pass reads `edge-delete.removed: true` and `edge-delete-undo.restored: true`.**
- [ ] Validation finds each error class (cycle, two triggers, orphan, missing input, duplicate edge) naming the node involved, and a clean graph reports "No problems". — **the server already found all five, by name, and none of them had ever been run through the pass.** The one probe pushed a second trigger onto the live graph, so four classes and the clean case were untested — and one deliberately broken graph could not have answered it anyway: the classes overlap, one masks another, and a panel rendering only the first finding reports the same codes as one rendering them all. Each class now gets its own graph, built from a valid spine (`event → action → end`) with exactly one defect, plus that spine alone as a control. **Not ticked until the pass reads `validate-classes` with the control clean and five `found: true`.** Building the spine by hand is where this nearly went wrong a second time: the first draft wired the port key `next` into both ends, and the registry has never had one — a trigger exports `out`, an action `success`/`error` — so `unknown_source_port` would have fired on all six cases at once and the five codes could never appear. The table would have reported the validation as broken when the probe was wrong.
- [ ] Undo/redo restores add, move, connect, delete and parameter edits at least 50 steps deep, and `⌘S` during a pending autosave does not write twice. — **the `⌘S` half did not exist, and the obvious way to add it would have made the criterion false.** There was no `⌘S` handler at all, so the browser offered its own save-the-page dialog to an author with unsaved work. Wiring that key straight to `persist` leaves the debounce armed, and one keystroke then writes twice: `graph_version` advances twice and a second tab is handed a conflict no author caused. The second race is the one that loses data — a press while a write is on the wire starts a second request quoting the same version, and whichever loses the database race is refused as a conflict the author manufactured by pressing the key that was meant to help. **Order is the rule:** in-flight is checked first, because a request that has left is the only one that can still collide on the version column, while an armed debounce has written nothing and can simply be cancelled. A joined press still shows "saving", because a key that visibly does nothing is indistinguishable from a broken one and gets pressed again. `arbitrateSave` is a pure function for the same reason `conflict.ts` is: a rule about whether a write may start cannot be tested inside a React callback — 5 tests, and reverting the check order (the classic version of this bug) turns one red (`391b4c6`). **Not ticked until the pass reads `cmd-s-writes-once` with `wroteSomething: true` and `settledSame: true`** — the criterion is invisible on the screen, since a second write lands while the indicator still reads "saved", so the probe reads the version three times and only the last one proves it (`69084d5`).
- [ ] Two tabs on one workflow: the second save answers `409` and the UI offers Reload while keeping the local copy visible instead of overwriting silently. — **the server half was right and the client half was a dead end.** `replace_graph` refuses a stale version and names the one it holds, and the `conflict` probe proved that with a raw `fetch` — which never touches the toolbar, so the half the criterion is actually about (the UI's answer) was never measured. Two defects underneath. **First: only one of the two exits existed.** The server's sentence offers "reload to see their change, or keep editing to overwrite it"; the client rendered Reload and nothing else, and after a 409 `versionRef` stayed at the value the tab loaded, so *every* later PUT quoted a version one behind and was refused again. The banner described a dead end: the only way to write anything was to discard the author's own work. `conflict.ts` now owns both questions (which version the server named, what the next save may quote) and the toolbar offers a second, confirmed exit that re-bases on the version the **server** named — never a locally derived `versionRef + 1`, which would be last-write-wins wearing the costume of a guard (`314bcda`). **Second, a guard that could be made to fail open:** `at version 7.5` parsed as `7` (the regex took the integer prefix), and a client quoting 7 is an overwrite that the guard refused. The trailing boundary closes it; 7 tests, and each of the two dangerous rules reverted to prove it goes red (`6222873`). **Not ticked until the pass reads `two-tab-conflict.refused`/`reloadOffered`/`localNodesKept` and `two-tab-keep-mine.resolved`** — the second of those is the reading that would have caught the dead end, since a save quoting a stale version comes back as a *second* conflict (`d995669`).
- [x] Saving the graph re-projects `steps` and the existing runner executes it end to end with no engine change. — slice 1: `graph::project` is the single projection path, `graph_store::project_steps` runs it in the same statement as the write, and the walkthrough reads `projection.valid` / `projection.step_count` back from the server.
- [x] The backfill gives every pre-existing workflow a valid graph that opens in the builder without manual repair. — and the gap the backfill left is closed: the column default was an *empty* graph, so every rule created after 0051 opened blank and refused its first save (`c864ca1`). `insert_workflow` now seeds `Graph::starter`, proven by a real insert plus the real validator (`0c9ee98`): the walkthrough reads `canvasNodes: 2` and `projection.valid: true` on a rule it created seconds earlier, where the same pass read `0` and `graph_invalid` before.
- [ ] "Run from here" on a mid-graph node starts a run whose first step is that node, earlier nodes stay `skipped`, and the trace says why. **The whole criterion is implemented, and none of it is expressible without a sixth step state** — a run's steps are all `pending` at creation and the engine claims them strictly in `step_no` order, so there was no way to say "these two did not run" (`4b7df30`). The finding worth keeping is that `skipped` cost **no engine change at all**: `claim_due_step` reads only `('pending','waiting')`, `settle_execution` counts open as `('pending','running','waiting')` and failures as `= 'failed'`, and `retry_step_from` re-opens four statuses. A state that had to be threaded through them would have been the tell that the schema was not ready. The plan is made against a new `graph::project_walk` rather than against the step list, because the two disagree in three places and **each one decides whether a node is startable at all**: a trigger contributes no step (and re-running a rule from the top is a real thing an operator wants), an end node contributes a `stop` (starting there leaves every other step pending forever), and a note is inert. A planner indexing the step list needs a special case per case, and a special case is where an off-by-one lives — **which is the first thing I wrote**: `position() + 1` made the clicked node itself skipped, and the assertion that caught it checks the clicked node's own `step_no`. The prefix is inserted as `skipped`, never inserted pending and updated after, because a crash between the two writes leaves a run whose prefix the engine is about to execute — the exact side effect the feature exists to prevent. The endpoint (`3a7ecc0`) is guarded by `workflows.run` and not `workflows.manage`, and the panel (`c355084`) offers the button wherever a run can start and disables it **with a stated reason** where it cannot. The test reads the **stored rows**, not the response body, because a handler that returned a plan-shaped payload while writing a full run would pass a body-only test and the criterion is about what runs: step 1 `skipped`, step 2 not, the reason on step 1 naming the node, the run still settling `completed`, and the skipped step holding **zero attempts** — which is what proves the engine never claimed it. **Not ticked until the pass reads `run-from-here` with `skipped > 0`, `reasonNamesNode: true` and `firstRunnableNo === firstSkippedNo + 1`** (`7bf3641`). The probe reads the run back through the API after the press, because a toast that says "Run started" proves the button was pressed and nothing else — and the failure it would miss is a run that quietly executed the prefix, reported like any other run.
- [ ] After a run each node shows its status pill, and clicking the node opens that step's inputs and output. — **the pill half is built; the click half is not, and the reason the criterion was unprovable was not missing code.** The engine had written `node_id` and `skip_reason` for two ticks and the canvas had nothing to paint with: `StepBody` carried a step's `status` and not the node it came from, and `ExecutionSummary` carried no `started_from_node`. All three fields were stored correctly and none of them was on the wire, so the probe had been reading `null` for everything it asked about — the criterion was *unprovable* rather than unmet (`a021cb4`). The mapping is a pure function (`node-status.ts`) for the reason `conflict.ts` is, and its three load-bearing rules are asserted: a node with **no step paints nothing** (a pill on a node the run never reached is a claim about work the engine never did, and a note is decoration, not work), a node whose **branches disagreed** is `diverged` rather than whichever branch ran (a `Map` keyed by node would keep whichever row the API returned *last*, so the answer would depend on row order), and steps with **no `node_id`** are dropped rather than bucketed under an empty key (a rule predating the builder has no node, and index-attribution is a guess that paints the first card on the canvas). The pill is fed from the *response body* on purpose: reading the run's rows back from the API would pass even if every card rendered nothing, so the probe now reads the canvas and compares the painted node set against the run's. 10 unit + 85 builder tests. **Not ticked until the pass reads `pillsPainted > 0` with `paintedButNotInRun` empty.**
- [ ] *Criterion 2, second half — clicking the node opens that step's inputs and output.* **BUILT, not ticked** (`7de6e5d`, `26d8dbc`, `9daeedb`). The gap was on the wire, not in the client, and it is the **same class of gap as the pill half one tick earlier**: `StepBody` carried `output` and not `params`, so the panel had one side of each step and nothing to show for the other. A stored `params` is **not** the node's authored `params` on the canvas — a run from a node, a retry, or an edit that was never saved leave the two different — so reusing `node.params` in the panel would have compiled, rendered, and quietly been the wrong number. `step-detail.ts` is a pure function and its three rules are exactly what a `steps.find(s => s.node_id === id)` throws away: a node with two branches opens **both** steps (showing the branch that ran and hiding the one that did not is the information the `diverged` pill exists to advertise); `null` ("no run read") is not `[]` ("a run with no steps"), because a rule whose first run is still `pending` has steps and collapsing the two makes a rule that has never run look like one whose nodes all sat out; and an **absent** payload is not an **empty** one, so the server now sends `{}` rather than omitting the key and a Rust test asserts the empty object *is* sent. Payloads are classified before they are rendered and never stringified: `JSON.stringify` throws on a cyclic value, and it throws during render, which takes the panel with it. The probe reads the **panel**, not the run — fetching `step.output` would pass against a trace that rendered nothing — and clicks a node read off a *painted, non-skipped* card, because clicking a node the run never reached proves the empty-state message instead of the panel. **Not ticked until the pass reads `step-trace` with `panelFound: true`, `kind: "node"`, `stepsShown ≥ 1`, both `inputsRendered` and `outputRendered` resolved, and `stepsWithParams === stepsTotal > 0`.**
- [x] "Retry this node" re-runs only that node without duplicating earlier side effects (proven with the mail sink). — **built and proven, and the walk corrected two of my own decisions rather than confirming them** (`633b620`). The criterion names its own instrument, and it is the right one: a run whose first step re-runs is *indistinguishable* from one that did not on any status column, so the walk asserts a **mail count** (the sink, built here for this walk) rather than a status comparison. It is deliberately **not** a narrowed `retry_step_from` — that write re-opens `step_no >= N` on purpose, because a run whose middle failed must not march on to completion with a hole in it, and reusing it here would re-send the earlier e-mail. The one row is its own write, and the walk asserts the returned count, so a later widening of the `WHERE` fails the walk rather than quietly repeating a side effect.

  **Two decisions the walk overturned, which is the worth keeping:**
  1. **A plain `run` never stamped `node_id` at all.** The per-node status layer was empty for the *most common* way to start a run — no pills, no click target, and a retry answering "took no part in this run" on **every card**. The walk was written for retry and found a defect that was not in retry: attribution belongs to every run, so `graph_store::attribute_steps_to_graph` is now called from the plain-run path too, and is **best-effort** — it is a decoration and must never fail a run that has already started.
  2. **The attempt counter is reset, and my first draft was wrong about that.** The reasoning was "one node is not a new budget", and the database refused the write: `workflow_steps_attempts_shape` caps `attempts` at `max_attempts` and `claim_due_step` *increments* on claim, so a re-queued step that had spent its budget produces a row the engine is **forbidden to claim**. The retry would have been accepted, audited, and then never run — a control whose stated purpose is "try this again" that cannot try again. Bounded by the step's own `max_attempts`, not unbounded.

  `retry_node::plan_retry_node` is a pure function because a branching node is **two rows**: a `find` reports "nothing to retry" on the very node the canvas paints red, and its four refusals are four sentences (succeeded · not-in-run · run-live · run-cancelled) because only the last two are about the run. **Not ticked on the browser pass** — the control is in the inspector with a stated reason on every refusal, and the probe for it has not run.
- [ ] "Listen for a real event" captures a real bus event into the inspector within one matcher tick, and the listener expires after 15 minutes leaving no stray token. — **BUILT and server-proven; the click half is unticked** (`fb3e8d3`, `711a32c`, `7cdcdad`, `9892c91`, `f2483c5`). The criterion is three claims and the interesting part is that **the existing listener could not answer any of them without becoming a different thing.** REQ-003's `automation_test_events` looked like the smaller change and is rule-shaped with no expiry and no token: a rule-level capture shows what arrived on the *bus*, and "what would *this* node receive" is the payload its **upstream** produced — a different question. So this is a new table, and the naming of that is the design.
  **The expiry is the half that is easy to get wrong and nothing on the screen would show it.** "Armed" reads most naturally as "not yet consumed", and a matcher filtering on `consumed_at is null` alone fills a row whose fifteen minutes ran out and reports a capture for an event **nobody was watching**. `listener_is_live` is the single definition — `consumed_at is null AND expires_at > now()` — asserted from both sides of a closed boundary, because the predicate is hand-written in three places (the partial index, the `UPDATE`, the sweeper) and three copies is three chances to disagree. **The read never filters by state either**: an expired row that vanished is indistinguishable from one that was never armed, and only the second is something the author can act on.
  **The walk is shaped so each clause fails loudly if it stops being true.** "Within one matcher tick" is *exactly one* `matcher::drain` — a second drain would still pass a `captured_at` check, and a listener needing two ticks is a broken one. "No stray token" is asserted against **the matcher's own predicate**, not a row count: a real event is driven through the real matcher at an expired row and then the walk asks whether a live-listener query still sees it. Only that separates "the row was deleted" from "the matcher cannot see it", and the second is the criterion.
  **Three of the walk's own first drafts were wrong, which is the part worth keeping.** (1) It pinned `expires_in_seconds` to 900; the server sends 899, because the number is `whole_seconds()` of a window that began microseconds before the read — so the assertion was checking a rounding rule, and the *window* is the gap between the two timestamps. **The test was wrong, not the code.** (2) It expired a row by back-dating `expires_at`, and **the migration's own `expires_at > created_at` constraint refused it** — a real sweeper would hit the same wall, so a constraint written for correctness is also a constraint on how time may be simulated. (3) It expected 404 for another tenant's rule and got 403 `cross_organization`; a 404 would claim the id is unknown, which this API does not do on any scoped surface. Each was corrected *toward* the platform's real guarantee rather than by loosening the test.
  The panel is in the **inspector above the node editor**, not inside it: it is a property of the rule and the selection, so nested in the inspector it would vanish the moment the author clicked the desk — which is exactly when they want the payload. Its countdown is recomputed from the expiry instant every second and takes the **smaller** of that and the server's number, because a local counter drifts while a tab is backgrounded and then says "12 minutes left" about a listener that expired four minutes ago. **Not ticked until the pass reads `listener` with `captureRendered: true`, `payloadRendered: true`, `windowSeconds: 900` and `tokenReturnedOnRead: false`.**
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

   **Shipped 2026-09-28** (`6c3f43b`, `775947a`, `f6d6a68`, `66442e9`, `b0dad65`).
   `0056_workflow_graph.sql` (renumbered this tick — see below), `crates/workflows/src/graph.rs` (Graph/Node/Edge, the 13-type
   `NODE_TYPES` registry with ports and parameter schemas, `validate` → findings, `project` →
   the step list), `graph_store.rs` (`replace_graph` carrying the version in its WHERE clause,
   `replace_ui_state` that bumps neither version nor steps), four endpoints, and the builder
   workspace. **Still open in this slice:** the backfill criterion is proved by a migration
   check rather than by the browser pass, and Table-mode parity (`/workflows/[id]`'s own tab)
   lands with slice 2 — the existing linear editor is reachable from the builder's toolbar.

   **In flight 2026-09-28** (`c88273a`, `3739414`, `bc48938`). `builder-history.ts` is a snapshot
   history with a coalescing window, so a drag, a re-type and a three-node delete are each one
   press; wired to the toolbar (Undo/Redo/Duplicate/Copy/Paste/Auto layout/Minimap), to
   Ctrl+Z / Ctrl+Shift+Z / Ctrl+Y / Ctrl+C / Ctrl+V / Ctrl+D / Escape, and to a marquee that
   moves a whole selection. `bc48938` closed the four gaps that were named as open: `⌘A`,
   `Shift+click`, edge selection with `Del` (now routed through `commit`, so an edge delete is
   undoable like every other change) and the palette keyboard route. 12 unit tests on the
   history; `pnpm typecheck` clean; `cargo check -p omnion-api --all-targets` clean.
   **Still open in this slice:** the browser pass, which is what the four new gestures need —
   a pass must watch `⌘A` highlight, `Shift+click` reach two cards, the palette add by keyboard
   and the edge delete fall in the server's `edge_count`. Then palette *drag*-and-drop, the
   incompatible-target refusal with a visible reason, and the expression autocomplete the
   inspector does not have yet.
2. **Interaction depth** — palette drag/click/keyboard, marquee and multi-move, snap and alignment, undo/redo, copy/paste, duplicate, delete, minimap, fit, auto-layout, inspector forms with expression autocomplete, problems panel with jump links.
   *Done when:* the keyboard-only acceptance pass works and undo/redo survives a reload of a saved graph.
3. **Run integration** — node status on the canvas, node↔step mapping, run-from-here, retry-this-node, trace deep links, batched progress event, audit entry.

   **In flight 2026-09-29** (`4b7df30`, `3a7ecc0`, `c355084`, `7bf3641`). *Run from here* is
   complete on the server, in the store and in the panel; 95 unit + 14 integration + 61 builder
   tests, all green against a real Postgres. `0122_workflow_run_from_node.sql` adds the sixth
   step state, the run's `started_from_node` and each step's `skip_reason` — plus two
   constraints that make the shape impossible to get wrong: a `skipped` step cannot also carry
   a start time, an output or an error, and a `skipped` step **cannot exist without a reason**
   (the criterion's "the trace says why" is a database invariant, not a code convention).
   Migration number is 0122, past every branch's high-water (0120 on wave5) — the shared
   numbering namespace again, so the number was read off the remotes rather than chosen from
   this branch's own tail.
   **Criterion 2 (node status pills) 2026-09-29** (`8f713fe`, `a021cb4`). The pill half is
   built; the *click* half is not. The blocker that made it unprovable was on the read
   path, not the write path — `node_id` and `skip_reason` were stored and never sent.
   **This branch cannot run the DB integration tests at all:** it is missing migrations
   `0019` and `0022` (the sequence jumps `0018 → 0020 → 0021 → 0023`), so every
   `OMNION_REQUIRE_DB=1` test dies at `VersionMissing(19)` before reaching any assertion.
   The gap is present at this tick's baseline commit, and the numbers are owned by
   `origin/wave2-cms` (0019) and `origin/wave4`/`origin/wave7` (0022) — none merged into
   `main`. Renumbering is not this worktree's to do; it needs the owner to land the two
   migrations or run a cross-branch reconciliation.

   **Criterion 3 (*Retry this node*) 2026-09-29** (`633b620`). Complete on the server, in
   the store and in the inspector, proven with the mail sink against a fresh database. The
   note above predicted the trap correctly — `retry_step_from` is a **tail** re-run and a
   single-node retry had to be a different write — and the walk then found two things the
   prediction did not cover: plain runs carried no `node_id` at all, and the attempt
   counter could not be preserved. Both are written up in the criterion's own box above,
   because the second one is the more interesting half: a constraint, not a test, is what
   corrected the design, and a button that re-queues a row the engine may never claim is a
   button that reports success and does nothing.

   **Still open in this slice:** the *click* half of criterion 2 (clicking a node opens
   that step's inputs and output — the mapping already carries `stepNos` for it; the code
   is built and unticked for want of the browser pass), the real-event listener
   (criterion 5) and Table-mode parity (criterion 8).
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
