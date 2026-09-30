"use client";

/**
 * `/health` — the system health overview (REQ-014, slice 1).
 *
 * This screen makes a claim about the platform's liveness, and the whole design
 * is about not making a claim it cannot support. Six rules, each one a way a
 * status dashboard lies:
 *
 * 1. **Every registered service is a row, including the ones never probed.** The
 *    server sends all of them and this client renders all of them. A screen that
 *    listed only the services it received would make an unprobed platform look
 *    like a small one, and — the expensive half — its banner would say "all
 *    systems operational" for a platform nothing has ever looked at.
 * 2. **`unknown` is a first-class badge with its own colour, its own icon and its
 *    own sentence.** It is deliberately not muted grey: a row we could not
 *    verify is a question the operator has to see, and grey-next-to-secondary-text
 *    is exactly how it stops being one. This is the same reasoning the security
 *    centre's `unknown` badge carries, for the same reason.
 * 3. **The banner is the server's sentence.** The client does not recompute the
 *    worst state, because a client that added up its own rows would disagree with
 *    the runner the moment the registry and the stored results diverged — which is
 *    exactly what happens before the first run.
 * 4. **A row's own checks are behind a disclosure, not dumped inline.** A
 *    `down` Redis row wants one sentence and a button; the reason it is down is
 *    one click away. Rendering every check for every row would bury the two rows
 *    that matter under six rows that are fine.
 * 5. **Loading is a skeleton of the rows that are coming**, not a spinner over an
 *    empty page, so the layout does not jump when the numbers land.
 * 6. **A failed run says which probe errored and leaves the previous readings
 *    visible.** A blanked page after a failed manual run destroys the very
 *    evidence the operator opened the screen to read.
 *
 * Keyboard: `r` re-reads, `f` toggles auto-refresh. Mobile: rows become cards and
 * the metric grid becomes one column, with the badge and the value both still
 * visible without scrolling.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import Link from "next/link";
import {
  Activity,
  ChevronDown,
  ChevronRight,
  CircleCheck,
  CircleSlash,
  Loader2,
  PlayCircle,
  RefreshCw,
  TriangleAlert,
  Unplug,
} from "lucide-react";

import {
  fetchHealthOverview,
  runHealthChecks,
  type ApiError,
} from "@/lib/api";
import type { HealthOverview, HealthService, HealthState } from "@/lib/types";

/** The four states and how each one reads. The label is never the colour alone. */
const STATE_LABEL: Record<HealthState, string> = {
  healthy: "Healthy",
  degraded: "Degraded",
  down: "Down",
  unknown: "Not checked yet",
};

const STATE_CLASS: Record<HealthState, string> = {
  healthy: "bg-emerald-100 text-emerald-900 dark:bg-emerald-950 dark:text-emerald-200",
  degraded: "bg-amber-100 text-amber-900 dark:bg-amber-950 dark:text-amber-200",
  down: "bg-red-100 text-red-900 dark:bg-red-950 dark:text-red-200",
  // Deliberately *not* muted. See rule 2.
  unknown: "bg-slate-200 text-slate-900 dark:bg-slate-700 dark:text-slate-100",
};

const STATE_ICON: Record<HealthState, typeof Activity> = {
  healthy: CircleCheck,
  degraded: TriangleAlert,
  down: Unplug,
  unknown: CircleSlash,
};

/** The keys the skeleton draws, in the order the registry lists them. */
const SKELETON_SERVICES = [
  "api",
  "postgres",
  "redis",
  "s3",
  "workers",
  "queue",
  "search",
  "host",
] as const;

/** The badge. Colour, icon and word together — never the colour alone. */
function StateBadge({ state }: { state: HealthState }) {
  const Icon = STATE_ICON[state];
  return (
    <span
      data-health-badge={state}
      className={`inline-flex items-center gap-1.5 rounded-full px-2 py-0.5 text-[12px] font-medium ${STATE_CLASS[state]}`}
    >
      <Icon aria-hidden className="h-3.5 w-3.5" />
      {STATE_LABEL[state]}
    </span>
  );
}

/** A metric's unit, rendered only when there is one. */
function Unit({ unit }: { unit: string }) {
  if (!unit) return null;
  return <span className="ml-1 text-[12px] text-muted">{unit}</span>;
}

