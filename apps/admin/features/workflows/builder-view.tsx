"use client";

/**
 * `/workflows/[id]/builder` — the visual workflow builder (docs/requests/REQ-004, slice 1).
 *
 * Slice 1 is the *graph core*, and this screen is what makes it usable rather than merely
 * correct: a palette that drops a node, a canvas that pans and zooms, an inspector that
 * writes the node's own parameters, a problems panel that says what is wrong, and a
 * toolbar that tells the truth about saving.
 *
 * Four decisions shape the whole file, and each of them is a way a screen can be quietly
 * lying:
 *
 * * **The graph is what is edited; the step list is generated.** The builder never writes a
 *   step, and the server re-projects on every save. A canvas that could drift from what runs
 *   would be worse than no canvas.
 * * **Unsaved means unsaved.** The toolbar says "Unsaved changes" the moment a change lands
 *   and "Saved Ns ago" only when the server has answered. A timer that claims success before
 *   the request is the reason autosave is distrusted.
 * * **A conflict keeps your copy.** When another editor has saved, the panel says so and
 *   offers Reload — it does not silently overwrite, and it does not throw away what you were
 *   holding either.
 * * **Layout is not semantics.** Pan and zoom go to `ui-state`, which bumps no version, so
 *   moving nodes around all afternoon never invalidates anybody's edit.
 *
 * The canvas is in-house DOM + SVG rather than a library (REQ-004 risks): keyboard and
 * screen-reader behaviour stay under our control, and no new build-time dependency enters
 * the core.
 */
import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type PointerEvent as ReactPointerEvent,
  type KeyboardEvent as ReactKeyboardEvent,
  type DragEvent as ReactDragEvent,
} from "react";

import {
  AlertTriangle,
  ArrowLeft,
  CheckCircle2,
  ClipboardCopy,
  ClipboardPaste,
  Copy,
  GitBranch,
  LayoutGrid,
  Loader2,
  Maximize,
  Minus,
  Play,
  Plus,
  RefreshCw,
  Redo2,
  Table2,
  Undo2,
  Wand2,
  Zap,
  ZoomIn,
  ZoomOut,
} from "lucide-react";
import Link from "next/link";
import { useRouter } from "next/navigation";

import { ApiError, fetchGraphNodeTypes, fetchWorkflowGraph, saveWorkflowGraph, saveWorkflowUiState, validateWorkflowGraph, type GraphEdge, type GraphFinding, type GraphNode, type GraphNodeType, type GraphNodeTypes, type WorkflowGraph } from "@/lib/api";
import {
  capabilities,
  emptyHistory,
  record,
  redo,
  redoTarget,
  snapshotOf,
  undo,
  undoTarget,
  type History,
  type HistorySnapshot,
} from "./builder-history";
import { decideConnection } from "./connect-edge";
import { readVersionFrom, resolveConflict } from "./conflict";
import {
  clearSelection,
  deleteTarget,
  EMPTY_SELECTION,
  extendGroup,
  isNodeSelected,
  membersOf,
  pruneSelection,
  selectAll,
  selectEdge,
  selectGroup,
  selectNode,
  selectionSize,
  toggleNode,
  whatEscapeClears,
  type CanvasSelection,
} from "./selection";

/** The snap grid the canvas draws and drops onto. */
const GRID = 8;

/** The canvas bounds, mirroring the server's own: a node dragged a million pixels out is a
 *  coordinate, not a layout, and it makes the minimap useless. */
const COORD_LIMIT = 20_000;

/** How far the viewport may zoom, mirroring the server's clamp. */
const MIN_ZOOM = 0.25;
const MAX_ZOOM = 2;
const ZOOM_STEP = 0.1;

/** How long to wait after the last change before autosaving, in milliseconds. */
const AUTOSAVE_MS = 1_200;

/** A node card's width — the value the edge maths has to agree with. */
const CARD_W = 220;
const CARD_H = 78;

/** Where the ports sit on a card, relative to it. */
const PORT_Y = CARD_H - 18;

/** One line of the status bar / problems panel, shared by both. */
type SaveState =
  | { kind: "clean" }
  | { kind: "dirty"; since: number }
  | { kind: "saving" }
  | { kind: "saved"; at: number }
  | { kind: "conflict"; message: string; version: number | null }
  | { kind: "error"; message: string };

/**
 * The builder workspace.
 *
 * Everything is derived from two pieces of state — the graph and the layout — plus the
 * registry, so there is exactly one place a change is made and one place it is written.
 */
