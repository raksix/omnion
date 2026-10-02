"use client";

/**
 * `/observability` — the landing screen of the observability centre (docs/requests/REQ-126).
 *
 * The request asks for "request rate, error ratio, p95, queue depth, AI spend today, exporter
 * health, links into each area". Six numbers and six doors, and the design problem is entirely in
 * the states:
 *
 * - **A dash is not a zero.** Every headline is `null` on an instance that has recorded nothing,
 *   which is the normal state of a fresh install and of a stack that was reset an hour ago. This
 *   screen renders `null` as "—" and says so underneath, because a row of `0`s on an idle
 *   instance reads as a healthy flat line, and the operator's first conclusion — "nothing is
 *   wrong, nothing is happening" — is the one conclusion the screen cannot support.
 * - **A tile that could not be read is named.** `unavailable` lists the sources the server could
 *   not compose, and those tiles are rendered as unavailable rather than as zero. A degraded
 *   dependency that reports "0 errors" is worse than one that reports "could not read".
 * - **An error ratio with no denominator is absent, not zero.** Zero errors out of zero requests
 *   is a division that happened to come out even; showing it as a clean `0%` would tell an
 *   operator during an incident that their error budget is untouched.
 * - **Alerts are the one thing that must never degrade silently.** When the alert store cannot be
 *   read the tile says the alert state is unknown rather than drawing zero alerts, because "no
 *   alerts" and "we cannot see the alerts" are opposites during an outage.
 *
 * Keyboard: `r` re-reads, `g` then `l`/`t`/`m`/`x`/`a`/`s` jumps to logs, traces, metrics,
 * exporters, alerts, settings — the six doors have single keys because during an incident nobody
 * wants to hunt for a link.
 */
import { useCallback, useEffect, useState } from "react";

import Link from "next/link";
import {
  AlertTriangle,
  BellRing,
  ChevronRight,
  Loader2,
  Radio,
  RefreshCw,
} from "lucide-react";

import {
  ApiError,
  fetchObservabilityOverview,
  type ObservabilityOverview,
} from "@/lib/api";

/** One headline tile, with the family it came from so the panel can link to its chart. */
function Tile({
  label,
  value,
  hint,
  metric,
  slot,
}: {
  label: string;
  value: number | null;
  hint: string;
  metric: string;
  slot: string;
}) {
  return (
    <div className="rounded-lg border border-line bg-surface p-4" data-tile={slot}>
      <div className="text-[12px] text-muted">{label}</div>
      <div className="mt-1 font-mono text-[22px] leading-tight">
        {value === null ? (
          <span className="text-muted" title="No samples in this window">
            —
          </span>
        ) : (
          value
        )}
      </div>
      <div className="mt-1 text-[11.5px] text-muted">{hint}</div>
      <div className="mt-2 font-mono text-[10.5px] text-muted">{metric}</div>
    </div>
  );
}

/** The exporter chips, with the drop count that decides whether to trust the chip. */
function Exporters({ overview }: { overview: ObservabilityOverview }) {
  if (overview.exporters.length === 0) {
    return (
      <p className="text-[12.5px] text-muted">
        No exporter is configured. Nothing leaves this instance until one is.
      </p>
    );
  }
  return (
    <ul className="space-y-2">
      {overview.exporters.map((exporter) => (
        <li
          key={exporter.name}
          className="flex items-center gap-3 rounded-lg border border-line bg-surface px-3 py-2"
        >
          <Radio
            className={`size-3.5 ${exporter.health === "healthy" ? "text-ok" : "text-warn"}`}
            aria-hidden="true"
          />
          <span className="text-[13px] font-medium">{exporter.name}</span>
          <span className="font-mono text-[11.5px] text-muted">{exporter.health}</span>
          <span className="ml-auto font-mono text-[11.5px] text-muted">
            {exporter.dropped_total} dropped
            {exporter.buffered > 0 ? ` · ${exporter.buffered} buffered` : ""}
          </span>
        </li>
      ))}
    </ul>
  );
}

