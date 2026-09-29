"use client";

/**
 * The Workspace tab (docs/requests/REQ-099, slice 2).
 *
 * A workspace is the scratch area a run reads from and writes to: the inputs somebody uploaded
 * for it, and the outputs it decided to keep. Three decisions in here are not obvious, and each
 * closes a way the tab can mislead:
 *
 * 1. **The path is an editable field, not the file's name.** The API refuses a request that
 *    omits the path precisely so a name the browser invented cannot reach a namespace the
 *    server validates. The tab therefore asks for the path, pre-filled from the filename and
 *    *editable* — a workspace holds `data/2026/q3.csv` as readily as `notes.md`, and the
 *    filename has no idea which of those the user meant.
 *
 * 2. **The usage bar says what is stored *and* what it costs.** "97 of 100 MB · 6 files" tells
 *    somebody which half of the pair to act on; a bare percentage tells them only that
 *    something is wrong. The bar's number is the API's `percent` — recomputing it here would let
 *    the bar disagree with the quota that produced the refusal.
 *
 * 3. **A drop zone is a real target, not a decoration.** Dragging a file in fills the same
 *    picker the button opens, because a drop zone that only looks droppable is a control that
 *    does nothing when somebody tries it.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { AlertTriangle, Download, FileUp, Loader2, Trash2, Upload } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  type AiAgentFile,
  type AiAgentWorkspace,
  aiAgentFileHref,
  deleteAiAgentFile,
  fetchAiAgentWorkspace,
  uploadAiAgentFile,
} from "@/lib/api";
import { formatBytes, formatTimestamp } from "@/lib/format";

/** The path rules, quoted so the tab can check a path before the server does. */
const PATH_MAX = 512;

/** Why a path the tab typed would be refused, or `null` when it is fine. */
export function checkWorkspacePath(path: string): string | null {
  const trimmed = path.trim();
  if (!trimmed) {
    return "A workspace needs a path — where the run will look for the file.";
  }
  if (trimmed.length > PATH_MAX) {
    return `The path is ${trimmed.length} characters; the limit is ${PATH_MAX}.`;
  }
  // A control character survives into a log line and breaks a terminal, so the tab refuses it
  // before the round trip. Written with escapes rather than literal control characters in the
  // source: a literal one is invisible in a diff and gets eaten by a copy.
  if (/[\u0000-\u001f\u007f]/.test(trimmed)) {
    return "The path contains a control character.";
  }
  if (trimmed.startsWith("/") || trimmed.startsWith("\\") || /^[a-zA-Z]:/.test(trimmed)) {
    return "The path is absolute — a workspace path is relative to the agent.";
  }
  if (trimmed.split(/[/\\]/).some((segment) => segment === "..")) {
    return "The path walks out of the workspace with `..`.";
  }
  if (trimmed.endsWith("/") || trimmed.endsWith("\\")) {
    return "The path ends with a separator.";
  }
  return null;
}

/** A readable file type from a content type, for the Type column. */
function typeLabel(contentType: string, path: string): string {
  if (contentType && contentType !== "application/octet-stream") {
    return contentType.split("/")[1]?.toUpperCase() || contentType;
  }
  // The extension is the better label for an octet-stream, which is what a browser sends for
  // anything it does not recognise.
  const dot = path.lastIndexOf(".");
  return dot > 0 ? path.slice(dot + 1).toUpperCase() : "File";
}

type WorkspaceTabProps = {
  /** The agent whose workspace this is. */
  agentId: string;
  /** The organization selector, for a platform-level account. */
  organizationId?: string | null;
  /** Let the parent say "the agent was saved", so the tab refetches the name-dependent state. */
  refreshToken?: number;
};

