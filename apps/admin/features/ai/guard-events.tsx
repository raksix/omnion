"use client";

/**
 * `/ai/guard/events` — what the guard decided (docs/requests/REQ-105, slice 1).
 *
 * An event row is a *decision*, never a payload. There is no column holding the inspected text,
 * so the screen cannot show one, and the drawer's only honest thing to print is the server's own
 * sentence saying so. That sentence is fetched with the rows rather than written here: two copies
 * of a security claim drift, and the copy that drifts is the one nobody re-reads.
 *
 * The filters are the screen's real content. A guard log is only useful if an operator can ask
 * "what did we refuse, for whom, in the last week", and each filter is one the server already
 * applies server-side — the client never filters a page it has not finished loading, which is
 * how a filtered table ends up claiming zero rows while page two holds nine.
 */
import { useCallback, useEffect, useState } from "react";
import Link from "next/link";

import { ChevronLeft, ChevronRight, X } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  type GuardEvent,
  type GuardEventDetail,
  fetchGuardEvent,
  fetchGuardEvents,
} from "@/lib/guard-api";
import { formatTimestamp } from "@/lib/format";

const PAGE_SIZE = 25;

/**
 * The action vocabulary, used to filter and to colour a row.
 *
 * **Three names, not five.** `0210` shipped `allowed`, `flagged`, `masked`, `blocked` and
 * `remapped`, but `flagged` and `remapped` are not values of the guard's verdict enum
 * (`clear` / `allowed` / `masked` / `blocked`), so nothing in the platform could ever produce
 * them: the filter offered two options that return nothing, forever, and looked like a
 * two-option mistake waiting to happen. `Action::Flag` is what makes `flagged` tempting — a
 * flagged match is stored as `allowed` with the rule key beside it, because flagging means "send
 * it, and record that a human should look", not "refuse".
 *
 * Kept in step with `GuardVerdict::recordable_names` (`crates/ai-hub/src/guard_data.rs`), which
 * is the single authority; the API rejects any other name in the `action` filter.
 */
const ACTIONS = ["allowed", "masked", "blocked"] as const;

/** How a row's action is coloured. `allowed` is deliberately quiet: it is the common case. */
function actionClass(action: string): string {
  switch (action) {
    case "blocked":
      return "bg-danger/10 text-danger";
    case "masked":
      return "bg-warning/10 text-warning";
    default:
      return "bg-muted/40 text-muted";
  }
}

