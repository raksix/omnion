# REQ-086 — Workflow Editor Canvas

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** admin (`apps/admin`) + `crates/workflows`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

The visual place where workflows are built.

- Node canvas: pan/zoom, grid, multi-select, copy/paste, undo/redo, auto-layout.
- Node palette with search, categories and drag-to-add.
- Connection editing with type-aware validation (data vs control flow), connection labels.
- CodeMirror 6 code editing inside Code nodes with syntax highlighting.
- Expression editor with autocomplete over upstream data, inline preview of evaluated values.
- Sticky notes, node renaming, comment nodes, and a minimap for large graphs.

## Implementation spec

### Scope (in / out)

**In**

- Graph as the authoring form: nodes (key, type, label, position, params, disabled) and connections
  (from node + output port, to node + input port, optional branch label) in a `graph` document. Saving
  compiles the graph into the existing `steps` array and writes both, so the editor needs no engine change.
- Canvas: wheel/pinch zoom, drag and `space`-drag pan, fit-to-view, zoom to selection, grid with snap
  toggle, alignment guides, and a layered left-to-right auto-layout that preserves manual positions.
- Selection and history: click, shift-click, marquee, select-all, delete, duplicate, copy/paste
  (including across workflows), arrow-key nudge, align/distribute, undo/redo of at least 100 operations
  with drag coalescing and a named toast ("Undo: connect Check role → Email").
- Connections: drag from a handle with valid-target highlighting, reconnect by dragging an endpoint,
  delete on selection, double-click to label (`true` / `false` / `case A` / `error`), and rejection of
  self-loops, duplicates and control-flow cycles with a named reason.
- Palette: search over label and key, category tree (trigger, flow, code, data, integration, helper,
  error), drag-to-add with an insertion ghost, click-to-add at viewport centre, recents, and nodes
  disabled with their cause ("needs a credential", "node package not installed").
- Node chrome: inline rename (`F2`), inspector on double-click, badges for retry, disabled, pinned,
  last-run state and item count, comment callouts, and sticky notes (colour, resize, text, never executed).
- Embedded editors: CodeMirror 6 for code nodes (javascript, python, json) with highlighting, bracket
  matching, line numbers, in-editor search and a diagnostics gutter from static validation (REQ-088); an
  expression field with highlighting, autocomplete from upstream outputs and variables, drag-a-field
  mapping, and an inline preview of the evaluated value for pinned sample data (server-side, REQ-092).
- Execution overlay: per-node state (running, succeeded, failed, waiting, skipped) with duration and
  item counts, connection activity, a run bar with cost-free run summary, and the `Execute step` /
  `Execute to here` entry points owned by REQ-093.
- Accessibility and responsive: full keyboard path (tab order, arrow movement, `Enter`, `⌘K` command
  palette, `Esc`, `⌘/` shortcut sheet), visible focus rings, theme contrast, reduced-motion mode, and a
  read-only canvas at ≤ 900 px with pan, zoom, fit and node inspection.

**Out**

- Expression language, sandbox and variables (REQ-092) — the canvas embeds the editor and calls preview.
- Execution history, item inspection, payload retention and partial-run internals (REQ-093).
- Versions, diff, restore, folders, sharing and the edit lock (REQ-095); the canvas writes the working graph.
- Trigger configuration (REQ-089) and templates (REQ-094) — trigger nodes are placed here and configured there.
- Any dynamic execution in the browser: previews and validation are server calls only.

### Screens (UI)

| Route | Screen |
|---|---|
| `/workflows` | Workflow list with graph indicator, trigger summary, last run, **Open editor** |
| `/workflows/new` | Blank canvas, palette open, trigger placeholder selected |
| `/workflows/<id>/edit` | Canvas editor — the default landing for a workflow |
| `/workflows/<id>/runs` | Run list for the workflow (REQ-093, linked from the canvas header) |

- **Layout.** Left rail: palette (collapsible). Centre: canvas with a toolbar (undo, redo, auto-layout,
  fit, zoom, grid, minimap) and a run bar (status chip, `Run`, `Stop`, `Execute step`, `Execute to
  here`). Right: inspector tabs — Parameters (schema-generated, REQ-087), Settings (label, notes,
  disabled, retry, continue-on-fail), Data (upstream sample, pinned items, drag handles), Errors.
  Bottom strip: node/connection counts, validation count, autosave state, last saved.
