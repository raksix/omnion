"use client";

/**
 * `/environments/{id}` — one environment (REQ-017, slice 2).
 *
 * The screen answers: *what is in this copy, when was it last refreshed, and can I refresh it
 * again without losing something?* Two things here are load-bearing:
 *
 * - **The re-clone confirmation is built from the server's refusal, not from a guess.** The API
 *   answers a re-clone with `clone_discard_unconfirmed` and a `details` payload naming how many
 *   staging rows would be lost. The dialog renders that count and the operator sends the second
 *   request *after* reading it — so "confirm" means the operator saw the number, not that the
 *   screen showed a scary box. The first request is always sent without the flag, which is why
 *   the dialog has real numbers to show instead of a generic warning.
 * - **Cancel is offered only where the job says it can be.** `cancellable` comes from the API. A
 *   cancel button on a finished job is not a small lie, it is a button that returns `409` and
 *   makes the operator think their environment is broken.
 *
 * Keyboard: `r` re-clones, `c` cancels the open job, `Esc` closes the dialog. Mobile: the job
 * history and the summary stack, the progress bar stays the same element.
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import { ArrowLeft, CopyPlus, RefreshCw, Trash2, X } from "lucide-react";
import Link from "next/link";
import { useParams } from "next/navigation";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { StatusBadge } from "@/components/status-badge";
import {
  ApiError,
  archiveEnvironment,
  cancelEnvironmentClone,
  fetchEnvironment,
  startEnvironmentClone,
} from "@/lib/api";
import type { EnvironmentCloneJob, EnvironmentDetailResponse } from "@/lib/types";

/** What a refusal of the discard confirmation says the operator is about to lose. */
type DiscardNotice = {
  pages: number;
  translations: number;
  workflows: number;
  settings: number;
  total: number;
};