export function WorkflowBuilder({ workflowId }: { workflowId: string }) {
  const router = useRouter();

  const [registry, setRegistry] = useState<GraphNodeTypes | null>(null);
  const [definition, setDefinition] = useState<WorkflowGraph | null>(null);
  const [nodes, setNodes] = useState<GraphNode[]>([]);
  const [edges, setEdges] = useState<GraphEdge[]>([]);
  const [viewport, setViewport] = useState({ x: 0, y: 0, zoom: 1 });

  // ---- selection ---------------------------------------------------------------------------
  // ONE owner (see `selection.ts`): a click, a Shift+click, a marquee, `⌘A` and an edge click
  // all return a new `CanvasSelection` through a named transition, and the outline, the
  // minimap, the status bar, `Del` and Escape all read the same object. Before this, the
  // answer lived in three pieces of state and each writer assembled its own — which is how
  // Shift+click could deselect a node and leave it drawn as selected, and how `Del` on an
  // edge was a branch in a key handler that nothing could assert.
  const [selection, setSelection] = useState<CanvasSelection>(EMPTY_SELECTION);
  const selected = selection.focus;
  const selectedEdge = selection.edge;
  const selectionCount = selectionSize(selection);
  const [findings, setFindings] = useState<GraphFinding[]>([]);
  const [save, setSave] = useState<SaveState>({ kind: "clean" });
  // A mirror of `save` for the handlers that run outside a render. `keepMine` has to read
  // the conflict the *server* just reported, and a click handler closing over the state it
  // was rendered with would be reading the state from the render before the conflict
  // arrived — so it would take the reload exit while the banner showed an overwrite.
  const saveRef = useRef<SaveState>(save);
  saveRef.current = save;
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [paletteQuery, setPaletteQuery] = useState("");
  const [problemsOpen, setProblemsOpen] = useState(true);
  const [running, setRunning] = useState(false);
  const [runMessage, setRunMessage] = useState<string | null>(null);

  const [dragging, setDragging] = useState<{
    id: string;
    offsetX: number;
    offsetY: number;
  } | null>(null);
  const [panning, setPanning] = useState<{ x: number; y: number; vx: number; vy: number } | null>(null);

  const canvasRef = useRef<HTMLDivElement>(null);
  const saveTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  // The version the editor loaded, and therefore the one a save must quote. Kept in a ref
  // because the save closure must read the value at press time, not at render time.
  const versionRef = useRef(0);
  // The graph as it is *right now*, for the same reason: a debounced save fires 1.2s after
  // the change that armed it, and by then the state it would have closed over is stale.
  const graphRef = useRef<{ nodes: GraphNode[]; edges: GraphEdge[] }>({ nodes: [], edges: [] });
  graphRef.current = { nodes, edges };
  // The in-tab clipboard. Deliberately not the system clipboard — see `copySelection`.
  const clipboardRef = useRef<{ nodes: GraphNode[]; edges: GraphEdge[] }>({ nodes: [], edges: [] });
  const [clipboardCount, setClipboardCount] = useState(0);
  // The graph as it was when the current drag began, so one press of undo removes the whole
  // gesture instead of one pointer sample of it.
  const dragOriginRef = useRef<HistorySnapshot | null>(null);
  // The rubber band, in screen pixels. `startX/startY` is kept so a drag that runs up and
  // left still measures from where the pointer went down rather than from the origin.
  const [marquee, setMarquee] = useState<{
    x: number;
    y: number;
    w: number;
    h: number;
    startX: number;
    startY: number;
    additive: boolean;
  } | null>(null);
  // The minimap's own rectangle, and whether the author wants to see it.
  const [minimapOpen, setMinimapOpen] = useState(true);
  // The selected *edge* and the node selection now live in one object (`selection.ts`), so an
  // edge chosen for deletion cannot be half-remembered by a handler that missed the state.
  // A connection being drawn: the source node and the port the user picked. Kept as state
  // rather than a ref because the canvas has to paint the in-flight line, and because the
  // reason a drop was refused has to survive the click that produced it long enough to be
  // read.
  const [linkDraft, setLinkDraft] = useState<{ nodeId: string; port: string } | null>(null);
  const [linkNotice, setLinkNotice] = useState<{ tone: "ok" | "error"; text: string } | null>(
    null,
  );

  const nodeTypes = useMemo(() => {
    const map = new Map<string, GraphNodeType>();
    for (const nodeType of registry?.node_types ?? []) {
      map.set(nodeType.key, nodeType);
    }
    return map;
  }, [registry]);

  // ---- load -------------------------------------------------------------------------------

  const load = useCallback(async () => {
    setLoading(true);
    setLoadError(null);
    try {
      const [types, graph] = await Promise.all([
        fetchGraphNodeTypes(),
        fetchWorkflowGraph(workflowId),
      ]);
      setRegistry(types);
      setDefinition(graph);
      setNodes(graph.graph.nodes);
      setEdges(graph.graph.edges);
      versionRef.current = graph.graph_version;
      setSave({ kind: "clean" });
      // The stored layout's viewport, when it has one: a rule that was arranged on a big
      // screen should not open zoomed into the top-left corner.
      const stored = (graph.ui_state?.viewport ?? {}) as { x?: number; y?: number; zoom?: number };
      setViewport({
        x: typeof stored.x === "number" ? stored.x : 0,
        y: typeof stored.y === "number" ? stored.y : 0,
        zoom: typeof stored.zoom === "number" ? clampZoom(stored.zoom) : 1,
      });
    } catch (error) {
      setLoadError(
        error instanceof ApiError ? error.message : "The builder could not be loaded.",
      );
    } finally {
      setLoading(false);
    }
  }, [workflowId]);

  useEffect(() => {
    void load();
  }, [load]);

  // ---- saving -----------------------------------------------------------------------------

  /**
   * Write the graph, and report what actually happened.
   *
   * The state reaches `saved` only once the server has answered — a builder that says
   * "Saved" while the request is in flight has lied once already, and the second thing an
   * author does after a lie is stop believing the indicator.
   *
   * Declared before `queueSave` (which calls it through a ref) because the reverse order is
   * a cycle: `queueSave` needs `persist`, and `persist` is the thing that clears the timer
   * the queue set.
   */
  const persist = useCallback(async () => {
    setSave({ kind: "saving" });
    try {
      const current = graphRef.current;
      const saved = await saveWorkflowGraph(workflowId, {
        graph: { nodes: current.nodes, edges: current.edges },
        graph_version: versionRef.current,
      });
      setDefinition(saved);
      versionRef.current = saved.graph_version;
      setSave({ kind: "saved", at: Date.now() });
      // The server's own verdict replaces the local one: it validated the graph it stored,
      // which is not always the graph the editor believes it stored.
      setFindings([]);
    } catch (error) {
      if (error instanceof ApiError && error.code === "graph_version_conflict") {
        // The server names the version it is holding, and it is read out of the message
        // rather than kept in a second field: `graph_store::replace_graph` is the only
        // producer of this error and it always says "it is now at version N". A client that
        // stored the version separately would have two answers to "what is the server on"
        // and they would drift the first time one of them changed.
        setSave({
          kind: "conflict",
          message: error.message,
          version: readVersionFrom(error.message),
        });
        return;
      }
      if (error instanceof ApiError && error.details) {
        const details = error.details as { findings?: GraphFinding[] };
        if (Array.isArray(details.findings) && details.findings.length > 0) {
          setFindings(details.findings);
          setProblemsOpen(true);
        }
      }
      setSave({
        kind: "error",
        message: error instanceof ApiError ? error.message : "The graph could not be saved.",
      });
    }
  }, [workflowId]);

  /**
   * Queue a save after the last change.
   *
   * The graph is read from `graphRef` rather than from the closure, because a save fired
   * 1.2s after a change must write the graph as it is *now* — three more changes may have
   * landed since the callback that armed the timer was created.
   */
  const queueSave = useCallback(() => {
    setSave((current) =>
      current.kind === "conflict" ? current : { kind: "dirty", since: Date.now() },
    );
    if (saveTimer.current) {
      clearTimeout(saveTimer.current);
    }
    saveTimer.current = setTimeout(() => {
      void persist();
    }, AUTOSAVE_MS);
  }, [persist]);

  /**
   * Take the conflict's second exit: write this tab's graph on top of the other editor's.
   *
   * The version quoted is the one the *server* named, taken from `conflict.ts`, and never a
   * locally derived `versionRef + 1`. That distinction is the whole safety argument: the
   * guard exists so two editors cannot both believe they won, and re-deriving the base turns
   * it into "last write wins" wearing the costume of a guard. The author has just been told,
   * in the button's own label, what this click destroys.
   *
   * The save goes out immediately rather than through the debounce — the author has already
   * waited for the conflict to be noticed, and one more 1.2s would read as a second refusal.
   */
  const keepMine = useCallback(() => {
    const current = saveRef.current;
    if (current.kind !== "conflict") return;
    const resolution = resolveConflict({ message: current.message, version: current.version });
    if (resolution.nextVersion === null) return;
    versionRef.current = resolution.nextVersion;
    setSave({ kind: "dirty", since: Date.now() });
    if (saveTimer.current) {
      clearTimeout(saveTimer.current);
      saveTimer.current = null;
    }
    void persist();
  }, [persist]);

  /** Save the layout, which bumps neither the version nor the step list. */
  const persistLayout = useCallback(
    (next: { x: number; y: number; zoom: number }) => {
      // Fire-and-forget: a layout write that fails must not interrupt arranging nodes, and
      // the next pan overwrites it anyway. The graph version is untouched either way.
      void saveWorkflowUiState(workflowId, { viewport: next }).catch(() => undefined);
    },
    [workflowId],
  );

  // ---- graph editing ----------------------------------------------------------------------

  /**
   * The undo history, in a ref because it is not render state.
   *
   * A `useState` history would re-render the canvas on every pointer move just to hold a
   * cursor, and the *reason* the history exists is to make those moves cheap.
   */
  const historyRef = useRef<History>(emptyHistory());
  // Re-render on history change alone, so the toolbar's undo/redo buttons can be enabled from
  // what is actually undoable rather than from "an entry has ever existed".
  const [historyTick, setHistoryTick] = useState(0);

  const pushHistory = useCallback((key: string, before: HistorySnapshot) => {
    const current = graphRef.current;
    historyRef.current = record(historyRef.current, {
      key,
      before,
      after: snapshotOf(current.nodes, current.edges),
    });
    setHistoryTick((n) => n + 1);
  }, []);

  /**
   * Apply a change, recording it as one undoable step.
   *
   * `before` is captured by the caller *before* it touches the graph — the state object at
   * that instant — because after `setNodes` the previous array is already gone, and a history
   * that records the new graph as "before" is an undo that does nothing.
   */
  const commit = useCallback(
    (key: string, before: HistorySnapshot, nextNodes: GraphNode[], nextEdges: GraphEdge[]) => {
      graphRef.current = { nodes: nextNodes, edges: nextEdges };
      setNodes(nextNodes);
      setEdges(nextEdges);
      pushHistory(key, before);
      queueSave();
    },
    [pushHistory, queueSave],
  );

  const currentSnapshot = useCallback(
    (): HistorySnapshot => snapshotOf(graphRef.current.nodes, graphRef.current.edges),
    [],
  );

  const doUndo = useCallback(() => {
    const target = undoTarget(historyRef.current);
    if (!target) {
      return;
    }
    historyRef.current = undo(historyRef.current);
    const restored = target as { nodes: GraphNode[]; edges: GraphEdge[] };
    setNodes(restored.nodes);
    setEdges(restored.edges);
    setHistoryTick((n) => n + 1);
    queueSave();
  }, [queueSave]);

  const doRedo = useCallback(() => {
    const target = redoTarget(historyRef.current);
    if (!target) {
      return;
    }
    historyRef.current = redo(historyRef.current);
    const restored = target as { nodes: GraphNode[]; edges: GraphEdge[] };
    setNodes(restored.nodes);
    setEdges(restored.edges);
    setHistoryTick((n) => n + 1);
    queueSave();
  }, [queueSave]);

  const { canUndo, canRedo } = capabilities(historyRef.current);

  const addNode = useCallback(
    (nodeType: GraphNodeType, at?: { x: number; y: number }) => {
      const before = currentSnapshot();
      // A drop carries the exact spot the card was released, so the node lands under the
      // pointer. Without one — a click, or a keyboard add — it lands at the viewport centre,
      // which is the only position a pointer-less gesture can honestly claim.
      const position = at ?? viewportCentre(canvasRef.current, viewport);
      const node: GraphNode = {
        id: uniqueId(nodeType.key, nodes),
        type: nodeType.key,
        label: nodeType.label,
        params: { ...nodeType.defaults },
        position: { x: snap(position.x), y: snap(position.y) },
      };
      setNodes((current) => [...current, node]);
      setSelection(selectNode(node.id));
      // The graph ref is written directly as well as through state: `pushHistory` reads the
      // ref, and a ref updated in a render body would be one render behind the change.
      graphRef.current = { nodes: [...graphRef.current.nodes, node], edges: graphRef.current.edges };
      pushHistory(`add:${node.id}`, before);
      queueSave();
    },
    [currentSnapshot, nodes, pushHistory, queueSave, viewport],
  );

  /**
   * Duplicate the selected node, offset so the copy is visibly a copy.
   *
   * The id has to be fresh: reusing the source's id would produce two nodes with one identity,
   * and the edge list cannot then say which of them an edge means.
   */
  const duplicateSelected = useCallback(() => {
    if (!selected) {
      return;
    }
    const source = nodes.find((node) => node.id === selected);
    if (!source) {
      return;
    }
    const before = currentSnapshot();
    const copy: GraphNode = {
      ...source,
      id: uniqueId(source.type, nodes),
      label: `${source.label} copy`,
      position: {
        x: clampCoord(snap(source.position.x + CARD_W + GRID * 2)),
        y: clampCoord(snap(source.position.y + GRID * 2)),
      },
      params: { ...source.params },
    };
    const nextNodes = [...nodes, copy];
    commit(`duplicate:${copy.id}`, before, nextNodes, edges);
    setSelection(selectNode(copy.id));
  }, [commit, currentSnapshot, edges, nodes, selected]);

  /**
   * Copy the selection to the clipboard slot.
   *
   * The system clipboard is not used on purpose: it would need a permission the browser
   * withholds until a real user gesture, and the paste then fails silently. An in-tab slot
   * cannot be read by the page the user is about to visit, which is the trade this screen
   * wants — a workflow graph is not something you paste into a text field.
   */
  const copySelection = useCallback(() => {
    const chosen = nodes.filter((node) => membersOf(selection).includes(node.id));
    if (chosen.length === 0) {
      return;
    }
    const ids = new Set(chosen.map((node) => node.id));
    clipboardRef.current = {
      nodes: chosen.map((node) => ({ ...node, position: { ...node.position }, params: { ...node.params } })),
      // Only the edges *within* the selection survive: an edge to a node that was not copied
      // would dangle, and validation would then refuse the whole pasted graph.
      edges: edges.filter((edge) => ids.has(edge.source) && ids.has(edge.target)),
    };
    setClipboardCount(clipboardRef.current.nodes.length);
  }, [edges, nodes, selection]);

  const pasteClipboard = useCallback(() => {
    const source = clipboardRef.current;
    if (source.nodes.length === 0) {
      return;
    }
    const before = currentSnapshot();
    // A fresh id per pasted node, mapped from the old one so the internal edges still connect.
    const remap = new Map<string, string>();
    const nextNodes = [...nodes];
    for (const node of source.nodes) {
      const fresh = uniqueId(node.type, nextNodes);
      remap.set(node.id, fresh);
      nextNodes.push({
        ...node,
        id: fresh,
        position: {
          x: clampCoord(snap(node.position.x + GRID * 4)),
          y: clampCoord(snap(node.position.y + GRID * 4)),
        },
        params: { ...node.params },
      });
    }
    const nextEdges = [...edges];
    for (const edge of source.edges) {
      const source_id = remap.get(edge.source);
      const target_id = remap.get(edge.target);
      if (!source_id || !target_id) {
        continue;
      }
      nextEdges.push({
        id: uniqueEdgeId(nextEdges),
        source: source_id,
        source_port: edge.source_port,
        target: target_id,
      });
    }
    commit("paste", before, nextNodes, nextEdges);
    const first = [...remap.values()][0];
    // The pasted group is selected as a group, not collapsed onto its first member: a paste
    // that highlights one card leaves the rest of the copy looking like it did not land.
    setSelection(selectGroup([...remap.values()]));
  }, [commit, currentSnapshot, edges, nodes]);

  const moveNode = useCallback(
    (id: string, x: number, y: number) => {
      setNodes((current) =>
        current.map((node) =>
          node.id === id
            ? { ...node, position: { x: clampCoord(snap(x)), y: clampCoord(snap(y)) } }
            : node,
        ),
      );
    },
    [],
  );

  const commitMove = useCallback(() => {
    queueSave();
  }, [queueSave]);

  const updateNode = useCallback(
    (id: string, patch: Partial<GraphNode>) => {
      const before = currentSnapshot();
      const nextNodes = nodes.map((node) => (node.id === id ? { ...node, ...patch } : node));
      // A re-type is one gesture: the same node and the same field, so it coalesces with the
      // keystrokes before it instead of costing a press of undo per character.
      const fields = Object.keys(patch).sort().join(",");
      commit(`edit:${id}:${fields}`, before, nextNodes, edges);
    },
    [commit, currentSnapshot, edges, nodes],
  );

  /**
   * Lay the graph out left to right by its edges.
   *
   * Longest-path layering rather than insertion order: a hand-built graph's node array says
   * nothing about what runs first, and laying out by array order puts a branch's two arms on
   * top of each other — a layout that *looks* broken and explains nothing on screen.
   *
   * The result is one undoable entry, so an automatic layout is as reversible as a manual one.
   */
  const autoLayout = useCallback(() => {
    if (nodes.length === 0) {
      return;
    }
    const before = currentSnapshot();
    const depth = new Map<string, number>();
    for (const node of nodes) {
      depth.set(node.id, 0);
    }
    // Relax the edges until nothing grows: a small DAG settles in a handful of passes, and
    // the visit cap keeps a cycle (which validation will report separately) from hanging here.
    for (let pass = 0; pass < nodes.length; pass += 1) {
      let moved = false;
      for (const edge of edges) {
        const from = depth.get(edge.source) ?? 0;
        const to = depth.get(edge.target) ?? 0;
        if (to < from + 1) {
          depth.set(edge.target, from + 1);
          moved = true;
        }
      }
      if (!moved) {
        break;
      }
    }
    const columns = new Map<number, number>();
    const ROW = CARD_H + GRID * 4;
    const COL = CARD_W + GRID * 6;
    const nextNodes = nodes.map((node) => {
      const column = depth.get(node.id) ?? 0;
      const row = columns.get(column) ?? 0;
      columns.set(column, row + 1);
      return {
        ...node,
        position: {
          x: clampCoord(40 + column * COL),
          y: clampCoord(40 + row * ROW),
        },
      };
    });
    commit("auto-layout", before, nextNodes, edges);
  }, [commit, currentSnapshot, edges, nodes]);

  /**
   * Remove one node or a whole selection, as a single undoable step.
   *
   * The edges go with them: a connection whose endpoint is gone is exactly the dangling edge
   * validation would then refuse the whole graph for. Removing three nodes at once is one
   * entry rather than three, because three presses of undo to undo one keypress is how a user
   * learns to distrust the button.
   */
  const removeNodes = useCallback(
    (ids: string[]) => {
      if (ids.length === 0) {
        return;
      }
      const doomed = new Set(ids);
      const before = currentSnapshot();
      const nextNodes = nodes.filter((node) => !doomed.has(node.id));
      if (nextNodes.length === nodes.length) {
        return;
      }
      const nextEdges = edges.filter(
        (edge) => !doomed.has(edge.source) && !doomed.has(edge.target),
      );
      commit(
        ids.length === 1 ? `remove:${ids[0]}` : `remove:${[...doomed].sort().join(",")}`,
        before,
        nextNodes,
        nextEdges,
      );
      // Prune rather than clear: a group delete should leave the nodes that survived
      // *selected* only if they were selected before, and the focus must land on a node
      // that still exists. The old code cleared the group and guarded the focus
      // independently, so deleting a focused node left the inspector holding a dead id and
      // the toolbar's Duplicate button still enabled.
      setSelection((current) => pruneSelection(current, nextNodes.map((node) => node.id)));
    },
    [commit, currentSnapshot, edges, nodes],
  );

  const removeNode = useCallback((id: string) => removeNodes([id]), [removeNodes]);

  // The rules live in `connect-edge.ts` so they can be tested without a canvas; this is the
  // part only the component can do — turn a decision into one undoable step and a notice.
  //
  // The decision reads the *live* graph off the ref rather than the render's `nodes`/`edges`
  // closure, because a gesture drawn during a drag must not be judged against a stale graph.
  const connect = useCallback(
    (source: string, sourcePort: string, target: string) => {
      const decision = decideConnection(
        source,
        sourcePort,
        target,
        graphRef.current.nodes,
        nodeTypes,
        graphRef.current.edges,
        uniqueEdgeId(graphRef.current.edges),
      );
      setLinkNotice({ tone: decision.ok ? "ok" : "error", text: decision.text });
      if (!decision.ok) {
        setLinkDraft(null);
        return false;
      }
      // Routed through `commit` like every other change, so a connection is one undoable
      // step. The criteria ask undo to "restore add, move, connect, delete …"; an edge added
      // behind the history's back is the one case that could not be undone.
      commit(
        "edge-add",
        currentSnapshot(),
        graphRef.current.nodes,
        [...graphRef.current.edges, decision.edge],
      );
      setLinkDraft(null);
      return true;
    },
    [commit, currentSnapshot, nodeTypes],
  );

  const removeEdge = useCallback(
    (id: string) => {
      // Routed through `commit` rather than `setEdges` + `queueSave` so an edge delete is one
      // undoable step like every other change. A delete the history does not know about is a
      // delete the user has to rebuild by hand, and the acceptance criteria ask for exactly
      // that undo ("restores add, move, connect, delete … at least 50 steps deep").
      const before = snapshotOf(graphRef.current.nodes, graphRef.current.edges);
      const nextEdges = graphRef.current.edges.filter((edge) => edge.id !== id);
      if (nextEdges.length === graphRef.current.edges.length) {
        return;
      }
      setSelection((current) => (current.edge === id ? clearSelection() : current));
      commit("edge-remove", before, graphRef.current.nodes, nextEdges);
    },
    [commit],
  );

  /**
   * Drag a node from the palette onto the canvas.
   *
   * HTML5 drag rather than pointer events: a palette item is a button, and a button that
   * answers a pointer-drag has to suppress the click that a plain click fires, which is how
   * "drag" ends up breaking "click". The drag carries only the node type key, and the drop
   * position is converted from screen to graph coordinates here — the drop handler receives
   * client coordinates, and a node placed at `clientX` would land wherever the viewport
   * happened to be scrolled.
   */
  const onPaletteDragStart = useCallback((event: ReactDragEvent<HTMLButtonElement>, key: string) => {
    event.dataTransfer.setData("application/x-omnion-node-type", key);
    event.dataTransfer.effectAllowed = "copy";
  }, []);

  const onCanvasDrop = useCallback(
    (event: ReactDragEvent<HTMLDivElement>) => {
      const key = event.dataTransfer.getData("application/x-omnion-node-type");
      const nodeType = key ? nodeTypes.get(key) : undefined;
      if (!nodeType) {
        return;
      }
      event.preventDefault();
      const rect = canvasRef.current?.getBoundingClientRect();
      if (!rect) {
        return;
      }
      addNode(nodeType, {
        // Centred on the pointer, the same way `viewportCentre` centres a click-add: the
        // card's *top-left* is not what the eye places, and a node that appears down and to
        // the right of where it was dropped reads as a drop that did not land where it aimed.
        x: (event.clientX - rect.left - viewport.x) / viewport.zoom - CARD_W / 2,
        y: (event.clientY - rect.top - viewport.y) / viewport.zoom - CARD_H / 2,
      });
    },
    [addNode, nodeTypes, viewport],
  );

  // ---- canvas interaction -----------------------------------------------------------------

  const zoomBy = useCallback(
    (delta: number) => {
      setViewport((current) => {
        const next = { ...current, zoom: clampZoom(current.zoom + delta * ZOOM_STEP) };
        persistLayout(next);
        return next;
      });
    },
    [persistLayout],
  );

  const fit = useCallback(() => {
    const element = canvasRef.current;
    if (!element || nodes.length === 0) {
      return;
    }
    const bounds = nodes.reduce(
      (box, node) => ({
        minX: Math.min(box.minX, node.position.x),
        minY: Math.min(box.minY, node.position.y),
        maxX: Math.max(box.maxX, node.position.x + CARD_W),
        maxY: Math.max(box.maxY, node.position.y + CARD_H),
      }),
      { minX: Infinity, minY: Infinity, maxX: -Infinity, maxY: -Infinity },
    );
    const padding = 48;
    const zoom = clampZoom(
      Math.min(
        (element.clientWidth - padding * 2) / Math.max(1, bounds.maxX - bounds.minX),
        (element.clientHeight - padding * 2) / Math.max(1, bounds.maxY - bounds.minY),
        1,
      ),
    );
    const next = {
      zoom,
      x: -bounds.minX + (element.clientWidth / zoom - (bounds.maxX - bounds.minX)) / 2,
      y: -bounds.minY + (element.clientHeight / zoom - (bounds.maxY - bounds.minY)) / 2,
    };
    setViewport(next);
    persistLayout(next);
  }, [nodes, persistLayout]);

  const onNodePointerDown = useCallback(
    (event: ReactPointerEvent<HTMLDivElement>, node: GraphNode) => {
      if (event.button !== 0) {
        return;
      }
      const element = canvasRef.current;
      if (!element) {
        return;
      }
      const rect = element.getBoundingClientRect();
      // Selection is decided HERE, on pointer-down, and nowhere else. It used to be decided
      // twice — once here (`setSelected(node.id)`, unconditionally) and again in `onClick`
      // (the Shift+click toggle) — so a Shift+click first erased the group and then toggled
      // against an empty one, and a de-selected node kept the focus the plain pointer-down
      // had just given it. Two writers to one piece of state is how a node could be
      // "deselected" and still drawn as selected.
      setSelection((current) => (event.shiftKey ? toggleNode(current, node.id) : selectNode(node.id)));
      setDragging({
        id: node.id,
        offsetX: (event.clientX - rect.left - viewport.x) / viewport.zoom - node.position.x,
        offsetY: (event.clientY - rect.top - viewport.y) / viewport.zoom - node.position.y,
      });
    },
    [viewport],
  );

  const onCanvasPointerDown = useCallback(
    (event: ReactPointerEvent<HTMLDivElement>) => {
      // Space-drag or the middle button pans; a plain drag on empty canvas draws a marquee,
      // and a click without movement clears the selection — what a person expects from
      // clicking the desk.
      if (event.button === 1 || spaceHeld.current) {
        setPanning({ x: event.clientX, y: event.clientY, vx: viewport.x, vy: viewport.y });
        return;
      }
      if (event.button !== 0) {
        return;
      }
      const rect = canvasRef.current?.getBoundingClientRect();
      if (!rect) {
        return;
      }
      // Shift keeps the existing selection, so a second marquee adds to the first instead of
      // replacing it — the one modifier every drawing tool agrees on. The marquee that
      // follows decides the group; this only decides whether it *extends*.
      if (!event.shiftKey) {
        setSelection(clearSelection());
      }
      setMarquee({
        x: event.clientX - rect.left,
        y: event.clientY - rect.top,
        w: 0,
        h: 0,
        startX: event.clientX - rect.left,
        startY: event.clientY - rect.top,
        additive: event.shiftKey,
      });
    },
    [viewport],
  );

  /** Centre the viewport on a graph point — the minimap's whole job. */
  const jumpTo = useCallback(
    (x: number, y: number) => {
      const element = canvasRef.current;
      if (!element) {
        return;
      }
      const rect = element.getBoundingClientRect();
      const next = {
        zoom: viewport.zoom,
        x: rect.width / 2 - x * viewport.zoom,
        y: rect.height / 2 - y * viewport.zoom,
      };
      setViewport(next);
      persistLayout(next);
    },
    [persistLayout, viewport.zoom],
  );

  const onPointerMove = useCallback(
    (event: ReactPointerEvent<HTMLDivElement>) => {
      const element = canvasRef.current;
      if (!element) {
        return;
      }
      const rect = element.getBoundingClientRect();
      if (panning) {
        const next = {
          ...viewport,
          x: panning.vx + (event.clientX - panning.x),
          y: panning.vy + (event.clientY - panning.y),
        };
        setViewport(next);
        persistLayout(next);
        return;
      }
      if (marquee) {
        setMarquee({
          ...marquee,
          w: event.clientX - rect.left - marquee.startX,
          h: event.clientY - rect.top - marquee.startY,
        });
        return;
      }
      if (dragging) {
        moveNode(
          dragging.id,
          (event.clientX - rect.left - viewport.x) / viewport.zoom - dragging.offsetX,
          (event.clientY - rect.top - viewport.y) / viewport.zoom - dragging.offsetY,
        );
      }
    },
    [dragging, marquee, moveNode, panning, persistLayout, viewport],
  );

  /**
   * What a marquee caught: every node whose card *overlaps* the band, in graph coordinates.
   *
   * Overlap rather than containment because a rubber band that only catches fully-enclosed
   * cards silently ignores the node you were obviously pointing at — and the band is drawn
   * from a single pointer position, so "obviously" is the common case.
   */
  const marqueeSelection = useCallback(
    (band: NonNullable<typeof marquee>): string[] => {
      const left = Math.min(band.startX, band.startX + band.w);
      const top = Math.min(band.startY, band.startY + band.h);
      const right = Math.max(band.startX, band.startX + band.w);
      const bottom = Math.max(band.startY, band.startY + band.h);
      return nodes
        .filter((node) => {
          const x0 = node.position.x * viewport.zoom + viewport.x;
          const y0 = node.position.y * viewport.zoom + viewport.y;
          return x0 < right && x0 + CARD_W * viewport.zoom > left && y0 < bottom && y0 + CARD_H * viewport.zoom > top;
        })
        .map((node) => node.id);
    },
    [nodes, viewport],
  );

  const onPointerUp = useCallback(() => {
    if (panning) {
      setPanning(null);
      return;
    }
    if (marquee) {
      const caught = marqueeSelection(marquee);
      // A click with no drag is a click on the desk, not a zero-area band that catches
      // whatever happens to overlap the pixel it landed on.
      const travelled = Math.abs(marquee.w) > 4 || Math.abs(marquee.h) > 4;
      if (travelled && caught.length > 0) {
        setSelection((current) =>
          marquee.additive ? extendGroup(current, caught) : selectGroup(caught),
        );
      }
      setMarquee(null);
      return;
    }
    if (dragging) {
      setDragging(null);
      commitMove();
    }
  }, [commitMove, dragging, marquee, marqueeSelection, panning]);

  const filteredTypes = filterTypes(registry, paletteQuery);

  /**
   * Palette keyboard support: arrows move a roving focus, Enter adds.
   *
   * The buttons are natively focusable, so Tab already reaches them and Enter already fires
   * onClick — what was missing is a way to *move* through the list without hunting for the
   * next item with Tab, and a way to get into the palette from the canvas. `⌘P` focuses the
   * first item, arrows walk it, Enter adds at the viewport centre. The focus is applied with
   * `data-palette-node` rather than a React ref array so one query finds it, which also means
   * a re-render after the add cannot leave focus pointing at a detached node.
   */
  const focusPaletteItem = useCallback((key: string) => {
    const next = document.querySelector<HTMLButtonElement>(`[data-palette-node="${key}"]`);
    next?.focus();
  }, []);

  const onPaletteKeyDown = useCallback(
    (event: ReactKeyboardEvent<HTMLButtonElement>, key: string) => {
      const order = filteredTypes.map((nodeType) => nodeType.key);
      const at = order.indexOf(key);
      if (at < 0) {
        return;
      }
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        // Stop at the ends rather than wrapping: wrapping past the last node back to the first
        // is a jump the user did not ask for, and on a short list it is a bigger jump than the
        // arrow they pressed.
        event.preventDefault();
        const target = event.key === "ArrowDown" ? order[at + 1] : order[at - 1];
        if (target) {
          focusPaletteItem(target);
        }
        return;
      }
      if (event.key === "Home" || event.key === "End") {
        event.preventDefault();
        const target = event.key === "Home" ? order[0] : order[order.length - 1];
        if (target) {
          focusPaletteItem(target);
        }
        return;
      }
    },
    [filteredTypes, focusPaletteItem],
  );

  // Space is the pan modifier; tracked in a ref because a keydown does not re-render.
  const spaceHeld = useRef(false);
  useEffect(() => {
    const down = (event: KeyboardEvent) => {
      if (event.code === "Space" && !isTypingTarget(event.target)) {
        spaceHeld.current = true;
      }
    };
    const up = (event: KeyboardEvent) => {
      if (event.code === "Space") {
        spaceHeld.current = false;
      }
    };
    window.addEventListener("keydown", down);
    window.addEventListener("keyup", up);
    return () => {
      window.removeEventListener("keydown", down);
      window.removeEventListener("keyup", up);
    };
  }, []);

  // ---- keyboard ---------------------------------------------------------------------------

  const onCanvasKeyDown = useCallback(
    (event: ReactKeyboardEvent<HTMLDivElement>) => {
      const step = event.shiftKey ? GRID * 5 : GRID;

      // ---- the clipboard and history keys, which need no selection to mean something -----
      // `isTypingTarget` has already let the field itself through, so a rename typed into the
      // inspector cannot be undone by a Ctrl+Z that the browser handled first.
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "z") {
        event.preventDefault();
        if (event.shiftKey) {
          doRedo();
        } else {
          doUndo();
        }
        return;
      }
      // Ctrl+Y is redo on Windows and macOS both, and a builder that only answers Ctrl+Shift+Z
      // is a builder whose redo nobody finds.
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "y") {
        event.preventDefault();
        doRedo();
        return;
      }
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "c") {
        event.preventDefault();
        copySelection();
        return;
      }
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "v") {
        event.preventDefault();
        pasteClipboard();
        return;
      }
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "d") {
        event.preventDefault();
        duplicateSelected();
        return;
      }
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "p") {
        // ⌘P brings focus to the palette, as the keyboard map promises. It is the entry point
        // that makes the palette reachable from the keyboard at all: without it, adding a
        // node without a pointer means Tab-ing through the whole toolbar first.
        event.preventDefault();
        if (filteredTypes.length > 0) {
          focusPaletteItem(filteredTypes[0].key);
        }
        return;
      }
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "a") {
        // Select every node. The inspector shows the last one in the array, so a select-all
        // still lands somewhere the user can act on rather than clearing everything.
        if (nodes.length > 0) {
          event.preventDefault();
          setSelection(selectAll(nodes.map((node) => node.id)));
        }
        return;
      }
      if (event.key === "Escape") {
        // One rule for the whole key, and it answers in priority order: a connection in
        // progress, then a selected edge, then the nodes. Escape is the gesture that says
        // "I did not mean that", and it has to reach the thing the user is actually holding
        // rather than whatever happened to be selected three gestures ago. With nothing
        // selected the key is *not* consumed, so the browser still closes the palette's
        // search box or a dialog.
        const step = whatEscapeClears(selection, linkDraft !== null);
        if (step === "connection") {
          event.preventDefault();
          setLinkDraft(null);
          setLinkNotice({ tone: "error", text: "Connection cancelled." });
          return;
        }
        if (step === "edge") {
          event.preventDefault();
          setSelection(clearSelection());
          return;
        }
        if (step === "nodes") {
          event.preventDefault();
          setSelection(clearSelection());
        }
        return;
      }

      if (event.key === "Delete" || event.key === "Backspace") {
        // What `Del` removes is one decision, made in `selection.ts` and asserted there: an
        // edge wins over a node selection (the user is pointing at a line and means the
        // line), and a group goes in one press so one undo brings the whole thing back.
        const target = deleteTarget(selection);
        if (target.kind === "edge") {
          event.preventDefault();
          removeEdge(target.id);
          return;
        }
        if (target.kind === "nodes") {
          event.preventDefault();
          removeNodes(target.ids);
        }
        return;
      }
      if (!selected && selectionCount === 0) {
        return;
      }
      const nudge: Record<string, [number, number]> = {
        ArrowUp: [0, -step],
        ArrowDown: [0, step],
        ArrowLeft: [-step, 0],
        ArrowRight: [step, 0],
      };
      const delta = nudge[event.key];
      if (!delta) {
        return;
      }
      event.preventDefault();
      // A multi-selection moves as one: nudging only the node the inspector is showing would
      // leave a marquee-selected group half moved and the user with no way to say so. The
      // set is the *drawn* selection, so an arrow key moves every outlined card — which is
      // what the user can see, and therefore what they expect to move.
      const moving = new Set(membersOf(selection));
      const before = currentSnapshot();
      const nextNodes = nodes.map((node) =>
        moving.has(node.id)
          ? {
              ...node,
              position: {
                x: clampCoord(snap(node.position.x + delta[0])),
                y: clampCoord(snap(node.position.y + delta[1])),
              },
            }
          : node,
      );
      commit("nudge", before, nextNodes, edges);
    },
    [commit, copySelection, currentSnapshot, doRedo, doUndo, duplicateSelected, edges, filteredTypes, focusPaletteItem, nodes, pasteClipboard, removeEdge, removeNodes, selected, selection, selectionCount],
  );

  // ---- actions ----------------------------------------------------------------------------

  const validateNow = useCallback(async () => {
    try {
      const result = await validateWorkflowGraph(workflowId, { nodes, edges });
      setFindings(result.findings);
      setProblemsOpen(true);
    } catch (error) {
      setFindings([
        {
          severity: "error",
          code: "validation_failed",
          message: error instanceof ApiError ? error.message : "Validation could not run.",
          node_id: null,
          related_node_id: null,
        },
      ]);
      setProblemsOpen(true);
    }
  }, [edges, nodes, workflowId]);

  const runOnce = useCallback(async () => {
    setRunning(true);
    setRunMessage(null);
    try {
      const response = await fetch(`/api/v1/workflows/${workflowId}/run`, {
        method: "POST",
        credentials: "same-origin",
        headers: { accept: "application/json" },
      });
      if (!response.ok) {
        const body = (await response.json().catch(() => null)) as
          | { error?: { message?: string } }
          | null;
        throw new Error(body?.error?.message ?? `Run failed with status ${response.status}.`);
      }
      setRunMessage("Run started. Its trace is on the rule's Runs tab.");
    } catch (error) {
      setRunMessage(error instanceof Error ? error.message : "The run could not be started.");
    } finally {
      setRunning(false);
    }
  }, [workflowId]);

  // ---- render -----------------------------------------------------------------------------

  if (loading) {
    return (
      <div className="flex min-h-[60vh] items-center justify-center" data-builder-state="loading">
        <div className="flex items-center gap-2 text-[13px] text-muted">
          <Loader2 className="h-4 w-4 animate-spin" aria-hidden="true" />
          Loading the builder…
        </div>
      </div>
    );
  }

  if (loadError) {
    return (
      <div
        className="flex min-h-[60vh] flex-col items-center justify-center gap-3"
        data-builder-state="error"
      >
        <p className="max-w-md text-center text-[13px] text-muted">{loadError}</p>
        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={() => void load()}
            className="inline-flex items-center gap-1.5 rounded-md bg-ink px-3 py-1.5 text-[12.5px] text-canvas"
            data-builder-retry
          >
            <RefreshCw className="h-3.5 w-3.5" aria-hidden="true" />
            Try again
          </button>
          <Link
            href={`/automations/${workflowId}`}
            className="rounded-md border border-line px-3 py-1.5 text-[12.5px]"
          >
            Back to the rule
          </Link>
        </div>
      </div>
    );
  }

  const errorCount = findings.filter((finding) => finding.severity === "error").length;
  const selectedNode = nodes.find((node) => node.id === selected) ?? null;
  const style = canvasStyle(viewport);

  return (
    <div className="flex h-[calc(100vh-2rem)] flex-col" data-builder>
      {/* ---- toolbar ---- */}
      <header
        className="flex flex-wrap items-center gap-2 border-b border-line bg-surface px-3 py-2"
        data-builder-toolbar
      >
        <button
          type="button"
          onClick={() => router.push(`/automations/${workflowId}`)}
          className="inline-flex items-center gap-1.5 rounded-md border border-line px-2 py-1.5 text-[12.5px] hover:bg-quiet-soft"
          data-builder-back
        >
          <ArrowLeft className="h-3.5 w-3.5" aria-hidden="true" />
          Rule
        </button>

        <SaveIndicator state={save} onReload={() => void load()} onKeepMine={keepMine} />

        <div className="ml-auto flex flex-wrap items-center gap-1.5">
          <ToolbarButton
            onClick={() => void validateNow()}
            icon={<Zap className="h-3.5 w-3.5" aria-hidden="true" />}
            label="Validate"
            testId="builder-validate"
          />
          <ToolbarButton
            onClick={() => void runOnce()}
            icon={
              running ? (
                <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden="true" />
              ) : (
                <Play className="h-3.5 w-3.5" aria-hidden="true" />
              )
            }
            label="Run once"
            testId="builder-run"
            disabled={definition ? !definition.projection.valid : false}
            title={
              definition && !definition.projection.valid
                ? (definition.projection.reason ?? "The graph has problems.")
                : undefined
            }
          />
          <ToolbarButton
            onClick={() => zoomBy(-1)}
            icon={<ZoomOut className="h-3.5 w-3.5" aria-hidden="true" />}
            label=""
            testId="builder-zoom-out"
          />
          <span className="min-w-[3.5rem] text-center font-mono text-[11.5px] text-muted">
            {Math.round(viewport.zoom * 100)}%
          </span>
          <ToolbarButton
            onClick={() => zoomBy(1)}
            icon={<ZoomIn className="h-3.5 w-3.5" aria-hidden="true" />}
            label=""
            testId="builder-zoom-in"
          />
          <ToolbarButton
            onClick={fit}
            icon={<Maximize className="h-3.5 w-3.5" aria-hidden="true" />}
            label="Fit"
            testId="builder-fit"
          />
          <ToolbarButton
            onClick={autoLayout}
            icon={<Wand2 className="h-3.5 w-3.5" aria-hidden="true" />}
            label="Auto layout"
            testId="builder-auto-layout"
            disabled={nodes.length === 0}
            title="Lay the nodes out by their edges"
          />
          <ToolbarButton
            onClick={() => setMinimapOpen((open) => !open)}
            icon={<LayoutGrid className="h-3.5 w-3.5" aria-hidden="true" />}
            label="Minimap"
            testId="builder-minimap"
            pressed={minimapOpen}
          />
          <ToolbarButton
            onClick={doUndo}
            icon={<Undo2 className="h-3.5 w-3.5" aria-hidden="true" />}
            label="Undo"
            testId="builder-undo"
            disabled={!canUndo}
            title={canUndo ? "Undo the last change" : "Nothing to undo"}
          />
          <ToolbarButton
            onClick={doRedo}
            icon={<Redo2 className="h-3.5 w-3.5" aria-hidden="true" />}
            label="Redo"
            testId="builder-redo"
            disabled={!canRedo}
            title={canRedo ? "Redo the change you undid" : "Nothing to redo"}
          />
          <ToolbarButton
            onClick={duplicateSelected}
            icon={<Copy className="h-3.5 w-3.5" aria-hidden="true" />}
            label="Duplicate"
            testId="builder-duplicate"
            disabled={!selected}
            title={selected ? "Duplicate the selected node" : "Select a node first"}
          />
          <ToolbarButton
            onClick={copySelection}
            icon={<ClipboardCopy className="h-3.5 w-3.5" aria-hidden="true" />}
            label="Copy"
            testId="builder-copy"
            disabled={!selected}
            title={selected ? "Copy the selection" : "Select a node first"}
          />
          <ToolbarButton
            onClick={pasteClipboard}
            icon={<ClipboardPaste className="h-3.5 w-3.5" aria-hidden="true" />}
            label={clipboardCount > 0 ? `Paste (${clipboardCount})` : "Paste"}
            testId="builder-paste"
            disabled={clipboardCount === 0}
            title={
              clipboardCount === 0
                ? "Nothing copied yet"
                : `Paste ${clipboardCount} copied node${clipboardCount === 1 ? "" : "s"}`
            }
          />
          <Link
            href={`/automations/${workflowId}`}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2 py-1.5 text-[12.5px] hover:bg-quiet-soft"
            data-builder-table-mode
          >
            <Table2 className="h-3.5 w-3.5" aria-hidden="true" />
            Table mode
          </Link>
        </div>
      </header>

      {runMessage ? (
        <p
          className="border-b border-line bg-quiet-soft/50 px-3 py-1.5 text-[12px] text-muted"
          data-builder-run-message
        >
          {runMessage}
        </p>
      ) : null}

      <div className="flex min-h-0 flex-1">
        {/* ---- palette ---- */}
        <aside
          className="flex w-[260px] shrink-0 flex-col border-r border-line bg-surface"
          data-builder-palette
        >
          <div className="border-b border-line p-2">
            <label className="block text-[11.5px] font-medium text-muted" htmlFor="builder-palette-search">
              Add a node
            </label>
            <input
              id="builder-palette-search"
              type="search"
              value={paletteQuery}
              onChange={(event) => setPaletteQuery(event.target.value)}
              placeholder="Search nodes…"
              className="mt-1 w-full rounded-md border border-line bg-canvas px-2 py-1.5 text-[12.5px]"
            />
          </div>
          <div className="min-h-0 flex-1 overflow-y-auto p-2">
            {registry?.categories.map((category) => {
              const inCategory = filteredTypes.filter(
                (nodeType) => nodeType.category === category,
              );
              if (inCategory.length === 0) {
                return null;
              }
              return (
                <section key={category} className="mb-3">
                  <h3 className="px-1 pb-1 text-[11px] font-medium uppercase tracking-wide text-muted">
                    {category}
                  </h3>
                  <ul className="flex flex-col gap-1">
                    {inCategory.map((nodeType) => (
                      <li key={nodeType.key}>
                        <button
                          type="button"
                          draggable
                          onDragStart={(event) => onPaletteDragStart(event, nodeType.key)}
                          onClick={() => addNode(nodeType)}
                          onKeyDown={(event) => onPaletteKeyDown(event, nodeType.key)}
                          className="w-full rounded-md border border-line bg-canvas px-2 py-1.5 text-left hover:border-accent"
                          data-palette-node={nodeType.key}
                          title={nodeType.summary}
                        >
                          <span className="block text-[12.5px] font-medium">{nodeType.label}</span>
                          <span className="block text-[11.5px] text-muted">{nodeType.summary}</span>
                        </button>
                      </li>
                    ))}
                  </ul>
                </section>
              );
            })}
            {registry && filteredTypes.length === 0 ? (
              <p className="px-1 text-[12px] text-muted" data-palette-empty>
                No node matches “{paletteQuery}”.
              </p>
            ) : null}
          </div>
        </aside>

        {/* ---- canvas ---- */}
        <div
          ref={canvasRef}
          className="relative min-w-0 flex-1 overflow-hidden bg-canvas"
          style={backgroundStyle(viewport)}
          onPointerDown={onCanvasPointerDown}
          onPointerMove={onPointerMove}
          onPointerUp={onPointerUp}
          onPointerLeave={onPointerUp}
          onDragOver={(event) => {
            // Without this the browser refuses the drop outright and the gesture ends with
            // nothing happening, which is indistinguishable from a broken drop handler.
            event.preventDefault();
            event.dataTransfer.dropEffect = "copy";
          }}
          onDrop={onCanvasDrop}
          onKeyDown={onCanvasKeyDown}
          tabIndex={0}
          role="application"
          aria-label="Workflow canvas"
          data-builder-canvas
        >
          {/* The marquee. Drawn in screen coordinates, not graph ones: a rubber band that
              scales with the zoom is a band the user cannot aim with. */}
          {marquee ? (
            <div
              className="pointer-events-none absolute border border-accent/60 bg-accent/10"
              style={{
                left: Math.min(marquee.x, marquee.x + marquee.w),
                top: Math.min(marquee.y, marquee.y + marquee.h),
                width: Math.abs(marquee.w),
                height: Math.abs(marquee.h),
              }}
              data-marquee
            />
          ) : null}

          {minimapOpen ? <Minimap nodes={nodes} viewport={viewport} selection={selection} onJump={jumpTo} /> : null}

          <div className="absolute inset-0" style={style}>
            <svg className="absolute left-0 top-0 overflow-visible" width="1" height="1">
              {edges.map((edge) => {
                const from = nodes.find((node) => node.id === edge.source);
                const to = nodes.find((node) => node.id === edge.target);
                if (!from || !to) {
                  return null;
                }
                const path = edgePath(from.position, to.position);
                const isSelected = selectedEdge === edge.id;
                return (
                  <g key={edge.id} data-edge={edge.id} data-edge-selected={isSelected ? "true" : undefined}>
                    {/* A fat transparent stroke under the visible line: a 2px bezier is close
                        to unclickable, and an edge you cannot select is an edge you cannot
                        delete. The hit area is the line's real geometry, not a bounding box. */}
                    <path
                      d={path}
                      fill="none"
                      stroke="transparent"
                      strokeWidth={14}
                      style={{ pointerEvents: "stroke", cursor: "pointer" }}
                      onPointerDown={(event) => {
                        event.stopPropagation();
                        setSelection((current) => selectEdge(current, edge.id));
                      }}
                    />
                    <path
                      d={path}
                      fill="none"
                      stroke={isSelected ? "var(--color-ink)" : "var(--color-line)"}
                      strokeWidth={isSelected ? 3 : 2}
                      markerEnd="url(#wf-arrow)"
                      style={{ pointerEvents: "none" }}
                    />
                    <text
                      x={(from.position.x + to.position.x) / 2 + CARD_W / 2}
                      y={(from.position.y + to.position.y) / 2 + CARD_H / 2 - 6}
                      textAnchor="middle"
                      className="fill-[var(--color-muted)] text-[10px]"
                    >
                      {edge.source_port}
                    </text>
                  </g>
                );
              })}
              <defs>
                <marker
                  id="wf-arrow"
                  viewBox="0 0 10 10"
                  refX="9"
                  refY="5"
                  markerWidth="6"
                  markerHeight="6"
                  orient="auto-start-reverse"
                >
                  <path d="M 0 0 L 10 5 L 0 10 z" fill="var(--color-muted)" />
                </marker>
              </defs>
            </svg>

            {nodes.map((node) => {
              const nodeType = nodeTypes.get(node.type);
              // One predicate decides the outline, the minimap dot and the status bar, so a
              // card cannot be drawn as selected in one place and unselected in another.
              const isSelected = isNodeSelected(selection, node.id);
              const hasProblems = findings.some(
                (finding) =>
                  finding.severity === "error" && finding.node_id === node.id,
              );
              return (
                <div
                  key={node.id}
                  className="absolute select-none rounded-lg border bg-surface shadow-sm"
                  style={{
                    left: node.position.x,
                    top: node.position.y,
                    width: CARD_W,
                    minHeight: CARD_H,
                    borderColor: hasProblems
                      ? "var(--color-accent)"
                      : isSelected
                        ? "var(--color-ink)"
                        : "var(--color-line)",
                    outline: isSelected ? "2px solid var(--color-ink)" : "none",
                    outlineOffset: 2,
                  }}
                  onPointerDown={(event) => {
                    event.stopPropagation();
                    onNodePointerDown(event, node);
                  }}
                  onClick={(event) => {
                    event.stopPropagation();
                    // A connection in progress swallows the click: the node the user just
                    // aimed at is the target, not a new selection.
                    if (linkDraft) {
                      connect(linkDraft.nodeId, linkDraft.port, node.id);
                      return;
                    }
                    // Selection is *not* decided here. `onNodePointerDown` already made the
                    // call — including the Shift+click toggle — and a second writer to the
                    // same state is what let a node be de-selected by one gesture and drawn as
                    // selected by the next. A click that started on empty canvas arrives here
                    // too, but the canvas handler owns that outcome.
                  }}
                  data-node-id={node.id}
                  data-node-type={node.type}
                  // A real marker, not a CSS string a probe has to parse. The QA pass used to
                  // read `style.includes("outline")`, which matches the `outline: none` React
                  // writes on *every* card — so "stillSelected: 3" after Escape was three
                  // unselected cards counted as selected, and the product was never wrong.
                  data-node-selected={isSelected ? "true" : "false"}
                  role="button"
                  tabIndex={-1}
                >
                  <div className="flex items-center gap-1.5 px-2.5 pt-2">
                    <GitBranch className="h-3.5 w-3.5 shrink-0 text-muted" aria-hidden="true" />
                    <span className="truncate text-[12.5px] font-medium">{node.label}</span>
                  </div>
                  <p className="truncate px-2.5 pb-2 pt-0.5 text-[11.5px] text-muted">
                    {nodeType?.summary ?? node.type}
                  </p>
                  <div className="absolute bottom-1.5 right-2 flex gap-1">
                    {(nodeType?.outputs ?? []).map((port) => (
                      <button
                        key={port.key}
                        type="button"
                        title={`Connect from ${port.label}`}
                        aria-label={`Connect from ${port.label}`}
                        className="h-2.5 w-2.5 rounded-full border border-muted bg-surface hover:bg-accent"
                        data-port-out={`${node.id}:${port.key}`}
                        data-port-key={port.key}
                        // The port the connection is currently leaving from is drawn as an
                        // accent dot, so "which of these four dots did I press?" has a
                        // visible answer while the gesture is in flight.
                        style={
                          linkDraft?.nodeId === node.id && linkDraft.port === port.key
                            ? { background: "var(--color-accent)", borderColor: "var(--color-accent)" }
                            : undefined
                        }
                        onClick={(event) => {
                          event.stopPropagation();
                          // Pressing a port focuses the node that owns it, so the inspector
                          // shows the card whose connection is being drawn — a connection
                          // started from a node nobody can see described is a gesture with no
                          // context.
                          setSelection(selectNode(node.id));
                          // Pressing a port starts a connection; pressing it again (or
                          // pressing Escape) cancels it. Without the toggle, a mis-click has
                          // no way out except finishing the link somewhere sensible.
                          setLinkDraft((current) =>
                            current?.nodeId === node.id && current.port === port.key
                              ? null
                              : { nodeId: node.id, port: port.key },
                          );
                          setLinkNotice(null);
                        }}
                      />
                    ))}
                  </div>
                </div>
              );
            })}
          </div>

          {nodes.length === 0 ? (
            <p
              className="absolute inset-0 flex items-center justify-center text-[13px] text-muted"
              data-builder-empty
            >
              This rule has no nodes. Add one from the palette.
            </p>
          ) : null}

          {/* The outcome of a connection attempt. Positioned inside the canvas (rather than in
              a toast) because the reason a drop was refused is only meaningful next to the
              nodes it names, and because the acceptance criteria ask for the refusal to be
              *visible* — a refused connection that says nothing is a dead gesture. */}
          {linkDraft ? (
            <p
              className="pointer-events-none absolute left-1/2 top-3 -translate-x-1/2 rounded-md border border-accent bg-surface px-2.5 py-1 text-[12px] shadow-sm"
              data-link-draft
            >
              Connecting from{" "}
              <strong>{nodeTypes.get(linkDraft.nodeId)?.label ?? linkDraft.nodeId}</strong> ·{" "}
              {linkDraft.port} — click a target node, or press Escape.
            </p>
          ) : null}
          {linkNotice ? (
            <p
              role="status"
              className="pointer-events-none absolute bottom-3 left-1/2 -translate-x-1/2 rounded-md border px-2.5 py-1 text-[12px] shadow-sm"
              style={{
                borderColor: linkNotice.tone === "error" ? "var(--color-accent)" : "var(--color-line)",
                color: linkNotice.tone === "error" ? "var(--color-ink)" : "var(--color-muted)",
                background: "var(--color-surface)",
              }}
              data-link-notice={linkNotice.tone}
            >
              {linkNotice.text}
            </p>
          ) : null}
        </div>

        {/* ---- inspector ---- */}
        <aside
          className="w-[320px] shrink-0 overflow-y-auto border-l border-line bg-surface"
          data-builder-inspector
        >
          {selectedNode ? (
            <NodeInspector
              node={selectedNode}
              nodeType={nodeTypes.get(selectedNode.type) ?? null}
              nodes={nodes}
              edges={edges}
              onChange={(patch) => updateNode(selectedNode.id, patch)}
              onDelete={() => removeNode(selectedNode.id)}
              onRemoveConnection={(edgeId) => removeEdge(edgeId)}
            />
          ) : (
            <div className="p-3">
              <h2 className="text-[13px] font-medium">Rule</h2>
              <p className="mt-1 text-[12px] text-muted">
                Select a node to edit it. The graph runs as {definition?.projection.step_count ?? 0}{" "}
                step(s) when the definition is valid.
              </p>
              {definition?.projection.reason ? (
                <p className="mt-2 rounded-md bg-accent-soft px-2 py-1.5 text-[12px] text-ink">
                  {definition.projection.reason}
                </p>
              ) : null}
            </div>
          )}
        </aside>
      </div>

      {/* ---- problems ---- */}
      <section
        className="shrink-0 border-t border-line bg-surface"
        data-builder-problems
      >
        <button
          type="button"
          onClick={() => setProblemsOpen((open) => !open)}
          className="flex w-full items-center gap-2 px-3 py-1.5 text-left text-[12px] font-medium"
          aria-expanded={problemsOpen}
          data-problems-toggle
        >
          {errorCount > 0 ? (
            <AlertTriangle className="h-3.5 w-3.5 text-accent" aria-hidden="true" />
          ) : (
            <CheckCircle2 className="h-3.5 w-3.5 text-positive" aria-hidden="true" />
          )}
          {errorCount > 0
            ? `${errorCount} problem${errorCount === 1 ? "" : "s"}`
            : findings.length > 0
              ? `${findings.length} warning${findings.length === 1 ? "" : "s"}`
              : "No problems"}
          <span className="ml-auto text-[11.5px] text-muted">
            {nodes.length} nodes · {edges.length} connections · v{definition?.graph_version ?? 0}
            {/* The selection, stated in words. A group highlight with nothing to say how
                many cards it covers is a status bar that cannot answer "what is this going to
                delete?" — and the count is read from the same predicate that draws them. */}
            {selectionCount > 0 || selectedEdge ? (
              <span data-builder-selection>
                {" · "}
                {selectedEdge
                  ? "1 connection selected (Del removes it)"
                  : `${selectionCount} selected`}
              </span>
            ) : null}
          </span>
        </button>
        {problemsOpen ? (
          <ul className="max-h-40 overflow-y-auto border-t border-line" data-problems-list>
            {findings.length === 0 ? (
              <li className="px-3 py-2 text-[12px] text-muted" data-problems-none>
                No problems. This graph is ready to run.
              </li>
            ) : (
              findings.map((finding, index) => (
                <li
                  key={`${finding.code}-${index}`}
                  className="flex items-center gap-2 border-b border-line px-3 py-1.5 text-[12px] last:border-b-0"
                  data-finding={finding.code}
                >
                  <span
                    className={
                      finding.severity === "error" ? "text-accent" : "text-caution"
                    }
                  >
                    {finding.severity === "error" ? "Error" : "Warning"}
                  </span>
                  <span className="min-w-0 flex-1">{finding.message}</span>
                  {finding.node_id ? (
                    <button
                      type="button"
                      onClick={() => setSelection(selectNode(finding.node_id as string))}
                      className="shrink-0 rounded-md border border-line px-1.5 py-0.5 text-[11.5px] hover:bg-quiet-soft"
                      data-finding-jump={finding.node_id}
                    >
                      Go to node
                    </button>
                  ) : null}
                </li>
              ))
            )}
          </ul>
        ) : null}
      </section>
    </div>
  );
}