- **States.** Empty graph offers "start from a template" and "start from a trigger". Validation issues
  badge the node and list in a panel that jumps to it. Dirty state shows an unsaved chip; save is `⌘S`
  with a 2 s autosave debounce, and a failed save keeps canvas state with a retry banner.
- **Conflict and lock.** Save sends the loaded revision; a mismatch returns a conflict and the editor
  offers compare-and-reload instead of overwriting. A second editor shows the lock holder and opens read-only.
- **Minimap.** Toggleable, nodes coloured by last-run state, click to centre, hidden below 20 nodes.
- **Keyboard.** `⌘K` palette · `⌘Z`/`⌘⇧Z` undo/redo · `⌘C`/`⌘V`/`⌘D` · `⌘A` · `⌘S` · `⌘0` fit ·
  `⌘⇧L` auto-layout · `F2` rename · `Enter` inspector · `Delete` · `Space`+drag pan · `Esc` cancel.
- **Mobile.** Read-only at ≤ 900 px (pan, zoom, fit, node sheet, run status) with an "open on a wider
  screen to edit" banner; the list screen stays fully usable.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/workflows/{id}/graph` | Graph document plus revision for the editor | `workflows.read` |
| PUT | `/api/v1/workflows/{id}/graph` | Save the whole graph, compile to steps (`If-Match` on the revision) | `workflows.manage` |
| POST | `/api/v1/workflows/{id}/graph/validate` | Compile and validate without saving; per-node issues | `workflows.read` |
| GET | `/api/v1/workflows/{id}/graph/upstream/{node_key}` | Upstream sample data (last run or pinned) | `workflows.read` |
| POST | `/api/v1/workflows/{id}/graph/expressions/preview` | Evaluate expressions against a node's sample data | `workflows.read` |
| POST | `/api/v1/workflows/{id}/graph/layout` | Server-side layered layout for very large graphs | `workflows.manage` |
| GET | `/api/v1/workflows/{id}/runs/latest` | Latest run state for the overlay | `workflows.read` |
| POST | `/api/v1/workflows/{id}/run` | Start a run from the canvas (existing, REQ-003) | `workflows.run` |
| POST | `/api/v1/workflows/{id}/runs/partial` | Execute step / execute to here (shared with REQ-093) | `workflows.run` |

Validation codes: `node_unknown_type`, `node_duplicate_key`, `node_param_required`, `node_param_invalid`,
`connection_port_unknown`, `connection_type_mismatch`, `connection_duplicate`, `connection_cycle`,
`graph_unreachable_node`, `graph_no_trigger`, `graph_terminal_missing`.

### Data model

Migration `0030_workflow_graph.sql` (reserved band 0030–0039 for the workflow editor family,
REQ-086–096; append-only ledger — take the next free number if taken).

```sql
alter table workflows
    add column graph jsonb not null default '{"nodes":[],"connections":[],"notes":[]}',
    add column graph_revision integer not null default 0,
    add column graph_updated_at timestamptz,
    add column graph_updated_by uuid references users (id) on delete set null;

alter table workflows add constraint workflows_graph_shape check (
    jsonb_typeof(graph -> 'nodes') = 'array' and jsonb_typeof(graph -> 'connections') = 'array'
    and jsonb_typeof(graph -> 'notes') = 'array');
create index workflows_graph_nodes_gin on workflows using gin ((graph -> 'nodes') jsonb_path_ops);
```

Node keys are stable editor strings (`http_1`, `if_2`) so connections survive edits; positions are floats.
The compiler is deterministic (same graph → same steps) and expands fan-out nodes into several steps at
compile time. Viewport and panel sizes are browser-local, never stored. Existing rows keep an empty graph
and keep running from `steps`.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `workflows.graph.saved` | Save succeeded | `workflow_id`, `revision`, `node_count`, `actor_user_id` |
| `workflows.graph.compiled` | Compiled with no errors | `workflow_id`, `revision`, `step_count` |
| `workflows.graph.validation_failed` | Validation issues returned | `workflow_id`, `code_counts` |
| `workflows.graph.layout_applied` | Server-side layout replaced positions | `workflow_id`, `node_count` |

