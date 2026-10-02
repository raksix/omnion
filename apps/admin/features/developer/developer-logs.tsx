"use client";

/**
 * The request log: filters, the table, and the detail drawer (REQ-022, slice 2).
 *
 * ## Every filter is a narrowing, and the toolbar says so
 *
 * Each control maps to exactly one query parameter, and a filter left alone means "no opinion"
 * rather than "match nothing" — which is what the store does, and a toolbar that implied
 * otherwise would be a screen that quietly hides rows. `Reset` clears all of them at once, and
 * the export always covers **what is on screen**, not the whole retention window: this table has
 * a CSV export and no bulk route, on purpose.
 *
 * ## The drawer is where the log earns its keep
 *
 * A row answers "what happened"; the drawer answers "why", by naming the permission the guard
 * resolved. That column is why the layer publishes the *permission it was asked for* on a
 * refusal — a 403 row that says only "403" costs an operator a round trip to the docs, and a
 * 403 row that names the missing scope does not.
 *
 * ## The retention window is printed, not hidden
 *
 * A log whose window nobody can find reads as "the platform never recorded that request", which
 * is the failure this whole surface exists to prevent. The number comes from the API, not from a
 * constant in this file, so it cannot drift from what the store actually keeps.
 */
import { useCallback, useEffect, useState } from "react";

import {
  Download,
  PanelRightOpen,
  RefreshCw,
  Search,
  TriangleAlert,
  X,
} from "lucide-react";
import { useSearchParams } from "next/navigation";

import { ApiError, fetchDeveloperKeys, fetchDeveloperLog, fetchDeveloperLogs } from "@/lib/api";
import {
  DEVELOPER_METHODS,
  DEVELOPER_STATUS_CLASSES,
  DEVELOPER_WINDOWS,
  developerLogsCsv,
  type DeveloperKey,
  type DeveloperLogDetail,
  type DeveloperLogFilters,
  type DeveloperLogRow,
} from "@/lib/developer";
import { formatTimestamp } from "@/lib/format";

type State =
  | { status: "loading" }
  | { status: "ready"; rows: DeveloperLogRow[]; nextBefore: number | null; retention: number | null }
  | { status: "error"; message: string };

