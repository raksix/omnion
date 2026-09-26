# REQ-088 — Core Node Families

> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** `crates/workflows`
> **Source:** deep documentation pass — features named in docs/01–09 that had no request yet

## Request

The nodes every workflow needs.

- Flow control: If, Switch, Filter, Merge, Split-in-batches, Loop-over-items, Wait-for-multiple.
- Code nodes: JavaScript and Python with static validation and result-shape validation.
- HTTP Request node (auth, pagination, retry, binary), SSH tunnel helper, file-system helper, data-table helper.
- Deduplication helper; binary data helper; date/time and crypto helpers.
- Error-handling nodes: Stop-and-error, Continue-on-fail, Error trigger.

## Implementation spec

### Scope (in / out)

**In**

- **Item model**: every step exchanges item arrays `{json, binary?, paired?, error?}`; outputs are addressed by port (`main`, `error`, numbered branches) and `paired` lineage is carried across fan-out and fan-in so an error names the input index that caused it (docs/09-N8N-TEARDOWN.md §13, lesson 6). Payloads above the inline threshold spill to object storage and travel as references, so a large node cannot exhaust runner memory.
- **Flow control family**, each node routing items rather than running them: `if` (one condition set, two labelled outputs), `switch` (n rules plus an explicit fallback), `filter` (drops items, keeps lineage), `merge` (append, merge-by-key, keep-matching, keep-non-matching, combine-all-pairs), `split_in_batches` (fixed size, sequential in v1), `loop_over_items` (collection expression, body sub-graph, `$loop.index`/`$loop.item`, iteration cap) and `wait_for_multiple` (join on N inputs or a timeout with partial output). Loops carry a cap and the engine's endless-loop guard applies.
- **Code family**: `code_javascript` and `code_python` with modes `run once for all items`, `run once per item`, `run once per item with shared state`; static validation before save (syntax, disallowed APIs, module allow-list), result-shape validation after run (item array or single object, named errors otherwise) and three distinct limit classes (wall-clock timeout, memory cap, output size cap). Execution is out of process in a runner with no credentials, no database and network denied unless allow-listed — dynamic code never runs in the core process (lesson 14).
- **Integration family v1**: `http_request` — method, URL template, query, headers, body, auth via a credential key (REQ-087), response format (json/text/binary), timeout, redirects, proxy, TLS toggle with a warning, pagination modes (`none`, `page`, `offset`, `cursor`, `link_header`, `next_url`) with a max-pages cap and stop condition, retry on 429/5xx honouring `Retry-After` with capped jittered backoff, response size cap, and binary responses spilling to storage as binary items.
- **Platform glue**: `ssh_tunnel` (open a tunnel for a following step, credential by reference, closed when the run settles), `file_system` (read, write, append, list, move, delete inside a configured root; traversal refused; size cap), `data_table` (run-scoped scratch rows with upsert by key), `data_store_get`/`data_store_set` (cross-run key/value), and `sub_workflow` (typed inputs, depth cap, no cycles).
- **Helpers**: `deduplicate` (scope workflow / execution / time window, dedup key expression, first or last wins), `binary` (create, normalise, move; filename, mime, checksum), `date_time` (parse, format, add/subtract, timezone conversion, period bounds, now), `crypto` (hash, hmac, base64/hex, uuid v4/v7, random bytes, constant-time compare), `sort`, `limit`, `aggregate` (count, sum, min, max, group-by) and `no_op` as a labelled passthrough.
- **Error family**: `stop_and_error` (named code, message expression, payload attached to the run), `continue_on_fail` as a per-node setting that turns a failure into items on the error port instead of failing the run, and **error-workflow routing** — a workflow declares an error workflow that starts with the failing run's identity, failed node and a bounded error payload.
- **Consistency**: every node declares a params schema (REQ-087), validates its params before execution, resolves expressions through REQ-092, reports its retry policy (attempts capped at five; resumed failures never retried) and records input/output counts plus timing for history (REQ-093).

**Out**