Consumed: `workflows.execution.state_changed`, `workflows.execution.finished` (overlay refresh),
`node_packages.installed`/`.removed` (palette availability), `workflows.credential.*` (node enablement).

### Acceptance criteria

- [ ] Saving writes graph and compiled steps; a two-node graph runs end to end through the existing engine.
- [ ] A stale-revision save returns a conflict and the editor offers compare-and-reload, never overwriting.
- [ ] Pan, zoom, fit and zoom-to-selection work with mouse, trackpad and keyboard; grid snap off changes nothing stored.
- [ ] Auto-layout arranges a 25-node graph without overlaps, and manually moved nodes keep positions after reload.
- [ ] Copy/paste duplicates a multi-node selection with new keys and internal rewiring, dropping outside edges.
- [ ] Undo/redo restores 100+ operations including a drag, a connection deletion and a param edit.
- [ ] A data output to a control input is refused with `connection_type_mismatch`; self-loops and control cycles with `connection_cycle`.
- [ ] Branch labels survive save/reload and appear on the canvas and in the run overlay.
- [ ] The palette searches, groups by category, opens on `/`, and disables nodes with the missing cause named.
- [ ] The code node highlights javascript and python, shows validator diagnostics, and never runs code in the browser (network trace checked).
- [ ] Expression autocomplete lists upstream outputs, variables and the current item; preview returns real pinned values or a positional error.
- [ ] Sticky notes and comments persist, do not appear as steps and never execute.
- [ ] A real completed run colours nodes with status, duration and item counts; failed and skipped look distinct.
- [ ] `⌘S` and the autosave debounce both persist; a blocked save keeps state and offers retry with no loss.
- [ ] The canvas is operable end to end from the keyboard and the shortcut sheet matches the implemented bindings.
- [ ] At 390 px the editor is read-only with working pan and node inspection, and the list has no horizontal scroll.
- [ ] A 100-node graph stays interactive on the reference machine and the walkthrough reports zero high findings.

### QA plan

Seed an empty workflow and one with a trigger plus five nodes and a sticky note. Walkthrough: open
`/workflows/new`, drag in a trigger, click-add `if` and `http_request`, connect, label branches, rename,
edit a param with an invalid value (expect a field message and badge), duplicate a two-node selection,
undo/redo, auto-layout, fit, minimap, sticky note, `⌘S`, reload and confirm positions, labels and notes
survived, then run and inspect overlay states. Open the workflow in a second session for the conflict
path and at 390 px for the read-only banner. Visual check: real icons and labels (no placeholder boxes),
visible selection and focus, legible badges, working minimap, overlay colours matching the real run.

### Slices

1. **Graph model, save, compile** — column, revision check, compiler, validation codes, events.
   Done: a saved graph runs on the existing engine and an unreachable node is rejected.
2. **Canvas interactions** — pan/zoom, selection, clipboard, undo, auto-layout, minimap, palette, node chrome.
   Done: the walkthrough passes visually and by keyboard.
3. **Connections and editors** — type-aware connects with labels, CodeMirror, expression field with preview.
   Done: mismatched connects refuse, both languages highlight, preview returns real values.
4. **Run overlay and polish** — per-node states, run bar, partial-run entry points, responsive and a11y pass.
   Done: a real run colours the canvas correctly and the accessibility checklist passes.

### Risks / notes

- Graph and stepping can drift if one is written without the other; the save transaction writes both and
  the deterministic compiler makes drift detectable.
- Large graphs are the performance risk: a full `jsonb` write per save. Debounce, revision check and a
  size warning; no per-node write endpoint in v1.
- Cycle detection belongs in the compiler, so imported graphs (REQ-094) hit the same wall as the editor.
- Browser clipboard payloads hold node params, never credentials (REQ-087 keeps secrets out of params).
- Loop and batch fan-out need compile-time expansion, so REQ-088 must land close behind this one.
- The overlay reads run state from the API rather than optimistic local state, so a resumed node cannot
  leave a stuck spinner (docs/09-N8N-TEARDOWN.md §13, lesson 7).