export function AgentWorkspace({ agentId, organizationId, refreshToken }: WorkspaceTabProps) {
  const [workspace, setWorkspace] = useState<AiAgentWorkspace | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [uploading, setUploading] = useState(false);
  const [dragging, setDragging] = useState(false);
  /** The path the user is about to upload to, so the confirmation is not a surprise. */
  const [pendingPath, setPendingPath] = useState<string | null>(null);
  const [pendingFile, setPendingFile] = useState<File | null>(null);
  const [pathError, setPathError] = useState<string | null>(null);
  const [busyPath, setBusyPath] = useState<string | null>(null);
  const fileInput = useRef<HTMLInputElement>(null);
  /** The drop zone's own counter, so dragenter/dragleave do not flicker on child elements. */
  const dragDepth = useRef(0);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setWorkspace(await fetchAiAgentWorkspace(agentId, organizationId));
    } catch (err) {
      setError(err instanceof ApiError ? err.message : String(err));
    } finally {
      setLoading(false);
    }
  }, [agentId, organizationId]);

  useEffect(() => {
    void load();
  }, [load, refreshToken]);

  const usage = workspace?.usage;

  // The 80% mark is where a bar stops being information and starts being a warning, so the
  // colour changes there rather than at 100% — by which point the next upload is already
  // being refused.
  const usageTone = useMemo(() => {
    if (!usage) {
      return "bg-accent";
    }
    if (usage.percent >= 80) {
      return "bg-amber-500";
    }
    return "bg-accent";
  }, [usage]);

  /** Ask for a path, then upload. The path is confirmed rather than assumed. */
  const offerFile = useCallback((file: File) => {
    if (file.size === 0) {
      setPathError("That file is empty — a workspace file is an input a run can read.");
      return;
    }
    setPendingFile(file);
    setPendingPath(file.name);
    setPathError(null);
  }, []);

  const commitUpload = useCallback(async () => {
    if (!pendingFile || !pendingPath) {
      return;
    }
    const problem = checkWorkspacePath(pendingPath);
    if (problem) {
      setPathError(problem);
      return;
    }
    setUploading(true);
    setPathError(null);
    try {
      await uploadAiAgentFile({ agentId, path: pendingPath, file: pendingFile, organizationId });
      setPendingFile(null);
      setPendingPath(null);
      // The listing and the quota come back together, so a successful upload cannot leave a
      // bar that disagrees with the table under it.
      await load();
    } catch (err) {
      setPathError(err instanceof ApiError ? err.message : String(err));
    } finally {
      setUploading(false);
    }
  }, [agentId, organizationId, load, pendingFile, pendingPath]);

  const remove = useCallback(
    async (file: AiAgentFile) => {
      setBusyPath(file.path);
      setError(null);
      try {
        await deleteAiAgentFile({ agentId, path: file.path, organizationId });
        await load();
      } catch (err) {
        setError(err instanceof ApiError ? err.message : String(err));
      } finally {
        setBusyPath(null);
      }
    },
    [agentId, organizationId, load],
  );

  const onDrop = useCallback(
    (event: React.DragEvent) => {
      event.preventDefault();
      dragDepth.current = 0;
      setDragging(false);
      const file = event.dataTransfer.files?.[0];
      if (file) {
        offerFile(file);
      }
    },
    [offerFile],
  );

  if (loading) {
    return <LoadingTable columns={5} />;
  }

  return (
    <div className="flex flex-col gap-4">
      {error ? (
        <div
          role="alert"
          className="flex flex-wrap items-center gap-2 rounded-xl border border-red-200 bg-red-50 px-3 py-2 text-[12.5px] text-red-800"
        >
          <AlertTriangle className="size-4 shrink-0" aria-hidden />
          <span className="min-w-0 flex-1">{error}</span>
          <button
            type="button"
            onClick={() => void load()}
            className="rounded-lg border border-red-300 px-2 py-1 text-[12px]"
          >
            Retry
          </button>
        </div>
      ) : null}

      {/* The quota. The label carries both halves of the pair on purpose: which files and how
          many are the two answers, and "97%" alone is neither. */}
      {usage ? (
        <div className="rounded-xl border border-line p-3">
          <div className="flex flex-wrap items-baseline justify-between gap-2 text-[12.5px]">
            <span className="font-medium">Workspace</span>
            <span className="text-muted">
              {formatBytes(usage.used_bytes)} of {formatBytes(usage.limit_bytes)} ·{" "}
              {usage.file_count} file{usage.file_count === 1 ? "" : "s"}
            </span>
          </div>
          <div
            className="mt-2 h-1.5 w-full overflow-hidden rounded-full bg-line"
            role="progressbar"
            aria-valuenow={usage.percent}
            aria-valuemin={0}
            aria-valuemax={100}
            aria-label="Workspace usage"
          >
            <div className={`h-full rounded-full ${usageTone}`} style={{ width: `${usage.percent}%` }} />
          </div>
          <p className="mt-1.5 text-[12px] text-muted">
            One file may be up to {formatBytes(usage.max_file_bytes)}. A path you upload again
            replaces what was there.
          </p>
        </div>
      ) : null}

      {/* The drop zone. It is a real input: the button, the keyboard path and the drop all end
          in the same pending-file state. */}
      <div
        onDragEnter={(event) => {
          event.preventDefault();
          dragDepth.current += 1;
          setDragging(true);
        }}
        onDragOver={(event) => event.preventDefault()}
        onDragLeave={() => {
          dragDepth.current = Math.max(0, dragDepth.current - 1);
          if (dragDepth.current === 0) {
            setDragging(false);
          }
        }}
        onDrop={onDrop}
        className={`rounded-xl border border-dashed p-6 text-center transition ${
          dragging ? "border-accent bg-accent/5" : "border-line"
        }`}
      >
        <FileUp className="mx-auto size-5 text-muted" aria-hidden />
        <p className="mt-2 text-[13px] font-medium">Add a workspace file</p>
        <p className="mx-auto mt-1 max-w-sm text-[12.5px] text-muted">
          Drop a file here, or pick one. A run is told to read it by its path — an input, not a
          place for a build&apos;s output.
        </p>
        <input
          ref={fileInput}
          type="file"
          className="sr-only"
          onChange={(event) => {
            const file = event.target.files?.[0];
            if (file) {
              offerFile(file);
            }
            // Reset so picking the same file twice fires `change` again — without this, a retry
            // after a failure silently does nothing, which reads as a broken button.
            event.target.value = "";
          }}
        />
        <button
          type="button"
          onClick={() => fileInput.current?.click()}
          className="mt-3 inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium"
        >
          <Upload className="size-3.5" aria-hidden />
          Choose a file
        </button>
      </div>

      {/* The path confirmation. Shown only after a file is chosen, because a path typed before
          the file exists is a path for nothing. */}
      {pendingFile ? (
        <div className="rounded-xl border border-line p-3">
          <label className="block text-[12px] font-medium" htmlFor="workspace-path">
            Path in the workspace
          </label>
          <p className="mt-1 text-[12px] text-muted">
            {pendingFile.name} · {formatBytes(pendingFile.size)}
          </p>
          <input
            id="workspace-path"
            value={pendingPath ?? ""}
            onChange={(event) => {
              setPendingPath(event.target.value);
              setPathError(null);
            }}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                void commitUpload();
              }
              if (event.key === "Escape") {
                setPendingFile(null);
                setPendingPath(null);
                setPathError(null);
              }
            }}
            placeholder="data/2026/q3.csv"
            aria-invalid={pathError ? true : undefined}
            aria-describedby={pathError ? "workspace-path-error" : undefined}
            className="mt-2 w-full rounded-lg border border-line bg-background px-2.5 py-1.5 text-[13px] outline-none focus:border-accent"
          />
          {pathError ? (
            <p id="workspace-path-error" className="mt-1 text-[12px] text-red-700">
              {pathError}
            </p>
          ) : null}
          <div className="mt-2 flex gap-2">
            <button
              type="button"
              onClick={() => void commitUpload()}
              disabled={uploading}
              className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white disabled:opacity-60"
            >
              {uploading ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : null}
              Upload
            </button>
            <button
              type="button"
              onClick={() => {
                setPendingFile(null);
                setPendingPath(null);
                setPathError(null);
              }}
              className="rounded-lg border border-line px-3 py-1.5 text-[12.5px]"
            >
              Cancel
            </button>
          </div>
        </div>
      ) : null}

      {workspace && workspace.files.length === 0 ? (
        <EmptyState
          title="No workspace file yet"
          hint="A workspace is where a run's inputs live and where it keeps what it produced. Add a file above and a run can be told to read it by its path."
        />
      ) : workspace ? (
        <div className="overflow-x-auto">
          <table className="w-full border-collapse text-left text-[13px]">
            <thead>
              <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                <th className="px-2 py-2" scope="col">
                  Path
                </th>
                <th className="px-2 py-2" scope="col">
                  Size
                </th>
                <th className="px-2 py-2" scope="col">
                  Type
                </th>
                <th className="px-2 py-2" scope="col">
                  Added
                </th>
                <th className="px-2 py-2" scope="col">
                  Last used
                </th>
                <th className="px-2 py-2 text-right" scope="col">
                  Actions
                </th>
              </tr>
            </thead>
            <tbody>
              {workspace.files.map((file) => (
                <tr key={file.id} className="border-b border-line/60 last:border-0">
                  <td className="px-2 py-2 font-medium">
                    <span className="block break-all">{file.path}</span>
                    <span
                      className="block text-[11.5px] text-muted"
                      title={file.checksum}
                    >
                      {file.checksum.slice(0, 12)}
                    </span>
                  </td>
                  <td className="whitespace-nowrap px-2 py-2">{formatBytes(file.size_bytes)}</td>
                  <td className="whitespace-nowrap px-2 py-2 text-muted">
                    {typeLabel(file.content_type, file.path)}
                  </td>
                  <td className="whitespace-nowrap px-2 py-2 text-muted">
                    {formatTimestamp(file.created_at)}
                  </td>
                  <td className="whitespace-nowrap px-2 py-2 text-muted">
                    {file.last_used_at ? formatTimestamp(file.last_used_at) : "—"}
                  </td>
                  <td className="px-2 py-2">
                    <div className="flex items-center justify-end gap-1">
                      <a
                        href={aiAgentFileHref({ agentId, path: file.path, organizationId })}
                        className="inline-flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[12px]"
                      >
                        <Download className="size-3.5" aria-hidden />
                        Download
                      </a>
                      <button
                        type="button"
                        onClick={() => void remove(file)}
                        disabled={busyPath === file.path}
                        className="inline-flex items-center gap-1 rounded-lg border border-line px-2 py-1 text-[12px] text-red-700 disabled:opacity-60"
                      >
                        {busyPath === file.path ? (
                          <Loader2 className="size-3.5 animate-spin" aria-hidden />
                        ) : (
                          <Trash2 className="size-3.5" aria-hidden />
                        )}
                        Delete
                      </button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : null}
    </div>
  );
}
