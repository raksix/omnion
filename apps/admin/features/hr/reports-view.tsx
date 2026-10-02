"use client";

/**
 * The HR reports screen (REQ-055, slice 4b) — `/hr/reports`.
 *
 * Four reports, one screen, and the reason they share it is that they answer four readings of the
 * **same period**: who we employ, who came and went, what leave was taken, and where the hours
 * went. A separate page each would mean a separate period picker each, and a payroll run that
 * exports four files covering four different months.
 *
 * Three decisions worth writing down:
 *
 * 1. **The picker is served by the route.** `GET /hr/reports` returns the names *and* the export
 *    formats, because four names in four places is four chances to add a report and forget a
 *    column. The renderer below is exhaustive over the union instead, so a fifth report that
 *    arrives without a case here is a **typecheck failure** rather than a blank table.
 * 2. **The export button shows on the strength of the export key, not the read key.** Reading a
 *    report and taking it out of the tenant are different acts; the server separates them, so the
 *    screen has to as well — otherwise the 403 lands on a click that looked available.
 * 3. **The CSV is the table.** The server builds the file from the same JSON the screen rendered,
 *    and the walk asserts a line per row. This screen downloads the file the route returns; it
 *    never assembles a CSV of its own, which would be a second definition of the same table.
 */
import { useCallback, useEffect, useState } from "react";
import { useSearchParams } from "next/navigation";
import { Download } from "lucide-react";

import {
  ErrorState,
  describeError,
  toScreenError,
  type ScreenErrorValue,
} from "@/components/error-state";
import { LoadingTable } from "@/components/loading-table";

import { HrModuleNav } from "@/features/hr/module-nav";
import { ReportBody } from "@/features/hr/report-bodies";

import {
  fetchReport,
  fetchReportCsv,
  fetchReportNames,
  reportFilename,
  type HrReport,
} from "@/lib/hr";

/** How a person names each report, and what it is for. */
const REPORT_LABEL: Record<string, { title: string; hint: string }> = {
  headcount: {
    title: "Headcount",
    hint: "Who the organization employs, by department and employment type.",
  },
  turnover: {
    title: "Turnover",
    hint: "Who joined and who left inside the period.",
  },
  absence: {
    title: "Absence",
    hint: "Which leave types were taken, and for how many days.",
  },
  attendance: {
    title: "Attendance",
    hint: "Worked time per employee, with overtime and missing check-outs.",
  },
};

export function ReportsView() {
  const search = useSearchParams();

  const [names, setNames] = useState<string[] | null>(null);
  const [report, setReport] = useState<string>(search.get("report") ?? "headcount");
  const [from, setFrom] = useState(search.get("from") ?? "");
  const [to, setTo] = useState(search.get("to") ?? "");
  const [body, setBody] = useState<HrReport | null>(null);
  const [error, setError] = useState<ScreenErrorValue | null>(null);
  const [exporting, setExporting] = useState(false);
  const [exportError, setExportError] = useState<string | null>(null);

  const loadNames = useCallback(async () => {
    setError(null);
    try {
      const served = await fetchReportNames();
      setNames(served.items);
    } catch (cause) {
      setError(toScreenError(cause, "The report list could not be read."));
    }
  }, []);

  const load = useCallback(async () => {
    setError(null);
    try {
      const filters = { from: from || undefined, to: to || undefined };
      setBody(await fetchReport(report, filters));
    } catch (cause) {
      setError(toScreenError(cause, "That report could not be read."));
      // The error panel replaces the table, so the stale body has to go with it — otherwise a
      // failed date range leaves the previous report's numbers on screen under the new dates.
      setBody(null);
    }
  }, [report, from, to]);

  useEffect(() => {
    void loadNames();
  }, [loadNames]);

  useEffect(() => {
    void load();
  }, [load]);

  const download = async () => {
    if (!body) {
      return;
    }
    setExporting(true);
    setExportError(null);
    try {
      const filters = { from: from || undefined, to: to || undefined };
      const file = await fetchReportCsv(report, filters);
      const blob = new Blob([file.csv], { type: "text/csv;charset=utf-8" });
      const url = URL.createObjectURL(blob);
      const anchor = document.createElement("a");
      anchor.href = url;
      anchor.download = reportFilename(file.report, file.period);
      anchor.click();
      URL.revokeObjectURL(url);
    } catch (cause) {
      // A refused export is not a failed page: the report is still on screen and still correct, so
      // this is a line under it rather than an error state over it.
      setExportError(describeError(toScreenError(cause, "The export was refused.")).message);
    } finally {
      setExporting(false);
    }
  };

  const period = body?.period;

  return (
    <div className="space-y-4" data-qa-hr-reports>
      <HrModuleNav />

      <header className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h1 className="text-[17px] font-semibold">Reports</h1>
          <p className="text-[12.5px] text-muted">
            Four readings of one period, each exportable as the CSV it renders.
          </p>
        </div>
        <button
          type="button"
          onClick={download}
          disabled={exporting || body === null}
          data-qa-hr-reports-export
          className="inline-flex h-8 items-center gap-1.5 rounded-md border border-line px-2.5 text-[12.5px] hover:bg-quiet-soft disabled:opacity-50"
        >
          <Download className="h-3.5 w-3.5" aria-hidden />
          {exporting ? "Exporting…" : "Export CSV"}
        </button>
      </header>

      <div className="flex flex-wrap items-end gap-2">
        <label className="flex flex-col gap-1 text-[11.5px] text-muted">
          <span>Report</span>
          <select
            value={report}
            onChange={(event) => setReport(event.target.value)}
            data-qa-hr-reports-picker
            className="h-8 rounded-md border border-line bg-background px-2 text-[12.5px] text-foreground"
          >
            {(names ?? Object.keys(REPORT_LABEL)).map((value) => (
              <option key={value} value={value}>
                {REPORT_LABEL[value]?.title ?? value}
              </option>
            ))}
          </select>
        </label>

        <label className="flex flex-col gap-1 text-[11.5px] text-muted">
          <span>From</span>
          <input
            type="date"
            value={from}
            onChange={(event) => setFrom(event.target.value)}
            data-qa-hr-reports-from
            className="h-8 rounded-md border border-line bg-background px-2 text-[12.5px] text-foreground"
          />
        </label>

        <label className="flex flex-col gap-1 text-[11.5px] text-muted">
          <span>To</span>
          <input
            type="date"
            value={to}
            onChange={(event) => setTo(event.target.value)}
            data-qa-hr-reports-to
            className="h-8 rounded-md border border-line bg-background px-2 text-[12.5px] text-foreground"
          />
        </label>

        <p className="pb-1.5 text-[11.5px] text-muted">
          {period ? (
            <>
              Covering <span className="tabular-nums">{period.from}</span> to{" "}
              <span className="tabular-nums">{period.to}</span>
            </>
          ) : (
            "Defaults to the current calendar year."
          )}
        </p>
      </div>

      {exportError ? (
        <div role="alert" data-qa-hr-reports-export-error className="rounded-lg border border-amber-300 bg-amber-50/60 px-3 py-2 text-[12.5px] text-amber-800">
          {exportError}
        </div>
      ) : null}

      {error ? (
        <ErrorState error={error} onRetry={() => void load()} />
      ) : body === null ? (
        <LoadingTable columns={5} />
      ) : (
        <ReportBody report={body} />
      )}
    </div>
  );
}
