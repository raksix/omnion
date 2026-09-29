/**
 * Table mode — the same definition as a list, editable and fully usable without a pointer.
 *
 * Criterion (REQ-004, criterion 8): *"Table mode renders the same definition, edits parameters,
 * and stays consistent with the canvas after a save in either mode."*
 *
 * ## The one thing this component must not be
 *
 * A second editor over a *copy*. The toolbar used to link "Table mode" at `/automations/{id}`,
 * which is REQ-003's linear step editor — a different view of a *different* projection, so
 * "consistent with the canvas after a save in either mode" was not a property anything could
 * satisfy. This view reads the same `graph` jsonb and commits through the same
 * `saveWorkflowGraph` call, quoting the same version, so a save here advances exactly the
 * version the canvas would and the next tab sees the same conflict the canvas would produce.
 *
 * `table-mode.ts` holds the rules; this file is the rendering, the empty/error states, and the
 * one place a draft is turned back into a graph.
 */

"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import Link from "next/link";
import { AlertTriangle, ArrowLeft, Check, Loader2, Table2, Workflow as WorkflowIcon } from "lucide-react";

import {
  ApiError,
  fetchWorkflowGraph,
  saveWorkflowGraph,
  type GraphEdge,
  type GraphFinding,
  type GraphNode,
  type WorkflowGraph,
} from "@/lib/api";
import {
  buildTable,
  diffTableEdits,
  paramEntries,
  setLabel,
  setParam,
  toGraph,
  type TableDraft,
} from "./table-mode";

type LoadState =
  | { kind: "loading" }
  | { kind: "error"; message: string }
  | { kind: "ready" };

type SaveState =
  | { kind: "idle" }
  | { kind: "saving" }
  | { kind: "saved" }
  | { kind: "conflict"; message: string }
  | { kind: "error"; message: string };

