"use client";

/**
 * `/deployment/migrations` — the ledger, the pending set and the lock (REQ-129, slice 1).
 *
 * ## The one distinction the layout exists to keep
 *
 * **Applied and pending are different facts from different places.** A pending migration has no
 * ledger row *by definition* — it is what is left — so a table that mixed both and printed one
 * "applied at" column would show a timestamp for a migration that has never run. So the pending
 * rows are a separate band above the ledger, each with a `would run` badge, and neither renders
 * the other's columns.
 *
 * ## `down_verified_at: null` is the honest answer, not a missing feature
 *
 * A migration whose reversal has never been rehearsed renders `never rehearsed` and never
 * `reversible`. The difference between the two is the difference between "take the backup" and
 * "you may not need it", and this screen never collapses them — a green tick nobody earned is
 * exactly the claim the request is written against.
 *
 * ## Drift is loud
 *
 * A row whose checksum disagrees with the ledger is rendered with BOTH hashes and the sentence
 * that fixes it. The fix is always a new migration; the screen says so rather than offering a
 * button that re-blesses the file, because re-blessing destroys the only evidence that the schema
 * on disk and the schema in the database were ever the same.
 *
 * Keyboard: `/` focuses the filter, `r` refreshes, `a` opens the plan preview, `g` jumps to the
 * gate verdict, `Esc` clears the filter. Under `sm:` the table becomes cards.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  AlertTriangle,
  CheckCircle2,
  CircleDashed,
  Clock,
  Copy,
  FileCode2,
  KeyRound,
  Lock,
  RefreshCw,
  Search,
  ShieldAlert,
  Unlock,
} from "lucide-react";
import Link from "next/link";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  applyMigrations,
  fetchMigrationLedger,
  previewMigrationPlan,
  type AppliedMigration,
  type MigrationLedger,
  type MigrationPlan,
  type PendingMigration,
} from "@/lib/deployment-api";
import { formatTimestamp } from "@/lib/format";

/** The gate verdict's vocabulary, with the wording an operator acts on. */
function gateWording(ledger: MigrationLedger): { tone: string; text: string } {
  if (ledger.drift.length > 0) {
    return {
      tone: "bad",
      text: `${ledger.drift.length} applied migration(s) no longer match their file — fix it with a new migration, never by re-blessing the file`,
    };
  }
  if (ledger.gate_fails) {
    return {
      tone: "bad",
      text: `${ledger.missing_down.length} pending migration(s) ship without a reversal — the CI gate refuses them until each has a waiver with a reason`,
    };
  }
  if (ledger.pending.length === 0) {
    return { tone: "good", text: "the database is up to date" };
  }
  return {
    tone: "warn",
    text: `${ledger.pending.length} migration(s) would run, and the gate passes`,
  };
}

function riskWording(risk: string): { tone: string; text: string } {
  switch (risk) {
    case "rewrites_table":
      return { tone: "bad", text: "rewrites the table under a write lock" };
    case "brief":
      return { tone: "warn", text: "brief exclusive lock" };
    default:
      return { tone: "good", text: "no write lock" };
  }
}

