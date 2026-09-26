"use client";

/**
 * `/analytics/pages` — the page report (REQ-007, slice 2).
 *
 * The table an editor actually works from: views, visitors, time on page, bounce rate, entrances
 * and exits per path, narrowing as the filters combine (path, title, device, country, source).
 * Sorting and paging are part of the URL, so a sorted, filtered table is a link — and the row
 * opens the drawer with that page's own series.
 */
import { useState } from "react";

import Link from "next/link";

import { fetchAnalyticsPageSeries, fetchAnalyticsPages, type AnalyticsPageRow } from "@/lib/api";

import { useAnalytics, useReport } from "./analytics-shell";
import {
  DataTable,
  Drawer,
  ErrorPanel,
  EmptyPanel,
  LoadingRows,
  Pager,
  Panel,
  SeriesChart,
  Stat,
  formatCount,
  formatDuration,
  formatRate,
  type Column,
} from "./parts";

/** The filters the screen writes into the URL. */
const FILTERS = [
  { key: "path", label: "Path contains", placeholder: "qa/landing" },
  { key: "title", label: "Title contains", placeholder: "Pricing" },
  { key: "country", label: "Country", placeholder: "TR" },
  { key: "source", label: "Source", placeholder: "newsletter" },
] as const;

/** `true` when the text is a country code the API will accept. */
function validCountry(value: string): boolean {
  return /^[A-Za-z]{2}$/.test(value.trim());
}

/** `true` when the text is a filter this screen (and the API) can carry. */
function invalidReason(key: string, value: string): string | null {
  if (key === "country" && value !== "" && !validCountry(value)) {
    return "A country is a two-letter code (for example TR).";
  }
  return null;
}

/** The device choices the API accepts. */
const DEVICES = ["desktop", "mobile", "tablet", "other"];

/** One page's own series, in the drawer. */
function PageSeries({ path }: { path: string }) {
  const { data, status, error } = useReport(fetchAnalyticsPageSeries, { path });

  if (status === "error" && error) {
    return <ErrorPanel message={error.message} code={error.code} />;
  }
  if (!data) {
    return <LoadingRows rows={5} label="Loading the page series" />;
  }

  const visitors = data.map((point) => point.visitors);
  const pageviews = data.map((point) => point.pageviews);

  return (
    <div className="flex flex-col gap-3 p-4">
      <div className="grid grid-cols-3 gap-3">
        <Stat label="Views" value={formatCount(pageviews.reduce((sum, value) => sum + value, 0))} />
        <Stat
          label="Peak visitors"
          value={formatCount(visitors.length ? Math.max(...visitors) : 0)}
        />
        <Stat label="Buckets" value={formatCount(data.length)} />
      </div>
      <SeriesChart points={data} compare={false} height={200} />
    </div>
  );
}