export function WorkflowTableView({ workflowId }: { workflowId: string }) {
  const [load, setLoad] = useState<LoadState>({ kind: "loading" });
  const [graph, setGraph] = useState<WorkflowGraph | null>(null);
  const [draft, setDraft] = useState<TableDraft | null>(null);
  const [save, setSave] = useState<SaveState>({ kind: "idle" });
  const [findings, setFindings] = useState<GraphFinding[]>([]);

  // The write owns the version column until it answers, so a second press joins it rather than
  // quoting a version the first one is about to replace — the same rule as the canvas's
  // `arbitrateSave`, and for the same reason.
  const inFlight = useRef(false);
  const versionRef = useRef<number>(0);
  const draftRef = useRef<TableDraft | null>(null);
  draftRef.current = draft;

  const loadGraph = useCallback(async () => {
    setLoad({ kind: "loading" });
    try {
      const read = await fetchWorkflowGraph(workflowId);
      setGraph(read);
      versionRef.current = read.graph_version;
      setDraft(buildTable(read.graph.nodes, read.graph.edges));
      setFindings([]);
      setSave({ kind: "idle" });
      setLoad({ kind: "ready" });
    } catch (error) {
      setLoad({
        kind: "error",
        message: error instanceof ApiError ? error.message : "The definition could not be read.",
      });
    }
  }, [workflowId]);

  useEffect(() => {
    void loadGraph();
  }, [loadGraph]);

  /**
   * Commit the draft. The guard is `diffTableEdits`, not "the button is enabled": a save of an
   * unedited draft advances `graph_version` and hands the next tab a conflict no author made.
   */
  const commit = useCallback(async () => {
    const current = draftRef.current;
    if (!current || !diffTableEdits(current)) return;
    if (inFlight.current) return;
    inFlight.current = true;
    setSave({ kind: "saving" });
    try {
      const nodes: GraphNode[] = toGraph(current, graph?.graph.nodes ?? []);
      const saved = await saveWorkflowGraph(workflowId, {
        graph: { nodes, edges: current.edges },
        graph_version: versionRef.current,
      });
      setGraph(saved);
      versionRef.current = saved.graph_version;
      // Re-seed the draft from the SERVER's answer, not from what we sent: the store
      // normalises, and a draft left holding the sent copy would keep reporting a dirty field
      // for an edit the server already accepted.
      setDraft(buildTable(saved.graph.nodes, saved.graph.edges));
      setFindings([]);
      setSave({ kind: "saved" });
    } catch (error) {
      if (error instanceof ApiError && error.code === "graph_version_conflict") {
        setSave({ kind: "conflict", message: error.message });
      } else {
        if (error instanceof ApiError && error.details) {
          const details = error.details as { findings?: GraphFinding[] };
          if (Array.isArray(details.findings) && details.findings.length > 0) {
            setFindings(details.findings);
          }
        }
        setSave({
          kind: "error",
          message: error instanceof ApiError ? error.message : "The definition could not be saved.",
        });
      }
    } finally {
      inFlight.current = false;
    }
  }, [workflowId, graph]);

  const dirty = draft ? diffTableEdits(draft) : false;

  // ---- states -------------------------------------------------------------------------

  if (load.kind === "loading") {
    return (
      <Frame
        toolbar={
          <Toolbar
            workflowId={workflowId}
            save={save}
            dirty={false}
            onCommit={commit}
            disabled
          />
        }
      >
        <div
          className="flex flex-col items-center justify-center gap-2 p-16 text-[13px] text-muted"
          data-table-loading
        >
          <Loader2 className="h-4 w-4 animate-spin" aria-hidden="true" />
          Loading the definition…
        </div>
      </Frame>
    );
  }

  if (load.kind === "error") {
    return (
      <Frame toolbar={<Toolbar workflowId={workflowId} save={save} dirty={false} onCommit={commit} disabled />}>
        <div
          className="flex flex-col items-center justify-center gap-3 p-16 text-center"
          data-table-error
        >
          <AlertTriangle className="h-5 w-5 text-danger" aria-hidden="true" />
          <p className="max-w-md text-[13px] text-muted">{load.message}</p>
          <button
            type="button"
            onClick={() => void loadGraph()}
            className="rounded-md border border-line px-3 py-1.5 text-[12.5px]"
            data-table-retry
          >
            Try again
          </button>
        </div>
      </Frame>
    );
  }

  const rows = draft?.rows ?? [];

  return (
    <Frame
      toolbar={
        <Toolbar workflowId={workflowId} save={save} dirty={dirty} onCommit={commit} disabled={false} />
      }
    >
      {save.kind === "conflict" ? (
        <p
          className="border-b border-line bg-warn-soft px-3 py-1.5 text-[12px]"
          data-table-conflict
        >
          {save.message} Reload to see the other tab&apos;s copy, or save again to overwrite it.
        </p>
      ) : null}
      {save.kind === "error" ? (
        <p className="border-b border-line bg-danger-soft px-3 py-1.5 text-[12px]" data-table-save-error>
          {save.message}
        </p>
      ) : null}
      {findings.length > 0 ? (
        <ul className="border-b border-line px-3 py-2 text-[12px]" data-table-findings>
          {findings.map((finding, index) => (
            <li key={`${finding.code}-${index}`} className="text-danger">
              {finding.message}
            </li>
          ))}
        </ul>
      ) : null}

      {rows.length === 0 ? (
        <div
          className="flex flex-col items-center justify-center gap-2 p-16 text-center"
          data-table-empty
        >
          <WorkflowIcon className="h-5 w-5 text-muted" aria-hidden="true" />
          <p className="text-[13px] text-muted">
            This workflow has no nodes yet. Add one on the canvas and it appears here.
          </p>
          <Link
            href={`/workflows/${workflowId}/builder`}
            className="rounded-md border border-line px-3 py-1.5 text-[12.5px]"
            data-table-empty-builder
          >
            Open the builder
          </Link>
        </div>
      ) : (
        <table className="w-full border-collapse text-left text-[12.5px]" data-table-rows>
          <thead>
            <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
              <th scope="col" className="px-3 py-2 font-medium">Label</th>
              <th scope="col" className="px-3 py-2 font-medium">Type</th>
              <th scope="col" className="px-3 py-2 font-medium">Parameters</th>
              <th scope="col" className="px-3 py-2 font-medium">Connections</th>
              <th scope="col" className="px-3 py-2 font-medium">Position</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((row) => (
              <tr key={row.id} className="border-b border-line align-top" data-table-row={row.id}>
                <td className="px-3 py-2">
                  <input
                    aria-label={`Label of ${row.id}`}
                    value={row.label}
                    onChange={(event) => setDraft((d) => (d ? setLabel(d, row.id, event.target.value) : d))}
                    className="w-40 rounded-md border border-line bg-canvas px-2 py-1 text-[12.5px]"
                    data-table-label={row.id}
                  />
                </td>
                <td className="px-3 py-2 text-muted" data-table-type={row.id}>
                  {row.typeLabel}
                </td>
                <td className="px-3 py-2">
                  {row.paramCount === 0 ? (
                    <span className="text-muted" data-table-no-params={row.id}>
                      No parameters
                    </span>
                  ) : (
                    <ul className="flex flex-col gap-1">
                      {paramEntries(row).map(([key, value]) => (
                        <li key={key} className="flex items-center gap-2">
                          <span className="w-28 shrink-0 text-[11.5px] text-muted">{key}</span>
                          <input
                            aria-label={`${key} of ${row.id}`}
                            defaultValue={value}
                            onBlur={(event) =>
                              setDraft((d) => (d ? setParam(d, row.id, key, event.target.value) : d))
                            }
                            className="w-56 rounded-md border border-line bg-canvas px-2 py-1 text-[12.5px]"
                            data-table-param={`${row.id}.${key}`}
                          />
                        </li>
                      ))}
                    </ul>
                  )}
                </td>
                <td className="px-3 py-2 text-muted">
                  {row.incoming.length === 0 && row.outgoing.length === 0 ? (
                    <span data-table-no-connections={row.id}>Not connected</span>
                  ) : (
                    <ul className="flex flex-col gap-0.5">
                      {row.incoming.map((line, index) => (
                        <li key={`in-${index}`} data-table-incoming={row.id}>
                          ← {line}
                        </li>
                      ))}
                      {row.outgoing.map((line, index) => (
                        <li key={`out-${index}`} data-table-outgoing={row.id}>
                          → {line}
                        </li>
                      ))}
                    </ul>
                  )}
                </td>
                <td className="px-3 py-2 text-[11.5px] text-muted" data-table-position={row.id}>
                  {Math.round(row.position.x)}, {Math.round(row.position.y)}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Frame>
  );
}

function Frame({ toolbar, children }: { toolbar: React.ReactNode; children: React.ReactNode }) {
  return (
    <div className="flex min-h-0 flex-1 flex-col" data-table-mode>
      {toolbar}
      <div className="min-h-0 flex-1 overflow-y-auto">{children}</div>
    </div>
  );
}

function Toolbar({
  workflowId,
  save,
  dirty,
  onCommit,
  disabled,
}: {
  workflowId: string;
  save: SaveState;
  dirty: boolean;
  onCommit: () => void;
  disabled: boolean;
}) {
  return (
    <header className="flex items-center justify-between gap-3 border-b border-line px-3 py-2">
      <div className="flex items-center gap-2">
        <Link
          href={`/workflows/${workflowId}/builder`}
          className="inline-flex items-center gap-1.5 rounded-md border border-line px-2 py-1.5 text-[12.5px] hover:bg-quiet-soft"
          data-table-back
        >
          <ArrowLeft className="h-3.5 w-3.5" aria-hidden="true" />
          Back to canvas
        </Link>
        <span className="inline-flex items-center gap-1.5 text-[12.5px] text-muted">
          <Table2 className="h-3.5 w-3.5" aria-hidden="true" />
          Table mode
        </span>
      </div>
      <div className="flex items-center gap-2">
        <span className="text-[12px] text-muted" data-table-save-state>
          {save.kind === "saving" ? "Saving…" : save.kind === "saved" ? "Saved" : dirty ? "Unsaved changes" : "Saved"}
        </span>
        <button
          type="button"
          onClick={onCommit}
          // Disabled for an unedited draft on purpose: a write here advances graph_version and
          // hands the next tab a conflict that no author created.
          disabled={disabled || !dirty || save.kind === "saving"}
          className="rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-50"
          data-table-save
          title={dirty ? "Save the parameters you changed" : "No changes to save"}
        >
          {save.kind === "saving" ? (
            <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden="true" />
          ) : (
            <Check className="h-3.5 w-3.5" aria-hidden="true" />
          )}
          Save
        </button>
      </div>
    </header>
  );
}
