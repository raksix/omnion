"use client";

/**
 * The `/developer` overview: the card row and the recent failures (REQ-022, slice 2).
 *
 * **What this screen is for.** An operator who has just issued their first key opens this and
 * wants one answer: *is anything actually calling it?* So the cards are ordered by how alarming
 * they are — the refusals first, the request count second, the key inventory last — and the
 * failure list sits directly under them, because a count without the rows is a number nobody
 * can act on.
 *
 * **Every number here has a table beside it, and this screen says so.** The card row is not a
 * summary of something else: it is the *same* query the key list and the log screen would run.
 * That is why the retention window is printed under the card row rather than hidden in a
 * settings page — an operator comparing "41 requests today" against a log that only holds 30
 * days of history needs to know that "today" is a real boundary, not a coincidence.
 *
 * **The failure list is a link, not a dead end.** Each row opens the log screen filtered to that
 * request, because "what went wrong" is a question with a follow-up and this is only its first
 * half.
 */
import { useCallback, useEffect, useState } from "react";

import {
  ArrowRight,
  CircleCheck,
  KeyRound,
  RefreshCw,
  TriangleAlert,
  XCircle,
} from "lucide-react";
import Link from "next/link";

import { ApiError, fetchDeveloperOverview } from "@/lib/api";
import type { DeveloperOverview } from "@/lib/developer";
import { formatTimestamp } from "@/lib/format";

/** Where the screen stands. `error` carries the request id when the API gave one. */
type State =
  | { status: "loading" }
  | { status: "ready"; overview: DeveloperOverview }
  | { status: "error"; message: string; requestId: string | null };

