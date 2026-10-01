"use client";

/**
 * `/deployment/backfills/{id}` — one job, and the statement it runs (REQ-129, slice 3).
 *
 * ## Why the SQL is on this screen
 *
 * This is the only payload on the deployment surface that carries interpolated SQL, and the
 * question it answers is the one an operator asks before pressing Resume: **what is this actually
 * going to write?** The statement comes from a migration file, is validated as an identifier by the
 * crate, and addresses no literals — so it is a query, not a credential. Everything else on this
 * surface stays metadata-only.
 *
 * ## The cursor is the resume point, stated as such
 *
 * A batch reads `where <key> > $cursor`, so the cursor IS where a resume starts. The screen says
 * that in words rather than leaving it to be inferred from a column header, because "pause" and
 * "start over" are one misreading apart and the difference is a full re-run over every row.
 *
 * Keyboard: `r` refreshes, `Escape` returns to the list.
 */
import { useCallback, useEffect, useState } from "react";

import {
  AlertTriangle,
  ArrowLeft,
  Copy,
  Database,
  FileCode2,
  Gauge,
  Pause,
  Play,
  RefreshCw,
} from "lucide-react";
import Link from "next/link";
import { useRouter } from "next/navigation";

import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  pauseBackfill,
  readBackfill,
  resumeBackfill,
  runBackfillBatch,
  type BackfillDetail,
  type BackfillRun,
} from "@/lib/deployment-api";
import { formatTimestamp } from "@/lib/format";

