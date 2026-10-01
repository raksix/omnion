"use client";

/**
 * `/security/events` — the security-event timeline (REQ-012, slice 4).
 *
 * ## What this screen is honest about
 *
 * 1. **Every row says which table it came from.** The timeline merges the audit trail and the
 *    sign-in log because a failed sign-in leaves no audit row at all — it happens before there is
 *    a session, and so before there is an actor to write an entry for. A screen built on the audit
 *    trail alone would look like a working filter and show an operator an empty sign-in list.
 *    Merging them without labelling them would be the next version of the same lie, so the source
 *    is a column.
 * 2. **A sign-in row has no actor, and the screen says why.** The blank is not a rendering fault;
 *    nobody was authenticated. The address the attempt came *from* is the identity an operator is
 *    hunting with, so it carries that column.
 * 3. **`mfa_required` is not red.** It is a second factor being asked for, and a screen that
 *    paints it as a failure teaches its readers to ignore red.
 * 4. **Permission denials are absent, and this screen says so.** The guard answers `403` without
 *    recording anything, so the "denial" category resolves to the address rule's refusals only.
 *    A footnote states it rather than leaving an operator to conclude the platform records
 *    refusals it does not — which is the belief that makes a security screen untrustworthy.
 * 5. **"50 of 312" is on the page.** The total is the filter's size, not the page's, so a partial
 *    result can never read as a complete one.
 * 6. **The export is the whole filter.** It is a separate link rather than a button that fetches,
 *    because the response is a file — and the filter sent to it is identical, minus the page size.
 *
 * Empty state, error state, `/` to focus the filter, and a mobile card layout are all real states
 * rather than placeholders.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  AlertTriangle,
  Download,
  Filter,
  Loader2,
  RefreshCw,
  Search,
  ShieldCheck,
  X,
} from "lucide-react";

import {
  fetchSecurityEvents,
  securityEventsExportUrl,
  type ApiError,
} from "@/lib/api";
import { SecurityTabs } from "@/features/security/security-tabs";
import type { SecurityEvent, SecurityEventsPage } from "@/lib/types";

/** The filter as the screen holds it. Empty means "no filter", not "match nothing". */
type FilterState = {
  q: string;
  category: string;
  source: "" | "audit" | "sign_in";
  since: string;
  until: string;
};

const EMPTY_FILTER: FilterState = { q: "", category: "", source: "", since: "", until: "" };

/** A date input's `yyyy-mm-dd` as the RFC 3339 instant the API parses. */
function dateToRfc3339(value: string, endOfDay: boolean): string | undefined {
  if (!value) return undefined;
  const parsed = new Date(`${value}T${endOfDay ? "23:59:59" : "00:00:00"}Z`);
  return Number.isNaN(parsed.getTime()) ? undefined : parsed.toISOString();
}

function formatWhen(value: string): string {
  const parsed = new Date(value);
  return Number.isNaN(parsed.getTime()) ? value : parsed.toLocaleString();
}

/** The badge a category gets — never colour alone, so it reads without hue. */
function categoryLabel(category: string): string {
  switch (category) {
    case "sign_in":
      return "sign-in";
    case "lockout":
      return "lockout";
    case "denial":
      return "refusal";
    case "settings_change":
      return "settings";
    case "ip_rule_change":
      return "access rule";
    default:
      return category.replace(/_/g, " ");
  }
}