/** The page report. */
export function AnalyticsPagesView() {
  const { params, patch, from, to } = useAnalytics();
  const [open, setOpen] = useState<AnalyticsPageRow | null>(null);
  const [invalid, setInvalid] = useState<Record<string, string>>({});
  const sort = params.get("sort") ?? "views";
  const direction = (params.get("dir") === "asc" ? "asc" : "desc") as "asc" | "desc";
  const page = Math.max(1, Number(params.get("page") ?? "1") || 1);

  const { data, status, error, retry } = useReport(fetchAnalyticsPages, {
    path: params.get("path") ?? undefined,
    title: params.get("title") ?? undefined,
    device: params.get("device") ?? undefined,
    country: params.get("country") ?? undefined,
    source: params.get("source") ?? undefined,
    sort,
    dir: direction,
    page,
    per_page: 50,
  });

  const onSort = (key: string) => {
    if (key === sort) {
      patch({ dir: direction === "asc" ? "desc" : "asc" });
    } else {
      patch({ sort: key, dir: "desc" });
    }
  };

  const columns: Column<AnalyticsPageRow>[] = [
    {
      key: "path",
      label: "Page",
      sortable: true,
      render: (row) => (
        <button
          type="button"
          onClick={() => setOpen(row)}
          data-analytics-page-link
          className="max-w-[280px] truncate text-left text-ink hover:text-accent-strong hover:underline"
        >
          {row.path}
        </button>
      ),
    },
    {
      key: "title",
      label: "Title",
      render: (row) => (
        <span className="block max-w-[220px] truncate text-muted">{row.title ?? "—"}</span>
      ),
    },
    { key: "views", label: "Views", align: "right", numeric: true, sortable: true, render: (row) => formatCount(row.views) },
    {
      key: "visitors",
      label: "Visitors",
      align: "right",
      numeric: true,
      sortable: true,
      render: (row) => formatCount(row.visitors),
    },
    {
      key: "views_per_visitor",
      label: "Views/visitor",
      align: "right",
      numeric: true,
      sortable: true,
      render: (row) => (row.views_per_visitor === null ? "—" : row.views_per_visitor.toFixed(2)),
    },
    {
      key: "avg_time",
      label: "Avg time",
      align: "right",
      numeric: true,
      sortable: true,
      render: (row) => formatDuration(row.avg_time_ms),
    },
    {
      key: "bounce_rate",
      label: "Bounce rate",
      align: "right",
      numeric: true,
      sortable: true,
      render: (row) => formatRate(row.bounce_rate),
    },
    {
      key: "entrances",
      label: "Entrances",
      align: "right",
      numeric: true,
      sortable: true,
      render: (row) => formatCount(row.entrances),
    },
    {
      key: "exits",
      label: "Exits",
      align: "right",
      numeric: true,
      sortable: true,
      render: (row) => formatCount(row.exits),
    },
  ];

  const activeFilters = FILTERS.filter((filter) => params.get(filter.key)).length;

  return (
    <div className="flex flex-col gap-4" data-analytics-pages>
      <Panel title="Filters" bodyClassName="px-4 py-3" testId="filters">
        <div className="flex flex-wrap items-end gap-3">
          {FILTERS.map((filter) => {
            const message = invalid[filter.key];
            const commit = (raw: string) => {
              const value = raw.trim();
              const reason = invalidReason(filter.key, value);
              setInvalid((current) => {
                const next = { ...current };
                if (reason) {
                  next[filter.key] = reason;
                } else {
                  delete next[filter.key];
                }
                return next;
              });
              if (reason || (params.get(filter.key) ?? "") === value) {
                return;
              }
              patch({
                [filter.key]:
                  filter.key === "country" ? value.toUpperCase() || null : value || null,
              });
            };
            return (
              <label
                key={filter.key}
                className="flex min-w-[150px] flex-col gap-1 text-[11px] tracking-wide text-muted uppercase"
              >
                {filter.label}
                <input
                  type="text"
                  defaultValue={params.get(filter.key) ?? ""}
                  placeholder={filter.placeholder}
                  data-analytics-filter={filter.key}
                  aria-invalid={message ? true : undefined}
                  onKeyDown={(event) => {
                    if (event.key === "Enter") {
                      commit(event.currentTarget.value);
                    }
                  }}
                  onBlur={(event) => commit(event.currentTarget.value)}
                  className={`rounded-lg border bg-canvas px-2 py-1.5 text-[12.5px] text-ink ${
                    message ? "border-accent" : "border-line"
                  }`}
                />
                {message ? (
                  <span
                    data-analytics-filter-error={filter.key}
                    className="text-[11px] normal-case text-accent-strong"
                  >
                    {message}
                  </span>
                ) : null}
              </label>
            );
          })}
          <label className="flex flex-col gap-1 text-[11px] tracking-wide text-muted uppercase">
            Device
            <select
              value={params.get("device") ?? ""}
              data-analytics-filter="device"
              onChange={(event) => patch({ device: event.target.value || null })}
              className="rounded-lg border border-line bg-canvas px-2 py-1.5 text-[12.5px] text-ink"
            >
              <option value="">Any</option>
              {DEVICES.map((device) => (
                <option key={device} value={device}>
                  {device}
                </option>
              ))}
            </select>
          </label>
          {activeFilters > 0 ? (
            <button
              type="button"
              onClick={() =>
                patch({
                  path: null,
                  title: null,
                  device: null,
                  country: null,
                  source: null,
                })
              }
              data-analytics-clear-filters
              className="rounded-lg border border-line px-2.5 py-1.5 text-[12px] text-ink transition hover:bg-quiet-soft"
            >
              Clear filters
            </button>
          ) : null}
        </div>
      </Panel>

      {status === "error" && error ? (
        <ErrorPanel message={error.message} code={error.code} onRetry={retry} />
      ) : (
        <Panel
          title="Pages"
          subtitle={
            data ? `${formatCount(data.total)} page${data.total === 1 ? "" : "s"} in this range` : undefined
          }
          bodyClassName="p-0"
          testId="pages-table"
        >
          {status === "loading" && !data ? (
            <LoadingRows rows={8} label="Loading the page report" />
          ) : data && data.rows.length === 0 ? (
            <EmptyPanel title="No page views match these filters">
              <p>
                {activeFilters > 0
                  ? "Loosen a filter or widen the range — the table shows exactly what the filters leave."
                  : "Nothing recorded in this range yet."}
              </p>
            </EmptyPanel>
          ) : data ? (
            <>
              <DataTable
                columns={columns}
                rows={data.rows}
                sort={sort}
                direction={direction}
                onSort={onSort}
                onRowClick={(row) => setOpen(row)}
                rowKey={(row) => row.path}
              />
              <Pager
                page={data.page}
                perPage={data.per_page}
                total={data.total}
                onPage={(next) => patch({ page: String(next) })}
              />
            </>
          ) : null}
        </Panel>
      )}

      {open ? (
        <Drawer
          title={open.path}
          subtitle={open.title ?? undefined}
          onClose={() => setOpen(null)}
          footer={
            <div className="flex flex-wrap items-center gap-3 text-[12px] text-muted">
              <span className="font-mono">views {formatCount(open.views)}</span>
              <span className="font-mono">visitors {formatCount(open.visitors)}</span>
              <span className="font-mono">avg {formatDuration(open.avg_time_ms)}</span>
              <Link
                href={`/analytics/pages?${new URLSearchParams({
                  from,
                  to,
                  path: open.path,
                }).toString()}`}
                className="ml-auto text-accent-strong hover:underline"
              >
                Filter the table by this page
              </Link>
            </div>
          }
        >
          <PageSeries path={open.path} />
        </Drawer>
      ) : null}
    </div>
  );
}
