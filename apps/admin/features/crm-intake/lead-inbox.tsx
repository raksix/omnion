"use client";

/**
 * `/crm/leads` — the lead inbox (docs/requests/REQ-117, slice 1).
 *
 * The screen answers one question — "what should I answer next?" — and every choice below is
 * shaped by that:
 *
 * 1. **The sort comes from the server, and it is not arrival order.** The API orders by
 *    breached first, then the soonest deadline. Re-sorting in the browser would fight the
 *    keyset cursor, because the page after this one is a window on *its* order.
 * 2. **The counters and the rows come from one read.** "3 open" above a table of five is the
 *    most damaging kind of wrong in a panel, so the two are never fetched separately and the
 *    screen says which filter they follow.
 * 3. **The empty state names the way out.** An empty inbox is either "no source captures
 *    yet" (an intake source has to exist before anything arrives) or "the filter hid them" —
 *    two different states with two different buttons, because a single dead end answers
 *    neither.
 * 4. **The status filter is a set of toggles, not a select.** The stored vocabulary is eight
 *    words, and an operator triaging an inbox thinks in plural ("new and unassigned"), so
 *    eight checkboxes beat a comma-joined single-select the reader has to know the syntax of.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";
import { useRouter, useSearchParams } from "next/navigation";
import { Check, Loader2, RefreshCw, Search, TriangleAlert, UserPlus, UserRoundCheck } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { ApiError } from "@/lib/api";
import {
  DECISION_LABEL,
  LEAD_STATUS_LABEL,
  LEAD_STATUS_TONE,
  LEAD_STATUSES,
  SLA_STATE_LABEL,
  SLA_STATE_TONE,
  contactLabel,
  countdown,
  isOpenLeadStatus,
  ownerLabel,
  relativeInstant,
  slaState,
} from "@/lib/crm-intake";
import {
  bulkAssignLeads,
  fetchIntakeSources,
  fetchLeads,
  fetchLeadMetrics,
  fetchLeadOwners,
  type BulkAssignReport,
  type IntakeSource,
  type Lead,
  type LeadInbox,
  type LeadOwner,
  type OrganizationMetrics,
} from "@/lib/crm-intake-api";

const PAGE = 25;

/** Read the filters out of the query string, so a filtered view can be pasted to a colleague. */
function filtersFrom(params: URLSearchParams): { status: string[]; owner: string; q: string; source: string } {
  const status = params.getAll("status").filter((value) => (LEAD_STATUSES as readonly string[]).includes(value));
  return {
    status,
    owner: params.get("owner") ?? "",
    q: params.get("q") ?? "",
    source: params.get("source") ?? "",
  };
}

