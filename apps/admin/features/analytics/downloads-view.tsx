"use client";

/**
 * `/analytics/downloads` — file downloads (REQ-007, slice 2).
 *
 * Two answers on one screen: which file people take, and which page they take it from. Both come
 * from the `download` events the tracking script sends, so a site that never fires them shows an
 * honest empty state instead of a zero table.
 */
import type { AnalyticsDownloadRow } from "@/lib/api";
import { fetchAnalyticsDownloads } from "@/lib/api";

import { useAnalytics, useReport } from "./analytics-shell";
import {
  DataTable,
  ErrorPanel,
  EmptyPanel,
  LoadingRows,
  Panel,
  formatCount,
  type Column,
} from "./parts";

/** The columns both tables share. */
function columns(label: string): Column<AnalyticsDownloadRow>[] {
  return [
    {
      key: "value",
      label,
      render: (row) => <span className="break-all text-ink">{row.value}</span>,
    },
    {
      key: "downloads",
      label: "Downloads",
      align: "right",
      numeric: true,
      render: (row) => formatCount(row.downloads),
    },
    {
      key: "visitors",
      label: "Visitors",
      align: "right",
      numeric: true,
      render: (row) => formatCount(row.visitors),
    },
  ];
}

/** The downloads report. */
export function AnalyticsDownloadsView() {
  const { params } = useAnalytics();
  const { data, status, error, retry } = useReport(fetchAnalyticsDownloads, {
    path: params.get("path") ?? undefined,
    device: params.get("device") ?? undefined,
    country: params.get("country") ?? undefined,
    source: params.get("source") ?? undefined,
  });

  if (status === "error" && error) {
    return <ErrorPanel message={error.message} code={error.code} onRetry={retry} />;
  }

  if (status === "loading" && !data) {
    return (
      <Panel title="Downloads" bodyClassName="p-0">
        <LoadingRows rows={7} label="Loading the downloads report" />
      </Panel>
    );
  }

  if (!data || data.total === 0) {
    return (
      <EmptyPanel title="No downloads in this range">
        <p>
          A download is a <code>download</code> event carrying the file: the tracking script sends
          one for every link that ends in a file, and a site can send its own for anything else.
        </p>
      </EmptyPanel>
    );
  }

  return (
    <div className="flex flex-col gap-4" data-analytics-downloads>
      <p className="text-[12.5px] text-muted">
        <span className="font-mono text-ink">{formatCount(data.total)}</span> downloads in this
        range.
      </p>

      <div className="grid gap-4 xl:grid-cols-2">
        <Panel title="By file" bodyClassName="p-0" testId="downloads-files">
          <DataTable
            columns={columns("File")}
            rows={data.files}
            rowKey={(row) => `file:${row.value}`}
          />
        </Panel>
        <Panel title="By page" bodyClassName="p-0" testId="downloads-pages">
          <DataTable
            columns={columns("Page")}
            rows={data.pages}
            rowKey={(row) => `page:${row.value}`}
          />
        </Panel>
      </div>
    </div>
  );
}
