"use client";

/**
 * The frame every `/analytics` screen sits in (REQ-007, slice 2).
 *
 * One toolbar for all of them — the range presets, the two pickers, the comparison switch, the
 * granularity select, the export and the refresh — because a report that is read with a different
 * date control than its neighbour is a different report. Every choice lives in the URL, so a
 * report is a link: the palette, a bookmark and the back button all land on the same numbers.
 *
 * The keyboard follows the spec: `d` reaches the range, `c` toggles the comparison, `r`
 * refreshes, `e` exports, and `g` followed by `o|p|s|a` jumps to Overview, Pages, Sources or
 * Audience. Shortcuts are ignored while a field has focus — a form is not a keyboard surface for
 * the screen behind it.
 */
import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";

import {
  BarChart3,
  CalendarRange,
  Download,
  FileDown,
  RefreshCw,
} from "lucide-react";
import Link from "next/link";
import { usePathname, useRouter, useSearchParams } from "next/navigation";

import { ApiError, downloadAnalyticsExport } from "@/lib/api";
import { useSites } from "@/lib/sites";

/** The screens of the analytics section. */
export const ANALYTICS_NAV = [
  { href: "/analytics", label: "Overview", shortcut: "o" },
  { href: "/analytics/pages", label: "Pages", shortcut: "p" },
  { href: "/analytics/sources", label: "Sources", shortcut: "s" },
  { href: "/analytics/audience", label: "Audience", shortcut: "a" },
  { href: "/analytics/events", label: "Events" },
  { href: "/analytics/downloads", label: "Downloads" },
  { href: "/analytics/forms", label: "Forms" },
  { href: "/analytics/goals", label: "Goals" },
  { href: "/analytics/realtime", label: "Realtime" },
] as const;

/** The named ranges the toolbar offers. */
const PRESETS: { key: string; label: string; days: number | "today" | "yesterday" }[] = [
  { key: "today", label: "Today", days: "today" },
  { key: "yesterday", label: "Yesterday", days: "yesterday" },
  { key: "7d", label: "7 days", days: 7 },
  { key: "30d", label: "30 days", days: 30 },
  { key: "12m", label: "12 months", days: 365 },
];

/** A day as `YYYY-MM-DD`, in UTC — the boundary every report shares. */
export function dayString(date: Date): string {
  return date.toISOString().slice(0, 10);
}

/** Today in UTC. */
export function today(): string {
  return dayString(new Date());
}

/** `days` before today, in UTC. */
function daysBefore(days: number): string {
  const date = new Date();
  date.setUTCDate(date.getUTCDate() - days);
  return dayString(date);
}

/** How many days a range covers, inclusive. */
export function rangeDays(from: string, to: string): number {
  const start = Date.parse(`${from}T00:00:00Z`);
  const end = Date.parse(`${to}T00:00:00Z`);
  if (Number.isNaN(start) || Number.isNaN(end)) {
    return 1;
  }
  return Math.max(1, Math.round((end - start) / 86_400_000) + 1);
}

type AnalyticsContextValue = {
  /** The site every request carries. */
  siteId: string | null;
  /** The range in effect (`YYYY-MM-DD`). */
  from: string;
  to: string;
  /** The preset the range matches, when it matches one. */
  preset: string;
  /** Whether the previous period is shown beside the numbers. */
  compare: boolean;
  /** `auto`, `hour` or `day`. */
  granularity: string;
  /** The whole query string of the screen, filters included. */
  params: URLSearchParams;
  /** Write parameters into the URL (`null` removes one). */
  patch: (changes: Record<string, string | null>) => void;
  /** Re-run the current request. */
  refreshToken: number;
  refresh: () => void;
  /** When the last request landed, for the toolbar's caption. */
  markLoaded: () => void;
  lastUpdated: Date | null;
  /** Export the report the screen is showing. */
  exportReport: (report: string) => Promise<void>;
  exporting: boolean;
  /** The last export, as the toolbar reports it. */
  exportNote: string | null;
};

const AnalyticsContext = createContext<AnalyticsContextValue | null>(null);

/** Read the analytics toolbar state; only valid inside [`AnalyticsShell`]. */
export function useAnalytics(): AnalyticsContextValue {
  const value = useContext(AnalyticsContext);
  if (!value) {
    throw new Error("useAnalytics must be used inside <AnalyticsShell>");
  }
  return value;
}