export function LeadInbox() {
  const router = useRouter();
  const params = useSearchParams();
  const filters = useMemo(
    () => filtersFrom(new URLSearchParams(params?.toString() ?? "")),
    [params],
  );

  const [inbox, setInbox] = useState<LeadInbox | null>(null);
  const [sources, setSources] = useState<IntakeSource[]>([]);
  // The roster behind the owner column. Read once, and its absence is invisible to the table:
  // a lead whose owner cannot be named still has to appear, with the short id the trail uses.
  const [owners, setOwners] = useState<LeadOwner[]>([]);
  // The bulk hand-over. Selection is a set of ids, the owner is one value and the reason is
  // one sentence shared by the whole batch — because twenty rows moved for twenty different
  // reasons is a trail nobody can read afterwards.
  const [selected, setSelected] = useState<string[]>([]);
  const [bulkOpen, setBulkOpen] = useState(false);
  const [bulkOwner, setBulkOwner] = useState("");
  const [bulkReason, setBulkReason] = useState("");
  const [bulkBusy, setBulkBusy] = useState(false);
  const [bulkReport, setBulkReport] = useState<BulkAssignReport | null>(null);
  const [bulkError, setBulkError] = useState<string | null>(null);
  const [cursor, setCursor] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(
    async (append = false) => {
      setLoading(true);
      setError(null);
      try {
        const page = await fetchLeads({
          status: filters.status,
          owner: filters.owner || undefined,
          q: filters.q || undefined,
          source: filters.source || undefined,
          limit: PAGE,
          before: append ? (cursor ?? undefined) : undefined,
        });
        setInbox((previous) =>
          append && previous ? { ...page, leads: [...previous.leads, ...page.leads] } : page,
        );
        setCursor(page.next_before);
        // A filter change re-reads the page, and a selection of twenty ids that are no longer
        // on it is a bulk press about rows the operator cannot see.
        if (!append) setSelected([]);
      } catch (caught) {
        setError(caught instanceof ApiError ? caught.message : "The inbox could not be read.");
      } finally {
        setLoading(false);
      }
    },
    [filters, cursor],
  );

  useEffect(() => {
    void load(false);
  }, [load]);

  // The source list drives the source filter and the empty state's call to action. It is read
  // once: it changes when an operator edits a source, and the inbox is not the place they do.
  useEffect(() => {
    let cancelled = false;
    fetchIntakeSources()
      .then((rows) => {
        if (!cancelled) setSources(rows);
      })
      .catch(() => {
        /* the filter degrades to "every source", which is the honest default */
      });
    return () => {
      cancelled = true;
    };
  }, []);

  // The same rule for the roster. A lead that cannot say who owns it is still a lead, so a
  // roster failure must not empty the table — it falls back to the short id.
  useEffect(() => {
    let cancelled = false;
    fetchLeadOwners()
      .then((rows) => {
        if (!cancelled) setOwners(rows);
      })
      .catch(() => {
        /* the owner column degrades to the id, which is still true */
      });
    return () => {
      cancelled = true;
    };
  }, []);

  /**
   * The organization-wide counters, read **beside** the page rather than out of it.
   *
   * The inbox's own metrics describe the rows on screen, which is the right answer for "how
   * many of these are breached" and the wrong one for "how fast do we answer". A median taken
   * from one filtered page moves every time somebody types in the search box and looks like a
   * service-level number, so it has its own endpoint — and it is read once, not on every
   * page turn, because no number on this screen is derived from the filter list.
   *
   * A failure here degrades the tile alone. The tile is a summary; the table underneath it is
   * the work, and a metrics read must never be the reason the inbox does not draw.
   */
  const [orgMetrics, setOrgMetrics] = useState<OrganizationMetrics | null>(null);
  const [orgMetricsFailed, setOrgMetricsFailed] = useState(false);
  useEffect(() => {
    let cancelled = false;
    fetchLeadMetrics()
      .then((answer) => {
        if (cancelled) return;
        setOrgMetrics(answer);
        setOrgMetricsFailed(false);
      })
      .catch(() => {
        if (cancelled) return;
        setOrgMetrics(null);
        setOrgMetricsFailed(true);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const toggleSelected = (id: string) => {
    setSelected((previous) =>
      previous.includes(id) ? previous.filter((row) => row !== id) : [...previous, id],
    );
  };

  const runBulk = async () => {
    setBulkBusy(true);
    setBulkError(null);
    try {
      const report = await bulkAssignLeads(
        selected,
        bulkOwner === "" ? null : bulkOwner,
        bulkReason.trim(),
      );
      setBulkReport(report);
      setBulkOpen(false);
      setBulkReason("");
      // The rows that moved now have an owner, so the table has to be re-read: leaving the
      // old owner in the column next to a toast that says otherwise is how an operator stops
      // believing the screen.
      await load(false);
    } catch (caught) {
      setBulkError(caught instanceof ApiError ? caught.message : "The batch could not be handed over.");
    } finally {
      setBulkBusy(false);
    }
  };

  const setParam = (key: string, value: string) => {
    const next = new URLSearchParams(params?.toString() ?? "");
    if (value) next.set(key, value);
    else next.delete(key);
    const query = next.toString();
    router.replace(query ? `/crm/leads?${query}` : "/crm/leads");
  };

  const toggleStatus = (status: string) => {
    const next = new URLSearchParams(params?.toString() ?? "");
    const current = next.getAll("status");
    next.delete("status");
    for (const value of current) {
      if (value !== status) next.append("status", value);
    }
    if (!current.includes(status)) next.append("status", status);
    const query = next.toString();
    router.replace(query ? `/crm/leads?${query}` : "/crm/leads");
  };

  const ownersById = new Map(owners.map((owner) => [owner.id, owner.label]));
  const leads = inbox?.leads ?? [];
  const metrics = inbox?.metrics ?? null;
  const activeFilters =
    filters.status.length + (filters.owner ? 1 : 0) + (filters.q ? 1 : 0) + (filters.source ? 1 : 0);
  const hasAnySource = sources.length > 0;

  return (
    <div className="flex flex-col gap-4" data-testid="crm-lead-inbox">
      {/* The counters. They follow the filter list, and the screen says so — a number nobody
          can interpret is decoration. */}
      {metrics ? (
        <section
          aria-label="Inbox counters"
          data-lead-metrics
          className="grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-6"
        >
          <Counter label="Open" value={metrics.open} test="open" />
          <Counter label="Breached" value={metrics.breached} test="breached" tone="breach" />
          <Counter label="Unassigned" value={metrics.unassigned} test="unassigned" />
          <Counter label="Duplicates" value={metrics.duplicates} test="duplicates" />
          <Counter label="Discarded" value={metrics.discarded} test="discarded" />
          <Counter label="Converted" value={metrics.converted} test="converted" />
        </section>
      ) : null}

      {/*
        The one tile that is NOT page-scoped, and it says so in its own words.

        The six above follow the filter list — they describe the rows underneath. A median
        response time that also followed the filters would be a service-level number that
        changes when somebody types in the search box, which is the kind of metric that gets
        quoted in a review and then turns out to have been one filtered page. This one is
        read from `/crm/leads/metrics` over every lead the organization holds.

        `null` is rendered as "no answered lead yet" and **not** as zero. The panel's own
        SLA work has already been bitten by a counter that quietly reported the absence of a
        fact as the presence of a number: zero minutes is the fastest team in the world, and
        it is what a lead nobody replied to looks like.
      */}
      {orgMetrics ? (
        <section
          aria-label="Organization response time"
          data-lead-metrics-org
          className="flex flex-wrap items-center justify-between gap-2 rounded-xl border border-line bg-surface px-3 py-2"
        >
          <div className="flex items-baseline gap-2">
            <span className="text-[11.5px] font-medium uppercase tracking-wide text-muted">
              Median first response
            </span>
            <span
              data-lead-metrics-median
              data-lead-metrics-median-value={orgMetrics.median_response_minutes ?? ""}
              className="text-[15px] font-semibold text-ink"
            >
              {orgMetrics.median_response_minutes === null
                ? "no answered lead yet"
                : `${orgMetrics.median_response_minutes} min`}
            </span>
          </div>
          <span className="text-[11.5px] text-muted">
            Across every lead, not this filter.
          </span>
        </section>
      ) : orgMetricsFailed ? (
        <section
          aria-label="Organization response time"
          data-lead-metrics-org
          data-lead-metrics-org-error
          className="rounded-xl border border-line bg-surface px-3 py-2"
        >
          <span className="text-[11.5px] text-muted">
            The organization-wide response time could not be read. The inbox below is
            unaffected.
          </span>
        </section>
      ) : null}

      <section
        aria-label="Filters"
        data-lead-filters
        className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-3"
      >
        <div className="flex flex-wrap items-end gap-3">
          <label className="flex flex-col gap-1 text-[11.5px] text-muted">
            <span className="font-medium">Search name, e-mail or message</span>
            <input
              id="lead-search"
              type="search"
              defaultValue={filters.q}
              placeholder="Name, e-mail, message"
              className="w-56 rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink outline-none focus-visible:ring-2 focus-visible:ring-accent"
              onChange={(event) => setParam("q", event.target.value.trim())}
            />
          </label>

          <label className="flex flex-col gap-1 text-[11.5px] text-muted">
            <span className="font-medium">Source</span>
            <select
              id="lead-source"
              value={filters.source}
              onChange={(event) => setParam("source", event.target.value)}
              className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink outline-none focus-visible:ring-2 focus-visible:ring-accent"
            >
              <option value="">Every source</option>
              {sources.map((source) => (
                <option key={source.id} value={source.id}>
                  {source.name}
                </option>
              ))}
            </select>
          </label>

          <label className="flex flex-col gap-1 text-[11.5px] text-muted">
            <span className="font-medium">Owner</span>
            {/* "Me" and "Unassigned" are the two an operator uses daily; the roster is the
                third, which is "everything one colleague is answerable for" — and it was
                impossible to ask before, because the only way to name a colleague was to
                paste their uuid. */}
            <select
              id="lead-owner"
              value={filters.owner}
              onChange={(event) => setParam("owner", event.target.value)}
              className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink outline-none focus-visible:ring-2 focus-visible:ring-accent"
            >
              <option value="">Anyone</option>
              <option value="me">Owned by me</option>
              <option value="unassigned">Unassigned</option>
              {owners.map((owner) => (
                <option key={owner.id} value={owner.id} data-owner-filter={owner.id}>
                  {owner.label} · {owner.open_leads} open
                </option>
              ))}
            </select>
          </label>

          <button
            type="button"
            onClick={() => void load(false)}
            data-lead-refresh
            className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] text-muted transition hover:text-ink"
          >
            <RefreshCw className="size-3.5" aria-hidden />
            Refresh
          </button>

          <div className="ml-auto flex items-center gap-2">
            {activeFilters > 0 ? (
              <button
                type="button"
                onClick={() => router.replace("/crm/leads")}
                data-lead-reset
                className="rounded-lg px-2.5 py-1.5 text-[12.5px] text-accent-strong hover:underline"
              >
                Reset filters
              </button>
            ) : null}
            <Link
              href="/crm/leads/duplicates"
              data-lead-duplicates-link
              className="rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] text-muted transition hover:text-ink"
            >
              Duplicate queue
            </Link>
          </div>
        </div>

        <fieldset className="flex flex-wrap items-center gap-1.5" data-lead-status-filter>
          <legend className="sr-only">Filter by status</legend>
          {LEAD_STATUSES.map((status) => {
            const active = filters.status.includes(status);
            // The chips read the OPEN half, because that is the question a
            // reader is asking when they scan this row: which of these still
            // need somebody. Inlining the inverse is what left
            // `OPEN_LEAD_STATUSES` uncalled for the module's whole life.
            const open = isOpenLeadStatus(status);
            return (
              <button
                key={status}
                type="button"
                aria-pressed={active}
                data-status={status}
                onClick={() => toggleStatus(status)}
                className={`rounded-full border px-2.5 py-1 text-[11.5px] transition ${
                  active
                    ? "border-accent bg-accent-soft text-accent-strong"
                    : open
                      ? "border-line text-ink hover:bg-quiet-soft"
                      : "border-line text-muted hover:text-ink"
                }`}
              >
                {LEAD_STATUS_LABEL[status] ?? status}
              </button>
            );
          })}
        </fieldset>
      </section>

      {error ? (
        <div
          data-lead-error
          role="alert"
          className="flex items-start gap-2 rounded-lg border border-red-500/40 bg-red-500/5 px-3 py-2.5 text-[12.5px]"
        >
          <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-red-600" aria-hidden />
          <span className="flex-1">{error}</span>
          <button type="button" onClick={() => void load(false)} className="text-accent-strong hover:underline">
            Retry
          </button>
        </div>
      ) : null}

      <div className="overflow-hidden rounded-xl border border-line bg-surface">
        {loading && leads.length === 0 ? (
          <LoadingTable columns={6} rows={6} />
        ) : leads.length === 0 ? (
          activeFilters > 0 ? (
            <EmptyState
              title="No leads match these filters."
              hint="The counters above follow the same filters, so they describe this view and not the whole inbox."
              action={
                <button
                  type="button"
                  onClick={() => router.replace("/crm/leads")}
                  className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-accent-strong hover:underline"
                >
                  Reset filters
                </button>
              }
            />
          ) : hasAnySource ? (
            <EmptyState
              title="No leads yet."
              hint="Nothing has arrived through an intake source. Check that the source is active and that its form posts to the capture path."
              action={
                <Link
                  href="/crm/settings/intake"
                  className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-accent-strong hover:underline"
                >
                  Open intake sources
                </Link>
              }
            />
          ) : (
            <EmptyState
              title="No intake source yet"
              hint="A lead arrives through a source — a website form or a keyed endpoint. Create the first one and its capture URL appears here."
              action={
                <Link
                  href="/crm/settings/intake"
                  data-lead-create-source
                  className="inline-flex items-center gap-1.5 rounded-lg border border-accent bg-accent-soft px-3 py-1.5 text-[12.5px] text-accent-strong"
                >
                  <UserPlus className="size-3.5" aria-hidden />
                  Create an intake source
                </Link>
              }
            />
          )
        ) : (
          <LeadTable
            leads={leads}
            ownersById={ownersById}
            selected={selected}
            onToggle={toggleSelected}
            onToggleAll={() =>
              setSelected((previous) =>
                previous.length === leads.length ? [] : leads.map((lead) => lead.id),
              )
            }
            allSelected={leads.length > 0 && selected.length === leads.length}
          />
        )}
      </div>

      {/* The bulk hand-over. It appears only when something is selected, because a permanent
          bar above a table of twenty leads is a control nobody reads. The reason is required
          and there is exactly one, because twenty rows moved for twenty different reasons is
          a trail nobody can read afterwards. */}
      {selected.length > 0 ? (
        <div
          data-lead-bulk
          className="sticky bottom-0 z-20 flex flex-col gap-2 rounded-xl border border-line bg-canvas/95 px-3 py-2.5 backdrop-blur"
        >
          <div className="flex flex-wrap items-center gap-2">
            <span className="text-[12.5px] font-medium" data-lead-bulk-count>
              {selected.length} selected
            </span>
            {!bulkOpen ? (
              <button
                type="button"
                data-lead-bulk-open
                onClick={() => {
                  setBulkReport(null);
                  setBulkError(null);
                  setBulkOpen(true);
                }}
                className="inline-flex items-center gap-1.5 rounded-lg border border-accent bg-accent-soft px-3 py-1.5 text-[12.5px] text-accent-strong"
              >
                <UserRoundCheck className="size-3.5" aria-hidden />
                Hand them over
              </button>
            ) : null}
            <button
              type="button"
              onClick={() => {
                setSelected([]);
                setBulkOpen(false);
              }}
              className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-muted"
            >
              Clear
            </button>
          </div>

          {bulkOpen ? (
            <div data-lead-bulk-panel className="flex flex-col gap-2 rounded-lg border border-line bg-surface p-2.5">
              <label className="flex flex-col gap-1 text-[11.5px] text-muted">
                <span className="font-medium">New owner</span>
                <select
                  data-lead-bulk-owner
                  value={bulkOwner}
                  onChange={(event) => setBulkOwner(event.target.value)}
                  className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink"
                >
                  <option value="">Unassigned queue</option>
                  {owners.map((owner) => (
                    <option key={owner.id} value={owner.id} data-bulk-owner-option={owner.id}>
                      {owner.label} · {owner.open_leads} open
                    </option>
                  ))}
                </select>
              </label>
              <label className="flex flex-col gap-1 text-[11.5px] text-muted">
                <span className="font-medium">Why (one reason for the whole batch)</span>
                <input
                  data-lead-bulk-reason
                  value={bulkReason}
                  onChange={(event) => setBulkReason(event.target.value)}
                  placeholder="On call this week"
                  className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink"
                />
              </label>
              <div className="flex flex-wrap items-center gap-2">
                <button
                  type="button"
                  data-lead-bulk-save
                  disabled={bulkBusy || bulkReason.trim() === ""}
                  onClick={() => void runBulk()}
                  className="inline-flex items-center gap-1.5 rounded-lg border border-accent bg-accent-soft px-3 py-1.5 text-[12.5px] text-accent-strong disabled:opacity-50"
                >
                  {bulkBusy ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : <Check className="size-3.5" aria-hidden />}
                  Hand over {selected.length}
                </button>
                <button
                  type="button"
                  onClick={() => setBulkOpen(false)}
                  className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-muted"
                >
                  Cancel
                </button>
                <span className="text-[11.5px] text-muted">
                  Each lead is decided on its own: a row filed as spam stays put and says why,
                  and the rest still move.
                </span>
              </div>
              {bulkReason.trim() === "" ? (
                <p className="text-[11.5px] text-muted">
                  A reason is required — a lead that changed hands with no explanation cannot be
                  explained to the person who had it.
                </p>
              ) : null}
            </div>
          ) : null}

          {bulkError ? (
            <p role="alert" data-lead-bulk-error className="text-[11.5px] text-caution">
              {bulkError}
            </p>
          ) : null}

          {bulkReport ? (
            /* The report is the answer, and it names the refusals. "20 assigned" against
               nineteen real hand-overs is the failure this whole shape exists to prevent. */
            <div data-lead-bulk-report className="flex flex-col gap-1.5 text-[11.5px]">
              <p
                className={
                  bulkReport.refused > 0
                    ? "text-caution"
                    : "text-positive"
                }
              >
                {bulkReport.summary}
              </p>
              {bulkReport.refused > 0 ? (
                <ul data-lead-bulk-refusals className="flex flex-col gap-0.5 text-muted">
                  {bulkReport.results
                    .filter((row) => !row.done)
                    .map((row) => (
                      <li key={row.id} data-bulk-refusal={row.id}>
                        {leads.find((lead) => lead.id === row.id)
                          ? contactLabel(leads.find((lead) => lead.id === row.id) as Lead)
                          : row.id.slice(0, 8)}
                        {" — "}
                        {row.reason}
                      </li>
                    ))}
                </ul>
              ) : null}
            </div>
          ) : null}
        </div>
      ) : null}

      {cursor ? (
        <div className="flex justify-center">
          <button
            type="button"
            data-lead-more
            disabled={loading}
            onClick={() => void load(true)}
            className="inline-flex items-center gap-1.5 rounded-lg border border-line bg-surface px-3 py-1.5 text-[12.5px] text-muted transition hover:text-ink disabled:opacity-50"
          >
            <Search className="size-3.5" aria-hidden />
            {loading ? "Loading…" : "Load older"}
          </button>
        </div>
      ) : null}
    </div>
  );
}

/** One counter tile. A breach is the only one that gets the panel's red. */
function Counter({
  label,
  value,
  test,
  tone,
}: {
  label: string;
  value: number;
  test: string;
  tone?: "breach";
}) {
  return (
    <div
      data-metric={test}
      className={`rounded-xl border px-3 py-2.5 ${
        tone === "breach" && value > 0
          ? "border-red-500/40 bg-red-500/5"
          : "border-line bg-surface"
      }`}
    >
      <p className="text-[11px] text-muted">{label}</p>
      <p
        className={`mt-0.5 text-[19px] font-semibold tabular-nums ${
          tone === "breach" && value > 0 ? "text-red-700" : "text-ink"
        }`}
      >
        {value}
      </p>
    </div>
  );
}

/**
 * The table. Every cell has an `md:` counterpart, so below the breakpoint the row collapses
 * into two lines (identity + status, then the details) rather than a horizontally scrolling
 * grid — a lead inbox is read on a phone, and a 7-column table on a 390px screen is a table
 * nobody reads.
 */
function LeadTable({
  leads,
  ownersById,
  selected,
  onToggle,
  onToggleAll,
  allSelected,
}: {
  leads: Lead[];
  ownersById: Map<string, string>;
  selected: string[];
  onToggle: (id: string) => void;
  onToggleAll: () => void;
  allSelected: boolean;
}) {
  return (
    <div className="overflow-x-auto">
      <table data-lead-table className="w-full border-collapse text-left text-[13px]">
        <thead>
          <tr className="border-b border-line text-[11.5px] text-muted">
            <th scope="col" className="w-8 px-3 py-2.5">
              <input
                type="checkbox"
                data-lead-select-all
                aria-label="Select every lead on this page"
                checked={allSelected}
                onChange={onToggleAll}
                className="size-3.5 accent-[var(--color-accent)]"
              />
            </th>
            <th scope="col" className="px-3 py-2.5 font-medium">Received</th>
            <th scope="col" className="px-3 py-2.5 font-medium">Contact</th>
            <th scope="col" className="px-3 py-2.5 font-medium">Product</th>
            <th scope="col" className="px-3 py-2.5 font-medium">Owner</th>
            <th scope="col" className="px-3 py-2.5 font-medium">First response</th>
            <th scope="col" className="px-3 py-2.5 font-medium">Status</th>
          </tr>
        </thead>
        <tbody>
          {leads.map((lead) => {
            const state = slaState(lead);
            const remaining = countdown(lead.first_response_due_at);
            return (
              <tr
                key={lead.id}
                data-lead-row={lead.id}
                className="border-b border-line/60 transition last:border-0 hover:bg-quiet-soft"
              >
                <td className="px-3 py-2.5">
                  <input
                    type="checkbox"
                    data-lead-select={lead.id}
                    aria-label={`Select the lead from ${contactLabel(lead)}`}
                    checked={selected.includes(lead.id)}
                    onChange={() => onToggle(lead.id)}
                    className="size-3.5 accent-[var(--color-accent)]"
                  />
                </td>
                <td className="px-3 py-2.5 text-muted tabular-nums" title={new Date(lead.received_at).toLocaleString()}>
                  {relativeInstant(lead.received_at)}
                </td>
                <td className="px-3 py-2.5">
                  <Link
                    href={`/crm/leads/${lead.id}`}
                    data-lead-open={lead.id}
                    className="block max-w-56 truncate font-medium text-accent-strong hover:underline"
                  >
                    {contactLabel(lead)}
                  </Link>
                  {lead.email && (lead.first_name || lead.last_name) ? (
                    <span className="block max-w-56 truncate text-[11.5px] text-muted">{lead.email}</span>
                  ) : null}
                </td>
                <td className="px-3 py-2.5 text-muted">
                  <span className="block max-w-32 truncate">{lead.product_interest ?? "—"}</span>
                </td>
                {/* The column reads a *name*. "Assigned" answered "is there somebody?" and
                    not "who?" — which is the only question this column exists to answer, and
                    the reason the inbox's owner filter was unusable without a second tab. */}
                <td className="px-3 py-2.5" data-lead-owner-cell>
                  {lead.owner_user_id ? (
                    <span className="block max-w-40 truncate text-muted" title={lead.owner_user_id}>
                      {ownerLabel(ownersById, lead.owner_user_id)}
                    </span>
                  ) : (
                    <span className="rounded-full bg-caution-soft px-1.5 py-0.5 text-[11px] text-caution">
                      Unassigned
                    </span>
                  )}
                </td>
                <td className="px-3 py-2.5">
                  <span
                    data-sla={state}
                    className={`inline-flex items-center rounded-md px-1.5 py-0.5 text-[11.5px] ${SLA_STATE_TONE[state]}`}
                    title={lead.first_response_due_at ?? undefined}
                  >
                    {SLA_STATE_LABEL[state]}
                    {remaining && state !== "none" && state !== "met" ? ` · ${remaining}` : ""}
                  </span>
                </td>
                <td className="px-3 py-2.5">
                  <span
                    className={`inline-flex items-center rounded-full px-2 py-0.5 text-[11px] font-medium ${LEAD_STATUS_TONE[lead.status] ?? "bg-quiet-soft text-muted"}`}
                  >
                    {LEAD_STATUS_LABEL[lead.status] ?? lead.status}
                  </span>
                  {lead.decision && lead.decision !== "created" ? (
                    <span className="mt-0.5 block text-[11px] text-muted">
                      {DECISION_LABEL[lead.decision] ?? lead.decision}
                    </span>
                  ) : null}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}
