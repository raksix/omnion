"use client";

/**
 * `/automation/projects/{id}/audit` — the project-scoped trail (REQ-133, slice 4).
 *
 * The screen is a stream with a filter and an export, and its whole reason for existing is that
 * the REQ asks for "the project audit screen" as a distinct thing from the instance audit. That
 * distinction is enforced in the store (`for_project` filters on `project_id`) rather than here,
 * because a client-side filter over an instance trail would show a project somebody else's
 * history — and the filter would be the leak.
 *
 * Three rules the screen keeps:
 *
 * 1. **The filter list is the project's own vocabulary**, delivered by the API from the rows the
 *    project actually holds. A hard-coded list would offer "role.changed" for a project where no
 *    role ever changed, and the reader would conclude a missing event was filtered out.
 * 2. **The action filter is sent to the API.** Filtering in the browser would report "nothing
 *    happened" for the rows the page never fetched — a truncated page and an empty project look
 *    identical otherwise.
 * 3. **The empty state distinguishes the two causes.** No rows at all, and no rows for this
 *    filter, are different sentences and only one of them is fixed by clearing the filter.
 */
import { useCallback, useEffect, useState } from "react";
import Link from "next/link";
import { useParams } from "next/navigation";
import { ArrowLeft, Download, Loader2, TriangleAlert } from "lucide-react";

import { ApiError, fetchProjectAudit } from "@/lib/api";
import type { ProjectAudit, ProjectAuditEntry } from "@/lib/types";

