"use client";

/**
 * `/media/duplicates` — what this site stores more than once (docs/requests/REQ-010, slice 3).
 *
 * The screen is built around four decisions the API forces, and each of them is a decision an
 * operator could get wrong if the screen let them:
 *
 * **There is no suggested keeper.** Every row asks "which of these do your pages use?" and the
 * answer is a radio the operator picks. A default would be an automatic tie-break by upload
 * time, and an automatic tie-break breaks a live page — the operator finds out from a broken
 * image rather than from this report. The one hint the screen gives is factual, not a choice:
 * the row that nothing points at is the cheapest keeper.
 *
 * **A merge trashes copies; it never deletes them.** The button reads `Merge group`, the result
 * says the bytes are *pending*, and the confirmation names the trash rather than saying
 * "permanently". Every other destructive action in the library has that property and a screen
 * that implied otherwise would be the one place an operator learns to be afraid of it.
 *
 * **A report that finds nothing explains how detection works.** "No duplicates found" with no
 * further sentence reads as "the scan did not run". A one-line statement of what a duplicate *is*
 * (the same bytes, in the same site, both still live) is what makes the empty state a fact
 * rather than a silence.
 *
 * **The cross-site view is a platform account's, and it is visibly labelled as read-only.** A
 * tenant account never sees the toggle, so it cannot discover that the question exists; a
 * platform account sees it with a note saying where the action lives, because offering a button
 * the API would refuse is the one thing this screen must not do.
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import { AlertTriangle, Copy, ExternalLink, Layers, Loader2, RefreshCw } from "lucide-react";
import Link from "next/link";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  fetchCrossSiteDuplicates,
  fetchMediaDuplicates,
  mergeMediaDuplicates,
} from "@/lib/api";
import { formatBytes, formatTimestamp } from "@/lib/format";
import { useSession } from "@/lib/session";
import { useSites } from "@/lib/sites";
import type {
  MediaCrossSiteReport,
  MediaDuplicateGroup,
  MediaDuplicateReport,
} from "@/lib/types";

/** How a file row in an expanded group is drawn: the unused one is the obvious keeper. */
function memberTone(referenceCount: number): string {
  return referenceCount === 0
    ? "border-positive/40 bg-positive-soft"
    : "border-line bg-panel";
}