// ---------------------------------------------------------------------------------------------
// Pieces
// ---------------------------------------------------------------------------------------------

/** The toolbar's save state, which is the toolbar's honesty. */
function SaveIndicator({
  state,
  onReload,
  onKeepMine,
}: {
  state: SaveState;
  onReload: () => void;
  onKeepMine: () => void;
}) {
  if (state.kind === "conflict") {
    // Two exits, and both of them work. The server's message offers the author a choice —
    // "reload to see their change, or keep editing to overwrite it" — and until this tick
    // only the first half was real: the tab kept quoting the version it had loaded, so every
    // later save was refused again and "keep editing" was a sentence describing a dead end.
    // Overwriting is the destructive half, so it is behind a confirm that names what it
    // destroys; a one-click "overwrite" would be the same silent loss the criterion exists
    // to prevent, one click earlier.
    const resolution = resolveConflict({ message: state.message, version: state.version });
    return (
      <span
        className="flex items-center gap-2 rounded-md bg-accent-soft px-2.5 py-1 text-[12px] text-ink"
        data-save-state="conflict"
        role="alert"
      >
        <AlertTriangle className="h-3.5 w-3.5" aria-hidden="true" />
        {state.message}
        <button
          type="button"
          onClick={onReload}
          className="rounded border border-ink px-1.5 py-0.5 text-[11.5px]"
          data-save-reload
        >
          Reload
        </button>
        {resolution.requiresConfirmation ? (
          <button
            type="button"
            onClick={onKeepMine}
            className="rounded border border-ink px-1.5 py-0.5 text-[11.5px]"
            data-save-keep-mine
            title={resolution.confirmLabel}
          >
            {resolution.confirmLabel}
          </button>
        ) : null}
      </span>
    );
  }
  if (state.kind === "error") {
    return (
      <span className="rounded-md bg-accent-soft px-2.5 py-1 text-[12px] text-ink" data-save-state="error">
        {state.message}
      </span>
    );
  }
  const text =
    state.kind === "dirty"
      ? "Unsaved changes"
      : state.kind === "saving"
        ? "Saving…"
        : state.kind === "saved"
          ? "Saved"
          : "Saved";
  return (
    <span
      className={`rounded-md px-2.5 py-1 text-[12px] ${
        state.kind === "dirty" ? "bg-caution-soft text-caution" : "text-muted"
      }`}
      data-save-state={state.kind}
    >
      {text}
    </span>
  );
}