/** The shell of every analytics screen. */
export function AnalyticsShell({
  active,
  report,
  children,
}: {
  /** The screen's route, for the sub-navigation and the export. */
  active: string;
  /** The report key the export sends. */
  report: string;
  children: ReactNode;
}) {
  const router = useRouter();
  const pathname = usePathname();
  const params = useSearchParams();
  const { selectedSite, status: siteStatus } = useSites();
  const [refreshToken, setRefreshToken] = useState(0);
  const [lastUpdated, setLastUpdated] = useState<Date | null>(null);
  const [exporting, setExporting] = useState(false);
  const [exportNote, setExportNote] = useState<string | null>(null);
  const rangeInput = useRef<HTMLInputElement | null>(null);
  const prefix = useRef<{ key: string; at: number } | null>(null);

  const from = params.get("from") ?? daysBefore(6);
  const to = params.get("to") ?? today();
  const compare = params.get("compare") === "1";
  const granularity = params.get("granularity") ?? "auto";
  const preset =
    PRESETS.find((entry) => {
      if (entry.days === "today") {
        return from === today() && to === today();
      }
      if (entry.days === "yesterday") {
        return from === daysBefore(1) && to === daysBefore(1);
      }
      return from === daysBefore(entry.days - 1) && to === today();
    })?.key ?? "custom";

  const patch = useCallback(
    (changes: Record<string, string | null>) => {
      const next = new URLSearchParams(params.toString());
      // Any change to what is being asked for starts the table over at page one.
      next.delete("page");
      for (const [key, value] of Object.entries(changes)) {
        if (value === null || value === "") {
          next.delete(key);
        } else {
          next.set(key, value);
        }
      }
      const search = next.toString();
      router.replace(search ? `${pathname}?${search}` : pathname, { scroll: false });
    },
    [params, pathname, router],
  );

  const markLoaded = useCallback(() => setLastUpdated(new Date()), []);

  const exportReport = useCallback(
    async (key: string) => {
      setExporting(true);
      setExportNote(null);
      try {
        const query: Record<string, string> = {
          report: key,
          format: "csv",
          from,
          to,
        };
        if (selectedSite) {
          query.site_id = selectedSite.id;
        }
        if (compare) {
          query.compare = "1";
        }
        if (granularity !== "auto") {
          query.granularity = granularity;
        }
        for (const filter of ["path", "title", "device", "country", "source", "group", "sort", "dir"]) {
          const value = params.get(filter);
          if (value) {
            query[filter] = value;
          }
        }

        const { blob, filename, rows } = await downloadAnalyticsExport(query);
        const url = URL.createObjectURL(blob);
        const anchor = document.createElement("a");
        anchor.href = url;
        anchor.download = filename;
        document.body.appendChild(anchor);
        anchor.click();
        anchor.remove();
        URL.revokeObjectURL(url);
        setExportNote(`${filename} · ${rows} rows`);
      } catch (cause) {
        setExportNote(
          cause instanceof ApiError ? cause.message : "The export could not be downloaded.",
        );
      } finally {
        setExporting(false);
      }
    },
    [compare, from, granularity, params, selectedSite, to],
  );

  // The keyboard of the section (the spec's list, nothing more).
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target instanceof HTMLInputElement ||
        target instanceof HTMLTextAreaElement ||
        target instanceof HTMLSelectElement ||
        target?.isContentEditable === true;
      if (event.metaKey || event.ctrlKey || event.altKey || typing) {
        return;
      }

      if (event.key === "d") {
        event.preventDefault();
        rangeInput.current?.focus();
        return;
      }
      if (event.key === "c") {
        patch({ compare: compare ? null : "1" });
        return;
      }
      if (event.key === "r") {
        setRefreshToken((token) => token + 1);
        return;
      }
      if (event.key === "e") {
        void exportReport(report);
        return;
      }
      if (event.key === "g") {
        prefix.current = { key: "g", at: Date.now() };
        return;
      }
      const armed = prefix.current && Date.now() - prefix.current.at < 1_500;
      if (armed) {
        const match = ANALYTICS_NAV.find((entry) => "shortcut" in entry && entry.shortcut === event.key);
        prefix.current = null;
        if (match) {
          const search = params.toString();
          router.push(search ? `${match.href}?${search}` : match.href);
        }
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [compare, exportReport, params, patch, report, router]);

  const value = useMemo<AnalyticsContextValue>(
    () => ({
      siteId: selectedSite?.id ?? null,
      from,
      to,
      preset,
      compare,
      granularity,
      params,
      patch,
      refreshToken,
      refresh: () => setRefreshToken((token) => token + 1),
      markLoaded,
      lastUpdated,
      exportReport,
      exporting,
      exportNote,
    }),
    [
      compare,
      exportNote,
      exportReport,
      exporting,
      from,
      granularity,
      lastUpdated,
      markLoaded,
      params,
      patch,
      preset,
      refreshToken,
      report,
      selectedSite,
      to,
    ],
  );

  const days = rangeDays(from, to);
  const hourAllowed = days <= 2;

  return (
    <AnalyticsContext.Provider value={value}>
      <div className="flex flex-col gap-4">
        <nav aria-label="Analytics screens" className="flex flex-wrap items-center gap-1.5">
          <span className="mr-1 flex items-center gap-1.5 rounded-lg bg-quiet-soft px-2 py-1 text-[12px] font-medium text-ink">
            <BarChart3 className="size-3.5 shrink-0" aria-hidden />
            Analytics
          </span>
          {ANALYTICS_NAV.map((entry) => {
            const current = entry.href === active;
            return (
              <Link
                key={entry.href}
                href={`${entry.href}?${params.toString()}`}
                aria-current={current ? "page" : undefined}
                data-analytics-nav={entry.label.toLowerCase()}
                className={`rounded-lg px-2.5 py-1.5 text-[12.5px] transition ${
                  current
                    ? "bg-accent-soft font-medium text-accent-strong"
                    : "text-muted hover:bg-quiet-soft hover:text-ink"
                }`}
              >
                {entry.label}
              </Link>
            );
          })}
        </nav>

        <div
          data-analytics-toolbar
          className="flex flex-wrap items-end gap-3 rounded-xl border border-line bg-surface px-4 py-3"
        >
          <div className="flex flex-wrap items-center gap-1.5">
            {PRESETS.map((entry) => (
              <button
                key={entry.key}
                type="button"
                data-analytics-preset={entry.key}
                aria-pressed={preset === entry.key}
                onClick={() => {
                  if (entry.days === "today") {
                    patch({ from: today(), to: today() });
                  } else if (entry.days === "yesterday") {
                    patch({ from: daysBefore(1), to: daysBefore(1) });
                  } else {
                    patch({ from: daysBefore(entry.days - 1), to: today() });
                  }
                }}
                className={`rounded-lg border px-2.5 py-1.5 text-[12px] transition ${
                  preset === entry.key
                    ? "border-accent bg-accent-soft text-accent-strong"
                    : "border-line text-muted hover:bg-quiet-soft hover:text-ink"
                }`}
              >
                {entry.label}
              </button>
            ))}
          </div>

          <label className="flex flex-col gap-1 text-[11px] tracking-wide text-muted uppercase">
            From
            <input
              ref={rangeInput}
              type="date"
              value={from}
              data-analytics-from
              onChange={(event) => {
                // Moving the start past the end drags the end with it: a range that ends before
                // it starts is not a range, and the report should never have to refuse one.
                const value = event.target.value;
                patch(value > to ? { from: value, to: value } : { from: value });
              }}
              className="rounded-lg border border-line bg-canvas px-2 py-1.5 font-mono text-[12px] text-ink"
            />
          </label>
          <label className="flex flex-col gap-1 text-[11px] tracking-wide text-muted uppercase">
            To
            <input
              type="date"
              value={to}
              data-analytics-to
              onChange={(event) => {
                const value = event.target.value;
                patch(value < from ? { from: value, to: value } : { to: value });
              }}
              className="rounded-lg border border-line bg-canvas px-2 py-1.5 font-mono text-[12px] text-ink"
            />
          </label>

          <label className="flex items-center gap-2 text-[12.5px] text-ink">
            <input
              type="checkbox"
              checked={compare}
              data-analytics-compare
              onChange={() => patch({ compare: compare ? null : "1" })}
              className="size-3.5 accent-accent"
            />
            Compare
          </label>

          <label className="flex flex-col gap-1 text-[11px] tracking-wide text-muted uppercase">
            Granularity
            <select
              value={granularity}
              data-analytics-granularity
              onChange={(event) => patch({ granularity: event.target.value })}
              className="rounded-lg border border-line bg-canvas px-2 py-1.5 text-[12px] text-ink"
            >
              <option value="auto">Auto</option>
              <option value="hour" disabled={!hourAllowed}>
                Hour{hourAllowed ? "" : " (short ranges only)"}
              </option>
              <option value="day">Day</option>
            </select>
          </label>

          <div className="ml-auto flex items-center gap-2">
            {lastUpdated ? (
              <span className="text-[11.5px] text-muted">
                Updated{" "}
                {lastUpdated.toLocaleTimeString("en", {
                  hour: "2-digit",
                  minute: "2-digit",
                  second: "2-digit",
                })}
              </span>
            ) : null}
            <button
              type="button"
              onClick={() => setRefreshToken((token) => token + 1)}
              data-analytics-refresh
              className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] text-ink transition hover:bg-quiet-soft"
            >
              <RefreshCw className="size-3.5" aria-hidden />
              Refresh
            </button>
            <button
              type="button"
              onClick={() => void exportReport(report)}
              disabled={exporting}
              data-analytics-export
              className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] text-ink transition hover:bg-quiet-soft disabled:opacity-50"
            >
              <Download className="size-3.5" aria-hidden />
              {exporting ? "Exporting…" : "Export CSV"}
            </button>
          </div>

          <p className="flex w-full items-center gap-2 text-[11.5px] text-muted">
            <CalendarRange className="size-3.5" aria-hidden />
            {selectedSite ? selectedSite.name : "No site selected"} · {from} → {to} ·{" "}
            {days === 1 ? "1 day" : `${days} days`}
            {exportNote ? (
              <span data-analytics-export-note className="ml-auto flex items-center gap-1.5">
                <FileDown className="size-3.5" aria-hidden />
                {exportNote}
              </span>
            ) : null}
          </p>
        </div>

        {siteStatus === "loading" ? (
          <p className="text-[12.5px] text-muted">Loading the sites of this account…</p>
        ) : null}
        {siteStatus === "ready" && !selectedSite ? (
          <p className="text-[12.5px] text-muted">
            This account has no site yet — a report needs one to count.
          </p>
        ) : null}

        {selectedSite ? children : null}
      </div>
    </AnalyticsContext.Provider>
  );
}