/** The screen. */
export function MediaDuplicatesView() {
  const { user } = useSession();
  const { selectedSite, sites, status: sitesStatus } = useSites();
  const siteId = selectedSite?.id ?? null;

  // A platform account (the first-run owner) runs the platform, not one tenant, so it may ask the
  // installation-wide question. An account with its own organization never sees the toggle at
  // all — it is not a control it could use, so showing it disabled would only teach that the
  // panel has buttons which do not work.
  const platformAccount = user ? user.organization_id === null : false;
  const [crossSite, setCrossSite] = useState(false);

  const [report, setReport] = useState<MediaDuplicateReport | null>(null);
  const [installation, setInstallation] = useState<MediaCrossSiteReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [reloadToken, setReloadToken] = useState(0);
  // Which group's members are expanded, and which file each expanded group would keep.
  const [expanded, setExpanded] = useState<string | null>(null);
  const [keepers, setKeepers] = useState<Record<string, string>>({});
  const [confirming, setConfirming] = useState<MediaDuplicateGroup | null>(null);

  const siteIds = useMemo(() => (sites ?? []).map((site) => site.id), [sites]);
  const reload = useCallback(() => setReloadToken((token) => token + 1), []);

  useEffect(() => {
    if (!siteId || !crossSite) {
      setReport(null);
      return;
    }
    let cancelled = false;
    setError(null);
    setInstallation(null);
    fetchCrossSiteDuplicates(siteIds.length > 0 ? siteIds : siteId ? [siteId] : [])
      .then((answer) => {
        if (!cancelled) {
          setInstallation(answer);
        }
      })
      .catch((cause: unknown) => {
        if (cancelled) {
          return;
        }
        setError(cause instanceof ApiError ? cause.message : "The report could not be read.");
      });
    return () => {
      cancelled = true;
    };
  }, [crossSite, siteId, siteIds, reloadToken]);

  useEffect(() => {
    if (!siteId || crossSite) {
      setReport(null);
      return;
    }
    let cancelled = false;
    setReport(null);
    setError(null);
    // Always expanded: the whole point of the screen is the choice of keeper, and a report that
    // hides the files behind a click cannot make that choice.
    fetchMediaDuplicates(siteId, { expand: true })
      .then((answer) => {
        if (!cancelled) {
          setReport(answer);
        }
      })
      .catch((cause: unknown) => {
        if (cancelled) {
          return;
        }
        setReport(null);
        setError(cause instanceof ApiError ? cause.message : "The report could not be read.");
      });
    return () => {
      cancelled = true;
    };
  }, [siteId, crossSite, reloadToken]);

  /** Merge the group, keeping the file the operator picked. */
  const onMerge = useCallback(
    async (group: MediaDuplicateGroup) => {
      if (!siteId) {
        return;
      }
      const keep = keepers[group.full_checksum];
      if (!keep) {
        setError("Choose which file to keep before merging this group.");
        return;
      }
      setBusy(true);
      setError(null);
      setNotice(null);
      try {
        const result = await mergeMediaDuplicates({
          siteId,
          checksum: group.full_checksum,
          keep,
        });
        setNotice(result.notice);
        setConfirming(null);
        setExpanded(null);
        reload();
      } catch (cause) {
        setError(cause instanceof ApiError ? cause.message : "The group could not be merged.");
      } finally {
        setBusy(false);
      }
    },
    [siteId, keepers, reload],
  );

  if (sitesStatus === "error") {
    return (
      <EmptyState
        title="The sites could not be loaded"
        hint="The duplicate report follows the selected site, so it needs the sites first."
      />
    );
  }

  if (sitesStatus === "ready" && !siteId) {
    return (
      <EmptyState
        title="No sites yet"
        hint="A site owns a media library. Create one and the duplicate report appears here."
      />
    );
  }

  const groups = report?.groups ?? [];
  const loading = report === null && !error;

  return (
    <div className="space-y-5" data-testid="media-duplicates">
      <header className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h2 className="text-sm font-semibold text-ink">Duplicate files</h2>
          <p className="mt-1 max-w-2xl text-xs text-muted">
            Files with the same bytes, in the same site, both still in the library. A group of one
            is not a duplicate, and neither is a copy already sitting in the trash — the trash
            screen will deal with that one.
          </p>
        </div>
        <div className="flex items-center gap-2">
          {platformAccount && (
            <label className="flex items-center gap-2 rounded-md border border-line bg-panel px-3 py-1.5 text-xs text-ink">
              <input
                id="duplicates-cross-site"
                type="checkbox"
                checked={crossSite}
                onChange={(event) => setCrossSite(event.target.checked)}
              />
              Whole installation
              <span className="text-muted">(platform owner)</span>
            </label>
          )}
          <button
            type="button"
            id="duplicates-refresh"
            onClick={reload}
            className="inline-flex items-center gap-1.5 rounded-md border border-line bg-panel px-3 py-1.5 text-xs text-ink hover:bg-quiet-soft"
          >
            <RefreshCw className="h-3.5 w-3.5" aria-hidden />
            Refresh
          </button>
        </div>
      </header>

      {error && (
        <p
          role="alert"
          className="flex items-start gap-2 rounded-md border border-caution/40 bg-caution-soft px-3 py-2 text-xs text-ink"
        >
          <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0 text-caution" aria-hidden />
          {error}
        </p>
      )}

      {notice && (
        <p
          role="status"
          className="rounded-md border border-positive/40 bg-positive-soft px-3 py-2 text-xs text-ink"
        >
          {notice}
        </p>
      )}

      {crossSite ? (
        <CrossSiteReportView report={installation} />
      ) : loading ? (
        <LoadingTable columns={5} rows={3} />
      ) : groups.length === 0 ? (
        <EmptyState
          title="No duplicates found"
          hint="Two files are duplicates when their bytes are identical, they belong to this site, and neither is in the trash. A file that was replaced keeps its old bytes as a version rather than as a second row, so it never shows up here."
        />
      ) : (
        <>
          <p className="text-xs text-muted" data-testid="duplicates-total">
            {report?.group_count} {report?.group_count === 1 ? "group" : "groups"} ·{" "}
            {formatBytes(report?.reclaimable_bytes ?? 0)} would be returned when the copies are
            purged — merging alone puts them in the trash.
          </p>

          <ul className="space-y-4">
            {groups.map((group) => {
              const files = group.files ?? [];
              const keep = keepers[group.full_checksum] ?? "";
              const open = expanded === group.full_checksum;
              return (
                <li
                  key={group.full_checksum}
                  className="rounded-lg border border-line bg-panel"
                  data-testid="duplicates-group"
                >
                  <div className="flex flex-wrap items-center gap-3 border-b border-line px-4 py-3">
                    <code className="font-mono text-xs text-muted">{group.checksum}</code>
                    <span className="text-xs text-ink">
                      {group.file_count} {group.file_count === 1 ? "copy" : "copies"}
                    </span>
                    <span className="text-xs text-muted">
                      {formatBytes(group.total_bytes)} held ·{" "}
                      {formatBytes(group.reclaimable_bytes)} reclaimable
                    </span>
                    <span className="text-xs text-muted">
                      first seen {formatTimestamp(group.first_seen)}
                    </span>
                    <div className="ml-auto flex items-center gap-2">
                      <button
                        type="button"
                        onClick={() => {
                          void navigator.clipboard
                            ?.writeText(group.full_checksum)
                            .catch(() => undefined);
                        }}
                        className="inline-flex items-center gap-1 rounded-md border border-line px-2 py-1 text-xs text-muted hover:bg-quiet-soft"
                        title="Copy the full checksum"
                      >
                        <Copy className="h-3 w-3" aria-hidden />
                        <span className="sr-only">Copy the full checksum</span>
                      </button>
                      <button
                        type="button"
                        id={`duplicates-toggle-${group.checksum.replace(/[^a-z0-9]/gi, "")}`}
                        aria-expanded={open}
                        onClick={() => setExpanded(open ? null : group.full_checksum)}
                        className="inline-flex items-center gap-1 rounded-md border border-line px-2 py-1 text-xs text-ink hover:bg-quiet-soft"
                      >
                        <Layers className="h-3 w-3" aria-hidden />
                        {open ? "Hide files" : "Show files"}
                      </button>
                    </div>
                  </div>

                  {open && (
                    <div className="px-4 py-3">
                      <table className="w-full text-left text-xs">
                        <thead className="text-muted">
                          <tr>
                            <th className="py-1 pr-3 font-medium">Keep</th>
                            <th className="py-1 pr-3 font-medium">Name</th>
                            <th className="py-1 pr-3 font-medium">Size</th>
                            <th className="py-1 pr-3 font-medium">Used in</th>
                            <th className="py-1 pr-3 font-medium">Uploaded</th>
                            <th className="py-1 font-medium" />
                          </tr>
                        </thead>
                        <tbody>
                          {files.map((file) => (
                            <tr
                              key={file.id}
                              className={`border-t border-line ${memberTone(file.reference_count)}`}
                            >
                              <td className="py-2 pr-3">
                                <input
                                  type="radio"
                                  name={`keep-${group.full_checksum}`}
                                  aria-label={`Keep ${file.filename}`}
                                  checked={keep === file.id}
                                  onChange={() =>
                                    setKeepers((current) => ({
                                      ...current,
                                      [group.full_checksum]: file.id,
                                    }))
                                  }
                                />
                              </td>
                              <td className="py-2 pr-3">
                                <Link
                                  href={`/media/files/${file.id}`}
                                  className="inline-flex items-center gap-1 text-ink hover:underline"
                                >
                                  {file.filename}
                                  <ExternalLink className="h-3 w-3" aria-hidden />
                                </Link>
                                {file.reference_count === 0 && (
                                  <span className="ml-2 rounded-full bg-positive-soft px-2 py-0.5 text-[11px] text-positive">
                                    nothing uses this
                                  </span>
                                )}
                              </td>
                              <td className="py-2 pr-3 text-muted">
                                {formatBytes(file.size_bytes)}
                              </td>
                              <td className="py-2 pr-3 text-muted">
                                {file.reference_count}{" "}
                                {file.reference_count === 1 ? "record" : "records"}
                              </td>
                              <td className="py-2 pr-3 text-muted">
                                {formatTimestamp(file.uploaded_at)}
                              </td>
                              <td className="py-2" />
                            </tr>
                          ))}
                        </tbody>
                      </table>

                      <div className="mt-4 flex items-center justify-between gap-3">
                        <p className="text-xs text-muted">
                          Merging points every reference at the file you keep and moves the others
                          to the trash. Nothing is deleted: the copies stay restorable until the
                          trash is purged.
                        </p>
                        <button
                          type="button"
                          id={`duplicates-merge-${group.checksum.replace(/[^a-z0-9]/gi, "")}`}
                          disabled={busy || !keep}
                          onClick={() => setConfirming(group)}
                          className="inline-flex items-center gap-1.5 rounded-md bg-terracotta px-3 py-1.5 text-xs font-medium text-white disabled:cursor-not-allowed disabled:opacity-50"
                        >
                          {busy ? (
                            <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
                          ) : null}
                          Merge group
                        </button>
                      </div>
                    </div>
                  )}
                </li>
              );
            })}
          </ul>
        </>
      )}

      {confirming && (
        <MergeConfirm
          group={confirming}
          keepName={
            (confirming.files ?? []).find((file) => file.id === keepers[confirming.full_checksum])
              ?.filename ?? "the file you chose"
          }
          busy={busy}
          onCancel={() => setConfirming(null)}
          onConfirm={() => void onMerge(confirming)}
        />
      )}
    </div>
  );
}