/** One toolbar control. */
function ToolbarButton({
  onClick,
  icon,
  label,
  testId,
  disabled,
  title,
  pressed,
}: {
  onClick: () => void;
  icon: React.ReactNode;
  label: string;
  testId: string;
  disabled?: boolean;
  title?: string;
  /** A toggle's state. Rendered as `aria-pressed` and a visible tint, because a button that
   *  looks the same whether the feature is on or off is a control that cannot be read. */
  pressed?: boolean;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      title={title}
      aria-label={label || undefined}
      aria-pressed={pressed}
      data-pressed={pressed === undefined ? undefined : pressed ? "on" : "off"}
      className={
        pressed
          ? "inline-flex items-center gap-1.5 rounded-md border border-line bg-quiet-soft px-2 py-1.5 text-[12.5px] hover:bg-quiet-soft disabled:opacity-40"
          : "inline-flex items-center gap-1.5 rounded-md border border-line px-2 py-1.5 text-[12.5px] hover:bg-quiet-soft disabled:opacity-40 disabled:hover:bg-transparent"
      }
      data-testid={testId}
    >
      {icon}
      {label}
    </button>
  );
}

/**
 * The inspector for one node.
 *
 * The form is generated from the registry's schema, which is the same schema the server
 * validates against — a field here that the server ignores would be a form that accepts
 * anything, so there is no hand-written field in this component.
 */
