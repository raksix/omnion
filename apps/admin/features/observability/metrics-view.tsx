"use client";

/**
 * `/observability/metrics` — the metric catalogue and the chart behind one selector
 * (docs/requests/REQ-126, slice 2).
 *
 * The screen exists because a metric name on its own is not information. An operator looking at
 * `omnion_http_requests_total` needs to know what it counts, what it is labelled by, whether this
 * instance is actually emitting it, and how close it is to the point where the platform starts
 * folding samples — and those four answers are four different columns, because collapsing them
 * produces the failure this whole request is about: a chart of something that is not happening.
 *
 * The three states the screen is careful about, each of which looks like a working screen if you
 * squint:
 *
 * - **Declared but never recorded** is not the same as **no samples right now**. A family with
 *   no `last_seen_at` is one this build has not emitted — a configuration problem or a module that
 *   is not installed. A family with samples an hour ago is idle traffic. They are rendered
 *   differently, and a `live` badge with no samples is not one of the two.
 * - **Over budget is not a warning about the data**, it is a warning about the *resolution*. The
 *   screen says what is being folded and where it went, because `rate()` on a series that is
 *   quietly absorbing its neighbours is a number an operator will act on.
 * - **An unknown selector is refused by the API with a message.** The screen does not have an
 *   "any metric" mode, so there is no state in which a blank graph could mean "wrong name".
 *
 * Keyboard: `/` focuses the family filter, `r` refreshes, `c` copies the PromQL for the chart on
 * screen, `Esc` clears the filter. Under `sm:` the catalogue table becomes cards.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { AlertTriangle, BarChart3, Copy, Gauge, RefreshCw, Search, Timer, TrendingUp } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  fetchMetricCatalog,
  fetchMetricQuery,
  syncMetricCatalog,
  type MetricCatalogResponse,
  type MetricFamily,
  type MetricQueryResponse,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";

/** The windows the range selector offers, in minutes. All of them are inside the API's cap. */
const WINDOWS: { label: string; minutes: number }[] = [
  { label: "15 min", minutes: 15 },
  { label: "1 h", minutes: 60 },
  { label: "6 h", minutes: 360 },
  { label: "24 h", minutes: 1440 },
];

/** The kind chip's tone. A histogram and a counter are read very differently, so they look different. */
const KIND_TONE: Record<string, string> = {
  counter: "bg-positive-soft text-positive",
  gauge: "bg-accent-soft text-accent",
  histogram: "bg-caution-soft text-caution",
};

const KIND_ICON: Record<string, typeof Gauge> = {
  counter: TrendingUp,
  gauge: Gauge,
  histogram: BarChart3,
};

/** A value in the family's own unit; a count is an integer, a seconds value keeps three decimals. */
function formatValue(value: number, unit: string): string {
  if (!Number.isFinite(value)) return "—";
  if (unit === "1" || unit === "") {
    return new Intl.NumberFormat("en", { maximumFractionDigits: 0 }).format(value);
  }
  if (Math.abs(value) >= 1000) {
    return `${(value / 1000).toFixed(1)}k ${unit}`;
  }
  return `${value.toFixed(value < 1 ? 3 : 1)} ${unit}`;
}

/** How a family is doing against its cap, as a percentage — `null` when it is not budgeted. */
function budgetPercent(family: MetricFamily): number | null {
  if (!family.budgeted || family.cardinality_budget <= 0) return null;
  return Math.min(100, (family.cardinality_estimate / family.cardinality_budget) * 100);
}

/**
 * The chart.
 *
 * Hand-drawn SVG rather than a charting library: the data is at most `max_points` minute buckets
 * per series, the shape needed is a line, and a dependency that renders its own axes would own
 * the contrast, the mobile layout and the empty state — all three of which this screen is judged
 * on. A `NaN` anywhere in the input would produce a silently blank plot, so the scale refuses
 * non-finite values rather than letting them through.
 */
