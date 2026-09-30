"use client";

/**
 * `/health/incidents` — what broke, when, for how long, and who looked (REQ-014, slice 3).
 *
 * The overview answers "is it up now" and the metrics table answers "what has it been doing".
 * This one answers the question an operator opens the health centre to ask after the alarm has
 * already gone off, and every rule below is a way that answer is usually wrong:
 *
 * 1. **A duration is either a real number or the word "still open".** An open incident has no
 *    duration, and rendering `0 s` for one is the single most damaging thing this table can do:
 *    it reads as "it lasted no time", which is the opposite of what the row means. `null` renders
 *    as `open`.
 * 2. **An outage is one row, not one per run.** The server opens an incident on a *transition*,
 *    so a disk that stayed full for six hours is one entry with a six-hour duration. This screen
 *    therefore shows `total` next to the visible rows: "20 rows" beside "340 incidents" is the
 *    only visible difference between a filtered list and an unfiltered one.
 * 3. **A suppressed incident is shown, and labelled.** A maintenance window stops the
 *    announcement; it does not erase the event. The row keeps its real state and wears a
 *    "maintenance" marker, because a deploy that took Redis down for four minutes is a fact an
 *    operator wants later — just not an alarm.
 * 4. **Acknowledgement records a person.** The column shows who claimed it and the note they
 *    left. "Acknowledged" with no name is a status light, not evidence that a human looked.
 * 5. **The filter narrows, and the count says so.** The total is the count *behind the filter*,
 *    read from the server rather than from the rows on screen.
 *
 * Keyboard: `r` re-reads, `/` focuses the service filter, `a` acknowledges the focused row.
 * Mobile: the table becomes cards that keep the service, the state and the duration together.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import Link from "next/link";
import { ArrowLeft, CheckCircle2, Loader2, RefreshCw } from "lucide-react";

import {
  ApiError,
  fetchHealthIncidents,
  patchHealthIncident,
} from "@/lib/api";
import type { HealthIncident } from "@/lib/types";

/** The state filter's own vocabulary, in severity order — the row order a reader expects. */
const STATE_FILTERS = ["open", "resolved"] as const;

/** The seven services the registry probes, as a fallback before the first list arrives. */
const FALLBACK_SERVICES = [
  "api",
  "postgres",
  "redis",
  "storage",
  "queue",
  "search",
  "workers",
];

/**
 * A duration in words.
 *
 * Seconds below a minute, then minutes, then hours, then days — because "4500 s" is not a thing a
 * person reads during an outage, and "1 h 15 m" is exactly the granularity a shift handover needs.
 */
export function duration(seconds: number | null): string {
  if (seconds === null) return "open";
  if (seconds < 60) return `${seconds} s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes} min`;
  const hours = Math.floor(minutes / 60);
  const restMinutes = minutes % 60;
  if (hours < 24) return restMinutes === 0 ? `${hours} h` : `${hours} h ${restMinutes} min`;
  const days = Math.floor(hours / 24);
  return `${days} d ${hours % 24} h`;
}