/** "12.3 s ago" / "just now" — the header's own freshness line. */
function ago(iso: string | null): string {
  if (!iso) return "never";
  const seconds = Math.max(0, Math.round((Date.now() - new Date(iso).getTime()) / 1000));
  if (seconds < 5) return "just now";
  if (seconds < 60) return `${seconds} s ago`;
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes} m ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 48) return `${hours} h ago`;
  return `${Math.round(hours / 24)} d ago`;
}

/** One service row, with its checks behind a disclosure. */
function ServiceRow({ service }: { service: HealthService }) {
  const [open, setOpen] = useState(false);
  const hasChecks = service.checks.length > 0;
  const detailEntries = Object.entries(service.detail).filter(
    ([, value]) => value !== null && typeof value !== "object",
  );

  return (
    <li
      data-health-service={service.service}
      data-health-state={service.state}
      className="border-t border-line first:border-t-0"
    >
      <div className="flex flex-wrap items-center gap-3 px-4 py-3">
        <StateBadge state={service.state} />
        <div className="min-w-0 flex-1">
          <Link
            href={service.href}
            data-health-service-link={service.service}
            className="text-[13.5px] font-medium hover:underline"
          >
            {service.service}
          </Link>
          <p className="truncate text-[12.5px] text-muted" title={service.message}>
            {service.message}
          </p>
        </div>
        <div className="text-right text-[12.5px] text-muted">
          {/*
            A dash, never `0 ms`. A zero-millisecond probe is a claim that was
            never made, and rendering it as `0 ms` is how an unprobed service ends
            up looking like the fastest one on the screen.
          */}
          <div>
            {service.latency_ms === null
              ? "—"
              : `${service.latency_ms} ms`}
          </div>
          <div className="text-[11.5px]">{ago(service.checked_at)}</div>
        </div>
        {hasChecks ? (
          <button
            type="button"
            data-health-checks-toggle={service.service}
            aria-expanded={open}
            onClick={() => setOpen((value) => !value)}
            className="inline-flex items-center gap-1 rounded px-2 py-1 text-[12.5px] text-muted hover:bg-surface"
          >
            {open ? <ChevronDown aria-hidden className="h-3.5 w-3.5" /> : <ChevronRight aria-hidden className="h-3.5 w-3.5" />}
            {service.checks.length} {service.checks.length === 1 ? "check" : "checks"}
          </button>
        ) : null}
      </div>

      {open && hasChecks ? (
        <div
          data-health-checks={service.service}
          className="border-t border-line bg-surface/50 px-4 py-2"
        >
          <ul className="space-y-1">
            {service.checks.map((check) => (
              <li
                key={check.check}
                data-health-check={check.check}
                data-health-check-state={check.state}
                className="flex flex-wrap items-baseline gap-2 text-[12.5px]"
              >
                <span className="font-mono text-[11.5px] text-muted">{check.check}</span>
                <span className={check.state === "healthy" ? "text-ink" : "text-amber-700 dark:text-amber-300"}>
                  {check.message}
                </span>
                <span className="text-[11.5px] text-muted">{check.latency_ms} ms</span>
              </li>
            ))}
          </ul>
          {detailEntries.length > 0 ? (
            <dl className="mt-2 flex flex-wrap gap-x-4 gap-y-1 text-[11.5px]">
              {detailEntries.map(([key, value]) => (
                <div key={key} className="flex gap-1">
                  <dt className="text-muted">{key.replaceAll("_", " ")}</dt>
                  <dd className="font-mono">{String(value)}</dd>
                </div>
              ))}
            </dl>
          ) : null}
        </div>
      ) : null}
    </li>
  );
}

/** A metric card. The threshold marker is only drawn when a threshold exists. */
function MetricCard({
  metric,
  value,
  unit,
  state,
  threshold,
}: {
  metric: string;
  value: number;
  unit: string;
  state: string;
  threshold: number | null;
}) {
  const over = threshold !== null && value >= threshold;
  return (
    <div
      data-health-metric={metric}
      data-health-metric-state={state}
      className="rounded-lg border border-line p-3"
    >
      <p className="text-[12px] text-muted">{metric.replaceAll("_", " ")}</p>
      <p className="mt-1 text-[20px] font-semibold tabular-nums">
        {value}
        <Unit unit={unit} />
      </p>
      <p className="mt-1 text-[11.5px] text-muted">
        {/*
          No threshold and no claim. A card that drew a warn line at zero would
          tell the operator their memory is fine because nothing has ever
          exceeded nothing.
        */}
        {threshold === null ? (
          "no threshold set"
        ) : (
          <span className={over ? "text-amber-700 dark:text-amber-300" : undefined}>
            threshold {threshold}
            {unit}
            {over ? " — over" : ""}
          </span>
        )}
      </p>
    </div>
  );
}

