"use client";

/**
 * `/deployment/migrations/{version}` — one migration: its SQL, its reversal, and the rehearsal
 * (REQ-129, slice 1).
 *
 * ## The rehearsal is the only button here, and it is deliberately awkward
 *
 * `Verify down` needs a **scratch database name**, typed by an operator who understands that the
 * reversal runs against a throwaway copy. That is not a usability accident: the request's rule is
 * absolute — the panel never offers the reversal on a production-marked environment, and the
 * route refuses a name that is the live database. A one-click "verify" would have to pick the name
 * itself, and every name it could pick is either shared (a rehearsal that collides with a
 * colleague) or live.
 *
 * ## A reversal written as prose is reported as absent
 *
 * 53 of this repository's 58 migrations carry their `down` block as COMMENTED statements, and the
 * detail screen says `rehearsable` only when the file contains statements this runner can execute.
 * That is the difference between "the database can be rolled back" and "somebody wrote about
 * rolling it back", and the screen never shows the second as the first.
 *
 * Keyboard: `v` rehearses, `r` refreshes, `Esc` closes the rehearsal panel.
 */
import { useCallback, useEffect, useState } from "react";

import {
  ArrowLeft,
  CheckCircle2,
  CircleAlert,
  FileCode2,
  Play,
  RefreshCw,
  RotateCcw,
} from "lucide-react";
import Link from "next/link";
import { useParams } from "next/navigation";

import { EmptyState } from "@/components/empty-state";
import {
  ApiError,
  fetchMigration,
  rehearseReversal,
  type MigrationDetail,
  type VerifyReport,
} from "@/lib/deployment-api";
import { formatTimestamp } from "@/lib/format";

