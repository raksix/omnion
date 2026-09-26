"use client";

/**
 * The pieces every analytics screen is built from (REQ-007, slice 2).
 *
 * One card, one chart, one bar panel, one table and one drawer — so the seven report screens
 * look like one product and behave the same way: skeletons while loading, a real assertion in
 * every empty state, a retry beside every error, and numbers formatted in one place. Nothing
 * here knows what a report *is*; it renders what a report answered.
 */
import { useEffect, useMemo, useState, type ReactNode } from "react";

import {
  AlertCircle,
  ArrowDown,
  ArrowUp,
  ChevronLeft,
  ChevronRight,
  ChevronsUpDown,
  RefreshCw,
  X,
} from "lucide-react";

import type { AnalyticsSeriesPoint } from "@/lib/api";
import { formatTimestamp } from "@/lib/format";

/** Thousands-separated integers, with a dash for "no number". */
export function formatCount(value: number | null | undefined): string {
  if (value === null || value === undefined || !Number.isFinite(value)) {
    return "—";
  }
  return new Intl.NumberFormat("en").format(value);
}

/** A ratio as a percentage; `null` stays a dash, never `0%`. */
export function formatRate(value: number | null | undefined, digits = 1): string {
  if (value === null || value === undefined || !Number.isFinite(value)) {
    return "—";
  }
  return `${(value * 100).toFixed(digits)}%`;
}

/** A duration in milliseconds as a person reads it (`1m 12s`, `840ms`). */
export function formatDuration(value: number | null | undefined): string {
  if (value === null || value === undefined || !Number.isFinite(value)) {
    return "—";
  }
  if (value < 1000) {
    return `${Math.round(value)}ms`;
  }
  const seconds = value / 1000;
  if (seconds < 60) {
    return `${seconds.toFixed(1)}s`;
  }
  const minutes = Math.floor(seconds / 60);
  const rest = Math.round(seconds % 60);
  return `${minutes}m ${rest}s`;
}

/** A share as the panel shows it (`42.3%`). */
export function formatShare(value: number): string {
  return `${(value * 100).toFixed(1)}%`;
}

/** The row heights a table shows while it is loading. */
export function LoadingRows({ rows = 6, label = "Loading" }: { rows?: number; label?: string }) {
  return (
    <div className="flex flex-col gap-2 p-4" role="status" aria-label={label}>
      {Array.from({ length: rows }).map((_, index) => (
        <div
          key={index}
          className="h-6 animate-pulse rounded-md bg-quiet-soft"
          style={{ width: `${92 - index * 6}%` }}
        />
      ))}
    </div>
  );
}

/** The state of a report that has nothing to show yet. */
export function EmptyPanel({
  title,
  children,
  action,
  dataAttribute = "analytics-empty",
}: {
  title: string;
  children?: ReactNode;
  action?: ReactNode;
  dataAttribute?: string;
}) {
  return (
    <div
      data-analytics-state={dataAttribute}
      className="flex flex-col items-center gap-3 rounded-xl border border-line bg-surface px-6 py-10 text-center"
    >
      <p className="text-[14px] font-medium text-ink">{title}</p>
      {children ? (
        <div className="max-w-xl text-[12.5px] leading-relaxed text-muted">{children}</div>
      ) : null}
      {action}
    </div>
  );
}

/** The state of a report that could not be answered. */
export function ErrorPanel({
  message,
  code,
  onRetry,
}: {
  message: string;
  code?: string;
  onRetry?: () => void;
}) {
  return (
    <div
      data-analytics-state="analytics-error"
      role="alert"
      className="flex flex-col items-start gap-2 rounded-xl border border-line bg-surface px-5 py-4"
    >
      <p className="flex items-center gap-2 text-[13px] font-medium text-ink">
        <AlertCircle className="size-4 text-accent" aria-hidden />
        {message}
      </p>
      {code ? <p className="font-mono text-[11.5px] text-muted">{code}</p> : null}
      {onRetry ? (
        <button
          type="button"
          onClick={onRetry}
          data-analytics-retry
          className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] text-ink transition hover:bg-quiet-soft"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Try again
        </button>
      ) : null}
    </div>
  );
}

