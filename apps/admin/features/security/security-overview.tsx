"use client";

/**
 * `/security` — the posture overview (REQ-012, slice 1).
 *
 * This screen makes a claim about the platform, and the whole design is about not making one
 * it cannot support. Four rules, each one a way a security dashboard lies:
 *
 * 1. **A check that has never run is a row, not a gap.** The server returns one row per
 *    registered check, including the ones with no stored result, and those render as
 *    "Not checked yet" with the run action next to them. A screen that listed only the checks
 *    that had run would make an unevaluated platform look like a small one.
 * 2. **`unknown` is a first-class badge with its own colour and its own sentence.** It is not
 *    grey-muted-and-therefore-invisible next to a green row, because the difference between
 *    "verified" and "not looked at" is the entire product.
 * 3. **The score is the server's number.** The client does not recompute it, and the ring's
 *    legend is the server's `summary` — a client that added up its own rows would disagree
 *    with the score the moment the registry and the stored results diverged, which is exactly
 *    what happens before the first run.
 * 4. **The detail drawer renders named fields, never raw JSON.** An operator reading a `warn`
 *    needs the sentence and the fact under it, not a blob — and a raw dump is where a
 *    mis-shaped detail turns into a leaked one.
 *
 * The rows are keyboard-reachable and the `/` key focuses the filter box on the findings tab;
 * this tab's own action is the run button, which is the first focusable control.
 */
import { useCallback, useEffect, useState } from "react";
import Link from "next/link";
import { Loader2, PlayCircle, RefreshCw, ShieldAlert, ShieldCheck, TriangleAlert } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import {
  fetchSecurityOverview,
  runSecurityChecks,
  type ApiError,
} from "@/lib/api";
import type { SecurityCheck, SecurityCheckState, SecurityOverview } from "@/lib/types";

/** The four states and how each one reads. The label is never the colour alone. */
const STATE_LABEL: Record<SecurityCheckState, string> = {
  pass: "Verified",
  warn: "Needs attention",
  fail: "Failing",
  unknown: "Not checked yet",
};

const STATE_CLASS: Record<SecurityCheckState, string> = {
  pass: "bg-emerald-100 text-emerald-900 dark:bg-emerald-950 dark:text-emerald-200",
  warn: "bg-amber-100 text-amber-900 dark:bg-amber-950 dark:text-amber-200",
  fail: "bg-red-100 text-red-900 dark:bg-red-950 dark:text-red-200",
  // Deliberately *not* muted. A row we could not verify is a question the operator has to
  // see; rendering it in the same grey as secondary text is how it stops being one.
  unknown: "bg-slate-200 text-slate-900 dark:bg-slate-800 dark:text-slate-100",
};

const STATE_ICON: Record<SecurityCheckState, typeof ShieldCheck> = {
  pass: ShieldCheck,
  warn: TriangleAlert,
  fail: ShieldAlert,
  unknown: TriangleAlert,
};

/** One ring colour per state, used by the score's own track. */
const SCORE_TRACK: Record<SecurityCheckState, string> = {
  pass: "stroke-emerald-500",
  warn: "stroke-amber-500",
  fail: "stroke-red-500",
  unknown: "stroke-slate-400",
};

/** The score's worst state, which is what the ring is coloured by. */
function worstState(summary: Partial<Record<SecurityCheckState, number>>): SecurityCheckState {
  if ((summary.fail ?? 0) > 0) return "fail";
  if ((summary.warn ?? 0) > 0) return "warn";
  if ((summary.unknown ?? 0) > 0) return "unknown";
  return "pass";
}

function when(iso: string | null): string {
  if (!iso) return "never";
  const parsed = new Date(iso);
  if (Number.isNaN(parsed.getTime())) return iso;
  return parsed.toLocaleString();
}

/** The named fields a check's detail drawer shows, in the order an operator reads them. */
function detailRows(detail: Record<string, unknown>): { label: string; value: string }[] {
  const rows: { label: string; value: string }[] = [];
  const summary = detail.summary;
  if (typeof summary === "string" && summary) rows.push({ label: "What we found", value: summary });
  const reason = detail.reason;
  if (typeof reason === "string" && reason) rows.push({ label: "Why", value: reason });
  const note = detail.note;
  if (typeof note === "string" && note) rows.push({ label: "Note", value: note });
  const stale = detail.stale_after_hours;
  if (typeof stale === "number") {
    rows.push({ label: "Stale after", value: `${stale} hours` });
  }
  return rows;
}

