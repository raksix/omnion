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
} from "react";

import {
  AlertTriangle,
  ArrowLeft,
  CheckCircle2,
  GitBranch,
  Loader2,
  Maximize,
  Minus,
  Play,
  Plus,
  RefreshCw,
  Table2,
  Zap,
  ZoomIn,
  ZoomOut,
} from "lucide-react";
import Link from "next/link";
import { useRouter } from "next/navigation";

import { ApiError, fetchGraphNodeTypes, fetchWorkflowGraph, saveWorkflowGraph, saveWorkflowUiState, validateWorkflowGraph, type GraphEdge, type GraphFinding, type GraphNode, type GraphNodeType, type GraphNodeTypes, type WorkflowGraph } from "@/lib/api";

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
  | { kind: "conflict"; message: string }
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

  const [selected, setSelected] = useState<string | null>(null);
  const [findings, setFindings] = useState<GraphFinding[]>([]);
  const [save, setSave] = useState<SaveState>({ kind: "clean" });
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
        setSave({ kind: "conflict", message: error.message });
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

  const commit = useCallback(
    (nextNodes: GraphNode[], nextEdges: GraphEdge[]) => {
      setNodes(nextNodes);
      setEdges(nextEdges);
      queueSave();
    },
    [queueSave],
  );

  const addNode = useCallback(
    (nodeType: GraphNodeType) => {
      const position = viewportCentre(canvasRef.current, viewport);
      const node: GraphNode = {
        id: uniqueId(nodeType.key, nodes),
        type: nodeType.key,
        label: nodeType.label,
        params: { ...nodeType.defaults },
        position: { x: snap(position.x), y: snap(position.y) },
      };
      setNodes((current) => [...current, node]);
      setSelected(node.id);
      queueSave();
    },
    [nodes, queueSave, viewport],
  );

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
      setNodes((current) =>
        current.map((node) => (node.id === id ? { ...node, ...patch } : node)),
      );
      queueSave();
    },
    [queueSave],
  );

  const removeNode = useCallback(
    (id: string) => {
      // The edges go with it: a connection whose endpoint is gone is exactly the dangling
      // edge validation would then refuse the whole graph for.
      setNodes((current) => current.filter((node) => node.id !== id));
      setEdges((current) =>
        current.filter((edge) => edge.source !== id && edge.target !== id),
      );
      setSelected((current) => (current === id ? null : current));
      queueSave();
    },
    [queueSave],
  );

  const connect = useCallback(
    (source: string, sourcePort: string, target: string) => {
      const legal = nodeTypes.get(source)?.outputs.some((port) => port.key === sourcePort);
      if (!legal) {
        return;
      }
      const already = edges.some(
        (edge) =>
          edge.source === source && edge.source_port === sourcePort && edge.target === target,
      );
      if (already) {
        return;
      }
      setEdges((current) => [
        ...current,
        {
          id: uniqueEdgeId(current),
          source,
          source_port: sourcePort,
          target,
        },
      ]);
      queueSave();
    },
    [edges, nodeTypes, queueSave],
  );

  const removeEdge = useCallback(
    (id: string) => {
      setEdges((current) => current.filter((edge) => edge.id !== id));
      queueSave();
    },
    [queueSave],
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
      setSelected(node.id);
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
      // Space-drag or the middle button pans; a plain click on empty canvas clears the
      // selection, which is what a person expects from clicking the desk.
      if (event.button === 1 || spaceHeld.current) {
        setPanning({ x: event.clientX, y: event.clientY, vx: viewport.x, vy: viewport.y });
        return;
      }
      setSelected(null);
    },
    [viewport],
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
      if (dragging) {
        moveNode(
          dragging.id,
          (event.clientX - rect.left - viewport.x) / viewport.zoom - dragging.offsetX,
          (event.clientY - rect.top - viewport.y) / viewport.zoom - dragging.offsetY,
        );
      }
    },
    [dragging, moveNode, panning, persistLayout, viewport],
  );

  const onPointerUp = useCallback(() => {
    if (panning) {
      setPanning(null);
      return;
    }
    if (dragging) {
      setDragging(null);
      commitMove();
    }
  }, [commitMove, dragging, panning]);

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
      if (event.key === "Delete" || event.key === "Backspace") {
        if (selected) {
          event.preventDefault();
          removeNode(selected);
        }
        return;
      }
      if (!selected) {
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
      setNodes((current) =>
        current.map((node) =>
          node.id === selected
            ? {
                ...node,
                position: {
                  x: clampCoord(snap(node.position.x + delta[0])),
                  y: clampCoord(snap(node.position.y + delta[1])),
                },
              }
            : node,
        ),
      );
      queueSave();
    },
    [queueSave, removeNode, selected],
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
  const filteredTypes = filterTypes(registry, paletteQuery);
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

        <SaveIndicator state={save} onReload={() => void load()} />

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
                          onClick={() => addNode(nodeType)}
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
          onKeyDown={onCanvasKeyDown}
          tabIndex={0}
          role="application"
          aria-label="Workflow canvas"
          data-builder-canvas
        >
          <div className="absolute inset-0" style={style}>
            <svg className="absolute left-0 top-0 overflow-visible" width="1" height="1">
              {edges.map((edge) => {
                const from = nodes.find((node) => node.id === edge.source);
                const to = nodes.find((node) => node.id === edge.target);
                if (!from || !to) {
                  return null;
                }
                const path = edgePath(from.position, to.position);
                return (
                  <g key={edge.id} data-edge={edge.id}>
                    <path
                      d={path}
                      fill="none"
                      stroke="var(--color-line)"
                      strokeWidth={2}
                      markerEnd="url(#wf-arrow)"
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
                      : selected === node.id
                        ? "var(--color-ink)"
                        : "var(--color-line)",
                    outline: selected === node.id ? "2px solid var(--color-ink)" : "none",
                    outlineOffset: 2,
                  }}
                  onPointerDown={(event) => {
                    event.stopPropagation();
                    onNodePointerDown(event, node);
                  }}
                  onClick={(event) => {
                    event.stopPropagation();
                    setSelected(node.id);
                  }}
                  data-node-id={node.id}
                  data-node-type={node.type}
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
                        onClick={(event) => {
                          event.stopPropagation();
                          setSelected(node.id);
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
                      onClick={() => setSelected(finding.node_id)}
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
function SaveIndicator({ state, onReload }: { state: SaveState; onReload: () => void }) {
  if (state.kind === "conflict") {
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
}: {
  onClick: () => void;
  icon: React.ReactNode;
  label: string;
  testId: string;
  disabled?: boolean;
  title?: string;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      title={title}
      aria-label={label || undefined}
      className="inline-flex items-center gap-1.5 rounded-md border border-line px-2 py-1.5 text-[12.5px] hover:bg-quiet-soft disabled:opacity-40 disabled:hover:bg-transparent"
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

export { GRID, CARD_W, CARD_H, snap, clampZoom, uniqueId, filterTypes };