/** A card with a title, an optional action and its own content. */
export function Panel({
  title,
  subtitle,
  action,
  children,
  className = "",
  bodyClassName = "",
  testId,
}: {
  title: string;
  subtitle?: string;
  action?: ReactNode;
  children: ReactNode;
  className?: string;
  bodyClassName?: string;
  testId?: string;
}) {
  return (
    <section
      data-analytics-panel={testId ?? title.toLowerCase().replace(/\s+/g, "-")}
      className={`flex flex-col rounded-xl border border-line bg-surface ${className}`}
    >
      <header className="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
        <div className="min-w-0 flex-1">
          <h2 className="truncate text-[13px] font-semibold text-ink">{title}</h2>
          {subtitle ? <p className="truncate text-[11.5px] text-muted">{subtitle}</p> : null}
        </div>
        {action}
      </header>
      <div className={`min-w-0 flex-1 ${bodyClassName}`}>{children}</div>
    </section>
  );
}

/** The change of a number against the period before it. */
export function DeltaBadge({
  value,
  previous,
  compare,
  available,
}: {
  value: number;
  previous: number | null;
  compare: boolean;
  available: boolean;
}) {
  if (!compare) {
    return null;
  }
  if (!available || previous === null) {
    return <span className="text-[11.5px] text-muted">no comparison</span>;
  }
  if (previous === 0) {
    return (
      <span className="rounded-md bg-quiet-soft px-1.5 py-0.5 text-[11.5px] text-muted">
        new
      </span>
    );
  }
  const change = (value - previous) / previous;
  const good = change >= 0;
  return (
    <span
      title={`Previous period: ${formatCount(previous)}`}
      className={`rounded-md px-1.5 py-0.5 text-[11.5px] font-medium ${
        good ? "bg-positive-soft text-positive" : "bg-accent-soft text-accent-strong"
      }`}
    >
      {good ? "+" : ""}
      {(change * 100).toFixed(1)}%
    </span>
  );
}

/** A tiny inline chart for one series of numbers. */
export function Sparkline({
  values,
  className = "",
}: {
  values: number[];
  className?: string;
}) {
  const max = Math.max(1, ...values);
  const step = values.length > 1 ? 100 / (values.length - 1) : 100;
  const points = values
    .map((value, index) => `${(index * step).toFixed(2)},${(28 - (value / max) * 26).toFixed(2)}`)
    .join(" ");

  return (
    <svg
      viewBox="0 0 100 30"
      preserveAspectRatio="none"
      aria-hidden
      className={`h-8 w-full ${className}`}
    >
      <polyline
        points={points}
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
        vectorEffect="non-scaling-stroke"
      />
    </svg>
  );
}

/** One headline number with its delta and, where it exists, its own shape. */
export function KpiCard({
  label,
  value,
  previous,
  compare,
  available,
  spark,
  testId,
}: {
  label: string;
  value: number;
  previous: number | null;
  compare: boolean;
  available: boolean;
  spark?: number[];
  testId: string;
}) {
  return (
    <div
      data-analytics-kpi={testId}
      className="flex flex-col gap-1.5 rounded-xl border border-line bg-surface px-4 py-3"
    >
      <span className="text-[11.5px] font-medium tracking-wide text-muted uppercase">
        {label}
      </span>
      <span className="flex items-baseline gap-2">
        <span data-analytics-kpi-value className="font-mono text-[22px] leading-none text-ink">
          {formatCount(value)}
        </span>
        <DeltaBadge value={value} previous={previous} compare={compare} available={available} />
      </span>
      {spark && spark.length > 1 ? (
        <span className="text-accent">
          <Sparkline values={spark} />
        </span>
      ) : null}
    </div>
  );
}

/** The colours the series chart draws with. */
const VISITORS_COLOUR = "var(--color-accent)";
const PAGEVIEWS_COLOUR = "var(--color-positive)";
const PREVIOUS_COLOUR = "var(--color-muted)";