/** The alert counts, and the newest firing incident when there is one. */
function Alerts({ overview }: { overview: ObservabilityOverview }) {
  if (overview.alerts === null) {
    return (
      <p className="flex items-start gap-2 text-[12.5px] text-warn">
        <AlertTriangle className="mt-0.5 size-3.5 shrink-0" aria-hidden="true" />
        The alert state could not be read. That is not the same as "no alerts" — the rules may
        well be firing.
      </p>
    );
  }
  const { firing, pending, latest_firing: latest } = overview.alerts;
  if (firing === 0 && pending === 0) {
    return <p className="text-[12.5px] text-muted">Nothing is firing and nothing is pending.</p>;
  }
  return (
    <div className="space-y-2">
      <p className="text-[13px]">
        <span className="font-mono">{firing}</span> firing ·{" "}
        <span className="font-mono">{pending}</span> pending
      </p>
      {latest && (
        <Link
          href="/observability/alerts"
          className="flex items-center gap-2 rounded-lg border border-line bg-surface px-3 py-2 text-[12.5px] hover:underline"
        >
          <BellRing className="size-3.5 shrink-0 text-warn" aria-hidden="true" />
          <span className="font-medium">{latest.rule ?? "a deleted rule"}</span>
          <span className="font-mono text-[11px] text-muted">{latest.started_at}</span>
          <ChevronRight className="ml-auto size-3.5 shrink-0 text-muted" aria-hidden="true" />
        </Link>
      )}
    </div>
  );
}

/** The six doors into the rest of the centre. */
const AREAS = [
  { href: "/observability/logs", label: "Logs", key: "l", blurb: "The bounded log explorer" },
  { href: "/observability/traces", label: "Traces", key: "t", blurb: "Trace search and waterfalls" },
  { href: "/observability/metrics", label: "Metrics", key: "m", blurb: "Every family this build can record" },
  { href: "/observability/exporters", label: "Exporters", key: "x", blurb: "Where telemetry goes, and what it dropped" },
  { href: "/observability/alerts", label: "Alerts", key: "a", blurb: "Rules, firing state and silences" },
  { href: "/observability/settings", label: "Settings", key: "s", blurb: "Sampling, retention, levels, budget" },
];

