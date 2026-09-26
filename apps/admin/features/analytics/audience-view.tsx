"use client";

/**
 * `/analytics/audience` — devices, browsers, systems, screens, languages and countries
 * (REQ-007, slice 2).
 *
 * Five bar panels and one country table. The country comes from the deployment's own edge (the
 * platform never geolocates an address itself): a visit whose edge reported no country is shown
 * as `(unknown)` rather than guessed.
 */
import { fetchAnalyticsAudience } from "@/lib/api";

import { useAnalytics, useReport } from "./analytics-shell";
import {
  BarPanel,
  DataTable,
  ErrorPanel,
  EmptyPanel,
  LoadingRows,
  Panel,
  formatCount,
  formatShare,
  type Column,
} from "./parts";
import type { AnalyticsCountryRow } from "@/lib/api";

/** The audience report. */
export function AnalyticsAudienceView() {
  const { params } = useAnalytics();
  const { data, status, error, retry } = useReport(fetchAnalyticsAudience, {
    device: params.get("device") ?? undefined,
    country: params.get("country") ?? undefined,
    source: params.get("source") ?? undefined,
    path: params.get("path") ?? undefined,
  });

  const columns: Column<AnalyticsCountryRow>[] = [
    {
      key: "code",
      label: "Country",
      render: (row) => <span className="text-ink">{row.code}</span>,
    },
    {
      key: "visitors",
      label: "Visitors",
      align: "right",
      numeric: true,
      render: (row) => formatCount(row.visitors),
    },
    {
      key: "views",
      label: "Views",
      align: "right",
      numeric: true,
      render: (row) => formatCount(row.views),
    },
    {
      key: "share",
      label: "Share",
      align: "right",
      numeric: true,
      render: (row) => (
        <span className="flex items-center justify-end gap-2">
          <span className="hidden h-1.5 w-24 overflow-hidden rounded-full bg-quiet-soft sm:block">
            <span
              className="block h-full rounded-full bg-accent"
              style={{ width: `${Math.max(2, row.share * 100).toFixed(1)}%` }}
            />
          </span>
          {formatShare(row.share)}
        </span>
      ),
    },
  ];

  if (status === "error" && error) {
    return <ErrorPanel message={error.message} code={error.code} onRetry={retry} />;
  }

  if (status === "loading" && !data) {
    return (
      <Panel title="Audience" bodyClassName="p-0">
        <LoadingRows rows={8} label="Loading the audience report" />
      </Panel>
    );
  }

  if (!data) {
    return null;
  }

  if (data.visitors === 0) {
    return (
      <EmptyPanel title="Nobody visited in this range">
        <p>
          The panels fill in from the visits Omnion records: devices, browsers, systems, screens,
          languages and the country the edge reported.
        </p>
      </EmptyPanel>
    );
  }

  return (
    <div className="flex flex-col gap-4" data-analytics-audience>
      <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
        {data.panels.map((panel) => (
          <BarPanel key={panel.kind} title={panel.title} rows={panel.rows} />
        ))}
      </div>

      <Panel
        title="Countries"
        subtitle={`${formatCount(data.visitors)} visitors in this range`}
        bodyClassName="p-0"
        testId="countries-table"
      >
        {data.countries.length === 0 ? (
          <EmptyPanel title="No countries recorded">
            <p>
              A country arrives from the edge in front of the API (for example Cloudflare&apos;s
              country header). Without an edge, visits are counted as (unknown).
            </p>
          </EmptyPanel>
        ) : (
          <DataTable columns={columns} rows={data.countries} rowKey={(row) => row.code} />
        )}
      </Panel>
    </div>
  );
}