function CheckRow({ check }: { check: SecurityCheck }) {
  const [open, setOpen] = useState(false);
  const Icon = STATE_ICON[check.state];
  const rows = detailRows(check.detail);
  const hasDetail = rows.length > 0;

  return (
    <li className="border-t border-line first:border-t-0">
      <div className="flex flex-wrap items-center gap-3 px-3 py-3 sm:px-4">
        <span
          className={`inline-flex items-center gap-1.5 rounded-full px-2 py-0.5 text-[11px] font-medium ${STATE_CLASS[check.state]}`}
          data-security-state={check.state}
        >
          <Icon aria-hidden className="h-3 w-3" />
          {STATE_LABEL[check.state]}
        </span>

        <div className="min-w-0 flex-1">
          <p className="truncate text-[13px] font-medium text-ink">{check.label}</p>
          {hasDetail ? (
            <p className="truncate text-[12px] text-muted">
              {String(rows[0].value)}
            </p>
          ) : (
            <p className="text-[12px] text-muted">
              {check.checked_at
                ? `Last checked ${when(check.checked_at)}`
                : "This check has never been evaluated on this deployment."}
            </p>
          )}
        </div>

        <div className="flex shrink-0 items-center gap-2">
          {hasDetail ? (
            <button
              type="button"
              onClick={() => setOpen((value) => !value)}
              aria-expanded={open}
              className="rounded border border-line px-2 py-1 text-[12px] text-muted hover:text-ink"
            >
              {open ? "Hide detail" : "Detail"}
            </button>
          ) : null}
          {/* The action is on every row, pass or not. A row with no action renders a dead
              control, which the definition of done forbids; and the link for a passing row is
              where an operator would go to *confirm* it, not only to fix it. */}
          <Link
            href={check.action_href}
            className="rounded border border-line px-2 py-1 text-[12px] text-ink hover:bg-surface"
          >
            {check.action_label}
          </Link>
        </div>
      </div>

      {open && hasDetail ? (
        <dl className="space-y-1 border-t border-line bg-surface px-4 py-3 text-[12px]">
          {rows.map((row) => (
            <div key={row.label} className="flex gap-2">
              <dt className="w-32 shrink-0 text-muted">{row.label}</dt>
              <dd className="min-w-0 break-words text-ink">{row.value}</dd>
            </div>
          ))}
          {check.checked_at ? (
            <div className="flex gap-2">
              <dt className="w-32 shrink-0 text-muted">Last checked</dt>
              <dd className="text-ink">{when(check.checked_at)}</dd>
            </div>
          ) : null}
        </dl>
      ) : null}
    </li>
  );
}