function NodeInspector({
  node,
  nodeType,
  nodes,
  edges,
  onChange,
  onDelete,
  onRemoveConnection,
}: {
  node: GraphNode;
  nodeType: GraphNodeType | null;
  nodes: GraphNode[];
  edges: GraphEdge[];
  onChange: (patch: Partial<GraphNode>) => void;
  onDelete: () => void;
  onRemoveConnection: (edgeId: string) => void;
}) {
  if (!nodeType) {
    return (
      <div className="p-3" data-inspector-unknown={node.type}>
        <h2 className="text-[13px] font-medium">{node.label}</h2>
        <p className="mt-1 text-[12px] text-muted">
          “{node.type}” is not a node type this build knows. Save the rule to see the problem it
          causes.
        </p>
        <button
          type="button"
          onClick={onDelete}
          className="mt-2 rounded-md border border-line px-2 py-1 text-[12px]"
          data-inspector-delete
        >
          Delete this node
        </button>
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-3 p-3" data-inspector={node.id}>
      <div>
        <h2 className="text-[13px] font-medium">{nodeType.label}</h2>
        <p className="text-[12px] text-muted">{nodeType.summary}</p>
      </div>

      <label className="block text-[12px] font-medium" htmlFor={`label-${node.id}`}>
        Label
        <input
          id={`label-${node.id}`}
          type="text"
          value={node.label}
          maxLength={80}
          onChange={(event) => onChange({ label: event.target.value })}
          className="mt-1 w-full rounded-md border border-line bg-canvas px-2 py-1.5 text-[12.5px]"
          data-inspector-field="label"
        />
      </label>

      {nodeType.params.map((field) => {
        const value = node.params[field.key];
        const inputId = `param-${node.id}-${field.key}`;
        return (
          <label key={field.key} className="block text-[12px] font-medium" htmlFor={inputId}>
            {field.label}
            {field.required ? <span className="text-accent"> *</span> : null}
            {field.kind === "select" ? (
              <select
                id={inputId}
                value={typeof value === "string" ? value : ""}
                onChange={(event) =>
                  onChange({ params: { ...node.params, [field.key]: event.target.value } })
                }
                className="mt-1 w-full rounded-md border border-line bg-canvas px-2 py-1.5 text-[12.5px]"
                data-inspector-field={field.key}
              >
                <option value="">Choose…</option>
                {field.options.map((option) => (
                  <option key={option} value={option}>
                    {option}
                  </option>
                ))}
              </select>
            ) : field.kind === "textarea" ? (
              <textarea
                id={inputId}
                value={typeof value === "string" ? value : ""}
                onChange={(event) =>
                  onChange({ params: { ...node.params, [field.key]: event.target.value } })
                }
                rows={3}
                className="mt-1 w-full rounded-md border border-line bg-canvas px-2 py-1.5 font-mono text-[12px]"
                data-inspector-field={field.key}
              />
            ) : (
              <input
                id={inputId}
                type={field.kind === "number" ? "number" : "text"}
                value={typeof value === "string" || typeof value === "number" ? String(value) : ""}
                onChange={(event) => {
                  const raw = event.target.value;
                  onChange({
                    params: {
                      ...node.params,
                      [field.key]: field.kind === "number" ? Number(raw) : raw,
                    },
                  });
                }}
                className="mt-1 w-full rounded-md border border-line bg-canvas px-2 py-1.5 text-[12.5px]"
                data-inspector-field={field.key}
              />
            )}
            {field.help ? <span className="mt-0.5 block text-[11.5px] text-muted">{field.help}</span> : null}
          </label>
        );
      })}

      {nodeType.outputs.length > 0 ? (
        <div>
          <h3 className="text-[12px] font-medium">Connections</h3>
          <ul className="mt-1 flex flex-col gap-1" data-inspector-connections>
            {edges
              .filter((edge) => edge.source === node.id)
              .map((edge) => {
                const target = nodes.find((candidate) => candidate.id === edge.target);
                return (
                  <li
                    key={edge.id}
                    className="flex items-center gap-1.5 rounded-md border border-line px-2 py-1 text-[11.5px]"
                    data-connection={edge.id}
                  >
                    <span className="font-mono">{edge.source_port}</span>
                    <span className="min-w-0 flex-1 truncate">
                      → {target?.label ?? edge.target}
                    </span>
                    <button
                      type="button"
                      onClick={() => onRemoveConnection(edge.id)}
                      className="shrink-0 rounded border border-line px-1 text-[11px] hover:bg-quiet-soft"
                      data-connection-remove={edge.id}
                      aria-label={`Remove the connection to ${target?.label ?? edge.target}`}
                    >
                      Remove
                    </button>
                  </li>
                );
              })}
            {edges.filter((edge) => edge.source === node.id).length === 0 ? (
              <li className="text-[11.5px] text-muted">No connections yet.</li>
            ) : null}
          </ul>
        </div>
      ) : null}

      <button
        type="button"
        onClick={onDelete}
        className="rounded-md border border-line px-2 py-1 text-left text-[12px] hover:bg-quiet-soft"
        data-inspector-delete
      >
        Delete this node
      </button>
    </div>
  );
}

// ---------------------------------------------------------------------------------------------
// Geometry and helpers
// ---------------------------------------------------------------------------------------------

/** A bezier between two cards, leaving the source's bottom and arriving at the target's top. */
export function edgePath(
  from: { x: number; y: number },
  to: { x: number; y: number },
): string {
  const x1 = from.x + CARD_W / 2;
  const y1 = from.y + PORT_Y;
  const x2 = to.x + CARD_W / 2;
  const y2 = to.y;
  const mid = Math.max(24, (y2 - y1) / 2);
  return `M ${x1} ${y1} C ${x1} ${y1 + mid}, ${x2} ${y2 - mid}, ${x2} ${y2}`;
}

function canvasStyle(viewport: { x: number; y: number; zoom: number }): CSSProperties {
  return {
    transform: `translate(${viewport.x}px, ${viewport.y}px) scale(${viewport.zoom})`,
    transformOrigin: "0 0",
  };
}

/** The dotted grid, so the canvas reads as a canvas without the grid fighting the nodes. */
function backgroundStyle(viewport: { x: number; y: number; zoom: number }): CSSProperties {
  const step = GRID * 2 * viewport.zoom;
  return {
    backgroundImage: "radial-gradient(var(--color-line) 1px, transparent 1px)",
    backgroundSize: `${Math.max(6, step)}px ${Math.max(6, step)}px`,
    backgroundPosition: `${viewport.x}px ${viewport.y}px`,
  };
}

function viewportCentre(
  element: HTMLDivElement | null,
  viewport: { x: number; y: number; zoom: number },
): { x: number; y: number } {
  if (!element) {
    return { x: 80, y: 80 };
  }
  return {
    x: (element.clientWidth / 2 - viewport.x) / viewport.zoom - CARD_W / 2,
    y: (element.clientHeight / 2 - viewport.y) / viewport.zoom - CARD_H / 2,
  };
}

function snap(value: number): number {
  return Math.round(value / GRID) * GRID;
}

function clampCoord(value: number): number {
  if (!Number.isFinite(value)) {
    return 0;
  }
  return Math.min(COORD_LIMIT, Math.max(-COORD_LIMIT, value));
}

function clampZoom(value: number): number {
  if (!Number.isFinite(value)) {
    return 1;
  }
  return Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, value));
}

