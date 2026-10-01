# REQ-004 — Visual Workflow Builder

> **Status:** in-progress (tick 60 · **the `step-trace` row's TARGET was derived from the row's own SUBJECT, so a pill regression made the gate VOID instead of red — the tick-57/58/59 defect for the fourth time and the worst instance** · 331 admin tests (+3), pnpm typecheck 2/2, `node --check` clean (14,451 → 14,515 lines), **9/9 mutations red** and the table-mode harness back to **8/8** · the row chose which node to click by asking the canvas for a painted status pill — `painted`, which is the `run-from-here` row's own read and the OTHER half of the same criterion — so when the pill regressed no card carried one, the id was `null`, no click was issued, the inspector never mounted, and `stepsWithoutBothSides: []` + `stepsInRunButNotShown: []` + `stepsWithParams === stepsWithOutput === stepsTotal > 0` (read off the wire) were **all green with the panel shut**; `panelFound: false` and `stepsShown: 0` were in the note and neither was in the conjunction, so the next tick's conjunction would have closed the criterion on a panel that had never opened · 57–59 each moved a read onto the screen without checking the screen could be put into that state, which left an *unsatisfiable* gate (red forever on correct code); this one leaves a *vacuous* gate, and vacuity prints the same digits as success — a gate a defect can make invisible is worse than a missing one · the target now comes from `after.steps` and is intersected with the cards the canvas drew, so a pill regression surfaces where it belongs (`pillsPainted: 0` / `inRunButNotPainted` on the row that measures it) and a null target beside a populated run reads as a graph/canvas divergence rather than a panel that failed to open · the click now waits on the panel's own `[data-step-trace]` marker (unconditional at the root of all three of its states, builder-view.tsx:3507) instead of `waitForTimeout(500)`, which was green against a page that had not drawn · note carries `rowIsMeasurable`, `targetFromRun`, `pillChosenNode`, `pillChoseSameAsRun`, `paintedOnTarget` · **one of my own new guards violated the lesson in its own header**: it spanned statements, so the *evidence* line below the declaration satisfied a claim about the declaration — scoped to the single `const paintedNodeId = …;` now · **NOT ticked**: no browser pass — the slot's holder is a LIVE w6 pass (pid 252267, `/proc/252267/cwd` = `/mnt/apopic/omnion-w6`, 400 files written in the four minutes before this tick's checks, 65 Chrome) · previous: in-progress (tick 59 · **the table-mode read was honest and still impossible — `NodeInspector` renders under `{selectedNode ? … : null}` and the row selected nothing** · 328 admin tests (+2), pnpm typecheck 2/2, **five mutations red** each naming its assertion · third instance of the same defect, and the same one: the read was moved off the wire but never made *possible*
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
- [x] Nodes move by mouse and by arrow keys, multi-select works (marquee, `Shift+click`, `⌘A`) and a group move keeps edges attached. — **TICKED 2026-09-29 (tick 23, pass `20260929-143037`).** The pass read `escape-clears: {cleared: true, stillSelected: 0}` and `shift-click-multi: {ok: true, expected: 2, selected: 2}`, plus `select-all {total: 3, selected: 3}` and `palette-drag {nearDropPoint: true}`. The three earlier deflections are now measured, not argued: the probe's `style.includes("outline")` never saw a count, and the product was never wrong. — **one of the two reported defects did not exist, and the other was a symptom of the same design.** Selection was one piece of state with five writers spread over two `useState` calls and a ref; the whole rule now lives in `selection.ts` (20 tests) and every gesture routes through it (`1814758`, `afbb92b`). `escape-clears`'s `stillSelected: 3` was a *probe* reading `style.includes("outline")` — React writes `outline: none` on every card, so the count was always the total and the product was never wrong (`c72bcf5`). Three real defects surfaced while routing: the shift-click toggle could not deselect (pointer-down re-focused the node it had just removed), a delete left a phantom focus that kept Duplicate enabled, and the arrow-key nudge moved only the ref's nodes when the selection lived in the focus. **Not ticked until the browser pass reads `escape-clears.cleared: true` and `shift-click-multi.ok: true` off the new marker.**
- [ ] A connection can be drawn between compatible ports; an incompatible target refuses with a visible reason; `Del` on a selected edge removes it. — **the draw gesture works now, and the reason it did not is why the pass was worth reading.** `connect()` looked the source *node id* up in the node-*type* map, so no node could ever be connected and every attempt refused with a confident "has no output ports, so nothing can leave it" (`3b4cbfd`, `671b43c`). The rules moved to `connect-edge.ts` and are tested (9 cases); the browser reads `tone: ok, "Wait · Next → Transform"` for a real connection and a named refusal ("Event · Next already leads to that node.") for a duplicate. **`Del` on a selected edge was a probe defect too, and of the same shape.** `locator.click()` aims at an element's bounding box, and a bezier's box is the rectangle *around* the arc — its centre is empty canvas. The click fell on the desk, the canvas handler cleared the selection (correctly), and the pass reported "no edge could be selected", which is indistinguishable from a dead product. The point now comes from `getPointAtLength` at the stroke midpoint, mapped through `getScreenCTM` so the viewport's pan and zoom are included, and the note reports `removed` (the *server's* edge count fell) rather than the canvas count. **Not ticked until the pass reads `edge-delete.removed: true` and `edge-delete-undo.restored: true`.**
- [ ] Validation finds each error class (cycle, two triggers, orphan, missing input, duplicate edge) naming the node involved, and a clean graph reports "No problems". — **the server already found all five, by name, and none of them had ever been run through the pass.** The one probe pushed a second trigger onto the live graph, so four classes and the clean case were untested — and one deliberately broken graph could not have answered it anyway: the classes overlap, one masks another, and a panel rendering only the first finding reports the same codes as one rendering them all. Each class now gets its own graph, built from a valid spine (`event → action → end`) with exactly one defect, plus that spine alone as a control. **Not ticked until the pass reads `validate-classes` with the control clean and five `found: true`.** Building the spine by hand is where this nearly went wrong a second time: the first draft wired the port key `next` into both ends, and the registry has never had one — a trigger exports `out`, an action `success`/`error` — so `unknown_source_port` would have fired on all six cases at once and the five codes could never appear. The table would have reported the validation as broken when the probe was wrong. — **MEASURED 2026-09-30 (tick 37, pass `20260930-132859`): four of five found, the cycle is NOT.** `validate-classes` reads `clean: {valid: true, codes: []}` — the control graph is clean, so the panel is not simply reporting a finding for every graph it is handed — and then `twoTriggers.found: true` naming ""Event t1" is a second trigger — a definition starts from exactly one", `orphan.found: true` naming ""Act stray" is not reachable from the trigger — it would never run", `missingInput.found: true` (codes ['missing_parameter']) naming ""Event t1" needs Event name (event)", and `duplicateEdge.found: true` naming ""Event t1" → "Act a1" is connected twice on the same port". **`cycle.found` is `false` with an empty `names`** — the one class the criterion names that the server did not report, and it is not reported as an error either: the graph it built for the case came back clean. The criterion asks for five, the pass produced four, so it stays unticked: a probe that cannot build a cycle has not shown the validator cannot find one. **— THE FIFTH WAS A REAL DEFECT, AND THE PROBE WAS RIGHT (`1d7213df`, tick 38).** The graph the probe built for the case was a correct loop, and the server had always been able to say so — `find_cycle` followed ONE target per node via `find_map`, which walks a *list* correctly and a *graph* wrongly. Any node with two leaves (a condition, a switch, an action with an `error` branch) was followed one way only, so a ring closing on the *other* port was invisible to both `validate` and `project`: the rule was stored, listed as valid, and hung the first time the retry branch fired. The walk now carries a cursor per stack frame, so a node with several children is visited once and then resumed rather than restarted from each child, and the ring it reports is the one that actually closes. The case stayed its own row (`cycleOnABranch`, a condition whose `false` points back at the trigger) precisely because folding two loop shapes into one row would hide the one that broke — the same reason the original probe was split per class. Proof: the unit test was written first and failed against the old traversal; reverting the fix turns exactly that test red and nothing else; 147 lib tests green. — **MEASURED 2026-09-30 (tick 42, pass `20260930-185148`): all five classes found, and TWO OF THE FIVE ROWS WERE MEASURING A CASE THE SERVER NEVER SEES.** `validate-classes` reads `clean: {valid: true, codes: []}` — the control is clean, so the panel is not simply reporting a finding for every graph it is handed — and then five rows with `found: true`: `twoTriggers` ("Event t1" is a second trigger), `orphan` ("Act stray" is not reachable from the trigger), `missingInput` (`missing_parameter`, "needs Event name"), `duplicateEdge` ("Event t1 → Act a1 is connected twice on the same port") and `ambiguousBranch` with **`orderIndependent: true`** off the reversed pair, which is the half that proves the verdict is a property of the graph rather than of array order. **The cycle is where the reading turns.** Both cycle rows carried a SECOND defect, and the pass recorded it in the note without anybody reading it: `cycleOnABranch` reads `codes: ["unknown_node_type", "graph_cycle"]` with `names` = "condition.if is not a node type the platform knows" — the probe built `condition.if` and the registry key is `condition`, so `found: true` was green off the codes while the sentence shown was about a node type the author never typed. And `cycle` read `found: true` with an **ambiguity** message: `spine()` already put an edge on `a1`'s `success` port, so closing the ring from the same port left two edges on one walked port and `ambiguous_branch` sorted ahead of `graph_cycle`. **That row reported the wrong class for three ticks, and one defect per graph is the entire reason this table was split per class.** Both rows are rebuilt from the registry and the `cycle` row now records its whole code list (`4f52d634`, `16e58d1c`), with a Rust test that builds the same graphs and asserts each carries EXACTLY one defect class — writing that test took three runs, because the first two fixtures had a `missing_parameter` of their own, which is the assertion catching a probe nobody had read. Reverting the type back to `condition.if` turns it red on `["unknown_node_type", "graph_cycle"]`, the exact pair this pass recorded. **STILL UNTICKED:** the corrected rows have not been through a browser pass yet — the measurement above is of the BROKEN probe, and the fix is unit-proven but not browser-proven. — this tick changed the traversal and the probe, and only the unit gate answered. What the four DO show is that one defect per graph is enough to surface it — the reason the single-broken-graph probe could never have answered this, since the classes mask one another. `validate-broken` also answers `status: 200, valid: false, errorCount: 3` with `namesNode: true`, and `problems-panel` renders the finding on screen rather than only in the response body.
— **TICK 25: the second wrong thing with the edges was one field.** `Edge` requires an `id` and is `deny_unknown_fields`, so `{source, source_port, target}` was rejected at the deserializer and all six cases came back `valid: null` with an empty `codes` list — which the note read as "the validator found nothing" when the request had never reached the validator. Ids are derived from the endpoints now, and the duplicate case carries two **different** ids for one port pair, because identity is the edge's own while the `(source, port, target)` triple is what the validator calls a duplicate (`b227846`). Still unticked: the pass that would measure it has not finished.
- [ ] Undo/redo restores add, move, connect, delete and parameter edits at least 50 steps deep, and `⌘S` during a pending autosave does not write twice. — **the `⌘S` half did not exist, and the obvious way to add it would have made the criterion false.** There was no `⌘S` handler at all, so the browser offered its own save-the-page dialog to an author with unsaved work. Wiring that key straight to `persist` leaves the debounce armed, and one keystroke then writes twice: `graph_version` advances twice and a second tab is handed a conflict no author caused. The second race is the one that loses data — a press while a write is on the wire starts a second request quoting the same version, and whichever loses the database race is refused as a conflict the author manufactured by pressing the key that was meant to help. **Order is the rule:** in-flight is checked first, because a request that has left is the only one that can still collide on the version column, while an armed debounce has written nothing and can simply be cancelled. A joined press still shows "saving", because a key that visibly does nothing is indistinguishable from a broken one and gets pressed again. `arbitrateSave` is a pure function for the same reason `conflict.ts` is: a rule about whether a write may start cannot be tested inside a React callback — 5 tests, and reverting the check order (the classic version of this bug) turns one red (`391b4c6`). **Not ticked until the pass reads `cmd-s-writes-once` with `wroteSomething: true` and `settledSame: true`** — the criterion is invisible on the screen, since a second write lands while the indicator still reads "saved", so the probe reads the version three times and only the last one proves it (`69084d5`) — **TICK 50: THE UNDO HALF WAS MEASURED AND GREEN, AND A DEFECT SAT RIGHT NEXT TO IT (`766549ef`).**
**TICK 51: TWO GESTURES SHARED ONE PRESS OF UNDO, BECAUSE THE KEY NAMED THE ACTION
(`57d20ebe`, `eacd5fa9`).** The history merges two entries when their coalesce keys match
inside `COALESCE_MS`, keeping the *first* `before` with the *second* `after`. That is right
for a drag — one change per pointer frame — and wrong for two different actions, so the key
has to answer "are these the same gesture?", and only the **subject** can. Four call sites
already named theirs (`add:${id}`, `remove:${ids}`, `edit:${id}:${fields}`,
`move:${ids}`) and four named only the action (`"nudge"`, `"edge-add"`, `"edge-remove"`,
`"paste"`, `"auto-layout"`). **Both groups were green, and the reason is the point:** the
history module's own tests only ever used keys that were *already* subject-bearing, so a key
that merged unrelated gestures was indistinguishable from one that did not.

Two defects, both reachable with the keyboard alone — no second tab, no conflict:

- **Nudge** a card right, select the next, nudge it right. Two presses of the arrow key well
  inside the 600ms window, and one press of undo silently reversed **both**.
- **Wire a chain.** Connect `a → b`, then 150ms later `b → c`, and one undo removed **both**
  edges, leaving a `b` nothing reaches. A chain is built one connection at a time, so this is
  not exotic timing — it is how a rule gets wired, and the *graph* is what goes wrong.

`gesture-key.ts` now owns every key, so the rule has one definition instead of eight call
sites. `dragKey` delegates to `moveKey` rather than restating it — a drag of `[a,b]` and a
nudge of `[a,b]` are one subject, which is precisely what lets the drag's `sealGroup` be the
gesture boundary instead of the key. Each key keeps what must still merge (holding an arrow
key on one card, re-typing one field), and connect-then-delete stays **two** entries: the
graph is back where it started, so one undo claiming to reverse both would describe a change
nobody made. That case is the one the old constant keys got right by accident, and the new
ones must not lose.

**Proof.** `node --test --experimental-strip-types features/workflows/*.test.ts` → **273
passed** (259 before, +14), `pnpm typecheck` clean (2/2), `cargo test -p omnion-workflows
--lib` **157 unchanged** (a client defect, not an engine one), `node --check
scripts/qa/walkthrough.cjs` clean. **Five mutations all red, one assertion each where
narrow:** the nudge call site reverted (1), the edge-add call sites reverted (1), `dragKey`
restated the key instead of delegating (1), a *new* gesture arriving with a bare key (2),
and the key functions inlined back into template literals (2).

**The guard and its three failures, all of them the test's fault.** The bare-key check reads
the canvas source and fails on any `commit`/`pushHistory` handed a string literal — the arm
the shortcut catalogue has and this rule did not. It fired on two **doc comments** still
describing the old keys, not on call sites; comments are now stripped first, and the strip is
itself asserted safe against a `//` inside a string literal, which is the case where a naive
strip eats real code and silently loses half the call sites. The chain case's first gesture had
`before === after`, so `record` dropped it as a no-op and the assertion measured a single entry
for entirely the wrong reason. And the count guard exists so deleting a gesture cannot satisfy
the rule above *by removing the thing the rule polices* — the rubber-stamp shape this REQ has
already paid for once (`pageIsAlive`, defined and never called).

**Not measured in a browser, and the box is worse than last tick.** No pass was forced: the
slot file's holder is a dead two-pid concatenation again (both pids gone, reapable), but
**three** live passes hold 30 Chrome (w7, w5, main), `/dev/shm` 85% and **0 free RAM** with
24G of 31G swap. The depth criterion is still not ticked in any part — see the note above it.

**TICK 52: THE DEPTH CLAIM IS NOW WALKED, AND THE TRIM WAS UNGUARDED
(`2f2b5f1d`, `9d0e2c47`).** The 50-step claim rested on `HISTORY_LIMIT = 100` and on the
tick-51 removal of one reason it was hollow. What was still missing is the part nobody had done:
*press undo fifty times*. `history-depth.test.ts` (7 tests) performs **50 gestures across all
five kinds** — add, move, edit, connect, delete — in a fixed rotation, then replays the presses
and compares the reconstructed graph against the canvas as it stood at five checkpoints and at
the end. Three properties make it a claim about presses rather than about an array:

- **The clock cannot be what separates them.** The whole run spans 500ms inside a 600ms window,
  and the test asserts that arithmetic, so only the coalesce key keeps the gestures apart.
- **The world must not end where it started.** Each cycle deletes the *oldest* card, so the
  final graph is a different graph; a stack that undid nothing would fail the premise before
  the walk even starts. An earlier draft deleted the card it had just added and the premise was
  vacuous.
- **The control must be able to fail.** The same fifty gestures under one bare action key
  collapse to a *single* entry — asserted as a fact about the product, not left implicit.

**Two real defects surfaced, and the first is the one a depth claim exists to rule out.** Fifty
gestures fit *inside* the limit, so nothing above ever reached the trim — and the trim is the
one line that decides which fifty survive when the stack is full. `entries.slice(0, LIMIT)`
instead of `entries.slice(entries.length - LIMIT)` keeps the **oldest** hundred, which passed
the entire suite: **278 green with the trim inverted.** Every retained entry still undoes
correctly, the count is right, and the author's last fifty edits are gone while the first fifty
stay resurrectable — a 100-deep history that is useless. The overflow test walks
`HISTORY_LIMIT + 50` gestures and presses undo fifty times, comparing each press against the
canvas that was on screen when that gesture was made. The second defect is smaller and is the
same shape as the first: the keyboard sheet read "the history is 50 steps deep" as a **string
literal**, answering to nothing, so lowering the limit would leave the sheet promising a depth
the stack no longer has. `historyDepthLabel()` derives it from the constant and the ⌘Z row
carries the function, with a test reading the row out of the catalogue.

**Proof.** `node --test --experimental-strip-types features/workflows/*.test.ts` → **280
passed** (273 before, +7), `pnpm typecheck` clean, `cargo test -p omnion-workflows --lib` **157
unchanged**. **Eight mutations red across the whole suite:** limit lowered to 20 (5), trim
inverted (1), coalesce disabled (1), `undoTarget` off-by-one (2), `redoTarget` off-by-one (1),
label decoupled from the constant (1), row hardcoded again (1), flattened catalogue emptied (1).
Two mutations survive *in this file* and were checked rather than waved through: dropping the
no-op guard is caught by the sibling `builder-history.test.ts`, and the redo-discard line is
behaviourally inert here because the cursor gates both `redo` and `redoTarget` and the trim
still bounds the array.

**Four of the seven tests were wrong before they were right**, and each was wrong the same way:
a check written about the array rather than about the press.

- The checkpoint index was inverted — `checkpoints[k]` is the graph *before* gesture `k*10`, so
  it is reached after `50 − k*10` presses, not `49 − press`. It read the state one gesture late
  and reported a cursor defect that was not there.
- The control used **five rotating** bare keys. Five names never match each other, so nothing
  merged and the control passed on a broken product — the tick-48 mistake in a new place.
- The redo walk read `entries[cursor + 1].after` directly instead of calling `redoTarget`, and
  was green with `redoTarget` itself off by one. A test that reaches past the function under
  test is testing its caller, and here the caller is the thing not written yet.
- The overflow walk compared card *counts*, and a stack holding the oldest hundred has the same
  count. The identity of the surviving entry (`add:x50` must be the oldest) is the assertion.

**Not measured in a browser.** The slot is held by a live `omnion-w5` pass (holder 1822875,
`/proc/1822875/cwd` = `/mnt/apopic/omnion-w5`, the other half of the file `1822897` is dead),
and w7 and main are running too: 45 Chrome, 0 free RAM, 24G of 31G swap. No second pass forced.
**The criterion is still NOT ticked** — the depth is now unit-proven, and the criterion's other
half (`cmd-s-writes-once`) was measured in tick 37, but the REQ's own gate requires a QA pass
before a box closes, and this REQ has four other rows still unmeasured.

**Next.** `undo-selection` (unmeasured since tick 50, and the two rows now disagree with each
other about the same toolbar button), then the REDO half of `reload-rebase`, then the
run-from-here / pill / table-mode rows. The plugin row stays BLOCKED on REQ-121.

**Next (tick 51 note, now done).** The 50-step claim still rests on the constant `HISTORY_LIMIT = 100`, and this tick
removed one reason it was hollow: the stack no longer merges unrelated gestures, so 50 entries
is 50 gestures. What is missing is a *test* that presses 50 distinct gestures and walks them
back — and it must be five kinds, not 50 adds, or it re-derives the tick-48 mistake. Then
`undo-selection` (unmeasured since tick 50), then the REDO half of `reload-rebase`.
`drag-undo` reads `afterUndo` and `returned` and both were correct, so the criterion's undo row
was satisfied — while `doUndo` and `doRedo` were leaving the selection naming a card the
restore had just removed. The row read the *button* and the *position*, and neither is where
the defect showed. It showed in three places the row did not look at: Duplicate and Copy
stayed enabled on a surviving id string, the status bar counted "1 selected" over a canvas
that did not contain it, and `Del` resolved to nothing. **The criterion is about the graph and
I measured the graph, which is the same mistake three ticks of this REQ have now made in four
different shapes.** The new `undo-selection` row reads the toolbar and the words, and carries
`cardWasSelected`/`cardRemovedByUndo` as preconditions because a zero count is also the answer
for a page that was never selected. **The criterion stays unticked in every part**: the depth
claim is still answered by the constant `HISTORY_LIMIT = 100`, the row is UNMEASURED, and the
run-from-here, pill and table-mode rows below are unchanged. — **MEASURED 2026-09-30 (tick 37, pass `20260930-132859`): the ⌘S half is proved, the depth half is not.** `cmd-s-writes-once` reads `before: 3, afterKey: 4, wroteSomething: true, settledSame: true` — one keystroke, exactly one version advance, and the indicator settled on the same version. This is the first pass on this stack that had a tenant to measure at all (`1 organization / 1 user / 1 site`, where every earlier pass read `1/0/0` because the wizard stopped after the owner step), which is why no previous pass could answer it. The version is read three times because the second write lands while the indicator still reads "saved": on the screen alone the criterion is invisible. The undo half is measured one level deep (`undo.afterUndo: 4, returned: true`) against a 50-deep claim, so **the depth is still NOT proved** and the criterion stays unticked in full.. — **THE MOVE HALF WAS BROKEN FOR A MOUSE, AND THE MODULE'S OWN TESTS COULD NOT SEE IT (`fac40efe`).** `commitMove` queued a save and recorded nothing, so dragging a card moved it on the canvas and left the Undo button grey — `⌘Z` was a no-op for every drag a mouse author made. The arrow-key nudge *was* recorded, so the history module's tests were green while half the gestures that perform a move were not undoable: one released key undoes, one released mouse does not, and nothing in the product distinguished them. The `before` had to be taken on the way DOWN — by `pointerup` every frame has already written the new position, and reconstructing it (`position - delta`) is wrong the moment a drag crosses a clamp boundary or a snap line, and wrong silently. `endDrag` **seals** before recording: `sealGroup` had been written for exactly this caller since the module was added and nothing called it, because there was no drag in the history to seal — and without it two quick drags of the same card merge inside `COALESCE_MS`, keeping the first `before` and the second `after`, so the position between them is reachable from neither. A history with a hole in it, produced by the fix for "a drag is not undoable". `moveNode` now writes `graphRef` as well as state, because a ref written in a render body is one render behind the last frame and the recorded `after` could otherwise be the position the card started at. Proof: **228 admin tests pass** (217 before, +11), `pnpm typecheck` clean, `cargo test -p omnion-workflows --lib` 157 unchanged. The load-bearing test reads `builder-view.tsx` — every unit test passes against a `commitMove` that records nothing, which is this bug unchanged — and **all three halves are proven to bite independently**: reverting the recording, the `beginDrag` call and the seal each turns exactly one assertion red. The walkthrough gains a `drag-undo` row that drags a card and reads the Undo button's own `disabled` attribute BEFORE the key press, because the existing `undo` step presses a key and therefore only ever measured the nudge path. **STILL UNTICKED:** the 50-step depth claim is a claim about the `HISTORY_LIMIT` and no row measures it, and no browser pass has run since — the QA slot is held by a live `omnion-w5` pass, so `drag-undo` is written and unit-proven but UNMEASURED.
- [ ] Two tabs on one workflow: the second save answers `409` and the UI offers Reload while keeping the local copy visible instead of overwriting silently. — **the server half was right and the client half was a dead end.** `replace_graph` refuses a stale version and names the one it holds, and the `conflict` probe proved that with a raw `fetch` — which never touches the toolbar, so the half the criterion is actually about (the UI's answer) was never measured. Two defects underneath. **First: only one of the two exits existed.** The server's sentence offers "reload to see their change, or keep editing to overwrite it"; the client rendered Reload and nothing else, and after a 409 `versionRef` stayed at the value the tab loaded, so *every* later PUT quoted a version one behind and was refused again. The banner described a dead end: the only way to write anything was to discard the author's own work. `conflict.ts` now owns both questions (which version the server named, what the next save may quote) and the toolbar offers a second, confirmed exit that re-bases on the version the **server** named — never a locally derived `versionRef + 1`, which would be last-write-wins wearing the costume of a guard (`314bcda`). **Second, a guard that could be made to fail open:** `at version 7.5` parsed as `7` (the regex took the integer prefix), and a client quoting 7 is an overwrite that the guard refused. The trailing boundary closes it; 7 tests, and each of the two dangerous rules reverted to prove it goes red (`6222873`). **Not ticked until the pass reads `two-tab-conflict.refused`/`reloadOffered`/`localNodesKept` and `two-tab-keep-mine.resolved`** — the second of those is the reading that would have caught the dead end, since a save quoting a stale version comes back as a *second* conflict (`d995669`). — **MEASURED 2026-09-30 (tick 37, pass `20260930-132859`): the second exit works; the criterion stays unticked on one field.** `two-tab-conflict` reads `tabTwoStatus: 200, state: "conflict", refused: true, reloadOffered: true, localNodesKept: 6` — tab one's save was refused, the toolbar offered Reload, and the author's six local nodes were still on the canvas rather than replaced. That is the dead end the previous two ticks were about, and it is closed: `two-tab-keep-mine` reads `offered: true, resolved: true, stateAfter: "saved", versionAfter: 11, nodes: 6`, so the second exit re-bases on the version the **server** named and the write lands instead of colliding a second time. `namesVersion` is `false` and the text is empty, which is why this is not a tick: the criterion's banner must NAME the version the other tab took, and this measurement does not show it does. The version is in the payload (`conflict.namesVersion: true` on the raw probe) and the banner does not carry it — the one remaining question, and it is a real one rather than a harness artefact. **— IT WAS A HARNESS ARTEFACT (`88810166`, tick 38).** The banner renders `state.message` verbatim, and that message ends "(it is now at version 10)" — but the probe read the banner **after** clicking "keep mine". That click resolves the conflict and the banner leaves the tree, so the locator's `catch()` returned `""` and the note recorded `text: ""` — an empty string that should have been read as the tell it was. `namesVersion: false` was measuring a node that no longer existed. The reading now happens above the click, because a banner is only on screen while the author has not answered it yet. The other three rows in the same note were correct throughout, which is precisely what made this look like a banner defect. **STILL UNTICKED:** the pass re-run, not this reasoning — the criterion is only closed by a reading off the live server. — **MEASURED 2026-09-29 (tick 23, pass `20260929-143037`), NOT TICKED: the client half is broken.** The server half is now proven twice, from two directions rather than one: the raw probe read `conflict: {status: 409, code: "graph_version_conflict", namesVersion: true}`, and the two-tab probe watched a real second tab move the stored version 1 -> 2 (`tabTwoStatus: 200, versionBefore: 1, versionAfter: 2`) — a refusal the product performed rather than a status code the probe asserted. But the same note read `state: "error"` with `reloadOffered: false` and `refused: false`, and `autosave.saveState` is `error` on that tab too: the toolbar is not reaching its conflict region because the save is failing first. A box that claims two halves with one of them red is a claim a reader cannot check, so it stays unticked until the client half is green.
  **THE CAUSE, AND IT WAS NOT WHERE THE NOTE POINTED** (tick 24, `9b9cb46`). The note blamed
  the client, and the client was broken — but the reason it was broken is a *read* that did not
  exist on the surfaces the panel opens a rule from. `graph_version` was on the column and on
  `GET /workflows/{id}/graph`, and on **neither list route**: not `/workflows`, and not
  `/automations`, which is the one the rule list actually reads. A save that started from a list
  row therefore had nothing to quote, and the only thing a client could do was send `0` — which
  the server refuses as `400 graph_version_required`, an error about a version the author was
  never shown. That is why the save failed *before* the toolbar could ever reach its conflict
  region, and why four separate notes read empty behind one write. Both list surfaces now carry
  it, and the round trip is asserted end to end: list → read the version off a row → PUT the
  graph that row names → `200`, with the version advancing exactly once. **The negative control
  is the part worth keeping:** quoting `0` instead turns the test red with
  `graph_version_required`, which is the *product* refusal the tick-23 note was misreading as a
  client defect — so the test cannot pass against a server that stopped sending the field, and
  the one thing that made this look like two different products is now the same story.
  The two rebuild paths carry the STORED version: a restore writes at the version the rule is
  on, and an update that let a request set the column would be a way to skip the concurrency
  check the graph write exists to perform.
- [x] Saving the graph re-projects `steps` and the existing runner executes it end to end with no engine change. — slice 1: `graph::project` is the single projection path, `graph_store::project_steps` runs it in the same statement as the write, and the walkthrough reads `projection.valid` / `projection.step_count` back from the server.
  **AND THE SAVE NO LONGER REFUSES A RULE THAT IS STILL BEING WIRED** (`91bcbda`). The criterion as written above is only true for a graph that *projects*, and the product made that the price of every save: a graph is edited one card at a time, so a rule's first save is a definition whose cards are not connected yet, and the projection error came straight out of `replace_graph` as a `400 graph_invalid`. The author's first keystroke on the feature was refused by the feature. The guard moved from "you may not save" to "you may not run", which was always the true statement, and it is the same place the projection already failed.
  **Three halves, and the trade is only safe because all three landed together.** (1) The save writes the graph and **records** the reason in `workflows.validation_error` instead of refusing — a rule that silently stops firing is worse than one that says why. (2) It **leaves the step list alone** on a graph that does not project (`steps = coalesce($3, steps)`), because blanking it is what makes "not yet runnable" indistinguishable from "has never run" on the list screen *and* hands a zero-step run to an engine that settles it as `completed`. (3) `start_run` reads the reason through `admit_to_run` — a pure function, because the save that records and the run that obeys are a release apart and the join is one column.
  **The test reads stored rows in both directions, because the dangerous states are two ordinary-looking 200s.** A server that saved an *empty* step list answers "the save worked" and "the run reported success" while doing nothing, so the assertions are: the previous `steps` survive, the reason is recorded and non-blank, and `workflow_executions` holds **zero** rows after the refusal. Then the same rule is wired for real and must both clear the reason and run — otherwise the guard is a rule that stopped working forever. A **blank** reason is `None` as far as the guard is concerned: reading it as a refusal makes a rule permanently unrunnable over a column nothing wrote, which is the silent-failure direction (it simply never fires and no screen says why).
  Also in the same commit: the save's response stopped filtering the body to errors, because `findings` and `error_count` were **one expression** (`error_count = findings.len()` after a `filter(is_error)`) and so could not disagree — which made the panel's own `severity !== "error"` branch unreachable and shipped a graph with a warning and no error as "No problems". The Rust test for it is worth keeping as a shape: the fixture must produce **exactly one warning and zero errors** (a long label on a proven-clean starter graph), because a fixture whose cleanliness is assumed is a fixture whose defects are the test's.

- [x] The backfill gives every pre-existing workflow a valid graph that opens in the builder without manual repair. — and the gap the backfill left is closed: the column default was an *empty* graph, so every rule created after 0051 opened blank and refused its first save (`c864ca1`). `insert_workflow` now seeds `Graph::starter`, proven by a real insert plus the real validator (`0c9ee98`): the walkthrough reads `canvasNodes: 2` and `projection.valid: true` on a rule it created seconds earlier, where the same pass read `0` and `graph_invalid` before.
- [ ] "Run from here" on a mid-graph node starts a run whose first step is that node, earlier nodes stay `skipped`, and the trace says why. **The whole criterion is implemented, and none of it is expressible without a sixth step state** — a run's steps are all `pending` at creation and the engine claims them strictly in `step_no` order, so there was no way to say "these two did not run" (`4b7df30`). The finding worth keeping is that `skipped` cost **no engine change at all**: `claim_due_step` reads only `('pending','waiting')`, `settle_execution` counts open as `('pending','running','waiting')` and failures as `= 'failed'`, and `retry_step_from` re-opens four statuses. A state that had to be threaded through them would have been the tell that the schema was not ready. The plan is made against a new `graph::project_walk` rather than against the step list, because the two disagree in three places and **each one decides whether a node is startable at all**: a trigger contributes no step (and re-running a rule from the top is a real thing an operator wants), an end node contributes a `stop` (starting there leaves every other step pending forever), and a note is inert. A planner indexing the step list needs a special case per case, and a special case is where an off-by-one lives — **which is the first thing I wrote**: `position() + 1` made the clicked node itself skipped, and the assertion that caught it checks the clicked node's own `step_no`. The prefix is inserted as `skipped`, never inserted pending and updated after, because a crash between the two writes leaves a run whose prefix the engine is about to execute — the exact side effect the feature exists to prevent. The endpoint (`3a7ecc0`) is guarded by `workflows.run` and not `workflows.manage`, and the panel (`c355084`) offers the button wherever a run can start and disables it **with a stated reason** where it cannot. The test reads the **stored rows**, not the response body, because a handler that returned a plan-shaped payload while writing a full run would pass a body-only test and the criterion is about what runs: step 1 `skipped`, step 2 not, the reason on step 1 naming the node, the run still settling `completed`, and the skipped step holding **zero attempts** — which is what proves the engine never claimed it. **Not ticked until the pass reads `run-from-here` with `skipped > 0`, `reasonNamesNode: true` and `firstRunnableNo === firstSkippedNo + 1`** (`7bf3641`). — **MEASURED 2026-09-30 (tick 43, pass `20260930-225411`): THE SCAN IS FIXED, THE CRITERION IS NOT, AND THE REMAINING BLOCKER IS THE PROBE'S OWN GRAPH.** The scan now reads three cards (`trigger`/`end`/`wait-3`) against `cardsOnCanvas: 6` with `canvasWasStable: true`, answers `isTriggerType` per card, and **chooses `wait-3`** — a mid-graph node — instead of the trigger, so the "skip the trigger" half works for the first time. The cycle rows read `codes: ["graph_cycle"]` alone now, so last tick's rebuild is browser-proven.

**`skipped` is still 0, and the product is RIGHT.** `projection` in the same pass reads `nodes: 3, edges: 1, valid: false` with the reason *"Wait\" is not reachable from the trigger — it would never run"*: the pass's `port-connect` row shows the only edge is `trigger → end` (its own connect was refused — *"Event · Next already leads to that node"* — because the starter graph already wires them), so `wait-3` is an **orphan**. A node the walk never reaches has no position to start from, and `plan_from_node` refuses it with `unknown_node` — a run promised steps the definition does not contain. The client's `canStart: "true"` is the disagreement, and it is the honest kind to be looking at: `startability()` is a pure function of node type and outgoing edges, and "has no outgoing edge" is the one thing it does not know.

**So the next tick wires the node instead of connecting the two ends.** A `run from here` criterion needs a graph with a real prefix, and every step this pass adds a node — palette click, drag, keyboard — drops it on the canvas **unconnected**, because the harness has no gesture that inserts a node *between* two wired cards. The fix is to build the prefix through the graph route before the scan (a `trigger → wait → end` spine, which is what `plan_from_node` needs), so `skipped`, `reasonNamesNode`, `firstRunnableNo` and `pillsPainted` are all measured against a definition that can actually be run from the middle. Not ticked: nothing about the skip path is proven yet, and `reasonNamesNode`/`firstRunnableNo` are `null` rather than true. — **TICK 25: the probe clicked the one card whose correct answer is always "no."** It selected "the second card in draw order", and a rule is born from `Graph::starter` as `[trigger, end]` — so the second card is *always* the end node. The note read `canStart: "false"` with the reason "The end of the graph has nothing after it to run", which is **the product being right**: an end node cannot start a run, and stating why is the refusal half of this very criterion. The control also lives in the **inspector**, so it renders for the selected node only; a page-wide query for startable nodes finds nothing and reads as a dead feature. The scan is now per card — select each, read its own answer, take the first that can start and is not the trigger (a run from the top is a whole run and proves nothing about a prefix) — and every answer is recorded, so "no node can start" is a finding with evidence rather than a `null` (`41f5ea3`). This also unblocks criterion 2's click half, which reads its pills off a run that never started. The probe reads the run back through the API after the press, because a toast that says "Run started" proves the button was pressed and nothing else — and the failure it would miss is a run that quietly executed the prefix, reported like any other run. — **TICK 44: the disagreement the last tick called honest was a LIVE BUTTON, and building the spine is what made it visible (`c557711b`).** The spine is built now — `trigger → wait → act → end` written through the graph route with `seconds` on the wait, and the projection is *verified* (`step_count: 3`, `valid: true`) before the scan measures against it, because a harness that builds a prefix and does not check it has built one the server may not share. The scan had already chosen `wait-3` correctly, so with a real path the run-from-here rows are finally measurable.

**And the defect was not in the probe at all: it was the product.** `startability()` answered `canStart: "true"` for the orphan, and the note above reads that disagreement as *"the honest kind to be looking at"* — it is not. **A live button that is refused on every press is the outcome this feature's own module doc names as its worst**, and the orphan shape is not exotic: **every node in this builder is dropped from the palette un-wired and connected afterwards**, so the editing state of the screen was the state the button got wrong. The refusal also read as a statement about the run ("not on the path from the trigger") rather than about the wiring, which is what made it survive three ticks of staring at it.

The two sides disagreed because each was answering the question it *could*: the card could see that no connection leaves it, and the server could see that the walk never reaches it. So the client now takes the verdict (`reachedByTrigger`) and refuses, naming the fix rather than the symptom. `reachability.ts` computes it by **reading `terminal` off the palette** — a connection on a port that ends the run does not carry the walk onward, which also refuses a condition wired only on `false` (the reachable-looking half). `graph.rs` turns `follows()` into `followed_port()` derived from `Port::terminal`, and asserts over the whole registry that this equals the list it replaced; a guard fails if either JavaScript file restates the rule. **Third copy of a walk rule on this branch, after the type registry and the trigger prefix, and the generalisable form is the one that keeps recurring: a rule restated in a file nothing compiles is a rule with no compiler.**

**Proof.** `cargo test -p omnion-workflows --lib` → **155 passed** (152 → 155; three new). `node --test` over the two client files → **25 passed** (13 → 25). `pnpm typecheck` clean. Both guards are **proven to bite**: removing the refusal turns exactly the two client tests red (11/13), and restoring a port-name list in `reachability.ts` turns the cross-language guard red. Still unticked: the browser reading of `skipped > 0`, `reasonNamesNode` and `pillsPainted` against the new spine — the pass was queued behind a live slot this tick.
- [ ] After a run each node shows its status pill, and clicking the node opens that step's inputs and output. — **THE GATE WAS ONE-SIDED, AND THE GATE IS THE ONLY THING THIS CRITERION IS CLOSED ON (`run-from-here-row.test.ts`).** The criterion says "*each* node shows its status pill" and the row's own comment claimed "*every* node the run touched is painted, **and nothing else is**" — a set **equality**, stated twice, computed once. `runNodes` (from the run's `steps`) was compared against `paintedIds` in one direction only:

```js
const paintedButNotInRun = paintedIds.filter((id) => !runNodes.has(id));
```

so a canvas that painted the two nodes which ran and painted **nothing** for the skipped prefix reported `pillsPainted: 2` and an empty `paintedButNotInRun` — and the gate written in this very line ("`pillsPainted > 0` with `paintedButNotInRun` empty") went **green**. The direction that catches a *missing* pill was the one that did not exist, and the criterion's own universal is what names it. This is the tick-48 shape in its quietest form: the row read the **right** markers, in the **right** place, with a note that looked complete — nothing about it signalled a defect, because there is no such thing as a wrong set comparison, only a missing half of one. The skipped prefix is where it bites hardest and not by accident: `node-status.ts` paints a `skipped` pill for exactly one reason (so an operator can see the prefix was skipped rather than run), which makes those nodes the most likely to go unpainted in a regression — and they are rows in the run's steps like any other, so the run names them and the comparison is well-founded without a second source of truth (a trigger and an inert note have no step, so they are correctly *absent* from `runNodes` and correctly unpainted). `inRunButNotPainted` is now computed and reported. **Proof:** 313 admin tests (307 → 313, +6), `pnpm typecheck` 2/2, `node --check` clean (14,280 → 14,303 lines), `cargo test -p omnion-workflows --lib` 157 unchanged, **nine mutations red**. **Not ticked: the reading is still unmeasured** — no browser pass (the slot's holder is a LIVE w6 pass, pid 2887474, `/proc/2887474/cwd` = `/mnt/apopic/omnion-w6`; load 80, 65 Chrome, 4G free of 32G, `/dev/shm` 86%), and the criterion needs `inRunButNotPainted: []` **alongside** `pillsPainted > 0` — the first alone is satisfied by a canvas that paints nothing at all. The row also had **no instrument test at all** before this tick, while its own criterion is the most explicitly instrumented in the file; that is why the half went missing without a check noticing. — **the pill half is built; the click half is not, and the reason the criterion was unprovable was not missing code.** The engine had written `node_id` and `skip_reason` for two ticks and the canvas had nothing to paint with: `StepBody` carried a step's `status` and not the node it came from, and `ExecutionSummary` carried no `started_from_node`. All three fields were stored correctly and none of them was on the wire, so the probe had been reading `null` for everything it asked about — the criterion was *unprovable* rather than unmet (`a021cb4`). The mapping is a pure function (`node-status.ts`) for the reason `conflict.ts` is, and its three load-bearing rules are asserted: a node with **no step paints nothing** (a pill on a node the run never reached is a claim about work the engine never did, and a note is decoration, not work), a node whose **branches disagreed** is `diverged` rather than whichever branch ran (a `Map` keyed by node would keep whichever row the API returned *last*, so the answer would depend on row order), and steps with **no `node_id`** are dropped rather than bucketed under an empty key (a rule predating the builder has no node, and index-attribution is a guess that paints the first card on the canvas). The pill is fed from the *response body* on purpose: reading the run's rows back from the API would pass even if every card rendered nothing, so the probe now reads the canvas and compares the painted node set against the run's. 10 unit + 85 builder tests. **Not ticked until the pass reads `pillsPainted > 0` with `paintedButNotInRun` empty.**
- [ ] *Criterion 2, second half — clicking the node opens that step's inputs and output.* **BUILT, not ticked, and the PROBE ITSELF LOST A BRANCH (`step-trace-row.test.ts`, tick 57).** The gap was on the wire, not in the client, and it was the **same class of gap as the pill half one tick earlier**: `StepBody` carried `output` and not `params`, so the panel had one side of each step and nothing to show for the other. A stored `params` is **not** the node's authored `params` on the canvas — a run from a node, a retry, or an edit that was never saved leave the two different — so reusing `node.params` in the panel would have compiled, rendered, and quietly been the wrong number. `step-detail.ts` is a pure function and its three rules are exactly what a `steps.find(s => s.node_id === id)` throws away: a node with two branches opens **both** steps (showing the branch that ran and hiding the one that did not is the information the `diverged` pill exists to advertise); `null` ("no run read") is not `[]` ("a run with no steps"), because a rule whose first run is still `pending` has steps and collapsing the two makes a rule that has never run look like one whose nodes all sat out; and an **absent** payload is not an **empty** one, so the server now sends `{}` rather than omitting the key and a Rust test asserts the empty object *is* sent. Payloads are classified before they are rendered and never stringified: `JSON.stringify` throws on a cyclic value, and it throws during render, which takes the panel with it. The probe reads the **panel**, not the run — fetching `step.output` would pass against a trace that rendered nothing — and clicks a node read off a *painted, non-skipped* card, because clicking a node the run never reached proves the empty-state message instead of the panel.

  **The probe read the panel's FIRST payload block, so it measured one step and called it the node's.** `panel.querySelector('[data-step-trace-payload="inputs"]')` is the first match, and the panel renders one Inputs/Output pair inside every `[data-step-trace-step]` container. A node with two steps — precisely the branching case `step-detail.ts` exists to keep — reported `stepsShown: 2` beside **one** step's payloads, and the second step could have rendered nothing with every number in the note unchanged. `stepsShown` counted blocks while `inputsRendered` counted one: two counts over two different sets, and only the first was a gate. This is the twelfth instance in this REQ, and the second where **the product guards a loss twice on purpose and the read throws the guard away** — the file's own comment says "the loss cannot happen twice", and the probe made it exactly once. The read is now scoped to each step's own container, `stepsWithoutBothSides` names the steps that failed to open *both* sides, and the panel's `data-step-trace-step` numbers are compared against the run's steps for that node **in both directions** (a one-sided set comparison is the tick-56 shape, one row up).

  **The wire probe read `params` and never `output` — tick 56's defect in a different place.** The note carried `stepsWithParams === stepsTotal > 0`, the gate this line was written against, while `outputRendered` measured a panel that could only ever have been fed by a half-populated wire: a server sending `params` and dropping `output` was a **healthy reading**. `hasOutput` now uses `"output" in step` rather than a truthiness test, because an explicit `null` ("the step produced nothing") is a different fact from an absent key ("the server never sent it") and the panel renders two different sentences for exactly that pair. **Proof:** 317 admin tests (313 → 317, +4), `pnpm typecheck` 2/2, `node --check` clean (14,303 → 14,372 lines), **eight mutations red**, each naming the assertion it turned — including M8, which typed `stepsWithOutput: 3` and came back red precisely because tick 55's M10 lesson is now asserted in this REQ's own vocabulary. **Not ticked: the reading is still unmeasured** — no browser pass (the slot's holder is a LIVE w6 pass, pid 2887474, `/proc/2887474/cwd` = `/mnt/apopic/omnion-w6`), and the pass must read `panelFound: true`, `kind: "node"`, `stepsShown ≥ 1`, `stepsWithoutBothSides: []`, `stepsInRunButNotShown: []`, `stepsWithParams === stepsWithOutput === stepsTotal > 0`, and `inputsRendered`/`outputRendered` resolved over **every** step rather than the first.
- [x] "Retry this node" re-runs only that node without duplicating earlier side effects (proven with the mail sink). — **built and proven, and the walk corrected two of my own decisions rather than confirming them** (`633b620`). The criterion names its own instrument, and it is the right one: a run whose first step re-runs is *indistinguishable* from one that did not on any status column, so the walk asserts a **mail count** (the sink, built here for this walk) rather than a status comparison. It is deliberately **not** a narrowed `retry_step_from` — that write re-opens `step_no >= N` on purpose, because a run whose middle failed must not march on to completion with a hole in it, and reusing it here would re-send the earlier e-mail. The one row is its own write, and the walk asserts the returned count, so a later widening of the `WHERE` fails the walk rather than quietly repeating a side effect.

  **Two decisions the walk overturned, which is the worth keeping:**
  1. **A plain `run` never stamped `node_id` at all.** The per-node status layer was empty for the *most common* way to start a run — no pills, no click target, and a retry answering "took no part in this run" on **every card**. The walk was written for retry and found a defect that was not in retry: attribution belongs to every run, so `graph_store::attribute_steps_to_graph` is now called from the plain-run path too, and is **best-effort** — it is a decoration and must never fail a run that has already started.
  2. **The attempt counter is reset, and my first draft was wrong about that.** The reasoning was "one node is not a new budget", and the database refused the write: `workflow_steps_attempts_shape` caps `attempts` at `max_attempts` and `claim_due_step` *increments* on claim, so a re-queued step that had spent its budget produces a row the engine is **forbidden to claim**. The retry would have been accepted, audited, and then never run — a control whose stated purpose is "try this again" that cannot try again. Bounded by the step's own `max_attempts`, not unbounded.

  `retry_node::plan_retry_node` is a pure function because a branching node is **two rows**: a `find` reports "nothing to retry" on the very node the canvas paints red, and its four refusals are four sentences (succeeded · not-in-run · run-live · run-cancelled) because only the last two are about the run. **Not ticked on the browser pass** — the control is in the inspector with a stated reason on every refusal, and the probe for it has not run.
- [ ] "Listen for a real event" captures a real bus event into the inspector within one matcher tick, and the listener expires after 15 minutes leaving no stray token. — **BUILT and server-proven; the click half is unticked** (`fb3e8d3`, `711a32c`, `7cdcdad`, `9892c91`, `f2483c5`). The criterion is three claims and the interesting part is that **the existing listener could not answer any of them without becoming a different thing.** REQ-003's `automation_test_events` looked like the smaller change and is rule-shaped with no expiry and no token: a rule-level capture shows what arrived on the *bus*, and "what would *this* node receive" is the payload its **upstream** produced — a different question. So this is a new table, and the naming of that is the design.
  **The expiry is the half that is easy to get wrong and nothing on the screen would show it.** "Armed" reads most naturally as "not yet consumed", and a matcher filtering on `consumed_at is null` alone fills a row whose fifteen minutes ran out and reports a capture for an event **nobody was watching**. `listener_is_live` is the single definition — `consumed_at is null AND expires_at > now()` — asserted from both sides of a closed boundary, because the predicate is hand-written in three places (the partial index, the `UPDATE`, the sweeper) and three copies is three chances to disagree. **The read never filters by state either**: an expired row that vanished is indistinguishable from one that was never armed, and only the second is something the author can act on.
  **The walk is shaped so each clause fails loudly if it stops being true.** "Within one matcher tick" is *exactly one* `matcher::drain` — a second drain would still pass a `captured_at` check, and a listener needing two ticks is a broken one. "No stray token" is asserted against **the matcher's own predicate**, not a row count: a real event is driven through the real matcher at an expired row and then the walk asks whether a live-listener query still sees it. Only that separates "the row was deleted" from "the matcher cannot see it", and the second is the criterion.
  **Three of the walk's own first drafts were wrong, which is the part worth keeping.** (1) It pinned `expires_in_seconds` to 900; the server sends 899, because the number is `whole_seconds()` of a window that began microseconds before the read — so the assertion was checking a rounding rule, and the *window* is the gap between the two timestamps. **The test was wrong, not the code.** (2) It expired a row by back-dating `expires_at`, and **the migration's own `expires_at > created_at` constraint refused it** — a real sweeper would hit the same wall, so a constraint written for correctness is also a constraint on how time may be simulated. (3) It expected 404 for another tenant's rule and got 403 `cross_organization`; a 404 would claim the id is unknown, which this API does not do on any scoped surface. Each was corrected *toward* the platform's real guarantee rather than by loosening the test.
  The panel is in the **inspector above the node editor**, not inside it: it is a property of the rule and the selection, so nested in the inspector it would vanish the moment the author clicked the desk — which is exactly when they want the payload. Its countdown is recomputed from the expiry instant every second and takes the **smaller** of that and the server's number, because a local counter drifts while a tab is backgrounded and then says "12 minutes left" about a listener that expired four minutes ago. **TICK 45: the two ticks of "the pass never reached the builder" were the harness, not the box.**
`pageIsAlive()` was defined and never called, and `reviveMainPage()` existed and could not
work: `browserContext.newPage` answers `Target page, context or browser has been closed` for a
dead TAB and a dead PROCESS alike, so the recovery could not tell the one case it fixes from
the case it cannot. On pass `20261001-012121` it never worked once — the same line appears
seventy-six times — and the site was `walkthrough.cjs:8307`, the sign-out block: the last
unguarded statement in `main`, sitting between the route loop and these passes, which is why
`pages: 55, mobile: 0` and no `workflowBuilder` key at all. Its comment said "exercised last so
it cannot break the walk" and EIGHT passes follow it. So the ticks spent on memory, shared
Chrome and the queue were spent on the box while the harness had already decided the outcome.
Wrapped, recorded as `signout-failed`, and the dead-process case is now distinguishable and
reported as `skippedForDeadBrowser` — a shortfall, not a defect on a screen never reached — with
one `browser-died` finding in the roll-up instead of seventy-six. `selfcheckRecovery` had
proven the dead-TAB case only, which is the case that recovers: the thing exercised was not the
thing that happens. It now closes the browser outright (6/6, both new checks proven to bite).
A Rust guard asserts the sign-out is wrapped, records its failure and runs BEFORE the builder
pass — the ordering is the claim that costs the measurements — and both halves were proven to
bite. Still unticked: this tick's pass has not finished, so every row below is still unmeasured.
**Not ticked until the pass reads `listener` with `captureRendered: true`, `payloadRendered: true`, `windowSeconds: 900` and `tokenReturnedOnRead: false`.**
- [ ] A plugin node appears in the palette with its badge when the plugin is enabled and disappears when it is disabled; a definition using it then reports an honest validation error instead of failing at run time. — **BUILT, not ticked** (`393687f`). The criterion is three claims and only the middle one needed a store, so the shape of the answer was forced by something `graph.rs` already admitted: a plugin node is *added beside* the core types rather than in place of them, because the core registry is a `const`. **The load-bearing decision is the direction of the dependency.** The plugin registry is a parameter of the *check* (`validate_with_plugins`), not of the core's knowledge — teaching the core that plugins exist would let anything able to produce a registry make an unrunnable rule look valid, and would move the failure from "Validate says so" to "the run died at 03:00". **The third clause costs nothing, and that is the whole design rather than a convenience.** A disabled plugin resolves to `Unknown`, which is the state a typo resolves to, so the core's existing `unknown_node_type` finding already reports it at edit time with the node named. The only new thing is *which sentence*: "came from a plugin node type this organization no longer has enabled — re-enable it" instead of "is not a node type the platform knows". Telling an author their working rule is nonsense because an admin disabled something is the fastest way to teach people to ignore the problems panel, and a test asserts the two sentences stay apart so they cannot collapse into each other. **Namespacing (`plugin.<plugin>.<node>`) is the security property, and it is asserted from both sides** — a manifest declaring `node: "action"` installs `plugin.mailer.action`, and `action` still resolves to the core node. The failure being designed against is a plugin silently taking over every rule in the organization. Registration is **all-or-nothing**: a half-installed manifest gives an admin a palette with some of a plugin's nodes and no way to see which, and a rule using the missing half fails validation looking like a bug. **No defaults are invented.** Only a `select` seeds a value, because that is the one choice the manifest itself made; nothing else is prefilled, and that includes booleans — a `false` in an unset boolean is the same lie as a fake `example.com`, since the field looks configured and the run behaves as if the author had chosen it. A `select` with no options is dropped rather than drawn unanswerable, and `required` is downgraded with it, because a card with a field the inspector cannot fill is a card the author can never save. A plugin node is **never `inert`** (it declares ports, so the author wired it expecting something) which means the dangling-output check applies to it like any other; and a type that could not be resolved at all contributes **one** finding rather than one per edge, or the panel shows the same sentence eight times and the author reads it as noise. `plugins_enabled_for` is the single seam REQ-121 fills in; it returns an empty registry today, which is exactly the state of an organization with no plugins, so the route is correct now rather than approximately correct. **Not ticked until the pass reads `plugin-palette` with `badgeRendered: true` and `tooltipNamesProvider: true`, then re-reads the same node with the plugin disabled and finds it absent from the palette and reported by `validate` as `unknown_node_type` with "re-enable" in the message** — the three claims, in the criterion's order, with the last one measured rather than asserted here.
- [ ] Table mode renders the same definition, edits parameters, and stays consistent with the canvas after a save in either mode. — **BUILT, not ticked** (`0558ec2`, `98010ed`, `b227846`, `96199e50`). **The probe's create was refused `422` on a missing field, so every row of this note was unmeasured rather than red.** It posted `{name, description}`; `WorkflowInput` deserializes `trigger` and `steps` as **required**, so the request died before any table code ran, the probe returned on `id: null`, and the rest of the block returned early — indistinguishable from a table that renders nothing. The trigger is structured (`{"kind":"manual"}`, never the bare string) and one task step is the smallest definition the engine accepts. The refusal **message** is now part of the note: `StepDefinition` is `deny_unknown_fields`, so it names the field, and three ticks each guessed at the payload instead of reading the one line that said which field was missing. The criterion was not partly done; it was **not satisfiable**, and the reason is what the toolbar link was pointing at. "Table mode" linked to `/automations/{id}` — REQ-003's linear step editor, a different projection of the rule — so "the same definition" named two things that were never the same object, and "consistent after a save in either mode" had nothing to be consistent with. A table over the linear editor is a perfectly good table of a definition the canvas never drew. `/workflows/{id}/table` now reads the same `graph` jsonb and commits through the same `saveWorkflowGraph` call quoting the same version, so a save there advances exactly the version the canvas would and the next tab meets exactly the conflict the canvas produces. **— TICK 59: THE REVERSE READ WAS HONEST AND STILL IMPOSSIBLE (`96199e50`).** Two ticks of work went into moving `builderSeesTableEdit` off the wire and onto the canvas, and the gate it produced cannot be satisfied by *any* product, this one included. `NodeInspector` renders under `{selectedNode ? … : null}` and nothing selects a node when a builder opens, so the row's `querySelector('[data-inspector="<id>"]')` matched nothing on a correct screen: `inspected: 0`, `builderSeesTableEdit: false`, and not one defect able to turn it. That is the worst kind of row — it does not fail loudly, it fails *misleadingly*, and the tick after this one would have opened the inspector to find a bug in correct code. **The fix is a click, and it is a click for the same reason the criterion's own instrument is a browser.** Which card carries the table's edit is not knowable from outside (the criterion never says), so every card is clicked in turn and the first whose panel holds the literal is the answer — a probe that guessed the node would be asserting an assumption and would be red for the wrong reason whenever the table's save landed on a different one. A card whose click missed is `Escape`d rather than left, because a half-armed connect gesture would survive into the `run` and `unfinished-save` rows below and turn their clicks into edge targets. **`clicked` and `inspected` are now separate fields**, since collapsing them is what let a green row look like a diagnosis. **Not ticked until the pass reads `table-save-survives` with `clicked > 0`, `inspected > 0` AND `builderSeesTableEdit: true` — a conjunction, and the first two are what make the third mean anything.**
  **The four decisions, each a shortcut that produces a plausible wrong answer.** (1) **An unedited draft is not committable** — Save is disabled on it, because a write with nothing changed advances `graph_version` and hands the next tab a conflict no author created, and the author who pressed it is right to be annoyed. (2) **A parameter edit is by KEY and re-typing a field's own value is an undo, not a change** — a `find` over the params, or a compare that ignores the pristine copy, moves the version for a keystroke. (3) **Clearing a field REMOVES the key rather than storing `""`** — the obvious version writes an empty string, the table shows the field filled, and the registry's own non-empty validation refuses the save, so the author is told their edit was invalid when they deleted a field. (4) **A dangling edge is NAMED `(missing node)` rather than dropped** — the canvas draws an edge heading nowhere, and a table that silently omits it is not rendering the same definition, it is rendering a *repairable* one.
  **The one that was my own bug, found by a test rather than by reading.** Clearing a label fell back to the node **id**, which reads like a sensible guard against three unnamed cards and is not reversible: the author clears one field and a node called "Send mail" is permanently called `n2`. Restoring the row's *own original* label makes the clear an undo, which is what clearing a field means everywhere else. 19 tests.
  **The probe goes through the link, not the route, and asserts the third claim in both directions.** Loading `/workflows/{id}/table` directly passes every row check while the link still points at the linear editor, so `builder-link.pointsAtTableRoute` is the assertion that catches it. A count would pass against the wrong nodes in the right number, so ids are compared one for one. The field is **uncontrolled** (a controlled one re-renders the whole draft per keystroke and the caret jumps), so "edits parameters" is proved by reading the **server's** copy back after Save — a table that never reads the input back looks correct until the author reloads. And "in either mode" is asserted in **both** directions: a label renamed on the canvas must appear in the table after a reload, and a value the table committed must still be in the graph when the builder is reopened. A table holding its own copy of the graph passes the first two and fails exactly the third. **Not ticked until the pass reads `workflow-table` with `pointsAtTableRoute: true`, `idsMatch: true`, `wroteToServer: true`, `seesCanvasRename: true` and `table-save-survives.builderSeesTableEdit: true`.**

**TICK 58: the third claim was read off the server, and it is the only row in the builder pass with no instrument test** (`table-mode-row.test.ts`, `table-mode-row.mutation.mjs`). `table-mode.test.ts` covers the table's RULES — `buildTable`, `diffTableEdits`, `toGraph` — and every one of them can be correct while the row that reports them renders nothing. `step-trace-row`, `run-from-here-row`, `undo-selection-edge-row` and `reload-rebase-row` all have an instrument test, and each was written because its row measured the wrong surface. The pattern is always the same: **the criterion names a surface and the row read a different one.**

The criterion's third clause is "stays consistent with **the canvas** after a save in either mode", and the row asserting it was a `fetch`:

```js
const builderSeesTableEdit = await page.evaluate(async (id) => {
  const current = await (await fetch(`/api/v1/workflows/${id}/graph`, …)).json();
  return Object.values(current.graph.nodes ?? {}).some((n) =>
    Object.values(n.params ?? {}).includes("qa.table.edited"));
}, workflowId);
```

The field is named `builderSeesTableEdit` and the line above it navigates to the builder, so the note reads as a claim about a screen while being a claim about Postgres: a canvas that mounted no node, or an inspector that never received the graph, reports `true` identically. This is the tick-57 defect one block up — there `step.output` was read off the wire where the criterion named the panel, and the fix is the same. The read is now **card → panel → field**: it waits for `[data-node-id]` and `[data-inspector]` rather than a `waitForTimeout(1500)` (a fixed delay is green against a page that has not drawn, and is wrong only on a slow machine, which is the worst way to be wrong), resolves each card's own panel by node id, and compares each field against the value it was handed. `nodeShowingValue` names the node that carries the value, so a `false` says WHICH node failed — a count of zero is the same number whether the canvas is empty or the value is on a node nobody clicked.

**Two of the eight mutations survived the first draft, and both were the class this file opens by documenting — I wrote the paragraph and then made the mistake inside it.** `data-inspector` is a **prefix** of `data-inspector-field`, so M3 (`const panel = document`) left a suite asserting `/data-inspector/` green; and M2 (counting any non-empty field rather than comparing to the committed value) satisfied an assertion made about the presence of the literal `qa.table.edited`, which is in the window anyway as the `evaluate` argument. Both are now asserted on the construct — the node-scoped selector with `${nodeId}` interpolated, and the `f.value === value` comparison — because a value is not in the report because it was typed into it. That is the fourteenth reading in this REQ that was green against a defect entirely unchanged, and the first two caught in the file that exists to prevent them.

**Proof:** 326 admin tests (317 → 326, +9) · `pnpm typecheck` 2/2 · `node --check` clean (14,372 → 14,410) · **eight mutations red**, each naming its assertion. **Not ticked: the reading is still unmeasured** — no browser pass (the slot's holder is a LIVE w6 pass, pid 2887474, `/proc/2887474/cwd` = `/mnt/apopic/omnion-w6`, 200 artifacts written in the ten minutes before this tick opened), and the pass must read `builderSeesTableEdit` alongside `inspected > 0` — the first alone is satisfied by a read that inspects nothing, which is the shape the whole file is about.
- [ ] A keyboard-only pass adds two nodes, connects them, edits a parameter, validates and runs, with the pointer untouched. — **BUILT, not ticked** (`b2d7b7a`). The criterion is five verbs in a row and **four of the five already had a key**; the fifth was bound to a 10px port dot you have to aim a mouse at. A keyboard author could place two nodes perfectly and then be unable to make a rule out of them, and the graph would then validate with "trigger has no connection" — the one error whose cause is invisible on a screen that looks finished. **Enter is the load-bearing guard.** It also activates whatever is focused, so consuming it unconditionally fires a connect attempt out of every inspector field. It commits only while a gesture is genuinely in flight, and a *refused* gesture counts as finished rather than pending, or the next Enter commits one behind the author's back. **`preventDefault` is per-case, not per-intent.** Escape is shared with the pointer gesture; preventing the default on read and deciding afterwards that the key was not ours leaves the browser's own Escape cancelled by a shortcut that did nothing. Each case claims the key itself, and the pointer handler asks `keyConnect` first. The three refusals are **stated, not swallowed** — self-connection (the first thing a double-tap tries), duplicate, and a source with no output port — because a key that does nothing gets pressed again harder, and a self-connection caught only by Validate is a cycle the author built by accident. The committed edge goes through `commit`, not `setEdges`, so a keyboard connection is one undoable step like a pointer one. `I` focuses the inspector's first *input* (focusing a heading is the difference between a shortcut that works and one that leaves the caret nowhere); `V` and `R` reach validate and run through a ref written every render, because the handler is built before those callbacks exist and a stale `runOnce` would start the run the graph had *before* the last edit — the one failure that looks like the feature working. The pass is also **data** (`KEYBOARD_PASS`): a sequence is the one thing unit tests are worst at, since each step can pass while the path between them is broken, and a criterion that lists its own instrument is the one that cannot be argued about later. **Not ticked until the pass drives that list with no pointer event at all** — two nodes added, an edge in the *server's* copy after save, a parameter read back from the server, findings rendered, a run started. The edge and the parameter are read from the **server**, not from the canvas: a keyboard path that drew an edge without committing it would pass every DOM assertion and fail the criterion.
- [ ] Below 1024px the builder is read-only with the banner, Table mode stays editable, and no control is unreachable. — **BUILT, not ticked** (`b2d7b7a`). Three claims, and the interesting one is that **a pointer-only lock has a hole exactly the size of a Bluetooth keyboard**: a phone or a tablet with a case gets the full shortcut set on a screen the banner calls read-only, and `Del` deletes a node. So `isReadingKey` is a **whitelist** — navigation and inspection, never mutation — and a shortcut added next year mutates by default and has to be added to the read set deliberately. Every `⌘` chord is refused for a specific reason: `⌘Z` on a screen where nothing was written is at best a no-op and at worst a stale undo from before the lock. **`inert` on the editing regions, not a pile of `disabled`s.** One attribute takes a whole region out of the tab order and out of hit testing, which is exactly the "no control is unreachable" half of the criterion; twelve `disabled` attributes would each have to be kept in step with a new palette entry, and the thirteenth control added later would be the reachable one nobody thought about. **Locked does not mean inert everywhere.** The inspector goes inert only when a node is selected, because with nothing selected it holds the read-only rule settings — run-as, rate limit, error policy — which is precisely what a narrow-screen reader came for. Pan and zoom stay live, and a card can still be *selected*, because the problems panel's jump links and the node's read-only state are how a phone answers "what is this?". Selection yes, drag no: the read-only branch returns *after* the selection is recorded, and that order is the only one satisfying both halves. An in-flight drag started before the window narrowed is dropped too, since a browser does not cancel a pointer capture on a media-query change and a phone rotated mid-drag would otherwise finish a move nobody can see or undo. **Unknown width is wide.** `viewportWidth` is `null` until measured and the lock treats that as editable: a reader on a slow device costs one stray edit, whereas locking on `null` flashes the banner at a desktop author for one frame. The banner sits *above* the toolbar rather than inside it, so the buttons the author is looking for never move, and it links to Table mode because a lock with no way out is a dead end. `matchMedia`, not `resize`: the criterion is about a breakpoint and `matchMedia` is the only source that agrees with the CSS query to the pixel. The comparison is `>= 1024`, and a test fails if it is ever turned into `<=` — that off-by-one is visible only in a screenshot taken at exactly 1024, which is why it gets a test instead of a careful read. **Not ticked until the pass resizes the builder below 1024 and reads `data-builder-locked="true"` with the banner present, then proves each of add / drag / connect / delete / `Del` produces no change to the server's copy, that a card is still selectable and the inspector still shows the node's read-only state, and that the Table-mode link from the banner lands on a page that *does* save** — a lock that locked Table mode too would satisfy "read-only" and fail the criterion.
- [ ] Empty, loading and error states exist on every screen; no dead buttons; `cargo test --workspace`, `pnpm typecheck && pnpm build` and the QA walkthrough are green with zero high findings. — **the empty/error half is now *announced*, which is a different claim from rendered and the one this box was quietly not asking.** Every state on the builder was drawn and every one of them was silent: an author who cannot see the problems panel and cannot hear that the save was refused has been told nothing by either channel. The three states this slice touched — save refused, connection refused, the narrow-screen lock — are the three where a mouse author reads text and a keyboard author gets nothing, and all three were `role="status"` on an element that only existed *while* the state did. **Not ticked:** the criterion also names `cargo test --workspace`, `pnpm build` and a green walkthrough, and none of the three has been run for this slice; this branch still cannot run the DB integration tests at all (missing migrations `0019`/`0022`, owned by other branches).

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

   **`⌘/` help 2026-09-29** (`3ae8e19`). Built, and the guard is the substance of it. The
   rows live in a **catalogue** (`SHORTCUT_GROUPS`) rather than being written into the overlay,
   because a help list is the one screen whose value is exactly as fresh as its last edit: it
   is correct the day it is written and silently incomplete the day a shortcut is added, and
   no test that presses the documented keys can see that. A unit test therefore reads the
   **canvas source** and fails on a chord with no row — proved by injection, not by assertion
   (a `⌘K` branch was added, the test named it, the file reverted clean). Two of the six new
   tests failed first and **both were the test's fault**, which is the only way to read a new
   test failing: one asserted `key: "Slash"` (that is `event.code`; binding it makes the
   shortcut layout-dependent), the other demanded `KEYMAP.cancel`'s raw `"escape"` against a
   row written `Esc`. The first fix made the assertion loose enough to pass a row for the
   wrong key, so the alias table replaced it. **The chord could not have been read where the
   single-key path is** — that block only runs with no modifier held, the same guard
   `readKey` applies — so `⌘/` is wired beside the other chords, and Escape closes the overlay
   *before* the canvas's three-step ladder, because a modal the keyboard cannot dismiss fails
   the keyboard-only criterion on the one screen that teaches the shortcuts. Locked rows say
   they are refused below 1024px: `isReadingKey` is a whitelist, so a shortcut added next year
   is refused by default and a phone author needs to know which keys will not work.
   **Not ticked until the pass reads `shortcut-help` with `openedByChord`, `closedByEscape`,
   `togglesOnTheSameKey` and `marksTheLockedRows`** — the pass is queued behind a live w8
   walkthrough, so a screen built this tick has not been in a browser yet.

   **Tab walk 2026-09-30** (`e30f96cb`). Built, and the defect was that the list was **lying**.
   The shortcut row said "Walk to the next card" and this criterion's own script says `Tab`/
   `ArrowRight` walks the selection onto a connection's target — and `onCanvasKeyDown` bound no
   `Tab` case at all. Every card is `tabIndex={-1}` (the canvas owns its focus ring, correctly),
   so the browser moved focus out to the next toolbar control and the selection never moved.
   `selection.ts` exported `focusOrder` and a unit test asserted its shape: **the walking order
   was written, tested and never called**, which is why two ticks of shortcut-catalogue work
   never noticed — the drift guard asks "does every key the handler binds have a row?", and a
   row for a key *nothing implements* passes it. The guard had no arm for that direction.

   Three questions the walk must answer, each a quiet way to be wrong: **where it starts** (the
   *focus*, not the group — a Shift+clicked group outlines several cards and resuming from the
   group skips them), **which way** (one rotation, not a scan; the test proves forward and
   backward are exact inverses at every id, since a rotation whose backward half is not the
   inverse of its forward half still looks plausible and only the wrap point disagrees), and
   **whether the key is ours at all** (a Tab inside a field is the field's — and this is what
   keeps the criterion satisfiable, because `I` focuses the inspector's first input, so a walk
   that ate Tab there would make "edits a parameter" impossible while looking like a broken
   shortcut). An edge is walkable, and that is *this* criterion's reason for it: "Del on a
   selected edge" is only satisfiable from a keyboard if a keyboard can reach an edge. The `<g>`
   gains `tabIndex={-1}` and an aria-label naming both ends; a landed edge is selected the way
   a click selects it, so `Del` removes the line the author is looking at.

   **The new guard is the missing arm, and it failed three times before it was right — every
   failure being the *instrument* blind rather than the product.** A key can be bound literally
   (`event.key === "Delete"`), **delegated** to a module predicate (`readKey`,
   `shouldWalkCanvas`) or **table-indexed** (`nudge[event.key]` for the arrows, which is not a
   comparison at all and a text search cannot see). The first version read `Enter` as unbound;
   the second read `Arrows` as unbound. A module now *declares* what it owns in `WALK_KEYS`
   rather than being added to an exception list, and a delegation counts only while the handler
   still calls it — asserted, so the check is not a rubber stamp.
   **The reachability assertion is stricter than it looks, and an injection is why.** Gating
   the case on a constant false left the call's text in the file and the first assertion
   passed **11/11 on a Tab that did nothing** — the exact defect reproduced inside the
   instrument. It now requires the call to be the *condition* of an `if`, and both injections
   (dead branch, full removal) turn it red. That is the honest limit of a text guard, and it is
   stated in the test rather than overclaimed: this stops the binding being *removed*; only a
   browser can prove Tab moves a selection.

   **Not ticked until the pass drives `Tab` on the canvas** and reads the selection and the
   focus ring landing on the same card, a second press reaching a connection, `⇧Tab` returning,
   and a Tab inside the inspector's field leaving the field rather than walking.

   **The accessibility assertions 2026-09-30** (`635c5191`). Built, and the defect was that the
   builder had three live regions that a screen reader would never have heard from — **all three
   were mounted conditionally**, which is the quiet way to have no live region at all. The save
   indicator rendered a *different element* per state, so `dirty → saving → saved` swapped the
   node three times and announced nothing; the only branch carrying a role was `conflict`, the
   branch that appears after a second tab has already overwritten the author. The connection
   notice had the same shape. The narrow-screen banner was `role="status"` and absent from the
   tree on every screen *wider* than the breakpoint — which is the moment a reader most needs
   it, because a narrow screen is usually narrow **before** the page loads. A live region is
   announced when its content changes *while it is already in the accessibility tree*, so the
   fix is structural: one permanent `LiveRegion` whose text changes, `polite` for the everyday
   transitions and `assertive` for the two that mean work was or is being lost — politeness
   follows consequence, not urgency, because "Saved" must not talk over the author's typing.

   The canvas also had nothing to say about itself. A card is `role="button"` with a truncated
   label and `tabIndex={-1}`, so a reader heard "Send mail, button" — no node type, no
   parameters, and **nothing at all when Tab moved the selection**, because an outline is a CSS
   class and not a text change. Cards now carry a name from `cardAnnouncement` (label, node
   type, the kind the palette already calls it, selection state, parameters) and a third region
   announces what `Del` would remove. A parameter that is *unset* is said as unset: `""`, `null`
   and `undefined` all render as nothing on the card, and a reader that skipped them would hear
   "to:" and stop, which reads as a broken render rather than a field nobody filled in.

   **The honest limit: a unit test cannot prove a screen reader speaks.** These are assertions
   about the DOM and about what the rules produce; only a reader can confirm the speech, and
   this repo has no assistive technology in CI. The probe is wired (`builder-announcements`) and
   has **not run** — the pass in flight started before this commit.

   **Still open in this slice:** the sample-plugin *run* (the palette half and the
   honest-validation half are built and unticked) — and it is **blocked, not merely unfinished**.
   The criterion needs a plugin that *contributes a node and runs*, and `plugins_enabled_for` is
   the seam **REQ-121** fills (wave 5b, unclaimed). It returns an empty registry today, so no
   node can appear in any browser, and the palette probe measures that honestly rather than
   hard-coding `badgeRendered: true` for a store that does not exist. Building a plugin store
   here would be taking a slice of wave 5b, which is not this branch's.

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