- Durability, queueing, cancellation and the sweeper (REQ-091, REQ-096) — nodes declare policy only.
- Expression language and variables (REQ-092); pinned and mock data storage (REQ-092/REQ-093).
- Triggers (REQ-089), waits and resume (REQ-090), AI nodes and agent tools (REQ-097…REQ-108).
- Third-party node implementations and packaging (REQ-087) and marketplace distribution (REQ-048).
- Execution history UI, partial execution and retention policy (REQ-093); no user code in the browser.

### Screens (UI)

Configuration lives in the canvas inspector (`/workflows/<id>/edit`) plus two test surfaces:

| Route | Screen |
|---|---|
| `/workflows/<id>/edit` | Inspector tabs per family: Parameters · Settings · Data · Errors |
| `/workflows/test/code` | Code-node test: language, mode, sample items, sandbox run, resource usage, shape check |
| `/workflows/test/http` | HTTP preview: resolved request, pagination plan, last-response sample |
| `/workflows/nodes?category=flow` | Library filtered to a family, reachable from "replace node" |

- **Flow panels.** `if`/`filter`: condition builder (field, operator, value, AND/OR groups, drag from the Data tab) with a match-count preview. `switch`: ordered rules with add/reorder/delete and a pinned fallback. `merge`: mode selector plus mode fields. `split_in_batches`: batch size with the resulting batch count. `loop_over_items`: collection, body scope, iteration cap, and a cap-reached warning from the last test run. `wait_for_multiple`: inputs to wait for, timeout, partial-output behaviour.
- **Code panel.** Language and mode selectors, CodeMirror 6 editor, diagnostics strip from static validation, **Run with sample data** showing item count, first items, elapsed time and memory against the caps, and a sandbox notice ("out of process, no credentials, network allow-list only"). Source failing static validation cannot be saved.
- **HTTP panel.** Request builder (method, URL with expression tokens, query and header grids with a secret-value guard, body per content type), credential picker, pagination with cap and stop condition, retry policy, timeout, and **Preview request** showing the resolved request without sending it.
- **Helper, error and data panels.** Small forms with expression inputs and live sample counts ("keeps 3 of 12"); `stop_and_error` shows code and message expression; Settings carries retry policy and continue-on-fail with the run-level consequence spelled out. The Data tab shows upstream items with indices, expanded JSON, binary list and the paired lineage trace, and supports dragging a field into any parameter or supplying sample data before a first run.
- **States and mobile.** Inspector skeletons while loading; a node missing its credential or package renders read-only with the cause. Panels are tablet-usable; at ≤ 900 px the canvas is read-only (REQ-086) so the inspector acts as a viewer.

### API

| Method | Path | Purpose | Permission |
|---|---|---|---|
| GET | `/api/v1/node-types` | Registry with per-family params schemas (REQ-087 owns the route) | `workflows.read` |
| POST | `/api/v1/workflow-code/validate` | Static-validate code source (language, mode, source) | `workflows.manage` |
| POST | `/api/v1/workflow-code/runs` | Run code against sample items in the sandbox | `workflows.run` |
| POST | `/api/v1/workflow-code/runs/{id}/cancel` | Cancel a sandbox run | `workflows.run` |
| POST | `/api/v1/nodes/preview-resolve` | Resolve params and expressions for one node against sample data | `workflows.read` |
| POST | `/api/v1/nodes/http/preview` | Build the resolved HTTP request without sending it | `workflows.read` |
| POST | `/api/v1/nodes/{key}/test` | Execute one node against pinned or sample input; returns items and timing | `workflows.run` |
| GET | `/api/v1/workflows/{id}/runs/{run}/steps/{step}/items` | Step items and lineage (shared with REQ-093) | `workflows.read` |
| GET · PUT | `/api/v1/workflows/{id}/error-workflow` | Read · set or clear the run's error workflow | `workflows.read` · `workflows.manage` |