/** One cell of the export. Quoted the way a spreadsheet expects, or the row shifts. */
function csvCell(value: unknown): string {
  if (value === null || value === undefined) return "";
  const text =
    typeof value === "string" ? value : typeof value === "object" ? JSON.stringify(value) : String(value);
  return /[",\n]/.test(text) ? `"${text.replace(/"/g, '""')}"` : text;
}

/** The metadata rendered as `key=value` pairs; never a raw JSON dump in a table cell. */
function metadataSummary(metadata: Record<string, unknown>): string {
  const parts = Object.entries(metadata ?? {}).map(([key, value]) => {
    const rendered = typeof value === "string" ? value : JSON.stringify(value);
    return `${key}=${rendered ?? "null"}`;
  });
  return parts.join("  ");
}

export function ProjectAuditScreen() {
  const params = useParams<{ id: string }>();
  const projectId = params?.id;

  const [body, setBody] = useState<ProjectAudit | null>(null);
  const [action, setAction] = useState("");
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(
    async (nextAction: string) => {
      if (!projectId) return;
      setLoading(true);
      setError(null);
      try {
        setBody(await fetchProjectAudit(projectId, nextAction));
      } catch (cause) {
        if (cause instanceof ApiError && cause.status === 404) {
          setError("no such project");
          return;
        }
        setError(cause instanceof ApiError ? cause.message : "the audit trail could not be read");
      } finally {
        setLoading(false);
      }
    },
    [projectId],
  );

  useEffect(() => {
    void load("");
  }, [load]);

  const exportCsv = useCallback(() => {
    if (!body) return;
    const header = ["recorded_at", "action", "actor_user_id", "actor_type", "target_type", "target_id", "metadata"];
    const rows = body.entries.map((entry: ProjectAuditEntry) => [
      entry.created_at,
      entry.action,
      entry.actor_user_id ?? "",
      entry.actor_type,
      entry.target_type ?? "",
      entry.target_id ?? "",
      entry.metadata ?? {},
    ]);
    const csv = [header, ...rows].map((row) => row.map(csvCell).join(",")).join("\n");
    const blob = new Blob([`${csv}\n`], { type: "text/csv;charset=utf-8" });
    const url = URL.createObjectURL(blob);
    const anchor = document.createElement("a");
    anchor.href = url;
    anchor.download = `${body.key.toLowerCase()}-audit.csv`;
    document.body.appendChild(anchor);
    anchor.click();
    anchor.remove();
    URL.revokeObjectURL(url);
  }, [body]);

  const filtering = action !== "";
  const rows = body?.entries ?? [];

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center gap-2">
        <Link
          href={projectId ? `/automation/projects/${projectId}` : "/automation/projects"}
          className="inline-flex items-center gap-1.5 text-[12.5px] text-muted hover:text-ink"
        >
          <ArrowLeft className="size-3.5" aria-hidden />
          {body ? `${body.key} — back to the project` : "Back to the project"}
        </Link>
        <button
          type="button"
          data-project-audit-export
          disabled={rows.length === 0}
          onClick={exportCsv}
          className="ml-auto inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] transition hover:text-ink disabled:opacity-50"
        >
          <Download className="size-3.5" aria-hidden />
          Export CSV
        </button>
      </div>

      {error ? (
        <div role="alert" data-project-audit-error className="flex items-start gap-2 rounded-lg border border-red-500/40 bg-red-500/5 px-3 py-2.5 text-[12.5px]">
          <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-red-600" aria-hidden />
          <span className="flex-1">{error}</span>
        </div>
      ) : null}

      <div className="flex flex-wrap items-end gap-2">
        <label className="flex flex-col gap-1">
          <span className="text-[11.5px] text-muted">Action</span>
          <select
            value={action}
            data-project-audit-filter
            onChange={(event) => {
              setAction(event.target.value);
              void load(event.target.value);
            }}
            className="rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px]"
          >
            <option value="">Every action</option>
            {(body?.actions ?? []).map((name) => (
              <option key={name} value={name}>
                {name}
              </option>
            ))}
          </select>
        </label>
        {loading ? (
          <span className="flex items-center gap-1.5 pb-1.5 text-[12px] text-muted">
            <Loader2 className="size-3.5 animate-spin" aria-hidden />
            Reading…
          </span>
        ) : (
          <span className="pb-1.5 text-[11.5px] text-muted">
            {rows.length} row{rows.length === 1 ? "" : "s"}
          </span>
        )}
      </div>

      {rows.length === 0 && !loading && !error ? (
        <p
          data-project-audit-empty
          className="rounded-lg border border-line bg-quiet-soft px-3 py-2.5 text-[12.5px] text-muted"
        >
          {filtering ? (
            <>
              No <strong>{action}</strong> rows in this project.{" "}
              <button
                type="button"
                data-project-audit-clear
                onClick={() => {
                  setAction("");
                  void load("");
                }}
                className="underline"
              >
                Show every action
              </button>{" "}
              to tell an empty filter apart from an empty trail.
            </>
          ) : (
            <>
              Nothing has been recorded in this project yet. Every membership change, archive,
              restore, move and ownership transfer writes a row here, so a trail this empty means
              the project is new rather than unobserved.
            </>
          )}
        </p>
      ) : null}

      {rows.length > 0 ? (
        <div className="overflow-hidden rounded-xl border border-line bg-surface">
          <div className="overflow-x-auto">
            <table data-project-audit className="w-full border-collapse text-left text-[13px]">
              <thead>
                <tr className="border-b border-line text-[11.5px] text-muted">
                  <th scope="col" className="px-3 py-2.5">When</th>
                  <th scope="col" className="px-3 py-2.5">Action</th>
                  <th scope="col" className="px-3 py-2.5">Actor</th>
                  <th scope="col" className="px-3 py-2.5">Detail</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((entry) => (
                  <tr
                    key={entry.id}
                    data-project-audit-row={entry.action}
                    className="border-b border-line last:border-b-0"
                  >
                    <td className="whitespace-nowrap px-3 py-2.5 text-[12px] text-muted">
                      {new Date(entry.created_at).toLocaleString()}
                    </td>
                    <td className="px-3 py-2.5 text-[12.5px]">{entry.action}</td>
                    <td className="px-3 py-2.5 text-[12px] text-muted">
                      {entry.actor_user_id ?? entry.actor_type}
                    </td>
                    <td className="px-3 py-2.5 text-[11.5px] text-muted">
                      {metadataSummary(entry.metadata)}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      ) : null}
    </div>
  );
}