/** The installation-wide report: where the copies are, and where the action lives. */
function CrossSiteReportView({ report }: { report: MediaCrossSiteReport | null }) {
  if (!report) {
    return <LoadingTable columns={4} rows={3} />;
  }
  if (report.groups.length === 0) {
    return (
      <EmptyState
        title="Nothing is stored twice across these sites"
        hint="This report groups by the file's bytes across every site in scope. Two tenants holding the same file show up here and in neither tenant's own duplicate report — which is why this question is asked from the platform level."
      />
    );
  }
  return (
    <div className="space-y-4">
      <p className="rounded-md border border-line bg-panel px-3 py-2 text-xs text-muted">
        {report.notice}
      </p>
      <ul className="space-y-3">
        {report.groups.map((group) => (
          <li
            key={group.full_checksum}
            className="rounded-lg border border-line bg-panel px-4 py-3"
            data-testid="duplicates-cross-group"
          >
            <div className="flex flex-wrap items-center gap-3 text-xs">
              <code className="font-mono text-muted">{group.checksum}</code>
              <span className="text-ink">
                {group.file_count} copies in {group.site_count} sites
              </span>
              <span className="text-muted">{formatBytes(group.total_bytes)}</span>
            </div>
            <ul className="mt-2 flex flex-wrap gap-2">
              {group.sites.map((copy) => (
                <li
                  key={copy.site_id}
                  className="rounded-full border border-line bg-quiet-soft px-2 py-0.5 text-[11px] text-muted"
                >
                  {copy.site_name} · {copy.file_count} · {formatBytes(copy.site_bytes)}
                </li>
              ))}
            </ul>
          </li>
        ))}
      </ul>
    </div>
  );
}