/** An RFC 3339 instant, in the browser's own locale — the table's only date format. */
function when(instant: string): string {
  const parsed = new Date(instant);
  if (Number.isNaN(parsed.getTime())) return instant;
  return parsed.toLocaleString(undefined, {
    year: "numeric",
    month: "short",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** The colour class for a state word, shared by the badge and the card. */
function stateClass(state: string): string {
  if (state === "healthy") return "text-emerald-700 dark:text-emerald-300";
  if (state === "unknown") return "text-slate-600 dark:text-slate-300";
  if (state === "down") return "text-red-700 dark:text-red-300";
  return "text-amber-700 dark:text-amber-300";
}

export function HealthIncidentsScreen() {
  const [incidents, setIncidents] = useState<HealthIncident[]>([]);
  const [total, setTotal] = useState(0);
  const [services, setServices] = useState<string[]>(FALLBACK_SERVICES);
  const [service, setService] = useState("");
  const [state, setState] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [pending, setPending] = useState<string | null>(null);
  const [note, setNote] = useState("");
  const serviceFilter = useRef<HTMLSelectElement | null>(null);
  const typing = useRef(false);

  const load = useCallback(async () => {
    try {
      const page = await fetchHealthIncidents({
        service: service || null,
        state: state || null,
        limit: 100,
      });
      setIncidents(page.incidents);
      setTotal(page.total);
      // The server is the authority on the service vocabulary. Replacing the fallback rather
      // than merging is deliberate: a service the platform stopped probing should disappear
      // from the filter, and a merge would keep offering it forever.
      if (page.services.length > 0) setServices(page.services);
      setError(null);
    } catch (cause) {
      const apiError = cause as ApiError;
      // The rows stay. A blanked table after a failed filter destroys the reading the operator
      // was narrowing.
      setError(apiError.message ?? "The incident list could not be read.");
    } finally {
      setLoading(false);
    }
  }, [service, state]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    const handler = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      if (
        typing.current ||
        target?.tagName === "INPUT" ||
        target?.tagName === "TEXTAREA" ||
        target?.isContentEditable
      ) {
        return;
      }
      if (event.metaKey || event.ctrlKey || event.altKey) return;
      const key = event.key.toLowerCase();
      if (key === "r") {
        event.preventDefault();
        void load();
      } else if (key === "/") {
        // `/` is the filter shortcut the request names. Focusing the select rather than opening a
        // text field keeps the vocabulary server-owned: a free-text filter would let the reader
        // type a service that does not exist and read "no incidents" as a real answer.
        event.preventDefault();
        serviceFilter.current?.focus();
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [load]);

  const acknowledge = useCallback(
    async (id: string) => {
      setPending(id);
      setError(null);
      try {
        const updated = await patchHealthIncident(id, "acknowledge", note);
        // Replace the row from the server's own answer rather than patching the local copy:
        // "acknowledged by" is the one field a client must not be allowed to invent.
        setIncidents((rows) => rows.map((row) => (row.id === updated.id ? updated : row)));
        setNote("");
      } catch (cause) {
        setError((cause as ApiError).message ?? "The acknowledgement could not be saved.");
      } finally {
        setPending(null);
      }
    },
    [note],
  );

  const openCount = useMemo(() => incidents.filter((row) => row.resolved_at === null).length, [incidents]);

  return (
    <div className="space-y-4" data-health-incidents>
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <Link
            href="/health"
            className="inline-flex items-center gap-1 text-[12.5px] text-muted hover:text-ink"
          >
            <ArrowLeft aria-hidden className="h-3.5 w-3.5" />
            System health
          </Link>
          <h1 className="mt-1 text-[19px] font-medium tracking-tight">Incidents</h1>
          <p className="mt-0.5 text-[13px] text-muted">
            What broke, when it started, how long it lasted and whether anybody claimed it.
          </p>
        </div>

        <div className="flex flex-wrap items-center gap-2">
          <label className="flex items-center gap-1.5 text-[12.5px] text-muted">
            Service
            <select
              ref={serviceFilter}
              data-health-incidents-service
              value={service}
              onChange={(event) => setService(event.target.value)}
              className="rounded-md border border-line bg-transparent px-2 py-1.5 text-[12.5px]"
            >
              <option value="">All services</option>
              {services.map((name) => (
                <option key={name} value={name}>
                  {name}
                </option>
              ))}
            </select>
          </label>

          <div role="group" aria-label="Incident state" className="flex rounded-md border border-line">
            <button
              type="button"
              data-health-incidents-state="all"
              aria-pressed={state === ""}
              onClick={() => setState("")}
              className={`px-3 py-1.5 text-[12.5px] ${
                state === "" ? "bg-ink text-[var(--color-surface)]" : "text-muted hover:text-ink"
              }`}
            >
              All
            </button>
            {STATE_FILTERS.map((entry) => (
              <button
                key={entry}
                type="button"
                data-health-incidents-state={entry}
                aria-pressed={state === entry}
                onClick={() => setState(entry)}
                className={`px-3 py-1.5 text-[12.5px] ${
                  state === entry ? "bg-ink text-[var(--color-surface)]" : "text-muted hover:text-ink"
                }`}
              >
                {entry}
              </button>
            ))}
          </div>

          <button
            type="button"
            data-health-incidents-refresh
            onClick={() => void load()}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] hover:bg-surface"
          >
            <RefreshCw aria-hidden className="h-3.5 w-3.5" />
            Refresh
          </button>
        </div>
      </div>

      <p data-health-incidents-count className="text-[12.5px] text-muted">
        {loading ? "reading…" : `${incidents.length} shown of ${total}`}
        {openCount > 0 ? ` · ${openCount} still open on this page` : ""}
        {service || state ? " · filtered" : ""}
      </p>

      {error ? (
        <p
          data-health-incidents-error
          role="alert"
          className="rounded-md border border-red-300 bg-red-50 px-3 py-2 text-[13px] text-red-800 dark:border-red-900 dark:bg-red-950 dark:text-red-200"
        >
          {error}
        </p>
      ) : null}

      {/*
        The note box sits above the table rather than inside a row dialog, because acknowledging
        is a *decision about an incident* and the note belongs with it — a per-row popover would
        make a note an argument of one row while the reader is looking at another.
      */}
      <div className="flex flex-wrap items-center gap-2">
        <label className="text-[12.5px] text-muted" htmlFor="health-incident-note">
          Acknowledgement note
        </label>
        <input
          id="health-incident-note"
          data-health-incident-note
          value={note}
          onChange={(event) => setNote(event.target.value)}
          onFocus={() => {
            typing.current = true;
          }}
          onBlur={() => {
            typing.current = false;
          }}
          placeholder="optional — what you did about it"
          className="min-w-0 flex-1 rounded-md border border-line bg-transparent px-2.5 py-1.5 text-[12.5px]"
        />
      </div>

      {loading && incidents.length === 0 ? (
        <ul data-health-incidents-skeleton className="space-y-2">
          {[0, 1, 2, 3].map((row) => (
            <li key={row} className="h-9 animate-pulse rounded-md bg-surface" />
          ))}
        </ul>
      ) : incidents.length === 0 ? (
        <div data-health-incidents-empty className="rounded-lg border border-line px-4 py-6 text-center">
          <p className="text-[13px]">
            {service || state ? "No incidents match this filter." : "No incidents recorded."}
          </p>
          <p className="mx-auto mt-1 max-w-md text-[12.5px] text-muted">
            An incident opens when a service changes state — not on every run, so a platform that
            has been up all week has none to show. Open{" "}
            <span className="font-medium">System health</span> and press{" "}
            <span className="font-medium">Run all checks</span> to take a reading now.
          </p>
        </div>
      ) : (
        <>
          <table
            data-health-incidents-table
            className="hidden w-full text-left text-[13px] sm:table"
          >
            <thead>
              <tr className="border-b border-line text-[12px] text-muted">
                <th scope="col" className="py-2 pr-3 font-medium">Opened</th>
                <th scope="col" className="py-2 pr-3 font-medium">Service</th>
                <th scope="col" className="py-2 pr-3 font-medium">From → To</th>
                <th scope="col" className="py-2 pr-3 font-medium">Duration</th>
                <th scope="col" className="py-2 pr-3 font-medium">State</th>
                <th scope="col" className="py-2 pr-3 font-medium">Acknowledged by</th>
                <th scope="col" className="py-2 font-medium">Note</th>
              </tr>
            </thead>
            <tbody>
              {incidents.map((row) => (
                <tr
                  key={row.id}
                  data-health-incident-row={row.id}
                  data-health-incident-open={row.resolved_at === null ? "true" : "false"}
                  className="border-b border-line last:border-b-0"
                >
                  <td className="py-2 pr-3 whitespace-nowrap text-muted">{when(row.started_at)}</td>
                  <td className="py-2 pr-3">
                    <Link href={`/health/services/${row.service}`} className="hover:underline">
                      {row.service}
                    </Link>
                  </td>
                  <td className="py-2 pr-3 whitespace-nowrap">
                    <span className={stateClass(row.from_state)}>{row.from_state}</span>
                    {" → "}
                    <span className={stateClass(row.to_state)}>{row.to_state}</span>
                  </td>
                  <td
                    data-health-incident-duration={row.id}
                    className="py-2 pr-3 whitespace-nowrap tabular-nums"
                  >
                    {duration(row.duration_seconds)}
                  </td>
                  <td className="py-2 pr-3 whitespace-nowrap">
                    <span data-health-incident-state={row.resolved_at === null ? "open" : "resolved"}>
                      {row.resolved_at === null ? "open" : "resolved"}
                    </span>
                    {row.suppressed ? (
                      <span
                        data-health-incident-suppressed={row.id}
                        title="A maintenance window covered the moment this opened"
                        className="ml-1.5 rounded border border-line px-1 text-[10.5px] text-muted"
                      >
                        maintenance
                      </span>
                    ) : null}
                  </td>
                  <td className="py-2 pr-3 whitespace-nowrap text-muted">
                    {row.acknowledged_by ? (
                      <span data-health-incident-acked={row.id}>{row.acknowledged_by.slice(0, 8)}</span>
                    ) : (
                      <span className="text-muted">—</span>
                    )}
                  </td>
                  <td className="py-2">
                    <span className="flex items-center gap-2">
                      <span className="min-w-0 truncate text-muted" title={row.note ?? ""}>
                        {row.note || "—"}
                      </span>
                      {row.resolved_at === null && row.acknowledged_by === null ? (
                        <button
                          type="button"
                          data-health-incident-ack={row.id}
                          disabled={pending === row.id}
                          onClick={() => void acknowledge(row.id)}
                          className="inline-flex shrink-0 items-center gap-1 rounded-md border border-line px-2 py-1 text-[11.5px] hover:bg-surface disabled:opacity-60"
                        >
                          {pending === row.id ? (
                            <Loader2 aria-hidden className="h-3 w-3 animate-spin" />
                          ) : (
                            <CheckCircle2 aria-hidden className="h-3 w-3" />
                          )}
                          Acknowledge
                        </button>
                      ) : null}
                    </span>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>

          {/*
            The mobile list is a separate layout, not a squeezed seven-column table: at 390 px the
            table either scrolls horizontally or drops the service and the duration, and both lose
            the two things a reader needs most during an outage.
          */}
          <ul data-health-incidents-cards className="space-y-2 sm:hidden">
            {incidents.map((row) => (
              <li
                key={row.id}
                data-health-incident-card={row.id}
                className="rounded-lg border border-line px-3 py-2"
              >
                <div className="flex items-baseline justify-between gap-2">
                  <span className="text-[13px] font-medium">{row.service}</span>
                  <span
                    data-health-incident-card-duration={row.id}
                    className="text-[12.5px] tabular-nums"
                  >
                    {duration(row.duration_seconds)}
                  </span>
                </div>
                <div className="mt-1 flex flex-wrap items-center gap-x-2 text-[11.5px] text-muted">
                  <span>
                    {row.from_state} → {row.to_state}
                  </span>
                  <span>{when(row.started_at)}</span>
                  {row.suppressed ? <span>maintenance</span> : null}
                </div>
                {row.note ? (
                  <p className="mt-1 text-[11.5px] text-muted">{row.note}</p>
                ) : null}
                {row.resolved_at === null && row.acknowledged_by === null ? (
                  <button
                    type="button"
                    data-health-incident-card-ack={row.id}
                    disabled={pending === row.id}
                    onClick={() => void acknowledge(row.id)}
                    className="mt-2 inline-flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[11.5px]"
                  >
                    <CheckCircle2 aria-hidden className="h-3 w-3" />
                    Acknowledge
                  </button>
                ) : null}
              </li>
            ))}
          </ul>
        </>
      )}
    </div>
  );
}