/**
 * The overview's line chart: visitors and pageviews over the range, the previous period as a
 * muted dashed line, and a readout that works with a mouse and with a keyboard (the chart
 * itself is one focus stop; the arrow keys walk the buckets).
 */
export function SeriesChart({
  points,
  compare,
  height = 240,
  emptyLabel = "No data in this range",
}: {
  points: AnalyticsSeriesPoint[];
  compare: boolean;
  height?: number;
  emptyLabel?: string;
}) {
  const [active, setActive] = useState<number | null>(null);
  const manageable = points.length > 0;

  const max = useMemo(() => {
    const values = points.flatMap((point) => [
      point.visitors,
      point.pageviews,
      point.previous_visitors ?? 0,
      point.previous_pageviews ?? 0,
    ]);
    return Math.max(1, ...values);
  }, [points]);

  useEffect(() => {
    setActive(null);
  }, [points]);

  if (!manageable) {
    return (
      <p className="px-4 py-10 text-center text-[12.5px] text-muted">{emptyLabel}</p>
    );
  }

  const width = 100;
  const inner = { left: 0, right: 100, top: 4, bottom: height - 22 };
  const step = points.length > 1 ? width / (points.length - 1) : width;
  const x = (index: number) => inner.left + index * step;
  const y = (value: number) => inner.bottom - (value / max) * (inner.bottom - inner.top);
  const line = (key: "visitors" | "pageviews" | "previous_visitors" | "previous_pageviews") =>
    points
      .map((point, index) => `${index === 0 ? "M" : "L"}${x(index).toFixed(2)},${y(point[key] ?? 0).toFixed(2)}`)
      .join(" ");
  const area = (key: "visitors" | "pageviews") =>
    `${line(key)} L${x(points.length - 1).toFixed(2)},${inner.bottom} L${inner.left},${inner.bottom} Z`;
  const labelEvery = Math.max(1, Math.ceil(points.length / 8));
  const current = active === null ? null : points[active];

  const onPointerMove = (event: React.PointerEvent<SVGSVGElement>) => {
    const box = event.currentTarget.getBoundingClientRect();
    if (box.width <= 0) {
      return;
    }
    const ratio = (event.clientX - box.left) / box.width;
    const index = Math.round(((ratio * width - inner.left) / step) || 0);
    setActive(Math.min(points.length - 1, Math.max(0, index)));
  };

  return (
    <div className="flex flex-col gap-2 px-4 py-3">
      <div className="flex flex-wrap items-center gap-3 text-[11.5px] text-muted">
        <span className="flex items-center gap-1.5">
          <span aria-hidden className="inline-block size-2 rounded-full" style={{ background: VISITORS_COLOUR }} />
          Visitors
        </span>
        <span className="flex items-center gap-1.5">
          <span aria-hidden className="inline-block size-2 rounded-full" style={{ background: PAGEVIEWS_COLOUR }} />
          Page views
        </span>
        {compare ? (
          <span className="flex items-center gap-1.5">
            <span aria-hidden className="inline-block h-0.5 w-3 bg-muted" />
            Previous period
          </span>
        ) : null}
        {current ? (
          <span
            data-analytics-chart-readout
            className="ml-auto font-mono text-ink"
          >
            {current.label} · visitors {formatCount(current.visitors)} · views{" "}
            {formatCount(current.pageviews)}
            {compare ? ` · previous ${formatCount(current.previous_visitors ?? 0)}` : ""}
          </span>
        ) : null}
      </div>

      <svg
        role="img"
        tabIndex={0}
        aria-label={`Series of ${points.length} buckets, highest value ${max}. Use the arrow keys to read each bucket.`}
        viewBox={`0 0 ${width} ${height}`}
        preserveAspectRatio="none"
        className="h-[240px] w-full touch-none outline-none focus-visible:ring-2 focus-visible:ring-accent"
        style={{ height }}
        onPointerMove={onPointerMove}
        onPointerLeave={() => setActive(null)}
        onKeyDown={(event) => {
          if (event.key === "ArrowRight" || event.key === "ArrowLeft") {
            event.preventDefault();
            const base = active ?? 0;
            const next = event.key === "ArrowRight" ? base + 1 : base - 1;
            setActive(Math.min(points.length - 1, Math.max(0, next)));
          }
          if (event.key === "Home") {
            setActive(0);
          }
          if (event.key === "End") {
            setActive(points.length - 1);
          }
        }}
      >
        {[0.25, 0.5, 0.75, 1].map((fraction) => (
          <line
            key={fraction}
            x1={inner.left}
            x2={inner.right}
            y1={y(max * fraction)}
            y2={y(max * fraction)}
            stroke="var(--color-line)"
            strokeWidth="0.5"
            vectorEffect="non-scaling-stroke"
          />
        ))}
        <path d={area("pageviews")} fill={PAGEVIEWS_COLOUR} opacity="0.08" />
        <path d={area("visitors")} fill={VISITORS_COLOUR} opacity="0.12" />
        {compare ? (
          <path
            d={line("previous_visitors")}
            fill="none"
            stroke={PREVIOUS_COLOUR}
            strokeWidth="1"
            strokeDasharray="3 3"
            vectorEffect="non-scaling-stroke"
            opacity="0.7"
          />
        ) : null}
        <path
          d={line("pageviews")}
          fill="none"
          stroke={PAGEVIEWS_COLOUR}
          strokeWidth="1.5"
          vectorEffect="non-scaling-stroke"
        />
        <path
          d={line("visitors")}
          fill="none"
          stroke={VISITORS_COLOUR}
          strokeWidth="2"
          vectorEffect="non-scaling-stroke"
        />
        {current && active !== null ? (
          <line
            x1={x(active)}
            x2={x(active)}
            y1={inner.top}
            y2={inner.bottom}
            stroke={VISITORS_COLOUR}
            strokeWidth="1"
            strokeDasharray="2 2"
            vectorEffect="non-scaling-stroke"
          />
        ) : null}
      </svg>

      <div className="flex justify-between text-[11px] text-muted">
        {points.map((point, index) =>
          index % labelEvery === 0 || index === points.length - 1 ? (
            <span key={point.bucket} data-analytics-axis-label>
              {point.label}
            </span>
          ) : (
            <span key={point.bucket} aria-hidden />
          ),
        )}
      </div>
    </div>
  );
}

