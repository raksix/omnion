"use client";

/**
 * The workflow editor canvas (REQ-086, slice 2).
 *
 * The shape of the screen is three columns and a strip, and each part has one job:
 *
 * - **Palette (left).** What can be added, grouped by category, searchable, and every entry
 *   carries the reason it cannot be placed right now. A disabled node with no reason is the
 *   single most common way an editor lies to the person using it.
 * - **Canvas (centre).** The document. Pan, zoom, marquee select, drag, connect, and every
 *   operation is keyboard-reachable — the REQ asks for a full keyboard path, and a canvas that
 *   needs a mouse is a canvas that excludes a person.
 * - **Inspector (right).** One node's parameters, generated from the registry's own schema, plus
 *   settings and the errors that apply to it. The schema is the registry's, never a second copy
 *   written here.
 * - **Strip (bottom).** Counts, the validation badge, and the autosave state. The REQ puts the
 *   last-saved time here, so a person always knows whether what they are looking at is stored.
 *
 * What this file deliberately does **not** do: validate. Every green tick and every red badge
 * comes from `POST …/graph/validate`, so the panel and the save agree by construction. A second
 * validator on the client is a second answer, and the one a person is looking at is the one
 * that has to be right.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  fetchGraph,
  fetchNodeTypes,
  previewExpressions,
  saveGraph,
  validateGraph,
} from "@/lib/api";
import { ApiError } from "@/lib/api";
import type {
  ExpressionPreview,
  GraphDocument,
  GraphIssue,
  NodeParam,
  NodeType,
} from "@/lib/types";

import {
  MAX_ZOOM,
  MIN_ZOOM,
  NODE_HEIGHT,
  NODE_WIDTH,
  SNAP,
  addNode,
  addNote,
  apply,
  applyLayout,
  boundsOf,
  clamp,
  connect,
  copySelection,
  deleteConnections,
  deleteNodes,
  emptyState,
  hasConnection,
  labelConnection,
  layeredLayout,
  moveNode,
  nudgeNode,
  pasteClipboard,
  redo,
  redoLabel,
  refuseConnect,
  sameDocument,
  selectAll,
  selectOnly,
  toCanvas,
  toggleSelection,
  undo,
  undoLabel,
  viewportFor,
} from "./graph-model";
import type { CanvasState, Clipboard, ConnectRefusal, Viewport } from "./graph-model";

/** The palette's category order, and the label each one gets. */
const CATEGORY_LABEL: Record<string, string> = {
  trigger: "Triggers",
  flow: "Flow",
  code: "Code",
  data: "Data",
  integration: "Integrations",
  helper: "Helpers",
  error: "Errors",
};

/** The order the palette lists the categories in, which is not alphabetical on purpose. */
const CATEGORY_ORDER = [
  "trigger",
  "flow",
  "code",
  "data",
  "integration",
  "helper",
  "error",
];

/** Palette entries a person has added recently, most recent first. */
const RECENT_KEY = "omnion.graph.recents";
const RECENT_LIMIT = 6;

/** Where the palette keeps what was added recently. */
function readRecents(): string[] {
  try {
    const raw = window.localStorage.getItem(RECENT_KEY);
    if (!raw) return [];
    const parsed: unknown = JSON.parse(raw);
    return Array.isArray(parsed) ? parsed.filter((item): item is string => typeof item === "string") : [];
  } catch {
    // A browser with storage disabled still gets a palette; it just forgets.
    return [];
  }
}

function writeRecents(keys: string[]): void {
  try {
    window.localStorage.setItem(RECENT_KEY, JSON.stringify(keys.slice(0, RECENT_LIMIT)));
  } catch {
    // As above: losing the recents list is not worth failing an edit over.
  }
}

/** How long after the last edit the autosave fires. The REQ's number. */
const AUTOSAVE_MS = 2000;

/**
 * How long the canvas waits after a keystroke before asking for a preview. Longer than the
 * autosave debounce on purpose: a save is worth doing promptly because it is a write the
 * person may walk away from, whereas a preview is only worth doing once they stop typing.
 */
const PREVIEW_DEBOUNCE_MS = 400;

/**
 * The sample an expression is evaluated against until somebody changes it.
 *
 * It is real, shaped data rather than a stub, and the inspector exposes it as editable — an
 * expression previewed against `{}` tells the reader nothing, and a preview whose sample they
 * cannot see is a preview they cannot reason about. The server never supplies this itself:
 * a preview that could reach live data would read rows the person editing may not be allowed
 * to see, and would answer differently on every call.
 */
const DEFAULT_SAMPLE: Record<string, unknown> = {
  node: {
    items: [
      { title: "First order", total: 42 },
      { title: "Second order", total: 17 },
    ],
    count: 2,
    author: { name: "Ada Lovelace", email: "ada@example.com" },
    summary: null,
  },
  vars: { site: "example.com", currency: "EUR" },
};

/** Below this width the canvas is read-only, which the REQ names at 900 px. */
const MOBILE_BREAKPOINT = 900;

/** What a save is doing, as the strip shows it. */
type SaveState =
  | { kind: "clean" }
  | { kind: "dirty" }
  | { kind: "saving" }
  | { kind: "saved"; at: Date }
  | { kind: "failed"; message: string };

/** The viewport size, tracked so `fit` and zoom-to-selection have a frame to work with. */
type Frame = { width: number; height: number };

/** One in-flight connection drag. */
type PendingWire = { from: string; from_port: string; at: { x: number; y: number } };

/** One node drag in progress, so a pointerup knows what moved and can name the undo. */
type Drag = {
  kind: "node";
  start: { x: number; y: number };
  origin: Record<string, { x: number; y: number }>;
  moved: boolean;
};

/** A marquee selection in progress. */
type Marquee = { start: { x: number; y: number }; at: { x: number; y: number }; additive: boolean };

/** Which end of a wire is being dragged, for the reconnect path. */
type Reconnecting = { index: number; end: "from" | "to" };

/** The shortcuts the sheet lists, and the keys that fire them. One list, so they cannot drift. */
type Binding = { keys: string; label: string };

const SHORTCUTS: Binding[] = [
  { keys: "⌘K", label: "Open the palette" },
  { keys: "⌘S", label: "Save" },
  { keys: "⌘Z", label: "Undo" },
  { keys: "⌘⇧Z", label: "Redo" },
  { keys: "⌘C", label: "Copy the selection" },
  { keys: "⌘V", label: "Paste" },
  { keys: "⌘D", label: "Duplicate" },
  { keys: "⌘A", label: "Select every node" },
  { keys: "⌘0", label: "Fit the graph" },
  { keys: "⌘⇧L", label: "Auto-layout" },
  { keys: "/", label: "Search the palette" },
  { keys: "F2", label: "Rename the selection" },
  { keys: "Delete", label: "Delete the selection" },
  { keys: "Space + drag", label: "Pan" },
  { keys: "↑ ↓ ← →", label: "Nudge by one unit" },
  { keys: "Esc", label: "Cancel" },
];

/** The portal's palette palette, so the canvas reads as part of the panel and not an app in it. */
const PANEL = {
  surface: "bg-card text-ink border-line",
  muted: "text-muted",
  accent: "text-accent",
} as const;

