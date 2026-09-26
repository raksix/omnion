# REQ-093 — Execution History & Debugging UI

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** admin + `crates/workflows`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

Finding out what a run actually did.

- Executions list: status, duration, started by, trigger, workflow version, filters and search.
- Execution detail: per-node input/output, timing, error, and the item lineage through nodes.
- Run-to-node / partial execution from the canvas ("execute step", "execute to here").
- Retry from a failed node; re-run a single execution with pinned data.
- Payload storage strategy and retention settings.

## Implementation spec

### Scope (in / out)

**In**

- **Executions list (cross-workflow and per-workflow)** — columns Status, Workflow, Started, Duration, Started by, Trigger, Version, Nodes ok/failed, Items, Error summary. Filters: workflow, folder or tag, status (running, succeeded, succeeded with errors, failed, cancelled, waiting), trigger kind, started-by, environment, date range, error code, free text over execution id and workflow name. Cursor pagination, sortable columns, personal saved views, and a bulk action bar (Cancel selected running, Re-run selected, Export list as CSV).
- **Execution detail** — run header (workflow, version, trigger, triggered by, environment, timings, totals, links to parent and child runs); a duration-bar timeline of nodes with hover details; one card per node holding resolved input, output, timing, attempts, and error rendered human-first with the code second; sub-execution links; wait state with its due time; version chip linking to the version diff (REQ-095); clear labels when input came from pinned data or a mock (REQ-092).
- **Item lineage** — for each step, the mapping from input item index to output item index (which incoming item produced which outgoing item), stored per item, with an "Explain item" view that walks one item through every node that touched it. Items that were dropped or produced by a fan-out are shown as explicit gaps, not silence.
- **Run to node / partial execution from the canvas** — node context menu gains **Execute step** (single node), **Execute to here** (run the graph up to and including this node, with a choice of *live inputs* or *last-run inputs*), and **Execute from here** (start at this node using its upstream data as recorded). All three are built on the run filters shipped in REQ-091 and produce a real run with a real trace; the confirmation step names the nodes that will run and warns when a node's side effect is declared non-idempotent.
- **Retry from a failed node** — one action that re-runs the first failed node and everything downstream of it, keeping succeeded steps and their recorded results; it is a new execution linked to its source. Per-step retry (REQ-091) stays available for a single node.
- **Re-run with pinned data** — re-runs an existing execution's inputs as recorded (payload snapshot) or with the node's pinned data instead of the live trigger, always as a new linked execution, never mutating the original.
- **Payload storage strategy** — step input/output is stored inline up to a per-organization threshold (default 64 KiB per item, 512 KiB per step); anything larger is offloaded to object storage and the row keeps a reference, a byte count and a signed, short-lived download link. Oversized payloads are truncated for display with an explicit marker and the original size, never silently shortened. Secret-looking values are redacted by key name before storage (token, password, secret, authorization, api key, cookie).
- **Retention** — per-organization windows: keep succeeded runs N days (default 30), keep failed runs longer (default 90), payload retention shorter than row retention (default 14 days), a total size budget, and a batch size for pruning. Pruning runs as two stages — payloads first, then rows — so the list and traces degrade gracefully. A usage panel shows rows, bytes and the oldest retained run, with a preview-then-apply manual prune.
- **Export** — one execution as JSON (definition snapshot, node results, item errors) with redaction applied, and a list export as CSV.

**Out**

- Engine internals, guards, ledger and sweeper behaviour — REQ-091.
- Dashboard widgets and alerting on run health — REQ-007 and REQ-014 own them; this request supplies the data they read.
- The canvas itself, its layout and rendering — REQ-004; this request adds three context-menu actions and the API calls behind them.
- Workflow version diffs and restore — REQ-095 (the version chip links into it).
- Human-in-the-loop wait decisions — REQ-090.

### Screens (UI)