/** The event log. */
export function GuardEvents() {
  const [rows, setRows] = useState<GuardEvent[]>([]);
  const [total, setTotal] = useState(0);
  const [offset, setOffset] = useState(0);
  const [note, setNote] = useState("");
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [detail, setDetail] = useState<GuardEventDetail | null>(null);

  const [action, setAction] = useState("");
  const [label, setLabel] = useState("");
  const [feature, setFeature] = useState("");
  const [blockedOnly, setBlockedOnly] = useState(false);
  const [from, setFrom] = useState("");
  const [to, setTo] = useState("");

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const page = await fetchGuardEvents({
        action: action || undefined,
        label: label || undefined,
        feature: feature || undefined,
        blocked: blockedOnly || undefined,
        // The date inputs are `date`, so the window is turned into whole ISO instants here rather
        // than sent as `2026-01-01`, which the server would read as midnight UTC and quietly
        // exclude a day the operator selected.
        from: from ? new Date(`${from}T00:00:00Z`).toISOString() : undefined,
        to: to ? new Date(`${to}T23:59:59Z`).toISOString() : undefined,
        limit: PAGE_SIZE,
        offset,
      });
      setRows(page.rows);
      setTotal(page.total);
      setNote(page.no_payload_note);
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : String(cause));
    } finally {
      setLoading(false);
    }
  }, [action, label, feature, blockedOnly, from, to, offset]);

  useEffect(() => {
    void load();
  }, [load]);

  /**
 * Every filter change returns to the first page.
 *
 * Written as six explicit setters rather than one generic `applyFilter(key, value)`: the generic
 * version needs a runtime key comparison to narrow the union, and TypeScript then types every
 * `value` as `string | boolean` — which is the compiler correctly reporting that the helper can
 * hand a boolean to a string setter. Six setters have no such hole and read the same at the call
 * site.
 */
  const reset = () => {
    setOffset(0);
    setAction("");
    setLabel("");
    setFeature("");
    setFrom("");
    setTo("");
    setBlockedOnly(false);
  };

  const openDetail = useCallback(async (id: number) => {
    setError(null);
    try {
      setDetail(await fetchGuardEvent(id));
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : String(cause));
    }
  }, []);

  const filtersActive = Boolean(action || label || feature || from || to || blockedOnly);

  return (
    <div className="flex flex-col gap-4" data-guard-events>
      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-5">
        <div>
          <label htmlFor="events-action" className="block text-[12px] font-medium">
            Action
          </label>
          <select
            id="events-action"
            data-guard-events-action
            value={action}
            onChange={(event) => {
              setOffset(0);
              setAction(event.target.value);
            }}
            className="mt-1 w-full rounded-md border border-line bg-bg px-2 py-1.5 text-[12.5px]"
          >
            <option value="">All</option>
            {ACTIONS.map((name) => (
              <option key={name} value={name}>
                {name}
              </option>
            ))}
          </select>
        </div>
        <div>
          <label htmlFor="events-label" className="block text-[12px] font-medium">
            Label
          </label>
          <input
            id="events-label"
            data-guard-events-label
            value={label}
            onChange={(event) => {
              setOffset(0);
              setLabel(event.target.value);
            }}
            placeholder="email_address"
            className="mt-1 w-full rounded-md border border-line bg-bg px-2 py-1.5 text-[12.5px]"
          />
        </div>
        <div>
          <label htmlFor="events-feature" className="block text-[12px] font-medium">
            Feature
          </label>
          <input
            id="events-feature"
            value={feature}
            onChange={(event) => {
              setOffset(0);
              setFeature(event.target.value);
            }}
            placeholder="crm"
            className="mt-1 w-full rounded-md border border-line bg-bg px-2 py-1.5 text-[12.5px]"
          />
        </div>
        <div>
          <label htmlFor="events-from" className="block text-[12px] font-medium">
            From
          </label>
          <input
            id="events-from"
            type="date"
            value={from}
            onChange={(event) => {
              setOffset(0);
              setFrom(event.target.value);
            }}
            className="mt-1 w-full rounded-md border border-line bg-bg px-2 py-1.5 text-[12.5px]"
          />
        </div>
        <div>
          <label htmlFor="events-to" className="block text-[12px] font-medium">
            To
          </label>
          <input
            id="events-to"
            type="date"
            value={to}
            onChange={(event) => {
              setOffset(0);
              setTo(event.target.value);
            }}
            className="mt-1 w-full rounded-md border border-line bg-bg px-2 py-1.5 text-[12.5px]"
          />
        </div>
      </div>

      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex items-center gap-3">
          <label className="flex items-center gap-2 text-[12.5px]">
            <input
              type="checkbox"
              checked={blockedOnly}
              onChange={(event) => {
                setOffset(0);
                setBlockedOnly(event.target.checked);
              }}
            />
            Refused only
          </label>
          {filtersActive ? (
            <button
              type="button"
              onClick={reset}
              className="text-[12px] underline underline-offset-2"
            >
              Clear filters
            </button>
          ) : null}
        </div>
        <p className="text-[12.5px] text-muted" aria-live="polite">
          {loading ? "Loading…" : `${total} event${total === 1 ? "" : "s"}`}
        </p>
      </div>

      {error ? (
        <p role="alert" className="rounded-md bg-danger/10 px-3 py-2 text-[12.5px] text-danger">
          {error}
        </p>
      ) : null}

      {loading && rows.length === 0 ? (
        <LoadingTable columns={6} />
      ) : rows.length === 0 ? (
        <EmptyState
          title={filtersActive ? "No event matches these filters." : "No guard events yet."}
          hint={
            filtersActive
              ? "Widen the window or clear the filters."
              : "An event is written the first time a prompt is inspected."
          }
          action={
            filtersActive ? (
              <button
                type="button"
                onClick={reset}
                className="rounded-md border border-line px-3 py-1.5 text-[12.5px]"
              >
                Clear filters
              </button>
            ) : (
              <Link
                href="/ai/guard/tester"
                className="rounded-md bg-ink px-3 py-1.5 text-[12.5px] text-bg"
              >
                Test a sample
              </Link>
            )
          }
        />
      ) : (
        <>
          <div className="hidden overflow-x-auto rounded-lg border border-line lg:block">
            <table className="w-full text-left text-[12.5px]">
              <caption className="sr-only">Guard decisions, newest first</caption>
              <thead className="border-b border-line text-[11.5px] text-muted">
                <tr>
                  <th scope="col" className="px-3 py-2 font-medium">Time</th>
                  <th scope="col" className="px-3 py-2 font-medium">Action</th>
                  <th scope="col" className="px-3 py-2 font-medium">Labels</th>
                  <th scope="col" className="px-3 py-2 font-medium">Matches</th>
                  <th scope="col" className="px-3 py-2 font-medium">Feature</th>
                  <th scope="col" className="px-3 py-2 font-medium">User</th>
                  <th scope="col" className="px-3 py-2 font-medium">Value hash</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-line">
                {rows.map((row) => (
                  <tr
                    key={row.id}
                    tabIndex={0}
                    role="button"
                    onClick={() => void openDetail(row.id)}
                    data-guard-event={row.id}
                    onKeyDown={(event) => {
                      if (event.key === "Enter" || event.key === " ") {
                        event.preventDefault();
                        void openDetail(row.id);
                      }
                    }}
                    className="cursor-pointer hover:bg-muted/20"
                  >
                    <td className="px-3 py-2">{formatTimestamp(row.created_at)}</td>
                    <td className="px-3 py-2">
                      <span className={`rounded px-1.5 py-0.5 text-[11px] ${actionClass(row.action)}`}>
                        {row.action}
                      </span>
                      {row.blocked ? (
                        <span className="ml-1.5 rounded bg-danger/10 px-1.5 py-0.5 text-[11px] text-danger">
                          refused
                        </span>
                      ) : null}
                    </td>
                    <td className="px-3 py-2">
                      <span className="flex flex-wrap gap-1">
                        {row.labels.length === 0 ? (
                          <span className="text-muted">—</span>
                        ) : (
                          row.labels.map((name) => (
                            <span key={name} className="rounded bg-muted/60 px-1.5 py-0.5 text-[11px]">
                              {name}
                            </span>
                          ))
                        )}
                      </span>
                    </td>
                    <td className="px-3 py-2 tabular-nums">{row.match_count}</td>
                    <td className="px-3 py-2">{row.feature ?? <span className="text-muted">—</span>}</td>
                    <td className="px-3 py-2 font-mono text-[11px]">
                      {row.user_id ? row.user_id.slice(0, 8) : <span className="text-muted">—</span>}
                    </td>
                    <td className="px-3 py-2 font-mono text-[11px]">
                      {row.value_hash ?? <span className="text-muted">—</span>}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          <ul className="flex flex-col gap-2 lg:hidden">
            {rows.map((row) => (
              <li key={row.id} data-guard-event-card={row.id}>
                <button
                  type="button"
                  onClick={() => void openDetail(row.id)}
                  className="w-full rounded-lg border border-line p-3 text-left"
                >
                  <div className="flex items-center justify-between gap-2">
                    <span className={`rounded px-1.5 py-0.5 text-[11px] ${actionClass(row.action)}`}>
                      {row.action}
                    </span>
                    <span className="text-[11.5px] text-muted">
                      {formatTimestamp(row.created_at)}
                    </span>
                  </div>
                  <p className="mt-1.5 flex flex-wrap gap-1">
                    {row.labels.map((name) => (
                      <span key={name} className="rounded bg-muted/60 px-1.5 py-0.5 text-[11px]">
                        {name}
                      </span>
                    ))}
                  </p>
                  <p className="mt-1 text-[11.5px] text-muted">
                    {row.match_count} match{row.match_count === 1 ? "" : "es"}
                    {row.feature ? ` · ${row.feature}` : ""}
                  </p>
                </button>
              </li>
            ))}
          </ul>

          {total > PAGE_SIZE ? (
            <nav aria-label="Event pages" className="flex items-center justify-between">
              <button
                type="button"
                onClick={() => setOffset(Math.max(0, offset - PAGE_SIZE))}
                disabled={offset === 0 || loading}
                className="inline-flex items-center gap-1 rounded-md border border-line px-3 py-1.5 text-[12.5px] disabled:opacity-40"
              >
                <ChevronLeft aria-hidden className="size-3.5" />
                Previous
              </button>
              <span className="text-[12.5px] text-muted">
                {offset + 1}–{Math.min(offset + PAGE_SIZE, total)} of {total}
              </span>
              <button
                type="button"
                onClick={() => setOffset(offset + PAGE_SIZE)}
                disabled={offset + PAGE_SIZE >= total || loading}
                className="inline-flex items-center gap-1 rounded-md border border-line px-3 py-1.5 text-[12.5px] disabled:opacity-40"
              >
                Next
                <ChevronRight aria-hidden className="size-3.5" />
              </button>
            </nav>
          ) : null}
        </>
      )}

      {note ? (
        <p className="rounded-md bg-muted/40 px-3 py-2 text-[12px] text-muted">{note}</p>
      ) : null}

      {detail ? <EventDrawer detail={detail} onClose={() => setDetail(null)} /> : null}
    </div>
  );
}

/**
 * The detail drawer.
 *
 * It shows the rules that fired, the counts per label and the short hashes — and it prints the
 * server's no-payload sentence rather than an apology for the absence. The hashes are worth
 * having: they prove a *specific* value was seen across two turns without either value being
 * recoverable, which is the only claim a guard log can honestly make.
 */
function EventDrawer({
  detail,
  onClose,
}: {
  detail: GuardEventDetail;
  onClose: () => void;
}) {
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <aside
      role="dialog"
      aria-label={`Guard event ${detail.id}`}
      data-guard-event-drawer
      className="rounded-lg border border-line"
    >
      <header className="flex items-center justify-between border-b border-line px-4 py-3">
        <div>
          <h2 className="text-[14px] font-medium">Event #{detail.id}</h2>
          <p className="text-[12px] text-muted">{formatTimestamp(detail.created_at)}</p>
        </div>
        <button
          type="button"
          onClick={onClose}
          aria-label="Close the event drawer"
          className="rounded-md border border-line p-1.5 hover:bg-muted/40"
        >
          <X aria-hidden className="size-3.5" />
        </button>
      </header>

      <div className="flex flex-col gap-3 p-4">
        <Row label="Decision">
          <span className={`rounded px-1.5 py-0.5 text-[11px] ${actionClass(detail.action)}`}>
            {detail.action}
          </span>
          {detail.blocked ? (
            <span className="ml-1.5 text-[12px] text-danger">
              refused{detail.error_code ? ` (${detail.error_code})` : ""}
            </span>
          ) : null}
        </Row>

        {detail.feature ? <Row label="Feature">{detail.feature}</Row> : null}
        {detail.user_id ? (
          <Row label="User">
            <span className="font-mono text-[11.5px]">{detail.user_id}</span>
          </Row>
        ) : null}
        {detail.run_id ? (
          <Row label="Run">
            <Link
              href={`/ai/runs/${detail.run_id}`}
              className="font-mono text-[11.5px] underline underline-offset-2"
            >
              {detail.run_id}
            </Link>
          </Row>
        ) : null}
        <Row label="Request">
          <span className="font-mono text-[11.5px]">{detail.request_id}</span>
        </Row>

        <div>
          <p className="text-[12px] font-medium">Rules that fired</p>
          {detail.rule_keys.length === 0 ? (
            <p className="text-[12.5px] text-muted">None — nothing matched.</p>
          ) : (
            <ul className="mt-1 flex flex-wrap gap-1.5">
              {detail.rule_keys.map((key) => (
                <li key={key} className="rounded bg-muted/60 px-1.5 py-0.5 font-mono text-[11px]">
                  {key}
                </li>
              ))}
            </ul>
          )}
        </div>

        <div>
          <p className="text-[12px] font-medium">Matches per label</p>
          <ul className="mt-1 flex flex-wrap gap-1.5">
            {Object.entries(detail.label_counts ?? {}).map(([label, count]) => (
              <li key={label} className="rounded bg-muted/60 px-1.5 py-0.5 text-[11px]">
                {label}: <span className="tabular-nums">{String(count)}</span>
              </li>
            ))}
          </ul>
        </div>

        {detail.value_hashes.length > 0 ? (
          <div>
            <p className="text-[12px] font-medium">Value hashes</p>
            <ul className="mt-1 flex flex-wrap gap-1.5">
              {detail.value_hashes.map((hash) => (
                <li key={hash} className="rounded bg-muted/60 px-1.5 py-0.5 font-mono text-[11px]">
                  {hash}
                </li>
              ))}
            </ul>
            <p className="mt-1 text-[11.5px] text-muted">
              The same hash on two events means the same value was seen twice. The value itself is
              not stored and cannot be recovered from here.
            </p>
          </div>
        ) : null}

        <p className="rounded-md bg-muted/40 px-3 py-2 text-[12px] text-muted">
          {detail.no_payload_note}
        </p>
      </div>
    </aside>
  );
}

/** One labelled row. The label is a real element so the pair reads the same with and without CSS. */
function Row({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex items-baseline gap-3">
      <span className="w-20 shrink-0 text-[12px] text-muted">{label}</span>
      <span className="text-[12.5px]">{children}</span>
    </div>
  );
}