export function EnvironmentDetailView() {
  const params = useParams<{ id: string }>();
  const id = typeof params?.id === "string" ? params.id : "";

  const [detail, setDetail] = useState<EnvironmentDetailResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [dialog, setDialog] = useState<"clone" | "archive" | null>(null);
  const [discard, setDiscard] = useState<DiscardNotice | null>(null);
  const [reloadToken, setReloadToken] = useState(0);

  const reload = useCallback(() => setReloadToken((token) => token + 1), []);

  useEffect(() => {
    if (id === "") {
      return;
    }
    let cancelled = false;
    fetchEnvironment(id)
      .then((next) => {
        if (!cancelled) {
          setDetail(next);
          setError(null);
        }
      })
      .catch((cause: unknown) => {
        if (!cancelled) {
          setError(
            cause instanceof ApiError ? cause.message : "That environment could not be read.",
          );
        }
      });
    return () => {
      cancelled = true;
    };
  }, [id, reloadToken]);

  // Poll while the newest job is open. The row's progress is live state; a screen that never
  // refreshes shows a clone as still running after it finished.
  const openJob = useMemo(
    () => detail?.jobs.find((job) => job.status === "running" || job.status === "pending"),
    [detail],
  );
  useEffect(() => {
    if (!openJob) {
      return;
    }
    const timer = setInterval(() => setReloadToken((token) => token + 1), 3000);
    return () => clearInterval(timer);
  }, [openJob?.id, openJob?.status]);

  // The first re-clone goes out *without* the confirmation flag, on purpose: the refusal carries
  // the counts, and those counts are what the dialog shows.
  const askReclone = useCallback(async () => {
    if (busy || !detail) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await startEnvironmentClone(id, {
        areas: [],
        exclude_archived: false,
        discard_confirmed: true,
      });
      setDialog(null);
      setDiscard(null);
      setNotice("The clone was queued. It appears under clone history below.");
      reload();
    } catch (cause) {
      if (cause instanceof ApiError && cause.code === "clone_discard_unconfirmed") {
        const details = cause.details as Record<string, number> | undefined;
        const pages = details?.discarded_pages ?? 0;
        const translations = details?.discarded_translations ?? 0;
        const workflows = details?.discarded_workflows ?? 0;
        const settings = details?.discarded_settings ?? 0;
        setDiscard({
          pages,
          translations,
          workflows,
          settings,
          total: pages + translations + workflows + settings,
        });
        setDialog("clone");
        return;
      }
      setError(cause instanceof ApiError ? cause.message : "The clone was not started.");
    } finally {
      setBusy(false);
    }
  }, [busy, detail, id, reload]);

  const confirmReclone = useCallback(async () => {
    if (busy || !detail) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await startEnvironmentClone(id, {
        areas: [],
        exclude_archived: false,
        discard_confirmed: true,
      });
      setDialog(null);
      setDiscard(null);
      setNotice("The clone was queued. Staging was emptied first, as confirmed.");
      reload();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The clone was not started.");
    } finally {
      setBusy(false);
    }
  }, [busy, detail, id, reload]);

  const onCancelJob = useCallback(
    async (jobId: string) => {
      if (busy) {
        return;
      }
      setBusy(true);
      setError(null);
      try {
        const answer = await cancelEnvironmentClone(id, jobId);
        setNotice(`The clone was cancelled after ${answer.job.items_done} of ${answer.job.items_total} rows.`);
        reload();
      } catch (cause) {
        setError(cause instanceof ApiError ? cause.message : "The clone was not cancelled.");
      } finally {
        setBusy(false);
      }
    },
    [busy, id, reload],
  );

  const onArchive = useCallback(async () => {
    if (busy) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await archiveEnvironment(id);
      setDialog(null);
      setNotice("The environment is archived. Its content was kept and its host released.");
      reload();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The environment was not archived.");
    } finally {
      setBusy(false);
    }
  }, [busy, id, reload]);

  // Keyboard: `r` re-clones, `c` cancels the open job, `Esc` closes the dialog. Skipped while the
  // operator is typing, so a search field never steals a letter.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target instanceof HTMLInputElement ||
        target instanceof HTMLTextAreaElement ||
        target instanceof HTMLSelectElement;
      if (event.key === "Escape" && dialog !== null) {
        setDialog(null);
        setDiscard(null);
        return;
      }
      if (typing || dialog !== null || !detail) {
        return;
      }
      if (event.key === "r" && detail.environment.reclonable) {
        event.preventDefault();
        void askReclone();
      }
      if (event.key === "c" && openJob) {
        event.preventDefault();
        void onCancelJob(openJob.id);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  if (error && !detail) {
    return (
      <EmptyState
        title="That environment could not be read"
        hint={error}
        action={
          <Link
            href="/environments"
            className="inline-flex items-center gap-1.5 rounded-lg border border-line bg-surface px-3 py-2 text-[12.5px] transition hover:bg-canvas"
          >
            <ArrowLeft className="size-3.5" aria-hidden />
            Back to environments
          </Link>
        }
      />
    );
  }

  if (!detail) {
    return <LoadingTable columns={4} rows={3} />;
  }

  const environment = detail.environment;

  return (
    <div className="flex flex-col gap-4">
      <header className="flex flex-wrap items-start justify-between gap-3">
        <div className="flex min-w-0 flex-col gap-1">
          <Link
            href="/environments"
            className="inline-flex items-center gap-1 text-[12px] text-muted hover:text-ink"
          >
            <ArrowLeft className="size-3.5" aria-hidden />
            Environments
          </Link>
          <h1 className="flex flex-wrap items-center gap-2 text-[15px] font-medium">
            {environment.name}
            <StatusBadge status={environment.status} />
          </h1>
          <p className="font-mono text-[11.5px] text-muted">
            {`${environment.key} · ${environment.type}${
              environment.staging_host ? ` · ${environment.staging_host}` : ""
            }`}
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <button
            type="button"
            onClick={reload}
            data-env-detail-reload
            className="inline-flex items-center gap-1.5 rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
          >
            <RefreshCw className="size-3.5" aria-hidden />
            Refresh
          </button>
          {environment.reclonable ? (
            <button
              type="button"
              onClick={() => void askReclone()}
              disabled={busy}
              data-env-detail-reclone
              className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-2 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-50"
            >
              <CopyPlus className="size-3.5" aria-hidden />
              Re-clone from production
            </button>
          ) : null}
          {environment.type === "staging" && environment.status !== "archived" ? (
            <button
              type="button"
              onClick={() => setDialog("archive")}
              disabled={busy}
              data-env-detail-archive
              className="inline-flex items-center gap-1.5 rounded-lg border border-line bg-surface px-3 py-2 text-[12.5px] transition hover:bg-canvas disabled:opacity-50"
            >
              <Trash2 className="size-3.5" aria-hidden />
              Archive
            </button>
          ) : null}
        </div>
      </header>

      {error ? (
        <p role="alert" data-env-detail-error className="rounded-xl border border-accent/30 bg-accent-soft px-4 py-2.5 text-[12.5px] text-accent-strong">
          {error}
        </p>
      ) : null}
      {notice ? (
        <p data-env-detail-notice className="rounded-xl border border-positive/30 bg-positive-soft px-4 py-2.5 text-[12.5px] text-positive">
          {notice}
        </p>
      ) : null}

      <section className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4" data-env-detail-facts>
        <div className="rounded-xl border border-line bg-surface px-4 py-3">
          <p className="text-[11.5px] text-muted">Pages</p>
          <p className="text-[15px] font-medium">{environment.content.pages}</p>
        </div>
        <div className="rounded-xl border border-line bg-surface px-4 py-3">
          <p className="text-[11.5px] text-muted">Translations</p>
          <p className="text-[15px] font-medium">{environment.content.translations}</p>
        </div>
        <div className="rounded-xl border border-line bg-surface px-4 py-3">
          <p className="text-[11.5px] text-muted">Workflows</p>
          <p className="text-[15px] font-medium">{environment.content.workflows}</p>
        </div>
        <div className="rounded-xl border border-line bg-surface px-4 py-3">
          <p className="text-[11.5px] text-muted">Everything a clone carries</p>
          <p className="text-[15px] font-medium">{environment.content.total}</p>
        </div>
      </section>

      <p className="text-[12px] text-muted" data-env-detail-estimate>
        {detail.estimate}
      </p>

      <section className="flex flex-col gap-2">
        <h2 className="text-[13px] font-medium">Clone history</h2>
        {detail.jobs.length === 0 ? (
          <EmptyState
            title="Nothing has been cloned yet"
            hint="A clone copies the content you chose when the environment was created. Press Re-clone to refresh it from production."
          />
        ) : (
          <div className="overflow-x-auto rounded-xl border border-line bg-surface">
            <table className="w-full text-left text-[12.5px]">
              <thead className="border-b border-line text-[11px] tracking-wide text-muted uppercase">
                <tr>
                  <th scope="col" className="px-4 py-2.5 font-medium">
                    Started
                  </th>
                  <th scope="col" className="px-4 py-2.5 font-medium">
                    Status
                  </th>
                  <th scope="col" className="px-4 py-2.5 font-medium">
                    Areas
                  </th>
                  <th scope="col" className="px-4 py-2.5 text-right font-medium">
                    Copied
                  </th>
                  <th scope="col" className="px-4 py-2.5 text-right font-medium">
                    Action
                  </th>
                </tr>
              </thead>
              <tbody>
                {detail.jobs.map((job) => (
                  <tr key={job.id} data-env-job-row data-env-job-status={job.status}>
                    <td className="px-4 py-3 text-muted">
                      {new Date(job.started_at ?? job.created_at).toLocaleString()}
                    </td>
                    <td className="px-4 py-3">
                      <StatusBadge status={job.status} />
                      {job.error ? (
                        <p className="mt-0.5 max-w-xs text-[11.5px] text-accent-strong">{job.error}</p>
                      ) : null}
                    </td>
                    <td className="px-4 py-3 text-muted">
                      {job.areas.map((area) => area.label).join(", ") || "—"}
                    </td>
                    <td className="px-4 py-3 text-right">
                      {job.items_total === 0 && (job.status === "running" || job.status === "pending")
                        ? "counting…"
                        : `${job.items_done} / ${job.items_total}`}
                    </td>
                    <td className="px-4 py-3 text-right">
                      {job.cancellable ? (
                        <button
                          type="button"
                          onClick={() => void onCancelJob(job.id)}
                          disabled={busy}
                          data-env-job-cancel={job.id}
                          className="rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12px] transition hover:bg-canvas disabled:opacity-50"
                        >
                          Cancel
                        </button>
                      ) : (
                        <span className="text-[11.5px] text-muted">—</span>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </section>

      {dialog === "clone" ? (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center bg-ink/40 p-4"
          onClick={() => {
            setDialog(null);
            setDiscard(null);
          }}
          data-env-reclone-overlay
        >
          <div
            role="dialog"
            aria-modal="true"
            aria-label="Confirm re-clone"
            data-env-reclone-dialog
            onClick={(event) => event.stopPropagation()}
            className="w-full max-w-md rounded-lg border border-line bg-panel p-5 shadow-lg"
          >
            <h3 className="text-sm font-semibold text-ink">Re-clone empties this environment</h3>
            <p className="mt-2 text-xs text-muted">
              A clone starts from an empty copy, so the staging rows below are discarded first.
              This is the work the environment exists to let you throw away — but only if you are
              finished with it.
            </p>
            <ul className="mt-3 flex flex-col gap-1 text-xs text-muted" data-env-reclone-discard>
              <li>{`${discard?.pages ?? 0} page${discard?.pages === 1 ? "" : "s"}`}</li>
              <li>{`${discard?.translations ?? 0} translation${discard?.translations === 1 ? "" : "s"}`}</li>
              <li>{`${discard?.workflows ?? 0} workflow${discard?.workflows === 1 ? "" : "s"}`}</li>
              <li>{`${discard?.settings ?? 0} setting${discard?.settings === 1 ? "" : "s"}`}</li>
            </ul>
            {discard !== null && discard.total === 0 ? (
              <p className="mt-2 text-xs text-muted">
                Nothing is at risk, so the confirmation is just the click.
              </p>
            ) : null}
            <div className="mt-4 flex items-center justify-end gap-2">
              <button
                type="button"
                onClick={() => {
                  setDialog(null);
                  setDiscard(null);
                }}
                data-env-reclone-cancel
                className="rounded-lg border border-line bg-surface px-3 py-2 text-[12.5px] transition hover:bg-canvas"
              >
                Keep this environment
              </button>
              <button
                type="button"
                onClick={() => void confirmReclone()}
                disabled={busy}
                data-env-reclone-confirm
                className="rounded-lg bg-accent px-3 py-2 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-50"
              >
                {busy ? "Queuing…" : "Discard and re-clone"}
              </button>
            </div>
          </div>
        </div>
      ) : null}

      {dialog === "archive" ? (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center bg-ink/40 p-4"
          onClick={() => setDialog(null)}
          data-env-archive-overlay
        >
          <div
            role="dialog"
            aria-modal="true"
            aria-label="Confirm archive"
            data-env-archive-dialog
            onClick={(event) => event.stopPropagation()}
            className="w-full max-w-md rounded-lg border border-line bg-panel p-5 shadow-lg"
          >
            <h3 className="text-sm font-semibold text-ink">Archive this environment?</h3>
            <p className="mt-2 text-xs text-muted">
              Its content is kept and it stops being served; the host it held is released so it
              can be pointed somewhere else. Archiving is reversible from the archived filter.
            </p>
            <div className="mt-4 flex items-center justify-end gap-2">
              <button
                type="button"
                onClick={() => setDialog(null)}
                data-env-archive-cancel
                className="rounded-lg border border-line bg-surface px-3 py-2 text-[12.5px] transition hover:bg-canvas"
              >
                Keep it
              </button>
              <button
                type="button"
                onClick={() => void onArchive()}
                disabled={busy}
                data-env-archive-confirm
                className="rounded-lg bg-accent px-3 py-2 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-50"
              >
                {busy ? "Archiving…" : "Archive"}
              </button>
            </div>
          </div>
        </div>
      ) : null}
    </div>
  );
}