export function OverviewView() {
  const [overview, setOverview] = useState<ObservabilityOverview | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setOverview(await fetchObservabilityOverview());
    } catch (caught) {
      setError(
        caught instanceof ApiError
          ? caught.message
          : "The observability overview could not be read.",
      );
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // `r` re-reads. `g` arms the six doors so they take a single key each while something is
  // actually happening — without the prefix `a` would fight the alert rules' own text input.
  useEffect(() => {
    let armed = false;
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      if (target && ["INPUT", "TEXTAREA", "SELECT"].includes(target.tagName)) return;
      if (event.metaKey || event.ctrlKey || event.altKey) return;

      if (event.key === "r") {
        event.preventDefault();
        void load();
        return;
      }
      if (event.key === "g") {
        armed = true;
        window.setTimeout(() => {
          armed = false;
        }, 2000);
        return;
      }
      if (!armed) return;
      const area = AREAS.find((candidate) => candidate.key === event.key);
      if (area) {
        event.preventDefault();
        armed = false;
        window.location.href = area.href;
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [load]);

  if (loading && !overview) {
    return (
      <div className="flex items-center gap-2 text-[13px] text-muted">
        <Loader2 className="size-3.5 animate-spin" aria-hidden="true" />
        Reading the registry, the exporters and the alert store…
      </div>
    );
  }

  if (error) {
    return (
      <div className="space-y-3">
        <p className="flex items-start gap-2 text-[13px] text-warn">
          <AlertTriangle className="mt-0.5 size-3.5 shrink-0" aria-hidden="true" />
          {error}
        </p>
        <button
          type="button"
          onClick={() => void load()}
          className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] hover:bg-muted-soft"
        >
          <RefreshCw className="size-3.5" aria-hidden="true" />
          Try again
        </button>
      </div>
    );
  }

  if (!overview) return null;

  const unavailable = new Set(overview.unavailable);
  const alertUnavailable = unavailable.has("alerts");

  return (
    <div className="space-y-6" data-view="observability-overview">
      <div className="flex items-center justify-between gap-3">
        <p className="text-[12.5px] text-muted">
          Measured over the last {overview.window_minutes} minutes. Press{" "}
          <kbd className="rounded border border-line px-1 font-mono text-[11px]">r</kbd> to
          re-read.
        </p>
        <button
          type="button"
          onClick={() => void load()}
          className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] hover:bg-muted-soft"
        >
          <RefreshCw className="size-3.5" aria-hidden="true" />
          Re-read
        </button>
      </div>

      {overview.no_traffic && (
        <p className="rounded-lg border border-line bg-surface px-4 py-3 text-[12.5px] text-muted">
          This instance has not served a request in the window. The dashes below are an absence of
          data, not a measurement of zero.
        </p>
      )}

      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
        <Tile
          label="Requests"
          slot="requests"
          value={overview.requests_total}
          hint={overview.no_traffic ? "nothing served yet" : "in the window"}
          metric="omnion_http_requests_total"
        />
        <Tile
          label="Error ratio"
          slot="error-ratio"
          value={overview.error_ratio === null ? null : Number((overview.error_ratio * 100).toFixed(2))}
          hint={overview.error_ratio === null ? "no requests to divide by" : "percent of requests"}
          metric="derived"
        />
        <Tile
          label="p95 latency"
          slot="p95"
          value={overview.p95_latency_seconds === null ? null : Number(overview.p95_latency_seconds.toFixed(3))}
          hint={overview.p95_latency_seconds === null ? "no timings recorded" : "seconds, slowest series"}
          metric="omnion_http_request_duration_seconds"
        />
        <Tile
          label="Queue depth"
          slot="queue"
          value={overview.queue_depth}
          hint={overview.queue_depth === null ? "no queue metrics" : "jobs waiting"}
          metric="omnion_queue_depth"
        />
        <Tile
          label="AI spend today"
          slot="spend"
          value={overview.ai_cost_micros_today === null ? null : Number((overview.ai_cost_micros_today / 1_000_000).toFixed(4))}
          hint={overview.ai_cost_micros_today === null ? "no AI calls" : "currency units"}
          metric="omnion_ai_cost_micros_total"
        />
        <div className="rounded-lg border border-line bg-surface p-4" data-tile="alerts">
          <div className="text-[12px] text-muted">Alerts</div>
          <div className="mt-1 font-mono text-[22px] leading-tight">
            {alertUnavailable ? (
              <span className="text-warn">unknown</span>
            ) : overview.alerts === null ? (
              <span className="text-muted">—</span>
            ) : (
              overview.alerts.firing
            )}
          </div>
          <div className="mt-1 text-[11.5px] text-muted">
            {alertUnavailable ? "the alert store could not be read" : "firing"}
          </div>
          <div className="mt-2 font-mono text-[10.5px] text-muted">obs_alert_events</div>
        </div>
      </div>

      <div className="grid gap-6 lg:grid-cols-2">
        <section>
          <h2 className="text-[13px] font-medium">Alerts</h2>
          <div className="mt-2">
            <Alerts overview={overview} />
          </div>
        </section>

        <section>
          <h2 className="text-[13px] font-medium">Exporters</h2>
          <div className="mt-2">
            <Exporters overview={overview} />
          </div>
        </section>
      </div>

      <section>
        <h2 className="text-[13px] font-medium">Everywhere else</h2>
        <div className="mt-2 grid gap-2 sm:grid-cols-2 lg:grid-cols-3">
          {AREAS.map((area) => (
            <Link
              key={area.href}
              href={area.href}
              className="flex items-center gap-2 rounded-lg border border-line bg-surface px-3 py-2 hover:bg-muted-soft"
            >
              <span className="text-[13px] font-medium">{area.label}</span>
              <span className="truncate text-[11.5px] text-muted">{area.blurb}</span>
              <ChevronRight className="ml-auto size-3.5 shrink-0 text-muted" aria-hidden="true" />
            </Link>
          ))}
        </div>
      </section>
    </div>
  );
}