/** A node id that cannot collide with one already in the graph. */
function uniqueId(prefix: string, nodes: GraphNode[]): string {
  return uniqueAmong(prefix, nodes.map((node) => node.id));
}

/** An edge id that cannot collide with one already in the graph. */
function uniqueEdgeId(edges: GraphEdge[]): string {
  return uniqueAmong("e", edges.map((edge) => edge.id));
}

function uniqueAmong(prefix: string, taken: string[]): string {
  const used = new Set(taken);
  let counter = taken.length + 1;
  let candidate = `${prefix}-${counter}`;
  while (used.has(candidate)) {
    counter += 1;
    candidate = `${prefix}-${counter}`;
  }
  return candidate;
}

function filterTypes(
  registry: GraphNodeTypes | null,
  query: string,
): GraphNodeType[] {
  if (!registry) {
    return [];
  }
  const needle = query.trim().toLowerCase();
  if (!needle) {
    return registry.node_types;
  }
  return registry.node_types.filter(
    (nodeType) =>
      nodeType.label.toLowerCase().includes(needle) ||
      nodeType.key.toLowerCase().includes(needle) ||
      nodeType.summary.toLowerCase().includes(needle),
  );
}

function isTypingTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) {
    return false;
  }
  return (
    target.tagName === "INPUT" ||
    target.tagName === "TEXTAREA" ||
    target.tagName === "SELECT" ||
    target.isContentEditable
  );
}

