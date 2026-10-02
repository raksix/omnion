"use client";

/**
 * `/themes/<key>/history` — theme settings revisions (REQ-062, slice 2).
 *
 * Four things the history screen has to be, and each has a version that looks fine and lies:
 *
 * 1. **The list is the server's, and a row knows three separate things**: which revision is
 *    live, which one is the draft, and which one is itself a restore of another. Three badges
 *    on a row is not decoration — "you are editing a draft" and "visitors see this" are
 *    different facts that a single `active` flag cannot both answer.
 * 2. **A restore is a NEW revision, and the screen says so before it does it.** A confirm
 *    dialog reading "Restore revision 2?" invites the reading that history rewinds; the store
 *    appends revision N+1 whose content came from 2, and the dialog says that instead.
 * 3. **The diff is per-field, from the server, and a first revision shows an empty state
 *    rather than a blank panel.** Revision 1 has nothing to be different from, and a screen
 *    that renders "no changes" there is indistinguishable from a screen whose diff failed.
 * 4. **A row can be opened and read even when it cannot be restored** — reading history is a
 *    `themes.read`, restoring is a `themes.customize`, and the 403 arrives as a fact on the
 *    action rather than as a missing button that leaves the operator guessing.
 */
import { useCallback, useEffect, useState } from "react";
import { AlertTriangle, Check, History, Loader2, RotateCcw } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import {
  ApiError,
  fetchThemeRevision,
  fetchThemeSettings,
  restoreThemeRevision,
} from "@/lib/api";
import type { ThemeRevisionDetail, ThemeSettingsView } from "@/lib/api";
import { useSites } from "@/lib/sites";

