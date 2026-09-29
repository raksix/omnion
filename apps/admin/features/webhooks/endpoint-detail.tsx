"use client";

/**
 * One endpoint: what it is, what its receiver has been doing, and every delivery it ever got.
 *
 * Three tabs because they answer three different questions, and the middle one is the one
 * people came for:
 *
 * - **Overview** — the settings, plus the three operations that change behaviour: send a test
 *   delivery, rotate the secret, and switch it off.
 * - **Deliveries** — the queue history, narrowed by status, event name, window and free text,
 *   with a single and a bulk redelivery. This is where a missing delivery is diagnosed, so the
 *   failed row's own error string is shown rather than summarised: "receiver answered 500" and
 *   "connection refused" are different problems with different fixes.
 * - **Stats** — the summary, over a window the reader chooses. Three numbers carry the weight:
 *   the success rate over *traffic*, the pending count, and the 95th percentile duration.
 *
 * Two things this screen refuses to do, both because the alternative is a lie:
 *
 * - **A pending row cannot be forced again, and the button says so** rather than failing with
 *   a toast. The runner holds that row's lease; a reset would hand it to the next claim while
 *   the attempt is still in flight, which is the one place a redelivery could double-send.
 * - **A success rate over a history that is only probes reads "no traffic", not "100%".** The
 *   API already excludes test rows from the rate; the screen says which it is looking at.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  Ban,
  ChevronDown,
  ChevronRight,
  Copy,
  Filter,
  KeyRound,
  Play,
  RefreshCw,
  RotateCw,
  Search,
  Trash2,
  TriangleAlert,
  Webhook,
  X,
} from "lucide-react";
import Link from "next/link";
import { useRouter, useSearchParams } from "next/navigation";

import {
  ApiError,
  deleteWebhookEndpoint,
  fetchEventCatalogue,
  fetchWebhookDeliveries,
  fetchWebhookEndpoint,
  fetchWebhookStats,
  redeliverWebhookDeliveries,
  rotateWebhookSecret,
  testWebhookEndpoint,
  updateWebhookEndpoint,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import type { EventCatalogue, WebhookDelivery, WebhookEndpoint } from "@/lib/types";

/** How many rows one page carries. The API clamps to 1..200. */
const PAGE = 25;

const STATUSES = [
  { value: "", label: "Any status" },
  { value: "pending", label: "Pending" },
  { value: "delivered", label: "Delivered" },
  { value: "failed", label: "Failed" },
] as const;

const WINDOWS = [
  { value: "24h", label: "Last 24 hours", hours: 24 },
  { value: "7d", label: "Last 7 days", hours: 24 * 7 },
  { value: "30d", label: "Last 30 days", hours: 24 * 30 },
  { value: "", label: "All time", hours: null },
] as const;

const STAT_WINDOWS = [
  { value: "1", label: "Last hour" },
  { value: "24", label: "Last 24 hours" },
  { value: "168", label: "Last 7 days" },
  { value: "720", label: "Last 30 days" },
] as const;

type Tab = "overview" | "deliveries" | "stats";

/** The host part of a URL — the readable column. */
function hostOf(url: string): string {
  try {
    return new URL(url).host || url;
  } catch {
    return url;
  }
}

/** How long a measured duration reads. */
function duration(ms: number | null): string {
  if (ms === null) return "—";
  if (ms < 1000) return `${ms} ms`;
  return `${(ms / 1000).toFixed(ms < 10_000 ? 2 : 1)} s`;
}