/** One bar panel: a dimension, its values and their share of the panel's total. */
export function BarPanel({
  title,
  rows,
  emptyLabel = "Nothing recorded",
}: {
  title: string;
  rows: { value: string; visitors: number | null; views: number | null }[];
  emptyLabel?: string;
}) {
  const total = rows.reduce((sum, row) => sum + (row.visitors ?? 0), 0);

  return (
    <Panel title={title} bodyClassName="px-4 py-3">
      {rows.length === 0 ? (
        <p className="py-6 text-center text-[12px] text-muted">{emptyLabel}</p>
      ) : (
        <ul className="flex flex-col gap-2.5">
          {rows.map((row) => {
            const share = total > 0 ? (row.visitors ?? 0) / total : 0;
            return (
              <li key={row.value} className="flex flex-col gap-1">
                <div className="flex items-baseline justify-between gap-2 text-[12.5px]">
                  <span className="truncate text-ink">{row.value}</span>
                  <span className="shrink-0 font-mono text-muted">
                    {formatCount(row.visitors)}{" "}
                    <span className="text-[11px]">({formatShare(share)})</span>
                  </span>
                </div>
                <div className="h-1.5 w-full overflow-hidden rounded-full bg-quiet-soft">
                  <div
                    className="h-full rounded-full bg-accent"
                    style={{ width: `${Math.max(2, share * 100).toFixed(1)}%` }}
                  />
                </div>
              </li>
            );
          })}
        </ul>
      )}
    </Panel>
  );
}

/** A column of a report table. */
export type Column<Row> = {
  key: string;
  label: string;
  align?: "left" | "right";
  numeric?: boolean;
  sortable?: boolean;
  render: (row: Row) => ReactNode;
};

