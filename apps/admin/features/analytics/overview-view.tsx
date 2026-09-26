"use client";

/**
 * `/analytics` — the overview (REQ-007, slice 2).
 *
 * The five headline numbers, the series behind them, and the three panels an operator looks at
 * first: the busiest pages, the busiest sources and the device split. Two states are spelled out
 * rather than implied: a range older than the site's retention says its numbers come from the
 * daily rollups, and a comparison against a period that holds nothing says "no comparison"
 * instead of drawing a delta against zero. A site with nothing recorded yet gets the snippet it
 * needs — not an empty chart with no way forward.
 */
import { useEffect, useState } from "react";

import { Check, Copy, ExternalLink } from "lucide-react";
import Link from "next/link";

import {
  fetchAnalyticsOverview,
  fetchAnalyticsSnippet,
  type AnalyticsSnippet,
} from "@/lib/api";

import { useAnalytics, useReport } from "./analytics-shell";
import {
  BarPanel,
  ErrorPanel,
  EmptyPanel,
  KpiCard,
  LoadingRows,
  Panel,
  SeriesChart,
  formatCount,
} from "./parts";

/** The headline numbers, in the order the brief lists them. */
const KPI_ORDER = [
  { key: "visitors", label: "Visitors", testId: "visitors" },
  { key: "pageviews", label: "Page views", testId: "pageviews" },
  { key: "conversions", label: "Conversions", testId: "conversions" },
  { key: "forms", label: "Forms", testId: "forms" },
  { key: "downloads", label: "Downloads", testId: "downloads" },
] as const;

/** The snippet a site pastes, with one copy control. */
function SnippetBlock({ siteId }: { siteId: string }) {
  const [snippet, setSnippet] = useState<AnalyticsSnippet | null>(null);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    let cancelled = false;
    fetchAnalyticsSnippet(siteId)
      .then((answer) => {
        if (!cancelled) {
          setSnippet(answer);
        }
      })
      .catch(() => {
        // The snippet is a courtesy in the empty state; the screen still explains itself.
      });
    return () => {
      cancelled = true;
    };
  }, [siteId]);

  if (!snippet) {
    return null;
  }

  return (
    <div className="flex w-full flex-col gap-2 text-left">
      <pre
        data-analytics-snippet
        className="max-w-full overflow-x-auto rounded-lg border border-line bg-canvas px-3 py-2 text-left font-mono text-[11.5px] text-ink"
      >
        {snippet.snippet}
      </pre>
      <div className="flex flex-wrap items-center gap-2">
        <button
          type="button"
          data-analytics-copy-snippet
          onClick={() => {
            void navigator.clipboard?.writeText(snippet.snippet).then(() => {
              setCopied(true);
              window.setTimeout(() => setCopied(false), 2_000);
            });
          }}
          className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] text-ink transition hover:bg-quiet-soft"
        >
          {copied ? (
            <Check className="size-3.5 text-positive" aria-hidden />
          ) : (
            <Copy className="size-3.5" aria-hidden />
          )}
          {copied ? "Copied" : "Copy snippet"}
        </button>
        <span className="font-mono text-[11.5px] text-muted">
          site key {snippet.site.key}
        </span>
      </div>
    </div>
  );
}