/** Format an ISO timestamp the way the rest of the panel does: short, local, no timezone noise. */
function when(value: string): string {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return date.toLocaleString(undefined, {
    year: "numeric",
    month: "short",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** A field name as a person reads it. The wire names are camelCase; the panel is prose. */
const FIELD_LABELS: Record<string, string> = {
  themeKey: "Theme",
  defaultMode: "Default mode",
  tokens: "Colour tokens",
  typography: "Typography",
  layout: "Layout",
  branding: "Branding",
  headerFooter: "Header & footer",
};

export function ThemeHistoryView() {
  const { selectedSite, status: siteStatus, error: siteError } = useSites();
  const [view, setView] = useState<ThemeSettingsView | null>(null);
  const [openNo, setOpenNo] = useState<number | null>(null);
  const [detail, setDetail] = useState<ThemeRevisionDetail | null>(null);
  const [loading, setLoading] = useState(false);
  const [detailLoading, setDetailLoading] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [confirm, setConfirm] = useState<number | null>(null);

  const load = useCallback(async () => {
    if (!selectedSite) return;
    setLoading(true);
    setError(null);
    try {
      const next = await fetchThemeSettings(selectedSite.id);
      setView(next);
      // Open the live revision on arrival when nothing is open yet: a history screen that
      // loads with a bare list makes the operator click to discover what is live.
      setOpenNo((current) => current ?? next.published?.revisionNo ?? next.revisions[0]?.revisionNo ?? null);
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setLoading(false);
    }
  }, [selectedSite]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    if (!selectedSite || openNo === null) {
      setDetail(null);
      return;
    }
    let cancelled = false;
    setDetailLoading(true);
    fetchThemeRevision(selectedSite.id, openNo)
      .then((next) => {
        if (!cancelled) setDetail(next);
      })
      .catch((caught: ApiError) => {
        if (!cancelled) {
          setDetail(null);
          setError(caught.message);
        }
      })
      .finally(() => {
        if (!cancelled) setDetailLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [openNo, selectedSite]);

  const restore = useCallback(async () => {
    if (!selectedSite || confirm === null) return;
    const target = confirm;
    setConfirm(null);
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const next = await restoreThemeRevision(selectedSite.id, target);
      setView(next);
      setOpenNo(next.draft?.revisionNo ?? target);
      setNotice(
        `Restored revision ${target} as draft revision ${next.draft?.revisionNo ?? "?"}. History is append-only, so the older revision is still here.`,
      );
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setBusy(false);
    }
  }, [confirm, selectedSite]);

  // ------------------------------------------------------------------ states
  if (siteStatus === "error") {
    return <EmptyState title="No site selected" hint={siteError ?? "The site list could not be loaded."} />;
  }
  if (!selectedSite) {
    return <EmptyState title="No site selected" hint="Pick a site to read its settings history." />;
  }
  if (loading && !view) {
    return (
      <div className="space-y-3" data-theme-history-loading>
        {[0, 1, 2].map((index) => (
          <div key={index} className="h-14 animate-pulse rounded-lg bg-line" />
        ))}
      </div>
    );
  }
  if (error && !view) {
    return (
      <EmptyState
        title="The revision history could not be loaded"
        hint={error}
        action={
          <button type="button" className="btn btn-ghost" onClick={() => void load()}>
            <RotateCcw className="h-4 w-4" aria-hidden />
            Try again
          </button>
        }
      />
    );
  }
  if (!view) {
    return <div className="h-64 animate-pulse rounded-lg bg-line" />;
  }

  return (
    <div className="space-y-6" data-theme-history>
      <header className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h2 className="flex items-center gap-2 text-sm font-semibold text-ink">
            <History className="h-4 w-4" aria-hidden />
            {view.themeKey} settings history
          </h2>
          <p className="mt-1 text-xs text-muted" data-theme-history-summary>
            {view.revisions.length === 0
              ? "No revision has been saved yet."
              : `${view.revisions.length} revision${view.revisions.length === 1 ? "" : "s"} · live: ${
                  view.published ? `revision ${view.published.revisionNo}` : "the theme defaults"
                }`}
          </p>
        </div>
        <button type="button" className="btn btn-ghost" onClick={() => void load()} disabled={loading}>
          <RotateCcw className={`h-4 w-4${loading ? " animate-spin" : ""}`} aria-hidden />
          Reload
        </button>
      </header>

      {error ? (
        <p className="text-sm text-danger" role="alert" data-theme-history-error>
          {error}
        </p>
      ) : null}
      {notice ? (
        <p className="text-sm text-ok" role="status" data-theme-history-notice>
          {notice}
        </p>
      ) : null}

      {view.revisions.length === 0 ? (
        <EmptyState
          title="No revisions yet"
          hint="Every save from the customize screen appends a revision here, and the first publish is what visitors see."
        />
      ) : (
        <div className="grid gap-4 lg:grid-cols-[minmax(0,20rem)_minmax(0,1fr)]">
          <ul className="space-y-2" data-theme-revision-list>
            {view.revisions.map((revision) => {
              const open = openNo === revision.revisionNo;
              return (
                <li key={revision.id}>
                  <button
                    type="button"
                    className={`w-full rounded-lg border px-3 py-2 text-left ${
                      open ? "border-accent bg-accent-soft" : "border-line bg-panel"
                    }`}
                    data-theme-revision={revision.revisionNo}
                    aria-expanded={open}
                    onClick={() => setOpenNo(revision.revisionNo)}
                  >
                    <span className="flex items-center justify-between gap-2">
                      <span className="text-sm font-medium text-ink">Revision {revision.revisionNo}</span>
                      <span className="flex items-center gap-1">
                        {revision.isPublished ? (
                          <span
                            className="inline-flex items-center gap-1 rounded-full bg-positive-soft px-2 py-0.5 text-[11px] text-positive"
                            data-theme-revision-live={revision.revisionNo}
                          >
                            <Check className="h-3 w-3" aria-hidden />
                            Live
                          </span>
                        ) : null}
                        {revision.isDraft ? (
                          <span
                            className="rounded-full bg-accent-soft px-2 py-0.5 text-[11px] text-ink"
                            data-theme-revision-draft={revision.revisionNo}
                          >
                            Draft
                          </span>
                        ) : null}
                        {revision.restoredFromNo !== null ? (
                          <span
                            className="rounded-full bg-line px-2 py-0.5 text-[11px] text-muted"
                            data-theme-revision-restored={revision.revisionNo}
                            title={`Restores revision ${revision.restoredFromNo}`}
                          >
                            ← {revision.restoredFromNo}
                          </span>
                        ) : null}
                      </span>
                    </span>
                    <span className="mt-0.5 block text-xs text-muted">
                      {when(revision.createdAt)}
                      {revision.createdByName ? ` · ${revision.createdByName}` : ""}
                      {revision.themeKey !== view.themeKey ? ` · ${revision.themeKey}` : ""}
                    </span>
                  </button>
                </li>
              );
            })}
          </ul>

          <section className="rounded-lg border border-line bg-panel p-4" data-theme-revision-detail>
            {detailLoading ? (
              <p className="flex items-center gap-2 text-sm text-muted" data-theme-revision-detail-loading>
                <Loader2 className="h-4 w-4 animate-spin" aria-hidden />
                Loading revision {openNo}…
              </p>
            ) : !detail || openNo === null ? (
              <p className="text-sm text-muted" data-theme-revision-detail-empty>
                Pick a revision to see what it changed.
              </p>
            ) : (
              <>
                <header className="flex flex-wrap items-start justify-between gap-2">
                  <div>
                    <h3 className="text-sm font-semibold text-ink">
                      Revision {detail.revision.revisionNo}
                      {detail.revision.publishedAt ? (
                        <span className="ml-2 text-xs font-normal text-muted">
                          published {when(detail.revision.publishedAt)}
                        </span>
                      ) : null}
                    </h3>
                    <p className="text-xs text-muted">
                      saved {when(detail.revision.createdAt)} · mode {detail.revision.defaultMode}
                    </p>
                  </div>
                  <button
                    type="button"
                    className="btn btn-ghost"
                    data-theme-revision-restore={detail.revision.revisionNo}
                    disabled={busy}
                    onClick={() => setConfirm(detail.revision.revisionNo)}
                    title="Copy this revision's values into a new draft"
                  >
                    {busy ? <Loader2 className="h-4 w-4 animate-spin" aria-hidden /> : <RotateCcw className="h-4 w-4" aria-hidden />}
                    Restore
                  </button>
                </header>

                {detail.diff.length === 0 ? (
                  <p className="mt-3 text-sm text-muted" data-theme-revision-diff-empty>
                    {detail.revision.revisionNo === 1
                      ? "This is the first revision, so there is nothing before it to compare against."
                      : "Nothing differs from the revision before it."}
                  </p>
                ) : (
                  <ul className="mt-3 space-y-2" data-theme-revision-diff>
                    {detail.diff.map((change) => (
                      <li
                        key={change.field}
                        className="rounded-md border border-line px-3 py-2 text-sm"
                        data-theme-diff-field={change.field}
                      >
                        <p className="font-medium text-ink">{FIELD_LABELS[change.field] ?? change.field}</p>
                        <p className="mt-1 text-xs text-muted">
                          <span className="line-through">{summarise(change.from)}</span>
                          <span aria-hidden> → </span>
                          <span className="text-ink">{summarise(change.to)}</span>
                        </p>
                      </li>
                    ))}
                  </ul>
                )}
              </>
            )}
          </section>
        </div>
      )}

      {confirm !== null ? (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4"
          role="dialog"
          aria-modal="true"
          aria-labelledby="theme-restore-title"
          data-theme-restore-confirm
        >
          <div className="w-full max-w-md rounded-lg border border-line bg-panel p-5 shadow-lg">
            <h2 id="theme-restore-title" className="text-base font-semibold text-ink">
              Restore revision {confirm}?
            </h2>
            <p className="mt-2 text-sm text-muted">
              This writes a <strong>new</strong> draft revision carrying revision {confirm}&apos;s
              values. History is append-only, so nothing is deleted and the restore is itself
              reversible.
            </p>
            <div className="mt-4 flex justify-end gap-2">
              <button type="button" className="btn btn-ghost" onClick={() => setConfirm(null)}>
                Cancel
              </button>
              <button
                type="button"
                className="btn btn-primary"
                data-theme-restore-accept
                onClick={() => void restore()}
              >
                Restore
              </button>
            </div>
          </div>
        </div>
      ) : null}
    </div>
  );
}

/** A diff value as one readable line. Objects become `key: value` pairs, not `[object Object]`. */
function summarise(value: unknown): string {
  if (value === null || value === undefined) return "—";
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  if (typeof value === "object") {
    const entries = Object.entries(value as Record<string, unknown>);
    if (entries.length === 0) return "{}";
    return entries
      .map(([key, item]) =>
        item && typeof item === "object"
          ? `${key}: {${Object.entries(item as Record<string, unknown>)
              .map(([mode, colour]) => `${mode} ${String(colour)}`)
              .join(", ")}}`
          : `${key}: ${String(item)}`,
      )
      .join(" · ");
  }
  return String(value);
}