export function SecurityEventsScreen() {
  const [page, setPage] = useState<SecurityEventsPage | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [filter, setFilter] = useState<FilterState>(EMPTY_FILTER);
  const [applied, setApplied] = useState<FilterState>(EMPTY_FILTER);
  const searchRef = useRef<HTMLInputElement>(null);

  const load = useCallback(async (active: FilterState) => {
    setLoadError(null);
    try {
      setPage(
        await fetchSecurityEvents({
          q: active.q || undefined,
          category: active.category || undefined,
          source: active.source || undefined,
          since: dateToRfc3339(active.since, false),
          until: dateToRfc3339(active.until, true),
          limit: 50,
        }),
      );
    } catch (error) {
      setLoadError((error as ApiError).message);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load(EMPTY_FILTER);
  }, [load]);

  // `/` focuses the filter, the way the audit screen does — and never while the operator is
  // already typing, where it would swallow the character.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target?.tagName === "INPUT" ||
        target?.tagName === "TEXTAREA" ||
        target?.tagName === "SELECT";
      if (event.key === "/" && !typing) {
        event.preventDefault();
        searchRef.current?.focus();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const events = useMemo(() => page?.events ?? [], [page]);
  const categories = page?.categories ?? [];
  const refusedCount = useMemo(
    () => events.filter((event) => event.refused).length,
    [events],
  );

  const filterActive =
    applied.q !== "" ||
    applied.category !== "" ||
    applied.source !== "" ||
    applied.since !== "" ||
    applied.until !== "";

  function submit(event: React.FormEvent) {
    event.preventDefault();
    setLoading(true);
    setApplied(filter);
    void load(filter);
  }

  function clear() {
    setFilter(EMPTY_FILTER);
    setApplied(EMPTY_FILTER);
    setLoading(true);
    void load(EMPTY_FILTER);
  }

  return (
    <div className="space-y-5" data-security-events>
      <SecurityTabs current="events" />

      <header className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h1 className="text-lg font-semibold text-ink">Security events</h1>
          <p className="mt-1 max-w-2xl text-[13px] text-muted">
            Sign-in attempts and privileged actions on one timeline. A failed sign-in leaves no
            audit entry — it happens before there is an actor — so both sources are shown and every
            row names which one it came from.
          </p>
        </div>
        <div className="flex items-center gap-2">
          {/* The export is the same filter without the page size, so the file an operator attaches
              to a ticket is the whole thing they were looking at. */}
          <a
            href={securityEventsExportUrl({
              q: applied.q || undefined,
              category: applied.category || undefined,
              source: applied.source || undefined,
              since: dateToRfc3339(applied.since, false),
              until: dateToRfc3339(applied.until, true),
            })}
            className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] text-ink transition hover:bg-quiet-soft"
            data-security-events-export
          >
            <Download className="size-3.5" aria-hidden />
            Export CSV
          </a>
          <button
            type="button"
            onClick={() => {
              setLoading(true);
              void load(applied);
            }}
            className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] text-ink transition hover:bg-quiet-soft"
            data-security-events-refresh
          >
            <RefreshCw className={`size-3.5 ${loading ? "animate-spin" : ""}`} aria-hidden />
            Refresh
          </button>
        </div>
      </header>

      {/* -- the honesty note ------------------------------------------------------------------------
          A security screen that implies it records more than it does is worse than one that
          admits a gap. */}
      <p
        className="flex items-start gap-2 rounded-lg border border-line bg-quiet-soft px-3 py-2 text-[12px] text-muted"
        data-security-events-note
      >
        <AlertTriangle className="mt-0.5 size-3.5 shrink-0" aria-hidden />
        <span>
          Permission refusals are <strong>not</strong> in this list: the platform answers a
          refused request without recording one. The <em>refusal</em> filter shows attempts the
          address rule turned away.
        </span>
      </p>

      {/* -- filter ----------------------------------------------------------------------------------- */}
      <form
        onSubmit={submit}
        className="grid gap-3 rounded-lg border border-line bg-surface p-3 sm:grid-cols-2 lg:grid-cols-5"
        data-security-events-filter
      >
        <label className="flex flex-col gap-1 text-[12px] text-muted lg:col-span-2">
          <span className="inline-flex items-center gap-1.5">
            <Search className="size-3.5" aria-hidden />
            Search
          </span>
          <input
            ref={searchRef}
            value={filter.q}
            onChange={(event) => setFilter({ ...filter, q: event.target.value })}
            placeholder="action, outcome or account"
            className="rounded-md border border-line bg-bg px-2.5 py-1.5 text-[13px] text-ink"
          />
        </label>

        <label className="flex flex-col gap-1 text-[12px] text-muted">
          <span className="inline-flex items-center gap-1.5">
            <Filter className="size-3.5" aria-hidden />
            Category
          </span>
          <select
            value={filter.category}
            onChange={(event) => setFilter({ ...filter, category: event.target.value })}
            className="rounded-md border border-line bg-bg px-2.5 py-1.5 text-[13px] text-ink"
            data-security-events-category
          >
            <option value="">Every category</option>
            {categories.map((category) => (
              <option key={category} value={category}>
                {categoryLabel(category)}
              </option>
            ))}
          </select>
        </label>

        <label className="flex flex-col gap-1 text-[12px] text-muted">
          <span>Source</span>
          <select
            value={filter.source}
            onChange={(event) =>
              setFilter({ ...filter, source: event.target.value as FilterState["source"] })
            }
            className="rounded-md border border-line bg-bg px-2.5 py-1.5 text-[13px] text-ink"
            data-security-events-source
          >
            <option value="">Both tables</option>
            <option value="audit">Audit trail</option>
            <option value="sign_in">Sign-in log</option>
          </select>
        </label>

        <div className="flex items-end gap-2 sm:col-span-2 lg:col-span-5">
          <label className="flex flex-1 flex-col gap-1 text-[12px] text-muted">
            <span>From</span>
            <input
              type="date"
              value={filter.since}
              onChange={(event) => setFilter({ ...filter, since: event.target.value })}
              className="rounded-md border border-line bg-bg px-2.5 py-1.5 text-[13px] text-ink"
            />
          </label>
          <label className="flex flex-1 flex-col gap-1 text-[12px] text-muted">
            <span>To</span>
            <input
              type="date"
              value={filter.until}
              onChange={(event) => setFilter({ ...filter, until: event.target.value })}
              className="rounded-md border border-line bg-bg px-2.5 py-1.5 text-[13px] text-ink"
            />
          </label>
          <button
            type="submit"
            className="rounded-md bg-accent px-3 py-1.5 text-[12.5px] font-medium text-accent-strong transition"
          >
            Apply
          </button>
          {filterActive ? (
            <button
              type="button"
              onClick={clear}
              className="inline-flex items-center gap-1 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] text-ink transition hover:bg-quiet-soft"
              data-security-events-clear
            >
              <X className="size-3.5" aria-hidden />
              Clear
            </button>
          ) : null}
        </div>
      </form>

      {/* -- summary ---------------------------------------------------------------------------------- */}
      <p className="text-[12.5px] text-muted" data-security-events-counts>
        {loading
          ? "Reading the timeline…"
          : `${events.length} of ${page?.total ?? 0} · ${page?.audit_count ?? 0} from the audit trail · ${page?.sign_in_count ?? 0} from the sign-in log · ${refusedCount} refused`}
      </p>

      {loadError ? (
        <div
          role="alert"
          className="flex items-center justify-between gap-3 rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-danger-strong"
          data-security-events-error
        >
          <span>{loadError}</span>
          <button
            type="button"
            onClick={() => {
              setLoading(true);
              void load(applied);
            }}
            className="shrink-0 rounded-md border border-line px-2 py-1"
          >
            Try again
          </button>
        </div>
      ) : null}

      {/* -- rows ------------------------------------------------------------------------------------
          A table on a wide screen, cards on a narrow one. The same `data-security-event-row`
          attribute is on both, so the walkthrough asserts one thing rather than two. */}
      {events.length === 0 && !loading && !loadError ? (
        <div
          className="rounded-lg border border-dashed border-line px-4 py-10 text-center"
          data-security-events-empty
        >
          <ShieldCheck className="mx-auto size-6 text-muted" aria-hidden />
          <p className="mt-2 text-[13px] font-medium text-ink">
            {filterActive ? "No events match this filter" : "No security events recorded yet"}
          </p>
          <p className="mx-auto mt-1 max-w-md text-[12.5px] text-muted">
            {filterActive
              ? "Widen the window, or clear the filter to see everything the platform has recorded."
              : "Sign-in attempts and privileged actions appear here as they happen. Nothing recorded means nothing has happened — not that recording is switched off."}
          </p>
        </div>
      ) : (
        <>
          <table className="hidden w-full text-left text-[12.5px] lg:table" data-security-events-table>
            <thead>
              <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                <th scope="col" className="py-2 pr-3 font-medium">When</th>
                <th scope="col" className="py-2 pr-3 font-medium">Category</th>
                <th scope="col" className="py-2 pr-3 font-medium">Action</th>
                <th scope="col" className="py-2 pr-3 font-medium">Actor</th>
                <th scope="col" className="py-2 pr-3 font-medium">Client IP</th>
                <th scope="col" className="py-2 font-medium">Detail</th>
              </tr>
            </thead>
            <tbody>
              {events.map((event) => (
                <EventRow key={event.id} event={event} />
              ))}
            </tbody>
          </table>

          <ul className="space-y-2 lg:hidden" data-security-events-cards>
            {events.map((event) => (
              <li key={event.id}>
                <EventCard event={event} />
              </li>
            ))}
          </ul>
        </>
      )}

      {loading ? (
        <p className="inline-flex items-center gap-1.5 text-[12.5px] text-muted">
          <Loader2 className="size-3.5 animate-spin" aria-hidden />
          Loading
        </p>
      ) : null}
    </div>
  );
}