export function BackfillDetailView({ id }: { id: string }) {
  const router = useRouter();
  const [detail, setDetail] = useState<BackfillDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  const load = useCallback(async () => {
    setError(null);
    try {
      setDetail(await readBackfill(id));
    } catch (caught) {
      setError(
        caught instanceof ApiError ? caught.message : "This backfill could not be loaded.",
      );
    } finally {
      setLoading(false);
    }
  }, [id]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      if (target?.tagName === "INPUT" || target?.tagName === "TEXTAREA") return;
      if (event.key === "r") {
        event.preventDefault();
        void load();
      } else if (event.key === "Escape") {
        router.push("/deployment/backfills");
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [load, router]);

  const act = useCallback(
    async (verb: string, work: () => Promise<BackfillRun | { job: BackfillDetail["job"] }>) => {
      setBusy(true);
      setNotice(null);
      try {
        const result = await work();
        const job = result.job;
        if (verb === "run") {
          const run = result as BackfillRun;
          setNotice(
            run.rows > 0
              ? `This request wrote ${run.rows.toLocaleString()} rows; the job is at ${job.rows_done.toLocaleString()} and the cursor is ${job.cursor_display}.`
              : `Nothing ran — the batch found no rows left to write.`,
          );
        } else if (verb === "resume") {
          const run = result as BackfillRun;
          const ran = run.batches ?? 0;
          const asked = run.requested_batches ?? ran;
          setNotice(
            asked > ran
              ? `You asked for ${asked} batches and ${ran} ran — that is the API's ceiling, not a failure. The cursor is now ${job.cursor_display}.`
              : `Ran ${ran} batch(es); the cursor is now ${job.cursor_display}.`,
          );
        } else {
          setNotice(
            `Paused with the cursor kept at ${job.cursor_display}. Resume continues from there.`,
          );
        }
        await load();
      } catch (caught) {
        setNotice(
          caught instanceof ApiError ? `${verb} refused: ${caught.message}` : `${verb} failed.`,
        );
        await load();
      } finally {
        setBusy(false);
      }
    },
    [load],
  );

  if (loading) return <LoadingTable columns={2} rows={4} />;

  if (error || !detail) {
    return (
      <div className="flex flex-col gap-3">
        <div
          role="alert"
          className="flex items-start gap-2 rounded-md border border-red-300 bg-red-50 px-3 py-2.5 text-[13px] text-red-900 dark:border-red-800 dark:bg-red-950/40 dark:text-red-200"
        >
          <AlertTriangle size={16} className="mt-0.5 shrink-0" aria-hidden />
          <span>
            {error ?? "This backfill is not here."}{" "}
            <button type="button" onClick={() => void load()} className="inline-flex underline">
              Try again
            </button>
          </span>
        </div>
        <Link href="/deployment/backfills" className="inline-flex items-center gap-1.5 text-[13px]">
          <ArrowLeft size={14} aria-hidden />
          Back to the backfills
        </Link>
      </div>
    );
  }

  const { job, descriptor } = detail;

  return (
    <div className="flex flex-col gap-5" data-view="deployment-backfill-detail">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <Link
          href="/deployment/backfills"
          className="inline-flex items-center gap-1.5 text-[13px] text-muted underline-offset-2 hover:underline"
        >
          <ArrowLeft size={14} aria-hidden />
          All backfills
          <kbd className="text-[10.5px]">Esc</kbd>
        </Link>
        <button
          type="button"
          onClick={() => void load()}
          className="inline-flex items-center gap-2 rounded-md border border-line px-3 py-2 text-sm"
        >
          <RefreshCw size={15} aria-hidden />
          Refresh
          <kbd className="text-[10.5px] text-muted">r</kbd>
        </button>
      </div>

      {notice ? (
        <p
          role="status"
          className="rounded-md border border-line bg-surface px-3 py-2 text-[12.5px]"
        >
          {notice}
        </p>
      ) : null}

      <header className="flex flex-col gap-1.5">
        <h2 className="text-[17px] font-medium">{job.name}</h2>
        <p className="flex flex-wrap items-center gap-x-3 gap-y-1 text-[12.5px] text-muted">
          <span className="inline-flex items-center gap-1">
            <Database size={13} aria-hidden />
            {job.table_name}.{job.column_name}
          </span>
          <span className="inline-flex items-center gap-1">
            <Gauge size={13} aria-hidden />
            {job.batch_size.toLocaleString()} rows per batch
          </span>
          <span>state: {job.state}</span>
        </p>
      </header>

      {job.last_error ? (
        <section
          aria-labelledby="error-heading"
          className="rounded-md border border-red-300 bg-red-50 p-3 dark:border-red-800 dark:bg-red-950/40"
        >
          <h3
            id="error-heading"
            className="flex items-center gap-2 text-[13px] font-medium text-red-900 dark:text-red-200"
          >
            <AlertTriangle size={15} aria-hidden />
            The statement failed
          </h3>
          <p className="mt-1 text-[12.5px] text-red-900/90 dark:text-red-200/90">
            This is the database&apos;s own message, kept verbatim. The statement below belongs to
            the migration author — fix it with a new migration, never by editing the job.
          </p>
          <pre className="mt-2 overflow-x-auto rounded bg-red-100/70 px-2 py-1.5 text-[11.5px] text-red-900 dark:bg-red-950/60 dark:text-red-200">
            {job.last_error}
          </pre>
        </section>
      ) : null}

      <section aria-labelledby="resume-heading" className="flex flex-col gap-2">
        <h3 id="resume-heading" className="text-[13px] font-medium">
          Where a resume continues from
        </h3>
        <p className="text-[12.5px] text-muted">
          A batch reads <code className="rounded bg-muted px-1 py-0.5 text-[11.5px]">
            {job.table_name}.{job.key_column} &gt; cursor
          </code>{" "}
          and stops after {job.batch_size.toLocaleString()} rows, so the cursor is the resume point.
          Pausing keeps it. It is the last key processed, compared in{" "}
          <code className="rounded bg-muted px-1 py-0.5 text-[11.5px]">
            {job.key_column}
          </code>
          &apos;s own type — never as text, which would sort &apos;99&apos; after &apos;100&apos; and
          restart the job below where it stopped.
        </p>
        <div className="grid gap-2 sm:grid-cols-3">
          <div className="rounded-md border border-line bg-surface p-3">
            <p className="text-[11.5px] text-muted">Cursor</p>
            <p className="mt-0.5 font-mono text-[14px]">{job.cursor_display}</p>
          </div>
          <div className="rounded-md border border-line bg-surface p-3">
            <p className="text-[11.5px] text-muted">Rows written</p>
            <p className="mt-0.5 text-[14px]">{job.rows_done.toLocaleString()}</p>
          </div>
          <div className="rounded-md border border-line bg-surface p-3">
            <p className="text-[11.5px] text-muted">Started</p>
            <p className="mt-0.5 text-[14px]">
              {job.started_at ? formatTimestamp(job.started_at) : "—"}
            </p>
          </div>
        </div>
      </section>

      <section aria-labelledby="statement-heading" className="flex flex-col gap-2">
        <div className="flex items-center justify-between gap-2">
          <h3 id="statement-heading" className="flex items-center gap-2 text-[13px] font-medium">
            <FileCode2 size={15} aria-hidden className="text-muted" />
            The statement the next batch runs
          </h3>
          {descriptor ? (
            <button
              type="button"
              onClick={() => {
                void navigator.clipboard?.writeText(descriptor.statement);
                setCopied(true);
                window.setTimeout(() => setCopied(false), 1500);
              }}
              className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
            >
              <Copy size={13} aria-hidden />
              {copied ? "Copied" : "Copy"}
            </button>
          ) : null}
        </div>
        {descriptor ? (
          <>
            <p className="text-[12.5px] text-muted">
              From migration {descriptor.version} · rate limit {descriptor.rate_limit_per_second}{" "}
              rows/second. This is a query from a migration file, not a credential — it addresses no
              values.
            </p>
            <pre className="overflow-x-auto rounded-md border border-line bg-surface p-3 font-mono text-[12px]">
              {descriptor.statement}
            </pre>
          </>
        ) : (
          <p className="rounded-md border border-line bg-surface px-3 py-2.5 text-[12.5px] text-muted">
            The descriptor row is gone, so this job cannot be re-run: there is no statement behind
            it any more. The rows it already wrote stay written.
          </p>
        )}
      </section>

      <section aria-labelledby="actions-heading" className="flex flex-col gap-2">
        <h3 id="actions-heading" className="text-[13px] font-medium">
          Actions
        </h3>
        <div className="flex flex-wrap gap-2">
          {job.can_pause ? (
            <button
              type="button"
              disabled={busy}
              onClick={() => void act("pause", () => pauseBackfill(job.id))}
              className="inline-flex items-center gap-2 rounded-md border border-line px-3 py-2 text-[13px] disabled:opacity-60"
            >
              <Pause size={14} aria-hidden />
              Pause, keeping the cursor
            </button>
          ) : null}
          {job.can_resume ? (
            <button
              type="button"
              disabled={busy}
              onClick={() => void act("resume", () => resumeBackfill(job.id, 1))}
              className="inline-flex items-center gap-2 rounded-md border border-line px-3 py-2 text-[13px] disabled:opacity-60"
            >
              <Play size={14} aria-hidden />
              Resume one batch from {job.cursor_display}
            </button>
          ) : null}
          {job.state === "running" ? (
            <button
              type="button"
              disabled={busy}
              onClick={() => void act("run", () => runBackfillBatch(job.id))}
              className="inline-flex items-center gap-2 rounded-md border border-line px-3 py-2 text-[13px] disabled:opacity-60"
            >
              <RefreshCw size={14} aria-hidden />
              Run one batch
            </button>
          ) : null}
          {!job.can_pause && !job.can_resume && job.state !== "running" ? (
            <p className="text-[12.5px] text-muted">
              This job is completed. A completed backfill cannot be resumed — it has nothing left
              to write, and starting it again would record rows that were never written.
            </p>
          ) : null}
        </div>
      </section>
    </div>
  );
}