/**
 * The report table: sortable headers on a wide screen, cards below `lg` — the spec's own mobile
 * rule, and a phone reads a card list better than a table that scrolls sideways off the screen.
 * Both layouts render the same rows through the same columns, so they cannot disagree.
 */
export function DataTable<Row>({
  columns,
  rows,
  sort,
  direction,
  onSort,
  onRowClick,
  rowKey,
  footer,
}: {
  columns: Column<Row>[];
  rows: Row[];
  sort?: string;
  direction?: "asc" | "desc";
  onSort?: (key: string) => void;
  onRowClick?: (row: Row) => void;
  rowKey: (row: Row) => string;
  footer?: ReactNode;
}) {
  const sortable = columns.filter((column) => column.sortable && onSort);
  return (
    <>
      <div className="lg:hidden">
        {sortable.length > 0 ? (
          <label className="flex items-center gap-2 px-3 pt-3 text-[11px] tracking-wide text-muted uppercase">
            Sort
            <select
              value={sort ?? sortable[0].key}
              data-analytics-sort-select
              onChange={(event) => onSort?.(event.target.value)}
              className="rounded-lg border border-line bg-canvas px-2 py-1 text-[12px] text-ink"
            >
              {sortable.map((column) => (
                <option key={column.key} value={column.key}>
                  {column.label}
                </option>
              ))}
            </select>
          </label>
        ) : null}
        <ul data-analytics-cards className="flex flex-col gap-2 p-3">
          {rows.map((row) => (
            <li key={rowKey(row)}>
              <div
                role={onRowClick ? "button" : undefined}
                tabIndex={onRowClick ? 0 : undefined}
                onClick={onRowClick ? () => onRowClick(row) : undefined}
                onKeyDown={
                  onRowClick
                    ? (event) => {
                        if (event.key === "Enter" || event.key === " ") {
                          event.preventDefault();
                          onRowClick(row);
                        }
                      }
                    : undefined
                }
                className={`flex flex-col gap-1.5 rounded-lg border border-line p-3 ${
                  onRowClick ? "cursor-pointer transition hover:bg-canvas" : ""
                }`}
              >
                {columns.map((column) => (
                  <div key={column.key} className="flex items-baseline justify-between gap-3">
                    <span className="shrink-0 text-[11px] tracking-wide text-muted uppercase">
                      {column.label}
                    </span>
                    <span
                      className={`min-w-0 text-right break-words ${
                        column.numeric ? "font-mono" : ""
                      }`}
                    >
                      {column.render(row)}
                    </span>
                  </div>
                ))}
              </div>
            </li>
          ))}
        </ul>
      </div>

      <div className="hidden min-w-0 overflow-x-auto lg:block">
      <table data-analytics-table className="w-full min-w-[720px] border-collapse text-[12.5px]">
        <thead>
          <tr className="border-b border-line text-left text-[11.5px] tracking-wide text-muted uppercase">
            {columns.map((column) => {
              const active = sort === column.key;
              return (
                <th
                  key={column.key}
                  scope="col"
                  className={`px-3 py-2 font-medium ${column.align === "right" ? "text-right" : ""}`}
                >
                  {column.sortable && onSort ? (
                    <button
                      type="button"
                      onClick={() => onSort(column.key)}
                      data-analytics-sort={column.key}
                      aria-sort={active ? (direction === "asc" ? "ascending" : "descending") : "none"}
                      className={`inline-flex items-center gap-1 rounded transition hover:text-ink ${
                        active ? "text-accent-strong" : ""
                      }`}
                    >
                      {column.label}
                      {active ? (
                        direction === "asc" ? (
                          <ArrowUp className="size-3" aria-hidden />
                        ) : (
                          <ArrowDown className="size-3" aria-hidden />
                        )
                      ) : (
                        <ChevronsUpDown className="size-3 opacity-60" aria-hidden />
                      )}
                    </button>
                  ) : (
                    column.label
                  )}
                </th>
              );
            })}
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => (
            <tr
              key={rowKey(row)}
              data-analytics-row
              onClick={onRowClick ? () => onRowClick(row) : undefined}
              className={`border-b border-line/60 last:border-b-0 ${
                onRowClick ? "cursor-pointer transition hover:bg-canvas" : ""
              }`}
            >
              {columns.map((column) => (
                <td
                  key={column.key}
                  className={`px-3 py-2 align-middle ${
                    column.align === "right" ? "text-right" : ""
                  } ${column.numeric ? "font-mono" : ""}`}
                >
                  {column.render(row)}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
        {footer ? <tfoot>{footer}</tfoot> : null}
      </table>
      </div>
    </>
  );
}

/** Paging controls of a report table. */
export function Pager({
  page,
  perPage,
  total,
  onPage,
}: {
  page: number;
  perPage: number;
  total: number;
  onPage: (page: number) => void;
}) {
  const pages = Math.max(1, Math.ceil(total / perPage));
  const first = total === 0 ? 0 : (page - 1) * perPage + 1;
  const last = Math.min(total, page * perPage);

  return (
    <div className="flex flex-wrap items-center gap-2 border-t border-line px-4 py-2.5 text-[12px] text-muted">
      <span data-analytics-pager>
        {first}–{last} of {formatCount(total)}
      </span>
      <span className="ml-auto flex items-center gap-1.5">
        <button
          type="button"
          disabled={page <= 1}
          onClick={() => onPage(page - 1)}
          className="flex items-center gap-1 rounded-lg border border-line px-2 py-1 transition enabled:hover:bg-quiet-soft disabled:opacity-40"
        >
          <ChevronLeft className="size-3.5" aria-hidden />
          Previous
        </button>
        <span className="font-mono">
          {page}/{pages}
        </span>
        <button
          type="button"
          disabled={page >= pages}
          onClick={() => onPage(page + 1)}
          className="flex items-center gap-1 rounded-lg border border-line px-2 py-1 transition enabled:hover:bg-quiet-soft disabled:opacity-40"
        >
          Next
          <ChevronRight className="size-3.5" aria-hidden />
        </button>
      </span>
    </div>
  );
}

/** The right-hand drawer a table row opens. */
export function Drawer({
  title,
  subtitle,
  onClose,
  children,
  footer,
}: {
  title: string;
  subtitle?: string;
  onClose: () => void;
  children: ReactNode;
  footer?: ReactNode;
}) {
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        onClose();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <div data-analytics-drawer className="fixed inset-0 z-40 flex justify-end">
      <button
        type="button"
        aria-label="Close detail"
        onClick={onClose}
        className="absolute inset-0 bg-ink/30"
      />
      <aside
        role="dialog"
        aria-modal="true"
        aria-label={title}
        className="relative flex h-full w-full max-w-xl flex-col border-l border-line bg-surface shadow-xl"
      >
        <header className="flex items-start gap-3 border-b border-line px-4 py-3">
          <div className="min-w-0 flex-1">
            <h2 className="truncate text-[14px] font-semibold text-ink">{title}</h2>
            {subtitle ? <p className="truncate text-[12px] text-muted">{subtitle}</p> : null}
          </div>
          <button
            type="button"
            onClick={onClose}
            aria-label="Close detail"
            data-analytics-drawer-close
            className="rounded-lg border border-line p-1.5 text-muted transition hover:bg-quiet-soft hover:text-ink"
          >
            <X className="size-4" aria-hidden />
          </button>
        </header>
        <div className="min-h-0 flex-1 overflow-y-auto">{children}</div>
        {footer ? <footer className="border-t border-line px-4 py-3">{footer}</footer> : null}
      </aside>
    </div>
  );
}

/** A small statistic inside a drawer. */
export function Stat({ label, value }: { label: string; value: ReactNode }) {
  return (
    <div className="flex flex-col gap-0.5">
      <span className="text-[11px] tracking-wide text-muted uppercase">{label}</span>
      <span className="font-mono text-[14px] text-ink">{value}</span>
    </div>
  );
}

/** The last-seen cell of a report table. */
export function LastSeen({ value }: { value: string | null }) {
  if (!value) {
    return <span className="text-muted">—</span>;
  }
  return <span className="text-muted">{formatTimestamp(value)}</span>;
}