Codes: `code_syntax`, `code_api_disallowed`, `code_module_denied`, `code_timeout`, `code_memory_limit`,
`code_output_too_large`, `code_result_shape`, `http_timeout`, `http_status`, `http_retry_exhausted`,
`http_response_too_large`, `pagination_cap_reached`, `loop_cap_reached`, `loop_detected`,
`fs_path_denied`, `fs_size_limit`, `tunnel_failed`, `workflow_call_depth`, `workflow_call_cycle`,
`error_workflow_missing`.

### Data model

Migration `0033_workflow_step_data.sql` (reserved band 0030–0039 for the workflow editor family, REQ-086–096; append-only ledger — take the next free number if taken).

```sql
-- step payloads: small ones stay inline, large ones are referenced (backpressure from day one)
alter table workflow_steps
    add column output_ref text, add column output_items integer,
    add column input_items integer, add column bytes_out bigint;

create table workflow_step_data (
    id uuid primary key default gen_random_uuid(),
    execution_id uuid not null references workflow_executions (id) on delete cascade,
    step_id uuid not null references workflow_steps (id) on delete cascade,
    workflow_id uuid not null references workflows (id) on delete cascade,
    node_key text not null, direction text not null, port integer not null default 0,
    item_count integer not null default 0, bytes bigint not null default 0,
    storage text not null default 'inline', object_key text, payload jsonb, sha256 text,
    created_at timestamptz not null default now(),
    constraint workflow_step_data_direction_valid check (direction in ('input', 'output')),
    constraint workflow_step_data_storage_valid check (storage in ('inline', 'object')),
    constraint workflow_step_data_shape check (
        (storage = 'inline' and payload is not null and object_key is null)
        or (storage = 'object' and object_key is not null)));
create unique index workflow_step_data_slot_uid on workflow_step_data (step_id, direction, port);
create index workflow_step_data_execution_idx on workflow_step_data (execution_id, created_at);

create table workflow_dedup_keys (
    id uuid primary key default gen_random_uuid(),
    organization_id uuid not null references organizations (id) on delete cascade,
    workflow_id uuid not null references workflows (id) on delete cascade,
    scope_key text not null,             -- workflow | execution id | time-window bucket
    value_hash text not null,            -- sha256 of the dedup key; raw values are never stored
    first_seen_at timestamptz not null default now(),
    last_seen_at timestamptz not null default now(), hits integer not null default 1);
create unique index workflow_dedup_keys_uid on workflow_dedup_keys (workflow_id, scope_key, value_hash);
create index workflow_dedup_keys_expiry_idx on workflow_dedup_keys (last_seen_at);
```

The inline threshold defaults to 256 KiB per step payload; binaries go through the storage crate (`fs`
or `s3`) with key and `sha256` recorded; retention sweeps follow REQ-093's settings.

### Events

| Event | When | Payload sketch |
|---|---|---|
| `workflows.step.data_spilled` | Payload exceeded the inline threshold | `execution_id`, `step_id`, `bytes`, `item_count` |
| `workflows.step.item_error` | A per-item error was captured | `execution_id`, `node_key`, `paired_index`, `code` |
| `workflows.code.run` · `workflows.code.limit_hit` | Sandbox run finished · a limit hit | `language`, `mode`, `duration_ms`, `limit` |
| `workflows.dedup.skipped` | Dedup helper dropped items | `workflow_id`, `execution_id`, `skipped`, `scope_key` |
| `workflows.pagination.capped` | A paginated HTTP node hit its cap | `execution_id`, `node_key`, `pages`, `cap` |
| `workflows.error_workflow.started` | A failed run routed to its error workflow | `failed_execution_id`, `error_workflow_id`, `node_key` |

Consumed: `workflows.execution.finished` (run-scoped dedup rows and tunnels cleaned up),
`node_packages.removed` (new runs refuse the node instead of failing mid-run).

### Acceptance criteria

