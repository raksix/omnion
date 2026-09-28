-- Omnion · 0050 · the visual workflow builder's graph (REQ-004 slice 1)
--
-- REQ-003 stored one definition as an *ordered list* of steps. That list is what the runner
-- executes and it stays the execution model — this migration does not touch it. What it adds is
-- the *authoring* representation the visual builder (REQ-004) draws: a graph of nodes and
-- edges, with positions kept separately so a layout change can never mean a semantic change.
--
-- Three columns on `workflows`, one on `workflow_executions`, two on `workflow_steps`:
--
--   * `graph` — the definition as nodes and edges, authoritative for the builder. Still
--     `{"nodes":[…],"edges":[…]}`.
--   * `ui_state` — where each node sits, the viewport, collapsed groups. The engine never
--     reads it and no layout write bumps `graph_version` (REQ-004 risks: "positions are not
--     semantics").
--   * `graph_version` — optimistic concurrency for `PUT /graph`. A stale write is a 409 with
--     the current definition, not a silent overwrite of somebody else's edit.
--   * `validated_at` / `validation_error` — the last validation verdict, so the rule list can
--     draw "invalid" without re-validating every row it renders.
--   * `workflow_executions.graph_version` — the graph a run started with, so a trace can
--     always resolve its `node_id`s even after the definition moved on.
--   * `workflow_steps.node_id` / `.branch` — which node and which output port produced the
--     step; the canvas paints run status per node from these.
--
-- The backfill synthesises a graph for every rule that already exists, from its own `steps`
-- array: trigger → one node per step → end. A rule written before the builder existed must
-- open in the builder without manual repair, and a SQL backfill is the only way to guarantee
-- that for every row rather than for the ones a test happened to create.
--
-- The backfill lives in SQL and not in a loop over rows: a workflow a person has not opened
-- should not be re-written by a deploy. Node ids are derived from the step index, so the
-- same definition always produces the same ids and `workflow_steps.node_id` joins back to
-- them.
--
-- Append-only (docs/05-VERSIONING.md): 0049 is the number before this one and is never
-- renumbered. It claims no column REQ-003 owns (`input`, `on_error`, `timeout_ms`,
-- `max_attempts`).

-- ---------------------------------------------------------------------------------------------
-- workflows: the graph, its layout and its version
-- ---------------------------------------------------------------------------------------------

alter table workflows add column if not exists graph jsonb not null default '{"nodes":[],"edges":[]}'::jsonb;
alter table workflows add column if not exists ui_state jsonb not null default '{}'::jsonb;
alter table workflows add column if not exists graph_version integer not null default 1;
alter table workflows add column if not exists validated_at timestamptz;
alter table workflows add column if not exists validation_error text;

-- The builder writes objects, not arrays. A `steps`-shaped value in `graph` is a bug in the
-- writer, and the constraint names it at the boundary instead of three layers deeper.
alter table workflows drop constraint if exists workflows_graph_is_object;
alter table workflows add constraint workflows_graph_is_object check (jsonb_typeof(graph) = 'object');

alter table workflows drop constraint if exists workflows_ui_state_is_object;
alter table workflows add constraint workflows_ui_state_is_object check (jsonb_typeof(ui_state) = 'object');

-- A graph is never "version -1": the version is what a stale write is compared against, and
-- zero would be a state a rule can never be written into.
alter table workflows drop constraint if exists workflows_graph_version_positive;
alter table workflows add constraint workflows_graph_version_positive check (graph_version > 0);

-- The rule list's "invalid" chip reads this instead of re-validating every row it draws.
create index workflows_invalid_idx
    on workflows (organization_id)
    where validation_error is not null;

-- ---------------------------------------------------------------------------------------------
-- Runs and steps: which node a run is on
-- ---------------------------------------------------------------------------------------------

-- The graph a run started with. A trace resolves its node ids against *this* version, not the
-- definition's current one, so a trace of yesterday's run still means something after today's
-- edit. `null` for runs started before this migration (no graph existed to pin).
alter table workflow_executions add column if not exists graph_version integer;

-- Which node of the graph produced this step, and which output port carried into it.
-- `null` on a step of a rule that has no graph — the runner does not care, and the builder
-- only reads these rows for a rule it has a graph for.
alter table workflow_steps add column if not exists node_id text;
alter table workflow_steps add column if not exists branch text;

create index workflow_steps_node_idx on workflow_steps (execution_id, node_id);

-- ---------------------------------------------------------------------------------------------
-- Backfill: every existing rule gets a graph, in SQL
-- ---------------------------------------------------------------------------------------------
--
-- Node ids are `n1`, `n2`, … for the steps and `trigger` / `end` for the two ends, so the
-- ids are stable for a given step list and a trace written before this migration still
-- resolves. Positions come from the step index: a left-to-right strip, which is what the
-- builder's auto-layout produces for a linear definition.
--
-- The step kind drives the node type through the same mapping the builder's registry uses
-- (`omnion_workflows::graph::node_type_for_step`), so a backfilled graph and a graph the
-- builder wrote agree about what a `wait` step is.

update workflows w
set graph = jsonb_build_object(
        'nodes',
        (select jsonb_agg(node order by ord)
         from (
             select 0 as ord, jsonb_build_object(
                        'id', 'trigger',
                        'type', case w.trigger_kind
                                    when 'schedule' then 'trigger.schedule'
                                    else 'trigger.event'
                                end,
                        'label', 'Trigger',
                        'params', jsonb_build_object(
                            'kind', w.trigger_kind,
                            'cron', w.schedule,
                            'event', w.trigger_event
                        ),
                        'position', jsonb_build_object('x', 40, 'y', 40)
                    ) as node
             union all
             select ord,
                    jsonb_build_object(
                        'id', 'n' || (ord - 1),
                        'type', case s.kind
                                    when 'wait' then 'wait'
                                    when 'branch' then 'condition'
                                    when 'stop' then 'end'
                                    when 'approval' then 'approval'
                                    else 'action'
                                end,
                        'label', s.name,
                        'params', s.params,
                        'position', jsonb_build_object('x', 40 + (ord - 1) * 260, 'y', 120)
                    )
             from jsonb_array_elements(w.steps) with ordinality as s(step, ord)
         ) as n),
        'edges',
        (select coalesce(jsonb_agg(e order by ord), '[]'::jsonb)
         from (
             select 1 as ord, jsonb_build_object(
                        'id', 'e0',
                        'source', 'trigger',
                        'source_port', 'out',
                        'target', 'n1'
                    ) as e
             where jsonb_array_length(w.steps) > 0
             union all
             select ord + 1,
                    jsonb_build_object(
                        'id', 'e' || ord,
                        'source', 'n' || ord,
                        'source_port', 'out',
                        'target', 'n' || (ord + 1)
                    )
             from jsonb_array_elements(w.steps) with ordinality as s(step, ord)
             where ord < jsonb_array_length(w.steps)
         ) as x)
    ),
    -- A rule with no steps still needs a valid graph: a trigger and an end, connected.
    -- jsonb_build_object with the two CASE arms keeps this one statement.
    ui_state = jsonb_build_object(
        'viewport', jsonb_build_object('x', 0, 'y', 0, 'zoom', 1),
        'positions', coalesce(
            (select jsonb_object_agg(
                    'n' || (ord - 1), jsonb_build_object('x', 40 + (ord - 1) * 260, 'y', 120))
             from jsonb_array_elements(w.steps) with ordinality as s(step, ord)),
            '{}'::jsonb)
    )
where jsonb_typeof(w.steps) = 'array';

-- A rule with an empty step array gets the minimal valid graph the registry produces for a
-- new definition: a trigger and an end joined by one edge. Written as a second statement
-- because the CASE cannot be expressed inside jsonb_build_object's argument list.
update workflows w
set graph = jsonb_build_object(
        'nodes', jsonb_build_array(
            jsonb_build_object(
                'id', 'trigger',
                'type', case w.trigger_kind
                            when 'schedule' then 'trigger.schedule'
                            else 'trigger.event'
                        end,
                'label', 'Trigger',
                'params', jsonb_build_object('kind', w.trigger_kind, 'cron', w.schedule, 'event', w.trigger_event),
                'position', jsonb_build_object('x', 40, 'y', 40)
            ),
            jsonb_build_object(
                'id', 'end',
                'type', 'end',
                'label', 'End',
                'params', '{}'::jsonb,
                'position', jsonb_build_object('x', 300, 'y', 120)
            )
        ),
        'edges', jsonb_build_array(
            jsonb_build_object('id', 'e0', 'source', 'trigger', 'source_port', 'out', 'target', 'end')
        )
    )
where jsonb_array_length(w.steps) = 0;

-- A backfilled graph is unproven, so it is not stamped as validated: `validated_at` stays
-- null and the first Validate press (or the builder's first save) is what writes the verdict.
-- A rule whose backfill produced a node type the registry does not know is caught there
-- rather than being marked valid by SQL that cannot know the registry.