function SeriesChart({ data }: { data: MetricQueryResponse }) {
  const width = 720;
  const height = 180;
  const padding = { top: 12, right: 12, bottom: 22, left: 48 };

  const finite = data.series
    .flatMap((series) => series.points.map((point) => point.value))
    .filter((value) => Number.isFinite(value));
  const peak = finite.length > 0 ? Math.max(...finite, 0) : 0;

  if (data.no_samples || data.series.length === 0) {
    return (
      <div
        className="flex h-[180px] items-center justify-center rounded-lg border border-dashed border-line text-[12.5px] text-muted"
        role="img"
        aria-label={`No samples for ${data.metric}`}
      >
        No samples recorded for this family yet
      </div>
    );
  }

  const longest = Math.max(...data.series.map((series) => series.points.length), 1);
  const usableWidth = width - padding.left - padding.right;
  const usableHeight = height - padding.top - padding.bottom;
  const xAt = (index: number) =>
    padding.left + (longest <= 1 ? usableWidth / 2 : (index / (longest - 1)) * usableWidth);
  const yAt = (value: number) => padding.top + usableHeight - (peak > 0 ? (value / peak) * usableHeight : 0);

  return (
    <div className="overflow-x-auto">
      <svg
        viewBox={`0 0 ${width} ${height}`}
        className="h-[180px] w-full min-w-[520px]"
        role="img"
        aria-label={`${data.metric} over ${data.window_minutes} minutes, peak ${formatValue(peak, data.unit)}`}
      >
        <line
          x1={padding.left}
          y1={padding.top + usableHeight}
          x2={width - padding.right}
          y2={padding.top + usableHeight}
          stroke="currentColor"
          className="text-line"
          strokeWidth="1"
        />
        <text
          x={padding.left - 8}
          y={padding.top + 4}
          textAnchor="end"
          className="fill-muted text-[10px]"
        >
          {formatValue(peak, data.unit)}
        </text>
        <text x={padding.left - 8} y={padding.top + usableHeight + 4} textAnchor="end" className="fill-muted text-[10px]">
          0
        </text>
        <text x={padding.left} y={height - 6} className="fill-muted text-[10px]">
          {data.window_minutes} min ago
        </text>
        <text x={width - padding.right} y={height - 6} textAnchor="end" className="fill-muted text-[10px]">
          now
        </text>
        {data.series.map((series, seriesIndex) => {
          const offset = seriesIndex * 0.55;
          const points = series.points
            .map((point, index) => {
              if (!Number.isFinite(point.value)) return null;
              return `${xAt(index)},${yAt(point.value)}`;
            })
            .filter((pair): pair is string => pair !== null)
            .join(" ");
          if (!points) return null;
          return (
            <polyline
              key={`${series.labels.join("|")}-${offset}`}
              points={points}
              fill="none"
              strokeWidth="1.75"
              className={
                seriesIndex === 0
                  ? "stroke-accent"
                  : seriesIndex % 3 === 1
                    ? "stroke-positive"
                    : seriesIndex % 3 === 2
                      ? "stroke-caution"
                      : "stroke-muted"
              }
            />
          );
        })}
      </svg>
    </div>
  );
}

/** The catalogue row, as a table row on wide screens and as a card under `sm:`. */
function FamilyRow({
  family,
  selected,
  onSelect,
}: {
  family: MetricFamily;
  selected: boolean;
  onSelect: () => void;
}) {
  const Icon = KIND_ICON[family.kind] ?? Gauge;
  const percent = budgetPercent(family);
  const never = family.last_seen_at === null;

  return (
    <>
      <tr
        className={`border-t border-line ${selected ? "bg-accent-soft" : ""}`}
        onClick={onSelect}
        aria-selected={selected}
      >
        <td className="px-4 py-3">
          <button
            type="button"
            onClick={onSelect}
            className="flex items-center gap-2 text-left text-[13px] font-medium hover:underline"
            aria-pressed={selected}
          >
            <Icon className="h-3.5 w-3.5 shrink-0 text-muted" aria-hidden="true" />
            <span className="font-mono text-[12.5px]">{family.name}</span>
          </button>
          <p className="mt-1 max-w-xl text-[12px] text-muted">{family.description}</p>
        </td>
        <td className="px-4 py-3">
          <span className={`rounded px-1.5 py-0.5 text-[11px] ${KIND_TONE[family.kind] ?? ""}`}>
            {family.kind}
          </span>
        </td>
        <td className="px-4 py-3 font-mono text-[11.5px] text-muted">
          {family.labels.length > 0 ? family.labels.join(", ") : "—"}
        </td>
        <td className="px-4 py-3 text-[12.5px] text-muted">{family.unit}</td>
        <td className="px-4 py-3">
          <div className="flex items-center gap-2">
            <span className="text-[12.5px] tabular-nums">
              {family.cardinality_estimate}
              {family.budgeted ? ` / ${family.cardinality_budget}` : ""}
            </span>
            {percent !== null ? (
              <span className="h-1 w-16 overflow-hidden rounded-full bg-quiet-soft" aria-hidden="true">
                <span
                  className={`block h-full ${percent >= 100 ? "bg-caution" : percent >= 75 ? "bg-caution" : "bg-accent"}`}
                  style={{ width: `${percent}%` }}
                />
              </span>
            ) : null}
          </div>
          {family.over_budget ? (
            <p className="mt-1 flex items-center gap-1 text-[11.5px] text-caution">
              <AlertTriangle className="h-3 w-3" aria-hidden="true" />
              folding into other
            </p>
          ) : null}
        </td>
        <td className="px-4 py-3 text-[12.5px]">
          {never ? (
            <span className="text-muted">not recorded</span>
          ) : (
            <span className="text-muted">{formatTimestamp(family.last_seen_at as string)}</span>
          )}
        </td>
      </tr>
      {/* The card under `sm:` — the same information, stacked. A table that scrolls sideways on a
          phone is not a layout, it is a desktop table the browser refused to break. */}
      <tr className="border-t border-line sm:hidden">
        <td className="px-4 py-3">
          <div className="flex flex-col gap-1.5 text-[12.5px]">
            <span className="flex items-center gap-2 font-mono text-[12px] font-medium">
              <Icon className="h-3.5 w-3.5 text-muted" aria-hidden="true" />
              {family.name}
            </span>
            <span className="text-muted">{family.description}</span>
            <span className="flex flex-wrap items-center gap-2 text-[11.5px] text-muted">
              <span className={`rounded px-1.5 py-0.5 ${KIND_TONE[family.kind] ?? ""}`}>
                {family.kind}
              </span>
              <span>{family.labels.join(", ") || "no labels"}</span>
              <span>
                {family.cardinality_estimate}
                {family.budgeted ? ` / ${family.cardinality_budget} series` : " series"}
              </span>
              <span>{never ? "not recorded" : formatTimestamp(family.last_seen_at as string)}</span>
              {family.over_budget ? <span className="text-caution">folding into other</span> : null}
            </span>
          </div>
        </td>
      </tr>
    </>
  );
}