export function WorkflowCanvas({ workflowId }: { workflowId: string }) {
  const [state, setState] = useState<CanvasState>(emptyState);
  const [revision, setRevision] = useState(0);
  const [baseDocument, setBaseDocument] = useState<GraphDocument | null>(null);
  const [save, setSave] = useState<SaveState>({ kind: "clean" });
  const [issues, setIssues] = useState<GraphIssue[]>([]);
  const [library, setLibrary] = useState<NodeType[]>([]);
  const [libraryState, setLibraryState] = useState<"loading" | "ready" | "failed">("loading");
  const [loadError, setLoadError] = useState<string | null>(null);
  const [conflict, setConflict] = useState<{ loaded: number; current: number } | null>(null);
  const [frame, setFrame] = useState<Frame>({ width: 0, height: 0 });
  const [search, setSearch] = useState("");
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [minimap, setMinimap] = useState(true);
  const [showShortcuts, setShowShortcuts] = useState(false);
  const [narrow, setNarrow] = useState(false);
  const [toast, setToast] = useState<string | null>(null);
  const [wire, setWire] = useState<PendingWire | null>(null);
  const [reconnecting, setReconnecting] = useState<Reconnecting | null>(null);
  const [refusal, setRefusal] = useState<ConnectRefusal | null>(null);
  const [marquee, setMarquee] = useState<Marquee | null>(null);
  const [drag, setDrag] = useState<Drag | null>(null);
  const [selectedEdge, setSelectedEdge] = useState<number | null>(null);
  const [renaming, setRenaming] = useState<string | null>(null);
  const [snapOn, setSnapOn] = useState(true);
  const [panning, setPanning] = useState<{ start: { x: number; y: number }; origin: Viewport } | null>(
    null,
  );
  const [recents, setRecents] = useState<string[]>([]);

  const surfaceRef = useRef<HTMLDivElement | null>(null);
  const clipboard = useRef<Clipboard | null>(null);
  // The autosave handle is a browser timer, so the type is the DOM's. `ReturnType<typeof
  // setTimeout>` resolves to Node's `Timeout` here (the `@types/node` global wins), and
  // assigning a `number` to it is a type error that says nothing about the code being wrong —
  // the mismatch is in which `setTimeout` the declaration picked up.
  const autosave = useRef<number | null>(null);
  const validating = useRef(false);

  const announce = useCallback((message: string) => {
    setToast(message);
    window.setTimeout(() => setToast(null), 2600);
  }, []);

  // --- load -------------------------------------------------------------------------------

  useEffect(() => {
    let live = true;
    void (async () => {
      try {
        const [read, types] = await Promise.all([
          fetchGraph(workflowId),
          fetchNodeTypes({ include_deprecated: true }).catch(() => null),
        ]);
        if (!live) return;
        setRevision(read.revision);
        setBaseDocument(read.graph);
        setState({ ...emptyState(), document: read.graph });
        setLibraryState(types ? "ready" : "failed");
        if (types) setLibrary(types.nodes);
        setRecents(readRecents());
      } catch (error) {
        if (!live) return;
        setLoadError(
          error instanceof ApiError ? error.message : "The workflow graph could not be read.",
        );
      }
    })();
    return () => {
      live = false;
    };
  }, [workflowId]);

  // --- the palette, grouped and searched ----------------------------------------------------

  const definition = useCallback(
    (key: string): NodeType | undefined => library.find((entry) => entry.key === key),
    [library],
  );

  const grouped = useMemo(() => {
    const term = search.trim().toLowerCase();
    const matches = library.filter((entry) => {
      if (!term) return true;
      return (
        entry.label.toLowerCase().includes(term) ||
        entry.key.toLowerCase().includes(term) ||
        entry.description.toLowerCase().includes(term)
      );
    });
    return CATEGORY_ORDER.map((category) => ({
      category,
      label: CATEGORY_LABEL[category] ?? category,
      entries: matches.filter((entry) => entry.category === category),
    })).filter((group) => group.entries.length > 0);
  }, [library, search]);

  /** Why a palette entry cannot be placed, or `null` when it can. Named, never blank. */
  const unavailable = useCallback(
    (entry: NodeType): string | null => {
      if (entry.state === "node_package_missing") {
        return entry.state_reason ?? "the node package that provides it is not installed";
      }
      if (entry.deprecated) {
        return entry.superseded_by
          ? `superseded by ${entry.superseded_by}`
          : "deprecated";
      }
      return null;
    },
    [],
  );

  const portsFor = useCallback(
    (nodeKey: string) => {
      const node = state.document.nodes.find((each) => each.key === nodeKey);
      const entry = node ? definition(node.type) : undefined;
      return {
        inputs: entry?.inputs.map((port) => port.name) ?? ["in"],
        outputs: entry?.outputs.map((port) => port.name) ?? ["out"],
      };
    },
    [definition, state.document.nodes],
  );

  // --- saving -----------------------------------------------------------------------------

  /**
   * Write a document. Shared by the autosave, `⌘S` and the toolbar's save.
   *
   * Declared before `commit` because `commit` closes over it, and a `const` used before its
   * declaration is the temporal dead zone rather than a lint warning — the build says nothing
   * and the call is `undefined` at run time.
   */
  const persist = useCallback(
    async (document: GraphDocument) => {
      setSave({ kind: "saving" });
      try {
        const answer = await saveGraph(workflowId, document, revision);
        setRevision(answer.revision);
        setBaseDocument(document);
        setSave({ kind: "saved", at: new Date() });
        setConflict(null);
      } catch (error) {
        if (error instanceof ApiError && error.code === "graph_revision_conflict") {
          const details = (error.details ?? {}) as { current_revision?: number };
          setConflict({ loaded: revision, current: details.current_revision ?? revision });
          // The canvas state is deliberately kept: a person who has typed a condition must be
          // able to reload *and* copy their work out, and the REQ's answer to a conflict is
          // compare-and-reload, not "your work is gone".
          setSave({ kind: "dirty" });
          return;
        }
        setSave({
          kind: "failed",
          message:
            error instanceof ApiError ? error.message : "The graph could not be saved.",
        });
      }
    },
    [revision, workflowId],
  );

  /**
   * Record one operation and schedule the autosave.
   *
   * The state is threaded through a **ref** rather than closed over, and that is the whole
   * point: every caller currently writes `setState(commit({...state, document}, label))`, which
   * captures the render's `state`. An autosave scheduled from such a closure writes the
   * document as it was *when the callback was made*, so a person who makes three edits quickly
   * gets three saves racing and the last one to land wins — which is a lost update the REQ's
   * conflict rule exists to prevent. The ref is read at fire time, so the debounce always
   * writes the newest document.
   */
  const stateRef = useRef(state);
  stateRef.current = state;

  const commit = useCallback(
    (next: CanvasState, label: string): CanvasState => {
      setSave({ kind: "dirty" });
      if (autosave.current) window.clearTimeout(autosave.current);
      autosave.current = window.setTimeout(() => {
        void persist(stateRef.current.document);
      }, AUTOSAVE_MS);
      return apply(stateRef.current, label, next.document);
    },
    [persist],
  );

  // --- validation -------------------------------------------------------------------------

  const runValidation = useCallback(
    async (document: GraphDocument) => {
      if (validating.current) return;
      validating.current = true;
      try {
        const answer = await validateGraph(workflowId, document, revision);
        setIssues(answer.issues);
      } catch {
        // A validation that cannot be reached leaves the previous report on screen rather than
        // claiming the graph is fine. "No badge" would be a lie nobody can catch.
        setIssues([]);
      } finally {
        validating.current = false;
      }
    },
    [revision, workflowId],
  );

  // --- expression preview ------------------------------------------------------------------

  // The sample an expression is evaluated against. It is **pinned** rather than fetched, and
  // that is not a placeholder: a preview answered from live data would show a different number
  // on every keystroke, so the value beside a field would not be the value the step gets. The
  // server refuses a request with no namespaces rather than inventing some, so this map is the
  // whole contract — a person can edit it, and an expression naming a namespace that is not
  // here is refused with a sentence that lists the ones that are.
  const [sample, setSample] = useState<Record<string, unknown>>(DEFAULT_SAMPLE);
  const [preview, setPreview] = useState<{
    nodeKey: string | null;
    fields: Record<string, ExpressionPreview>;
    error: string | null;
    loading: boolean;
  }>({ nodeKey: null, fields: {}, error: null, loading: false });

  const previewing = useRef(false);

  const [sampleText, setSampleText] = useState(() => JSON.stringify(DEFAULT_SAMPLE, null, 2));
  const [sampleError, setSampleError] = useState<string | null>(null);

  const onSample = useCallback((text: string) => {
    setSampleText(text);
    try {
      const parsed: unknown = JSON.parse(text);
      if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
        setSampleError("The sample must be a JSON object keyed by namespace.");
        return;
      }
      setSampleError(null);
      setSample(parsed as Record<string, unknown>);
    } catch (error) {
      // The previous sample stays in force. Refusing to evaluate against nothing is the safe
      // direction, and the message says which of the two things is wrong.
      setSampleError(
        error instanceof Error ? `Not valid JSON: ${error.message}` : "Not valid JSON.",
      );
    }
  }, []);

  const runPreview = useCallback(
    async (nodeKey: string, params: Record<string, unknown>) => {
      // One in flight at a time, for the same reason validation has one: two answers arriving
      // out of order would put the *older* evaluation next to the text that caused the newer
      // one, which is a preview that lies rather than one that lags.
      if (previewing.current) return;
      previewing.current = true;
      setPreview((current) => ({ ...current, nodeKey, loading: true, error: null }));
      try {
        const answer = await previewExpressions(workflowId, params, sample);
        const fields: Record<string, ExpressionPreview> = {};
        for (const entry of answer.previews) fields[entry.field] = entry;
        setPreview({ nodeKey, fields, error: null, loading: false });
      } catch (error) {
        // The refusal is a sentence about a named field, and the inspector's whole job is to
        // show it on that row. Dropping it into a banner at the top of the canvas is how a
        // person ends up looking in the wrong place.
        const message =
          error instanceof ApiError ? error.message : "the preview could not be reached";
        setPreview({ nodeKey, fields: {}, error: message, loading: false });
      } finally {
        previewing.current = false;
      }
    },
    [sample, workflowId],
  );


  // --- operations -------------------------------------------------------------------------

  const add = useCallback(
    (entry: NodeType) => {
      const centre = canvasCentre();
      const result = addNode(state.document, entry.key, centre, snapOn);
      setState(commit({ ...state, document: result.document }, `add ${entry.label}`));
      setState((current) => ({ ...current, selected: [result.node.key], inspecting: result.node.key }));
      const next = [entry.key, ...recents.filter((key) => key !== entry.key)];
      setRecents(next);
      writeRecents(next);
      announce(`Added ${entry.label}`);
      void runValidation(result.document);
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [announce, recents, runValidation, snapOn, state],
  );

  /** The middle of the visible canvas, which is where a click-to-add lands. */
  function canvasCentre(): { x: number; y: number } {
    const current = state.viewport;
    return toCanvas({ x: frame.width / 2, y: frame.height / 2 }, current);
  }

  const removeSelected = useCallback(() => {
    if (state.selected.length === 0) return;
    const count = state.selected.length;
    const next = deleteNodes(state.document, state.selected);
    setState(
      apply(
        { ...state, document: next, selected: [], inspecting: null },
        `delete ${count} node${count === 1 ? "" : "s"}`,
        next,
      ),
    );
    announce(`Deleted ${count} node${count === 1 ? "" : "s"}`);
    void runValidation(next);
  }, [announce, runValidation, state]);

  const duplicateSelection = useCallback(() => {
    if (state.selected.length === 0) return;
    const clipboardCopy = copySelection(state.document, state.selected);
    const anchor = state.document.nodes.find((node) => node.key === state.selected[0]);
    if (!anchor) return;
    const pasted = pasteClipboard(
      state.document,
      clipboardCopy,
      { x: anchor.position.x + 40, y: anchor.position.y + 40 },
    );
    setState(
      apply(
        { ...state, document: pasted.document, selected: pasted.keys },
        `duplicate ${state.selected.length} node${state.selected.length === 1 ? "" : "s"}`,
        pasted.document,
      ),
    );
    announce(`Duplicated ${pasted.keys.length} node${pasted.keys.length === 1 ? "" : "s"}`);
    void runValidation(pasted.document);
  }, [announce, runValidation, state]);

  const doCopy = useCallback(() => {
    if (state.selected.length === 0) return;
    clipboard.current = copySelection(state.document, state.selected);
    announce(`Copied ${state.selected.length} node${state.selected.length === 1 ? "" : "s"}`);
  }, [announce, state]);

  const doPaste = useCallback(() => {
    if (!clipboard.current) return;
    const pasted = pasteClipboard(state.document, clipboard.current, canvasCentre());
    if (pasted.keys.length === 0) return;
    setState(
      apply(
        { ...state, document: pasted.document, selected: pasted.keys },
        `paste ${pasted.keys.length} node${pasted.keys.length === 1 ? "" : "s"}`,
        pasted.document,
      ),
    );
    announce(`Pasted ${pasted.keys.length} node${pasted.keys.length === 1 ? "" : "s"}`);
    void runValidation(pasted.document);
  }, // eslint-disable-next-line react-hooks/exhaustive-deps
  [announce, runValidation, state]);

  const fitAll = useCallback(() => {
    const box = boundsOf(state.document.nodes);
    if (!box) {
      setState({ ...state, viewport: { x: 0, y: 0, zoom: 1 } });
      return;
    }
    setState({ ...state, viewport: viewportFor(box, frame) });
  }, [frame, state]);

  const zoomToSelection = useCallback(() => {
    const chosen = state.document.nodes.filter((node) => state.selected.includes(node.key));
    const box = boundsOf(chosen.length > 0 ? chosen : state.document.nodes);
    if (!box) return;
    setState({ ...state, viewport: viewportFor(box, frame) });
  }, [frame, state]);

  const autoLayout = useCallback(() => {
    const next = applyLayout(state.document, layeredLayout(state.document));
    setState(apply(state, "auto-layout", next));
    // The fit after the layout is the point of the button: a layout a person cannot see all of
    // has not helped them.
    const box = boundsOf(next.nodes);
    if (box) setState((current) => ({ ...current, viewport: viewportFor(box, frame) }));
    announce("Arranged the graph by depth");
    void runValidation(next);
  }, [announce, frame, runValidation, state]);

  // --- keyboard ---------------------------------------------------------------------------

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const mod = event.metaKey || event.ctrlKey;
      const inField =
        event.target instanceof HTMLElement &&
        (event.target.tagName === "INPUT" ||
          event.target.tagName === "TEXTAREA" ||
          event.target.isContentEditable);

      if (mod && event.key.toLowerCase() === "k") {
        event.preventDefault();
        setPaletteOpen(true);
        return;
      }
      if (mod && event.key.toLowerCase() === "s") {
        event.preventDefault();
        if (autosave.current) window.clearTimeout(autosave.current);
        void persist(state.document);
        return;
      }
      if (mod && event.key.toLowerCase() === "z") {
        event.preventDefault();
        const next = event.shiftKey ? redo(state) : undo(state);
        setState(next);
        announce(`${event.shiftKey ? "Redo" : "Undo"}: ${redoLabel(state) ?? undoLabel(state) ?? "nothing"}`);
        return;
      }
      if (mod && event.key.toLowerCase() === "a" && !inField) {
        event.preventDefault();
        setState({ ...state, selected: selectAll(state.document) });
        return;
      }
      if (mod && event.key.toLowerCase() === "c" && !inField) {
        doCopy();
        return;
      }
      if (mod && event.key.toLowerCase() === "v" && !inField) {
        doPaste();
        return;
      }
      if (mod && event.key.toLowerCase() === "d" && !inField) {
        event.preventDefault();
        duplicateSelection();
        return;
      }
      if (mod && event.key === "0") {
        event.preventDefault();
        fitAll();
        return;
      }
      if (mod && event.shiftKey && event.key.toLowerCase() === "l") {
        event.preventDefault();
        autoLayout();
        return;
      }
      if (event.key === "/" && !inField) {
        event.preventDefault();
        setPaletteOpen(true);
        return;
      }
      if (event.key === "?" && !inField) {
        setShowShortcuts((open) => !open);
        return;
      }
      if (event.key === "F2" && !inField && state.selected.length === 1) {
        event.preventDefault();
        setRenaming(state.selected[0]);
        return;
      }
      if (event.key === "Escape") {
        setWire(null);
        setReconnecting(null);
        setRefusal(null);
        setMarquee(null);
        setSelectedEdge(null);
        setShowShortcuts(false);
        return;
      }
      if ((event.key === "Delete" || event.key === "Backspace") && !inField) {
        if (selectedEdge !== null) {
          const next = deleteConnections(state.document, [selectedEdge]);
          setState(apply({ ...state, document: next }, "delete a connection", next));
          setSelectedEdge(null);
          void runValidation(next);
          return;
        }
        event.preventDefault();
        removeSelected();
        return;
      }
      if (!inField && state.selected.length > 0 && event.key.startsWith("Arrow")) {
        event.preventDefault();
        const step = event.shiftKey ? state.nudge * 10 : state.nudge;
        const dx = event.key === "ArrowRight" ? step : event.key === "ArrowLeft" ? -step : 0;
        const dy = event.key === "ArrowDown" ? step : event.key === "ArrowUp" ? -step : 0;
        let next = state.document;
        for (const key of state.selected) next = nudgeNode(next, key, dx, dy);
        setState(apply(state, `nudge ${state.selected.length} node${state.selected.length === 1 ? "" : "s"}`, next));
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [
    autoLayout,
    doCopy,
    doPaste,
    duplicateSelection,
    fitAll,
    persist,
    removeSelected,
    runValidation,
    selectedEdge,
    state,
  ]);

  // --- pointer ----------------------------------------------------------------------------

  /** Pan, zoom, and the frame size `fit` needs. */
  useEffect(() => {
    const element = surfaceRef.current;
    if (!element) return;
    const observer = new ResizeObserver((entries) => {
      const entry = entries[0];
      if (entry) setFrame({ width: entry.contentRect.width, height: entry.contentRect.height });
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    const query = window.matchMedia(`(max-width: ${MOBILE_BREAKPOINT}px)`);
    const apply_ = () => setNarrow(query.matches);
    apply_();
    query.addEventListener("change", apply_);
    return () => query.removeEventListener("change", apply_);
  }, []);

  const onWheel = useCallback(
    (event: React.WheelEvent) => {
      // Ctrl+wheel is the pinch gesture on every trackpad; a plain wheel pans. Treating them
      // the same is what makes a trackpad feel broken.
      if (!event.ctrlKey && !event.metaKey) return;
      event.preventDefault();
      const factor = event.deltaY < 0 ? 1.1 : 1 / 1.1;
      setState((current) => {
        const zoom = clamp(current.viewport.zoom * factor, MIN_ZOOM, MAX_ZOOM);
        return { ...current, viewport: { ...current.viewport, zoom } };
      });
    },
    [],
  );

  const localPoint = useCallback(
    (event: { clientX: number; clientY: number }): { x: number; y: number } => {
      const rect = surfaceRef.current?.getBoundingClientRect();
      return { x: event.clientX - (rect?.left ?? 0), y: event.clientY - (rect?.top ?? 0) };
    },
    [],
  );

  const canvasPoint = useCallback(
    (event: { clientX: number; clientY: number }) => toCanvas(localPoint(event), state.viewport),
    [localPoint, state.viewport],
  );

  const beginPan = useCallback(
    (event: React.PointerEvent) => {
      if (event.button !== 0 && event.button !== 1) return;
      const start = localPoint(event);
      setPanning({ start, origin: state.viewport });
      (event.target as Element).setPointerCapture?.(event.pointerId);
    },
    [localPoint, state.viewport],
  );

  const onPointerMove = useCallback(
    (event: React.PointerEvent) => {
      if (panning) {
        const point = localPoint(event);
        setState((current) => ({
          ...current,
          viewport: {
            ...current.viewport,
            x: panning.origin.x + (point.x - panning.start.x),
            y: panning.origin.y + (point.y - panning.start.y),
          },
        }));
        return;
      }
      if (drag) {
        const point = canvasPoint(event);
        const dx = point.x - drag.start.x;
        const dy = point.y - drag.start.y;
        let next = state.document;
        for (const [key, origin] of Object.entries(drag.origin)) {
          next = moveNode(next, key, { x: origin.x + dx, y: origin.y + dy }, snapOn);
        }
        if (!drag.moved) setDrag({ ...drag, moved: true });
        setState((current) => ({ ...current, document: next }));
        return;
      }
      if (marquee) {
        setMarquee({ ...marquee, at: localPoint(event) });
        return;
      }
      if (wire) {
        setWire({ ...wire, at: canvasPoint(event) });
        return;
      }
      if (reconnecting) {
        setWire({
          from: reconnecting.end === "from" ? "" : "",
          from_port: "",
          at: canvasPoint(event),
        });
      }
    },
    [canvasPoint, drag, localPoint, marquee, panning, reconnecting, snapOn, state.document, wire],
  );

  const endPointer = useCallback(
    (event: React.PointerEvent) => {
      if (panning) {
        setPanning(null);
        return;
      }
      if (drag?.moved) {
        setState((current) =>
          apply(current, `move ${Object.keys(drag.origin).length} node${Object.keys(drag.origin).length === 1 ? "" : "s"}`, current.document),
        );
        void runValidation(state.document);
        setDrag(null);
        return;
      }
      if (drag) {
        setDrag(null);
        return;
      }
      if (marquee) {
        const box = {
          left: Math.min(marquee.start.x, marquee.at.x),
          top: Math.min(marquee.start.y, marquee.at.y),
          right: Math.max(marquee.start.x, marquee.at.x),
          bottom: Math.max(marquee.start.y, marquee.at.y),
        };
        const hit = state.document.nodes
          .filter((node) => {
            const x = node.position.x * state.viewport.zoom + state.viewport.x;
            const y = node.position.y * state.viewport.zoom + state.viewport.y;
            return x + NODE_WIDTH >= box.left && x <= box.right && y + NODE_HEIGHT >= box.top && y <= box.bottom;
          })
          .map((node) => node.key);
        setState((current) => ({
          ...current,
          selected: marquee.additive ? [...new Set([...current.selected, ...hit])] : hit,
        }));
        setMarquee(null);
        return;
      }
      if (reconnecting) {
        // Dropping on a node's input port finishes the reconnect; dropping anywhere else
        // cancels it, which is what a person means by letting go in blank space.
        const point = canvasPoint(event);
        const target = state.document.nodes.find((node) => {
          const x = node.position.x * state.viewport.zoom + state.viewport.x;
          const y = node.position.y * state.viewport.zoom + state.viewport.y;
          return point.x >= x && point.x <= x + NODE_WIDTH && point.y >= y && point.y <= y + NODE_HEIGHT;
        });
        if (target) {
          const wire2 = state.document.connections[reconnecting.index];
          const edge =
            reconnecting.end === "from"
              ? { from: target.key, from_port: "out", to: wire2.to, to_port: wire2.to_port }
              : { from: wire2.from, from_port: wire2.from_port, to: target.key, to_port: "in" };
          const refusal = refuseConnect(state.document, edge, portsFor);
          if (refusal) {
            setRefusal(refusal);
          } else {
            const next = {
              ...state.document,
              connections: state.document.connections.map((entry, index) =>
                index === reconnecting.index ? { ...entry, ...edge } : entry,
              ),
            };
            setState(apply(state, "reconnect", next));
            void runValidation(next);
          }
        }
        setReconnecting(null);
        setWire(null);
      }
    },
    [canvasPoint, drag, marquee, panning, portsFor, reconnecting, runValidation, state],
  );

  // --- rendering helpers ------------------------------------------------------------------

  const toScreen = useCallback(
    (point: { x: number; y: number }) => ({
      x: point.x * state.viewport.zoom + state.viewport.x,
      y: point.y * state.viewport.zoom + state.viewport.y,
    }),
    [state.viewport],
  );

  const issuesByNode = useMemo(() => {
    const map = new Map<string, GraphIssue[]>();
    for (const issue of issues) {
      if (!issue.node_key) continue;
      const list = map.get(issue.node_key) ?? [];
      list.push(issue);
      map.set(issue.node_key, list);
    }
    return map;
  }, [issues]);

  const issueByEdge = useMemo(() => {
    const map = new Map<number, GraphIssue[]>();
    for (const issue of issues) {
      if (issue.connection_index === undefined) continue;
      const list = map.get(issue.connection_index) ?? [];
      list.push(issue);
      map.set(issue.connection_index, list);
    }
    return map;
  }, [issues]);

  const dirty = useMemo(
    () => baseDocument !== null && !sameDocument(baseDocument, state.document),
    [baseDocument, state.document],
  );

  const inspecting = state.document.nodes.find((node) => node.key === state.inspecting) ?? null;
  const inspectingType = inspecting ? definition(inspecting.type) : undefined;

  // Re-run for the node being inspected whenever one of its parameters carries an expression.
  // The effect lives HERE rather than beside `runPreview` because `inspecting` is derived below
  // from `state`: an effect placed above it reads a binding that does not exist yet, which is
  // a temporal dead zone the type checker reports and the browser turns into a ReferenceError.
  useEffect(() => {
    if (!inspecting) return;
    const params = inspecting.params;
    const carriesExpression = Object.values(params).some(
      (value) => typeof value === "string" && value.includes("{{"),
    );
    if (!carriesExpression) {
      setPreview({ nodeKey: null, fields: {}, error: null, loading: false });
      return;
    }
    const timer = window.setTimeout(() => {
      void runPreview(inspecting.key, params);
    }, PREVIEW_DEBOUNCE_MS);
    return () => window.clearTimeout(timer);
  }, [inspecting, runPreview]);
  const readOnly = narrow;

  // --- render -----------------------------------------------------------------------------

  if (loadError) {
    return (
      <div className={`rounded-lg border p-6 ${PANEL.surface}`}>
        <h2 className="text-sm font-semibold">This graph could not be opened</h2>
        <p className={`mt-2 text-[13px] ${PANEL.muted}`}>{loadError}</p>
      </div>
    );
  }

  return (
    <div className="flex h-[calc(100vh-8.5rem)] flex-col gap-2">
      {/* the read-only banner the REQ names for a narrow screen */}
      {readOnly ? (
        <p className="rounded-md border border-line bg-card px-3 py-2 text-[12px] text-muted">
          This canvas is read-only at this width. Open it on a wider screen to edit — you can
          still pan, zoom, fit and inspect a node here.
        </p>
      ) : null}

      {conflict ? (
        <div
          role="alert"
          className="flex flex-wrap items-center gap-3 rounded-md border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-[12px]"
        >
          <span>
            This canvas was loaded at revision {conflict.loaded}; the workflow is now at{" "}
            {conflict.current}. Your unsaved work is still here.
          </span>
          <button
            type="button"
            className="rounded border border-line px-2 py-1"
            onClick={() => {
              setConflict(null);
              setState((current) => ({ ...current, document: baseDocument ?? current.document }));
              announce("Reloaded the stored graph — your unsaved changes are in the list above only if you copied them out");
            }}
          >
            Reload the stored graph
          </button>
        </div>
      ) : null}

      <div className="flex min-h-0 flex-1 gap-2">
        {/* ------------------------------------------------------------ palette */}
        <aside
          className={`flex w-64 shrink-0 flex-col rounded-lg border ${PANEL.surface}`}
          aria-label="Node palette"
        >
          <div className="border-b border-line p-2">
            <button
              type="button"
              onClick={() => setPaletteOpen((open) => !open)}
              aria-expanded={paletteOpen}
              className="flex w-full items-center justify-between text-[13px] font-medium"
            >
              <span>Node palette</span>
              <span className={`text-[11px] ${PANEL.muted}`}>{paletteOpen ? "Hide" : "Show"}</span>
            </button>
            {paletteOpen ? (
              <input
                value={search}
                onChange={(event) => setSearch(event.target.value)}
                placeholder="Search nodes…"
                aria-label="Search nodes"
                className="mt-2 w-full rounded border border-line bg-background px-2 py-1 text-[12px]"
              />
            ) : null}
          </div>

          <div className="min-h-0 flex-1 overflow-y-auto p-2">
            {!paletteOpen ? (
              <p className={`px-1 text-[12px] ${PANEL.muted}`}>
                Open the palette to add a node, or press <kbd className="rounded border border-line px-1">/</kbd>.
              </p>
            ) : libraryState === "loading" ? (
              <p className={`px-1 text-[12px] ${PANEL.muted}`}>Loading the registry…</p>
            ) : libraryState === "failed" ? (
              <p className="px-1 text-[12px] text-amber-600">
                The node library could not be read. Existing nodes still work; new ones cannot be
                added until it is.
              </p>
            ) : recents.length > 0 && !search ? (
              <section className="mb-3">
                <h3 className={`px-1 text-[11px] uppercase ${PANEL.muted}`}>Recently added</h3>
                <ul className="mt-1 space-y-1">
                  {recents
                    .map((key) => library.find((entry) => entry.key === key))
                    .filter((entry): entry is NodeType => Boolean(entry))
                    .map((entry) => (
                      <li key={`recent-${entry.key}`}>
                        <PaletteRow
                          entry={entry}
                          reason={readOnly ? "this canvas is read-only" : unavailable(entry)}
                          onAdd={() => add(entry)}
                        />
                      </li>
                    ))}
                </ul>
              </section>
            ) : null}

            {paletteOpen
              ? grouped.map((group) => (
                  <section key={group.category} className="mb-3">
                    <h3 className={`px-1 text-[11px] uppercase ${PANEL.muted}`}>
                      {group.label}
                      <span className="ml-1">{group.entries.length}</span>
                    </h3>
                    <ul className="mt-1 space-y-1">
                      {group.entries.map((entry) => (
                        <li key={entry.key}>
                          <PaletteRow
                            entry={entry}
                            reason={readOnly ? "this canvas is read-only" : unavailable(entry)}
                            onAdd={() => add(entry)}
                          />
                        </li>
                      ))}
                    </ul>
                  </section>
                ))
              : null}
            {paletteOpen && grouped.length === 0 && libraryState === "ready" ? (
              <p className={`px-1 text-[12px] ${PANEL.muted}`}>
                Nothing matches “{search}”. Node keys are searchable too.
              </p>
            ) : null}
          </div>
        </aside>

        {/* ------------------------------------------------------------ canvas */}
        <div className={`flex min-w-0 flex-1 flex-col rounded-lg border ${PANEL.surface}`}>
          <div className="flex flex-wrap items-center gap-1 border-b border-line p-1.5">
            <ToolbarButton
              label="Undo"
              hint={undoLabel(state) ? `Undo: ${undoLabel(state)}` : "Nothing to undo"}
              disabled={state.past.length === 0}
              onClick={() => {
                setState(undo(state));
                announce(`Undo: ${undoLabel(state) ?? "nothing"}`);
              }}
            >
              Undo
            </ToolbarButton>
            <ToolbarButton
              label="Redo"
              hint={redoLabel(state) ? `Redo: ${redoLabel(state)}` : "Nothing to redo"}
              disabled={state.future.length === 0}
              onClick={() => {
                setState(redo(state));
                announce(`Redo: ${redoLabel(state) ?? "nothing"}`);
              }}
            >
              Redo
            </ToolbarButton>
            <span className="mx-1 h-4 w-px bg-line" aria-hidden />
            <ToolbarButton label="Auto-layout" hint="⌘⇧L" onClick={autoLayout} disabled={readOnly}>
              Auto-layout
            </ToolbarButton>
            <ToolbarButton label="Fit" hint="⌘0" onClick={fitAll}>
              Fit
            </ToolbarButton>
            <ToolbarButton
              label="Zoom to selection"
              hint="Frame the selected nodes"
              onClick={zoomToSelection}
              disabled={state.document.nodes.length === 0}
            >
              Zoom to selection
            </ToolbarButton>
            <ToolbarButton
              label="Grid snap"
              hint={snapOn ? `Snapping to ${SNAP}px` : "Positions are free"}
              onClick={() => {
                setSnapOn((on) => !on);
                setState((current) => ({ ...current, snap: !current.snap }));
              }}
            >
              {snapOn ? "Snap on" : "Snap off"}
            </ToolbarButton>
            <ToolbarButton
              label="Minimap"
              hint={minimap ? "Hide the minimap" : "Show the minimap"}
              onClick={() => setMinimap((on) => !on)}
            >
              {minimap ? "Minimap on" : "Minimap off"}
            </ToolbarButton>
            <span className="mx-1 h-4 w-px bg-line" aria-hidden />
            <ToolbarButton
              label="Sticky note"
              hint="A comment that never runs"
              disabled={readOnly}
              onClick={() => {
                const next = addNote(state.document, canvasCentre());
                setState(apply(state, "add a sticky note", next));
              }}
            >
              Sticky note
            </ToolbarButton>
            <ToolbarButton label="Shortcuts" hint="Press ? for this sheet" onClick={() => setShowShortcuts((open) => !open)}>
              ?
            </ToolbarButton>
            <span className="flex-1" />
            <span className={`text-[11px] ${PANEL.muted}`}>
              {Math.round(state.viewport.zoom * 100)}%
            </span>
            <ToolbarButton label="Save" hint="⌘S" disabled={!dirty && save.kind !== "failed"} onClick={() => void persist(state.document)}>
              Save
            </ToolbarButton>
          </div>

          <div
            ref={surfaceRef}
            role="application"
            aria-label="Workflow canvas"
            className="relative min-h-0 flex-1 overflow-hidden bg-background"
            style={{
              cursor: panning ? "grabbing" : "default",
              backgroundImage: state.viewport.zoom > 0.4
                ? `radial-gradient(circle, var(--line) 1px, transparent 1px)`
                : undefined,
              backgroundSize: `${SNAP * state.viewport.zoom}px ${SNAP * state.viewport.zoom}px`,
              backgroundPosition: `${state.viewport.x}px ${state.viewport.y}px`,
            }}
            onWheel={onWheel}
            onPointerDown={beginPan}
            onPointerMove={onPointerMove}
            onPointerUp={endPointer}
            onPointerCancel={endPointer}
            onClick={(event) => {
              if (event.target === event.currentTarget) {
                setState((current) => ({ ...current, selected: [], inspecting: null }));
                setSelectedEdge(null);
                setMarquee({
                  start: localPoint(event),
                  at: localPoint(event),
                  additive: event.shiftKey,
                });
              }
            }}
          >
            {/* wires */}
            <svg className="absolute inset-0 h-full w-full" aria-hidden>
              {state.document.connections.map((connection, index) => {
                const from = state.document.nodes.find((node) => node.key === connection.from);
                const to = state.document.nodes.find((node) => node.key === connection.to);
                if (!from || !to) return null;
                const a = toScreen({
                  x: from.position.x + NODE_WIDTH,
                  y: from.position.y + NODE_HEIGHT / 2,
                });
                const b = toScreen({ x: to.position.x, y: to.position.y + NODE_HEIGHT / 2 });
                const mid = (a.x + b.x) / 2;
                const path = `M ${a.x} ${a.y} C ${mid} ${a.y}, ${mid} ${b.y}, ${b.x} ${b.y}`;
                const bad = (issueByEdge.get(index)?.length ?? 0) > 0;
                return (
                  <g key={`${connection.from}-${connection.from_port}-${connection.to}-${connection.to_port}`}>
                    <path
                      d={path}
                      fill="none"
                      stroke={bad ? "#dc2626" : selectedEdge === index ? "var(--accent)" : "var(--line-strong)"}
                      strokeWidth={selectedEdge === index ? 2.5 : 1.5}
                      className="cursor-pointer"
                      onClick={(event) => {
                        event.stopPropagation();
                        setSelectedEdge(index);
                        setState((current) => ({ ...current, selected: [] }));
                      }}
                    />
                    <path
                      d={path}
                      fill="none"
                      stroke="transparent"
                      strokeWidth={14}
                      className="cursor-pointer"
                      onClick={(event) => {
                        event.stopPropagation();
                        setSelectedEdge(index);
                      }}
                    />
                    {connection.label ? (
                      <text
                        x={mid}
                        y={(a.y + b.y) / 2 - 6}
                        textAnchor="middle"
                        className="pointer-events-none fill-current text-[10px]"
                      >
                        {connection.label}
                      </text>
                    ) : null}
                  </g>
                );
              })}
              {wire ? (
                <line
                  x1={wire.at.x}
                  y1={wire.at.y}
                  x2={wire.at.x}
                  y2={wire.at.y}
                  stroke="var(--accent)"
                  strokeDasharray="4 3"
                />
              ) : null}
            </svg>

            {/* the wire being dragged, drawn in canvas space so it tracks the pointer */}
            {wire && reconnecting === null ? (
              <svg className="pointer-events-none absolute inset-0 h-full w-full" aria-hidden>
                {state.document.nodes
                  .filter((node) => node.key === wire.from)
                  .map((node) => {
                    const a = toScreen({
                      x: node.position.x + NODE_WIDTH,
                      y: node.position.y + NODE_HEIGHT / 2,
                    });
                    const b = toScreen(wire.at);
                    return (
                      <line
                        key={wire.from}
                        x1={a.x}
                        y1={a.y}
                        x2={b.x}
                        y2={b.y}
                        stroke="var(--accent)"
                        strokeWidth={2}
                        strokeDasharray="4 3"
                      />
                    );
                  })}
              </svg>
            ) : null}

            {marquee ? (
              <div
                className="pointer-events-none absolute border border-accent bg-accent/10"
                style={{
                  left: Math.min(marquee.start.x, marquee.at.x),
                  top: Math.min(marquee.start.y, marquee.at.y),
                  width: Math.abs(marquee.at.x - marquee.start.x),
                  height: Math.abs(marquee.at.y - marquee.start.y),
                }}
              />
            ) : null}

            {/* nodes */}
            {state.document.nodes.map((node) => {
              const position = toScreen(node.position);
              const entry = definition(node.type);
              const nodeIssues = issuesByNode.get(node.key) ?? [];
              const selected = state.selected.includes(node.key);
              return (
                <div
                  key={node.key}
                  data-node-key={node.key}
                  data-node-type={node.type}
                  data-selected={selected ? "true" : "false"}
                  data-has-issues={nodeIssues.length > 0 ? "true" : "false"}
                  tabIndex={0}
                  role="button"
                  aria-pressed={selected}
                  aria-label={`${node.label || node.type} (${node.type})`}
                  onClick={(event) => {
                    event.stopPropagation();
                    if (readOnly) {
                      setState((current) => ({ ...current, inspecting: node.key }));
                      return;
                    }
                    setState((current) => ({
                      ...current,
                      selected: event.shiftKey
                        ? toggleSelection(current.selected, node.key)
                        : selectOnly(current.selected, node.key),
                      inspecting: node.key,
                    }));
                  }}
                  onKeyDown={(event) => {
                    if (event.key === "Enter") {
                      event.preventDefault();
                      setState((current) => ({ ...current, inspecting: node.key }));
                    }
                  }}
                  onPointerDown={(event) => {
                    if (readOnly) return;
                    event.stopPropagation();
                    const keys = event.shiftKey
                      ? toggleSelection(state.selected, node.key)
                      : selectOnly(state.selected, node.key);
                    setState((current) => ({ ...current, selected: keys, inspecting: node.key }));
                    const origin: Record<string, { x: number; y: number }> = {};
                    for (const key of keys) {
                      const found = state.document.nodes.find((each) => each.key === key);
                      if (found) origin[key] = found.position;
                    }
                    setDrag({
                      kind: "node",
                      start: canvasPoint(event),
                      origin,
                      moved: false,
                    });
                  }}
                  className={`absolute cursor-grab rounded-lg border bg-card px-3 py-2 shadow-sm transition-shadow focus:outline-none focus-visible:ring-2 focus-visible:ring-accent ${
                    selected ? "border-accent ring-2 ring-accent/30" : "border-line"
                  } ${node.disabled ? "opacity-60" : ""} ${nodeIssues.length > 0 ? "border-red-500" : ""}`}
                  style={{
                    left: position.x,
                    top: position.y,
                    width: NODE_WIDTH,
                    minHeight: NODE_HEIGHT,
                  }}
                >
                  <div className="flex items-start justify-between gap-2">
                    <div className="min-w-0">
                      {renaming === node.key ? (
                        <input
                          autoFocus
                          defaultValue={node.label}
                          aria-label="Node name"
                          className="w-full rounded border border-line bg-background px-1 text-[12px] font-medium"
                          onBlur={(event) => {
                            const label = event.target.value.trim();
                            const next = {
                              ...state.document,
                              nodes: state.document.nodes.map((each) =>
                                each.key === node.key ? { ...each, label: label || each.type } : each,
                              ),
                            };
                            setState(apply(state, `rename to ${label || node.type}`, next));
                            setRenaming(null);
                          }}
                          onKeyDown={(event) => {
                            if (event.key === "Enter") event.currentTarget.blur();
                            if (event.key === "Escape") setRenaming(null);
                          }}
                        />
                      ) : (
                        <p className="truncate text-[12px] font-medium">
                          {node.label || entry?.label || node.type}
                        </p>
                      )}
                      <p className="truncate text-[10px] text-muted">{node.type}</p>
                    </div>
                    <div className="flex shrink-0 gap-1">
                      {node.disabled ? (
                        <span className="rounded bg-muted/20 px-1 text-[9px] uppercase">off</span>
                      ) : null}
                      {nodeIssues.length > 0 ? (
                        <span
                          title={nodeIssues.map((issue) => issue.message).join("\n")}
                          className="rounded bg-red-500/15 px-1 text-[9px] uppercase text-red-600"
                        >
                          {nodeIssues.length}
                        </span>
                      ) : null}
                    </div>
                  </div>

                  {!readOnly ? (
                    <div className="mt-1.5 flex items-center justify-between">
                      <div className="flex gap-1">
                        {(entry?.inputs ?? []).map((port) => (
                          <button
                            key={`in-${port.name}`}
                            type="button"
                            aria-label={`Connect into ${port.name}`}
                            title={`Input: ${port.name}`}
                            className="h-2.5 w-2.5 rounded-full border border-line-strong bg-background"
                            onPointerDown={(event) => {
                              event.stopPropagation();
                              if (reconnecting) setReconnecting({ index: reconnecting.index, end: "to" });
                            }}
                            onClick={(event) => {
                              event.stopPropagation();
                              announce(`Drop a wire on the left edge of ${node.label || node.type} to connect into ${port.name}`);
                            }}
                          />
                        ))}
                      </div>
                      <div className="flex gap-1">
                        {(entry?.outputs ?? []).map((port) => (
                          <button
                            key={`out-${port.name}`}
                            type="button"
                            aria-label={`Drag a wire from ${port.name}`}
                            title={`Output: ${port.name}`}
                            className="h-2.5 w-2.5 cursor-crosshair rounded-full border border-line-strong bg-accent"
                            onPointerDown={(event) => {
                              event.stopPropagation();
                              if (readOnly) return;
                              const point = canvasPoint(event);
                              setWire({ from: node.key, from_port: port.name, at: point });
                            }}
                            onClick={(event) => {
                              event.stopPropagation();
                              // Click-to-connect: the REQ's keyboard path needs a way to make a
                              // wire without a drag, and this is it.
                              const target = state.document.nodes.find(
                                (candidate) =>
                                  candidate.key !== node.key &&
                                  !hasConnection(state.document, {
                                    from: node.key,
                                    from_port: port.name,
                                    to: candidate.key,
                                    to_port: "in",
                                  }),
                              );
                              if (!target) {
                                announce("No free input port to connect to");
                                return;
                              }
                              const edge = { from: node.key, from_port: port.name, to: target.key, to_port: "in" };
                              const refusal = refuseConnect(state.document, edge, portsFor);
                              if (refusal) {
                                setRefusal(refusal);
                                return;
                              }
                              const next = connect(state.document, edge);
                              setState(apply(state, `connect ${node.label} → ${target.label}`, next));
                              void runValidation(next);
                            }}
                          />
                        ))}
                      </div>
                    </div>
                  ) : null}
                </div>
              );
            })}

            {/* sticky notes */}
            {state.document.notes.map((note) => {
              const position = toScreen(note.position);
              return (
                <div
                  key={note.id}
                  className="absolute rounded-md border border-line bg-amber-500/10 p-2"
                  style={{ left: position.x, top: position.y, width: note.width, minHeight: note.height }}
                >
                  <textarea
                    value={note.text}
                    readOnly={readOnly}
                    aria-label={`Sticky note ${note.id}`}
                    placeholder="A note for whoever works on this next…"
                    className="h-full w-full resize-none bg-transparent text-[11px] outline-none"
                    onChange={(event) => {
                      const next = {
                        ...state.document,
                        notes: state.document.notes.map((each) =>
                          each.id === note.id ? { ...each, text: event.target.value } : each,
                        ),
                      };
                      setState(commit({ ...state, document: next }, `edit note ${note.id}`));
                    }}
                  />
                </div>
              );
            })}

            {/* empty state — the REQ's "start from a template" / "start from a trigger" */}
            {state.document.nodes.length === 0 && state.document.notes.length === 0 ? (
              <div className="absolute inset-0 flex items-center justify-center">
                <div className="max-w-sm text-center">
                  <h3 className="text-sm font-semibold">This workflow has no nodes yet</h3>
                  <p className="mt-1 text-[12px] text-muted">
                    Start with a trigger from the palette, or open the node library to see what
                    each one needs. A workflow with no trigger cannot run.
                  </p>
                  <button
                    type="button"
                    className="mt-3 rounded border border-line px-3 py-1.5 text-[12px]"
                    onClick={() => setPaletteOpen(true)}
                  >
                    Open the palette
                  </button>
                </div>
              </div>
            ) : null}

            {/* minimap — hidden below 20 nodes, as the REQ says */}
            {minimap && state.document.nodes.length >= 20 ? (
              <div className="absolute bottom-2 right-2 h-28 w-40 rounded border border-line bg-card/90 p-1">
                {state.document.nodes.map((node) => {
                  const box = boundsOf(state.document.nodes, 0);
                  if (!box) return null;
                  const scale = Math.min(150 / box.width, 100 / box.height);
                  const left = (node.position.x - box.x) * scale;
                  const top = (node.position.y - box.y) * scale;
                  return (
                    <button
                      key={`mini-${node.key}`}
                      type="button"
                      aria-label={`Centre on ${node.label || node.type}`}
                      className="absolute h-1.5 w-3 rounded-sm bg-accent/70"
                      style={{ left: 4 + left, top: 4 + top }}
                      onClick={() => {
                        const centre = boundsOf([node], 0);
                        if (centre) setState((current) => ({ ...current, viewport: viewportFor(centre, frame) }));
                      }}
                    />
                  );
                })}
              </div>
            ) : null}

            {/* the refusal toast — every refusal is named, never silent */}
            {refusal ? (
              <div
                role="alert"
                className="absolute left-1/2 top-3 -translate-x-1/2 rounded-md border border-red-500/50 bg-red-500/10 px-3 py-1.5 text-[12px] text-red-700"
              >
                {refusal.message}
                <button type="button" className="ml-2 underline" onClick={() => setRefusal(null)}>
                  Dismiss
                </button>
              </div>
            ) : null}

            {toast ? (
              <div
                role="status"
                aria-live="polite"
                className="absolute bottom-2 left-1/2 -translate-x-1/2 rounded-md border border-line bg-card px-3 py-1.5 text-[12px] shadow"
              >
                {toast}
              </div>
            ) : null}

            {showShortcuts ? (
              <div className="absolute right-2 top-2 w-64 rounded-md border border-line bg-card p-3 shadow-lg">
                <h3 className="text-[12px] font-semibold">Keyboard</h3>
                <dl className="mt-2 space-y-1">
                  {SHORTCUTS.map((binding) => (
                    <div key={binding.keys} className="flex justify-between gap-2 text-[11px]">
                      <dt className="text-muted">{binding.label}</dt>
                      <dd>
                        <kbd className="rounded border border-line px-1">{binding.keys}</kbd>
                      </dd>
                    </div>
                  ))}
                </dl>
              </div>
            ) : null}
          </div>

          {/* ---------------------------------------------------------- the strip */}
          <div className="flex flex-wrap items-center gap-x-4 gap-y-1 border-t border-line px-3 py-1.5 text-[11px]">
            <span>
              {state.document.nodes.length} node{state.document.nodes.length === 1 ? "" : "s"}
            </span>
            <span>
              {state.document.connections.length} connection
              {state.document.connections.length === 1 ? "" : "s"}
            </span>
            <span>
              {state.document.notes.length} note{state.document.notes.length === 1 ? "" : "s"}
            </span>
            <span data-testid="graph-validation-count">
              {issues.length === 0 ? "No validation issues" : `${issues.length} validation issue${issues.length === 1 ? "" : "s"}`}
            </span>
            <span className="flex-1" />
            <SaveBadge save={save} dirty={dirty} revision={revision} />
          </div>
        </div>

        {/* ------------------------------------------------------------ inspector */}
        <aside
          className={`flex w-72 shrink-0 flex-col rounded-lg border ${PANEL.surface}`}
          aria-label="Node inspector"
        >
          <div className="border-b border-line p-2 text-[13px] font-medium">Inspector</div>
          <div className="min-h-0 flex-1 overflow-y-auto p-2">
            {inspecting ? (
              <NodeInspector
                nodeKey={inspecting.key}
                label={inspecting.label || inspecting.type}
                params={inspecting.params}
                disabled={inspecting.disabled}
                type={inspectingType}
                issues={issuesByNode.get(inspecting.key) ?? []}
                readOnly={readOnly}
                preview={preview.nodeKey === inspecting.key ? preview : null}
                sampleText={sampleText}
                sampleError={sampleError}
                onSample={onSample}
                onParam={(name, value) => {
                  const next = {
                    ...state.document,
                    nodes: state.document.nodes.map((each) =>
                      each.key === inspecting.key
                        ? { ...each, params: { ...each.params, [name]: value } }
                        : each,
                    ),
                  };
                  setState(commit({ ...state, document: next }, `set ${name}`));
                  void runValidation(next);
                }}
                onToggleDisabled={() => {
                  const next = {
                    ...state.document,
                    nodes: state.document.nodes.map((each) =>
                      each.key === inspecting.key ? { ...each, disabled: !each.disabled } : each,
                    ),
                  };
                  setState(commit({ ...state, document: next }, inspecting.disabled ? "enable a node" : "disable a node"));
                  void runValidation(next);
                }}
                onRename={(label) => {
                  const next = {
                    ...state.document,
                    nodes: state.document.nodes.map((each) =>
                      each.key === inspecting.key ? { ...each, label } : each,
                    ),
                  };
                  setState(commit({ ...state, document: next }, `rename to ${label}`));
                }}
              />
            ) : (
              <div>
                <p className={`text-[12px] ${PANEL.muted}`}>
                  Select a node to edit its parameters, or a connection to give it a branch label.
                </p>
                {selectedEdge !== null && state.document.connections[selectedEdge] ? (
                  <div className="mt-3 rounded-md border border-line p-2">
                    <h3 className="text-[12px] font-medium">Branch label</h3>
                    <p className="mt-0.5 text-[11px] text-muted">
                      {state.document.connections[selectedEdge].from} →{" "}
                      {state.document.connections[selectedEdge].to}
                    </p>
                    <input
                      aria-label="Branch label"
                      placeholder="true / false / case A / error"
                      readOnly={readOnly}
                      className="mt-2 w-full rounded border border-line bg-background px-2 py-1 text-[12px]"
                      value={state.document.connections[selectedEdge].label ?? ""}
                      onChange={(event) => {
                        const next = labelConnection(state.document, selectedEdge, event.target.value);
                        setState(commit({ ...state, document: next }, "label a branch"));
                      }}
                    />
                    {state.document.connections[selectedEdge].label ? (
                      <p className="mt-1 text-[11px] text-muted">
                        The run overlay shows this label on the branch.
                      </p>
                    ) : null}
                    {readOnly ? null : (
                      <button
                        type="button"
                        className="mt-2 rounded border border-line px-2 py-1 text-[11px]"
                        onClick={() => {
                          const next = deleteConnections(state.document, [selectedEdge]);
                          setState(apply(state, "delete a connection", next));
                          setSelectedEdge(null);
                          void runValidation(next);
                        }}
                      >
                        Delete this connection
                      </button>
                    )}
                  </div>
                ) : null}

                {issues.length > 0 ? (
                  <div className="mt-3">
                    <h3 className="text-[12px] font-medium">
                      {issues.length} issue{issues.length === 1 ? "" : "s"}
                    </h3>
                    <ul className="mt-1 space-y-1">
                      {issues.map((issue, index) => (
                        <li key={`${issue.code}-${index}`}>
                          <button
                            type="button"
                            className="w-full rounded border border-line px-2 py-1 text-left text-[11px]"
                            onClick={() => {
                              if (issue.node_key) {
                                setState((current) => ({
                                  ...current,
                                  selected: [issue.node_key as string],
                                  inspecting: issue.node_key as string,
                                }));
                              }
                            }}
                          >
                            <span className="font-mono text-[10px] text-red-600">{issue.code}</span>
                            <span className="mt-0.5 block">{issue.message}</span>
                          </button>
                        </li>
                      ))}
                    </ul>
                  </div>
                ) : null}
              </div>
            )}
          </div>
        </aside>
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------------------------
// Pieces
// ---------------------------------------------------------------------------------------------

/** One palette row: a real button, disabled only with a reason. */
function PaletteRow({
  entry,
  reason,
  onAdd,
}: {
  entry: NodeType;
  reason: string | null;
  onAdd: () => void;
}) {
  return (
    <button
      type="button"
      disabled={reason !== null}
      title={reason ?? `Add ${entry.label}`}
      onClick={onAdd}
      data-node-key={entry.key}
      data-unavailable={reason ? "true" : "false"}
      className="w-full rounded border border-line px-2 py-1.5 text-left text-[12px] disabled:cursor-not-allowed disabled:opacity-60"
    >
      <span className="block truncate font-medium">{entry.label}</span>
      <span className="block truncate text-[10px] text-muted">
        {reason ?? entry.description}
      </span>
    </button>
  );
}

/** A toolbar button whose tooltip carries its shortcut, so the two cannot disagree. */
function ToolbarButton({
  children,
  label,
  hint,
  disabled,
  onClick,
}: {
  children: React.ReactNode;
  label: string;
  hint: string;
  disabled?: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      aria-label={label}
      title={hint}
      disabled={disabled}
      onClick={onClick}
      className="rounded border border-line px-2 py-1 text-[11px] disabled:cursor-not-allowed disabled:opacity-50"
    >
      {children}
    </button>
  );
}

/** The strip's save state, including the REQ's "last saved" and the failure's retry. */
function SaveBadge({
  save,
  dirty,
  revision,
}: {
  save: SaveState;
  dirty: boolean;
  revision: number;
}) {
  if (save.kind === "failed") {
    return (
      <span role="alert" className="flex items-center gap-2 text-red-600">
        Not saved — {save.message}
        <span className="text-muted">Your work is still on the canvas; press ⌘S to retry.</span>
      </span>
    );
  }
  if (save.kind === "saving") return <span className="text-muted">Saving…</span>;
  if (dirty || save.kind === "dirty") {
    return (
      <span className="text-amber-600">
        Unsaved — autosaving in a moment, or press ⌘S
      </span>
    );
  }
  if (save.kind === "saved") {
    return (
      <span className="text-muted">
        Saved {save.at.toLocaleTimeString()} · revision {revision}
      </span>
    );
  }
  return <span className="text-muted">Revision {revision} · no changes</span>;
}

/** One node's parameter form, generated from the registry's own schema. */
function NodeInspector({
  nodeKey,
  label,
  params,
  disabled,
  type,
  issues,
  readOnly,
  preview,
  sampleText,
  sampleError,
  onSample,
  onParam,
  onToggleDisabled,
  onRename,
}: {
  nodeKey: string;
  label: string;
  params: Record<string, unknown>;
  disabled: boolean;
  type: NodeType | undefined;
  issues: GraphIssue[];
  readOnly: boolean;
  /** The node's evaluated fields, or null when the node carries no expression. */
  preview: {
    fields: Record<string, ExpressionPreview>;
    error: string | null;
    loading: boolean;
  } | null;
  sampleText: string;
  sampleError: string | null;
  onSample: (text: string) => void;
  onParam: (name: string, value: unknown) => void;
  onToggleDisabled: () => void;
  onRename: (label: string) => void;
}) {
  const [name, setName] = useState(label);
  useEffect(() => setName(label), [label, nodeKey]);

  const schema: NodeParam[] = type?.params ?? [];

  return (
    <div>
      <label className="block text-[11px] text-muted" htmlFor={`label-${nodeKey}`}>
        Name
      </label>
      <input
        id={`label-${nodeKey}`}
        value={name}
        readOnly={readOnly}
        onChange={(event) => setName(event.target.value)}
        onBlur={() => name.trim() && name !== label && onRename(name.trim())}
        className="mt-1 w-full rounded border border-line bg-background px-2 py-1 text-[12px]"
      />

      <div className="mt-3 flex items-center justify-between">
        <span className="text-[11px] text-muted">{type?.label ?? label}</span>
        <button
          type="button"
          disabled={readOnly}
          onClick={onToggleDisabled}
          aria-pressed={disabled}
          className="rounded border border-line px-2 py-0.5 text-[11px] disabled:cursor-not-allowed disabled:opacity-50"
        >
          {disabled ? "Enable" : "Disable"}
        </button>
      </div>
      {disabled ? (
        <p className="mt-1 text-[11px] text-muted">
          A disabled node compiles to no step at all — it is not skipped at run time, it is not
          part of the workflow.
        </p>
      ) : null}

      {issues.length > 0 ? (
        <ul className="mt-3 space-y-1">
          {issues.map((issue, index) => (
            <li key={`${issue.code}-${index}`} className="rounded border border-red-500/40 px-2 py-1 text-[11px] text-red-600">
              <span className="font-mono text-[10px]">{issue.code}</span>
              <span className="mt-0.5 block">{issue.message}</span>
            </li>
          ))}
        </ul>
      ) : null}

      <h3 className="mt-4 text-[12px] font-medium">Parameters</h3>
      {schema.length === 0 ? (
        <p className="mt-1 text-[11px] text-muted">
          {type ? "This node takes no parameters." : "The registry has no schema for this node type."}
        </p>
      ) : (
        <div className="mt-1 space-y-2">
          {schema.map((field) => (
            <ParamField
              key={field.name}
              field={field}
              value={params[field.name]}
              readOnly={readOnly}
              preview={preview?.fields[field.name] ?? null}
              previewLoading={preview?.loading ?? false}
              onChange={(value) => onParam(field.name, value)}
            />
          ))}
        </div>
      )}

      {preview?.error ? (
        <p className="mt-2 rounded border border-amber-500/40 bg-amber-500/10 p-1.5 text-[11px] text-amber-700 dark:text-amber-300">
          {preview.error}
        </p>
      ) : null}

      <h3 className="mt-4 text-[12px] font-medium">Preview sample</h3>
      <p className="mt-0.5 text-[10px] text-muted">
        Expressions are evaluated against this, on the server. The server never fetches it for
        you, so what you see here is exactly what a preview can read.
      </p>
      <textarea
        aria-label="Preview sample"
        readOnly={readOnly}
        value={sampleText}
        onChange={(event) => onSample(event.target.value)}
        spellCheck={false}
        className="mt-1 h-40 w-full rounded border border-line bg-background px-2 py-1 font-mono text-[10px]"
      />
      {sampleError ? (
        <p className="mt-0.5 text-[10px] text-red-500">{sampleError}</p>
      ) : (
        <p className="mt-0.5 text-[10px] text-muted">
          {namespaceCount(sampleText)} namespace
          {namespaceCount(sampleText) === 1 ? "" : "s"}:{" "}
          {namespaceList(sampleText).join(", ") || "none"}
        </p>
      )}
    </div>
  );
}

/**
 * What a previewed value IS, shown beside it when it is not a string.
 *
 * The point is the difference between `2` and `"2"`: a field that will send a number and a
 * field that will send its text look identical in a preview that renders everything as text,
 * and the whole reason the server keeps a lone expression's type is so the person editing
 * can be told which one they have.
 */
function typeName(value: unknown): string {
  if (value === null) return "null";
  if (Array.isArray(value)) return "list";
  switch (typeof value) {
    case "number":
      return "number";
    case "boolean":
      return "true/false";
    case "object":
      return "object";
    default:
      return "text";
  }
}

/**
 * The namespaces a sample edit declares, read from the TEXT rather than from the parsed
 * object. The parsed object only exists when the text is valid, and this line is exactly the
 * one that is rendered when it is not — so reading it from the object would be reading the
 * last good value and describing it as the current one.
 */
function namespaceList(text: string): string[] {
  try {
    const parsed: unknown = JSON.parse(text);
    if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) return [];
    return Object.keys(parsed as Record<string, unknown>);
  } catch {
    return [];
  }
}

function namespaceCount(text: string): number {
  return namespaceList(text).length;
}

/** Whether a parameter value carries an expression at all. */
function carriesExpression(value: unknown): boolean {
  return typeof value === "string" && value.includes("{{");
}

/** One parameter row, rendered from the registry's declared `ui`. */
function ParamField({
  field,
  value,
  readOnly,
  preview,
  onChange,
}: {
  field: NodeParam;
  value: unknown;
  readOnly: boolean;
  preview: ExpressionPreview | null;
  previewLoading: boolean;
  onChange: (value: unknown) => void;
}) {
  const id = `param-${field.name}`;
  // Loading is shown only for a field that is actually carrying an expression, so a node with
  // no expressions at all does not show a spinner next to every one of its parameters.
  const previewLoading = !preview && carriesExpression(value);
  const current = value === undefined || value === null ? "" : String(value);
  const missing = field.required && current.trim() === "";

  return (
    <div>
      <label htmlFor={id} className="block text-[11px] text-muted">
        {field.label}
        {field.required ? <span className="ml-0.5 text-red-500">*</span> : null}
      </label>

      {field.ui === "select" ? (
        <select
          id={id}
          value={current}
          disabled={readOnly}
          onChange={(event) => onChange(event.target.value)}
          className="mt-1 w-full rounded border border-line bg-background px-2 py-1 text-[12px]"
        >
          <option value="">Choose…</option>
          {field.options.map((option) => (
            <option key={option} value={option}>
              {option}
            </option>
          ))}
        </select>
      ) : field.ui === "boolean" ? (
        <label className="mt-1 flex items-center gap-2 text-[12px]">
          <input
            id={id}
            type="checkbox"
            checked={value === true}
            disabled={readOnly}
            onChange={(event) => onChange(event.target.checked)}
          />
          {field.label}
        </label>
      ) : field.ui === "textarea" || field.ui === "code" ? (
        <textarea
          id={id}
          value={current}
          readOnly={readOnly}
          placeholder={field.placeholder ?? ""}
          onChange={(event) => onChange(event.target.value)}
          className="mt-1 w-full rounded border border-line bg-background px-2 py-1 font-mono text-[11px]"
          rows={field.ui === "code" ? 6 : 3}
        />
      ) : (
        <input
          id={id}
          value={current}
          readOnly={readOnly}
          placeholder={field.placeholder ?? ""}
          onChange={(event) =>
            onChange(field.ui === "number" ? Number(event.target.value) : event.target.value)
          }
          className="mt-1 w-full rounded border border-line bg-background px-2 py-1 text-[12px]"
        />
      )}

      {missing && !readOnly ? (
        <p className="mt-0.5 text-[10px] text-amber-600">
          {field.help ?? "This field is required before the workflow can run."}
        </p>
      ) : field.help ? (
        <p className="mt-0.5 text-[10px] text-muted">{field.help}</p>
      ) : null}
      {field.secret_field ? (
        <p className="mt-0.5 text-[10px] text-muted">
          This field names a credential, never its secret.
        </p>
      ) : null}

      {previewLoading ? (
        <p className="mt-1 text-[10px] text-muted">evaluating…</p>
      ) : preview ? (
        <p
          data-preview-for={field.name}
          className="mt-1 rounded border border-line bg-muted/40 px-1.5 py-1 font-mono text-[10px] break-all"
        >
          <span className="text-muted">→ </span>
          {preview.rendered}
          {preview.typed && typeof preview.value !== "string" ? (
            <span className="ml-1 text-muted">({typeName(preview.value)})</span>
          ) : null}
        </p>
      ) : null}
    </div>
  );
}