- **`/automations/executions`** — the cross-workflow list above, with a summary strip (running now, failed today, median duration, success rate 7d), saved views in a side rail, filter chips that survive reload, and a row menu (Open, Re-run, Re-run with pinned data, Cancel, Export JSON, Delete payloads).
- **`/automations/[id]` → Runs tab** — the same table scoped to one workflow, plus a per-version breakdown so a regression after publish is visible at a glance.
- **`/automations/[id]/runs/[run_id]`** — header, timeline, node cards, lineage table, error panel, actions (Cancel, Retry from failed node, Re-run with same inputs, Re-run with pinned data, Export JSON, Delete payloads), parent/child links, and a right-hand inspector that follows the selected node or item.
- **Canvas context menu (in the builder)** — Execute step, Execute to here (live or last-run inputs), Execute from here, each opening a small confirmation dialog that lists affected nodes, warns about side effects, and offers "run in test mode" where a node supports it.
- **`/settings/workflows/retention`** — retention windows, payload thresholds, size budget, prune batch size, live usage (rows, bytes, oldest run), next prune preview, Prune now, and the offload backend status. Editing states the effect in plain words ("runs older than 30 days stop appearing in the list").
- **States** — empty, loading and error states on both lists; a run with no payloads (pruned) says so instead of rendering blank cards; lineage table scrolls horizontally with its first column pinned; on mobile the list becomes cards and the timeline an accordion while item lineage stays readable.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/workflow-executions` | Cross-workflow list: filters, cursor, sort | `workflows.read` |
| GET | `/api/v1/workflow-executions/{id}` | Run header and totals | `workflows.read` |
| GET | `/api/v1/workflow-executions/{id}/timeline` | Node timings for the duration bars | `workflows.read` |
| GET | `/api/v1/workflow-executions/{id}/lineage?step_no=` | Input-to-output item mapping | `workflows.read` |
| GET | `/api/v1/workflow-executions/{id}/lineage/{item_index}/explain` | One item's path through nodes | `workflows.read` |
| GET | `/api/v1/workflow-executions/{id}/export` | Redacted JSON export | `workflows.read` |
| GET | `/api/v1/workflow-executions/{id}/payloads/{step_no}/{item_index}` | Signed link for an offloaded payload | `workflows.read` |
| POST | `/api/v1/workflow-executions/{id}/retry-failed` | New run from the first failed node | `workflows.run` |
| POST | `/api/v1/workflow-executions/{id}/re-run` | New run: `same_inputs` or `pinned_data` | `workflows.run` |
| POST | `/api/v1/workflows/{id}/execute-node` | Canvas partial run: node, mode (live or last-run), include downstream | `workflows.run` |
| GET/POST/DELETE | `/api/v1/workflow-executions/views` | Personal saved views | `workflows.read` |
| DELETE | `/api/v1/workflow-executions/{id}/payloads` | Delete payloads, keep the row | `workflows.manage` |
| GET/PATCH | `/api/v1/settings/workflows/retention` | Read / change retention and thresholds | `workflows.read` / `workflows.manage` |
| POST | `/api/v1/settings/workflows/retention/prune` | Preview then apply a manual prune | `workflows.manage` |

No new permission keys.

### Data model

Migration `database/migrations/0017_execution_history.sql` (next free number if taken).

Adds to `workflow_executions`: `workflow_version_id uuid` (null until REQ-095 ships its table; the FK is added there), `retried_from_execution_id uuid` self-reference set null, `environment text default 'production'`, `node_count int default 0`, `item_count int default 0`, `error_code text`, `duration_ms int`, `payload_bytes bigint default 0`, `payloads_pruned_at timestamptz`.

| Table | Columns (types) | Indexes / rules |
|---|---|---|
| `workflow_execution_payloads` | id uuid pk, execution_id uuid → workflow_executions cascade, step_no int, item_index int, direction text ('in','out'), storage text ('inline','object'), body jsonb, object_key text, bytes int, original_bytes int, truncated boolean default false, expires_at timestamptz, created_at | unique `(execution_id, step_no, item_index, direction)`; index `(expires_at)` for pruning; check `(storage = 'inline') = (body is not null)`; check `bytes <= original_bytes` |
| `workflow_item_links` | id bigserial pk, execution_id uuid cascade, step_no int, input_index int, output_index int, kind text ('paired','fan_out','dropped') | index `(execution_id, step_no)`; unique `(execution_id, step_no, input_index, output_index)` |
| `workflow_retention_settings` | id uuid pk, organization_id uuid → organizations cascade unique, keep_succeeded_days int default 30, keep_failed_days int default 90, payload_keep_days int default 14, payload_inline_max_bytes int default 65536, step_inline_max_bytes int default 524288, size_budget_bytes bigint default 21474836480, prune_batch_size int default 500, updated_at | range checks on all integers; `payload_keep_days <= keep_succeeded_days` |
| `workflow_saved_views` | id uuid pk, organization_id uuid, user_id uuid → users cascade, name text, query jsonb, created_at | unique `(user_id, name)`; check `length(btrim(name)) > 0` |

Indexes on existing tables: `(organization_id, started_at desc)`, `(status, started_at desc)` partial where running, `(triggered_by, started_at desc)`, `(error_code)` partial where not null, and an expression index on lower(workflow name) for list search.

### Events

| Event | Kind | Notes |
|---|---|---|
| `workflow.execution.payload_pruned` | emitted | execution id, bytes freed; row kept |
| `workflow.execution.exported` | emitted | actor, execution, redaction count |
| `workflow.execution.retried` | emitted | links source and new execution |
| `workflow.execution.rerun_requested` | emitted | mode (`same_inputs`, `pinned_data`) |
| `workflow.partial_execution.started` | emitted | node, mode, whether side effects were confirmed |
| `workflow.retention.settings_updated` | emitted | actor and changed fields |
| `workflow.retention.pruned` | emitted | batch summary: rows, payloads, bytes |
| `workflow.lineage.item_dropped` | emitted | debug-level: step where items were dropped (count only) |

Payload bodies, item values and exported content never appear in event payloads.

### Acceptance criteria

- [ ] The cross-workflow executions list and the per-workflow Runs tab both exist, filter by every listed dimension, and keep filters through reload and back navigation.
- [ ] Saved views persist per user with a name, appear in the side rail, and can be renamed and deleted.
- [ ] Run detail renders the timeline, node cards (input, output, timing, attempts, error) and totals for a run with at least 12 nodes.
- [ ] Item lineage shows which input item produced which output item for a mapping node, marks fan-out and dropped items explicitly, and Explain item walks one item across nodes.
- [ ] A large payload (above the inline threshold) is offloaded, and its signed link expires — an expired link returns a clear message, not a stack trace.
- [ ] A payload above the display cap is truncated with the original size shown; the trace never pretends truncated data is complete.
- [ ] Secret-looking keys are redacted before storage: a node whose output carries an authorization header stores it redacted and the export shows the redaction.
- [ ] Execute step runs exactly one node and produces a real trace; Execute to here stops at the node and reports the nodes that did not run; Execute from here reuses recorded upstream data.
- [ ] Partial execution asks for confirmation when a node declares a non-idempotent side effect, and refuses silently to run without it.
- [ ] Retry from failed node re-runs only the failed node and its downstream, keeps earlier results, and links the new execution to its source.
- [ ] Re-run with same inputs reproduces the original inputs; re-run with pinned data uses pin data — both shown in the new run's labels, originals unchanged.
- [ ] Environment and version columns are present and correct on both lists, and the version chip opens the matching diff.
- [ ] `/settings/workflows/retention` shows live usage (rows, bytes, oldest run); changing a window updates the preview without applying anything.
- [ ] Prune now previews affected counts and applies exactly those on confirm; stage order is payloads then rows, and the events match the counts.
- [ ] Delete payloads removes payload rows for one execution, keeps the trace readable, and marks the run as pruned.
- [ ] A pruned run renders its metadata with an explicit "payloads pruned" note on every step card.
- [ ] `cargo test --workspace`, `pnpm typecheck && pnpm build` and the QA walkthrough pass with zero high findings; the redaction and prune tests fail when their code is reverted.

### QA plan

The walkthrough must: produce runs of each status (succeeded, succeeded with errors, failed, cancelled, waiting) and confirm the summary strip and filters; open a multi-node run and read timeline, node cards, error panel and totals; trace one item through a mapping node and a fan-out node and read the explained path; force a payload above both thresholds and open the offloaded and the truncated variants; run Execute step, Execute to here and Execute from here from the canvas, once with a non-idempotent node to see the confirmation; retry from a failed node and re-run twice (same inputs, pinned data) and compare the three traces; export JSON and CSV and check redaction; change retention settings, preview a prune, apply it, and re-open a pruned run; delete one execution's payloads; finish with the mobile pass over list cards, timeline accordion and lineage table.

The visual check must see: duration bars proportional and labelled, error text wrapping inside cards, the lineage table's pinned first column, filter chips not colliding with the summary strip at 1280px, and pruned or truncated markers visible without scrolling.

### Slices

1. **Lists, filters and views** — cross-workflow list with all filters, cursor pagination, summary strip, saved views, CSV export, per-workflow Runs tab columns and per-version breakdown.
   *Done when:* every filter returns the expected fixture set and a saved view survives a reload unchanged.
2. **Run detail, timeline and lineage** — header, timeline, node cards, error rendering, item links and Explain item, parent/child links, pinned and mock labels.
   *Done when:* an item traced through a three-node chain reports the correct indices at each hop and dropped items are shown as dropped.
3. **Partial execution and re-runs** — Execute step / to here / from here, confirmation and side-effect warnings, retry from failed node, re-run with same inputs and with pinned data, linked executions.
   *Done when:* a partial run's trace proves only the intended nodes executed and a retry re-uses earlier results without repeating a side effect.
4. **Payloads and retention** — payload storage with offload and truncation, redaction, signed links, retention settings screen and endpoints, two-stage prune, usage panel, payload deletion.
   *Done when:* the prune preview numbers equal the applied numbers and a pruned run still renders completely.

### Risks / notes

- Payloads are the privacy surface: redact by key name before storage, keep signed links short-lived, and let retention delete payloads without deleting the audit trail.
- History reads grow with volume — keep list queries on covering indexes and never join payloads into a list response.
- Partial execution must never become a permission bypass: a node reached off the normal path still evaluates the run-as account's authority.
- Re-runs must be new executions with a link to the source, or audit trails become unreadable.
- Retention defaults must be visible and editable per organization; a silent 30-day deletion is a support incident waiting to happen.
- Lineage only helps if it covers every node kind: a node that does not report pairing must show "not reported" rather than implying a clean 1:1 mapping.