- [ ] Every node named in the request exists in the registry with a params schema, asserted by a completeness test.
- [ ] `if` and `switch` route the expected item counts to each labelled output and to the fallback.
- [ ] `filter` drops items while `paired` lineage survives, and a downstream error names the original input index.
- [ ] `merge` append, merge-by-key and both match modes pass their fixture pairs.
- [ ] `split_in_batches` produces correctly sized batches and rejects a size above the cap with `node_param_invalid`.
- [ ] `loop_over_items` iterates a 25-item collection with working `$loop` values; a runaway body aborts with `loop_cap_reached` or `loop_detected`.
- [ ] `wait_for_multiple` completes on all inputs and emits partial output on timeout with the flag visible.
- [ ] Both code languages run out of process (container or process inspection) with no database credentials and network denied unless allow-listed.
- [ ] A disallowed API fails static validation with `code_api_disallowed`; a syntax error fails with `code_syntax` and a line number in the editor gutter.
- [ ] A wrong return shape fails with `code_result_shape`; timeout and memory caps fail with their own codes and are distinguishable in run history.
- [ ] HTTP sends with a credential reference, honours `Retry-After` on 429, exhausts retries with `http_retry_exhausted`, and never logs secret header values.
- [ ] Each pagination mode collects the expected pages against the fixture API; the cap emits `workflows.pagination.capped` and stops cleanly.
- [ ] A binary response becomes a binary item with a storage key and checksum, absent from inline step data, and the `binary` helper reads it back byte-identical.
- [ ] `ssh_tunnel` closes on settle; `file_system` refuses traversal with `fs_path_denied`; `data_table` upserts by key within a run.
- [ ] `deduplicate` keeps first occurrences, stores hashes only, and workflow-scoped repeats are skipped on a second run.
- [ ] `date_time` and `crypto` pass table-driven vectors including a DST-boundary conversion and a known HMAC vector.
- [ ] `stop_and_error` fails the run with its code; `continue_on_fail` yields error items and the run completes; a declared error workflow starts with the failing run's identity.
- [ ] Item counts above the threshold spill to object storage and the history API returns counts and references without loading payloads.

### QA plan

Seed graphs for each family: flow control (if → switch → merge → batches), a bounded loop, code with one
javascript and one python node plus an invalid one, HTTP against the fixture API (json, binary, paginated,
429-then-200) and an error graph using `stop_and_error` with an error workflow. Walkthrough: configure
each node, drag a field from the Data tab into a parameter, run valid and invalid snippets in the code test
surface, preview an HTTP request without sending it, run each graph end to end, open step items to confirm
counts and lineage, trigger the error workflow, and check that a large payload shows as a reference.
Visual check: panels show schema-generated fields, validation messages sit on the offending field, the
sandbox notice is visible, item counts match fixtures, and the error-workflow run links back to the failed run.

### Slices

1. **Item model and flow control** — item shape, spill-to-storage, routing family with caps and lineage. Done: fixture graphs produce the expected per-output counts and the loop guard aborts a runaway body.
2. **Code family and sandbox** — code nodes, static and shape validation, out-of-process runner, test surface. Done: both languages run real items and each limit class reports its own code.
3. **Integration and platform glue** — HTTP with credential auth, retry, pagination and binary spill; tunnel, file system, data table, data store, sub-workflow. Done: fixture cases pass and no secret reaches logs.
4. **Helpers and error handling** — dedup, binary, date/time, crypto, sort/limit/aggregate, no-op, stop-and-error, continue-on-fail, error-workflow routing. Done: vectors pass and both error paths keep lineage intact.

### Risks / notes

- Item arrays are memory-hungry (lesson 17): the reference/spill model must exist before the first integration node ships, with a conservative default threshold and a warning in the data tab.
- The sandbox is the highest-risk surface: out of process, no credentials, no database, allow-listed network, three named limit classes. Static validation is a usability control, never the boundary.
- Loop and pagination caps are contract: a failure must name the cap and the remedy, or operators will disable guards.
- Expression evaluation inside params is REQ-092's runtime; this REQ must not grow a second evaluator, and the preview endpoints exist so nothing evaluates in the browser.
- `sub_workflow` needs a depth cap and a compile-time cycle check, otherwise it hangs instead of erroring.
- Error-workflow payloads stay bounded (identifiers, node key, message, small sample) so a failure cannot copy a large payload into a second run.