export function MetricsView() {
  const [catalog, setCatalog] = useState<MetricCatalogResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const [selected, setSelected] = useState<string | null>(null);
  const [windowMinutes, setWindowMinutes] = useState(60);
  const [chart, setChart] = useState<MetricQueryResponse | null>(null);
  const [chartLoading, setChartLoading] = useState(false);
  const [chartError, setChartError] = useState<string | null>(null);
  const [syncing, setSyncing] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const searchRef = useRef<HTMLInputElement>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const data = await fetchMetricCatalog();
      setCatalog(data);
      setSelected((current) => {
        if (current && data.families.some((family) => family.name === current)) return current;
        return data.families[0]?.name ?? null;
      });
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The metric catalogue could not be read.");
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // The chart follows the selection. The family list is loaded first and the chart second, so a
  // failure in one does not blank the other — the catalogue is still useful without a chart.
  useEffect(() => {
    if (!selected) {
      setChart(null);
      return;
    }
    let cancelled = false;
    setChartLoading(true);
    setChartError(null);
    fetchMetricQuery(selected, windowMinutes)
      .then((data) => {
        if (!cancelled) setChart(data);
      })
      .catch((cause) => {
        if (!cancelled) {
          setChart(null);
          setChartError(
            cause instanceof ApiError ? cause.message : "The chart could not be read.",
          );
        }
      })
      .finally(() => {
        if (!cancelled) setChartLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [selected, windowMinutes]);

  const families = catalog?.families ?? [];
  const visible = useMemo(() => {
    const needle = filter.trim().toLowerCase();
    if (!needle) return families;
    return families.filter(
      (family) =>
        family.name.toLowerCase().includes(needle) ||
        family.description.toLowerCase().includes(needle) ||
        family.labels.some((label) => label.toLowerCase().includes(needle)),
    );
  }, [families, filter]);

  const overBudget = catalog?.over_budget ?? [];

  // Every early return in a keyboard handler has to hand focus back: a shortcut that strands focus
  // in an input is swallowed by the typing guard for the rest of the visit, so `/` works once and
  // then nothing does.
  const onKeyDown = useCallback(
    (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target?.tagName === "INPUT" || target?.tagName === "TEXTAREA" || target?.isContentEditable;
      if (event.key === "Escape") {
        if (filter) {
          setFilter("");
          searchRef.current?.focus();
        } else if (typing) {
          (target as HTMLElement).blur();
        }
        return;
      }
      if (typing || event.metaKey || event.ctrlKey || event.altKey) return;
      if (event.key === "/") {
        event.preventDefault();
        searchRef.current?.focus();
      } else if (event.key === "r") {
        event.preventDefault();
        void load();
      } else if (event.key === "c" && chart) {
        event.preventDefault();
        void navigator.clipboard?.writeText(chart.promql).then(() => {
          setNotice("PromQL copied");
          window.setTimeout(() => setNotice(null), 2000);
        });
      }
    },
    [filter, chart, load],
  );

  useEffect(() => {
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [onKeyDown]);

  const resync = async () => {
    setSyncing(true);
    setNotice(null);
    try {
      const data = await syncMetricCatalog();
      setCatalog(data);
      setNotice(`Catalogue re-seeded — ${data.families.length} families`);
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "The catalogue could not be re-seeded.");
    } finally {
      setSyncing(false);
    }
  };

  return (
    <div className="flex flex-col gap-4" data-view="observability-metrics">
      {overBudget.length > 0 ? (
        <div
          role="status"
          className="flex items-start gap-2 rounded-lg border border-caution-soft bg-caution-soft px-3 py-2.5 text-[12.5px]"
        >
          <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0 text-caution" aria-hidden="true" />
          <p>
            <span className="font-medium">
              {overBudget.length === 1 ? "One family is" : `${overBudget.length} families are`} at
              their cardinality cap.
            </span>{" "}
            Samples are being folded into the <code className="font-mono">other</code> series and
            counted in{" "}
            <code className="font-mono">omnion_registry_budget_exceeded</code>, so the totals stay
            honest but the per-label resolution is reduced:{" "}
            <span className="font-mono">{overBudget.join(", ")}</span>
          </p>
        </div>
      ) : null}

      {notice ? (
        <p role="status" className="text-[12.5px] text-positive">
          {notice}
        </p>
      ) : null}

      {error ? (
        <div role="alert" className="rounded-lg border border-danger-soft bg-danger-soft px-3 py-2.5 text-[12.5px]">
          {error}
        </div>
      ) : null}

      <section className="rounded-xl border border-line bg-surface">
        <header className="flex flex-col gap-2 border-b border-line px-4 py-3 sm:flex-row sm:items-center sm:justify-between">
          <div>
            <h2 className="text-[13.5px] font-semibold">Metric families</h2>
            <p className="text-[12px] text-muted">
              {catalog
                ? `${catalog.families.length} documented · ${catalog.global_budget} series cap`
                : "Loading the catalogue"}
            </p>
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <label className="flex items-center gap-1.5 text-[12px] text-muted">
              <Search className="h-3.5 w-3.5" aria-hidden="true" />
              <span className="sr-only">Filter families</span>
              <input
                ref={searchRef}
                value={filter}
                onChange={(event) => setFilter(event.target.value)}
                placeholder="Filter families"
                aria-label="Filter metric families"
                className="w-40 rounded-md border border-line bg-background px-2 py-1 text-[12.5px]"
              />
            </label>
            <button
              type="button"
              onClick={() => void load()}
              className="flex items-center gap-1.5 rounded-md border border-line px-2 py-1 text-[12.5px] hover:bg-quiet-soft"
            >
              <RefreshCw className="h-3.5 w-3.5" aria-hidden="true" />
              Refresh
            </button>
            <button
              type="button"
              onClick={() => void resync()}
              disabled={syncing}
              className="rounded-md border border-line px-2 py-1 text-[12.5px] hover:bg-quiet-soft disabled:opacity-60"
            >
              {syncing ? "Re-seeding…" : "Re-seed from registry"}
            </button>
          </div>
        </header>

        {loading ? (
          <LoadingTable columns={6} rows={6} />
        ) : visible.length === 0 ? (
          <EmptyState
            title={filter ? `No family matches “${filter}”` : "The catalogue is empty"}
            hint={
              filter
                ? "Clear the filter to see every documented family."
                : "The catalogue is seeded from the registry on every boot. Re-seed it to populate the table."
            }
            action={
              filter ? (
                <button
                  type="button"
                  onClick={() => {
                    setFilter("");
                    searchRef.current?.focus();
                  }}
                  className="rounded-md border border-line px-2 py-1 text-[12.5px]"
                >
                  Clear the filter
                </button>
              ) : (
                <button
                  type="button"
                  onClick={() => void resync()}
                  className="rounded-md border border-line px-2 py-1 text-[12.5px]"
                >
                  Re-seed from registry
                </button>
              )
            }
          />
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full border-collapse text-left text-[13px]">
              <thead>
                <tr className="text-[11.5px] uppercase tracking-wide text-muted">
                  <th className="px-4 py-2 font-medium">Family</th>
                  <th className="px-4 py-2 font-medium">Kind</th>
                  <th className="hidden px-4 py-2 font-medium sm:table-cell">Labels</th>
                  <th className="hidden px-4 py-2 font-medium sm:table-cell">Unit</th>
                  <th className="px-4 py-2 font-medium">Series</th>
                  <th className="px-4 py-2 font-medium">Last seen</th>
                </tr>
              </thead>
              <tbody>
                {visible.map((family) => (
                  <FamilyRow
                    key={family.name}
                    family={family}
                    selected={family.name === selected}
                    onSelect={() => setSelected(family.name)}
                  />
                ))}
              </tbody>
            </table>
          </div>
        )}
      </section>

      <section className="rounded-xl border border-line bg-surface">
        <header className="flex flex-col gap-2 border-b border-line px-4 py-3 sm:flex-row sm:items-center sm:justify-between">
          <div>
            <h2 className="text-[13.5px] font-semibold">
              {chart ? chart.metric : "Select a family"}
            </h2>
            <p className="text-[12px] text-muted">
              {chartLoading
                ? "Loading the series…"
                : chart
                  ? `${chart.series.length} series · ${chart.window_minutes} minute window · ${
                      chart.kind === "counter"
                        ? "per-minute delta"
                        : chart.kind === "gauge"
                          ? "last value"
                          : "mean per request"
                    }`
                  : "Pick a family above to chart it"}
            </p>
          </div>
          <div className="flex flex-wrap items-center gap-1.5">
            {WINDOWS.map((option) => (
              <button
                key={option.minutes}
                type="button"
                onClick={() => setWindowMinutes(option.minutes)}
                aria-pressed={windowMinutes === option.minutes}
                className={`rounded-md border px-2 py-1 text-[12px] ${
                  windowMinutes === option.minutes
                    ? "border-accent bg-accent-soft text-accent"
                    : "border-line hover:bg-quiet-soft"
                }`}
              >
                {option.label}
              </button>
            ))}
            {chart ? (
              <button
                type="button"
                onClick={() => {
                  void navigator.clipboard?.writeText(chart.promql).then(() => {
                    setNotice("PromQL copied");
                    window.setTimeout(() => setNotice(null), 2000);
                  });
                }}
                className="flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[12px] hover:bg-quiet-soft"
              >
                <Copy className="h-3 w-3" aria-hidden="true" />
                Copy as PromQL
              </button>
            ) : null}
          </div>
        </header>

        <div className="px-4 py-3">
          {chartError ? (
            <div role="alert" className="rounded-lg border border-danger-soft bg-danger-soft px-3 py-2.5 text-[12.5px]">
              {chartError}
            </div>
          ) : chartLoading ? (
            <div className="flex h-[180px] items-center justify-center text-[12.5px] text-muted" aria-busy="true">
              <Timer className="mr-2 h-3.5 w-3.5 animate-pulse" aria-hidden="true" />
              Loading the series…
            </div>
          ) : chart ? (
            <>
              <SeriesChart data={chart} />
              {chart.series.length > 0 ? (
                <ul className="mt-3 flex flex-wrap gap-3 text-[12px]">
                  {chart.series.slice(0, 8).map((series, index) => (
                    <li key={series.labels.join("|")} className="flex items-center gap-1.5">
                      <span
                        className={`h-2 w-2 rounded-full ${
                          index === 0
                            ? "bg-accent"
                            : index % 3 === 1
                              ? "bg-positive"
                              : index % 3 === 2
                                ? "bg-caution"
                                : "bg-muted"
                        }`}
                        aria-hidden="true"
                      />
                      <span className="font-mono text-[11.5px] text-muted">
                        {series.labels.join(" · ") || "(no labels)"}
                      </span>
                      <span className="tabular-nums">{formatValue(series.total, chart.unit)}</span>
                    </li>
                  ))}
                  {chart.series.length > 8 ? (
                    <li className="text-muted">+{chart.series.length - 8} more</li>
                  ) : null}
                </ul>
              ) : null}
              <details className="mt-3">
                <summary className="cursor-pointer text-[12px] text-muted">
                  PromQL for this selection
                </summary>
                <code className="mt-1 block overflow-x-auto rounded-md bg-quiet-soft px-2 py-1.5 font-mono text-[11.5px]">
                  {chart.promql}
                </code>
              </details>
            </>
          ) : (
            <EmptyState
              title="No family selected"
              hint="Choose a family from the catalogue above to chart it."
            />
          )}
        </div>
      </section>
    </div>
  );
}
