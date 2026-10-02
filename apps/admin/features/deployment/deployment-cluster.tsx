"use client";

/**
 * `/deployment/kubernetes` — the cluster panel (REQ-024, slice 4).
 *
 * The screen has two shapes and **only** two: a cluster table, or the single-instance process
 * card. There is no third, and the reason is in the spec — *"the route only renders when the
 * runtime reports a cluster, never a disabled card as a tease."* A card that says "not a cluster"
 * with a greyed-out replica count is a card an operator learns to ignore, and the next time the
 * deployment *is* a cluster they will not believe it.
 *
 * Three states on this screen are silent failures, so each is rendered as a distinct state rather
 * than as a number:
 *
 * * **A metric the runtime did not report** is an em dash carrying its reason in a tooltip. It is
 *   never a `0`: an idle-looking cluster is the reading an operator acts on, and "0" is what a
 *   missing metric looks like to every eye except the one that wrote the code.
 * * **A usage with no limit** has no percentage and no bar. A workload that declares no CPU limit
 *   is *unlimited*, which is a fact about the deployment — not 0%, and not 100%.
 * * **A gap in the sparkline** stays a gap. A straight line drawn across three unmeasured minutes
 *   says the value was steady, and the operator cannot tell that from a measured flat line.
 *
 * The over-limit report is separate from the bars for the same reason: a bar clamped to 100% looks
 * like "at capacity" rather than "throttled", and 240% is the thing the operator opened the screen
 * for.
 */

import { useCallback, useEffect, useMemo, useState } from "react";

import {
  Activity,
  CircleAlert,
  Cpu,
  Loader2,
  MemoryStick,
  RefreshCw,
  RotateCw,
  Server,
  TriangleAlert,
} from "lucide-react";

import {
  ApiError,
  fetchDeploymentCluster,
  restartClusterWorkload,
  runClusterSample,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import type {
  ClusterMetric,
  ClusterSparkline,
  ClusterWorkload,
  DeploymentClusterResponse,
  DeploymentProcessCard,
} from "@/lib/types";

/** The three environments, from one list, so the API cannot grow a fourth the screen forgets. */
const ENVIRONMENTS = ["production", "staging", "sandbox"] as const;
type EnvironmentName = (typeof ENVIRONMENTS)[number];

type Reading = DeploymentClusterResponse | DeploymentProcessCard;

export function DeploymentCluster({ environment }: { environment?: EnvironmentName }) {
  const [env, setEnv] = useState<EnvironmentName>(environment ?? "production");
  const [reading, setReading] = useState<Reading | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [sampling, setSampling] = useState(false);
  const [restarting, setRestarting] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setReading(await fetchDeploymentCluster(env));
    } catch (caught) {
      // A read that failed for any other reason is a real error state with a retry, not the
      // single-instance card: showing the process card for a network error would tell the
      // operator their cluster is not a cluster.
      setError(caught instanceof ApiError ? caught.message : String(caught));
      setReading(null);
    } finally {
      setLoading(false);
    }
  }, [env]);

  useEffect(() => {
    void load();
  }, [load]);

  const onSample = useCallback(async () => {
    setSampling(true);
    setNotice(null);
    try {
      const result = await runClusterSample(env);
      setNotice(
        result.runtime === "cluster"
          ? `Recorded ${result.written} sample${result.written === 1 ? "" : "s"} and pruned ${result.pruned}.`
          : "This deployment runs as a single instance, so there is nothing to sample.",
      );
      await load();
    } catch (caught) {
      setNotice(caught instanceof ApiError ? caught.message : String(caught));
    } finally {
      setSampling(false);
    }
  }, [env, load]);

  const onRestart = useCallback(
    async (workload: ClusterWorkload) => {
      // The typed confirmation is required for production only, and the string being typed is the
      // workload's own name: a restart of the wrong workload is an outage of the wrong thing.
      const typed =
        env === "production"
          ? window.prompt(
              `Restart ${workload.name}? Type the workload name to confirm.`,
              "",
            )
          : "confirm";
      if (typed === null) return;
      setRestarting(workload.name);
      setNotice(null);
      try {
        const result = await restartClusterWorkload({
          environment: env,
          workload: workload.name,
          confirm: typed,
          reason: `restarted from the cluster panel`,
        });
        setNotice(result.message);
        await load();
      } catch (caught) {
        // The server's own sentence: a mismatch, a busy environment and a missing token are three
        // different problems and re-wording them here would lose the distinction.
        setNotice(caught instanceof ApiError ? caught.message : String(caught));
      } finally {
        setRestarting(null);
      }
    },
    [env, load],
  );

  // Narrowed on the discriminant, not on a truthiness check: `!cluster` does not tell TypeScript
  // that `reading` is the process card rather than the cluster response, and a cast here would
  // hide exactly the mistake this screen is most able to make.
  const cluster: DeploymentClusterResponse | null =
    reading?.runtime === "cluster" ? reading : null;
  const processCard: DeploymentProcessCard | null =
    reading?.runtime === "single" ? reading : null;

  return (
    <section className="space-y-4">
      <div className="flex flex-wrap items-center gap-2">
        <label className="text-sm font-medium" htmlFor="cluster-environment">
          Environment
        </label>
        <select
          id="cluster-environment"
          className="rounded-md border px-2 py-1 text-sm"
          value={env}
          onChange={(event) => setEnv(event.target.value as EnvironmentName)}
        >
          {ENVIRONMENTS.map((name) => (
            <option key={name} value={name}>
              {name}
            </option>
          ))}
        </select>
        <button
          type="button"
          className="inline-flex items-center gap-1 rounded-md border px-2 py-1 text-sm"
          onClick={() => void load()}
          disabled={loading}
        >
          {loading ? <Loader2 className="h-4 w-4 animate-spin" aria-hidden /> : <RefreshCw className="h-4 w-4" aria-hidden />}
          Refresh
        </button>
        <button
          type="button"
          className="inline-flex items-center gap-1 rounded-md border px-2 py-1 text-sm"
          onClick={() => void onSample()}
          disabled={sampling}
        >
          {sampling ? <Loader2 className="h-4 w-4 animate-spin" aria-hidden /> : <Activity className="h-4 w-4" aria-hidden />}
          Sample now
        </button>
        {cluster && (
          <span className="text-xs text-muted-foreground">
            Read at {formatTimestamp(cluster.read_at)} · {cluster.window_minutes}-minute window
          </span>
        )}
      </div>

      {notice && (
        <p
          role="status"
          data-cluster-notice="1"
          className="rounded-md border border-border bg-muted px-3 py-2 text-sm"
        >
          {notice}
        </p>
      )}

      {error && (
        <div role="alert" className="flex items-center gap-2 rounded-md border border-destructive/40 px-3 py-2 text-sm">
          <CircleAlert className="h-4 w-4 text-destructive" aria-hidden />
          <span>{error}</span>
          <button type="button" className="ml-auto underline" onClick={() => void load()}>
            Retry
          </button>
        </div>
      )}

      {loading && !reading && <ClusterSkeleton />}

      {processCard && <ProcessCard reading={processCard} />}

      {cluster && (
        <ClusterTable
          cluster={cluster}
          restarting={restarting}
          onRestart={(workload) => void onRestart(workload)}
        />
      )}
    </section>
  );
}