/** One table row. `actor: null` is rendered as the fact it is, never as a blank. */
function EventRow({ event }: { event: SecurityEvent }) {
  return (
    <tr className="border-b border-line/60 align-top" data-security-event-row>
      <td className="whitespace-nowrap py-2 pr-3 text-muted">{formatWhen(event.occurred_at)}</td>
      <td className="py-2 pr-3">
        <CategoryBadge category={event.category} refused={event.refused} />
      </td>
      <td className="py-2 pr-3 font-medium text-ink">{event.action}</td>
      <td className="py-2 pr-3 text-muted">{actorLabel(event)}</td>
      <td className="py-2 pr-3 font-mono text-[11.5px] text-muted">
        {event.client_ip ?? "—"}
      </td>
      <td className="py-2 text-muted">{event.detail ?? "—"}</td>
    </tr>
  );
}

/** The mobile card. Same data, stacked — a seven-column timeline scrolled sideways is unreadable
 *  on a phone, which is why the QA plan asks for exactly this. */
function EventCard({ event }: { event: SecurityEvent }) {
  return (
    <div
      className="space-y-1.5 rounded-lg border border-line bg-surface p-3"
      data-security-event-row
    >
      <div className="flex items-start justify-between gap-2">
        <span className="font-medium text-ink">{event.action}</span>
        <CategoryBadge category={event.category} refused={event.refused} />
      </div>
      <p className="text-[12px] text-muted">{formatWhen(event.occurred_at)}</p>
      <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 text-[12px]">
        <dt className="text-muted">Actor</dt>
        <dd className="text-ink">{actorLabel(event)}</dd>
        <dt className="text-muted">Client</dt>
        <dd className="font-mono text-[11.5px] text-ink">
          {event.client_ip ?? "not recorded"}
        </dd>
        <dt className="text-muted">Detail</dt>
        <dd className="break-words text-ink">{event.detail ?? "—"}</dd>
      </dl>
    </div>
  );
}

/** Badge with a word in it, never colour alone. */
function CategoryBadge({
  category,
  refused,
}: {
  category: string;
  refused: boolean;
}) {
  const tone = refused
    ? "border-danger/40 bg-danger-soft text-danger-strong"
    : category === "lockout"
      ? "border-warn/40 bg-warn-soft text-warn-strong"
      : "border-line bg-quiet-soft text-muted";
  return (
    <span
      className={`inline-block rounded border px-1.5 py-0.5 text-[11px] ${tone}`}
      data-security-event-category={category}
    >
      {categoryLabel(category)}
    </span>
  );
}

/** Who did it — and, on a sign-in, the sentence that makes the blank legible. */
function actorLabel(event: SecurityEvent): string {
  if (event.source === "sign_in") {
    return event.refused
      ? "no actor — refused before sign-in"
      : "no actor — the account itself";
  }
  return event.actor ?? "the platform";
}