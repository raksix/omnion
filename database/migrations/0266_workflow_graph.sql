-- Omnion · 0146 · the visual workflow graph
--
-- The editor (docs/requests/REQ-086, slice 1) authors a graph; the engine runs steps. This
-- migration adds the graph *alongside* `steps`, not instead of it: the save transaction writes
-- both, and the deterministic compiler in `omnion_workflows::graph` is the only thing that
-- produces one from the other, so drift is detectable rather than discovered when a run does
-- the wrong thing.
--
-- Three columns, and only three:
--
--   * `graph` is the document the canvas edits. The default is an empty, well-formed graph so
--     every existing row reads back as a blank canvas rather than a null the editor has to
--     special-case, and so a workflow written before the editor existed keeps running from
--     `steps` untouched.
--   * `graph_revision` is the optimistic-concurrency counter. A save sends the revision it
--     loaded; the update is `where graph_revision = $n`, so a second editor cannot silently
--     overwrite the first one. A per-save counter rather than `updated_at` because two saves in
--     the same millisecond are still two saves, and the loser must be told.
--   * `graph_updated_at` / `graph_updated_by` say who last touched the canvas and when, which is
--     what the lock banner and REQ-095's history both read.
--
-- The GIN index is on `graph -> 'nodes'` with `jsonb_path_ops`, not on the whole document: the
-- only query that reaches into the graph is REQ-087's credential-usage probe, which asks
-- "which nodes name this credential key". Indexing the whole document would index the
-- connections and notes too, which nothing searches, and every save would rewrite more of the
-- index than the workload needs.
--
-- The shape check is the same one the default satisfies, so a corrupt write is refused by the
-- database rather than by a `serde_json` failure three layers up. Released migrations are
-- append-only (docs/05-VERSIONING.md); the wave-3b family reserved 0030–0039, and 0030 is
-- already taken by another wave, so the number comes from the shared high-water mark.

alter table workflows
    add column graph jsonb not null default '{"nodes":[],"connections":[],"notes":[]}'::jsonb,
    add column graph_revision integer not null default 0,
    add column graph_updated_at timestamptz,
    add column graph_updated_by uuid references users (id) on delete set null;

-- The document is three arrays. A node that is not an object, or a connections value that is not
-- an array, is refused here — the editor would otherwise have to defend against a shape its own
-- writer could not produce.
alter table workflows add constraint workflows_graph_shape check (
    jsonb_typeof(graph -> 'nodes') = 'array'
    and jsonb_typeof(graph -> 'connections') = 'array'
    and jsonb_typeof(graph -> 'notes') = 'array'
);

-- Revisions only ever go up, and start at zero: a negative revision would let a client send
-- `If-Match: -1` and match a row that has never been edited.
alter table workflows add constraint workflows_graph_revision_floor check (graph_revision >= 0);

-- REQ-087's usage probe: which nodes name this credential key. Path-ops is right for the
-- containment queries this index is for, and is half the size of the default GIN operator class.
create index workflows_graph_nodes_gin on workflows using gin ((graph -> 'nodes') jsonb_path_ops);
