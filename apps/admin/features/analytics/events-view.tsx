"use client";

/**
 * `/analytics/events` — custom events (REQ-007, slice 2).
 *
 * What the site itself reports: a name, how often it fired, how many people fired it, the values
 * it carried and when it was last seen. The drawer answers the next question — *what* the events
 * carried — with the property breakdown, because "signup fired 40 times" is only useful beside
 * "30 of them were the pro plan".
 */
import { useState } from "react";

import { fetchAnalyticsEvent, fetchAnalyticsEvents, type AnalyticsEventRow } from "@/lib/api";

import { useAnalytics, useReport } from "./analytics-shell";
import {
  DataTable,
  Drawer,
  ErrorPanel,
  EmptyPanel,
  LastSeen,
  LoadingRows,
  Panel,
  Stat,
  formatCount,
  type Column,
} from "./parts";

/** One event, in the drawer. */
function EventDetail({ name }: { name: string }) {
  const { data, status, error } = useReport(
    (query) => fetchAnalyticsEvent(name, query),
    { name },
  );

  if (status === "error" && error) {
    return <ErrorPanel message={error.message} code={error.code} />;
  }
  if (!data) {
    return <LoadingRows rows={6} label="Loading the event" />;
  }

  const max = Math.max(1, ...data.series.map((point) => point.count));

  return (
    <div className="flex flex-col gap-5 p-4">
      <div className="grid grid-cols-3 gap-3">
        <Stat label="Events" value={formatCount(data.count)} />
        <Stat label="Visitors" value={formatCount(data.visitors)} />
        <Stat label="Value" value={formatCount(data.value_sum)} />
      </div>

      <section className="flex flex-col gap-2">
        <h3 className="text-[12px] font-semibold tracking-wide text-muted uppercase">
          Per day
        </h3>
        <ul className="flex flex-col gap-1.5">
          {data.series.map((point) => (
            <li key={point.bucket} className="flex items-center gap-2 text-[12px]">
              <span className="w-16 shrink-0 text-muted">{point.label}</span>
              <span className="h-2 flex-1 overflow-hidden rounded-full bg-quiet-soft">
                <span
                  className="block h-full rounded-full bg-accent"
                  style={{ width: `${((point.count / max) * 100).toFixed(1)}%` }}
                />
              </span>
              <span className="w-10 shrink-0 text-right font-mono text-ink">
                {formatCount(point.count)}
              </span>
            </li>
          ))}
        </ul>
      </section>

      <section className="flex flex-col gap-2">
        <h3 className="text-[12px] font-semibold tracking-wide text-muted uppercase">
          Properties
        </h3>
        {data.properties.length === 0 ? (
          <p className="text-[12px] text-muted">
            This event carried no properties — nothing to break down.
          </p>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full min-w-[320px] border-collapse text-[12.5px]">
              <thead>
                <tr className="border-b border-line text-left text-[11px] tracking-wide text-muted uppercase">
                  <th scope="col" className="px-2 py-1.5 font-medium">
                    Property
                  </th>
                  <th scope="col" className="px-2 py-1.5 font-medium">
                    Value
                  </th>
                  <th scope="col" className="px-2 py-1.5 text-right font-medium">
                    Count
                  </th>
                </tr>
              </thead>
              <tbody>
                {data.properties.map((property) => (
                  <tr
                    key={`${property.key}:${property.value}`}
                    className="border-b border-line/60 last:border-b-0"
                  >
                    <td className="px-2 py-1.5 font-mono text-muted">{property.key}</td>
                    <td className="px-2 py-1.5 text-ink">{property.value}</td>
                    <td className="px-2 py-1.5 text-right font-mono">
                      {formatCount(property.count)}
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

/** The events report. */
export function AnalyticsEventsView() {
  const { params } = useAnalytics();
  const [open, setOpen] = useState<AnalyticsEventRow | null>(null);
  const { data, status, error, retry } = useReport(fetchAnalyticsEvents, {
    path: params.get("path") ?? undefined,
    device: params.get("device") ?? undefined,
    country: params.get("country") ?? undefined,
    source: params.get("source") ?? undefined,
  });

  const columns: Column<AnalyticsEventRow>[] = [
    {
      key: "name",
      label: "Event",
      render: (row) => (
        <button
          type="button"
          onClick={() => setOpen(row)}
          data-analytics-event-link
          className="text-left font-mono text-ink hover:text-accent-strong hover:underline"
        >
          {row.name}
        </button>
      ),
    },
    {
      key: "count",
      label: "Count",
      align: "right",
      numeric: true,
      render: (row) => formatCount(row.count),
    },
    {
      key: "visitors",
      label: "Unique visitors",
      align: "right",
      numeric: true,
      render: (row) => formatCount(row.visitors),
    },
    {
      key: "value_sum",
      label: "Value sum",
      align: "right",
      numeric: true,
      render: (row) => formatCount(row.value_sum),
    },
    { key: "last_seen", label: "Last seen", render: (row) => <LastSeen value={row.last_seen} /> },
  ];

  return (
    <div className="flex flex-col gap-4" data-analytics-events>
      {status === "error" && error ? (
        <ErrorPanel message={error.message} code={error.code} onRetry={retry} />
      ) : (
        <Panel
          title="Events"
          subtitle={data ? `${formatCount(data.rows.length)} event names` : undefined}
          bodyClassName="p-0"
          testId="events-table"
        >
          {status === "loading" && !data ? (
            <LoadingRows rows={7} label="Loading the events report" />
          ) : data && data.rows.length === 0 ? (
            <EmptyPanel title="No custom events in this range">
              <p>
                Events come from the tracking script: a site calls <code>track(&quot;name&quot;)</code>{" "}
                for the things it cares about (signups, downloads, form starts), and they are
                counted here with their properties and values.
              </p>
            </EmptyPanel>
          ) : data ? (
            <DataTable
              columns={columns}
              rows={data.rows}
              onRowClick={(row) => setOpen(row)}
              rowKey={(row) => row.name}
            />
          ) : null}
        </Panel>
      )}

      {open ? (
        <Drawer
          title={open.name}
          subtitle="Counts, values and the property breakdown"
          onClose={() => setOpen(null)}
        >
          <EventDetail name={open.name} />
        </Drawer>
      ) : null}
    </div>
  );
}