/** The card skeletons, so a slow read is a shape rather than a blank page. */
function ClusterSkeleton() {
  return (
    <div aria-busy="true" aria-label="Loading the cluster" className="space-y-2">
      {[0, 1, 2].map((row) => (
        <div key={row} className="h-12 animate-pulse rounded-md bg-muted" />
      ))}
    </div>
  );
}

/**
 * The single-instance card.
 *
 * The reason is shown, and it is a *different sentence* for "you are not in a cluster" and "you
 * are in a cluster and its token is missing" — the server decides which, and the operator needs to
 * know which, because the second one is a real misconfiguration and the first one is not.
 */
function ProcessCard({ reading }: { reading: DeploymentProcessCard }) {
  const process = reading.process;
  return (
    <div className="rounded-lg border border-border p-4" data-cluster-runtime="single">
      <div className="flex items-center gap-2">
        <Server className="h-5 w-5" aria-hidden />
        <h2 className="text-base font-semibold">Single instance</h2>
      </div>
      <p className="mt-1 text-sm text-muted-foreground" data-cluster-reason="single">
        {reading.reason ?? "This deployment runs as one process, so it has no cluster to read."}
      </p>
      {process ? (
        <dl className="mt-3 grid gap-2 sm:grid-cols-2">
          <div>
            <dt className="text-xs text-muted-foreground">Uptime</dt>
            <dd className="text-sm font-medium">{formatAge(process.uptime_seconds)}</dd>
          </div>
          <div>
            <dt className="text-xs text-muted-foreground">Resident memory</dt>
            <dd className="text-sm font-medium">
              <MetricCell metric={process.resident_memory} />
            </dd>
          </div>
        </dl>
      ) : (
        <p className="mt-3 text-sm text-muted-foreground">
          Process figures are not available from this platform.
        </p>
      )}
    </div>
  );
}

