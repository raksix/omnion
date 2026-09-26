"use client";

/**
 * `/analytics/forms` — form runs (REQ-007, slice 2).
 *
 * Submissions per form, the people behind them and the values they carried. Completion is
 * computed only when the site also emits a `form_start` for the same form — a platform that
 * invents a completion rate out of submissions alone would be reporting a number nobody measured,
 * so a form without starts shows a dash and the screen says what to emit.
 */
import type { AnalyticsFormRow } from "@/lib/api";
import { fetchAnalyticsForms } from "@/lib/api";

import { useAnalytics, useReport } from "./analytics-shell";
import {
  DataTable,
  ErrorPanel,
  EmptyPanel,
  LastSeen,
  LoadingRows,
  Panel,
  formatCount,
  formatRate,
  type Column,
} from "./parts";

/** The forms report. */
export function AnalyticsFormsView() {
  const { params } = useAnalytics();
  const { data, status, error, retry } = useReport(fetchAnalyticsForms, {
    path: params.get("path") ?? undefined,
    device: params.get("device") ?? undefined,
    country: params.get("country") ?? undefined,
    source: params.get("source") ?? undefined,
  });

  const columns: Column<AnalyticsFormRow>[] = [
    {
      key: "form",
      label: "Form",
      render: (row) => <span className="text-ink">{row.form}</span>,
    },
    {
      key: "submissions",
      label: "Submissions",
      align: "right",
      numeric: true,
      render: (row) => formatCount(row.submissions),
    },
    {
      key: "visitors",
      label: "Visitors",
      align: "right",
      numeric: true,
      render: (row) => formatCount(row.visitors),
    },
    {
      key: "value_sum",
      label: "Value",
      align: "right",
      numeric: true,
      render: (row) => formatCount(row.value_sum),
    },
    {
      key: "starts",
      label: "Starts",
      align: "right",
      numeric: true,
      render: (row) => formatCount(row.starts),
    },
    {
      key: "completion_rate",
      label: "Completion",
      align: "right",
      numeric: true,
      render: (row) => formatRate(row.completion_rate),
    },
    {
      key: "abandonment",
      label: "Abandonment",
      align: "right",
      numeric: true,
      render: (row) => formatCount(row.abandonment),
    },
    { key: "last_seen", label: "Last submission", render: (row) => <LastSeen value={row.last_seen} /> },
  ];

  if (status === "error" && error) {
    return <ErrorPanel message={error.message} code={error.code} onRetry={retry} />;
  }

  if (status === "loading" && !data) {
    return (
      <Panel title="Forms" bodyClassName="p-0">
        <LoadingRows rows={6} label="Loading the forms report" />
      </Panel>
    );
  }

  if (!data || data.rows.length === 0) {
    return (
      <EmptyPanel title="No form activity in this range">
        <p>
          Form reports read two events: <code>form_submit</code> (a visitor sent the form) and{" "}
          <code>form_start</code> (a visitor began it). Send the first for submissions, both for a
          completion rate.
        </p>
      </EmptyPanel>
    );
  }

  const withoutStarts = data.rows.some((row) => row.completion_rate === null);

  return (
    <div className="flex flex-col gap-4" data-analytics-forms>
      <Panel title="Forms" bodyClassName="p-0" testId="forms-table">
        <DataTable columns={columns} rows={data.rows} rowKey={(row) => row.form} />
      </Panel>
      {withoutStarts ? (
        <p
          data-analytics-forms-hint
          className="rounded-lg border border-line bg-quiet-soft px-3 py-2 text-[12px] text-muted"
        >
          A dash means the site never emitted a <code>form_start</code> for that form: completion
          and abandonment are not measured, not zero.
        </p>
      ) : null}
    </div>
  );
}