export function MigrationsView() {
  const [ledger, setLedger] = useState<MigrationLedger | null>(null);
  const [plan, setPlan] = useState<MigrationPlan | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const [copied, setCopied] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const searchRef = useRef<HTMLInputElement | null>(null);

  const load = useCallback(async () => {
    setError(null);
    try {
      setLedger(await fetchMigrationLedger());
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : String(cause));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // Keyboard. Bound on the window rather than the table so `/` works from anywhere on the screen,
  // which is the whole reason the request asks for it.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target &&
        (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.isContentEditable);
      if (event.key === "Escape") {
        setFilter("");
        setPlan(null);
        return;
      }
      if (typing) return;
      if (event.key === "/") {
        event.preventDefault();
        searchRef.current?.focus();
      }
      if (event.key === "r") void load();
      if (event.key === "a") void runPlan();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [load]);

  const runPlan = useCallback(async () => {
    setBusy(true);
    setNotice(null);
    setError(null);
    try {
      setPlan(await previewMigrationPlan());
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : String(cause));
    } finally {
      setBusy(false);
    }
  }, []);

  const runApply = useCallback(async () => {
    setBusy(true);
    setNotice(null);
    setError(null);
    try {
      const report = await applyMigrations();
      setNotice(report.summary);
      await load();
      await runPlan();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : String(cause));
    } finally {
      setBusy(false);
    }
  }, [load, runPlan]);

  const needle = filter.trim().toLowerCase();
  // The source is read under its own name rather than destructured inline: naming the fallback
  // `ledger` shadowed the state variable inside this callback, so the reference resolved to the
  // `const` being declared — which `tsc` catches as TS2448 and a reader as nothing at all.
  const source = ledger;
  const matches = useMemo(() => {
    const hit = (version: string, name: string) =>
      !needle || version.includes(needle) || name.toLowerCase().includes(needle);
    const applied = source?.applied ?? [];
    const pending = source?.pending ?? [];
    return {
      applied: applied.filter((row) => hit(row.version, row.name)),
      pending: pending.filter((row) => hit(row.version, row.name)),
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [source, needle]);

  const copy = async (value: string, key: string) => {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(key);
      window.setTimeout(() => setCopied(null), 1200);
    } catch {
      setNotice("this browser refused the clipboard — the full hash is in the row");
    }
  };

  const gate = ledger ? gateWording(ledger) : { tone: "warn", text: "reading the ledger…" };

  if (loading) return <LoadingTable columns={8} rows={6} />;

  if (error && !ledger) {
    return (
      <div className="space-y-4">
        <div
          role="alert"
          className="rounded-lg border border-red-300 bg-red-50 p-4 text-sm text-red-900 dark:border-red-900 dark:bg-red-950/40 dark:text-red-100"
        >
          <p className="font-medium">The migration ledger could not be read.</p>
          <p className="mt-1">{error}</p>
          <button
            type="button"
            onClick={() => void load()}
            className="mt-3 rounded-md border border-red-400 px-3 py-1.5 text-sm"
          >
            Try again
          </button>
        </div>
      </div>
    );
  }

  if (!ledger) return null;

  return (
    <div className="space-y-6" data-view="deployment-migrations">
      {/* ---- the lock, the gate and the drift: the three answers an operator opens this screen for */}
      <div className="grid gap-3 sm:grid-cols-3">
        <div className="rounded-lg border border-line p-4">
          <p className="flex items-center gap-2 text-xs font-medium uppercase tracking-wide text-muted">
            {ledger.lock.held ? <Lock className="h-4 w-4" /> : <Unlock className="h-4 w-4" />}
            Migration lock
          </p>
          {ledger.lock.held ? (
            <>
              <p className="mt-1 text-sm font-medium">
                {ledger.lock.version} is running ({ledger.lock.direction})
              </p>
              <p className="mt-1 text-xs text-muted">
                {ledger.lock.age_seconds}s · by {ledger.lock.actor} from {ledger.lock.source}
              </p>
            </>
          ) : (
            <p className="mt-1 text-sm">Free — no run in flight</p>
          )}
          <p className="mt-2 font-mono text-xs text-muted">{ledger.lock.lock_key}</p>
          {ledger.lock.blocked.length > 0 && (
            <ul className="mt-2 space-y-1 text-xs text-amber-700 dark:text-amber-300">
              {ledger.lock.blocked.map((query) => (
                <li key={query.pid}>
                  pid {query.pid} waiting {query.age_seconds}s — {query.query.slice(0, 60)}
                </li>
              ))}
            </ul>
          )}
        </div>

        <div className="rounded-lg border border-line p-4">
          <p className="flex items-center gap-2 text-xs font-medium uppercase tracking-wide text-muted">
            {gate.tone === "good" ? (
              <CheckCircle2 className="h-4 w-4" />
            ) : (
              <ShieldAlert className="h-4 w-4" />
            )}
            CI gate
          </p>
          <p className="mt-1 text-sm" data-testid="gate-verdict">
            {gate.text}
          </p>
          <p className="mt-1 text-xs text-muted">{ledger.summary}</p>
        </div>

        <div className="rounded-lg border border-line p-4">
          <p className="flex items-center gap-2 text-xs font-medium uppercase tracking-wide text-muted">
            <Clock className="h-4 w-4" />
            Timeouts
          </p>
          <p className="mt-1 text-sm">
            lock {ledger.policy.lock_timeout_ms}ms · statement{" "}
            {ledger.policy.statement_timeout_ms}ms
          </p>
          <p className="mt-1 text-xs text-muted">
            Backfill batches of {ledger.policy.backfill_batch_size} at{" "}
            {ledger.policy.backfill_rate_per_second}/s
          </p>
        </div>
      </div>

      {ledger.drift.length > 0 && (
        <div
          role="alert"
          className="rounded-lg border border-red-300 bg-red-50 p-4 text-sm dark:border-red-900 dark:bg-red-950/40"
        >
          <p className="flex items-center gap-2 font-medium text-red-900 dark:text-red-100">
            <AlertTriangle className="h-4 w-4" />
            {ledger.drift.length} applied migration(s) no longer match their file
          </p>
          <ul className="mt-2 space-y-2">
            {ledger.drift.map((drift) => (
              <li key={drift.version} className="text-xs">
                <span className="font-mono font-medium">{drift.version}</span>{" "}
                {drift.name} — the ledger recorded{" "}
                <span className="font-mono">{drift.recorded.slice(0, 12)}</span>, the file now hashes to{" "}
                <span className="font-mono">{drift.current.slice(0, 12)}</span>. An applied migration
                is immutable: the fix is a new migration, never an edit to this file.
              </li>
            ))}
          </ul>
        </div>
      )}

      {notice && (
        <p
          role="status"
          className="rounded-lg border border-line bg-muted/40 p-3 text-sm"
          data-testid="apply-notice"
        >
          {notice}
        </p>
      )}
      {error && ledger && (
        <p role="alert" className="rounded-lg border border-red-300 p-3 text-sm text-red-800 dark:border-red-800 dark:text-red-200">
          {error}
        </p>
      )}

      <div className="flex flex-wrap items-center gap-2">
        <label className="sr-only" htmlFor="migration-filter">
          Filter migrations
        </label>
        <div className="relative flex-1 min-w-[12rem]">
          <Search className="pointer-events-none absolute left-2.5 top-2.5 h-4 w-4 text-muted" />
          <input
            id="migration-filter"
            ref={searchRef}
            value={filter}
            onChange={(event) => setFilter(event.target.value)}
            placeholder="Filter by version or name  ( / )"
            className="w-full rounded-md border border-line bg-transparent py-2 pl-8 pr-3 text-sm"
            data-testid="migration-filter"
          />
        </div>
        <button
          type="button"
          onClick={() => void runPlan()}
          disabled={busy}
          className="inline-flex items-center gap-2 rounded-md border border-line px-3 py-2 text-sm disabled:opacity-60"
          data-testid="plan-preview"
        >
          <FileCode2 className="h-4 w-4" />
          Preview what would run
        </button>
        <button
          type="button"
          onClick={() => void runApply()}
          disabled={busy || ledger.pending.length === 0}
          className="inline-flex items-center gap-2 rounded-md bg-red-600 px-3 py-2 text-sm text-white disabled:opacity-60"
          data-testid="apply-migrations"
        >
          <KeyRound className="h-4 w-4" />
          Apply pending
        </button>
        <button
          type="button"
          onClick={() => void load()}
          className="inline-flex items-center gap-2 rounded-md border border-line px-3 py-2 text-sm"
          data-testid="refresh-ledger"
        >
          <RefreshCw className="h-4 w-4" />
          Refresh
        </button>
      </div>

      {/* ---- pending: the band above the ledger, with no "applied at" column because there is none */}
      <section aria-labelledby="pending-heading" className="space-y-2" data-pending-band>
        <h2 id="pending-heading" className="flex items-center gap-2 text-sm font-semibold">
          <CircleDashed className="h-4 w-4" />
          Pending
          <span className="rounded-full bg-muted px-2 py-0.5 text-xs font-normal">
            {matches.pending.length}
          </span>
        </h2>
        {matches.pending.length === 0 ? (
          <EmptyState
            title={ledger.pending.length === 0 ? "Nothing pending" : "No pending migration matches"}
            hint={
              ledger.pending.length === 0
                ? "This binary ships no migration the database has not applied. The ledger below is the whole history."
                : "Clear the filter to see the rest of the pending set."
            }
          />
        ) : (
          <>
            {/* md: the table. The "would run" badge is the request's word for this band. */}
            <div className="hidden overflow-x-auto md:block">
              <table className="w-full min-w-[46rem] text-left text-sm">
                <thead className="text-xs uppercase tracking-wide text-muted">
                  <tr>
                    <th scope="col" className="py-2 pr-3">Version</th>
                    <th scope="col" className="py-2 pr-3">Name</th>
                    <th scope="col" className="py-2 pr-3">State</th>
                    <th scope="col" className="py-2 pr-3">Statements</th>
                    <th scope="col" className="py-2 pr-3">Lock risk</th>
                    <th scope="col" className="py-2 pr-3">Reversal</th>
                    <th scope="col" className="py-2">Checksum</th>
                  </tr>
                </thead>
                <tbody>
                  {matches.pending.map((row: PendingMigration) => (
                    <tr key={row.version} className="border-t border-line" data-migration-row data-state="pending">
                      <td className="py-2 pr-3 font-mono">
                        <Link className="underline" href={`/deployment/migrations/${row.version}`}>
                          {row.version}
                        </Link>
                      </td>
                      <td className="py-2 pr-3">{row.name}</td>
                      <td className="py-2 pr-3">
                        <span className="rounded-full bg-blue-100 px-2 py-0.5 text-xs text-blue-900 dark:bg-blue-900/40 dark:text-blue-100">
                          would run
                        </span>
                      </td>
                      <td className="py-2 pr-3">{row.statement_count}</td>
                      <td className="py-2 pr-3">{riskWording(row.lock_risk).text}</td>
                      <td className="py-2 pr-3">
                        {row.has_down ? (
                          <span className="text-xs">present</span>
                        ) : row.declared_no_down ? (
                          <span className="text-xs text-muted">declared irreversible</span>
                        ) : (
                          <span className="text-xs text-red-700 dark:text-red-300">absent</span>
                        )}
                      </td>
                      <td className="py-2 font-mono text-xs">
                        <button
                          type="button"
                          onClick={() => void copy(row.checksum, row.version)}
                          className="inline-flex items-center gap-1 underline"
                        >
                          {row.checksum.slice(0, 12)}
                          {copied === row.version ? (
                            <CheckCircle2 className="h-3 w-3" />
                          ) : (
                            <Copy className="h-3 w-3" />
                          )}
                        </button>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>

            {/* sm: the same rows as cards. */}
            <ul className="space-y-3 md:hidden">
              {matches.pending.map((row: PendingMigration) => (
                <li key={row.version} className="rounded-lg border border-line p-3 text-sm">
                  <div className="flex items-center justify-between gap-2">
                    <Link className="font-mono underline" href={`/deployment/migrations/${row.version}`}>
                      {row.version}
                    </Link>
                    <span className="rounded-full bg-blue-100 px-2 py-0.5 text-xs text-blue-900 dark:bg-blue-900/40 dark:text-blue-100">
                      would run
                    </span>
                  </div>
                  <p className="mt-1">{row.name}</p>
                  <dl className="mt-2 grid grid-cols-2 gap-x-3 gap-y-1 text-xs">
                    <dt className="text-muted">Statements</dt>
                    <dd>{row.statement_count}</dd>
                    <dt className="text-muted">Lock risk</dt>
                    <dd>{riskWording(row.lock_risk).text}</dd>
                    <dt className="text-muted">Reversal</dt>
                    <dd>{row.has_down ? "present" : "absent"}</dd>
                    <dt className="text-muted">Checksum</dt>
                    <dd className="font-mono">{row.checksum.slice(0, 12)}</dd>
                  </dl>
                </li>
              ))}
            </ul>
          </>
        )}
      </section>

      {/* ---- the plan preview: statements, timeouts, violations, and nothing executed */}
      {plan && (
        <section
          aria-labelledby="plan-heading"
          className="space-y-2 rounded-lg border border-line p-4"
          data-testid="plan-preview-panel"
        >
          <h2 id="plan-heading" className="text-sm font-semibold">
            Plan — nothing above was executed
          </h2>
          <p className="text-sm">{plan.summary}</p>
          <p className="text-xs text-muted">
            lock_timeout {plan.policy.lock_timeout_ms}ms · statement_timeout{" "}
            {plan.policy.statement_timeout_ms}ms · backfill batch{" "}
            {plan.policy.backfill_batch_size} at {plan.policy.backfill_rate_per_second}/s
          </p>
          {plan.violations.length > 0 && (
            <ul className="mt-2 space-y-1 text-xs">
              {plan.violations.slice(0, 12).map((finding) => (
                <li key={`${finding.version}:${finding.pattern}:${finding.line}`}>
                  <span className="font-mono">{finding.version}</span> line {finding.line}:{" "}
                  <span className="font-medium">{finding.pattern}</span> — {finding.excerpt.slice(0, 80)}
                </li>
              ))}
            </ul>
          )}
        </section>
      )}

      {/* ---- the ledger itself */}
      <section aria-labelledby="ledger-heading" className="space-y-2" data-ledger-band>
        <h2 id="ledger-heading" className="flex items-center gap-2 text-sm font-semibold">
          Ledger
          <span className="rounded-full bg-muted px-2 py-0.5 text-xs font-normal">
            {matches.applied.length}
          </span>
        </h2>
        {matches.applied.length === 0 ? (
          <EmptyState
            title={
              ledger.applied.length === 0
                ? "The ledger is empty"
                : "No applied migration matches"
            }
            hint={
              ledger.applied.length === 0
                ? "No migration this installation ran has a ledger row. `omnion migrate status` says whether the database has any history at all."
                : "Clear the filter to see the whole history."
            }
          />
        ) : (
          <>
            <div className="hidden overflow-x-auto md:block">
              <table className="w-full min-w-[56rem] text-left text-sm">
                <thead className="text-xs uppercase tracking-wide text-muted">
                  <tr>
                    <th scope="col" className="py-2 pr-3">Version</th>
                    <th scope="col" className="py-2 pr-3">Name</th>
                    <th scope="col" className="py-2 pr-3">Applied</th>
                    <th scope="col" className="py-2 pr-3">Duration</th>
                    <th scope="col" className="py-2 pr-3">Checksum</th>
                    <th scope="col" className="py-2 pr-3">Reversal</th>
                    <th scope="col" className="py-2 pr-3">Actor</th>
                    <th scope="col" className="py-2">Source</th>
                  </tr>
                </thead>
                <tbody>
                  {matches.applied.map((row: AppliedMigration) => (
                    <tr key={row.version} className="border-t border-line" data-migration-row data-state="applied">
                      <td className="py-2 pr-3 font-mono">
                        <Link className="underline" href={`/deployment/migrations/${row.version}`}>
                          {row.version}
                        </Link>
                      </td>
                      <td className="py-2 pr-3">{row.name}</td>
                      <td className="py-2 pr-3">{formatTimestamp(row.applied_at)}</td>
                      <td className="py-2 pr-3">{row.duration_ms}ms</td>
                      <td className="py-2 pr-3 font-mono text-xs">{row.checksum.slice(0, 12)}</td>
                      <td className="py-2 pr-3">
                        {row.down_verified_at ? (
                          <span
                            className="inline-flex items-center gap-1 text-xs text-emerald-700 dark:text-emerald-300"
                            title={`rehearsed by ${row.down_verified_by}`}
                          >
                            <CheckCircle2 className="h-3 w-3" />
                            rehearsed {formatTimestamp(row.down_verified_at)}
                          </span>
                        ) : row.has_down ? (
                          <span className="text-xs text-muted">never rehearsed</span>
                        ) : (
                          <span className="text-xs text-amber-700 dark:text-amber-300">
                            no reversal in file
                          </span>
                        )}
                      </td>
                      <td className="py-2 pr-3 text-xs">{row.actor}</td>
                      <td className="py-2 text-xs">{row.source}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>

            <ul className="space-y-3 md:hidden">
              {matches.applied.map((row: AppliedMigration) => (
                <li key={row.version} className="rounded-lg border border-line p-3 text-sm">
                  <div className="flex items-center justify-between gap-2">
                    <Link className="font-mono underline" href={`/deployment/migrations/${row.version}`}>
                      {row.version}
                    </Link>
                    <span className="text-xs text-muted">{row.source}</span>
                  </div>
                  <p className="mt-1">{row.name}</p>
                  <dl className="mt-2 grid grid-cols-2 gap-x-3 gap-y-1 text-xs">
                    <dt className="text-muted">Applied</dt>
                    <dd>{formatTimestamp(row.applied_at)}</dd>
                    <dt className="text-muted">Duration</dt>
                    <dd>{row.duration_ms}ms</dd>
                    <dt className="text-muted">Checksum</dt>
                    <dd className="font-mono">{row.checksum.slice(0, 12)}</dd>
                    <dt className="text-muted">Reversal</dt>
                    <dd>
                      {row.down_verified_at
                        ? "rehearsed"
                        : row.has_down
                          ? "never rehearsed"
                          : "none in file"}
                    </dd>
                    <dt className="text-muted">Actor</dt>
                    <dd className="truncate">{row.actor}</dd>
                  </dl>
                </li>
              ))}
            </ul>
          </>
        )}
      </section>
    </div>
  );
}