/**
 * The minimap: the whole graph at a glance, with the part you are looking at marked.
 *
 * Two decisions that a minimap usually gets wrong:
 *
 * * **It scales to the nodes, not to a fixed scale.** A fixed scale makes a five-node graph a
 *   speck in the corner and a fifty-node graph an unreadable smear; here the bounds are the
 *   nodes' own, so the map is full whatever the size, and the scale itself is printed so a
 *   user knows the map is not lying about distance.
 * * **It is a button, not a decoration.** Clicking it centres the viewport there. A minimap
 *   that only shows you where you are, on a graph of any size, is a picture.
 *
 * Selection is read through a ref rather than a prop so that dragging a marquee across forty
 * nodes does not re-render the map on every pointer sample — the map only needs to know *that*
 * something is selected, which is why it takes the count.
 */
function Minimap({
  nodes,
  viewport,
  selection,
  onJump,
}: {
  nodes: GraphNode[];
  viewport: { x: number; y: number; zoom: number };
  selection: CanvasSelection;
  onJump: (x: number, y: number) => void;
}) {
  const W = 180;
  const H = 120;
  const PAD = 8;

  if (nodes.length === 0) {
    // An empty box with a scale would claim a graph exists. Say what is actually true.
    return (
      <div
        className="absolute bottom-3 right-3 rounded-md border border-line bg-panel/90 p-2 text-[11px] text-muted"
        data-minimap
        data-minimap-state="empty"
      >
        No nodes yet
      </div>
    );
  }

  const minX = Math.min(...nodes.map((node) => node.position.x));
  const minY = Math.min(...nodes.map((node) => node.position.y));
  const maxX = Math.max(...nodes.map((node) => node.position.x + CARD_W));
  const maxY = Math.max(...nodes.map((node) => node.position.y + CARD_H));
  const spanX = Math.max(maxX - minX, 1);
  const spanY = Math.max(maxY - minY, 1);
  const scale = Math.min((W - PAD * 2) / spanX, (H - PAD * 2) / spanY);

  const project = (x: number, y: number) => ({
    left: PAD + (x - minX) * scale,
    top: PAD + (y - minY) * scale,
  });

  // The viewport rectangle, inverted back into graph space: the map is drawn in graph units
  // scaled down, and the visible window is the inverse of that.
  const viewWidth = (W - PAD * 2) / scale;
  const viewHeight = (H - PAD * 2) / scale;
  const view = project(-viewport.x / viewport.zoom, -viewport.y / viewport.zoom);
  // The same predicate the canvas outline uses, so a card cannot be highlighted on the map
  // and unhighlighted on the board. The map only needs to know *that* something is selected,
  // which is why it renders the count as an attribute.
  const selected = new Set(membersOf(selection));

  return (
    <div
      className="absolute bottom-3 right-3 rounded-md border border-line bg-panel/90 p-1"
      data-minimap
      data-minimap-state="ready"
      data-minimap-selection={selectionSize(selection)}
    >
      <button
        type="button"
        className="relative block"
        style={{ width: W, height: H }}
        onClick={(event) => {
          const rect = event.currentTarget.getBoundingClientRect();
          onJump(
            minX + (event.clientX - rect.left - PAD) / scale,
            minY + (event.clientY - rect.top - PAD) / scale,
          );
        }}
        aria-label="Centre the view here"
        data-minimap-canvas
      >
        {nodes.map((node) => {
          const at = project(node.position.x, node.position.y);
          return (
            <span
              key={node.id}
              className={
                selected.has(node.id)
                  ? "absolute rounded-[2px] bg-accent"
                  : "absolute rounded-[2px] bg-muted/60"
              }
              style={{
                left: at.left,
                top: at.top,
                width: Math.max(CARD_W * scale, 2),
                height: Math.max(CARD_H * scale, 2),
              }}
              data-minimap-node={node.id}
            />
          );
        })}
        <span
          className="pointer-events-none absolute border border-foreground/50"
          style={{
            left: view.left,
            top: view.top,
            width: Math.min(viewWidth * scale, W),
            height: Math.min(viewHeight * scale, H),
          }}
          data-minimap-viewport
        />
      </button>
      <p className="px-1 pt-1 text-[10px] text-muted">
        {nodes.length} node{nodes.length === 1 ? "" : "s"} · {(1 / scale).toFixed(0)}px/px
      </p>
    </div>
  );
}

export { GRID, CARD_W, CARD_H, snap, clampZoom, uniqueId, filterTypes };
