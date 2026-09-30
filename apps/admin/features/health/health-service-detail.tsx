"use client";

/**
 * `/health/services/[key]` — one service, its checks and what it has published
 * (REQ-014, slice 1).
 *
 * This screen exists because the overview is a *summary*: it tells the operator
 * which rows are unhappy and nothing about why. Four rules, and each is a way a
 * drill-down screen becomes worse than no drill-down:
 *
 * 1. **A key the platform does not probe is a `404`, rendered as a sentence.**
 *    The service rows link here from `/health`, so the reachable set is exactly
 *    the registry's. Rendering a "not found" *card* for an unknown key would be a
 *    second, differently-worded answer to the same question, and the panel's own
 *    vocabulary for "this is not a thing here" is the 404 the API already sends.
 * 2. **The detail's metrics come from the store, not from the response's own
 *    report.** The report is this run's readings; the table is everything the
 *    service has published. A probe that changed its metric set last week must
 *    still show its history rather than silently losing a column.
 * 2b. **Every row carries its own 24 h trend, read in the same request.** The
 *    request asks for "a 24 h trend chart" on this screen, and a table of current
 *    values with the history somewhere else is not a trend chart. One window, one
 *    instant, all rows — two series read at two different times are not
 *    comparable, and two charts on one page have to be.
 * 3. **An empty metric table says "no samples yet" and keeps the checks.** The
 *    checks are live and the history is not; blanking the whole screen because a
 *    fresh database has no history would hide the one part that does work.
 * 4. **A failed read leaves the previous service on screen** with the error
 *    bannered above it, for the same reason the overview does.
 *
 * Keyboard: `r` re-reads. Mobile: the checks table becomes a card list and the
 * metric table scrolls horizontally rather than squeezing five columns into a
 * phone.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import Link from "next/link";
import { useParams } from "next/navigation";
import {
  ArrowLeft,
  CircleCheck,
  CircleSlash,
  RefreshCw,
  TriangleAlert,
  Unplug,
} from "lucide-react";

import { fetchHealthService, type ApiError } from "@/lib/api";
import type { HealthServiceDetail, HealthState } from "@/lib/types";

import { Sparkline } from "./sparkline";

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
  unknown: "bg-slate-200 text-slate-900 dark:bg-slate-700 dark:text-slate-100",
};

const STATE_ICON: Record<HealthState, typeof CircleCheck> = {
  healthy: CircleCheck,
  degraded: TriangleAlert,
  down: Unplug,
  unknown: CircleSlash,
};

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

export function HealthServiceDetailScreen() {
  const params = useParams<{ key: string }>();
  const key = Array.isArray(params?.key) ? params.key[0] : params?.key ?? "";

  const [detail, setDetail] = useState<HealthServiceDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notFound, setNotFound] = useState(false);
  const [loading, setLoading] = useState(true);
  const keyHandler = useRef(false);

  const load = useCallback(async () => {
    if (!key) return;
    try {
      const next = await fetchHealthService(key);
      setDetail(next);
      setError(null);
      setNotFound(false);
    } catch (cause) {
      const apiError = cause as ApiError & { status?: number };
      if (apiError.status === 404) {
        // The API already decided this key is not a thing here. Rendering an
        // empty "healthy-looking" service here would be a second answer.
        setNotFound(true);
        setDetail(null);
      } else {
        setError(apiError.message ?? "This service could not be read.");
      }
    } finally {
      setLoading(false);
    }
  }, [key]);

  useEffect(() => {
    void load();
  }, [load]);

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
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [load]);

  if (loading) {
    return (
      <div data-health-detail-screen="loading" className="space-y-3">
        <div className="h-6 w-40 animate-pulse rounded bg-surface" />
        <div className="h-24 animate-pulse rounded-lg bg-surface" />
        <div className="h-32 animate-pulse rounded-lg bg-surface" />
      </div>
    );
  }

  if (notFound) {
    return (
      <div
        data-health-detail-screen="not-found"
        className="rounded-lg border border-line p-6 text-center"
      >
        <p className="text-[14px] font-medium">{key} is not a service this platform probes</p>
        <p className="mt-1 text-[12.5px] text-muted">
          The probe registry has no entry for it, so there is nothing to report about it.
        </p>
        <Link
          href="/health"
          className="mt-3 inline-flex items-center gap-1.5 text-[12.5px] text-accent hover:underline"
        >
          <ArrowLeft aria-hidden className="h-3.5 w-3.5" />
          Back to system health
        </Link>
      </div>
    );
  }

  if (!detail) {
    return (
      <div data-health-detail-screen="error" className="rounded-lg border border-line p-6">
        <p className="text-[13.5px] text-red-700 dark:text-red-300">
          {error ?? "This service could not be read."}
        </p>
        <button
          type="button"
          data-health-detail-retry
          onClick={() => void load()}
          className="mt-3 inline-flex items-center gap-1.5 rounded border border-line px-2 py-1 text-[12.5px] hover:bg-surface"
        >
          <RefreshCw aria-hidden className="h-3.5 w-3.5" />
          Try again
        </button>
      </div>
    );
  }

  // `ServiceDetailBody` flattens the service row into the response, so the detail is
  // the *same shape* as an overview row plus `metrics` — not a `service` property
  // holding one. Rest-destructuring keeps the row's own fields (`service` is a string
  // here) reachable without a wrapper that the server never sends.
  const { metrics, ...service } = detail;

  return (
    <div data-health-detail-screen="ready" className="space-y-5">
      <div className="flex flex-wrap items-center gap-3">
        <Link
          href="/health"
          className="inline-flex items-center gap-1.5 text-[12.5px] text-muted hover:text-ink"
        >
          <ArrowLeft aria-hidden className="h-3.5 w-3.5" />
          All services
        </Link>
        <h2 className="text-[16px] font-semibold">{service.service}</h2>
        <StateBadge state={service.state} />
        <button
          type="button"
          data-health-detail-refresh
          onClick={() => void load()}
          className="ml-auto inline-flex items-center gap-1.5 rounded border border-line px-2 py-1 text-[12.5px] hover:bg-surface"
        >
          <RefreshCw aria-hidden className="h-3.5 w-3.5" />
          Re-read
        </button>
      </div>

      {error ? (
        <p data-health-detail-error className="rounded border border-line px-3 py-2 text-[12.5px] text-amber-700 dark:text-amber-300">
          {error}
        </p>
      ) : null}

      <p className="text-[13px] text-muted">{service.description}</p>
      <p className="text-[13px]">{service.message}</p>

      <dl className="grid grid-cols-2 gap-3 sm:grid-cols-4">
        <div className="rounded-lg border border-line p-3">
          <dt className="text-[12px] text-muted">Latency</dt>
          <dd className="mt-1 text-[18px] font-semibold tabular-nums">
            {service.latency_ms === null ? "—" : `${service.latency_ms} ms`}
          </dd>
        </div>
        <div className="rounded-lg border border-line p-3">
          <dt className="text-[12px] text-muted">Last checked</dt>
          <dd className="mt-1 text-[13px]">{ago(service.checked_at)}</dd>
        </div>
        <div className="rounded-lg border border-line p-3">
          <dt className="text-[12px] text-muted">Checks</dt>
          <dd className="mt-1 text-[18px] font-semibold tabular-nums">
            {service.checks.length}
          </dd>
        </div>
        <div className="rounded-lg border border-line p-3">
          <dt className="text-[12px] text-muted">Metrics recorded</dt>
          <dd className="mt-1 text-[18px] font-semibold tabular-nums">{metrics.length}</dd>
        </div>
      </dl>

      <section>
        <h3 className="mb-2 text-[13.5px] font-medium">Checks</h3>
        {service.checks.length === 0 ? (
          <p
            data-health-detail-no-checks
            className="rounded-lg border border-line px-3 py-4 text-[12.5px] text-muted"
          >
            This service has not been probed yet, so it has no checks to show. Press
            {" “Re-read” "}
            or run the checks from the overview.
          </p>
        ) : (
          <>
            {/* Desktop: the table the request asks for. */}
            <table
              data-health-check-table
              className="hidden w-full text-left text-[12.5px] sm:table"
            >
              <thead>
                <tr className="border-b border-line text-muted">
                  <th className="py-1.5 pr-3 font-medium">Check</th>
                  <th className="py-1.5 pr-3 font-medium">State</th>
                  <th className="py-1.5 pr-3 font-medium">Latency</th>
                  <th className="py-1.5 pr-3 font-medium">Message</th>
                </tr>
              </thead>
              <tbody>
                {service.checks.map((check) => (
                  <tr key={check.check} data-health-check={check.check} className="border-b border-line/60">
                    <td className="py-1.5 pr-3 font-mono text-[11.5px]">{check.check}</td>
                    <td className="py-1.5 pr-3">
                      <StateBadge state={check.state as HealthState} />
                    </td>
                    <td className="py-1.5 pr-3 tabular-nums">{check.latency_ms} ms</td>
                    <td className="py-1.5 pr-3">{check.message}</td>
                  </tr>
                ))}
              </tbody>
            </table>
            {/* Mobile: the same rows as cards, because four columns on a phone is a
                table nobody can read. */}
            <ul data-health-check-cards className="space-y-2 sm:hidden">
              {service.checks.map((check) => (
                <li
                  key={check.check}
                  data-health-check={check.check}
                  className="rounded-lg border border-line p-3 text-[12.5px]"
                >
                  <div className="flex items-center justify-between gap-2">
                    <span className="font-mono text-[11.5px]">{check.check}</span>
                    <StateBadge state={check.state as HealthState} />
                  </div>
                  <p className="mt-1.5">{check.message}</p>
                  <p className="mt-1 text-[11.5px] text-muted tabular-nums">
                    {check.latency_ms} ms
                  </p>
                </li>
              ))}
            </ul>
          </>
        )}
      </section>

      {/*
        The trend column is the point of this screen, and the reason it is on the
        *row* rather than behind a second request is in the request itself: the detail
        page's job is "a 24 h trend chart", and a table of current values with the
        history on another screen is a table, not a drill-down. Every row's series
        comes from the server in the same response, over the same window — see
        `SERVICE_TREND_RANGE` in `health_panel.rs`. Two lines on one page that were
        read at two different instants would not be comparable, which is the single
        thing two charts sharing a screen must be.
      */}
      <section>
        <h3 className="mb-2 text-[13.5px] font-medium">Published metrics · last 24 h</h3>
        {metrics.length === 0 ? (
          <p
            data-health-detail-no-metrics
            className="rounded-lg border border-line px-3 py-4 text-[12.5px] text-muted"
          >
            No samples recorded for this service yet. Its checks above are live; the history
            starts with the first run that stores one.
          </p>
        ) : (
          <div className="overflow-x-auto">
            <table data-health-metric-table className="w-full text-left text-[12.5px]">
              <thead>
                <tr className="border-b border-line text-muted">
                  <th className="py-1.5 pr-3 font-medium">Metric</th>
                  <th className="py-1.5 pr-3 font-medium">Value</th>
                  <th className="py-1.5 pr-3 font-medium">Unit</th>
                  <th className="py-1.5 pr-3 font-medium">Sampled at</th>
                  <th className="py-1.5 font-medium">24 h trend</th>
                </tr>
              </thead>
              <tbody>
                {metrics.map((metric) => (
                  <tr
                    key={metric.metric}
                    data-health-detail-metric={metric.metric}
                    className="border-b border-line/60"
                  >
                    <td className="py-1.5 pr-3 font-mono text-[11.5px]">{metric.metric}</td>
                    <td className="py-1.5 pr-3 tabular-nums">{metric.value}</td>
                    <td className="py-1.5 pr-3 text-muted">{metric.unit || "—"}</td>
                    <td className="py-1.5 pr-3 text-muted">{ago(metric.sampled_at)}</td>
                    <td className="py-1.5">
                      <Sparkline values={metric.series} label={metric.metric} widthClass="w-32" />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </section>
    </div>
  );
}