/** The confirmation a merge asks for, worded so the trash — not deletion — is what happens. */
function MergeConfirm({
  group,
  keepName,
  busy,
  onCancel,
  onConfirm,
}: {
  group: MediaDuplicateGroup;
  keepName: string;
  busy: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  // The keeper is not passed as an id here: the dialog says what will happen in *words*, and a
  // file name is the word. The count comes from the group rather than from a running total, so
  // the sentence cannot drift from the rows above it.
  const others = (group.files ?? []).length - 1;
  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-ink/40 p-4"
      role="dialog"
      aria-modal="true"
      aria-label="Merge duplicate group"
    >
      <div className="w-full max-w-md rounded-lg border border-line bg-panel p-5 shadow-lg">
        <h3 className="text-sm font-semibold text-ink">Merge this group?</h3>
        <p className="mt-2 text-xs text-muted">
          {others} {others === 1 ? "copy" : "copies"} will be moved to the trash and{" "}
          {formatBytes(group.reclaimable_bytes)} will be reclaimed once the trash is purged.
          Everything pointing at them follows <strong className="text-ink">{keepName}</strong>.
        </p>
        <ul className="mt-3 space-y-1 text-xs text-muted">
          <li>The copies stay restorable until the retention window purges them.</li>
          <li>Any share link over a copy is closed — the holder is told the link stopped working.</li>
          <li>You can restore a copy from the trash, which puts the group back.</li>
        </ul>
        <div className="mt-5 flex justify-end gap-2">
          <button
            type="button"
            onClick={onCancel}
            className="rounded-md border border-line px-3 py-1.5 text-xs text-ink hover:bg-quiet-soft"
          >
            Cancel
          </button>
          <button
            type="button"
            id="duplicates-merge-confirm"
            disabled={busy}
            onClick={onConfirm}
            className="inline-flex items-center gap-1.5 rounded-md bg-terracotta px-3 py-1.5 text-xs font-medium text-white disabled:opacity-50"
          >
            {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : null}
            Merge {others} {others === 1 ? "copy" : "copies"}
          </button>
        </div>
      </div>
    </div>
  );
}