/** The overview screen. */
export function AnalyticsOverviewView() {
  const { siteId, compare, exportNote } = useAnalytics();
  const { data, status, error, retry } = useReport(fetchAnalyticsOverview);

  if (status === "error" && error) {
    return <ErrorPanel message={error.message} code={error.code} onRetry={retry} />;
  }

  if (status === "loading" && !data) {
    return (
      <div className="flex flex-col gap-4">
        <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-5">
          {KPI_ORDER.map((entry) => (
            <div
              key={entry.key}
              className="h-[86px] animate-pulse rounded-xl border border-line bg-surface"
            />
          ))}
        </div>
        <Panel title="Traffic" bodyClassName="p-0">
          <LoadingRows rows={7} label="Loading the overview" />
        </Panel>
      </div>
    );
  }

  if (!data || !siteId) {
    return null;
  }

  const visitors = data.series.map((point) => point.visitors);
  const pageviews = data.series.map((point) => point.pageviews);
  const empty = data.kpis.visitors.value === 0 && data.kpis.pageviews.value === 0;

  return (
    <div className="flex flex-col gap-4" data-analytics-overview>
      {!data.exact ? (
        <p
          data-analytics-approximate
          className="rounded-lg border border-line bg-caution-soft px-3 py-2 text-[12px] text-caution"
        >
          This range reaches past the site&apos;s retention window, so the numbers come from the
          daily rollups: a visitor is counted once per day there, while a raw row would count them
          once for the whole period.
        </p>
      ) : null}
      {data.compare && !data.previous_has_data ? (
        <p
          data-analytics-no-comparison
          className="rounded-lg border border-line bg-quiet-soft px-3 py-2 text-[12px] text-muted"
        >
          No comparison: {data.previous_range.from} → {data.previous_range.to} holds no traffic.
        </p>
      ) : null}

      {empty ? (
        <EmptyPanel
          title="No data yet — nothing recorded in this range"
          dataAttribute="analytics-overview-empty"
        >
          <p>
            Paste this snippet into the site&apos;s pages and the first beacon shows up here.
            Omnion&apos;s tracker is cookieless: it writes nothing to the visitor&apos;s browser
            and stores a daily-salted hash instead of an address.
          </p>
          <div className="mt-3" />
          <SnippetBlock siteId={siteId} />
        </EmptyPanel>
      ) : (
        <>
          <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-5">
            {KPI_ORDER.map((entry) => {
              const metric = data.kpis[entry.key];
              return (
                <KpiCard
                  key={entry.key}
                  testId={entry.testId}
                  label={entry.label}
                  value={metric.value}
                  previous={metric.previous}
                  compare={compare}
                  available={data.previous_has_data}
                  spark={
                    entry.key === "visitors"
                      ? visitors
                      : entry.key === "pageviews"
                        ? pageviews
                        : undefined
                  }
                />
              );
            })}
          </div>

          <Panel
            title="Traffic"
            subtitle={
              data.granularity === "hour"
                ? "Visitors and page views per hour"
                : "Visitors and page views per day"
            }
            bodyClassName="p-0"
            testId="traffic"
          >
            <SeriesChart points={data.series} compare={compare} />
          </Panel>

          <div className="grid gap-4 lg:grid-cols-3">
            <Panel
              title="Top pages"
              testId="top-pages"
              action={
                <Link
                  href={`/analytics/pages?${new URLSearchParams({
                    from: data.range.from,
                    to: data.range.to,
                  }).toString()}`}
                  className="flex items-center gap-1 text-[12px] text-accent-strong hover:underline"
                >
                  View all
                  <ExternalLink className="size-3" aria-hidden />
                </Link>
              }
              bodyClassName="px-4 py-3"
            >
              {data.top_pages.length === 0 ? (
                <p className="py-6 text-center text-[12px] text-muted">Nothing recorded</p>
              ) : (
                <ul className="flex flex-col gap-2.5">
                  {data.top_pages.map((row) => (
                    <li key={row.value} className="flex items-baseline justify-between gap-3">
                      <Link
                        href={`/analytics/pages?${new URLSearchParams({
                          from: data.range.from,
                          to: data.range.to,
                          path: row.value,
                        }).toString()}`}
                        className="truncate text-[12.5px] text-ink hover:text-accent-strong hover:underline"
                      >
                        {row.value}
                      </Link>
                      <span className="shrink-0 font-mono text-[12px] text-muted">
                        {formatCount(row.views)}
                      </span>
                    </li>
                  ))}
                </ul>
              )}
            </Panel>

            <Panel
              title="Top sources"
              testId="top-sources"
              action={
                <Link
                  href={`/analytics/sources?${new URLSearchParams({
                    from: data.range.from,
                    to: data.range.to,
                  }).toString()}`}
                  className="flex items-center gap-1 text-[12px] text-accent-strong hover:underline"
                >
                  View all
                  <ExternalLink className="size-3" aria-hidden />
                </Link>
              }
              bodyClassName="px-4 py-3"
            >
              {data.top_sources.length === 0 ? (
                <p className="py-6 text-center text-[12px] text-muted">Nothing recorded</p>
              ) : (
                <ul className="flex flex-col gap-2.5">
                  {data.top_sources.map((row) => (
                    <li key={row.value} className="flex items-baseline justify-between gap-3">
                      <Link
                        href={`/analytics/sources?${new URLSearchParams({
                          from: data.range.from,
                          to: data.range.to,
                          source: row.value,
                        }).toString()}`}
                        className="truncate text-[12.5px] text-ink hover:text-accent-strong hover:underline"
                      >
                        {row.value}
                      </Link>
                      <span className="shrink-0 font-mono text-[12px] text-muted">
                        {formatCount(row.visitors)}
                      </span>
                    </li>
                  ))}
                </ul>
              )}
            </Panel>

            <BarPanel title="Devices" rows={data.devices} />
          </div>
        </>
      )}

      {exportNote ? (
        <p className="text-[11.5px] text-muted">Last export: {exportNote}</p>
      ) : null}
    </div>
  );
}
