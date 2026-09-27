"use client";

/**
 * Media trash: what a delete left behind (docs/requests/REQ-010, slice 1).
 *
 * A delete in the browser is a *trash*, not a purge — the bytes and the row stay until they are
 * purged here or the retention window (slice 4) runs. That is what makes a mistaken delete
 * recoverable, and this screen is where the recovery happens: restore one file or the whole
 * selection, or purge for good.
 *
 * The countdown is the honest part: a row states the day its bytes go, and a file inside seven days
 * of it is marked, because "it will be deleted eventually" is not what an operator needs to see
 * when they are deciding whether to restore something.
 */
import { useCallback, useEffect, useState } from "react";

import { RotateCcw, Trash2, X } from "lucide-react";
import Link from "next/link";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { ScanBadge, daysUntil, fileIcon } from "@/features/media/media-shared";
import {
  ApiError,
  emptyMediaTrash,
  fetchMediaTrash,
  mediaBulkAction,
  mediaRawUrl,
  purgeMediaFile,
  restoreMediaFile,
} from "@/lib/api";
import { formatBytes, formatTimestamp } from "@/lib/format";
import { useSites } from "@/lib/sites";
import type { MediaTrash } from "@/lib/types";

/** The trashed files of the selected site, with restore and purge. */
export function MediaTrashView() {
  const { selectedSite, status: sitesStatus } = useSites();
  const [trash, setTrash] = useState<MediaTrash | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [selection, setSelection] = useState<string[]>([]);
  const [reloadToken, setReloadToken] = useState(0);

  const siteId = selectedSite?.id ?? null;
  const reload = useCallback(() => setReloadToken((token) => token + 1), []);

  useEffect(() => {
    if (!siteId) {
      setTrash(null);
      return;
    }
    let cancelled = false;
    setTrash(null);
    setError(null);
    fetchMediaTrash(siteId)
      .then((answer) => {
        if (!cancelled) {
          setTrash(answer);
        }
      })
      .catch((cause: unknown) => {
        if (cancelled) {
          return;
        }
        setError(
          cause instanceof ApiError ? cause.message : "The trash could not be loaded.",
        );
      });
    return () => {
      cancelled = true;
    };
  }, [siteId, reloadToken]);

  const restoreOne = async (id: string, name: string) => {
    setBusy(true);
    setError(null);
    try {
      await restoreMediaFile(id);
      setNotice(`“${name}” restored to the folder it was deleted from.`);
      setSelection((current) => current.filter((entry) => entry !== id));
      reload();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The file could not be restored.");
    } finally {
      setBusy(false);
    }
  };

  const purgeOne = async (id: string, name: string) => {
    if (!window.confirm(`Delete “${name}” for good?\n\nIts bytes are removed from storage. This cannot be undone.`)) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await purgeMediaFile(id);
      setNotice(`“${name}” deleted permanently.`);
      setSelection((current) => current.filter((entry) => entry !== id));
      reload();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The file could not be purged.");
    } finally {
      setBusy(false);
    }
  };

  const restoreSelection = async () => {
    if (!siteId || selection.length === 0) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const result = await mediaBulkAction(siteId, "restore", selection);
      setNotice(`${result.changed} of ${result.requested} files restored.`);
      setSelection([]);
      reload();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The selection could not be restored.");
    } finally {
      setBusy(false);
    }
  };

  const emptyTrash = async () => {
    if (!siteId) {
      return;
    }
    // A typed confirmation, because this is the one action on the screen that cannot be undone.
    if (window.prompt("Empty the trash?\n\nType EMPTY to delete every file in it for good.") !== "EMPTY") {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const result = await emptyMediaTrash(siteId);
      setNotice(`${result.changed} files deleted permanently.`);
      setSelection([]);
      reload();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The trash could not be emptied.");
    } finally {
      setBusy(false);
    }
  };

  if (sitesStatus === "error") {
    return (
      <div className="rounded-xl border border-line bg-surface">
        <EmptyState
          title="The site list could not be loaded"
          hint="The trash follows the selected site, so it needs the sites first."
        />
      </div>
    );
  }

  if (sitesStatus === "ready" && !selectedSite) {
    return (
      <div className="rounded-xl border border-line bg-surface">
        <EmptyState title="No sites yet" hint="A trash belongs to a site." />
      </div>
    );
  }

  const entries = trash?.entries ?? [];

  return (
    <div className="overflow-hidden rounded-xl border border-line bg-surface">
      <div className="flex flex-wrap items-center justify-between gap-2 border-b border-line px-4 py-3">
        <div className="flex items-baseline gap-2">
          <h2 className="text-[13.5px] font-medium">Trash</h2>
          {trash ? (
            <span className="text-[12px] text-muted">
              {trash.file_count} {trash.file_count === 1 ? "file" : "files"} ·{" "}
              {formatBytes(trash.total_bytes)}
            </span>
          ) : null}
        </div>
        <div className="flex items-center gap-1.5">
          <Link
            href="/media"
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
          >
            Back to the library
          </Link>
          <button
            type="button"
            onClick={() => void emptyTrash()}
            disabled={busy || !trash || trash.file_count === 0}
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-accent-strong transition hover:bg-canvas disabled:cursor-not-allowed disabled:text-muted"
          >
            Empty trash
          </button>
        </div>
      </div>

      {trash && trash.file_count > 0 ? (
        <p className="border-b border-line bg-canvas/60 px-4 py-2 text-[12px] text-muted">
          Files are kept for {trash.retention_days} days after they are deleted, then their bytes
          are removed. Restore anything you still need before the countdown runs out.
        </p>
      ) : null}

      {notice ? (
        <p className="border-b border-line bg-canvas/60 px-4 py-2 text-[12px] text-muted">
          {notice}
        </p>
      ) : null}

      {error ? (
        <div
          role="alert"
          className="flex flex-col items-center gap-3 border-b border-line px-6 py-6 text-center"
        >
          <p className="text-[12.5px] text-accent-strong">{error}</p>
          <button
            type="button"
            onClick={reload}
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
          >
            Try again
          </button>
        </div>
      ) : null}

      {trash === null && !error ? (
        <LoadingTable columns={5} />
      ) : entries.length === 0 ? (
        <EmptyState
          title="The trash is empty"
          hint="Deleting a file in the library brings it here first, where it can be restored before its bytes are removed."
        />
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full border-collapse text-left text-[13px]">
            <thead>
              <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                <th scope="col" className="w-8 px-3 py-2">
                  <label className="sr-only" htmlFor="trash-select-all">
                    Select every file in the trash
                  </label>
                  <input
                    id="trash-select-all"
                    type="checkbox"
                    checked={selection.length === entries.length && entries.length > 0}
                    onChange={(event) =>
                      setSelection(event.target.checked ? entries.map((entry) => entry.id) : [])
                    }
                  />
                </th>
                <th scope="col" className="px-3 py-2 font-medium">
                  Name
                </th>
                <th scope="col" className="px-3 py-2 font-medium">
                  Deleted
                </th>
                <th scope="col" className="px-3 py-2 font-medium">
                  Purges in
                </th>
                <th scope="col" className="px-3 py-2 font-medium">
                  Actions
                </th>
              </tr>
            </thead>
            <tbody>
              {entries.map((entry) => {
                const Icon = fileIcon(entry);
                const remaining = daysUntil(entry.purges_at);
                const urgent = remaining !== null && remaining <= 7;
                return (
                  <tr
                    key={entry.id}
                    className="border-t border-line transition hover:bg-canvas/60"
                  >
                    <td className="px-3 py-2.5">
                      <label className="sr-only" htmlFor={`trash-select-${entry.id}`}>
                        Select {entry.filename}
                      </label>
                      <input
                        id={`trash-select-${entry.id}`}
                        type="checkbox"
                        checked={selection.includes(entry.id)}
                        onChange={() =>
                          setSelection((current) =>
                            current.includes(entry.id)
                              ? current.filter((value) => value !== entry.id)
                              : [...current, entry.id],
                          )
                        }
                      />
                    </td>
                    <td className="px-3 py-2.5">
                      <div className="flex min-w-0 items-center gap-2">
                        <span className="flex size-8 shrink-0 items-center justify-center rounded border border-line bg-canvas">
                          <Icon className="size-3.5 text-muted" aria-hidden />
                        </span>
                        <div className="min-w-0">
                          <p className="truncate font-medium">{entry.filename}</p>
                          <p className="truncate text-[11.5px] text-muted">
                            {formatBytes(entry.size_bytes)} · {entry.kind}
                          </p>
                        </div>
                      </div>
                    </td>
                    <td className="px-3 py-2.5 text-muted">
                      {formatTimestamp(entry.deleted_at)}
                    </td>
                    <td className="px-3 py-2.5">
                      {remaining === null ? (
                        <span className="text-muted">—</span>
                      ) : (
                        <span className={urgent ? "font-medium text-caution" : "text-muted"}>
                          {remaining === 0 ? "today" : `${remaining} ${remaining === 1 ? "day" : "days"}`}
                        </span>
                      )}
                      <span className="ml-2">
                        <ScanBadge status={entry.scan_status} />
                      </span>
                    </td>
                    <td className="px-3 py-2.5">
                      <div className="flex items-center gap-1">
                        <a
                          href={mediaRawUrl(entry.id)}
                          aria-label={`Download ${entry.filename}`}
                          className="rounded px-2 py-1 text-[12px] text-muted transition hover:text-ink"
                        >
                          Download
                        </a>
                        <button
                          type="button"
                          onClick={() => void restoreOne(entry.id, entry.filename)}
                          disabled={busy}
                          className="flex items-center gap-1 rounded px-2 py-1 text-[12px] text-muted transition hover:text-ink disabled:opacity-50"
                        >
                          <RotateCcw className="size-3" aria-hidden />
                          Restore
                        </button>
                        <button
                          type="button"
                          onClick={() => void purgeOne(entry.id, entry.filename)}
                          disabled={busy}
                          aria-label={`Delete ${entry.filename} for good`}
                          className="rounded p-1.5 text-muted transition hover:text-accent-strong disabled:opacity-50"
                        >
                          <Trash2 className="size-3.5" aria-hidden />
                        </button>
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}

      {selection.length > 0 ? (
        <div
          role="region"
          aria-label="Selection"
          className="sticky bottom-0 flex flex-wrap items-center gap-2 border-t border-line bg-surface px-4 py-2.5"
        >
          <span className="text-[12.5px] font-medium">{selection.length} selected</span>
          <button
            type="button"
            onClick={() => void restoreSelection()}
            disabled={busy}
            className="rounded-lg border border-line px-2.5 py-1 text-[12px] transition hover:bg-canvas disabled:opacity-50"
          >
            Restore selection
          </button>
          <button
            type="button"
            onClick={() => setSelection([])}
            className="ml-auto flex items-center gap-1.5 rounded-lg px-2.5 py-1 text-[12px] text-muted transition hover:text-ink"
          >
            <X className="size-3" aria-hidden />
            Clear
          </button>
        </div>
      ) : null}
    </div>
  );
}