export function SecurityOverviewScreen() {
  const [data, setData] = useState<SecurityOverview | null>(null);
  const [error, setError] = useState<ApiError | null>(null);
  const [loading, setLoading] = useState(true);
  const [running, setRunning] = useState(false);
  const [runError, setRunError] = useState<ApiError | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setData(await fetchSecurityOverview());
    } catch (caught) {
      setError(caught as ApiError);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const run = useCallback(async () => {
    setRunning(true);
    setRunError(null);
    try {
      // The server's whole overview replaces what we had, rather than being merged in. A
      // merge would keep a stored `pass` for a check this run could not evaluate, which is
      // precisely the stale-green this screen exists to avoid.
      setData(await runSecurityChecks());
    } catch (caught) {
      setRunError(caught as ApiError);
    } finally {
      setRunning(false);
    }
  }, []);

  if (loading && !data) {
    return (
      <p className="flex items-center gap-2 text-[13px] text-muted" data-security-loading>
        <Loader2 aria-hidden className="h-3.5 w-3.5 animate-spin" />
        Reading the platform&apos;s security posture…
      </p>
    );
  }

  if (error && !data) {
    return (
      <div className="space-y-3" data-security-error>
        <p className="text-[13px] text-red-700 dark:text-red-300">
          The security overview could not be read: {error.message}
        </p>
        <button
          type="button"
          onClick={() => void load()}
          className="inline-flex items-center gap-1.5 rounded border border-line px-3 py-1.5 text-[13px] text-ink hover:bg-surface"
        >
          <RefreshCw aria-hidden className="h-3.5 w-3.5" />
          Try again
        </button>
      </div>
    );
  }

  if (!data) return null;

  const neverRun = data.last_run_at === null;
  const worst = worstState(data.summary);
  const openTotal = data.open_findings.reduce((sum, entry) => sum + entry.count, 0);
  const circumference = 2 * Math.PI * 34;

  return (
    <div className="space-y-6" data-security-overview>
      <header className="flex flex-wrap items-start justify-between gap-4">
        <div>
          <p className="text-[12px] text-muted">
            {neverRun
              ? "No scan has run on this deployment yet."
              : `Last scan ${when(data.last_run_at)}.`}
          </p>
          {runError ? (
            <p className="mt-1 text-[12px] text-red-700 dark:text-red-300" data-security-run-error>
              The run did not complete: {runError.message}
            </p>
          ) : null}
        </div>
        <button
          type="button"
          onClick={() => void run()}
          disabled={running}
          className="inline-flex items-center gap-1.5 rounded bg-ink px-3 py-1.5 text-[13px] text-canvas disabled:opacity-60"
          data-security-run
        >
          {running ? (
            <Loader2 aria-hidden className="h-3.5 w-3.5 animate-spin" />
          ) : (
            <PlayCircle aria-hidden className="h-3.5 w-3.5" />
          )}
          {running ? "Running checks…" : "Run checks"}
        </button>
      </header>

      <div className="grid gap-4 sm:grid-cols-[auto_1fr] sm:items-center">
        {/* The ring. The number is the server's; the colour is the worst state, so a ring
            showing 90 in amber is legible as "mostly fine, something needs attention" without
            reading the number at all. */}
        <div className="flex items-center gap-4">
          <div className="relative h-24 w-24" data-security-score={data.score}>
            <svg viewBox="0 0 80 80" className="h-24 w-24 -rotate-90" aria-hidden>
              <circle
                cx="40"
                cy="40"
                r="34"
                className="fill-none stroke-line"
                strokeWidth="8"
              />
              <circle
                cx="40"
                cy="40"
                r="34"
                className={`fill-none ${SCORE_TRACK[worst]}`}
                strokeWidth="8"
                strokeLinecap="round"
                strokeDasharray={circumference}
                strokeDashoffset={circumference * (1 - Math.max(0, Math.min(100, data.score)) / 100)}
              />
            </svg>
            <span className="absolute inset-0 flex items-center justify-center text-xl font-semibold text-ink">
              {data.score}
            </span>
          </div>
          <div className="space-y-1 text-[12px]">
            {(Object.keys(STATE_LABEL) as SecurityCheckState[]).map((state) => (
              <div key={state} className="flex items-center gap-2">
                <span
                  aria-hidden
                  className={`inline-block h-2 w-2 rounded-full ${
                    state === "pass"
                      ? "bg-emerald-500"
                      : state === "warn"
                        ? "bg-amber-500"
                        : state === "fail"
                          ? "bg-red-500"
                          : "bg-slate-400"
                  }`}
                />
                <span className="text-muted">{STATE_LABEL[state]}</span>
                <span className="font-medium text-ink">{data.summary[state] ?? 0}</span>
              </div>
            ))}
          </div>
        </div>

        <div className="rounded border border-line p-3">
          <p className="text-[12px] text-muted">Open findings</p>
          {openTotal === 0 ? (
            <p className="mt-1 text-[13px] text-ink">There are none.</p>
          ) : (
            <ul className="mt-1 flex flex-wrap gap-x-4 gap-y-1">
              {data.open_findings
                .filter((entry) => entry.count > 0)
                .map((entry) => (
                  <li key={entry.severity} className="text-[13px] text-ink">
                    <span className="capitalize">{entry.severity}</span>{" "}
                    <span className="text-muted">{entry.count}</span>
                  </li>
                ))}
            </ul>
          )}
          <Link
            href="/security/findings"
            className="mt-2 inline-block text-[12px] text-ink underline underline-offset-2"
          >
            Review findings
          </Link>
        </div>
      </div>

      {neverRun ? (
        <EmptyState
          title="No scan has run yet"
          hint="Every check below is waiting for its first evaluation. Press “Run checks” and the platform will say what it can actually verify — and what it cannot."
          action={
            <button
              type="button"
              onClick={() => void run()}
              disabled={running}
              className="inline-flex items-center gap-1.5 rounded bg-ink px-3 py-1.5 text-[13px] text-canvas disabled:opacity-60"
              data-security-first-run
            >
              <PlayCircle aria-hidden className="h-3.5 w-3.5" />
              Run the first scan
            </button>
          }
        />
      ) : null}

      <section aria-label="Posture checks">
        <ul className="overflow-hidden rounded border border-line" data-security-checks>
          {data.checks.map((check) => (
            <CheckRow key={check.key} check={check} />
          ))}
        </ul>
        <p className="mt-2 text-[12px] text-muted">
          {data.registry.length} checks in this build. A check that cannot be evaluated reports
          “Not checked yet” rather than a pass — the platform does not claim a result it did
          not verify.
        </p>
      </section>
    </div>
  );
}