/** The cluster table, with the rollout banner above it. */
function ClusterTable({
  cluster,
  restarting,
  onRestart,
}: {
  cluster: DeploymentClusterResponse;
  restarting: string | null;
  onRestart: (workload: ClusterWorkload) => void;
}) {
  if (cluster.workloads.length === 0) {
    return (
      <div className="rounded-lg border border-border p-6 text-center">
        <p className="text-sm font-medium">No workloads are running in this namespace.</p>
        <p className="mt-1 text-sm text-muted-foreground">
          The cluster answered and reported nothing. That is different from a cluster that could
          not be read, which would say so above.
        </p>
      </div>
    );
  }

  return (
    <div className="space-y-3">
      {cluster.rollout_banner && (
        <p className="flex items-center gap-2 rounded-md border border-border bg-muted px-3 py-2 text-sm">
          <RefreshCw className="h-4 w-4" aria-hidden />
          {cluster.rollout_banner}
        </p>
      )}

      {cluster.over_limit.length > 0 && (
        <ul className="space-y-1" aria-label="Workloads over their limits">
          {cluster.over_limit.map((line) => (
            <li
              key={line}
              className="flex items-center gap-2 rounded-md border border-destructive/40 px-3 py-2 text-sm"
            >
              <TriangleAlert className="h-4 w-4 text-destructive" aria-hidden />
              {line}
            </li>
          ))}
        </ul>
      )}

      {/* The table scrolls horizontally on a phone rather than compressing six columns into
          unreadable slivers, and the log-pane-style independent scroll region is what the spec
          asks for on mobile. */}
      <div className="overflow-x-auto rounded-lg border border-border" data-cluster-runtime="cluster">
        <table className="w-full min-w-[900px] text-sm">
          <caption className="sr-only">
            Cluster workloads with replicas, resource requests, limits and usage
          </caption>
          <thead>
            <tr className="border-b border-border text-left">
              <th scope="col" className="px-3 py-2 font-medium">Workload</th>
              <th scope="col" className="px-3 py-2 font-medium">Replicas</th>
              <th scope="col" className="px-3 py-2 font-medium">CPU</th>
              <th scope="col" className="px-3 py-2 font-medium">Memory</th>
              <th scope="col" className="px-3 py-2 font-medium">30-minute CPU</th>
              <th scope="col" className="px-3 py-2 font-medium">Restarts</th>
              <th scope="col" className="px-3 py-2 font-medium">Age</th>
              <th scope="col" className="px-3 py-2 font-medium">Action</th>
            </tr>
          </thead>
          <tbody>
            {cluster.workloads.map((workload) => (
              <tr key={workload.name} data-cluster-workload={workload.name} className="border-b border-border last:border-0 align-top">
                <th scope="row" className="px-3 py-2 text-left font-medium">
                  {workload.name}
                  {workload.rollout_banner && (
                    <span className="mt-1 block text-xs font-normal text-muted-foreground">
                      {workload.rollout_banner}
                    </span>
                  )}
                </th>
                <td className="px-3 py-2">
                  <Replicas workload={workload} />
                </td>
                <td className="px-3 py-2">
                  <Resource
                    icon={<Cpu className="h-3.5 w-3.5" aria-hidden />}
                    request={workload.cpu_request}
                    limit={workload.cpu_limit}
                    usage={workload.cpu_usage}
                    percent={workload.cpu_percent}
                    over={workload.cpu_over_limit}
                  />
                </td>
                <td className="px-3 py-2">
                  <Resource
                    icon={<MemoryStick className="h-3.5 w-3.5" aria-hidden />}
                    request={workload.memory_request}
                    limit={workload.memory_limit}
                    usage={workload.memory_usage}
                    percent={workload.memory_percent}
                    over={workload.memory_over_limit}
                  />
                </td>
                <td className="px-3 py-2">
                  <SparklineCell line={workload.cpu_sparkline} expected={workload.cpu_expected_samples} />
                </td>
                <td className="px-3 py-2">{workload.restarts}</td>
                <td className="px-3 py-2 whitespace-nowrap">{workload.age_label}</td>
                <td className="px-3 py-2">
                  <button
                    type="button"
                    className="inline-flex items-center gap-1 rounded-md border border-destructive/50 px-2 py-1 text-xs"
                    onClick={() => onRestart(workload)}
                    disabled={restarting === workload.name}
                  >
                    {restarting === workload.name ? (
                      <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
                    ) : (
                      <RotateCw className="h-3.5 w-3.5" aria-hidden />
                    )}
                    Restart
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}

/**
 * `ready / desired`, with the scaled-to-zero case said rather than shown as a percentage.
 *
 * `0/0` is printed literally instead of "0%": a workload scaled to zero is complete, and a bar at
 * zero percent is a rendering that implies something is wrong with it.
 */
function Replicas({ workload }: { workload: ClusterWorkload }) {
  const percent = workload.ready_percent;
  return (
    <span>
      <span className="font-medium">
        {workload.replicas_ready}/{workload.replicas_desired}
      </span>
      {percent === null ? (
        <span className="ml-1 text-xs text-muted-foreground">scaled to zero</span>
      ) : (
        <span className="ml-1 text-xs text-muted-foreground">{percent}%</span>
      )}
    </span>
  );
}

/** One resource's request, limit and usage, with a bar only when both sides are known. */
function Resource({
  icon,
  request,
  limit,
  usage,
  percent,
  over,
}: {
  icon: React.ReactNode;
  request: ClusterMetric;
  limit: ClusterMetric;
  usage: ClusterMetric;
  percent: number | null;
  over: boolean;
}) {
  return (
    <div className="space-y-0.5 text-xs">
      <div className="flex items-center gap-1">
        {icon}
        <span className="font-medium">
          <MetricCell metric={usage} />
        </span>
        {over && (
          <span className="font-semibold text-destructive">
            {percent}% of limit
          </span>
        )}
      </div>
      <div className="text-muted-foreground">
        request <MetricCell metric={request} /> · limit <MetricCell metric={limit} />
      </div>
      {percent !== null && (
        <div className="h-1 w-24 overflow-hidden rounded-full bg-muted">
          {/* The bar is clamped at its own width; the over-limit *label* carries the overflow,
              because a bar pinned at 100% for a workload at 240% cannot show the difference. */}
          <div
            className={`h-full ${over ? "bg-destructive" : "bg-foreground/70"}`}
            style={{ width: `${Math.min(100, percent)}%` }}
          />
        </div>
      )}
    </div>
  );
}

/**
 * A metric cell.
 *
 * The three states are visually distinct, because the em dash is doing a lot of work here and it
 * has to read as "nobody measured this" rather than as a value: the reason is in the `title`, and
 * screen readers get the same sentence.
 */
function MetricCell({ metric }: { metric: ClusterMetric }) {
  if (metric.state === "unknown") {
    return (
      <span className="text-muted-foreground" title={metric.reason} aria-label={metric.reason}>
        —
      </span>
    );
  }
  if (metric.state === "millicores") {
    return <span>{metric.value} m</span>;
  }
  return <span>{formatBytes(metric.value)}</span>;
}

/**
 * The 30-minute sparkline.
 *
 * Drawn as columns rather than a `polyline` so a gap is a visible hole instead of a line drawn
 * across three unmeasured minutes. A flat series is a flat line in the middle, and the gaps count
 * is labelled, so "the value held" and "nobody measured it" are never the same picture.
 */
function SparklineCell({
  line,
  expected,
}: {
  line: ClusterSparkline | null;
  expected: number;
}) {
  if (!line || line.points.length === 0) {
    return (
      <span className="text-xs text-muted-foreground">
        No samples yet — the sampler records one a minute.
      </span>
    );
  }
  return (
    <div className="space-y-0.5">
      <div
        className="flex h-8 items-end gap-px"
        role="img"
        aria-label={`CPU over the last ${expected} minutes: min ${line.min ?? "unknown"}, max ${line.max ?? "unknown"}${line.gaps > 0 ? `, ${line.gaps} minutes unmeasured` : ""}`}
      >
        {line.points.map((height, index) => (
          <span
            // A 50 is either a gap or a flat series; both are drawn hollow so neither is mistaken
            // for a measurement at mid-height.
            key={index}
            className={`w-0.5 ${height === 50 && line.flat ? "bg-foreground/30" : "bg-foreground/70"}`}
            style={{ height: `${Math.max(2, (height / 100) * 32)}px` }}
          />
        ))}
      </div>
      <span className="text-xs text-muted-foreground">
        {line.min ?? "—"}–{line.max ?? "—"} m
        {line.gaps > 0 && ` · ${line.gaps} min unmeasured`}
      </span>
    </div>
  );
}

/** Binary units, matching the server's formatter so the two never disagree on screen. */
function formatBytes(bytes: number): string {
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit + 1 < units.length) {
    value /= 1024;
    unit += 1;
  }
  return unit === 0 ? `${bytes} B` : value < 10 ? `${value.toFixed(1)} ${units[unit]}` : `${Math.round(value)} ${units[unit]}`;
}

/** The same duration rule the server uses, for the single-instance card. */
function formatAge(seconds: number): string {
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
  if (seconds < 86_400) return `${Math.floor(seconds / 3600)}h ${Math.floor((seconds % 3600) / 60)}m`;
  return `${Math.floor(seconds / 86_400)}d ${Math.floor((seconds % 86_400) / 3600)}h`;
}
