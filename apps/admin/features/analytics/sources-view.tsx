"use client";

/**
 * `/analytics/sources` — referrers and UTM (REQ-007, slice 2).
 *
 * The five UTM columns beside the channel that brought the visit, with a `Group by` selector for
 * the question a marketer actually asks ("which campaigns worked?"). Conversion needs a goal: a
 * visitor who reached one inside the range is a conversion, and the rate is against the visitors
 * of the row — never against the whole site.
 */
import type { AnalyticsSourceRow } from "@/lib/api";
import { fetchAnalyticsSources } from "@/lib/api";

import { useAnalytics, useReport } from "./analytics-shell";
import {
  DataTable,
  ErrorPanel,
  EmptyPanel,
  LoadingRows,
  Panel,
  formatCount,
  formatRate,
  type Column,
} from "./parts";

/** The group-by modes the API accepts, with the labels the screen shows. */
const GROUPS = [
  { value: "combination", label: "Source + medium + campaign" },
  { value: "source", label: "Source" },
  { value: "medium", label: "Medium" },
  { value: "campaign", label: "Campaign" },
  { value: "referrer", label: "Referrer" },
];

/** The sources report. */
export function AnalyticsSourcesView() {
  const { params, patch } = useAnalytics();
  const group = params.get("group") ?? "combination";
  const { data, status, error, retry } = useReport(fetchAnalyticsSources, {
    group,
    device: params.get("device") ?? undefined,
    country: params.get("country") ?? undefined,
    path: params.get("path") ?? undefined,
  });

  const grouped = group !== "combination";

  const columns: Column<AnalyticsSourceRow>[] = [
    {
      key: "source",
      label: grouped ? group.charAt(0).toUpperCase() + group.slice(1) : "Source",
      render: (row) => <span className="text-ink">{row.source}</span>,
    },
    ...(grouped
      ? []
      : ([
          {
            key: "medium",
            label: "Medium",
            render: (row: AnalyticsSourceRow) => (
              <span className="text-muted">{row.medium ?? "—"}</span>
            ),
          },
          {
            key: "campaign",
            label: "Campaign",
            render: (row: AnalyticsSourceRow) => (
              <span className="text-muted">{row.campaign ?? "—"}</span>
            ),
          },
          {
            key: "term",
            label: "Term",
            render: (row: AnalyticsSourceRow) => (
              <span className="text-muted">{row.term ?? "—"}</span>
            ),
          },
          {
            key: "content",
            label: "Content",
            render: (row: AnalyticsSourceRow) => (
              <span className="text-muted">{row.content ?? "—"}</span>
            ),
          },
        ] as Column<AnalyticsSourceRow>[])),
    { key: "visits", label: "Visits", align: "right", numeric: true, render: (row) => formatCount(row.visits) },
    {
      key: "visitors",
      label: "Visitors",
      align: "right",
      numeric: true,
      render: (row) => formatCount(row.visitors),
    },
    {
      key: "conversions",
      label: "Conversions",
      align: "right",
      numeric: true,
      render: (row) => formatCount(row.conversions),
    },
    {
      key: "conversion_rate",
      label: "Conversion rate",
      align: "right",
      numeric: true,
      render: (row) => formatRate(row.conversion_rate),
    },
  ];

  return (
    <div className="flex flex-col gap-4" data-analytics-sources>
      <Panel title="Group by" bodyClassName="px-4 py-3" testId="group-by">
        <div className="flex flex-wrap items-center gap-2">
          {GROUPS.map((entry) => {
            const active = entry.value === group;
            return (
              <button
                key={entry.value}
                type="button"
                data-analytics-group={entry.value}
                aria-pressed={active}
                onClick={() => patch({ group: entry.value === "combination" ? null : entry.value })}
                className={`rounded-lg border px-2.5 py-1.5 text-[12px] transition ${
                  active
                    ? "border-accent bg-accent-soft text-accent-strong"
                    : "border-line text-muted hover:bg-quiet-soft hover:text-ink"
                }`}
              >
                {entry.label}
              </button>
            );
          })}
          <span className="ml-auto text-[11.5px] text-muted">
            A conversion is a visitor who reached a goal in this range.
          </span>
        </div>
      </Panel>

      {status === "error" && error ? (
        <ErrorPanel message={error.message} code={error.code} onRetry={retry} />
      ) : (
        <Panel
          title="Sources"
          subtitle={data ? `${formatCount(data.rows.length)} rows` : undefined}
          bodyClassName="p-0"
          testId="sources-table"
        >
          {status === "loading" && !data ? (
            <LoadingRows rows={8} label="Loading the sources report" />
          ) : data && data.rows.length === 0 ? (
            <EmptyPanel title="No traffic sources in this range">
              <p>
                A visit records the campaign it carried and the host that referred it; nothing
                arrived with either in this range.
              </p>
            </EmptyPanel>
          ) : data ? (
            <DataTable columns={columns} rows={data.rows} rowKey={(row) => `${row.source}|${row.medium ?? ""}|${row.campaign ?? ""}|${row.term ?? ""}|${row.content ?? ""}`} />
          ) : null}
        </Panel>
      )}
    </div>
  );
}