export function MigrationDetailView() {
  const params = useParams<{ version: string }>();
  const version = String(params?.version ?? "");
  const [detail, setDetail] = useState<MigrationDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [scratch, setScratch] = useState("");
  const [busy, setBusy] = useState(false);
  const [report, setReport] = useState<VerifyReport | null>(null);

  const load = useCallback(async () => {
    setError(null);
    try {
      setDetail(await fetchMigration(version));
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : String(cause));
    } finally {
      setLoading(false);
    }
  }, [version]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        setReport(null);
        setError(null);
        return;
      }
      const target = event.target as HTMLElement | null;
      const typing =
        target &&
        (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.isContentEditable);
      if (typing) return;
      if (event.key === "r") void load();
      if (event.key === "v") {
        document.getElementById("scratch-name")?.focus();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [load]);

  const rehearse = async () => {
    const name = scratch.trim();
    if (!name) {
      setError(
        "name the throwaway database to rehearse against — the rehearsal drops and recreates it",
      );
      return;
    }
    setBusy(true);
    setError(null);
    setReport(null);
    try {
      const answer = await rehearseReversal(version, name);
      setReport(answer.report);
      await load();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : String(cause));
    } finally {
      setBusy(false);
    }
  };

  if (loading) return <p className="text-sm text-muted">Reading migration {version}…</p>;

  if (!detail) {
    return (
      <div className="space-y-4">
        <Link href="/deployment/migrations" className="inline-flex items-center gap-1 text-sm underline">
          <ArrowLeft className="h-4 w-4" />
          Back to the ledger
        </Link>
        <div role="alert" className="rounded-lg border border-red-300 p-4 text-sm dark:border-red-800">
          <p className="font-medium">Migration {version} could not be read.</p>
          <p className="mt-1">{error}</p>
        </div>
      </div>
    );
  }

  const rehearsable = detail.down_statements.length > 0;

  return (
    <div className="space-y-6">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <Link href="/deployment/migrations" className="inline-flex items-center gap-1 text-sm underline">
          <ArrowLeft className="h-4 w-4" />
          Back to the ledger
        </Link>
        <button
          type="button"
          onClick={() => void load()}
          className="inline-flex items-center gap-2 rounded-md border border-line px-3 py-2 text-sm"
        >
          <RefreshCw className="h-4 w-4" />
          Refresh
        </button>
      </div>

      <header className="space-y-1">
        <h2 className="font-mono text-xl font-semibold">
          {detail.version} · {detail.name}
        </h2>
        <p className="text-sm text-muted">{detail.filename}</p>
        <div className="flex flex-wrap items-center gap-2 text-xs">
          <span className="rounded-full bg-muted px-2 py-0.5">
            {detail.state === "applied" ? "applied" : "would run"}
          </span>
          {detail.statement_count !== null && (
            <span className="rounded-full bg-muted px-2 py-0.5">
              {detail.statement_count} statements
            </span>
          )}
          {rehearsable ? (
            <span className="rounded-full bg-emerald-100 px-2 py-0.5 text-emerald-900 dark:bg-emerald-900/40 dark:text-emerald-100">
              rehearsable
            </span>
          ) : (
            <span className="rounded-full bg-amber-100 px-2 py-0.5 text-amber-900 dark:bg-amber-900/40 dark:text-amber-100">
              {detail.declared_no_down ? "declared irreversible" : "no executable reversal"}
            </span>
          )}
        </div>
      </header>

      {error && (
        <p role="alert" className="rounded-lg border border-red-300 p-3 text-sm text-red-800 dark:border-red-800 dark:text-red-200" data-testid="rehearsal-error">
          {error}
        </p>
      )}

      <div className="grid gap-4 lg:grid-cols-2">
        <section aria-labelledby="sql-heading" className="space-y-2">
          <h3 id="sql-heading" className="flex items-center gap-2 text-sm font-semibold">
            <FileCode2 className="h-4 w-4" />
            Up script
          </h3>
          <ol className="space-y-2">
            {detail.statements.map((statement, index) => (
              <li key={index}>
                <p className="mb-1 text-xs text-muted">statement {index + 1}</p>
                {/* The pane scrolls HORIZONTALLY inside its own region rather than wrapping the
                    SQL: a wrapped `drop table` is harder to scan for the exact shape that matters
                    than a scrolled one, and the request asks for a scrollable pane. */}
                <pre className="max-h-56 overflow-x-auto rounded-md border border-line bg-muted/30 p-3 text-xs">
                  {statement}
                </pre>
              </li>
            ))}
          </ol>
        </section>

        <section aria-labelledby="down-heading" className="space-y-2">
          <h3 id="down-heading" className="flex items-center gap-2 text-sm font-semibold">
            <RotateCcw className="h-4 w-4" />
            Down script
          </h3>
          {rehearsable ? (
            <ol className="space-y-2">
              {detail.down_statements.map((statement, index) => (
                <li key={index}>
                  <p className="mb-1 text-xs text-muted">reversal {index + 1}</p>
                  <pre className="max-h-56 overflow-x-auto rounded-md border border-line bg-muted/30 p-3 text-xs">
                    {statement}
                  </pre>
                </li>
              ))}
            </ol>
          ) : (
            <EmptyState
              title="This file has no executable reversal"
              hint="A down block written as prose or as commented statements is not something the runner can execute, so this screen will not call it reversible. The only fix is a real reversal in a new migration — an applied file is immutable."
            />
          )}
        </section>
      </div>

      <section aria-labelledby="rehearse-heading" className="space-y-3 rounded-lg border border-line p-4">
        <h3 id="rehearse-heading" className="flex items-center gap-2 text-sm font-semibold">
          <Play className="h-4 w-4" />
          Rehearse the reversal
        </h3>
        <p className="text-sm text-muted">
          The reversal runs against a throwaway copy of the schema on this server, never against
          the live database. The name is yours because every name this screen could pick itself is
          either shared or live.
        </p>
        <div className="flex flex-wrap items-end gap-2">
          <div className="flex-1 min-w-[14rem]">
            <label htmlFor="scratch-name" className="block text-xs font-medium">
              Scratch database name
            </label>
            <input
              id="scratch-name"
              value={scratch}
              onChange={(event) => setScratch(event.target.value)}
              placeholder="omnion_scratch_0207"
              className="mt-1 w-full rounded-md border border-line bg-transparent px-3 py-2 text-sm"
              data-testid="scratch-name"
            />
          </div>
          <button
            type="button"
            onClick={() => void rehearse()}
            disabled={busy || !rehearsable}
            className="inline-flex items-center gap-2 rounded-md bg-red-600 px-3 py-2 text-sm text-white disabled:opacity-60"
            data-testid="rehearse-reversal"
          >
            <Play className="h-4 w-4" />
            {busy ? "Rehearsing…" : "Verify down"}
          </button>
        </div>

        {report && (
          <div
            className="space-y-2 rounded-md border border-line p-3 text-sm"
            data-testid="rehearsal-report"
          >
            <p className="flex items-center gap-2 font-medium">
              {report.restored ? (
                <CheckCircle2 className="h-4 w-4 text-emerald-600" />
              ) : (
                <CircleAlert className="h-4 w-4 text-red-600" />
              )}
              {report.restored
                ? "the reversal ran and the structure came back exactly"
                : "the reversal ran but the structure did not come back exactly"}
            </p>
            <p className="text-xs text-muted">
              {report.statements} statement(s) in {report.duration_ms}ms
            </p>
            <details className="text-xs">
              <summary className="cursor-pointer">tables before / after</summary>
              <p className="mt-1">{report.tables_before.join(", ")}</p>
              <p>{report.tables_after.join(", ")}</p>
            </details>
          </div>
        )}
      </section>

      <section aria-labelledby="history-heading" className="space-y-2">
        <h3 id="history-heading" className="text-sm font-semibold">
          Run history
        </h3>
        {detail.runs.length === 0 ? (
          <EmptyState
            title="No run recorded"
            hint="A row appears here the moment a runner starts — a row is written before the first statement, so a crashed run leaves evidence rather than nothing."
          />
        ) : (
          <ul className="space-y-2">
            {detail.runs.map((run) => (
              <li key={run.id} className="rounded-md border border-line p-3 text-sm">
                <div className="flex flex-wrap items-center justify-between gap-2">
                  <span className="font-medium">
                    {run.direction} · {run.status}
                  </span>
                  <span className="text-xs text-muted">{formatTimestamp(run.started_at)}</span>
                </div>
                <p className="mt-1 text-xs text-muted">by {run.actor}</p>
              </li>
            ))}
          </ul>
        )}
      </section>

      {detail.ledger && (
        <section aria-labelledby="ledger-heading" className="space-y-1 text-sm">
          <h3 id="ledger-heading" className="text-sm font-semibold">
            Ledger row
          </h3>
          <p className="text-xs text-muted">
            applied {formatTimestamp(detail.ledger.applied_at)} by {detail.ledger.actor} from{" "}
            {detail.ledger.source} in {detail.ledger.duration_ms}ms
          </p>
          <p className="font-mono text-xs">{detail.ledger.checksum}</p>
        </section>
      )}
    </div>
  );
}