export function DeveloperLogsScreen() {
  const params = useSearchParams();
  const [state, setState] = useState<State>({ status: "loading" });
  const [keys, setKeys] = useState<DeveloperKey[]>([]);

  // The filters start from the URL so a link from the overview ("open the 4xx log") lands on the
  // right rows, and so a filtered view is shareable. `focus` is the overview's deep link into one
  // request; it opens the drawer and is then forgotten, because a drawer that reopens on every
  // reload is a drawer nobody can close.
  const [keyId, setKeyId] = useState<string>(() => params.get("api_key_id") ?? "");
  const [method, setMethod] = useState<string>(() => params.get("method") ?? "");
  const [prefix, setPrefix] = useState<string>(() => params.get("path_prefix") ?? "");
  const [statusClass, setStatusClass] = useState<string>(
    () => params.get("status_class") ?? "",
  );
  const [windowDays, setWindowDays] = useState<string>(
    () => params.get("window_days") ?? "7",
  );
  const [focus, setFocus] = useState<string | null>(() => params.get("focus"));
  const [open, setOpen] = useState<DeveloperLogDetail | null>(null);
  const [cursor, setCursor] = useState<number | null>(null);

  useEffect(() => {
    let cancelled = false;
    fetchDeveloperKeys()
      .then((value) => {
        if (!cancelled) setKeys(value);
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, []);

  const load = useCallback(
    async (before: number | null) => {
      setState({ status: "loading" });
      const filters: DeveloperLogFilters = {
        api_key_id: keyId || null,
        method: method || null,
        path_prefix: prefix || null,
        status_class: statusClass || null,
        window_days: windowDays ? Number(windowDays) : null,
      };
      try {
        const page = await fetchDeveloperLogs(filters, before);
        setState((current) => {
          // Paging appends; a fresh filter replaces. Doing this by comparing the cursor rather
          // than by a separate flag is what stops a filter change from *appending* to the old
          // result set — the shape of bug where the screen shows 200 rows and the filter says 4xx.
          const appending =
            before !== null &&
            current.status === "ready" &&
            current.nextBefore === before;
          return {
            status: "ready",
            rows: appending ? [...current.rows, ...page.rows] : page.rows,
            nextBefore: page.next_before,
            retention: null,
          };
        });
        setCursor(before);
      } catch (cause: unknown) {
        setState({
          status: "error",
          message:
            cause instanceof ApiError ? cause.message : "The request log could not be loaded.",
        });
      }
    },
    [keyId, method, prefix, statusClass, windowDays],
  );

  useEffect(() => {
    void load(null);
  }, [load]);

  // The overview's deep link: fetch that one request and open it. A drawer fed by the same
  // `get_log` route the row list would use, rather than by re-filtering the list to find it.
  useEffect(() => {
    if (!focus) return;
    const id = Number(focus);
    if (!Number.isFinite(id)) return;
    let cancelled = false;
    fetchDeveloperLog(id)
      .then((detail) => {
        if (!cancelled) setOpen(detail);
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, [focus]);

  const reset = () => {
    setKeyId("");
    setMethod("");
    setPrefix("");
    setStatusClass("");
    setWindowDays("7");
  };

  const exportCsv = () => {
    if (state.status !== "ready") return;
    const csv = developerLogsCsv(state.rows);
    // A `Blob` + object URL rather than a server route: the platform deliberately exposes no
    // bulk export for this table, and building one to satisfy a button would undo that.
    const url = URL.createObjectURL(new Blob([csv], { type: "text/csv;charset=utf-8" }));
    const anchor = document.createElement("a");
    anchor.href = url;
    anchor.download = "omnion-request-log.csv";
    anchor.click();
    URL.revokeObjectURL(url);
  };

  return (
    <div className="flex flex-col gap-4" data-developer-logs>
      <div className="flex flex-wrap items-end gap-2">
        <label className="flex flex-col gap-1">
          <span className="text-[11.5px] text-muted">Key</span>
          <select
            value={keyId}
            onChange={(event) => setKeyId(event.target.value)}
            className="rounded-lg border border-line bg-surface px-2.5 py-2 text-[12.5px]"
          >
            <option value="">Every key</option>
            {keys.map((key) => (
              <option key={key.id} value={key.id}>
                {key.name}
              </option>
            ))}
          </select>
        </label>

        <label className="flex flex-col gap-1">
          <span className="text-[11.5px] text-muted">Method</span>
          <select
            value={method}
            onChange={(event) => setMethod(event.target.value)}
            className="rounded-lg border border-line bg-surface px-2.5 py-2 text-[12.5px]"
          >
            {DEVELOPER_METHODS.map((value) => (
              <option key={value || "any"} value={value}>
                {value || "Any"}
              </option>
            ))}
          </select>
        </label>

        <label className="flex flex-col gap-1">
          <span className="text-[11.5px] text-muted">Status class</span>
          <select
            value={statusClass}
            onChange={(event) => setStatusClass(event.target.value)}
            className="rounded-lg border border-line bg-surface px-2.5 py-2 text-[12.5px]"
          >
            <option value="">Any status</option>
            {DEVELOPER_STATUS_CLASSES.map((value) => (
              <option key={value} value={value}>
                {value}
              </option>
            ))}
          </select>
        </label>

        <label className="flex flex-col gap-1">
          <span className="text-[11.5px] text-muted">Window</span>
          <select
            value={windowDays}
            onChange={(event) => setWindowDays(event.target.value)}
            className="rounded-lg border border-line bg-surface px-2.5 py-2 text-[12.5px]"
          >
            {DEVELOPER_WINDOWS.map((days) => (
              <option key={days} value={days}>
                Last {days} {days === 1 ? "day" : "days"}
              </option>
            ))}
          </select>
        </label>

        <label className="flex min-w-48 flex-1 flex-col gap-1">
          <span className="text-[11.5px] text-muted">Path starts with</span>
          <span className="relative">
            <Search
              className="pointer-events-none absolute top-2.5 left-2.5 size-3.5 text-muted"
              aria-hidden
            />
            <input
              type="search"
              value={prefix}
              onChange={(event) => setPrefix(event.target.value)}
              placeholder="/api/v1/media"
              className="w-full rounded-lg border border-line bg-surface py-2 pr-3 pl-8 font-mono text-[12.5px] outline-none focus:border-accent"
            />
          </span>
        </label>

        <button
          type="button"
          onClick={reset}
          className="rounded-lg border border-line px-2.5 py-2 text-[12.5px] transition hover:bg-quiet-soft"
        >
          Reset
        </button>
        <button
          type="button"
          onClick={() => void load(cursor)}
          disabled={state.status !== "ready" || state.nextBefore === null}
          className="rounded-lg border border-line px-2.5 py-2 text-[12.5px] transition hover:bg-quiet-soft disabled:opacity-40"
        >
          Older
        </button>
        <button
          type="button"
          onClick={exportCsv}
          disabled={state.status !== "ready" || state.rows.length === 0}
          className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-2 text-[12.5px] transition hover:bg-quiet-soft disabled:opacity-40"
        >
          <Download className="size-3.5" aria-hidden />
          Export this view
        </button>
        <button
          type="button"
          onClick={() => void load(null)}
          aria-label="Reload the log"
          className="rounded-lg border border-line bg-surface p-2 text-muted transition hover:text-ink"
        >
          <RefreshCw className="size-3.5" aria-hidden />
        </button>
      </div>

      {state.status === "loading" ? (
        <div className="overflow-hidden rounded-xl border border-line bg-surface" aria-busy="true">
          {[0, 1, 2, 3].map((row) => (
            <div key={row} className="flex gap-3 border-b border-line px-4 py-3 last:border-0">
              <span className="block h-3.5 w-32 animate-pulse rounded bg-quiet-soft" />
              <span className="block h-3.5 w-16 animate-pulse rounded bg-quiet-soft" />
              <span className="block h-3.5 w-48 animate-pulse rounded bg-quiet-soft" />
            </div>
          ))}
        </div>
      ) : null}

      {state.status === "error" ? (
        <div role="alert" className="rounded-xl border border-line bg-surface px-4 py-6">
          <p className="flex items-center gap-2 text-[13px] text-caution">
            <TriangleAlert className="size-4" aria-hidden />
            {state.message}
          </p>
          <button
            type="button"
            onClick={() => void load(null)}
            className="mt-3 inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-quiet-soft"
          >
            <RefreshCw className="size-3.5" aria-hidden />
            Try again
          </button>
        </div>
      ) : null}

      {state.status === "ready" && state.rows.length === 0 ? (
        <div className="rounded-xl border border-line bg-surface px-6 py-12 text-center">
          <p className="text-[13.5px] font-medium">No request matches these filters</p>
          <p className="mx-auto mt-1 max-w-sm text-[12.5px] text-muted">
            The log keeps a bounded window, and a path filter is a prefix match —{" "}
            <code className="font-mono">/api/v1/media</code> returns that subtree, not
            substrings.
          </p>
          <button
            type="button"
            onClick={reset}
            className="mt-3 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-quiet-soft"
          >
            Reset the filters
          </button>
        </div>
      ) : null}

      {state.status === "ready" && state.rows.length > 0 ? (
        <div className="rounded-xl border border-line bg-surface">
          {/*
            Same reasoning as the key list: seven columns do not fit a phone, and the two columns
            that must survive the squeeze are the call itself and the status. The card keeps both
            plus the caller, and the detail button keeps its 44 px hit target — a debugging table
            you cannot read on the device you are holding is half a table.
          */}
          <div className="hidden overflow-x-auto sm:block">
          <table className="w-full border-collapse text-left text-[13px]">
            <thead>
              <tr className="border-b border-line text-[11.5px] text-muted">
                <th scope="col" className="px-4 py-2.5 font-medium">Time</th>
                <th scope="col" className="px-4 py-2.5 font-medium">Method</th>
                <th scope="col" className="px-4 py-2.5 font-medium">Path</th>
                <th scope="col" className="px-4 py-2.5 font-medium">Status</th>
                <th scope="col" className="px-4 py-2.5 font-medium">Duration</th>
                <th scope="col" className="px-4 py-2.5 font-medium">Caller</th>
                <th scope="col" className="px-4 py-2.5 font-medium">
                  <span className="sr-only">Detail</span>
                </th>
              </tr>
            </thead>
            <tbody>
              {state.rows.map((row) => (
                <tr key={row.id} className="border-b border-line last:border-0">
                  <td className="px-4 py-2.5 text-[12px] whitespace-nowrap text-muted">
                    {formatTimestamp(row.created_at)}
                  </td>
                  <td className="px-4 py-2.5 font-mono text-[12px]">{row.method}</td>
                  <td className="px-4 py-2.5 font-mono text-[12px]">
                    {row.path}
                    {row.permission ? (
                      <span className="ml-2 text-[11px] text-muted">{row.permission}</span>
                    ) : null}
                  </td>
                  <td className="px-4 py-2.5">
                    <span
                      className={`inline-flex w-11 justify-center rounded px-1.5 py-0.5 text-[11px] font-medium ${
                        row.status >= 500
                          ? "bg-caution-soft text-caution"
                          : row.status >= 400
                            ? "bg-caution-soft text-caution"
                            : "bg-positive-soft text-positive"
                      }`}
                    >
                      {row.status}
                    </span>
                  </td>
                  <td className="px-4 py-2.5 text-[12px] tabular-nums text-muted">
                    {row.duration_ms} ms
                  </td>
                  <td className="px-4 py-2.5 text-[12px] text-muted">
                    {/* Key prefix first, then the session's name, then an explicit dash. The
                        `||` needs its parentheses: without them this is a syntax error, and
                        with a naive fix it becomes `(prefix ?? actor) || "—"` which is a
                        different expression from the one being read. */}
                    {row.api_key_prefix ?? (row.actor_name || "—")}
                  </td>
                  <td className="px-4 py-2.5 text-right">
                    <button
                      type="button"
                      onClick={() => {
                        setFocus(String(row.id));
                        void fetchDeveloperLog(row.id).then(setOpen).catch(() => undefined);
                      }}
                      aria-label={`Open request ${row.id}`}
                      className="rounded-lg border border-line p-1.5 text-muted transition hover:text-ink"
                    >
                      <PanelRightOpen className="size-3.5" aria-hidden />
                    </button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          </div>

          <ul data-developer-log-cards className="divide-y divide-line sm:hidden">
            {state.rows.map((row) => (
              <li
                key={row.id}
                data-developer-log-card={row.id}
                data-developer-log-card-status={row.status}
                className="flex flex-col gap-1.5 px-4 py-3"
              >
                <div className="flex items-baseline justify-between gap-2">
                  <span className="min-w-0 truncate font-mono text-[12px]">
                    {row.method} {row.path}
                  </span>
                  <span
                    className={`inline-flex w-11 shrink-0 justify-center rounded px-1.5 py-0.5 text-[11px] font-medium ${
                      row.status >= 400 ? "bg-caution-soft text-caution" : "bg-positive-soft text-positive"
                    }`}
                  >
                    {row.status}
                  </span>
                </div>
                <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-[11.5px] text-muted">
                  <span>{formatTimestamp(row.created_at)}</span>
                  <span className="tabular-nums">{row.duration_ms} ms</span>
                  <span className="truncate">
                    {row.api_key_prefix ?? (row.actor_name || "—")}
                  </span>
                </div>
                <button
                  type="button"
                  onClick={() => {
                    setFocus(String(row.id));
                    void fetchDeveloperLog(row.id).then(setOpen).catch(() => undefined);
                  }}
                  aria-label={`Open request ${row.id}`}
                  className="inline-flex min-h-11 w-fit items-center gap-1.5 rounded-lg border border-line px-2.5 text-[12.5px] transition"
                >
                  <PanelRightOpen className="size-3.5" aria-hidden />
                  Open the detail
                </button>
              </li>
            ))}
          </ul>
        </div>
      ) : null}

      {open ? <LogDrawer detail={open} onClose={() => setOpen(null)} /> : null}
    </div>
  );
}

/**
 * One request in full.
 *
 * The client fingerprint is shown as a short prefix, not in full: it is a keyed hash, so it is
 * safe to show, but it is also a stable identifier for a client and nothing here needs to
 * correlate beyond a handful of characters.
 */
function LogDrawer({
  detail,
  onClose,
}: {
  detail: DeveloperLogDetail;
  onClose: () => void;
}) {
  const { row } = detail;
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <div className="fixed inset-0 z-40 flex justify-end bg-ink/40" onClick={onClose}>
      <aside
        role="dialog"
        aria-modal="true"
        aria-labelledby="log-drawer-title"
        data-developer-log-drawer
        className="flex h-full w-full max-w-md flex-col gap-3 overflow-y-auto border-l border-line bg-surface p-5"
        onClick={(event) => event.stopPropagation()}
      >
        <div className="flex items-start justify-between gap-3">
          <h2 id="log-drawer-title" className="text-[15px] font-semibold">
            Request #{row.id}
          </h2>
          <button
            type="button"
            onClick={onClose}
            aria-label="Close"
            className="rounded-lg p-1 text-muted transition hover:text-ink"
          >
            <X className="size-4" aria-hidden />
          </button>
        </div>

        <dl className="flex flex-col gap-2 text-[12.5px]">
          <Row label="When">{formatTimestamp(row.created_at)}</Row>
          <Row label="Call">
            <span className="font-mono">
              {row.method} {row.path}
            </span>
          </Row>
          <Row label="Status">
            <span className="font-mono">
              {row.status} ({detail.status_class})
            </span>
          </Row>
          <Row label="Duration">
            <span className="tabular-nums">{row.duration_ms} ms</span>
          </Row>
          <Row label="Key">
            {row.api_key_prefix ? (
              <span className="font-mono">{row.api_key_prefix}</span>
            ) : (
              <span className="text-muted">a signed-in session</span>
            )}
          </Row>
          <Row label="Actor">
            {row.actor_name || (
              <span className="text-muted">
                nobody — this request was refused before it authenticated
              </span>
            )}
          </Row>
          <Row label="Scope checked">
            {row.permission ? (
              <span className="font-mono">{row.permission}</span>
            ) : (
              <span className="text-muted">no guard on this route</span>
            )}
          </Row>
          <Row label="Client">
            {row.client_fingerprint ? (
              // Never the address, and never the whole digest: the fingerprint is a keyed hash,
              // so the value is safe, but this screen only needs to tell two clients apart.
              <span className="font-mono">{row.client_fingerprint.slice(0, 12)}…</span>
            ) : (
              <span className="text-muted">no connection info</span>
            )}
          </Row>
        </dl>

        <p className="mt-auto rounded-lg border border-line bg-quiet-soft px-3 py-2 text-[11.5px] text-muted">
          No request body and no query string are stored. This log keeps {detail.retention_days}{" "}
          days; the audit trail is the record that outlives it.
        </p>
      </aside>
    </div>
  );
}

function Row({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="grid grid-cols-[110px_1fr] gap-2">
      <dt className="text-muted">{label}</dt>
      <dd className="min-w-0 break-words">{children}</dd>
    </div>
  );
}