export function DeveloperOverviewScreen() {
  const [state, setState] = useState<State>({ status: "loading" });

  const load = useCallback(async () => {
    setState({ status: "loading" });
    try {
      const overview = await fetchDeveloperOverview();
      setState({ status: "ready", overview });
    } catch (cause: unknown) {
      setState({
        status: "error",
        message:
          cause instanceof ApiError
            ? cause.message
            : "The developer overview could not be loaded.",
        requestId: null,
      });
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  if (state.status === "loading") {
    return <OverviewSkeleton />;
  }

  if (state.status === "error") {
    return (
      <div
        role="alert"
        className="flex flex-col items-start gap-3 rounded-xl border border-line bg-surface px-4 py-6"
      >
        <p className="flex items-center gap-2 text-[13px] font-medium text-caution">
          <XCircle className="size-4" aria-hidden />
          {state.message}
        </p>
        {/* The request id is what a platform operator pastes into a ticket. Printing it only when
            the API sent one keeps the line from reading as an empty promise. */}
        {state.requestId ? (
          <p className="text-[12px] text-muted">Request {state.requestId}</p>
        ) : null}
        <button
          type="button"
          onClick={() => void load()}
          className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-quiet-soft"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Try again
        </button>
      </div>
    );
  }

  const { overview } = state;
  const failures = overview.recent_failures;
  const nothingFailed = overview.errors_today === 0;

  return (
    <div className="flex flex-col gap-5" data-developer-overview>
      <div className="flex items-center justify-end">
        <button
          type="button"
          onClick={() => void load()}
          aria-label="Reload the overview"
          className="rounded-lg border border-line bg-surface p-2 text-muted transition hover:text-ink"
        >
          <RefreshCw className="size-3.5" aria-hidden />
        </button>
      </div>

      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 xl:grid-cols-4">
        <Card
          label="Refusals today"
          value={overview.errors_today}
          tone={overview.errors_today > 0 ? "bad" : "good"}
          hint={
            overview.errors_today > 0
              ? "Open the list below to see which scopes were missing."
              : "Every request so far was accepted."
          }
          icon={overview.errors_today > 0 ? TriangleAlert : CircleCheck}
        />
        <Card
          label="Requests today"
          value={overview.requests_today}
          tone="plain"
          hint={`Since midnight UTC. The log keeps ${overview.log_retention_days} days.`}
          icon={RefreshCw}
        />
        <Card
          label="Active keys"
          value={overview.keys.active}
          tone="plain"
          hint={
            overview.keys.expired > 0
              ? `${overview.keys.expired} expired and no longer authenticates.`
              : "Keys that can still authenticate right now."
          }
          icon={KeyRound}
        />
        <Card
          label="Revoked keys"
          value={overview.keys.revoked}
          tone="plain"
          hint="Kept for their history — the row and its log rows stay."
          icon={XCircle}
        />
      </div>

      <section
        aria-labelledby="developer-failures"
        className="overflow-hidden rounded-xl border border-line bg-surface"
      >
        <div className="flex flex-wrap items-center justify-between gap-2 border-b border-line px-4 py-3">
          <h2 id="developer-failures" className="text-[13.5px] font-medium">
            Recent failures
          </h2>
          <Link
            href="/developer/logs?status_class=4xx"
            className="inline-flex items-center gap-1 text-[12px] text-accent-strong hover:underline"
          >
            Open the full log
            <ArrowRight className="size-3.5" aria-hidden />
          </Link>
        </div>

        {failures.length === 0 ? (
          // An empty state that says *why* it is empty. "No recent failures" next to a card
          // reading 3 refusals would be a contradiction, and the only honest empty state is the
          // one that distinguishes "nothing has gone wrong" from "nothing has happened yet".
          <p className="px-4 py-8 text-center text-[12.5px] text-muted">
            {nothingFailed
              ? "Nothing has been refused today. The newest refusals will appear here."
              : `${overview.errors_today} refusals today, all older than the ${overview.log_retention_days}-day window this list covers.`}
          </p>
        ) : (
          <ul className="divide-y divide-line">
            {failures.map((line) => (
              <li key={line.id}>
                <Link
                  href={`/developer/logs?focus=${line.id}`}
                  className="flex flex-wrap items-baseline gap-x-3 gap-y-1 px-4 py-2.5 transition hover:bg-quiet-soft"
                >
                  <span
                    className={`inline-flex w-12 shrink-0 justify-center rounded px-1.5 py-0.5 text-[11px] font-medium ${
                      line.status >= 500
                        ? "bg-caution-soft text-caution"
                        : "bg-caution-soft text-caution"
                    }`}
                  >
                    {line.status}
                  </span>
                  <span className="font-mono text-[12px]">
                    {line.method} {line.path}
                  </span>
                  <span className="text-[11.5px] text-muted">
                    {formatTimestamp(line.created_at)}
                  </span>
                  {/* The scope the guard asked for is the single most useful thing on this row:
                      it is the difference between "your key is broken" and "your key is missing
                      exactly this permission". */}
                  {line.permission ? (
                    <span className="ml-auto font-mono text-[11.5px] text-muted">
                      {line.key_prefix ? `${line.key_prefix} · ` : ""}
                      needs {line.permission}
                    </span>
                  ) : null}
                </Link>
              </li>
            ))}
          </ul>
        )}
      </section>
    </div>
  );
}

type CardProps = {
  label: string;
  value: number;
  hint: string;
  tone: "plain" | "good" | "bad";
  icon: typeof KeyRound;
};

/** One number, its label, and the sentence that makes it mean something. */
function Card({ label, value, hint, tone, icon: Icon }: CardProps) {
  const toneClass =
    tone === "bad"
      ? "text-caution"
      : tone === "good"
        ? "text-positive"
        : "text-ink";
  return (
    <div className="flex flex-col gap-1 rounded-xl border border-line bg-surface px-4 py-3.5">
      <span className="flex items-center gap-1.5 text-[12px] text-muted">
        <Icon className="size-3.5" aria-hidden />
        {label}
      </span>
      <span className={`text-[22px] font-semibold tabular-nums ${toneClass}`}>{value}</span>
      <span className="text-[11.5px] text-muted">{hint}</span>
    </div>
  );
}

/** The card row's own loading state: four bars, so the screen does not jump when they land. */
function OverviewSkeleton() {
  return (
    <div
      className="grid grid-cols-1 gap-3 sm:grid-cols-2 xl:grid-cols-4"
      aria-busy="true"
      aria-label="Loading the developer overview"
    >
      {[0, 1, 2, 3].map((index) => (
        <div
          key={index}
          className="flex flex-col gap-2 rounded-xl border border-line bg-surface px-4 py-3.5"
        >
          <span className="block h-3 w-24 animate-pulse rounded bg-quiet-soft" />
          <span className="block h-6 w-12 animate-pulse rounded bg-quiet-soft" />
          <span className="block h-3 w-32 animate-pulse rounded bg-quiet-soft" />
        </div>
      ))}
    </div>
  );
}