/**
 * Run one report request and keep its state.
 *
 * `extra` is part of the request identity: a screen that adds a filter re-runs, a screen that
 * re-renders does not. A stale answer is dropped by a request counter, because a slow first
 * request must not overwrite a fast second one.
 */
export function useReport<T>(
  fetcher: (query: Record<string, string | number>) => Promise<T>,
  extra: Record<string, string | number | undefined> = {},
): {
  data: T | null;
  status: "idle" | "loading" | "ready" | "error";
  error: { message: string; code: string } | null;
  retry: () => void;
} {
  const { siteId, from, to, compare, granularity, refreshToken, markLoaded } = useAnalytics();
  const [data, setData] = useState<T | null>(null);
  const [status, setStatus] = useState<"idle" | "loading" | "ready" | "error">("idle");
  const [error, setError] = useState<{ message: string; code: string } | null>(null);
  const [attempt, setAttempt] = useState(0);
  const identity = JSON.stringify(extra);
  const latest = useRef(0);

  useEffect(() => {
    if (!siteId) {
      setData(null);
      setStatus("idle");
      return;
    }

    const ticket = latest.current + 1;
    latest.current = ticket;
    setStatus("loading");
    setError(null);

    const query: Record<string, string | number> = {
      site_id: siteId,
      from,
      to,
    };
    if (compare) {
      query.compare = "1";
    }
    if (granularity !== "auto") {
      query.granularity = granularity;
    }
    for (const [key, value] of Object.entries(JSON.parse(identity) as Record<string, string>)) {
      if (value !== undefined && value !== null && value !== "") {
        query[key] = value;
      }
    }

    fetcher(query)
      .then((answer) => {
        if (latest.current !== ticket) {
          return;
        }
        setData(answer);
        setStatus("ready");
        markLoaded();
      })
      .catch((cause: unknown) => {
        if (latest.current !== ticket) {
          return;
        }
        setData(null);
        setStatus("error");
        setError(
          cause instanceof ApiError
            ? { message: cause.message, code: cause.code }
            : { message: "The report could not be loaded.", code: "unknown_error" },
        );
      });
    // `fetcher` is a module-level function on every screen; the request identity is the query.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [siteId, from, to, compare, granularity, refreshToken, attempt, identity, markLoaded]);

  return {
    data,
    status,
    error,
    retry: () => setAttempt((value) => value + 1),
  };
}