export function EndpointDetail({ endpointId }: { endpointId: string }) {
  const router = useRouter();
  const params = useSearchParams();
  const search = params?.toString() ?? "";
  const view = new URLSearchParams(search);
  const rawTab = view.get("tab");
  const tab: Tab = rawTab === "deliveries" || rawTab === "stats" ? rawTab : "overview";
  const status = view.get("status") ?? "";
  const name = view.get("name") ?? "";
  const windowKey = view.get("window") ?? "30d";
  const q = view.get("q") ?? "";
  const statWindow = view.get("stat_window") ?? "24";

  const [endpoint, setEndpoint] = useState<WebhookEndpoint | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const [rows, setRows] = useState<WebhookDelivery[]>([]);
  const [total, setTotal] = useState(0);
  const [hasMore, setHasMore] = useState(false);
  const [historyLoading, setHistoryLoading] = useState(false);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [expanded, setExpanded] = useState<string | null>(null);

  // The keyset the next page is read with, or `null` at the end. Declared before
  // `loadHistory` reads it; it is state the screen renders (whether "Load older" is offered)
  // and not a ref, because its absence is the signal.
  const [pageCursor, setPageCursor] = useState<{ at: string; id: string } | null>(null);

  const [catalogue, setCatalogue] = useState<EventCatalogue | null>(null);
  const [stats, setStats] = useState<Awaited<ReturnType<typeof fetchWebhookStats>> | null>(null);

  const [issuedSecret, setIssuedSecret] = useState<string | null>(null);
  const [secretStored, setSecretStored] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const searchInput = useRef<HTMLInputElement>(null);

  const activeWindow =
    WINDOWS.find((entry) => entry.value === windowKey) ?? WINDOWS[2];
  const windowStart = useMemo(() => {
    if (activeWindow.hours === null) return undefined;
    return new Date(Date.now() - activeWindow.hours * 3_600_000).toISOString();
  }, [activeWindow.hours]);

  const write = useCallback(
    (mutate: (next: URLSearchParams) => void) => {
      const next = new URLSearchParams(search);
      mutate(next);
      const value = next.toString();
      router.replace(value ? `/webhooks/${endpointId}?${value}` : `/webhooks/${endpointId}`);
    },
    [endpointId, router, search],
  );

  const loadEndpoint = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setEndpoint(await fetchWebhookEndpoint(endpointId));
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setLoading(false);
    }
  }, [endpointId]);

  const loadHistory = useCallback(
    async (append = false) => {
      setHistoryLoading(true);
      setError(null);
      try {
        const page = await fetchWebhookDeliveries(endpointId, {
          ...(status ? { status: [status] } : {}),
          ...(name ? { name: [name] } : {}),
          ...(q ? { q } : {}),
          ...(windowStart ? { from: windowStart } : {}),
          limit: PAGE,
          ...(append && pageCursor ? { cursor_at: pageCursor.at, cursor_id: pageCursor.id } : {}),
        });
        setRows((current) => (append ? [...current, ...page.deliveries] : page.deliveries));
        setTotal(page.total);
        setHasMore(page.has_more);
        setPageCursor(page.next_cursor);
      } catch (caught) {
        setError((caught as ApiError).message);
      } finally {
        setHistoryLoading(false);
      }
    },
    // `pageCursor` is read, not a dependency: including it would make every load re-run the
    // moment a cursor is stored, and the "Load older" button is the only thing that should.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [endpointId, status, name, q, windowStart],
  );

  useEffect(() => {
    void loadEndpoint();
  }, [loadEndpoint]);

  useEffect(() => {
    if (tab !== "deliveries") return;
    setPageCursor(null);
    void loadHistory(false);
  }, [tab, loadHistory]);

  useEffect(() => {
    if (tab !== "stats") return;
    void (async () => {
      try {
        setStats(await fetchWebhookStats(endpointId, Number(statWindow)));
        setError(null);
      } catch (caught) {
        setError((caught as ApiError).message);
      }
    })();
  }, [tab, endpointId, statWindow]);

  useEffect(() => {
    void (async () => {
      try {
        setCatalogue(await fetchEventCatalogue());
      } catch {
        // The event filter is a convenience; a catalogue that did not load leaves the other
        // three filters working rather than blocking the tab.
        setCatalogue(null);
      }
    })();
  }, []);

  const run = async (work: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await work();
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setBusy(false);
    }
  };

  const sendTest = () =>
    run(async () => {
      const report = await testWebhookEndpoint(endpointId);
      setNotice(
        report.deliveries > 0
          ? "Test delivery queued. It appears in the Deliveries tab as soon as the runner claims it."
          : "The test delivery was recorded but no endpoint was left to deliver it to.",
      );
    });

  const rotate = () =>
    run(async () => {
      const answer = await rotateWebhookSecret(endpointId);
      setIssuedSecret(answer.secret);
      setSecretStored(false);
    });

  const setEnabled = (next: boolean) =>
    run(async () => {
      await updateWebhookEndpoint(endpointId, { enabled: next });
      setNotice(
        next
          ? "Deliveries resume on the next event this endpoint subscribes to."
          : "Deliveries stop. The history stays readable and a test delivery still reaches the receiver.",
      );
      await loadEndpoint();
    });

  const remove = () =>
    run(async () => {
      await deleteWebhookEndpoint(endpointId);
      router.push("/webhooks");
    });

  const redeliver = (ids: string[]) =>
    run(async () => {
      const answer = await redeliverWebhookDeliveries(endpointId, ids);
      const parts = [`${answer.queued} queued again.`];
      if (answer.skipped.length > 0) {
        // The skipped rows are named with the reason the API gave, because the three
        // refusals call for different things from the operator.
        const reasons = new Map<string, string>();
        for (const skip of answer.skipped) reasons.set(skip.code, skip.message);
        parts.push(
          `${answer.skipped.length} skipped: ${[...reasons.values()].join(" ")}`,
        );
      }
      setNotice(parts.join(" "));
      setSelected(new Set());
      await loadHistory(false);
    });

  const toggleSelected = (id: string) => {
    setSelected((current) => {
      const next = new Set(current);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const selectable = useMemo(
    () => rows.filter((row) => row.status !== "pending"),
    [rows],
  );
  const pendingSelected = useMemo(
    () => rows.filter((row) => row.status === "pending" && selected.has(row.id)),
    [rows, selected],
  );

  const copy = async (text: string, what: string) => {
    try {
      await navigator.clipboard.writeText(text);
      setNotice(`Copied ${what}.`);
    } catch {
      setNotice("The browser refused clipboard access — select the value and copy it by hand.");
    }
  };

  const curl = (row: WebhookDelivery) =>
    [
      `curl -i -X POST '${endpoint?.url ?? ""}' \\`,
      `  -H 'Content-Type: application/json' \\`,
      `  -H 'X-Omnion-Delivery: ${row.id}' \\`,
      `  -H 'X-Omnion-Event: ${row.event_name}' \\`,
      `  -H 'X-Omnion-Signature: <the value from the delivery headers>' \\`,
      `  --data '${JSON.stringify({ id: row.event_id, name: row.event_name })}'`,
    ].join("\n");

  if (loading) {
    return (
      <p aria-busy="true" className="text-[13px] text-muted">
        Loading the endpoint…
      </p>
    );
  }

  if (!endpoint) {
    return (
      <div className="flex flex-col gap-3">
        <p data-webhook-missing className="text-[13px] text-muted">
          {error ?? "This endpoint does not exist, or it belongs to another organization."}
        </p>
        <Link href="/webhooks" className="text-[12.5px] text-accent-strong hover:underline">
          Back to the endpoints
        </Link>
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-4">
      {/* The header: the name, the receiver, and the state. Repeated on every tab because the
          question "which endpoint am I looking at" is asked on each of them. */}
      <div className="flex flex-wrap items-start gap-3 rounded-xl border border-line bg-surface p-3">
        <Webhook className="mt-0.5 size-4 shrink-0 text-muted" aria-hidden />
        <div className="min-w-0 flex-1">
          <p className="text-[14px] font-medium">{endpoint.name}</p>
          <p className="truncate text-[12.5px] text-muted" title={endpoint.url}>
            {hostOf(endpoint.url)}
            {endpoint.enabled ? "" : " · not delivering"}
          </p>
        </div>
        <Link
          href={`/webhooks/${endpoint.id}/edit`}
          data-webhook-edit
          className="rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] text-muted hover:text-ink"
        >
          Edit
        </Link>
      </div>

      <div role="tablist" aria-label="Endpoint views" className="flex gap-1 border-b border-line">
        {(
          [
            { id: "overview", label: "Overview" },
            { id: "deliveries", label: "Deliveries" },
            { id: "stats", label: "Stats" },
          ] as const
        ).map((entry) => {
          const active = tab === entry.id;
          return (
            <button
              key={entry.id}
              type="button"
              role="tab"
              aria-selected={active}
              data-webhook-tab={entry.id}
              onClick={() =>
                write((next) => {
                  if (entry.id === "overview") next.delete("tab");
                  else next.set("tab", entry.id);
                })
              }
              className={`-mb-px border-b-2 px-3 py-2 text-[13px] transition ${
                active
                  ? "border-accent font-medium text-ink"
                  : "border-transparent text-muted hover:text-ink"
              }`}
            >
              {entry.label}
            </button>
          );
        })}
      </div>

      {notice ? (
        <p
          data-webhook-notice
          role="status"
          className="rounded-lg border border-line bg-surface px-3 py-2 text-[12.5px] text-muted"
        >
          {notice}
        </p>
      ) : null}

      {error ? (
        <div
          data-webhook-error
          role="alert"
          className="flex items-start gap-2 rounded-lg border border-red-500/40 bg-red-500/5 px-3 py-2.5 text-[12.5px]"
        >
          <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-red-600" aria-hidden />
          <span className="flex-1">{error}</span>
          <button type="button" onClick={() => void loadHistory(false)} className="text-accent-strong hover:underline">
            Retry
          </button>
        </div>
      ) : null}

      {issuedSecret ? (
        <div className="flex flex-col gap-3 rounded-xl border border-caution/40 bg-caution-soft p-3" data-webhook-rotation>
          <p className="flex items-start gap-2 text-[12.5px]">
            <TriangleAlert className="mt-0.5 size-4 shrink-0 text-caution" aria-hidden />
            <span>
              The secret is rotated. Every delivery signed with the previous one stops verifying,
              so update the receiver before the next event is published — a receiver that has not
              been updated rejects everything this endpoint sends.
            </span>
          </p>
          <div className="flex items-center gap-2">
            <code
              data-webhook-rotation-value
              className="flex-1 overflow-x-auto rounded-lg border border-line bg-canvas px-2.5 py-2 font-mono text-[12.5px]"
            >
              {issuedSecret}
            </code>
            <button
              type="button"
              onClick={() => void copy(issuedSecret, "the new secret")}
              data-webhook-rotation-copy
              className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-2 text-[12.5px] text-muted hover:text-ink"
            >
              <Copy className="size-3.5" aria-hidden />
              Copy
            </button>
          </div>
          <label className="flex items-start gap-2 text-[12.5px]">
            <input
              type="checkbox"
              checked={secretStored}
              data-webhook-rotation-stored
              onChange={(event) => setSecretStored(event.target.checked)}
              className="mt-0.5"
            />
            <span>The receiver has been updated with this secret.</span>
          </label>
          <button
            type="button"
            data-webhook-rotation-done
            disabled={!secretStored}
            onClick={() => setIssuedSecret(null)}
            className="self-start rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white disabled:opacity-50"
          >
            Done
          </button>
        </div>
      ) : null}

      {tab === "overview" ? (
        <div className="flex flex-col gap-4">
          <dl className="grid gap-3 rounded-xl border border-line bg-surface p-3 text-[12.5px] sm:grid-cols-2">
            <div>
              <dt className="text-muted">Receiver URL</dt>
              <dd className="break-all text-ink">{endpoint.url}</dd>
            </div>
            <div>
              <dt className="text-muted">Subscriptions</dt>
              <dd className="text-ink">{endpoint.events.length} events</dd>
            </div>
            <div>
              <dt className="text-muted">Created</dt>
              <dd className="text-ink">{formatTimestamp(endpoint.created_at)}</dd>
            </div>
            <div>
              <dt className="text-muted">Last change</dt>
              <dd className="text-ink">{formatTimestamp(endpoint.updated_at)}</dd>
            </div>
          </dl>

          <div className="flex flex-wrap gap-2">
            <button
              type="button"
              onClick={() => void sendTest()}
              disabled={busy}
              data-webhook-overview-test
              className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] text-muted hover:text-ink disabled:opacity-50"
            >
              <Play className="size-3.5" aria-hidden />
              Send a test delivery
            </button>
            <button
              type="button"
              onClick={() => void rotate()}
              disabled={busy}
              data-webhook-overview-rotate
              className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] text-muted hover:text-ink disabled:opacity-50"
            >
              <RotateCw className="size-3.5" aria-hidden />
              Rotate secret
            </button>
            <button
              type="button"
              onClick={() => void setEnabled(!endpoint.enabled)}
              disabled={busy}
              data-webhook-overview-toggle
              className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] text-muted hover:text-ink disabled:opacity-50"
            >
              {endpoint.enabled ? (
                <Ban className="size-3.5" aria-hidden />
              ) : (
                <Play className="size-3.5" aria-hidden />
              )}
              {endpoint.enabled ? "Stop delivering" : "Deliver again"}
            </button>
            <button
              type="button"
              onClick={() => setConfirmDelete(true)}
              data-webhook-overview-delete
              className="ml-auto inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] text-muted hover:text-red-600"
            >
              <Trash2 className="size-3.5" aria-hidden />
              Remove endpoint
            </button>
          </div>

          {confirmDelete ? (
            <div
              role="alertdialog"
              aria-label="Remove the endpoint"
              data-webhook-confirm
              className="flex flex-wrap items-center gap-3 rounded-xl border border-red-500/40 bg-red-500/5 px-3 py-2.5 text-[12.5px]"
            >
              <TriangleAlert className="size-4 shrink-0 text-red-600" aria-hidden />
              <span className="flex-1">
                Remove <strong>{endpoint.name}</strong> and its delivery history?
              </span>
              <button
                type="button"
                onClick={() => setConfirmDelete(false)}
                className="rounded-lg border border-line px-2.5 py-1.5 text-muted hover:text-ink"
              >
                <X className="size-3.5" aria-hidden />
                Keep it
              </button>
              <button
                type="button"
                onClick={() => void remove()}
                disabled={busy}
                data-webhook-confirm-remove
                className="rounded-lg bg-red-600 px-2.5 py-1.5 font-medium text-white disabled:opacity-50"
              >
                Remove it
              </button>
            </div>
          ) : null}
        </div>
      ) : null}

      {tab === "deliveries" ? (
        <>
          <section
            aria-label="Delivery filters"
            data-webhook-delivery-filters
            className="flex flex-wrap items-end gap-3 rounded-xl border border-line bg-surface p-3"
          >
            <label className="flex flex-col gap-1 text-[11.5px] text-muted">
              <span className="font-medium">Find</span>
              <input
                ref={searchInput}
                type="search"
                value={q}
                placeholder="Delivery id or event name"
                onChange={(event) =>
                  write((next) => {
                    if (event.target.value) next.set("q", event.target.value);
                    else next.delete("q");
                  })
                }
                className="w-52 rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink outline-none focus-visible:ring-2 focus-visible:ring-accent"
              />
            </label>

            <label className="flex flex-col gap-1 text-[11.5px] text-muted">
              <span className="font-medium">Status</span>
              <select
                value={status}
                data-webhook-delivery-status
                onChange={(event) =>
                  write((next) => {
                    if (event.target.value) next.set("status", event.target.value);
                    else next.delete("status");
                  })
                }
                className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink outline-none focus-visible:ring-2 focus-visible:ring-accent"
              >
                {STATUSES.map((entry) => (
                  <option key={entry.value} value={entry.value}>
                    {entry.label}
                  </option>
                ))}
              </select>
            </label>

            <label className="flex flex-col gap-1 text-[11.5px] text-muted">
              <span className="font-medium">Event</span>
              <select
                value={name}
                data-webhook-delivery-name
                onChange={(event) =>
                  write((next) => {
                    if (event.target.value) next.set("name", event.target.value);
                    else next.delete("name");
                  })
                }
                className="max-w-52 rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink outline-none focus-visible:ring-2 focus-visible:ring-accent"
              >
                <option value="">Any event</option>
                {(catalogue?.events ?? [])
                  .filter((entry) => endpoint.events.includes(entry.name) || entry.name.endsWith(".*"))
                  .map((entry) => (
                    <option key={entry.name} value={entry.name}>
                      {entry.name}
                    </option>
                  ))}
              </select>
            </label>

            <label className="flex flex-col gap-1 text-[11.5px] text-muted">
              <span className="font-medium">Window</span>
              <select
                value={windowKey}
                data-webhook-delivery-window
                onChange={(event) =>
                  write((next) => {
                    if (event.target.value && event.target.value !== "30d")
                      next.set("window", event.target.value);
                    else next.delete("window");
                  })
                }
                className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink outline-none focus-visible:ring-2 focus-visible:ring-accent"
              >
                {WINDOWS.map((entry) => (
                  <option key={entry.value} value={entry.value}>
                    {entry.label}
                  </option>
                ))}
              </select>
            </label>

            <button
              type="button"
              onClick={() => void loadHistory(false)}
              data-webhook-delivery-refresh
              className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] text-muted hover:text-ink"
            >
              <RefreshCw className="size-3.5" aria-hidden />
              Refresh
            </button>

            {selected.size > 0 ? (
              <button
                type="button"
                onClick={() => void redeliver([...selected])}
                disabled={busy}
                data-webhook-redeliver-bulk
                className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-2.5 py-1.5 text-[12.5px] font-medium text-white disabled:opacity-50"
              >
                <RotateCw className="size-3.5" aria-hidden />
                Send {selected.size} again
              </button>
            ) : null}
          </section>

          {pendingSelected.length > 0 ? (
            <p data-webhook-pending-note className="text-[11.5px] text-caution">
              {pendingSelected.length} of the selected deliveries are already queued for another
              attempt. The runner holds those rows, so they are left alone; the rest are sent
              again.
            </p>
          ) : null}

          <div className="flex items-center gap-2 text-[11.5px] text-muted">
            <Filter className="size-3.5" aria-hidden />
            <span data-webhook-delivery-total>
              {rows.length === total
                ? `${total} deliver${total === 1 ? "y" : "ies"}`
                : `Showing ${rows.length} of ${total}`}
            </span>
            {selectable.length > 0 ? (
              <button
                type="button"
                onClick={() =>
                  setSelected(
                    selected.size === selectable.length
                      ? new Set()
                      : new Set(selectable.map((row) => row.id)),
                  )
                }
                data-webhook-select-all
                className="ml-auto text-accent-strong hover:underline"
              >
                {selected.size === selectable.length ? "Clear selection" : "Select all sent rows"}
              </button>
            ) : null}
          </div>

          <div className="overflow-hidden rounded-xl border border-line bg-surface">
            {historyLoading && rows.length === 0 ? (
              <div aria-busy="true" data-webhook-delivery-skeleton className="flex flex-col gap-2 p-4">
                {Array.from({ length: 5 }, (_, index) => (
                  <div key={index} className="h-9 animate-pulse rounded bg-quiet-soft" />
                ))}
              </div>
            ) : rows.length === 0 ? (
              <div data-webhook-delivery-empty className="px-4 py-12 text-center">
                <p className="text-[13px] font-medium">No delivery matches these filters.</p>
                <p className="text-[12.5px] text-muted">
                  {status || name || q
                    ? "Widen the window or clear a filter."
                    : "Send a test delivery from the Overview tab to see the first one."}
                </p>
              </div>
            ) : (
              <div className="overflow-x-auto">
                <table data-webhook-delivery-table className="w-full border-collapse text-left text-[13px]">
                  <thead>
                    <tr className="border-b border-line text-[11.5px] text-muted">
                      <th scope="col" className="w-9 px-3 py-2.5">
                        <span className="sr-only">Expand</span>
                      </th>
                      <th scope="col" className="w-9 px-3 py-2.5">
                        <span className="sr-only">Select</span>
                      </th>
                      <th scope="col" className="px-3 py-2.5 font-medium">
                        Delivery
                      </th>
                      <th scope="col" className="px-3 py-2.5 font-medium">
                        Event
                      </th>
                      <th scope="col" className="px-3 py-2.5 font-medium">
                        Status
                      </th>
                      <th scope="col" className="px-3 py-2.5 font-medium">
                        Attempts
                      </th>
                      <th scope="col" className="px-3 py-2.5 font-medium">
                        Answer
                      </th>
                      <th scope="col" className="px-3 py-2.5 font-medium">
                        Duration
                      </th>
                      <th scope="col" className="px-3 py-2.5 font-medium">
                        Queued
                      </th>
                      <th scope="col" className="px-3 py-2.5 font-medium">
                        <span className="sr-only">Actions</span>
                      </th>
                    </tr>
                  </thead>
                  <tbody>
                    {rows.map((row) => {
                      const isOpen = expanded === row.id;
                      const isSelected = selected.has(row.id);
                      return (
                        <tr
                          key={row.id}
                          data-webhook-delivery-row={row.id}
                          data-open={isOpen ? "true" : undefined}
                          className="border-b border-line/60"
                        >
                          <td className="px-3 py-2.5">
                            <button
                              type="button"
                              aria-label={isOpen ? "Collapse the delivery" : "Expand the delivery"}
                              aria-expanded={isOpen}
                              data-webhook-delivery-expand={row.id}
                              onClick={() => setExpanded(isOpen ? null : row.id)}
                              className="rounded p-0.5 text-muted hover:text-ink"
                            >
                              {isOpen ? (
                                <ChevronDown className="size-4" aria-hidden />
                              ) : (
                                <ChevronRight className="size-4" aria-hidden />
                              )}
                            </button>
                          </td>
                          <td className="px-3 py-2.5">
                            <input
                              type="checkbox"
                              // A pending row is not selectable at all rather than selectable
                              // and then refused: the runner owns it, and a checkbox that
                              // leads to a 409 is a control the screen is lying about.
                              disabled={row.status === "pending"}
                              checked={isSelected}
                              data-webhook-delivery-select={row.id}
                              onChange={() => toggleSelected(row.id)}
                              aria-label={`Select the delivery of ${row.event_name}`}
                            />
                          </td>
                          <td className="px-3 py-2.5">
                            <button
                              type="button"
                              onClick={() => void copy(row.id, "the delivery id")}
                              data-webhook-delivery-copy={row.id}
                              title="Copy the delivery id"
                              className="font-mono text-[12px] text-muted hover:text-ink"
                            >
                              {row.id.slice(0, 8)}
                            </button>
                            {row.trigger !== "event" ? (
                              <span
                                data-webhook-trigger={row.id}
                                className="ml-1.5 rounded-full bg-quiet-soft px-1.5 py-0.5 text-[10.5px] uppercase tracking-wide text-muted"
                              >
                                {row.trigger}
                              </span>
                            ) : null}
                          </td>
                          <td className="px-3 py-2.5 text-muted">{row.event_name}</td>
                          <td className="px-3 py-2.5">
                            <span
                              data-webhook-delivery-state={row.id}
                              className={
                                row.status === "failed"
                                  ? "text-red-600"
                                  : row.status === "delivered"
                                    ? "text-positive"
                                    : "text-muted"
                              }
                            >
                              {row.status}
                            </span>
                            {row.error ? (
                              /* The receiver's own words, not a summary: "answered 500" and
                                 "connection refused" are different problems. */
                              <span
                                data-webhook-delivery-error={row.id}
                                className="mt-0.5 block max-w-xs truncate text-[11.5px] text-muted"
                                title={row.error}
                              >
                                {row.error}
                              </span>
                            ) : null}
                          </td>
                          <td className="px-3 py-2.5 tabular-nums text-muted">
                            {row.attempts}/{row.max_attempts}
                            {row.redeliver_count > 0 ? (
                              <span className="ml-1 text-[11px]">· sent {row.redeliver_count}×</span>
                            ) : null}
                          </td>
                          <td className="px-3 py-2.5 tabular-nums text-muted">
                            {row.response_status ?? "—"}
                          </td>
                          <td className="px-3 py-2.5 tabular-nums text-muted">
                            {duration(row.duration_ms)}
                          </td>
                          <td className="px-3 py-2.5 text-muted">
                            {formatTimestamp(row.created_at)}
                          </td>
                          <td className="px-3 py-2.5">
                            <div className="flex items-center justify-end gap-1">
                              <button
                                type="button"
                                title="Copy as cURL"
                                aria-label="Copy this delivery as a cURL command"
                                data-webhook-delivery-curl={row.id}
                                onClick={() => void copy(curl(row), "the cURL command")}
                                className="rounded p-1.5 text-muted hover:bg-quiet-soft hover:text-ink"
                              >
                                <Copy className="size-3.5" aria-hidden />
                              </button>
                              <button
                                type="button"
                                title="Send this delivery again"
                                aria-label={`Send the delivery of ${row.event_name} again`}
                                data-webhook-delivery-redeliver={row.id}
                                disabled={busy || row.status === "pending"}
                                onClick={() => void redeliver([row.id])}
                                className="rounded p-1.5 text-muted hover:bg-quiet-soft hover:text-ink disabled:opacity-40"
                              >
                                <RotateCw className="size-3.5" aria-hidden />
                              </button>
                            </div>
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              </div>
            )}
          </div>

          {hasMore ? (
            <button
              type="button"
              onClick={() => void loadHistory(true)}
              data-webhook-load-older
              className="self-start rounded-lg border border-line px-3 py-1.5 text-[12.5px] text-muted hover:text-ink"
            >
              Load older deliveries
            </button>
          ) : null}
        </>
      ) : null}

      {tab === "stats" ? (
        <div className="flex flex-col gap-4">
          <div className="flex items-end gap-3">
            <label className="flex flex-col gap-1 text-[11.5px] text-muted">
              <span className="font-medium">Window</span>
              <select
                value={statWindow}
                data-webhook-stat-window
                onChange={(event) =>
                  write((next) => next.set("stat_window", event.target.value))
                }
                className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] text-ink outline-none focus-visible:ring-2 focus-visible:ring-accent"
              >
                {STAT_WINDOWS.map((entry) => (
                  <option key={entry.value} value={entry.value}>
                    {entry.label}
                  </option>
                ))}
              </select>
            </label>
          </div>

          {stats ? (
            <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4" data-webhook-stats>
              <div className="rounded-xl border border-line bg-surface p-3">
                <p className="text-[11.5px] text-muted">Success rate</p>
                <p data-webhook-stat-rate className="text-[20px] font-medium tabular-nums">
                  {/* A history that is only probes has no rate at all. Rendering 100% there
                      would be the most flattering possible lie on this screen. */}
                  {stats.success_rate === null
                    ? "No traffic"
                    : `${Math.round(stats.success_rate * 100)}%`}
                </p>
                <p className="text-[11.5px] text-muted">
                  {stats.tests} test deliver{stats.tests === 1 ? "y" : "ies"} excluded
                </p>
              </div>
              <div className="rounded-xl border border-line bg-surface p-3">
                <p className="text-[11.5px] text-muted">Delivered</p>
                <p data-webhook-stat-delivered className="text-[20px] font-medium tabular-nums">
                  {stats.delivered}
                </p>
                <p className="text-[11.5px] text-muted">accepted by the receiver</p>
              </div>
              <div className="rounded-xl border border-line bg-surface p-3">
                <p className="text-[11.5px] text-muted">Failed</p>
                <p
                  data-webhook-stat-failed
                  className={`text-[20px] font-medium tabular-nums ${
                    stats.failed > 0 ? "text-red-600" : ""
                  }`}
                >
                  {stats.failed}
                </p>
                <p className="text-[11.5px] text-muted">ran out of attempts</p>
              </div>
              <div className="rounded-xl border border-line bg-surface p-3">
                <p className="text-[11.5px] text-muted">Waiting</p>
                <p data-webhook-stat-pending className="text-[20px] font-medium tabular-nums">
                  {stats.pending}
                </p>
                <p className="text-[11.5px] text-muted">
                  95th percentile {duration(stats.p95_duration_ms)}
                </p>
              </div>
            </div>
          ) : (
            <div aria-busy="true" data-webhook-stat-skeleton className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
              {Array.from({ length: 4 }, (_, index) => (
                <div key={index} className="h-24 animate-pulse rounded-xl bg-quiet-soft" />
              ))}
            </div>
          )}

          <p className="text-[11.5px] text-muted">
            The rate counts only deliveries the platform made for real: probes and rows still
            waiting are left out, so a queue that has not drained yet never reads as a failure.
          </p>
        </div>
      ) : null}
    </div>
  );
}