export function HealthOverviewScreen() {
  const [overview, setOverview] = useState<HealthOverview | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [running, setRunning] = useState(false);
  // `null` until the client has read the preference, so the first paint does not
  // flash a 15 s interval at somebody who turned it off.
  const [autoRefresh, setAutoRefresh] = useState<number | null>(null);
  const keyHandler = useRef(false);

  const load = useCallback(async () => {
    try {
      const next = await fetchHealthOverview();
      setOverview(next);
      setError(null);
    } catch (cause) {
      const apiError = cause as ApiError;
      // The previous readings stay on screen. A blanked page after a failed read
      // destroys the evidence the operator opened the screen to read.
      setError(apiError.message ?? "The health overview could not be read.");
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    const stored = Number(window.localStorage.getItem("omnion-health-refresh") ?? "15");
    // `0` is the "off" case and is a real choice, so it is not folded into the
    // fallback the way a NaN would be.
    setAutoRefresh(Number.isFinite(stored) && stored >= 0 ? stored : 15);
    void load();
  }, [load]);

  useEffect(() => {
    if (autoRefresh === null || autoRefresh === 0) return undefined;
    const timer = window.setInterval(() => void load(), autoRefresh * 1000);
    return () => window.clearInterval(timer);
  }, [autoRefresh, load]);

  // `r` and `f`, but never while the operator is typing: a filter box on a
  // sub-screen would otherwise steal a keystroke on every refresh tick.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      if (target && ["INPUT", "TEXTAREA", "SELECT"].includes(target.tagName)) return;
      if (event.metaKey || event.ctrlKey || event.altKey) return;
      if (keyHandler.current) return;
      keyHandler.current = true;
      window.setTimeout(() => {
        keyHandler.current = false;
      }, 250);
      if (event.key === "r") {
        event.preventDefault();
        void load();
      }
      if (event.key === "f") {
        event.preventDefault();
        setAutoRefresh((current) => {
          const next = current === 0 ? 15 : 0;
          window.localStorage.setItem("omnion-health-refresh", String(next));
          return next;
        });
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [load]);

  const onRun = async () => {
    setRunning(true);
    setError(null);
    try {
      // The server answers with the whole overview, so what the operator sees
      // afterwards is what the server now believes — not a merge that could keep
      // the last stored `healthy` for a service that has just gone down.
      setOverview(await runHealthChecks());
    } catch (cause) {
      setError((cause as ApiError).message ?? "The checks could not be run.");
    } finally {
      setRunning(false);
    }
  };

  const onAutoRefresh = (value: number) => {
    setAutoRefresh(value);
    window.localStorage.setItem("omnion-health-refresh", String(value));
  };

  return (
    <div className="space-y-4" data-health-screen={loading && !overview ? "loading" : "ready"}>
      {/*
        The banner is the server's own sentence. This client does not recompute
        the worst state — see rule 3.
      */}
      {overview ? (
        <div
          data-health-banner={overview.banner.state}
          className={`rounded-lg border border-line p-4 ${
            overview.banner.state === "down"
              ? "border-red-300 bg-red-50 dark:border-red-900 dark:bg-red-950/40"
              : overview.banner.state === "degraded"
                ? "border-amber-300 bg-amber-50 dark:border-amber-900 dark:bg-amber-950/40"
                : overview.banner.state === "healthy"
                  ? "border-emerald-300 bg-emerald-50 dark:border-emerald-900 dark:bg-emerald-950/40"
                  : ""
          }`}
        >
          <p className="text-[15px] font-semibold">{overview.banner.headline}</p>
          <p className="mt-0.5 text-[12.5px] text-muted">
            {Object.entries(overview.counts)
              .map(([state, count]) => `${count} ${STATE_LABEL[state as HealthState].toLowerCase()}`)
              .join(" · ")}
          </p>
        </div>
      ) : null}

      {error ? (
        <div
          role="alert"
          data-health-error
          className="flex flex-wrap items-center gap-3 rounded-lg border border-red-300 bg-red-50 p-3 text-[13px] dark:border-red-900 dark:bg-red-950/40"
        >
          <span className="flex-1">{error}</span>
          <button
            type="button"
            onClick={() => void load()}
            className="rounded border border-line px-2 py-1 text-[12.5px]"
          >
            Try again
          </button>
        </div>
      ) : null}

      <div className="flex flex-wrap items-center gap-2">
        <button
          type="button"
          data-health-run
          onClick={() => void onRun()}
          disabled={running}
          className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-1.5 text-[13px] disabled:opacity-60"
        >
          {running ? (
            <Loader2 aria-hidden className="h-4 w-4 animate-spin" />
          ) : (
            <PlayCircle aria-hidden className="h-4 w-4" />
          )}
          Run all checks
        </button>
        <button
          type="button"
          data-health-refresh-now
          onClick={() => void load()}
          className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-1.5 text-[13px]"
        >
          <RefreshCw aria-hidden className="h-4 w-4" />
          Reload
        </button>
        <label className="flex items-center gap-2 text-[12.5px] text-muted">
          Auto-refresh
          <select
            data-health-auto-refresh
            value={autoRefresh ?? 15}
            onChange={(event) => onAutoRefresh(Number(event.target.value))}
            className="rounded border border-line bg-surface px-2 py-1 text-[12.5px]"
          >
            <option value={5}>5 s</option>
            <option value={15}>15 s</option>
            <option value={60}>60 s</option>
            <option value={0}>off</option>
          </select>
        </label>
        {overview ? (
          <span data-health-last-checked className="text-[12.5px] text-muted">
            Last checked {ago(overview.last_checked_at)}
          </span>
        ) : null}
      </div>

      {loading && !overview ? (
        // Skeletons shaped like the rows that are coming, so the layout does not
        // jump when the numbers land.
        //
        // The eight keys are spelled out rather than read from `overview.registry`
        // because this branch is exactly the one where `overview` is `null` —
        // TypeScript narrows it to `never` here, and the honest fix is the one
        // that does not depend on a value the branch has already ruled out. It
        // also keeps the skeleton honest if the registry ever grows: the server
        // always sends a row for every registered service, so a skeleton with
        // eight rows and an overview with nine is a question the *server* raises.
        <ul className="rounded-lg border border-line" data-health-skeleton>
          {SKELETON_SERVICES.map((key) => (
            <li key={key} className="flex items-center gap-3 border-t border-line px-4 py-3 first:border-t-0">
              <span className="h-5 w-24 animate-pulse rounded bg-surface" />
              <span className="h-4 flex-1 animate-pulse rounded bg-surface" />
            </li>
          ))}
        </ul>
      ) : null}

      {overview ? (
        <>
          <section aria-labelledby="health-services-heading" className="space-y-2">
            <h2 id="health-services-heading" className="text-[13px] font-semibold">
              Services
            </h2>
            <ul className="rounded-lg border border-line" data-health-services={overview.services.length}>
              {overview.services.map((service) => (
                <ServiceRow key={service.service} service={service} />
              ))}
            </ul>
          </section>

          {overview.host.length > 0 ? (
            <section aria-labelledby="health-metrics-heading" className="space-y-2">
              <h2 id="health-metrics-heading" className="text-[13px] font-semibold">
                Host
              </h2>
              {/* One column on mobile, six on a desktop. The grid is `sm:`-prefixed
                  rather than fixed because a six-across grid on a phone is a
                  horizontal scrollbar, and the acceptance criteria name mobile as
                  "the status and the value visible without scrolling". */}
              <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-6">
                {overview.host.map((metric) => (
                  <MetricCard
                    key={metric.metric}
                    metric={metric.metric}
                    value={metric.value}
                    unit={metric.unit}
                    state={metric.state}
                    threshold={metric.threshold}
                  />
                ))}
              </div>
            </section>
          ) : null}
        </>
      ) : null}
    </div>
  );
}
